// **Viewport-dynamic contrast** (T-528): the display range measured from *the cells that are on the
// screen right now*, rather than from the region (anchored, T-470) or from the whole of every tile
// that happens to intersect the viewport (`auto`).
//
// ## Why a third mode rather than a fix to the second
//
// `auto` reads each resident tile's own `range_db` — a number the route computed over **the whole
// tile**. That is the right thing for a cheap "nothing clips" control, and it is what T-470 kept as
// the opt-in escape hatch. But it is not what a user means by *scale to what I am looking at*: zoom
// into a quiet 200 kHz sliver of a tile that also contains a broadcast carrier and the carrier —
// off screen, in the same tile — still sets the top of the ramp, so the quiet band sits in the
// bottom few percent exactly as it did anchored. The user asked for the other thing, and both are
// worth having, so this is a mode beside the other two rather than a redefinition of one.
//
// ## Shadow cells may not drag the scale
//
// A [[CELL.SHADOW]] cell (T-519/T-520) carries a real measurement of an **earlier time**, drawn
// dimmed and scanlined because it is not a measurement of the cell it sits in. It is therefore not
// a measurement of the current viewport either, and letting it set the range would mean panning
// across a swept-then-abandoned band crushed the live cells beside it against the floor. The rule
// here is one line and it excludes shadows by construction rather than by naming them: **only a
// cell whose state byte is exactly `CELL.OBSERVED` contributes.** That is the same predicate the
// shader uses to decide a cell is a measurement of its own extent, and it happens to be a total
// guard against `NaN` too, since every other state's value is `NaN` by `decodeTile`'s own rule.
//
// ## The cost, and why it is a block pyramid rather than a scan
//
// This runs per frame, for every tile drawn, on the same thread as the frame. A 256 × 256 tile is
// 65 536 cells and a busy screen draws tens of them, so re-scanning the visible cells every frame
// is millions of float comparisons per frame — a cost paid whether or not the view is moving.
//
// So each tile is summarised **once**, when it is first asked about, into a 16 × 16 grid of
// per-block `(min, max)` over its observed cells, memoised on the [[TileData]] object in a
// `WeakMap` (no field on `TileData`, no touch to the cache's entry type, and the summary is
// collected exactly when the tile it describes is). Per frame the accumulator then reads only the
// blocks intersecting the visible sub-rect: at most 256 per tile, whatever the tile's resolution.
// `ui/test/surface-vscale.test.ts` measures both halves.
//
// The block grid makes the answer **conservative at the edges**: a block straddling the edge of the
// viewport contributes whole, so up to one block (1/16 of the tile per axis) of just-off-screen
// measurement can reach the range. That is the deliberate direction — it can only *widen* the
// range, never invent a level that is not in the tile, and a range slightly wider than the visible
// extremes clips nothing. A per-cell answer would be exact and cost 256× more for a difference no
// viewer can see.

import { CELL } from "./cellrule";
import type { Box } from "./lattice";
import type { TileData } from "./tile";

/** Blocks per axis in a tile's summary. 16 → ≤256 blocks read per tile per frame, at any cell count. */
export const BLOCKS = 16;

/**
 * **The smallest display span this mode will state, in dB.**
 *
 * A viewport whose observed cells are all within a dB of each other — a flat noise floor, the
 * commonest thing on this surface — has no dynamic range to spread. Stretching it across the whole
 * ramp would turn the front end's own quantisation into vivid structure, which is the same defect
 * as upscaling a coarse tile and presenting it as detail. So below this the range is widened about
 * the viewport's midpoint instead, and the ramp honestly shows a flat band as flat.
 */
export const MIN_SPAN_DB = 12;

/**
 * Margin added below the visible minimum and above the visible maximum, in dB.
 *
 * Small and symmetric: enough that the strongest cell is not pinned to the white end and the
 * weakest not to the black one (where a further dB of difference stops being visible at all),
 * and not so much that the "spreads across the whole ramp" the mode exists for is given away.
 */
export const HEADROOM_DB = 2;

/** A tile's summary: per-block `(min, max)` over its `OBSERVED` cells. Empty blocks stay ±Infinity. */
export interface BlockStats {
  /** Blocks along frequency and time, and the cells per block on each axis. */
  readonly bf: number;
  readonly bt: number;
  readonly sf: number;
  readonly st: number;
  readonly nf: number;
  readonly nt: number;
  /** Row-major `[bt * bf + bf]`, same order as the cells. */
  readonly lo: Float32Array;
  readonly hi: Float32Array;
  /** How many cells contributed — 0 for a tile with no observed cell at all. */
  readonly cells: number;
}

/** Built once per decoded tile, and collected with it. Never a field on `TileData`. */
const SUMMARIES = new WeakMap<TileData, BlockStats>();

/** How many tile summaries have been built. A cost the tests read, not a display. */
let built = 0;
export function summariesBuilt(): number { return built; }
/** Reset the build counter. Tests only. */
export function resetSummaryCount(): void { built = 0; }

/**
 * The tile's block summary, built on first ask and memoised on the tile.
 *
 * O(cells), once — the same order as the decode that produced the planes, and paid only for a tile
 * that is actually drawn while this mode is on.
 */
