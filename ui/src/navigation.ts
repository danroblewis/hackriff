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

/**
 * How far off DC to place a selection of `needHz` inside a window of `spanHz` — **the dodge, not a
 * nicety** (T-418, and T-409 arriving from the other direction).
 *
 * A front end mixes to baseband, so the centre of every window carries the tuner's own DC/LO
 * leakage spike. T-317 and T-382 both measured it, and the tuner-DC box is one of the few T-394
 * still leaves above 20 dB. A narrowing that centred the target *perfectly* would therefore land it
 * on the worst part of the band and make it the one thing in the window that cannot be cleanly
 * demodulated. So the window is opened deliberately lopsided: the selection sits to one side of DC.
 *
 * **The offset is derived, not picked.** Two bounds and an ideal, in that order:
 *
 *  - **Ideal: `spanHz / 4`.** The usable half-band runs from DC (0) to the window edge
 *    (`spanHz / 2`), and its two hazards sit at the ends — the LO spike at 0, the anti-alias
 *    filter's roll-off at Nyquist. The point maximally far from **both** is the midpoint of that
 *    half-band, which is `spanHz / 4`. (This is the classical quarter-rate offset, and it is the
 *    *only* offset with a reason rather than a preference behind it.)
 *  - **Bound: the selection must stay inside the window.** Sliding the selection by `off` costs
 *    `off` of the slack at one edge, so `off <= (spanHz - needHz) / 2`.
 *  - **Bound: the snap's own worst case.** `snapCenter` afterwards moves the centre by up to half a
 *    tuning step, and that half step has to come out of the same slack or the selection's far edge
 *    leaves the window. An unknown step subtracts nothing, because nothing is snapped then.
 *
 * **When the dodge is not free, the window does not widen to buy it.** A selection wider than half
 * the window has no offset that clears DC of it — DC lands *inside* the selection whichever way the
 * window is placed, and only *where* is left to choose. Widening the capture to make room would
 * throw away exactly the resolution this whole path exists to gain, so the offset takes the largest
 * value the slack allows and DC ends up off the selection's **centre** rather than out of it. At
 * `needHz == spanHz` the answer is 0: the window *is* the selection, and there is nothing to slide.
 */
export function dcOffsetHz(spanHz: number, needHz: number, stepHz: number | null): number {
  if (!Number.isFinite(spanHz) || !(spanHz > 0)) return 0;
  const w = Number.isFinite(needHz) && needHz > 0 ? Math.min(needHz, spanHz) : 0;
  const margin = stepHz !== null && Number.isFinite(stepHz) && stepHz > 0 ? stepHz / 2 : 0;
  const room = (spanHz - w) / 2 - margin;
  return room > 0 ? Math.min(spanHz / 4, room) : 0;
}

// ---------------------------------------------------------------------------
// Nudging the tune off DC (T-409)
// ---------------------------------------------------------------------------
//
// **The user's ask:** buttons that shift the tuned centre left/right by 1/8, 1/4 or 1/2 of the
// current tuned span. **The reason it exists** is the same one [`dcOffsetHz`] exists for, arrived at
// from the other side: a signal sitting on the centre frequency sits on the tuner's own DC/LO
// leakage spike, and cannot be cleanly demodulated or decoded there. T-317 ruled that spike out as
// an external cause and flags `spur_reason` on it, T-382 measured the host comb at −678.048 Hz
// relative to tuner DC, and the 2026-09-15 fixture's tuner-DC box is one of the few T-394 still
// leaves above 20 dB. It is real, it is ours, and it sits exactly where a user naturally tunes.
// A nudge moves the radio out from under it while keeping the signal in the window.
//
// Where T-418 *places* a window it is already opening, this *moves* one the user is already in. The
// arithmetic is deliberately the same arithmetic, so the two cannot disagree about where DC is safe.

/** The fractions of the tuned span the nudge control offers, narrowest first (the user's ask). */
export const NUDGE_FRACTIONS = [1 / 8, 1 / 4, 1 / 2] as const;

/**
 * The offered fraction that lands a previously-centred signal on [`dcOffsetHz`]'s derived ideal —
 * the midpoint of the usable half-band, `span/4` off DC.
 *
 * Not a preference among the three: it is the *only* one of them with a derivation behind it, and
 * it is the same derivation the region-select already uses. The other two are offered because the
 * user asked for them and because a nudge is also how you step along a band, not because they are
 * equally good at the job this control exists to do.
 */
