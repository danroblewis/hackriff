// **The instantaneous spectrum trace** (T-457, docs/16 §8.5b finding 1).
//
// The cutover retired the live FFT plot that sat above the old waterfall and reported that it had
// no home. That report's load-bearing sentence is right, and this file is built on it rather than
// against it:
//
// > The surface draws **folded cells over time**; a trace is **this frame across frequency**.
//
// So a trace is not a zoom level of the surface and cannot be obtained by zooming into it. What it
// *can* share is the one thing that must not diverge: **the axes**. The trace's x is the pane's own
// frequency window, placed through the renderer's own [[toClip]]; the trace's y is the surface's one
// measured display range, the same `lo`/`hi` the ramp is relative to. A peak that paints yellow in
// the waterfall therefore sits near the top of the trace above the very column it paints, and the
// two pictures cannot tell different stories about the same energy.
//
// ## The trace is TIME-ADDRESSABLE, and it is the pane's own time (user, 2026-09-17)
//
// A trace pinned to *now* is the naive version, and it is explicitly not what this is. **The trace is
// a horizontal slice of the surface at the viewport's own time position** — the newest instant in
// that pane's window, `box.t1Ns`. For a pane following the growing edge that instant *is* the live
// edge, so live playback shows the newest sample, guaranteed; for a pane scrubbed into last hour it
// is last hour, and the trace is the spectrum of *then*. Nothing here reads a global clock, which is
// the same rule every other time-varying thing on this surface obeys.
//
// Two consequences that are part of the spec rather than niceties:
//
//  - **It spans the whole viewport width**, not one tile. The columns tile the pane's entire
//    frequency window and are filled from **every** tile that intersects it, so a slice across a
//    stitched or multi-device span is one continuous plot.
//  - **It is drawn only where data exists.** A column nothing answered for is `NaN` and emits no
//    quad at all. *Unobserved is not quiet*: a line drawn across a gap — or dropped to the floor —
//    would claim a measurement the radio never took, which is the grey rule one surface up.
//
// ## Three series, and where each honestly comes from
//
//  - **slice** — the spectrum at the pane's time position, from whichever source can answer finest:
//      * the **live spectrum row** (`ui/src/app/centre/live-edge.ts`) when its own capture time falls
//        inside the pane's topmost time cell. This is the one quantity on this screen no tile can
//        supply — a cell is a fold over *at least* one row, and a frame is one row;
//      * otherwise **the pyramid**, read as the one row of cells covering that instant
//        ([[sliceColumns]]). That row is a max-hold over the cell's own duration, which the readout
//        states rather than passing off as an instant.
//  - **max-hold** — the column-wise maximum over the pane's **whole** window ([[maxHoldColumns]]).
//    With a time-addressable slice these are now two clearly different questions about one viewport:
//    *what is there at this instant* and *what was the loudest thing here across this window*.
//
// ### Why max-hold needed no accumulator, and no new tier
//
// The ticket asked whether max-hold belongs in the tile ladder as "a max-reduction tier beside the
// mean one". It does not, and the reason is that **there is no mean tier**: the ladder's only
// reduction already is max-hold, stated by the server in its own words
// (`hk-api`'s `MAX_HOLD_RULE`, served verbatim beside every grid):
//
// > *max-hold: a cell is the maximum of the source cells folded into it, so folding further never
// > lowers a value and a brief emission survives the collapse; the max of nothing is unobserved, not
// > zero*
//
// A level-0 cell is already the maximum of the STFT frames under it — `hk_dsp::Spectrum::max_hold`
// is literally what `FrameInput::peak` carries — and every coarser cell is the maximum of those. So
// the max-hold tier exists, is on demand in exactly T-453's sense (a coarse node is a product of
// *looking*, not of capture), and **this trace needed no backend change at all**. Adding a second
// max tier would have put write work back on the capture path that T-453 spent a whole ticket
// removing, to duplicate a reduction already in the ladder.
//
// That also disposes of the objection to a client-side buffer. A buffer "restarts every time the
// user navigates and cannot answer *what was the peak here last hour*"; these columns are derived
// from the pane's box, so panning to last hour shows last hour's peak, because the pyramid kept it.
// Time-addressability makes that sharper rather than weaker: *"max over what window"* now has a
// viewport answer — the window this pane is showing — and a per-pane accumulator would have to be
// thrown away and restarted on every pan, every zoom and every split, while the pyramid simply
// answers about the box it is asked about.
//
// **A note for the incremental-tile work.** This file asks `TileCache` for whatever is resident and
// treats an absent tile as a gap; it never reasons about *when* a node was sealed. If tiles begin
// arriving at the live edge incrementally, the slice at the edge gets finer and this code needs no
// change — the live row is preferred there only because it is finer than any cell, and that stays
// true however the cell was produced.
//
// **This is not the client re-reducing a measurement.** The warning in `hk-api`'s
// `overview_semantics_json` — *"a series whose statistic is unstated is one a client will eventually
// re-reduce for itself"* — is about inventing a statistic the server did not state. Here the server
// states it, and the statistic is **idempotent and associative**: a max of max-holds is a max-hold,
// and it is the same number the server would return for one tile spanning the whole box. That
// property is what licenses the reduction, it is a property of the fold *as the server defined it*,
// and `ui/test/surface-trace.test.ts` asserts it directly rather than assuming it.
//
// ## What this file may not do
//
// Nothing about signals. It pools numbers onto columns and turns columns into [[OverlayQuad]]s —
// stroked, drawn by `overlay.ts`'s program, which has no sampler and no ramp and therefore cannot
// express a measurement colour. A trace can sit beside the waterfall; it can never tint it.

