// **The cutover itself** (T-445, docs/16 §8.5): the two bespoke edge scrubbers and the separate
// history view are gone, and each defect that lived in the seam between two renderers is
// **unreachable**, not merely absent.
//
// The distinction matters, and it is the whole reason this file exists rather than a note in the
// planning log. "Absent" is what you get by deleting the code that had the bug; the bug comes back
// with the next surface that needs a waterfall. "Unreachable" is a claim about structure: there is
// one renderer, one ramp, one time mapping, one wheel, and the second implementation that each
// defect needed cannot be written without failing a test here.
//
// The five, from §8.5:
//
//  | defect | what it needed | why it cannot happen now |
//  |---|---|---|
//  | T-420 sliver-of-data | a row ring whose height was drawn regardless of what was served | there is no row ring; a pane's drawn extent IS its box (`surface-marks.test.ts`) |
//  | T-388 box-jump | a per-poll overlay layout over a per-frame scroll | the marks are computed inside `frame()` from that frame's `PaneView` (`surface-marks.test.ts`) |
//  | T-397/T-411 fill and resolution | a view-side row budget beside a server-side level choice | the pane resolves its own level and the chrome states the level it was DRAWN with |
//  | T-397 axis/colormap divergence | a second colour ramp | the ramp exists in one module, and the overlay program has no ramp at all (below) |
//  | T-412 wheel-zoom mismatch | a second wheel handler | one handler, `surface/input.ts`, for both mounts (below) |
//
// Two of those are proved next door in `ui/test/surface-marks.test.ts`, because the subject there is
// the geometry. What is here is the repo-level half: the files that are gone, the things that are
// singular, and the guards that used to hold the retired widgets and now hold the canvas.

import { test } from "node:test";
import assert from "node:assert/strict";
import { existsSync, readdirSync, readFileSync } from "node:fs";
import { CMAP_GLSL, CMAP_STOPS, cmapBytes } from "../src/cmap";

/** Every `.ts` file under `dir`, as paths relative to ui/ (tests run from there). */
function walk(dir: string): string[] {
  const out: string[] = [];
  for (const e of readdirSync(dir, { withFileTypes: true })) {
    const p = `${dir}/${e.name}`;
    if (e.isDirectory()) out.push(...walk(p));
    else if (e.name.endsWith(".ts")) out.push(p);
  }
  return out;
}

