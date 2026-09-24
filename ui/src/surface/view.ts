// The surface, assembled (T-443): panes, the minimap, the overlay pass and the chrome, driven by
// one `frame()`.
//
// **Everything time-varying is laid out on every frame, from the state it describes.** The panes'
// boxes come from `PaneModel.list()`/`views()`, the minimap's rectangles come from those same pane
// states through the renderer's own `toClip`, and the chrome's levels come from the `PaneReport[]`
// the renderer just returned. There is no change event to subscribe to and nothing is cached
// between frames except tiles — because re-laying-out overlays on a poll cadence while the canvas
// scrolls per frame is **T-388's box-jump**, and it is the reason T-442 deliberately left
// `rects()`/`list()` event-free.
//
// **One render call.** The minimap's `PaneView` is appended to the panes' and handed to the same
// `Surface.render`, so it shares the context, the LRU, the ramp, the display range and the grey
// rule. It has **no fetch path of its own** — nothing in ui/src/surface/minimap.ts or this file
// requests a tile; `TileCache` is the only thing that does, for every viewport alike.
//
// **The order is data, then overlays, then chrome**, and the overlay pass is a different program
// that runs after the tile draws have been submitted. That is what keeps the T-437 trap — a
// translucent wash drawn over the point being measured — out of reach rather than merely
// discouraged.
//
// The active-capture list is **passed in**, not fetched here: it is `GET /api/navigation`'s
// `windows`, read by the one reader in ui/src/navigators.ts. This file decides nothing about the
// radio; a pan is a pan (retune is T-444).

import type { ActiveWindow } from "../navigators";
import { SurfaceChrome, readoutOf, type Readout, type RowActionFor, type WidthActionsFor } from "./chrome";
import type { Box, Lattice, LatticeSet } from "./lattice";
import {
  Minimap, liveSegmentQuads, paneOutlineQuads,
  type OverlayQuad, type OverlayStyle,
} from "./minimap";
import { OverlayPass } from "./overlay";
import { TracePass } from "./tracepass";
import type { TracePath } from "./trace";
import { PaneModel, levelDivergenceNote, paneStatuses, type FreqWindow, type PaneStatus } from "./panes";
import { Surface, type PaneRect, type PaneReport, type PaneView, type SurfaceOptions, type TilePlanes } from "./surface";
import type { TileCache, TileTextures } from "./tilecache";
import { rulerLabel } from "./ticks";
import { HudAxes, hudLabels, hudTickQuads, paneRuler, type HudLabel, type PaneRuler } from "./hud";

