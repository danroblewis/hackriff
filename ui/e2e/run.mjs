// `npm run e2e`: the browser tier (T-455).
//
// Shape deliberately mirrors `ui/test/run.mjs` — discover by filename, one node process per file,
// exit code from the failures — so there is one way this repo runs a UI suite. What it adds is the
// lifecycle the unit tier does not need: **one** `hk serve` over a recorded fixture, started once
// and shared, because starting it per file is most of the runtime.
//
// Every file is handed `HK_E2E_ORIGIN` and `HK_E2E_TOKEN` and brings its own browser.
import { readdirSync } from "node:fs";
import { spawnSync } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { startBackend, assertRealCsp, tileCost } from "./backend.mjs";
import { findChrome } from "./cdp.mjs";
import { waitForSurfaceHistory } from "./harness.mjs";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const only = process.argv.slice(2).filter((a) => !a.startsWith("-"));
const files = readdirSync(HERE)
  .filter((f) => f.endsWith(".e2e.mjs") && (only.length === 0 || only.some((o) => f.includes(o))))
  .sort();
if (files.length === 0) { console.error("no e2e files matched"); process.exit(1); }

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
for (const sig of ["SIGINT", "SIGTERM"]) process.on(sig, () => { backend.stop(); process.exit(130); });

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
    const r = spawnSync(process.execPath, [path.join(HERE, f)], {
      stdio: "inherit",
      env: {
        ...process.env,
        HK_E2E_ORIGIN: backend.origin, HK_E2E_TOKEN: backend.token, CHROME: chrome,
        HK_E2E_TILE_LIMIT: String(cost.in_flight_limit ?? ""),
      },
    });
    const ms = Date.now() - s;
    times.push([f, ms]);
    if (r.status !== 0) failed.push(f);
  }
} finally {
  backend.stop();
}

const total = Date.now() - t0;
console.log(`\ne2e: ${files.length - failed.length}/${files.length} files passed in ${(total / 1000).toFixed(1)} s` +
  ` (backend ${(tUp / 1000).toFixed(1)} s)${failed.length ? `; failed: ${failed.join(", ")}` : ""}`);
for (const [f, ms] of times) console.log(`  ${(ms / 1000).toFixed(1)} s  ${f}`);
process.exit(failed.length ? 1 : 0);
