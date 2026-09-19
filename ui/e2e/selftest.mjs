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
      const from = "this.ceiling = this.limit = Math.max(1, opts.inFlight ?? 4);";
      if (!src.includes(from)) throw new Error(`selftest: anchor not found in tilecache.ts: ${from}`);
      return src.replace(from,
        "this.ceiling = this.limit = 64; // injected by ui/e2e/selftest.mjs — ignore the server's cap");
    },
  },
  {
    // The other half of T-454: a client that backs off on a refusal but never releases the slots it
    // walked away from asks the route for slots it is still using. This is the pre-T-454 behaviour,
    // and the guard catches it at the **cap** assertion (measured: peak 5 against a cap of 4), not
    // at the steady-state one — which is worth saying plainly, because an earlier draft of this
    // entry claimed the opposite and the run disagreed.
    name: "t454-forget-abandoned-slots",
    expect: "surface-nav.e2e.mjs",
    what: "T-454, the other half: abandoned requests stop being charged to the budget, so a client " +
      "that aborts on every viewport change asks the route for slots it is still using. Refusals " +
      "then continue indefinitely rather than stopping once the controller has converged.",
    file: "surface/tilecache.ts",
    patch: (src) => {
      const from = "return this.abandonedUntil.length;";
      if (!src.includes(from)) throw new Error(`selftest: anchor not found in tilecache.ts: ${from}`);
      return src.replace(from, "return 0; // injected by ui/e2e/selftest.mjs — forget abandoned slots");
    },
  },
  {
    // The fault that exercises `surface-nav`'s STEADY-STATE bound specifically, so that assertion
    // is not one no fault reproduces. The client still counts refusals but never backs off, so it
    // sits at the ceiling permanently and is refused for as long as it keeps rendering — the
    // difference between backpressure as a *probe* and backpressure as a *regime*, which is the
    // whole reason the bound is "none after convergence" rather than "none at all".
    name: "t454-never-back-off",
    expect: "surface-nav.e2e.mjs",
    what: "T-454's controller removed: the client notices the 503 but does not halve its operating " +
      "cap, so it never finds its share and keeps being refused indefinitely, including long after " +
      "the user has stopped touching the view.",
    file: "surface/tilecache.ts",
    patch: (src) => {
      const from = "this.limit = Math.max(1, Math.min(Math.floor(this.limit / 2), this.ceiling));";
      if (!src.includes(from)) throw new Error(`selftest: anchor not found in tilecache.ts: ${from}`);
      return src.replace(from, "/* injected by ui/e2e/selftest.mjs — never back off */");
    },
  },
  {
    // T-472, and the smallest edit that is it: take the uniform branch out of `SurfacePreview.wheel`
    // and the two axes go back to taking the same factor and each clamping alone — which is exactly
    // T-456 as shipped, not a mutant. The guard sees it as a plain wheel that keeps widening
    // frequency long after the time axis has stopped.
    name: "t472-per-axis-uniform-zoom",
    expect: "surface-nav.e2e.mjs",
    what: "T-472: a plain wheel hands the same factor to both axes and lets each clamp on its own, " +
      "so past the end of the record time pins while frequency keeps scaling — the aspect ratio " +
      "drifts, the view jumps, and the user has to shift-scroll the frequency axis back every time.",
    file: "surface/preview.ts",
    patch: (src) => {
      const from = "    if (axes.freq && axes.time) { this.view.panes.zoomBoth(id, factor, fx, ty); return; }\n";
      if (!src.includes(from)) throw new Error("selftest: anchor not found in preview.ts: zoomBoth");
      return src.replace(from, "    // injected by ui/e2e/selftest.mjs — no aspect lock; each axis clamps alone\n");
    },
  },
  {
    // T-486 exactly as the user reported it, twice, and the smallest edit that is it: take the
    // commit point out of the pointer-up handler. Every threshold in `panes.ts` survives — the dead
    // zone, the hysteresis, the one-way door, and all their unit tests — and none of it is ever
    // consulted, because nothing ends a gesture. `panTime` freezes on the first pixel and the pane
    // stays frozen, which is the pre-T-486 behaviour verbatim. A policy nothing invokes, again.
    name: "t486-no-commit-on-release",
    expect: "surface-nav.e2e.mjs",
    what: "T-486: the follow/pause transition has no dead zone, so a 1 px time-pan drops the pane " +
      "out of live and a drag released at the live edge stays paused a few rows short of it.",
    file: "surface/input.ts",
    patch: (src) => {
      const from = "    settle(d);\n";
      if (!src.includes(from)) throw new Error("selftest: anchor not found in input.ts: settle(d)");
      return src.replace(from, "    // injected by ui/e2e/selftest.mjs — a release commits nothing\n");
    },
  },
  {
    // The bootstrap half, found by this tier before T-454 landed: `probeSurface` makes one
    // `/api/tiles` call to learn the lattice, and treating its `503` as fatal meant one busy tab
    // could stop a second one from opening at all. Removing the retry restores that.
    name: "t454-probe-gives-up-on-503",
    expect: "surface-contention.e2e.mjs",
    what: "T-454's bootstrap half: the surface probe stops retrying the tile route's 503, so a " +
      "second tab opened while the first is demanding tiles shows \"The surface could not be " +
      "addressed\" instead of waiting the refusal out.",
    file: "surface/preview.ts",
    patch: (src) => {
      const from = "const retries = bp.retries ?? 5;";
      if (!src.includes(from)) throw new Error(`selftest: anchor not found in preview.ts: ${from}`);
      return src.replace(from, "const retries = 0; // injected by ui/e2e/selftest.mjs — give up on the first 503");
    },
  },
  {
    // T-460 as the user reported it. The refresh POLICY stays in `tilecache.ts` and every unit test
    // of it still passes — only the CALL is removed. That is the defect's real shape: a policy
    // nothing invokes, which is T-450 one layer up, and it is why the fault is injected here rather
    // than inside the cache.
    name: "t460-frozen-live-edge",
    expect: "live-edge.e2e.mjs",
    what: "T-460: nothing tells the cache the live edge moved, so a resident live-edge tile is " +
      "served from cache until the pane scrolls into a new address — once every 256 s at level_t 0. " +
      "The rows are recorded and served; the client never asks for them.",
    file: "surface/preview.ts",
    patch: (src) => {
      const from = "if (this.edgeFn) this.refreshLiveEdge(this.lastFrame);";
      if (!src.includes(from)) throw new Error(`selftest: anchor not found in preview.ts: ${from}`);
      return src.replace(from, "// injected by ui/e2e/selftest.mjs — never refresh the live edge");
    },
  },
  {
    // The T-397 axis-divergence family, stated for the trace: the numbers are right and the picture
    // is placed wrong, so the readout and the pixels describe different things. Injected at exactly
    // the line `trace.ts` says is load-bearing — *"a trace placed by arithmetic of its own would
    // drift from the column it describes the moment either side changed, and nobody would see it
    // until it mattered"* — by adding an offset to the frequency each column is placed at.
    //
    // As a fraction of the pane's own span, not a fixed number of hertz, so the fault is the same
    // number of screen columns whatever viewport the page opens on: 5 % of the width, against a
    // tolerance of one pooled column plus a stroke.
    //
    // **Aimed at the render-path assertion on purpose** (T-487). The obvious alternative — shifting
    // the frequency the READOUT states — also turns the file red, but it is caught by the *data
    // path* check above it, which compares the stated peak against the delivered row. That would be
    // a demonstration of the wrong assertion. Moving the picture leaves the readout truthful about
    // the socket, so the first thing to fail is the check that the pixels agree with it.
    name: "t487-trace-drawn-in-the-wrong-column",
    expect: "app-trace.e2e.mjs",
    what: "T-397/T-487: the trace's columns are placed 5 % of the viewport away from the frequency " +
      "they carry, so the readout is right about the frame and the picture beneath it is of " +
      "somewhere else — a divergence every arithmetic test of the trace still passes.",
    file: "surface/trace.ts",
    patch: (src) => {
      const from = "      { f0Hz: box.f0Hz + c * colHz, f1Hz: box.f0Hz + (c + 1) * colHz, t0Ns: box.t0Ns, t1Ns: box.t1Ns },";
      if (!src.includes(from)) throw new Error(`selftest: anchor not found in surface/trace.ts: ${from}`);
      return src.replace(from,
        "      // injected by ui/e2e/selftest.mjs — place each column 5 % of the span from its own frequency\n" +
        "      { f0Hz: box.f0Hz + c * colHz + span * 0.05, f1Hz: box.f0Hz + (c + 1) * colHz + span * 0.05," +
        " t0Ns: box.t0Ns, t1Ns: box.t1Ns },");
    },
  },
  {
    // T-479, restored as the inverted predicate rather than as a removed line: `retryable` goes back
    // to "everything the server said is worth asking again", which is exactly the default the real
    // defect had.
    name: "t479-retry-a-4xx-forever",
    expect: "live-edge.e2e.mjs",
    what: "T-479: a non-503 refusal is not terminal, so a place the route answered 400 for is " +
      "re-asked on every frame, forever — the console flood the user watched.",
    file: "surface/tilecache.ts",
    patch: (src) => {
      const from = "  return typeof status !== \"number\";";
      if (!src.includes(from)) throw new Error(`selftest: anchor not found in tilecache.ts: ${from}`);
      return src.replace(from, "  return true; // injected by ui/e2e/selftest.mjs — everything is retryable");
    },
  },
  {
    // T-466: `surface-region.e2e.mjs` and `app-surface.e2e.mjs` are the first specs that drive the
    // APP (`/`), not `/surface.html` — and until now `build()` compiled only the surface bundle, so
    // a fault meant for either one failed every app-tier spec at once (the page never mounted at
    // all) rather than the guard named for it. Per-guard attribution for these two was therefore
    // never actually measured. Now that `build()` also compiles the app bundle (T-460's fix,
    // 6f4c7052), the two faults below restore two of the three `surface-region.e2e.mjs`'s own header
    // comment (T-458) recorded as tested BY HAND against `ui/src` directly and never captured here
    // as a standing fault: the browser tier's non-vacuity claim about this file rested on a manual
    // run that nothing re-checks. (The header's third fault is addressed, not reproduced, just
    // below — it no longer measures true against this tree.)
    //
    // `dragIntent` first: the shift modifier is read but never trusted, so every stroke is a pan and
    // shift+drag behaves exactly like a plain drag. T-458 measured this as reddening tests 1, 3 and
    // 4 of `surface-region.e2e.mjs`. It cannot touch `app-surface.e2e.mjs`'s own drag test, which
    // never holds shift — `dragIntent(e)` returns "pan" for that stroke either way.
    name: "t458-region-modifier-ignored",
    expect: "surface-region.e2e.mjs",
    what: "T-458: `dragIntent` stops reading the shift modifier, so shift+drag pans the view instead " +
      "of marking out a region — the gesture T-458 added is unreachable.",
    file: "surface/preview.ts",
    patch: (src) => {
      const from = "  return e.shiftKey === true ? \"region\" : \"pan\";";
      if (!src.includes(from)) throw new Error(`selftest: anchor not found in preview.ts: ${from}`);
      return src.replace(from, "  return \"pan\"; // injected by ui/e2e/selftest.mjs — the modifier is never read");
    },
  },
  {
    // The second of T-458's three: the modifier is still read correctly and a region is still
    // tracked, but the early `return` that keeps a region stroke from also panning is gone, so the
    // pane's window drags right along under the rectangle being drawn. T-458 measured this as tests
    // 1 and 3. `app-surface.e2e.mjs`'s drag test holds no modifier, so `dragging.region` is never set
    // for it and this branch is never entered — the fault cannot reach that file.
    name: "t458-region-falls-through-and-pans",
    expect: "surface-region.e2e.mjs",
    what: "T-458: a region stroke's early return is removed, so marking out a region also pans the " +
      "view under the rectangle being drawn — the load-bearing negative the file's test 1 checks.",
    file: "surface/input.ts",
    patch: (src) => {
      const from =
        "      dragging.region = { ...dragging.region, b: point(e) };\n" +
        "      opts.onRegionDrag?.(dragging.region);\n" +
        "      return; // a region stroke is not a pan: the view must not move under the rectangle\n";
      if (!src.includes(from)) throw new Error(`selftest: anchor not found in input.ts: ${from}`);
      return src.replace(from,
        "      dragging.region = { ...dragging.region, b: point(e) };\n" +
        "      opts.onRegionDrag?.(dragging.region);\n" +
        "      // injected by ui/e2e/selftest.mjs — a region stroke falls through and pans too\n");
    },
  },
  // T-458's THIRD fault — the tap gate (`far`) hard-wired true — is deliberately NOT here. It was
  // measured (build a scratch copy, patch `input.ts`'s `far` to always be `true`, rebuild, run
  // `surface-region.e2e.mjs` alone) and came back GREEN, twice, against this tree: test 3's 2 px
  // shift-tap still marks out nothing, because `commitRegion` (`app/explore/region.ts`) has its OWN
  // independent gate, `regionIsReal` (`f1Hz > f0Hz && t1Ns > t0Ns`), and at this test's zoom level a
  // 2-device-px stroke apparently still degenerates there even once the pointer-side gate is
  // disabled. `surface-region.e2e.mjs`'s own header comment states this fault reddens test 3 "alone"
  // (recorded when T-458 landed); that no longer measures true here, whether because the domain gate
  // was added or tightened since, or the header's original claim was never quite right. Recorded
  // rather than silently dropped: a FAULTS entry that comes back green is a hole, and shipping one
  // that is already known not to redden anything would be exactly the vacuity this file exists to
  // rule out.
  {
    // `app-surface.e2e.mjs` is the cutover's own guard (T-445): the retired widgets — `timenav`,
    // `freqnav`, `live`, `axis` — must be GONE from the page, not merely unmounted. A leftover slot
    // sitting in `index.html`, dead HTML nobody mounts into any more, is exactly the incomplete
    // retirement T-445 was written to rule out — the widget itself is gone, but its hook survives.
    //
    // The first attempt at this fault dropped `data-slot="outputs"` instead: `dom.ts`'s `slot()`
    // THROWS on a missing slot (by design — "a layout bug"), so that patch crashed the app's whole
    // eager mount sequence and reddened every app-tier spec, reproducing the exact defect T-466
    // exists to fix rather than a fault this file alone catches. `querySelector` returns the FIRST
    // match, so an EXTRA slot never throws — mounting proceeds untouched, and only the one assertion
    // that counts `[data-slot="timenav"]` sees it.
    name: "t445-app-retired-slot-left-behind",
    expect: "app-surface.e2e.mjs",
    what: "T-445: a `data-slot=\"timenav\"` element is left in `index.html` after the widget it named " +
      "was retired — the cutover's own check that a gone widget stays gone.",
    file: "app/index.html",
    patch: (src) => {
      const from = '      <div class="capture" data-slot="capture"></div>\n';
      if (!src.includes(from)) throw new Error(`selftest: anchor not found in index.html: ${from}`);
      return src.replace(from,
        from + '      <div data-slot="timenav" hidden></div>' +
        ' <!-- injected by ui/e2e/selftest.mjs — a retired slot left behind -->\n');
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
  // **And the app**, because `/` is a different bundle from `/surface.html` — a different esbuild
  // line, with `--splitting`, mounting inside the shell beside a store and a stream socket. Before
  // T-460 this dist held only the preview page, so `hk serve --ui-dist` answered 404 for `/` and
  // every app-tier guard failed for the wrong reason: a fault could be reported as caught when what
  // was caught was the missing page. The two subjects are different, which is the whole point of
  // `app-surface.e2e.mjs` existing beside `surface-load.e2e.mjs`.
  run([path.join(root, "src/app/main.ts"), "--bundle", "--minify", "--splitting", "--format=esm",
    "--target=es2020", `--outdir=${dist}`, "--entry-names=app", "--chunk-names=chunks/[name]-[hash]"]);
  run([path.join(root, "src/app/app.css"), "--bundle", "--minify", `--outfile=${path.join(dist, "app.css")}`]);
  run([path.join(root, "src/audio-worklet.ts"), "--bundle", "--minify", "--target=es2020",
    `--outfile=${path.join(dist, "audio-worklet.js")}`]);
  cpSync(path.join(root, "src/app/index.html"), path.join(dist, "index.html"));
  cpSync(path.join(root, "src/app/index.html"), path.join(dist, "app.html"));
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
