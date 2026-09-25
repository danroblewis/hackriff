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

import type { Box } from "./lattice";
import type { PaneRect } from "./surface";

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
 * and a duration, that's a rectangle"). A feature whose box is at least this many CSS px on screen
 * in BOTH axes is represented by its box — the detections layer (T-808) strokes it — and the pin
 * draws NO glyph there, only an invisible hit area over the box. Below it in either axis the box is
 * too small to read, and the feature generalizes to the glyph at its centre.
 */
export const PIN_GENERALIZE_BELOW_PX = 6;

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
    const family = r.family ?? r.explanations?.[0]?.label ?? null;
    const kind: PinKind = r.state === "confirmed" ? "confirmed" : family === null ? "unknown" : "candidate";
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
}

export interface PaneLayout {
  readonly placed: PlacedPin[];
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
 * A pin whose frequency or interval does not intersect the pane is not placed — a pin clamped to
 * the pane's border would claim a place the object is not. A detection's interval that does
 * intersect is clamped to the visible part of itself, so an ongoing signal that began off-screen
 * still has its pin on the screen, inside its own box.
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
  // Newest time is at the TOP of the pane (clip y = +1 at t1Ns).
  const yOf = (tNs: number) => top + h * (1 - (tNs - box.t0Ns) / tSpan);
  const inPane: PlacedPin[] = [];
  for (const p of pins) {
    if (p.fHz < box.f0Hz || p.fHz > box.f1Hz) continue;
    const t1 = p.t1Ns ?? edgeNs;
    const vis0 = Math.max(p.t0Ns, box.t0Ns), vis1 = Math.min(t1, box.t1Ns);
    if (vis1 < vis0) continue;
    const yTop = yOf(vis1), yBot = yOf(vis0);
    const y = Math.min(yTop + PIN_EDGE_INSET_PX, (yTop + yBot) / 2);
    const xOf = (f: number) => left + w * ((f - box.f0Hz) / fSpan);
    const x0 = xOf(Math.max(Math.min(p.f0Hz, p.f1Hz), box.f0Hz)), x1 = xOf(Math.min(Math.max(p.f0Hz, p.f1Hz), box.f1Hz));
    const big = x1 - x0 >= PIN_GENERALIZE_BELOW_PX && yBot - yTop >= PIN_GENERALIZE_BELOW_PX;
    inPane.push({ pin: p, paneId, x: xOf(p.fHz), y, area: big ? { x0, y0: yTop, x1, y1: yBot } : null });
  }
  if (inPane.length <= cap) return { placed: inPane, overCap: 0 };
  inPane.sort((a, b) => PRIORITY[a.pin.kind] - PRIORITY[b.pin.kind]);
  return { placed: inPane.slice(0, cap), overCap: inPane.length - cap };
}

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
  private placed: PlacedPin[] = [];
  private index = new PinIndex([]);
  private focusedKey: string | null = null;

  constructor(private readonly root: HTMLElement, private readonly hooks: PinLayerHooks = {}) {
    this.overEl = root.ownerDocument.createElement("div");
    this.overEl.className = "sf-pins-over";
    this.overEl.setAttribute("role", "status");
    this.overEl.hidden = true;
    root.appendChild(this.overEl);
  }

  /** Re-lay the layer out for this frame. `hoveredId`/`selectedId` style the states. */
  update(layouts: readonly PaneLayout[], hoveredId: string | null, selectedId: string | null): void {
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
        el.addEventListener("focus", () => { this.focusedKey = key; const q = this.byEl.get(el!); if (q) this.hooks.onFocus?.(q); });
        el.addEventListener("blur", () => { if (this.focusedKey === key) this.focusedKey = null; this.hooks.onFocus?.(null); });
        el.addEventListener("click", () => { const q = this.byEl.get(el!); if (q) this.hooks.onSelect?.(q); });
        el.addEventListener("keydown", (e) => this.onKey(e, el!));
        this.root.appendChild(el);
        this.els.set(key, el);
      }
      this.byEl.set(el, p);
      const cls = `sf-pin ${p.pin.kind} ${p.pin.source}${p.area ? " area" : ""}`
        + (p.pin.id === hoveredId ? " hovered" : "") + (p.pin.id === selectedId ? " selected" : "");
      if (el.className !== cls) el.className = cls;
      const label = pinLabel(p.pin);
      if (el.getAttribute("aria-label") !== label) el.setAttribute("aria-label", label);
      const pressed = String(p.pin.id === selectedId);
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
    const over = layouts.reduce((n, l) => n + l.overCap, 0);
    const text = over > 0 ? `${over} more marker${over === 1 ? "" : "s"} in view than can be pinned — zoom in to resolve them` : "";
    if (this.overEl.textContent !== text) this.overEl.textContent = text;
    this.overEl.hidden = over === 0;
  }

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
    this.overEl.remove();
  }
}
