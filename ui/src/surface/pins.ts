// T-809 (MAP-09): pins — signal/event markers with rest / hover (MapTip) / selected states.
// The contract is docs/24 §14 (normative) and ADR-0023 §3.
//
// ## What a pin is, and what it may never claim
//
// A pin is a **glyph at a (time, frequency) place** that stands for one object the backend already
// served: an inventory row (a *detection marker*, churning with the live catalogue) or a durable
// marker a human placed (a *curated marker*, `/api/collections`, MAP-17/MAP-21). It is the point
// form of the same thing the detections layer strokes as a box; it adds no claim of its own.
//
//  - **Placement is in capture time and Hz**, laid out in the render frame through the pane's own
//    box and rect — the same `toClip` arithmetic the tiles and the boxes are placed by — so a pin
//    moves with pan/zoom exactly like the rows it sits on (the one-shared-time-axis invariant).
//  - **A pin never fabricates a timespan.** A detection's pin sits INSIDE its served interval
//    (`presence.last_interval`), near the newest visible edge of it (the live edge for an ongoing
//    signal, by the ADR-0019 assumption the box already makes). A row with no stated interval gets
//    no pin, exactly as it gets no box. A pin is a glyph, never a widened box (§14.3's rule).
//  - **Nothing here is signal logic.** Kind, centre, bandwidth, family and on-air are read off the
//    row as served. "Unknown" is a presentation of "the backend offered no explanation and no
//    family", not a judgement made here.
//
// ## Picking: DOM for focus, a CPU quadtree for the pointer
//
// Pins are DOM (`<button>`s in band 1) because a mark must be keyboard-focusable and carry an
// accessible name. The container and the pins are `pointer-events: none`, and the pointer's hover
// and click are resolved by [[PinIndex]] — the quadtree §14.4 names — from the canvas's own pointer
// handler. That keeps docs/23 §3's "the same gesture everywhere": a drag that starts on a pin still
// pans, a wheel over a pin still zooms, because the canvas is what the pointer is on. Keyboard focus
// does not need pointer events: Tab reaches the pins, focus shows the MapTip, Enter selects, arrow
// keys step to the neighbour in that direction ([[neighbourOf]]).
//
// Thin client: this file fetches nothing and commands nothing. Hovering, focusing and selecting a
// pin change presentation state only (the spy-client empty-call-list rule).
//
// ## T-910: this is the FEATURE layer now, and a pin is only a generalization
//
// User decision 2026-09-24 (docs/23 §10.6 rule 6): the map is GIS, not Google Maps. A feature is
// its (t, f) polygon — the box `marks.ts` draws in the overlay pass, in its class's symbology — and
// this file draws NO glyph for a detection at all. What it owns is everything about a feature that
// has to be DOM:
//
//  - **Identify = hit-test the polygon.** Each placed feature is an invisible, focusable button
//    over the VISIBLE PART of its box (the box clipped to the pane — never keyed on the centre, so a
//    wide emitter whose centre is off the pane is still hit where it is on it). A box too thin to
//    point at (a bar) gets a hit area padded about its own axis; a generalized feature (under the
//    size [[isGeneralized]] decides, the SAME predicate the overlay pass draws its symbol by) is
//    hit through the quadtree at its symbol.
//  - **Keyboard.** Tab walks features in READING order — newest time first (the top of the pane),
//    then frequency, left to right — because the buttons are kept in that DOM order.
//  - **Labels by placement rules** ([[placeLabels]]): `freq · bw · class` inside a box that fits it
//    or just above one wide enough; colliding labels are thinned by priority (Confirmed > Candidate
//    > unexplained), and an unexplained feature's label leads with '?' so the short form survives.

import type { Box } from "./lattice";
import type { PaneRect } from "./surface";
import { OUTPUT_KIND_GLYPH, OUTPUT_KIND_WORDS, activityWords, type FeatureActivity } from "./badges";
import { SYMBOL_CSS_PX, isGeneralized, type FeatureClass } from "./marks";

const S_TO_NS = 1e9;

/** docs/24 §14.4: the hard cap on pin elements per pane. Above it the pane draws the next coarser
 * cluster level (MAP-10); until that lands, the excess is **counted and stated**, never silently
 * dropped. */
export const PIN_CAP_PER_PANE = 400;
/** The hit radius, CSS px — half of §14.2's ≥ 24 px hit area. */
export const PIN_HIT_RADIUS_PX = 12;
/** How far below the newest visible edge of its interval a detection's pin sits, CSS px (the
 * mockup's `yTop + 10`), never further than the middle of the visible part of the interval. */
export const PIN_EDGE_INSET_PX = 10;

/**
 * GIS scale-dependent generalization (user decision 2026-09-24: "a signal has a frequency width
 * and a duration, that's a rectangle"). The threshold lives in `marks.ts` with the predicate
 * ([[isGeneralized]]: under it in BOTH axes), so what is drawn and what is hit decide alike.
 */
export { GENERALIZE_BELOW_CSS_PX as PIN_GENERALIZE_BELOW_PX } from "./marks";
/** A feature's hit area is never thinner than this, CSS px, on either axis: a bar a pointer cannot
 * land on is not identifiable. Padded about the bar's own centre line; never drawn. */
export const MIN_HIT_CSS_PX = 12;

/** docs/24 §14.2's glyph vocabulary. Shape carries the kind, so state is never hue alone. */
export type PinKind = "confirmed" | "candidate" | "unknown" | "curated";

