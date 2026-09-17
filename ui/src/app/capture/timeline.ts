// Capture-timeline pure helpers (ADR-0013 §3.3, §4.4, §8). Owner: T-150. Scrub↔time mapping, the
// "reviewing N ago" wording and the shading arithmetic for a grid the backend measured — no DOM, no
// fetch, no RF constants, unit-tested.
//
// T-338 — **the band's span is not this file's to decide.** It was a hard-coded `WINDOW_S = 48 h`,
// which is the failure the user's invariant names: a scrubber sized from anything but the capture
// window offers times the IQ ring has already overwritten, and looks right while it lies. Every
// function here now takes `windowS` and there is no default, so nothing can silently fall back to a
// constant; `GET /api/timeline` supplies it from the ring's configured retention (docs/api.md,
// "The capture window, and the overview drawn on it").
//
// The band's *content* is not this file's either. Reducing a history grid to drawn columns used to
// happen here (`reduceActivity`, a max over every frequency cell plus a min/max normalisation);
// both were measurements, and T-334's rule puts measurements in the backend. `/api/timeline` now
// serves the compressed grid and its dynamic range, and what is left below is arithmetic over
// numbers the backend already decided.

/** The capture window `GET /api/timeline` reports: the IQ ring's configured retention (ADR-0014),
 * which is what the band spans — never the longer, lossy spectrum-history horizon. */
export interface CaptureWindow {
  /** Band start / end / span, Unix s. */
  t0S: number; t1S: number; spanS: number;
  /** What the ring currently holds, inside the band; `null` when it holds nothing. */
  buffered: { t0S: number; t1S: number } | null;
}

/** The `GET /api/timeline` response fields the band reads. */
export interface TimelineResponse {
  window?: {
    enabled?: boolean;
    retention_s?: number | null;
    t0_s?: number | null; t1_s?: number | null; span_s?: number | null;
    buffered?: { t0_s: number; t1_s: number } | null;
  } | null;
  grid?: OverviewResponse | null;
}

/**
 * The capture window, or `null` when the server reports none — no ring, no retention, or no live
 * edge yet. `null` is **unknown**, and the band must draw it as unknown: a default span here would
 * be the 48 h constant all over again.
 */
export function captureWindow(r: TimelineResponse | null): CaptureWindow | null {
  const w = r?.window;
  if (!w) return null;
  const { t0_s, t1_s, span_s } = w;
  if (typeof t0_s !== "number" || typeof t1_s !== "number" || typeof span_s !== "number") return null;
  if (!(span_s > 0) || !(t1_s > t0_s)) return null;
  const b = w.buffered;
  return {
    t0S: t0_s, t1S: t1_s, spanS: span_s,
    buffered: b && typeof b.t0_s === "number" && typeof b.t1_s === "number" ? { t0S: b.t0_s, t1S: b.t1_s } : null,
  };
}

/** The compressed overview grid `GET /api/timeline` draws the band from: `nt × nf` cells, row-major
 * (time then frequency), already folded onto the capture window by the backend. `null` in `max_db`
 * is **not observed**, never quiet (C26). */
export interface OverviewResponse {
  nt: number; nf: number;
  max_db: readonly (number | null)[];
  range_db?: { lo: number; hi: number } | null;
  /** Cells in the grid, and the ones something was folded into. */
  cells?: number; observed_cells?: number;
}

/** The observed fraction of the capture window, from the grid's own counts; `null` when no grid has
 * been served (unknown, never "nothing was observed"). */
export function observedFraction(grid: OverviewResponse | null | undefined): number | null {
  if (!grid || typeof grid.cells !== "number" || typeof grid.observed_cells !== "number") return null;
  return grid.cells > 0 ? grid.observed_cells / grid.cells : null;
}

/**
 * The 0–1 shade of one overview cell, against the range **the backend measured** (`range_db`), or
 * `null` for a cell nothing was observed in.
 *
 * Deliberately not a normalisation over the values in hand: deciding a band's dynamic range from
 * whatever came back is a measurement, and it belongs where the floor and the occupancy are known.
 * With no range served there is nothing measured to shade against, so every cell reads unknown.
 */
export function overviewShade(grid: OverviewResponse, index: number): number | null {
  const v = grid.max_db[index];
  const r = grid.range_db;
  if (v === null || v === undefined || !r) return null;
  const span = r.hi > r.lo ? r.hi - r.lo : 1;
  return Math.max(0, Math.min(1, (v - r.lo) / span));
}

export interface Scrub { live: boolean; tS: number; agoS: number }

/** Maps a pointer's percent along the band to a time cursor: > 98.5 % is live, otherwise it's
 * `agoS` seconds before `nowS` (0…`windowS`, clamped). `windowS` is the capture window's span and
 * has no default — see the note at the top of this file. */
export function scrubToTime(pct: number, nowS: number, windowS: number): Scrub {
  const clamped = Math.max(0, Math.min(100, pct));
  const live = clamped > 98.5;
  const agoS = live ? 0 : ((100 - clamped) / 100) * windowS;
  return { live, tS: nowS - agoS, agoS };
}

/** The percent along the band for a given "ago" duration (the inverse of `scrubToTime`, for
 * drawing the playhead while reviewing). */