import { CELL } from "./cellrule";
import { extentOf, tilesFor, type Box, type Lattice } from "./lattice";
import type { OverlayQuad } from "./minimap";
import { toClip, type PaneRect } from "./surface";
import type { TileCache } from "./tilecache";
import type { TilePlanes } from "./surface";

/**
 * How many columns a trace is drawn at, at most.
 *
 * One quad per column per series, and the pane is redrawn every frame, so this is a real budget
 * rather than a tidiness limit. 256 is the tile's own cell count (`RENDER_CELLS`), which is the
 * finest thing the surface beside it is made of — drawing the trace finer than the picture it sits
 * above would be claiming resolution the neighbouring pixels do not have.
 */
export const TRACE_COLUMNS = 256;

/** Ink for the two series. Far from the ramp and from `PANE_MARK`'s amber, and from each other. */
export const SLICE_INK: readonly [number, number, number, number] = [0.62, 0.95, 1.0, 0.95];
export const HOLD_INK: readonly [number, number, number, number] = [1.0, 0.45, 0.85, 0.8];

/**
 * The newest spectrum row: **one frame, across frequency**, on the capture clock.
 *
 * `db` is the row exactly as the stream delivered it — lowest frequency first, one value per bin,
 * dBFS/Hz (`hk-pipeline`'s `spectrum.rs` writes `PowerUnit::DbfsPerHz`, and the history store's
 * `max_db` is the same scale, which is what lets one y axis carry both series).
 */
export interface LiveFrame {
  readonly f0Hz: number;
  readonly f1Hz: number;
  readonly db: Float32Array;
  /** The row's own absolute capture time. Never a browser clock. */
  readonly tNs: number;
}

/**
 * The newest row, and only the newest row.
 *
 * Deliberately **not** in the app store: a spectrum row arrives tens of times a second, and the old
 * live view's own note — *"spectrum rows go straight to `Waterfall.push` and never enter the
 * store"* — is the reason. A holder that keeps exactly one row is also the reason this is not the
 * client-side accumulator the ticket warned about: it holds no history and answers no question
 * about the past. The past is the pyramid's job, and [[maxHoldColumns]] asks it.
 */
export class LiveRow {
  private frame: LiveFrame | null = null;
  /** Rows seen since the last geometry change. Surfaced so a readout can say "nothing yet". */
  rows = 0;

  set(f: LiveFrame): void {
    this.frame = f;
    this.rows++;
  }

  /** Forget the row: a retune re-plumbs the stream, and the old band's frame is not this band's. */
  clear(): void {
    this.frame = null;
    this.rows = 0;
  }

  get(): LiveFrame | null { return this.frame; }
}

const NOT = Number.NaN;

/**
 * The frame, pooled onto `n` columns spanning `box`'s frequency window.
 *
 * **Max-pooling, not sampling**, wherever a column covers more than one bin: it is the same rule the
 * server folds by and the same rule the retired `decimateRow` used, so a narrow emission that lands
 * between two sample points survives the collapse instead of disappearing at some zoom levels. Where
 * a column is *narrower* than a bin the nearest bin is replicated — the honest statement, since the
 * front end measured no finer.
 *
 * A column outside the tuned band is `NaN`: there is no current frame out there, and a floor drawn
 * across it would claim quiet where the radio is not listening.
 */
export function sampleFrame(fr: LiveFrame, box: Box, n: number): Float32Array {
  const out = new Float32Array(n).fill(NOT);
  const bins = fr.db.length;
  const span = box.f1Hz - box.f0Hz;
  if (!(bins > 0) || !(span > 0) || !(fr.f1Hz > fr.f0Hz) || !(n > 0)) return out;
  const binHz = (fr.f1Hz - fr.f0Hz) / bins;
  const colHz = span / n;
  for (let c = 0; c < n; c++) {
    const lo = box.f0Hz + c * colHz, hi = lo + colHz;
    if (hi <= fr.f0Hz || lo >= fr.f1Hz) continue;
    let i0 = Math.floor((Math.max(lo, fr.f0Hz) - fr.f0Hz) / binHz);
    let i1 = Math.ceil((Math.min(hi, fr.f1Hz) - fr.f0Hz) / binHz);
    i0 = Math.max(0, Math.min(bins - 1, i0));
    i1 = Math.max(i0 + 1, Math.min(bins, i1));
    let m = -Infinity;
    for (let i = i0; i < i1; i++) {
      const v = fr.db[i];
      if (Number.isFinite(v) && v > m) m = v;
    }
    if (m > -Infinity) out[c] = m;
  }
  return out;
}

