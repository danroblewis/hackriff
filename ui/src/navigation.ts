// Snapping navigation to achievable capture states (T-341). Pure arithmetic over the grid the
// backend handed us: no DOM, no client, no measurement. Unit-tested in ui/test/navigation.test.ts.
//
// **The split, from the user:** *"Backend reports the achievable (centre, span) grid +
// full-spectrum survey overview; UI does the navigators/gestures/snap/styling."*
//
// So the rule set lives in `GET /api/navigation` (docs/api.md) — which centres exist, which spans a
// device can open, which time cells the history holds, and whether a requested state is live-IQ
// backed or survey overview. This module is the other half: given that grid, find the nearest point
// on it. That is the same class of work as ui/src/axis.ts — pixel↔Hz mapping and clamping over
// already-known state — and deliberately **not** a second opinion about what the radio can do. If a
// question here needs to know about noise floors, occupancy or what the front end is capable of,
// it is the backend's question, not this file's.
//
// Two rules the arithmetic must not break, both from the exploration-first honesty principle:
//
//  1. **An unknown grid snaps nothing.** `centerStepHz: null` means the source cannot state its
//     tuning step — never that any centre is reachable. `snapCenter` returns null, and a caller
//     must then not claim the device can sit exactly where the user pointed.
//  2. **A view never claims detail the front end did not capture.** `detailOf` is the client-side
//     copy of the backend's own verdict, for styling a gesture in flight; the authoritative answer
//     is `resolved.source` on the wire, and the two must agree.

/**
 * Just the centre axis of the grid: the bounds and the step.
 *
 * Split out because `/api/control/state`'s `device` object carries the same two facts
 * (`frequency_ranges_hz`, `tuning_step_hz`), so a surface that already has the device state can
 * snap a centre without a second fetch.
 */
export interface CenterGrid {
  /** Tunable centre bounds, Hz. */
  ranges_hz: [number, number][];
  /** Grid spacing anchored at 0 Hz, or **null when the source cannot say** — never read as 1 Hz
   * and never as continuous. A null step snaps nothing. */
  center_step_hz: number | null;
}

/** `frequency` from `GET /api/navigation`: the achievable (centre, span) grid of one front end. */
export interface FrequencyGrid extends CenterGrid {
  device_id: string | null;
  driver: string;
  controllable: boolean;
  /** `"uniform"` with a step, or `"unknown"` — never read as 1 Hz and never as continuous. */
  center_step: "uniform" | "unknown";
  /** The span axis: a live window's span *is* its sample rate. */
  spans_hz: { min: number; max: number } | { values: number[] };
  /** Widest span that is still one capture window; wider is survey overview. */
  max_live_span_hz: number | null;
  current: { center_hz: number; span_hz: number } | null;
}

/** One discrete resolution tier of the spectrum-history pyramid. */
export interface HistoryTier { level: number; t_cell_s: number; f_cell_hz: number; max_age_s: number | null }

/** `time` from `GET /api/navigation`. */
export interface TimeGrid {
  tiers: HistoryTier[];
  min_t_cell_s: number | null;
  max_t_cell_s: number | null;
  latest_s: number | null;
}

/** Which tier backs what is on screen. Ordered by how much detail it claims. */
export type DetailSource = "live-iq" | "spectrum-history" | "survey-overview";

export interface NavigationGrid { frequency: FrequencyGrid | null; time: TimeGrid | null }

/**
 * The nearest achievable centre to `hz`, or **null when the grid is unknown**.
 *
 * Null is not "leave it where it is": it means nothing may claim the device can sit exactly there.
 * A caller that wants to draw the pointer anyway draws it as a request, not as a tuned state.
 *
 * Mirrors `SourceCapabilities::snap_center_hz` (hk-core): snap to the grid, then walk one step
 * *inward* if the nearest point fell outside the band — inside and reachable beats outside and
 * merely close.
 */
export function snapCenter(g: CenterGrid | null, hz: number): number | null {
  const step = g?.center_step_hz ?? null;
  if (!g || step === null || !(step > 0) || !Number.isFinite(step) || !Number.isFinite(hz)) return null;
  const range = nearestRange(g, hz);
  if (!range) return null;
  const [lo, hi] = range;
  let v = Math.round(hz / step) * step;
  if (v < lo) v = Math.ceil(lo / step) * step;
  if (v > hi) v = Math.floor(hi / step) * step;
  return v >= lo && v <= hi ? v : null;
}

