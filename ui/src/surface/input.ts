// **One input implementation, for every mount of this surface** (T-445).
//
// Moved verbatim out of `preview-main.ts`, which was the only host when T-450 wrote it. The cutover
// adds a second host — the app's Explore centre — and a second copy of "what a wheel means" is
// precisely **T-412's wheel-zoom mismatch**: the waterfall and the frequency navigator each had
// their own wheel handler, they disagreed about direction and factor, and the disagreement was the
// bug. Two hosts with one handler cannot reproduce it; two hosts with two handlers eventually will.
//
// So this file is the single answer to: where is the pointer, which viewport is under it, and what
// does a drag or a wheel do to that viewport. Everything it calls is `SurfacePreview`'s, which is
// arithmetic over `PaneModel` — T-442 asserted the whole gesture vocabulary against a spy client and
// saw an **empty call list**, and that property is unchanged by having one more caller.
//
// Screen y runs down and the drawing buffer's runs up, so the vertical delta is negated exactly
// once, here, and every consumer below is in one convention.

import { type SurfacePreview, wheelAxes, zoomFactor } from "./preview";

/** A point in drawing-buffer coordinates, GL convention (origin bottom-left). */
export interface GlPoint { x: number; y: number }

export interface SurfaceInputOptions {
  /** Called after any gesture that moved a viewport, so a host can mirror the view into its own
   * state. It is handed nothing: the authority is `preview.view`, and a second copy of the window
   * passed through here would be a second place for it to be wrong. */
  onView?: () => void;
  /** Double-click on the map sends the active pane there. A discrete act rather than a threshold on
   * a pointer stream — T-407's lesson, kept even though nothing here can reach a radio. */
  mapDoubleClick?: boolean;
  /** Called for a click that was not a drag, with the point it landed on. Hosts use it to focus
   * whatever is under the cursor; the preview page passes none. */
  onClick?: (p: GlPoint, e: PointerEvent) => void;
  /** Called on every pointer move over the canvas, for a hover readout. */
  onHover?: (p: GlPoint | null, e: PointerEvent) => void;
  /** Called for a context-menu request (right-click) at a point. */
  onContext?: (p: GlPoint, e: MouseEvent) => void;
}

/** Travel, in device px, past which a pointer stream is a drag rather than a click. The same 6 px
 * the retired waterfall used, and measured as a **distance** — T-407 found `clientX + clientY`,
 * under which a stroke *across* an axis counted as travel *along* it. */
export const DRAG_PX = 6;

/**
 * Wire `canvas`'s pointer, wheel and double-click events to `preview`. Returns a disposer.
 *
 * The resize/frame loop is deliberately **not** here: it is three lines, it is per-host (the app
 * resizes on a layout change, the page on `window.resize`), and nothing about it can diverge in a
 * way a user sees.
 */
export function attachSurfaceInput(
  canvas: HTMLCanvasElement, preview: SurfacePreview, opts: SurfaceInputOptions = {},
): () => void {
  const point = (e: { clientX: number; clientY: number }): GlPoint => {
    const r = canvas.getBoundingClientRect();
    return {
      x: (e.clientX - r.left) * (canvas.width / Math.max(1, r.width)),
      y: canvas.height - (e.clientY - r.top) * (canvas.height / Math.max(1, r.height)),
    };
  };
  const moved = () => opts.onView?.();

  let dragging: { x: number; y: number; map: boolean; pane: string | null; travel: number } | null = null;

  const onDown = (e: PointerEvent) => {
    if (e.button !== 0) return;
    const p = point(e);
    const map = preview.onMap(p);
    const pane = map ? null : preview.paneAt(p);
    if (pane) preview.activePane = pane;
    dragging = { x: e.clientX, y: e.clientY, map, pane, travel: 0 };
    canvas.setPointerCapture(e.pointerId);
  };

  const onMove = (e: PointerEvent) => {
    if (!dragging || !e.buttons) { opts.onHover?.(point(e), e); return; }
    const scale = canvas.width / Math.max(1, canvas.getBoundingClientRect().width);
    const dx = (e.clientX - dragging.x) * scale, dy = -(e.clientY - dragging.y) * scale;
    dragging.travel += Math.hypot(e.clientX - dragging.x, e.clientY - dragging.y);
    dragging.x = e.clientX;
    dragging.y = e.clientY;
    if (dragging.map) preview.dragMap(dx, dy);
    else if (dragging.pane) preview.drag(dragging.pane, dx, dy);
    moved();
  };

  const onUp = (e: PointerEvent) => {
    const d = dragging;
    dragging = null;
    if (d && d.travel < DRAG_PX && !d.map) opts.onClick?.(point(e), e);
  };
  const onCancel = () => { dragging = null; };
  const onLeave = (e: PointerEvent) => { if (!dragging) opts.onHover?.(null, e); };

  const onWheel = (e: WheelEvent) => {
    e.preventDefault();
    const p = point(e);
    const f = zoomFactor(e.deltaY, e.deltaMode);
    const axes = wheelAxes(e);
    if (preview.onMap(p)) { preview.wheelMap(p, f, axes); moved(); return; }
    const pane = preview.paneAt(p);
    if (pane) { preview.activePane = pane; preview.wheel(pane, p, f, axes); moved(); }
  };

  const onDbl = (e: MouseEvent) => {
    if (opts.mapDoubleClick === false) return;
    const p = point(e);
    if (preview.onMap(p)) { preview.goToOnMap(p); moved(); }
  };

  const onMenu = (e: MouseEvent) => {
    if (!opts.onContext) return;
    e.preventDefault();
    opts.onContext(point(e), e);
  };

  canvas.addEventListener("pointerdown", onDown);
  canvas.addEventListener("pointermove", onMove);
  canvas.addEventListener("pointerup", onUp);
  canvas.addEventListener("pointercancel", onCancel);
  canvas.addEventListener("pointerleave", onLeave);
  canvas.addEventListener("wheel", onWheel, { passive: false });
  canvas.addEventListener("dblclick", onDbl);
  canvas.addEventListener("contextmenu", onMenu);

  return () => {
    canvas.removeEventListener("pointerdown", onDown);
    canvas.removeEventListener("pointermove", onMove);
    canvas.removeEventListener("pointerup", onUp);
    canvas.removeEventListener("pointercancel", onCancel);
    canvas.removeEventListener("pointerleave", onLeave);
    canvas.removeEventListener("wheel", onWheel);
    canvas.removeEventListener("dblclick", onDbl);
    canvas.removeEventListener("contextmenu", onMenu);
  };
}
