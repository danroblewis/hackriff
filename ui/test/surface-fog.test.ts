// T-807 (MAP-07): the coverage-fog layer — grey/unknown as an explicit, per-pane, toggleable figure
// over the one cell rule (docs/24 §3b, §13.3).
//
// The claims, each against the degenerate implementation that would otherwise pass:
//  1. **Shown (the default) is exactly the picture before T-807**: every state, every pixel, the same
//     as `cellPixel` with no flag — so the four coverage states stay drawn distinctly per
//     `resolution.grey_rule`, and the fog adds no second grey.
//  2. **Hidden touches ONLY the fog states** (unobserved, unknown). Every measurement-bearing state
//     — observed, no-level, awaiting, the last-known shadow and the excluded notch — is pixel-for-
//     pixel unchanged, so hiding the fog can never hide data we hold, nor strip the notch's ink.
//  3. **Hidden is not grey, not a level, not the unknown hatch**: a flat ground off the ramp.
//  4. **It is a flag on the ONE rule**: the generated shader takes it in `cellMark`, still contains
//     the grey exactly once, and the renderer sets it per pane — two panes over the same tiles, one
//     fog hidden, rasterise to grey in one and bare ground in the other.
//  5. **The layers menu's key is painted by the same rule** and names the shadow as not-fog.
import { test } from "node:test";
import assert from "node:assert/strict";
import { cmap } from "../src/cmap";
import {
  BACKDROP, CELL, CELL_RULE_GLSL, FOG_HIDDEN, FOG_STATES, GREY, PENDING, TIER, cellPixel, isFogState, markFor,
} from "../src/surface/cellrule";
import { fogKeyEntries, swatchPixels } from "../src/surface/legend";
import { defaultPaneLayers, isLayerVisible, layerDef } from "../src/surface/layers";
import { keyOf, type Lattice, type TileAddr } from "../src/surface/lattice";
import { Surface, type PaneView } from "../src/surface/surface";
import { TileCache } from "../src/surface/tilecache";
import type { TileData } from "../src/surface/tile";
import { stubGl } from "./surface-glstub";
import { countColour, rasterize } from "./surface-raster";

const ALL_STATES = Object.values(CELL) as number[];
const PXS = [{ x: 0.5, y: 0.5 }, { x: 3.5, y: 1.5 }, { x: 2.5, y: 6.5 }, { x: 11.5, y: 4.5 }];
const XS = [0, 0.2, 0.5, 0.9, 1];
const px = (state: number, x: number, p: { x: number; y: number }, fog?: boolean) =>
  cellPixel({ state, x, px: p, tier: TIER.LIVE_IQ, srcPx: { x: 4, y: 4 }, fallback: false, ...(fog === undefined ? {} : { fog }) });
const near = (a: readonly number[], b: readonly number[], eps = 1e-6) => b.every((v, i) => Math.abs(a[i] - v) < eps);

test("MAP-07: the fog layer is a data-plane layer, shown by default", () => {
  assert.equal(layerDef("coverage")!.plane, "data");
  assert.equal(isLayerVisible(defaultPaneLayers("p1"), "coverage"), true, "grey is the survey; it may not start hidden");
  assert.deepEqual([...FOG_STATES].sort(), [CELL.UNOBSERVED, CELL.UNKNOWN].sort());
});

test("MAP-07: shown is exactly the pre-T-807 picture, for every state", () => {
  for (const s of ALL_STATES) for (const x of XS) for (const p of PXS) {
    assert.deepEqual(px(s, x, p, true), px(s, x, p), `state ${s} changed with the fog shown`);
  }
  // …so the four coverage states are still four distinct marks, and the grey is still only unobserved.
  assert.deepEqual(markFor(CELL.UNOBSERVED), { kind: "flat", rgb: GREY });
  for (const s of ALL_STATES.filter((s) => s !== CELL.UNOBSERVED)) {
    for (const x of XS) for (const p of PXS) assert.ok(!near(px(s, x, p, true), GREY), `state ${s} draws THE grey`);
  }
});

