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
// computed. Retune-on-pan is **T-444** (`./retune.ts`) — a pan here is a pan, and commands nothing:
// that module reads pane state and produces an *offer*, and only an explicit act on that offer
// reaches the front end, through T-343's one gate.

import { fmtBandwidth } from "../app/explore/format";
import type { Box, Lattice, ViewTier } from "./lattice";
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
  /**
   * **The snap-to-live dead zone at the live edge, in DEVICE PIXELS of the pane (T-486).**
   *
   * `holdPx` is how far a *following* pane may be dragged and still be held in follow; `snapPx` is
   * how near a *frozen* pane must end up to snap back to it. `snapPx < holdPx` is the hysteresis —
   * see [[PaneModel.settleTime]] for why the two differ and what the band between them is for.
   */
  holdPx?: number;
  snapPx?: number;
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
/** A tuned window worth acting on: finite centre, positive finite span. */
function validTuned(t: { centerHz: number; spanHz: number } | null): t is { centerHz: number; spanHz: number } {
  return !!t && Number.isFinite(t.centerHz) && Number.isFinite(t.spanHz) && t.spanHz > 0;
}

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
  /** T-486's dead zone, device px. See [[settleTime]]. */
  private readonly holdPx: number;
  private readonly snapPx: number;
  /**
   * **The in-flight time gesture per pane** — opened on its first move, read once at
   * [[settleTime]], then dropped.
   *
   * This is *gesture* state, not view state, and the distinction is what keeps T-347 intact: it
   * holds no coordinate, so it cannot describe a window that disagrees with [[TimeWindow]]'s tag.
   * The pane's window is still the whole of its pause; there is still no `paused` field.
   *
   * `left` is **the hysteresis, as a one-way door rather than as a second constant**. A stroke that
   * began live holds follow until it is dragged beyond `holdPx`, and once it has been, it has left —
   * for the rest of that stroke, whatever the pointer does next. Without the latch the two
   * thresholds would be read against a moving lag and the pane would flicker in and out of live
   * while a hand hovered at the boundary, which is precisely the oscillation the brief warned about.
   * With it, `isFollowing` is **monotone within a stroke**: it can fall once and never rise again
   * before the release, and the release is the only place it can rise.
   *
   * `maxLagNs` is **the direction test, and it is what keeps the zone from eating an explicit
   * Pause.** Pausing is a coordinate change, so a pane paused while live sits at lag *zero* — inside
   * the snap-back zone by construction. Without this, the user's next small scrub would be pulled
   * straight back to live and the pane could not be nudged off the edge at all. The snap-back exists
   * to catch *"I was heading back to live and fell short"*, so it asks that the stroke actually came
   * back from somewhere: the release must be nearer the edge than the furthest the stroke reached.
   * A stroke that only ever moved **away** from live is never a request to return to it.
   */
  private readonly gesture = new Map<string, { readonly live: boolean; left: boolean; maxLagNs: number }>();
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
    this.holdPx = Math.max(0, opts.holdPx ?? 10);
    this.snapPx = Math.max(0, Math.min(opts.snapPx ?? 6, this.holdPx));
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
    this.gesture.delete(id);
    return true;
  }

  /** Move the boundary of the split that owns `id` (its own edge against its sibling). `frac` is
   * the share `id` itself gets, whichever side of the divider it is on. */
  setSplitFraction(id: string, frac: number): boolean {
    const next = reFrac(this.root, id, clamp(frac, 0.05, 0.95));
    if (!next) return false;
    this.root = next;
    return true;
  }

  /**
   * **Every divider of the layout (T-1005)** — one per split, in GL device px, with what a drag on
   * it needs: the axis it moves along, the parent rectangle its fraction is a share of, and how to
   * address it. A divider between two panes is addressed by the pane on its first side (`paneId`,
   * through [[setSplitFraction]]); one between two splits has no such pane, so every divider also
   * carries its `path` from the root for [[setDividerFraction]].
   */
  dividers(wPx = this.wPx, hPx = this.hPx): Divider[] {
    const out: Divider[] = [];
    dividersInto(this.root, { x: 0, y: 0, w: wPx, h: hPx }, "", this.gapPx, out);
    return out;
  }

  /** Set the first child's share of the split at `path` (a [[Divider]]'s own `path`). */
  setDividerFraction(path: string, frac: number): boolean {
    const f = clamp(frac, 0.05, 0.95);
    const next = atPath(this.root, path, (n) => ({ ...n, frac: f }));
    if (!next) return false;
    this.root = next;
    return true;
  }

  /** The orientation of the split that owns pane `id`, or null when `id` is the only pane. */
  splitDirOf(id: string): SplitDir | null {
    const path = ownerPath(this.root, id, "");
    return path === null ? null : (nodeAt(this.root, path) as SplitNode).dir;
  }

  /** **Rows ⇄ columns (T-1005):** re-orient the split that owns `id`, keeping its fraction and both
   * panes' view state — a layout change, never a view change. False with a single pane. */
  setSplitDir(id: string, dir: SplitDir): boolean {
    const path = ownerPath(this.root, id, "");
    if (path === null) return false;
    const next = atPath(this.root, path, (n) => ({ ...n, dir }));
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

  // ——— T-472: the uniform gesture, and the aspect ratio it must never change ———

  /**
   * **The part of `factor` that BOTH axes can take.** `1` when either of them is already at a bound.
   *
   * This is the whole of T-472. [[zoomFreq]] and [[zoomTime]] each clamp on their own — which is
   * right, and is what T-434 de-welded them for — but a *uniform* gesture that hands the same
   * requested factor to both and lets each clamp separately **stops being uniform the moment one of
   * them saturates**: the user zooms out past the end of the record, time pins to the retained
   * window, frequency keeps widening, and the picture's aspect ratio walks away under a gesture that
   * promised to preserve it. That was the reported bug, and the recovery — shift-scroll the
   * frequency axis back — had to be performed by hand after every wheel.
   *
   * So the requested factor is reduced, *before* either axis is touched, to the one both can honour:
   * the **realized** factor of each axis (what its span would actually become, over what it is now),
   * taken at whichever is nearer 1. Zooming out that is the smaller, zooming in the larger. Applying
   * it then lands each axis strictly inside its own clamp, so neither clamp fires and both spans
   * scale by exactly the same number — which is the aspect ratio, preserved by construction rather
   * than by a tolerance.
   *
   * **What it does NOT do, and this is the distinction the ticket turns on.** It constrains *the
   * gesture*, never the pyramid. The two axes remain independently levelled: they are still moved by
   * two separate calls with two separate anchors, still clamped separately, and still resolve their
   * own levels from their own cell sizes — so after a plain wheel `levelF` and `levelT` legitimately
   * differ, which is what `levelDivergenceNote` exists to say. A fix that collapsed the levels to
   * make the pixels square would undo T-434/T-438/T-440.
   *
   * **Why a centre clamp is not a stop.** The bound that matters here is the *span* bound, because
   * the aspect ratio is a ratio of spans. [[normalise]]'s centre clamp moves a window without
   * changing either span, so it cannot skew the picture — and treating it as a stop would make it
   * impossible to zoom out to the whole surface from anywhere near an edge, which is a worse gesture
   * than the one being fixed.
   */
  lockedZoomFactor(id: string, factor: number): number {
    const p = this.panes.get(id);
    if (!p || !Number.isFinite(factor) || factor <= 0) return 1;
    const gf = p.freq.spanHz > 0 ? this.freqSpanAfter(p, factor) / p.freq.spanHz : 1;
    const gt = p.time.spanNs > 0 ? this.timeSpanAfter(p, factor) / p.time.spanNs : 1;
    if (factor > 1) return Math.max(1, Math.min(gf, gt));
    if (factor < 1) return Math.min(1, Math.max(gf, gt));
    return 1;
  }

  /**
   * **The plain wheel: one factor, both axes, and the aspect ratio held.**
   *
   * The two axes are still two calls with two anchors — see [[lockedZoomFactor]] for why that is the
   * point rather than an implementation detail. A locked factor of exactly 1 returns without calling
   * either, so at a bound the pane's record is left *identical*, not recomputed into something
   * floating-point-equal: `(c + k) − k` is not always `c`, and a gesture that is supposed to do
   * nothing must do nothing.
   */
  zoomBoth(id: string, factor: number, anchorF = 0.5, anchorT = 1): void {
    const f = this.lockedZoomFactor(id, factor);
    if (f === 1) return;
    this.zoomFreq(id, f, anchorF);
    this.zoomTime(id, f, anchorT);
  }

  /** The span [[zoomFreq]] would actually leave, clamps and all. Mirrors it plus [[normalise]]. */
  private freqSpanAfter(p: PaneState, factor: number): number {
    const fullF = this.bounds.f1Hz - this.bounds.f0Hz;
    return clamp(clamp(p.freq.spanHz * factor, this.minSpanHz, fullF), Math.min(this.minSpanHz, fullF), fullF);
  }

  /** The same for [[zoomTime]]. Its ceiling is the record, which grows, so it is read per call. */
  private timeSpanAfter(p: PaneState, factor: number): number {
    const fullT = Math.max(this.minSpanNs, this.timeTop() - this.bounds.t0Ns);
    return clamp(clamp(p.time.spanNs * factor, this.minSpanNs, fullT), Math.min(this.minSpanNs, fullT), fullT);
  }

  // ——— time: the same window is the pause state ———

  /**
   * Scrub. **A pan in time freezes the pane first**, because a pane pinned to the edge that also
   * carries an offset from it is precisely the third state T-347 refused to have.
   *
   * **The motion is unthresholded and stays that way (T-456).** The window moves 1:1 with `dNs`
   * from the first pixel; there is no gate here, no minimum travel, and no suppressed first move.
   * What T-486 adds is not a threshold on *this* — it is a threshold on the **derived follow
   * state**, applied once, at the end of the gesture, by [[settleTime]]. A pan is still a pan, and
   * the sentence this doc used to end on — "re-entering follow is an explicit act, never a side
   * effect of a gesture ending near the edge" — moved there rather than going away: the explicit
   * act is now the *release*, because the user reported twice that requiring a button press after a
   * drag that plainly ended at the live edge is the defect, not the discipline.
   *
   * **A scrub of zero is not a scrub (T-484).** `Preview.drag` pans both axes on every pointer
   * move, so a drag straight along frequency arrives here as `panTime(id, 0)` — and the version
   * without this guard froze the pane anyway, because `freezeAt` ran before the delta was looked
   * at. A sideways drag silently stopped the waterfall following the live edge: the user changed
   * frequency and the pane quietly entered the state *"scrubbed into the past"*, which is the one
   * thing [[follow]]'s contract says may only happen by an explicit act. It surfaced through T-484:
   * with the finest tier at the display row rather than at 1 s, a frozen pane drifts visibly behind
   * the edge within a few hundred milliseconds, and `paneRetuneOffer` began — correctly — answering
   * `block: "past"` for a viewport whose only gesture had been sideways. T-486 gives the guard a
   * second job: a zero scrub must not *open a gesture* either, or a sideways drag would reach
   * [[settleTime]] claiming to be a time gesture that ended at the live edge.
   */
  panTime(id: string, dNs: number): void {
    if (dNs === 0) return;
    const p = this.panes.get(id);
    if (!p) return;
    let g = this.gesture.get(id);
    if (!g) this.gesture.set(id, (g = { live: p.time.live, left: !p.time.live, maxLagNs: this.lagBehindEdge(p.time) }));
    this.update(id, (q) => {
      const t = freezeAt(q.time, this.edgeNs);
      return { ...q, time: { live: false, centerNs: t.centerNs + dNs, spanNs: t.spanNs } };
    });
    const q = this.panes.get(id)!;
    const lag = this.lagBehindEdge(q.time);
    g.maxLagNs = Math.max(g.maxLagNs, lag);
    // The one-way door: the instant this stroke carries the viewport past the hold zone it has left
    // follow, and nothing later in the same stroke puts it back. See [[gesture]].
    if (!g.left && lag > this.zoneNs(id, q.time.spanNs, this.holdPx)) g.left = true;
  }

  /**
   * **End a time gesture and commit the follow/pause decision — T-486's snap-to-live dead zone.**
   *
   * The reported bug was two halves of one missing thing. A 1 px time-pan dropped the pane out of
   * live, so a twitch during a *frequency* drag cost you the live edge; and a drag back toward the
   * top, released as a new row appended under the cursor, landed a few pixels short and re-paused —
   * the pane came to rest *nearly* following, which is the state that produces the first half again
   * on the next twitch. A pane that is almost live is the defect, not a near-miss of the fix.
   *
   * So the gesture's **end** asks one question: where did the viewport come to rest, relative to the
   * live edge? Within the zone the pane follows and is **pinned exactly to the edge** — `followAgain`
   * drops the centre, so `box.t1Ns === edgeNs` by construction rather than by arithmetic that could
   * leave it a pixel short. Beyond it the pause is committed.
   *
   * **The threshold is on the state, not on the pan.** Nothing above suppressed a pixel of motion:
   * the view tracked the pointer the whole way, and this reads no pointer at all — only the window
   * it left behind, measured against the edge. That is why T-407 is not the precedent here (its
   * defect was a gate on a *pointer stream*, and travel summed across axes); the one rule of T-407's
   * that does apply — measure a distance, never a sum of axes — is honoured trivially, because the
   * only distance measured is along one axis, time.
   *
   * **Pixels, not rows, and the choice matters.** The zone is `holdPx`/`snapPx` **device pixels of
   * this pane**, converted to ns through the pane's own time scale (`spanNs / rectHeightPx`). It
   * exists because a hand cannot hold a pointer to the pixel and because the user's question is
   * *"am I still at the top of the waterfall?"* — a question about the picture, which a pixel
   * answers at every zoom and a row does not. A row threshold is a claim about data: after T-484 the
   * finest tier is the display row rather than a 1 s cell, so "a few rows" silently changed meaning
   * by ~25×, and five levels out ten rows is minutes of capture. The cost of the pixel rule is at
   * the zoomed-out extreme: on a day-wide viewport ten pixels is minutes, so a pane cannot be parked
   * within minutes of live there — which is correct, because at that scale minutes *is* ten pixels
   * and the two windows are the same picture. At the zoomed-in extreme the zone shrinks with the
   * span and stays exactly as wide as the hand's tremor, which is the whole point.
   *
   * **Hysteresis, and why it is not symmetric.** Leaving follow takes more than `holdPx`; re-entering
   * it takes less than `snapPx`. Between them is a band that **keeps whatever the pane had**, so a
   * pane the user parked just off live is not yanked back by a fine adjustment, while a pane the user
   * meant to keep live survives a twitch. Which threshold applies is decided by [[gesture]] — the
   * state the stroke *began* in, and whether it has already been out — not by the lag at the instant
   * of asking, which is what would oscillate. Note what cannot flap at all: a following pane is
   * **pinned**, so its lag is
   * identically zero and no amount of capture can carry it out of the zone; and an advancing edge
   * moves a *frozen* pane monotonically **away** from `snapPx`, never toward it. Neither state can
   * be left without a gesture, and a gesture commits once.
   *
   * Capture, the ring and detection are untouched, as everywhere else in this file.
   */
  settleTime(id: string, atNs = this.edgeNs): void {
    // Like [[pause]]'s argument, `atNs` is a report of where capture has got to, so it advances the
    // known edge: the rows that appended *during* the drag are exactly the ones the second half of
    // the bug is about, and measuring the lag against a stale edge would re-create it.
    this.edgeNs = Math.max(this.edgeNs, atNs);
    const g = this.gesture.get(id);
    this.gesture.delete(id);
    if (!g) return; // no time gesture was in flight; there is nothing to commit
    const p = this.panes.get(id);
    if (!p || p.time.live) return;
    // Never left the hold zone → it was never a pause, and it pins. Left it (or began frozen) → it
    // is a pause unless the release both lands inside the tighter snap-back zone AND came back from
    // further out: a stroke that only moved away from live is not a request to return to it.
    if (g.left) {
      const lag = this.lagBehindEdge(p.time);
      if (lag > this.zoneNs(id, p.time.spanNs, this.snapPx) || lag >= g.maxLagNs) return;
    }
    this.update(id, (q) => ({ ...q, time: followAgain(q.time) }));
  }

  /** How far this window's newest row sits behind the live edge, ns. Zero for a following pane,
   * which has no offset to have — that is what the live arm means. */
  private lagBehindEdge(t: TimeWindow): number {
    if (t.live) return 0;
    return Math.max(0, this.edgeNs - (t.centerNs + t.spanNs / 2));
  }

  /**
   * `px` device pixels of pane `id`, in ns at its current time scale.
   *
   * **Zero when the viewport is unknown**, which is the fail-closed direction: without a pixel
   * height there is no pixel to be a threshold in, and answering anything else would snap panes to
   * live on a guess. A model that has never been given a viewport therefore keeps the pre-T-486
   * behaviour exactly.
   */
  private zoneNs(id: string, spanNs: number, px: number): number {
    const h = this.rects().get(id)?.h ?? 0;
    if (!(h > 1) || !(spanNs > 0)) return 0;
    return (px / h) * spanNs;
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
    this.gesture.delete(id); // an explicit act ends any gesture's claim on the decision
    this.update(id, (p) => ({ ...p, time: freezeAt(p.time, atNs) }));
  }

  /** **Play this pane** — re-pin it to the growing edge. */
  follow(id: string): void {
    this.gesture.delete(id);
    this.update(id, (p) => ({ ...p, time: followAgain(p.time) }));
  }

  /** The play/pause toggle, as one control over one state (T-347's shape, per pane). */
  setFollowing(id: string, on: boolean): void { if (on) this.follow(id); else this.pause(id); }

  /** Does this pane's frequency window overlap `[centerHz ± spanHz/2]`? View arithmetic only. */
  overlapsFreq(id: string, centerHz: number, spanHz: number): boolean {
    const p = this.panes.get(id);
    if (!p) return false;
    const lo = p.freq.centerHz - p.freq.spanHz / 2, hi = p.freq.centerHz + p.freq.spanHz / 2;
    return lo < centerHz + spanHz / 2 && hi > centerHz - spanHz / 2;
  }

  /**
   * **Is this pane at the live edge of the tuned window?** (T-955) — following in TIME *and*
   * showing the tuned window in FREQUENCY. `tuned = null` (no front end reports one: a replay, a
   * server with no device) reduces it to [[isFollowing]]. A pane following the live edge of
   * spectrum the radio has left is NOT at the tuned live edge: it reads "LIVE" over nothing new.
   *
   * This is the state the follow-live control toggles on (the FAB today, T-1001's per-pane
   * Live/Freeze later): at the tuned live edge a press freezes; anywhere else it brings the pane
   * there with [[followTuned]] — so the control can never freeze a pane that is not at the edge.
   */
  atTunedLiveEdge(id: string, tuned: { centerHz: number; spanHz: number } | null): boolean {
    if (!this.isFollowing(id)) return false;
    return !validTuned(tuned) || this.overlapsFreq(id, tuned.centerHz, tuned.spanHz);
  }

  /**
   * **Bring this pane to the live edge of the tuned window** (T-955): follow in time, and — only
   * when the pane does not overlap the tuned window — move its frequency window onto it. A pane
   * already overlapping the tuned window keeps its centre and span, so a user's zoom inside the
   * tuned band survives the press. View state only; the radio is not asked anything.
   */
  followTuned(id: string, tuned: { centerHz: number; spanHz: number } | null): void {
    this.follow(id);
    if (validTuned(tuned) && !this.overlapsFreq(id, tuned.centerHz, tuned.spanHz)) {
      this.setFreq(id, tuned.centerHz, tuned.spanHz);
    }
  }

  /**
   * **Is this pane following the live edge?** The *committed* state — which, mid-gesture, is not
   * the same as the window's tag.
   *
   * A time gesture has to freeze the window to carry the offset the pointer is asking for (see
   * [[panTime]]), but freezing the window is not the same event as **deciding to pause**: T-486's
   * whole point is that the decision belongs to where the viewport comes to rest, and until
   * [[settleTime]] runs it has not been made. So a stroke that began live and has not yet been
   * dragged beyond `holdPx` still reads as following, which is what makes a 1 px time-pan leave the
   * pane in live rather than blinking it out and back.
   *
   * This is a **pure function of the window, the edge and the gesture's origin** — not a stored flag
   * beside the window that could disagree with it, and there is still no `paused` field anywhere in
   * this file. It is the one answer to the question: `data-following`, the play/pause control and
   * T-460's refresh set all read it here rather than each deriving it again (T-397).
   */
  isFollowing(id: string): boolean {
    const p = this.panes.get(id);
    if (!p) return false;
    if (p.time.live) return true;
    const g = this.gesture.get(id);
    return !!g && !g.left;
  }

  /** Jump a pane to an absolute capture time (the timeline scrubber's landing). Freezes it, since
   * an absolute centre and following the edge are different windows. */
  goTo(id: string, centerNs: number): void {
    this.gesture.delete(id);
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

type SplitNode = Extract<LayoutNode, { kind: "split" }>;

/**
 * **Which pane is active after a close (T-1005).** `panes` and `rects` are the layout AFTER the
 * close (the survivor has grown into the closed pane's area), `at` the pointer in the same GL px or
 * null. The rule: the pane under the pointer; else the first pane that follows the live edge; else
 * `prev` if it survived; else the first pane. Before this a close made the first pane active,
 * whatever the user was looking at.
 */
export function activeAfterClose(panes: readonly PaneState[], rects: ReadonlyMap<string, PaneRect>,
  at: { x: number; y: number } | null, prev: string): string | null {
  if (at) {
    for (const p of panes) {
      const r = rects.get(p.id);
      if (r && at.x >= r.x && at.x < r.x + r.w && at.y >= r.y && at.y < r.y + r.h) return p.id;
    }
  }
  return panes.find((p) => p.time.live)?.id ?? (panes.some((p) => p.id === prev) ? prev : panes[0]?.id ?? null);
}

/** One divider of the layout, for a drag (see [[PaneModel.dividers]]). */
export interface Divider {
  /** `a`/`b` steps from the root to the split this divider belongs to (`""` = the root split). */
  readonly path: string;
  readonly dir: SplitDir;
  /** The first child's (`a`: left, or top) share of `parent`. */
  readonly frac: number;
  /** The split's own rectangle, GL device px — a drag's fraction is measured across it. */
  readonly parent: PaneRect;
  /** The gap between the two sides, GL device px (its thickness is the pane gap). */
  readonly rect: PaneRect;
  /** The pane directly on the first side, if that side is a pane rather than another split. */
  readonly paneId: string | null;
}

function dividersInto(n: LayoutNode, r: PaneRect, path: string, gap: number, out: Divider[]): void {
  if (n.kind === "pane") return;
  const paneId = n.a.kind === "pane" ? n.a.id : null;
  if (n.dir === "columns") {
    const w = r.w * n.frac;
    out.push({ path, dir: n.dir, frac: n.frac, parent: r, paneId, rect: { x: r.x + w - gap / 2, y: r.y, w: gap, h: r.h } });
    dividersInto(n.a, { ...r, w }, path + "a", gap, out);
    dividersInto(n.b, { ...r, x: r.x + w, w: r.w - w }, path + "b", gap, out);
  } else {
    const h = r.h * n.frac;
    out.push({ path, dir: n.dir, frac: n.frac, parent: r, paneId, rect: { x: r.x, y: r.y + (r.h - h) - gap / 2, w: r.w, h: gap } });
    dividersInto(n.a, { ...r, y: r.y + (r.h - h), h }, path + "a", gap, out);
    dividersInto(n.b, { ...r, h: r.h - h }, path + "b", gap, out);
  }
}

function nodeAt(n: LayoutNode, path: string): LayoutNode | null {
  let cur: LayoutNode = n;
  for (const c of path) {
    if (cur.kind !== "split") return null;
    cur = c === "a" ? cur.a : cur.b;
  }
  return cur;
}

/** `n` with the split at `path` replaced by `f(it)`, or null when `path` names no split. */
function atPath(n: LayoutNode, path: string, f: (s: SplitNode) => SplitNode): LayoutNode | null {
  if (n.kind !== "split") return null;
  if (path === "") return f(n);
  const rest = path.slice(1);
  if (path[0] === "a") { const a = atPath(n.a, rest, f); return a ? { ...n, a } : null; }
  const b = atPath(n.b, rest, f);
  return b ? { ...n, b } : null;
}

/** Path of the split whose direct child is pane `id`, or null. */
function ownerPath(n: LayoutNode, id: string, path: string): string | null {
  if (n.kind === "pane") return null;
  if ((n.a.kind === "pane" && n.a.id === id) || (n.b.kind === "pane" && n.b.id === id)) return path;
  return ownerPath(n.a, id, path + "a") ?? ownerPath(n.b, id, path + "b");
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
  /** **Which tier this pane drew from** (T-505) — the lattice the measurement came off, not only
   * the level within it. */
  readonly tier: ViewTier;
  /** That tier in one clause, for a user: what the pane is showing and why. */
  readonly tierLabel: string;
  /** `LIVE`, or how far behind the live edge the frozen window's newest row sits. */
  readonly timeLabel: string;
  /** **The pane's actual time window**, absolute capture ns, exactly as the frame drew it — the
   * state `timeLabel` rounds for a reader. Stated so a caller that must decide "did the time axis
   * move?" reads the axis itself, not a label that rounds a sub-second move to the same `−23 s`. */
  readonly t0Ns: number;
  readonly t1Ns: number;
  readonly freqLabel: string;
  readonly tiles: number;
  readonly fallbacks: number;
  readonly pending: number;
  /** **Resident tiles whose answer does not reach the live edge** (T-532) — see
   * [[PaneReport.behind]]. Not part of `pending`: the tile arrived, its newest rows had not. */
  readonly behind: number;
  /** Resident tiles that drew NOTHING because their horizon is at or below their own start
   * ([[PaneReport.blank]]) — held, and yet the pane's ground is what is on screen. */
  readonly blank: number;
  /** [[PaneReport.stale]]: resident tiles drawn here that have gone longer than `staleAfterMs`
   * without a confirmed answer (T-1039) — still on screen, just old, and said so. */
  readonly stale: number;
  /** [[PaneReport.shortNs]]: how far short of this pane's own window top the drawing reached. */
  readonly shortNs: number;
  /**
   * **Places the coverage survey settled as never sampled, and so were never requested** (T-580)
   * — [[PaneReport.surveyed]].
   *
   * It is stated for the same reason every other count here is: a pane drawn entirely from the
   * survey holds no tiles and waits for none, so without it the readout says `0 tiles · 0 coarse
   * stand-ins · 0 pending` — word for word what a pane that has drawn *nothing at all* says. Grey
   * is the normal state of a 6 GHz canvas, not an edge case, and "the radio never looked here" and
   * "this pane has not started" are the two things a coverage readout exists to tell apart.
   */
  readonly surveyed: number;
  /** Other panes in this frame resolved to a different `(levelF, levelT)`. Not a warning: a fact
   * the pane must say about itself, so a legitimate difference is not read as a bug. */
  readonly differsFrom: readonly string[];
  /**
   * **What the last-known (shadow) cells on this pane were read at** (T-916), in one clause — or
   * `null` when no tile drawn here carries a shadow read from a coarser source than its own level.
   *
   * The pane already states the level it was drawn at; a shadow answered by the spectrum-history
   * ladder is drawn at a *different* one, and a max-hold over the ladder's larger box reads hotter
   * than the row the band was last live on (10–15 dB over the departed FM band, T-911). Saying so
   * is the same rule as [[tierLabel]]: the surface never implies a resolution it did not have.
   */
  readonly shadowLabel: string | null;
}

/**
 * **The pane's statement of which tier it drew from** (T-505), and why.
 *
 * CLAUDE.md: *"each pane states the level it was actually drawn at, so a wide or deep zoom shows
 * overview rather than upscaled detail presented as measurement"*. Since the tiers became two real
 * tile sources, the level alone no longer says that — the same index means a different cell on
 * each lattice — so the pane names the source as well.
 */
function tierStatement(r: PaneReport): string {
  if (r.tier === "detail") {
    return r.clamped
      ? "detail tier, zoomed past its ceiling: the cells drawn are finer than a pixel."
      : "detail tier: the live chain's own lattice, at the resolution the front end measured.";
  }
  return "overview tier: folded from the spectrum-history pyramid, because this window is wider or "
    + "longer than the detail lattice can be read over. Survey resolution, not live-IQ detail.";
}

/**
 * **The pane's statement about where its last-known cells came from** (T-916), or `null` when every
 * shadow on it was read at the tile's own level (or there is none).
 *
 * Only the coarser case is named, and deliberately: an own-level shadow *is* the cell its band's
 * last live row was drawn with (T-911), so there is nothing about it the pane's own level does not
 * already say. The ladder's answer is a second resolution on one screen, and the direction of its
 * error is known — a max-hold over a bigger box can only read the same or hotter — so the sentence
 * states the cell AND which way it leans, rather than leaving a viewer to discover that a departed
 * band's noise floor looks livelier than it was.
 */
function shadowStatement(r: PaneReport): string | null {
  if (r.shadowLadder <= 0) return null;
  const cell = r.shadowCellHz > 0 || r.shadowCellS > 0
    ? `${fmtBandwidth(r.shadowCellHz)} × ${fmtSpan(r.shadowCellS)} cells`
    : "a coarser, unstated cell";
  return `last-known cells on ${r.shadowLadder} tile${r.shadowLadder === 1 ? "" : "s"} were read from the `
    + `spectrum-history ladder (${cell}), not this pane's own level: a max-hold over a larger box, so `
    + `they read at or hotter than the row the band was last live on`;
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
  edgeNs: number,
  rects?: ReadonlyMap<string, PaneRect>,
  /** Whether a viewport is following, **asked of the model that owns the answer**. Mid-gesture that
   * is not the same as the window's tag (T-486, [[PaneModel.isFollowing]]), and the chrome must not
   * be a second derivation of it — the pane's border and the refresh set have to agree, or one of
   * them is lying about the same pane in the same frame. Defaults to the tag for callers with only
   * states in hand. */
  isFollowing?: (id: string) => boolean,
): PaneStatus[] {
  // **No lattice parameter** (T-505). Each report carries the lattice its pane was drawn on,
  // because the two tiers are two lattices and a single one held here would label a pane's cells
  // with a size they do not have.
  const byId = new Map(reports.map((r) => [r.id, r]));
  const out: PaneStatus[] = [];
  for (const p of panes) {
    const r = byId.get(p.id);
    if (!r) continue;
    // **The tier is part of the identity of a level** (T-505): level 4 on the detail lattice and
    // level 4 on the overview lattice are different cells, so two panes at the same indices on
    // different tiers DO differ and the note must say so.
    const mine = `${r.tier}:${r.levelF}/${r.levelT}`;
    const differsFrom = reports.filter((o) => o.id !== p.id && `${o.tier}:${o.levelF}/${o.levelT}` !== mine).map((o) => o.id);
    // Off the pane's OWN lattice, the one the renderer drew with — never a second one held here.
    const cellHz = cellHzAt(r.lat, r.levelF), cellS = cellSAt(r.lat, r.levelT);
    const t = timeExtentOf(p.time, edgeNs);
    const live = isFollowing ? isFollowing(p.id) : p.time.live;
    out.push({
      id: p.id,
      rect: rects?.get(p.id) ?? null,
      following: live,
      device: p.device,
      levelF: r.levelF,
      levelT: r.levelT,
      cellHz,
      cellS,
      levelLabel: `${fmtBandwidth(cellHz)} × ${fmtSpan(cellS)} cells (${r.tier} tier, level ${r.levelF}/${r.levelT})`,
      tier: r.tier,
      tierLabel: tierStatement(r),
      timeLabel: live ? "LIVE" : `−${fmtSpan((edgeNs - t.t1Ns) / 1e9)}`,
      t0Ns: t.t0Ns,
      t1Ns: t.t1Ns,
      freqLabel: `${(p.freq.centerHz / 1e6).toFixed(3)} MHz ± ${fmtBandwidth(p.freq.spanHz / 2)}`,
      tiles: r.tiles,
      fallbacks: r.fallbacks,
      pending: r.pending,
      behind: r.behind,
      blank: r.blank,
      stale: r.stale,
      shortNs: r.shortNs,
      surveyed: r.surveyed,
      shadowLabel: shadowStatement(r),
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
  const distinct = new Set(statuses.map((s) => `${s.tier}:${s.levelF}/${s.levelT}`));
  if (distinct.size < 2) return null;
  const parts = statuses.map((s) => `${s.id} ${s.levelLabel}`).join("; ");
  return `Panes are at different pyramid levels (${parts}). A coarser cell is the maximum over more cells, so the same energy legitimately reads differently — same ramp, same scale, stated level.`;
}
