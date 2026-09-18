// Retune-on-pan for the unified surface (T-444, docs/16 §8.4): panning a pane to spectrum the
// radio is **not looking at** offers a retune — and an offer is all a pan may ever produce.
//
// ## The control this file exists under, and why it is not negotiable
//
// T-340's control drags ±1.0 of the whole 6 GHz bar through a spy client and asserts an **empty
// call list**; T-442 re-asserted the same shape over the entire pane vocabulary (pan, zoom, pause,
// resume, split, close, `views`). A pane's pan is a pan. So the retune here is **a discrete,
// explicit act on a separate control** — the precedent is T-343's `edgeOffer` (a pan past the band
// edge records an offer in the store and returns; it never calls the client) and T-392's
// region-select-on-release, which this repo has already argued through.
//
// Nothing below performs a device call. [[paneRetuneOffer]] is pure arithmetic over pane state and
// the windows the backend reported; [[acceptPaneRetune]] is the only function here that reaches the
// front end, and it reaches it **through T-343's one gate**, `applyDeviceAction`, with a typed
// `DeviceAction` the backend records `device_id` against. There is no second path to the device,
// and this file names no device route (`ui/test/app-centre.test.ts` asserts that set against the
// source, so a third route-naming file has to be argued for rather than appear).
//
// ## T-407's lesson, one surface over
//
// T-407 found **two** ways a finger could retune the radio, both latent until T-392 removed a
// confirmation step: the drag threshold was the mouse's 6 px, so a fat-fingered tap was a drag; and
// travel was measured as `clientX + clientY`, so a stroke *across* a bar counted as travel *along*
// it. The shape of both is the same — **a continuous pointer stream was misread as a committing
// act** — and neither was introduced by the ticket that exposed them.
//
// The structural answer here is not a better threshold. It is that the commit is not a gesture at
// all, plus one guard that makes the moving-target version of the same failure impossible:
// **[[acceptPaneRetune]] re-derives the offer from the pane's state at the instant of the commit
// and refuses if it has moved** (`"moved"`). A pan therefore *invalidates* a pending offer rather
// than silently re-aiming it, so the radio goes where the button said or it goes nowhere. A finger
// that drags the pane while the offer sits under it cannot command a frequency the button was never
// labelled with.
//
// ## What is reused rather than re-derived
//
// The whole capture configuration comes from [[retunePlan]] (ui/src/navigation.ts): T-341's
// `snapCenter` for the achievable-centre grid (`HACKRF_ONE_TUNING_STEP_HZ` = 30 MHz / 2²⁰ ≈
// 28.61 Hz), `smallestCoveringSpan` for the narrowest window that still covers the pane, and
// T-418's derived off-DC placement (`span/4` — the midpoint of the usable half-band, maximally far
// from the LO spike at DC and the anti-alias roll-off at Nyquist), bounded by the selection staying
// inside the window and by the snap's own half-step. **The window is never widened to buy the
// dodge.** None of that arithmetic is restated here; this module decides *whether* to offer, and
// hands the region to the planner.

import { retunePlan, type FrequencyGrid, type RetunePlan } from "../navigation";
import type { ActiveWindow } from "../navigators";
import { applyDeviceAction, retuneAction, type DeviceAction } from "../app/centre/view";
import type { AppContext } from "../app/context";
import { boxOf, timeExtentOf, type PaneState } from "./panes";

/**
 * The window a pane is **already covered by**, or null.
 *
 * Full containment, not overlap: a pane straddling the edge of the tuned window is showing
 * spectrum the radio is not looking at, which is exactly the condition this feature is for.
 *
 * `device` is the pane's coverage selector (`any` = the union). A pane pinned to one front end is
 * not covered by another front end's window, because the grey it draws is that device's grey.
 */
export function coveringWindow(
  windows: readonly ActiveWindow[], loHz: number, hiHz: number, device = "any",
): ActiveWindow | null {
  for (const w of windows) {
    if (device !== "any" && w.deviceId !== device) continue;
    if (loHz >= w.loHz && hiHz <= w.hiHz) return w;
  }
  return null;
}