/** What the MapTip shows: read off the served object, nothing derived. */
export interface PinTip {
  readonly centreHz: number;
  readonly bandwidthHz: number | null;
  /** The backend's family, else its top-ranked explanation's label — a suggestion, never truth. */
  readonly family: string | null;
  /** `true` = the interval is still open (on air, ADR-0019); `false` = an end was detected. */
  readonly onAir: boolean;
  /** Interval start / detected end, capture-clock ns (`endNs` null while on air). */
  readonly startNs: number | null;
  readonly endNs: number | null;
  /** A curated marker's name. */
  readonly name?: string;
}

export interface Pin {
  readonly id: string;
  readonly kind: PinKind;
  /** Detection markers render the live catalogue; curated markers are durable research objects. */
  readonly source: "detection" | "curated";
  readonly fHz: number;
  /** The feature's frequency extent — the same edges its detections-layer box is drawn from. A
   * curated marker is a point (`f0Hz === f1Hz`), so it always generalizes to its glyph. */
  readonly f0Hz: number;
  readonly f1Hz: number;
  /** The time extent the pin may sit in. A curated marker is an instant (`t0Ns === t1Ns`). `null`
   * = open at the live edge. */
  readonly t0Ns: number;
  readonly t1Ns: number | null;
  readonly tip: PinTip;
}

/** An inventory row as a pin reads it — `GET /api/inventory`'s shape, narrowed. */
export interface PinRow {
  readonly id: string;
  readonly state: string;
  readonly f_center_hz: number;
  readonly bandwidth_hz: number;
  readonly f_lo_hz: number;
  readonly f_hi_hz: number;
  readonly family?: string | null;
  readonly explanations?: readonly { readonly label: string }[] | null;
  readonly refined?: { readonly center_hz?: number | null } | null;
  readonly user_band?: { f_lo: number; f_hi: number } | null;
  readonly presence?: { last_interval?: { t_start_s: number; t_end_s: number; open: boolean } | null } | null;
  readonly relation?: { kind: string } | null;
}

/** A curated marker as a pin reads it (MAP-17's time–frequency marker). */
export interface PinMarker {
  readonly id: string;
  readonly name: string;
  readonly f_hz: number;
  readonly t_s: number;
}

/** The backend's family, else its top-ranked explanation's label — a suggestion, never truth. */
function familyOf(r: Pick<PinRow, "family" | "explanations">): string | null {
  return r.family ?? r.explanations?.[0]?.label ?? null;
}

/**
 * An UNEXPLAINED feature: a Candidate the backend offered neither a family nor an explanation for.
 * A Confirmed row is never demoted to it (the stronger claim stands). This is the one place the
 * surface asks — `marks.ts`'s symbology takes it as `signalMarkBoxes`' third argument — and it is a
 * presentation of "nothing was offered", not a judgement made here.
 */
export function isUnexplained(r: Pick<PinRow, "state" | "family" | "explanations">): boolean {
  return r.state !== "confirmed" && familyOf(r) === null;
}

/** A pin's kind as the symbology's feature class. */
export const PIN_CLASS: Readonly<Record<PinKind, FeatureClass>> = {
  confirmed: "confirmed", candidate: "candidate", unknown: "unexplained", curated: "curated",
};

/**
 * The detection pins for `rows`. The same filter the detections layer's boxes use
 * (`marks.ts`'s `signalMarkBoxes`): only Candidate/Confirmed rows with a stated interval, and a
 * `suppressed-by`/`duplicate-of` row stays unpinned — a stronger row already stands for it.
 */
export function detectionPins(rows: readonly PinRow[]): Pin[] {
  const out: Pin[] = [];
  for (const r of rows) {
    if (r.state !== "candidate" && r.state !== "confirmed") continue;
    const iv = r.presence?.last_interval;
    if (!iv) continue;
    const rel = r.relation?.kind;
    if (rel === "suppressed-by" || rel === "duplicate-of") continue;
    const family = familyOf(r);
    const kind: PinKind = r.state === "confirmed" ? "confirmed" : isUnexplained(r) ? "unknown" : "candidate";
    const centreHz = r.user_band
      ? (r.user_band.f_lo + r.user_band.f_hi) / 2
      : r.refined?.center_hz ?? r.f_center_hz;
    const bandwidthHz = r.user_band ? r.user_band.f_hi - r.user_band.f_lo : r.bandwidth_hz;
    const t0Ns = iv.t_start_s * S_TO_NS;
    const t1Ns = iv.open ? null : iv.t_end_s * S_TO_NS;
    out.push({
      id: r.id, kind, source: "detection", fHz: centreHz,
      f0Hz: r.user_band ? r.user_band.f_lo : r.f_lo_hz, f1Hz: r.user_band ? r.user_band.f_hi : r.f_hi_hz, t0Ns, t1Ns,
      tip: { centreHz, bandwidthHz, family, onAir: iv.open, startNs: t0Ns, endNs: t1Ns },
    });
  }
  return out;
}

/** The curated pins for `markers`: an instant at a frequency, placed exactly where it was put. */
export function curatedPins(markers: readonly PinMarker[]): Pin[] {
  return markers.map((m) => {
    const t = m.t_s * S_TO_NS;
    return {
      id: m.id, kind: "curated", source: "curated", fHz: m.f_hz, f0Hz: m.f_hz, f1Hz: m.f_hz, t0Ns: t, t1Ns: t,
      tip: { centreHz: m.f_hz, bandwidthHz: null, family: null, onAir: false, startNs: t, endNs: null, name: m.name },
    };
  });
}

