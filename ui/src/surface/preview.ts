// **The host.** T-450: the thing that mounts `ui/src/surface/` against the live `GET /api/tiles`,
// so the surface can be *looked at* before anything is retired.
//
// T-442 wrote it plainly — *"nothing mounts it yet, because nothing mounts the surface yet"* — and
// left `rects()`/`list()` as the seam. Twelve production files sat behind that seam with no
// importer. This file is the importer, and it is deliberately **additive**: it is reached from its
// own page (`/surface.html`), it shares no state with the app, and nothing under `ui/src/app/`
// imports it. `ui/test/surface-preview.test.ts` asserts that by walking the app's import graph,
// because "additive" is a claim about the whole repo and not something a diff review can see.
//
// ## Historical only, and structurally so
//
// The scope is a **pannable view over recorded history**. It must not depend on T-439 (live-edge
// tiles) or T-445 (the cutover), and "must not" is easy to violate by accident because
// `PaneModel`'s default window *is* the following one. So the edge is pinned instead of policed:
// [[SurfaceOrigin.edgeNs]] is resolved once from `GET /api/navigation`'s `time.latest_s` and handed
// to every `frame()` unchanged, and every viewport is frozen at open. A following pane derives its
// box from the edge it is given, so an edge that never moves is a view that cannot creep forward —
// there is no clock here to accidentally wire up.
//
// For the same reason no active-capture segment is drawn: `liveSegmentQuads` places a mark at the
// live edge, and this preview does not read the live edge. Drawing one from a fixed historical
// instant would be a live claim with no live evidence, which is the class of thing this whole
// milestone exists to refuse. The chrome says so rather than leaving it to be noticed.
//
// ## Nothing here can command the radio
//
// T-444's retune-on-pan exists (`./retune.ts`) and is **not imported**. Nor is `../app/centre/view`,
// which owns `applyDeviceAction`. Every gesture below is arithmetic on `PaneModel`, which T-442
// proved reaches nothing outside itself. The only routes named in this file are `GET /api/tiles`,
// `GET /api/navigation` and `GET /api/coverage`, all read-only; the test asserts that set against
// the source, so a device route cannot appear here without an argument.

import { ControlError } from "../controls/client";
import {
  coverageUrl, observedExtent, openingWindow, orientationNote, recentObservedExtent, shadeRange, surfaceBounds,
  type CoverageCensus, type CoverageSlice, type NavigationSlice, type OpeningWindow, type SurfaceOrigin,
} from "./bootstrap";
import { batchedTileSource } from "./tilebatch";
import { oneTier, tileUrl, type Box, type Lattice, type LatticeSet, type TileAddr } from "./lattice";
import type { RowActionFor, WidthActionsFor } from "./chrome";
import type { PaneStatus } from "./panes";
import type { OverlayQuad } from "./minimap";
import type { TracePath } from "./trace";
import type { ActiveWindow } from "../navigators";
import { probeAddr, fetchTile, latticeOf, type TileFetch, type TileResponse } from "./tile";
import { TileCache, type MovingViewport, type Viewport } from "./tilecache";
import { LiveRowFeeds, type RowOpener } from "./rowfeed";
import { SURVEY_EVERY_MS, decodeSurvey, surveyUrl, type SurveyResponse } from "./survey";
import {
  FALLBACK_RANGE, FALLBACK_RANGE_SOURCE,
  type DisplayRange, type PaneRect, type PaneReport, type PaneView, type RangeMode, type TilePlanes,
} from "./surface";
import { SurfaceView, type SurfaceFrame } from "./view";
import type { HudReserve } from "./hud";

/** Cells per tile edge the preview renders at — the route's own default, and the size the cache
 * budget in `tilecache.ts` was measured against. */
export const RENDER_CELLS = 256;
/** The orientation coverage map's grid. 128 × 32 = 4096 cells, the route's own cap. */
export const ORIENT_CELLS = 128, ORIENT_ROWS = 32;

/** How the bootstrap asks. One function so a test can record exactly what was requested. */
export type Getter = (path: string) => Promise<unknown>;

/**
 * How the bootstrap handles the route's ingest backpressure.
 *
 * **The probe is the one tile fetch that does not go through `TileCache`**, and that is how T-454's
 * defect reached the user as a page-fatal banner quoting the route's `503` verbatim. The cache had
 * the cap, the cancellation and the backoff; this path had none of them, because it is one cheap
 * `cells=8` tile asked once at open — and a refusal is precisely what a *shared* budget serves to a
 * new arrival. Reloading the page mid-drag was enough: `hk-api` keeps a [`TileSlot`] until its read
 * finishes, so the previous page's abandoned reads were still holding all four.
 *
 * A refusal is not a failure here either. It is answered the same way the cache answers it — wait,
 * ask again — and only an exhausted retry is something the user should ever be told about.
 */
export interface BackpressureOptions {
  /** Attempts after the first, per request. Default: [[retriesForBudget]] of [[RETRY_BUDGET_MS]]. */
  retries?: number;
  /** First wait, ms; doubles per attempt, capped at [[maxBackoffMs]]. */
  backoffMs?: number;
  /** The most any one wait may grow to, ms. */
  maxBackoffMs?: number;
  sleep?: (ms: number) => Promise<void>;
}

const REFUSAL_STATUS = 503;

/**
 * **The bootstrap's retry budget, in TIME rather than in attempts** (T-690).
 *
 * It was five attempts with an uncapped doubling backoff: 150, 300, 600, 1200, 2400 — four and a
 * half seconds, most of it asleep, and then the page gives up and tells the user to reload. That
 * is a bet on how fast the tile route answers, and the route's speed is not a constant: measured
 * across runs of this repo's own browser tier it moves from ~167 ms a tile on a quiet box to
 * ~3612 ms under a full suite, with all four slots held by another tab's storm. At the slow end
 * the budget expires before one round of production finishes, and a second tab cannot open at all
 * — observed as "GET /api/tiles refused every attempt".
 *
 * A `503` here is **documented, transient backpressure**: the route is producing for somebody and
 * releases the slot when it finishes. The page's own advice for it is *reload in a moment*, which
 * is asking the user to do by hand exactly what this loop does. So the bound is stated as the
 * thing it is — how long the surface is willing to wait for a busy route — and the wait is CAPPED
 * so the client keeps asking on a steady cadence instead of sleeping longer and longer through the
 * window it has.
 *
 * Twenty seconds is "something is wrong", not a race margin: four slots at the slowest service
 * time ever measured here is ~14 s, so a budget that cannot cover one full turn of the route would
 * be a bound on the wrong thing again. It is NOT unbounded — an exhausted budget still reports as
 * backpressure, and `preview-main.ts` still says so rather than quoting the route at the user.
 *
 * The real answer to one greedy tab holding every slot is a fair-share decision on the route
 * itself (T-630); this only stops the client from giving up while the route is still working.
 */
export const RETRY_BUDGET_MS = 20_000;
/** The most any one backoff wait may grow to, ms. See [[RETRY_BUDGET_MS]]. */
export const MAX_BACKOFF_MS = 1000;

/**
 * How many retries fit in `budgetMs`, given a first wait of `first` ms doubling up to `cap`.
 *
 * Exported so the budget is checkable as a PROPERTY (the waits sum to about the budget, and no one
 * wait exceeds the cap) rather than as a magic attempt count somebody would have to re-derive.
 */
export function retriesForBudget(first: number, cap: number, budgetMs: number): number {
  let spent = 0, n = 0;
  for (;;) {
    const wait = Math.min(cap, first * 2 ** n);
    if (spent + wait > budgetMs) return n;
    spent += wait;
    n++;
  }
}

/** Is this the route saying "too many at once" rather than something being wrong? */
export function isBackpressure(e: unknown): boolean {
  return e instanceof ControlError && e.status === REFUSAL_STATUS;
}