export const NUDGE_IDEAL_FRACTION = 1 / 4;

/**
 * Where the window's edge region begins, as a fraction of the way from DC to the band edge — the
 * threshold [`NudgeGeometry.intoSkirt`] is decided by.
 *
 * It is **not** `1`. An anti-alias filter does not start at Nyquist and stop being a problem a hertz
 * inside it; it rolls off *before* the edge, and the front end's baseband filter is at most as wide
 * as the sample rate, so the outermost part of every window is skirt however the numbers round. The
 * value also has to be far larger than any snap: a half-span nudge lands at `1 − (step/span)` or
 * `1 + (step/span)` depending on which way `snapCenter` rounded — about one part in 10^5 on a
 * HackRF — and a hazard flag that flipped on that would be noise, not a warning. A tenth of the
 * half-band is conservative in the right direction and stable against both.
 */
export const NUDGE_SKIRT_LANDING = 0.9;

/**
 * What a nudge does to the picture, in the terms the two hazards are stated in — **derived from the
 * shift actually taken**, never from the fraction on the button (they differ by up to half a
 * tuning step once the centre is snapped).
 */
export interface NudgeGeometry {
  /** The signed shift of the tuned centre, Hz. */
  shiftHz: number;
  /**
   * Where a signal that is **on DC now** ends up, as a fraction of the distance from DC (0) to the
   * window edge (1).
   *
   * This is the number the whole control is about, and it is why the largest offered fraction is
   * not the best one. The usable half-band has a hazard at each end — the LO spike at 0, the
   * anti-alias filter's roll-off at Nyquist — so `0.5`, the midpoint, is maximally far from both
   * (`NUDGE_IDEAL_FRACTION`, and `dcOffsetHz`'s `span/4` said the other way round). `landing` is
   * `2 × |shift| / span`, so 1/8 span lands at 0.25, 1/4 span at 0.5, and **1/2 span at 1.0 — at
   * the band edge, in the skirt**: out of one hazard and straight into the other.
   */
  landing: number;
  /** Fraction of the window in force that is still inside the window after the nudge: `1 − |shift|
   * / span`. At 1/2 span exactly half the window is left behind — and a formerly-centred signal is
   * exactly on the boundary of what was kept. */
  overlap: number;
  /** `landing >= NUDGE_SKIRT_LANDING`: a centred signal lands in the window's edge region, where
   * the anti-alias filter rolls off. The trap the largest offered fraction sets for the very case
   * this control exists to fix — stated, not silently offered, and not withheld either. */
  intoSkirt: boolean;
}

/** [`NudgeGeometry`] for a shift of `shiftHz` inside a window of `spanHz`; null when either is not
 * a number this can be derived from. */
export function nudgeGeometry(spanHz: number, shiftHz: number): NudgeGeometry | null {
  if (!Number.isFinite(spanHz) || !(spanHz > 0) || !Number.isFinite(shiftHz)) return null;
  const d = Math.abs(shiftHz);
  const landing = d / (spanHz / 2);
  return { shiftHz, landing, overlap: Math.max(0, 1 - d / spanHz), intoSkirt: landing >= NUDGE_SKIRT_LANDING };
}

/** Why a nudge cannot be taken. Each one **disables** the button rather than shrinking the move —
 * see [`nudgePlan`] on why a clamp would be the worse answer. */
export type NudgeRefusal =
  /** No tunable range reported, so nothing may be claimed achievable (the same refusal
   * `retunePlan` makes, for the same reason). */
  | "no_grid"
  /** The tuned centre is not known yet, so there is nothing to nudge *from*. */
  | "no_center"
  /** The tuned span is not known yet, so the fraction names no distance. */
  | "no_span"
  /** The nudge would land the centre outside every band the front end can tune. */
  | "out_of_range";

