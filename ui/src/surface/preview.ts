// **The host.** T-450: the thing that mounts `ui/src/surface/` against the live `GET /api/tiles`,
// so the surface can be *looked at* before anything is retired.
//
// T-442 wrote it plainly — *"nothing mounts it yet, because nothing mounts the surface yet"* — and
// left `rects()`/`list()` as the seam. Twelve production files sat behind that seam with no
// importer. This file is the importer, and it is deliberately **additive**: it is reached from its
// own page (`/surface.html`), it shares no state with the app, and nothing under `ui/src/app/`
// imports it. `ui/test/surface-preview.test.ts` asserts that by walking the app's import graph,
// because "additive" is a claim about the whole repo and not something a diff review can see.
//
// ## Historical only, and structurally so
//
// The scope is a **pannable view over recorded history**. It must not depend on T-439 (live-edge
// tiles) or T-445 (the cutover), and "must not" is easy to violate by accident because
// `PaneModel`'s default window *is* the following one. So the edge is pinned instead of policed:
// [[SurfaceOrigin.edgeNs]] is resolved once from `GET /api/navigation`'s `time.latest_s` and handed
// to every `frame()` unchanged, and every viewport is frozen at open. A following pane derives its
// box from the edge it is given, so an edge that never moves is a view that cannot creep forward —
// there is no clock here to accidentally wire up.
//
// For the same reason no active-capture segment is drawn: `liveSegmentQuads` places a mark at the
// live edge, and this preview does not read the live edge. Drawing one from a fixed historical
// instant would be a live claim with no live evidence, which is the class of thing this whole
// milestone exists to refuse. The chrome says so rather than leaving it to be noticed.
//
// ## Nothing here can command the radio
//
// T-444's retune-on-pan exists (`./retune.ts`) and is **not imported**. Nor is `../app/centre/view`,
// which owns `applyDeviceAction`. Every gesture below is arithmetic on `PaneModel`, which T-442
// proved reaches nothing outside itself. The only routes named in this file are `GET /api/tiles`,
// `GET /api/navigation` and `GET /api/coverage`, all read-only; the test asserts that set against
// the source, so a device route cannot appear here without an argument.

import { ControlError } from "../controls/client";
import {
  coverageUrl, observedExtent, openingWindow, orientationNote, surfaceBounds,
  type CoverageCensus, type CoverageSlice, type NavigationSlice, type OpeningWindow, type SurfaceOrigin,
} from "./bootstrap";
import { tileUrl, type Box, type Lattice, type TileAddr } from "./lattice";
import type { OverlayQuad } from "./minimap";
import type { ActiveWindow } from "../navigators";
import { probeAddr, fetchTile, latticeOf, type TileFetch, type TileResponse } from "./tile";
import { TileCache } from "./tilecache";
import type { PaneRect, PaneReport, PaneView, TilePlanes } from "./surface";
import { SurfaceView, type SurfaceFrame } from "./view";

/** Cells per tile edge the preview renders at — the route's own default, and the size the cache
 * budget in `tilecache.ts` was measured against. */
export const RENDER_CELLS = 256;
/** The orientation coverage map's grid. 128 × 32 = 4096 cells, the route's own cap. */
export const ORIENT_CELLS = 128, ORIENT_ROWS = 32;

/** How the bootstrap asks. One function so a test can record exactly what was requested. */
export type Getter = (path: string) => Promise<unknown>;

/**
 * How the bootstrap handles the route's ingest backpressure.
 *
 * **The probe is the one tile fetch that does not go through `TileCache`**, and that is how T-454's
 * defect reached the user as a page-fatal banner quoting the route's `503` verbatim. The cache had
 * the cap, the cancellation and the backoff; this path had none of them, because it is one cheap
 * `cells=8` tile asked once at open — and a refusal is precisely what a *shared* budget serves to a
 * new arrival. Reloading the page mid-drag was enough: `hk-api` keeps a [`TileSlot`] until its read
 * finishes, so the previous page's abandoned reads were still holding all four.
 *
 * A refusal is not a failure here either. It is answered the same way the cache answers it — wait,
 * ask again — and only an exhausted retry is something the user should ever be told about.
 */
