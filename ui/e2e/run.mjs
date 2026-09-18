// `npm run e2e`: the browser tier (T-455).
//
// Shape deliberately mirrors `ui/test/run.mjs` — discover by filename, one node process per file,
// exit code from the failures — so there is one way this repo runs a UI suite. What it adds is the
// lifecycle the unit tier does not need: **one** `hk serve` over a recorded fixture, started once
// and shared, because starting it per file is most of the runtime.
//
// Every file is handed `HK_E2E_ORIGIN` and `HK_E2E_TOKEN` and brings its own browser.
//
// **Per-spec timeout (T-473).** Observed live: a spec sat for 31 minutes with zero output and Chrome
// parked at `about:blank`, and the gate waited the whole time because nothing here could tell "stuck"
// from "slow". Every spec now runs under `HK_E2E_SPEC_TIMEOUT_MS` (default below); on expiry the spec
// is reported as a FAILURE naming the file and the deadline — a timeout must never read as a pass —
// and its **whole process tree** is killed before the runner moves on: not just the spawned node
// process, but the Chrome it launched (cdp.mjs deliberately puts Chrome in its own process group, so
// killing the node process alone leaves it running) and, for a file that starts its own `hk serve`
// (canvas-journey.e2e.mjs), that backend too. The kill walks the OS parent-child tree from the spec's
// pid (`killSpecTree` below) rather than relying on process groups, because that is the one
// relationship Chrome's own detachment cannot escape.
import { readdirSync } from "node:fs";
import { execFileSync, spawn } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { startBackend, assertRealCsp, tileCost } from "./backend.mjs";
import { findChrome } from "./cdp.mjs";
import { waitForSurfaceHistory } from "./harness.mjs";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const only = process.argv.slice(2).filter((a) => !a.startsWith("-"));
const discovered = readdirSync(HERE)
  .filter((f) => f.endsWith(".e2e.mjs") && (only.length === 0 || only.some((o) => f.includes(o))))
  .sort();
// `HK_E2E_EXTRA_SPECS`: a comma-separated list of extra spec files to run alongside the discovered
// ones, given as absolute paths. **Selftest-only** (`ui/e2e/selftest-timeout.mjs`, T-473): no normal
// run sets this. It exists because `discovered` above only ever looks inside `ui/e2e/` itself, so a
// deliberately-hanging fixture living elsewhere (`ui/e2e/selftest-fixtures/hang.e2e.mjs`) is invisible
// to every real run by construction — this is the door the selftest uses to drive it through the real
// runner instead of a reimplementation of the timeout logic.
const extraSpecs = (process.env.HK_E2E_EXTRA_SPECS ?? "").split(",").map((s) => s.trim()).filter(Boolean);
const files = [...discovered, ...extraSpecs.map((p) => path.relative(HERE, p))];
if (files.length === 0) { console.error("no e2e files matched"); process.exit(1); }

// Well above the slowest file today (canvas-journey, ~95 s) so a healthy run never trips it, and far
// below the 31-minute hang that motivated it. Override for a deliberately short deadline in the
// timeout selftest.
const SPEC_TIMEOUT_MS = Number(process.env.HK_E2E_SPEC_TIMEOUT_MS ?? 300_000);

// Fail before spending a backend start on a machine that cannot drive a browser. Like `just
// test-ui` (T-358), this FAILS rather than skips: a tier that silently does nothing is worse than
// one that blocks, because the green is silent and unbounded in time.
const chrome = findChrome();

const t0 = Date.now();
const backend = await startBackend();
// Every exit from here on goes through `stop()`. A leaked `hk serve` holds the port and the ring,
// so the next run starts against a stranger's history — and the first version of this file leaked
// one on its first failure.
process.on("exit", backend.stop);
// The spec currently running, if any — so a SIGINT/SIGTERM to THIS process (a developer's Ctrl-C, or
// the gate's own deadline) also takes down whatever that spec spawned, rather than leaving a Chrome
// or an `hk serve` it started to survive this process's own death.
let activeSpec = null;
for (const sig of ["SIGINT", "SIGTERM"]) {
  process.on(sig, () => {
    if (activeSpec) killSpecTree(activeSpec.pid);
    backend.stop();
    process.exit(130);
  });
}

/** The direct children of `pid`, from the OS process table. Best effort: an empty array on any
 * failure (no children — `pgrep` exits 1 — or `pgrep` itself is missing), never a thrown error,
 * because a timeout's cleanup must not itself throw and abandon the sweep partway through. */
function childPids(pid) {
  try {
    return execFileSync("pgrep", ["-P", String(pid)], { encoding: "utf8" })
      .split("\n").map((s) => s.trim()).filter(Boolean).map(Number);
  } catch {
    return [];
  }
}

/** Every descendant of `pid`, walking the OS parent-child (PPID) tree rather than the process-GROUP
 * tree. PPID is the relationship that matters here: `cdp.mjs`'s `launch()` deliberately puts Chrome
 * in its OWN process group (so its renderer/GPU children die together without taking the node
 * process that launched it), which means a plain `kill(-pid)` on the spec's group would miss Chrome
 * entirely. PPID survives that — Chrome is still, and stays, a child of the spec process. */
function descendantPids(pid) {
  const out = [];
  let frontier = [pid];
  while (frontier.length) {
    const next = frontier.flatMap(childPids);
    out.push(...next);
    frontier = next;
  }
  return out;
}

