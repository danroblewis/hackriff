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

// ---- time-window select (T-194) ----

/** Pointer travel (px) that turns a scrub into a deliberate time-window drag (mirrors the live
 * view's DRAG_PX rule, ADR-0013 §5, centre/overlays.ts). */
export const DRAG_PX = 6;

export interface TimeWindow { t_lo: number; t_hi: number }

/**
 * The `[t_lo, t_hi]` a drag from percent `pctA` to `pctB` along the band selects, chronologically
 * ordered (through `scrubToTime`'s mapping, so the LIVE edge resolves to `nowS`); null when it has
 * no width (both ends resolved to the same instant — e.g. both past the LIVE threshold).
 */
export function timeWindowFromScrub(pctA: number, pctB: number, nowS: number, windowS: number = WINDOW_S): TimeWindow | null {
  const a = scrubToTime(pctA, nowS, windowS), b = scrubToTime(pctB, nowS, windowS);
  const t_lo = Math.min(a.tS, b.tS), t_hi = Math.max(a.tS, b.tS);
  return t_hi > t_lo ? { t_lo, t_hi } : null;
}

/** A default name for a time-window selection: its UTC wall-clock span, to the second. */
export function timeRegionName(t_lo: number, t_hi: number): string {
  const f = (t: number) => `${new Date(t * 1000).toISOString().slice(11, 19)}Z`;
  return `${f(t_lo)}–${f(t_hi)}`;
}

// ---- past events on the scrubber, and what backs a scrub-back (T-263, ADR-0017 TM-7) ----
//
// The marks are the durable events a user scrubs *to*. They are placed from timespans the API
// already serves per row — `presence.last_interval` (the latest interval intersecting the request's
// window) and `recurrence.recent[]` (its recent appearances) — and nothing here decides which rows
// qualify or what state they are in: that is the backend's predicate (thin-client rule, §1).
//
// Deliberately no liveness on a mark. `presence.last_interval` carries `open`; a `recurrence`
// appearance carries no such field, so a mark that claimed one for both would be asserting
// something about half of them that was never measured. A mark says only *an event happened over
// this timespan*; liveness is the list's answer, from `presence.liveness`.

/** The `/api/inventory` row fields the scrubber's marks are placed from. */
export interface EventRow {
  id: string;
  state: string;
  presence?: { last_interval: { t_start_s: number; t_end_s: number } | null };
  recurrence?: { recent?: readonly { t_start_s: number; t_end_s: number }[] } | null;
}

export interface EventMark {
  id: string; state: "candidate" | "confirmed";
  leftPct: number; widthPct: number;
  tStartS: number; tEndS: number;
}

/** Narrowest a mark is drawn, % of the band. A one-off burst is milliseconds against a 48 h band,
 * so its true width rounds to nothing; it is still the event this whole model exists to make
 * first-class (ADR-0017 §1.2), so it is drawn at this floor. The floor is a **drawing** minimum —
 * `tStartS`/`tEndS` keep the measured timespan, and nothing reads the width back as a duration. */
export const MIN_MARK_PCT = 0.3;

/** Most marks drawn at once. A render cap, never a claim about what happened: the newest are kept,
 * and History (TM-8) is where the complete catalogue lives. */
export const MAX_MARKS = 400;

/**
 * Past events placed on the retained band, newest last. One mark per distinct timespan a row
 * reports — its `presence.last_interval` and each `recurrence.recent[]` appearance, de-duplicated
 * where they name the same span. A timespan that ended before the retained window, or that has not
 * started, is omitted rather than clamped to the band's edge (which would put an event at a time it
 * never happened). Rows in neither list are skipped.
 */
export function eventMarks(rows: readonly EventRow[], nowS: number, windowS: number = WINDOW_S): EventMark[] {
  const out: EventMark[] = [];
  for (const r of rows) {
    if (r.state !== "candidate" && r.state !== "confirmed") continue;
    const state = r.state;
    const spans = [...(r.recurrence?.recent ?? [])];
    const last = r.presence?.last_interval;
    if (last) spans.push(last);
    const seen = new Set<string>();
    for (const s of spans) {
      if (![s.t_start_s, s.t_end_s].every(Number.isFinite)) continue;
      const key = `${s.t_start_s}|${s.t_end_s}`;
      if (seen.has(key)) continue;
      seen.add(key);
      if (nowS - s.t_end_s > windowS || s.t_start_s > nowS) continue;
      const leftPct = pctForAgo(nowS - s.t_start_s, windowS), rightPct = pctForAgo(nowS - s.t_end_s, windowS);
      out.push({
        id: r.id, state,
        leftPct: Math.min(leftPct, 100 - MIN_MARK_PCT),
        widthPct: Math.max(MIN_MARK_PCT, rightPct - leftPct),
        tStartS: s.t_start_s, tEndS: s.t_end_s,
      });
    }
  }
  out.sort((a, b) => a.tEndS - b.tEndS);
  return out.length > MAX_MARKS ? out.slice(out.length - MAX_MARKS) : out;
}