export interface BackpressureOptions {
  /** Attempts after the first, per request. */
  retries?: number;
  /** First wait, ms; doubles per attempt. */
  backoffMs?: number;
  sleep?: (ms: number) => Promise<void>;
}

const REFUSAL_STATUS = 503;

/** Is this the route saying "too many at once" rather than something being wrong? */
export function isBackpressure(e: unknown): boolean {
  return e instanceof ControlError && e.status === REFUSAL_STATUS;
}

/** Everything the preview needs before its first frame, and where each part came from. */
export interface SurfaceProbe {
  readonly lattice: Lattice;
  readonly origin: SurfaceOrigin;
  readonly census: CoverageCensus;
  readonly opening: OpeningWindow;
  /** The sentence that makes a 99.4 %-grey first screen a finding instead of a bug report. */
  readonly note: string;
  /** Paths requested, in order. Surfaced so the page can show them and a test can assert them. */
  readonly requests: readonly string[];
  /** Routes that failed and what the preview did instead — never silently degraded. */
  readonly degraded: readonly string[];
}

/**
 * Three read-only GETs, in dependency order.
 *
 * Only the first is fatal: without a tile answer there is no lattice, and a client that guessed one
 * would be addressing a pyramid that does not exist. The other two degrade *and say so* — an
 * invented bound must never be indistinguishable from a reported one.
 */
export async function probeSurface(get: Getter, nowS?: number, bp: BackpressureOptions = {}): Promise<SurfaceProbe> {
  const requests: string[] = [];
  const degraded: string[] = [];
  const retries = bp.retries ?? 5;
  const backoffMs = bp.backoffMs ?? 150;
  const sleep = bp.sleep ?? ((ms: number) => new Promise<void>((r) => setTimeout(r, ms)));
  // Every request here retries a `503`, not only the tile probe: the refusal means the history lock
  // is busy, and asking again is the whole of the right response. Each attempt is pushed to
  // `requests`, so what the client actually asked for is visible to the page and to a test.
  const ask = async (path: string): Promise<unknown> => {
    for (let attempt = 0; ; attempt++) {
      requests.push(path);
      try {
        return await get(path);
      } catch (e) {
        if (attempt >= retries || !isBackpressure(e)) throw e;
        await sleep(backoffMs * 2 ** attempt);
      }
    }
  };

  const probePath = tileUrl(probeAddr());
  const probe = (await ask(probePath)) as TileResponse & {
    coverage?: { horizon?: { oldest_record_s?: number | null } | null } | null;
  };
  const lattice = latticeOf(probe, RENDER_CELLS);

  let nav: NavigationSlice | null = null;
  try {
    nav = (await ask("/api/navigation")) as NavigationSlice;
  } catch (e) {
    degraded.push(`GET /api/navigation failed (${describe(e)}): the surface's extent falls back to the view lattice's own axes and this client's clock.`);
  }

  const origin = surfaceBounds(nav, probe.coverage?.horizon?.oldest_record_s ?? null, lattice, nowS);

  let cov: CoverageSlice | null = null;
  try {
    cov = (await ask(coverageUrl(origin.bounds, ORIENT_CELLS, ORIENT_ROWS))) as CoverageSlice;
  } catch (e) {
    degraded.push(`GET /api/coverage failed (${describe(e)}): the view opens on the whole surface, because nothing said where the radio looked.`);
  }

  const census = observedExtent(cov);
  // **One refinement pass, measured rather than assumed.** A 128-cell map of a 6.5 GHz surface has
  // 51.2 MHz cells, so the coarse box around a 2.4 MHz capture is ~20x too wide — measured on a
  // replay: 0.78 % observed, and the observed box came back as 51.2–102.4 MHz for a recording that
  // spans 99.6–102 MHz. Opening there would put the capture in a twentieth of the pane's width and
  // read as "still nothing here". Asking the *same route* again over the box it just returned costs
  // one request and is the same question at the resolution the answer made available.
  let refined = census;
  if (census.box) {
    try {
      refined = observedExtent((await ask(coverageUrl(census.box, ORIENT_CELLS, ORIENT_ROWS))) as CoverageSlice);
    } catch (e) {
      degraded.push(`the coverage refinement pass failed (${describe(e)}): the view opens on the coarse observed box, which may be much wider than what was actually sampled.`);
      refined = census;
    }
    // A refinement that found nothing is not evidence against the coarse answer — the coarse cell
    // was observed, so something is in there. Keep the wider box rather than opening on nowhere.
    if (refined.observed === 0) refined = census;
  }
  // The note's share is the SURFACE-wide census: it is a statement about the whole surface, and
  // quoting the refined pass's share (measured inside coverage, so near 100 %) would invert it.
  const opening = openingWindow(origin.bounds, refined.box);
  return { lattice, origin, census, opening, note: orientationNote(census, opening), requests, degraded };
}

