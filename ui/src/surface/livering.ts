// **The live ring** (T-1042 / LSR-1): the rows `/ws/spectrum/live` publishes, kept in a ring and
// painted at each following pane's live edge — so the newest rows are on screen because they were
// *recorded*, and the live edge is never gated on a tile.
//
// The invariant this serves, in the user's words (docs/16, "Live rendering"): *"the live view renders
// like a classic SDR waterfall: rows append in real time as they are recorded, and live is NEVER
// gated on batch tile generation."* The tile pyramid remains the *history*: it is what a zoom, a
// scrub or a second pane reads, and what survives this page. What it is not, any more, is the source
// of the newest two seconds — a tile has to be produced, revalidated and re-fetched before a row
// that already exists can appear, and every latency on that path is a stall the user sees as jank.
//
// # What this module is, and what it is not
//
// It is a **ring of rows with their own capture times**, plus the pure arithmetic that says where
// those rows land in a pane. It holds no GL (the texture it is uploaded into lives in
// `./surface.ts`, beside the tile planes, so there is still exactly one context and one ramp) and it
// makes no request of its own: the socket is `app/centre/live-edge.ts`'s, already open, already
// consumed for the edge and the trace. LSR-1 therefore adds **no new subscriber** and no new work on
// the capture thread — T-453's constraint is about what the *backend* pays per arriving row, and the
// producer here is one that was already running (docs/api.md: `spectrum/live` is computed while
// somebody reads it, and this page already was).
//
// It is **not** the client-side history accumulator T-457 declined to build. Its capacity is fixed
// at construction ([[DEFAULT_RING_ROWS]] rows ≈ 40 s at the production row period), it answers
// nothing about a time it does not hold, and every row in it carries the backend's absolute capture
// time — never a browser clock, never an index counted up from mount.
//
// # Honesty rules it keeps, and why each one is a rule
//
//  1. **A row is placed by its own capture time**, and a run of rows is laid out at the cadence
//     *measured across that run* ([[RingSpan]]). The stream can drop rows (a busy producer, a
//     re-plumb); assuming a nominal period across a gap would slide every row after it against the
//     tiles below, which is the T-388 family of drift.
//  2. **A gap is a gap.** A delta longer than [[GAP_FACTOR]] row periods ends the span, so nothing
//     is stretched over rows that were never delivered, and the strip the tile lane is allowed to
//     skip is only the *newest contiguous run* ([[RingFrame.live]]) — the ring paints every span it
//     holds, and excludes tiles under only the one it can vouch for continuously.
//  3. **A retune is a different band, so the ring is emptied.** Rows of the old window drawn under
//     the new one would not be stale, they would be false — the same reason `live-edge.ts` drops its
//     one held row and nulls `edgeTS`.
//  4. **A row must not be drawn wider than it was measured.** Zoomed out in time, one ring row is
//     less than a pixel tall and NEAREST sampling would show one row in N and hide the others — an
//     aliased pick presented as the picture. Below [[MIN_ROW_PX]] the ring stands aside and the
//     pyramid answers, which is what the pyramid's folds are *for* ([[ringPlan]]).
//  5. **A slot nothing was written into is [[CELL.UNKNOWN]]**, never grey and never a level: the
//     ring has no row there, and grey is reserved for "the radio never looked".
import { CELL } from "./cellrule";
import type { Box } from "./lattice";
import type { LiveFrame } from "./trace";
import { intersect } from "./vscale";

/**
 * Rows one ring holds: ~40 s at the production row period (40.1 ms, docs/api.md's display stream),
 * and 3 MB of texture at a 1024-bin front end — the same order as one 256² tile pair, for the whole
 * live edge.
 *
 * Deep enough that a following pane at the live-IQ tier is painted from rows alone (a level-0 tile is
 * 10.24 s), shallow enough that it is not a history: what is older than the ring is the pyramid's,
 * which is where the answer for it has always come from.
 */
