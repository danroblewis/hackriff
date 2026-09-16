// Pure frequency/time axis math for the live view (T-045). No DOM, no WebGL: unit-tested in
// ui/test/axis.test.ts, including against numbers the real pipeline produced
// (ui/test/spectrum_axis.golden.json, written by tests/e2e/tests/spectrum_axis.rs).
//
// Spectrum row convention (hk-stream `spectrum`, `rf32_le`; `StreamHeader::spectrum_bin_hz`;
// hk-dsp `Spectrum` bin order): bin i of N is DC-centred at
//   f_i = center_hz + (i − floor(N/2)) · bandwidth_hz / N
// and covers [f_i − df/2, f_i + df/2], df = bandwidth_hz / N. The full displayed band is therefore
// [f_0 − df/2, f_{N−1} + df/2]; for even N that is center − bw/2 − df/2 .. center + bw/2 − df/2
// (bin N/2 is DC, bin 0 is −fs/2).

/** Row geometry from the stream header. */
export interface Geometry {
  centerHz: number;
  bandwidthHz: number;
  bins: number;
}

/** A displayed frequency window (the zoom state), Hz. */
export interface View {
  loHz: number;
  hiHz: number;
}

/** Geometry from a spectrum stream header; null when it lacks the fields. */
export function geometryOf(h: { center_hz?: number; bandwidth_hz?: number; fft_size?: number }): Geometry | null {
  const { center_hz: c, bandwidth_hz: bw, fft_size: n } = h;
  if (c === undefined || !Number.isFinite(c) || bw === undefined || !(bw > 0) || n === undefined || !(n >= 1)) return null;
  return { centerHz: c, bandwidthHz: bw, bins: Math.floor(n) };
}

export const binWidthHz = (g: Geometry) => g.bandwidthHz / g.bins;

/** Centre frequency of bin `i`, Hz. */
export const binHz = (g: Geometry, i: number) => g.centerHz + (i - Math.floor(g.bins / 2)) * binWidthHz(g);

/** Nearest bin to `hz` (may be outside 0..N−1). */
export const hzToBin = (g: Geometry, hz: number) => Math.round((hz - g.centerHz) / binWidthHz(g)) + Math.floor(g.bins / 2);

/**
 * The centre of the bin under `hz` (clamped to the row): what a readout can honestly report, since
 * the spectrum resolves one bin. The pixel at the middle of the canvas sits half a bin below DC for
 * even N and reads the DC bin, i.e. the tuned centre.
 */
export const snapHz = (g: Geometry, hz: number) => binHz(g, Math.min(g.bins - 1, Math.max(0, hzToBin(g, hz))));

/** The whole band: first bin's lower edge to last bin's upper edge. */
export function fullView(g: Geometry): View {
  const df = binWidthHz(g);
  return { loHz: binHz(g, 0) - df / 2, hiHz: binHz(g, g.bins - 1) + df / 2 };
}

/** Frequency at horizontal fraction `x` (0 = left edge, 1 = right edge) of a view. */
export const fracToHz = (v: View, x: number) => v.loHz + x * (v.hiHz - v.loHz);

/** Horizontal fraction of `hz` in a view (outside 0..1 when out of view). */
export const hzToFrac = (v: View, hz: number) => (hz - v.loHz) / (v.hiHz - v.loHz);

/** Texture-coordinate window [u0, u1] of a view, where 0..1 spans the full band (bins in order). */
export function textureWindow(g: Geometry, v: View): [number, number] {
  const f = fullView(g);
  return [hzToFrac(f, v.loHz), hzToFrac(f, v.hiHz)];
}

/**
 * Pointer position → fraction of an element's box. Uses client coordinates and the element's
 * bounding rect, not `offsetX` (relative to whichever element is the event target, which changes
 * with overlays and pointer capture).
 */
export function pointerFrac(clientX: number, rect: { left: number; width: number }): number {
  if (!(rect.width > 0)) return 0;
  return Math.min(1, Math.max(0, (clientX - rect.left) / rect.width));
}

