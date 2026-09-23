// `npm run e2e`: the browser tier (T-455).
//
// Shape deliberately mirrors `ui/test/run.mjs` — discover by filename, one node process per file,
// exit code from the failures — so there is one way this repo runs a UI suite. What it adds is the
// lifecycle the unit tier does not need: an `hk serve` over a recorded fixture, started once per
// **lane** and shared by the specs that lane runs, because starting it per file is most of the
// runtime.
//
// Every file is handed `HK_E2E_ORIGIN` and `HK_E2E_TOKEN` and brings its own browser.
//
// **Lanes (the spec pool).** Until 2026-09-22 this ran `for (const f of files)` against ONE shared
// backend: 12 specs, strictly one browser at a time on a 28-core box — a quarter of the merge gate.
// The loop is now a bounded pool of `HK_E2E_CONCURRENCY` lanes (default 2), and **each lane gets its
// own `hk serve` on its own port**, not a share of one. Measured on a quiet box, same tree, same
// day: **695.3 s sequential -> 207.7 s at 3 lanes**, 11 of 12 specs passing either way (the one red,
// `surface-address`, fails alone too and is nothing to do with this).
//
// Per-lane backends rather than one shared one, deliberately: `/api/tiles`' backpressure cap
// (`cost.in_flight_limit`) is counted PER SERVER, so N browsers against one server would make each
// other's reads get refused — `surface-contention` asserts on exactly that cap, and `live-edge` and
// `surface-nav` read it — and the inter-file `tileCost` drain below exists precisely because one
// browser's leftovers already spoiled the next file's first request. With a backend per lane a spec
// sees exactly what it saw sequentially: one server, one browser, no stranger's history. The only
// thing lanes share is the machine.
//
// Ports: lane `i` gets its own base (`lanePortBase`), handed down as `HK_E2E_PORT` so that the four
// specs which start a SECOND backend of their own (canvas-journey, fog-of-war, scan-everything,
// surface-retune) also allocate inside their lane's range. `backend.mjs`'s `freePort` then steps
// within that range, and the ranges do not overlap — two lanes can never race for one port.
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
//
// **Cleanup must not depend on how THIS process dies (T-740).** The timeout above, and the
// SIGINT/SIGTERM handlers below, both call `killSpecTree` — but a run ended any OTHER way (a
// coordinator stopping it, a gate timeout, a SIGKILL from a parent process) used to exit with no
// sweep at all, and Chrome does not exit when its parent does (that is exactly why `cdp.mjs` gives it
// its own process group). Measured: ten orphaned Chrome processes survived for three hours after one
// such kill, contending for the machine the whole time. SIGINT and SIGTERM are trappable, so those
// two sweep synchronously, same as the timeout. **SIGKILL of run.mjs itself cannot be trapped by
// anything in this file** — Node offers no hook for it — so that one case is handled differently and
// explicitly, not left as a silent gap: every run records its own pid, a MARKER and a process START
// TIME for every child it has spawned so far (across every lane), in a lock file (`LOCK_DIR`,
// `persistLock` below), and the FIRST thing the NEXT run does, before starting a browser or a backend
// of its own, is `sweepStaleRuns()` — read every lock file, and for any whose recorded pid is no
// longer alive, kill every RECORDED CHILD ONLY AFTER RE-VERIFYING IT AGAINST THE OS (`verifyStillOurs`
// below) that its pid still runs the same process, not a stranger that has since reused the number. A
// killed run's orphans therefore survive until the next invocation of this file, and are swept then;
// see `ui/e2e/selftest-orphan-sweep.mjs`, which proves the SIGINT/SIGTERM legs clean synchronously and
// the SIGKILL leg cleans at the next run's start.
//
// **Why the verify step exists (review finding, T-740 fix round 1).** A bare recorded pid is not
// enough: pids wrap (macOS: ~99,998) and this box runs cargo/rustc for up to four agents at once, so
// reuse of a low pid is plausible, especially across a long gap (a lock left by a run that died and
// was never cleaned, found much later). Killing on the pid number ALONE would then SIGKILL whatever
// unrelated process — another agent's Chrome, the merge runner, a tmux shell — now holds that number,
// plus its whole tree and group. `verifyStillOurs` re-reads the CURRENT process's start time
// (`ps -o lstart=`, recorded at `trackChild` time and compared exactly) and command line (must still
// contain the marker recorded at `trackChild` time — the child's own `hk-e2e-chrome-*` /
// `hk-e2e-data-*` profile/data dir, or its spec's file path) before every kill in `sweepStaleRuns`;
// anything that does not match BOTH is left alone, exactly as the ticket's own notes require ("a
// blanket sweep is not the fix here" — the agent that found the leak also checked, rather than
// assumed, that 14 other Chrome processes belonged to a different agent's live worktree).
import { mkdirSync, readFileSync, readdirSync, unlinkSync, writeFileSync } from "node:fs";
import { execFileSync, spawn } from "node:child_process";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { startBackend, assertRealCsp, tileCost } from "./backend.mjs";
import { findChrome } from "./cdp.mjs";
import { waitForSurfaceHistory } from "./harness.mjs";