export const DEFAULT_RING_ROWS = 1024;

/** How many row periods a delta may reach before it is a gap rather than the next row. */
export const GAP_FACTOR = 1.75;

/**
 * The height, in device pixels, one ring row must have before the ring is drawn at all.
 *
 * Under it the pane is zoomed out past the rows' own resolution: NEAREST sampling would draw one row
 * and drop its neighbours, which is a *sample* of the live edge presented as the live edge. The
 * pyramid's coarse levels are max-folds of these same rows, so standing aside hands the question to
 * the one answer that does not lose the burst between two rows (docs/16 §4).
 */
export const MIN_ROW_PX = 0.75;

/** How many recent deltas the row period is measured over. */
const PERIOD_WINDOW = 16;

/** A contiguous run of ring rows: `rows` texture rows from `row0`, covering `[t0Ns, t1Ns)`. */
export interface RingSpan {
  /** First texture row of the run (a row index into the ring, not a row number). */
  readonly row0: number;
  readonly rows: number;
  /** The first row's own capture time. */
  readonly t0Ns: number;
  /** The end of the last row's extent: its capture time plus the cadence measured across this run. */
  readonly t1Ns: number;
}

/**
 * What the renderer reads, once per frame: the ring's planes, its band, and the runs inside it.
 *
 * `value`/`state` are the ring's own buffers — `capacity` rows of `nf` cells, row-major, the same
 * layout and the same `dBFS/Hz` scale as a tile's planes (`hk-pipeline`'s `PowerUnit::DbfsPerHz`,
 * which is what `max_db` is too), so one display range and one ramp colour both.
 */
export interface RingFrame {
  /** Bumped by every [[LiveRing.clear]] — a geometry change, or a retune. A holder of a texture
   * built from this ring re-uploads when it changes, rather than patching rows of a band that ended. */
  readonly epoch: number;
  /** Rows appended since the epoch, monotone. The newest row is at slot `(writes - 1) % capacity`. */
  readonly writes: number;
  readonly nf: number;
  readonly capacity: number;
  readonly f0Hz: number;
  readonly f1Hz: number;
  /** The row period, **measured** over the most recent deltas — never a header's nominal rate. */
  readonly rowPeriodNs: number;
  readonly value: Float32Array;
  readonly state: Uint8Array;
  /** Every run the ring holds, oldest first. */
  readonly spans: readonly RingSpan[];
  /** The newest run: the only extent the tile lane may skip (rule 2 in the header). */
  readonly live: RingSpan | null;
}

/**
 * The rows of one band, newest `capacity` kept.
 *
 * Fed by the one socket `app/centre/live-edge.ts` already holds. Nothing here polls, fetches or
 * reads a clock: a row's time is the row's own.
 */
export class LiveRing {
  readonly capacity: number;
  private nf = 0;
  private f0Hz = 0;
  private f1Hz = 0;
  private value = new Float32Array(0);
  private state = new Uint8Array(0);
  private times = new Float64Array(0);
  private writesN = 0;
  private epochN = 0;
  /** Rows refused because their time did not move forward (a re-plumbed stream can repeat one). */
  dropped = 0;

  constructor(opts: { rows?: number } = {}) {
    const rows = opts.rows ?? DEFAULT_RING_ROWS;
    this.capacity = Math.max(2, Math.floor(rows));
  }

  get writes(): number { return this.writesN; }
  get epoch(): number { return this.epochN; }

