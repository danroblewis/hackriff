// **T-528 — viewport-dynamic auto-contrast.**
//
// The claim under test, in the user's words: *the scale comes from what is currently on screen, so
// a quiet band spreads across the whole ramp instead of sitting in the bottom few percent*. The
// fixture is built so that the **existing** `auto` mode cannot make that claim — one tile whose own
// `range_db` spans 60 dB, with the quiet cells at one end of it and the loud cells at the other, so
// a viewport over either half reads the identical tile range and a distinguishable cell range. A
// test whose fixture the old mode already satisfies would prove nothing about the new one.
//
// What is pinned here:
//
//  1. the scale follows the *viewport*, not the tile, and tightens onto a quiet band;
//  2. `CELL.SHADOW` cells never reach it — a viewport that is mostly last-known must not crush the
//     live cells beside it (T-519/T-520; a shadow is a measurement of another time);
//  3. **the stated scale is the drawn scale**, read off the uniforms the render pass really
//     submitted rather than off the field it was hoped to have set (T-475's rule);
//  4. the preference round-trips and falls back, and the presses are total;
//  5. switching mode reaches **no route** — asserted against a spy on the tile fetcher, not against
//     a reading of the source;
//  6. the per-frame cost, measured.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { CELL } from "../src/surface/cellrule";
import {
  DEFAULT_RANGE_MODE, RANGE_MODE_KEY, autoContrastButton, loadRangeMode, pressAutoContrast,
  pressViewportScale, saveRangeMode, viewportScaleButton,
} from "../src/surface/contrast";
import { extentOf, keyOf, type Box, type Lattice, type TileAddr } from "../src/surface/lattice";
import { rangeEntry, rangeLabel } from "../src/surface/legend";
import { Surface, VIEWPORT_SOURCE_EMPTY, type PaneView } from "../src/surface/surface";
import { TileCache } from "../src/surface/tilecache";
import type { TileData } from "../src/surface/tile";
import {
  BLOCKS, HEADROOM_DB, MIN_SPAN_DB, ViewportScale, blockStats, resetSummaryCount, summariesBuilt,
} from "../src/surface/vscale";
import { stubGl, type GlOp } from "./surface-glstub";

// ——— the lattice: one level-0 tile is 100 kHz × 16 s, served as a 16 × 16 grid ———
//
// 16 cells an axis means the block summary's 16 × 16 grid is one block per cell, so every number
// below is exact and the block quantisation is tested separately (see the last section).
const LAT: Lattice = { scheme: "view", cells: 16, f0Hz: 6250, t0Ns: 1e9, levelsF: 20, levelsT: 15 };
const N = 16;
const TILE_HZ = 6250 * N;          // 100 kHz
const TILE_NS = 1e9 * N;           // 16 s
const W = 1200, H = 800;
const RECT = { x: 0, y: 0, w: W, h: H };
const flush = () => new Promise((r) => setImmediate(r));

/** dB of cell column `f`: a clean 4 dB-per-cell ramp from −120 at the low edge to −60 at the high. */
const dbOf = (f: number) => -120 + 4 * f;

/**
 * The fixture tile. Every cell observed, the level a function of frequency only, and the tile's own
 * `range_db` the **whole** −120…−60 — which is exactly what `auto` reads, and exactly what makes
 * `auto` blind to which half of it is on screen.
 */
function plain(a: TileAddr): TileData {
  const value = new Float32Array(N * N);
  const state = new Uint8Array(N * N).fill(CELL.OBSERVED);
  for (let t = 0; t < N; t++) for (let f = 0; f < N; f++) value[t * N + f] = dbOf(f);
  return tile(a, value, state, { lo: -120, hi: -60 });
}

/**
 * The shadow fixture: the low twelve columns are **last-known** cells carrying a loud −50 dBFS, the
 * top four are live and quiet (−100…−97). Three quarters of this viewport is a memory of a band
 * that was swept and left; if it reaches the scale, the live quarter is crushed.
 */
