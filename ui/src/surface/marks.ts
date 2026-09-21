// **The signal boxes, on the canvas** (T-445): a Candidate/Confirmed presence interval or a
// selection, drawn as a stroked rectangle *inside a pane*, through the pane's own mapping.
//
// ## Why this file exists at all, and why it is only geometry
//
// This is where **T-388's box-jump** stops being reachable. That defect was a DOM overlay laid out
// on the inventory poll (~1 s) over a WebGL waterfall scrolling per frame: the two agreed only at
// the instant of the poll, so every box drifted and then snapped. Three separate fixes were tried
// against the symptom. The structural fix is that there is no second layout any more —
// `SurfaceView.frame()` hands this file **the very `PaneView` the data pass was just given**, and
// [[toClip]] is **the same function** `Surface` places its tiles with. A box and the energy under it
// are therefore placed by one mapping on one frame. They cannot disagree, because there is nothing
// left to disagree *with*.
//
// Everything here is a **stroke**: four edges of a rectangle, never a wash over its interior.
// `overlay.ts`'s program has no sampler, no ramp and no cell-state uniform, so nothing drawn from
// here can express a measurement colour or a grey (docs/16 §8.4b). That is what lets the boxes and
// the honesty rule coexist without a flag anyone has to remember.
//
// **No signal logic.** Which rows exist, what state each is in, when each interval started and
// whether it is still open are all backend answers (`GET /api/inventory`'s `presence`,
// `GET /api/selections`). This file converts a rectangle in (Hz, capture-seconds) into clip space
// and picks an ink. It computes no time extent of its own: an **open** interval runs to `edgeNs`,
// which is reported in, and a closed one stops exactly at the `t_end_s` the API served.

import type { Box } from "./lattice";
import type { OverlayQuad } from "./minimap";
import { toClip, type PaneRect } from "./surface";

const S_TO_NS = 1e9;

/** Confirmed: teal, the same token the retired DOM overlay used, so nothing the user recognises
 * changes colour in the cutover. */
export const CONFIRMED_MARK: readonly [number, number, number, number] = [0.322, 0.761, 0.682, 0.95];
/** Candidate: lavender, drawn thinner — a candidate is the weaker claim (T-389's ordering). */
export const CANDIDATE_MARK: readonly [number, number, number, number] = [0.639, 0.584, 0.878, 0.85];
/** A selection: amber, as it has always been. */
export const SELECTION_MARK: readonly [number, number, number, number] = [0.941, 0.647, 0.259, 0.85];
/** T-587: a row the backend has explained as a receiver artifact (`relation.kind` `artifact-of` or
 * `retune-sibling-of`) — neutral grey, deliberately outside the teal/lavender "this is a signal"
 * palette, so an explained artifact reads as explained on the surface too, not as a third kind of
 * mystery signal. Drawn thinner than either signal mark: the weakest claim of the three. */
export const ARTIFACT_MARK: readonly [number, number, number, number] = [0.55, 0.58, 0.6, 0.7];
/** The focused row or selection, whichever it is: the same hue, opaque. */
export const FOCUS_ALPHA = 1;
/**
 * The newest edge of an interval still on the air. T-410/ADR-0019: an open interval's box runs to
 * the **live edge** by assumption and is capped only when an end is detected, so this edge *is* the
 * live edge — which is why it is marked differently from a measured end.
 */
export const OPEN_EDGE_MARK: readonly [number, number, number, number] = [0.98, 0.82, 0.25, 1];
/** A region being stroked out but not yet committed (T-458): white, so it reads as the pointer's
 * own mark rather than as a claim about the air, and cannot be mistaken for the amber a selection
 * that exists is drawn in. */
export const PENDING_MARK: readonly [number, number, number, number] = [1, 1, 1, 0.9];

export interface MarkStyle {
  /** Edge thickness, device px. */
  strokePx?: number;
  /** Thickness of an open interval's live-edge cap, device px. */
  openPx?: number;
  /**
   * A **drawing** floor, device px: a 15 kHz emission in a 2 MHz pane is 0.7 % of the width and a
   * 200 ms burst in a 10 minute pane rounds away entirely. A box that cannot be seen is the one
   * thing this mark exists to show. Nothing reads the drawn size back as a bandwidth or a duration
   * — the same rule `liveSegmentQuads`'s `minSegmentPx` follows.
   */
  minPx?: number;
}

/** One rectangle to stroke, in absolute surface coordinates. Presentation input only: every number
 * came off the API, and `t1Ns === null` means *open at the live edge*, never *unknown*. */
