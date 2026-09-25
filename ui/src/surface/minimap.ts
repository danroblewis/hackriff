// The zoomable minimap (T-443, docs/16 §8.3) — **another viewport, not a fifth widget**.
//
// That sentence is the whole design, and it is worth saying why. The old frequency navigator showed
// "every currently-active capture window as a lit segment" and the old time navigator showed a
// compressed history of the selected band, and **keeping either of them in step with the main view
// is what kept failing**: sliver-of-data (T-420), box-jump (T-388), fill and resolution
// (T-397/T-411), colormap divergence (T-397), wheel-zoom mismatch (T-412). Every one of those was
// two implementations of one idea drifting apart. So the minimap here is a [[PaneModel]] with one
// pane, handed to the same `Surface.render` call as every other pane: it reads **the same tiles from
// the same LRU at a different level**, through the same shader and the same ramp. There is nothing
// left to keep in step.
//
// **It is nearly always at a different level, and that is stated, not hidden** (docs/16 §8.5a). The
// spike measured that two viewports at the *same* level sample bit-identical pixels — but the
// guarantee is *"same ramp, same scale, **stated level**"*, not "same picture", because a coarser
// cell is a **max over more cells**. A 6 GHz-wide minimap is essentially never at a pane's level, so
// `paneStatuses()` carries the minimap in the same list as the panes and `levelDivergenceNote()`
// explains the difference. A user who reads a legitimate difference as "the strip doesn't match the
// waterfall" files a bug otherwise.
//
// **The overlays live here as geometry, not as drawing.** The functions below return quads in the
// minimap's clip space; `ui/src/surface/overlay.ts` submits them in its own pass, after the data
// pass. The separation is not tidiness — the T-437 spike's own first minimap comparison failed
// because a **translucent pane-viewport wash had been drawn over the sample point**, which is an
// overlay tinting a measurement: exactly the defect class this surface exists to prevent, appearing
// inside the spike that was proving it. Two rules follow, and they are asserted in
// ui/test/surface-minimap.test.ts:
//
//   1. **Overlays are strokes, never washes.** Every quad below is thin in one dimension — four
//      edges of a rectangle, or a bar at the live edge. Nothing covers the interior of a region, so
//      the measurement under a pane rectangle stays readable and stays *itself*.
//   2. **Overlay colour is never measurement colour.** These marks are bright and saturated; the
//      cell marks (ui/src/surface/cellrule.ts) are dark, and the ramp is `CMAP_GLSL`'s alone. The
//      overlay program has no ramp and no cell-state uniform — it cannot express a measurement.
//
// Presentation only, per ADR-0013 §1: boxes are in the surface's own absolute coordinates, the
// active-capture list is whatever the backend reported, and nothing here fetches, decides a level,
// or commands a radio.

import { activeWindows, type ActiveWindow } from "../navigators";
import type { Box, Lattice } from "./lattice";
import { PaneModel, boxOf, type FreqWindow, type PaneState } from "./panes";
import { toClip, type PaneRect, type PaneView } from "./surface";

/**
 * **The one reader of the backend's active-capture list**, re-exported rather than re-implemented.
 *
 * The frequency navigator's rule (ui/src/navigators.ts) is that the lit segments come from
 * `GET /api/navigation`'s `windows` array and nothing else — never `frequency.current`, which is one
 * device's tuned state and not an enumeration. A second reader here would be free to disagree with
 * it, so there isn't one: a test asserts these are the *same function object*.
 */
export { activeWindows };
export type { ActiveWindow };

/**
 * Whose coverage plane answers for a window's device — **the same identity a pane's tiles are keyed
 * by** (`TileAddr.device`, `PaneState.device`, `any` = the union).
 *
 * This is what "the lit segments come from the same coverage the panes read" means concretely: a
 * pane's grey and a device's lit segment are two answers about one named front end, from the
 * backend's own enumeration, rather than two client-side notions of "which radio".
 */
export const coverageDeviceOf = (w: ActiveWindow): string => w.deviceId ?? "any";