/** A pin laid out in one pane, in CSS px relative to the canvas's top-left. */
export interface PlacedPin {
  readonly pin: Pin;
  readonly paneId: string;
  readonly x: number;
  readonly y: number;
  /** The feature's visible box on screen (CSS px) when it is at least
   * [[PIN_GENERALIZE_BELOW_PX]] in both axes: then there is NO glyph, and this rectangle is the
   * invisible hit/focus area. Absent/null = generalized to the glyph at `(x, y)`. */
  readonly area?: { readonly x0: number; readonly y0: number; readonly x1: number; readonly y1: number } | null;
  /** T-910: the VISIBLE part of the feature's box as drawn (CSS px), before any hit padding — the
   * rectangle a label is placed against. Absent for a generalized feature and a hand-built pin. */
  readonly drawn?: { readonly x0: number; readonly y0: number; readonly x1: number; readonly y1: number } | null;
}

export interface PaneLayout {
  readonly placed: PlacedPin[];
  /** The pane's rectangle, CSS px from the canvas's top-left — where its labels may go. */
  readonly bounds?: { readonly x0: number; readonly y0: number; readonly x1: number; readonly y1: number };
  /** Pins in the pane beyond [[PIN_CAP_PER_PANE]]: counted, stated, never silently dropped. */
  readonly overCap: number;
}

/** Draw priority under the cap: what the user curated, then the stronger claim. */
const PRIORITY: Record<PinKind, number> = { curated: 0, confirmed: 1, unknown: 2, candidate: 3 };

/**
 * Lay `pins` out in one pane. `rect` is the pane's rectangle in drawing-buffer px (GL convention,
 * y up, as `PaneView.rect`); the result is CSS px from the canvas's top-left, the coordinate the
 * HUD labels use (`hud.ts`'s `hudLabels`).
 *
 * A feature whose box does not intersect the pane is not placed — one clamped to the pane's border
 * would claim a place the object is not. One that does intersect is placed on the VISIBLE PART of
 * its box (T-910: the polygon clipped to the pane, not its centre — a wide emitter whose centre
 * frequency is off the pane is still hit where it is on it), so an ongoing signal that began
 * off-screen still has its hit area on the screen, inside its own box.
 *
 * Generalization is decided on the box's TRUE on-screen size by [[isGeneralized]] — the predicate
 * `marks.ts` draws its symbol by — so a feature is hit exactly as it is drawn: over its box, or at
 * its symbol (the centre of the visible part).
 */
export function layoutPanePins(
  pins: readonly Pin[], paneId: string, box: Box, rect: PaneRect,
  canvasHpx: number, dpr: number, edgeNs: number, cap = PIN_CAP_PER_PANE,
): PaneLayout {
  const k = dpr > 0 ? dpr : 1;
  const left = rect.x / k, top = (canvasHpx - (rect.y + rect.h)) / k;
  const w = rect.w / k, h = rect.h / k;
  const fSpan = box.f1Hz - box.f0Hz, tSpan = box.t1Ns - box.t0Ns;
  if (!(fSpan > 0) || !(tSpan > 0) || !(w > 0) || !(h > 0)) return { placed: [], overCap: 0 };
  const bounds = { x0: left, y0: top, x1: left + w, y1: top + h };
  // Newest time is at the TOP of the pane (clip y = +1 at t1Ns).
  const yOf = (tNs: number) => top + h * (1 - (tNs - box.t0Ns) / tSpan);
  const xOf = (f: number) => left + w * ((f - box.f0Hz) / fSpan);
  const inPane: PlacedPin[] = [];
  for (const p of pins) {
    const lo = Math.min(p.f0Hz, p.f1Hz), hi = Math.max(p.f0Hz, p.f1Hz);
    if (hi < box.f0Hz || lo > box.f1Hz) continue;
    const t1 = p.t1Ns ?? edgeNs;
    const vis0 = Math.max(p.t0Ns, box.t0Ns), vis1 = Math.min(t1, box.t1Ns);
    if (vis1 < vis0) continue;
    const yTop = yOf(vis1), yBot = yOf(vis0);
    const x0 = xOf(Math.max(lo, box.f0Hz)), x1 = xOf(Math.min(hi, box.f1Hz));
    // The TRUE size, not the visible sliver: generalization is scale-dependent, never pan-dependent.
    const wFull = w * ((hi - lo) / fSpan), hFull = h * ((t1 - p.t0Ns) / tSpan);
    const xc = (x0 + x1) / 2, yc = (yTop + yBot) / 2;
    if (isGeneralized(wFull, hFull)) {
      inPane.push({ pin: p, paneId, x: xc, y: yc, area: null, drawn: null });
      continue;
    }
    // A bar too thin to point at is padded about its own centre line — for the hit area only.
    const pad = (a: number, b: number, c: number) => (b - a >= MIN_HIT_CSS_PX ? [a, b] : [c - MIN_HIT_CSS_PX / 2, c + MIN_HIT_CSS_PX / 2]);
    const [ax0, ax1] = pad(x0, x1, xc), [ay0, ay1] = pad(yTop, yBot, yc);
    inPane.push({
      pin: p, paneId, x: xc, y: Math.min(yTop + PIN_EDGE_INSET_PX, yc),
      area: { x0: ax0, y0: ay0, x1: ax1, y1: ay1 }, drawn: { x0, y0: yTop, x1, y1: yBot },
    });
  }
  if (inPane.length <= cap) return { placed: inPane, overCap: 0, bounds };
  inPane.sort((a, b) => PRIORITY[a.pin.kind] - PRIORITY[b.pin.kind]);
  return { placed: inPane.slice(0, cap), overCap: inPane.length - cap, bounds };
}