export function pctForAgo(agoS: number, windowS: number): number {
  return Math.max(0, Math.min(100, 100 - (agoS / windowS) * 100));
}

/** "reviewing N ago" wording: minutes under an hour, else hours to one decimal. */
export function agoText(agoS: number): string {
  const mins = Math.round(agoS / 60);
  return mins < 60 ? `${Math.max(0, mins)} min` : `${(mins / 60).toFixed(1)} h`;
}

/** How much of the capture window was observed, from the overview grid the backend served: its own
 * `observed_cells / cells`, over the window it says it spans. `null` (not asked yet, or no window)
 * reads as unknown, never as "nothing observed". */
export function coverageText(observedFraction: number | null, windowS: number | null): string {
  if (observedFraction === null || windowS === null) return "coverage unknown";
  return `${Math.round(observedFraction * 100)}% of the retained ${durationText(windowS)} observed`;
}

/** A capture window's length in words. Unlike `agoText` this keeps seconds, because a retention is
 * commonly seconds (`--iq-retention 90s`) and rounding it to "2 min" would misstate the span the
 * band is claiming to be. */
export function durationText(s: number): string {
  if (s < 90) return `${Math.round(s)} s`;
  if (s < 3600) return `${Math.round(s / 60)} min`;
  return `${(s / 3600).toFixed(1)} h`;
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

/**
 * A key that changes exactly when [[currentSpan]] does (T-386): the band the capture panel's
 * **content** — its overview grid, its coverage fraction and gaps, its event marks — is an answer
 * about.
 *
 * The panel polls once a minute. Before T-386 it read the span *inside* that poll, so a retune or a
 * zoom left up to 60 s of the **previous band's** overview, coverage and marks on screen, labelled
 * as this window's. That is worse than an empty surface: it is the window rule broken by rendering
 * data that exists for some *other* window as if it were this one. Tagging every answer with the
 * band it came from lets the panel discard it the instant the view leaves that band, rather than
 * drawing it until the next tick.
 *
 * Rounded to whole hertz so a float that re-derives to the same tuning does not look like a move.
 */
export function bandKey(span: { loHz: number; hiHz: number } | null): string {
  return span ? `${Math.round(span.loHz)}/${Math.round(span.hiHz)}` : "";
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
export function timeWindowFromScrub(pctA: number, pctB: number, nowS: number, windowS: number): TimeWindow | null {
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

/** Narrowest a mark is drawn, % of the band. A one-off burst is milliseconds against a band of
 * minutes, so its true width rounds to nothing; it is still the event this whole model exists to make
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
export function eventMarks(rows: readonly EventRow[], nowS: number, windowS: number): EventMark[] {
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

/** One `coverage_summary.gaps[]` run of `GET /api/history`: a stretch in which **no** cell of the
 * grid was observed. "A gap is never reported as quiet." */
export interface CoverageGap { t0_s: number; t1_s: number }

/**
 * What the ring **holds** placed on the band (ADR-0014), or `null` when it holds nothing or no
 * window has been answered — both "unknown", never "the ring is empty".
 *
 * T-338: this sits *inside* the band rather than sizing it. The band is the ring's configured
 * retention; a ring part-way through filling covers only part of it, and that difference is the
 * thing this track exists to show.
 */
export function bufferedSpan(w: CaptureWindow | null): SelSpan | null {
  if (!w?.buffered) return null;
  const { t0S, t1S } = w.buffered;
  if (!(t1S > t0S)) return null;
  const leftPct = pctForAgo(w.t1S - t0S, w.spanS), rightPct = pctForAgo(w.t1S - t1S, w.spanS);
  return rightPct > leftPct ? { id: "ring", leftPct, widthPct: rightPct - leftPct } : null;
}

/** Whether the reviewed instant still has IQ behind it: `"live"` at the live edge, `"ring"` inside
 * what the ring holds, `"outside-ring"` past it, `"unknown"` when no window has been answered. */
export type IqBacking = "live" | "ring" | "outside-ring" | "unknown";

export function iqBackingAt(tS: number, live: boolean, w: CaptureWindow | null): IqBacking {
  if (live) return "live";
  if (!w?.buffered) return "unknown";
  return tS >= w.buffered.t0S && tS <= w.buffered.t1S ? "ring" : "outside-ring";
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
export function scrubDataNote(tS: number, live: boolean, w: CaptureWindow | null, gaps: readonly CoverageGap[] | null): string {
  if (live) return "";
  if (inCoverageGap(tS, gaps)) return "nothing was observed here — no data for this window, not a quiet band";
  switch (iqBackingAt(tS, live, w)) {
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
export function selectionSpans(list: readonly { id: string; t_lo?: number; t_hi?: number }[], nowS: number, windowS: number): SelSpan[] {
  const out: SelSpan[] = [];
  for (const s of list) {
    if (s.t_lo === undefined || s.t_hi === undefined) continue;
    const leftPct = pctForAgo(nowS - s.t_lo, windowS), rightPct = pctForAgo(nowS - s.t_hi, windowS);
    if (rightPct > leftPct) out.push({ id: s.id, leftPct, widthPct: rightPct - leftPct });
  }
  return out;
}