export interface MarkBox {
  readonly id: string;
  readonly kind: "signal-box" | "selection-box" | "pending-region";
  readonly f0Hz: number;
  readonly f1Hz: number;
  readonly t0Ns: number;
  /** `null` = still on the air: the box runs to the edge reported in. */
  readonly t1Ns: number | null;
  readonly rgba: readonly [number, number, number, number];
  /** Mark the newest edge as the live edge rather than a measured end. */
  readonly open: boolean;
}

/** A row as this file reads it — the shape `GET /api/inventory` serves, narrowed to what a
 * rectangle needs. Nothing is derived: `open` and the interval bounds are the API's own. */
export interface MarkRow {
  readonly id: string;
  readonly state: string;
  readonly f_lo_hz: number;
  readonly f_hi_hz: number;
  /** `GET /api/inventory`'s user-band override, in its own wire shape (`f_lo`/`f_hi`). */
  readonly user_band?: { f_lo: number; f_hi: number } | null;
  readonly presence?: { last_interval?: { t_start_s: number; t_end_s: number; open: boolean } | null } | null;
  /** T-587: `relation.kind`, narrowed to what a box's ink needs — `docs/api.md` `relation` or
   * `null`/`undefined` for a row with no standing claim. */
  readonly relation?: { kind: string } | null;
}

/** A selection as this file reads it (`GET /api/selections`). */
export interface MarkSelection {
  readonly id: string;
  readonly f_lo: number;
  readonly f_hi: number;
  readonly t_lo?: number | null;
  readonly t_hi?: number | null;
}

const withAlpha = (c: readonly [number, number, number, number], a: number): readonly [number, number, number, number] =>
  [c[0], c[1], c[2], a];

/**
 * The rectangles for the inventory rows in `rows`.
 *
 * A row with no `presence.last_interval` yields **nothing** — no zero-duration box is fabricated for
 * a row whose time extent the backend did not state. That is the retired `presenceBoxes`' rule,
 * kept verbatim, because it is a claim about evidence rather than a rendering detail.
 *
 * A **user band** override wins over the detected one where the API reports it (T-193): the box
 * shows the band in force, not the one that was superseded.
 */
export function signalMarkBoxes(rows: readonly MarkRow[], focusedId: string | null): MarkBox[] {
  const out: MarkBox[] = [];
  for (const r of rows) {
    if (r.state !== "candidate" && r.state !== "confirmed") continue;
    const iv = r.presence?.last_interval;
    if (!iv) continue;
    // T-219, unchanged: `suppressed-by`/`duplicate-of` stay undrawn — a stronger row already
    // covers them, so there is no second box to draw at all, on the list or the surface.
    const relKind = r.relation?.kind;
    if (relKind === "suppressed-by" || relKind === "duplicate-of") continue;
    // T-587: an explained artifact draws its own neutral ink, on either tab — the point is that
    // the box on screen is no longer indistinguishable from an unexplained signal's.
    const base = relKind === "artifact-of" || relKind === "retune-sibling-of"
      ? ARTIFACT_MARK
      : r.state === "confirmed" ? CONFIRMED_MARK : CANDIDATE_MARK;
    const band = r.user_band ?? null;
    out.push({
      id: r.id, kind: "signal-box",
      f0Hz: band ? band.f_lo : r.f_lo_hz,
      f1Hz: band ? band.f_hi : r.f_hi_hz,
      t0Ns: iv.t_start_s * S_TO_NS,
      t1Ns: iv.open ? null : iv.t_end_s * S_TO_NS,
      rgba: r.id === focusedId ? withAlpha(base, FOCUS_ALPHA) : base,
      open: iv.open,
    });
  }
  return out;
}

/**
 * The rectangles for `sels`.
 *
 * A selection with **no** time extent is a frequency-only region — "any time" — so it is drawn
 * spanning the pane's whole time axis rather than being given a duration it was never made with.
 * That is the honest rendering the retired DOM layer reached for a full-height box to express, and
 * on this surface it is the same statement in the same pass as everything else.
 */
export function selectionMarkBoxes(
  sels: readonly MarkSelection[], focusedId: string | null, paneBox: Box,
): MarkBox[] {
  const out: MarkBox[] = [];
  for (const s of sels) {
    const timed = typeof s.t_lo === "number" && typeof s.t_hi === "number";
    out.push({
      id: s.id, kind: "selection-box",
      f0Hz: s.f_lo, f1Hz: s.f_hi,
      t0Ns: timed ? (s.t_lo as number) * S_TO_NS : paneBox.t0Ns,
      t1Ns: timed ? (s.t_hi as number) * S_TO_NS : paneBox.t1Ns,
      rgba: s.id === focusedId ? withAlpha(SELECTION_MARK, FOCUS_ALPHA) : SELECTION_MARK,
      open: false,
    });
  }
  return out;
}

