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
import { SurfaceChrome, readoutOf, type Readout } from "./chrome";
import type { Box, Lattice } from "./lattice";
import {
  Minimap, liveSegmentQuads, paneOutlineQuads,
  type OverlayQuad, type OverlayStyle,
} from "./minimap";
import { OverlayPass } from "./overlay";
import { PaneModel, levelDivergenceNote, paneStatuses, type FreqWindow, type PaneStatus } from "./panes";
import { Surface, type PaneRect, type PaneReport, type PaneView, type SurfaceOptions, type TilePlanes } from "./surface";
import type { TileCache, TileTextures } from "./tilecache";

export interface SurfaceViewOptions {
  canvas: HTMLCanvasElement;
  lattice: Lattice;
  /** The surface's extent: the device-available range and the retained window. Backend numbers. */
  bounds: Box;
  cache: TileCache<TilePlanes> | ((tex: TileTextures<TilePlanes>) => TileCache<TilePlanes>);
  /** Height of the minimap strip along the bottom of the same canvas, device px. 0 hides it. */
  minimapPx?: number;
  /** Where the per-pane level readout is mounted. Without one the statuses are still returned. */
  chrome?: HTMLElement | null;
  /** Draw the overlay pass. A user preference — **not** what keeps the data pass untinted. */
  overlays?: boolean;
  overlayStyle?: OverlayStyle;
  surface?: SurfaceOptions;
  /** The first pane's opening window, and whose coverage decides its grey. */
  freq?: FreqWindow;
  spanNs?: number;
  device?: string;
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
  readonly readout: Readout;
}

export class SurfaceView {
  readonly surface: Surface;
  readonly panes: PaneModel;
  readonly minimap: Minimap;
  /** Draw the overlay pass this frame. */
  overlays: boolean;
  minimapPx: number;
  private readonly overlay: OverlayPass;
  private readonly chrome: SurfaceChrome | null;
  private readonly overlayStyle: OverlayStyle;
  private readonly canvas: HTMLCanvasElement;

  constructor(opts: SurfaceViewOptions) {
    this.canvas = opts.canvas;
    this.surface = new Surface(opts.canvas, opts.lattice, opts.cache, opts.surface ?? {});
    this.overlay = new OverlayPass(this.surface.gl);
    this.panes = new PaneModel({
      bounds: opts.bounds, lattice: opts.lattice, freq: opts.freq, spanNs: opts.spanNs, device: opts.device,
    });
    this.minimap = new Minimap({ bounds: opts.bounds, lattice: opts.lattice });
    this.minimapPx = opts.minimapPx ?? 96;
    this.overlays = opts.overlays ?? true;
    this.overlayStyle = opts.overlayStyle ?? {};
    this.chrome = opts.chrome ? new SurfaceChrome(opts.chrome) : null;
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
    const paneViews = this.panes.views(edgeNs, w, paneH)
      .map((v) => ({ ...v, rect: { ...v.rect, y: v.rect.y + mapH } }));
    const mapRect: PaneRect | null = mapH > 0 ? { x: 0, y: 0, w, h: mapH } : null;
    const mapView = mapRect ? this.minimap.view(mapRect, edgeNs) : null;
    const views = mapView ? [...paneViews, mapView] : paneViews;

    // 1. the data pass: one call, one context, one LRU, one ramp, one display range.
    const reports = this.surface.render(views);

    // 2. the overlays: geometry re-derived from pane state THIS frame, drawn by a different program
    //    afterwards. Computed even when not drawn, so a caller can place the same marks in the DOM.
    const quads = mapView && mapRect
      ? [
        ...paneOutlineQuads(this.panes.list(), edgeNs, mapView.box, mapRect, this.overlayStyle),
        ...liveSegmentQuads(windows, edgeNs, mapView.box, mapRect, this.overlayStyle),
      ]
      : [];
    const overlaysDrawn = this.overlays && mapRect ? this.overlay.draw(mapRect, quads) : 0;

    // 3. the chrome: the level each viewport resolved to, from the report it was actually drawn
    //    with — the minimap among them, because it is another viewport.
    const rects = new Map(views.map((v) => [v.id, v.rect]));
    const states = mapView ? [...this.panes.list(), this.minimap.state()] : this.panes.list();
    const statuses = paneStatuses(states, reports, this.surface.lat, edgeNs, rects);
    const readout = readoutOf(statuses, mapView ? this.minimap.id : null);
    this.chrome?.update(readout);

    return {
      edgeNs, views, reports, statuses, note: levelDivergenceNote(statuses),
      quads, overlaysDrawn, mapRect, mapBox: mapView?.box ?? null, readout,
    };
  }

  dispose(): void {
    this.chrome?.dispose();
    this.surface.dispose();
  }
}
