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
// ## The classic analyser look (T-475): amplitude colour, a smooth line, and afterglow
//
// The user's report of the first version was *"a plain line, missing everything"*. Three things were
// missing, and each one is a property of this file plus `./tracepass.ts`:
//
// **1. The trace is coloured by amplitude, from the SAME ramp as the cells beneath it.** A vertex at
// `db` is painted `cmap((db - lo) / (hi - lo))` — character for character the argument the tile
// shader computes (`surface.ts`: `cellMark(s, (v - uLo) / max(uHi - uLo, 1e-6), px)`), against the
// same `lo`/`hi`. So the same dB *is* the same colour in both pictures, which is what makes the two
// readable as one. The ramp comes from `ui/src/cmap.ts` — the one module that defines one (T-397,
// guarded repo-wide by `ui/test/surface-cutover.test.ts`); nothing here has stops of its own.
//
// **2. It is a smooth, anti-aliased curve, not a per-bin staircase.** The first version emitted one
// axis-aligned quad per column, which is literally a flight of stairs — the "looks like pixels" the
// user saw. [[tracePaths]] instead runs a **monotone cubic** (Fritsch–Carlson) through the column
// centres and `./tracepass.ts` strokes the result with a mitred, feathered edge.
//
// *This is line rendering, not faked resolution* (user, 2026-09-18, ruling on the objection). The
// declare-your-resolution rule exists so a zoomed **waterfall** never implies cells the hardware did
// not sample; drawing a curve through samples that genuinely exist is ordinary plotting. The part of
// that rule which does still bind is the narrow, checkable one T-457 already implemented and this
// keeps: **a column nothing answered for emits nothing, and the smoothing never crosses one.** A
// `NaN` splits the columns into runs and each run is its own path, so a curve can only ever be drawn
// between two measurements. Monotone-cubic is chosen for the same reason: it passes exactly through
// every measured value and **cannot overshoot past its bracketing samples**, so no point on the line
// is outside the range the radio reported there.
//
// **3. Phosphor persistence, from the PANE'S OWN WINDOW.** [[persistenceSlices]] reads the rows just
// before the pane's time position out of the pyramid — the very cells the pane just drew — rather
// than out of a client-side buffer of whatever happened to arrive while the page was open. That is
// the difference between afterglow that works when scrubbed and afterglow that only works live: a
// pane frozen on last hour glows with last hour's rows, because the pyramid kept them.
//
// ## What this file may not do
//
// Nothing about signals. It pools numbers onto columns and turns columns into [[TracePath]]s. It can
// express a measurement *colour* now, which T-457 could not — so the property that keeps a trace from
// being mistakable **for** tile data is no longer "it has no ramp". It is structural and stronger:
// the trace is drawn into a rectangle **carved off** the pane (`view.ts`), scissored to it, by a
// program with **no sampler** — so it cannot read a tile, cannot reach a tile's pixels, and cannot
// express `cellrule.ts`'s greys or its tier and fallback marks. And the data pass still cannot see
// any of it: it has already been submitted when the trace draws, which
// `ui/test/surface-trace.test.ts` asserts byte for byte.

import { cmap } from "../cmap";
import { CELL } from "./cellrule";
import { extentOf, tilesFor, type Box, type Lattice } from "./lattice";
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

/**
 * Ink for the **max-hold**, and it is deliberately *not* from the ramp.
 *
 * The slice is the measurement the eye is meant to read against the cells below it, so it carries
 * the ramp colour. The max-hold is a different question about a different interval — *the loudest
 * thing anywhere in this window* — and giving it the ramp too would put two lines the same colour on
 * one strip with nothing to tell them apart. Magenta is off the ramp entirely: the ramp runs
 * near-black → blue → cyan → yellow → red → white, and none of those has a high red **and** a high
 * blue with a low green, so a magenta pixel can only be the max-hold. `ui/e2e/app-trace.e2e.mjs`
 * relies on exactly that to separate the two series by colour alone.
 */
export const HOLD_INK: readonly [number, number, number, number] = [1.0, 0.45, 0.85, 0.8];

/**
 * Points drawn per interval between two measured columns.
 *
 * It is a *rendering* number, not a resolution claim: every one of these points lies on the monotone
 * cubic through the measured samples, which passes through each sample exactly and never leaves the
 * band its neighbours bracket. Four is where the curve stops looking faceted at this strip height.
 */
