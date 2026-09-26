// **The instantaneous spectrum trace** (T-457) — docs/16 §8.5b finding 1, built rather than dropped.
//
// ## What each assertion here is a property OF
//
// This milestone keeps producing proofs that are sound about the wrong subject: T-441 verified a
// shader on 114 973 of 115 200 pixels **for a module that could not load in a browser**; T-454's
// counter measured a pump rather than a cancellation; T-448's gate guaranteed every counter except
// the asserted one. So each group below says what it is about, and what it deliberately is not:
//
//  - **`sampleFrame`** — about the *placement and pooling of the delivered row*. It asserts that a
//    spike at a named frequency lands in the column that contains that frequency, and that a spike
//    narrower than a column survives the collapse. It says nothing about whether the app fetched the
//    row; `ui/e2e/app-trace.e2e.mjs` is the tier that taps the socket and compares.
//  - **`maxHoldColumns`** — about *the server's own fold, re-applied*. The load-bearing one is the
//    idempotence test: a max of max-holds is a max-hold, so reducing four fine tiles and reducing
//    one coarse tile built as their max give the **same** trace. That is what licenses the client to
//    reduce at all, and it is a property of `max` as `hk-api`'s `MAX_HOLD_RULE` defines it — not an
//    observation that today's numbers happen to agree.
//  - **`tracePaths`** — about *the three shared readings of one scale*. x is asserted against
//    `toClip` itself (the function the tiles are placed with), not against a recomputation of its
//    arithmetic; y is asserted against the surface's `lo`/`hi`, including that moving them moves the
//    trace; and since T-475 **colour** is asserted to be `cmap` of the *same* normalisation the tile
//    shader computes, so "the same dB is the same colour" is an equality rather than an impression.
//  - **the smoothing (T-475)** — about *the values the curve passes through*, never about vertex
//    counts. A curve with more vertices is not a curve drawn through the same measurements, and that
//    is precisely the adjacent-question trap this milestone keeps falling into. So: every measured
//    column is ON the curve, every interpolated point lies between the two samples that bracket it,
//    and a gap is still a gap.
//  - **`persistenceSlices`** — about *the afterglow being a function of the pane's window*. The rows
//    it returns move when the pane's time position moves, which is the difference between afterglow
//    that survives a scrub and a client-side buffer of whatever arrived while the page was open.
//  - **`SurfaceView`** — about *the strip being carved out of the pane rather than painted over it*,
//    and about the data pass being byte-identical with the trace on and off.
//
// The trap this ticket was warned about — *"a trace that renders is not a trace showing the current
// frame"* — is answered by asserting **values**, never quad counts: every test below that could be
// satisfied by "something was drawn" also pins where and how high.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { CELL } from "../src/surface/cellrule";
import { keyOf, type Lattice, type TileAddr } from "../src/surface/lattice";
import { clampToRect, paneAtPoint } from "../src/surface/preview";
import { pointOn } from "../src/surface/marks";
import { Surface, toClip, type PaneRect } from "../src/surface/surface";
import { TileCache } from "../src/surface/tilecache";
import type { TileData } from "../src/surface/tile";
import {
  HOLD_INK, LiveRow, SHADOW_ALPHA, TRACE_SUBDIV, levelFrac, liveFrameFits, maxHoldColumns, peakOf,
  afterglowAbsence, persistenceShortTiles, persistenceSlices, sampleFrame, sliceColumns, sliceWindow, tracePaths,
  type LiveFrame, type TracePath,
} from "../src/surface/trace";
import { cmap } from "../src/cmap";
import { SurfaceView } from "../src/surface/view";
import { stubGl } from "./surface-glstub";

const S = 1e9;
const T0 = 1_700_000_000 * S;
/** A pane looking at 2.4 MHz around 100.8 MHz over 20 s, ending at the edge. */
const PANE = { f0Hz: 99.6e6, f1Hz: 102e6, t0Ns: T0 - 20 * S, t1Ns: T0 };
const RECT: PaneRect = { x: 0, y: 0, w: 1000, h: 500 };
/** A trace band: the pane's width, a fraction of its height. */
const STRIP: PaneRect = { x: 0, y: 500, w: 1000, h: 96 };
const LO = -110, HI = -40;

const near = (a: number, b: number, eps = 1e-9) => assert.ok(Math.abs(a - b) <= eps, `${a} ≉ ${b}`);

/** Every point of a set of paths, flattened: clip x/y and the linear RGB the vertex carries. */
function pointsOf(paths: readonly TracePath[]): { x: number; y: number; rgb: [number, number, number] }[] {
  const out: { x: number; y: number; rgb: [number, number, number] }[] = [];
  for (const p of paths) {
    for (let i = 0; i < p.xy.length / 2; i++) {
      out.push({ x: p.xy[2 * i], y: p.xy[2 * i + 1], rgb: [p.rgb[3 * i], p.rgb[3 * i + 1], p.rgb[3 * i + 2]] });
    }
  }
  return out;
}

/** A point's y read back as a dB against a display range — the inverse of the trace's y mapping. */
const dbOf = (y: number, lo = LO, hi = HI) => lo + ((y + 1) / 2) * (hi - lo);

/** A flat-ink style, so a test about geometry is not also a test about colour. */
const INK = { ink: [1, 1, 1] as const };

/** A frame across the pane's own band, flat at `floor` with a spike of `db` at `atHz`. */
function frame(bins: number, floor: number, atHz: number, db: number, tNs = T0 - S): LiveFrame {
  const f0Hz = 99.6e6, f1Hz = 102e6;
  const v = new Float32Array(bins).fill(floor);
  const i = Math.floor(((atHz - f0Hz) / (f1Hz - f0Hz)) * bins);
  v[i] = db;
  return { f0Hz, f1Hz, db: v, tNs };
}

// ---------------------------------------------------------------------------
// sampleFrame: the delivered row, placed and pooled
// ---------------------------------------------------------------------------

test("a spike at a named frequency lands in the column that CONTAINS that frequency", () => {
  const spikeHz = 100.3e6;
  const cols = sampleFrame(frame(4096, -100, spikeHz, -42), PANE, 256);
  const pk = peakOf(cols, PANE)!;
  assert.equal(pk.db, -42, "the peak value is the row's own value, not a smoothed one");
  const colHz = (PANE.f1Hz - PANE.f0Hz) / 256;
  assert.ok(Math.abs(pk.hz - spikeHz) <= colHz,
    `the peak is reported at ${pk.hz} Hz; the spike is at ${spikeHz} Hz and a column is ${colHz} Hz`);
  // …and the neighbours are the floor, so this is a located spike and not a wash.
  assert.equal(cols[pk.col - 2], -100);
  assert.equal(cols[pk.col + 2], -100);
});

test("a spike NARROWER than a column survives the collapse — the pooling is max, like the server's fold", () => {
  // 4096 bins across 256 columns: sixteen bins fold into each. A sampling rule that took the
  // column's first or middle bin would report −100 here fifteen times out of sixteen, and the
  // emission would blink in and out as the pane was zoomed. That is the whole reason the fold is a
  // max, and the reason `decimateRow` was a max-decimate before it.
  for (let offset = 0; offset < 16; offset++) {
    const binHz = (PANE.f1Hz - PANE.f0Hz) / 4096;
    const at = PANE.f0Hz + (2048 + offset + 0.5) * binHz;
    const cols = sampleFrame(frame(4096, -100, at, -42), PANE, 256);
    assert.equal(peakOf(cols, PANE)!.db, -42, `a spike at bin offset ${offset} in its column was lost`);
  }
});

test("outside the tuned band is a GAP, not a floor — there is no current frame out there", () => {
  // The pane is zoomed out well past what the front end is tuned to.
  const wide = { ...PANE, f0Hz: 90e6, f1Hz: 110e6 };
  const cols = sampleFrame(frame(1024, -100, 100.3e6, -42), wide, 200);
  const colHz = (wide.f1Hz - wide.f0Hz) / 200;
  for (let c = 0; c < 200; c++) {
    const lo = wide.f0Hz + c * colHz, hi = lo + colHz;
    const overlaps = hi > 99.6e6 && lo < 102e6;
    assert.equal(Number.isFinite(cols[c]), overlaps,
      `column ${c} (${lo / 1e6}–${hi / 1e6} MHz) ${overlaps ? "overlaps" : "does not overlap"} the tuned band`);
  }
  // And a gap draws nothing at all: a curve through it would invent the value the radio never took.
  // The check is on the drawn EXTENT rather than on a count, because the line is smooth now (T-475):
  // the measured columns form one contiguous run, so they are ONE path, and no point of it may sit in
  // a column the row did not answer for. Smoothing that bridged a gap would show up here as a point
  // out past the band edge; nothing about vertex count would.
  const paths = tracePaths(cols, wide, STRIP, LO, HI, "trace-slice", "p", INK);
  assert.equal(paths.length, 1, "the answered columns are contiguous, so they are ONE curve");
  const hzOf = (x: number) => wide.f0Hz + ((x + 1) / 2) * (wide.f1Hz - wide.f0Hz);
  for (const pt of pointsOf(paths)) {
    const hz = hzOf(pt.x);
    assert.ok(hz >= 99.6e6 - colHz && hz <= 102e6 + colHz,
      `the curve reaches ${(hz / 1e6).toFixed(3)} MHz, outside the tuned band it was drawn from`);
  }
});