  /**
   * File one published row.
   *
   * A row of a **different band** empties the ring first (header rule 3): `live-edge.ts` clears on
   * the header, and this is the same rule applied to the rows themselves, so a re-plumb that is seen
   * only in the row geometry cannot leave one band's rows under another's.
   */
  push(fr: LiveFrame): void {
    const nf = fr.db.length;
    if (!(nf > 0) || !Number.isFinite(fr.tNs) || !(fr.f1Hz > fr.f0Hz)) return;
    if (nf !== this.nf || fr.f0Hz !== this.f0Hz || fr.f1Hz !== this.f1Hz) {
      this.clear();
      this.nf = nf;
      this.f0Hz = fr.f0Hz;
      this.f1Hz = fr.f1Hz;
      this.value = new Float32Array(nf * this.capacity);
      // Unknown, not grey and not a measurement: the ring holds no row here yet (header rule 5).
      this.state = new Uint8Array(nf * this.capacity).fill(CELL.UNKNOWN);
      this.times = new Float64Array(this.capacity);
    } else if (this.writesN > 0 && fr.tNs <= this.times[(this.writesN - 1) % this.capacity]) {
      // Time did not move forward. A row placed below one already at the edge would draw the past
      // over the present; the backend splices a looping replay onto one monotone axis (T-474), so
      // this is a floor under a promise, not a repair of a wrap this client expects.
      this.dropped++;
      return;
    }
    const slot = this.writesN % this.capacity;
    this.value.set(fr.db, slot * this.nf);
    this.state.fill(CELL.OBSERVED, slot * this.nf, (slot + 1) * this.nf);
    this.times[slot] = fr.tNs;
    this.writesN++;
  }

  /** Forget every row: a retune, or a band this ring is not about. */
  clear(): void {
    if (this.writesN === 0 && this.nf === 0) return;
    this.writesN = 0;
    this.dropped = 0;
    this.epochN++;
    this.nf = 0;
    this.f0Hz = 0;
    this.f1Hz = 0;
    this.value = new Float32Array(0);
    this.state = new Uint8Array(0);
    this.times = new Float64Array(0);
  }

  /**
   * The ring as the renderer reads it, or `null` while it cannot yet place a row.
   *
   * Two rows are the minimum, and that is the honest floor rather than a convenience: a row's
   * *extent* is its start plus a period, and with one row nothing has been measured about the
   * cadence. At 25–40 rows a second the second row is ~40 ms behind the first.
   */
  frame(): RingFrame | null {
    if (this.writesN < 2 || this.nf === 0) return null;
    const order = this.order();
    const period = this.periodNs(order);
    if (!(period > 0)) return null;
    const spans = spansOf(order, this.times, period);
    if (!spans.length) return null;
    return {
      epoch: this.epochN, writes: this.writesN, nf: this.nf, capacity: this.capacity,
      f0Hz: this.f0Hz, f1Hz: this.f1Hz, rowPeriodNs: period,
      value: this.value, state: this.state,
      spans, live: spans[spans.length - 1],
    };
  }

  /** The retained slots, oldest first. */
  private order(): number[] {
    const n = Math.min(this.writesN, this.capacity);
    const first = this.writesN <= this.capacity ? 0 : this.writesN % this.capacity;
    const out = new Array<number>(n);
    for (let i = 0; i < n; i++) out[i] = (first + i) % this.capacity;
    return out;
  }

  /**
   * The row period, **measured**: the median of the last [[PERIOD_WINDOW]] deltas.
   *
   * The median rather than the mean because a dropped row is one enormous delta, and a mean over it
   * would stretch every row's extent to cover rows that never arrived. The stream header's rate is
   * not consulted at all — a nominal rate is what the producer aims at, not what it delivered.
   */
  private periodNs(order: readonly number[]): number {
    const d: number[] = [];
    for (let i = Math.max(1, order.length - PERIOD_WINDOW); i < order.length; i++) {
      const dt = this.times[order[i]] - this.times[order[i - 1]];
      if (dt > 0) d.push(dt);
    }
    if (!d.length) return 0;
    d.sort((a, b) => a - b);
    return d[Math.floor(d.length / 2)];
  }
}

/**
 * Split the retained rows into runs: at a texture wrap (row `capacity - 1` to row `0` is not one
 * quad) and at a gap longer than [[GAP_FACTOR]] periods.
 *
 * A run's own cadence is `(last - first) / (rows - 1)`, and its extent ends one cadence past its last
 * row. So the rows inside a run are laid out at the rate they actually arrived at, and a run of one
 * row is one period tall — the only extrapolation in this module, and it is the same one every
 * waterfall makes about the row it is currently drawing.
 */