/** Everything the preview needs before its first frame, and where each part came from. */
export interface SurfaceProbe {
  /** The **detail** lattice: the live chain's own, and the one an edge invalidation is about. */
  readonly lattice: Lattice;
  /**
   * **Both tiers** (T-505). `overview` is `scheme=overview`, anchored on the spectrum-history
   * pyramid, and it is what answers a viewport the detail lattice cannot be read over. When that
   * probe fails the set is [[oneTier]] of the detail lattice — the surface then behaves exactly as
   * it did before, with the failure stated in `degraded` rather than silently halving the reach.
   */
  readonly lattices: LatticeSet;
  readonly origin: SurfaceOrigin;
  readonly census: CoverageCensus;
  readonly opening: OpeningWindow;
  /**
   * **The anchored display range** (T-470): one `(lo, hi)` in the tiles' own dBFS, measured by the
   * backend over the region, resolved **once** here and never from the viewport. It is what makes
   * the same measurement the same colour at every zoom.
   */
  readonly range: { readonly lo: number; readonly hi: number; readonly source: string };
  /** The sentence that makes a 99.4 %-grey first screen a finding instead of a bug report. */
  readonly note: string;
  /** Paths requested, in order. Surfaced so the page can show them and a test can assert them. */
  readonly requests: readonly string[];
  /** Routes that failed and what the preview did instead — never silently degraded. */
  readonly degraded: readonly string[];
}

/**
 * Three read-only GETs, in dependency order.
 *
 * Only the first is fatal: without a tile answer there is no lattice, and a client that guessed one
 * would be addressing a pyramid that does not exist. The other two degrade *and say so* — an
 * invented bound must never be indistinguishable from a reported one.
 */
/**
 * T-946(b): the orientation sentence re-derived from the backend's coverage as it is NOW. The probe's
 * note is a first-paint census; a sweep that lights 20 MHz afterwards left "0.8 % … (32 of 4096)"
 * on screen. Same route, same box, same opening as the probe — only the answer has moved.
 */
export async function refreshOrientationNote(get: Getter, probe: SurfaceProbe): Promise<string> {
  // The probe's box ends at the first-paint `latest_s`; coverage swept since lies past it. Ask the
  // backend where the newest capture is NOW and extend the top of the box (never shrink it).
  const nav = (await get("/api/navigation")) as NavigationSlice | null;
  const latest = nav?.time?.latest_s;
  const b = probe.origin.bounds;
  const t1Ns = typeof latest === "number" && Number.isFinite(latest) ? Math.max(b.t1Ns, latest * 1e9) : b.t1Ns;
  const cov = (await get(coverageUrl({ ...b, t1Ns }, ORIENT_CELLS, ORIENT_ROWS))) as CoverageSlice;
  return orientationNote(observedExtent(cov), probe.opening);
}

export async function probeSurface(get: Getter, nowS?: number, bp: BackpressureOptions = {}): Promise<SurfaceProbe> {
  const requests: string[] = [];
  const degraded: string[] = [];
  const backoffMs = bp.backoffMs ?? 150;
  const maxBackoffMs = bp.maxBackoffMs ?? MAX_BACKOFF_MS;
  // Attempts are derived from the TIME budget rather than being a constant of their own, so the
  // two can never drift apart (T-690). An explicit `retries` still wins — that is how the unit
  // tests pin the bounded-refusal case without waiting out a real budget.
  const retries = bp.retries ?? retriesForBudget(backoffMs, maxBackoffMs, RETRY_BUDGET_MS);
  const sleep = bp.sleep ?? ((ms: number) => new Promise<void>((r) => setTimeout(r, ms)));
  // Every request here retries a `503`, not only the tile probe: the refusal means the history lock
  // is busy, and asking again is the whole of the right response. Each attempt is pushed to
  // `requests`, so what the client actually asked for is visible to the page and to a test.
  const ask = async (path: string): Promise<unknown> => {
    for (let attempt = 0; ; attempt++) {
      requests.push(path);
      try {
        return await get(path);
      } catch (e) {
        if (attempt >= retries || !isBackpressure(e)) throw e;
        await sleep(Math.min(maxBackoffMs, backoffMs * 2 ** attempt));
      }
    }
  };

  const probePath = tileUrl(probeAddr());
  const probe = (await ask(probePath)) as TileResponse & {
    coverage?: { horizon?: { oldest_record_s?: number | null } | null } | null;
  };
  const lattice = latticeOf(probe, RENDER_CELLS);

  // **The second tier, probed the same way and never guessed** (T-505). One extra `cells = 8`
  // request. It is not fatal: a server with no overview lattice (or one that refuses the address)
  // leaves every viewport on the detail tier, which is what the surface did before this existed.
  let lattices: LatticeSet = oneTier(lattice);
  try {
    const over = (await ask(tileUrl(probeAddr("any", "overview")))) as TileResponse;
    lattices = { detail: lattice, overview: latticeOf(over, RENDER_CELLS) };
  } catch (e) {
    degraded.push(`GET /api/tiles?scheme=overview failed (${describe(e)}): wide or long viewports stay on the detail lattice, which cannot be read over them — they will show coarse stand-ins and pending rather than survey overview.`);
  }

  let nav: NavigationSlice | null = null;
  try {
    nav = (await ask("/api/navigation")) as NavigationSlice;
  } catch (e) {
    degraded.push(`GET /api/navigation failed (${describe(e)}): the surface's extent falls back to the view lattice's own axes and this client's clock.`);
  }

  const origin = surfaceBounds(nav, probe.coverage?.horizon?.oldest_record_s ?? null, lattice, nowS);

  let cov: CoverageSlice | null = null;
  let fine: CoverageSlice | null = null;
  try {
    cov = (await ask(coverageUrl(origin.bounds, ORIENT_CELLS, ORIENT_ROWS))) as CoverageSlice;
  } catch (e) {
    degraded.push(`GET /api/coverage failed (${describe(e)}): the view opens on the whole surface, because nothing said where the radio looked.`);
  }

  const census = observedExtent(cov);
  // T-955: **frequency from what is observed NOW, time from everything observed.** A session that
  // tuned to one band for an hour and retuned since holds both bands in one record horizon, and
  // `census.box` — a bounding rectangle over every observed cell ever — spans the gap between them
  // (measured live: a page opening on "100–1100 MHz × 1.6 h" with the tuned band a sliver). So the
  // FREQUENCY the view opens on is the band observed in the newest observed rows
  // (`recentObservedExtent`, anchored at the newest OBSERVED row, never the grid's end: a server's
  // `latest_s` was seen 34 min in the future). The TIME it opens on stays the whole census's — the
  // recent box is a few rows by construction, and opening on its time would cut an hour of history to
  // two minutes (the regression the first T-955 pass shipped).
  const recent = recentObservedExtent(cov);
  // **One refinement pass, measured rather than assumed.** A 128-cell map of a 6.5 GHz surface has
  // 51.2 MHz cells, so the coarse box around a 2.4 MHz capture is ~20x too wide — measured on a
  // replay: 0.78 % observed, and the observed box came back as 51.2–102.4 MHz for a recording that
  // spans 99.6–102 MHz. Opening there would put the capture in a twentieth of the pane's width and
  // read as "still nothing here". Asking the *same route* again over the recent box costs one
  // request and is the same question at the resolution the answer made available — in time too, so
  // its newest observed row (an eighth of a coarse row) separates a retune seconds old from the band
  // the radio just left, which a coarse row of a long history straddles.
  let freqBox: Box | null = recent.box;
  if (recent.box) {
    try {
      fine = (await ask(coverageUrl(recent.box, ORIENT_CELLS, ORIENT_ROWS))) as CoverageSlice;
      // A refinement that found nothing is not evidence against the coarse answer — the coarse cell
      // was observed, so something is in there. Keep the wider box rather than opening on nowhere.
      freqBox = recentObservedExtent(fine, 1).box ?? recent.box;
    } catch (e) {
      degraded.push(`the coverage refinement pass failed (${describe(e)}): the view opens on the coarse observed box, which may be much wider than what was actually sampled.`);
    }
  }
  // The note's share is the SURFACE-wide census: it is a statement about the whole surface, and
  // quoting the refined pass's share (measured inside coverage, so near 100 %) would invert it.
  const opening = openingWindow(origin.bounds, freqBox && census.box
    ? { f0Hz: freqBox.f0Hz, f1Hz: freqBox.f1Hz, t0Ns: census.box.t0Ns, t1Ns: census.box.t1Ns }
    : null);
  // **The colour scale, decided here and only here** (T-470). Preferred from the refinement pass,
  // because that answer was measured over the observed region rather than over 6.5 GHz of mostly
  // grey; the coarse pass stands in when there was no refinement to make. Both are already in hand,
  // so the anchor costs nothing, and neither depends on where any viewport later goes.
  const range = shadeRange(fine, "the observed region")
    ?? shadeRange(cov, "the whole surface")
    ?? { ...FALLBACK_RANGE, source: FALLBACK_RANGE_SOURCE };
  return { lattice, lattices, origin, census, opening, range, note: orientationNote(census, opening), requests, degraded };
}