const HERE = path.dirname(fileURLToPath(import.meta.url));

// One JSON file per run, `{pid, startedAt, children: [{pid, marker, lstart}]}` — the record
// `sweepStaleRuns()` reads at the START of the NEXT run to find what a SIGKILLed run left behind.
// Lives in the OS tmp dir, not `ui/e2e/`, so it survives independent of this checkout and is
// naturally per-machine. A lock older than this is deleted outright rather than acted on — by then
// any recorded child is either long gone or, if its pid number has been reused, definitely not ours;
// `verifyStillOurs` below would reach the same answer, this just skips the `ps` calls to get there.
const LOCK_DIR = path.join(os.tmpdir(), "hk-e2e-runs");
const STALE_LOCK_MAX_AGE_MS = 24 * 60 * 60 * 1000;
try { mkdirSync(LOCK_DIR, { recursive: true }); } catch { /* already exists */ }
const LOCK_FILE = path.join(LOCK_DIR, `${process.pid}.json`);
// pid -> { marker, lstart }. `marker` is a substring that MUST appear in that pid's own command line
// (its `hk-e2e-chrome-*` / `hk-e2e-data-*` dir, or its spec's file path) and `lstart` is that
// process's OS-reported start time at the moment it was tracked — together, what `verifyStillOurs`
// checks before a later run is allowed to kill it.
const trackedChildren = new Map();

/** Rewrite this run's own lock file with its current child set. Best-effort: a failed write only
 * means THIS run's SIGKILL case would go unswept next time, never a correctness problem for the run
 * in progress. */
function persistLock() {
  try {
    const children = [...trackedChildren].map(([pid, info]) => ({ pid, marker: info.marker, lstart: info.lstart }));
    writeFileSync(LOCK_FILE, JSON.stringify({ pid: process.pid, startedAt: Date.now(), children }));
  } catch { /* best effort */ }
}

/** The OS's own record of when `pid` started (`ps -o lstart=`), or `null` if `pid` does not exist (or
 * `ps` itself is unavailable) right now. Exact-string-compared later, never parsed into a Date: the
 * only question that matters is "is this still the SAME process", and the OS's own formatting of its
 * own field answers that without this file taking on a timezone/locale parsing bug of its own. */
function processStartTime(pid) {
  try {
    const out = execFileSync("ps", ["-o", "lstart=", "-p", String(pid)], { encoding: "utf8" }).trim();
    return out || null;
  } catch {
    return null; // no such pid, or ps missing
  }
}

/** `pid`'s own full command line right now (`-ww`: never truncated to a terminal width, which
 * matters here — Chrome's real command line is hundreds of characters and the marker this checks for
 * can sit well past 80 columns), or `""` if `pid` does not exist. */
function processCommand(pid) {
  try {
    return execFileSync("ps", ["-ww", "-o", "command=", "-p", String(pid)], { encoding: "utf8" });
  } catch {
    return "";
  }
}

/**
 * Record `pid` as this run's own child, with a MARKER that must appear in its command line and its
 * OS-reported start time — both read from the live process right now, at the moment it is known to be
 * ours, so a LATER run has something to check a same-numbered stranger against.
 */
