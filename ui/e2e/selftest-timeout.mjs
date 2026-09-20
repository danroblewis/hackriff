// `npm run e2e:selftest-timeout`: proves the T-473 per-spec timeout actually fires, actually kills
// the hung spec's whole process tree (its Chrome and the extra `hk serve` it started), and actually
// reports the spec red — not a hang, and not a false green.
//
// Companion to `selftest.mjs`, which proves the suite's ASSERTIONS catch known defects. This proves
// its LIVENESS does: it drives the real `run.mjs`, not a reimplementation of the timeout logic,
// against `selftest-fixtures/hang.e2e.mjs` — a spec that launches a Chrome and a backend and then
// hangs forever — via `HK_E2E_EXTRA_SPECS`, the door `run.mjs` opens for exactly this. A short
// `HK_E2E_SPEC_TIMEOUT_MS` keeps this fast; the mechanism under test does not care what the deadline
// number is.
import { execFileSync, spawnSync } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const HANG = path.join(HERE, "selftest-fixtures/hang.e2e.mjs");
const TIMEOUT_MS = 5_000; // short: this run's only job is to prove the mechanism, not do real work
const BUDGET_MS = TIMEOUT_MS + 60_000; // generous margin over the deadline; nowhere near the 31-minute hang

/** How many processes on this WHOLE MACHINE currently match `pattern` — used only as a before/after
 * delta, never as an absolute count, because other agents' e2e runs legitimately have their own
 * `hk-e2e-chrome-*` / `hk-e2e-data-*` processes up at the same time (CLAUDE.md: up to four agents in
 * parallel). A delta of zero across this selftest's own run is what "no orphans" means here. */
function pgrepCount(pattern) {
  try {
    return execFileSync("pgrep", ["-f", pattern], { encoding: "utf8" }).trim().split("\n").filter(Boolean).length;
  } catch {
    return 0; // pgrep exits 1 on no match, or is missing
  }
}
const machineCount = () => pgrepCount("hk-e2e-chrome-") + pgrepCount("hk-e2e-data-");

const before = machineCount();
console.log(`e2e selftest-timeout: ${before} pre-existing hk-e2e-* process(es) on this machine ` +
  "(other agents' runs, if any — expected nonzero on a busy box; only the DELTA after this run matters).");

const t0 = Date.now();
// `__never_matches_any_real_spec__` empties `discovered` in run.mjs, so the only file this run
// drives is the hang fixture added via HK_E2E_EXTRA_SPECS — this selftest's job is to prove the
// timeout mechanism, not to re-run the real suite.
const r = spawnSync(process.execPath, [path.join(HERE, "run.mjs"), "__never_matches_any_real_spec__"], {
  cwd: HERE, encoding: "utf8", timeout: BUDGET_MS + 30_000,
  env: { ...process.env, HK_E2E_SPEC_TIMEOUT_MS: String(TIMEOUT_MS), HK_E2E_EXTRA_SPECS: HANG },
});
const ms = Date.now() - t0;
const out = `${r.stdout ?? ""}${r.stderr ?? ""}`;
console.log(out);

const after = machineCount();

const reportedTimeout = /TIMEOUT/.test(out) && /hang\.e2e\.mjs/.test(out);
const reportedFailed = /failed:.*hang\.e2e\.mjs/.test(out);
const exitedNonZero = r.status !== 0;
const bounded = ms < BUDGET_MS;
const noOrphans = after <= before;

console.log(`\ne2e selftest-timeout: run.mjs took ${ms} ms (budget ${BUDGET_MS} ms), exit ${r.status}`);
console.log(`  reported TIMEOUT naming hang.e2e.mjs: ${reportedTimeout}`);
console.log(`  hang.e2e.mjs listed under "failed:":  ${reportedFailed}`);
console.log(`  run.mjs exit code nonzero:            ${exitedNonZero}`);
console.log(`  bounded by the deadline (not a hang): ${bounded}`);
console.log(`  hk-e2e-* process count before/after:  ${before} / ${after}  (no orphans: ${noOrphans})`);

const ok = reportedTimeout && reportedFailed && exitedNonZero && bounded && noOrphans;
console.log(ok
  ? "\nPASS: the runner killed the hung spec's process tree and reported it red, with no orphans."
  : "\nFAIL: the per-spec timeout does not do what T-473 requires. See the flags above.");
process.exit(ok ? 0 : 1);
