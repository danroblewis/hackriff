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
import type { PaneView, TilePlanes } from "./surface";
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

/** A pointer position in the drawing buffer, GL convention: origin **bottom-left**. */
interface GlPoint { readonly x: number; readonly y: number }

/** Wheel steps to a zoom factor. `> 1` zooms out, matching `PaneModel`'s own convention. */
export function zoomFactor(deltaY: number, deltaMode = 0): number {
  // `deltaMode` 1 is lines and 2 is pages; normalise to the pixel scale a trackpad reports.
  const px = deltaY * (deltaMode === 1 ? 16 : deltaMode === 2 ? 400 : 1);
  return Math.min(4, Math.max(0.25, Math.exp(px * 0.0015)));
}

/** Which axes a wheel gesture zooms. Stated as data so the page can print the same table it obeys. */
export function wheelAxes(e: { shiftKey?: boolean; altKey?: boolean }): { freq: boolean; time: boolean } {
  if (e.altKey) return { freq: true, time: true };
  if (e.shiftKey) return { freq: true, time: false };
  return { freq: false, time: true };
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

  /** The pane under a point, or null. */
  paneAt(p: GlPoint): string | null {
    for (const v of this.lastFrame?.views ?? []) {
      if (v.id === this.view.minimap.id) continue;
      const r = v.rect;
      if (p.x >= r.x && p.x < r.x + r.w && p.y >= r.y && p.y < r.y + r.h) return v.id;
    }
    return null;
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
   * Wheel over a pane, anchored so the cell under the pointer stays put.
   *
   * The two axes zoom **independently** — that is the whole point of T-434's de-welding, and a
   * wheel that always moved both would hide it. `wheelAxes` names which.
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
