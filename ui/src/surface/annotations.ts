// **Human-authored annotations, on the canvas** (T-820 / MAP-20, docs/25 §5, docs/23 §7).
//
// An annotation is a researcher's own note — a box, a marker or a text note — stored by
// `POST /api/annotations` (T-816). It is **not** a claim about the air, so it must never be
// mistaken for one: every detection, selection and measurement on this surface is a *solid* stroke
// (`./marks.ts`), and an annotation is the one mark drawn **dashed**, in its own rose ink. Shape and
// pattern, not hue alone, carry the difference (docs/23 §10.5: state is never encoded in hue alone).
//
// **Only geometry.** Everything here is (Hz, capture-seconds) → the pane's own [[toClip]], the same
// function the tiles and every other mark are placed by, so an annotation moves with the rows it was
// drawn over on the same frame (the one-shared-time-axis rule; T-388's drift cannot reappear). An
// annotation's `t0_s`/`t1_s` are fixed capture-clock times: a human-set extent never grows to the
// live edge, so nothing here reads one. No fetch, no poll, no signal logic.

import type { Box } from "./lattice";
import type { OverlayQuad } from "./minimap";
import { toClip, type PaneRect } from "./surface";
import type { GlPoint } from "./input";

const S_TO_NS = 1e9;

/** The annotation ink: rose — outside the teal/lavender (detections), amber (selections), sky
 * (measurements) and grey (artifacts) palette, so a human note reads as a human note. */
export const ANNOTATION_MARK: readonly [number, number, number, number] = [0.953, 0.475, 0.639, 0.95];

export type AnnotationKind = "text" | "box" | "marker";

/** An annotation as `GET /api/annotations` serves it, narrowed to what the canvas draws. */
export interface MarkAnnotation {
  readonly id: string;
  readonly kind: AnnotationKind;
  readonly f_lo_hz: number;
  readonly f_hi_hz: number;
  readonly t0_s: number;
  readonly t1_s: number;
  readonly label: string;
}

export interface AnnotationStyle {
  /** Stroke thickness, device px. */
  strokePx?: number;
  /** Dash and gap of a box's edges, device px. */
  dashPx?: number;
  gapPx?: number;
  /** Half-size of a point glyph (marker cross, note square), device px. */
  glyphPx?: number;
}

type Clip = readonly [number, number, number, number];

/** A point annotation's anchor: the centre of its (usually zero-area) extent. */
function anchorOf(a: MarkAnnotation): { fHz: number; tNs: number } {
  return { fHz: (a.f_lo_hz + a.f_hi_hz) / 2, tNs: ((a.t0_s + a.t1_s) / 2) * S_TO_NS };
}

/** Whether the annotation is drawn as a rectangle: a `box`, or any kind the author gave a positive
 * extent on both axes. Everything else is a point glyph. */
export function isBoxShaped(a: MarkAnnotation): boolean {
  return a.f_hi_hz > a.f_lo_hz && a.t1_s > a.t0_s && (a.kind === "box" || a.kind === "text");
}

/** Clip-space x/y of a surface point in a pane. */
function clipPoint(fHz: number, tNs: number, paneBox: Box): [number, number] {
  const [x, y] = toClip({ f0Hz: fHz, f1Hz: fHz, t0Ns: tNs, t1Ns: tNs }, paneBox);
  return [x, y];
}

/** Dashes along one horizontal or vertical run `[a, b]` (clip units), `unit` clip units per px. */
function dashes(a: number, b: number, unit: number, dashPx: number, gapPx: number): [number, number][] {
  const out: [number, number][] = [];
  const step = (dashPx + gapPx) * unit;
  if (!(step > 0) || !(b > a)) return out;
  for (let s = a; s < b; s += step) out.push([s, Math.min(b, s + dashPx * unit)]);
  return out;
}

/**
 * The quads for every annotation in one pane. A box is four **dashed** edges; a marker is a `+`; a
 * text note is a small hollow square. Anything wholly off the pane draws nothing — an annotation
 * clamped to the border would claim a place it is not.
 */