/** Tab order (T-910): reading order — newest time first (top of the pane), then frequency left to
 * right; panes in the order they were laid out. Returns a new array. */
export function readingOrder(placed: readonly PlacedPin[]): PlacedPin[] {
  const paneRank = new Map<string, number>();
  for (const p of placed) if (!paneRank.has(p.paneId)) paneRank.set(p.paneId, paneRank.size);
  const top = (p: PlacedPin) => (p.drawn ? p.drawn.y0 : p.y);
  const leftOf = (p: PlacedPin) => (p.drawn ? p.drawn.x0 : p.x);
  return [...placed].sort((a, b) => (paneRank.get(a.paneId)! - paneRank.get(b.paneId)!)
    || (top(a) - top(b)) || (leftOf(a) - leftOf(b)) || (a.pin.id < b.pin.id ? -1 : a.pin.id > b.pin.id ? 1 : 0));
}

// ---- labels by placement rules (T-910, docs/23 §10.6 rule 6) ----

/** Label metrics, CSS px: a 10 px monospace face (`centre.css`'s `.sf-flabel`). */
export const LABEL_CHAR_PX = 6.1;
export const LABEL_H_PX = 13;
export const LABEL_PAD_PX = 3;

export interface PlacedLabel {
  readonly key: string;
  readonly pinId: string;
  readonly paneId: string;
  readonly text: string;
  readonly cls: FeatureClass;
  /** `inside` the box's top-left, `above` its top edge, or `beside` a generalized symbol. */
  readonly where: "inside" | "above" | "beside";
  readonly x: number;
  readonly y: number;
  readonly w: number;
  readonly h: number;
}

/** Label priority: Confirmed > Candidate > unexplained (docs/23 §10.6 rule 6). Lower wins. */
const LABEL_PRIORITY: Record<PinKind, number> = { confirmed: 0, candidate: 1, unknown: 2, curated: 1 };

/** A feature's label text: `freq · bw · class`. An unexplained feature leads with '?'. */
export function labelText(p: Pin): string {
  const bw = p.tip.bandwidthHz != null && p.tip.bandwidthHz > 0 ? fmtBw(p.tip.bandwidthHz) : null;
  const cls = p.kind === "unknown" ? null : p.tip.family ?? (p.kind === "confirmed" ? "confirmed" : p.kind === "curated" ? p.tip.name ?? "marker" : "candidate");
  const body = [fmtMHz(p.tip.centreHz, 3), bw, cls].filter(Boolean).join(" · ");
  return p.kind === "unknown" ? `? ${body}` : body;
}

const labelW = (t: string) => t.length * LABEL_CHAR_PX + 2 * LABEL_PAD_PX;

/**
 * Where each feature's label goes, if anywhere. The rules, in order, per feature (highest priority
 * first, the selected one before all):
 *
 *  1. **Inside** the top-left of its visible box, when the box fits the full label.
 *  2. **Just above** the box's top edge, when the box is at least as wide as the label (and there is
 *     room under the pane's top).
 *  3. An unexplained feature too small for either keeps its **'?'** alone, inside or beside.
 *  4. A label that would overlap one already placed, or leave its pane, is **thinned** (dropped).
 *
 * Pure layout over [[layoutPanePins]]' output: no text is measured, so the width is the monospace
 * estimate [[LABEL_CHAR_PX]] — a label is dropped rather than risk overrunning its box.
 */
export function placeLabels(layouts: readonly PaneLayout[], selectedId: string | null = null): PlacedLabel[] {
  const out: PlacedLabel[] = [];
  for (const l of layouts) {
    const b = l.bounds;
    if (!b) continue;
    const taken: PlacedLabel[] = [];
    const fits = (r: { x: number; y: number; w: number; h: number }) =>
      r.x >= b.x0 && r.y >= b.y0 && r.x + r.w <= b.x1 && r.y + r.h <= b.y1
      && !taken.some((q) => r.x < q.x + q.w && q.x < r.x + r.w && r.y < q.y + q.h && q.y < r.y + r.h);
    const order = [...l.placed].sort((a, c) => (a.pin.id === selectedId ? -1 : 0) - (c.pin.id === selectedId ? -1 : 0)
      || LABEL_PRIORITY[a.pin.kind] - LABEL_PRIORITY[c.pin.kind]
      || area(c) - area(a));
    for (const p of order) {
      const cls = PIN_CLASS[p.pin.kind];
      const key = `${p.paneId}|${p.pin.id}`;
      const mk = (text: string, where: PlacedLabel["where"], x: number, y: number): PlacedLabel =>
        ({ key, pinId: p.pin.id, paneId: p.paneId, text, cls, where, x, y, w: labelW(text), h: LABEL_H_PX });
      const tries: PlacedLabel[] = [];
      const d = p.drawn;
      const full = labelText(p.pin);
      if (d) {
        const bw = d.x1 - d.x0, bh = d.y1 - d.y0;
        if (bw >= labelW(full) + 2 && bh >= LABEL_H_PX + 2) tries.push(mk(full, "inside", d.x0 + 1, d.y0 + 1));
        if (bw >= labelW(full)) tries.push(mk(full, "above", d.x0, d.y0 - LABEL_H_PX - 1));
        if (p.pin.kind === "unknown") {
          if (bw >= labelW("?") + 2 && bh >= LABEL_H_PX + 2) tries.push(mk("?", "inside", d.x0 + 1, d.y0 + 1));
          tries.push(mk("?", "above", d.x0, d.y0 - LABEL_H_PX - 1));
        }
      } else if (p.pin.kind === "unknown") {
        tries.push(mk("?", "beside", p.x + 7, p.y - LABEL_H_PX / 2));
      }
      const got = tries.find(fits);
      if (got) { taken.push(got); out.push(got); }
    }
  }
  return out;
}