test("MAP-07: hiding the fog changes ONLY unobserved and unknown — every measurement is still drawn", () => {
  for (const s of ALL_STATES) for (const x of XS) for (const p of PXS) {
    const hidden = px(s, x, p, false);
    if (isFogState(s)) assert.deepEqual(hidden, [...FOG_HIDDEN], `fog state ${s} did not recede`);
    else assert.deepEqual(hidden, px(s, x, p, true), `state ${s} carries a measurement and changed when the fog was hidden`);
  }
  // The excluded notch keeps its ruled ink with the fog off: somewhere on its pattern the pixel is
  // not the plain ramp, or LO leakage would read as a signal on air.
  const ruled = PXS.concat([{ x: 0.2, y: 0.2 }, { x: 1.2, y: 3.3 }]).some((p) => !near(px(CELL.EXCLUDED, 0.95, p, false), cmap(0.95)));
  assert.ok(ruled, "hiding the fog stripped the excluded notch's ink");
});

test("MAP-07: the hidden ground is not grey, not the unknown hatch, not a level, not pending", () => {
  assert.ok(!near(FOG_HIDDEN, GREY, 0.02), "hidden fog must not be THE grey");
  assert.ok(!near(FOG_HIDDEN, PENDING, 0.005) && !near(FOG_HIDDEN, BACKDROP, 0.005));
  // Darker than the grey, so the absence recedes when the viewer asks it to.
  const lum = (c: readonly number[]) => 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
  assert.ok(lum(FOG_HIDDEN) < lum(GREY));
  // Neutral and off the ramp: no position of cmap is within reach of it.
  for (let i = 0; i <= 200; i++) {
    const c = cmap(i / 200);
    const d = Math.hypot(c[0] - FOG_HIDDEN[0], c[1] - FOG_HIDDEN[1], c[2] - FOG_HIDDEN[2]);
    assert.ok(d > 0.05, `hidden fog reads as ramp level x=${i / 200}`);
  }
  // Flat: no texture, so it can never look like the unknown hatch (which is what it replaces).
  const seen = new Set<string>();
  for (let y = 0; y < 16; y++) for (let x = 0; x < 16; x++) seen.add(px(CELL.UNKNOWN, 0.5, { x: x + 0.5, y: y + 0.5 }, false).join(","));
  assert.equal(seen.size, 1);
});

test("MAP-07: the flag lives in the one generated rule — and the shader still has exactly one grey", () => {
  assert.match(CELL_RULE_GLSL, /vec3 cellMark\(int s, float x, vec2 px, float gain, bool fog\)/);
  assert.ok(CELL_RULE_GLSL.includes(`vec3(${FOG_HIDDEN.join(",")})`), "the hidden ground is generated from FOG_HIDDEN");
  assert.equal(CELL_RULE_GLSL.split(`vec3(${GREY.join(",")})`).length - 1, 1, "the fog branch added a second grey");
  const g = stubGl();
  new Surface(g.canvas, LAT, (tex) => new TileCache(tex, () => Promise.reject(new Error("unused")), {}));
  const fs = g.shaders.find((s) => s.includes("cellMark"))!;
  assert.match(fs, /uniform bool\s+uFog;/);
  assert.match(fs, /cellMark\([^;]*uFog\)/);
});

// ---- the renderer: per pane ----

const LAT: Lattice = { scheme: "view", cells: 256, f0Hz: 6250, t0Ns: 1e9, levelsF: 20, levelsT: 15 };
const TILE_HZ = LAT.f0Hz * LAT.cells;
const TILE_NS = LAT.t0Ns * LAT.cells;
const W = 240, H = 120;

/** A 16×16 tile cycling observed / unobserved / observed / unknown, so any part of it a pane shows
 * holds all three kinds of cell. */