function shadowed(a: TileAddr): TileData {
  const value = new Float32Array(N * N);
  const state = new Uint8Array(N * N);
  for (let t = 0; t < N; t++) {
    for (let f = 0; f < N; f++) {
      const i = t * N + f;
      if (f < 12) { state[i] = CELL.SHADOW; value[i] = -50; }
      else { state[i] = CELL.OBSERVED; value[i] = -100 + (f - 12) * 4; }
    }
  }
  // The route reports the grid's range; the shadow plane is not part of it, so the tile range here
  // is the live cells'. `auto` would therefore be *right* on this fixture — the point is that the
  // viewport mode must reach the same conclusion from the cells, without being told.
  return tile(a, value, state, { lo: -100, hi: -88 });
}

function tile(a: TileAddr, value: Float32Array, state: Uint8Array, rangeDb: { lo: number; hi: number }): TileData {
  return {
    addr: a, key: keyOf(a), nf: N, nt: N, t1Ns: null, value, state,
    tier: "spectrum-history", answeredLevel: 0, fold: { frequency: "exact", time: "exact" },
    measured: { nf: N, nt: N }, rangeDb, bytes: N * N * 3, serverInFlightLimit: null, serverInFlightShare: null,
  };
}

function harness(make: (a: TileAddr) => TileData = plain) {
  const fetched: string[] = [];
  const g = stubGl(W, H);
  const surface = new Surface(
    g.canvas, LAT,
    (tex) => new TileCache(tex, (a) => { fetched.push(keyOf(a)); return Promise.resolve(make(a)); },
      { inFlight: 64, now: () => 0 }),
    { pinParents: false },
  );
  return { g, surface, fetched };
}

const pane = (box: Box, id = "a"): PaneView => ({ id, rect: RECT, box });

/** A box over cell columns `[f0, f1)` of tile 0, over its whole time extent. */
const cols = (f0: number, f1: number): Box => ({
  f0Hz: (f0 / N) * TILE_HZ, f1Hz: (f1 / N) * TILE_HZ, t0Ns: 0, t1Ns: TILE_NS,
});

const WHOLE = cols(0, N);
const QUIET = cols(0, 4);   // −120 … −108
const LOUD = cols(12, N);   // −72  … −60

async function settle(s: Surface, box: Box, frames = 8): Promise<void> {
  for (let i = 0; i < frames; i++) { s.render([pane(box)]); await flush(); }
}

// ———————————————————————————————————————————————————————————————————————————
// 1. the scale follows the viewport
// ———————————————————————————————————————————————————————————————————————————

test("the range is measured from the cells IN VIEW: panning to a quiet band tightens onto it", async () => {
  const { surface } = harness();
  surface.setScale(-120, -60, "the region");
  surface.setRangeMode("viewport");

  await settle(surface, QUIET);
  const quiet = { lo: surface.lo, hi: surface.hi };
  assert.deepEqual(quiet, { lo: -120 - HEADROOM_DB, hi: -108 + HEADROOM_DB },
    "the quiet band's own extremes, plus the stated headroom, and nothing from off screen");

  await settle(surface, LOUD);
  const loud = { lo: surface.lo, hi: surface.hi };
  assert.deepEqual(loud, { lo: -72 - HEADROOM_DB, hi: -60 + HEADROOM_DB });

  // The whole point: a quiet band SPREADS. The 12 dB it occupies used to be 20 % of the 60 dB tile
  // range; it is now the whole ramp bar the headroom.
  const share = (12) / (quiet.hi - quiet.lo);
  assert.ok(share > 0.7, `the quiet band fills only ${(share * 100).toFixed(0)} % of the ramp`);

  await settle(surface, WHOLE);
  assert.deepEqual({ lo: surface.lo, hi: surface.hi }, { lo: -122, hi: -58 },
    "zoomed out to the whole tile the mode reads the whole tile — it tracks the view, it does not shrink");
});