export interface MinimapOptions {
  /** The surface's extent: the device-available range, and the retained window. Backend numbers. */
  bounds: Box;
  lattice?: Lattice;
  /** Opening frequency window. Defaults to the whole surface — the map is a map. */
  freq?: FreqWindow;
  /** Opening time span, ns. Defaults to the whole retained window. */
  spanNs?: number;
  /** Whose coverage decides the minimap's grey. `any` (the union) is the point of a map. */
  device?: string;
  id?: string;
}

/**
 * A viewport at a coarse level, over the same surface.
 *
 * Implemented as a one-pane [[PaneModel]] on purpose: pan, zoom, the zoom floor, clamping into the
 * surface's extent and the follow/freeze union all come from the pane model unchanged, so
 * "zoomable" needed no new arithmetic and cannot acquire semantics a pane does not have.
 */
export class Minimap {
  private readonly model: PaneModel;

  constructor(opts: MinimapOptions) {
    this.model = new PaneModel({
      bounds: opts.bounds,
      lattice: opts.lattice,
      freq: opts.freq,
      spanNs: opts.spanNs,
      device: opts.device ?? "any",
      idPrefix: opts.id ?? "map",
      gapPx: 0,
    });
  }

  get id(): string { return this.model.list()[0].id; }
  /** The minimap's view state, in the same shape as a pane's — it goes in the same status list. */
  state(): PaneState { return this.model.list()[0]; }
  box(edgeNs: number): Box { return boxOf(this.state(), edgeNs); }

  /** This frame's `PaneView`, for the **same** `Surface.render` call the panes go into. */
  view(rect: PaneRect, edgeNs: number): PaneView {
    const p = this.state();
    return { id: p.id, rect, box: boxOf(p, edgeNs), device: p.device };
  }

  setBounds(b: Box): void { this.model.setBounds(b); }
  panFreq(dHz: number): void { this.model.panFreq(this.id, dHz); }
  zoomFreq(factor: number, anchor = 0.5): void { this.model.zoomFreq(this.id, factor, anchor); }
  setFreq(centerHz: number, spanHz: number): void { this.model.setFreq(this.id, centerHz, spanHz); }
  panTime(dNs: number): void { this.model.panTime(this.id, dNs); }
  /** End a time gesture on the map and commit its follow/pause decision (T-486). The map is a
   * viewport, so it gets the viewport's dead zone — one answer about what a released drag means. */
  settleTime(atNs?: number): void { this.model.settleTime(this.id, atNs); }
  /** The map's own pixel height, so its dead zone is in ITS pixels rather than a pane's. */
  setViewport(wPx: number, hPx: number): void { this.model.setViewport(wPx, hPx); }
  zoomTime(factor: number, anchor = 1): void { this.model.zoomTime(this.id, factor, anchor); }
  /** The plain wheel's aspect-locked zoom (T-472). The map is a viewport, so it gets the viewport's
   * gesture — not a second, squarer-or-not opinion about what a plain wheel does. */
  zoomBoth(factor: number, anchorF = 0.5, anchorT = 1): void { this.model.zoomBoth(this.id, factor, anchorF, anchorT); }
  /** Follow the growing edge, or freeze — the minimap's pause is its time window, like a pane's. */
  setFollowing(on: boolean): void { this.model.setFollowing(this.id, on); }
  get following(): boolean { return this.model.isFollowing(this.id); }

  /**
   * Where a point on the minimap is, in the surface's own coordinates: `(0,0)` is the low-frequency
   * **oldest** corner, `(1,1)` the high-frequency newest one (time runs **up** in GL convention,
   * which is the top of the pane on screen).
   *
   * Arithmetic over this frame's box, so a click resolves against the picture that was drawn. What
   * a caller *does* with it — move a pane, offer a retune — is not decided here: a pan is a pan
   * (T-444 owns retune).
   */
  locate(xFrac: number, yFrac: number, edgeNs: number): { hz: number; ns: number } {
    const b = this.box(edgeNs);
    return {
      hz: b.f0Hz + (b.f1Hz - b.f0Hz) * clamp01(xFrac),
      ns: b.t0Ns + (b.t1Ns - b.t0Ns) * clamp01(yFrac),
    };
  }
}

const clamp01 = (v: number) => (Number.isFinite(v) ? Math.min(1, Math.max(0, v)) : 0);

