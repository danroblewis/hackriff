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
import type { OverlayPattern, OverlayQuad } from "./minimap";
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
/** A saved measurement (T-822 / MAP-22): sky blue, distinct from every other ink on the surface —
 * a measurement is a claim about a **span the user marked**, not a detection (teal/lavender), a
 * selection (amber) or an explained artifact (grey). */
export const MEASUREMENT_MARK: readonly [number, number, number, number] = [0.365, 0.686, 0.937, 0.9];

/**
 * T-994: a feature with an OPEN OUTPUT — a Listen stream, a running decode pipeline, a recording
 * or a stream-out the backend reports for it. Hot pink: outside every claim-about-the-air ink
 * (teal/lavender/grey), the user's own marks (amber/sky/white) and the live-edge yellow, so an
 * active box reads as "something is being done with this", never as a fourth kind of signal. It is
 * drawn as a separate HALO outside the box's own outline (shape, not hue alone, tells it from
 * `selected`, which is a heavier outline plus corner handles on the box itself).
 */
export const ACTIVE_MARK: readonly [number, number, number, number] = [0.96, 0.33, 0.62, 0.95];
/** The active halo's gap outside the box's outline, and its resting thickness, CSS px. */
export const ACTIVE_GAP_CSS_PX = 2;
export const ACTIVE_RING_CSS_PX = 2;
/** How much thicker the halo draws at full audio level (the server-reported level, 0..1), CSS px:
 * the "level pulse" — a presentation of a number the stream's status records carry, never a
 * measurement made here. */
export const ACTIVE_PULSE_CSS_PX = 3;

