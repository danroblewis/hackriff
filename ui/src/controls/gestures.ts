// Frequency-axis gestures (T-051): drag to pan, wheel / trackpad pinch / two-finger pinch to zoom.
// The maths is axis.ts (panView, zoomAt, wheelFactor); this only tracks pointers.
//
// T-343 — **no gesture in this file reaches the device.** Panning, zooming and pinching change
// what is shown; they clamp at the tuned band's edges and stop there. Moving the front end is a
// retune: it can stop and re-plumb the running capture, so it is a device action that only an
// explicit user request may reach (ui/src/app/centre/view.ts `applyDeviceAction`), never the
// continuation of a drag.
//
// Before T-343, a pan whose accumulated overflow passed 5 % of the view width called straight
// through to the control API's centre route on pointerup — so letting go slightly too far could
// stop capture. What that overflow now produces is an *offer*: `edgeOffer` records that the pan
// reached the edge, the UI draws a button, and the device moves only if the user presses it. This
// module therefore imports no client and names no device route; that is the property, not a
// convention, and ui/test/app-centre.test.ts asserts it against this file's source.
import * as ax from "../axis";

export interface ViewHooks {
  geometry(): ax.Geometry | null;
  view(): ax.View | null;
  setView(v: ax.View): void;
  /**
   * A pan was let go past the band edge: centring on `centerHz` would show `requested`.
   *
   * **This is not a retune.** The implementor may only record an offer for the user to accept;
   * it must not call the control API. Reaching the device from here is the T-343 defect.
   */
  edgeOffer(centerHz: number, requested: ax.View): void;
}

/**
 * How far past the band edge a pan must be let go before the UI offers a retune, as a fraction of
 * the view width.
 *
 * This is an *offer* threshold, not a device threshold: crossing it draws a button and nothing
 * else. Before T-343 the same number decided whether a drag commanded the front end, which is why
 * it could not stay a number in a gesture handler.
 */
const EDGE_OFFER_FRAC = 0.05;

/** Wheel zoom (and ctrl+wheel trackpad pinch) over `el`, around the pointer. */
export function attachWheelZoom(el: HTMLElement, hooks: ViewHooks) {
  el.addEventListener("wheel", (e) => {
    const g = hooks.geometry(), v = hooks.view();
    if (!g || !v || e.deltaY === 0) return;
    e.preventDefault();
    const x = ax.pointerFrac(e.clientX, el.getBoundingClientRect());
    hooks.setView(ax.zoomAt(g, v, x, ax.wheelFactor(e.deltaY, e.deltaMode)));
  }, { passive: false });
}

/** Drag-to-pan and two-pointer pinch on the axis strip. */
export function attachAxisGestures(el: HTMLElement, hooks: ViewHooks) {
  const pts = new Map<number, number>(); // pointerId → clientX
  let start: { view: ax.View; x: number; dist: number; mid: number } | null = null;
  let overflow = 0, last: ax.View | null = null;

  const begin = () => {
    const v = hooks.view();
    if (!v) { start = null; return; }
    const xs = [...pts.values()], r = el.getBoundingClientRect();
    const mid = xs.reduce((a, b) => a + b, 0) / xs.length;
    start = { view: v, x: mid, dist: xs.length > 1 ? Math.abs(xs[0] - xs[1]) : 0, mid: ax.pointerFrac(mid, r) };
    overflow = 0;
  };

  el.addEventListener("pointerdown", (e) => {
    if (e.pointerType === "mouse" && e.button !== 0) return;
    // A control drawn inside the strip is not a grab handle: capturing the pointer and calling
    // preventDefault() here would swallow its click. This is how the retune offer's button stays
    // pressable (T-343) — and why a pan can never be mistaken for pressing it.
    if ((e.target as Element | null)?.closest?.("button, a, input, select")) return;
    pts.set(e.pointerId, e.clientX);
    el.setPointerCapture(e.pointerId);
    el.classList.add("dragging");
    begin();
    e.preventDefault();
  });
  el.addEventListener("pointermove", (e) => {
    if (!pts.has(e.pointerId)) return;
    pts.set(e.pointerId, e.clientX);
    const g = hooks.geometry(), r = el.getBoundingClientRect();
    if (!g || !start || !(r.width > 0)) return;
    const xs = [...pts.values()];
    if (xs.length > 1 && start.dist > 0) {
      const dist = Math.abs(xs[0] - xs[1]);
      last = ax.zoomAt(g, start.view, start.mid, Math.max(1e-3, dist) / start.dist);
      overflow = 0;
    } else {
      const w = start.view.hiHz - start.view.loHz;
      const p = ax.panView(g, start.view, (-(e.clientX - start.x) / r.width) * w);
      last = p.view;
      overflow = p.overflowHz;
    }
    hooks.setView(last);
  });
  const end = (e: PointerEvent) => {
    if (!pts.delete(e.pointerId)) return;
    if (pts.size) { begin(); return; } // pinch → pan with the remaining finger
    el.classList.remove("dragging");
    const w = last ? last.hiHz - last.loHz : 0;
    if (last && e.type === "pointerup" && Math.abs(overflow) > EDGE_OFFER_FRAC * w) {
      hooks.edgeOffer(ax.panRetuneCenter(last, overflow), { loHz: last.loHz + overflow, hiHz: last.hiHz + overflow });
    }
    start = null; last = null; overflow = 0;
  };
  el.addEventListener("pointerup", end);
  el.addEventListener("pointercancel", end);
  attachWheelZoom(el, hooks);
}
