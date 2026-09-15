// Capture-timeline pure helpers (ADR-0013 §3.3, §4.4, §8). Owner: T-150. Activity-band reduction,
// scrub↔time mapping and the "reviewing N ago" wording — no DOM, no fetch, unit-tested. API GAP 1
// (rolling capture buffer / clip export): none of this invents buffered-hours or byte-quota
// numbers; the retained window is a fixed UI constant (this device's own history), and the note
// text uses only `coverage_summary.observed_fraction` from `GET /api/history`.

/** The retained window the capture band always shows (48 h, matching the mockup). */
export const WINDOW_S = 48 * 3600;

/** The `/api/history` fields the activity band needs. */
export interface HistoryGrid { nf: number; nt: number; max_db: readonly (number | null)[] }

/**
 * Reduces a `nt × nf` history grid (row-major: time then frequency) to `columns` values in
 * chronological order: each column is the max `max_db` over its share of time rows and every
 * frequency bin, normalised 0–1 against the grid's own observed range. `null` means no cell in
 * that share of the grid was observed — an unobserved gap, drawn grey (C26), never quiet.
 */
export function reduceActivity(grid: HistoryGrid, columns: number): (number | null)[] {
  const { nf, nt, max_db } = grid;
  if (columns <= 0) return [];
  if (nt <= 0 || nf <= 0) return new Array(columns).fill(null);
  let lo = Infinity, hi = -Infinity;
  for (const v of max_db) if (v !== null) { if (v < lo) lo = v; if (v > hi) hi = v; }
  const range = hi > lo ? hi - lo : 1;
  const out: (number | null)[] = [];
  for (let c = 0; c < columns; c++) {
    const t0 = Math.floor((c * nt) / columns), t1 = Math.max(t0 + 1, Math.floor(((c + 1) * nt) / columns));
    let m: number | null = null;
    for (let t = t0; t < t1; t++) {
      const row = t * nf;
      for (let f = 0; f < nf; f++) {
        const v = max_db[row + f];
        if (v !== null && (m === null || v > m)) m = v;
      }
    }
    out.push(m === null ? null : Math.max(0, Math.min(1, (m - lo) / range)));
  }
  return out;
}

export interface Scrub { live: boolean; tS: number; agoS: number }

/** Maps a pointer's percent along the band to a time cursor: > 98.5 % is live, otherwise it's
 * `agoS` seconds before `nowS` (0…`windowS`, clamped). */
export function scrubToTime(pct: number, nowS: number, windowS: number = WINDOW_S): Scrub {
  const clamped = Math.max(0, Math.min(100, pct));
  const live = clamped > 98.5;
  const agoS = live ? 0 : ((100 - clamped) / 100) * windowS;
  return { live, tS: nowS - agoS, agoS };
}

/** The percent along the band for a given "ago" duration (the inverse of `scrubToTime`, for
 * drawing the playhead while reviewing). */
export function pctForAgo(agoS: number, windowS: number = WINDOW_S): number {
  return Math.max(0, Math.min(100, 100 - (agoS / windowS) * 100));
}

/** "reviewing N ago" wording: minutes under an hour, else hours to one decimal. */
export function agoText(agoS: number): string {
  const mins = Math.round(agoS / 60);
  return mins < 60 ? `${Math.max(0, mins)} min` : `${(mins / 60).toFixed(1)} h`;
}

/** GAP 1 interim coverage note from `coverage_summary.observed_fraction` (no invented buffered
 * hours/bytes); null when the history poll hasn't answered yet. */
export function coverageText(observedFraction: number | null): string {
  return observedFraction === null ? "coverage unknown" : `${Math.round(observedFraction * 100)}% of the last 48 h observed`;
}

/** The current view span to query `/api/history` for: the live geometry once T-152 sets it,
 * else a fallback from the device's tuned centre/rate (both from the store, no analysis). */
export function currentSpan(input: {
  live: { loHz: number; hiHz: number } | null;
  device: { centerHz: number | null; sampleRateHz: number | null };
}): { loHz: number; hiHz: number } | null {
  if (input.live) return input.live;
  const { centerHz, sampleRateHz } = input.device;
  if (centerHz === null || sampleRateHz === null) return null;
  return { loHz: centerHz - sampleRateHz / 2, hiHz: centerHz + sampleRateHz / 2 };
}
