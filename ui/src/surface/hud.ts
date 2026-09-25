// **HUD axes** (T-805 / MAP-05, closing T-459): a floating frequency ruler along each pane's bottom
// edge and a time ruler down its left edge, with ticks and labels, **anchored in content space**.
//
// T-459 put a *sentence* of tick values on the chrome readout row. That answered "where are the
// intermediate marks" in words, but a sentence cannot be laid against a feature to read its
// frequency or its age off the screen. This module is the ruler proper, in the map layout's own
// vocabulary (docs/23 §10.1): **ticks are band 0** — strokes in the overlay pass, on the one canvas —
// and **labels are band 2** — crisp, selectable DOM text floated over it. Both are placed from the
// *same* per-frame [[PaneRuler]], which is itself derived from the `PaneView` the data pass was just
// handed and the `(cellHz, cellS)` the frame's `PaneStatus` reported, so a tick, its label and the
// rows beneath them cannot use two mappings (the T-388 drift family) or two opinions about the level.
//
// Three rules the arithmetic keeps:
//
//  - **Anchored in capture time and Hz, never in screen space.** A tick's *value* is a nice multiple
//    of its step in absolute Hz / absolute capture ns; its *position* is that value through the
//    pane's box. So a pan or a zoom, or the live edge advancing under a following pane, moves the
//    ticks with the rows — and a following pane's time ticks scroll exactly as its rows do.
//  - **Honesty floors the step at the drawn cell** (T-459's rule, docs/16 §8.3): a mark finer than
//    the pane's own resolution would claim the hardware resolved something it did not. So the major
//    step is `>= cell`, a minor step is drawn only when it too is `>= cell`, and a pane too narrow
//    for a single interior mark draws none rather than inventing one.
//  - **Labels state units.** Frequency labels say `MHz`; time labels are an offset behind the live
//    edge in this surface's one time vocabulary (`−12 s`, `live edge`) with the absolute UTC capture
//    instant beside it — the same `HH:MM:SSZ` the hover readout prints.
//
// **No signal logic.** Everything here is pixel↔(Hz, time) mapping and string formatting over boxes
// the renderer already drew — thin-client presentation (CLAUDE.md), and it reaches no route.

import type { Box } from "./lattice";
import type { OverlayQuad } from "./minimap";
import { fmtSpan } from "./panes";
import type { PaneRect } from "./surface";
import { niceStep } from "./ticks";

/** One mark on a ruler. */
export interface RulerTick {
  /** Hz on the frequency ruler; absolute capture-time ns on the time ruler. */
  readonly value: number;
  /** Device px along the axis: from the pane's LEFT edge (frequency) or its TOP edge (time). */
  readonly pos: number;
  /** A labelled mark. Minor marks subdivide a major step and carry no label. */
  readonly major: boolean;
  /** The label, units included; `null` on a minor mark. */
  readonly label: string | null;
  /** Secondary text beside the label (the UTC instant on the time ruler); `null` when none. */
  readonly sub: string | null;
}

/** Both rulers of one pane for one frame. */
export interface PaneRuler {
  readonly id: string;
  /** The pane's rectangle, device px, GL convention (origin bottom-left) — `PaneView.rect`. */
  readonly rect: PaneRect;
  readonly freqStepHz: number;
  readonly timeStepNs: number;
  readonly freq: readonly RulerTick[];
  readonly time: readonly RulerTick[];
}

/** Minimum spacing between LABELLED marks, CSS px — wide enough for `433.920 MHz` / `−1 m 20 s`. */
export const FREQ_LABEL_PX = 96;
export const TIME_LABEL_PX = 64;

/** How a nice step subdivides into minor marks: 1 and 5 by fifths, 2 by quarters. */
function minorOf(step: number): number {
  const m = Math.round(step / 10 ** Math.floor(Math.log10(step) + 1e-9));
  return step / (m === 2 ? 4 : 5);
}

function mhzDecimals(stepHz: number): number {
  return Math.max(0, Math.min(6, Math.ceil(-Math.log10(stepHz / 1e6) - 1e-9)));
}

/** A frequency ruler label: MHz, with just enough decimals that neighbouring labels differ. */
export function fmtRulerHz(hz: number, stepHz: number): string {
  return `${(hz / 1e6).toFixed(mhzDecimals(stepHz))} MHz`;
}

/**
 * Which form the TIME ruler's primary label takes (T-1007, the ⋯ settings menu's "Time ruler").
 *
 * `age` is T-805's own: how far behind the live edge the mark is, with the UTC instant beside it.
 * `clock` swaps the two, for reading a recording against an external log. Labelling only — the marks
 * are at the identical capture instants either way, and the frequency ruler is unaffected.
 */
export type RulerMode = "age" | "clock";

/** A time ruler label: how far behind the live edge `tNs` is, or `live edge` for the mark on it. */
export function fmtRulerAge(tNs: number, edgeNs: number, stepNs: number): string {
  const d = edgeNs - tNs;
  if (Math.abs(d) < stepNs * 1e-3) return "live edge";
  return `${d > 0 ? "−" : "+"}${fmtSpan(Math.abs(d) / 1e9)}`;
}

