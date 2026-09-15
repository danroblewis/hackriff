// Frequency axis strip (T-152; ADR-0013 §4.3): "nice" ticks of the current view (axis.ts `ticks`),
// labelled to the tick step. Drag pans and pinch/wheel zooms via controls/gestures.ts.
import * as ax from "../../axis";
import { attachAxisGestures } from "../../controls/gestures";
import type { AppContext } from "../context";
import { h } from "../dom";
import { viewHooks } from "./view";

export interface Tick { leftPct: number; label: string }

/** Minimum label spacing, px: 4 ticks at 400 px, 13 at 1200 px. */
export const TICK_SPACING_PX = 90;

export function tickModel(v: ax.View | null, widthPx: number, spacingPx = TICK_SPACING_PX): Tick[] {
  if (!v || !(widthPx > 0)) return [];
  const t = ax.ticks(v, Math.max(2, Math.floor(widthPx / spacingPx)));
  const step = t.length > 1 ? t[1].hz - t[0].hz : v.hiHz - v.loHz;
  return t.map((k) => ({ leftPct: k.frac * 100, label: ax.fmtMHz(k.hz, step) }));
}

export function mountAxis(el: HTMLElement, ctx: AppContext) {
  el.replaceChildren();
  const render = () => {
    const ts = tickModel(ctx.store.get().live.view, el.clientWidth);
    el.replaceChildren(...ts.map((t) => {
      const e = h("div", { class: "c-tick" }, h("span", {}, t.label));
      e.style.left = `${t.leftPct}%`;
      return e;
    }));
  };
  let queued = 0;
  const schedule = () => { if (!queued) queued = requestAnimationFrame(() => { queued = 0; render(); }); };
  ctx.store.select((s) => s.live.view, schedule, { immediate: true });
  if (typeof ResizeObserver !== "undefined") new ResizeObserver(schedule).observe(el);
  attachAxisGestures(el, viewHooks(ctx));
}