// ——— overlay geometry: computed every frame, from pane state, through the renderer's own mapping ———

/** A quad in a pane's clip space, with the colour to fill it. Geometry; the GL is overlay.ts's. */
export interface OverlayQuad {
  /** `[x0, y0, x1, y1]`, the renderer's clip convention (`toClip`). */
  readonly clip: readonly [number, number, number, number];
  readonly rgba: readonly [number, number, number, number];
  /** `signal-box` and `selection-box` are T-445's in-pane marks (`./marks.ts`); `pending-region` is
   * T-458's in-flight stroke; `trace-slice` and `trace-hold` are T-457's spectrum trace
   * (`./trace.ts`), drawn in the strip above a pane; `time-rule` is T-506's full-width line at one
   * capture instant (the IQ horizon and the retention bound); `hud-tick` is T-805's HUD ruler mark
   * (`./hud.ts`) along a pane's bottom and left edges; `prior-band` is T-812's dashed band-plan
   * allocation edge/bracket (`./priors.ts`); the remaining two are the map's own.
   * All are strokes, and `overlay.ts` can draw nothing else. */
  readonly kind: "pane-outline" | "live-segment" | "signal-box" | "selection-box" | "measurement-box" | "pending-region" | "trace-slice" | "trace-hold" | "time-rule" | "hud-tick" | "artifact-link" | "prior-band";
  /** The pane id, or the device id, this mark is about. */
  readonly id: string;
}

/** The mark for "a pane is looking here". Amber: nowhere near any cell mark or any ramp stop. */
export const PANE_MARK: readonly [number, number, number, number] = [0.98, 0.82, 0.25, 0.95];
/** A pane that is following the live edge draws heavier, so the map says which one is moving. */
export const PANE_MARK_LIVE: readonly [number, number, number, number] = [1.0, 0.93, 0.55, 1.0];

/** One mark per front end, in the order the backend enumerated them. */
export const DEVICE_MARKS: readonly (readonly [number, number, number, number])[] = [
  [0.30, 0.88, 0.60, 0.95],
  [0.38, 0.68, 1.00, 0.95],
  [0.95, 0.52, 0.80, 0.95],
  [0.85, 0.85, 0.42, 0.95],
];

export interface OverlayStyle {
  /** Outline thickness, device px. */
  strokePx?: number;
  /** Live-segment thickness along the time axis, device px. */
  segmentPx?: number;
  /**
   * A **drawing** floor, device px: a 2.4 MHz capture window on a 6 GHz map is 0.04 % of the width
   * and would round away to nothing, and a window that cannot be seen is the one thing this mark
   * exists to show. Nothing reads the drawn width back as a span — `ActiveWindow` keeps the numbers
   * (the same rule `placeOn`'s `minPct` follows in ui/src/navigators.ts).
   */
  minSegmentPx?: number;
}

/**
 * A rectangle per pane, showing where that pane is looking — **re-derived from pane state on every
 * frame that calls it**.
 *
 * Deliberately taking `PaneState[]` and `edgeNs` rather than subscribing to anything: T-442 left
 * `list()`/`rects()` event-free because re-laying-out on a change event rather than per frame is
 * **T-388's box-jump** — a per-poll layout against a per-frame scroll drifts, then snaps. And the
 * placement goes through [[toClip]], the *same* function the data pass places tiles with, so a
 * rectangle and the energy under it cannot use two mappings.
 *
 * A pane wholly outside the minimap's box draws nothing; a partly-visible one draws only the edges
 * that are actually on the map, clipped to it — an edge invented at the map's border would claim a
 * boundary the pane does not have.
 */