const area = (p: PlacedPin) => (p.drawn ? (p.drawn.x1 - p.drawn.x0) * (p.drawn.y1 - p.drawn.y0) : 0);

// ---- picking: a point quadtree over the laid-out set (docs/24 §14.4) ----

interface QNode {
  x0: number; y0: number; x1: number; y1: number;
  items: PlacedPin[] | null;
  kids: QNode[] | null;
}

const LEAF_MAX = 8;
const MAX_DEPTH = 12;

/**
 * A CPU point quadtree over one frame's placed pins, rebuilt in the frame that lays them out. It
 * answers "which pin is under this pointer" in O(log n) instead of a scan, which is what makes the
 * cap's per-frame budget predictable (§14.4's 2 ms trigger for a GPU pass).
 */
export class PinIndex {
  private readonly root: QNode | null;
  readonly size: number;

  constructor(pins: readonly PlacedPin[]) {
    this.size = pins.length;
    if (pins.length === 0) { this.root = null; return; }
    let x0 = Infinity, y0 = Infinity, x1 = -Infinity, y1 = -Infinity;
    for (const p of pins) {
      x0 = Math.min(x0, p.x); y0 = Math.min(y0, p.y); x1 = Math.max(x1, p.x); y1 = Math.max(y1, p.y);
    }
    this.root = { x0, y0, x1: Math.max(x1, x0 + 1), y1: Math.max(y1, y0 + 1), items: [], kids: null };
    for (const p of pins) insert(this.root, p, 0);
  }

  /** The pin nearest `(x, y)` within `radius` CSS px, or null. Ties go to the higher-priority kind
   * (a curated marker over a detection, a Confirmed over a Candidate). */
  pick(x: number, y: number, radius = PIN_HIT_RADIUS_PX): PlacedPin | null {
    if (!this.root) return null;
    let best: PlacedPin | null = null, bestD = radius * radius;
    const visit = (n: QNode) => {
      if (x + radius < n.x0 || x - radius > n.x1 || y + radius < n.y0 || y - radius > n.y1) return;
      if (n.items) {
        for (const p of n.items) {
          const d = (p.x - x) ** 2 + (p.y - y) ** 2;
          if (d < bestD || (d === bestD && (!best || PRIORITY[p.pin.kind] < PRIORITY[best.pin.kind]))) { best = p; bestD = d; }
        }
      } else if (n.kids) for (const c of n.kids) visit(c);
    };
    visit(this.root);
    return best;
  }
}

function insert(n: QNode, p: PlacedPin, depth: number): void {
  if (n.items) {
    n.items.push(p);
    if (n.items.length <= LEAF_MAX || depth >= MAX_DEPTH) return;
    const items = n.items;
    const mx = (n.x0 + n.x1) / 2, my = (n.y0 + n.y1) / 2;
    n.items = null;
    n.kids = [
      { x0: n.x0, y0: n.y0, x1: mx, y1: my, items: [], kids: null },
      { x0: mx, y0: n.y0, x1: n.x1, y1: my, items: [], kids: null },
      { x0: n.x0, y0: my, x1: mx, y1: n.y1, items: [], kids: null },
      { x0: mx, y0: my, x1: n.x1, y1: n.y1, items: [], kids: null },
    ];
    for (const q of items) insert(n, q, depth);
    return;
  }
  const mx = (n.x0 + n.x1) / 2, my = (n.y0 + n.y1) / 2;
  insert(n.kids![(p.x >= mx ? 1 : 0) + (p.y >= my ? 2 : 0)], p, depth + 1);
}

// ---- keyboard: arrow keys step to the neighbour in that direction ----

export type PinDirection = "left" | "right" | "up" | "down";

/**
 * The placed pin nearest `from` in `dir` (Google's marker-accessibility pattern, docs/24 §5): only
 * pins strictly on that side count, and the off-axis distance weighs double so "right" prefers the
 * pin level with you over one far above. Same pane only — a pane is where you look from.
 */
export function neighbourOf(placed: readonly PlacedPin[], from: PlacedPin, dir: PinDirection): PlacedPin | null {
  let best: PlacedPin | null = null, bestScore = Infinity;
  for (const p of placed) {
    if (p === from || p.paneId !== from.paneId) continue;
    const dx = p.x - from.x, dy = p.y - from.y;
    const along = dir === "right" ? dx : dir === "left" ? -dx : dir === "down" ? dy : -dy;
    if (!(along > 0)) continue;
    const off = dir === "left" || dir === "right" ? Math.abs(dy) : Math.abs(dx);
    const score = along + 2 * off;
    if (score < bestScore) { best = p; bestScore = score; }
  }
  return best;
}

