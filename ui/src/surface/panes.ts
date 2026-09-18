// The pane model of the unified surface (T-442, docs/16 §8.4): **N viewports onto ONE surface**,
// each with its own `(center_f, span_f, center_t, span_t)`, its own follow-mode, and its own pause.
//
// **Why panes exist: multiple SDRs.** Tuned ranges may be far apart — 100 MHz and 2.4 GHz are not
// watchable together at any useful density — so panes split the canvas and independently pan, zoom
// and follow-live different sections **of the same surface**.
//
// **T-380's invariant, in its stronger form.** There is still exactly *one* thing being looked at.
// A pane is **where you look from**, not another thing to look at: `split` hands back two panes
// showing the *identical* box, and they diverge only because the user moves one. Additional SDRs
// widen *coverage* (which is why a pane carries a `device` — whose coverage plane decides its grey,
// `any` = the union); they never split the subject.
//
// **A pane's pause IS its time window (T-347).** T-347 retired `/api/control/pause` outright,
// because a run-wide boolean cannot represent N viewers, and made the client's time window the one
// mechanism: *pausing is entering the state scrubbing already produced*, with no third state for
// "scrubbed but not paused". This file holds that line per pane, and makes it structural rather
// than disciplined:
//
//   - [[TimeWindow]] is a **discriminated union whose two arms carry different data**. A following
//     pane has NO centre of its own — it borrows the growing edge — and a frozen one does. So
//     `{live: true}` + a centre ("scrubbed but not paused") is not a state this type can spell, and
//     neither is a `paused` flag beside a window that disagrees with it. There is no `paused` field
//     anywhere below; grep for it.
//   - [[freezeAt]] is a **coordinate change, not a mode change**: it writes down the window the pane
//     was already showing, so the frame on which you pause is pixel-identical to the frame before
//     it. Pausing a frozen pane is a no-op rather than a jump to the edge.
//   - Nothing here talks to the network. Pan, zoom, pause, resume, split and close are arithmetic
//     over this client's own view state, so **pausing is, on the wire, nothing** — which is exactly
//     why one pane cannot reach another pane, another tab, or the radio. Capture, the ring and
//     detection are always-on and never consulted here.
//
// Presentation-and-view-state only, per ADR-0013 §1: the boxes are in the surface's own absolute
// coordinates (Hz, capture-time ns), every bound comes from the backend, and no signal fact is
// computed. Retune-on-pan is **T-444** — a pan here is a pan, and commands nothing.

import { fmtBandwidth } from "../app/explore/format";
import type { Box, Lattice } from "./lattice";
import type { PaneRect, PaneReport, PaneView } from "./surface";

/** A pane's frequency window. Centre and span, exactly as §8.4 states it. */
export interface FreqWindow {
  readonly centerHz: number;
  readonly spanHz: number;
}

/**
 * A pane's time window — **and therefore its pause state, because they are one thing**.
 *
 * `live`: the pane is pinned to the growing edge and has no centre to store; its window is
 * `[edge − span, edge]`, re-derived every frame from whatever the edge now is.
 * Not `live`: the pane is frozen on an absolute centre in capture time. That is what "paused"
 * means here, and it is also what "scrubbed" means, because T-347 established they are the same
 * state wearing two labels.
 */
export type FollowingWindow = { readonly live: true; readonly spanNs: number };
export type FrozenWindow = { readonly live: false; readonly centerNs: number; readonly spanNs: number };
export type TimeWindow = FollowingWindow | FrozenWindow;

/** One viewport's view state. Immutable: every mutator replaces the record, so a caller may hold
 * one and compare it later — which is how the independence tests prove a pane was untouched. */
export interface PaneState {
  readonly id: string;
  readonly freq: FreqWindow;
  readonly time: TimeWindow;
  /** Whose coverage decides this pane's grey. `any` is the union of every device — the default,
   * and the one that keeps the surface single even when several front ends fill it. */
  readonly device: string;
}

export type SplitDir = "columns" | "rows";

type LayoutNode =
  | { readonly kind: "pane"; readonly id: string }
  | { readonly kind: "split"; readonly dir: SplitDir; readonly frac: number; readonly a: LayoutNode; readonly b: LayoutNode };