test("the live row is preferred only INSIDE the cell the slice asks about — a scrubbed pane gets no line from now", () => {
  const lat: Lattice = { scheme: "view", cells: 256, f0Hz: 6250, t0Ns: 1e9, levelsF: 20, levelsT: 15 };
  const fr = frame(64, -100, 100.3e6, -42, T0 - 0.4 * S);
  // A pane following the edge: its time position is the edge, and the newest row is in that cell.
  assert.equal(liveFrameFits(fr, sliceWindow(lat, 0, T0)), true);
  // A pane scrubbed five minutes back: the same row is nowhere near the cell being asked about, so
  // the pyramid answers instead. Drawing the row here would be a spectrum of NOW over a picture of
  // THEN, and it would look completely convincing.
  assert.equal(liveFrameFits(fr, sliceWindow(lat, 0, T0 - 300 * S)), false);
  // …and a coarse level legitimately widens the cell, so the same row can fit a coarser slice.
  assert.equal(liveFrameFits(fr, sliceWindow(lat, 4, T0)), true);
  assert.equal(liveFrameFits(null, sliceWindow(lat, 0, T0)), false);
  assert.equal(liveFrameFits({ ...fr, tNs: Number.NaN }, sliceWindow(lat, 0, T0)), false,
    "an unknown time is not a time inside the cell");
});

test("T-1051: a FOLLOWING pane's slice is the newest row even when it has run past the edge cell", () => {
  // The display floor: 40 ms cells. The pane's edge is written from the stream at most every
  // EDGE_WRITE_S = 0.25 s, so the newest delivered row routinely sits several cells past the cell
  // the pane's time position names. Measured in app-trace.e2e.mjs as a trace that flickered between
  // "(live frame)" and a not-yet-built pyramid cell, and a page held on the wrong side of it.
  const lat: Lattice = { scheme: "view", cells: 256, f0Hz: 6250, t0Ns: 40e6, levelsF: 20, levelsT: 15 };
  const edge = T0;
  const win = sliceWindow(lat, 0, edge);
  for (const aheadMs of [0, 40, 120, 240]) {
    const fr = frame(64, -100, 100.3e6, -42, edge + aheadMs * 1e6);
    assert.equal(liveFrameFits(fr, win, true), true,
      `a following pane dropped the newest row ${aheadMs} ms past its edge cell for a pyramid cell`);
    if (aheadMs > 0) {
      // A FROZEN pane is a view over the past: the same row is newer than its window, so no line
      // from now over a picture of then — the rule the test above states, unchanged.
      assert.equal(liveFrameFits(fr, win, false), false, `a frozen pane took a row ${aheadMs} ms after its window`);
    }
  }
  // A row OLDER than the edge cell is not the top-most sample of a following pane either.
  assert.equal(liveFrameFits(frame(64, -100, 100.3e6, -42, win.t0Ns - 1e6), win, true), false);
  assert.equal(liveFrameFits(null, win, true), false);
});

test("sliceWindow is ONE lattice cell, snapped to the grid, ending at the pane's time position", () => {
  const lat: Lattice = { scheme: "view", cells: 256, f0Hz: 6250, t0Ns: 1e9, levelsF: 20, levelsT: 15 };
  for (const levelT of [0, 1, 5]) {
    const cell = 1e9 * 2 ** levelT;
    const w = sliceWindow(lat, levelT, T0 + 3 * S);
    assert.equal(w.t1Ns - w.t0Ns, cell, "the slice is exactly one cell — it cannot be finer than the ladder");
    assert.equal(w.t0Ns % cell, 0, "and it is the lattice's own cell, not a window straddling two");
    assert.ok(w.t0Ns <= T0 + 3 * S && T0 + 3 * S <= w.t1Ns, "…containing the instant asked about");
  }
  // On a boundary it is the cell ENDING there: the pane's time position is the top of its window,
  // so the newest instant it shows is the end of the newest cell it drew.
  const onEdge = sliceWindow(lat, 0, T0);
  assert.equal(onEdge.t1Ns, T0);
  assert.equal(onEdge.t0Ns, T0 - 1e9);
});

test("LiveRow keeps exactly one row, and a retune clears it — this is not an accumulator", () => {
  const r = new LiveRow();
  assert.equal(r.get(), null);
  r.set(frame(8, -100, 100.3e6, -42, T0 - 2 * S));
  r.set(frame(8, -100, 100.3e6, -30, T0 - S));
  assert.equal(r.rows, 2);
  assert.equal(r.get()!.tNs, T0 - S, "the newest row stands; nothing older is retained to be maxed over");
  assert.equal(peakOf(sampleFrame(r.get()!, PANE, 64), PANE)!.db, -30,
    "the holder cannot answer -42 — it kept no history, which is the point");
  r.clear();
  assert.equal(r.get(), null, "the old band's frame is not the new band's");
  assert.equal(r.rows, 0);
});

// ---------------------------------------------------------------------------
// maxHoldColumns: the server's own fold, re-applied
// ---------------------------------------------------------------------------

const LAT: Lattice = { scheme: "view", cells: 4, f0Hz: 600_000, t0Ns: 5 * S, levelsF: 8, levelsT: 8 };

/** A tile carrying `value` (row-major, earliest row first) with a state plane to match. */
function tileOf(a: TileAddr, nf: number, nt: number, value: number[], unobserved: number[] = []): TileData {
  const state = new Uint8Array(nf * nt).fill(CELL.OBSERVED);
  for (const k of unobserved) state[k] = CELL.UNOBSERVED;
  return {
    addr: a, key: keyOf(a), nf, nt,
    // `decodeTile` writes NaN wherever the state plane does not say OBSERVED; mirror that here so
    // this fixture cannot be more forgiving than the decoder.
    value: new Float32Array(value.map((v, k) => (state[k] === CELL.OBSERVED ? v : Number.NaN))),
    state,
    tier: "live-iq", answeredLevel: a.levelF, fold: { frequency: "exact", time: "exact" },
    measured: { nf, nt }, rangeDb: null, bytes: nf * nt * 3, serverInFlightLimit: null, serverInFlightShare: null,
  };
}

/** A cache that answers `peek` from a fixed table — the residency the renderer would have. */
function peeker(tiles: Map<string, TileData>) {
  return {
    peek(a: TileAddr) {
      const d = tiles.get(keyOf(a));
      return d ? ({ addr: a, key: keyOf(a), data: d } as never) : null;
    },
  };
}

test("a column is the MAXIMUM of the observed cells over it, and an unobserved cell contributes nothing", () => {
  // One level-0 tile, 4 × 4 cells: 600 kHz and 5 s each, so 2.4 MHz × 20 s starting at the lattice
  // origin. Column 2's cells are −70, −50 (the max), −80, and one UNOBSERVED carrying a value that
  // would win if the state plane were ignored.
  const a: TileAddr = { device: "any", scheme: "view", levelF: 0, levelT: 0, fIndex: 0, tIndex: 0, cells: 4 };
  const v = new Array(16).fill(-95);
  v[0 * 4 + 2] = -70; v[1 * 4 + 2] = -50; v[2 * 4 + 2] = -80; v[3 * 4 + 2] = -10;
  const tiles = new Map([[keyOf(a), tileOf(a, 4, 4, v, [3 * 4 + 2])]]);
  const box = { f0Hz: 0, f1Hz: 2.4e6, t0Ns: 0, t1Ns: 20 * S };
  const cols = maxHoldColumns(LAT, peeker(tiles) as never, box, 0, 0, "any", 4);
  assert.deepEqual([...cols], [-95, -95, -50, -95],
    "the −10 cell is UNOBSERVED: the max of nothing observed is not the biggest number in memory");
  const pk = peakOf(cols, box)!;
  near(pk.hz, 1.5e6, 1e-6);
  assert.equal(pk.db, -50);
});