export interface MarkStyle {
  /** Edge thickness, device px. */
  strokePx?: number;
  /** Device px per CSS px (T-910). The symbology's lengths — dash, hatch, symbol, handles and the
   * generalization threshold — are CSS px, so a feature reads the same on a dpr-2 screen. */
  dpr?: number;
  /**
   * T-910's scale-dependent generalization threshold, CSS px. When set, a box carrying
   * [[MarkBox.symbology]] whose on-screen extent is under this in BOTH axes is drawn as a small
   * symbol at its centre, with the same symbology ([[isGeneralized]]). Absent: never generalize
   * (selections, measurements and the rubber band have no symbology and never do).
   */
  generalizeBelowPx?: number;
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

// ---- T-910: GIS feature symbolization (docs/23 §10.6 rule 6) ----
//
// The map is GIS, not Google Maps: a feature is drawn at its TRUE (t, f) extent, and a marker is a
// generalization of it, never the representation. So:
//
//  - **Symbology by attribute, on the polygon.** Confirmed = solid outline + a light hatch fill;
//    Candidate = dashed outline, no fill; unexplained = a plain thin outline (its '?' is the label
//    layer's, `pins.ts`); an explained artifact = thin grey; curated/human = a double outline. The
//    class is carried by outline and fill (and the label) — never by glyph shape, never by hue alone.
//  - **Scale-dependent generalization.** Under [[GENERALIZE_BELOW_CSS_PX]] in BOTH axes a feature
//    collapses to a small square symbol at the centre of its visible part, in the same symbology.
//    Both, not either: a single-frame impulse is by rule 6 "a thin bar of its measured bandwidth",
//    and a narrow carrier that has been on for an hour is a thin bar of its duration — collapsing
//    either to a dot would throw away the one extent it does have on screen. (docs/23's own words:
//    "Only when a box is under ~6 px on screen"; the user's handoff: "< 6 px in BOTH axes".)
//  - **Selected** = a heavier outline plus corner handles.
//
// Every one of these is still a stroke: the dashes and the hatch are cut by `overlay.ts`'s
// screen-door pattern, which DISCARDS off-pattern fragments, so the pixels between two hatch lines
// are the measurement exactly as the data pass drew it.

/** Below this many CSS px in BOTH axes a feature is drawn as its symbol, not its box. */
export const GENERALIZE_BELOW_CSS_PX = 6;
/** The generalized symbol's side, CSS px. */
export const SYMBOL_CSS_PX = 9;
/** A dashed outline: `DASH_ON` inked of every `DASH_PERIOD`, CSS px. */
export const DASH_ON_CSS_PX = 5;
export const DASH_PERIOD_CSS_PX = 8;
/** The light fill: 1 px diagonal hatch lines every `HATCH_PERIOD`, at `FILL_ALPHA` of the ink. */
export const HATCH_PERIOD_CSS_PX = 7;
export const HATCH_ON_CSS_PX = 1;
export const FILL_ALPHA = 0.45;
/** A selected feature's outline is this much heavier, CSS px, and carries corner handles. */
export const SELECTED_EXTRA_CSS_PX = 2;
export const HANDLE_LEN_CSS_PX = 8;
export const HANDLE_THICK_CSS_PX = 3;
/** Inner outline of a `double` (curated) outline: inset and thickness, CSS px. */
export const DOUBLE_GAP_CSS_PX = 2;

/** The feature classes symbology is keyed on. Presentation of served attributes only. */
export type FeatureClass = "confirmed" | "candidate" | "unexplained" | "artifact" | "curated";

export interface MarkSymbology {
  readonly cls: FeatureClass;
  readonly outline: "solid" | "dashed" | "double";
  /** A light hatch fill over the interior (a screen-door pattern, never a wash). */
  readonly fill: boolean;
}

/** The symbology table (docs/23 §10.6 rule 6; palette T-813). */
export const SYMBOLOGY: Readonly<Record<FeatureClass, MarkSymbology>> = {
  confirmed: { cls: "confirmed", outline: "solid", fill: true },
  candidate: { cls: "candidate", outline: "dashed", fill: false },
  unexplained: { cls: "unexplained", outline: "solid", fill: false },
  artifact: { cls: "artifact", outline: "solid", fill: false },
  curated: { cls: "curated", outline: "double", fill: false },
};

/** The generalization rule, in CSS px — the ONE predicate both the overlay pass (what is drawn) and
 * the pin layer (what is hit/focused, `pins.ts`) decide by, so they cannot disagree. */
export function isGeneralized(wCssPx: number, hCssPx: number, belowPx = GENERALIZE_BELOW_CSS_PX): boolean {
  return wCssPx < belowPx && hCssPx < belowPx;
}

/** One rectangle to stroke, in absolute surface coordinates. Presentation input only: every number
 * came off the API, and `t1Ns === null` means *open at the live edge*, never *unknown*. */
export interface MarkBox {
  readonly id: string;
  readonly kind: "signal-box" | "selection-box" | "pending-region" | "measurement-box" | "research-box";
  readonly f0Hz: number;
  readonly f1Hz: number;
  readonly t0Ns: number;
  /** `null` = still on the air: the box runs to the edge reported in. */
  readonly t1Ns: number | null;
  readonly rgba: readonly [number, number, number, number];
  /** Mark the newest edge as the live edge rather than a measured end. */
  readonly open: boolean;
  /** Per-box edge thickness (device px), overriding the style's: a Confirmed box is the stronger
   * claim and is drawn heavier than a Candidate (T-808). Presentation only. */
  readonly strokePx?: number;
  /** T-910: a feature's class symbology. Present on signal and research boxes; absent on the
   * user's own interaction marks (selections, measurements, the rubber band), which stay plain. */
  readonly symbology?: MarkSymbology;
  /** T-910: the selected feature — heavier outline and corner handles. */
  readonly selected?: boolean;
  /** T-994: the feature has an open output (served by the backend's open-output records), drawn as
   * an [[ACTIVE_MARK]] halo outside its outline. */
  readonly active?: boolean;
  /** T-994: the active audio output's level, 0..1 (from the server-reported dBFS), thickening the
   * halo — the level pulse. `null`/absent = no level to show (a decode, a recording). */
  readonly activeLevel?: number | null;
}

/** Confirmed edge / Candidate-and-artifact edge thickness, device px (T-808). */
export const CONFIRMED_STROKE_PX = 3;
export const CANDIDATE_STROKE_PX = 1;

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
 *
 * T-910: each box carries its class's [[SYMBOLOGY]]. Whether a row is *unexplained* is not this
 * file's question — `unexplained` is `pins.ts`'s `isUnexplained`, the one place that reads the
 * served family/explanations — so absent it, a row is its served state (or an artifact).
 */
export function signalMarkBoxes<R extends MarkRow>(
  rows: readonly R[], focusedId: string | null, unexplained?: (row: R) => boolean,
  active?: ReadonlyMap<string, { readonly level: number | null }>,
): MarkBox[] {
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
    const artifact = relKind === "artifact-of" || relKind === "retune-sibling-of";
    const base = artifact ? ARTIFACT_MARK : r.state === "confirmed" ? CONFIRMED_MARK : CANDIDATE_MARK;
    const cls: FeatureClass = artifact ? "artifact" : unexplained?.(r) ? "unexplained"
      : r.state === "confirmed" ? "confirmed" : "candidate";
    const band = r.user_band ?? null;
    out.push({
      id: r.id, kind: "signal-box",
      f0Hz: band ? band.f_lo : r.f_lo_hz,
      f1Hz: band ? band.f_hi : r.f_hi_hz,
      t0Ns: iv.t_start_s * S_TO_NS,
      t1Ns: iv.open ? null : iv.t_end_s * S_TO_NS,
      rgba: r.id === focusedId ? withAlpha(base, FOCUS_ALPHA) : base,
      open: iv.open,
      strokePx: base === CONFIRMED_MARK ? CONFIRMED_STROKE_PX : CANDIDATE_STROKE_PX,
      symbology: SYMBOLOGY[cls],
      selected: r.id === focusedId,
      ...(active?.has(r.id) ? { active: true, activeLevel: active.get(r.id)!.level } : {}),
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

/** A saved measurement as this file reads it (`GET /api/measurements`, T-822 / MAP-22), narrowed to
 * what a rectangle needs. A measurement with zero extent on an axis (a pure `delta_t` or a pure
 * `delta_f`) draws no box — the same rule a zero-width selection or signal box already follows —
 * and is still readable from the list/hover path, never fabricated a floor width to be seen. */
export interface MarkMeasurement {
  readonly id: string;
  readonly f_lo_hz: number;
  readonly f_hi_hz: number;
  readonly t0_s: number;
  readonly t1_s: number;
}

/** The rectangles for saved measurements. Never open: a measurement is two cursors the user placed,
 * with nothing left to grow at a live edge. */
export function measurementMarkBoxes(items: readonly MarkMeasurement[], focusedId: string | null): MarkBox[] {
  return items.map((m) => ({
    id: m.id, kind: "measurement-box",
    f0Hz: m.f_lo_hz, f1Hz: m.f_hi_hz,
    t0Ns: m.t0_s * S_TO_NS, t1Ns: m.t1_s * S_TO_NS,
    rgba: m.id === focusedId ? withAlpha(MEASUREMENT_MARK, FOCUS_ALPHA) : MEASUREMENT_MARK,
    open: false,
  }));
}

/**
 * Stroke `boxes` into one pane: four edges per rectangle, clipped to the pane, through the data
 * pass's own [[toClip]].
 *
 * A box wholly outside the pane draws nothing — a rectangle clamped to the pane's border would
 * claim a boundary the signal does not have (`paneOutlineQuads`' rule, one subject over). A box
 * only partly on screen draws only the edges that are genuinely on it.
 *
 * T-910: a box carrying [[MarkBox.symbology]] is drawn in its class's outline (solid / dashed /
 * double) and fill, and — with `style.generalizeBelowPx` set — as its symbol when it is under that
 * size in both axes. A selected feature gets a heavier outline and corner handles.
 */
export function markQuads(
  boxes: readonly MarkBox[], edgeNs: number, paneBox: Box, rect: PaneRect, style: MarkStyle = {},
): OverlayQuad[] {
  const strokePx = style.strokePx ?? 2;
  const openPx = style.openPx ?? 3;
  const minPx = style.minPx ?? 2;
  const k = style.dpr && style.dpr > 0 ? style.dpr : 1;
  const W = Math.max(1, rect.w), H = Math.max(1, rect.h);
  const minW = (2 * minPx) / W;
  const minH = (2 * minPx) / H;
  const out: OverlayQuad[] = [];
  for (const b of boxes) {
    const t1Ns = b.t1Ns ?? edgeNs;
    const sym = b.symbology;
    const spx = (b.strokePx ?? strokePx) + (b.selected && sym ? SELECTED_EXTRA_CSS_PX * k : 0);
    const sx = (2 * spx) / W;
    const sy = (2 * spx) / H;
    if (!(b.f1Hz > b.f0Hz) || !(t1Ns > b.t0Ns)) continue;
    let [x0, y0, x1, y1] = toClip({ f0Hz: b.f0Hz, f1Hz: b.f1Hz, t0Ns: b.t0Ns, t1Ns }, paneBox);
    const push = (clip: readonly [number, number, number, number], rgba: readonly [number, number, number, number],
      part: OverlayQuad["part"], pattern?: OverlayPattern) =>
      // A mark with no symbology (a selection, a measurement, the rubber band) is the plain stroke
      // it always was — no `part`, no pattern — so nothing about the user's own marks changes.
      out.push(!sym ? { clip, rgba, kind: b.kind, id: b.id }
        : pattern ? { clip, rgba, kind: b.kind, id: b.id, part, pattern } : { clip, rgba, kind: b.kind, id: b.id, part });
    // The generalization decision is made on the box's TRUE on-screen size (scale-dependent, not
    // pan-dependent), in CSS px — [[isGeneralized]], the predicate `pins.ts` also decides by.
    if (sym && style.generalizeBelowPx !== undefined
      && isGeneralized(((x1 - x0) / 2) * W / k, ((y1 - y0) / 2) * H / k, style.generalizeBelowPx)) {
      const vx0 = Math.max(x0, -1), vx1 = Math.min(x1, 1), vy0 = Math.max(y0, -1), vy1 = Math.min(y1, 1);
      if (vx1 < vx0 || vy1 < vy0) continue;
      const scx = (vx0 + vx1) / 2, scy = (vy0 + vy1) / 2;
      if (b.active) {
        const hx = (SYMBOL_CSS_PX * k) / W, hy = (SYMBOL_CSS_PX * k) / H;
        activeQuads(push, scx - hx, scy - hy, scx + hx, scy + hy, b.activeLevel ?? null, 2 * k, k, W, H);
      }
      symbolQuads(push, scx, scy, b, sym, spx, k, W, H);
      continue;
    }
    // The drawing floor. Widened about the centre, and never read back as a measurement.
    if (x1 - x0 < minW) { const m = (x0 + x1) / 2; x0 = m - minW / 2; x1 = m + minW / 2; }
    if (y1 - y0 < minH) { const m = (y0 + y1) / 2; y0 = m - minH / 2; y1 = m + minH / 2; }
    const cx0 = Math.max(x0, -1), cx1 = Math.min(x1, 1);
    const cy0 = Math.max(y0, -1), cy1 = Math.min(y1, 1);
    if (!(cx1 > cx0) || !(cy1 > cy0)) continue; // wholly off this pane: draw nothing, claim nothing
    const origin: readonly [number, number] = [x0, y0];
    // T-994: the active halo first, OUTSIDE the outline, so the feature's own symbology is untouched.
    if (sym && b.active) activeQuads(push, x0, y0, x1, y1, b.activeLevel ?? null, spx, k, W, H);
    const dashed = sym?.outline === "dashed";
    const dash = (along: "x" | "y"): OverlayPattern | undefined => dashed
      ? { mode: along === "x" ? "dash-x" : "dash-y", periodPx: DASH_PERIOD_CSS_PX * k, onPx: DASH_ON_CSS_PX * k, origin }
      : undefined;
    // A thin bar (one axis no thicker than two strokes — an impulse, a narrow carrier): one quad at
    // its true extent on the long axis, dashed along it when the class is dashed.
    if (sym && (x1 - x0 <= 2 * sx || y1 - y0 <= 2 * sy)) {
      const alongX = x1 - x0 > y1 - y0;
      push([cx0, cy0, cx1, cy1], b.rgba, "edge", dash(alongX ? "x" : "y"));
      if (b.selected) handleQuads(push, x0, y0, x1, y1, b.rgba, spx, k, W, H);
      continue;
    }
    // The light fill first, so the outline is drawn over it.
    if (sym?.fill) {
      push([cx0, cy0, cx1, cy1], withAlpha(b.rgba, b.rgba[3] * FILL_ALPHA), "fill",
        { mode: "hatch", periodPx: HATCH_PERIOD_CSS_PX * k, onPx: HATCH_ON_CSS_PX * k, origin });
    }
    const edge = (clip: readonly [number, number, number, number], rgba: readonly [number, number, number, number], pat?: OverlayPattern) =>
      push(clip, rgba, "edge", pat);
    if (x0 >= -1) edge([x0, cy0, Math.min(x0 + sx, cx1), cy1], b.rgba, dash("y"));
    if (x1 <= 1) edge([Math.max(x1 - sx, cx0), cy0, x1, cy1], b.rgba, dash("y"));
    if (y0 >= -1) edge([cx0, y0, cx1, Math.min(y0 + sy, cy1)], b.rgba, dash("x"));
    if (y1 <= 1) {
      // The newest edge. Open → this *is* the live edge, and it says so by being marked
      // differently from a measured end (ADR-0019's open cap, expressed as an edge rather than a
      // fill because this pass can only stroke). The live edge is always solid: it is a statement
      // about the air, not about the class.
      const h = (2 * (b.open ? Math.max(openPx, spx) : spx)) / H;
      edge([cx0, Math.max(y1 - h, cy0), cx1, y1], b.open ? OPEN_EDGE_MARK : b.rgba, b.open ? undefined : dash("x"));
    }
    if (sym?.outline === "double") {
      // Curated/human: a second, thinner outline just inside the first.
      const ix = (2 * (spx + DOUBLE_GAP_CSS_PX * k)) / W, iy = (2 * (spx + DOUBLE_GAP_CSS_PX * k)) / H;
      const tx = (2 * k) / W, ty = (2 * k) / H;
      const a0 = x0 + ix, a1 = x1 - ix, b0 = y0 + iy, b1 = y1 - iy;
      if (a1 - a0 > 2 * tx && b1 - b0 > 2 * ty) rectEdges(edge, a0, b0, a1, b1, tx, ty, b.rgba);
    }
    if (b.selected && sym) handleQuads(push, x0, y0, x1, y1, b.rgba, spx, k, W, H);
  }
  return out;
}

type PushQuad = (clip: readonly [number, number, number, number], rgba: readonly [number, number, number, number],
  part: OverlayQuad["part"], pattern?: OverlayPattern) => void;

/** The halo's thickness at a level (0..1, `null` = resting), CSS px. */
export function activeRingCssPx(level: number | null): number {
  const l = level === null || !Number.isFinite(level) ? 0 : Math.max(0, Math.min(1, level));
  return ACTIVE_RING_CSS_PX + l * ACTIVE_PULSE_CSS_PX;
}

/** T-994: the active halo — a solid [[ACTIVE_MARK]] ring just outside the rectangle `(x0,y0)-(x1,y1)`
 * (clip), one outline width (`spx`, device px) plus [[ACTIVE_GAP_CSS_PX]] out — so it also clears a
 * selected feature's corner handles, which sit one outline width out. Each edge off the pane is not
 * drawn, like every other mark's. */
function activeQuads(
  push: PushQuad, x0: number, y0: number, x1: number, y1: number, level: number | null,
  spx: number, k: number, W: number, H: number,
): void {
  const off = spx + ACTIVE_GAP_CSS_PX * k, th = activeRingCssPx(level) * k;
  const ox = (2 * (off + th)) / W, oy = (2 * (off + th)) / H, tx = (2 * th) / W, ty = (2 * th) / H;
  rectEdges((clip, rgba) => push(clip, rgba, "active"), x0 - ox, y0 - oy, x1 + ox, y1 + oy, tx, ty, ACTIVE_MARK);
}

/** Four edges of a rectangle in clip space, each clipped to the pane; an edge off the pane is not drawn. */
function rectEdges(
  edge: (clip: readonly [number, number, number, number], rgba: readonly [number, number, number, number], pat?: OverlayPattern) => void,
  x0: number, y0: number, x1: number, y1: number, tx: number, ty: number,
  rgba: readonly [number, number, number, number], pat?: (along: "x" | "y") => OverlayPattern | undefined,
): void {
  const cx0 = Math.max(x0, -1), cx1 = Math.min(x1, 1), cy0 = Math.max(y0, -1), cy1 = Math.min(y1, 1);
  if (!(cx1 > cx0) || !(cy1 > cy0)) return;
  if (x0 >= -1) edge([x0, cy0, Math.min(x0 + tx, cx1), cy1], rgba, pat?.("y"));
  if (x1 <= 1) edge([Math.max(x1 - tx, cx0), cy0, x1, cy1], rgba, pat?.("y"));
  if (y0 >= -1) edge([cx0, y0, cx1, Math.min(y0 + ty, cy1)], rgba, pat?.("x"));
  if (y1 <= 1) edge([cx0, Math.max(y1 - ty, cy0), cx1, y1], rgba, pat?.("x"));
}

/** A selected feature's corner handles: an L-bracket just outside each corner that is on the pane. */
function handleQuads(
  push: PushQuad, x0: number, y0: number, x1: number, y1: number,
  rgba: readonly [number, number, number, number], spx: number, k: number, W: number, H: number,
): void {
  const ink = withAlpha(rgba, FOCUS_ALPHA);
  const off = spx, len = HANDLE_LEN_CSS_PX * k, th = HANDLE_THICK_CSS_PX * k;
  const ox = (2 * off) / W, oy = (2 * off) / H, lx = (2 * len) / W, ly = (2 * len) / H, tx = (2 * th) / W, ty = (2 * th) / H;
  for (const [cx, sxg] of [[x0 - ox, 1], [x1 + ox, -1]] as const) {
    for (const [cy, syg] of [[y0 - oy, 1], [y1 + oy, -1]] as const) {
      if (cx < -1 || cx > 1 || cy < -1 || cy > 1) continue; // a corner off the pane has no handle
      const hx: readonly [number, number, number, number] = sxg > 0 ? [cx, cy, cx + lx, cy + syg * ty] : [cx - lx, cy, cx, cy + syg * ty];
      const vx: readonly [number, number, number, number] = sxg > 0 ? [cx, cy, cx + tx, cy + syg * ly] : [cx - tx, cy, cx, cy + syg * ly];
      for (const q of [hx, vx]) {
        push([Math.max(-1, Math.min(q[0], q[2])), Math.max(-1, Math.min(q[1], q[3])), Math.min(1, Math.max(q[0], q[2])), Math.min(1, Math.max(q[1], q[3]))], ink, "handle");
      }
    }
  }
}

/** A generalized feature: a [[SYMBOL_CSS_PX]] square at `(cx, cy)` (clip), in the class's outline
 * and fill, with handles when selected. The same symbology as its box, at a readable size. */
function symbolQuads(
  push: PushQuad, cx: number, cy: number, b: MarkBox, sym: MarkSymbology, spx: number, k: number, W: number, H: number,
): void {
  const hx = (SYMBOL_CSS_PX * k) / W, hy = (SYMBOL_CSS_PX * k) / H; // half-side, in clip units (2 * side/2 / W)
  const x0 = cx - hx, x1 = cx + hx, y0 = cy - hy, y1 = cy + hy;
  const origin: readonly [number, number] = [x0, y0];
  const th = Math.min(spx, 2 * k) + (b.selected ? SELECTED_EXTRA_CSS_PX * k : 0) / 2;
  const tx = (2 * Math.max(k, th)) / W, ty = (2 * Math.max(k, th)) / H;
  if (sym.fill) {
    push([Math.max(x0, -1), Math.max(y0, -1), Math.min(x1, 1), Math.min(y1, 1)], withAlpha(b.rgba, b.rgba[3] * FILL_ALPHA), "fill",
      { mode: "hatch", periodPx: 3 * k, onPx: HATCH_ON_CSS_PX * k, origin });
  }
  const dashed = sym.outline === "dashed";
  const edge = (clip: readonly [number, number, number, number], rgba: readonly [number, number, number, number], pat?: OverlayPattern) =>
    push(clip, rgba, "symbol", pat);
  rectEdges(edge, x0, y0, x1, y1, tx, ty, b.rgba, dashed
    ? (along) => ({ mode: along === "x" ? "dash-x" : "dash-y", periodPx: 4 * k, onPx: 2 * k, origin })
    : undefined);
  if (sym.outline === "double") {
    const ix = (2 * 3 * k) / W, iy = (2 * 3 * k) / H;
    rectEdges(edge, x0 + ix, y0 + iy, x1 - ix, y1 - iy, (2 * k) / W, (2 * k) / H, b.rgba);
  }
  if (b.selected) handleQuads(push, x0, y0, x1, y1, b.rgba, 2 * k, k, W, H);
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
