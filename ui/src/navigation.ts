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

/**
 * Whether `hz` is inside one of the grid's own bands — **not** "nearest band", which is what
 * [`snapCenter`] answers.
 *
 * The two are different questions and only this one may refuse a gesture. `snapCenter` deliberately
 * walks an out-of-band request *inward* to the nearest reachable point, so asking it "did this
 * snap?" would call every out-of-range centre achievable at the band edge. A region-select that is
 * genuinely off the end of what the front end (or, on a replay, the recording) covers has to be
 * told apart from one that is merely off the synthesiser's grid.
 */
export function containsCenter(g: CenterGrid | null, hz: number): boolean {
  if (!g || !Number.isFinite(hz)) return false;
  return g.ranges_hz.some((r) => Array.isArray(r) && hz >= r[0] && hz <= r[1]);
}

/**
 * The **smallest** achievable span that still covers `needHz`, or null when none does.
 *
 * "Smallest that covers" and "nearest" are different answers, and only the first is right for a
 * region-select: a discrete ladder whose nearest entry is *narrower* than the region would open a
 * window that cuts the selection in half, and a wider-than-necessary one would throw away
 * resolution the front end could have given. So a discrete list takes the least entry `>= needHz`
 * (the boundary counts as covering — a window exactly as wide as the region contains it), and a
 * continuous rate range takes `needHz` itself lifted to the range's floor.
 *
 * Null is "no configuration covers this", which is one of the three refusals the user allows.
 */
export function smallestCoveringSpan(g: FrequencyGrid | null, needHz: number): number | null {
  if (!g || !Number.isFinite(needHz) || !(needHz > 0)) return null;
  const s = g.spans_hz;
  if ("values" in s) {
    let best: number | null = null;
    for (const v of s.values) if (Number.isFinite(v) && v >= needHz && (best === null || v < best)) best = v;
    return best;
  }
  if (!(s.max >= s.min)) return null;
  return needHz <= s.max ? Math.max(s.min, needHz) : null;
}

/** Why no achievable configuration can capture a requested region. The whole list — anything not
 * here **retunes** rather than refusing, which is the half of the invariant that is easy to lose. */
export type RetuneRefusal =
  /** No grid was reported, so nothing can be claimed achievable. */
  | "no_grid"
  /** The region's centre is outside every band the front end (or the recording) covers. */
  | "center_out_of_range"
  /** Wider than one live window: no single capture can hold it. */
  | "span_too_wide";

/** The capture configuration that would cover a region, or the reason none can. */
export type RetunePlan =
  | { ok: true; centerHz: number; spanHz: number; snappedCenter: boolean; source: DetailSource }
  | { ok: false; reason: RetuneRefusal };

/**
 * The `(centre, span)` a front end would have to take to capture `[lo, hi]` — **the one navigator
 * decision that commands the radio** (CLAUDE.md, the frequency navigator).
 *
 * Unlike time, which is always a view over IQ already captured, a frequency outside the current
 * window can only be reached by tuning there. So this computes the config rather than offering one:
 * centre at the region's centre snapped to the achievable grid, and the **smallest** sample
 * rate/span that still covers the region *after* that snap (snapping moves the centre, and a span
 * chosen before it could leave an edge of the selection outside the window).
 *
 * It refuses only when nothing can capture the region — the three [`RetuneRefusal`]s. Everything
 * else is a retune.
 */
export function retunePlan(g: FrequencyGrid | null, lo: number, hi: number): RetunePlan {
  if (!g) return { ok: false, reason: "no_grid" };
  if (!Number.isFinite(lo) || !Number.isFinite(hi) || !(hi > lo)) return { ok: false, reason: "no_grid" };
  const want = (lo + hi) / 2;
  if (!containsCenter(g, want)) return { ok: false, reason: "center_out_of_range" };
  // Refuse a span wider than one live window before snapping anything: the front end's
  // instantaneous bandwidth is the hard bound, and `max_live_span_hz` is the backend's statement of
  // it. An unreported bound is not evidence of a wide window, but it is not grounds to refuse
  // either — the span ladder below then decides.
  const max = g.max_live_span_hz;
  if (max !== null && hi - lo > max) return { ok: false, reason: "span_too_wide" };
  const snapped = snapCenter(g, want);
  const centerHz = snapped ?? want;
  // Cover the region *from the centre the radio will actually sit on*, not from the one asked for:
  // a span picked before the snap can leave an edge of the selection outside the window, and on a
  // coarse tuning grid that is not a rounding detail but a miss.
  const needHz = 2 * Math.max(centerHz - lo, hi - centerHz);
  // The fallback is for the one case where only the snap pushed it over the top rung: the region
  // itself fits a live window, so the widest covering span is right and refusing would be a refusal
  // about a fraction of a tuning step.
  const spanHz = smallestCoveringSpan(g, needHz) ?? smallestCoveringSpan(g, hi - lo);
  if (spanHz === null) return { ok: false, reason: "span_too_wide" };
  return { ok: true, centerHz, spanHz, snappedCenter: snapped !== null, source: detailOf(g, spanHz) };
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