/** The band containing `hz`, else the nearest one; null when the grid lists none. */
export function nearestRange(g: CenterGrid, hz: number): [number, number] | null {
  let best: [number, number] | null = null, bestD = Infinity;
  for (const r of g.ranges_hz) {
    const d = hz < r[0] ? r[0] - hz : hz > r[1] ? hz - r[1] : 0;
    if (d < bestD) { best = r; bestD = d; }
  }
  return best;
}

/**
 * The nearest achievable span to `hz`: a continuous rate range clamps, a discrete list picks its
 * nearest entry. Null when the grid reports no spans.
 */
export function snapSpan(g: FrequencyGrid | null, hz: number): number | null {
  if (!g || !Number.isFinite(hz)) return null;
  const s = g.spans_hz;
  if ("values" in s) {
    let best: number | null = null, bestD = Infinity;
    for (const v of s.values) {
      const d = Math.abs(v - hz);
      if (Number.isFinite(v) && d < bestD) { best = v; bestD = d; }
    }
    return best;
  }
  return s.max >= s.min ? Math.min(s.max, Math.max(s.min, hz)) : null;
}

/**
 * The tier that answers a requested time cell: **the finest tier no finer than `wantS`** — err
 * coarser, never finer (T-334). A request below every tier is answered by the finest one, which is
 * coarser than asked and reported as such. Null with no tiers.
 */
export function snapTimeCell(t: TimeGrid | null, wantS: number): HistoryTier | null {
  if (!t?.tiers.length || !Number.isFinite(wantS)) return t?.tiers[0] ?? null;
  let best: HistoryTier | null = null;
  for (const tier of t.tiers) if (tier.t_cell_s <= wantS) best = tier;
  return best ?? t.tiers[0];
}

/**
 * Whether a span of `spanHz` could have come from **one** capture window — the client-side copy of
 * the backend's verdict, for styling a gesture before the answer lands.
 *
 * The boundary counts as inside: a view exactly as wide as the sample rate is one window's worth.
 * A hertz past it is overview, because every extra hertz came from a different dwell. **Without a
 * `max_live_span_hz` the answer is `survey-overview`** — not knowing the window is not evidence
 * that a span fits inside it, and under-claiming costs a styling cue while over-claiming is the lie
 * the invariant forbids.
 */
export function detailOf(g: FrequencyGrid | null, spanHz: number): DetailSource {
  const max = g?.max_live_span_hz ?? null;
  if (max === null || !(spanHz > 0) || !Number.isFinite(spanHz) || spanHz > max) return "survey-overview";
  return "live-iq";
}

/** A state resolved against the grid, and which axes had to move to reach it. */
export interface SnappedState {
  centerHz: number | null;
  spanHz: number | null;
  tier: HistoryTier | null;
  source: DetailSource;
  /** Axes that moved; empty when the request was already realizable. */
  snapped: ("centerHz" | "spanHz" | "tCellS")[];
  matched: boolean;
}

/**
 * The nearest realizable `(centre, span[, time cell])` to what a gesture asked for, and the detail
 * claim that comes with it. Asking for a history tier is asking the pyramid, so it drops the claim
 * from `live-iq` to `spectrum-history` even for a span that fits one window — the same rule the
 * backend applies in `resolved.source`.
 */
export function snapState(grid: NavigationGrid, centerHz: number, spanHz: number, wantTCellS?: number): SnappedState {
  const g = grid.frequency;
  const centre = snapCenter(g, centerHz);
  const span = snapSpan(g, spanHz);
  const tier = wantTCellS === undefined ? null : snapTimeCell(grid.time, wantTCellS);
  const snapped: SnappedState["snapped"] = [];
  if (centre !== null && centre !== centerHz) snapped.push("centerHz");
  if (span !== null && span !== spanHz) snapped.push("spanHz");
  if (tier !== null && wantTCellS !== undefined && tier.t_cell_s !== wantTCellS) snapped.push("tCellS");
  const live = detailOf(g, spanHz);
  const source: DetailSource = live === "live-iq" && wantTCellS !== undefined ? "spectrum-history" : live;
  return { centerHz: centre, spanHz: span, tier, source, snapped, matched: snapped.length === 0 };
}

/** Short label for the detail claim, e.g. beside a span readout. Presentation only. */
export const detailLabel = (s: DetailSource): string =>
  s === "live-iq" ? "live IQ" : s === "spectrum-history" ? "history" : "survey overview";
