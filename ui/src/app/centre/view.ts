// Centre view logic (T-152; ADR-0013 §4.1, §4.3): the live geometry from the `live` slice, the view
// after a new stream header, the Go to decision (pan inside the tuned band, retune outside it when
// live), and the retune action (`POST /api/control/center`). Axis arithmetic only (axis.ts).
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
import * as ax from "../../axis";
import { ControlError } from "../../controls/client";
import type { ViewHooks } from "../../controls/gestures";
import { snapCenter } from "../../navigation";
import { currentSpan } from "../capture/timeline";
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
 * The tuned band is the same fallback the capture band and the History surface already take
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
 * `RetuneOffer` for the user to accept, never a `DeviceAction`.
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
   * selection wider than the current rate does not cover it at all. Non-null and different from the
   * rate in force means one extra device call, `POST /api/control/rate`, before the centre.
   */
  spanHz?: number | null;
  /** The view to restore once the new header arrives, when the request implies one. */
  want: ax.View | null;
  source: "goto" | "bookmark" | "edge-offer" | "navigator";
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
 */
export async function applyDeviceAction(ctx: AppContext, action: DeviceAction): Promise<void> {
  const { store } = ctx;
  const dev = store.get().device;
  if (!mayRetune(dev)) { store.set(toast(NOT_LIVE_TEXT)); return; }
  // T-341: a retune goes to an **achievable** centre. `Math.round` picks a whole hertz, which the
  // route requires, but a whole hertz is not a state the front end has: a HackRF's synthesiser
  // moves in ~28.6 Hz steps, so 28 of every 29 rounded values land it somewhere other than the
  // number the UI just showed. Snapping to the grid the backend reported asks for a centre the
  // radio can actually take. When the source cannot state a step, `snapCenter` returns null and
  // rounding is all that is left — and nothing then claims the device sits exactly there.
  const centerHz = snapCenter(dev.centerGrid, action.centerHz) ?? Math.round(action.centerHz);
  store.set((s) => ({ live: { ...s.live, pendingView: action.want, retuneOffer: null } }));
  try {
    // T-392: a configuration, not just a centre. The rate goes first so the last header the retune
    // produces is the one carrying the requested centre, and it is skipped entirely when the window
    // is already the right width — a rate change re-plumbs the capture, so it is not made idly.
    const spanHz = action.spanHz ?? null;
    if (spanHz !== null && Number.isFinite(spanHz) && spanHz > 0 && Math.round(spanHz) !== dev.sampleRateHz) {
      await ctx.client.post("/api/control/rate", { sample_rate_hz: Math.round(spanHz) });
    }
    await ctx.client.post("/api/control/center", { center_hz: centerHz });
    const on = store.get().device.deviceId;
    store.set(toast(`Retuning ${on ? `${on} ` : ""}to ${(centerHz / 1e6).toFixed(4)} MHz`));
  } catch (e) {
    store.set((s) => ({ live: { ...s.live, pendingView: null } }));
    store.set(toast(retuneErrorText(e)));
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