/**
 * The anchor a host will actually use, validated (T-470).
 *
 * The probe states one, but this runs in the constructor of the thing that draws **every pane**, and
 * a range that arrives non-finite or inverted would not produce a bad picture — it would produce
 * `NaN` out of the shader's divide and blank the screen. So a nonsensical range is replaced by the
 * stated fallback, which is the same direction as `sourceCellPx`'s totality: a renderer may not have
 * an input that turns the whole surface off.
 */
export function anchorOf(
  range: { lo?: number; hi?: number; source?: string } | null | undefined,
): { lo: number; hi: number; source: string } {
  const lo = range?.lo, hi = range?.hi;
  if (typeof lo === "number" && typeof hi === "number" && Number.isFinite(lo) && Number.isFinite(hi) && hi > lo) {
    return { lo, hi, source: range?.source ?? FALLBACK_RANGE_SOURCE };
  }
  return { ...FALLBACK_RANGE, source: FALLBACK_RANGE_SOURCE };
}

const describe = (e: unknown): string =>
  e instanceof ControlError ? `HTTP ${e.status} ${e.code}` : e instanceof Error ? e.message : String(e);

/** A pointer position in the drawing buffer, GL convention: origin **bottom-left**.
 *
 * Deliberately NOT exported: `ui/src/surface/input.ts` already exports this name, and two exported
 * spellings of one idea is the drift this directory keeps closing. Callers pass object literals. */
interface GlPoint { readonly x: number; readonly y: number }

const inRect = (r: PaneRect, p: GlPoint): boolean =>
  p.x >= r.x && p.x < r.x + r.w && p.y >= r.y && p.y < r.y + r.h;

/**
 * **Which pane a pointer belongs to — and the rule that the trace strip is part of its pane.**
 *
 * T-457 carves a strip off the top of each pane's rectangle for the spectrum trace. **The strip is a
 * readout, not a control: it passes every pointer event through to the pane it describes.** A point
 * in it resolves to that pane, and callers clamp it into the pane's own rectangle, so it reads as a
 * point on the pane's **top edge** — the same frequency, at the pane's newest instant, which is
 * exactly the instant the strip is a spectrum *of*. Nothing about a gesture changes because it
 * started a few pixels higher.
 *
 * The alternative — the strip handling pointers itself with a meaning of its own — was rejected
 * twice over: the obvious meaning for a vertical drag on a dB axis is *set the display range by
 * hand*, which is the control T-457 deliberately did not restore; and a second gesture vocabulary on
 * one canvas is T-412's wheel-zoom mismatch waiting to happen.
 *
 * **Why this is a function and not two lines inside `paneAt`.** T-457 and T-458 were each green
 * alone and broke on merge: one changed the geometry the other's gestures are measured in, and the
 * strip became a hole that swallowed every drag starting in it — not only the new region stroke, but
 * plain and alt drags that T-456 had settled. The invariant that catches that class is *"turning the
 * trace on may not shrink the set of points a gesture can start from"*, and it is only checkable if
 * the resolution is a pure function of a frame. `ui/test/surface-trace.test.ts` asserts it over a
 * grid of points, with and without the strip, knowing nothing about any particular gesture.
 */
export function paneAtPoint(frame: SurfaceFrame | null, minimapId: string, p: GlPoint): string | null {
  for (const v of frame?.views ?? []) {
    if (v.id === minimapId) continue;
    if (inRect(v.rect, p)) return v.id;
  }
  for (const t of frame?.traces ?? []) {
    if (inRect(t.rect, p)) return t.id;
  }
  return null;
}

/** `p` clamped into `rect`. A point in a pane's trace strip becomes a point on its top edge. */
export function clampToRect(rect: PaneRect, p: GlPoint): GlPoint {
  return {
    x: Math.min(Math.max(p.x, rect.x), rect.x + rect.w),
    y: Math.min(Math.max(p.y, rect.y), rect.y + rect.h),
  };
}

/** Wheel steps to a zoom factor. `> 1` zooms out, matching `PaneModel`'s own convention. */
export function zoomFactor(deltaY: number, deltaMode = 0): number {
  // `deltaMode` 1 is lines and 2 is pages; normalise to the pixel scale a trackpad reports.
  const px = deltaY * (deltaMode === 1 ? 16 : deltaMode === 2 ? 400 : 1);
  return Math.min(4, Math.max(0.25, Math.exp(px * 0.0015)));
}

/**
 * The scroll this wheel event actually carried, as one number.
 *
 * **Why `deltaX` is read at all, and only under shift.** macOS — and Chrome and Safari generally —
 * deliver a *shift-held* wheel as a HORIZONTAL scroll: the scroll arrives in `deltaX` and `deltaY`
 * is 0. Reading `deltaY` alone therefore makes shift+wheel, the frequency axis, do nothing at all on
 * the machine this runs on, and **no CDP-synthesised event would ever show it**, because the swap
 * happens in the platform's input layer rather than in the page. So the shift case takes whichever
 * axis carried the scroll.
 *
 * It is the **dominant** axis, never the sum. T-407's defect was travel measured as
 * `clientX + clientY`, which let a stroke *across* a bar count as travel *along* it; adding the two
 * deltas here would be the same mistake, and would make a diagonal trackpad flick zoom twice as far
 * as either of its components asked for. Without shift, `deltaX` is left alone entirely: a two-
 * finger horizontal swipe is a scroll, not a zoom, and on a canvas whose drag already pans it would
 * be startling for one to change the view's scale.
 */
export function wheelDelta(e: { deltaX?: number; deltaY?: number; shiftKey?: boolean }): number {
  const y = e.deltaY ?? 0, x = e.deltaX ?? 0;
  return e.shiftKey === true && Math.abs(x) > Math.abs(y) ? x : y;
}

/**
 * Which axes a wheel gesture zooms. Stated as data so the page can print the same table it obeys.
 *
 * **T-456 — Google-Maps navigation, with the axes still independently reachable:**
 *
 * | gesture | axes |
 * |---|---|
 * | drag | pans **both** (see [[SurfacePreview.drag]]) |
 * | plain wheel | zooms **both**, uniformly, about the cursor |
 * | **shift** + wheel | frequency (X) only |
 * | **alt / option** + wheel | time (Y) only |
 *
 * **Why ALT/OPTION for the time axis and not CTRL.** The brief allowed either and asked for the
 * reason; there are three, and the first is not testable from inside a browser at all:
 *
 *  1. **macOS takes ctrl+scroll before any browser sees it.** System Settings → Accessibility →
 *     Zoom → *"Use scroll gesture with modifier keys to zoom"* defaults to **^Control**, and when it
 *     is on the OS consumes the event: no `wheel` is dispatched, so there is nothing to
 *     `preventDefault` and no in-browser test can observe the difference between "the user did not
 *     scroll" and "the OS ate it". A binding whose failure mode is invisible to its own guard is the
 *     wrong binding.
 *  2. **ctrl+wheel is also how a trackpad PINCH arrives.** Chrome and Safari synthesise a pinch as a
 *     wheel event with `ctrlKey` set. Binding time to ctrl would make a pinch — the most Google-Maps
 *     gesture there is — zoom one axis. Leaving ctrl unbound drops a pinch into the uniform branch
 *     below, which zooms both axes about the cursor: exactly what a pinch should do.
 *  3. **Alt/Option carries no OS or browser default on a wheel**, so the `preventDefault` in
 *     `preview-main.ts` is a complete answer for it, where for ctrl it is only a partial one.
 *
 * Ctrl and meta are therefore *deliberately* not read here. They are still `preventDefault`ed at the
 * canvas, so a ctrl+wheel or cmd+wheel over the surface zooms the surface rather than the page.
 *
 * **This does not re-weld the axes.** Uniform zoom is a *gesture* that applies one factor to two
 * independent windows; each axis is still clamped on its own and still resolves its own pyramid
 * level (T-434/T-438/T-440). After a plain wheel the two may legitimately sit at different levels —
 * which is what `levelDivergenceNote` exists to say.
 */