const bare = (f: string) => readFileSync(f, "utf8").replace(/\/\*[\s\S]*?\*\//g, "").replace(/^\s*\/\/.*$/gm, "");

/** Everything the cutover removed, and what each one was. */
const RETIRED = {
  "src/app/centre/navigators.ts": "the two bespoke edge scrubbers (1 528 lines)",
  "src/app/centre/live-spectrum.ts": "the live waterfall mount and its DOM overlay layer",
  "src/app/centre/overlays.ts": "the per-poll overlay geometry (T-388's subject)",
  "src/app/centre/review-render.ts": "the live pane's second render path over GET /api/history (T-420's subject)",
  "src/app/centre/axis-view.ts": "the frequency axis strip",
  "src/app/review/history.ts": "the separate history view, with its own colormap LUT (T-397's subject)",
  "src/waterfall.ts": "the old WebGL waterfall renderer",
  "src/timebox.ts": "its time-box placement",
};

test("the retired surfaces are GONE, and nothing under src/ still reaches for them", () => {
  for (const [f, what] of Object.entries(RETIRED)) {
    assert.ok(!existsSync(f), `${f} (${what}) should have been retired`);
  }
  const stems = Object.keys(RETIRED).map((f) => f.replace(/^src\//, "").replace(/\.ts$/, ""));
  for (const f of [...walk("src"), ...walk("test")]) {
    const src = readFileSync(f, "utf8");
    for (const stem of stems) {
      // An import of a retired module, not a mention of it in prose: these files explain what they
      // replaced, and they should go on doing that.
      const re = new RegExp(`from "[^"]*${stem.replace(/\//g, "\\/")}"`);
      assert.ok(!re.test(src), `${f} still imports the retired ${stem}`);
    }
  }
});

test("the Explore centre is ONE surface: one mount, one canvas, and its CSS is scoped to it", () => {
  const html = readFileSync("src/app/index.html", "utf8");
  assert.match(html, /data-slot="surface"/, "the centre's spectrum region is the surface");
  for (const slot of ["timenav", "freqnav", "live", "axis"]) {
    assert.ok(!html.includes(`data-slot="${slot}"`), `the retired ${slot} slot is still in the page`);
  }
  // The capture band stays: it is the record control and the retained-window overview, not a
  // spectrum renderer, and it writes the SAME time cursor the surface does.
  assert.match(html, /data-slot="capture"/);

  const css = readFileSync("src/app/centre/centre.css", "utf8").replace(/\/\*[\s\S]*?\*\//g, "");
  const selectors = [...css.matchAll(/([^{}@]+)\{[^{}]*\}/g)].map((m) => m[1].trim()).filter((s) => s && !s.startsWith("@"));
  assert.ok(selectors.length > 5);
  for (const sel of selectors) for (const part of sel.split(",")) {
    assert.match(part.trim(), /^\.(surface|sf-[a-z]+)\b/, `unscoped: ${part}`);
  }
  for (const m of css.matchAll(/min-width:\s*(\d+)px/g)) assert.ok(Number(m[1]) <= 400, "responsive to ~400px");
  // The surface owns its own pan and zoom, so the browser must not also scroll or pinch the canvas.
  assert.match(css, /\.sf-canvas \{[\s\S]*?touch-action: none/);
});

// ---------------------------------------------------------------------------
// T-397: ONE colour ramp
// ---------------------------------------------------------------------------

test("T-397: there is ONE colour ramp in the client, it reaches yellow, red and white, and no pass can grow a second", () => {
  // The symptom the user reported: a strip was cyan whatever the value, because its ramp ran
  // dark-teal → cyan and stopped. Cyan is the stop at x ≈ 0.45 of the real ramp.
  const [, cyanG, cyanB] = cmapBytes(0.45);
  assert.ok(cyanG > 150 && cyanB > 180, `0.45 should be cyan: ${cmapBytes(0.45)}`);
  const [yr, yg, yb] = cmapBytes(0.7);
  assert.ok(yr > 200 && yg > 180 && yb < 80, `0.7 should be yellow: ${cmapBytes(0.7)}`);
  const [rr, rg, rb] = cmapBytes(0.9);
  assert.ok(rr > 200 && rg < 90 && rb < 60, `0.9 should be red: ${cmapBytes(0.9)}`);
  assert.deepEqual(cmapBytes(1), [255, 255, 255], "the top of the ramp is white");
  assert.deepEqual(cmapBytes(0), [0, 0, 10], "and the bottom is near-black");
  assert.deepEqual(cmapBytes(9), cmapBytes(1));
  assert.deepEqual(cmapBytes(-9), cmapBytes(0));
  assert.deepEqual(cmapBytes(NaN), cmapBytes(0));
  // The GLSL the surface compiles is GENERATED from the same stops, so shader and canvas cannot
  // drift: every stop's colour appears in it, in order.
  for (const [, c] of CMAP_STOPS) {
    const vec = `vec3(${c.map((v) => (Number.isInteger(v) ? v.toFixed(1) : String(v))).join(",")})`;
    assert.ok(CMAP_GLSL.includes(vec), `${vec} missing from the generated shader ramp`);
  }

  // THE CONTROL, and it is now repo-wide rather than a list of the files that had drifted: exactly
  // ONE module under src/ defines a ramp, and it is `cmap.ts`. The retired history view carried a
  // hand-written LUT with its own stops; a third copy is the quickest fix and the reason two
  // renderings disagreed in the first place.
  const definers = walk("src").filter((f) => f !== "src/cmap.ts" &&
    (/vec3 cmap\(float/.test(readFileSync(f, "utf8")) || readFileSync(f, "utf8").includes("0.05,0.1,0.55")));
  assert.deepEqual(definers, [], "a second colour ramp exists");
  // Two consumers, one source: the data shader and the legend swatches.
  assert.match(readFileSync("src/surface/surface.ts", "utf8"), /CMAP_GLSL/);

  // And the overlay pass — where every box, pane rectangle and lit segment is drawn — is incapable
  // of a ramp: no sampler, no stops, one flat colour uniform. A divergent ramp cannot be introduced
  // there without writing a shader that this assertion sees.
  const overlay = bare("src/surface/overlay.ts");
  assert.ok(!/sampler2D|texture\(/.test(overlay), "the overlay program must have no sampler");
  assert.ok(!/cmap|CMAP|STOPS/.test(overlay), "…and no ramp");
  assert.match(overlay, /uniform vec4 uInk/, "one flat colour is all it can express");
});

// ---------------------------------------------------------------------------
// T-412: ONE wheel
// ---------------------------------------------------------------------------

test("T-412: there is ONE wheel/drag handler, and every mount of the surface goes through it", () => {
  // The mismatch was two widgets each interpreting a wheel their own way — different direction,
  // different factor, different axis — and the disagreement WAS the bug. The cutover gives the
  // surface a second host (the app's Explore centre beside the /surface page), which is exactly the
  // condition that produced the defect, so the handler is shared rather than copied.
  const hosts = ["src/surface/preview-main.ts", "src/app/centre/surface.ts"];
  for (const f of hosts) {
    const src = readFileSync(f, "utf8");
    assert.match(src, /attachSurfaceInput\(/, `${f} must use the shared input handler`);
    assert.ok(!/addEventListener\("wheel"/.test(src), `${f} must not add a wheel handler of its own`);
    assert.ok(!/addEventListener\("pointerdown"/.test(src), `${f} must not add a drag handler of its own`);
  }
  // Repo-wide: the canvas's gesture vocabulary is interpreted in exactly one file.
  const wheelers = walk("src").filter((f) => /addEventListener\("wheel"/.test(readFileSync(f, "utf8")));
  assert.deepEqual(wheelers, ["src/controls/gestures.ts", "src/surface/input.ts"].filter((f) => existsSync(f)),
    "a second wheel interpretation has appeared");
  // The zoom arithmetic itself has one definition, which is what "one wheel" has to mean to be
  // worth anything: the handler calls `zoomFactor`/`wheelAxes` rather than carrying its own.
  const input = readFileSync("src/surface/input.ts", "utf8");
  assert.match(input, /import \{ type SurfacePreview, wheelAxes, zoomFactor \} from ".\/preview"/);
  assert.ok(!/Math\.exp\(/.test(input), "the factor is not recomputed here");
});

// ---------------------------------------------------------------------------
// The guards that used to hold the widgets, re-pointed
// ---------------------------------------------------------------------------

test("T-393/T-386 CLOCK GUARD: no clock of the browser's own reaches the centre modules", () => {
  // T-393 held this over the two navigator modules; the surface inherits it, because the live edge,
  // the pane windows and every box on them are on the CAPTURE clock. A replay or a time-compressed
  // mock scene runs on a clock of its own — the fixture behind T-379 sat 3.5 days from wall time —
  // so a surface that reached for `Date.now()` would window on a range the capture never covered.
  for (const f of ["src/app/centre/view.ts", "src/app/centre/surface.ts", "src/app/centre/live-edge.ts",
    "src/surface/marks.ts", "src/surface/panes.ts", "src/surface/minimap.ts", "src/surface/view.ts"]) {
    const src = bare(f);
    for (const word of ["Date.now", "performance.now", "toLocaleTimeString", "getTimezoneOffset"]) {
      assert.ok(!src.includes(word), `${f} must not contain "${word}"`);
    }
  }
});

test("T-347: pausing is still a VIEW change — no file under src/ names the retired pause routes", () => {
  // T-347 retired `/api/control/pause` because a run-wide boolean cannot represent N viewers. The
  // surface makes the same statement in a stronger form (T-442: a pane's pause IS its time window,
  // so "scrubbed but not paused" is not a state the type can spell), and the route must stay gone.
  for (const f of walk("src")) {
    // Comments explaining why the route is gone are the point, not a caller.
    const src = bare(f).replace(/^\s*\*.*$/gm, "");
    assert.ok(!src.includes("/api/control/pause"), `${f} names the retired pause route`);
    assert.ok(!src.includes("/api/control/resume"), `${f} names the retired resume route`);
  }
  // And the app's mount reaches the device through the one gate, never a route of its own.
  const src = readFileSync("src/app/centre/surface.ts", "utf8");
  assert.match(src, /acceptPaneRetune\(/, "the retune goes through T-444's guarded accept");
  assert.ok(!/\/api\/control\/(center|rate|gains|bias_tee)/.test(src),
    "the surface mount must not name a device route — that is view.ts's applyDeviceAction");
});

test("the surface mount is a thin client: no signal logic, and the only writes are view state", () => {
  const src = bare("src/app/centre/surface.ts");
  // It reads rows and selections and turns them into rectangles; it decides nothing about what a
  // signal is, how wide it is, or what it might be (ADR-0013 §1, CLAUDE.md's thin-client rule).
  for (const word of ["classif", "explanation", "bandwidth_hz", "demod", "estimat"]) {
    assert.ok(!new RegExp(word).test(src), `the mount must not contain "${word}"`);
  }
  // The routes it names are read-only, plus the tile route the renderer addresses.
  const routes = [...src.matchAll(/"(\/api\/[a-z/_]+)"/g)].map((m) => m[1]).sort();
  assert.deepEqual([...new Set(routes)], ["/api/navigation"],
    "the only route this file names is the navigation read; tiles are the cache's, the device is view.ts's");
});