const describe = (e: unknown): string =>
  e instanceof ControlError ? `HTTP ${e.status} ${e.code}` : e instanceof Error ? e.message : String(e);

/** A pointer position in the drawing buffer, GL convention: origin **bottom-left**.
 *
 * Deliberately NOT exported: `ui/src/surface/input.ts` already exports this name, and two exported
 * spellings of one idea is the drift this directory keeps closing. Callers pass object literals. */
interface GlPoint { readonly x: number; readonly y: number }

const inRect = (r: PaneRect, p: GlPoint): boolean =>
  p.x >= r.x && p.x < r.x + r.w && p.y >= r.y && p.y < r.y + r.h;

/**
 * **Which pane a pointer belongs to — and the rule that the trace strip is part of its pane.**
 *
 * T-457 carves a strip off the top of each pane's rectangle for the spectrum trace. **The strip is a
 * readout, not a control: it passes every pointer event through to the pane it describes.** A point
 * in it resolves to that pane, and callers clamp it into the pane's own rectangle, so it reads as a
 * point on the pane's **top edge** — the same frequency, at the pane's newest instant, which is
 * exactly the instant the strip is a spectrum *of*. Nothing about a gesture changes because it
 * started a few pixels higher.
 *
 * The alternative — the strip handling pointers itself with a meaning of its own — was rejected
 * twice over: the obvious meaning for a vertical drag on a dB axis is *set the display range by
 * hand*, which is the control T-457 deliberately did not restore; and a second gesture vocabulary on
 * one canvas is T-412's wheel-zoom mismatch waiting to happen.
 *
 * **Why this is a function and not two lines inside `paneAt`.** T-457 and T-458 were each green
 * alone and broke on merge: one changed the geometry the other's gestures are measured in, and the
 * strip became a hole that swallowed every drag starting in it — not only the new region stroke, but
 * plain and alt drags that T-456 had settled. The invariant that catches that class is *"turning the
 * trace on may not shrink the set of points a gesture can start from"*, and it is only checkable if
 * the resolution is a pure function of a frame. `ui/test/surface-trace.test.ts` asserts it over a
 * grid of points, with and without the strip, knowing nothing about any particular gesture.
 */
export function paneAtPoint(frame: SurfaceFrame | null, minimapId: string, p: GlPoint): string | null {
  for (const v of frame?.views ?? []) {
    if (v.id === minimapId) continue;
    if (inRect(v.rect, p)) return v.id;
  }
  for (const t of frame?.traces ?? []) {
    if (inRect(t.rect, p)) return t.id;
  }
  return null;
}

/** `p` clamped into `rect`. A point in a pane's trace strip becomes a point on its top edge. */
export function clampToRect(rect: PaneRect, p: GlPoint): GlPoint {
  return {
    x: Math.min(Math.max(p.x, rect.x), rect.x + rect.w),
    y: Math.min(Math.max(p.y, rect.y), rect.y + rect.h),
  };
}

/** Wheel steps to a zoom factor. `> 1` zooms out, matching `PaneModel`'s own convention. */
export function zoomFactor(deltaY: number, deltaMode = 0): number {
  // `deltaMode` 1 is lines and 2 is pages; normalise to the pixel scale a trackpad reports.
  const px = deltaY * (deltaMode === 1 ? 16 : deltaMode === 2 ? 400 : 1);
  return Math.min(4, Math.max(0.25, Math.exp(px * 0.0015)));
}