/**
 * Stroke `boxes` into one pane: four edges per rectangle, clipped to the pane, through the data
 * pass's own [[toClip]].
 *
 * A box wholly outside the pane draws nothing — a rectangle clamped to the pane's border would
 * claim a boundary the signal does not have (`paneOutlineQuads`' rule, one subject over). A box
 * only partly on screen draws only the edges that are genuinely on it.
 */
export function markQuads(
  boxes: readonly MarkBox[], edgeNs: number, paneBox: Box, rect: PaneRect, style: MarkStyle = {},
): OverlayQuad[] {
  const strokePx = style.strokePx ?? 2;
  const openPx = style.openPx ?? 3;
  const minPx = style.minPx ?? 2;
  const sx = (2 * strokePx) / Math.max(1, rect.w);
  const sy = (2 * strokePx) / Math.max(1, rect.h);
  const minW = (2 * minPx) / Math.max(1, rect.w);
  const minH = (2 * minPx) / Math.max(1, rect.h);
  const out: OverlayQuad[] = [];
  for (const b of boxes) {
    const t1Ns = b.t1Ns ?? edgeNs;
    if (!(b.f1Hz > b.f0Hz) || !(t1Ns > b.t0Ns)) continue;
    let [x0, y0, x1, y1] = toClip({ f0Hz: b.f0Hz, f1Hz: b.f1Hz, t0Ns: b.t0Ns, t1Ns }, paneBox);
    // The drawing floor. Widened about the centre, and never read back as a measurement.
    if (x1 - x0 < minW) { const m = (x0 + x1) / 2; x0 = m - minW / 2; x1 = m + minW / 2; }
    if (y1 - y0 < minH) { const m = (y0 + y1) / 2; y0 = m - minH / 2; y1 = m + minH / 2; }
    const cx0 = Math.max(x0, -1), cx1 = Math.min(x1, 1);
    const cy0 = Math.max(y0, -1), cy1 = Math.min(y1, 1);
    if (!(cx1 > cx0) || !(cy1 > cy0)) continue; // wholly off this pane: draw nothing, claim nothing
    const edge = (clip: readonly [number, number, number, number], rgba: readonly [number, number, number, number]) =>
      out.push({ clip, rgba, kind: b.kind, id: b.id });
    if (x0 >= -1) edge([x0, cy0, Math.min(x0 + sx, cx1), cy1], b.rgba);
    if (x1 <= 1) edge([Math.max(x1 - sx, cx0), cy0, x1, cy1], b.rgba);
    if (y0 >= -1) edge([cx0, y0, cx1, Math.min(y0 + sy, cy1)], b.rgba);
    if (y1 <= 1) {
      // The newest edge. Open → this *is* the live edge, and it says so by being marked
      // differently from a measured end (ADR-0019's open cap, expressed as an edge rather than a
      // fill because this pass can only stroke).
      const h = (2 * (b.open ? openPx : strokePx)) / Math.max(1, rect.h);
      edge([cx0, Math.max(y1 - h, cy0), cx1, y1], b.open ? OPEN_EDGE_MARK : b.rgba);
    }
  }
  return out;
}

/**
 * The mark under a point in surface coordinates, or null — **the same rectangles that were drawn**.
 *
 * Hit-testing against the drawn geometry rather than against a separate frequency-only predicate is
 * the point: the retired click handler resolved a click by frequency alone, through
 * `clickTarget`/`inspectHalfWidthHz`, so on a waterfall it could focus a row whose box was nowhere
 * near the pointer *in time*. Here a click lands on what it looks like it landed on.
 *
 * Narrowest wins, so a burst inside a wider emitter's box is reachable. A selection is preferred
 * only when it is strictly narrower — a frequency-only selection spans the whole pane and would
 * otherwise swallow every signal under it.
 */
export function markAt(
  boxes: readonly MarkBox[], edgeNs: number, fHz: number, tNs: number,
): MarkBox | null {
  let best: MarkBox | null = null, bestArea = Infinity;
  for (const b of boxes) {
    const t1 = b.t1Ns ?? edgeNs;
    if (fHz < b.f0Hz || fHz > b.f1Hz || tNs < b.t0Ns || tNs > t1) continue;
    const area = (b.f1Hz - b.f0Hz) * (t1 - b.t0Ns);
    if (area < bestArea) { best = b; bestArea = area; }
  }
  return best;
}

/** The point in surface coordinates under a point in a pane's rectangle (drawing-buffer px, GL
 * convention). The exact inverse of the [[toClip]] the marks and the tiles are both placed by. */