// ---- words: the accessible name and the MapTip ----

const fmtMHz = (hz: number, digits = 4) => `${(hz / 1e6).toFixed(digits)} MHz`;
const fmtBw = (hz: number) => (hz >= 1e6 ? `${(hz / 1e6).toFixed(2)} MHz` : hz >= 1e3 ? `${(hz / 1e3).toFixed(1)} kHz` : `${Math.round(hz)} Hz`);
/** Capture-clock instant as UTC — the surface's own `at` idiom; never a browser clock (T-393). */
const utc = (ns: number) => `${new Date(ns / 1e6).toISOString().slice(11, 19)}Z`;

const KIND_WORD: Record<PinKind, string> = {
  confirmed: "confirmed signal", candidate: "candidate signal", unknown: "unexplained signal", curated: "my marker",
};

/** The pin's accessible name: kind in words (never hue alone), then where. */
export function pinLabel(p: Pin): string {
  const name = p.tip.name ? `${p.tip.name}, ` : "";
  return `${name}${KIND_WORD[p.kind]} at ${fmtMHz(p.tip.centreHz)}`;
}

/** The MapTip's lines: centre / bandwidth / family suggestion / on-air (§14.2). */
export function pinTipLines(p: Pin): string[] {
  const lines = [p.tip.name ? `${p.tip.name} · ${fmtMHz(p.tip.centreHz)}` : fmtMHz(p.tip.centreHz)];
  const bw = p.tip.bandwidthHz != null && p.tip.bandwidthHz > 0 ? `bw ${fmtBw(p.tip.bandwidthHz)}` : null;
  lines.push([KIND_WORD[p.kind], bw].filter(Boolean).join(" · "));
  if (p.source === "detection") {
    lines.push(p.tip.family ? `suggests ${p.tip.family} (a suggestion, not a finding)` : "no explanation yet");
    lines.push(p.tip.onAir
      ? `on air since ${p.tip.startNs != null ? utc(p.tip.startNs) : "?"}`
      : `ended ${p.tip.endNs != null ? utc(p.tip.endNs) : "?"}`);
  } else {
    lines.push(`placed at ${p.tip.startNs != null ? utc(p.tip.startNs) : "?"}`);
  }
  return lines;
}

// ---- the DOM layer (band 1) ----

export interface PinLayerHooks {
  /** A pin gained keyboard focus (or lost it: `null`) — the caller shows or hides its MapTip. */
  onFocus?: (p: PlacedPin | null) => void;
  /** Enter / Space on a focused pin, or a click delivered to the element itself (an assistive
   * technology's activation). */
  onSelect?: (p: PlacedPin) => void;
}

/**
 * The band-1 pin layer: one `<button>` per placed pin, pooled by `paneId|pinId` so a 60 Hz frame
 * moves existing elements rather than rebuilding them (and so keyboard focus survives a frame).
 * Positioned only by [[update]], which the caller runs inside the render frame.
 */
export class PinLayer {
  private readonly els = new Map<string, HTMLButtonElement>();
  private readonly byEl = new WeakMap<HTMLElement, PlacedPin>();
  private readonly overEl: HTMLElement;
  private readonly labelsEl: HTMLElement;
  private readonly labelEls = new Map<string, HTMLElement>();
  private placed: PlacedPin[] = [];
  private index = new PinIndex([]);
  private focusedKey: string | null = null;
  /** The button keys in the DOM order last applied — reading order (T-910). */
  private domOrder: string[] = [];
  /** Set while buttons are being re-ordered, so the blur/focus a move causes is not reported. */
  private reordering = false;
  private labelsNow: PlacedLabel[] = [];
  private readonly badgesEl: HTMLElement;
  private readonly badgeEls = new Map<string, HTMLElement>();

  constructor(private readonly root: HTMLElement, private readonly hooks: PinLayerHooks = {}) {
    this.overEl = root.ownerDocument.createElement("div");
    this.overEl.className = "sf-pins-over";
    this.overEl.setAttribute("role", "status");
    this.overEl.hidden = true;
    root.appendChild(this.overEl);
    // T-910: the feature labels — text only, never a pointer or focus target (the button beside it
    // carries the accessible name), laid out in the same per-frame pass as the buttons.
    this.labelsEl = root.ownerDocument.createElement("div");
    this.labelsEl.className = "sf-flabels";
    this.labelsEl.setAttribute("aria-hidden", "true");
    root.appendChild(this.labelsEl);
    // T-994: the active-output badges — text only (the button beside it names the outputs in its
    // accessible name), laid out in the same per-frame pass as the buttons.
    this.badgesEl = root.ownerDocument.createElement("div");
    this.badgesEl.className = "sf-obadges";
    this.badgesEl.setAttribute("aria-hidden", "true");
    root.appendChild(this.badgesEl);
  }