test("**the property that licenses this reduction**: a max of max-holds IS a max-hold", () => {
  // `hk-api`'s MAX_HOLD_RULE: *"a cell is the maximum of the source cells folded into it, so folding
  // further never lowers a value"*. Idempotent and associative — which is why reducing the tiles a
  // pane already holds gives the number the server would have returned for one tile spanning the
  // whole box, and why this is NOT the client inventing a statistic the server did not state.
  //
  // Four level-0 tiles tile the box; one level-1 tile covers the same box, its cells built as the
  // max of the four cells beneath each. Both must produce the same trace.
  const box = { f0Hz: 0, f1Hz: 4.8e6, t0Ns: 0, t1Ns: 40 * S };
  const fine = new Map<string, TileData>();
  // A repeatable spread of values; nothing here is symmetric, so an averaging bug cannot pass.
  const val = (fi: number, ti: number, j: number, r: number) => -100 + ((fi * 37 + ti * 17 + j * 5 + r * 3) % 53);
  for (let fi = 0; fi < 2; fi++) {
    for (let ti = 0; ti < 2; ti++) {
      const a: TileAddr = { device: "any", scheme: "view", levelF: 0, levelT: 0, fIndex: fi, tIndex: ti, cells: 4 };
      const v: number[] = [];
      for (let r = 0; r < 4; r++) for (let j = 0; j < 4; j++) v.push(val(fi, ti, j, r));
      fine.set(keyOf(a), tileOf(a, 4, 4, v));
    }
  }
  // The coarse node, built the way the store builds one: each cell the max of its four children.
  const c: TileAddr = { device: "any", scheme: "view", levelF: 1, levelT: 1, fIndex: 0, tIndex: 0, cells: 4 };
  const cv: number[] = [];
  for (let r = 0; r < 4; r++) {
    for (let j = 0; j < 4; j++) {
      const fi = j >> 1, ti = r >> 1, j0 = (j & 1) * 2, r0 = (r & 1) * 2;
      cv.push(Math.max(
        val(fi, ti, j0, r0), val(fi, ti, j0 + 1, r0),
        val(fi, ti, j0, r0 + 1), val(fi, ti, j0 + 1, r0 + 1),
      ));
    }
  }
  const coarse = new Map([[keyOf(c), tileOf(c, 4, 4, cv)]]);

  // Asked at the COARSE node's own cell pitch (four 1.2 MHz columns over 4.8 MHz), which is the
  // resolution at which the two are the same question. Asking finer than the coarse node measured
  // would compare a max-hold against a replication of one, which is a different claim — and one the
  // renderer makes visible rather than smooths over (`sourceCellPx`, the survey-overview lattice).
  const atFine = maxHoldColumns(LAT, peeker(fine) as never, box, 0, 0, "any", 4);
  const atCoarse = maxHoldColumns(LAT, peeker(coarse) as never, box, 1, 1, "any", 4);
  assert.deepEqual([...atCoarse], [...atFine],
    "the ladder's own reduction and this one disagree — then one of them is not a max-hold");
  // And the control: an AVERAGE would not agree, so the test above is not vacuously true of any
  // reduction at all.
  const mean = [...atFine].reduce((s, v) => s + v, 0) / atFine.length;
  assert.ok([...atFine].some((v) => Math.abs(v - mean) > 1), "the fixture is flat — it would pass under any rule");
});

test("a column no resident tile answers for stays a GAP — a budget cannot manufacture a level", () => {
  const box = { f0Hz: 0, f1Hz: 4.8e6, t0Ns: 0, t1Ns: 20 * S };
  // Only the left half is resident; nothing at all is known about the right.
  const a: TileAddr = { device: "any", scheme: "view", levelF: 0, levelT: 0, fIndex: 0, tIndex: 0, cells: 4 };
  const cols = maxHoldColumns(LAT, peeker(new Map([[keyOf(a), tileOf(a, 4, 4, new Array(16).fill(-77))]])) as never,
    box, 0, 0, "any", 8);
  assert.deepEqual([...cols].slice(0, 4), [-77, -77, -77, -77]);
  assert.ok([...cols].slice(4).every((v) => Number.isNaN(v)),
    "an absent tile is 'not loaded', which is not 'quiet' and not the bottom of the scale");
  assert.equal(tracePaths(cols, box, STRIP, LO, HI, "trace-hold", "p", INK).length, 1,
    "four answered columns and four gaps make ONE curve over the answered half");
});

test("only rows INSIDE the window contribute — a max-hold is over the viewport's own time range", () => {
  // The tile spans 20 s in four 5 s rows; the box asks for the last 10 s only. The loud cell is in
  // the first row, outside it, and must not appear.
  const a: TileAddr = { device: "any", scheme: "view", levelF: 0, levelT: 0, fIndex: 0, tIndex: 0, cells: 4 };
  const v = new Array(16).fill(-90);
  v[0 * 4 + 1] = -20;  // row 0 = 0–5 s
  v[3 * 4 + 1] = -60;  // row 3 = 15–20 s
  const tiles = peeker(new Map([[keyOf(a), tileOf(a, 4, 4, v)]])) as never;
  const whole = maxHoldColumns(LAT, tiles, { f0Hz: 0, f1Hz: 2.4e6, t0Ns: 0, t1Ns: 20 * S }, 0, 0, "any", 4);
  const recent = maxHoldColumns(LAT, tiles, { f0Hz: 0, f1Hz: 2.4e6, t0Ns: 10 * S, t1Ns: 20 * S }, 0, 0, "any", 4);
  assert.equal(whole[1], -20, "over the whole window the loud row wins");
  assert.equal(recent[1], -60, "over the last ten seconds it is not there to win");
});

// ---------------------------------------------------------------------------
// The slice is TIME-ADDRESSABLE, viewport-wide, and absent where nothing was observed
// ---------------------------------------------------------------------------

/** One level-0 tile whose four time rows each carry a spike in a different column. */
function striped(fIndex: number, tIndex: number, base: number): [TileAddr, TileData] {
  const a: TileAddr = { device: "any", scheme: "view", levelF: 0, levelT: 0, fIndex, tIndex, cells: 4 };
  const v = new Array(16).fill(-95);
  for (let r = 0; r < 4; r++) v[r * 4 + r] = base - r; // row r peaks in column r
  return [a, tileOf(a, 4, 4, v)];
}

test("the slice is the spectrum AT THE ASKED-FOR TIME — a different instant is a different spectrum", () => {
  // Four 5 s rows over 20 s, each with its peak in a different frequency cell. Asking at each row's
  // own instant must return that row, and nothing else: a trace pinned to the newest row would
  // answer the same thing four times, and a max over the window would answer the loudest once.
  const [a, d] = striped(0, 0, -30);
  const cache = peeker(new Map([[keyOf(a), d]])) as never;
  const box = { f0Hz: 0, f1Hz: 2.4e6, t0Ns: 0, t1Ns: 20 * S };
  for (let r = 0; r < 4; r++) {
    const tAt = (r + 1) * 5 * S;            // the end of row r
    const cols = sliceColumns(LAT, cache, box, 0, 0, "any", 4, tAt);
    assert.deepEqual([...cols].map((v) => (v === -95 ? "." : v)), [".", ".", ".", "."].map((x, i) => (i === r ? -30 - r : x)),
      `the slice at ${tAt / S} s is not row ${r}`);
    assert.equal(peakOf(cols, box)!.db, -30 - r);
  }
  // …and the max-hold over the whole window is a different question with a different answer: every
  // row's peak at once. Two series, two claims — which is exactly why both are drawn.
  const hold = maxHoldColumns(LAT, cache, box, 0, 0, "any", 4);
  assert.deepEqual([...hold], [-30, -31, -32, -33]);
});

test("the slice spans the WHOLE viewport, stitched from every tile that answers", () => {
  // Two tiles side by side in frequency; the box covers both. One continuous plot, no seam.
  const [a0, d0] = striped(0, 0, -30);
  const [a1, d1] = striped(1, 0, -40);
  const both = new Map([[keyOf(a0), d0], [keyOf(a1), d1]]);
  const box = { f0Hz: 0, f1Hz: 4.8e6, t0Ns: 0, t1Ns: 20 * S };
  const tAt = 2 * 5 * S;                    // the end of row 1
  const all = sliceColumns(LAT, peeker(both) as never, box, 0, 0, "any", 8, tAt);
  assert.equal(all.filter((v) => Number.isFinite(v)).length, 8, "the whole width is answered");
  assert.equal(all[1], -31, "the left tile's row 1");
  assert.equal(all[5], -41, "the right tile's row 1, in the same plot");

  // **The absence, and the proof it is not vacuous.** Drop the right tile: those four columns must
  // go quiet — as GAPS, not as a floor and not as a line drawn across them — while the left four are
  // unchanged. Then put it back and they return. Same box, same instant, same call: the only thing
  // that differs is whether the data exists.
  const left = sliceColumns(LAT, peeker(new Map([[keyOf(a0), d0]])) as never, box, 0, 0, "any", 8, tAt);
  assert.deepEqual([...left].slice(0, 4), [...all].slice(0, 4), "losing the right tile changed the left");
  assert.ok([...left].slice(4).every((v) => Number.isNaN(v)),
    `the unobserved half is ${JSON.stringify([...left].slice(4))} — unobserved is not quiet and is not the bottom of the scale`);
  // The drawn EXTENT, in the box's own frequency terms — the question a smooth line makes sharper
  // than a quad count ever could: does the curve stop at the data, or reach across the gap?
  const reach = (c: Float32Array) => {
    const pts = pointsOf(tracePaths(c, box, STRIP, LO, HI, "trace-slice", "p", INK));
    if (!pts.length) return null;
    const xs = pts.map((p) => p.x);
    return { lo: Math.min(...xs), hi: Math.max(...xs) };
  };
  near(reach(left)!.lo, -1);
  near(reach(left)!.hi, 0,
    // the midpoint of the box: the curve must stop dead at the edge of the data, not reach into the
    // half nothing answered for.
    1e-9);
  near(reach(all)!.lo, -1);
  near(reach(all)!.hi, 1, 1e-9);   // restoring the tile makes it span the window
});