/** A nudge that can be taken, or the reason it cannot. */
export type NudgePlan =
  | {
    ok: true;
    /** The centre to ask the radio for — already on the grid, so re-snapping it downstream is a
     * no-op and the number in the tooltip is the number that goes out. */
    centerHz: number;
    /** `fromHz + fraction × span`, before the snap. */
    requestedHz: number;
    /** The centre the nudge starts from. */
    fromHz: number;
    /** `centerHz − fromHz`: what the radio will **actually** move, which is the fraction's distance
     * plus the snap's own displacement. */
    shiftHz: number;
    /** `fraction × span`: what the button's label claims. Differs from `shiftHz` by `snapErrorHz`. */
    advertisedHz: number;
    /** `centerHz − requestedHz`, at most half a tuning step. Non-zero is the honest reason the
     * actual shift is not exactly the advertised one, and the readout says so. */
    snapErrorHz: number;
    /** Whether `centerHz` is a point the source *stated* is achievable. False when the grid reports
     * no step: the nudge is still taken (there is a radio and it can be tuned), but nothing may
     * claim the front end will sit exactly here — rule 1 at the top of this file. */
    onGrid: boolean;
    geometry: NudgeGeometry;
  }
  | { ok: false; reason: NudgeRefusal; requestedHz: number | null };

/**
 * The nudge of `fraction` of `spanHz` from `fromHz`, resolved against the achievable centre grid.
 *
 * **Two things this must not do, and they pull in opposite directions.**
 *
 * *It must snap.* Achievable centres are a grid (`center_step_hz`; a HackRF's is 30 MHz / 2^20 =
 * 28.6102294921875 Hz), and `centre + fraction × span` is not generally on it. So the centre asked
 * for is `snapCenter`'s, and the shift the radio actually takes then differs from the advertised
 * fraction by up to half a step — which `snapErrorHz` carries out to the readout rather than
 * quietly absorbing.
 *
 * *It must not clamp.* At the edge of the device's range a clamped nudge would move **less than the
 * button says**, and a control that quietly does less than it claims is the failure mode T-392
 * already met and chose against: `containsCenter` is not `snapCenter` **precisely because** snapping
 * walks an out-of-band request inward, which would call every out-of-range centre achievable at the
 * band edge. So the test here is `containsCenter` on the *requested* centre, and a nudge that cannot
 * be taken in full is refused — the caller disables the button and says why. (Snapping afterwards
 * can still move the centre by half a step; that is the grid, not a clamp, and it is reported.)
 *
 * **Reversibility.** `snapCenter` rounds to the nearest multiple of the step, so from a centre that
 * is on the grid, a nudge and its opposite return to exactly where they started: `round((k·s + d)/s)
 * = k + round(d/s)`, and subtracting the same `d` subtracts the same `round(d/s)`. From a centre
 * that is *off* the grid the round trip lands on the nearest grid point instead — within one snap
 * step, which is the most any grid can promise. That makes the buttons navigable rather than a
 * one-way drift, and it is asserted in `ui/test/navigation.test.ts`.
 */