export const TRACE_SUBDIV = 4;

/** How many earlier rows glow behind the current one. */
export const TRACE_PERSISTENCE = 4;

/**
 * The afterglow's opacity by age, newest first — a decay, not a fade-to-a-flat-grey.
 *
 * The newest shadow is already well under half the current trace's weight, so the line the readout
 * describes is never in doubt; by the fourth it is a hint of where the energy was.
 */
export const SHADOW_ALPHA: readonly number[] = [0.34, 0.23, 0.15, 0.09];

/** The grey a `"mono"` series runs between, floor of the scale to top. See [[TraceStyle.shade]]. */
export const MONO_LO = 0.2;
export const MONO_HI = 0.6;

/**
 * Stroke width, device px: the current slice, its glow, an afterglow row, the max-hold.
 *
 * The slice is **3 px and not less**, and that is a measurement rather than a taste. `tracepass.ts`
 * feathers coverage over the outermost device pixel of a stroke, so a line 2 px across has almost no
 * fully-covered core and every pixel of it is a few percent blended with the backdrop — which showed
 * up as the trace's colour sitting 9–12/255 away from the identical cell below it, on a claim that is
 * supposed to be an equality. At 3 px there is a full pixel of core carrying the vertex colour
 * unblended, and `ui/e2e/app-trace.e2e.mjs` reads it back off the framebuffer.
 */