export interface SurfaceViewOptions {
  canvas: HTMLCanvasElement;
  /** The **detail** lattice. Also what the pane model and the minimap snap their windows to. */
  lattice: Lattice;
  /** Both tiers (T-505). Omitted, every viewport is drawn from `lattice` alone. */
  lattices?: LatticeSet;
  /** The surface's extent: the device-available range and the retained window. Backend numbers. */
  bounds: Box;
  cache: TileCache<TilePlanes> | ((tex: TileTextures<TilePlanes>) => TileCache<TilePlanes>);
  /** Height of the minimap strip along the bottom of the same canvas, device px. 0 hides it. */
  minimapPx?: number;
  /** Where the per-pane level readout is mounted. Without one the statuses are still returned. */
  chrome?: HTMLElement | null;
  /**
   * A **per-viewport control** the host puts on each pane's chrome row, re-asked every frame
   * (T-476). Strings and an enabled bit only — see `chrome.ts` for why this slot is anonymous.
   *
   * Per frame rather than per poll, for the same reason the marks are: the sentence on the control
   * describes the viewport's *current* window, and a label produced on a 1 s poll while the window
   * moves per frame is the label naming somewhere the pane no longer is.
   */
  chromeAction?: RowActionFor | null;
  /** The press. A discrete click on that row's button; nothing here reads a pointer stream. */
  onChromeAction?: ((paneId: string) => void) | null;
  /** Capture-width presets (T-496), re-asked every frame for the same reason `chromeAction` is. */
  widthActions?: WidthActionsFor | null;
  /** The press, naming which preset (its opaque `key`). A discrete click; nothing here reads a
   * pointer stream. */
  onWidthAction?: ((paneId: string, key: string) => void) | null;
  /** Draw the overlay pass. A user preference — **not** what keeps the data pass untinted. */
  overlays?: boolean;
  overlayStyle?: OverlayStyle;
  surface?: SurfaceOptions;
  /** The first pane's opening window, and whose coverage decides its grey. */
  freq?: FreqWindow;
  spanNs?: number;
  device?: string;
  /**
   * Extra stroked marks drawn **inside each pane** — the signal boxes and selections the old
   * waterfall's DOM overlay layer used to carry (T-445, `./marks.ts`).
   *
   * It is a *function called per frame*, not a list set on a poll, and that is the whole point:
   * T-388's box-jump was a per-poll DOM layout racing a per-frame scroll. Here the caller is handed
   * the very `PaneView` the data pass was just given, so a box and the energy under it are placed
   * by the same `toClip` on the same frame and **cannot** use two mappings.
   */
  marks?: ((pane: PaneView, edgeNs: number) => readonly OverlayQuad[]) | null;
  /**
   * **The instantaneous spectrum trace** (T-457, `./trace.ts`): quads for the strip carved off the
   * top of each pane, in that strip's own clip space.
   *
   * It is given the pane's `PaneView` *and the `PaneReport` the data pass just produced*, because a
   * trace reduced from a different pyramid level than the picture it sits above would be a second
   * opinion about one window — the same reason the chrome states the level it was **drawn** with.
   *
   * The strip is taken out of the pane's **rectangle**, never painted over it: an overlay covering
   * the newest rows would make "the top of the pane is the newest row" false, which is the one thing
   * every mark on this surface is placed through.
   *
   * Since T-475 it returns [[TracePath]]s for `./tracepass.ts` rather than overlay quads, because the
   * series are coloured **by amplitude from the one ramp** and `overlay.ts`'s program is — and stays
   * — incapable of a measurement colour. That is a deliberate widening, not a loosening: see the
   * header of `./tracepass.ts` for the two properties it re-establishes structurally.
   */
  trace?: ((pane: PaneView, edgeNs: number, report: PaneReport, strip: PaneRect) => readonly TracePath[]) | null;
  /** Height of that strip, device px. 0 (the default) draws no trace and takes no space. */
  tracePx?: number;
  /**
   * **HUD axes** (T-805, `./hud.ts`): a frequency ruler along each pane's bottom edge and a time
   * ruler down its left, re-derived every frame from the box the pane was drawn with and the cell
   * its `PaneStatus` reports. `true` draws the ticks (band 0, the overlay program); an element also
   * receives the labels (band 2, DOM). Off by default.
   */
  hudAxes?: boolean;
  hud?: HTMLElement | null;
  /** The chrome's fade, `0..1`, asked every frame; the ticks' ink is multiplied by it. Default 1. */
  hudAlpha?: (() => number) | null;
  /**
   * **Band-1 DOM marks** (T-809, `./pins.ts`): called once per frame, after the overlays and the
   * HUD, with the SAME pane views the data pass was handed — so a DOM mark is placed by the very
   * box and rect the tiles were, on the same frame (the one-shared-time-axis rule), never on a
   * poll. `canvasHpx`/`dpr` convert the GL-convention rects to CSS px, as the HUD labels do.
   */
  dom?: ((panes: readonly PaneView[], edgeNs: number, canvasHpx: number, dpr: number) => void) | null;
}

/** One pane's trace strip this frame: where it is, and the window it is a trace across. */
export interface TraceRect {
  readonly id: string;
  readonly rect: PaneRect;
  readonly box: Box;
}

