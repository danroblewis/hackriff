// Frequency axis strip (T-152; ADR-0013 §4.3): "nice" ticks of the current view (axis.ts `ticks`),
// labelled to the tick step. Drag pans and pinch/wheel zooms via controls/gestures.ts.
//
// T-343: a pan let go past the band edge leaves a *retune offer* in the store, and this strip draws
// it as a button. Pressing it is the explicit user action that moves the front end — which can stop
// and re-plumb the capture — so it says so on its face, names the device it will move, and is the
// only thing here that reaches the control API. Panning itself never does.
import * as ax from "../../axis";
import { attachAxisGestures } from "../../controls/gestures";
import type { AppContext } from "../context";
import { h } from "../dom";
import { applyDeviceAction, centreView, centreViewKey, retuneAction, retuneLabel, setRetuneOffer, viewHooks } from "./view";

export interface Tick { leftPct: number; label: string }

/** Minimum label spacing, px: 4 ticks at 400 px, 13 at 1200 px. */
export const TICK_SPACING_PX = 90;

export function tickModel(v: ax.View | null, widthPx: number, spacingPx = TICK_SPACING_PX): Tick[] {
  if (!v || !(widthPx > 0)) return [];
  const t = ax.ticks(v, Math.max(2, Math.floor(widthPx / spacingPx)));
  const step = t.length > 1 ? t[1].hz - t[0].hz : v.hiHz - v.loHz;
  return t.map((k) => ({ leftPct: k.frac * 100, label: ax.fmtMHz(k.hz, step) }));
}

/**
 * What the retune offer draws (T-343), or null when nothing is offered. Pure, so the wording and
 * the side it sits on are testable without a DOM.
 *
 * `belowBand` is which edge the pan ran off, so the button appears where the drag ended. `device`
 * names the front end the press would move, from `/api/control/state` — a device action says which
 * device it is about to change before it happens, not after.
 */
export function retuneOfferModel(
  offer: { centerHz: number } | null,
  current: number | null,
  device: string | null,
): { label: string; title: string; belowBand: boolean } | null {
  if (!offer || !Number.isFinite(offer.centerHz)) return null;
  const on = device ? ` on ${device}` : "";
  return {
    label: retuneLabel(offer.centerHz),
    title: `Moves the radio${on}. A retune can briefly interrupt capture; panning and zooming do not.`,
    belowBand: current !== null && offer.centerHz < current,
  };
}

export function mountAxis(el: HTMLElement, ctx: AppContext) {
  el.replaceChildren();
  const render = () => {
    const s = ctx.store.get();
    // T-386: the same view the overlays are placed in, so brackets never draw over an unlabelled
    // axis (and vice versa) — the ticks fall back to the tuned band before a stream header, exactly
    // as the boxes above them do.
    const ts = tickModel(centreView(s), el.clientWidth);
    const nodes: HTMLElement[] = ts.map((t) => {
      const e = h("div", { class: "c-tick" }, h("span", {}, t.label));
      e.style.left = `${t.leftPct}%`;
      return e;
    });
    const m = retuneOfferModel(s.live.retuneOffer, s.live.centerHz, s.device.deviceId);
    if (m && s.live.retuneOffer) {
      const offer = s.live.retuneOffer;
      const btn = h("button", {
        class: "c-retune-offer", type: "button", title: m.title,
        "data-side": m.belowBand ? "lo" : "hi",
      }, m.label);
      // The one explicit user action on this strip that reaches the front end.
      btn.addEventListener("click", () => {
        void applyDeviceAction(ctx, retuneAction(offer.centerHz, "edge-offer", offer.view));
      });
      nodes.push(btn);
      const dismiss = h("button", { class: "c-retune-dismiss", type: "button", title: "Keep the current tuning" }, "✕");
      dismiss.addEventListener("click", () => ctx.store.set(setRetuneOffer(null)));
      nodes.push(dismiss);
    }
    el.replaceChildren(...nodes);
  };
  let queued = 0;
  const schedule = () => { if (!queued) queued = requestAnimationFrame(() => { queued = 0; render(); }); };
  ctx.store.select((s) => s.live.view, schedule, { immediate: true });
  ctx.store.select((s) => s.live.retuneOffer, schedule);
  ctx.store.select((s) => s.device.deviceId, schedule);
  ctx.store.select(centreViewKey, schedule); // T-386: a device answer moves the view too
  if (typeof ResizeObserver !== "undefined") new ResizeObserver(schedule).observe(el);
  attachAxisGestures(el, viewHooks(ctx));
}