function trackChild(pid, marker) {
  trackedChildren.set(pid, { marker, lstart: processStartTime(pid) });
  persistLock();
}
function untrackChild(pid) { trackedChildren.delete(pid); persistLock(); }
function clearLock() { try { unlinkSync(LOCK_FILE); } catch { /* already gone, or never written */ } }

/**
 * Is the CURRENTLY-RUNNING process at `pid` still the one `trackChild` recorded? `"match"` only if
 * BOTH the OS start time is byte-identical to what was recorded (pid reuse virtually always changes
 * it — two processes starting in the same clock tick and getting the same recycled pid is the
 * coincidence this alone would have to fail on) AND the current command line still contains the
 * marker recorded at track time. `"gone"` (pid no longer exists) and `"reused"`/`"mismatch"` (it
 * exists but fails one of the two checks) are both refusals to kill — this function has exactly one
 * job, and getting it wrong in the kill direction is the bug the review that required it was filed
 * over.
 */
function verifyStillOurs(pid, marker, lstart) {
  const curLstart = processStartTime(pid);
  if (curLstart === null) return "gone";
  if (curLstart !== lstart) return "reused";
  if (!processCommand(pid).includes(marker)) return "mismatch";
  return "match";
}

/**
 * Sweep every lock file left by a run whose pid is no longer alive — the SIGKILL case, handled here
 * because it is the one case nothing else in this file can catch. A lock file whose pid IS alive
 * belongs to a run genuinely still in flight (this repo runs up to four agents at once) and is never
 * touched. Runs before anything else in this file starts a browser or a backend, so a killed run's
 * orphans never contend with the run doing the sweeping.
 *
 * Every recorded child is re-verified against the live OS (`verifyStillOurs`) immediately before it
 * is killed — see the file-header note on why a bare recorded pid is never enough to act on alone.
 */
function sweepStaleRuns() {
  let names = [];
  try { names = readdirSync(LOCK_DIR); } catch { return; }
  for (const name of names) {
    if (!/^\d+\.json$/.test(name)) continue;
    const file = path.join(LOCK_DIR, name);
    let entry;
    try { entry = JSON.parse(readFileSync(file, "utf8")); } catch { continue; }
    if (typeof entry?.pid !== "number") { try { unlinkSync(file); } catch { /* ignore */ } continue; }
    if (typeof entry.startedAt === "number" && Date.now() - entry.startedAt > STALE_LOCK_MAX_AGE_MS) {
      try { unlinkSync(file); } catch { /* already gone */ }
      continue; // too old to touch — see the constant's own comment above
    }
    let alive = true;
    try { process.kill(entry.pid, 0); } catch { alive = false; }
    if (alive) continue; // a concurrent run genuinely still in flight — leave it alone
    for (const child of entry.children ?? []) {
      if (!child || typeof child.pid !== "number" || typeof child.marker !== "string" || typeof child.lstart !== "string") {
        continue; // an entry this run cannot verify identity for — fail closed, never kill on a guess
      }
      const verdict = verifyStillOurs(child.pid, child.marker, child.lstart);
      if (verdict !== "match") {
        if (verdict !== "gone") {
          console.log(`e2e: NOT sweeping pid ${child.pid} (marker "${child.marker}") recorded by a killed ` +
            `run (pid ${entry.pid}) — verify said "${verdict}", so it is no longer (or never was) this ` +
            "harness's process; leaving it alone.");
        }
        continue;
      }
      const victims = killSpecTree(child.pid);
      console.log(`e2e: swept ${victims.length} orphaned process(es) for verified pid ${child.pid} ` +
        `(marker "${child.marker}") left by a killed run (pid ${entry.pid}): [${victims.join(", ")}]`);
    }
    try { unlinkSync(file); } catch { /* already gone */ }
  }
}
sweepStaleRuns();
persistLock(); // publish this run's own pid immediately, so a run killed before spawning anything is
                // still visible to the NEXT run's sweep (with an empty children list — nothing to kill).