/**
 * The scroll this wheel event actually carried, as one number.
 *
 * **Why `deltaX` is read at all, and only under shift.** macOS — and Chrome and Safari generally —
 * deliver a *shift-held* wheel as a HORIZONTAL scroll: the scroll arrives in `deltaX` and `deltaY`
 * is 0. Reading `deltaY` alone therefore makes shift+wheel, the frequency axis, do nothing at all on
 * the machine this runs on, and **no CDP-synthesised event would ever show it**, because the swap
 * happens in the platform's input layer rather than in the page. So the shift case takes whichever
 * axis carried the scroll.
 *
 * It is the **dominant** axis, never the sum. T-407's defect was travel measured as
 * `clientX + clientY`, which let a stroke *across* a bar count as travel *along* it; adding the two
 * deltas here would be the same mistake, and would make a diagonal trackpad flick zoom twice as far
 * as either of its components asked for. Without shift, `deltaX` is left alone entirely: a two-
 * finger horizontal swipe is a scroll, not a zoom, and on a canvas whose drag already pans it would
 * be startling for one to change the view's scale.
 */
export function wheelDelta(e: { deltaX?: number; deltaY?: number; shiftKey?: boolean }): number {
  const y = e.deltaY ?? 0, x = e.deltaX ?? 0;
  return e.shiftKey === true && Math.abs(x) > Math.abs(y) ? x : y;
}

/**
 * Which axes a wheel gesture zooms. Stated as data so the page can print the same table it obeys.
 *
 * **T-456 — Google-Maps navigation, with the axes still independently reachable:**
 *
 * | gesture | axes |
 * |---|---|
 * | drag | pans **both** (see [[SurfacePreview.drag]]) |
 * | plain wheel | zooms **both**, uniformly, about the cursor |
 * | **shift** + wheel | frequency (X) only |
 * | **alt / option** + wheel | time (Y) only |
 *
 * **Why ALT/OPTION for the time axis and not CTRL.** The brief allowed either and asked for the
 * reason; there are three, and the first is not testable from inside a browser at all:
 *
 *  1. **macOS takes ctrl+scroll before any browser sees it.** System Settings → Accessibility →
 *     Zoom → *"Use scroll gesture with modifier keys to zoom"* defaults to **^Control**, and when it
 *     is on the OS consumes the event: no `wheel` is dispatched, so there is nothing to
 *     `preventDefault` and no in-browser test can observe the difference between "the user did not
 *     scroll" and "the OS ate it". A binding whose failure mode is invisible to its own guard is the
 *     wrong binding.
 *  2. **ctrl+wheel is also how a trackpad PINCH arrives.** Chrome and Safari synthesise a pinch as a
 *     wheel event with `ctrlKey` set. Binding time to ctrl would make a pinch — the most Google-Maps
 *     gesture there is — zoom one axis. Leaving ctrl unbound drops a pinch into the uniform branch
 *     below, which zooms both axes about the cursor: exactly what a pinch should do.
 *  3. **Alt/Option carries no OS or browser default on a wheel**, so the `preventDefault` in
 *     `preview-main.ts` is a complete answer for it, where for ctrl it is only a partial one.
 *
 * Ctrl and meta are therefore *deliberately* not read here. They are still `preventDefault`ed at the
 * canvas, so a ctrl+wheel or cmd+wheel over the surface zooms the surface rather than the page.
 *
 * **This does not re-weld the axes.** Uniform zoom is a *gesture* that applies one factor to two
 * independent windows; each axis is still clamped on its own and still resolves its own pyramid
 * level (T-434/T-438/T-440). After a plain wheel the two may legitimately sit at different levels —
 * which is what `levelDivergenceNote` exists to say.
 */
export function wheelAxes(
  e: { shiftKey?: boolean; altKey?: boolean; ctrlKey?: boolean; metaKey?: boolean },
): { freq: boolean; time: boolean } {
  const freqOnly = e.shiftKey === true, timeOnly = e.altKey === true;
  // Both modifiers held is not a third gesture: it names both axes, which is the uniform one.
  if (freqOnly !== timeOnly) return { freq: freqOnly, time: timeOnly };
  return { freq: true, time: true };
}

/** The part of a `WheelEvent` a zoom gesture reads. A structural type, so this file stays testable
 * without a DOM and a host can pass the real event straight in. */
export interface WheelLike {
  readonly deltaX?: number;
  readonly deltaY?: number;
  readonly deltaMode?: number;
  readonly shiftKey?: boolean;
  readonly altKey?: boolean;
  readonly ctrlKey?: boolean;
  readonly metaKey?: boolean;
}