/** The absolute capture instant, UTC — sub-second only when the step is. */
export function fmtRulerClock(tNs: number, stepNs: number): string {
  const iso = new Date(tNs / 1e6).toISOString();
  return `${stepNs < 1e9 ? iso.slice(11, 23) : iso.slice(11, 19)}Z`;
}

/** Marks at multiples of `step` inside `[lo, hi]`, ends inclusive to within `eps`. */
function multiples(lo: number, hi: number, step: number): number[] {
  const eps = step * 1e-6;
  const out: number[] = [];
  const first = Math.ceil((lo - eps) / step);
  for (let k = first; ; k++) {
    const v = k * step;
    if (v > hi + eps) break;
    out.push(v);
    if (out.length > 4096) break; // a degenerate box can never spin the frame
  }
  return out;
}

/**
 * Both rulers of one pane, from the box it was DRAWN with and the cell it was drawn AT.
 *
 * `dpr` converts the CSS label spacing into the device px `rect` is measured in. Returns empty tick
 * lists — never a guess — when the box, the rectangle or the cell is degenerate.
 */
export function paneRuler(
  id: string, box: Box, rect: PaneRect, cellHz: number, cellS: number, edgeNs: number, dpr = 1,
  rulerMode: RulerMode = "age",
): PaneRuler {
  const spanHz = box.f1Hz - box.f0Hz, spanNs = box.t1Ns - box.t0Ns;
  const cellNs = cellS * 1e9;
  let freqStepHz = 0, timeStepNs = 0;
  const freq: RulerTick[] = [], time: RulerTick[] = [];
  const k = Math.max(0.25, dpr);
  if (spanHz > 0 && rect.w > 0 && cellHz > 0) {
    freqStepHz = niceStep(Math.max(cellHz, (spanHz * FREQ_LABEL_PX * k) / rect.w));
    const minor = minorOf(freqStepHz);
    const px = (f: number) => ((f - box.f0Hz) / spanHz) * rect.w;
    const isMajor = (v: number) => Math.abs(v / freqStepHz - Math.round(v / freqStepHz)) < 1e-6;
    const steps = minor >= cellHz ? multiples(box.f0Hz, box.f1Hz, minor) : multiples(box.f0Hz, box.f1Hz, freqStepHz);
    for (const v of steps) {
      const major = isMajor(v);
      freq.push({ value: v, pos: px(v), major, label: major ? fmtRulerHz(v, freqStepHz) : null, sub: null });
    }
  }
  if (spanNs > 0 && rect.h > 0 && cellNs > 0) {
    timeStepNs = niceStep(Math.max(cellNs, (spanNs * TIME_LABEL_PX * k) / rect.h));
    const minor = minorOf(timeStepNs);
    // From the TOP: the newest instant the pane draws is its top row (T-388's "the top is the newest").
    const px = (t: number) => ((box.t1Ns - t) / spanNs) * rect.h;
    const isMajor = (v: number) => Math.abs(v / timeStepNs - Math.round(v / timeStepNs)) < 1e-6;
    const steps = minor >= cellNs ? multiples(box.t0Ns, box.t1Ns, minor) : multiples(box.t0Ns, box.t1Ns, timeStepNs);
    for (const v of steps) {
      const major = isMajor(v);
      // T-1007: `clock` swaps the primary and the secondary — the same two strings, read the other
      // way round, so a mark can never carry a time the other mode would not have put there.
      const age = major ? fmtRulerAge(v, edgeNs, timeStepNs) : null;
      const clock = major ? fmtRulerClock(v, timeStepNs) : null;
      time.push({
        value: v, pos: px(v), major,
        label: rulerMode === "clock" ? clock : age,
        sub: rulerMode === "clock" ? age : clock,
      });
    }
  }
  return { id, rect, freqStepHz, timeStepNs, freq, time };
}

/** Tick ink: the chrome's light text colour, translucent, so a tick reads over any ramp stop. */
export const HUD_TICK: readonly [number, number, number, number] = [0.835, 0.871, 0.886, 0.8];

export interface HudTickStyle {
  /** Major / minor tick length, device px. */
  majorPx?: number;
  minorPx?: number;
  /** Tick thickness, device px. */
  thickPx?: number;
  /** Multiplies the ink's alpha — the chrome's idle fade. */
  alpha?: number;
}

/**
 * The ticks as strokes in the pane's clip space: short vertical marks rising from the pane's
 * bottom edge (frequency) and short horizontal marks from its left edge (time). Each is a few
 * device px in both directions — a stroke, never a wash — so it can never cover a measurement.
 */
