// Centre view logic (T-152; ADR-0013 §4.1, §4.3): the live geometry from the `live` slice, the view
// after a new stream header, the Go to decision (pan inside the tuned band, retune outside it when
// live), and the retune action (`POST /api/control/window` for a whole capture configuration,
// `POST /api/control/center` when only a centre is named). Axis arithmetic only (axis.ts).
//
// T-343 — **device actions are a type here, not a threshold in a gesture handler.**
//
// Two kinds of thing happen in the centre view, and they are not interchangeable:
//
// - a **view change** (pan, zoom, scrub, pause) alters what is drawn and nothing else;
// - a **device action** moves the front end. A retune re-derives the window's content class and,
//   when the class or sample rate changes, stops and re-plumbs the running capture. It inherits
//   the HackRF one-agent-at-a-time rule — only one process can hold the device — and it is
//   recorded in the audit log against the device's provenance `device_id`.
//
// So every call that reaches a device route goes through one function, `applyDeviceAction`, and it
// takes a `DeviceAction` value. `DeviceAction` values are only ever built by an explicit user
// request: submitting Go to, clicking a bookmark, or pressing the retune button a pan-to-the-edge
// offers. A gesture cannot build one — `viewHooks().edgeOffer` records an offer in the store and
// returns; it never calls the client.
//
// That is why exactly two files under ui/src name a device route: this one, and the SDR control
// panel (`app/review/device.ts`), whose whole purpose is device settings. `ui/test/app-centre.ts`
// asserts that set against the source, so a third one has to be argued for rather than appear.
//
// T-529 — **one user retune is one request.** A retune to a region names a centre AND a span, and
// this module used to commit it as two posts, deciding for itself whether the rate one was needed.
// Two posts are two device actions over two windows, the first of which nobody asked for; see
// `applyDeviceAction` and docs/api.md, "A window is one device action".
import * as ax from "../../axis";
import { ControlError } from "../../controls/client";
import type { ViewHooks } from "../../controls/gestures";
import { snapCenter, type DetailPlan } from "../../navigation";
import { currentSpan } from "./capture-window";
import type { AppContext } from "../context";
import { toast } from "../shell-slice";
import type { AppState } from "../state";
import type { LiveSlice, RetuneOffer } from "./slice";

/** The spectrum geometry the `live` slice holds; null before the first header. */
export function geometryOfLive(l: Pick<LiveSlice, "centerHz" | "bandwidthHz" | "bins">): ax.Geometry | null {
  if (l.centerHz === null || l.bandwidthHz === null || l.bins === null) return null;
  return { centerHz: l.centerHz, bandwidthHz: l.bandwidthHz, bins: l.bins };
}

/**
 * The frequency window the centre pane's **overlays and axis** are placed in (T-386).
 *
 * Not `live.view` alone, and that is the fix. `live.view` exists only once a spectrum stream
 * header has arrived, so the whole overlay layer — every bracket, every Confirmed band, every
 * selection box, the frequency ticks — was gated on a live socket: a replay whose stream had not
 * connected, a paused session, or a run whose stream ended drew **no boxes at all** while
 * `/api/inventory` was answering with rows for exactly the band the device reports. That is "we
 * have it but didn't render it" with a single point of failure in front of it.
 *
 * The tuned band is the same fallback Record IQ and the History surface already take
 * ([[currentSpan]]), so this adds no new source of truth: it reads `/api/control/state`'s own
 * centre and sample rate. `null` stays *unknown* — nothing is invented when neither has answered.
 *
 * What it deliberately does **not** cover: the waterfall's rows and the time-extent boxes drawn in
 * its render pass need the stream's bin geometry and the waterfall's own clock, and gestures need
 * both. A view is enough to *place* a frequency; it is not enough to draw or to hit-test time.
 */
export function centreView(s: CentreViewState): ax.View | null {
  return currentSpan({ live: s.live.view, device: s.device });
}

