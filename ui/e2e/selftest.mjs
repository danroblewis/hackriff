// `npm run e2e:selftest`: **put each known defect back and check this suite goes red** (T-455).
//
// A guard that has never failed is a guard nobody has checked. This repo's standing bar is that
// every guard demonstrates non-vacuity, and for a browser tier that is not a formality: the two
// defects it exists for were both invisible to a full green board, so "it passes" says nothing
// about whether it can see them.
//
// How it works, and why it is not a source patch you have to remember to undo: each fault copies
// `ui/src` into `ui/.e2e-selftest/src`, edits the COPY, builds it to `ui/.e2e-selftest/dist/<fault>`
// with the same esbuild command `npm run build:surface` uses, and points `hk serve` at that dist via
// `HK_E2E_UI_DIST`. `ui/src` is never touched, so an interrupted run leaves nothing behind.
//
// Then it runs the real suite (`e2e/run.mjs`) and **expects a non-zero exit**. A fault that comes
// back green is reported as a hole in the tier, which is the finding worth having.
import { cpSync, mkdirSync, readFileSync, rmSync, writeFileSync, existsSync } from "node:fs";
import { spawnSync } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { UI_DIR } from "./backend.mjs";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const WORK = path.join(UI_DIR, ".e2e-selftest");

/**
 * The defects, each written as the smallest edit that reproduces the real one.
 *
 * `expect` names the file whose assertions must break. It is checked, not decorative: a fault that
 * fails the *other* guard is a fault the tier caught by accident, and the point of this run is to
 * know which guard sees which defect.
 */
const FAULTS = [
  {
    name: "t450-csp-eval",
    expect: "surface-load.e2e.mjs",
    what: "T-450: a `new Function` at MODULE SCOPE in cellrule.ts. `hk serve` sends default-src " +
      "'self' with no unsafe-eval, so the module throws while being evaluated and the whole bundle " +
      "never finishes — the page is blank, and no node-hosted unit test can see it.",
    file: "surface/cellrule.ts",
    patch: (src) => src + `

// —— injected by ui/e2e/selftest.mjs; never in the real source ——
const __selftestPredicate = new Function("v", "return v > 0;");
export const __selftestMark = __selftestPredicate(1);
`,
  },
  {
    name: "t454-ignore-the-cap",
    expect: "surface-nav.e2e.mjs",
    what: "T-454: the client stops obeying the tile route's in-flight cap. The server answers 503 " +
      "over cost.in_flight_limit, and the refusal reaches the user — a defect a client with a cap, " +
      "an AbortController per request and measured cancellation still had.",
    file: "surface/tilecache.ts",
    patch: (src) => {
      const from = "this.limit = opts.inFlight ?? 4;";
      if (!src.includes(from)) throw new Error(`selftest: anchor not found in tilecache.ts: ${from}`);
      return src.replace(from, "this.limit = 64; // injected by ui/e2e/selftest.mjs — ignore the server's cap");
    },
  },
];

function build(fault) {
  const root = path.join(WORK, fault.name);
  rmSync(root, { recursive: true, force: true });
  mkdirSync(root, { recursive: true });
  cpSync(path.join(UI_DIR, "src"), path.join(root, "src"), { recursive: true });

  const target = path.join(root, "src", fault.file);
  if (!existsSync(target)) throw new Error(`selftest: ${fault.file} does not exist`);
  const before = readFileSync(target, "utf8");
  const after = fault.patch(before);
  if (after === before) throw new Error(`selftest: ${fault.name} changed nothing — the anchor has moved`);
  writeFileSync(target, after);

  // The same command `npm run build:surface` runs, against the patched copy. Only the surface page
  // is built: that is all the browser tier loads.
  const dist = path.join(root, "dist");
  mkdirSync(dist, { recursive: true });
  const esbuild = path.join(UI_DIR, "node_modules", ".bin", "esbuild");
  const run = (args) => {
    const r = spawnSync(esbuild, args, { cwd: UI_DIR, stdio: "pipe", encoding: "utf8" });
    if (r.status !== 0) throw new Error(`selftest: esbuild failed for ${fault.name}:\n${r.stderr}`);
  };
  run([path.join(root, "src/surface/preview-main.ts"), "--bundle", "--minify",
    "--format=esm", "--target=es2020", `--outfile=${path.join(dist, "surface.js")}`]);
  run([path.join(root, "src/surface/preview.css"), "--bundle", "--minify",
    `--outfile=${path.join(dist, "surface.css")}`]);
  cpSync(path.join(root, "src/surface/preview.html"), path.join(dist, "surface.html"));
  return dist;
}