  /** Re-lay the layer out for this frame. `hoveredId`/`selectedId` style the states; `activity`
   * (T-994) is each feature's open outputs, by id — the backend's records, read by the caller. */
  update(layouts: readonly PaneLayout[], hoveredId: string | null, selectedId: string | null,
    activity?: ReadonlyMap<string, FeatureActivity>, selectedPaneId?: string | null): void {
    const doc = this.root.ownerDocument;
    this.placed = layouts.flatMap((l) => l.placed);
    this.index = new PinIndex(this.placed.filter((p) => !p.area));
    const seen = new Set<string>();
    for (const p of this.placed) {
      const key = `${p.paneId}|${p.pin.id}`;
      seen.add(key);
      let el = this.els.get(key);
      if (!el) {
        el = doc.createElement("button");
        el.type = "button";
        el.dataset.pin = p.pin.id;
        el.dataset.pane = p.paneId;
        el.addEventListener("focus", () => {
          if (this.reordering) return;
          this.focusedKey = key; const q = this.byEl.get(el!); if (q) this.hooks.onFocus?.(q);
        });
        el.addEventListener("blur", () => {
          if (this.reordering) return;
          if (this.focusedKey === key) this.focusedKey = null; this.hooks.onFocus?.(null);
        });
        el.addEventListener("click", () => { const q = this.byEl.get(el!); if (q) this.hooks.onSelect?.(q); });
        el.addEventListener("keydown", (e) => this.onKey(e, el!));
        this.root.appendChild(el);
        this.els.set(key, el);
      }
      this.byEl.set(el, p);
      // T-910: a detection never draws a DOM glyph — the overlay pass draws its box or, generalized,
      // its symbol. `area` = a hit area over the box; `symbol` = the hit/focus point over the symbol.
      const form = p.area ? " area" : p.pin.source === "detection" ? " symbol" : "";
      const act = activity?.get(p.pin.id);
      // T-1004: selection is one piece of page state, but it was MADE in one pane. In any other
      // pane the same feature is the selection's linked ghost (`marks.ts` draws the faint brackets),
      // never a second selection — so with a split view the user can see where the selection is and
      // where it is merely mirrored. `selectedPaneId` absent (a single-pane host) = the old
      // behaviour exactly: the selected feature is selected wherever it is placed.
      const owns = selectedPaneId === undefined || selectedPaneId === null || p.paneId === selectedPaneId;
      const sel = p.pin.id === selectedId && owns;
      const linked = p.pin.id === selectedId && !owns;
      const cls = `sf-pin ${p.pin.kind} ${p.pin.source}${form}`
        + (p.pin.id === hoveredId ? " hovered" : "") + (sel ? " selected" : "") + (linked ? " linked" : "") + (act ? " active" : "");
      if (el.className !== cls) el.className = cls;
      const base = linked ? `${pinLabel(p.pin)} · linked: selected in another viewport` : pinLabel(p.pin);
      const label = act ? `${base} · active: ${activityWords(act)}` : base;
      if (el.getAttribute("aria-label") !== label) el.setAttribute("aria-label", label);
      const pressed = String(sel);
      if (el.getAttribute("aria-pressed") !== pressed) el.setAttribute("aria-pressed", pressed);
      if (p.area) {
        // No glyph: an invisible, focusable hit area exactly over the box the detections layer draws.
        el.style.transform = `translate(${p.area.x0.toFixed(1)}px, ${p.area.y0.toFixed(1)}px)`;
        el.style.width = `${(p.area.x1 - p.area.x0).toFixed(1)}px`;
        el.style.height = `${(p.area.y1 - p.area.y0).toFixed(1)}px`;
      } else {
        el.style.transform = `translate(${p.x.toFixed(1)}px, ${p.y.toFixed(1)}px)`;
        if (el.style.width) { el.style.width = ""; el.style.height = ""; }
      }
    }
    for (const [key, el] of this.els) {
      if (seen.has(key)) continue;
      if (this.focusedKey === key) { this.focusedKey = null; this.hooks.onFocus?.(null); }
      el.remove();
      this.els.delete(key);
    }
    this.applyReadingOrder();
    this.updateLabels(layouts, selectedId);
    this.updateBadges(activity);
    const over = layouts.reduce((n, l) => n + l.overCap, 0);
    const text = over > 0 ? `${over} more marker${over === 1 ? "" : "s"} in view than can be pinned — zoom in to resolve them` : "";
    if (this.overEl.textContent !== text) this.overEl.textContent = text;
    this.overEl.hidden = over === 0;
  }

  /**
   * Keep the buttons in reading order in the DOM, so a native Tab walks features newest-first, then
   * left to right (T-910). Under a pan or zoom every feature moves together and the order does not
   * change, so this touches the DOM only when features appear, go, or cross — and it restores the
   * focus a move would drop without reporting a blur/focus pair for it.
   */
  private applyReadingOrder(): void {
    const want = readingOrder(this.placed).map((p) => `${p.paneId}|${p.pin.id}`);
    if (want.length === this.domOrder.length && want.every((k, i) => k === this.domOrder[i])) return;
    const focused = this.focusedKey ? this.els.get(this.focusedKey) ?? null : null;
    this.reordering = true;
    try {
      for (const k of want) { const el = this.els.get(k); if (el) this.root.appendChild(el); }
      if (focused) focused.focus({ preventScroll: true });
    } finally { this.reordering = false; }
    this.domOrder = want;
  }