export function wheelAxes(
  e: { shiftKey?: boolean; altKey?: boolean; ctrlKey?: boolean; metaKey?: boolean },
): { freq: boolean; time: boolean } {
  const freqOnly = e.shiftKey === true, timeOnly = e.altKey === true;
  // Both modifiers held is not a third gesture: it names both axes, which is the uniform one.
  if (freqOnly !== timeOnly) return { freq: freqOnly, time: timeOnly };
  return { freq: true, time: true };
}

/** The part of a `WheelEvent` a zoom gesture reads. A structural type, so this file stays testable
 * without a DOM and a host can pass the real event straight in. */
export interface WheelLike {
  readonly deltaX?: number;
  readonly deltaY?: number;
  readonly deltaMode?: number;
  readonly shiftKey?: boolean;
  readonly altKey?: boolean;
  readonly ctrlKey?: boolean;
  readonly metaKey?: boolean;
}

/**
 * **The whole of a wheel gesture, decided in one place: which axes, and by how much.**
 *
 * The surface has more than one host — the preview page, and the app's Explore centre after the
 * cutover — and *two hosts each doing their own wheel arithmetic is T-412's wheel-zoom mismatch by
 * construction*. So a host's listener does exactly three things: `preventDefault`, convert the
 * pointer to drawing-buffer coordinates, and call this. Nothing host-side may read `deltaY`,
 * `shiftKey` or `altKey` itself — `ui/test/surface-preview.test.ts` asserts that against the source
 * of every file in this directory that registers a `wheel` listener, because the failure mode of
 * getting it wrong is silent: `zoomFactor(e.deltaY, …)` compiles, runs, and makes shift+wheel inert
 * on macOS, where a shift-held wheel arrives in `deltaX`.
 */
export function wheelZoom(e: WheelLike): { factor: number; axes: { freq: boolean; time: boolean }; delta: number } {
  const delta = wheelDelta(e);
  return { delta, factor: zoomFactor(delta, e.deltaMode ?? 0), axes: wheelAxes(e) };
}

/**
 * **Ctrl+Shift+wheel is the shadow-brightness gesture, not a zoom** (T-526).
 *
 * Decided here, beside [[wheelAxes]], under the same rule the doc comment above states: a host's
 * listener may not read a modifier bit itself, so this is the one place that says what Ctrl+Shift on
 * a wheel means, and `input.ts` only calls it. It must be checked **before** [[wheelZoom]] — shift
 * alone already means "zoom frequency only", so the ordinary zoom path is a different gesture, not a
 * degraded form of this one.
 */
export function isShadowGainWheel(e: { ctrlKey?: boolean; shiftKey?: boolean }): boolean {
  return e.ctrlKey === true && e.shiftKey === true;
}

/** The part of a `PointerEvent` a drag gesture reads. */
export interface PointerLike {
  readonly shiftKey?: boolean;
  readonly altKey?: boolean;
  readonly ctrlKey?: boolean;
  readonly metaKey?: boolean;
}

/**
 * **What a press means: pan the view, or mark out a region** (T-458).
 *
 * It lives here, beside [[wheelAxes]], for the same reason and under the same rule: a host's
 * listener may not read a modifier bit itself, so there is exactly one file that says what a
 * modifier means, whichever event carries it. The source guards in
 * `ui/test/surface-preview.test.ts` and `ui/test/surface-cutover.test.ts` enforce that on
 * `input.ts` by name, and they pass unedited because of this function.
 *
 * **Shift, and the three that were rejected.**
 * - *Ctrl* is the same invisible failure T-456 rejected ctrl+wheel for, in its pointer form: on
 *   macOS ctrl+click **is** the secondary click, so the browser sends `contextmenu` and
 *   `button === 2` and the stroke silently becomes "open the menu".
 * - *Alt* is Chrome's copy-drag modifier and is grabbed by common Linux window managers to move the
 *   window — again, a gesture the page never learns it did not receive.
 * - *Right-drag* would have to fight the context menu, which is this surface's only route to
 *   Promote / Delete / Adjust band / Reset band.
 * - A *mode toggle* is a state a user can be in without noticing; the surface already has one such
 *   (Live/Paused) and a second would compound it. (A mode is still right for naming *which* signal
 *   a band override applies to, where the target has to be said out loud anyway — that is
 *   `explore.bandEdit`, and it selects the stroke's destination rather than arming the stroke.)
 *
 * **Why shift does not collide with T-456's `shift + wheel = frequency`.** A wheel and a captured
 * pointer drag are disjoint event streams — no event can be claimed by both bindings, and a user
 * cannot be mid-gesture in both — and the two readings are one idea rather than two: shift confines
 * the gesture to a *region of frequency* instead of sliding the whole view. Alt would have been the
 * real collision, since `alt + wheel` means "time only" and an alt-drag meaning "select" has no such
 * story.
 */
export function dragIntent(e: PointerLike): "pan" | "region" {
  return e.shiftKey === true ? "region" : "pan";
}

/**
 * **Touch has no Shift, so a finger says "region" by waiting** (T-824, docs/23 §10.5: "region
 * select = the retune *offer*"). A finger that rests at least this long before it travels
 * [[DRAG_PX]]-worth marks out a region; one that moves sooner pans. It is the phone's own
 * long-press-then-drag-to-select, and — like shift — it is decided ONCE, at the moment the stroke
 * first travels, and latched: a stroke that has started panning can never become a region, and a
 * region stroke never pans. A region still only *offers* a retune (T-444/T-476): nothing here, and
 * nothing the stroke commits, reaches a device route.
 */
export const HOLD_TO_MARK_MS = 450;

/** The touch twin of [[dragIntent]]: what a single finger's stroke means, from how long it rested
 * at the press before it first travelled. `heldMs` below zero or non-finite is a pan. */
export function touchIntent(heldMs: number): "pan" | "region" {
  return Number.isFinite(heldMs) && heldMs >= HOLD_TO_MARK_MS ? "region" : "pan";
}

/** The smallest finger spread a pinch is credited with, CSS px, so two fingers meeting cannot divide
 * by zero or fling the zoom (the same guard `navigators.ts` keeps for its strip). */
export const MIN_PINCH_SPREAD_PX = 12;

/**
 * **A two-finger pinch is the touch twin of a plain wheel** (T-824, docs/23 §10.5: "pinch = zoom
 * (view)"): uniform on both axes, so it takes [[SurfacePreview.wheel]]'s aspect lock, anchored at the
 * fingers' midpoint by the caller. `factor` is `PaneModel`'s convention — `> 1` zooms out — so
 * fingers spreading apart (`spread > prevSpread`) zoom in. Clamped per event to the same 4x bound as
 * [[zoomFactor]]. A view change only: it never reaches a route.
 */
