// **The `paths` layer** (T-897, docs/23 §10.6 rule 2, ADR-0023 §2): a chirp, a sweep or a hop
// sequence drawn on the surface as a thin **traced line** through (time × frequency) — the map's
// "blue directions line" — beside the interval box ADR-0019 already draws for it.
//
// ## Only geometry, and only through the tiles' own mapping
//
// Every vertex is a backend answer (`GET /api/paths`): an absolute capture time and a frequency
// the detector measured. Nothing here derives a vertex, fits a line or decides what a path is —
// that is `hk_model::path`. This file converts vertices into **strokes** inside one pane, through
// the very [[toClip]] `Surface` places its tiles with, and it is called from inside
// `SurfaceView.frame()` through the one `marks` hook, with the `PaneView` the data pass was just
// handed. A path vertex and the tile row at its capture time are therefore placed by one mapping on
// one frame, and cannot drift apart under scroll or zoom (T-388's structural fix, carried over).
//
// ## Why a sloped line is a run of axis-aligned strokes
//
// The overlay pass (`overlay.ts`) draws flat-coloured axis-aligned rectangles and nothing else: no
// sampler, no ramp, no rotated geometry — which is exactly why it can never tint a measurement.
// Rather than teach it a second primitive, a segment is cut into steps of a few device pixels
// along its major axis and each step is stroked as the small rectangle spanning it, stroke-thick in
// the minor axis. The run is continuous (each step shares its end with the next's start), a hop's
// dwell (constant frequency) or an instant's jump (constant time) is ONE rectangle, and the whole
// thing is still only strokes: the byte-identical-with-overlays-on/off guard needs no change.
//
// ## Bounded per frame
//
// A segment is first clipped to the pane (Liang–Barsky), so a route mostly off screen costs only
// its visible part; the step grows with the on-screen length so one path never exceeds
// [[MAX_PATH_QUADS]] rectangles. Presentation arithmetic only: the step is never read back as a
// measurement.

import type { Box } from "./lattice";
import type { OverlayQuad } from "./minimap";
import { toClip, type PaneRect } from "./surface";

const S_TO_NS = 1e9;

/** The directions-line ink: a saturated blue, deeper than a measurement box's sky blue and off
 * every cell mark and ramp stop. Opaque enough to read over energy, thin enough not to hide it. */
export const PATH_MARK: readonly [number, number, number, number] = [0.29, 0.45, 1.0, 0.95];
/** Stroke thickness, device px. */
export const PATH_STROKE_PX = 2;
/** Step along a sloped segment's major axis, device px. */
export const PATH_STEP_PX = 3;
/** Most rectangles one path may cost per pane per frame. */
export const MAX_PATH_QUADS = 600;

export type PathKind = "chirp" | "sweep" | "hop";

/** One path as the client holds it: ordered vertices at absolute capture time, from the wire. */
export interface MarkPath {
  readonly id: string;
  readonly kind: PathKind;
  readonly vertices: readonly { readonly tNs: number; readonly fHz: number }[];
}

/**
 * The request for the panes whose `paths` layer is on: one `GET /api/paths` over the union of
 * their boxes, or `null` when there is nothing to ask about (no pane showing the layer, or no
 * live edge yet). A viewport route answers about a viewport, so all four bounds are always sent.
 */
export function pathsRequest(boxes: readonly Box[]): string | null {
  const ok = boxes.filter((b) => b.f1Hz > b.f0Hz && b.t1Ns > b.t0Ns && b.t0Ns > 0);
  if (ok.length === 0) return null;
  const f0 = Math.max(0, Math.min(...ok.map((b) => b.f0Hz)));
  const f1 = Math.max(...ok.map((b) => b.f1Hz));
  const t0 = Math.min(...ok.map((b) => b.t0Ns)) / S_TO_NS;
  const t1 = Math.max(...ok.map((b) => b.t1Ns)) / S_TO_NS;
  return `/api/paths?f_lo=${f0}&f_hi=${f1}&t0=${t0}&t1=${t1}`;
}

/** The wire answer (`docs/api.md` "GET /api/paths") → [[MarkPath]]s. Anything malformed is dropped
 * rather than drawn: a vertex with no time or frequency has no place on the map. */
export function parsePaths(body: unknown): MarkPath[] {
  const list = (body as { paths?: unknown } | null)?.paths;
  if (!Array.isArray(list)) return [];
  const out: MarkPath[] = [];
  for (const p of list as Record<string, unknown>[]) {
    const kind = p?.kind;
    if (typeof p?.id !== "string" || (kind !== "chirp" && kind !== "sweep" && kind !== "hop")) continue;
    const vs = Array.isArray(p.vertices) ? (p.vertices as Record<string, unknown>[]) : [];
    const vertices = vs
      .filter((v) => typeof v?.t_s === "number" && typeof v?.f_hz === "number" && Number.isFinite(v.t_s) && Number.isFinite(v.f_hz))
      .map((v) => ({ tNs: (v.t_s as number) * S_TO_NS, fHz: v.f_hz as number }));
    if (vertices.length >= 2) out.push({ id: p.id, kind, vertices });
  }
  return out;
}