test("NON-VACUITY: `auto` cannot tell the two viewports apart, which is why this mode exists", async () => {
  const { surface } = harness();
  surface.setScale(-120, -60, "the region");
  surface.setAutoScale(true);
  await settle(surface, QUIET, 40);
  const quiet = { lo: surface.lo, hi: surface.hi };
  await settle(surface, LOUD, 40);
  const loud = { lo: surface.lo, hi: surface.hi };
  assert.ok(Math.abs(quiet.lo - loud.lo) < 0.5 && Math.abs(quiet.hi - loud.hi) < 0.5,
    `the tile-range mode distinguished the two halves (${JSON.stringify(quiet)} vs ${JSON.stringify(loud)}); `
    + "the fixture no longer isolates what T-528 adds");
  assert.ok(quiet.lo < -125 && quiet.hi > -58, "…and it is reading the whole tile's range, as designed");
});

test("a flat viewport is widened to the stated minimum rather than stretched across the ramp", async () => {
  const { surface } = harness((a) => {
    const value = new Float32Array(N * N).fill(-93.5);
    return tile(a, value, new Uint8Array(N * N).fill(CELL.OBSERVED), { lo: -93.5, hi: -93.5 });
  });
  surface.setRangeMode("viewport");
  await settle(surface, WHOLE);
  assert.equal(Number((surface.hi - surface.lo).toFixed(6)), MIN_SPAN_DB,
    "a viewport with no dynamic range was given one: quantisation drawn as structure");
  assert.equal((surface.lo + surface.hi) / 2, -93.5, "…and it is widened about the level it measured");
  assert.match(surface.range.source, /flat/, "the widening is stated, not silent");
});

test("nothing observed on screen HOLDS the range and says so, rather than scaling over grey", async () => {
  // The tile's own `range_db` is deliberately a wide, loud one: if this mode ever fell back to the
  // tile range over grey, these numbers would show up instead of the held anchor.
  const { surface } = harness((a) => tile(a, new Float32Array(N * N).fill(Number.NaN),
    new Uint8Array(N * N).fill(CELL.UNOBSERVED), { lo: -200, hi: -1 }));
  surface.setScale(-95, -45, "the region");
  surface.setRangeMode("viewport");
  await settle(surface, WHOLE);
  assert.deepEqual({ lo: surface.lo, hi: surface.hi }, { lo: -95, hi: -45 });
  assert.equal(surface.range.source, VIEWPORT_SOURCE_EMPTY);
});

test("the MAP does not decide the scale: an orientation viewport over the whole surface is not the subject", async () => {
  const { surface } = harness();
  surface.setRangeMode("viewport");
  const half = { x: 0, y: 0, w: W, h: H / 2 };
  const map: PaneView = { id: "map", rect: half, box: WHOLE, scales: false };
  const looking: PaneView = { id: "a", rect: { ...half, y: H / 2 }, box: QUIET };
  for (let i = 0; i < 8; i++) { surface.render([looking, map]); await flush(); }
  assert.deepEqual({ lo: surface.lo, hi: surface.hi }, { lo: -122, hi: -106 },
    "the map's whole-surface content set the range, so zooming into a quiet band does nothing — "
    + "which is the complaint this mode exists to answer");

  // And the exclusion is `scales`, not the id or the rectangle: the same map viewport WITHOUT the
  // flag contributes, which is what makes the assertion above about the flag.
  const { surface: b } = harness();
  b.setRangeMode("viewport");
  for (let i = 0; i < 8; i++) { b.render([looking, { ...map, scales: undefined }]); await flush(); }
  assert.deepEqual({ lo: b.lo, hi: b.hi }, { lo: -122, hi: -58 });
});

test("SurfaceView is what passes `scales: false`, so no host has to remember to", () => {
  const src = readFileSync("src/surface/view.ts", "utf8");
  assert.match(src, /this\.minimap\.view\(mapRect, edgeNs\), scales: false/,
    "the map viewport no longer opts out of deciding the display range");
});

