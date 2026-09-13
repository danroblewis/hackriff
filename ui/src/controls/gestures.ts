// Frequency-axis gestures (T-051): drag to pan, wheel / trackpad pinch / two-finger pinch to zoom.
// The maths is axis.ts (panView, zoomAt, wheelFactor); this only tracks pointers.
import * as ax from "../axis";

export interface ViewHooks {
  geometry(): ax.Geometry | null;
  view(): ax.View | null;
  setView(v: ax.View): void;
  /** A pan let go past the band edge: `centerHz` would show `requested`. */
  overflow(centerHz: number, requested: ax.View): void;
}

/** Pan past the edge by more than this fraction of the view before a retune is offered. */
const OVERFLOW_FRAC = 0.05;

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
    if (last && e.type === "pointerup" && Math.abs(overflow) > OVERFLOW_FRAC * w) {
      hooks.overflow(ax.panRetuneCenter(last, overflow), { loHz: last.loHz + overflow, hiHz: last.hiHz + overflow });
    }
    start = null; last = null; overflow = 0;
  };
  el.addEventListener("pointerup", end);
  el.addEventListener("pointercancel", end);
  attachWheelZoom(el, hooks);
}