/** A zoom to [loHz, hiHz], clamped to the full band and at least `minBins` bins wide. */
export function zoomTo(g: Geometry, loHz: number, hiHz: number, minBins = 8): View {
  const full = fullView(g);
  let lo = Math.min(loHz, hiHz), hi = Math.max(loHz, hiHz);
  const minW = Math.min(full.hiHz - full.loHz, minBins * binWidthHz(g));
  if (hi - lo < minW) {
    const mid = (lo + hi) / 2;
    lo = mid - minW / 2;
    hi = mid + minW / 2;
  }
  if (lo < full.loHz) { hi += full.loHz - lo; lo = full.loHz; }
  if (hi > full.hiHz) { lo -= hi - full.hiHz; hi = full.hiHz; }
  return { loHz: Math.max(full.loHz, lo), hiHz: Math.min(full.hiHz, hi) };
}

/** Zooms by `factor` (> 1 zooms in) keeping the frequency under fraction `x` of the view fixed (T-051 wheel/pinch). */
export function zoomAt(g: Geometry, v: View, x: number, factor: number, minBins = 8): View {
  if (!(factor > 0) || !Number.isFinite(factor)) return v;
  const hz = fracToHz(v, x), w = (v.hiHz - v.loHz) / factor;
  return zoomTo(g, hz - x * w, hz - x * w + w, minBins);
}

/** Wheel delta → zoom factor (`deltaMode` 0 pixels, 1 lines, 2 pages; scrolling up zooms in). */
export function wheelFactor(deltaY: number, deltaMode = 0): number {
  const px = deltaY * (deltaMode === 1 ? 16 : deltaMode === 2 ? 400 : 1);
  return Math.exp(-Math.max(-400, Math.min(400, px)) * 0.002);
}

/**
 * A view panned by `deltaHz` (positive: towards higher frequencies), keeping its width and clamped
 * to the band. `overflowHz` is how far past the band edge the pan asked to go (negative below,
 * positive above, 0 inside): the display zoom is client-side, so leaving the band needs an explicit
 * retune.
 */
export function panView(g: Geometry, v: View, deltaHz: number): { view: View; overflowHz: number } {
  const full = fullView(g), w = Math.min(v.hiHz - v.loHz, full.hiHz - full.loHz);
  let lo = v.loHz + deltaHz, overflow = 0;
  if (lo < full.loHz) { overflow = lo - full.loHz; lo = full.loHz; }
  if (lo + w > full.hiHz) { overflow = lo + w - full.hiHz; lo = full.hiHz - w; }
  return { view: { loHz: lo, hiHz: lo + w }, overflowHz: overflow };
}

/** The centre that would show a pan's requested (unclamped) view in the middle of a retuned band. */
export const panRetuneCenter = (clamped: View, overflowHz: number) => (clamped.loHz + clamped.hiHz) / 2 + overflowHz;

/**
 * The texels a screen pixel max-pools (T-051 fix of the T-045 review nit): the pixel centred at
 * texture coordinate `u` covers [u − uPerPx/2, u + uPerPx/2]; returns `[x0, count]` of the texels
 * overlapping it (at most `maxTaps`, centred), or the single texel under `u` when a pixel is
 * narrower than a texel. Mirrored by the waterfall and persistence shaders.
 */
export function poolWindow(u: number, uPerPx: number, texW: number, maxTaps = 64): [number, number] {
  const a = (u - uPerPx / 2) * texW, b = (u + uPerPx / 2) * texW;
  const clampX = (x: number) => Math.min(texW - 1, Math.max(0, x));
  if (!(b - a > 1)) return [clampX(Math.floor(u * texW)), 1];
  let x0 = Math.floor(a), n = Math.max(1, Math.ceil(b) - x0);
  if (n > maxTaps) { x0 = Math.floor((a + b) / 2) - Math.floor(maxTaps / 2); n = maxTaps; }
  const lo = clampX(x0), hi = clampX(x0 + n - 1);
  return [lo, hi - lo + 1];
}