export function pointOn(paneBox: Box, rect: PaneRect, xPx: number, yPx: number): { fHz: number; tNs: number } {
  const u = (xPx - rect.x) / Math.max(1, rect.w);
  const v = (yPx - rect.y) / Math.max(1, rect.h);
  return {
    fHz: paneBox.f0Hz + u * (paneBox.f1Hz - paneBox.f0Hz),
    tNs: paneBox.t0Ns + v * (paneBox.t1Ns - paneBox.t0Ns),
  };
}

// ---- the region being stroked out (T-458) ----

/** A rectangle in surface coordinates, ordered low-to-high on both axes. */
export interface MarkRegion { f0Hz: number; f1Hz: number; t0Ns: number; t1Ns: number }

/**
 * The two corners of a region stroke, ordered.
 *
 * **One ordering, used twice.** The box that is drawn while the pointer is down and the region that
 * is committed on release both come through here, so a stroke made upward or leftward cannot end up
 * drawn as one rectangle and committed as another — the drift-between-two-implementations failure
 * this milestone exists to close, in miniature.
 */
export function normalizeRegion(
  a: { fHz: number; tNs: number }, b: { fHz: number; tNs: number },
): MarkRegion {
  return {
    f0Hz: Math.min(a.fHz, b.fHz), f1Hz: Math.max(a.fHz, b.fHz),
    t0Ns: Math.min(a.tNs, b.tNs), t1Ns: Math.max(a.tNs, b.tNs),
  };
}

/**
 * The pending region as a box for [[markQuads]], or nothing when there is no stroke in progress.
 *
 * It goes through the **same** pass as every other mark rather than a second overlay of its own —
 * T-388's box-jump was precisely a second layout beside the first, and a rubber band that lagged
 * the pointer by a poll would be that defect wearing a different hat. `t1Ns` is never `null`: a
 * stroke has two ends the user made, so it has nothing to say about the live edge.
 */
export function pendingMarkBox(region: MarkRegion | null): MarkBox[] {
  if (!region) return [];
  return [{
    id: "pending", kind: "pending-region",
    f0Hz: region.f0Hz, f1Hz: region.f1Hz, t0Ns: region.t0Ns, t1Ns: region.t1Ns,
    rgba: PENDING_MARK, open: false,
  }];
}

// ---- a rule across the pane at one capture instant (T-506) ----

export interface TimeRuleStyle {
  /** Line thickness, device px. */
  thickPx?: number;
  /** Dash and gap lengths, device px; absent is a solid line. */
  dashPx?: number;
  gapPx?: number;
}

/**
 * A horizontal line across the whole pane at capture instant `tNs`, through the data pass's own
 * [[toClip]] — so it is laid out in the same pass, on the same frame, as the rows it marks, and
 * moves with them (the one-shared-time-axis rule).
 *
 * A stroke, never a wash: it marks a boundary *in time* and says nothing about the cells either side
 * of it, so the coverage grey and the ramp under it are untouched. An instant outside the pane draws
 * **nothing** — a rule pinned to the pane's edge would claim a boundary at a time it is not.
 *
 * Dashes are laid out from the pane's left edge in device px, so a dashed rule reads as the same
 * pattern at every zoom; that is the second cue (after the ink) that tells two rules apart when
 * they coincide.
 */
export function timeRuleQuads(
  tNs: number, rgba: readonly [number, number, number, number], id: string,
  paneBox: Box, rect: PaneRect, style: TimeRuleStyle = {},
): OverlayQuad[] {
  if (!Number.isFinite(tNs) || !(paneBox.t1Ns > paneBox.t0Ns)) return [];
  if (tNs < paneBox.t0Ns || tNs > paneBox.t1Ns) return [];
  const thick = (2 * (style.thickPx ?? 2)) / Math.max(1, rect.h);
  const [, y] = toClip({ f0Hz: paneBox.f0Hz, f1Hz: paneBox.f1Hz, t0Ns: tNs, t1Ns: tNs }, paneBox);
  const y0 = Math.max(-1, Math.min(1 - thick, y - thick / 2)), y1 = y0 + thick;
  const dash = style.dashPx ?? 0, gap = style.gapPx ?? dash;
  if (!(dash > 0)) return [{ clip: [-1, y0, 1, y1], rgba, kind: "time-rule", id }];
  const out: OverlayQuad[] = [];
  const w = Math.max(1, rect.w);
  for (let x = 0; x < w; x += dash + gap) {
    const x0 = (2 * x) / w - 1, x1 = (2 * Math.min(w, x + dash)) / w - 1;
    out.push({ clip: [x0, y0, x1, y1], rgba, kind: "time-rule", id });
  }
  return out;
}

/** The widest a mark may be in device px and still be a stroke rather than a wash — the assertion
 * `ui/test/surface-minimap.test.ts` already makes about the map's own quads, available for these. */
export { quadSizePx } from "./minimap";