export function blockStats(d: TileData): BlockStats {
  const seen = SUMMARIES.get(d);
  if (seen) return seen;
  const nf = Math.max(1, d.nf | 0), nt = Math.max(1, d.nt | 0);
  const sf = Math.max(1, Math.ceil(nf / BLOCKS)), st = Math.max(1, Math.ceil(nt / BLOCKS));
  const bf = Math.ceil(nf / sf), bt = Math.ceil(nt / st);
  const lo = new Float32Array(bf * bt).fill(Number.POSITIVE_INFINITY);
  const hi = new Float32Array(bf * bt).fill(Number.NEGATIVE_INFINITY);
  let cells = 0;
  const value = d.value, state = d.state;
  for (let t = 0; t < nt; t++) {
    const row = t * nf, brow = Math.floor(t / st) * bf;
    for (let f = 0; f < nf; f++) {
      const i = row + f;
      // **The whole shadow rule** (see the header). `OBSERVED` and nothing else: a SHADOW cell is a
      // measurement of another time, and every other state's value is `NaN`.
      if (state[i] !== CELL.OBSERVED) continue;
      const v = value[i];
      if (!Number.isFinite(v)) continue;
      const b = brow + Math.floor(f / sf);
      if (v < lo[b]) lo[b] = v;
      if (v > hi[b]) hi[b] = v;
      cells++;
    }
  }
  const out: BlockStats = { bf, bt, sf, st, nf, nt, lo, hi, cells };
  SUMMARIES.set(d, out);
  built++;
  return out;
}

/** The overlap of two boxes, or `null` when they do not overlap. */
export function intersect(a: Box, b: Box): Box | null {
  const f0Hz = Math.max(a.f0Hz, b.f0Hz), f1Hz = Math.min(a.f1Hz, b.f1Hz);
  const t0Ns = Math.max(a.t0Ns, b.t0Ns), t1Ns = Math.min(a.t1Ns, b.t1Ns);
  return f1Hz > f0Hz && t1Ns > t0Ns ? { f0Hz, f1Hz, t0Ns, t1Ns } : null;
}

/**
 * **The accumulator for one frame.** One instance per `render()`, fed once per drawn quad, and
 * discarded — nothing here is retained between frames, for the same reason nothing else in the
 * renderer is (T-388: a value carried across frames is a value that can drift out of step with the
 * geometry it describes).
 */
export class ViewportScale {
  lo = Number.POSITIVE_INFINITY;
  hi = Number.NEGATIVE_INFINITY;
  /** Blocks that contributed. `0` means nothing observed is on screen — which is a real state of
   * this surface (most of 6 GHz is grey) and not the same as "the range is 0 dB wide". */
  blocks = 0;
  /** Tiles asked about, whether or not they contributed. The per-frame cost, in one number. */
  quads = 0;

  /**
   * Fold in the part of one drawn quad that is actually inside the viewport.
   *
   * @param data      the tile whose texture this quad samples — the *ancestor* for a coarse stand-in,
   *                  because that is the tile whose cells are on the screen.
   * @param tileExt   that tile's own extent on the surface.
   * @param region    the surface region this quad covers (a child's extent, for a stand-in).
   * @param view      the pane's box. The intersection of `region` and this is what is visible.
   */
  add(data: TileData, tileExt: Box, region: Box, view: Box): void {
    this.quads++;
    const onScreen = intersect(region, view);
    if (!onScreen) return;
    const vis = intersect(onScreen, tileExt);
    if (!vis) return;
    const s = blockStats(data);
    if (!s.cells) return;
    const df = tileExt.f1Hz - tileExt.f0Hz, dt = tileExt.t1Ns - tileExt.t0Ns;
    if (!(df > 0) || !(dt > 0)) return;
    // Cell index ranges, half-open and clamped into the tile. `ceil` on the far end so a visible
    // sliver narrower than one cell still reads the cell it is inside.
    const f0 = clampIdx(Math.floor(((vis.f0Hz - tileExt.f0Hz) / df) * s.nf), s.nf);
    const f1 = clampIdx(Math.ceil(((vis.f1Hz - tileExt.f0Hz) / df) * s.nf) - 1, s.nf);
    const t0 = clampIdx(Math.floor(((vis.t0Ns - tileExt.t0Ns) / dt) * s.nt), s.nt);
    const t1 = clampIdx(Math.ceil(((vis.t1Ns - tileExt.t0Ns) / dt) * s.nt) - 1, s.nt);
    const bf0 = Math.floor(f0 / s.sf), bf1 = Math.floor(f1 / s.sf);
    const bt0 = Math.floor(t0 / s.st), bt1 = Math.floor(t1 / s.st);
    for (let bt = bt0; bt <= bt1; bt++) {
      const row = bt * s.bf;
      for (let bf = bf0; bf <= bf1; bf++) {
        const b = row + bf;
        const blo = s.lo[b];
        if (!Number.isFinite(blo)) continue;
        const bhi = s.hi[b];
        if (blo < this.lo) this.lo = blo;
        if (bhi > this.hi) this.hi = bhi;
        this.blocks++;
      }
    }
  }

  /**
   * The display range this frame's screen asks for, with [[HEADROOM_DB]] and [[MIN_SPAN_DB]]
   * applied — or `null` when no observed cell was on screen at all, which is the caller's cue to
   * **hold** the range it has rather than invent one over grey.
   */
  range(): { lo: number; hi: number } | null {
    if (!this.blocks || !Number.isFinite(this.lo) || !Number.isFinite(this.hi)) return null;
    let lo = this.lo - HEADROOM_DB, hi = this.hi + HEADROOM_DB;
    const short = MIN_SPAN_DB - (hi - lo);
    if (short > 0) { lo -= short / 2; hi += short / 2; }
    return { lo, hi };
  }
}

const clampIdx = (i: number, n: number) => (Number.isFinite(i) ? Math.min(n - 1, Math.max(0, i)) : 0);