/** A frequency selection between two horizontal fractions of a view. */
export function selectionHz(v: View, x0: number, x1: number): { loHz: number; hiHz: number; bandwidthHz: number } {
  const a = fracToHz(v, Math.min(x0, x1)), b = fracToHz(v, Math.max(x0, x1));
  return { loHz: a, hiHz: b, bandwidthHz: b - a };
}

/** Where a vertical fraction of the live canvas falls: the spectrum (top) or a waterfall row. */
export type YHit = { area: "spectrum" } | { area: "waterfall"; rowsBack: number };

/**
 * Vertical fraction `y` (0 = top) → area. The top `specFrac` of the canvas is the spectrum; the
 * rest shows `rows` waterfall rows, newest at the top (`rowsBack` 0 = newest row).
 */
export function yHit(y: number, specFrac: number, rows: number): YHit {
  if (y < specFrac) return { area: "spectrum" };
  const r = (y - specFrac) / (1 - specFrac);
  return { area: "waterfall", rowsBack: Math.min(rows - 1, Math.max(0, Math.floor(r * rows))) };
}

/**
 * The one canonical time→screen mapping (T-337, the user's "one shared time axis" invariant):
 * absolute capture time (Unix s) → rows-back, fractional, 0 = the newest drawn row.
 *
 * It is supplied by whatever holds the rows — `Waterfall.rowsBackAt`, which inverts the very
 * per-row capture times `timeAt` reads back, so it is the exact inverse of `timeAt` at every
 * integer row. **It is never a nominal rate.** A rows-per-second figure (the spectrum header's
 * declared `sample_rate_hz`, a configured `rows_per_s`) describes how fast rows are *produced*,
 * not where the ones on screen sit: a gated row, a dropped run or a backlog-skipped frame
 * advances capture time without advancing the ring, and on a gated stream the declared rate is
 * deliberately up to 10 % above the actual row rate (`hk_pipeline::class::RowPlan::declared_hz`).
 * Dividing an age by such a rate therefore drifts against the rows, linearly with age — which the
 * user names as a violation of the invariant, not a cosmetic bug.
 */
export type RowsBackAt = (tS: number) => number;

/** A row's absolute capture time (Unix s), `rowsBack` rows before the newest drawn row; NaN when
 * that row carries none (`Waterfall.timeAt`). Times decrease as `rowsBack` grows. */
export type TimeAt = (rowsBack: number) => number;

/**
 * The canonical mapping, built by inverting the rows' own capture times: absolute capture time →
 * rows-back, fractional. `n` is how many rows-back carry a time (a contiguous run from the newest).
 *
 * **A row's time is the first sample of its span**, not its midpoint or its end: that is the
 * contract for both sources of it — a spectrum record's `t` is the timestamp of the first element
 * (`docs/stream-contract.md` §5.2, produced from `SpectrumFrame.t.sample_index`, the frame's
 * `frame_start`), and a review grid's row *k* starts at `t0_s + k·t_cell_s` (`docs/api.md`,
 * span-matched resolution). So row *k* covers capture time `[timeAt(k), timeAt(k−1))`, and the
 * boundary drawn at position *k* — between rows *k−1* and *k* — is at `timeAt(k−1)`. Position 0 is
 * the live edge, one row's duration past the newest row's start.
 *
 * Inverting *that* is what makes the mapping exact: an emission occupying exactly row *k* (capture
 * time `[timeAt(k), timeAt(k−1))`) places at exactly `[k, k+1]` — on the energy, not a row above
 * it. Interpolation inside a row is linear in its own duration; a time off either end extrapolates
 * from the nearest boundary pair (negative past the live edge, `> n` past the oldest row), so an
 * overlay crossing an edge still places the part of it that is on screen. Binary search, since the
 * boundaries descend. NaN until two rows carry a time — one row gives no duration to interpolate.
 */