// ———————————————————————————————————————————————————————————————————————————
// 2. shadow cells must not drag the scale
// ———————————————————————————————————————————————————————————————————————————

test("SHADOW cells are excluded: a viewport that is three-quarters last-known does not crush its live cells", async () => {
  const { surface } = harness(shadowed);
  surface.setRangeMode("viewport");
  await settle(surface, WHOLE);

  // The shadows read −50 dBFS. If a single one reached the scale the top of the ramp would be −48
  // and the four live cells (−100…−88) would sit in the bottom quarter of it — the exact defect.
  assert.deepEqual({ lo: surface.lo, hi: surface.hi }, { lo: -100 - HEADROOM_DB, hi: -88 + HEADROOM_DB },
    "a remembered level set the display range");
  assert.ok(surface.hi < -80, `the loud shadow reached the top of the ramp (${surface.hi})`);
  const live = (-88 - -100) / (surface.hi - surface.lo);
  assert.ok(live > 0.2, `the live cells occupy only ${(live * 100).toFixed(0)} % of the ramp`);
  assert.match(surface.range.source, /shadows excluded/);
});

test("the exclusion is the state byte, one rule, so every non-measurement is out with it", () => {
  const value = new Float32Array(6);
  const state = new Uint8Array(6);
  const put = (i: number, s: number, v: number) => { state[i] = s; value[i] = v; };
  put(0, CELL.OBSERVED, -90);
  put(1, CELL.SHADOW, -10);      // a measurement of another time
  put(2, CELL.UNOBSERVED, -10);  // THE grey — value is NaN in the product, a level here to be sure
  put(3, CELL.UNKNOWN, -10);
  put(4, CELL.AWAITING, -10);
  put(5, CELL.NO_LEVEL, -10);
  const d = { nf: 6, nt: 1, value, state } as unknown as TileData;
  const s = blockStats(d);
  assert.equal(s.cells, 1, "a state other than OBSERVED contributed a level");
  let lo = Infinity, hi = -Infinity;
  for (let i = 0; i < s.lo.length; i++) {
    if (Number.isFinite(s.lo[i])) { lo = Math.min(lo, s.lo[i]); hi = Math.max(hi, s.hi[i]); }
  }
  assert.deepEqual({ lo, hi }, { lo: -90, hi: -90 });
  // The predicate is spelled once, in the summariser, and reads the same byte the shader does.
  const src = readFileSync("src/surface/vscale.ts", "utf8");
  assert.equal((src.match(/state\[i\] !== CELL\.OBSERVED/g) ?? []).length, 1,
    "the observed-only rule is written more than once, or has moved");
});

// ———————————————————————————————————————————————————————————————————————————
// 3. the stated scale IS the drawn scale (T-475)
// ———————————————————————————————————————————————————————————————————————————

/** The `(uLo, uHi)` every quad of the recorded frame was really submitted with. */
function drawnRanges(ops: readonly GlOp[]): { lo: number; hi: number }[] {
  return ops.filter((o) => o.kind === "draw" && o.u?.uLo && o.u?.uHi)
    .map((o) => ({ lo: o.u!.uLo[0], hi: o.u!.uHi[0] }));
}

test("every quad on the screen was drawn with the pair the readout quotes — in all three modes", async () => {
  for (const mode of ["anchored", "auto", "viewport"] as const) {
    const { g, surface } = harness();
    surface.setScale(-120, -60, "the region");
    surface.setRangeMode(mode);
    // Settle, then take ONE more frame and read the uniforms of exactly that frame.
    await settle(surface, QUIET);
    for (const box of [QUIET, LOUD, WHOLE, QUIET]) {
      g.ops.length = 0;
      surface.render([pane(box)]);
      await flush();
      const drawn = drawnRanges(g.ops);
      assert.ok(drawn.length > 0, `${mode}: nothing was drawn, so the check is vacuous`);
      for (const d of drawn) {
        assert.deepEqual(d, { lo: surface.lo, hi: surface.hi },
          `${mode}: a quad was coloured with ${JSON.stringify(d)} while the surface states `
          + `${surface.lo}…${surface.hi} — the readout's numbers are not the pixels' own`);
      }
      assert.deepEqual({ lo: surface.range.lo, hi: surface.range.hi }, { lo: surface.lo, hi: surface.hi });
    }
  }
});