/**
 * **The whole of a wheel gesture, decided in one place: which axes, and by how much.**
 *
 * The surface has more than one host — the preview page, and the app's Explore centre after the
 * cutover — and *two hosts each doing their own wheel arithmetic is T-412's wheel-zoom mismatch by
 * construction*. So a host's listener does exactly three things: `preventDefault`, convert the
 * pointer to drawing-buffer coordinates, and call this. Nothing host-side may read `deltaY`,
 * `shiftKey` or `altKey` itself — `ui/test/surface-preview.test.ts` asserts that against the source
 * of every file in this directory that registers a `wheel` listener, because the failure mode of
 * getting it wrong is silent: `zoomFactor(e.deltaY, …)` compiles, runs, and makes shift+wheel inert
 * on macOS, where a shift-held wheel arrives in `deltaX`.
 */
export function wheelZoom(e: WheelLike): { factor: number; axes: { freq: boolean; time: boolean }; delta: number } {
  const delta = wheelDelta(e);
  return { delta, factor: zoomFactor(delta, e.deltaMode ?? 0), axes: wheelAxes(e) };
}

/** The part of a `PointerEvent` a drag gesture reads. */
export interface PointerLike {
  readonly shiftKey?: boolean;
  readonly altKey?: boolean;
  readonly ctrlKey?: boolean;
  readonly metaKey?: boolean;
}

/**
 * **What a press means: pan the view, or mark out a region** (T-458).
 *
 * It lives here, beside [[wheelAxes]], for the same reason and under the same rule: a host's
 * listener may not read a modifier bit itself, so there is exactly one file that says what a
 * modifier means, whichever event carries it. The source guards in
 * `ui/test/surface-preview.test.ts` and `ui/test/surface-cutover.test.ts` enforce that on
 * `input.ts` by name, and they pass unedited because of this function.
 *
 * **Shift, and the three that were rejected.**
 * - *Ctrl* is the same invisible failure T-456 rejected ctrl+wheel for, in its pointer form: on
 *   macOS ctrl+click **is** the secondary click, so the browser sends `contextmenu` and
 *   `button === 2` and the stroke silently becomes "open the menu".
 * - *Alt* is Chrome's copy-drag modifier and is grabbed by common Linux window managers to move the
 *   window — again, a gesture the page never learns it did not receive.
 * - *Right-drag* would have to fight the context menu, which is this surface's only route to
 *   Promote / Delete / Adjust band / Reset band.
 * - A *mode toggle* is a state a user can be in without noticing; the surface already has one such
 *   (Live/Paused) and a second would compound it. (A mode is still right for naming *which* signal
 *   a band override applies to, where the target has to be said out loud anyway — that is
 *   `explore.bandEdit`, and it selects the stroke's destination rather than arming the stroke.)
 *
 * **Why shift does not collide with T-456's `shift + wheel = frequency`.** A wheel and a captured
 * pointer drag are disjoint event streams — no event can be claimed by both bindings, and a user
 * cannot be mid-gesture in both — and the two readings are one idea rather than two: shift confines
 * the gesture to a *region of frequency* instead of sliding the whole view. Alt would have been the
 * real collision, since `alt + wheel` means "time only" and an alt-drag meaning "select" has no such
 * story.
 */
export function dragIntent(e: PointerLike): "pan" | "region" {
  return e.shiftKey === true ? "region" : "pan";
}