export function hudTickQuads(r: PaneRuler, style: HudTickStyle = {}): OverlayQuad[] {
  const { rect } = r;
  if (!(rect.w > 0) || !(rect.h > 0)) return [];
  const major = style.majorPx ?? 10, minor = style.minorPx ?? 5, thick = style.thickPx ?? 1;
  const a = Math.max(0, Math.min(1, style.alpha ?? 1));
  const rgbaMajor: [number, number, number, number] = [HUD_TICK[0], HUD_TICK[1], HUD_TICK[2], HUD_TICK[3] * a];
  const rgbaMinor: [number, number, number, number] = [HUD_TICK[0], HUD_TICK[1], HUD_TICK[2], HUD_TICK[3] * a * 0.55];
  const cx = (px: number) => (2 * px) / rect.w - 1;
  const cy = (pxFromTop: number) => 1 - (2 * pxFromTop) / rect.h;
  const out: OverlayQuad[] = [];
  for (const t of r.freq) {
    const len = t.major ? major : minor;
    const x0 = cx(Math.max(0, Math.min(rect.w - thick, t.pos - thick / 2)));
    const x1 = x0 + (2 * thick) / rect.w;
    out.push({ clip: [x0, -1, x1, -1 + (2 * len) / rect.h], rgba: t.major ? rgbaMajor : rgbaMinor, kind: "hud-tick", id: r.id });
  }
  for (const t of r.time) {
    const len = t.major ? major : minor;
    const yTop = Math.max(0, Math.min(rect.h - thick, t.pos - thick / 2));
    out.push({ clip: [-1, cy(yTop + thick), -1 + (2 * len) / rect.w, cy(yTop)], rgba: t.major ? rgbaMajor : rgbaMinor, kind: "hud-tick", id: r.id });
  }
  return out;
}

// ——— band 2: the labels ———

/** Where one label goes, in CSS px relative to the canvas's top-left. */
export interface HudLabel {
  readonly axis: "freq" | "time";
  readonly paneId: string;
  readonly x: number;
  readonly y: number;
  readonly text: string;
  readonly sub: string | null;
  readonly value: number;
}

/** Keep a label this far (CSS px) from a pane corner, so the two rulers never print over each other. */
const CORNER_CSS = { freqLeft: 64, freqRight: 36, timeTop: 10, timeBottom: 30 };

/**
 * The labels of one pane's rulers, in CSS px from the canvas's top-left. `canvasHpx` is the drawing
 * buffer's height (device px) — the GL rectangle's origin is bottom-left, the DOM's top-left.
 * A label that would collide with the other ruler at a corner is dropped, not moved: a label moved
 * off its tick would name a place it is not at.
 */
export function hudLabels(r: PaneRuler, canvasHpx: number, dpr = 1): HudLabel[] {
  const k = dpr > 0 ? dpr : 1;
  const left = r.rect.x / k, top = (canvasHpx - (r.rect.y + r.rect.h)) / k;
  const w = r.rect.w / k, h = r.rect.h / k;
  const out: HudLabel[] = [];
  for (const t of r.freq) {
    if (!t.major || t.label === null) continue;
    const x = t.pos / k;
    if (x < CORNER_CSS.freqLeft || x > w - CORNER_CSS.freqRight) continue;
    out.push({ axis: "freq", paneId: r.id, x: left + x, y: top + h, text: t.label, sub: t.sub, value: t.value });
  }
  for (const t of r.time) {
    if (!t.major || t.label === null) continue;
    const y = t.pos / k;
    if (y < CORNER_CSS.timeTop || y > h - CORNER_CSS.timeBottom) continue;
    out.push({ axis: "time", paneId: r.id, x: left, y: top + y, text: t.label, sub: t.sub, value: t.value });
  }
  return out;
}

/**
 * The band-2 label layer. Re-laid-out **inside the render frame**, from the frame's own rulers, and
 * nowhere else — a label positioned on a poll or a timer is T-388 again. Elements are pooled so a
 * 60 Hz frame reuses DOM nodes rather than rebuilding them.
 */
export class HudAxes {
  private readonly pool: HTMLElement[] = [];
  private last = "";

  constructor(private readonly root: HTMLElement) {}

  update(labels: readonly HudLabel[]): void {
    const doc = this.root.ownerDocument;
    while (this.pool.length < labels.length) {
      const el = doc.createElement("div");
      el.appendChild(doc.createElement("b"));
      el.appendChild(doc.createElement("span"));
      this.root.appendChild(el);
      this.pool.push(el);
    }
    for (let i = 0; i < this.pool.length; i++) {
      const el = this.pool[i];
      const l = labels[i];
      if (!l) { if (!el.hidden) el.hidden = true; continue; }
      if (el.hidden) el.hidden = false;
      const cls = `sf-hud-label ${l.axis}`;
      if (el.className !== cls) el.className = cls;
      el.style.transform = `translate(${l.x.toFixed(1)}px, ${l.y.toFixed(1)}px)`;
      const [b, s] = [el.children[0] as HTMLElement, el.children[1] as HTMLElement];
      if (b.textContent !== l.text) b.textContent = l.text;
      const sub = l.sub ?? "";
      if (s.textContent !== sub) s.textContent = sub;
      el.dataset.value = String(l.value);
      el.dataset.pane = l.paneId;
    }
    this.last = labels.map((l) => `${l.axis}:${l.text}`).join("|");
  }

  /** What the layer states right now — for tests and for a screen reader's summary. */
  get text(): string { return this.last; }

  dispose(): void {
    for (const el of this.pool) el.remove();
    this.pool.length = 0;
  }
}