function spansOf(order: readonly number[], times: Float64Array, periodNs: number): RingSpan[] {
  const out: RingSpan[] = [];
  let start = 0;
  const close = (from: number, to: number): void => {
    const rows = to - from;
    if (rows <= 0) return;
    const t0 = times[order[from]], tLast = times[order[to - 1]];
    const cadence = rows > 1 ? (tLast - t0) / (rows - 1) : periodNs;
    out.push({ row0: order[from], rows, t0Ns: t0, t1Ns: tLast + cadence });
  };
  for (let i = 1; i < order.length; i++) {
    // The retained slots are contiguous modulo the capacity, so the one place a run cannot stay one
    // quad is where the ring wrapped back to slot 0.
    const wrapped = order[i] === 0;
    const dt = times[order[i]] - times[order[i - 1]];
    if (wrapped || !(dt > 0) || dt > GAP_FACTOR * periodNs) {
      close(start, i);
      start = i;
    }
  }
  close(start, order.length);
  return out;
}

/** One quad: the run, and the region of the surface it is painted over in this pane. */
export interface RingDraw {
  readonly span: RingSpan;
  readonly region: Box;
}

/**
 * What a pane paints from the ring this frame, and what the tile lane may therefore leave alone.
 *
 * Re-derived per pane per frame from the pane's own box — never cached, for the reason nothing else
 * in the renderer is (T-388: a rectangle carried across frames is a rectangle that drifts out of
 * step with the scroll it was measured against).
 */
export interface RingPlan {
  /**
   * The extent the ring paints **continuously**, clipped to the pane: what the tile lane skips
   * ([[ringCovers]]). Only ever the newest run — see the header's rule 2.
   */
  readonly cover: Box | null;
  /** The quads, oldest run first, so the newest rows are submitted last and win where they overlap. */
  readonly draws: readonly RingDraw[];
  /** One ring row's height in this pane, device px: the eligibility measurement, reported so a
   * readout can say why the ring stood aside rather than leaving it a mystery. */
  readonly rowPx: number;
}

const EMPTY = { cover: null, draws: [] } as const;

/** The pane's plan. `rowPx` below [[MIN_ROW_PX]] draws nothing: the pyramid answers (rule 4). */
export function ringPlan(fr: RingFrame, box: Box, hPx: number, minRowPx = MIN_ROW_PX): RingPlan {
  const spanNs = box.t1Ns - box.t0Ns;
  const rowPx = spanNs > 0 && hPx > 0 ? (hPx * fr.rowPeriodNs) / spanNs : 0;
  if (!(rowPx >= minRowPx)) return { ...EMPTY, rowPx };
  const draws: RingDraw[] = [];
  for (const span of fr.spans) {
    const region = intersect({ f0Hz: fr.f0Hz, f1Hz: fr.f1Hz, t0Ns: span.t0Ns, t1Ns: span.t1Ns }, box);
    if (region) draws.push({ span, region });
  }
  const live = fr.live
    ? intersect({ f0Hz: fr.f0Hz, f1Hz: fr.f1Hz, t0Ns: fr.live.t0Ns, t1Ns: fr.live.t1Ns }, box)
    : null;
  return { cover: live, draws, rowPx };
}

/**
 * **Is everything this pane shows of `ext` already painted by the ring?**
 *
 * The test is against the tile's extent **clipped to the pane**, not against the whole tile, and
 * that is the whole point of it: a live tile's extent runs past the live edge into rows that do not
 * exist yet, so full containment would never be true of the one tile the ring is replacing. What
 * matters is the strip of it the pane would draw.
 */