export function pinchZoom(prevSpread: number, spread: number): { factor: number; axes: { freq: boolean; time: boolean } } {
  const a = Math.max(MIN_PINCH_SPREAD_PX, Number.isFinite(prevSpread) ? prevSpread : 0);
  const b = Math.max(MIN_PINCH_SPREAD_PX, Number.isFinite(spread) ? spread : 0);
  return { factor: Math.min(4, Math.max(0.25, a / b)), axes: { freq: true, time: true } };
}

export interface PreviewOptions {
  canvas: HTMLCanvasElement;
  probe: SurfaceProbe;
  token: string;
  fetchFn: TileFetch;
  /** Where `SurfaceChrome` mounts its per-viewport level readout. Null in a headless test. */
  chrome?: HTMLElement | null;
  /**
   * A per-viewport control on each pane's chrome row, and its press (T-476).
   *
   * Forwarded, never produced here: these are strings and a callback, so this host stays unable to
   * reach a device route and `./retune.ts` stays out of the preview's import graph — which
   * `ui/test/surface-preview.test.ts` asserts, and which is the whole reason the slot is typed as
   * `RowAction` rather than as an offer.
   */
  chromeAction?: RowActionFor | null;
  onChromeAction?: ((paneId: string) => void) | null;
  /** Capture-width presets on each pane's row, and their press (T-496). Forwarded the same way
   * `chromeAction` is — see that field's note; the same import-graph rule applies. */
  widthActions?: WidthActionsFor | null;
  onWidthAction?: ((paneId: string, key: string) => void) | null;
  minimapPx?: number;
  /**
   * **The growing edge, reported in (T-445).** Omit it and the surface is historical: the edge is
   * the one `probeSurface` resolved, it never advances, and every viewport opens frozen — which is
   * exactly T-450's preview and stays its behaviour unchanged.
   *
   * Supply it and the same host becomes the *live* view: the first pane opens **following**, and
   * `frame()` asks this function where capture has got to. It is still *reported in* and never
   * controlled from here — T-442's rule, unchanged: capture, the ring and detection are never
   * consulted by a gesture, and a pane's pause is still only its own time window.
   *
   * There is deliberately no second host class for "the live one". The live-versus-history split
   * is the seam docs/16 §8.5 retires; two hosts would be that seam moved into the client.
   */
  edge?: (() => number) | null;
  /**
   * The currently-active capture windows to light on the map, re-read every frame (T-445). The
   * preview passes none, because it does not read the live edge and a segment placed from a fixed
   * historical instant would be a live claim with no live evidence.
   */
  windows?: (() => readonly ActiveWindow[]) | null;
  /**
   * Extra stroked marks to draw **inside each pane**, re-derived on every frame from the state they
   * describe — signal boxes and selections (T-445, `./marks.ts`).
   *
   * Per frame, not per poll: a per-poll layout against a per-frame scroll is T-388's box-jump, and
   * the whole reason the boxes move here is that the mapping they are placed through is the *same*
   * `toClip` the tiles are placed through. Strokes only: `overlay.ts` has no sampler and no ramp, so
   * nothing drawn here can tint a measurement.
   */
  marks?: ((pane: PaneView, edgeNs: number) => readonly OverlayQuad[]) | null;
  /** Per-pane coverage-fog visibility (T-807), forwarded to `SurfaceView`'s `fog`. */
  fog?: ((paneId: string) => boolean) | null;
  /**
   * **The instantaneous spectrum trace** (T-457): quads for the strip carved off the top of each
   * pane. Like `marks`, a function called per frame — but handed the `PaneReport` the data pass just
   * produced as well, so the reduction behind it is at the level the picture beneath it was drawn at.
   *
   * The historical preview passes none. A trace of *this frame* needs a stream, and this host is
   * deliberately the one that reads no live edge.
   */
  trace?: ((pane: PaneView, edgeNs: number, report: PaneReport, strip: PaneRect) => readonly TracePath[]) | null;
  /** Height of that strip, device px. 0 draws no trace and gives the space back to the pane. */
  tracePx?: number;
  /** HUD axes (T-805, `./hud.ts`): the label layer, and the chrome's fade asked every frame. */
  hud?: HTMLElement | null;
  hudAlpha?: (() => number) | null;
  /** T-997: the floating chrome's top-left column; a time label that would print into it is dropped. */
  hudReserve?: (() => HudReserve | null) | null;
  /** Band-1 DOM marks laid out in the render frame (T-809, `./pins.ts`). See `SurfaceViewOptions.dom`. */
  dom?: ((
    panes: readonly PaneView[], edgeNs: number, canvasHpx: number, dpr: number,
    statuses: readonly PaneStatus[],
  ) => void) | null;
  /**
   * **Ask the coverage map before asking for tiles** (T-580, `./survey.ts`): how this host reads
   * `GET /api/coverage` for the survey. Supplied, no tile is requested until the first survey lands,
   * and a tile over spectrum it settles as never sampled is not requested at all; a following
   * surface re-asks every [[SURVEY_EVERY_MS]]. Omitted, every tile is fetched (the pre-T-580
   * behaviour), which is also what a failed survey falls back to.
   */
  survey?: ((path: string) => Promise<unknown>) | null;
  /** The clock the survey cadence is measured on, ms. Injected by tests; never a capture time. */
  now?: () => number;
  /**
   * **The transport for `GET /ws/tiles/rows`** (T-893, `./rowfeed.ts`'s [[wsRowOpener]] in a
   * browser). Supplied with an `edge`, every column a FOLLOWING pane draws at its live edge holds a
   * row subscription and rows reach the screen as they are recorded, instead of when the polling
   * lane next comes round. Omitted, the live edge advances by polling alone (T-460), as before.
   */
  rows?: RowOpener | null;
}

/**
 * The mounted surface: one `SurfaceView`, one fixed edge, and the gestures that move viewports.
 *
 * **Every gesture is a view change.** Pan and zoom call `PaneModel`/`Minimap` methods and nothing
 * else; there is no path from a pointer event to a device route, and `./retune.ts` is not imported.
 */
export class SurfacePreview {
  readonly view: SurfaceView;
  readonly probe: SurfaceProbe;
  /** The pane the chrome acts on (T-1000): see [[activePane]]. */
  private active: string;
  private readonly activeListeners = new Set<(id: string) => void>();
  /**
   * **The pane gestures and chrome apply to: the last one pressed, right-clicked, wheeled or chosen
   * by key.** An accessor rather than a field (T-1000) so that every writer — `input.ts`'s press and
   * wheel, a split, a close, the app's pane keys — tells [[onActiveChange]]'s listeners in the same
   * call, and the outline and the chrome that name the pane move in the same frame as the press. An
   * id that is not a pane is refused: an active pane that does not exist would name nothing.
   */
  get activePane(): string { return this.active; }
  set activePane(id: string) {
    if (id === this.active || !this.view.panes.has(id)) return;
    this.active = id;
    for (const f of this.activeListeners) f(id);
  }
  /** Be told when the active pane changes. Returns a disposer. Presentation only: a listener is
   * handed the new id and nothing else, and the change itself moved no view and reached no route. */
  onActiveChange(f: (id: string) => void): () => void {
    this.activeListeners.add(f);
    return () => { this.activeListeners.delete(f); };
  }
  lastFrame: SurfaceFrame | null = null;
  private readonly canvas: HTMLCanvasElement;
  private raf = 0;
  private disposed = false;
  private readonly edgeFn: (() => number) | null;
  private readonly windowsFn: (() => readonly ActiveWindow[]) | null;
  /** The newest edge seen. A live edge must never go backwards under the boxes placed on it. */
  private edgeSeen: number;
  /** The anchored range this host returns to, validated once. See [[anchorOf]]. */
  private readonly anchor: { lo: number; hi: number; source: string };
  private readonly surveyFn: ((path: string) => Promise<unknown>) | null;
  private readonly nowMs: () => number;
  private surveyInFlight = false;
  /** When the next survey may be asked, ms; 0 = at the first frame. */
  private surveyNextAt = 0;
  /** How far back the survey must reach: the surface's floor, widened to `recording_began_s`. */
  private surveyFloorNs = Number.POSITIVE_INFINITY;
  /** The survey requests this host has built, in order — the T-367 guard reads them. */
  readonly surveyRequests: string[] = [];
  /** Pushed rows for the following panes' columns (T-893); null without a transport or an edge. */
  readonly rowFeeds: LiveRowFeeds | null;