export interface PaneModelOptions {
  /** The surface's own extent: the device-available frequency range, and the retained time window.
   * Backend numbers (T-438's `axes`/`extent`); nothing here invents an RF constant. */
  bounds: Box;
  /** Supplies the zoom floor: a pane may not magnify below `minCells` level-0 cells across, which
   * would present a measurement at a resolution it was never made at (docs/16 §4). */
  lattice?: Lattice;
  minCells?: number;
  minSpanHz?: number;
  minSpanNs?: number;
  /** Backdrop showing between panes, device px. */
  gapPx?: number;
  width?: number;
  height?: number;
  /** The first pane's window. Defaults to the whole surface. */
  freq?: FreqWindow;
  spanNs?: number;
  device?: string;
  idPrefix?: string;
}

const clamp = (v: number, lo: number, hi: number) => (lo > hi ? (lo + hi) / 2 : Math.min(hi, Math.max(lo, v)));

/** The window a time anchor resolves to against the current live edge. */
export function timeExtentOf(time: TimeWindow, edgeNs: number): { t0Ns: number; t1Ns: number } {
  if (time.live) return { t0Ns: edgeNs - time.spanNs, t1Ns: edgeNs };
  return { t0Ns: time.centerNs - time.spanNs / 2, t1Ns: time.centerNs + time.spanNs / 2 };
}

/** A pane's box on the surface, at this frame's live edge. */
export function boxOf(p: PaneState, edgeNs: number): Box {
  const t = timeExtentOf(p.time, edgeNs);
  return { f0Hz: p.freq.centerHz - p.freq.spanHz / 2, f1Hz: p.freq.centerHz + p.freq.spanHz / 2, ...t };
}

/**
 * Freeze a window **where it already is**.
 *
 * This is the whole of "pause" (T-347): the pane stops borrowing the edge and starts holding the
 * centre it was showing at that instant, so the first frozen frame draws exactly what the last
 * following frame drew. An already-frozen window is returned unchanged — pausing twice must not
 * teleport a scrubbed pane to the live edge.
 */
export function freezeAt(time: TimeWindow, edgeNs: number): FrozenWindow {
  if (!time.live) return time;
  return { live: false, centerNs: edgeNs - time.spanNs / 2, spanNs: time.spanNs };
}

/** Resume following: drop the stored centre and re-pin to the growing edge. */
export const followAgain = (time: TimeWindow): TimeWindow => ({ live: true, spanNs: time.spanNs });

/** Is this pane pinned to the growing edge? The one question the two arms answer differently. */
export const following = (p: PaneState): boolean => p.time.live;

/**
 * The panes, the tree that lays them out, and the arithmetic that moves them.
 *
 * Deliberately **event-free**: there is no change notification to subscribe to, because a consumer
 * that re-laid-out on a change event rather than on every render frame is T-388's box-jump. The
 * minimap (T-443) and the chrome read [[list]] and [[views]] each frame, like the renderer does.
 */
export class PaneModel {
  private readonly panes = new Map<string, PaneState>();
  private root: LayoutNode;
  private bounds: Box;
  private readonly minSpanHz: number;
  private readonly minSpanNs: number;
  readonly gapPx: number;
  private wPx: number;
  private hPx: number;
  private nextId = 1;
  private readonly prefix: string;
  /** The newest live edge [[views]] has been shown, in capture ns. The model never asks for it;
   * capture is always-on and the caller reports where it has got to. */
  private edgeNs: number;

  constructor(opts: PaneModelOptions) {
    this.bounds = opts.bounds;
    this.prefix = opts.idPrefix ?? "pane";
    const cells = opts.minCells ?? 16;
    this.minSpanHz = opts.minSpanHz ?? (opts.lattice ? opts.lattice.f0Hz * cells : (opts.bounds.f1Hz - opts.bounds.f0Hz) / 2 ** 20);
    this.minSpanNs = opts.minSpanNs ?? (opts.lattice ? opts.lattice.t0Ns * cells : (opts.bounds.t1Ns - opts.bounds.t0Ns) / 2 ** 12);
    this.gapPx = opts.gapPx ?? 4;
    this.wPx = opts.width ?? 0;
    this.hPx = opts.height ?? 0;
    this.edgeNs = opts.bounds.t1Ns;
    const id = this.mint();
    const freq = opts.freq ?? { centerHz: (opts.bounds.f0Hz + opts.bounds.f1Hz) / 2, spanHz: opts.bounds.f1Hz - opts.bounds.f0Hz };
    const spanNs = opts.spanNs ?? opts.bounds.t1Ns - opts.bounds.t0Ns;
    this.panes.set(id, this.normalise({ id, freq, time: { live: true, spanNs }, device: opts.device ?? "any" }));
    this.root = { kind: "pane", id };
  }

