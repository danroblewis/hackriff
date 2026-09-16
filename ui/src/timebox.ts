// Time–frequency overlay boxes (T-362). Pure arithmetic: a box is a rectangle in the two axes the
// waterfall itself is drawn in — a fraction of the **full band** across, and **absolute capture
// time** down — and carries no screen position at all. `placeTimeBoxes` turns a list of them into
// screen rectangles, and it is called from inside the waterfall's own render pass (`waterfall.ts`
// `frame`), from the same ring head and the same per-row capture times the row pass reads, every
// animation frame.
//
// Why that placement lives here and not in the code that supplies the boxes (T-362, the user's bug):
// T-337 made a box's position a pure function of its capture time, but the position was *evaluated*
// on the ~1 s data poll while the waterfall scrolls every frame, so between polls a box sat still
// while the rows scrolled under it and then jumped when the poll caught up. The mapping was right
// and the clock it ran on was wrong. Placing the box inside the render pass removes the second
// clock rather than remembering to wind it: there is no stored screen coordinate to go stale, so a
// box cannot be more or less than one frame out of step with the rows it sits on.
//
// The mapping is `axis.timeSpanRows` over the rows' OWN timestamps (`RowsBackAt`). Never a
// rows-per-second: see the note on `axis.RowsBackAt` for the two ways a declared rate is wrong, both
// growing linearly with age.

import { timeSpanRows, type RowsBackAt } from "./axis";

/** How a box is painted. Presentation only — whoever builds the box decides, nothing here reads a
 * signal. Colours are RGBA 0..1 over the waterfall's own (always dark) colormap. */
export interface BoxStyle {
  fill: readonly [number, number, number, number];
  border: readonly [number, number, number, number];
  /** Border colour of the newest (top) edge, when it should differ — an interval still open. */
  top: readonly [number, number, number, number];
  borderPx: number;
  /** Thickness of the top edge (falls back to `borderPx` when smaller). */
  topPx: number;
  /** Dash period along the border, px; 0 draws it solid. */
  dashPx: number;
  /** Diagonal hatch over the fill: a rectangle known to approximate the thing it covers. */
  hatch: boolean;
}

/**
 * A rectangle to draw over the waterfall, in the waterfall's own axes.
 *
 * `u0`/`u1` are fractions of the **full band** (0..1, bins in order) — the same coordinates
 * `Waterfall.setView`'s zoom window is expressed in, so a zoom moves a box and the rows under it by
 * construction. `tLo`/`tHi` are **absolute capture time** (Unix s), older edge first. `title` is the
 * hover description; it is text, never a position.
 */
export interface TimeBox {
  id: string;
  u0: number;
  u1: number;
  tLo: number;
  tHi: number;
  style: BoxStyle;
  title?: string;
}

/** A placed box: fractions of the **waterfall pane**, x left→right across the zoom window, y
 * down from the newest row (0) to the oldest (1). Produced fresh every frame and never stored. */
export interface PlacedBox {
  id: string;
  x0: number;
  x1: number;
  y0: number;
  y1: number;
  style: BoxStyle;
}

/**
 * Every box that has any part on screen, placed through `rowsBackAt` — the rows' own timestamps,
 * inverted — and through the zoom window `[u0, u1]` the rows are drawn with.
 *
 * `minW`/`minH` are minimum sizes as fractions of the pane (a box thinner than a pixel or two would
 * otherwise vanish); a box is widened about its own centre, never moved off it. A box whose time
 * span has scrolled off the rows held returns nothing at all — no clamped rectangle standing for a
 * duration it never had (`timeSpanRows` null). Horizontal clipping is left to the viewport, which
 * clips exactly, so a box running off the side still draws the part that is on screen.
 */
export function placeTimeBoxes(
  boxes: readonly TimeBox[], rowsBackAt: RowsBackAt, rows: number, u0: number, u1: number,
  minW = 0, minH = 0,
): PlacedBox[] {
  const out: PlacedBox[] = [];
  const du = u1 - u0;
  if (!(du > 0)) return out;
  for (const b of boxes) {
    const ys = timeSpanRows(b.tLo, b.tHi, rowsBackAt, rows);
    if (!ys) continue;
    const lo = Math.min(b.u0, b.u1), hi = Math.max(b.u0, b.u1);
    if (!Number.isFinite(lo) || !Number.isFinite(hi)) continue;
    const [x0, x1] = widen((lo - u0) / du, (hi - u0) / du, minW);
    if (!(x1 > 0) || !(x0 < 1)) continue; // wholly outside the zoom window
    const [y0, y1] = widen(ys[0], ys[1], minH);
    out.push({ id: b.id, x0, x1, y0, y1, style: b.style });
  }
  return out;
}

/** [a, b] grown about its centre to at least `min` wide. */
function widen(a: number, b: number, min: number): [number, number] {
  if (!(min > 0) || b - a >= min) return [a, b];
  const mid = (a + b) / 2;
  return [mid - min / 2, mid + min / 2];
}

/**
 * The box under a point given in the boxes' own axes — band fraction `u`, absolute capture time
 * `tS` — topmost (last drawn) first; null when none contains it.
 *
 * Hit-testing in capture time rather than in pixels is what keeps interaction from becoming the
 * second layer this change exists to remove: there is no stored rectangle to test against, so a
 * pointer resolves to the same box the render pass drew under it, however many rows have scrolled
 * since the boxes were supplied.
 */
export function boxAt(boxes: readonly TimeBox[], u: number, tS: number): TimeBox | null {
  if (!Number.isFinite(u) || !Number.isFinite(tS)) return null;
  for (let i = boxes.length - 1; i >= 0; i--) {
    const b = boxes[i];
    if (u >= Math.min(b.u0, b.u1) && u <= Math.max(b.u0, b.u1) && tS >= b.tLo && tS <= b.tHi) return b;
  }
  return null;
}