test("a slice at an instant nothing covers is entirely absent, not a flat line at the floor", () => {
  const [a, d] = striped(0, 0, -30);
  const box = { f0Hz: 0, f1Hz: 2.4e6, t0Ns: 0, t1Ns: 20 * S };
  // 100 s in: the tile spans 0–20 s, so nothing was observed at that instant.
  const cols = sliceColumns(LAT, peeker(new Map([[keyOf(a), d]])) as never, box, 0, 0, "any", 4, 100 * S);
  assert.ok([...cols].every((v) => Number.isNaN(v)));
  assert.equal(tracePaths(cols, box, STRIP, LO, HI, "trace-slice", "p", INK).length, 0);
  assert.equal(peakOf(cols, box), null, "and there is no peak to state — the readout says so instead");
});

test("T-880: at realistic EPOCH-ns times the slice is exactly ONE cell, never a max over two", () => {
  // The lattice the route states for a 10 ms finest cell, recovered the way `latticeFrom` does
  // (`cell_s * 1e9`), 256 cells a tile. Times are today's epoch in ns (~1.76e18 — past 2^53, so a
  // double holds them to 256 ns). The ground truth is computed in BigInt: cell k of the level holds
  // the value v(k), and neighbouring cells always differ, so a slice that reached into a
  // neighbouring row (the T-880 defect: floor/ceil on raw epoch-ns doubles) shows the louder one.
  const cellNsBig = 10_000_000n, cells = 256;
  const lat: Lattice = { scheme: "view", cells, f0Hz: 600_000, t0Ns: (Number(cellNsBig) / 1e9) * 1e9, levelsF: 8, levelsT: 8 };
  const nf = 4;
  const v = (k: bigint) => -100 + Number(k % 7n) * 10;
  const tiles = {
    peek(a: TileAddr) {
      const vals: number[] = [];
      for (let r = 0; r < cells; r++) {
        const k = BigInt(a.tIndex) * BigInt(cells) + BigInt(r);
        for (let j = 0; j < nf; j++) vals.push(v(k));
      }
      return { addr: a, key: keyOf(a), data: tileOf(a, nf, cells, vals) } as never;
    },
  };
  const fw = lat.f0Hz * cells;
  let wrong = 0;
  const firstK = 175_880_000_000n; // 2025-09 in 10 ms cells
  for (let i = 0n; i < 600n; i++) {
    const k = firstK + i * 37n + (i % 5n === 0n ? 255n - (firstK % 256n) : 0n); // incl. tile-edge cells
    const tAt = Number(k * cellNsBig + cellNsBig / 2n);        // mid-cell: unambiguous
    const box = { f0Hz: 0, f1Hz: fw, t0Ns: tAt - 2 * S, t1Ns: tAt };
    const cols = sliceColumns(lat, tiles, box, 0, 0, "any", nf, tAt);
    if (!cols.every((c) => c === v(k))) wrong++;
  }
  assert.equal(wrong, 0, `${wrong}/600 slices were not exactly their own cell's row`);
});

// ---------------------------------------------------------------------------
// T-475 persistenceSlices: the afterglow is a function of the PANE'S WINDOW
// ---------------------------------------------------------------------------
//
// What these are a property of: **which rows the glow is made of**, never that a glow was drawn.
// "Several fading lines appeared" is satisfied by a client-side ring buffer of whatever arrived while
// the page was open — which shows the last few seconds of WALL CLOCK over a pane scrubbed to last
// hour, the same defect as a trace pinned to now, one layer down. So every test below moves the
// pane's time position and asserts the VALUES change with it.

/** Four 5 s rows over 2.4 MHz, each row flat at its own dB, so a row is identifiable by value. */
const ROWS = [-90, -80, -70, -60];
function rowTile() {
  const a: TileAddr = { device: "any", scheme: "view", levelF: 0, levelT: 0, fIndex: 0, tIndex: 0, cells: 4 };
  const v: number[] = [];
  for (const db of ROWS) for (let c = 0; c < 4; c++) v.push(db);
  return peeker(new Map([[keyOf(a), tileOf(a, 4, 4, v)]]));
}
const ROW_BOX = { f0Hz: 0, f1Hz: 2.4e6, t0Ns: 0, t1Ns: 20 * S };
const glowAt = (tAtNs: number, box = ROW_BOX) =>
  persistenceSlices(LAT, rowTile() as never, box, 0, 0, "any", 4, tAtNs);

test("the afterglow is the rows just before the PANE'S instant — and it MOVES when the pane does", () => {
  // At the edge, the slice is the newest row (−60) and the glow is the three before it, newest first.
  const live = glowAt(20 * S);
  assert.deepEqual(live.map((g) => g.cols[0]), [-70, -80, -90]);
  assert.deepEqual(live.map((g) => g.age), [1, 2, 3]);
  assert.deepEqual(live.map((g) => g.tAtNs / S), [15, 10, 5], "each shadow states the instant its cell ends");
  // **"Four EARLIER rows, not four copies of this one"**, stated as the equality it is. This is the
  // half `ui/e2e/app-trace.e2e.mjs` deliberately does NOT assert from pixels: a shadow that coincides
  // with the current line is drawn under a wider opaque core and is invisible by construction, so how
  // far the glow separates on screen is a fact about how much the band moved, not about where the
  // rows came from. Two pixel thresholds over that were written and both measured the fixture's
  // liveliness instead (27/25/7 columns on identical runs; then a spread that collapsed when a 40 ms
  // row became a 1 s one). Here it is deterministic.
  const sliceNow = sliceColumns(LAT, rowTile() as never, ROW_BOX, 0, 0, "any", 4, 20 * S);
  assert.equal(sliceNow[0], -60, "the slice is the newest row");
  assert.ok(!live.some((g) => g.cols[0] === sliceNow[0]), "a shadow is repeating the current row");
  assert.equal(new Set(live.map((g) => g.cols[0])).size, live.length, "two shadows are the same row");

  // **Scrubbed back one cell — this is the whole claim.** The glow is now the rows before THAT
  // instant, and the newest row (−60) is nowhere in it: it is in the pane's future. A buffer of
  // recently-arrived frames would still be glowing with −60 here, and would look entirely convincing.
  const past = glowAt(15 * S);
  assert.deepEqual(past.map((g) => g.cols[0]), [-80, -90]);
  assert.ok(!past.some((g) => g.cols[0] === -60),
    "a viewport in the past is glowing with a row it is not showing — that is now, drawn behind then");
  // …and the two are genuinely different answers to the same call, which is what makes the first
  // assertion non-vacuous: the function is reading its `tAtNs` argument, not a constant.
  assert.notDeepEqual(live.map((g) => g.cols[0]), past.map((g) => g.cols[0]));
});

test("a shadow whose row is outside the pane's window is DROPPED, not clamped", () => {
  // The strip is a view over this pane's window. Glowing with a row the pane is not showing would be
  // the gap rule inverted — data drawn where the viewport says it is not looking.
  const narrow = glowAt(20 * S, { ...ROW_BOX, t0Ns: 10 * S });
  assert.deepEqual(narrow.map((g) => g.cols[0]), [-70],
    "only the one earlier row that is inside the window may glow");
  assert.deepEqual(glowAt(10 * S).map((g) => g.cols[0]), [-90]);
  assert.deepEqual(glowAt(5 * S).map((g) => g.cols[0]), [],
    "the oldest row is the slice itself: there is nothing before it in this window to glow");
});