  constructor(opts: PreviewOptions) {
    const { probe } = opts;
    this.canvas = opts.canvas;
    this.probe = probe;
    this.edgeFn = opts.edge ?? null;
    this.windowsFn = opts.windows ?? null;
    this.edgeSeen = probe.origin.edgeNs;
    this.surveyFn = opts.survey ?? null;
    this.nowMs = opts.now ?? (() => Date.now());
    // A historical surface (no edge) follows nothing, so it never opens a feed.
    this.rowFeeds = opts.rows && this.edgeFn
      ? new LiveRowFeeds(opts.rows, (col, block) => this.view.surface.cache.applyRows(col, block), { now: this.nowMs })
      : null;
    this.view = new SurfaceView({
      canvas: opts.canvas,
      lattice: probe.lattice,
      lattices: probe.lattices,
      bounds: probe.origin.bounds,
      // T-573: the cache still asks for one address at a time — its slots, aborts and refresh
      // lane are per-tile facts — and `batchedTileSource` coalesces the calls one pump makes into
      // ONE `GET /api/tiles/batch`. A viewport render costs a small constant of requests instead
      // of one per tile, and nothing about how a tile is scheduled, aborted or decoded changes.
      cache: (tex) => new TileCache<TilePlanes>(tex, batchedTileSource(opts.token, opts.fetchFn)),
      minimapPx: opts.minimapPx ?? 120,
      chrome: opts.chrome ?? null,
      chromeAction: opts.chromeAction ?? null,
      onChromeAction: opts.onChromeAction ?? null,
      widthActions: opts.widthActions ?? null,
      onWidthAction: opts.onWidthAction ?? null,
      freq: probe.opening.freq,
      spanNs: probe.opening.spanNs,
      marks: opts.marks ?? null,
      fog: opts.fog ?? null,
      trace: opts.trace ?? null,
      tracePx: opts.tracePx ?? 0,
      hud: opts.hud ?? null,
      hudAlpha: opts.hudAlpha ?? null,
      hudReserve: opts.hudReserve ?? null,
      dom: opts.dom ?? null,
    });
    // **Anchor the colour scale before the first frame** (T-470). `Surface` opens anchored to its
    // own stated fallback, so this is the one place a *measured* scale replaces it — once, from the
    // probe, never from a viewport. Nothing below this line, and nothing in `frame()`, moves it.
    this.anchor = anchorOf(probe.range);
    this.view.surface.setScale(this.anchor.lo, this.anchor.hi, this.anchor.source);
    // **Freeze everything at open, unless an edge was reported in.** A following viewport borrows
    // the growing edge; without one there is nothing to borrow, so nothing follows (T-450's
    // historical preview). `pause` is a coordinate change (T-347/T-442), so this costs no frame and
    // no jump. With a live edge the first pane stays following and the map follows too — "live" is
    // then just the finest growing edge of this same surface (docs/16 §8.1), not a second mode.
    this.active = this.view.panes.list()[0].id;
    if (!this.edgeFn) {
      this.view.panes.pause(this.activePane, probe.origin.edgeNs);
      this.view.panes.goTo(this.activePane, probe.opening.centerNs);
    }
    this.view.minimap.setFollowing(!!this.edgeFn);
    // Coverage FIRST (T-580): with a survey source, nothing is requested until it has answered.
    if (this.surveyFn) this.view.surface.setSurvey("awaiting");
    // The map opens on the whole surface — it is the thing that says where the opened pane sits in
    // a mostly-grey world, which is half the answer to the empty-screen problem.
    this.view.minimap.setFreq(
      (probe.origin.bounds.f0Hz + probe.origin.bounds.f1Hz) / 2,
      probe.origin.bounds.f1Hz - probe.origin.bounds.f0Hz,
    );
  }

  /**
   * The newest instant this surface draws.
   *
   * Without an `edge` supplier it is the one `probeSurface` resolved and it never advances (the
   * historical preview). With one it is whatever capture has reported, clamped monotone: a
   * re-plumbed stream's first rows can repeat, and an edge that went backwards would drag every
   * following pane and every box on it backwards with it.
   *
   * **T-474 settled which side owns that (and it is not this one).** A `--loop` replay re-opens the
   * recording, so the *source's* timestamps restart every pass; the backend splices each pass onto
   * one monotone capture-time axis before anything is served (`hk_pipeline::capture`'s `Axis`),
   * because a clock that rewound would be silently dropped rows in the history pyramid, not a
   * drawing glitch. So the clamp here is a floor under a promise the server keeps, not a repair of
   * a wrap this client expects: if it ever starts biting, the bug is upstream of the browser.
   */
  get edgeNs(): number {
    if (!this.edgeFn) return this.probe.origin.edgeNs;
    const v = this.edgeFn();
    if (Number.isFinite(v) && v > this.edgeSeen) this.edgeSeen = v;
    return this.edgeSeen;
  }
  get bounds(): Box { return this.boundsNow ?? this.probe.origin.bounds; }
  private boundsNow: Box | null = null;

  /**
   * Extend the surface's time extent back to `t0Ns`, never forward (T-506).
   *
   * The probe sizes the time axis from the record horizon (`surfaceBounds`). On a young server that
   * is seconds old, which would leave the **retained capture window** — the IQ ring's configured
   * retention, which the canvas absorbed from the retired Capture panel (T-338) — partly
   * unreachable: its bound would sit below the floor, and neither it nor the IQ horizon could be
   * panned to. The host passes the window's start here, so the extent is always at least the
   * retention window and, once history outgrows it, the history horizon as before. The span added
   * is drawn by the one cell rule like any other (grey / unknown where nothing was recorded) — this
   * widens where a pane may look, never what is claimed there.
   */
  extendTimeFloor(t0Ns: number): boolean {
    const b = this.bounds;
    if (!Number.isFinite(t0Ns) || !(t0Ns < b.t0Ns)) return false;
    this.boundsNow = { ...b, t0Ns };
    this.view.setBounds(this.boundsNow);
    // A survey that does not reach the new floor is stale about the part it cannot see.
    this.surveyNextAt = 0;
    return true;
  }

  /** Draw one frame. With no `windows` supplier the list is empty — the preview reads no live edge,
   * and a lit segment placed from a fixed historical instant would be a live claim with no live
   * evidence. */
  frame(): SurfaceFrame {
    this.maybeSurvey();
    this.lastFrame = this.view.frame(this.edgeNs, this.windowsFn?.() ?? []);
    if (this.edgeFn) this.refreshLiveEdge(this.lastFrame);
    // **After the refresh, never before** (T-538): both end up spending the same four slots, and the
    // live edge must have had its chance at one before a guess is allowed to take it. In practice
    // [[TileCache.prefetchAhead]] cannot take it anyway — it asks only while the cache holds nothing
    // at all — but the ordering is the statement of priority and does not depend on that.
    this.prefetchFrozenPanes(this.lastFrame);
    return this.lastFrame;
  }