// A clean lock file means the NEXT run's sweep has nothing to do for THIS run — covers every exit
// this process can observe (normal completion, an uncaught throw, `process.exit` from anywhere,
// SIGINT/SIGTERM below); only a SIGKILL of this process skips this handler, which is exactly the
// case `sweepStaleRuns()` exists for.
process.on("exit", clearLock);

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
// `ui/e2e/quarantine.json` (user, 2026-09-22, the merge-queue crisis): specs the gate SKIPS, each
// with the reason and the date, printed loudly on every run so a quarantine can never be quiet. A
// spec named explicitly on the command line still runs (that is how it is worked on). Not a
// retry, not a looser assertion: the spec stays in the tree, red, until its entry is removed —
// and the entry is removed in the same commit that fixes it, never before.
let quarantined = [];
try {
  quarantined = JSON.parse(readFileSync(path.join(HERE, "quarantine.json"), "utf8"));
} catch (e) {
  if (e.code !== "ENOENT") throw e;
}
const skipped = only.length === 0 ? quarantined.filter((q) => discovered.includes(q.spec)) : [];
for (const q of skipped) console.log(`e2e: QUARANTINED ${q.spec} since ${q.since} — ${q.reason}`);
const files = [...discovered.filter((f) => !skipped.some((q) => q.spec === f)), ...extraSpecs.map((p) => path.relative(HERE, p))];
if (files.length === 0) { console.error("no e2e files matched"); process.exit(1); }

// Well above the slowest file today (canvas-journey, ~170 s sequential and dearer when three other
// lanes are also driving a browser) so a healthy run never trips it, and far below the 31-minute hang
// that motivated it. Override for a deliberately short deadline in the timeout selftest.
const SPEC_TIMEOUT_MS = Number(process.env.HK_E2E_SPEC_TIMEOUT_MS ?? 600_000);

// How many specs run at once, each in its own lane with its own backend.
//
// **3, measured, not guessed.** The pool's floor is its longest single spec — canvas-journey, ~195 s
// — and at 3 lanes the rest already fit underneath it: 207.7 s at 3 against 196.0 s at 4, an 11 s
// difference for 33 % more browsers on the box. And that extra load is not free: at 4 lanes
// `surface-nav`'s T-472 wheel-bound assertion went red ("4 alt wheels outward did not move the time
// axis"), passed alone in 61 s, and passed again at 3 — a load flake bought for 11 s. So the default
// sits where the wall clock stops improving rather than where the box stops fitting.
// 1 restores the old strictly-sequential behaviour exactly.
// Default 2, not the 3 that measured fastest (208 s vs ~300 s): the first gate at 3 lanes went
// red on three DIFFERENT specs across two runs (app-surface; then surface-nav + canvas-journey),
// each green alone - the specs' readiness waits are still time-based, and a third lane beside
// one bounded worker is enough to trip them. Two lanes keep most of the saving with margin;
// raise it again when the waits read server state (docs/10 §3.6).
const CONCURRENCY = Math.max(1, Number(process.env.HK_E2E_CONCURRENCY ?? 2));

/**
 * Lane `i`'s base port. Lane 0 keeps the historical 8791 so a single-lane run is byte-for-byte the
 * old one; the rest are 32 apart (wider than `freePort`'s 24-port sweep, so lanes cannot collide)
 * and chosen to clear the reserved 8788/8789/8899/8900 block that `backend.mjs` refuses outright.
 */
const PORT_BASE = Number(process.env.HK_E2E_PORT ?? 8791);
const lanePortBase = (i) => (i === 0 ? PORT_BASE : 8951 + (i - 1) * 32);

/**
 * Rough per-spec seconds, measured from `$HACKRIFF_OPS/merge-runner.log` (2026-09-22 medians).
 *
 * A SCHEDULING HINT ONLY: it decides the order specs are handed to lanes (longest first, so the pool
 * does not end with one long spec and three idle lanes), and nothing else. A spec missing from the
 * table gets the default below and runs in the middle of the pack — a new spec is never skipped,
 * mis-run or reported differently for being absent here, so this list going stale costs a little
 * wall clock and nothing else.
 */
const SPEC_SECONDS = {
  "canvas-journey.e2e.mjs": 173, "live-edge.e2e.mjs": 107, "surface-nav.e2e.mjs": 72,
  "surface-colour.e2e.mjs": 56, "fog-of-war.e2e.mjs": 50, "scan-everything.e2e.mjs": 47,
  "app-trace.e2e.mjs": 33, "surface-contention.e2e.mjs": 24, "surface-retune.e2e.mjs": 23,
  "surface-load.e2e.mjs": 13, "surface-address.e2e.mjs": 10, "app-surface.e2e.mjs": 9,
  "surface-region.e2e.mjs": 7,
};
const SPEC_SECONDS_DEFAULT = 20;
const queue = [...files].sort(
  (a, b) => (SPEC_SECONDS[b] ?? SPEC_SECONDS_DEFAULT) - (SPEC_SECONDS[a] ?? SPEC_SECONDS_DEFAULT),
);
const lanes = Math.min(CONCURRENCY, files.length);