  private mint(): string { return `${this.prefix}${this.nextId++}`; }

  /** Panes in layout order (left-to-right, top-to-bottom). */
  list(): PaneState[] { return order(this.root).map((id) => this.panes.get(id)!); }
  get count(): number { return this.panes.size; }
  get(id: string): PaneState | null { return this.panes.get(id) ?? null; }
  has(id: string): boolean { return this.panes.has(id); }
  /** The live edge as last reported to [[views]]. */
  get lastEdgeNs(): number { return this.edgeNs; }

  setViewport(wPx: number, hPx: number): void { this.wPx = wPx; this.hPx = hPx; }

  /** New surface extent — the device range, or a retention window that has moved on. Panes are
   * re-clamped into it, which is how a frozen pane whose data has aged out of retention is carried
   * to the oldest data that still exists instead of pointing at nothing. */
  setBounds(b: Box): void {
    this.bounds = b;
    for (const [id, p] of this.panes) this.panes.set(id, this.normalise(p));
  }

  /**
   * Split `id` in two. **Both panes show the identical box**: one surface, two places to look from.
   * They diverge when the user moves one, and not before — which is the invariant made operational
   * rather than asserted.
   */
  split(id: string, dir: SplitDir = "columns", frac = 0.5): string | null {
    const src = this.panes.get(id);
    if (!src) return null;
    const nid = this.mint();
    this.panes.set(nid, { ...src, id: nid });
    this.root = replace(this.root, id, { kind: "split", dir, frac: clamp(frac, 0.05, 0.95), a: { kind: "pane", id }, b: { kind: "pane", id: nid } });
    return nid;
  }

  /** Close a pane. **The last one never closes**: with no viewport there is nowhere to look from,
   * and the surface does not stop existing because the window did. */
  close(id: string): boolean {
    if (!this.panes.has(id) || this.panes.size === 1) return false;
    this.root = drop(this.root, id)!;
    this.panes.delete(id);
    return true;
  }

  /** Move the boundary of the split that owns `id` (its own edge against its sibling). */
  setSplitFraction(id: string, frac: number): boolean {
    const next = reFrac(this.root, id, clamp(frac, 0.05, 0.95));
    if (!next) return false;
    this.root = next;
    return true;
  }

  // ——— frequency: a pan is a pan (T-340/T-444). Nothing below commands a radio. ———

  panFreq(id: string, dHz: number): void {
    this.update(id, (p) => ({ ...p, freq: { ...p.freq, centerHz: p.freq.centerHz + dHz } }));
  }

  /** Zoom about `anchor` (0 = the pane's low-frequency edge, 1 = its high edge, 0.5 = centre), so a
   * wheel keeps the frequency under the cursor still. `factor > 1` zooms out. */
  zoomFreq(id: string, factor: number, anchor = 0.5): void {
    this.update(id, (p) => {
      const span = clamp(p.freq.spanHz * factor, this.minSpanHz, this.bounds.f1Hz - this.bounds.f0Hz);
      const at = p.freq.centerHz + (anchor - 0.5) * p.freq.spanHz;
      return { ...p, freq: { centerHz: at - (anchor - 0.5) * span, spanHz: span } };
    });
  }

  setFreq(id: string, centerHz: number, spanHz: number): void {
    this.update(id, (p) => ({ ...p, freq: { centerHz, spanHz } }));
  }

  // ——— time: the same window is the pause state ———