/** What one frame did — enough to assert on without a framebuffer. */
export interface SurfaceFrame {
  readonly edgeNs: number;
  readonly views: readonly PaneView[];
  readonly reports: readonly PaneReport[];
  readonly statuses: readonly PaneStatus[];
  readonly note: string | null;
  /** Overlay geometry for this frame, whether or not it was drawn. */
  readonly quads: readonly OverlayQuad[];
  readonly overlaysDrawn: number;
  readonly mapRect: PaneRect | null;
  readonly mapBox: Box | null;
  /** The trace strips drawn this frame, one per pane; empty when no trace is configured. */
  readonly traces: readonly TraceRect[];
  readonly readout: Readout;
  /** The HUD rulers laid out this frame, one per pane; empty when HUD axes are off (T-805). */
  readonly rulers: readonly PaneRuler[];
  /** Their tick strokes, as submitted to the overlay pass. */
  readonly hudQuads: readonly OverlayQuad[];
}

export class SurfaceView {
  readonly surface: Surface;
  readonly panes: PaneModel;
  readonly minimap: Minimap;
  /** Draw the overlay pass this frame. */
  overlays: boolean;
  minimapPx: number;
  private readonly overlay: OverlayPass;
  /** The trace's own program: vertex colour from the one ramp, no sampler. See `./tracepass.ts`. */
  private readonly tracePass: TracePass;
  private readonly chrome: SurfaceChrome | null;
  /** Per-viewport control, re-asked every frame (T-476). Null when the host offers none. */
  private readonly chromeAction: RowActionFor | null;
  /** Per-viewport width presets, re-asked every frame (T-496). Null when the host offers none. */
  private readonly widthActions: WidthActionsFor | null;
  private readonly overlayStyle: OverlayStyle;
  private readonly canvas: HTMLCanvasElement;
  /** Per-pane marks, re-derived every frame. See [[SurfaceViewOptions.marks]]. */
  marks: ((pane: PaneView, edgeNs: number) => readonly OverlayQuad[]) | null;
  /** Per-pane spectrum trace, re-derived every frame. See [[SurfaceViewOptions.trace]]. */
  trace: ((pane: PaneView, edgeNs: number, report: PaneReport, strip: PaneRect) => readonly TracePath[]) | null;
  /** Height of the trace strip above each pane, device px. 0 hides it and returns the space. */
  tracePx: number;
  /** Draw the HUD rulers (T-805). */
  hudAxes: boolean;
  private readonly hud: HudAxes | null;
  private readonly hudAlpha: (() => number) | null;
  private readonly dom: ((panes: readonly PaneView[], edgeNs: number, canvasHpx: number, dpr: number) => void) | null;

  constructor(opts: SurfaceViewOptions) {
    this.marks = opts.marks ?? null;
    this.trace = opts.trace ?? null;
    this.tracePx = opts.tracePx ?? 0;
    this.hudAxes = opts.hudAxes ?? !!opts.hud;
    this.hud = opts.hud ? new HudAxes(opts.hud) : null;
    this.hudAlpha = opts.hudAlpha ?? null;
    this.dom = opts.dom ?? null;
    this.canvas = opts.canvas;
    this.surface = new Surface(opts.canvas, opts.lattices ?? opts.lattice, opts.cache, opts.surface ?? {});
    this.overlay = new OverlayPass(this.surface.gl);
    this.tracePass = new TracePass(this.surface.gl);
    this.panes = new PaneModel({
      bounds: opts.bounds, lattice: opts.lattice, freq: opts.freq, spanNs: opts.spanNs, device: opts.device,
    });
    this.minimap = new Minimap({ bounds: opts.bounds, lattice: opts.lattice });
    this.minimapPx = opts.minimapPx ?? 96;
    this.overlays = opts.overlays ?? true;
    this.overlayStyle = opts.overlayStyle ?? {};
    this.chromeAction = opts.chromeAction ?? null;
    this.widthActions = opts.widthActions ?? null;
    this.chrome = opts.chrome
      ? new SurfaceChrome(opts.chrome, opts.onChromeAction ?? null, opts.onWidthAction ?? null)
      : null;
  }

  /** The surface's extent moved: a retention window that has rolled, or a new front end's range. */
  setBounds(b: Box): void {
    this.panes.setBounds(b);
    this.minimap.setBounds(b);
  }