/** What [[centreView]] reads. `AppState` satisfies it structurally. */
export interface CentreViewState {
  live: Pick<LiveSlice, "view">;
  device: { centerHz: number | null; sampleRateHz: number | null };
}

/**
 * A key that changes exactly when [[centreView]] would — the subscription every surface placing
 * something in that view takes, so none of them re-derives which inputs matter.
 *
 * It is not `live.view` alone for the same reason [[centreView]] is not: a device answer moves the
 * window without touching the stream slice, and a surface watching only the stream then goes on
 * drawing in the window it was mounted in.
 */
export function centreViewKey(s: CentreViewState): string {
  const v = centreView(s);
  return v ? `${v.loHz}/${v.hiHz}` : "";
}

/**
 * The view after a header with geometry `g`: the requested view (`want`, from a retune) or the
 * previous one, clamped into the band; one that lies wholly outside the band (a retune) keeps its
 * width, centred on the new centre; none → the full band.
 */
export function nextView(g: ax.Geometry, prev: ax.View | null, want: ax.View | null = null): ax.View {
  const full = ax.fullView(g), cand = want ?? prev;
  if (!cand || !(cand.hiHz > cand.loHz)) return full;
  if (cand.hiHz <= full.loHz || cand.loHz >= full.hiHz) {
    const w = Math.min(cand.hiHz - cand.loHz, full.hiHz - full.loHz);
    return ax.zoomTo(g, g.centerHz - w / 2, g.centerHz + w / 2);
  }
  return ax.zoomTo(g, cand.loHz, cand.hiHz);
}

export type GotoDecision = { kind: "pan"; view: ax.View } | { kind: "retune"; centerHz: number } | { kind: "not_live" } | null;

/** Go to `hz`: pan (keeping the zoom width) when it is inside the tuned band, else retune when the
 * device may be live, else `not_live`. Null for an invalid frequency. */
export function gotoDecision(g: ax.Geometry | null, v: ax.View | null, hz: number, live: boolean): GotoDecision {
  if (!Number.isFinite(hz) || hz < 0) return null;
  if (g) {
    const full = ax.fullView(g);
    if (hz >= full.loHz && hz <= full.hiHz) {
      const cur = v ?? full, w = cur.hiHz - cur.loHz;
      return { kind: "pan", view: ax.zoomTo(g, hz - w / 2, hz + w / 2) };
    }
  }
  return live ? { kind: "retune", centerHz: hz } : { kind: "not_live" };
}

/**
 * A request that **reaches the front end** (T-343), as opposed to one that changes the view.
 *
 * `source` names the explicit user request it came from, so the toast (and any future log) can say
 * what moved the radio. There is deliberately no variant for a gesture: a pan produces a
 * `RetuneOffer` for the user to accept, never a `DeviceAction`. (`"nudge"` is T-409's button press —
 * a discrete, explicit action like the offer button, not the continuation of anything.
 * `"pane-offer"` is T-444's, on the unified surface: panning a pane to un-tuned spectrum *offers*,
 * and taking the offer is the discrete act — same shape, one surface over. `"pane-width"` is
 * T-496's: an explicit capture-WIDTH preset, pressed directly rather than discovered by zooming
 * then retuning — the pane's own centre is kept and only `spanHz` is asked for.)
 */
export type DeviceAction = {
  kind: "retune";
  centerHz: number;
  /**
   * The span the window must open to (T-392), or null to keep the rate it is on.
   *
   * A frequency-navigator region-select names a whole capture configuration, not just a centre: the
   * smallest achievable sample rate that covers the selection (`retunePlan`). A centre alone would
   * put the selection inside a window of whatever width the radio happened to be at, which for a
   * selection wider than the current rate does not cover it at all.
   *
   * **Non-null means the request is `POST /api/control/window`** — one device action carrying both
   * halves (T-529). It is not compared against the rate in force first: "is this rate a change?"
   * is a question about the device, and answering it here made the client decide how many device
   * actions one press is, from a copy of the device state a poll had handed it. Null means "keep
   * the rate", which `POST /api/control/center` already says.
   */
  spanHz?: number | null;
  /** The view to restore once the new header arrives, when the request implies one. */
  want: ax.View | null;
  source: "goto" | "bookmark" | "edge-offer" | "navigator" | "nudge" | "pane-offer" | "pane-width";
};