export function nudgePlan(
  g: CenterGrid | null, fromHz: number | null, spanHz: number | null, fraction: number,
): NudgePlan {
  // A null grid refuses, exactly as `retunePlan` does: not knowing the tunable range is not evidence
  // that a centre is reachable, and this control commands the radio.
  if (!g) return { ok: false, reason: "no_grid", requestedHz: null };
  if (fromHz === null || !Number.isFinite(fromHz)) return { ok: false, reason: "no_center", requestedHz: null };
  if (spanHz === null || !Number.isFinite(spanHz) || !(spanHz > 0)) {
    return { ok: false, reason: "no_span", requestedHz: null };
  }
  if (!Number.isFinite(fraction) || fraction === 0) return { ok: false, reason: "no_span", requestedHz: null };
  const advertisedHz = fraction * spanHz;
  const requestedHz = fromHz + advertisedHz;
  if (!containsCenter(g, requestedHz)) return { ok: false, reason: "out_of_range", requestedHz };
  const snapped = snapCenter(g, requestedHz);
  const centerHz = snapped ?? requestedHz;
  const shiftHz = centerHz - fromHz;
  return {
    ok: true,
    centerHz,
    requestedHz,
    fromHz,
    shiftHz,
    advertisedHz,
    snapErrorHz: centerHz - requestedHz,
    onGrid: snapped !== null,
    geometry: nudgeGeometry(spanHz, shiftHz)!,
  };
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
  | {
    ok: true;
    centerHz: number;
    spanHz: number;
    snappedCenter: boolean;
    source: DetailSource;
    /**
     * How far the selection's centre sits from the window's DC, in Hz — signed, so its sign is
     * which side of DC the target landed on ([`dcOffsetHz`]). `0` means the dodge was not
     * available: the selection is too wide a fraction of the narrowest covering window for any
     * placement to clear DC of it, and widening the capture to buy the dodge would cost more
     * resolution than it saves.
     */
    dcOffsetHz: number;
    /** True when DC falls **outside** the selection — the dodge worked and the target is clear of
     * the tuner's own spike. False when the selection is wide enough that DC is inside it whatever
     * the placement, which the readout says out loud rather than implying a clean window. */
    clearsDc: boolean;
  }
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
  const width = hi - lo;
  const max = g.max_live_span_hz;
  if (max !== null && width > max) return { ok: false, reason: "span_too_wide" };
  // The narrowest window that could hold the selection at all, before any placement — this is the
  // "snaps to the narrowest achievable instantaneous window" half. For a sub-2-MHz region on a
  // HackRF it is the 2 Msps floor, because the device has no narrower state to ask for.
  const floor = smallestCoveringSpan(g, width);
  if (floor === null) return { ok: false, reason: "span_too_wide" };
  // T-418: place the selection off DC (`dcOffsetHz`). Only when there is a tuner to move — a
  // recording has a fixed centre and no local oscillator of ours leaking into it, so there is
  // nothing to dodge and nothing that could be re-placed if there were.
  const off = g.controllable ? dcOffsetHz(floor, width, g.center_step_hz) : 0;
  // Below DC by `off` puts the target above it; if that centre is off the end of the band, take the
  // mirror placement instead. Derived from the band, not a preference between the two sides.
  const lowSide = want - off;
  const asked = off > 0 && !containsCenter(g, lowSide) ? want + off : lowSide;
  const snapped = snapCenter(g, asked);
  const centerHz = snapped ?? asked;
  // Cover the region *from the centre the radio will actually sit on*, not from the one asked for:
  // a span picked before the snap can leave an edge of the selection outside the window, and on a
  // coarse tuning grid that is not a rounding detail but a miss. The offset is inside this reach
  // too, so the window that comes out holds the selection *where it was placed*.
  const needHz = 2 * Math.max(centerHz - lo, hi - centerHz);
  // The fallback is for the one case where only the snap pushed it over the top rung: the region
  // itself fits a live window, so the widest covering span is right and refusing would be a refusal
  // about a fraction of a tuning step.
  const spanHz = smallestCoveringSpan(g, needHz) ?? smallestCoveringSpan(g, width);
  if (spanHz === null) return { ok: false, reason: "span_too_wide" };
  // Measured from where the radio will sit, not from where the offset asked it to — the snap moved
  // it, and the number the readout prints has to be the one the hardware will produce.
  const actual = want - centerHz;
  return {
    ok: true,
    centerHz,
    spanHz,
    snappedCenter: snapped !== null,
    source: detailOf(g, spanHz),
    dcOffsetHz: actual,
    // DC is the window's centre. It is clear of the selection exactly when it falls outside it.
    clearsDc: centerHz < lo || centerHz > hi,
  };
}

// ---------------------------------------------------------------------------
// T-1028: fitting a view to what the front end can actually capture
// ---------------------------------------------------------------------------
//
// **Retune mode's one arithmetic addition, and the reason it is here rather than in the surface.**
// In retune mode the view's frequency window *is* the tune request (CLAUDE.md's amended navigation
// rule), and a view is under no obligation to be capturable: the surface spans 1 MHz–6 GHz and a
// user zoomed out past one instantaneous bandwidth has asked for a window no single capture holds.
// The user's ruling (2026-09-25) is that this is **not an error in the mode**: *"For areas that are
// too large and can't be tuned, use the largest possible size instead of denying them."*
//
// So the too-wide case resolves to the **largest achievable window centred on the view**, and that
// is a fact about the grid — which spans exist, which centres exist — so it is computed here, beside
// [`smallestCoveringSpan`], and handed to [`retunePlan`] as an ordinary region. There is still ONE
// planner: the mode does not get a second way to derive a capture configuration, it gets a smaller
// region to ask the same planner about. The three [`RetuneRefusal`]s are unchanged and still mean
// what they meant; what changes is only which region the mode asks about.