  /**
   * Scrub. **A pan in time freezes the pane first**, because a pane pinned to the edge that also
   * carries an offset from it is precisely the third state T-347 refused to have. Dragging forward
   * clamps at the edge and stays frozen: re-entering follow is an explicit act ([[follow]]), never
   * a side effect of a gesture ending near the edge.
   */
  panTime(id: string, dNs: number): void {
    this.update(id, (p) => {
      const t = freezeAt(p.time, this.edgeNs);
      return { ...p, time: { live: false, centerNs: t.centerNs + dNs, spanNs: t.spanNs } };
    });
  }

  /**
   * Zoom time. A following pane **stays following** — its anchor is the growing edge by
   * construction, so only the span changes and the newest row stays put. A frozen pane zooms about
   * `anchor` (0 = oldest edge of the pane, 1 = newest).
   */
  zoomTime(id: string, factor: number, anchor = 1): void {
    this.update(id, (p) => {
      const maxSpan = Math.max(this.minSpanNs, this.timeTop() - this.bounds.t0Ns);
      const span = clamp(p.time.spanNs * factor, this.minSpanNs, maxSpan);
      if (p.time.live) return { ...p, time: { live: true, spanNs: span } };
      const at = p.time.centerNs + (anchor - 0.5) * p.time.spanNs;
      return { ...p, time: { live: false, centerNs: at - (anchor - 0.5) * span, spanNs: span } };
    });
  }

  /**
   * **Pause this pane** — freeze its view at the window it is showing now. Capture, the ring and
   * detection are untouched (this function reaches nothing outside this object), and so is every
   * other pane and every other viewer.
   */
  pause(id: string, atNs = this.edgeNs): void {
    // `atNs` is a report of where capture has got to, exactly as [[views]]'s argument is, so it
    // advances the known edge. Without that, pausing between frames would clamp the pane back to
    // the previous frame's edge and the pause would visibly jump — the one thing it must not do.
    this.edgeNs = Math.max(this.edgeNs, atNs);
    this.update(id, (p) => ({ ...p, time: freezeAt(p.time, atNs) }));
  }

  /** **Play this pane** — re-pin it to the growing edge. */
  follow(id: string): void {
    this.update(id, (p) => ({ ...p, time: followAgain(p.time) }));
  }

  /** The play/pause toggle, as one control over one state (T-347's shape, per pane). */
  setFollowing(id: string, on: boolean): void { if (on) this.follow(id); else this.pause(id); }
  isFollowing(id: string): boolean { return this.panes.get(id)?.time.live ?? false; }

  /** Jump a pane to an absolute capture time (the timeline scrubber's landing). Freezes it, since
   * an absolute centre and following the edge are different windows. */
  goTo(id: string, centerNs: number): void {
    this.update(id, (p) => ({ ...p, time: { live: false, centerNs, spanNs: p.time.spanNs } }));
  }

  /** Whose coverage decides this pane's grey. A *coverage* selector, not a second subject: the
   * surface stays one surface, and this only says which device's observation log answers for it. */
  setDevice(id: string, device: string): void { this.update(id, (p) => ({ ...p, device })); }

  // ——— resolving to the renderer ———

  /** Pixel rectangles in the renderer's GL convention (origin bottom-left), gapped by [[gapPx]]. */
  rects(wPx = this.wPx, hPx = this.hPx): Map<string, PaneRect> {
    const out = new Map<string, PaneRect>();
    layoutInto(this.root, { x: 0, y: 0, w: wPx, h: hPx }, this.gapPx, out);
    return out;
  }

  /**
   * This frame's `PaneView[]`, ready for `Surface.render`.
   *
   * `edgeNs` is where capture has got to — always-on, reported in, never controlled from here. A
   * following pane's box is re-derived from it every frame (so its box tracks the growing edge with
   * no stored coordinate to go stale, T-337/T-362); a frozen pane's box does not move at all, and
   * cannot be moved by anything that happens to another pane.
   */
  views(edgeNs = this.edgeNs, wPx = this.wPx, hPx = this.hPx): PaneView[] {
    this.edgeNs = Math.max(this.edgeNs, edgeNs);
    const rects = this.rects(wPx, hPx);
    const out: PaneView[] = [];
    for (const p of this.list()) {
      const rect = rects.get(p.id);
      if (!rect) continue;
      out.push({ id: p.id, rect, box: boxOf(p, edgeNs), device: p.device });
    }
    return out;
  }