/** A retune of the live device to `centerHz`, from the explicit user request `source`. */
export const retuneAction = (
  centerHz: number,
  source: DeviceAction["source"],
  want: ax.View | null = null,
  spanHz: number | null = null,
): DeviceAction => ({ kind: "retune", centerHz, spanHz, want, source });

/** A retune may be tried when the device is live or its state hasn't loaded yet (the server then
 * answers `409 not_live` on a replay). */
export const mayRetune = (d: { loaded: boolean; live: boolean }) => !d.loaded || d.live;

export const NOT_LIVE_TEXT = "Outside the tuned band, and this run is not live (replay): cannot retune.";

export function retuneErrorText(e: unknown): string {
  if (e instanceof ControlError) {
    if (e.code === "not_live") return NOT_LIVE_TEXT;
    // T-343: the front end is one shared resource. The server names who holds it; say so rather
    // than retrying, so a second claimant never races the holder to the device.
    if (e.code === "device_busy") return `The radio is busy: ${e.message}`;
    return `Retune refused: ${e.message}`;
  }
  return `Retune failed: ${e instanceof Error ? e.message : String(e)}`;
}

export const setLiveView = (v: ax.View) => (s: AppState): Partial<AppState> => ({ live: { ...s.live, view: v } });

/** Records (or clears) the pending retune a pan to the band edge offers. A store write only: no
 * request is made until the user accepts it. */
export const setRetuneOffer = (offer: RetuneOffer | null) => (s: AppState): Partial<AppState> =>
  ({ live: { ...s.live, retuneOffer: offer } });

/** The frequency a device action would move the front end to, as the offer button labels it. */
export const retuneLabel = (centerHz: number) => `Retune to ${(centerHz / 1e6).toFixed(4)} MHz`;

/**
 * **The only path in the UI to a device route** (T-343).
 *
 * Applies `action` to the live front end: a real retune, which can stop and re-plumb the running
 * capture, is recorded in the server's audit log against the device's `device_id`, and is refused
 * with `device_busy` when something else holds the device. Callers must pass a `DeviceAction` built
 * from an explicit user request; nothing here makes one from a gesture.
 *
 * **Returns whether the front end took it** (T-444). Every existing caller ignores the answer and
 * is unaffected, but a caller that must do something *only after a real retune* — invalidating the
 * growing edge's tiles, which describe the tuning that has just ended — cannot read that off a
 * toast. `false` is a refusal that has already been reported to the user here: not live, the device
 * busy, or the route rejecting the configuration.
 */