/**
 * The **widest** span that is still one live capture window, or null when the grid names none.
 *
 * The mirror of [`smallestCoveringSpan`], and bounded the same way `retunePlan` bounds a request:
 * `max_live_span_hz` is the front end's instantaneous bandwidth, and a span past it is survey
 * overview however wide the rate ladder goes. A grid that reports no bound is not evidence of a wide
 * window, but it is not grounds to refuse either — the ladder's own top then answers, exactly as the
 * refusal path already lets it.
 */
export function largestLiveSpan(g: FrequencyGrid | null): number | null {
  if (!g) return null;
  const cap = g.max_live_span_hz;
  const within = (v: number) => Number.isFinite(v) && v > 0 && (cap === null || !Number.isFinite(cap) || v <= cap);
  const s = g.spans_hz;
  if ("values" in s) {
    let best: number | null = null;
    for (const v of s.values) if (within(v) && (best === null || v > best)) best = v;
    return best;
  }
  if (!(s.max >= s.min)) return null;
  const top = cap === null || !Number.isFinite(cap) ? s.max : Math.min(s.max, cap);
  return top >= s.min && top > 0 ? top : null;
}

/**
 * The region a front end could actually capture, from the region a **view** names — the view itself
 * when one window covers it, else the largest achievable window **centred on the view's centre**,
 * with that centre clamped into the tunable range at the band edges.
 *
 * Null is the one case that stays an error (the user's ruling names it): a view with **no overlap at
 * all** with the tunable range, which no clamp can reach without moving the radio somewhere the user
 * is not looking. Everything else narrows rather than refusing — and narrowing is honest here in a
 * way it is not for a region-select, because the mode's user is looking at the whole view and the
 * coverage fog will show them which part of it the radio took.
 *
 * It does **not** plan: the returned region goes to [`retunePlan`], which snaps the centre, places
 * the window off DC where there is room, and may still refuse. Deliberately so — this narrows a
 * request, it does not decide what the front end will do with it.
 */
export function fitToLiveWindow(
  g: FrequencyGrid | null, lo: number, hi: number,
): { loHz: number; hiHz: number } | null {
  if (!g || !Number.isFinite(lo) || !Number.isFinite(hi) || !(hi > lo)) return null;
  const widest = largestLiveSpan(g);
  if (widest === null) return null;
  // The band the view actually overlaps, preferring the one it overlaps MOST — a view straddling a
  // gap between two bands belongs to the one it has more of, and a view inside a single band picks
  // that band by the same rule. No overlap anywhere is the refusal: see above.
  let band: [number, number] | null = null, bestOverlap = 0;
  for (const r of g.ranges_hz) {
    if (!Array.isArray(r)) continue;
    const overlap = Math.min(hi, r[1]) - Math.max(lo, r[0]);
    if (overlap > bestOverlap) { band = r; bestOverlap = overlap; }
  }
  if (!band) return null;
  // The view's own centre, clamped to the achievable-centre band. A clamp only ever happens at a band
  // edge, and it is a clamp rather than a refusal because the view still overlaps the band: the
  // window lands as near the view as the front end can sit.
  const centerHz = Math.min(Math.max((lo + hi) / 2, band[0]), band[1]);
  const spanHz = Math.min(hi - lo, widest);
  return { loHz: centerHz - spanHz / 2, hiHz: centerHz + spanHz / 2 };
}

// ---------------------------------------------------------------------------
// The resolution half (T-418)
// ---------------------------------------------------------------------------
//
// **The user's principle, verbatim:** *"Detail on a narrow view comes from resolution/decimation,
// never from an impossible narrow capture."*
//
// The device floor is the reason it has to. A HackRF's minimum sample rate is 2 Msps, so a
// sub-2-MHz window **cannot be captured** — there is no narrower capture to ask for, and
// `smallestCoveringSpan` bottoming out at the floor is the correct answer, not a missing feature.
// Everything finer than that floor has to come from the transform, not the front end.
//
// **What makes a bin finer, and what only looks like it.** Hz/bin is `spanHz / fftSize`, so there
// are exactly two honest levers: narrow the window (the retune half above), or lengthen the
// transform. A longer transform is a **genuine measurement** — an N-point DFT over N samples
// resolves N bins because it integrated N samples, and doubling N doubles the frequency resolution
// of the estimate. Stretching a short transform's output across more pixels is not: it invents no
// information and must never be what happens when a user zooms in. (`ui/src/waterfall.ts` is
// careful about this in the other direction — when the row is shorter than the texture it
// *nearest-repeats* rather than interpolating, and when it is longer it max-decimates. Repeating a
// measurement is honest; inventing one between two is not. But a repeated bin is still one bin's
// worth of detail, which is precisely why the fix is to ask for more bins rather than to draw the
// same ones wider.)
//
// So the resolution half is a **larger FFT**, requested through `POST /api/control/display`
// (`fft_size`), which the spectrum reader rebuilds at its next chunk boundary. It is deliberately
// *not* a device action: it re-plumbs nothing, moves no oscillator, and costs no settle gap —
// which is also why it is the lever to reach for first.

