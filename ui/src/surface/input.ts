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

import {
  dragIntent, isShadowGainWheel, pinchZoom, type SurfacePreview, touchIntent, wheelDelta, wheelZoom,
} from "./preview";

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
  /**
   * **Measurement mode is a toggle elsewhere, not a modifier** (T-822 / MAP-22, docs/23 §10.4's
   * "tool mode" table): while `true`, a BARE (unmodified) drag on a pane marks out a measurement
   * instead of panning it. It is sampled once, at the press, exactly like `dragIntent` reads shift
   * for a region — the same T-407 discipline: a mode flipped mid-stroke must not turn the
   * measurement being marked out into a pan of the view it is being marked out on.
   *
   * **`Shift + drag` never changes meaning, in any tool mode** (docs/23 §10.4: "`Shift + drag`
   * therefore never changes meaning") — it always marks a region, mode or no mode, and is checked
   * BEFORE this flag. A tool mode re-binds only what a bare drag does.
   */
  measureMode?: boolean;
  /** A measurement stroke in progress, every move; `null` when it ended or was abandoned. Same
   * shape and the same "not a commit" rule as [[onRegionDrag]]. */
  onMeasureDrag?: (r: SurfaceRegion | null) => void;
  /**
   * A measurement stroke that **committed**. Unlike a region, whose two corners must be apart on
   * BOTH axes to describe a rectangle worth selecting, a measurement is meaningful with extent on
   * just ONE axis (a pure Δt or a pure Δf, inspectrum-style) — so only the tap/drag distance gate
   * applies here, never `SurfaceRegion`'s degenerate-axis refusal.
   */
  onMeasure?: (r: SurfaceRegion) => void;
  /**
   * **The Annotate and Pin tool modes** (T-820 / MAP-20, docs/23 §10.4's gesture table), sampled
   * once at the press exactly like [[measureMode]], and — like it — re-binding only the BARE
   * drag/click. `Shift + drag` is still checked first and still marks a region.
   *
   * - `"annotate"`: a bare drag strokes out an annotation **box** ([[onAnnotateDrag]] while it is
   *   drawn, [[onAnnotateBox]] when it commits); a bare tap drops a **text note** at the point.
   * - `"pin"`: a bare tap drops a **marker**; a bare drag is unchanged — it pans (the table's "-").
   *
   * In either mode a tap never also focuses a row (`onClick` does not fire): one gesture, one meaning.
   */
  annotateMode?: "annotate" | "pin" | null;
  /** An annotation box being stroked, every move; `null` when it ended or was abandoned. */
  onAnnotateDrag?: (r: SurfaceRegion | null) => void;
  /** An annotation box that **committed**: travel past [[DRAG_PX]] and a positive extent on BOTH
   * axes — the store's own rule for a `box` (docs/api.md, Annotations: a box needs a positive extent
   * in both axes), so a stroke the server would refuse is never offered to it. */
  onAnnotateBox?: (r: SurfaceRegion) => void;
  /** A tap in a tool mode that drops a point annotation: `"text"` in Annotate, `"marker"` in Pin. */
  onAnnotatePoint?: (p: { pane: string; at: GlPoint }, kind: "text" | "marker") => void;
  /**
   * **A view-moving gesture touched a PANE** (T-1028), so a host that is in retune mode knows when
   * to act and when a gesture ended.
   *
   * `ended: false` is "the view moved just now"; `ended: true` is the gesture's own end — the
   * pointer released, or a pinch's second finger lifted — which is the same commit point T-486's
   * follow/pause decision is made at, for the same reason: a gesture is over when the browser says
   * it is over, not when a timer guesses. A wheel has no end and only ever reports `false`; the
   * host is the one that decides what stillness means.
   *
   * **This file still reaches nothing.** It reports that a pane's view moved; what that means is the
   * host's, exactly as `measureMode` and `annotateMode` are. The map strip is deliberately not
   * reported: it is a navigator onto the surface, not a pane's window, and nothing about dragging it
   * says where the radio should look.
   */
  onGesture?: (g: { pane: string; ended: boolean }) => void;
  /** **Ctrl+Shift+wheel adjusts the shadow's brightness instead of zooming** (T-526) — a client-only
   * display preference, never a view or device change. `notches` is the gesture's own signed count
   * (positive brightens); the host clamps and persists (`./shadow-gain.ts`) and calls
   * `Surface.setShadowGain`. Present only where a host wants the gesture; absent, ctrl+shift falls
   * through to the ordinary zoom below (shift alone already zooms frequency only). */
  onShadowGain?: (notches: number) => void;
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
  /**
   * A viewport moved. `pane` names the pane when the gesture was on one (T-1028's hook); the map
   * strip passes none, because it is not a pane's window.
   */
  const moved = (pane: string | null = null, ended = false) => {
    opts.onView?.();
    if (pane) opts.onGesture?.({ pane, ended });
  };
  /** Make the pane under `p` active (the map strip is not a pane and changes nothing). */
  const activate = (p: GlPoint) => {
    if (preview.onMap(p)) return;
    const pane = preview.paneAt(p);
    if (pane) preview.activePane = pane;
  };

  let dragging:
    | {
      x: number; y: number; x0: number; y0: number; map: boolean; pane: string | null; travel: number;
      region: SurfaceRegion | null; measuring: SurfaceRegion | null;
      annotating: SurfaceRegion | null; tool: "annotate" | "pin" | null;
      /** T-824: a finger's stroke whose meaning is not decided yet — pan, or (held first) a region.
       * It moves nothing until it has travelled `DRAG_PX` from the press; then `touchIntent` decides,
       * once. `t0` is the press's own `timeStamp`, so the hold is read off the events, not a clock. */
      undecided: boolean; t0: number; press: GlPoint;
    }
    | null = null;

  // **Touch (T-824, docs/23 §10.5).** Fingers on the canvas, by pointer id, in client px. Two of them
  // are a pinch: zoom both axes about their midpoint and pan with it — view arithmetic on the same
  // `preview` methods a wheel and a drag use, so it reaches exactly what they reach: nothing. A third
  // finger is ignored. When a pinch ends, the finger left down does nothing until it lifts: carrying
  // it on as a pan would move the view by however far the fingers drifted apart.
  const fingers = new Map<number, { x: number; y: number }>();
  let pinch: { map: boolean; pane: string | null; spread: number; mid: { x: number; y: number } } | null = null;
  const spreadMid = () => {
    const [a, b] = [...fingers.values()];
    return { spread: Math.hypot(a.x - b.x, a.y - b.y), mid: { x: (a.x + b.x) / 2, y: (a.y + b.y) / 2 } };
  };
  const isTouch = (e: PointerEvent) => e.pointerType === "touch";

  const endRegion = () => {
    if (dragging?.region) opts.onRegionDrag?.(null);
    if (dragging?.measuring) opts.onMeasureDrag?.(null);
    if (dragging?.annotating) opts.onAnnotateDrag?.(null);
  };

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
    if (e.button !== 0) {
      // T-1000: a right (or middle) press on a pane still says which pane the user is pointing at,
      // so it sets the active pane exactly as a primary press does — before this, a right-click
      // opened a pane's context menu while the chrome went on acting on whichever pane was pressed
      // last. It starts no stroke: nothing pans, marks or measures, and nothing reaches a device.
      if (!isTouch(e)) activate(point(e));
      return;
    }
    if (isTouch(e)) {
      if (fingers.size >= 2) return; // a third finger means nothing
      fingers.set(e.pointerId, { x: e.clientX, y: e.clientY });
      if (fingers.size === 2) {
        // The second finger turns whatever the first began into a pinch. A region or measurement
        // stroke in progress is abandoned (not committed); a pan already made stays made.
        endRegion();
        const d = dragging;
        dragging = null;
        const { spread, mid } = spreadMid();
        const p = point({ clientX: mid.x, clientY: mid.y });
        const map = d ? d.map : preview.onMap(p);
        const pane = map ? null : d?.pane ?? preview.paneAt(p);
        if (pane) preview.activePane = pane;
        pinch = { map, pane, spread, mid };
        canvas.setPointerCapture(e.pointerId);
        return;
      }
    }
    const p = point(e);
    const map = preview.onMap(p);
    const pane = map ? null : preview.paneAt(p);
    if (pane) preview.activePane = pane;
    // **`Shift + drag` never changes meaning, in any tool mode** (docs/23 §10.4's gesture table): it
    // is checked FIRST, so entering measurement mode re-binds only the BARE drag/click — exactly as
    // `dragIntent`'s own modifier check already read it, mode or no mode. Only when shift is not the
    // press's own intent does a tool mode get to claim a bare drag for itself.
    const region = dragIntent(e) === "region" && pane && opts.onRegion ? { pane, a: p, b: p } : null;
    const measuring = !region && opts.measureMode === true && pane && opts.onMeasure ? { pane, a: p, b: p } : null;
    // T-820: the Annotate/Pin mode, latched here too. Only a pane press that is not a region or a
    // measurement is claimed; the map strip keeps its own meaning in every mode.
    const tool = !region && !measuring && pane && opts.annotateMode ? opts.annotateMode : null;
    const annotating = tool === "annotate" && pane && opts.onAnnotateBox ? { pane, a: p, b: p } : null;
    // A finger with no mode of its own to follow waits to see whether it was held (T-824). A tool
    // mode is a mode of its own: Annotate claims the stroke like Measure does, and in Pin a finger
    // pans and taps exactly as a mouse does, so a tap drops the marker rather than focusing a row.
    const undecided = isTouch(e) && !region && !measuring && !tool;
    dragging = {
      x: e.clientX, y: e.clientY, x0: e.clientX, y0: e.clientY, map, pane, travel: 0, region, measuring, annotating, tool,
      undecided, t0: e.timeStamp, press: p,
    };
    if (region) opts.onRegionDrag?.(region);
    if (measuring) opts.onMeasureDrag?.(measuring);
    if (annotating) opts.onAnnotateDrag?.(annotating);
    canvas.setPointerCapture(e.pointerId);
  };

  const onMove = (e: PointerEvent) => {
    if (isTouch(e) && fingers.has(e.pointerId)) fingers.set(e.pointerId, { x: e.clientX, y: e.clientY });
    if (pinch) {
      if (!fingers.has(e.pointerId) || fingers.size < 2) return;
      const { spread, mid } = spreadMid();
      const scale = canvas.width / Math.max(1, canvas.getBoundingClientRect().width);
      const { factor, axes } = pinchZoom(pinch.spread, spread);
      const at = point({ clientX: mid.x, clientY: mid.y });
      const dx = (mid.x - pinch.mid.x) * scale, dy = -(mid.y - pinch.mid.y) * scale;
      if (pinch.map) { preview.wheelMap(at, factor, axes); preview.dragMap(dx, dy); }
      else if (pinch.pane) { preview.wheel(pinch.pane, at, factor, axes); preview.drag(pinch.pane, dx, dy); }
      pinch.spread = spread;
      pinch.mid = mid;
      moved(pinch.map ? null : pinch.pane);
      return;
    }
    if (!dragging || !e.buttons) { opts.onHover?.(point(e), e); return; }
    if (dragging.undecided) {
      // Nothing moves until the finger has gone somewhere (a jittery tap stays a tap); then the
      // stroke's meaning is decided once, from how long it rested first.
      dragging.travel += Math.hypot(e.clientX - dragging.x, e.clientY - dragging.y);
      dragging.x = e.clientX;
      dragging.y = e.clientY;
      if (Math.hypot(e.clientX - dragging.x0, e.clientY - dragging.y0) < DRAG_PX) return;
      dragging.undecided = false;
      if (touchIntent(e.timeStamp - dragging.t0) === "region" && dragging.pane && opts.onRegion) {
        dragging.region = { pane: dragging.pane, a: dragging.press, b: point(e) };
        opts.onRegionDrag?.(dragging.region);
        return;
      }
      // A pan: catch the view up with the finger, from the press, so the data under it stays under it.
      const scale = canvas.width / Math.max(1, canvas.getBoundingClientRect().width);
      const dx = (e.clientX - dragging.x0) * scale, dy = -(e.clientY - dragging.y0) * scale;
      if (dragging.map) preview.dragMap(dx, dy);
      else if (dragging.pane) preview.drag(dragging.pane, dx, dy);
      moved(dragging.map ? null : dragging.pane);
      return;
    }
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
    if (dragging.measuring) {
      dragging.measuring = { ...dragging.measuring, b: point(e) };
      opts.onMeasureDrag?.(dragging.measuring);
      return; // a measurement stroke is not a pan either
    }
    if (dragging.annotating) {
      dragging.annotating = { ...dragging.annotating, b: point(e) };
      opts.onAnnotateDrag?.(dragging.annotating);
      return; // nor is an annotation box: the view must not move under what is being drawn on it
    }
    if (dragging.map) preview.dragMap(dx, dy);
    else if (dragging.pane) preview.drag(dragging.pane, dx, dy);
    moved(dragging.map ? null : dragging.pane);
  };

  // **The gesture ended, so the pane's follow/pause decision is committed now** (T-486).
  //
  // A pan moves the view on every pointer move; whether the pane ends up *paused* is decided once,
  // here, from where the viewport came to rest against the live edge. It is called for a cancel as
  // well as an up, because a stroke the browser takes away still ended — leaving a pane frozen one
  // pixel off live because the pointer was captured elsewhere is the reported bug with a different
  // cause. A region stroke never panned, so there is nothing to commit for one.
  const settle = (d: {
    map: boolean; pane: string | null; region: SurfaceRegion | null; measuring: SurfaceRegion | null; annotating: SurfaceRegion | null;
  }) => {
    if (d.region || d.measuring || d.annotating) return;
    if (d.map) preview.endDragMap();
    else if (d.pane) preview.endDrag(d.pane);
    else return;
    // T-1028: the gesture ENDED. Reported here, with T-486's own commit point, so a host in retune
    // mode acts on the view the user came to rest on rather than on one of the moves along the way.
    moved(d.map ? null : d.pane, true);
  };

  // A pinch ends when either finger lifts: its target commits its follow/pause decision exactly as
  // a drag's does (T-486), and the finger still down is inert until it lifts too.
  const endPinch = () => {
    const p = pinch;
    pinch = null;
    if (p) { settle({ map: p.map, pane: p.pane, region: null, measuring: null, annotating: null }); }
  };

  const onUp = (e: PointerEvent) => {
    if (isTouch(e)) fingers.delete(e.pointerId);
    if (pinch) { endPinch(); return; }
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
    if (d.measuring) {
      opts.onMeasureDrag?.(null);
      // Same tap/distance gate as a region, minus the both-axes rule: a measurement with extent on
      // only one axis (a vertical Δt stroke, a horizontal Δf stroke) is still a real measurement.
      const r: SurfaceRegion = { ...d.measuring, b: point(e) };
      const far = Math.hypot(e.clientX - d.x0, e.clientY - d.y0) >= DRAG_PX;
      if (far) opts.onMeasure?.(r);
      return;
    }
    if (d.annotating) {
      opts.onAnnotateDrag?.(null);
      // The region's rule exactly: net displacement as a distance, and extent on both axes. A tap
      // in Annotate is not a box — it is a text note at the point, below.
      const r: SurfaceRegion = { ...d.annotating, b: point(e) };
      const far = Math.hypot(e.clientX - d.x0, e.clientY - d.y0) >= DRAG_PX;
      if (far) {
        if (r.a.x !== r.b.x && r.a.y !== r.b.y) opts.onAnnotateBox?.(r);
      } else if (d.pane) opts.onAnnotatePoint?.({ pane: d.pane, at: r.a }, "text");
      return;
    }
    if (d.tool === "pin") {
      // A pin-mode drag panned (and `settle` committed it); only a tap drops a marker.
      if (d.travel < DRAG_PX && d.pane) opts.onAnnotatePoint?.({ pane: d.pane, at: point(e) }, "marker");
      return;
    }
    if (d.undecided) {
      // A finger that never travelled: a tap selects, as a click does; one that was HELD is the
      // touch long-press — the context menu, at the press (docs/23 §10.5). Decided here rather than
      // by the browser's own `contextmenu`, which fires mid-hold and would open the menu under a
      // finger about to drag out a region (see `onMenu`).
      if (touchIntent(e.timeStamp - d.t0) === "region") { if (!d.map) opts.onContext?.(d.press, e); }
      else if (!d.map) opts.onClick?.(point(e), e);
      return;
    }
    if (d.travel < DRAG_PX && !d.map) opts.onClick?.(point(e), e);
  };
  const onCancel = (e: PointerEvent) => {
    if (e && isTouch(e)) fingers.delete(e.pointerId);
    if (pinch) { endPinch(); return; }
    endRegion();
    if (dragging && !dragging.undecided) settle(dragging);
    dragging = null;
  };
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
    // **Ctrl+Shift is claimed for shadow brightness, before the zoom gesture is even read** (T-526).
    // It must never reach `wheelZoom`/`preview.wheel*`: those are the only routes that can move a
    // viewport, and this gesture changes nothing about time, frequency or the device — only how dark
    // the last-known mark renders. Without a host-supplied handler the combo falls through unchanged
    // to the ordinary zoom (shift alone already means "frequency only").
    if (isShadowGainWheel(e) && opts.onShadowGain) {
      const d = wheelDelta(e);
      if (d !== 0) opts.onShadowGain(d < 0 ? 1 : -1);
      return;
    }
    const p = point(e);
    const { factor, axes } = wheelZoom(e);
    if (preview.onMap(p)) { preview.wheelMap(p, factor, axes); moved(); return; }
    const pane = preview.paneAt(p);
    // A wheel has no release, so it is never reported as ENDED: stillness is the host's call.
    if (pane) { preview.activePane = pane; preview.wheel(pane, p, factor, axes); moved(pane); }
  };

  const onDbl = (e: MouseEvent) => {
    if (opts.mapDoubleClick === false) return;
    const p = point(e);
    if (preview.onMap(p)) { preview.goToOnMap(p); moved(); }
  };

  const onMenu = (e: MouseEvent) => {
    // T-1000: the menu request itself names the pane too — a keyboard's context-menu key, or a
    // platform whose secondary click arrives with no non-primary `pointerdown` before it.
    if (fingers.size === 0) activate(point(e));
    if (!opts.onContext) return;
    e.preventDefault();
    // A finger is still down (T-824): the browser's long-press menu, fired mid-hold. The stroke
    // itself decides at release whether it was a long-press (menu) or a hold-then-drag (region).
    if (fingers.size > 0) return;
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