  /** The upper bound on time: the live edge, or the retained extent, whichever is later. */
  private timeTop(): number { return Math.max(this.edgeNs, this.bounds.t1Ns); }

  private update(id: string, f: (p: PaneState) => PaneState): void {
    const p = this.panes.get(id);
    if (!p) return;
    this.panes.set(id, this.normalise(f(p)));
  }

  /** Clamp one pane into the surface's extent and the zoom floor. Pure per pane: it reads nothing
   * about any other pane, which is the structural half of pane independence. */
  private normalise(p: PaneState): PaneState {
    const fullF = this.bounds.f1Hz - this.bounds.f0Hz;
    const spanHz = clamp(p.freq.spanHz, Math.min(this.minSpanHz, fullF), fullF);
    const centerHz = clamp(p.freq.centerHz, this.bounds.f0Hz + spanHz / 2, this.bounds.f1Hz - spanHz / 2);
    const top = this.timeTop();
    const fullT = Math.max(this.minSpanNs, top - this.bounds.t0Ns);
    const spanNs = clamp(p.time.spanNs, Math.min(this.minSpanNs, fullT), fullT);
    const time: TimeWindow = p.time.live
      ? { live: true, spanNs }
      : { live: false, spanNs, centerNs: clamp(p.time.centerNs, this.bounds.t0Ns + spanNs / 2, top - spanNs / 2) };
    return { id: p.id, device: p.device, freq: { centerHz, spanHz }, time };
  }
}

/** In-order pane ids: left before right, top before bottom. */
function order(n: LayoutNode): string[] {
  return n.kind === "pane" ? [n.id] : [...order(n.a), ...order(n.b)];
}

function replace(n: LayoutNode, id: string, with_: LayoutNode): LayoutNode {
  if (n.kind === "pane") return n.id === id ? with_ : n;
  return { ...n, a: replace(n.a, id, with_), b: replace(n.b, id, with_) };
}

/** Remove a pane, collapsing its split into the surviving sibling. */
function drop(n: LayoutNode, id: string): LayoutNode | null {
  if (n.kind === "pane") return n.id === id ? null : n;
  const a = drop(n.a, id), b = drop(n.b, id);
  if (!a) return b;
  if (!b) return a;
  return { ...n, a, b };
}

function reFrac(n: LayoutNode, id: string, frac: number): LayoutNode | null {
  if (n.kind === "pane") return null;
  if (n.a.kind === "pane" && n.a.id === id) return { ...n, frac };
  if (n.b.kind === "pane" && n.b.id === id) return { ...n, frac: 1 - frac };
  const a = reFrac(n.a, id, frac);
  if (a) return { ...n, a };
  const b = reFrac(n.b, id, frac);
  return b ? { ...n, b } : null;
}

/**
 * Rectangles, in GL convention: y grows **up** from the bottom of the drawing buffer, so a `rows`
 * split gives child `a` the TOP band (reading order) by placing it at the higher `y`.
 */
function layoutInto(n: LayoutNode, r: PaneRect, gap: number, out: Map<string, PaneRect>): void {
  if (n.kind === "pane") {
    const g = gap / 2;
    out.set(n.id, { x: Math.round(r.x + g), y: Math.round(r.y + g), w: Math.max(1, Math.round(r.w - gap)), h: Math.max(1, Math.round(r.h - gap)) });
    return;
  }
  if (n.dir === "columns") {
    const w = r.w * n.frac;
    layoutInto(n.a, { ...r, w }, gap, out);
    layoutInto(n.b, { ...r, x: r.x + w, w: r.w - w }, gap, out);
  } else {
    const h = r.h * n.frac;
    layoutInto(n.a, { ...r, y: r.y + (r.h - h), h }, gap, out);
    layoutInto(n.b, { ...r, h: r.h - h }, gap, out);
  }
}

// ——— §8.5a: state the level per pane ———

/**
 * What a pane is showing, **including which pyramid level it resolved to**.
 *
 * docs/16 §8.5a corrected §8.5's anti-divergence claim after the spike measured it: the guarantee
 * is *"same ramp, same scale, **stated level**"*, **not** "same picture". Two viewports at
 * different `(level_f, level_t)` legitimately differ, because a coarser cell is a max over more
 * cells — and the minimap (T-443), being 6 GHz wide, is nearly always at a different level. The fix
 * is to **state the level**, not to hide the difference: a user who reads "the strip looks
 * different from the waterfall" as a defect will file it as one.
 */