/** Run the real suite against a dist, and report which files failed. */
function runSuite(dist) {
  const r = spawnSync(process.execPath, [path.join(HERE, "run.mjs")], {
    cwd: UI_DIR, stdio: "pipe", encoding: "utf8",
    env: dist ? { ...process.env, HK_E2E_UI_DIST: dist } : process.env,
  });
  const out = `${r.stdout ?? ""}${r.stderr ?? ""}`;
  return {
    status: r.status, out,
    failedFiles: (out.match(/failed: ([^\n]+)/)?.[1] ?? "").split(", ").filter(Boolean),
  };
}

const only = process.argv.slice(2).filter((a) => !a.startsWith("-"));
const faults = FAULTS.filter((f) => only.length === 0 || only.some((o) => f.name.includes(o)));
if (faults.length === 0) { console.error(`no fault matched; known: ${FAULTS.map((f) => f.name).join(", ")}`); process.exit(1); }

// The baseline, first, because a fault whose guard is ALREADY red on this tree proves nothing about
// the fault: the run would have been red either way. That distinction is the whole value of a
// non-vacuity check, so it is measured rather than assumed.
console.log("=== selftest: baseline (unmodified build) ===");
const tBase = Date.now();
const baseline = runSuite(null);
console.log(baseline.out.split("\n").filter((l) => /^(e2e:|✔|✖|  \d)/.test(l)).join("\n"));
console.log(`-> baseline ${baseline.status === 0 ? "GREEN" : `RED (${baseline.failedFiles.join(", ")})`}` +
  ` in ${((Date.now() - tBase) / 1000).toFixed(1)} s`);

const results = [];
for (const fault of faults) {
  console.log(`\n=== selftest: ${fault.name} ===\n${fault.what}\n`);
  const dist = build(fault);
  const t0 = Date.now();
  const { status, out, failedFiles } = runSuite(dist);
  const alreadyRed = baseline.failedFiles.includes(fault.expect);
  const caught = status !== 0;
  const byTheRightGuard = failedFiles.includes(fault.expect);
  results.push({ fault, caught, byTheRightGuard, alreadyRed, failedFiles, ms: Date.now() - t0 });
  console.log(out.split("\n").filter((l) => /^(e2e:|✔|✖|ℹ|  \d)/.test(l)).join("\n"));
  console.log(caught
    ? `-> RED, as it must be (${failedFiles.join(", ") || "runner aborted"}) in ${((Date.now() - t0) / 1000).toFixed(1)} s`
    : "-> GREEN. THE SUITE CANNOT SEE THIS DEFECT.");
}

rmSync(WORK, { recursive: true, force: true });

console.log("\n=== selftest summary ===");
let bad = 0;
for (const r of results) {
  const ok = r.caught && r.byTheRightGuard;
  const verdict = !ok ? "FAIL" : r.alreadyRed ? "INCONCLUSIVE" : "PASS";
  if (!ok) bad++;
  console.log(`${verdict}  ${r.fault.name}  ` +
    `expected ${r.fault.expect} to fail; failed: ${r.failedFiles.join(", ") || "(none — suite stayed green)"}  ` +
    `[${(r.ms / 1000).toFixed(1)} s]` +
    (r.alreadyRed ? `\n         ${r.fault.expect} is ALREADY red on this tree without the fault, so this run` +
      " does not prove the guard sees it. Re-run once that guard is green." : ""));
}
if (bad) console.log(`\n${bad} fault(s) the browser tier does not catch, or catches with the wrong guard.`);
process.exit(bad ? 1 : 0);