export interface PreviewOptions {
  canvas: HTMLCanvasElement;
  probe: SurfaceProbe;
  token: string;
  fetchFn: TileFetch;
  /** Where `SurfaceChrome` mounts its per-viewport level readout. Null in a headless test. */
  chrome?: HTMLElement | null;
  minimapPx?: number;
  /**
   * **The growing edge, reported in (T-445).** Omit it and the surface is historical: the edge is
   * the one `probeSurface` resolved, it never advances, and every viewport opens frozen — which is
   * exactly T-450's preview and stays its behaviour unchanged.
   *
   * Supply it and the same host becomes the *live* view: the first pane opens **following**, and
   * `frame()` asks this function where capture has got to. It is still *reported in* and never
   * controlled from here — T-442's rule, unchanged: capture, the ring and detection are never
   * consulted by a gesture, and a pane's pause is still only its own time window.
   *
   * There is deliberately no second host class for "the live one". The live-versus-history split
   * is the seam docs/16 §8.5 retires; two hosts would be that seam moved into the client.
   */
  edge?: (() => number) | null;
  /**
   * The currently-active capture windows to light on the map, re-read every frame (T-445). The
   * preview passes none, because it does not read the live edge and a segment placed from a fixed
   * historical instant would be a live claim with no live evidence.
   */
  windows?: (() => readonly ActiveWindow[]) | null;
  /**
   * Extra stroked marks to draw **inside each pane**, re-derived on every frame from the state they
   * describe — signal boxes and selections (T-445, `./marks.ts`).
   *
   * Per frame, not per poll: a per-poll layout against a per-frame scroll is T-388's box-jump, and
   * the whole reason the boxes move here is that the mapping they are placed through is the *same*
   * `toClip` the tiles are placed through. Strokes only: `overlay.ts` has no sampler and no ramp, so
   * nothing drawn here can tint a measurement.
   */
  marks?: ((pane: PaneView, edgeNs: number) => readonly OverlayQuad[]) | null;
  /**
   * **The instantaneous spectrum trace** (T-457): quads for the strip carved off the top of each
   * pane. Like `marks`, a function called per frame — but handed the `PaneReport` the data pass just
   * produced as well, so the reduction behind it is at the level the picture beneath it was drawn at.
   *
   * The historical preview passes none. A trace of *this frame* needs a stream, and this host is
   * deliberately the one that reads no live edge.
   */
  trace?: ((pane: PaneView, edgeNs: number, report: PaneReport, strip: PaneRect) => readonly OverlayQuad[]) | null;
  /** Height of that strip, device px. 0 draws no trace and gives the space back to the pane. */
  tracePx?: number;
}

/**
 * The mounted surface: one `SurfaceView`, one fixed edge, and the gestures that move viewports.
 *
 * **Every gesture is a view change.** Pan and zoom call `PaneModel`/`Minimap` methods and nothing
 * else; there is no path from a pointer event to a device route, and `./retune.ts` is not imported.
 */
export class SurfacePreview {
  readonly view: SurfaceView;
  readonly probe: SurfaceProbe;
  /** The pane gestures apply to: the last one pointed at. */
  activePane: string;
  lastFrame: SurfaceFrame | null = null;
  private readonly canvas: HTMLCanvasElement;
  private raf = 0;
  private disposed = false;
  private readonly edgeFn: (() => number) | null;
  private readonly windowsFn: (() => readonly ActiveWindow[]) | null;
  /** The newest edge seen. A live edge must never go backwards under the boxes placed on it. */
  private edgeSeen: number;

  constructor(opts: PreviewOptions) {
    const { probe } = opts;
    this.canvas = opts.canvas;
    this.probe = probe;
    this.edgeFn = opts.edge ?? null;
    this.windowsFn = opts.windows ?? null;
    this.edgeSeen = probe.origin.edgeNs;
    this.view = new SurfaceView({
      canvas: opts.canvas,
      lattice: probe.lattice,
      bounds: probe.origin.bounds,
      cache: (tex) => new TileCache<TilePlanes>(tex, (a: TileAddr, signal?: AbortSignal) =>
        fetchTile(a, opts.token, opts.fetchFn, signal)),
      minimapPx: opts.minimapPx ?? 120,
      chrome: opts.chrome ?? null,
      freq: probe.opening.freq,
      spanNs: probe.opening.spanNs,
      marks: opts.marks ?? null,
      trace: opts.trace ?? null,
      tracePx: opts.tracePx ?? 0,
    });
    // **Freeze everything at open, unless an edge was reported in.** A following viewport borrows
    // the growing edge; without one there is nothing to borrow, so nothing follows (T-450's
    // historical preview). `pause` is a coordinate change (T-347/T-442), so this costs no frame and
    // no jump. With a live edge the first pane stays following and the map follows too — "live" is
    // then just the finest growing edge of this same surface (docs/16 §8.1), not a second mode.
    this.activePane = this.view.panes.list()[0].id;
    if (!this.edgeFn) {
      this.view.panes.pause(this.activePane, probe.origin.edgeNs);
      this.view.panes.goTo(this.activePane, probe.opening.centerNs);
    }
    this.view.minimap.setFollowing(!!this.edgeFn);
    // The map opens on the whole surface — it is the thing that says where the opened pane sits in
    // a mostly-grey world, which is half the answer to the empty-screen problem.
    this.view.minimap.setFreq(
      (probe.origin.bounds.f0Hz + probe.origin.bounds.f1Hz) / 2,
      probe.origin.bounds.f1Hz - probe.origin.bounds.f0Hz,
    );
  }