/** A mark's hover text: when it ended and how long it lasted — the measured timespan, never the
 * drawn width. A sub-second event reads in milliseconds rather than rounding to "0 s". */
export function eventMarkTitle(m: Pick<EventMark, "tStartS" | "tEndS">, nowS: number): string {
  const dur = Math.max(0, m.tEndS - m.tStartS);
  const lasted = dur < 1 ? `${Math.round(dur * 1000)} ms` : dur < 60 ? `${dur.toFixed(1)} s` : agoText(dur);
  return `${agoText(Math.max(0, nowS - m.tEndS))} ago · lasted ${lasted}`;
}

// ---- what backs a scrub-back: the IQ ring, and whether anything was observed at all ----
//
// Two different "nothing here" answers that must never be collapsed. *Nothing was on the air* is a
// measurement. *No data for this window* is the absence of one — the receiver was not listening, or
// the IQ ring has already rolled past it. The user acts differently on each, so neither is ever
// rendered as the other, and an unanswered poll reads as **unknown**, not as either.

/** The `GET /api/iqbuffer` status fields the timeline reads (docs/api.md `IqBufferStatus`). */
export interface RingStatus { enabled: boolean; t0: number | null; t1: number | null }

/** One `coverage_summary.gaps[]` run of `GET /api/history`: a stretch in which **no** cell of the
 * grid was observed. "A gap is never reported as quiet." */
export interface CoverageGap { t0_s: number; t1_s: number }

/** The retained IQ ring placed on the band (ADR-0014; 30 min on staging against a 48 h band), or
 * `null` when there is no ring, it holds nothing, or the status has not been answered — all of
 * which are "unknown", never "the ring is empty". */
export function ringSpan(ring: RingStatus | null, nowS: number, windowS: number = WINDOW_S): SelSpan | null {
  if (!ring?.enabled || ring.t0 === null || ring.t1 === null) return null;
  if (!(ring.t1 > ring.t0) || nowS - ring.t1 > windowS) return null;
  const leftPct = pctForAgo(nowS - ring.t0, windowS), rightPct = pctForAgo(nowS - ring.t1, windowS);
  return rightPct > leftPct ? { id: "ring", leftPct, widthPct: rightPct - leftPct } : null;
}

/** Whether the reviewed instant still has IQ behind it: `"live"` at the live edge, `"ring"` inside
 * the retained ring, `"outside-ring"` past it, `"unknown"` when no status has been answered. */
export type IqBacking = "live" | "ring" | "outside-ring" | "unknown";

export function iqBackingAt(tS: number, live: boolean, ring: RingStatus | null): IqBacking {
  if (live) return "live";
  if (!ring?.enabled || ring.t0 === null || ring.t1 === null) return "unknown";
  return tS >= ring.t0 && tS <= ring.t1 ? "ring" : "outside-ring";
}

/** Whether the reviewed instant falls in a stretch the history grid reports as observed by nothing;
 * `null` when no coverage summary has been answered (unknown, not "observed"). */
export function inCoverageGap(tS: number, gaps: readonly CoverageGap[] | null): boolean | null {
  return gaps === null ? null : gaps.some((g) => tS >= g.t0_s && tS <= g.t1_s);
}

/**
 * What the scrubbed instant is backed by, in one clause for the capture note. Empty while live (the
 * band's own coverage text already speaks for the live edge).
 *
 * An unobserved stretch wins over everything else: it is the one case where an empty Candidate list
 * means *the receiver was not listening*, and saying anything else there would let silence read as
 * a measurement.
 */
export function scrubDataNote(tS: number, live: boolean, ring: RingStatus | null, gaps: readonly CoverageGap[] | null): string {
  if (live) return "";
  if (inCoverageGap(tS, gaps)) return "nothing was observed here — no data for this window, not a quiet band";
  switch (iqBackingAt(tS, live, ring)) {
    case "ring": return "IQ retained for this window";
    case "outside-ring": return "past the IQ ring — lists come from stored history, no IQ detail";
    default: return "IQ coverage unknown";
  }
}

export interface SelSpan { id: string; leftPct: number; widthPct: number }

/**
 * Time-windowed selections placed on the retained band (the inverse of `scrubToTime`, via
 * `pctForAgo`). A selection with no time window, or whose window falls wholly outside the
 * retained window, is omitted.
 */
export function selectionSpans(list: readonly { id: string; t_lo?: number; t_hi?: number }[], nowS: number, windowS: number = WINDOW_S): SelSpan[] {
  const out: SelSpan[] = [];
  for (const s of list) {
    if (s.t_lo === undefined || s.t_hi === undefined) continue;
    const leftPct = pctForAgo(nowS - s.t_lo, windowS), rightPct = pctForAgo(nowS - s.t_hi, windowS);
    if (rightPct > leftPct) out.push({ id: s.id, leftPct, widthPct: rightPct - leftPct });
  }
  return out;
}