export async function applyDeviceAction(ctx: AppContext, action: DeviceAction): Promise<boolean> {
  const { store } = ctx;
  const dev = store.get().device;
  if (!mayRetune(dev)) { store.set(toast(NOT_LIVE_TEXT)); return false; }
  // T-341: a retune goes to an **achievable** centre. `Math.round` picks a whole hertz, which the
  // route requires, but a whole hertz is not a state the front end has: a HackRF's synthesiser
  // moves in ~28.6 Hz steps, so 28 of every 29 rounded values land it somewhere other than the
  // number the UI just showed. Snapping to the grid the backend reported asks for a centre the
  // radio can actually take. When the source cannot state a step, `snapCenter` returns null and
  // rounding is all that is left — and nothing then claims the device sits exactly there.
  const centerHz = snapCenter(dev.centerGrid, action.centerHz) ?? Math.round(action.centerHz);
  store.set((s) => ({ live: { ...s.live, pendingView: action.want, retuneOffer: null } }));
  try {
    // T-392: a configuration, not just a centre. T-529: and therefore **one** request.
    //
    // This used to be two posts — `/api/control/rate` when the width in force looked wrong, then
    // `/api/control/center` — which made the client decide, from its own copy of the device state,
    // *how many device actions one press is*. That is a plan, and planning is not this layer's job;
    // worse, it is a plan the backend then completed from whatever was in force, so one press
    // commanded two windows with the old centre at the new rate in between (docs/api.md, "A window
    // is one device action"). The client now states the window it wants and nothing else: whether
    // the rate really changes, whether that needs a re-plumb, and in what order the driver is
    // touched are all the backend's to decide, and it decides them once.
    //
    // An action carrying no span still means "this centre, keep the rate", which is exactly what
    // `/api/control/center` says — a nudge or a bookmark must not send a rate it merely read from a
    // poll, because sending it would turn a stale read into a command.
    const spanHz = action.spanHz ?? null;
    const wholeWindow = spanHz !== null && Number.isFinite(spanHz) && spanHz > 0;
    if (wholeWindow) {
      await ctx.client.post("/api/control/window", { center_hz: centerHz, sample_rate_hz: Math.round(spanHz) });
    } else {
      await ctx.client.post("/api/control/center", { center_hz: centerHz });
    }
    const on = store.get().device.deviceId;
    // T-498: name the span too, when this action carries one (a nudge does not — it never touches
    // the span, so there is nothing here to claim). `spanHz` is already what was actually asked for
    // above — the SNAPPED, achievable width the plan computed — never a re-derivation, so this
    // cannot say a different number from the one the request just posted.
    const wide = spanHz !== null && Number.isFinite(spanHz) && spanHz > 0 ? `, ${(spanHz / 1e6).toFixed(3)} MHz wide` : "";
    store.set(toast(`Retuning ${on ? `${on} ` : ""}to ${(centerHz / 1e6).toFixed(4)} MHz${wide}`));
    return true;
  } catch (e) {
    store.set((s) => ({ live: { ...s.live, pendingView: null } }));
    store.set(toast(retuneErrorText(e)));
    return false;
  }
}

/**
 * Asks the spectrum reader for a **longer transform** (T-418) — the resolution half of a narrow
 * selection, and deliberately **not** a device action.
 *
 * `POST /api/control/display` is a view control: the reader rebuilds its STFT at the next chunk
 * boundary, and nothing about the front end moves. No oscillator, no re-plumb, no settle gap, no
 * `device` key on the answer and none in the audit log — which is exactly why it is not in
 * `app-centre.test.ts`'s `DEVICE_ROUTES` and why T-343's "only an explicit device action reaches
 * the front end" is untouched by it. It lives here beside [`applyDeviceAction`] because this module
 * owns the control-plane calls the centre view makes, not because it is one of them.
 *
 * A null plan is "leave the transform alone" — no bounds reported, or the size in force is already
 * right — and makes no request. A failure is reported and swallowed: the zoom itself succeeded, and
 * a coarser row than asked for is a worse picture, not a broken one.
 */
export async function applyDisplayDetail(ctx: AppContext, plan: DetailPlan | null): Promise<void> {
  if (!plan || !Number.isFinite(plan.fftSize) || plan.fftSize <= 0) return;
  try {
    await ctx.client.post("/api/control/display", { fft_size: plan.fftSize });
  } catch (e) {
    ctx.store.set(toast(`Zoomed, but the resolution could not be raised: ${e instanceof Error ? e.message : String(e)}`));
  }
}

/** Zoom/pan hooks for controls/gestures.ts over the `live` slice. */
export function viewHooks(ctx: AppContext): ViewHooks {
  return {
    geometry: () => geometryOfLive(ctx.store.get().live),
    view: () => ctx.store.get().live.view,
    setView: (v) => ctx.store.set(setLiveView(v)),
    // T-343: a pan that ran past the band edge offers a retune; it does not perform one. No client
    // call happens here, on any code path, at any overflow.
    edgeOffer: (centerHz, requested) => {
      if (!mayRetune(ctx.store.get().device)) return;
      ctx.store.set(setRetuneOffer({ centerHz, view: requested }));
    },
  };
}