/** A vertex in the pane's clip space, through the tiles' own [[toClip]]. */
function clipOf(tNs: number, fHz: number, box: Box): [number, number] {
  const c = toClip({ f0Hz: fHz, f1Hz: fHz, t0Ns: tNs, t1Ns: tNs }, box);
  return [c[0], c[1]];
}

/** Liang–Barsky: the part of `a→b` inside `[lo, hi]²`, or `null`. */
function clipSegment(a: [number, number], b: [number, number], lo: number, hi: number): [[number, number], [number, number]] | null {
  const dx = b[0] - a[0], dy = b[1] - a[1];
  let u0 = 0, u1 = 1;
  for (const [p, q] of [[-dx, a[0] - lo], [dx, hi - a[0]], [-dy, a[1] - lo], [dy, hi - a[1]]] as const) {
    if (p === 0) { if (q < 0) return null; continue; }
    const r = q / p;
    if (p < 0) { if (r > u1) return null; if (r > u0) u0 = r; } else { if (r < u0) return null; if (r < u1) u1 = r; }
  }
  return [[a[0] + u0 * dx, a[1] + u0 * dy], [a[0] + u1 * dx, a[1] + u1 * dy]];
}

export interface PathStyle { strokePx?: number; stepPx?: number; maxQuads?: number; rgba?: readonly [number, number, number, number] }

/**
 * Stroke `paths` into one pane. Each segment between consecutive vertices is clipped to the pane
 * and drawn as a continuous run of stroke-thick rectangles (see the header). A path wholly off the
 * pane draws nothing.
 */
export function pathQuads(paths: readonly MarkPath[], paneBox: Box, rect: PaneRect, style: PathStyle = {}): OverlayQuad[] {
  const w = Math.max(1, rect.w), h = Math.max(1, rect.h);
  const strokePx = style.strokePx ?? PATH_STROKE_PX;
  const hx = strokePx / w, hy = strokePx / h; // half a stroke each side, in clip units (2 clip = w px)
  const rgba = style.rgba ?? PATH_MARK;
  const maxQuads = style.maxQuads ?? MAX_PATH_QUADS;
  const out: OverlayQuad[] = [];
  const toPx = (dx: number, dy: number) => [Math.abs(dx) * w / 2, Math.abs(dy) * h / 2];
  const push = (x0: number, y0: number, x1: number, y1: number, id: string) => {
    const cx0 = Math.max(-1, Math.min(x0, x1) - hx), cx1 = Math.min(1, Math.max(x0, x1) + hx);
    const cy0 = Math.max(-1, Math.min(y0, y1) - hy), cy1 = Math.min(1, Math.max(y0, y1) + hy);
    if (cx1 > cx0 && cy1 > cy0) out.push({ clip: [cx0, cy0, cx1, cy1], rgba, kind: "path-stroke", id });
  };
  for (const p of paths) {
    // The visible part of every segment first, so the step can be chosen from what is on screen.
    const segs: [[number, number], [number, number]][] = [];
    let lenPx = 0;
    for (let i = 1; i < p.vertices.length; i++) {
      const a = clipOf(p.vertices[i - 1].tNs, p.vertices[i - 1].fHz, paneBox);
      const b = clipOf(p.vertices[i].tNs, p.vertices[i].fHz, paneBox);
      const s = clipSegment(a, b, -1 - hx - hy, 1 + hx + hy);
      if (!s) continue;
      segs.push(s);
      const [px, py] = toPx(s[1][0] - s[0][0], s[1][1] - s[0][1]);
      lenPx += Math.max(px, py);
    }
    const step = Math.max(style.stepPx ?? PATH_STEP_PX, lenPx / Math.max(1, maxQuads - segs.length));
    for (const [a, b] of segs) {
      const [px, py] = toPx(b[0] - a[0], b[1] - a[1]);
      // Axis-aligned (a dwell, or an instant's jump): one rectangle is exact.
      const n = px < 0.5 || py < 0.5 ? 1 : Math.max(1, Math.ceil(Math.max(px, py) / step));
      for (let k = 0; k < n; k++) {
        const u0 = k / n, u1 = (k + 1) / n;
        push(a[0] + u0 * (b[0] - a[0]), a[1] + u0 * (b[1] - a[1]), a[0] + u1 * (b[0] - a[0]), a[1] + u1 * (b[1] - a[1]), p.id);
      }
    }
  }
  return out;
}