/**
 * A retune a pane's position **suggests**. Inert data: holding one commands nothing.
 *
 * `plan.ok === false` is an offer that is **stated and disabled**, never a clamped partial move —
 * T-409's rule, which this repo chose deliberately: at the edge of the device's range a clamped
 * move goes less far than it says, and a control that quietly does less than it claims is a control
 * that lies. `containsCenter` (not `snapCenter`) is what decides that, because snapping walks an
 * out-of-band request *inward* and would call every out-of-range centre achievable at the band edge.
 */
export interface PaneRetuneOffer {
  readonly paneId: string;
  /** The pane's frequency window, which is the region a retune would have to cover. */
  readonly loHz: number;
  readonly hiHz: number;
  /** The pane's coverage selector at the time the offer was made. */
  readonly device: string;
  /** The capture configuration, or the reason none can capture the pane (T-392's three refusals). */
  readonly plan: RetunePlan;
  /**
   * How much of the pane's width the planned window would still leave outside it, Hz.
   *
   * Effectively always 0 or a fraction of a tuning step — `retunePlan` covers the region *from the
   * centre the radio will actually sit on* — but it is derived and reported rather than assumed,
   * because "the window covers the pane" is the claim the offer is making.
   */
  readonly shortfallHz: number;
}

/** Whether an offer may be acted on. A refusal is shown, not clamped into something takeable. */
export const offerAcceptable = (o: PaneRetuneOffer | null): boolean => !!o && o.plan.ok;

/**
 * The offer a pane's current position suggests, or **null when there is nothing to offer**.
 *
 * Three ways to get null, and each is a statement rather than an omission:
 *
 *  - **A live window already covers the pane.** The radio is looking there; nothing to do.
 *  - **The pane is not showing the growing edge.** A retune changes only what is captured *from
 *    now on*, so a pane frozen ten minutes back over a band that was tuned then has nothing to gain
 *    from one — and offering would imply the past could be re-observed, which is the same class of
 *    claim as manufacturing grey. (`edgeGraceNs` allows for a pane frozen a frame or two behind.)
 *  - **No grid and no windows were reported.** Not knowing what the front end can do is not
 *    evidence that it can do this, so nothing is offered (the same refusal `retunePlan` and
 *    `nudgePlan` make, for the same reason).
 */
export function paneRetuneOffer(
  pane: PaneState,
  windows: readonly ActiveWindow[],
  grid: FrequencyGrid | null,
  edgeNs: number,
  edgeGraceNs = 0,
): PaneRetuneOffer | null {
  const box = boxOf(pane, edgeNs);
  const t = timeExtentOf(pane.time, edgeNs);
  if (!(t.t1Ns + edgeGraceNs >= edgeNs)) return null;
  if (coveringWindow(windows, box.f0Hz, box.f1Hz, pane.device)) return null;
  if (!grid) return null;
  const plan = retunePlan(grid, box.f0Hz, box.f1Hz);
  return {
    paneId: pane.id,
    loHz: box.f0Hz,
    hiHz: box.f1Hz,
    device: pane.device,
    plan,
    shortfallHz: plan.ok ? shortfall(plan.centerHz, plan.spanHz, box.f0Hz, box.f1Hz) : 0,
  };
}

/** Pane width left outside a window of `(centerHz, spanHz)`, Hz. */
function shortfall(centerHz: number, spanHz: number, loHz: number, hiHz: number): number {
  const lo = centerHz - spanHz / 2, hi = centerHz + spanHz / 2;
  return Math.max(0, lo - loHz) + Math.max(0, hiHz - hi);
}

/** The sentence beside the button, for an offer that can be taken and for one that cannot. */
export function offerLabel(o: PaneRetuneOffer): string {
  if (!o.plan.ok) {
    switch (o.plan.reason) {
      case "center_out_of_range":
        return "Outside the front end's tunable range: no retune can reach here.";
      case "span_too_wide":
        return "Wider than one capture window: this is survey overview, and no single retune covers it.";
      default:
        return "The front end has not reported a tunable range, so nothing may be claimed achievable.";
    }
  }
  const mhz = (o.plan.centerHz / 1e6).toFixed(4);
  const msps = (o.plan.spanHz / 1e6).toFixed(3);
  const dc = o.plan.clearsDc
    ? `clear of DC by ${Math.abs(o.plan.dcOffsetHz / 1e3).toFixed(1)} kHz`
    : "DC falls inside this span — the window cannot be placed to clear it without widening";
  return `Retune to ${mhz} MHz at ${msps} MHz span (${dc})`;
}