/**
 * The column-wise maximum over `box`, from the tiles the pane is **already** drawing.
 *
 * `levelF`/`levelT` are the levels the frame actually resolved (`PaneReport`), not a second
 * calculation beside it — the same discipline the chrome's stated level follows, and for the same
 * reason: a trace reduced from a different level than the picture under it would be a second
 * opinion about one window.
 *
 * Only resident tiles contribute, and only `OBSERVED` cells (`decodeTile` already writes `NaN`
 * elsewhere). A column no tile has answered for stays `NaN` and is drawn as a **gap** — never as the
 * bottom of the scale, which is the trace's form of the grey rule: *the max of nothing is
 * unobserved, not zero*.
 */
export function maxHoldColumns(
  lat: Lattice,
  cache: Pick<TileCache<TilePlanes>, "peek">,
  box: Box,
  levelF: number,
  levelT: number,
  device: string,
  n: number,
): Float32Array {
  const out = new Float32Array(n).fill(NOT);
  const span = box.f1Hz - box.f0Hz;
  if (!(span > 0) || !(n > 0)) return out;
  for (const a of tilesFor(lat, box, levelF, levelT, device)) {
    // `peek` without a pin: reading the trace must not reorder the LRU the picture depends on.
    const e = cache.peek(a);
    if (!e) continue;
    const d = e.data;
    const ext = extentOf(lat, a);
    const cellHz = (ext.f1Hz - ext.f0Hz) / d.nf;
    const cellNs = (ext.t1Ns - ext.t0Ns) / d.nt;
    if (!(cellHz > 0) || !(cellNs > 0)) continue;
    const r0 = Math.max(0, Math.floor((box.t0Ns - ext.t0Ns) / cellNs));
    const r1 = Math.min(d.nt, Math.ceil((box.t1Ns - ext.t0Ns) / cellNs));
    if (!(r1 > r0)) continue;
    for (let j = 0; j < d.nf; j++) {
      const f0 = ext.f0Hz + j * cellHz;
      const c0 = Math.max(0, Math.floor(((f0 - box.f0Hz) / span) * n));
      const c1 = Math.min(n, Math.ceil(((f0 + cellHz - box.f0Hz) / span) * n));
      if (!(c1 > c0)) continue;
      let m = -Infinity;
      for (let r = r0; r < r1; r++) {
        const k = r * d.nf + j;
        // The state plane is the authority on observed-ness; the value plane merely agrees.
        if (d.state[k] !== CELL.OBSERVED) continue;
        const v = d.value[k];
        if (Number.isFinite(v) && v > m) m = v;
      }
      if (m === -Infinity) continue;
      // `!(a >= b)` rather than `a < b`, so a `NaN` column is filled rather than skipped.
      for (let c = c0; c < c1; c++) if (!(out[c] >= m)) out[c] = m;
    }
  }
  return out;
}

/**
 * **The interval the trace is a spectrum OF**: the pyramid's own time cell at `levelT` covering the
 * instant `tAtNs`.
 *
 * "The spectrum at this time" cannot be finer than the thing answering it. At `levelT` the finest
 * interval the ladder holds is one cell, so the slice is that cell — snapped to the lattice's own
 * grid so it is the *same* cell the picture below it is drawn from, and never a window straddling
 * two of them, which would be a max over more data than the row under the cursor.
 *
 * `tAtNs` on a cell boundary resolves to the cell **ending** there, because the pane's time position
 * is the top of its window: the newest instant it is showing is the end of the newest cell it drew.
 */
export function sliceWindow(lat: Lattice, levelT: number, tAtNs: number): { t0Ns: number; t1Ns: number } {
  const cell = lat.t0Ns * 2 ** Math.max(0, levelT);
  const t1Ns = Math.ceil(tAtNs / cell) * cell;
  return { t0Ns: t1Ns - cell, t1Ns };
}

/**
 * Is the live spectrum row a *finer* answer than the pyramid for this pane's time position?
 *
 * Only when the row's own capture time falls inside the cell the slice is asking about. That single
 * rule gives both halves of the spec: a pane at the growing edge shows the newest delivered frame,
 * and a pane scrubbed into the past shows the pyramid's row for *that* instant instead of a line
 * from now laid over a picture of then. "Pause freezes the view, not the capture" means the rows
 * keep arriving — it does not mean the newest of them is true of the window being held.
 */