// Fail before spending a backend start on a machine that cannot drive a browser. Like `just
// test-ui` (T-358), this FAILS rather than skips: a tier that silently does nothing is worse than
// one that blocks, because the green is silent and unbounded in time.
const chrome = findChrome();

const t0 = Date.now();
// Every exit from here on goes through `stopAll()`. A leaked `hk serve` holds the port and the ring,
// so the next run starts against a stranger's history — and the first version of this file leaked
// one on its first failure.
const backends = [];
const stopAll = () => { for (const b of backends) { try { b.stop(); } catch { /* already gone */ } } };
process.on("exit", stopAll);
// Backends are tracked individually per lane in `startLane` below, each with its own `--data-dir` as
// the marker `verifyStillOurs` checks a later run's sweep against — so a SIGKILL of THIS process
// still lets the next run's sweep find and kill any lane's backend that survives, without risking a
// same-numbered stranger.
//
// Every spec currently running, across every lane — kept only so a lane's timeout can name and race
// one particular child. Signal cleanup (next) does not use it: a SIGINT/SIGTERM to THIS process (a
// developer's Ctrl-C, or the gate's own deadline) sweeps every child `trackChild` currently knows
// about — every lane's backend, every spec process AND the readiness-probe browser each `startLane`
// opens directly (outside any spec) — not just the running specs, because a check that only ever
// looked at `activeSpecs` was blind to exactly that browser.
const activeSpecs = new Set();
for (const sig of ["SIGINT", "SIGTERM"]) {
  process.on(sig, () => {
    for (const pid of [...trackedChildren.keys()]) killSpecTree(pid);
    stopAll();
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
 * **Two gathers before any kill (T-740).** A Chrome that has JUST launched keeps forking helper
 * processes (GPU, network, storage, renderer) for a couple hundred ms — a kill landing mid-startup
 * (exactly what an outside SIGINT/SIGTERM/SIGKILL cannot schedule around) can snapshot the tree
 * before some of them exist. Re-gathering once, a short pause later, catches almost all of that
 * window while `pid` is still alive to be walked from; killing `pid` first would orphan anything
 * forked after, with no ancestor left to find it through.
 *
 * Returns the pids it attempted, for the caller to report — this is the "no orphans" guarantee's own
 * evidence, checkable independently with `pgrep`.
 */
function killSpecTree(pid) {
  const first = descendantPids(pid);
  execFileSync("sleep", ["0.3"]); // synchronous: this function runs on a signal/timeout path with
                                   // nothing else to do meanwhile, and 300 ms is well under any
                                   // caller's own deadline.
  const second = descendantPids(pid);
  const victims = [pid, ...new Set([...first, ...second])];
  for (const p of victims) {
    try { process.kill(-p, "SIGKILL"); } catch { /* not a group leader, or already gone */ }
    try { process.kill(p, "SIGKILL"); } catch { /* already gone */ }
  }
  return victims;
}

/**
 * Run one spec file, racing it against `SPEC_TIMEOUT_MS`. Resolves `{ status, timedOut, out }` —
 * never throws, and never lets a timeout read as `status === 0`.
 *
 * Output is CAPTURED rather than inherited, and printed as one block when the spec finishes. With
 * lanes running concurrently, inherited stdio interleaves four specs' `✔`/`✖` lines into something
 * no one can read or grep; a whole spec's output arriving at once, under a header naming the file,
 * reads exactly like the sequential run did.
 */
function runSpec(file, env) {
  return new Promise((resolve) => {
    const child = spawn(process.execPath, [path.join(HERE, file)], {
      stdio: ["ignore", "pipe", "pipe"], env,
      // Its own process group, isolating it from this runner's — belt-and-braces alongside the
      // PPID-based sweep above: nothing about the kill below depends on this, but it keeps a
      // half-dead spec from ever sharing a signal target with the runner itself.
      detached: true,
    });
    activeSpecs.add(child);
    // Recorded with the spec's own file path as marker (it is a substring of this exact child's
    // command line, `node <HERE>/<file>`) — if THIS process is SIGKILLed mid-spec, the next run's
    // sweep still finds this spec (and, via descendantPids, its Chrome) and can verify it is still
    // this same spec process before killing it.
    trackChild(child.pid, file);
    let out = "";
    child.stdout.on("data", (d) => { out += d; });
    child.stderr.on("data", (d) => { out += d; });
    let timedOut = false;
    const timer = setTimeout(() => {
      timedOut = true;
      out += `\ne2e: TIMEOUT — ${file} exceeded HK_E2E_SPEC_TIMEOUT_MS=${SPEC_TIMEOUT_MS} ms; ` +
        "killing its process tree (its Chrome and any backend it started) and reporting it FAILED\n";
      const victims = killSpecTree(child.pid);
      out += `e2e: killed ${victims.length} process(es) for ${file}: [${victims.join(", ")}]\n`;
    }, SPEC_TIMEOUT_MS);
    let done = false;
    let grace = null;
    const finish = (status) => {
      if (done) return;
      done = true;
      clearTimeout(timer);
      if (grace) clearTimeout(grace);
      activeSpecs.delete(child);
      untrackChild(child.pid);
      resolve({ status: timedOut ? 1 : (status ?? 1), timedOut, out });
    };
    // `close` (both pipes drained) is what completes the output block, but it is NOT guaranteed to
    // arrive: a grandchild that inherited the pipe and outlived the kill holds the write end open
    // forever. So `exit` starts a short grace period and then finishes anyway — a lost tail of
    // output is a cosmetic loss; a runner that never returns is the bug T-473 was written against.
    child.on("close", (code) => finish(code));
    child.on("exit", (code) => { grace = setTimeout(() => finish(code), 2000); });
    child.on("error", () => finish(1));
  });
}

/** Start lane `i`'s backend and get it to the state a spec expects to meet. */
async function startLane(i) {
  const backend = await startBackend({ port: lanePortBase(i) });
  backends.push(backend);
  // Recorded in the lock file with its own `--data-dir` (a fresh `mkdtempSync` per run, so this is
  // unique to THIS backend, not just "some hk serve") as the marker `verifyStillOurs` checks a later
  // run's sweep against — so a SIGKILL of THIS process still lets the next run's sweep find and kill
  // this lane's backend if it survives, without risking a same-numbered stranger.
  trackChild(backend.proc.pid, path.basename(backend.dataDir));
  const csp = await assertRealCsp(backend.origin);
  // The server's own backpressure cap, read HERE rather than from a test: over the cap
  // `/api/tiles` answers 503, so a test process asking for it while its own browser holds four
  // reads in flight gets refused — the harness would have manufactured the very condition it
  // exists to detect.
  // T-740: this call owns a Chrome directly (not through any spec), so it is tracked exactly like
  // the backend and every spec's process — `onSpawn` fires the instant the pid exists, before any of
  // the polling below (or Chrome's own helper-process forking) that could be interrupted by a signal.
  let readinessBrowserPid = null;
  const cov = await waitForSurfaceHistory(backend.origin, backend.token, {
    onSpawn: (pid, profile) => { readinessBrowserPid = pid; trackChild(pid, path.basename(profile)); },
  });
  if (readinessBrowserPid) untrackChild(readinessBrowserPid); // waitForSurfaceHistory's own
                                                                // `finally` already closed it normally
  // AFTER the readiness page, not before, and this order is load-bearing: that page leaves up to
  // `in_flight_limit` tile reads outstanding, and the server counts them until they finish. Since
  // `tileCost` retries a 503, asking for the cap here doubles as the drain — the next page opens
  // against a route that is answering, instead of being refused while it addresses the surface.
  const cost = await tileCost(backend.origin, backend.token);
  return { i, backend, cost, csp, cov };
}

const failed = [];
const times = [];
let tUp = 0;
try {
  const started = await Promise.all(Array.from({ length: lanes }, (_, i) => startLane(i)));
  tUp = Date.now() - t0;
  const lane0 = started[0];
  console.log(`e2e: chrome ${chrome}`);
  console.log(`e2e: ${lanes} lane(s) (HK_E2E_CONCURRENCY), one hk serve each, ready in ${tUp} ms`);
  for (const l of started) console.log(`e2e:   lane ${l.i} — ${l.backend.origin}`);
  console.log(`e2e: CSP under test — ${lane0.csp.replace(/\s+/g, " ").trim()}`);
  console.log(`e2e: /api/tiles cost.in_flight_limit = ${lane0.cost.in_flight_limit}, fair_share = ${lane0.cost.fair_share}`);
  console.log(`e2e: per-spec timeout ${SPEC_TIMEOUT_MS} ms (HK_E2E_SPEC_TIMEOUT_MS)`);
  for (const l of started) {
    console.log(l.cov.ok
      ? `e2e: lane ${l.i}: ${l.cov.text} (after ${l.cov.ms} ms) — pages now open on the observed region`
      : `e2e: WARNING — lane ${l.i}'s surface is not ready after ${l.cov.ms} ms: ${l.cov.reason}\n` +
        "     Running anyway: the guards below are written to diagnose exactly this, and aborting here\n" +
        "     would report a broken page as a broken harness.");
  }

  await Promise.all(started.map(async ({ i, backend, cost }) => {
    for (;;) {
      const f = queue.shift();
      if (f === undefined) return;
      // Drain before every file. A browser that has just been killed can leave up to
      // `in_flight_limit` tile reads outstanding, and `hk serve` counts them until they finish — so
      // the next page's single probe request gets the 503 and the page puts up "The surface could not
      // be addressed". That is a real product finding (see ui/e2e/README.md), but between files it is
      // this harness's mess, and a suite whose second file fails because of its first is a suite
      // nobody trusts. `tileCost` retries a 503, so asking for the cap is the drain.
      await tileCost(backend.origin, backend.token);
      const s = Date.now();
      console.log(`\ne2e: lane ${i} ▶ ${f}`);
      const { status, timedOut, out } = await runSpec(f, {
        ...process.env,
        HK_E2E_ORIGIN: backend.origin, HK_E2E_TOKEN: backend.token, CHROME: chrome,
        // The lane's own port range, for the specs that start a SECOND `hk serve` themselves:
        // `backend.mjs` defaults to `HK_E2E_PORT ?? 8791`, so without this every lane's spec would
        // start probing the same port at the same moment.
        HK_E2E_PORT: String(lanePortBase(i)),
        HK_E2E_TILE_LIMIT: String(cost.in_flight_limit ?? ""),
        // T-630: whether this server divides its slots between clients, or serves whoever asks
        // first. Read from the route itself, so the fair-share spec knows which route it met and a
        // red baseline run (`HK_TILE_FAIR_SHARE=off`) cannot be mistaken for a green one.
        HK_E2E_TILE_FAIR: String(cost.fair_share ?? ""),
      });
      const ms = Date.now() - s;
      process.stdout.write(out);
      console.log(`e2e: lane ${i} ${status === 0 ? "✔" : "✖"} ${f} in ${(ms / 1000).toFixed(1)} s`);
      times.push([f, ms, timedOut]);
      if (status !== 0) failed.push(f);
    }
  }));
} finally {
  stopAll();
}

// **This summary line's shape is parsed** — by `ops/merge-runner.sh` (to re-run just the failed
// specs) and by `ui/e2e/selftest.mjs`. Do not reword it.
const total = Date.now() - t0;
console.log(`\ne2e: ${files.length - failed.length}/${files.length} files passed in ${(total / 1000).toFixed(1)} s` +
  ` (backend ${(tUp / 1000).toFixed(1)} s)${failed.length ? `; failed: ${failed.join(", ")}` : ""}`);
// Slowest first: with lanes, the order specs *finished* in says nothing, but which one is the pool's
// lower bound says everything about what to fix next.
for (const [f, ms, timedOut] of [...times].sort((a, b) => b[1] - a[1])) {
  console.log(`  ${(ms / 1000).toFixed(1)} s  ${f}${timedOut ? "  [TIMED OUT]" : ""}`);
}
process.exit(failed.length ? 1 : 0);