export function rowsBackAt(timeAt: TimeAt, n: number, tS: number): number {
  if (!Number.isFinite(tS) || !(n > 1)) return NaN;
  const u = (j: number) => (j === 0 ? 2 * timeAt(0) - timeAt(1) : timeAt(j - 1));
  const between = (j: number) => {
    const a = u(j), d = a - u(j + 1); // the row's own duration, > 0 for a sane clock
    return d > 0 ? j + (a - tS) / d : j;
  };
  if (tS >= u(0)) return between(0); // past the live edge: negative
  if (tS <= u(n)) return between(n - 1); // older than the oldest row's start
  let lo = 0, hi = n; // u(lo) > tS > u(hi); bisect to adjacent boundaries
  while (hi - lo > 1) {
    const mid = (lo + hi) >> 1;
    if (u(mid) > tS) lo = mid; else hi = mid;
  }
  return between(lo);
}

/** Rows-back → vertical fraction of the **waterfall pane**; the exact inverse of [[yHit]]'s
 * `rowsBack`, clamped to the pane. */
export const rowFrac = (back: number, rows: number): number => Math.min(rows, Math.max(0, back)) / rows;

/**
 * The vertical span [top, bottom] of a time range as fractions of the **waterfall pane**, placed
 * through `rowsBackAt` — the same mapping the rows themselves were drawn with — so an overlay sits
 * on, and scrolls with, the exact energy it describes. Null when the range has scrolled off the
 * rows held (or is not yet on screen).
 *
 * The pane, not the canvas: the rows are drawn in it, and since T-362 so is every time-varying
 * overlay, in the same pass (`ui/src/timebox.ts`). There is deliberately **no** canvas-fraction
 * variant of this any more — one existed for the DOM overlay layer, and a placement helper in
 * another coordinate system is how a second layer gets started.
 */
export function timeSpanRows(tLo: number, tHi: number, rowsBackAt: RowsBackAt, rows: number): [number, number] | null {
  if (!Number.isFinite(tLo) || !Number.isFinite(tHi)) return null;
  const backNew = rowsBackAt(tHi), backOld = rowsBackAt(tLo);
  if (!Number.isFinite(backNew) || !Number.isFinite(backOld)) return null;
  if (backNew >= rows || backOld < -1) return null;
  return [rowFrac(backNew, rows), rowFrac(backOld, rows)];
}

/** Evenly spaced "nice" ticks inside a view (1/2/5 × 10^k Hz steps). */
export function ticks(v: View, maxTicks: number): { hz: number; frac: number }[] {
  const span = v.hiHz - v.loHz;
  if (!(span > 0) || maxTicks < 1) return [];
  const raw = span / maxTicks, p = Math.pow(10, Math.floor(Math.log10(raw)));
  const step = [1, 2, 5, 10].map((m) => m * p).find((s) => s >= raw) ?? 10 * p;
  const out: { hz: number; frac: number }[] = [];
  for (let k = Math.ceil(v.loHz / step); k * step <= v.hiHz; k++) out.push({ hz: k * step, frac: hzToFrac(v, k * step) });
  return out;
}

/** MHz text with enough decimals to resolve `resolutionHz`. */
export function fmtMHz(hz: number, resolutionHz: number): string {
  const d = Math.min(6, Math.max(1, Math.ceil(-Math.log10(Math.max(resolutionHz, 1) / 1e6) - 1e-9)));
  return (hz / 1e6).toFixed(d);
}

/** A bandwidth in Hz, kHz or MHz. */
export function fmtBandwidth(hz: number): string {
  if (hz >= 1e6) return `${(hz / 1e6).toFixed(3)} MHz`;
  if (hz >= 1e3) return `${(hz / 1e3).toFixed(2)} kHz`;
  return `${hz.toFixed(0)} Hz`;
}

/** "centre 100.8000 MHz · span 2.4000 MHz · 585.9 Hz/bin", plus the zoom window when zoomed. */
export function describe(g: Geometry, v: View): string {
  const df = binWidthHz(g), full = fullView(g);
  let s = `centre ${(g.centerHz / 1e6).toFixed(4)} MHz · span ${(g.bandwidthHz / 1e6).toFixed(4)} MHz · ${df.toFixed(1)} Hz/bin`;
  if (v.loHz > full.loHz + df / 2 || v.hiHz < full.hiHz - df / 2) s += ` · view ${fmtMHz(v.loHz, df)}–${fmtMHz(v.hiHz, df)} MHz`;
  return s;
}