/** Just the FFT axis of `/api/control/state`'s `display_limits`: the bounds a larger transform may
 * be asked for within. Structurally a subset of `DisplayLimits` (ui/src/controls/model.ts), so a
 * caller holding the whole block can pass it straight in. */
export interface FftBounds { fft_size_min: number; fft_size_max: number }

/** The transform a slice needs, and what it actually buys. */
export interface DetailPlan {
  /** Bins per row to ask `POST /api/control/display` for: a power of two inside the bounds. */
  fftSize: number;
  /** Hz per bin that yields — `spanHz / fftSize`, **measured**, not drawn. */
  binHz: number;
  /** Bins that land across the selection at that size. */
  binsAcross: number;
  /**
   * True when `fft_size_max` bound the answer: the selection gets **fewer** bins than were asked
   * for, so the client will nearest-repeat to fill the pixels. The view must say so — this is the
   * exact point at which more zoom stops buying more detail, and the honesty rule (a view never
   * claims detail the front end did not capture) makes it something to state rather than hide.
   */
  capped: boolean;
}

/** The largest power of two `<= n`, and the smallest `>= n`, for clamping a bound that is not one
 * (the limits this repo serves are, but nothing on the wire promises it). */
const pow2Floor = (n: number) => 2 ** Math.floor(Math.log2(n));
const pow2Ceil = (n: number) => 2 ** Math.ceil(Math.log2(n));

/**
 * The **smallest** transform that puts `wantBins` measured bins across a `needHz` slice of a
 * `spanHz` window — the resolution half of a narrow selection.
 *
 * Smallest, not largest, for the same reason `smallestCoveringSpan` takes the least covering entry:
 * an FFT longer than the view can show costs work and latency for bins nothing draws, while one
 * shorter than the view leaves the client repeating bins across pixels. The target is therefore
 * **one measured bin per pixel of the selection**, the same rule T-397 applied to the navigator
 * strips (`stripCells`, one backend-folded cell per pixel) — the client's own render width is the
 * only non-arbitrary statement of how much detail a view can actually show.
 *
 * Null when no bounds were reported: nothing may then claim a size the server would accept, and the
 * caller leaves the transform where it is rather than guessing at a limit.
 */
export function detailPlan(
  b: FftBounds | null, spanHz: number, needHz: number, wantBins: number,
): DetailPlan | null {
  if (!b || !Number.isFinite(spanHz) || !(spanHz > 0)) return null;
  const min = b.fft_size_min, max = b.fft_size_max;
  if (!Number.isFinite(min) || !Number.isFinite(max) || !(min > 0) || !(max >= min)) return null;
  const lo = pow2Ceil(min), hi = pow2Floor(max);
  if (!(hi >= lo)) return null;
  // A slice wider than the window is the window: nothing outside it was captured.
  const slice = Number.isFinite(needHz) && needHz > 0 ? Math.min(needHz, spanHz) : spanHz;
  const want = Number.isFinite(wantBins) && wantBins > 0 ? wantBins : 1;
  // `wantBins` across the slice means `wantBins * spanHz / slice` across the whole window, because
  // the transform spans the window and the slice is only part of it. This is where the gain comes
  // from: the narrower the selection, the longer the transform needed to hold its detail.
  const need = want * spanHz / slice;
  let n = lo;
  while (n < need && n < hi) n *= 2;
  const binHz = spanHz / n;
  return { fftSize: n, binHz, binsAcross: slice / binHz, capped: n < need };
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
