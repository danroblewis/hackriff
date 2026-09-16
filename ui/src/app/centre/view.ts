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
  /** The view to restore once the new header arrives, when the request implies one. */
  want: ax.View | null;
  source: "goto" | "bookmark" | "edge-offer" | "navigator";
};

/** A retune of the live device to `centerHz`, from the explicit user request `source`. */
export const retuneAction = (
  centerHz: number,
  source: DeviceAction["source"],
  want: ax.View | null = null,
): DeviceAction => ({ kind: "retune", centerHz, want, source });

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
  if (!mayRetune(store.get().device)) { store.set(toast(NOT_LIVE_TEXT)); return; }
  store.set((s) => ({ live: { ...s.live, pendingView: action.want, retuneOffer: null } }));
  try {
    await ctx.client.post("/api/control/center", { center_hz: Math.round(action.centerHz) });
    const on = store.get().device.deviceId;
    store.set(toast(`Retuning ${on ? `${on} ` : ""}to ${(action.centerHz / 1e6).toFixed(4)} MHz`));
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