test("the per-frame range never reaches the shader a frame early: moving the view cannot desynchronise it", async () => {
  const { g, surface } = harness();
  surface.setRangeMode("viewport");
  await settle(surface, QUIET);
  // Pan a long way in one step — the worst case for a range computed from one frame's geometry and
  // applied to another's. The statement must still be true of the pixels of the frame it describes.
  for (const box of [LOUD, QUIET, LOUD, WHOLE]) {
    g.ops.length = 0;
    surface.render([pane(box)]);
    await flush();
    for (const d of drawnRanges(g.ops)) assert.deepEqual(d, { lo: surface.lo, hi: surface.hi });
  }
});

test("the legend and the one-line label both name the viewport mode and its trade", () => {
  const r = { lo: -102, hi: -95, mode: "viewport" as const, source: "measured over 48 blocks of observed cells now on screen (shadows excluded, ±2 dB headroom)" };
  const e = rangeEntry(r);
  assert.match(e.label, /-102\.0 … -95\.0 dBFS/);
  assert.match(e.note, /viewport-dynamic/i);
  assert.match(e.note, /shadows excluded/, "the legend must say what the measurement left out");
  assert.match(e.note, /as you pan/i, "the viewport mode's own trade is re-measurement under a PAN");
  assert.match(rangeLabel(r), /viewport-dynamic/);
  assert.match(rangeLabel(r), /-102\.0…-95\.0 dBFS/);
});

// ———————————————————————————————————————————————————————————————————————————
// 4. the preference and the presses
// ———————————————————————————————————————————————————————————————————————————

function installFakeStorage(behavior: "ok" | "throws" | "missing") {
  const orig = (globalThis as { localStorage?: Storage }).localStorage;
  if (behavior === "missing") {
    // @ts-expect-error deliberately deleting the global to simulate its absence
    delete (globalThis as { localStorage?: Storage }).localStorage;
  } else if (behavior === "throws") {
    (globalThis as { localStorage: Storage }).localStorage = {
      getItem() { throw new Error("storage disabled"); },
      setItem() { throw new Error("storage disabled"); },
    } as unknown as Storage;
  } else {
    const store = new Map<string, string>();
    (globalThis as { localStorage: Storage }).localStorage = {
      getItem: (k: string) => (store.has(k) ? store.get(k)! : null),
      setItem: (k: string, v: string) => { store.set(k, v); },
      removeItem: (k: string) => { store.delete(k); },
      clear: () => store.clear(), key: () => null, get length() { return store.size; },
    } as unknown as Storage;
  }
  return () => { (globalThis as { localStorage?: Storage }).localStorage = orig; };
}

test("the mode round-trips through storage, defaults to ANCHORED, and survives a hostile localStorage", () => {
  let restore = installFakeStorage("ok");
  try {
    assert.equal(DEFAULT_RANGE_MODE, "anchored", "the default must stay today's behaviour");
    assert.equal(loadRangeMode(), "anchored", "nothing written yet");
    for (const m of ["viewport", "auto", "anchored"] as const) {
      saveRangeMode(m);
      assert.equal(loadRangeMode(), m, `${m} did not round-trip`);
    }
    localStorage.setItem(RANGE_MODE_KEY, "logarithmic");
    assert.equal(loadRangeMode(), "anchored", "an unrecognised stored value must fall back, not be trusted");
  } finally { restore(); }

  restore = installFakeStorage("throws");
  try {
    assert.equal(loadRangeMode(), "anchored");
    assert.doesNotThrow(() => saveRangeMode("viewport"));
  } finally { restore(); }

  restore = installFakeStorage("missing");
  try {
    assert.equal(loadRangeMode(), "anchored");
    assert.doesNotThrow(() => saveRangeMode("viewport"));
  } finally { restore(); }
});