const N = 16;
const CYCLE = [CELL.OBSERVED, CELL.UNOBSERVED, CELL.OBSERVED, CELL.UNKNOWN];
function data(a: TileAddr): TileData {
  const state = Uint8Array.from({ length: N * N }, (_, i) => CYCLE[i % 4]);
  const value = Float32Array.from(state, (s, i) => (s === CELL.OBSERVED ? -95 + (i % 7) * 5 : NaN));
  return {
    addr: a, key: keyOf(a), nf: N, nt: N, value, state,
    tier: "live-iq", answeredLevel: 1, fold: { frequency: "exact", time: "exact" },
    measured: { nf: N, nt: N },
    rangeDb: { lo: -100, hi: -60 }, bytes: 4096, serverInFlightLimit: null, serverInFlightShare: null,
  };
}
const flush = () => new Promise((r) => setImmediate(r));

test("MAP-07: two panes over the same tiles — fog shown draws grey, fog hidden draws bare ground", async () => {
  const g = stubGl(W, H);
  const surface = new Surface(g.canvas, LAT, (tex) => new TileCache(tex, (a) => Promise.resolve(data(a)), { now: () => 0 }), { pinParents: false });
  surface.setScale(-100, -60);
  const box = { f0Hz: 0, f1Hz: TILE_HZ, t0Ns: 0, t1Ns: TILE_NS };
  const left = { x: 0, y: 0, w: W / 2, h: H }, right = { x: W / 2, y: 0, w: W / 2, h: H };
  const panes: PaneView[] = [
    { id: "shown", rect: left, box },
    { id: "hidden", rect: right, box, fog: false },
  ];
  for (let i = 0; i < 4; i++) { surface.render(panes); await flush(); }
  g.reset();
  surface.render(panes);
  const fogUniforms = g.ops.filter((o) => o.kind === "draw" && o.u).map((o) => o.u!.uFog?.[0]);
  assert.ok(fogUniforms.includes(1) && fogUniforms.includes(0), `uFog was not set per pane: ${fogUniforms}`);
  const fb = rasterize(g.ops, W, H);
  assert.ok(countColour(fb, GREY, left) > 0, "the shown pane lost its grey");
  assert.equal(countColour(fb, FOG_HIDDEN, left), 0);
  assert.equal(countColour(fb, GREY, right), 0, "the hidden pane still draws grey");
  assert.ok(countColour(fb, FOG_HIDDEN, right) > 0, "the hidden pane drew no bare ground");
  // The measurements are the same pixels in both panes: hiding the fog hid no data.
  const hatch = markFor(CELL.UNKNOWN) as { rgb: readonly [number, number, number]; ink: readonly [number, number, number] };
  const fogPx = (r: typeof left) => [GREY, hatch.rgb, hatch.ink, FOG_HIDDEN].reduce((n, c) => n + countColour(fb, c, r), 0);
  const measured = (r: typeof left) => r.w * r.h - fogPx(r);
  assert.ok(measured(left) > 0);
  assert.equal(measured(right), measured(left), "hiding the fog changed how many measured pixels were drawn");
});

test("MAP-07: the layers menu's fog key is painted by the one rule, and the shadow is named not-fog", () => {
  const keys = fogKeyEntries();
  assert.deepEqual(keys.map((e) => e.key), ["unobserved", "unknown", "observed", "excluded", "shadow", "fog-hidden"]);
  const hidden = keys.find((e) => e.key === "fog-hidden")!;
  const pix = swatchPixels(hidden, 4, 4);
  assert.deepEqual([pix[0], pix[1], pix[2]], FOG_HIDDEN.map((v) => Math.round(v * 255)));
  const grey = swatchPixels(keys[0], 4, 4);
  assert.deepEqual([grey[0], grey[1], grey[2]], GREY.map((v) => Math.round(v * 255)));
  assert.match(keys.find((e) => e.key === "shadow")!.note, /Not fog/);
});
