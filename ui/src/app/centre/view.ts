// Centre view logic (T-152; ADR-0013 §4.1, §4.3): the live geometry from the `live` slice, the view
// after a new stream header, the Go to decision (pan inside the tuned band, retune outside it when
// live), and the retune action (`POST /api/control/center`). Axis arithmetic only (axis.ts).
import * as ax from "../../axis";
import { ControlError } from "../../controls/client";
import type { ViewHooks } from "../../controls/gestures";
import type { AppContext } from "../context";
import { toast } from "../shell-slice";
import type { AppState } from "../state";
import type { LiveSlice } from "./slice";

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

/** A retune may be tried when the device is live or its state hasn't loaded yet (the server then
 * answers `409 not_live` on a replay). */
export const mayRetune = (d: { loaded: boolean; live: boolean }) => !d.loaded || d.live;

export const NOT_LIVE_TEXT = "Outside the tuned band, and this run is not live (replay): cannot retune.";

export function retuneErrorText(e: unknown): string {
  if (e instanceof ControlError) return e.code === "not_live" ? NOT_LIVE_TEXT : `Retune refused: ${e.message}`;
  return `Retune failed: ${e instanceof Error ? e.message : String(e)}`;
}

export const setLiveView = (v: ax.View) => (s: AppState): Partial<AppState> => ({ live: { ...s.live, view: v } });

/** Retunes the live device to `centerHz`; the next header applies `want` (when given). */
export async function retune(ctx: AppContext, centerHz: number, want: ax.View | null): Promise<void> {
  const { store } = ctx;
  if (!mayRetune(store.get().device)) { store.set(toast(NOT_LIVE_TEXT)); return; }
  store.set((s) => ({ live: { ...s.live, pendingView: want } }));
  try {
    await ctx.client.post("/api/control/center", { center_hz: Math.round(centerHz) });
    store.set(toast(`Retuning to ${(centerHz / 1e6).toFixed(4)} MHz`));
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
    overflow: (c, requested) => { void retune(ctx, c, requested); },
  };
}
