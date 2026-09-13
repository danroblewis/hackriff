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
 * The vertical span [top, bottom] (fractions of the canvas) of a time range on the waterfall,
 * given the newest row's time and the row period; null when the range has scrolled off (or is
 * not yet on screen).
 */
export function timeSpanY(tLo: number, tHi: number, newestT: number, rowPeriodS: number, specFrac: number, rows: number): [number, number] | null {
  if (![tLo, tHi, newestT].every(Number.isFinite) || !(rowPeriodS > 0)) return null;
  const backNew = (newestT - tHi) / rowPeriodS, backOld = (newestT - tLo) / rowPeriodS;
  if (backNew >= rows || backOld < -1) return null;
  const y = (back: number) => specFrac + (Math.min(rows, Math.max(0, back)) / rows) * (1 - specFrac);
  return [y(backNew), y(backOld)];
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