/**
 * The typed device action an accepted offer becomes, or **null for one that cannot be taken**.
 *
 * `source: "pane-offer"` names the explicit user request in the audit trail, exactly as `"nudge"`
 * and `"edge-offer"` do. There is deliberately no variant meaning "a gesture did this".
 */
export function paneRetuneAction(o: PaneRetuneOffer | null): DeviceAction | null {
  if (!o || !o.plan.ok) return null;
  return retuneAction(o.plan.centerHz, "pane-offer", { loHz: o.loHz, hiHz: o.hiHz }, o.plan.spanHz);
}

/**
 * What the accept needs from the surface around it, so the commit can check itself.
 *
 * `offerNow` is the guard, not a convenience: it must recompute from **live** pane state, because
 * comparing an offer to itself proves nothing. `invalidateEdge` is T-437 §5.2's one client-side
 * addition (see [[acceptPaneRetune]]).
 */
export interface PaneRetuneSite {
  offerNow(paneId: string): PaneRetuneOffer | null;
  invalidateEdge(): number;
}

export type PaneRetuneOutcome =
  | { ok: true; action: DeviceAction; invalidated: number }
  | {
    ok: false;
    /**
     * `not_acceptable`: the offer states a refusal, and a refusal is not a smaller retune.
     * `moved`: the pane is no longer where the offer was made, so the button's label and the
     * radio's destination had come apart — T-407's failure mode, refused structurally.
     * `refused`: the front end (or the gate) said no; `applyDeviceAction` has already said why.
     */
    reason: "not_acceptable" | "moved" | "refused";
  };

/**
 * **Take** an offer: the one explicit act in this file, and the only thing here that moves a radio.
 *
 * Never call this from a pointer handler that is also panning. It is for a click on the offer's own
 * control, a keyboard confirm, or a command — a discrete act, separate from the gesture that
 * produced the offer.
 *
 * Three things happen in order, and the order matters:
 *
 *  1. **The offer is re-derived and compared.** A pane that has moved since the offer was drawn
 *     refuses with `"moved"` rather than retuning to the new position: the user consented to the
 *     frequency on the button. This is the T-407 guard.
 *  2. **The action goes through T-343's one gate.** `applyDeviceAction` snaps the centre to the
 *     achievable grid again (a no-op here, since [[retunePlan]] already did), posts the covering
 *     sample rate first and only when the window in force is the wrong width, then the centre, and
 *     surfaces `device_busy` rather than racing whoever holds the front end. One capture at a time,
 *     settle gap honoured, `device_id` recorded **by the backend**, not claimed by this client.
 *  3. **The growing edge is invalidated** (T-437 §5.2, the spike's one client-side ask). The finest
 *     tiles at the live edge were computed *on request* from the tuning that has just ended; a
 *     cached one would now be served as though the radio had been looking somewhere it was not.
 *     Dropping them is not a performance choice — it is the grey-honesty rule, since a stale edge
 *     tile is an observation claim about a tuning that no longer exists.
 */
export async function acceptPaneRetune(
  ctx: AppContext, site: PaneRetuneSite, offer: PaneRetuneOffer,
): Promise<PaneRetuneOutcome> {
  const action = paneRetuneAction(offer);
  if (!action) return { ok: false, reason: "not_acceptable" };
  const now = site.offerNow(offer.paneId);
  if (!sameTarget(offer, now)) return { ok: false, reason: "moved" };
  const ok = await applyDeviceAction(ctx, action);
  if (!ok) return { ok: false, reason: "refused" };
  return { ok: true, action, invalidated: site.invalidateEdge() };
}

/**
 * Do two offers name the same capture configuration for the same pane?
 *
 * Compared on the **planned** centre and span rather than the pane's raw box, because that is what
 * the button said and what the radio would be asked for. A pan too small to change the snapped
 * configuration is not a change of target, and refusing it would make the control unusable while
 * protecting nothing.
 */
function sameTarget(a: PaneRetuneOffer, b: PaneRetuneOffer | null): boolean {
  if (!b || b.paneId !== a.paneId || b.device !== a.device) return false;
  if (!a.plan.ok || !b.plan.ok) return false;
  return a.plan.centerHz === b.plan.centerHz && a.plan.spanHz === b.plan.spanHz;
}