export function paneOutlineQuads(
  panes: readonly PaneState[],
  edgeNs: number,
  mapBox: Box,
  mapRect: PaneRect,
  style: OverlayStyle = {},
): OverlayQuad[] {
  const strokePx = style.strokePx ?? 2;
  const sx = (2 * strokePx) / Math.max(1, mapRect.w);
  const sy = (2 * strokePx) / Math.max(1, mapRect.h);
  const out: OverlayQuad[] = [];
  for (const p of panes) {
    const [x0, y0, x1, y1] = toClip(boxOf(p, edgeNs), mapBox);
    const cx0 = Math.max(x0, -1), cx1 = Math.min(x1, 1);
    const cy0 = Math.max(y0, -1), cy1 = Math.min(y1, 1);
    if (!(cx1 > cx0) || !(cy1 > cy0)) continue; // wholly off the map: draw nothing, claim nothing
    const rgba = p.time.live ? PANE_MARK_LIVE : PANE_MARK;
    const edge = (clip: readonly [number, number, number, number]) =>
      out.push({ clip, rgba, kind: "pane-outline", id: p.id });
    if (x0 >= -1) edge([x0, cy0, Math.min(x0 + sx, cx1), cy1]);
    if (x1 <= 1) edge([Math.max(x1 - sx, cx0), cy0, x1, cy1]);
    if (y0 >= -1) edge([cx0, y0, cx1, Math.min(y0 + sy, cy1)]);
    if (y1 <= 1) edge([cx0, Math.max(y1 - sy, cy0), cx1, y1]);
  }
  return out;
}

/**
 * A lit segment per **reported** active capture window: where each SDR is live, right now.
 *
 * Two things this places honestly:
 *
 *  - **It sits at the live edge, because that is *when* it is true.** The segment is laid out
 *    through the same time mapping as everything else (the one shared time axis), so on a minimap
 *    scrubbed back into the past the live edge is off-screen and **no segment is drawn** — those
 *    captures are live *now*, and now is not on the picture. Pinning the bar to the top of the pane
 *    regardless would be the fixed-screen-coordinate overlay the invariant forbids.
 *  - **A window outside the map's frequency range is omitted, not clamped**, which would put a
 *    capture where it is not (ui/src/navigators.ts's `litSegments` rule, unchanged).
 *
 * With `windows: []` this returns `[]`: a segment appears because a window was reported, never
 * because the code assumes there is one.
 */
export function liveSegmentQuads(
  windows: readonly ActiveWindow[],
  edgeNs: number,
  mapBox: Box,
  mapRect: PaneRect,
  style: OverlayStyle = {},
): OverlayQuad[] {
  const segPx = style.segmentPx ?? 3;
  const minPx = style.minSegmentPx ?? 2;
  const sy = (2 * segPx) / Math.max(1, mapRect.h);
  const minW = (2 * minPx) / Math.max(1, mapRect.w);
  const [, , , yEdge] = toClip({ ...mapBox, t0Ns: edgeNs, t1Ns: edgeNs }, mapBox);
  if (!(yEdge >= -1) || !(yEdge <= 1)) return []; // the live edge is not on this picture
  const y0 = Math.max(-1, yEdge - sy), y1 = Math.min(1, yEdge);
  if (!(y1 > y0)) return [];
  const order = new Map<string, number>();
  const out: OverlayQuad[] = [];
  for (const w of windows) {
    const dev = coverageDeviceOf(w);
    if (!order.has(dev)) order.set(dev, order.size);
    const [a, , b] = toClip({ ...mapBox, f0Hz: w.loHz, f1Hz: w.hiHz }, mapBox);
    if (b < -1 || a > 1) continue; // off the map entirely
    let x0 = Math.max(a, -1), x1 = Math.min(b, 1);
    if (x1 - x0 < minW) { // the drawing floor: a 2.4 MHz window on 6 GHz must still be visible
      const mid = Math.min(1 - minW / 2, Math.max(-1 + minW / 2, (x0 + x1) / 2));
      x0 = mid - minW / 2;
      x1 = mid + minW / 2;
    }
    out.push({ clip: [x0, y0, x1, y1], rgba: DEVICE_MARKS[order.get(dev)! % DEVICE_MARKS.length], kind: "live-segment", id: dev });
  }
  return out;
}

/** The widest dimension a quad may have in device px and still be a *stroke* rather than a wash. */
export function quadSizePx(q: OverlayQuad, rect: PaneRect): { wPx: number; hPx: number } {
  return {
    wPx: ((q.clip[2] - q.clip[0]) / 2) * rect.w,
    hPx: ((q.clip[3] - q.clip[1]) / 2) * rect.h,
  };
}