  /**
   * The newest instant this surface draws.
   *
   * Without an `edge` supplier it is the one `probeSurface` resolved and it never advances (the
   * historical preview). With one it is whatever capture has reported, clamped monotone: a
   * re-plumbed stream's first rows can repeat, and an edge that went backwards would drag every
   * following pane and every box on it backwards with it.
   */
  get edgeNs(): number {
    if (!this.edgeFn) return this.probe.origin.edgeNs;
    const v = this.edgeFn();
    if (Number.isFinite(v) && v > this.edgeSeen) this.edgeSeen = v;
    return this.edgeSeen;
  }
  get bounds(): Box { return this.probe.origin.bounds; }

  /** Draw one frame. With no `windows` supplier the list is empty — the preview reads no live edge,
   * and a lit segment placed from a fixed historical instant would be a live claim with no live
   * evidence. */
  frame(): SurfaceFrame {
    this.lastFrame = this.view.frame(this.edgeNs, this.windowsFn?.() ?? []);
    return this.lastFrame;
  }

  /** Match the drawing buffer to the element's CSS box at the device's pixel ratio. */
  resize(cssW: number, cssH: number, dpr = 1): boolean {
    const w = Math.max(1, Math.round(cssW * dpr)), h = Math.max(1, Math.round(cssH * dpr));
    if (this.canvas.width === w && this.canvas.height === h) return false;
    this.canvas.width = w;
    this.canvas.height = h;
    return true;
  }

  /** Run until [[dispose]]. Every overlay re-lays-out per frame, which is the T-388 rule. */
  start(): void {
    const tick = () => {
      if (this.disposed) return;
      this.frame();
      this.raf = requestAnimationFrame(tick);
    };
    tick();
  }

  dispose(): void {
    this.disposed = true;
    if (this.raf) cancelAnimationFrame(this.raf);
    this.view.dispose();
  }

  // ——— hit testing ———

  /** Is this point in the minimap strip rather than in a pane? */
  onMap(p: GlPoint): boolean {
    const r = this.lastFrame?.mapRect;
    return !!r && p.x >= r.x && p.x < r.x + r.w && p.y >= r.y && p.y < r.y + r.h;
  }

  /** The pane under a point, or null. See [[paneAtPoint]] for what "under" includes. */
  paneAt(p: GlPoint): string | null {
    return paneAtPoint(this.lastFrame, this.view.minimap.id, p);
  }

  private rectOf(id: string): { x: number; y: number; w: number; h: number } | null {
    return this.lastFrame?.views.find((v) => v.id === id)?.rect ?? null;
  }

  // ——— gestures. All of these are arithmetic on view state. ———

  /**
   * Drag: the data under the pointer stays under the pointer.
   *
   * `dx`/`dy` are in **drawing-buffer** pixels with y up (GL convention), which is why the time
   * term has no sign flip: moving the pointer toward the top of the screen is `dy > 0`, and the top
   * of a pane is the newest row, so the window walks backward in time by the same fraction.
   */
  drag(id: string, dx: number, dy: number): void {
    const r = this.rectOf(id);
    const box = this.lastFrame?.views.find((v) => v.id === id)?.box;
    if (!r || !box) return;
    this.view.panes.panFreq(id, -(dx / Math.max(1, r.w)) * (box.f1Hz - box.f0Hz));
    this.view.panes.panTime(id, -(dy / Math.max(1, r.h)) * (box.t1Ns - box.t0Ns));
  }

  /** The same for the map, which is one pane's worth of the same model. */
  dragMap(dx: number, dy: number): void {
    const r = this.lastFrame?.mapRect, box = this.lastFrame?.mapBox;
    if (!r || !box) return;
    this.view.minimap.panFreq(-(dx / Math.max(1, r.w)) * (box.f1Hz - box.f0Hz));
    this.view.minimap.panTime(-(dy / Math.max(1, r.h)) * (box.t1Ns - box.t0Ns));
  }