test("the glow FADES, and a row nothing answered for does not glow at all", () => {
  const live = glowAt(20 * S);
  for (let i = 1; i < live.length; i++) {
    assert.ok(live[i].alpha < live[i - 1].alpha, "the afterglow must decay with age, not hold");
  }
  assert.ok(live[0].alpha < 0.5, "the newest shadow must stay well under the current trace's weight");
  assert.deepEqual(live.map((g) => g.alpha), SHADOW_ALPHA.slice(0, live.length));
  // No tile resident: the honest answer is no glow. A floor, or the newest row repeated, would each
  // be a claim about a row nobody has.
  assert.deepEqual(persistenceSlices(LAT, peeker(new Map()) as never, ROW_BOX, 0, 0, "any", 4, 20 * S), []);
});

test("an absent afterglow says NOT LOADED when the pane is not in hand, and 'no earlier row' only when it is", () => {
  // `persistenceSlices` reads resident tiles only, so "no glow" is either a fact about the radio
  // (everything in hand, nothing observed before this instant) or a fact about memory and latency.
  // The gate read "no earlier row in this window" off a pane still fetching most of its window.
  assert.equal(afterglowAbsence({ pending: 0, fallbacks: 0, refused: 0 }), "afterglow — no earlier row in this window");
  for (const r of [{ pending: 3, fallbacks: 0, refused: 0 }, { pending: 0, fallbacks: 1, refused: 0 }, { pending: 0, fallbacks: 0, refused: 2 }]) {
    const said = afterglowAbsence(r);
    assert.doesNotMatch(said, /no earlier row/, `a pane with ${JSON.stringify(r)} not in hand claimed there was nothing before`);
    assert.match(said, /not loaded is not unobserved/);
  }
  assert.equal(afterglowAbsence({ pending: 3, fallbacks: 1, refused: 0 }),
    "afterglow — the rows before this instant are not all in hand yet (3 pending, 1 coarse stand-in) — not loaded is not unobserved");
  // In hand, but answered only up to an instant before the afterglow's cells (T-532's horizon).
  assert.match(afterglowAbsence({ pending: 0, fallbacks: 0, refused: 0 }, 1),
    /not all in hand yet \(0 pending, 0 coarse stand-ins, 1 answered only up to an earlier instant\)/);
});

test("an afterglow cell a resident copy's HORIZON does not reach is counted as not in hand", () => {
  // Tile 0 spans 0..20 s in four 5 s rows. A slice at 20 s is the cell [15, 20); its afterglow is the
  // cells [0, 15). A copy answered only up to 10 s cannot speak about [10, 15) — the tile-boundary
  // case, where the rows before a slice sit at the END of a tile fetched while it was live.
  const a: TileAddr = { device: "any", scheme: "view", levelF: 0, levelT: 0, fIndex: 0, tIndex: 0, cells: 4 };
  const withHorizon = (asOfNs: number | null) => {
    const v: number[] = [];
    for (const db of ROWS) for (let c = 0; c < 4; c++) v.push(db);
    return peeker(new Map([[keyOf(a), { ...tileOf(a, 4, 4, v), asOfNs }]])) as never;
  };
  const short = (asOfNs: number | null, tAtNs = 20 * S, box = ROW_BOX) =>
    persistenceShortTiles(LAT, withHorizon(asOfNs), box, 0, 0, "any", tAtNs);
  assert.equal(short(10 * S), 1, "a copy answered to 10 s was counted as reaching the afterglow's cells up to 15 s");
  assert.equal(short(15 * S), 0, "a copy that reaches the slice's own cell covers every afterglow cell");
  assert.equal(short(null), 0, "no stated horizon reaches everywhere, as in the renderer's clip");
  assert.equal(short(10 * S, 10 * S), 0, "a slice at 10 s has its afterglow in [0, 5): the horizon reaches it");
  assert.equal(short(10 * S, 5 * S), 0, "the oldest row has no afterglow inside the window at all");
  assert.equal(persistenceShortTiles(LAT, peeker(new Map()) as never, ROW_BOX, 0, 0, "any", 20 * S), 0,
    "a tile not in hand is the PaneReport's pending/stand-in count, not this one's");
});

// ---------------------------------------------------------------------------
// tracePaths: three readings of ONE scale — x, y, and (T-475) colour
// ---------------------------------------------------------------------------