export function ringCovers(cover: Box, ext: Box, box: Box): boolean {
  const vis = intersect(ext, box);
  if (!vis) return false;
  return vis.f0Hz >= cover.f0Hz && vis.f1Hz <= cover.f1Hz && vis.t0Ns >= cover.t0Ns && vis.t1Ns <= cover.t1Ns;
}

/**
 * The ring a pane draws from, asked **per pane, per frame**.
 *
 * Per pane because a ring is a live-edge thing: a pane frozen on an hour ago is a view over recorded
 * data, which the pyramid answers and always has, and painting the newest rows into it would be a
 * live claim about a window that is not live. The host decides which panes qualify (`preview.ts`
 * asks `PaneModel.isFollowing`, the same read `refreshLiveEdge` makes), so this renderer still
 * learns nothing about follow, pause or devices.
 */
export interface LiveRingSource {
  ringFor(paneId: string): RingFrame | null;
}

/**
 * **The trace reads the ring too** (T-1047 / LSR-6).
 *
 * `./trace.ts`'s slice and max-hold were built entirely from the pyramid, because at T-457 that was
 * the only place either question could be answered. Since LSR-1 the tile lane is *told to stand
 * aside* over the ring's own extent ([[ringCovers]]), so a tile-only trace now has a hole exactly
 * where the picture beneath it is freshest: the newest few seconds are on screen from rows, and the
 * trace over the same seconds would read "unobserved" from a tile that was never requested. These two
 * functions are the ring's own answers to the same two questions [[maxHoldColumns]]/[[sliceColumns]]
 * ask of the pyramid, so a caller can fold them in exactly where the ring, not the pyramid, is the
 * fresher source — never instead of the pyramid, only where a tile genuinely has nothing to say.
 */

/**
 * The ring row whose own extent contains `tNs` — the ring's form of "genuinely finer than the
 * pyramid" ([[trace.ts]]'s old `liveFrameFits`): a row that CONTAINS the instant asked about is a
 * strictly better answer than any cell that merely spans it, at every level. Searches the newest
 * span first, since a following pane's time position is almost always at or near the live edge.
 * `null` when the ring holds no row there — older than the ring, or a retune emptied it.
 */
export function ringRowAt(ring: RingFrame, tNs: number): { readonly slot: number; readonly tNs: number } | null {
  if (!Number.isFinite(tNs)) return null;
  for (let i = ring.spans.length - 1; i >= 0; i--) {
    const span = ring.spans[i];
    if (tNs < span.t0Ns || tNs >= span.t1Ns || !(span.rows > 0)) continue;
    const cadence = (span.t1Ns - span.t0Ns) / span.rows;
    if (!(cadence > 0)) continue;
    const offset = Math.min(span.rows - 1, Math.floor((tNs - span.t0Ns) / cadence));
    return { slot: (span.row0 + offset) % ring.capacity, tNs: span.t0Ns + offset * cadence };
  }
  return null;
}

/**
 * One ring row, pooled onto `n` columns spanning `box`'s frequency window.
 *
 * The ring's own [[trace.ts]] `sampleFrame`: max-pooling where a column covers more than one bin (a
 * narrow burst between two sample points must survive the collapse, the same reason the server folds
 * this way), the nearest bin replicated where a column is narrower than one. A column outside the
 * ring's band, or a cell this ring has not written, is left `NaN` — never a floor, never a level the
 * ring does not hold.
 */
export function sampleRingRow(ring: RingFrame, slot: number, box: Box, n: number): Float32Array {
  const out = new Float32Array(n).fill(Number.NaN);
  const { nf, f0Hz, f1Hz } = ring;
  const span = box.f1Hz - box.f0Hz;
  if (!(nf > 0) || !(f1Hz > f0Hz) || !(span > 0) || !(n > 0)) return out;
  const binHz = (f1Hz - f0Hz) / nf;
  const colHz = span / n;
  const base = slot * nf;
  for (let c = 0; c < n; c++) {
    const lo = box.f0Hz + c * colHz, hi = lo + colHz;
    if (hi <= f0Hz || lo >= f1Hz) continue;
    let i0 = Math.floor((Math.max(lo, f0Hz) - f0Hz) / binHz);
    let i1 = Math.ceil((Math.min(hi, f1Hz) - f0Hz) / binHz);
    i0 = Math.max(0, Math.min(nf - 1, i0));
    i1 = Math.max(i0 + 1, Math.min(nf, i1));
    let m = -Infinity;
    for (let i = i0; i < i1; i++) {
      const k = base + i;
      if (ring.state[k] !== CELL.OBSERVED) continue;
      const v = ring.value[k];
      if (Number.isFinite(v) && v > m) m = v;
    }
    if (m > -Infinity) out[c] = m;
  }
  return out;
}

