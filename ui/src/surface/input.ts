// **One input implementation, for every mount of this surface** (T-445).
//
// Moved verbatim out of `preview-main.ts`, which was the only host when T-450 wrote it. The cutover
// adds a second host — the app's Explore centre — and a second copy of "what a wheel means" is
// precisely **T-412's wheel-zoom mismatch**: the waterfall and the frequency navigator each had
// their own wheel handler, they disagreed about direction and factor, and the disagreement was the
// bug. Two hosts with one handler cannot reproduce it; two hosts with two handlers eventually will.
//
// So this file is the single answer to: where is the pointer, and which viewport is under it.
// **What the gesture MEANS is still not decided here** — `wheelZoom` (in `preview.ts`) reads the
// modifier bits and the deltas, and this file forwards the event and places the result. That split
// is T-456's, and it is what makes "one handler" worth anything: a host that re-read `deltaY` would
// be a second opinion about a wheel even while sharing a listener. Everything it calls is
// `SurfacePreview`'s, which is arithmetic over `PaneModel` — T-442 asserted the whole gesture
// vocabulary against a spy client and saw an **empty call list**, and that property is unchanged by
// having one more caller.
//
// Screen y runs down and the drawing buffer's runs up, so the vertical delta is negated exactly
// once, here, and every consumer below is in one convention.

import { dragIntent, type SurfacePreview, wheelZoom } from "./preview";

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
  /** A region stroke in progress, every move, for the host to draw as a pending box; `null` when
   * the stroke ended or was abandoned. It is *not* a commit — see [[SurfaceInputOptions.onRegion]]. */
  onRegionDrag?: (r: SurfaceRegion | null) => void;
  /** A region stroke that **committed**: shift was held at the press, the pointer travelled far
   * enough not to be a tap, and the rectangle is non-degenerate. */
  onRegion?: (r: SurfaceRegion) => void;
}

/** A rectangle strokes out on one pane, as its two corners in drawing-buffer coordinates. The
 * corners are in the order they were made (`a` is the press) and are **not** normalised here:
 * `normalizeRegion` in `./marks` is the one place that orders them, so the box that is drawn and
 * the region that is committed cannot be ordered by two different rules. */
export interface SurfaceRegion { pane: string; a: GlPoint; b: GlPoint }