/**
 * Kill a spec's whole process tree: itself, and everything it spawned transitively — its Chrome (and
 * Chrome's own renderer/GPU children), and, for a file that starts its own `hk serve`
 * (canvas-journey.e2e.mjs), that backend too. SIGKILL throughout: a hung spec has already shown it
 * will not respond to anything politer, and this runs when nothing else is watching it. Each victim
 * is killed both as a plain pid and as a process-group leader (`-pid`) — the latter is a no-op
 * (caught) for anything that is not one, and is exactly what reaches Chrome's isolated group.
 *
 * Returns the pids it attempted, for the caller to report — this is the "no orphans" guarantee's own
 * evidence, checkable independently with `pgrep`.
 */
function killSpecTree(pid) {
  const victims = [pid, ...descendantPids(pid)];
  for (const p of victims) {
    try { process.kill(-p, "SIGKILL"); } catch { /* not a group leader, or already gone */ }
    try { process.kill(p, "SIGKILL"); } catch { /* already gone */ }
  }
  return victims;
}

/**
 * Run one spec file, racing it against `SPEC_TIMEOUT_MS`. Resolves `{ status, timedOut }` — never
 * throws, and never lets a timeout read as `status === 0`.
 */
function runSpec(file, env) {
  return new Promise((resolve) => {
    const child = spawn(process.execPath, [path.join(HERE, file)], {
      stdio: "inherit", env,
      // Its own process group, isolating it from this runner's — belt-and-braces alongside the
      // PPID-based sweep above: nothing about the kill below depends on this, but it keeps a
      // half-dead spec from ever sharing a signal target with the runner itself.
      detached: true,
    });
    activeSpec = child;
    let timedOut = false;
    const timer = setTimeout(() => {
      timedOut = true;
      console.error(`\ne2e: TIMEOUT — ${file} exceeded HK_E2E_SPEC_TIMEOUT_MS=${SPEC_TIMEOUT_MS} ms; ` +
        "killing its process tree (its Chrome and any backend it started) and reporting it FAILED");
      const victims = killSpecTree(child.pid);
      console.error(`e2e: killed ${victims.length} process(es) for ${file}: [${victims.join(", ")}]`);
    }, SPEC_TIMEOUT_MS);
    const finish = (status) => {
      clearTimeout(timer);
      if (activeSpec === child) activeSpec = null;
      resolve({ status: timedOut ? 1 : (status ?? 1), timedOut });
    };
    child.on("exit", (code) => finish(code));
    child.on("error", () => finish(1));
  });
}

const failed = [];
const times = [];
let tUp = 0;
try {
  const csp = await assertRealCsp(backend.origin);
  // The server's own backpressure cap, read HERE rather than from a test: over the cap
  // `/api/tiles` answers 503, so a test process asking for it while its own browser holds four
  // reads in flight gets refused — the harness would have manufactured the very condition it
  // exists to detect.
  const cov = await waitForSurfaceHistory(backend.origin, backend.token);
  // AFTER the readiness page, not before, and this order is load-bearing: that page leaves up to
  // `in_flight_limit` tile reads outstanding, and the server counts them until they finish. Since
  // `tileCost` retries a 503, asking for the cap here doubles as the drain — the next page opens
  // against a route that is answering, instead of being refused while it addresses the surface.
  const cost = await tileCost(backend.origin, backend.token);
  tUp = Date.now() - t0;
  console.log(`e2e: chrome ${chrome}`);
  console.log(`e2e: hk serve on ${backend.origin}, ready in ${tUp} ms`);
  console.log(`e2e: CSP under test — ${csp.replace(/\s+/g, " ").trim()}`);
  console.log(`e2e: /api/tiles cost.in_flight_limit = ${cost.in_flight_limit}`);
  console.log(`e2e: per-spec timeout ${SPEC_TIMEOUT_MS} ms (HK_E2E_SPEC_TIMEOUT_MS)`);
  console.log(cov.ok
    ? `e2e: ${cov.text} (after ${cov.ms} ms) — pages now open on the observed region`
    : `e2e: WARNING — the surface is not ready after ${cov.ms} ms: ${cov.reason}\n` +
      "     Running anyway: the guards below are written to diagnose exactly this, and aborting here\n" +
      "     would report a broken page as a broken harness.");

  for (const f of files) {
    // Drain before every file. A browser that has just been killed can leave up to
    // `in_flight_limit` tile reads outstanding, and `hk serve` counts them until they finish — so
    // the next page's single probe request gets the 503 and the page puts up "The surface could not
    // be addressed". That is a real product finding (see ui/e2e/README.md), but between files it is
    // this harness's mess, and a suite whose second file fails because of its first is a suite
    // nobody trusts. `tileCost` retries a 503, so asking for the cap is the drain.
    await tileCost(backend.origin, backend.token);
    const s = Date.now();
    const { status, timedOut } = await runSpec(f, {
      ...process.env,
      HK_E2E_ORIGIN: backend.origin, HK_E2E_TOKEN: backend.token, CHROME: chrome,
      HK_E2E_TILE_LIMIT: String(cost.in_flight_limit ?? ""),
    });
    const ms = Date.now() - s;
    times.push([f, ms, timedOut]);
    if (status !== 0) failed.push(f);
  }
} finally {
  backend.stop();
}

const total = Date.now() - t0;
console.log(`\ne2e: ${files.length - failed.length}/${files.length} files passed in ${(total / 1000).toFixed(1)} s` +
  ` (backend ${(tUp / 1000).toFixed(1)} s)${failed.length ? `; failed: ${failed.join(", ")}` : ""}`);
for (const [f, ms, timedOut] of times) {
  console.log(`  ${(ms / 1000).toFixed(1)} s  ${f}${timedOut ? "  [TIMED OUT]" : ""}`);
}
process.exit(failed.length ? 1 : 0);