  /**
   * Draw one frame at `edgeNs` — where capture has got to, **reported in**, never controlled here.
   *
   * `windows` is the backend's list of currently-active capture windows; `[]` means none was
   * reported, and then no segment is lit.
   */
  frame(edgeNs: number, windows: readonly ActiveWindow[] = []): SurfaceFrame {
    const w = this.canvas.width, hPx = this.canvas.height;
    // The minimap takes a strip along the bottom of the SAME canvas — it is a viewport on this
    // surface, so it is laid out in this surface's pixels, not in a widget of its own.
    // Capped at half the surface: the map is where you see *where the panes are*, so it may not
    // become the thing you are looking at.
    const mapH = Math.max(0, Math.min(Math.floor(this.minimapPx), Math.floor(hPx / 2)));
    const paneH = Math.max(1, hPx - mapH);
    this.panes.setViewport(w, paneH);
    // The map is laid out in the same pixels, and it needs them for the same reason the panes do:
    // T-486's dead zone is a number of *device pixels*, so a viewport that does not know its own
    // height has no pixel to measure in (and, per `PaneModel.zoneNs`, no dead zone at all).
    this.minimap.setViewport(w, mapH);
    // T-457's strip, taken off the **top** of each pane's rectangle rather than painted over it.
    // Capped at a third of the pane area for the same reason the map is capped at half: the thing
    // you are looking at must not become the thing beside it. Per pane, so a split shows one trace
    // per frequency window instead of one strip trying to be true of two.
    const traceH = this.trace ? Math.max(0, Math.min(Math.floor(this.tracePx), Math.floor(paneH / 3))) : 0;
    const paneViews = this.panes.views(edgeNs, w, paneH)
      .map((v) => ({ ...v, rect: { ...v.rect, y: v.rect.y + mapH, h: Math.max(1, v.rect.h - traceH) } }));
    const mapRect: PaneRect | null = mapH > 0 ? { x: 0, y: 0, w, h: mapH } : null;
    // `scales: false` (T-528): the map is another viewport onto the same surface and is drawn with
    // the same ramp and the same range — but it is a viewport over the WHOLE surface, so it may not
    // be what a viewport-measured range is measured over. See `Surface`'s `PaneView.scales`.
    const mapView = mapRect ? { ...this.minimap.view(mapRect, edgeNs), scales: false } : null;
    const views = mapView ? [...paneViews, mapView] : paneViews;

    // 1. the data pass: one call, one context, one LRU, one ramp, one display range.
    const reports = this.surface.render(views);

    // 2. the overlays: geometry re-derived from pane state THIS frame, drawn by a different program
    //    afterwards. Computed even when not drawn, so a caller can place the same marks in the DOM.
    const mapQuads = mapView && mapRect
      ? [
        ...paneOutlineQuads(this.panes.list(), edgeNs, mapView.box, mapRect, this.overlayStyle),
        ...liveSegmentQuads(windows, edgeNs, mapView.box, mapRect, this.overlayStyle),
      ]
      : [];
    // The in-pane marks: signal boxes and selections, derived from the **same** `PaneView` the data
    // pass was handed, this frame. One mapping, one frame — never a poll's layout over a scroll.
    const paneQuads: { rect: PaneRect; quads: readonly OverlayQuad[] }[] = [];
    if (this.marks) {
      for (const v of paneViews) {
        const q = this.marks(v, edgeNs);
        if (q.length) paneQuads.push({ rect: v.rect, quads: q });
      }
    }
    // 2b. the trace strips: the same overlay program, into the rectangle carved off each pane's top.
    //     Scissored to that strip, so a trace cannot reach the measurement it is a trace of, and
    //     handed the report the data pass produced so its reduction is at the level that was drawn.
    const traces: TraceRect[] = [];
    const tracePaths: { rect: PaneRect; paths: readonly TracePath[] }[] = [];
    if (this.trace && traceH > 0) {
      const byId = new Map(reports.map((r) => [r.id, r]));
      for (const v of paneViews) {
        const report = byId.get(v.id);
        if (!report) continue;
        const rect: PaneRect = { x: v.rect.x, y: v.rect.y + v.rect.h, w: v.rect.w, h: traceH };
        traces.push({ id: v.id, rect, box: v.box });
        const q = this.trace(v, edgeNs, report, rect);
        if (q.length) tracePaths.push({ rect, paths: q });
      }
    }
    let overlaysDrawn = 0;
    if (this.overlays) {
      if (mapRect) overlaysDrawn += this.overlay.draw(mapRect, mapQuads);
      for (const p of paneQuads) overlaysDrawn += this.overlay.draw(p.rect, p.quads);
    }
    // The trace is not an overlay preference: it is a series in a rectangle of its own, so it draws
    // whether or not the map's outlines and the in-pane marks are switched off.
    for (const p of tracePaths) this.tracePass.draw(p.rect, p.paths);
    const quads = paneQuads.length ? [...mapQuads, ...paneQuads.flatMap((p) => p.quads)] : mapQuads;

    // 3. the chrome: the level each viewport resolved to, from the report it was actually drawn
    //    with — the minimap among them, because it is another viewport.
    const rects = new Map(views.map((v) => [v.id, v.rect]));
    const states = mapView ? [...this.panes.list(), this.minimap.state()] : this.panes.list();
    const statuses = paneStatuses(states, reports, edgeNs, rects, (id) =>
      id === this.minimap.id ? this.minimap.following : this.panes.isFollowing(id));
    // T-459: the ruler line, from the SAME box `views` was just drawn from and the SAME
    // `(cellHz, cellS)` `statuses` just reported — never a second read of the pane's window, which
    // is the drift family §8.5a closed.
    const boxById = new Map(views.map((v) => [v.id, v.box]));
    const statusById = new Map(statuses.map((s) => [s.id, s]));
    const rulerFor = (id: string): string | null => {
      const box = boxById.get(id), s = statusById.get(id);
      return box && s ? rulerLabel(box.f0Hz, box.f1Hz, box.t0Ns, box.t1Ns, s.cellHz, s.cellS, edgeNs) : null;
    };
    const readout = readoutOf(
      statuses, mapView ? this.minimap.id : null, this.chromeAction, rulerFor, this.widthActions,
    );
    this.chrome?.update(readout);

    // 4. the HUD rulers (T-805): per pane, from the SAME box and the SAME `(cellHz, cellS)` the
    //    readout above was just built from — ticks through the overlay program (band 0), labels into
    //    the DOM layer (band 2), both in this frame. The minimap has none: it is where you see where
    //    the panes are, not a place to read a value off.
    const rulers: PaneRuler[] = [];
    const hudQuads: OverlayQuad[] = [];
    if (this.hudAxes) {
      const cssW = this.canvas.clientWidth;
      const dpr = cssW > 0 ? w / cssW : 1;
      const alpha = this.hudAlpha ? this.hudAlpha() : 1;
      const labels: HudLabel[] = [];
      for (const v of paneViews) {
        const s = statusById.get(v.id);
        if (!s) continue;
        const r = paneRuler(v.id, v.box, v.rect, s.cellHz, s.cellS, edgeNs, dpr);
        rulers.push(r);
        const q = hudTickQuads(r, { alpha, majorPx: 10 * dpr, minorPx: 5 * dpr, thickPx: Math.max(1, Math.round(dpr)) });
        if (q.length) { this.overlay.draw(v.rect, q); hudQuads.push(...q); }
        labels.push(...hudLabels(r, hPx, dpr));
      }
      this.hud?.update(labels);
    }

    // 5. band-1 DOM marks (T-809's pins): the same pane views, this frame.
    if (this.dom) {
      const cssW = this.canvas.clientWidth;
      this.dom(paneViews, edgeNs, hPx, cssW > 0 ? w / cssW : 1);
    }

    return {
      edgeNs, views, reports, statuses, note: levelDivergenceNote(statuses),
      quads, overlaysDrawn, mapRect, mapBox: mapView?.box ?? null, traces, readout, rulers, hudQuads,
    };
  }

  dispose(): void {
    this.chrome?.dispose();
    this.hud?.dispose();
    this.surface.dispose();
  }
}
