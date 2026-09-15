// Click-to-inspect (T-044): the inventory emitter nearest a clicked frequency. Pure helpers only;
// the MUI app's rendering lives in ui/src/app/centre and ui/src/app/explore.

type Extent = { f_lo_hz: number; f_hi_hz: number; bandwidth_hz: number; last_seen_s: number };

/** Distance from `hz` to an emitter's frequency extent (0 inside it). */
export const extentDistanceHz = (r: { f_lo_hz: number; f_hi_hz: number }, hz: number) =>
  hz < r.f_lo_hz ? r.f_lo_hz - hz : hz > r.f_hi_hz ? hz - r.f_hi_hz : 0;

/** The row nearest `hz` within `maxHz` (ties: narrower, then most recently seen); null if none. */
export function nearestEntry<T extends Extent>(rows: readonly T[], hz: number, maxHz: number): T | null {
  let best: T | null = null, bestD = Infinity;
  for (const r of rows) {
    const d = extentDistanceHz(r, hz);
    if (d > maxHz) continue;
    const better = d < bestD || (d === bestD && best !== null &&
      (r.bandwidth_hz < best.bandwidth_hz || (r.bandwidth_hz === best.bandwidth_hz && r.last_seen_s > best.last_seen_s)));
    if (better) { best = r; bestD = d; }
  }
  return best;
}

/** Search half-width around a click: 4 bins, 0.5 % of the view, or 5 kHz, whichever is widest. */
export const inspectHalfWidthHz = (viewSpanHz: number, binWidthHz: number) => Math.max(4 * binWidthHz, 0.005 * viewSpanHz, 5e3);