  /**
   * Wheel over a pane, **anchored so the cell under the pointer stays put** — on whichever axes
   * `wheelAxes` named, which after T-456 is both of them for a plain wheel.
   *
   * The two axes are still moved by **two separate calls with two separate anchors**, and each
   * clamps and resolves its level on its own: a uniform gesture is one factor applied twice, never
   * one level applied to two axes. That is the distinction T-434's de-welding rests on, and it is
   * why `factor` may zoom frequency while time sits clamped at the record's floor — a legitimate
   * outcome the chrome states rather than hides.
   */
  wheel(id: string, p: GlPoint, factor: number, axes: { freq: boolean; time: boolean }): void {
    const r = this.rectOf(id);
    if (!r) return;
    const fx = clamp01((p.x - r.x) / Math.max(1, r.w));
    // `zoomTime`'s anchor is 0 = oldest (bottom of the pane) and 1 = newest, which is already the
    // GL y direction — so no flip here either.
    const ty = clamp01((p.y - r.y) / Math.max(1, r.h));
    if (axes.freq) this.view.panes.zoomFreq(id, factor, fx);
    if (axes.time) this.view.panes.zoomTime(id, factor, ty);
  }

  wheelMap(p: GlPoint, factor: number, axes: { freq: boolean; time: boolean }): void {
    const r = this.lastFrame?.mapRect;
    if (!r) return;
    if (axes.freq) this.view.minimap.zoomFreq(factor, clamp01((p.x - r.x) / Math.max(1, r.w)));
    if (axes.time) this.view.minimap.zoomTime(factor, clamp01((p.y - r.y) / Math.max(1, r.h)));
  }

  /** Send the active pane to a point on the map, keeping its spans. A view move, not a retune. */
  goToOnMap(p: GlPoint): void {
    const r = this.lastFrame?.mapRect;
    if (!r) return;
    const at = this.view.minimap.locate(
      (p.x - r.x) / Math.max(1, r.w),
      (p.y - r.y) / Math.max(1, r.h),
      this.edgeNs,
    );
    const pane = this.view.panes.get(this.activePane);
    if (!pane) return;
    this.view.panes.setFreq(this.activePane, at.hz, pane.freq.spanHz);
    this.view.panes.goTo(this.activePane, at.ns);
  }

  // ——— the two orientation actions ———

  /** Put the active pane back on observed coverage — the opening window, recomputed from the same
   * census. What a user reaches for after panning into the grey and losing the survey. */
  fitToCoverage(): void {
    const o = this.probe.opening;
    this.setWindow(this.activePane, o.freq.centerHz, o.freq.spanHz, o.centerNs, o.spanNs);
  }

  /** Zoom the active pane out to the whole surface. */
  fitToSurface(): void {
    const b = this.bounds;
    this.setWindow(
      this.activePane, (b.f0Hz + b.f1Hz) / 2, b.f1Hz - b.f0Hz,
      (b.t0Ns + b.t1Ns) / 2, b.t1Ns - b.t0Ns,
    );
  }

  /**
   * Put a pane on an absolute window. Span first, then centre: `PaneModel.normalise` clamps the
   * centre against the span it is being given, so setting the centre against the *old* span can
   * land it somewhere the new span would never have allowed.
   */
  private setWindow(id: string, centerHz: number, spanHz: number, centerNs: number, spanNs: number): void {
    const p = this.view.panes.get(id);
    if (!p) return;
    this.view.panes.setFreq(id, centerHz, spanHz);
    // `zoomTime` is the only span control the pane model offers, and at anchor 0.5 it holds the
    // centre — so this is a span change and the `goTo` after it is the centre change.
    this.view.panes.zoomTime(id, spanNs / Math.max(1, p.time.spanNs), 0.5);
    this.view.panes.goTo(id, centerNs);
  }

  split(dir: "columns" | "rows"): void {
    const id = this.view.panes.split(this.activePane, dir);
    if (id) this.activePane = id;
  }

  closeActive(): void {
    if (!this.view.panes.close(this.activePane)) return;
    this.activePane = this.view.panes.list()[0].id;
  }
}

const clamp01 = (v: number) => (Number.isFinite(v) ? Math.min(1, Math.max(0, v)) : 0);