test("the two presses are total, and no sequence of them reaches a state the buttons cannot describe", () => {
  const modes = ["anchored", "auto", "viewport"] as const;
  for (const m of modes) {
    for (const next of [pressAutoContrast(m), pressViewportScale(m)]) {
      assert.ok(modes.includes(next), `${m} pressed into ${next}`);
      const a = autoContrastButton(next), v = viewportScaleButton(next);
      assert.equal(a.pressed, next !== "anchored");
      assert.equal(v.pressed, next === "viewport");
      assert.match(a.label, /^Auto-contrast: (on|off)$/);
      assert.match(v.label, /^Viewport scale: (on|off)$/);
      assert.ok(a.title.length > 40 && v.title.length > 40, "a control that changes a colour must say what it does");
    }
  }
  // The two claims the presses make, stated rather than left to the table above.
  assert.equal(pressAutoContrast("viewport"), "anchored", "Auto-contrast OFF means off, from either tracking mode");
  assert.equal(pressViewportScale("anchored"), "viewport", "the viewport button must not be a switch that does nothing");
  assert.equal(pressViewportScale("viewport"), "auto", "…and turning it off leaves tracking on");
  assert.match(viewportScaleButton("anchored").title, /auto-contrast on/i, "…and it says that it does that");
});

// ———————————————————————————————————————————————————————————————————————————
// 5. it reaches no route
// ———————————————————————————————————————————————————————————————————————————

test("SPY: switching contrast mode fetches nothing — not on the press, and not on the frames after it", async () => {
  const { surface, fetched } = harness();
  surface.setScale(-120, -60, "the region");
  await settle(surface, QUIET);
  const before = fetched.length;
  assert.ok(before > 0, "no tile was ever fetched, so an unchanged count proves nothing");

  for (const m of ["viewport", "auto", "anchored", "viewport"] as const) {
    surface.setRangeMode(m);
    assert.equal(fetched.length, before, `${m}: the press itself asked the backend for something`);
    await settle(surface, QUIET, 4);
    assert.equal(fetched.length, before, `${m}: the frames after the press re-fetched tiles already in hand`);
  }
});