export const SLICE_PX = 3;
export const GLOW_PX = 7;
export const SHADOW_PX = 1.6;
export const HOLD_PX = 1.4;

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
 * How far apart two absolute times may be and still be the same instant, given that each is a
 * double of epoch nanoseconds: a few ulps of the largest magnitude involved (2^-48 relative, ~6 µs
 * at today's epoch), which covers the rounding of `k * cell` products and of a non-integer cell
 * recovered from `cell_s * 1e9`. Nothing the lattice resolves is that short.
 */
export function timeSlackNs(...ts: number[]): number {
  let m = 0;
  for (const t of ts) if (Number.isFinite(t)) m = Math.max(m, Math.abs(t));
  return m * 2 ** -48;
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
    // T-880: the edges are epoch nanoseconds (~1.8e18, past 2^53), so a double holds them only to
    // ~256 ns, and a window snapped to a cell edge by `sliceWindow` lands a few ulps either side of
    // the edge this tile's own arithmetic puts there. A raw floor/ceil then pulls in the NEIGHBOURING
    // row — a slice that is the max of two cells. Edges within the representable error of a cell
    // edge are that edge; the slack is capped far below a cell so a genuine partial row still counts.
    const slack = Math.min(cellNs / 64, timeSlackNs(box.t0Ns, box.t1Ns, ext.t0Ns));
    const r0 = Math.max(0, Math.floor((box.t0Ns - ext.t0Ns + slack) / cellNs));
    const r1 = Math.min(d.nt, Math.ceil((box.t1Ns - ext.t0Ns - slack) / cellNs));
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
 * Only when the row's own capture time falls inside the cell the slice is asking about — or, for a
 * FOLLOWING pane, at or after that cell's start (see the note in the body). That rule gives both halves of the spec: a pane at the growing edge shows the newest delivered frame,
 * and a pane scrubbed into the past shows the pyramid's row for *that* instant instead of a line
 * from now laid over a picture of then. "Pause freezes the view, not the capture" means the rows
 * keep arriving — it does not mean the newest of them is true of the window being held.
 */
export function liveFrameFits(
  fr: LiveFrame | null, slice: { t0Ns: number; t1Ns: number }, following = false,
): boolean {
  if (!fr || !Number.isFinite(fr.tNs) || fr.tNs < slice.t0Ns) return false;
  // **A FOLLOWING pane's time position is the growing edge, so the newest row is its slice even
  // when that row has run past the edge cell** (T-1051). "Following live it is the top-most sample"
  // (CLAUDE.md, T-457). The pane's edge is written from the stream at most every
  // `EDGE_WRITE_S` = 0.25 s (`app/centre/slice.ts`) while a cell at the display floor is 40–80 ms,
  // so the newest row sits one to several cells past `slice.t1Ns` most of the time. Requiring it
  // to be inside the cell made a following pane's trace flicker between the live row and a pyramid
  // cell that is usually not built yet ("no tile in hand"), several times a second — and a page
  // held on either side of that flicker stayed there: `app-trace.e2e.mjs` measured it as "the page
  // would not hold still in the state under test". A row OLDER than the cell is still refused, and
  // a frozen pane (`following` false) keeps the cell-only rule, so a scrubbed pane still gets no
  // line from now.
  return following || fr.tNs <= slice.t1Ns;
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

/**
 * **One stroked curve** — the unit `./tracepass.ts` draws.
 *
 * `xy` is in the strip's own clip space (the pane's [[toClip]] for x, the surface's display range for
 * y); `rgb` carries one linear-RGB colour **per point**, so amplitude colour travels with the vertex
 * rather than being a uniform the pass would have to pick. `alpha` and `widthPx` are the whole of a
 * path's style: an afterglow row differs from the current slice only in those two numbers.
 */
export interface TracePath {
  readonly kind: string;
  readonly id: string;
  /** Points, x,y interleaved, in the strip's clip space. At least two. */
  readonly xy: Float32Array;
  /** One linear RGB per point, r,g,b interleaved. `xy.length / 2 * 3` long. */
  readonly rgb: Float32Array;
  readonly alpha: number;
  readonly widthPx: number;
  /**
   * **Which vertices are a MEASURED column's own value**, as point indices into `xy` — the rest are
   * [[TRACE_SUBDIV]] interpolation between two of them (and the two end caps, which repeat their
   * neighbour's value across the column's frequency extent).
   *
   * Only one reader needs the distinction and it is the honest subject of T-475's equality: at a
   * measured column the trace's dB **is** a cell's dB, so its colour must be that cell's colour,
   * while between two of them the monotone interpolant is a value no cell carries and a pixel
   * comparison against the cells is asking a question the picture cannot answer. See [[coreSamples]].
   */
  readonly measured: Uint32Array;
}

export interface TraceStyle {
  /** Stroke thickness, device px. */
  readonly widthPx?: number;
  readonly alpha?: number;
  /**
   * A flat ink instead of the amplitude ramp — what the max-hold uses.
   *
   * `null`/absent means colour by [[shade]].
   */
  readonly ink?: readonly [number, number, number] | null;
  /**
   * **How a vertex takes its colour from its amplitude.**
   *
   * `"ramp"` (the default) is `cmap(levelFrac(db, lo, hi))` — the same normalisation and the same
   * ramp as the cell at that dB in the picture below. `"mono"` is a neutral grey at the same
   * position on the scale.
   *
   * **Exactly one line on a strip is drawn `"ramp"`, and it is the one the readout describes.** The
   * afterglow and the bloom are `"mono"` and the max-hold is a flat ink, so a measurement colour on
   * this strip belongs to the current slice and to nothing else. That is the strip's own version of
   * the one-ramp rule: four ramp-coloured lines would be *more* confusable with the cells below, not
   * less, and a reader could not tell which of them the stated peak was a peak of. It is also what
   * lets `ui/e2e/app-trace.e2e.mjs` isolate the current slice in the framebuffer by colour alone —
   * grey is achromatic and the max-hold's magenta is off the ramp — without a second copy of the
   * ramp to compare against and without a test-only flag to switch anything off.
   */
  readonly shade?: "ramp" | "mono";
  /** Points per interval between measured columns. See [[TRACE_SUBDIV]]. */
  readonly subdiv?: number;
}

/**
 * **Monotone cubic slopes** (Fritsch–Carlson 1980) for uniformly-spaced samples.
 *
 * Chosen over a Catmull-Rom or a plain cubic spline for one reason that is an honesty property
 * rather than an aesthetic one: the Fritsch–Carlson limiter guarantees the interpolant is **monotone
 * on every interval its data is monotone on**, which means it never overshoots past the samples that
 * bracket it. A Catmull-Rom through a noise floor with one tall spike rings *below the floor* on
 * either side of the spike — inventing a quieter measurement than the radio reported, at a frequency
 * where it reported something else. The curve here can only ever sit between the two dB values its
 * neighbours measured.
 */
function monotoneSlopes(y: Float64Array, h: number): Float64Array {
  const n = y.length;
  const m = new Float64Array(n);
  if (n < 2) return m;
  const d = new Float64Array(n - 1);
  for (let i = 0; i < n - 1; i++) d[i] = (y[i + 1] - y[i]) / h;
  m[0] = d[0];
  m[n - 1] = d[n - 2];
  for (let i = 1; i < n - 1; i++) m[i] = (d[i - 1] + d[i]) / 2;
  for (let i = 0; i < n - 1; i++) {
    if (d[i] === 0) { m[i] = 0; m[i + 1] = 0; continue; }
    const a = m[i] / d[i], b = m[i + 1] / d[i];
    // A negative ratio is a local extremum: flatten, or the curve turns the wrong way through it.
    if (a < 0) m[i] = 0;
    if (b < 0) m[i + 1] = 0;
    const s = a * a + b * b;
    if (s > 9) {
      const t = 3 / Math.sqrt(s);
      m[i] = t * a * d[i];
      m[i + 1] = t * b * d[i];
    }
  }
  return m;
}

/**
 * Columns to stroked curves, in the strip's own clip space.
 *
 * **The x mapping is the renderer's**, not a copy of it: each column's frequency extent goes through
 * the same [[toClip]] the tiles and the signal boxes go through, against the same pane box. That is
 * the frequency-axis form of T-388's rule — a trace placed by arithmetic of its own would drift from
 * the column it describes the moment either side changed, and nobody would see it until it mattered.
 *
 * **The y mapping is the surface's one display range.** `lo`/`hi` are `Surface.lo`/`Surface.hi`, the
 * range the served tiles reported (`range_db`) and the ramp is relative to — and since T-475 the
 * **colour** is that same pair through that same ramp, so the trace's height, the trace's colour and
 * the waterfall's colour are three readings of one scale rather than two and a decoration.
 *
 * **A `NaN` column ends a path**, and the next run starts a new one. That is the whole of the
 * honesty rule here: the interpolation only ever runs between two measured columns, so a smooth
 * curve can never bridge a span nothing answered for. A run of a single column is drawn as a flat
 * tick across **its own** frequency extent — the width the radio measured, not a point and not a
 * reach toward its neighbours.
 */
export function tracePaths(
  cols: Float32Array,
  box: Box,
  rect: PaneRect,
  lo: number,
  hi: number,
  kind: string,
  id: string,
  style: TraceStyle = {},
): TracePath[] {
  const out: TracePath[] = [];
  const n = cols.length;
  const span = box.f1Hz - box.f0Hz;
  if (!(n > 0) || !(span > 0) || !(rect.h > 0)) return out;
  const widthPx = style.widthPx ?? SLICE_PX;
  const alpha = style.alpha ?? 1;
  const ink = style.ink ?? null;
  const subdiv = Math.max(1, Math.floor(style.subdiv ?? TRACE_SUBDIV));
  const colHz = span / n;
  // The renderer's own x, per column: the same `toClip` the tiles go through, against the same box.
  const edge = (c: number): [number, number] => {
    const [x0, , x1] = toClip(
      { f0Hz: box.f0Hz + c * colHz, f1Hz: box.f0Hz + (c + 1) * colHz, t0Ns: box.t0Ns, t1Ns: box.t1Ns },
      box,
    );
    return [x0, x1];
  };
  const yOf = (db: number) => -1 + 2 * levelFrac(db, lo, hi);
  const mono = style.shade === "mono";
  const rgbOf = (db: number): readonly [number, number, number] => {
    if (ink) return ink;
    const f = levelFrac(db, lo, hi);
    if (!mono) return cmap(f);
    // Bright enough to read as an afterglow, dark enough that no grey can be mistaken for the top of
    // the ramp — which is white, and the one ramp colour that is achromatic too.
    const g = MONO_LO + (MONO_HI - MONO_LO) * f;
    return [g, g, g];
  };

  const emit = (c0: number, c1: number): void => {
    const len = c1 - c0;
    const xs: number[] = [], ys: number[] = [], rgb: number[] = [];
    // The vertices that carry a measured column's own value rather than an interpolated one; see
    // [[TracePath.measured]]. Recorded where the points are pushed, so the two cannot drift apart.
    const measured: number[] = [];
    const push = (x: number, db: number, exact = false) => {
      if (exact) measured.push(xs.length);
      xs.push(x); ys.push(yOf(db));
      const c = rgbOf(db);
      rgb.push(c[0], c[1], c[2]);
    };
    const [firstX0] = edge(c0);
    const lastX1 = edge(c1 - 1)[1];
    if (len === 1) {
      // One column, alone between two gaps: a flat tick the width of the column it measured. Both
      // ends carry that column's own value, so both are measured vertices.
      push(firstX0, cols[c0], true);
      push(lastX1, cols[c0], true);
    } else {
      const v = new Float64Array(len);
      const mid = new Float64Array(len);
      for (let i = 0; i < len; i++) {
        v[i] = cols[c0 + i];
        const [x0, x1] = edge(c0 + i);
        mid[i] = (x0 + x1) / 2;
      }
      // Uniform spacing in *index*; the x's are uniform too because `toClip` is affine in frequency
      // and the columns are equal in frequency. Interpolating against the index keeps the slope
      // limiter's arithmetic in the units the samples are in.
      const m = monotoneSlopes(v, 1);
      // A half-column cap at each end, so a run covers exactly the frequency extent it measured —
      // the same extent the per-column quads used to cover, and what the band-edge check reads.
      push(firstX0, v[0], true);
      for (let i = 0; i < len - 1; i++) {
        const y0 = v[i], y1 = v[i + 1], m0 = m[i], m1 = m[i + 1];
        for (let k = 0; k < subdiv; k++) {
          const t = k / subdiv;
          const t2 = t * t, t3 = t2 * t;
          const db = (2 * t3 - 3 * t2 + 1) * y0 + (t3 - 2 * t2 + t) * m0
            + (-2 * t3 + 3 * t2) * y1 + (t3 - t2) * m1;
          // k === 0 is the column's own midpoint and its own value; the rest interpolate.
          push(mid[i] + (mid[i + 1] - mid[i]) * t, db, k === 0);
        }
      }
      push(mid[len - 1], v[len - 1], true);
      push(lastX1, v[len - 1], true);
    }
    out.push({
      kind, id, alpha, widthPx,
      xy: Float32Array.from(xs.flatMap((x, i) => [x, ys[i]])),
      rgb: Float32Array.from(rgb),
      measured: Uint32Array.from(measured),
    });
  };

  let run = -1;
  for (let c = 0; c < n; c++) {
    const ok = Number.isFinite(cols[c]);
    if (ok && run < 0) run = c;
    else if (!ok && run >= 0) { emit(run, c); run = -1; }
  }
  if (run >= 0) emit(run, n);
  return out;
}

/**
 * **One point of a stroke the trace pass drew: where its CORE is on the screen, and the colour it
 * was stroked in** (T-1050).
 *
 * ## Why the page has to state this
 *
 * T-475's claim is an equality — *the same dB is the same colour on the trace and in the cells* —
 * and `ui/e2e/app-trace.e2e.mjs` reads it off a real framebuffer, which means it has to find the
 * stroke's core among the pixels. While the trace had a strip of its own that was a question the
 * pixels could answer: the band was backdrop, so the brightest pixel in a column was the core.
 * Since T-1041 the layer is drawn **over the pane's own rows**, and both halves of the old rule
 * broke at once:
 *
 *  - where the claim HOLDS, the core pixel is *identical* to the cell under it, so a difference
 *    against a trace-off baseline — the instrument that says which pixels are the layer's — throws
 *    away exactly the pixels that pass and keeps the ones that fail;
 *  - the bloom is achromatic (`shade: "mono"`) and 18 % opaque, so over a cell it is a lightened,
 *    desaturated copy of *that cell* — brighter than a dark core and chromatic — and "the brightest
 *    pixel in the stroke" returns the bloom. Measured on the T-1041 misses: bloom `[76,169,193]`
 *    against a cell `[31,185,203]`, and the property read 57.8 % of columns where it holds.
 *
 * Guessing the core's row from stroke geometry in the test was the third attempt and is the wrong
 * shape of instrument anyway: it re-derives in the test what the renderer already knows, which is
 * the T-388 family (two derivations of one picture).
 *
 * So the page states it. This is the **geometry and the ink of the vertices that were handed to the
 * GPU** — read off the very `TracePath`s `TracePass.draw` submitted, never recomputed from `cmap`
 * and never from the pane box a second time — and the test reads the *colour* out of the
 * framebuffer at the position stated here. What the test then asserts is therefore two things a
 * statement alone could not fake: that the pixel at the stated place carries the stated ink (the
 * layer drew what it says it drew), and that the same pixel is a colour the waterfall paints in
 * that column (T-475's equality). A wrong statement of position fails the first; a diverged ramp
 * fails the second.
 *
 * `xPx`/`yPx` are **CSS px from the canvas element's top-left** — the frame of reference a screenshot
 * and `getBoundingClientRect` share — so nothing downstream has to know about GL's upside-down
 * viewport or the device-pixel ratio. `rgb` is 0–255, `round(255 · v)` of the vertex's own linear
 * colour, which is what the fragment shader writes into the drawing buffer at full coverage.
 */
export interface TraceCoreSample {
  readonly xPx: number;
  readonly yPx: number;
  readonly rgb: readonly [number, number, number];
  /**
   * **This pixel is a MEASURED column's own dB**, not a point on the interpolant between two of them
   * ([[TracePath.measured]]) — the only samples T-475's equality can be asked of, because only they
   * are a dB some cell under the trace also carries. Always true when `onlyMeasured` was asked for.
   */
  readonly measured: boolean;
}

/**
 * The core of `paths` as [[TraceCoreSample]]s, **one per device pixel column**, in increasing x.
 *
 * ## Why this is a pixel-centre walk and not a list of the vertices
 *
 * A vertex is on the stroke's centreline, but a *pixel* is only fully covered if its own centre is
 * within `half − 1` px of that line (`tracepass.ts`: `cov = clamp((1 − |e|) · halfPx)`), and rounding
 * a vertex to the nearest pixel centre moves it by up to 0.707 px diagonally — which on a 3 px stroke
 * leaves ≈ 0.79 coverage, i.e. a fifth of the bloom-over-cell behind it mixed in. Measured, reporting
 * vertices: 58 of 110 stated samples carried the stated ink, and the misses were all that shape
 * (`said [42,187,194]`, pixel `[48,166,172]` — duller and desaturated, which is the achromatic bloom
 * showing through).
 *
 * Walking pixel **columns** removes the rounding instead of tolerating it: for each column the
 * centreline's y at that column's own centre `x = i + 0.5` is where the stroke is, and the pixel row
 * whose centre is nearest to it is off by at most 0.5 px *vertically*, hence at most 0.5 px
 * perpendicular however steep the segment is — fully covered for any `widthPx ≥ 3`. So every column a
 * path crosses yields exactly one sample, and it is a pixel the shader wrote the vertex colour into
 * without blending. The colour is interpolated along the segment the same way the rasteriser
 * interpolates it, because that is the colour that pixel was given.
 *
 * `measured` marks the columns that hold a measured vertex, i.e. where the dB is a cell's own dB and
 * not a point on the interpolant between two columns — the only ones whose colour a cell can be asked
 * about. (Measured over every column instead: 84–88 % matched, with every miss on the steep flank of
 * the FM carrier, where 5.6 px of interpolation climbs between two pooled columns whose own colours
 * are 20/255 apart. That is the interpolant being drawn honestly, not the ramp diverging, and it does
 * not belong in the same number.)
 *
 * **`onlyMeasured` is a COST option, and the caller on the render path uses it.** This runs in the
 * frame it describes, so its cost is paid whether or not anyone reads it: measured at a 1440 px pane,
 * 256 pooled columns, `JSON.stringify` included — 0.86 ms and a 36.8 kB payload per frame reporting
 * every column, 0.49 ms / 6.0 kB walking every column but reporting only the measured ones, and
 * **0.28 ms / 6.0 kB** skipping the allocation for the rest. On a 16 ms budget the first is 5 %,
 * which is not a diagnostic's share of a frame; the last is what `ui/src/app/centre/surface.ts` asks
 * for, and nothing at all with the layer off (its default).
 *
 * `canvasHPx` is the drawing buffer's height in device px (GL's y runs up from its bottom, and `rect`
 * is in that frame); `dpr` scales device px to the CSS px a screenshot and `getBoundingClientRect`
 * are indexed by. A path narrower than 3 px is skipped rather than reported unreliably: the caller
 * asks about the core, and a 1.4 px max-hold has none.
 */
export function coreSamples(
  paths: readonly TracePath[],
  rect: PaneRect,
  canvasHPx: number,
  dpr: number,
  { onlyMeasured = false }: { onlyMeasured?: boolean } = {},
): TraceCoreSample[] {
  const out: TraceCoreSample[] = [];
  const s = dpr > 0 ? dpr : 1;
  if (!(rect.w > 0) || !(rect.h > 0)) return out;
  const seen = new Set<number>();
  for (const p of paths) {
    const half = Math.max(0.5, p.widthPx / 2);
    if (half < 1.5) continue;
    const n = p.xy.length >> 1;
    const xAt = (i: number) => rect.x + ((p.xy[2 * i] + 1) / 2) * rect.w;
    const yAt = (i: number) => canvasHPx - (rect.y + ((p.xy[2 * i + 1] + 1) / 2) * rect.h);
    // The pixel columns a measured vertex falls in — the ones whose dB is a cell's dB.
    const exact = new Set<number>();
    for (const i of p.measured) {
      const x = xAt(i);
      if (Number.isFinite(x)) exact.add(Math.floor(x));
    }
    for (let i = 0; i < n - 1; i++) {
      // Ordered by x so the walk is one direction; the colours travel with their ends.
      const fwd = xAt(i) <= xAt(i + 1);
      const a = fwd ? i : i + 1, b = fwd ? i + 1 : i;
      const x0 = xAt(a), x1 = xAt(b), y0 = yAt(a), y1 = yAt(b);
      if (!Number.isFinite(x0) || !Number.isFinite(x1) || !Number.isFinite(y0) || !Number.isFinite(y1)) continue;
      const dx = x1 - x0;
      for (let col = Math.ceil(x0 - 0.5); col + 0.5 <= x1; col++) {
        if (col < 0 || seen.has(col)) continue;
        const isMeasured = exact.has(col);
        if (onlyMeasured && !isMeasured) continue;
        const t = dx > 1e-9 ? (col + 0.5 - x0) / dx : 0;
        const yTop = y0 + (y1 - y0) * t;
        const row = Math.round(yTop - 0.5);
        if (!(row >= 0)) continue;
        seen.add(col);
        const mix = (k: number) => p.rgb[3 * a + k] + (p.rgb[3 * b + k] - p.rgb[3 * a + k]) * t;
        out.push({
          xPx: (col + 0.5) / s,
          yPx: (row + 0.5) / s,
          rgb: [Math.round(255 * mix(0)), Math.round(255 * mix(1)), Math.round(255 * mix(2))],
          measured: isMeasured,
        });
      }
    }
  }
  out.sort((a, b) => a.xPx - b.xPx);
  return out;
}

/** One earlier row behind the current slice: when it is, what it measured, and how faded. */
export interface TraceShadow {
  /** The instant that row's cell **ends** at — capture time, from the pane's own window. */
  readonly tAtNs: number;
  readonly cols: Float32Array;
  /** 1 = the row immediately before the slice. */
  readonly age: number;
  readonly alpha: number;
}

/**
 * **The afterglow: the rows just before the pane's time position**, from the pyramid.
 *
 * Phosphor persistence on a CRT analyser is the previous sweeps still glowing. The naive client-side
 * version of that is a ring buffer of frames the page happened to receive — which shows the last few
 * seconds of *wall clock* whatever window the pane is on, so a pane scrubbed to last hour glows with
 * now. That is the same defect as a trace pinned to now, one layer down.
 *
 * So the shadows are asked for the same way the slice is: cell `k` back from the slice's own cell, at
 * the level the pane was **drawn** at. They come out of the very tiles the pane just rendered — a
 * shadow is inside the pane's box by construction, and [[maxHoldColumns]] only reads resident tiles —
 * so this works identically live and scrubbed, and costs no fetch of its own.
 *
 * A shadow whose cell starts before the pane's window is **dropped**, not clamped: the strip is a
 * view over this pane's window, and glowing with a row the pane is not showing would be the inverse
 * of the gap rule.
 */
export function persistenceSlices(
  lat: Lattice,
  cache: Pick<TileCache<TilePlanes>, "peek">,
  box: Box,
  levelF: number,
  levelT: number,
  device: string,
  n: number,
  tAtNs: number,
  depth = TRACE_PERSISTENCE,
): TraceShadow[] {
  const out: TraceShadow[] = [];
  const cell = lat.t0Ns * 2 ** Math.max(0, levelT);
  if (!(cell > 0) || !(depth > 0)) return out;
  const here = sliceWindow(lat, levelT, tAtNs);
  for (let k = 1; k <= depth; k++) {
    const end = here.t0Ns - (k - 1) * cell;
    if (end - cell < box.t0Ns) break;
    const cols = maxHoldColumns(lat, cache, { ...box, t0Ns: end - cell, t1Ns: end }, levelF, levelT, device, n);
    let any = false;
    for (let c = 0; c < cols.length && !any; c++) any = Number.isFinite(cols[c]);
    if (!any) continue;
    out.push({ tAtNs: end, cols, age: k, alpha: SHADOW_ALPHA[Math.min(k, SHADOW_ALPHA.length) - 1] });
  }
  return out;
}

/**
 * **Afterglow cells whose tile is in hand but whose ANSWER stops short of them** (T-532's horizon).
 *
 * A resident copy states how far forward its records reach (`asOfNs`); cells after that were
 * recorded after the copy was built and read as not-observed in it. For the afterglow that is the
 * tile-boundary case: a slice in the first rows of tile N has its earlier rows at the END of tile
 * N-1, and a copy of N-1 fetched while it was still the live tile does not reach them. Counted per
 * tile over the afterglow's own window (the `depth` cells before the slice's cell that lie inside the
 * pane), with [[persistenceSlices]]'s bounds. A copy with no finite horizon reaches everywhere, as in
 * the renderer's own horizon clip. Tiles not in hand at all are the PaneReport's business.
 */
export function persistenceShortTiles(
  lat: Lattice,
  cache: Pick<TileCache<TilePlanes>, "peek">,
  box: Box,
  levelF: number,
  levelT: number,
  device: string,
  tAtNs: number,
  depth = TRACE_PERSISTENCE,
): number {
  const cell = lat.t0Ns * 2 ** Math.max(0, levelT);
  if (!(cell > 0) || !(depth > 0)) return 0;
  const end = sliceWindow(lat, levelT, tAtNs).t0Ns;
  const k = Math.min(depth, Math.floor((end - box.t0Ns) / cell));
  if (!(k > 0)) return 0;
  const win = { ...box, t0Ns: end - k * cell, t1Ns: end };
  let n = 0;
  for (const a of tilesFor(lat, win, levelF, levelT, device)) {
    const e = cache.peek(a);
    if (!e) continue;
    const asOf = e.data.asOfNs;
    if (asOf === null || !Number.isFinite(asOf)) continue;
    if (asOf < Math.min(win.t1Ns, extentOf(lat, a).t1Ns)) n++;
  }
  return n;
}

/**
 * **What the readout says when there is no afterglow to draw**, and it must not say more than it knows.
 *
 * [[persistenceSlices]] reads only what RESIDENT tiles answered, so an empty afterglow has two
 * different causes: the rows before the pane's instant are in hand and none of them was observed —
 * a fact about the radio — or they are not in hand: their tile is pending, drawn by a coarse
 * stand-in or refused (the PaneReport's counts), or in hand but answered only up to a horizon short
 * of them (`shortTiles`, [[persistenceShortTiles]]) — a fact about memory and latency. The slice and
 * max-hold clauses beside it have always kept those apart ("not loaded is not unobserved"); the
 * afterglow clause said "no earlier row in this window" in every case, and `ui/e2e/app-trace.e2e.mjs`
 * read exactly that off a scrubbed pane whose max-hold over 45 s was its own one slice — the slice's
 * tile freshly started, the rows before it in a tile drawn by a coarse stand-in.
 *
 * Conservative on purpose: any place of the pane not in hand withholds the claim, even one the
 * afterglow's cells do not touch. Over-hedging costs a word; the other direction is a false
 * statement that there was nothing there.
 */
export function afterglowAbsence(
  report: { readonly pending: number; readonly fallbacks: number; readonly refused: number },
  shortTiles = 0,
): string {
  const { pending, fallbacks, refused } = report;
  if (pending + fallbacks + refused + shortTiles === 0) return "afterglow — no earlier row in this window";
  const parts = [`${pending} pending`, `${fallbacks} coarse stand-in${fallbacks === 1 ? "" : "s"}`];
  if (refused > 0) parts.push(`${refused} refused`);
  if (shortTiles > 0) parts.push(`${shortTiles} answered only up to an earlier instant`);
  return `afterglow — the rows before this instant are not all in hand yet (${parts.join(", ")}) — not loaded is not unobserved`;
}