  /**
   * **Tell the cache the edge moved, so live actually advances** (T-460).
   *
   * Before this, `TileCache.acquire` answered a resident tile unconditionally and the only path that
   * could drop one was the retune, so a following pane redrew the *same* tile for the 256 s it took
   * to scroll into a new address: the newest rows were recorded, served and never asked for. The
   * request is made here rather than inside `SurfaceView` because this is the only object that knows
   * **which viewports are following** — a frozen pane is a view over data that cannot change, and
   * refreshing for it would be cost with nothing to show for it.
   *
   * It is guarded on `edgeFn`, so T-450's historical preview — which reports no edge and freezes
   * every viewport at open — issues no refresh at all and keeps its behaviour exactly.
   *
   * The levels come off the `PaneReport`s the renderer *just drew with*, never a second calculation
   * beside them: refreshing a level the pane is not showing would be the T-397 shape of defect (two
   * derivations of one number) applied to the fetch path.
   */
  private refreshLiveEdge(f: SurfaceFrame): void {
    const following: Viewport[] = [];
    const panes: Viewport[] = [];
    for (const r of f.reports) {
      const v = f.views.find((x) => x.id === r.id);
      if (!v) continue;
      const map = r.id === this.view.minimap.id;
      const live = map ? this.view.minimap.following : this.view.panes.isFollowing(r.id);
      if (!live) continue;
      following.push({ box: v.box, levelF: r.levelF, levelT: r.levelT });
      if (!map) panes.push({ box: v.box, levelF: r.levelF, levelT: r.levelT });
    }
    // Called with an EMPTY list too: that is how the cache learns nothing follows any more, and
    // drops the next-row look-ahead it was holding for a pane that has since frozen (T-890).
    this.view.surface.cache.refreshEdge(this.view.surface.lat, f.edgeNs, following);
    // **Rows pushed to the columns a following PANE draws** (T-893). The map is left to the polling
    // lane: its coarse rows commit every 2^level cells, and the route's feeds are few (16 a server).
    // A pane that froze drops out of `panes`, which closes its feeds — pausing never follows.
    this.rowFeeds?.want(this.view.surface.cache.liveColumns(this.view.surface.lat, f.edgeNs, panes));
  }

  /**
   * **Re-ask the coverage survey when it is due** (T-580). Once at open; again every
   * [[SURVEY_EVERY_MS]] while anything follows the live edge (the edge is where "never sampled"
   * stops being true, the moment the radio tunes there); again when the surface's floor moves. A
   * historical surface's edge never advances, so its first survey stays true and it asks once.
   *
   * A failed or unreadable survey drops back to fetching every tile — the saving is lost, never an
   * answer — and is retried on the same cadence.
   */
  private maybeSurvey(): void {
    const get = this.surveyFn;
    if (!get || this.surveyInFlight) return;
    const t = this.nowMs();
    if (t < this.surveyNextAt) return;
    const b = this.bounds;
    const t0 = Math.min(b.t0Ns, this.surveyFloorNs);
    const t1 = Math.max(b.t1Ns, this.edgeNs);
    if (!(t1 > t0) || !(b.f1Hz > b.f0Hz)) return;
    const path = surveyUrl(b.f0Hz, b.f1Hz, t0, t1);
    this.surveyRequests.push(path);
    this.surveyInFlight = true;
    const following = !!this.edgeFn;
    // Historical: one survey is the whole answer, unless it failed or could not see far enough back.
    this.surveyNextAt = following ? t + SURVEY_EVERY_MS : Number.POSITIVE_INFINITY;
    get(path).then(
      (body) => {
        const s = decodeSurvey(body as SurveyResponse);
        if (this.disposed) return;
        if (s && !s.complete) {
          // The survey says recording began before the window it was asked over, so rows a shadow
          // could come from were outside it: ask again over the whole past, and keep whatever the
          // surface had (still "awaiting" at open) rather than skip nothing in the meantime.
          // Only ever further back, so a server that keeps answering short cannot make this re-ask
          // on every frame: then it waits out the ordinary cadence like a failure.
          if (s.floorNs < this.surveyFloorNs) { this.surveyFloorNs = s.floorNs; this.surveyNextAt = 0; }
          else this.surveyNextAt = t + SURVEY_EVERY_MS;
          return;
        }
        if (!s) this.surveyNextAt = t + SURVEY_EVERY_MS;
        this.view.surface.setSurvey(s);
      },
      () => {
        if (this.disposed) return;
        this.surveyNextAt = t + SURVEY_EVERY_MS;
        this.view.surface.setSurvey(null);
      },
    ).finally(() => { this.surveyInFlight = false; });
  }

  /**
   * **The other half of [[refreshLiveEdge]]'s split** (T-538): the viewports that are **not**
   * following get the look-ahead lane, the ones that are get the refresh lane, and nothing is in
   * both.
   *
   * That split is the point, not bookkeeping. A following pane's box advances with the record on
   * every frame — it is moving without anyone moving it — so feeding it to a lane whose whole input
   * is *displacement* would turn "the user is panning" into "time is passing", which is a poll. The
   * minimap follows whatever the panes do and is excluded for exactly the same reason. What is left
   * is a frozen pane over recorded data, which moves only when a gesture moves it: a still one is
   * going nowhere and `prefetchAhead` issues nothing for it, by having no direction rather than by
   * being silenced.
   *
   * Unguarded by `edgeFn`, unlike [[refreshLiveEdge]]: T-450's historical preview freezes every
   * viewport at open, and a frozen pane is exactly what this lane is for. It still asks for nothing
   * until one of them is dragged.
   *
   * The levels come off the `PaneReport`s the renderer just drew with, for the T-397 reason
   * [[refreshLiveEdge]] gives: a second derivation of a number this frame already computed.
   */
  private prefetchFrozenPanes(f: SurfaceFrame): void {
    const frozen: MovingViewport[] = [];
    for (const r of f.reports) {
      const v = f.views.find((x) => x.id === r.id);
      if (!v) continue;
      const live = r.id === this.view.minimap.id
        ? this.view.minimap.following
        : this.view.panes.isFollowing(r.id);
      if (!live) frozen.push({ id: r.id, box: v.box, levelF: r.levelF, levelT: r.levelT });
    }
    if (frozen.length) this.view.surface.cache.prefetchAhead(this.view.surface.lat, frozen);
  }

  /** Match the drawing buffer to the element's CSS box at the device's pixel ratio. */
  resize(cssW: number, cssH: number, dpr = 1): boolean {
    const w = Math.max(1, Math.round(cssW * dpr)), h = Math.max(1, Math.round(cssH * dpr));
    if (this.canvas.width === w && this.canvas.height === h) return false;
    this.canvas.width = w;
    this.canvas.height = h;
    return true;
  }

  /** Run until [[dispose]]. Every overlay re-lays-out per frame, which is the T-388 rule. */
  start(): void {
    const tick = () => {
      if (this.disposed) return;
      this.frame();
      this.raf = requestAnimationFrame(tick);
    };
    tick();
  }

  dispose(): void {
    this.disposed = true;
    if (this.raf) cancelAnimationFrame(this.raf);
    this.rowFeeds?.close();
    this.view.dispose();
  }

  // ——— hit testing ———

  /** Is this point in the minimap strip rather than in a pane? */
  onMap(p: GlPoint): boolean {
    const r = this.lastFrame?.mapRect;
    return !!r && p.x >= r.x && p.x < r.x + r.w && p.y >= r.y && p.y < r.y + r.h;
  }

  /** The pane under a point, or null. See [[paneAtPoint]] for what "under" includes. */
  paneAt(p: GlPoint): string | null {
    return paneAtPoint(this.lastFrame, this.view.minimap.id, p);
  }

  private rectOf(id: string): { x: number; y: number; w: number; h: number } | null {
    return this.lastFrame?.views.find((v) => v.id === id)?.rect ?? null;
  }

  // ——— gestures. All of these are arithmetic on view state. ———

  /**
   * Drag: the data under the pointer stays under the pointer.
   *
   * `dx`/`dy` are in **drawing-buffer** pixels with y up (GL convention), which is why the time
   * term has no sign flip: moving the pointer toward the top of the screen is `dy > 0`, and the top
   * of a pane is the newest row, so the window walks backward in time by the same fraction.
   */
  drag(id: string, dx: number, dy: number): void {
    const r = this.rectOf(id);
    const box = this.lastFrame?.views.find((v) => v.id === id)?.box;
    if (!r || !box) return;
    this.view.panes.panFreq(id, -(dx / Math.max(1, r.w)) * (box.f1Hz - box.f0Hz));
    this.view.panes.panTime(id, -(dy / Math.max(1, r.h)) * (box.t1Ns - box.t0Ns));
  }