/**
 * **The slice, from the ring**: the row covering the pane's own instant `tAtNs`, pooled across the
 * whole viewport. `null` when the ring has no row there — a scrub past the ring's ~40 s, a band the
 * ring is not about, or no ring at all — and the caller falls back to [[sliceColumns]] exactly as it
 * did before the ring existed.
 */
export function ringSliceColumns(ring: RingFrame, box: Box, n: number, tAtNs: number): Float32Array | null {
  const at = ringRowAt(ring, tAtNs);
  if (!at) return null;
  return sampleRingRow(ring, at.slot, box, n);
}

/**
 * **The ring-covered max-hold**: the column-wise maximum over every ring row whose own extent falls
 * inside `box`'s time window.
 *
 * **`box`'s frequency extent decides where the `n` columns fall, and it must be the CALLER's own
 * column domain — ordinarily the pane's whole frequency window, the same one the caller divided into
 * `n` to answer the pyramid — never a narrower band such as [[RingPlan.cover]]'s.** A column is index
 * `c` of `n` evenly spaced across `box.f0Hz..box.f1Hz`; if `box` is narrower than the range the
 * caller's `n` columns actually span, column `c` here and column `c` there are different frequencies,
 * and folding the two arrays together by index puts this function's answer at the wrong place in the
 * caller's picture the moment the pane is wider than the ring's own band. A column outside the ring's
 * own band comes back `NaN` regardless — [[sampleRingRow]] checks that itself — so widening `box`'s
 * frequency costs nothing.
 *
 * `box`'s TIME extent is the one field this is meant to narrow: pass [[RingPlan.cover]]'s `t0Ns`/
 * `t1Ns` (with the caller's own frequency window) to fold in only the strip [[ringCovers]] tells the
 * tile lane not to request — not the pane's whole window, over the rest of which the pyramid is still
 * the answer, and still the finer one where both exist (history reaches back further than this ring
 * ever will). Folding the two together is safe because a max-hold is idempotent and associative
 * (`hk-api`'s `MAX_HOLD_RULE`): taking the greater of a ring answer and a pyramid answer over the same
 * cell is still a max-hold over the union.
 */
export function ringMaxHoldColumns(ring: RingFrame, box: Box, n: number): Float32Array {
  const out = new Float32Array(n).fill(Number.NaN);
  const span = box.f1Hz - box.f0Hz;
  if (!(span > 0) || !(n > 0)) return out;
  for (const s of ring.spans) {
    if (s.t1Ns <= box.t0Ns || s.t0Ns >= box.t1Ns || !(s.rows > 0)) continue;
    const cadence = (s.t1Ns - s.t0Ns) / s.rows;
    if (!(cadence > 0)) continue;
    const r0 = Math.max(0, Math.floor((box.t0Ns - s.t0Ns) / cadence));
    const r1 = Math.min(s.rows, Math.ceil((box.t1Ns - s.t0Ns) / cadence));
    for (let r = r0; r < r1; r++) {
      const row = sampleRingRow(ring, (s.row0 + r) % ring.capacity, box, n);
      for (let c = 0; c < n; c++) {
        const v = row[c];
        if (Number.isFinite(v) && !(out[c] >= v)) out[c] = v;
      }
    }
  }
  return out;
}