  private updateLabels(layouts: readonly PaneLayout[], selectedId: string | null): void {
    const doc = this.root.ownerDocument;
    this.labelsNow = placeLabels(layouts, selectedId);
    const seen = new Set<string>();
    for (const l of this.labelsNow) {
      seen.add(l.key);
      let el = this.labelEls.get(l.key);
      if (!el) {
        el = doc.createElement("span");
        this.labelsEl.appendChild(el);
        this.labelEls.set(l.key, el);
      }
      const cls = `sf-flabel ${l.cls} ${l.where}${l.pinId === selectedId ? " selected" : ""}`;
      if (el.className !== cls) el.className = cls;
      if (el.textContent !== l.text) el.textContent = l.text;
      el.dataset.pin = l.pinId;
      el.style.transform = `translate(${l.x.toFixed(1)}px, ${l.y.toFixed(1)}px)`;
    }
    for (const [key, el] of this.labelEls) {
      if (seen.has(key)) continue;
      el.remove();
      this.labelEls.delete(key);
    }
  }

  /**
   * T-994: one badge per placed feature with an open output, at the TOP-RIGHT corner inside its
   * drawn box (the newest edge — a live signal's top is the pane's live edge, so a badge above it
   * would leave the pane), or beside a generalized symbol. One glyph per output kind; the audio
   * glyph pulses with the server-reported level through `--lvl` (0..1).
   */
  private updateBadges(activity: ReadonlyMap<string, FeatureActivity> | undefined): void {
    const doc = this.root.ownerDocument;
    const seen = new Set<string>();
    if (activity && activity.size > 0) {
      for (const p of this.placed) {
        const act = activity.get(p.pin.id);
        if (!act || act.kinds.length === 0) continue;
        const key = `${p.paneId}|${p.pin.id}`;
        seen.add(key);
        let el = this.badgeEls.get(key);
        if (!el) {
          el = doc.createElement("span");
          el.dataset.pin = p.pin.id;
          el.dataset.pane = p.paneId;
          this.badgesEl.appendChild(el);
          this.badgeEls.set(key, el);
        }
        const kinds = act.kinds.join(" ");
        if (el.dataset.kinds !== kinds) {
          el.dataset.kinds = kinds;
          el.className = `sf-obadge ${p.area ? "box" : "symbol"}`;
          el.title = activityWords(act);
          el.replaceChildren(...act.kinds.map((k) => {
            const g = doc.createElement("i");
            g.className = `k-${k}`;
            g.textContent = OUTPUT_KIND_GLYPH[k];
            g.title = OUTPUT_KIND_WORDS[k];
            return g;
          }));
        }
        const lvl = act.level === null ? "0" : act.level.toFixed(2);
        if (el.style.getPropertyValue("--lvl") !== lvl) el.style.setProperty("--lvl", lvl);
        const x = p.drawn ? p.drawn.x1 - 2 : p.x + SYMBOL_CSS_PX + 2;
        const y = p.drawn ? p.drawn.y0 + 2 : p.y - SYMBOL_CSS_PX;
        // Right-aligned against the box's right edge (translateX(-100%)); beside a symbol, to its right.
        el.style.transform = p.drawn
          ? `translate(${x.toFixed(1)}px, ${y.toFixed(1)}px) translateX(-100%)`
          : `translate(${x.toFixed(1)}px, ${y.toFixed(1)}px)`;
      }
    }
    for (const [key, el] of this.badgeEls) {
      if (seen.has(key)) continue;
      el.remove();
      this.badgeEls.delete(key);
    }
  }

  /** The active badges placed this frame, by `paneId|pinId` (tests). */
  get badges(): ReadonlyMap<string, HTMLElement> { return this.badgeEls; }

  /** The labels placed this frame (tests). */
  get labels(): readonly PlacedLabel[] { return this.labelsNow; }

  /** The feature under a pointer, CSS px from the canvas's top-left — not the DOM. A generalized
   * glyph (the quadtree) wins, being the more specific target; otherwise anywhere inside a box
   * hits that feature, the smallest box first where boxes nest. */
  pick(x: number, y: number): PlacedPin | null {
    const glyph = this.index.pick(x, y);
    if (glyph) return glyph;
    let best: PlacedPin | null = null, bestA = Infinity;
    for (const p of this.placed) {
      const a = p.area;
      if (!a || x < a.x0 || x > a.x1 || y < a.y0 || y > a.y1) continue;
      const size = (a.x1 - a.x0) * (a.y1 - a.y0);
      if (size < bestA) { best = p; bestA = size; }
    }
    return best;
  }

  /** Every placed pin this frame (tests, and a screen reader's summary). */
  get pins(): readonly PlacedPin[] { return this.placed; }

  /** The element standing for a pin, if placed. */
  element(paneId: string, pinId: string): HTMLButtonElement | null { return this.els.get(`${paneId}|${pinId}`) ?? null; }

  private onKey(e: KeyboardEvent, el: HTMLButtonElement): void {
    const from = this.byEl.get(el);
    if (!from) return;
    const dir: PinDirection | null = e.key === "ArrowLeft" ? "left" : e.key === "ArrowRight" ? "right"
      : e.key === "ArrowUp" ? "up" : e.key === "ArrowDown" ? "down" : null;
    if (!dir) return; // Enter/Space activate the button natively → the click listener selects.
    e.preventDefault();
    const to = neighbourOf(this.placed, from, dir);
    if (to) this.element(to.paneId, to.pin.id)?.focus();
  }

  dispose(): void {
    for (const el of this.els.values()) el.remove();
    this.els.clear();
    this.labelEls.clear();
    this.labelsEl.remove();
    this.badgeEls.clear();
    this.badgesEl.remove();
    this.overEl.remove();
  }
}