test("the x mapping is the DATA PASS's own toClip — not arithmetic that resembles it", () => {
  const n = 8;
  const cols = new Float32Array(n).fill(-70);
  const pts = pointsOf(tracePaths(cols, PANE, STRIP, LO, HI, "trace-slice", "p", { ...INK, subdiv: 1 }));
  const colHz = (PANE.f1Hz - PANE.f0Hz) / n;
  // Every column's CENTRE is a point on the curve, at the x `toClip` puts that column at.
  for (let c = 0; c < n; c++) {
    const [x0, , x1] = toClip(
      { f0Hz: PANE.f0Hz + c * colHz, f1Hz: PANE.f0Hz + (c + 1) * colHz, t0Ns: PANE.t0Ns, t1Ns: PANE.t1Ns }, PANE);
    const mid = (x0 + x1) / 2;
    assert.ok(pts.some((p) => Math.abs(p.x - mid) < 1e-9),
      `no point at column ${c}'s centre (${mid}), where toClip places it`);
  }
  // The curve tiles the pane with no gap and no overlap, so the trace is over the whole window.
  near(Math.min(...pts.map((p) => p.x)), -1);
  near(Math.max(...pts.map((p) => p.x)), 1);
  const src = readFileSync("src/surface/trace.ts", "utf8");
  assert.match(src, /import \{ toClip/, "and it imports it rather than reimplementing it");
});

test("the y mapping is the SURFACE's one measured range — the same lo/hi the ramp is relative to", () => {
  const yOf = (db: number, lo = LO, hi = HI) => {
    const pts = pointsOf(tracePaths(Float32Array.from([db]), PANE, STRIP, lo, hi, "trace-slice", "p", INK));
    return pts[0].y;
  };
  near(yOf(LO), -1, 1e-9);                 // the floor of the scale sits on the floor of the strip
  near(yOf(HI), 1, 1e-9);                  // and its top on the top
  near(yOf((LO + HI) / 2), 0, 1e-9);       // linear in between, like the ramp
  assert.ok(yOf(-70) < yOf(-50), "louder is higher");
  // Moving the range moves the trace: it is reading the surface's numbers, not carrying its own.
  assert.notEqual(yOf(-70), yOf(-70, -90, -30));
  // Out of range clamps into the strip rather than drawing outside it or vanishing.
  near(yOf(-200), -1, 1e-9);
  near(yOf(0), 1, 1e-9);
  assert.equal(levelFrac(-200, LO, HI), 0);
  assert.equal(levelFrac(0, LO, HI), 1);
});

test("T-475: a vertex's COLOUR is the waterfall's ramp at the SAME normalisation the tile shader uses", () => {
  // The user's ask, stated as an equality rather than an impression: "the same dB must be the same
  // colour on the trace as in the cells beneath it". The tile shader computes
  // `cellMark(s, (v - uLo) / max(uHi - uLo, 1e-6), px)`, and for an OBSERVED cell `cellMark` is
  // `cmap(x)` — so the claim is that a trace vertex at `db` is `cmap((db - lo) / (hi - lo))`, from
  // the SAME module, with nothing in between.
  const dbs = [-110, -100, -92.5, -85, -70, -55, -44, -40];
  const pts = pointsOf(tracePaths(Float32Array.from(dbs), PANE, STRIP, LO, HI, "trace-slice", "p", { subdiv: 1 }));
  for (const db of dbs) {
    const want = cmap((db - LO) / (HI - LO));
    const got = pts.find((p) => Math.abs(dbOf(p.y) - db) < 1e-6);
    assert.ok(got, `no vertex at ${db} dB`);
    for (let k = 0; k < 3; k++) near(got.rgb[k], want[k], 1e-6);
  }
  // …and it really is the ramp and not a coincidence: the ramp's ends and middle are different
  // colours, and the trace reproduces that difference.
  const lowest = pts.find((p) => Math.abs(dbOf(p.y) - -110) < 1e-6)!;
  const highest = pts.find((p) => Math.abs(dbOf(p.y) - -40) < 1e-6)!;
  for (let k = 0; k < 3; k++) { near(lowest.rgb[k], cmap(0)[k], 1e-6); near(highest.rgb[k], cmap(1)[k], 1e-6); }
  assert.notDeepEqual(lowest.rgb, highest.rgb, "the ramp's ends are different colours, and so is the trace's");
  // The ONE definer rule (T-397/T-445) holds with nothing weakened: the colour is imported, not
  // re-stated. `ui/test/surface-cutover.test.ts` holds the repo-wide half of this.
  const src = readFileSync("src/surface/trace.ts", "utf8");
  assert.match(src, /import \{ cmap \} from "\.\.\/cmap"/, "the ramp must be imported, never copied");
  assert.ok(!/0\.05\s*,\s*0\.1\s*,\s*0\.55/.test(src), "trace.ts must not carry stops of its own");
});

test("T-475: the colour follows the RANGE, because the cells below it do", () => {
  // The same dB against a different measured range is a different colour in the waterfall (T-470's
  // whole subject), so it must be a different colour on the trace. A trace that had cached a colour,
  // or carried a scale of its own, would hold still here while the picture under it changed.
  const one = pointsOf(tracePaths(Float32Array.from([-70]), PANE, STRIP, LO, HI, "t", "p", {}))[0];
  const two = pointsOf(tracePaths(Float32Array.from([-70]), PANE, STRIP, -90, -30, "t", "p", {}))[0];
  assert.notDeepEqual(one.rgb, two.rgb);
  const want = cmap((-70 - -90) / (-30 - -90));
  for (let k = 0; k < 3; k++) near(two.rgb[k], want[k], 1e-6);
});

// ---------------------------------------------------------------------------
// T-475: the line is SMOOTH — and it is smooth THROUGH THE MEASUREMENTS
// ---------------------------------------------------------------------------
//
// The trap named in the brief: *a trace that looks smoother is not a trace that is drawing the same
// measurements.* Every test here therefore asserts the VALUES the curve passes through. Vertex
// counts appear only where the claim is literally about density, and never on their own.

test("every measured column is ON the curve — smoothing adds points, it does not move samples", () => {
  const dbs = [-100, -95, -60, -44, -70, -88, -86, -99];
  const cols = Float32Array.from(dbs);
  const pts = pointsOf(tracePaths(cols, PANE, STRIP, LO, HI, "trace-slice", "p", INK));
  const n = cols.length;
  const colHz = (PANE.f1Hz - PANE.f0Hz) / n;
  for (let c = 0; c < n; c++) {
    const [x0, , x1] = toClip(
      { f0Hz: PANE.f0Hz + c * colHz, f1Hz: PANE.f0Hz + (c + 1) * colHz, t0Ns: PANE.t0Ns, t1Ns: PANE.t1Ns }, PANE);
    const at = pts.filter((p) => Math.abs(p.x - (x0 + x1) / 2) < 1e-9);
    assert.equal(at.length, 1, `column ${c} should contribute exactly one sample point`);
    near(dbOf(at[0].y), dbs[c], 1e-4);
  }
});

test("the curve NEVER OVERSHOOTS its bracketing samples — a monotone cubic, not a ringing spline", () => {
  // A Catmull-Rom through a flat noise floor with one spike rings BELOW the floor on either side of
  // it — a quieter measurement than the radio reported, at a frequency where it reported something
  // else. That is inventing a value, which is the one thing smoothing may not do. Fritsch–Carlson
  // cannot: the interpolant is monotone wherever the data is.
  const dbs = [-100, -100, -100, -42, -100, -100, -100, -100];
  const cols = Float32Array.from(dbs);
  const paths = tracePaths(cols, PANE, STRIP, LO, HI, "trace-slice", "p", INK);
  const pts = pointsOf(paths);
  const n = cols.length;
  const colHz = (PANE.f1Hz - PANE.f0Hz) / n;
  const centre = (c: number) => {
    const [x0, , x1] = toClip(
      { f0Hz: PANE.f0Hz + c * colHz, f1Hz: PANE.f0Hz + (c + 1) * colHz, t0Ns: PANE.t0Ns, t1Ns: PANE.t1Ns }, PANE);
    return (x0 + x1) / 2;
  };
  for (const p of pts) {
    // Which interval is this point in, and what do its two ends measure?
    let i = 0;
    while (i < n - 2 && p.x > centre(i + 1) + 1e-12) i++;
    const a = Math.min(dbs[i], dbs[i + 1]), b = Math.max(dbs[i], dbs[i + 1]);
    const db = dbOf(p.y);
    assert.ok(db >= a - 1e-4 && db <= b + 1e-4,
      `a point at ${db.toFixed(2)} dB sits outside the ${a}…${b} dB its neighbours measured — ` +
      "the smoothing invented a value");
  }
  // The floor is never breached ANYWHERE, which is the same claim said the blunt way.
  assert.ok(pts.every((p) => dbOf(p.y) >= -100 - 1e-4), "the curve dipped below the measured floor");
  assert.ok(pts.some((p) => Math.abs(dbOf(p.y) - -42) < 1e-4), "…and the spike is still on it");
});

test("the staircase is GONE: consecutive points step gently, where per-column quads jumped", () => {
  // The user's report was "looks like pixels", and a per-column quad trace is literally a flight of
  // stairs: each column is drawn at one constant height, so the whole change between two columns
  // happens in a single vertical edge. This asserts the shape rather than the vertex count — the
  // largest step between two consecutive points must be a fraction of the largest step between two
  // consecutive COLUMNS, which is the size of the staircase's riser.
  const dbs = [-100, -90, -80, -70, -60, -50, -44, -40];
  const cols = Float32Array.from(dbs);
  const pts = pointsOf(tracePaths(cols, PANE, STRIP, LO, HI, "trace-slice", "p", INK));
  let biggestPoint = 0;
  for (let i = 1; i < pts.length; i++) biggestPoint = Math.max(biggestPoint, Math.abs(pts[i].y - pts[i - 1].y));
  let biggestColumn = 0;
  for (let c = 1; c < dbs.length; c++) {
    biggestColumn = Math.max(biggestColumn, Math.abs(levelFrac(dbs[c], LO, HI) - levelFrac(dbs[c - 1], LO, HI)) * 2);
  }
  assert.ok(biggestPoint < biggestColumn / 2,
    `the largest step on the curve is ${biggestPoint} of a riser of ${biggestColumn} — still a staircase`);
  // Density, said as what it is: points per measured interval, which is TRACE_SUBDIV by definition.
  assert.ok(pts.length >= (dbs.length - 1) * TRACE_SUBDIV, `${pts.length} points over ${dbs.length} columns`);
  // The control: with no subdivision it IS the polyline through the samples, and the biggest step
  // is back to a riser. So the test above is measuring the smoothing, not the y mapping.
  const raw = pointsOf(tracePaths(cols, PANE, STRIP, LO, HI, "trace-slice", "p", { ...INK, subdiv: 1 }));
  let biggestRaw = 0;
  for (let i = 1; i < raw.length; i++) biggestRaw = Math.max(biggestRaw, Math.abs(raw[i].y - raw[i - 1].y));
  assert.ok(biggestRaw > biggestPoint * 2, "the comparison is vacuous — subdivision changed nothing");
});

test("a lone column between two gaps is a tick ACROSS ITS OWN EXTENT — not a point, not a reach", () => {
  const cols = Float32Array.from([Number.NaN, -60, Number.NaN, -80, Number.NaN]);
  const paths = tracePaths(cols, PANE, STRIP, LO, HI, "trace-slice", "p", INK);
  assert.equal(paths.length, 2, "two answered columns, two gaps between them, two curves");
  const colW = 2 / 5;
  for (const [i, want] of [[0, -60], [1, -80]] as const) {
    const pts = pointsOf([paths[i]]);
    assert.ok(pts.every((p) => Math.abs(dbOf(p.y) - want) < 1e-4), "a lone sample is flat at its own value");
    near(Math.max(...pts.map((p) => p.x)) - Math.min(...pts.map((p) => p.x)), colW, 1e-6);
  }
});

test("a NaN column draws NOTHING, and an unchanged input draws BYTE-IDENTICAL geometry", () => {
  const cols = Float32Array.from([-70, Number.NaN, -60, Number.NaN]);
  const a = tracePaths(cols, PANE, STRIP, LO, HI, "trace-hold", "p", INK);
  assert.equal(a.length, 2);
  assert.deepEqual(a.map((x) => x.kind), ["trace-hold", "trace-hold"]);
  assert.deepEqual(tracePaths(cols, PANE, STRIP, LO, HI, "trace-hold", "p", INK), a);
});

test("a trace is a STROKE with a width, so it can never be a wash over the pane it describes", () => {
  const cols = new Float32Array(256).fill(-70);
  for (const p of tracePaths(cols, PANE, STRIP, LO, HI, "trace-slice", "p", { widthPx: 2.2 })) {
    assert.ok(p.widthPx > 0 && p.widthPx <= 8, `a ${p.widthPx} px stroke is a fill, not a line`);
  }
  // The max-hold is off the ramp entirely, so the two series are still tellable apart by colour —
  // which `ui/e2e/app-trace.e2e.mjs` relies on to separate them in the framebuffer.
  const [hr, hg, hb] = HOLD_INK;
  for (let x = 0; x <= 1.0001; x += 0.02) {
    const [r, g, b] = cmap(x);
    assert.ok(Math.hypot(r - hr, g - hg, b - hb) > 0.25,
      `the max-hold's ink is on the ramp at x=${x.toFixed(2)} — the two series would be confusable`);
  }
  // **And apart in the one form that survives ANTI-ALIASING** (T-532), which the distance above does
  // not. The browser tier reads composited pixels, and a stroke's outermost pixel is a partial blend
  // of its ink with whatever is under it: the distance to the ramp shrinks toward zero along the
  // feather, and so does any *ratio* between channels, because the framebuffer is sRGB-encoded and
  // that encoding does not preserve ratios under partial coverage. A ratio rule (`b > 1.3 g`) read
  // the max-hold's own top edge as the current slice in 9 of 24 runs under load.
  //
  // What blending cannot change is the SIGN of `r - g` and `b - g`, because every other series on
  // the strip is achromatic or near it and sRGB encoding is monotonic. `HOLD_INK` is magenta, so
  // both are positive for it; the assertion is that they are never both positive on the ramp — red
  // above green only at the ramp's yellow-to-white end, where blue is far below it, and blue above
  // green only at its black-to-cyan end, where red is below it.
  for (let x = 0; x <= 1.0001; x += 0.005) {
    const [r, g, b] = cmap(x);
    assert.ok(!(r > g && b > g),
      `the ramp at x=${x.toFixed(3)} is (${r.toFixed(3)}, ${g.toFixed(3)}, ${b.toFixed(3)}) — red and ` +
      "blue both above green, which is the max-hold's own shape. `ui/e2e/app-trace.e2e.mjs` " +
      "separates the two series in a composited framebuffer by exactly that ordering, and it would " +
      "now read this ramp colour as the max-hold.");
  }
  assert.ok(hr > hg && hb > hg, "the max-hold's ink must have that shape for the rule to be about it");
}); 

// ---------------------------------------------------------------------------
// The trace is a LAYER OVER the pane's top rows and reserves nothing (T-1041)
// ---------------------------------------------------------------------------

const BOUNDS = { f0Hz: 1e6, f1Hz: 6e9, t0Ns: T0 - 86_400 * S, t1Ns: T0 };
const VIEW_LAT: Lattice = { scheme: "view", cells: 256, f0Hz: 6250, t0Ns: 1e9, levelsF: 20, levelsT: 15 };

function stubTile(a: TileAddr): TileData {
  return tileOf(a, 2, 2, [-90, -80, -70, -60]);
}

function viewWith(tracePx: number, trace: SurfaceViewTrace | null) {
  const g = stubGl(1200, 600);
  const view = new SurfaceView({
    canvas: g.canvas, lattice: VIEW_LAT, bounds: BOUNDS, minimapPx: 150,
    cache: (tex) => new TileCache(tex, (a) => Promise.resolve(stubTile(a)), { inFlight: 64, now: () => 0 }),
    surface: { pinParents: false },
    freq: { centerHz: 100.8e6, spanHz: 2.4e6 }, spanNs: 20 * S,
    trace, tracePx,
  });
  return { g, view };
}
type SurfaceViewTrace = NonNullable<ConstructorParameters<typeof SurfaceView>[0]["trace"]>;

test("the trace layer RESERVES NO SPACE — the pane keeps its first row, band or no band", () => {
  // T-457 carved the band off the pane's rectangle so that "the top of the pane is the newest row"
  // stayed true of the pixels. The user's full-bleed ruling (2026-09-25) is that the reserved band
  // reads as a black bar across the top of the map and costs the waterfall its newest rows — a
  // worse failure of the same rule. So the pane keeps every pixel, and the band is drawn INSIDE it.
  const seen: { paneId: string; strip: PaneRect; levelF: number; levelT: number; paneRect: PaneRect }[] = [];
  const { view } = viewWith(96, (pane, _edge, report, strip) => {
    seen.push({ paneId: pane.id, strip, levelF: report.levelF, levelT: report.levelT, paneRect: pane.rect });
    return tracePaths(new Float32Array(32).fill(-70), pane.box, strip, LO, HI, "trace-slice", pane.id);
  });
  const f = view.frame(T0, []);
  assert.equal(seen.length, view.panes.count, "once per pane, and not for the minimap");
  assert.equal(f.traces.length, view.panes.count);
  for (const s of seen) {
    const pane = f.views.find((v) => v.id === s.paneId)!;
    assert.deepEqual(s.paneRect, pane.rect, "the callback is handed the rectangle the data pass drew into");
    assert.equal(s.strip.h, 96);
    assert.equal(s.strip.w, pane.rect.w);
    assert.equal(s.strip.x, pane.rect.x);
    // GL origin is bottom-left, so the pane's TOP rows are the last 96 px of its own rectangle.
    assert.equal(s.strip.y, pane.rect.y + pane.rect.h - 96, "the band is the pane's own top rows");
    assert.equal(s.strip.y + s.strip.h, pane.rect.y + pane.rect.h, "…flush with the pane's top edge");
  }
  // And the pane really did keep the space rather than giving it up: same rectangle, trace or none.
  const { view: noTrace } = viewWith(0, null);
  const tall = noTrace.frame(T0, []).views.find((v) => v.id !== noTrace.minimap.id)!;
  const short = f.views.find((v) => v.id !== view.minimap.id)!;
  assert.deepEqual(short.rect, tall.rect, "switching the trace on moved the pane's rectangle");
  // The level the trace reduces at is the level the picture under it was drawn at.
  const report = f.reports.find((r) => r.id === seen[0].paneId)!;
  assert.equal(seen[0].levelF, report.levelF);
  assert.equal(seen[0].levelT, report.levelT);
  view.dispose();
  noTrace.dispose();
});

test("the data pass is byte-identical with the trace on and off — a trace cannot tint a measurement", () => {
  const run = (on: boolean) => {
    const { g, view } = viewWith(on ? 96 : 0,
      on ? (pane, _e, _r, strip) => tracePaths(new Float32Array(64).fill(-70), pane.box, strip, LO, HI, "trace-slice", pane.id) : null);
    view.frame(T0, []);
    // The data program is the one with a sampler and a display range; neither `overlay.ts`'s nor
    // `tracepass.ts`'s has one. Only the uniforms of the *data* draws are compared.
    //
    // T-475 is why this test is worth more than it was. The trace now carries a MEASUREMENT COLOUR —
    // the same ramp as the cells — so "a trace cannot tint a measurement" can no longer rest on the
    // pass being incapable of colour. It rests on this: the tile draws were all submitted before the
    // trace program was ever bound, and they are bit-for-bit the same whether or not it runs.
    const ops = g.ops.filter((o) => o.kind === "draw" && o.u && "uLo" in o.u).map((o) => JSON.stringify(o.u));
    view.dispose();
    return ops;
  };
  // Since T-1041 the pane is the same rectangle either way, so this is now an equality of the
  // whole uniform set in all but name; it is still compared on the colour decision — same ramp,
  // same range, same tier, same fallback marks — because that is the claim being made.
  const colourOf = (ops: string[]) => ops.map((s) => {
    const u = JSON.parse(s) as Record<string, unknown>;
    return JSON.stringify({ uLo: u.uLo, uHi: u.uHi, uKind: u.uKind, uTier: u.uTier, uFallback: u.uFallback, uFlat: u.uFlat });
  });
  assert.deepEqual(colourOf(run(true)), colourOf(run(false)));
});

test("EACH split pane gets its own trace, at its OWN time position", () => {
  // The user's third addition: the trace is per viewport, like the marks are. So two panes looking
  // at two different instants must be handed two different time positions and two different strips —
  // and "the pane's time position" must come from the pane's own box, never from a shared clock.
  const seen: { id: string; tAtNs: number; strip: PaneRect }[] = [];
  const { view } = viewWith(72, (pane, _e, _r, strip) => {
    seen.push({ id: pane.id, tAtNs: pane.box.t1Ns, strip });
    return [];
  });
  const first = view.panes.list()[0].id;
  const second = view.panes.split(first, "columns")!;
  // Freeze the second one a minute back. `pause` is a coordinate change (T-442), so this is just a
  // different window — not a different mode, and not a second clock.
  view.panes.pause(second, T0);
  view.panes.goTo(second, T0 - 60 * S);
  const f = view.frame(T0, []);

  assert.equal(seen.length, 2, "one trace per pane");
  const a = seen.find((s) => s.id === first)!, b = seen.find((s) => s.id === second)!;
  assert.equal(a.tAtNs, T0, "the following pane's time position is the growing edge");
  assert.ok(b.tAtNs < T0 - 30 * S, `the frozen pane's time position is its own window's top, not the edge (${b.tAtNs})`);
  // Two bands, side by side, each over the top rows of its own pane.
  assert.notEqual(a.strip.x, b.strip.x);
  for (const s of seen) {
    const pane = f.views.find((v) => v.id === s.id)!;
    assert.equal(s.strip.w, pane.rect.w);
    assert.equal(s.strip.y + s.strip.h, pane.rect.y + pane.rect.h, "each band sits over its own pane's top rows");
    assert.ok(s.strip.y >= pane.rect.y, "…and inside it");
  }
  assert.equal(f.traces.length, 2);
  view.dispose();
});

// ---------------------------------------------------------------------------
// The trace is a READOUT, not a control: it must not swallow a gesture
// ---------------------------------------------------------------------------

/**
 * **The regression test for the T-457 × T-458 merge break, written to need neither ticket.**
 *
 * Both branches were green alone. T-457 carved a strip off the top of each pane's rectangle;
 * `SurfacePreview.paneAt` walked the *drawn* pane rects; so every pointer that came down in the
 * strip resolved to no pane and `input.ts` dropped the gesture. T-458's region stroke committed
 * nothing — but so did a **plain** drag and an **alt** drag, which are T-456's settled bindings and
 * have nothing to do with regions. One ticket changed the geometry another ticket's gestures are
 * measured in.
 *
 * The property below is stated in terms of neither gesture, which is the point: **turning the trace
 * on may not shrink the set of points a gesture can start from.** Since T-1041 the trace carves
 * nothing out, so it holds structurally rather than by a second lookup — but the property is about
 * any decoration, not about this one, so it stays: a future band that took the space back would
 * fail it without knowing that region strokes, alt-drags or T-458 exist at all.
 */
function grid(w: number, h: number, step = 7): { x: number; y: number }[] {
  const pts: { x: number; y: number }[] = [];
  for (let y = 1; y < h; y += step) for (let x = 1; x < w; x += step) pts.push({ x, y });
  return pts;
}

test("turning the trace ON may not make any point unreachable — the trace is not a hole", () => {
  const W = 1200, H = 600;
  const frameOf = (tracePx: number) => {
    const { view } = viewWith(tracePx, tracePx > 0 ? () => [] : null);
    const f = view.frame(T0, []);
    const mapId = view.minimap.id;
    const resolved = new Map(grid(W, H).map((p) => [`${p.x},${p.y}`, paneAtPoint(f, mapId, p)]));
    view.dispose();
    return { f, mapId, resolved };
  };
  const off = frameOf(0), on = frameOf(96);

  const lost: string[] = [];
  for (const [k, id] of off.resolved) if (id !== null && on.resolved.get(k) === null) lost.push(k);
  assert.deepEqual(lost, [],
    `${lost.length} points could start a gesture without the trace and cannot with it ` +
    `(first: ${lost[0]}). A decoration carved out of a pane must pass pointers through to it.`);

  // And the band specifically resolves to ITS OWN pane, not merely to some pane: on a split, a
  // band that answered with its neighbour's id would pan the wrong viewport.
  const trace = on.f.traces[0];
  assert.ok(trace, "no trace band in the frame");
  for (const p of [
    { x: trace.rect.x + 1, y: trace.rect.y + 1 },
    { x: trace.rect.x + trace.rect.w - 2, y: trace.rect.y + trace.rect.h - 1 },
    { x: trace.rect.x + trace.rect.w / 2, y: trace.rect.y + trace.rect.h / 2 },
  ]) {
    assert.equal(paneAtPoint(on.f, on.mapId, p), trace.id, `a point in the band at ${JSON.stringify(p)}`);
  }
});

test("a point in the BAND reads the row under it — the band is over the pane, not instead of it", () => {
  // The T-457 form of this asserted the clamp: a point in the reserved strip became a point on the
  // pane's top edge, because the strip was not the pane. With the band drawn over the pane's own
  // rows (T-1041) there is nothing to clamp — the point is already in the pane, and the instant it
  // reads is the instant of the row genuinely under the cursor, which is the stronger answer.
  const { view } = viewWith(96, () => []);
  const f = view.frame(T0, []);
  const pane = f.views.find((v) => v.id !== view.minimap.id)!;
  const strip = f.traces[0];
  // Halfway up the band, three-quarters across.
  const raw = { x: strip.rect.x + strip.rect.w * 0.75, y: strip.rect.y + strip.rect.h / 2 };
  const at = clampToRect(pane.rect, raw);
  assert.deepEqual(at, raw, "a point in the band is already a point in its pane: nothing is clamped");
  const on = pointOn(pane.box, pane.rect, at.x, at.y);
  // Its instant is inside the pane's window and within the band's own depth of the newest row.
  const perPx = (pane.box.t1Ns - pane.box.t0Ns) / pane.rect.h;
  assert.ok(on.tNs <= pane.box.t1Ns && on.tNs >= pane.box.t1Ns - 96 * perPx,
    `the band reads a row in the pane's newest 96 px (${on.tNs} vs ${pane.box.t1Ns})`);
  // The same frequency the top-edge point would have read, so a hover in the band names the column
  // under the cursor rather than an offset one.
  near(on.fHz, pointOn(pane.box, pane.rect, raw.x, pane.rect.y).fHz, 1e-6);
  view.dispose();
});

test("tracePx 0 draws no trace and reserves nothing — the toggle is a layer, not a layout", () => {
  let called = 0;
  const { view } = viewWith(0, () => { called++; return []; });
  const f = view.frame(T0, []);
  assert.equal(called, 0, "a trace that is off is not computed and then discarded");
  assert.deepEqual(f.traces, []);
  view.dispose();
});

// ---------------------------------------------------------------------------
// The module boundary
// ---------------------------------------------------------------------------

test("no signal logic, no clock and no route in the trace module", () => {
  const src = readFileSync("src/surface/trace.ts", "utf8").replace(/\/\*[\s\S]*?\*\//g, "").replace(/^\s*\/\/.*$/gm, "");
  for (const word of ["Date.now", "performance.now", "toLocaleTimeString", "getTimezoneOffset", "fetch(", "/api/", "localStorage"]) {
    assert.ok(!src.includes(word), `trace.ts must not contain "${word}"`);
  }
  assert.ok(!/inventory|explanation|classif|demod|modulation/i.test(src),
    "it decides nothing about what a signal IS — it pools numbers onto columns");
  // No RF constant: a trace module that named a frequency would be deciding where to look.
  for (const m of src.matchAll(/(\d[\d_]*(?:\.\d+)?)e(\d+)/g)) {
    assert.ok(["1e-6"].includes(m[0]), `trace.ts names an RF-looking constant: ${m[0]}`);
  }
});

test("T-475: the trace's own pass has NO SAMPLER — it can colour, but it can never read a tile", () => {
  // The property T-457 used to get from `overlay.ts` having no ramp, re-established where it now
  // has to live. A trace that carries a measurement colour cannot be drawn by a program incapable of
  // one, so the argument "it cannot tint a measurement because it cannot express a colour" is gone.
  // What replaces it is stronger and is checked here: the trace program cannot READ a measurement,
  // cannot express `cellrule.ts`'s grey, its tier hatching or its fallback mark, and takes its colour
  // as a vertex attribute computed by `trace.ts` from the ONE ramp module. The repo-wide half — that
  // no second module defines a ramp — is `ui/test/surface-cutover.test.ts`'s, and it walks `src/`,
  // so it already covers this file.
  const src = readFileSync("src/surface/tracepass.ts", "utf8");
  const bare = src.replace(/\/\*[\s\S]*?\*\//g, "").replace(/^\s*\/\/.*$/gm, "");
  assert.ok(!/sampler2D|texture\(/.test(bare), "the trace program must have no sampler");
  assert.ok(!/vec3 cmap\(float|CELL_RULE_GLSL|cellMark|tierMark|fallbackMark/.test(bare),
    "…and no cell rule: it cannot draw a grey, a tier mark or a fallback hatch");
  assert.match(bare, /in vec4 aRgba/, "its colour arrives as a vertex attribute, not as a ramp it owns");
  // And the data pass is not reachable from it: it binds no texture and shares no display range.
  assert.ok(!/uLo|uHi|bindTexture/.test(bare), "the trace pass must not touch the data pass's state");
});

test("the renderer's one display range is what the trace reads — Surface still owns it", () => {
  const g = stubGl(64, 64);
  const s = new Surface(g.canvas, VIEW_LAT, (tex) => new TileCache(tex, (a) => Promise.resolve(stubTile(a)), { now: () => 0 }));
  s.setScale(-100, -50);
  assert.equal(s.autoScale, false);
  // `setScale` exists for tests and for a future caller; the app deliberately does not call it (the
  // manual dB range is not coming back — see `ui/src/app/centre/surface.ts`). What matters here is
  // that `lo`/`hi` are the numbers a trace is drawn against, wherever they came from.
  const pts = pointsOf(tracePaths(Float32Array.from([-75]), PANE, STRIP, s.lo, s.hi, "trace-slice", "p", INK));
  near(pts[0].y, 0, 1e-6);
  s.dispose();
});