test("the toggle's code in the app centre names no route and writes no store state", () => {
  const src = readFileSync("src/app/centre/surface.ts", "utf8");
  const from = src.indexOf("const paint = (btn");
  const to = src.indexOf("renderRange();", src.indexOf("vscaleBtn.addEventListener"));
  assert.ok(from > 0 && to > from, "the contrast control's block moved; re-point this guard");
  const region = src.slice(from, to);
  assert.ok(!/\/api\//.test(region), "a route is named inside the contrast control");
  assert.ok(!/store\.set/.test(region), "the contrast control writes app state");
  assert.match(src, /vscaleBtn = h\("button",/);
});

// ———————————————————————————————————————————————————————————————————————————
// 6. the cost
// ———————————————————————————————————————————————————————————————————————————

test("COST: a tile is summarised ONCE however many frames read it", async () => {
  resetSummaryCount();
  const { surface } = harness();
  surface.setRangeMode("viewport");
  await settle(surface, WHOLE, 30);
  assert.equal(summariesBuilt(), 1, `${summariesBuilt()} summaries for one tile: the memo is not holding`);
});

test("COST: the per-frame read is bounded by the block grid, not by the cell count", () => {
  // A realistic screen: 40 resident 256 × 256 tiles, every cell observed — 2.6 M cells. The
  // summaries are built once (the price of the first frame); the per-frame work is the block reads.
  const tiles: TileData[] = [];
  const ext: Box[] = [];
  for (let i = 0; i < 40; i++) {
    const n = 256;
    const value = new Float32Array(n * n);
    for (let k = 0; k < value.length; k++) value[k] = -120 + (k % 61);
    const a: TileAddr = { device: "any", scheme: "view", levelF: 0, levelT: 0, fIndex: i, tIndex: 0, cells: LAT.cells };
    tiles.push({
      ...tile(a, value, new Uint8Array(n * n).fill(CELL.OBSERVED), { lo: -120, hi: -60 }),
      nf: n, nt: n, measured: { nf: n, nt: n },
    });
    ext.push(extentOf(LAT, a));
  }
  const view: Box = { f0Hz: ext[0].f0Hz, f1Hz: ext[39].f1Hz, t0Ns: 0, t1Ns: TILE_NS };

  const build = process.hrtime.bigint();
  const first = new ViewportScale();
  for (let i = 0; i < 40; i++) first.add(tiles[i], ext[i], ext[i], view);
  const buildMs = Number(process.hrtime.bigint() - build) / 1e6;

  const FRAMES = 120;
  const t0 = process.hrtime.bigint();
  for (let n = 0; n < FRAMES; n++) {
    const acc = new ViewportScale();
    for (let i = 0; i < 40; i++) acc.add(tiles[i], ext[i], ext[i], view);
    if (!acc.range()) throw new Error("the accumulator found nothing");
  }
  const perFrameMs = Number(process.hrtime.bigint() - t0) / 1e6 / FRAMES;
  console.log(`  T-528 cost: first frame (40 × 256² summarised) ${buildMs.toFixed(1)} ms; `
    + `steady state ${(perFrameMs * 1000).toFixed(0)} µs/frame over ${40 * BLOCKS * BLOCKS} block reads`);
  // Budgets, not measurements: a frame is 16.7 ms, and this may not be a visible part of it. They
  // are deliberately loose — the assertion that matters is that the steady state does not scale
  // with the 2.6 M cells, which is why the two numbers are printed side by side.
  assert.ok(perFrameMs < 2, `${perFrameMs.toFixed(2)} ms/frame: the per-frame path is scanning cells`);
  assert.ok(buildMs < 400, `${buildMs.toFixed(0)} ms to summarise 40 tiles`);
});

test("the block grid is bounded and conservative: it may widen a range, never invent a level", () => {
  const n = 64;
  const value = new Float32Array(n * n);
  for (let t = 0; t < n; t++) for (let f = 0; f < n; f++) value[t * n + f] = -120 + f;
  const a: TileAddr = { device: "any", scheme: "view", levelF: 0, levelT: 0, fIndex: 0, tIndex: 0, cells: LAT.cells };
  const d: TileData = {
    ...tile(a, value, new Uint8Array(n * n).fill(CELL.OBSERVED), { lo: -120, hi: -57 }),
    nf: n, nt: n, measured: { nf: n, nt: n },
  };
  const s = blockStats(d);
  assert.equal(s.bf * s.bt, BLOCKS * BLOCKS, "the summary is not the stated block grid");
  assert.equal(s.sf, n / BLOCKS);

  // A viewport over one block's worth of columns reads that block, and only that block.
  const ex = extentOf(LAT, a);
  const acc = new ViewportScale();
  acc.add(d, ex, ex, { ...ex, f0Hz: ex.f0Hz, f1Hz: ex.f0Hz + (s.sf / n) * (ex.f1Hz - ex.f0Hz) });
  assert.deepEqual({ lo: acc.lo, hi: acc.hi }, { lo: -120, hi: -120 + s.sf - 1 });

  // A viewport straddling two blocks reads both, whole — wider than the visible extremes, never
  // narrower, and never a level the tile does not contain.
  const straddle = new ViewportScale();
  const px = (ex.f1Hz - ex.f0Hz) / n;
  straddle.add(d, ex, ex, { ...ex, f0Hz: ex.f0Hz + px * (s.sf - 1), f1Hz: ex.f0Hz + px * (s.sf + 1) });
  assert.ok(straddle.lo <= -120 + s.sf - 1 && straddle.hi >= -120 + s.sf, "the straddled blocks were not both read");
  assert.ok(straddle.lo >= -120 && straddle.hi <= -120 + n - 1, "a level outside the tile was invented");
});