/**
 * Travel, in CSS px, past which a pointer stream that has already panned is **not also a click**.
 *
 * Read what this is not. T-456 removed the drag *threshold*: a drag pans from its first move, a
 * zero-pixel move moves the view by zero, and nothing accumulates toward a decision — which is
 * T-407's lesson, since that defect was a 6 px threshold turning a tap into a drag. This constant
 * gates only whether `onClick` fires on release, and a click here focuses a row. It **cannot reach
 * a device**: the retune is a separate, explicit press (T-444), and `acceptPaneRetune` re-derives
 * its target at the instant of the commit precisely so no pointer stream can aim it.
 *
 * It is a **distance** (`Math.hypot`), never `dx + dy`: T-407's second defect was travel summed
 * across axes, so a stroke *across* one counted as travel *along* it.
 *
 * T-458 gives it a **second, opposite** job: a *region* stroke commits only once travel has passed
 * it. The two uses are consistent — below the threshold a pointer stream is a **tap**, and a tap
 * focuses a row and marks out nothing — and the pan is still unthresholded, because a pan that
 * moves the view by the distance travelled cannot turn a tap into anything.
 */
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

  let dragging:
    | { x: number; y: number; x0: number; y0: number; map: boolean; pane: string | null; travel: number; region: SurfaceRegion | null }
    | null = null;

  const endRegion = () => { if (dragging?.region) opts.onRegionDrag?.(null); };

  // **What the press means is decided ONCE, at the press, and it is decided by `dragIntent`.**
  //
  // Which modifier and why is `preview.ts`'s call, not this file's — the same split as `wheelZoom`,
  // and for the same reason: one place says what a modifier means, whichever event carries it. The
  // source guards in `surface-preview.test.ts` and `surface-cutover.test.ts` forbid a modifier bit
  // being read here at all.
  //
  // Latched, and never re-read per move: a modifier sampled on `pointermove` would let a stroke
  // change meaning halfway through — release shift mid-drag and the region you were marking out
  // becomes a pan of the very view you were marking it out on. T-407's family of defect is exactly
  // a pointer stream whose meaning is settled by something other than how it began.
  //
  // Nothing here moves a viewport, so a region stroke is not a pan: T-444's offer is neither
  // invalidated nor created by one, which is correct — the viewport did not move.
  const onDown = (e: PointerEvent) => {
    if (e.button !== 0) return;
    const p = point(e);
    const map = preview.onMap(p);
    const pane = map ? null : preview.paneAt(p);
    if (pane) preview.activePane = pane;
    const region = dragIntent(e) === "region" && pane && opts.onRegion ? { pane, a: p, b: p } : null;
    dragging = { x: e.clientX, y: e.clientY, x0: e.clientX, y0: e.clientY, map, pane, travel: 0, region };
    if (region) opts.onRegionDrag?.(region);
    canvas.setPointerCapture(e.pointerId);
  };

  const onMove = (e: PointerEvent) => {
    if (!dragging || !e.buttons) { opts.onHover?.(point(e), e); return; }
    const scale = canvas.width / Math.max(1, canvas.getBoundingClientRect().width);
    const dx = (e.clientX - dragging.x) * scale, dy = -(e.clientY - dragging.y) * scale;
    dragging.travel += Math.hypot(e.clientX - dragging.x, e.clientY - dragging.y);
    dragging.x = e.clientX;
    dragging.y = e.clientY;
    if (dragging.region) {
      dragging.region = { ...dragging.region, b: point(e) };
      opts.onRegionDrag?.(dragging.region);
      return; // a region stroke is not a pan: the view must not move under the rectangle
    }
    if (dragging.map) preview.dragMap(dx, dy);
    else if (dragging.pane) preview.drag(dragging.pane, dx, dy);
    moved();
  };

  // **The gesture ended, so the pane's follow/pause decision is committed now** (T-486).
  //
  // A pan moves the view on every pointer move; whether the pane ends up *paused* is decided once,
  // here, from where the viewport came to rest against the live edge. It is called for a cancel as
  // well as an up, because a stroke the browser takes away still ended — leaving a pane frozen one
  // pixel off live because the pointer was captured elsewhere is the reported bug with a different
  // cause. A region stroke never panned, so there is nothing to commit for one.
  const settle = (d: { map: boolean; pane: string | null; region: SurfaceRegion | null }) => {
    if (d.region) return;
    if (d.map) preview.endDragMap();
    else if (d.pane) preview.endDrag(d.pane);
    else return;
    moved();
  };

  const onUp = (e: PointerEvent) => {
    const d = dragging;
    dragging = null;
    if (!d) return;
    settle(d);
    if (d.region) {
      opts.onRegionDrag?.(null);
      // A TAP IS NEVER A REGION, and the test is on the rectangle the user actually ended up with.
      //
      // The gate is the **net** press-to-release displacement, as a `Math.hypot` distance — not the
      // accumulated path length `d.travel`, which a stroke that wandered out and came back can pass
      // while enclosing nothing, and emphatically not `dx + dy`, which was T-407's second defect.
      // Plus a non-degenerate extent on **both** axes, since a rectangle flat in either one is a
      // line: neither condition implies the other, so both are asked.
      const r: SurfaceRegion = { ...d.region, b: point(e) };
      const far = Math.hypot(e.clientX - d.x0, e.clientY - d.y0) >= DRAG_PX;
      if (far && r.a.x !== r.b.x && r.a.y !== r.b.y) opts.onRegion?.(r);
      return;
    }
    if (d.travel < DRAG_PX && !d.map) opts.onClick?.(point(e), e);
  };
  const onCancel = () => { endRegion(); if (dragging) settle(dragging); dragging = null; };
  const onLeave = (e: PointerEvent) => { if (!dragging) opts.onHover?.(null, e); };

  // **Every wheel over the canvas is the surface's, whatever is held down (T-456).**
  //
  // The listener is `{ passive: false }` precisely so this `preventDefault` binds, and it is
  // unconditional: ctrl+wheel is the browser's page-zoom shortcut and cmd+wheel is Safari's, so a
  // conditional one would let a user who reached for ctrl zoom the *document* on top of the surface.
  // What it cannot do is reach past the browser — macOS's own ctrl+scroll zoom (Accessibility →
  // Zoom) consumes the event before any `wheel` is dispatched, which is why `wheelAxes` puts the
  // time axis on ALT and leaves ctrl to fall through to the uniform gesture (where a trackpad pinch,
  // which Chrome and Safari deliver as ctrl+wheel, also belongs).
  //
  // `wheelZoom` is the whole of the interpretation, deltas included: shift-held on macOS arrives as
  // a *horizontal* scroll with `deltaY === 0`, so a host reading `deltaY` itself would make the
  // frequency axis inert on a real Mac while every synthesised test passed.
  const onWheel = (e: WheelEvent) => {
    e.preventDefault();
    const p = point(e);
    const { factor, axes } = wheelZoom(e);
    if (preview.onMap(p)) { preview.wheelMap(p, factor, axes); moved(); return; }
    const pane = preview.paneAt(p);
    if (pane) { preview.activePane = pane; preview.wheel(pane, p, factor, axes); moved(); }
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