export function liveFrameFits(fr: LiveFrame | null, slice: { t0Ns: number; t1Ns: number }): boolean {
  return !!fr && Number.isFinite(fr.tNs) && fr.tNs >= slice.t0Ns && fr.tNs <= slice.t1Ns;
}

/**
 * **The slice: the spectrum across the whole viewport at one instant**, from the tiles.
 *
 * It is [[maxHoldColumns]] over a one-cell-tall window, and saying it that way is the point: at the
 * level the pane drew, "the spectrum at this time" *is* the row of cells at this time, and a cell is
 * already a max-hold over its own duration. So there is one reduction in this file, used to answer
 * two different questions, rather than two reductions that could disagree.
 *
 * Whole-viewport by construction: it iterates every tile intersecting the pane's frequency window,
 * so a span stitched from several tiles — or later, several devices — comes out as one plot, and the
 * parts nothing has answered for come out as gaps.
 */
export function sliceColumns(
  lat: Lattice,
  cache: Pick<TileCache<TilePlanes>, "peek">,
  box: Box,
  levelF: number,
  levelT: number,
  device: string,
  n: number,
  tAtNs: number,
): Float32Array {
  const w = sliceWindow(lat, levelT, tAtNs);
  return maxHoldColumns(lat, cache, { ...box, t0Ns: w.t0Ns, t1Ns: w.t1Ns }, levelF, levelT, device, n);
}

/** Where a set of columns peaks, in the box's own frequency terms. `null` when nothing was said. */
export function peakOf(cols: Float32Array, box: Box): { hz: number; db: number; col: number } | null {
  const n = cols.length;
  if (!(n > 0)) return null;
  let best = -Infinity, at = -1;
  for (let c = 0; c < n; c++) {
    const v = cols[c];
    if (Number.isFinite(v) && v > best) { best = v; at = c; }
  }
  if (at < 0) return null;
  const colHz = (box.f1Hz - box.f0Hz) / n;
  return { hz: box.f0Hz + (at + 0.5) * colHz, db: best, col: at };
}

/** The pane-relative position of a dB value on the strip, clamped into it. `0` bottom, `1` top. */
export function levelFrac(db: number, lo: number, hi: number): number {
  const f = (db - lo) / Math.max(1e-6, hi - lo);
  return Number.isFinite(f) ? Math.min(1, Math.max(0, f)) : 0;
}

export interface TraceStyle {
  /** Stroke thickness, device px. */
  readonly thickPx?: number;
}

/**
 * Columns to quads, in the strip's own clip space.
 *
 * **The x mapping is the renderer's**, not a copy of it: each column's frequency extent goes through
 * the same [[toClip]] the tiles and the signal boxes go through, against the same pane box. That is
 * the frequency-axis form of T-388's rule — a trace placed by arithmetic of its own would drift from
 * the column it describes the moment either side changed, and nobody would see it until it mattered.
 *
 * **The y mapping is the surface's one display range.** `lo`/`hi` are `Surface.lo`/`Surface.hi`, the
 * range the served tiles reported (`range_db`) and the ramp is relative to, so the trace's height and
 * the waterfall's colour are two readings of one scale.
 *
 * A `NaN` column emits nothing. A gap in a trace is the honest picture of a column nothing answered
 * for; a line drawn through it would invent the value the fold refused to.
 */
export function traceQuads(
  cols: Float32Array,
  box: Box,
  rect: PaneRect,
  lo: number,
  hi: number,
  rgba: readonly [number, number, number, number],
  kind: "trace-slice" | "trace-hold",
  id: string,
  style: TraceStyle = {},
): OverlayQuad[] {
  const out: OverlayQuad[] = [];
  const n = cols.length;
  const span = box.f1Hz - box.f0Hz;
  if (!(n > 0) || !(span > 0) || !(rect.h > 0)) return out;
  const thickPx = style.thickPx ?? 2;
  // Clip spans 2 over `rect.h` device px, so one px is `2 / rect.h` and a half-stroke is this.
  const half = thickPx / Math.max(1, rect.h);
  const colHz = span / n;
  for (let c = 0; c < n; c++) {
    const v = cols[c];
    if (!Number.isFinite(v)) continue;
    const [x0, , x1] = toClip(
      { f0Hz: box.f0Hz + c * colHz, f1Hz: box.f0Hz + (c + 1) * colHz, t0Ns: box.t0Ns, t1Ns: box.t1Ns },
      box,
    );
    const y = -1 + 2 * levelFrac(v, lo, hi);
    out.push({
      clip: [x0, Math.max(-1, y - half), x1, Math.min(1, y + half)],
      rgba, kind, id,
    });
  }
  return out;
}