export interface PaneStatus {
  readonly id: string;
  readonly rect: PaneRect | null;
  readonly following: boolean;
  readonly device: string;
  readonly levelF: number;
  readonly levelT: number;
  /** The cell this pane's pixels are made of: the measurement's real resolution, not the screen's. */
  readonly cellHz: number;
  readonly cellS: number;
  readonly levelLabel: string;
  /** `LIVE`, or how far behind the live edge the frozen window's newest row sits. */
  readonly timeLabel: string;
  readonly freqLabel: string;
  readonly tiles: number;
  readonly fallbacks: number;
  readonly pending: number;
  /** Other panes in this frame resolved to a different `(levelF, levelT)`. Not a warning: a fact
   * the pane must say about itself, so a legitimate difference is not read as a bug. */
  readonly differsFrom: readonly string[];
}

const cellHzAt = (lat: Lattice, level: number) => lat.f0Hz * 2 ** level;
const cellSAt = (lat: Lattice, level: number) => (lat.t0Ns * 2 ** level) / 1e9;

/** A duration in capture terms, coarse on purpose: this labels a window, not a measurement. */
export function fmtSpan(s: number): string {
  const a = Math.abs(s);
  if (a < 1) return `${(a * 1000).toFixed(0)} ms`;
  if (a < 90) return `${a < 10 ? a.toFixed(1) : a.toFixed(0)} s`;
  if (a < 5400) return `${Math.floor(a / 60)} m ${Math.round(a % 60)} s`;
  return `${(a / 3600).toFixed(1)} h`;
}

/**
 * Join pane state to what the renderer actually drew for it.
 *
 * `reports` come straight from `Surface.render` — the levels are the ones the frame resolved, never
 * a second calculation that could disagree with the pixels.
 */
export function paneStatuses(
  panes: readonly PaneState[],
  reports: readonly PaneReport[],
  lat: Lattice,
  edgeNs: number,
  rects?: ReadonlyMap<string, PaneRect>,
): PaneStatus[] {
  const byId = new Map(reports.map((r) => [r.id, r]));
  const out: PaneStatus[] = [];
  for (const p of panes) {
    const r = byId.get(p.id);
    if (!r) continue;
    const mine = `${r.levelF}/${r.levelT}`;
    const differsFrom = reports.filter((o) => o.id !== p.id && `${o.levelF}/${o.levelT}` !== mine).map((o) => o.id);
    const cellHz = cellHzAt(lat, r.levelF), cellS = cellSAt(lat, r.levelT);
    const t = timeExtentOf(p.time, edgeNs);
    out.push({
      id: p.id,
      rect: rects?.get(p.id) ?? null,
      following: p.time.live,
      device: p.device,
      levelF: r.levelF,
      levelT: r.levelT,
      cellHz,
      cellS,
      levelLabel: `${fmtBandwidth(cellHz)} × ${fmtSpan(cellS)} cells (level ${r.levelF}/${r.levelT})`,
      timeLabel: p.time.live ? "LIVE" : `−${fmtSpan((edgeNs - t.t1Ns) / 1e9)}`,
      freqLabel: `${(p.freq.centerHz / 1e6).toFixed(3)} MHz ± ${fmtBandwidth(p.freq.spanHz / 2)}`,
      tiles: r.tiles,
      fallbacks: r.fallbacks,
      pending: r.pending,
      differsFrom,
    });
  }
  return out;
}

/**
 * The sentence to show when panes are at different levels — §8.5a's fix, written out.
 *
 * `null` when every pane resolved to the same level, because then there is nothing to explain.
 */
export function levelDivergenceNote(statuses: readonly PaneStatus[]): string | null {
  const distinct = new Set(statuses.map((s) => `${s.levelF}/${s.levelT}`));
  if (distinct.size < 2) return null;
  const parts = statuses.map((s) => `${s.id} ${s.levelLabel}`).join("; ");
  return `Panes are at different pyramid levels (${parts}). A coarser cell is the maximum over more cells, so the same energy legitimately reads differently — same ramp, same scale, stated level.`;
}
