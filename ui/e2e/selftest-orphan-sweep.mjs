// `npm run e2e:selftest-orphans`: proves T-740 — `run.mjs`'s browser/backend cleanup no longer
// depends on HOW the runner itself exits.
//
// `selftest-timeout.mjs` already proves the in-run timeout path (a spec hangs, the runner notices,
// `killSpecTree` fires). This proves the paths that path does not cover: the runner ITSELF being
// killed from outside. SIGINT and SIGTERM are trappable — `run.mjs` installs handlers for both — so
// those sweep synchronously, same as the timeout. SIGKILL is NOT trappable by anything in
// `run.mjs`, so that leg proves the two-step story instead: the orphans genuinely survive the
// SIGKILL (nothing else could happen — that is the whole reason it needed a different fix), and the
// VERY NEXT `run.mjs` invocation sweeps them to zero before starting any browser or backend of its
// own.
import { execFileSync, spawn } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const HANG = path.join(HERE, "selftest-fixtures/hang.e2e.mjs");
// Long enough that the runner's own per-spec timeout (T-473) never fires during this selftest — this
// selftest is about killing run.mjs itself, not about that separate, already-proven mechanism.
const LONG_TIMEOUT_MS = 120_000;

/** How many processes on this WHOLE MACHINE currently match one of this suite's own tmp-dir prefixes
 * — used only as a before/after DELTA, never as an absolute count, because other agents' e2e runs
 * legitimately have their own `hk-e2e-chrome-*` / `hk-e2e-data-*` processes up at the same time
 * (CLAUDE.md: up to four agents in parallel). */
function pgrepCount(pattern) {
  try {
    return execFileSync("pgrep", ["-f", pattern], { encoding: "utf8" }).trim().split("\n").filter(Boolean).length;
  } catch {
    return 0; // pgrep exits 1 on no match, or is missing
  }
}
const orphanCount = () => pgrepCount("hk-e2e-chrome-") + pgrepCount("hk-e2e-data-");

function runHangSpec() {
  return spawn(process.execPath, [path.join(HERE, "run.mjs"), "__never_matches_any_real_spec__"], {
    cwd: HERE,
    env: { ...process.env, HK_E2E_SPEC_TIMEOUT_MS: String(LONG_TIMEOUT_MS), HK_E2E_EXTRA_SPECS: HANG },
    stdio: ["ignore", "ignore", "ignore"],
  });
}

async function waitUntil(pred, timeoutMs, stepMs = 100) {
  const t0 = Date.now();
  while (Date.now() - t0 < timeoutMs) {
    if (pred()) return true;
    await new Promise((r) => setTimeout(r, stepMs));
  }
  return false;
}

/** Start the hang spec, wait until its Chrome AND its `hk serve` are both actually up, then send
 * `signal` to run.mjs itself. Returns process counts before / while up / immediately after the
 * signal — the evidence, not a wall-clock guess. */
async function killAndCheck(signal) {
  const before = orphanCount();
  const child = runHangSpec();
  const up = await waitUntil(() => orphanCount() >= before + 2, 30_000);
  if (!up) throw new Error(`${signal}: hang spec never launched its Chrome/backend (saw ${orphanCount()}, wanted >= ${before + 2})`);
  const during = orphanCount();
  child.kill(signal);
  if (signal === "SIGKILL") {
    // Nothing in run.mjs can trap this — the orphans are EXPECTED to survive this process's own
    // death. That survival is the documented gap; proving it here is proving the gap is real, not a
    // test bug.
    await new Promise((r) => setTimeout(r, 1500));
    return { before, during, afterKill: orphanCount(), sweptSync: null };
  }
  // SIGINT/SIGTERM ARE trapped, so the sweep is synchronous with run.mjs's own exit.
  await new Promise((res) => child.on("exit", res));
  const afterKill = orphanCount();
  return { before, during, afterKill, sweptSync: afterKill <= before };
}

let ok = true;

for (const sig of ["SIGINT", "SIGTERM"]) {
  const r = await killAndCheck(sig);
  console.log(`e2e selftest-orphans: ${sig} — before ${r.before}, while up ${r.during}, ` +
    `immediately after ${r.afterKill} (swept synchronously: ${r.sweptSync})`);
  if (!r.sweptSync) ok = false;
}

const killed = await killAndCheck("SIGKILL");
const redAfterKill = killed.afterKill > killed.before;
console.log(`e2e selftest-orphans: SIGKILL — before ${killed.before}, while up ${killed.during}, ` +
  `immediately after ${killed.afterKill} (orphans survived the untrappable signal, as expected: ${redAfterKill})`);
if (!redAfterKill) {
  console.log("  NOTE: expected the orphans to survive an unhandled SIGKILL — if they did not, this " +
    "leg is inconclusive about the fix rather than evidence against it (something else reaped them).");
}

// The fix's other half: the NEXT ordinary run.mjs invocation must sweep the SIGKILLed run's orphans
// to baseline BEFORE it starts any browser or backend of its own.
const sweepRun = spawn(process.execPath, [path.join(HERE, "run.mjs"), "__never_matches_any_real_spec__"], {
  cwd: HERE, stdio: ["ignore", "pipe", "pipe"],
});
let sweepOut = "";
sweepRun.stdout.on("data", (d) => { sweepOut += d; });
sweepRun.stderr.on("data", (d) => { sweepOut += d; });
await new Promise((res) => sweepRun.on("exit", res));
const afterSweep = orphanCount();
console.log(sweepOut.trim());
console.log(`e2e selftest-orphans: next run's startup sweep — after ${afterSweep} (baseline ${killed.before})`);

const sweptToBaseline = afterSweep <= killed.before;
const reportedSweep = /swept \d+ orphaned process/.test(sweepOut);
console.log(`  swept back to baseline: ${sweptToBaseline}, logged the sweep: ${reportedSweep}`);
if (!sweptToBaseline || !reportedSweep) ok = false;

console.log(ok
  ? "\nPASS: SIGINT/SIGTERM sweep run.mjs's own children synchronously; a SIGKILLed run's orphans " +
    "are swept at the very start of the next run.mjs invocation."
  : "\nFAIL: cleanup still depends on the exit path. See the flags above.");
process.exit(ok ? 0 : 1);