  /**
   * **The drag ended — commit the follow/pause decision** (T-486).
   *
   * Motion is per pointer move ([[drag]]); *deciding* whether the pane is paused is per gesture, and
   * this is where the gesture ends. Both halves of the reported bug live here: a 1 px twitch during
   * a frequency drag comes to rest inside the dead zone and stays live, and a drag back toward the
   * top released a few rows short — rows that appended *during* the drag — snaps to the edge instead
   * of resting nearly-following. The edge is read **now**, not at the last move, which is what makes
   * the second half work.
   */
  endDrag(id: string): void { this.view.panes.settleTime(id, this.edgeNs); }

  /** The same for the map, which is one pane's worth of the same model. */
  endDragMap(): void { this.view.minimap.settleTime(this.edgeNs); }

  /** The same for the map, which is one pane's worth of the same model. */
  dragMap(dx: number, dy: number): void {
    const r = this.lastFrame?.mapRect, box = this.lastFrame?.mapBox;
    if (!r || !box) return;
    this.view.minimap.panFreq(-(dx / Math.max(1, r.w)) * (box.f1Hz - box.f0Hz));
    this.view.minimap.panTime(-(dy / Math.max(1, r.h)) * (box.t1Ns - box.t0Ns));
  }

  /**
   * Wheel over a pane, **anchored so the cell under the pointer stays put** — on whichever axes
   * `wheelAxes` named, which after T-456 is both of them for a plain wheel.
   *
   * The two axes are still moved by **two separate calls with two separate anchors**, and each
   * clamps and resolves its level on its own: a uniform gesture is one factor applied twice, never
   * one level applied to two axes. That is the distinction T-434's de-welding rests on, and it is
   * why one plain wheel leaves the pane at two different levels — a legitimate outcome the chrome
   * states rather than hides.
   *
   * **T-472: the uniform branch takes the aspect lock, and it takes it here.** The earlier reading
   * of the sentence above — "`factor` may zoom frequency while time sits clamped" — was the bug the
   * user reported: past the end of the record the time axis pins, the frequency axis keeps widening,
   * and the *aspect ratio* of a gesture that promised uniformity walks away, jumping the view and
   * forcing a shift-scroll to recover. So when both axes are named, `PaneModel.zoomBoth` reduces the
   * factor to the one both can honour and applies that — stopping both together at either axis's
   * bound. Only the single-axis branches below, which the user asked for with **shift** (frequency)
   * or **alt** (time), may change the ratio.
   *
   * It lives on this side of the split — in `preview.ts`, over `PaneModel`, rather than in
   * `input.ts` — for T-456's reason: the surface has two hosts, and a handler that decided when to
   * stop a zoom would be a second opinion about what a wheel does.
   */
  wheel(id: string, p: GlPoint, factor: number, axes: { freq: boolean; time: boolean }): void {
    const r = this.rectOf(id);
    if (!r) return;
    const fx = clamp01((p.x - r.x) / Math.max(1, r.w));
    // `zoomTime`'s anchor is 0 = oldest (bottom of the pane) and 1 = newest, which is already the
    // GL y direction — so no flip here either.
    const ty = clamp01((p.y - r.y) / Math.max(1, r.h));
    if (axes.freq && axes.time) { this.view.panes.zoomBoth(id, factor, fx, ty); return; }
    if (axes.freq) this.view.panes.zoomFreq(id, factor, fx);
    if (axes.time) this.view.panes.zoomTime(id, factor, ty);
  }

  /** The same gesture on the map, which is a viewport and therefore gets the same lock. */
  wheelMap(p: GlPoint, factor: number, axes: { freq: boolean; time: boolean }): void {
    const r = this.lastFrame?.mapRect;
    if (!r) return;
    const fx = clamp01((p.x - r.x) / Math.max(1, r.w));
    const ty = clamp01((p.y - r.y) / Math.max(1, r.h));
    if (axes.freq && axes.time) { this.view.minimap.zoomBoth(factor, fx, ty); return; }
    if (axes.freq) this.view.minimap.zoomFreq(factor, fx);
    if (axes.time) this.view.minimap.zoomTime(factor, ty);
  }

  /** Send the active pane to a point on the map, keeping its spans. A view move, not a retune. */
  goToOnMap(p: GlPoint): void {
    const r = this.lastFrame?.mapRect;
    if (!r) return;
    const at = this.view.minimap.locate(
      (p.x - r.x) / Math.max(1, r.w),
      (p.y - r.y) / Math.max(1, r.h),
      this.edgeNs,
    );
    const pane = this.view.panes.get(this.activePane);
    if (!pane) return;
    this.view.panes.setFreq(this.activePane, at.hz, pane.freq.spanHz);
    this.view.panes.goTo(this.activePane, at.ns);
  }

  // ——— the two orientation actions ———

  /** Put the active pane back on observed coverage — the opening window, recomputed from the same
   * census. What a user reaches for after panning into the grey and losing the survey. */
  fitToCoverage(): void {
    const o = this.probe.opening;
    this.setWindow(this.activePane, o.freq.centerHz, o.freq.spanHz, o.centerNs, o.spanNs);
  }

  // ——— the colour scale ———

  /**
   * Turn the opt-in contrast tracker on, or go back to the anchor the probe measured (T-470).
   *
   * Returns the range now in force, so a caller can state it without asking twice. It is a *view*
   * control in the strictest sense — it changes nothing the backend sent and nothing about where
   * any pane is looking — but it is the one control that can make two zooms disagree about a
   * colour, which is why it is a deliberate press rather than a side effect of navigating.
   */
  setAutoScale(on: boolean): DisplayRange {
    return this.setRangeMode(on ? "auto" : "anchored");
  }

  /**
   * Put the surface on one of the three ways of deciding the range (T-528).
   *
   * Going back to `anchored` returns to **the anchor the probe measured**, not to wherever tracking
   * happened to leave the numbers: that is the one range in the set with a stated provenance a
   * viewer can check, so it is what "off" means here even though `Surface.setRangeMode` on its own
   * would hold the tracked pair.
   */
  setRangeMode(mode: RangeMode): DisplayRange {
    const s = this.view.surface;
    if (mode === "anchored") s.setScale(this.anchor.lo, this.anchor.hi, this.anchor.source);
    else s.setRangeMode(mode);
    return s.range;
  }

  /** The display range every pane is coloured by, with its provenance. What a legend states. */
  get range(): DisplayRange { return this.view.surface.range; }

  /** Zoom the active pane out to the whole surface. */
  fitToSurface(): void {
    const b = this.bounds;
    this.setWindow(
      this.activePane, (b.f0Hz + b.f1Hz) / 2, b.f1Hz - b.f0Hz,
      (b.t0Ns + b.t1Ns) / 2, b.t1Ns - b.t0Ns,
    );
  }

  /**
   * Put a pane on an absolute window. Span first, then centre: `PaneModel.normalise` clamps the
   * centre against the span it is being given, so setting the centre against the *old* span can
   * land it somewhere the new span would never have allowed.
   */
  private setWindow(id: string, centerHz: number, spanHz: number, centerNs: number, spanNs: number): void {
    const p = this.view.panes.get(id);
    if (!p) return;
    this.view.panes.setFreq(id, centerHz, spanHz);
    // `zoomTime` is the only span control the pane model offers, and at anchor 0.5 it holds the
    // centre — so this is a span change and the `goTo` after it is the centre change.
    this.view.panes.zoomTime(id, spanNs / Math.max(1, p.time.spanNs), 0.5);
    this.view.panes.goTo(id, centerNs);
  }

  split(dir: "columns" | "rows"): void {
    const id = this.view.panes.split(this.activePane, dir);
    if (id) this.activePane = id;
  }

  closeActive(): void {
    if (!this.view.panes.close(this.activePane)) return;
    this.activePane = this.view.panes.list()[0].id;
  }
}

const clamp01 = (v: number) => (Number.isFinite(v) ? Math.min(1, Math.max(0, v)) : 0);