export function annotationQuads(
  items: readonly MarkAnnotation[], focusedId: string | null, paneBox: Box, rect: PaneRect, style: AnnotationStyle = {},
): OverlayQuad[] {
  const spx = style.strokePx ?? 2, dashPx = style.dashPx ?? 6, gapPx = style.gapPx ?? 4, g = style.glyphPx ?? 7;
  const ux = 2 / Math.max(1, rect.w), uy = 2 / Math.max(1, rect.h);
  const out: OverlayQuad[] = [];
  const push = (clip: Clip, id: string, rgba: Clip) => out.push({ clip, rgba, kind: "annotation", id });
  for (const a of items) {
    const rgba: Clip = a.id === focusedId ? [ANNOTATION_MARK[0], ANNOTATION_MARK[1], ANNOTATION_MARK[2], 1] : ANNOTATION_MARK;
    const w = (a.id === focusedId ? spx + 1 : spx);
    if (isBoxShaped(a)) {
      const [x0, y0, x1, y1] = toClip({ f0Hz: a.f_lo_hz, f1Hz: a.f_hi_hz, t0Ns: a.t0_s * S_TO_NS, t1Ns: a.t1_s * S_TO_NS }, paneBox);
      const cx0 = Math.max(x0, -1), cx1 = Math.min(x1, 1), cy0 = Math.max(y0, -1), cy1 = Math.min(y1, 1);
      if (!(cx1 > cx0) || !(cy1 > cy0)) continue;
      const sx = w * ux, sy = w * uy;
      for (const [d0, d1] of dashes(cx0, cx1, ux, dashPx, gapPx)) {
        if (y0 >= -1) push([d0, y0, d1, Math.min(y0 + sy, cy1)], a.id, rgba);
        if (y1 <= 1) push([d0, Math.max(y1 - sy, cy0), d1, y1], a.id, rgba);
      }
      for (const [d0, d1] of dashes(cy0, cy1, uy, dashPx, gapPx)) {
        if (x0 >= -1) push([x0, d0, Math.min(x0 + sx, cx1), d1], a.id, rgba);
        if (x1 <= 1) push([Math.max(x1 - sx, cx0), d0, x1, d1], a.id, rgba);
      }
      continue;
    }
    const p = anchorOf(a);
    const [x, y] = clipPoint(p.fHz, p.tNs, paneBox);
    if (x < -1 || x > 1 || y < -1 || y > 1) continue; // off this pane: draw nothing, claim nothing
    const gx = g * ux, gy = g * uy, hx = (w / 2) * ux, hy = (w / 2) * uy;
    if (a.kind === "marker") {
      push([x - gx, y - hy, x + gx, y + hy], a.id, rgba);
      push([x - hx, y - gy, x + hx, y + gy], a.id, rgba);
    } else {
      push([x - gx, y - gy, x + gx, y - gy + 2 * hy], a.id, rgba);
      push([x - gx, y + gy - 2 * hy, x + gx, y + gy], a.id, rgba);
      push([x - gx, y - gy, x - gx + 2 * hx, y + gy], a.id, rgba);
      push([x + gx - 2 * hx, y - gy, x + gx, y + gy], a.id, rgba);
    }
  }
  return out;
}

/** Where an annotation's label sits, in the pane's drawing-buffer px (GL convention, origin
 * bottom-left): just right of a point glyph, or at a box's top-left (its newest, earliest-drawn
 * corner). Only annotations whose anchor is inside the pane get one. */
export interface AnnotationLabel { readonly id: string; readonly text: string; readonly x: number; readonly y: number }

export function annotationLabels(
  items: readonly MarkAnnotation[], paneBox: Box, rect: PaneRect, glyphPx = 7,
): AnnotationLabel[] {
  const out: AnnotationLabel[] = [];
  for (const a of items) {
    const box = isBoxShaped(a);
    const fHz = box ? a.f_lo_hz : anchorOf(a).fHz;
    const tNs = box ? a.t1_s * S_TO_NS : anchorOf(a).tNs;
    const [cx, cy] = clipPoint(fHz, tNs, paneBox);
    if (cx < -1 || cx > 1 || cy < -1 || cy > 1) continue;
    const x = rect.x + ((cx + 1) / 2) * rect.w + (box ? 0 : glyphPx + 3);
    const y = rect.y + ((cy + 1) / 2) * rect.h;
    out.push({ id: a.id, text: a.label, x, y });
  }
  return out;
}

/** The annotation under a drawing-buffer point, or null: inside a box, or within the glyph of a
 * point. Narrowest box wins, and a point glyph beats any box it sits in. */
export function annotationAt(
  items: readonly MarkAnnotation[], paneBox: Box, rect: PaneRect, at: GlPoint, glyphPx = 7,
): MarkAnnotation | null {
  let best: MarkAnnotation | null = null, bestArea = Infinity;
  const fHz = paneBox.f0Hz + ((at.x - rect.x) / Math.max(1, rect.w)) * (paneBox.f1Hz - paneBox.f0Hz);
  const tNs = paneBox.t0Ns + ((at.y - rect.y) / Math.max(1, rect.h)) * (paneBox.t1Ns - paneBox.t0Ns);
  for (const a of items) {
    if (isBoxShaped(a)) {
      if (fHz < a.f_lo_hz || fHz > a.f_hi_hz || tNs < a.t0_s * S_TO_NS || tNs > a.t1_s * S_TO_NS) continue;
      const area = (a.f_hi_hz - a.f_lo_hz) * (a.t1_s - a.t0_s);
      if (area < bestArea) { best = a; bestArea = area; }
      continue;
    }
    const p = anchorOf(a);
    const [cx, cy] = clipPoint(p.fHz, p.tNs, paneBox);
    const px = rect.x + ((cx + 1) / 2) * rect.w, py = rect.y + ((cy + 1) / 2) * rect.h;
    if (Math.abs(px - at.x) <= glyphPx + 2 && Math.abs(py - at.y) <= glyphPx + 2) return a;
  }
  return best;
}
