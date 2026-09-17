// The two edge navigators (T-340, corrected by T-367), one parallel to each waterfall axis.
//
// **The user's invariant** (CLAUDE.md, "Time, the waterfall, and the live view"): *each waterfall
// axis has an edge navigator parallel to it, **and the two control DIFFERENT axes**. The left
// vertical bar is the TIME navigator: it selects the time range and shows a compressed history
// waterfall **of the currently-selected frequency range only** — it never changes frequency. The
// bottom horizontal bar is the FREQUENCY navigator: it sets the centre and span across the whole
// device-available spectrum — it never scrubs time. Each pans and zooms only its own axis.*
//
// T-367 fixed two divergences from that:
//
//  - **The time bar's picture was of no frequency range at all.** It asked `/api/timeline` for an
//    overview without `f_lo`/`f_hi`, and that route answers a `null` grid with no region — so the
//    vertical bar, whose whole content is "what has been happening *here* over the retained
//    window", drew an empty canvas. It now asks over the range the main view is on
//    ([`ui/src/navigators.ts`]'s `timelineRequest`) and re-asks when that range moves, so the
//    picture follows the frequency selection without the bar ever *setting* one.
//  - **Each bar's gestures are now confined to its own axis by construction, not by inspection.**
//    Every time gesture resolves to a [`TimeTarget`] and is applied by [`applyTimeTarget`], which
//    writes the `time` cursor and nothing else; every frequency gesture resolves to a [`FreqZoom`]
//    and is applied by [`applyFreqZoom`], which writes the `live` slice and nothing else. The other
//    axis's slice therefore comes out of a gesture **bit-identical** (same object), which is what
//    `navigators.test.ts` asserts, alongside a source split: `mountTimeNav` names no frequency
//    writer and `mountFreqNav` names no time writer.
//
// Four properties this file exists to hold, each with a test:
//
//  1. **A navigator never moves the radio.** Panning and zooming either bar changes what is drawn.
//     Moving the front end is a device action (T-343): it can stop and re-plumb the running
//     capture and it takes the one radio. A region dragged *outside* the tuned band therefore
//     leaves a `RetuneOffer` in the store — the same offer a pan to the band edge leaves — and the
//     device is reached only when the user presses it, through `view.ts`'s `applyDeviceAction`
//     with `source: "navigator"`. Nothing here names a device route; `app-centre.test.ts` asserts
//     that against the source of every file under `src/`.
//  2. **The time bar spans the capture window, not the history horizon.** Its extent is
//     `GET /api/timeline`'s window (the IQ ring's configured retention, T-338), so every position
//     on it has capture behind it. `/api/navigation`'s `time` block is the *spectrum-history*
//     horizon — longer, tiered, lossy — and is read here only to name which tier would answer a
//     time zoom, never to size the bar.
//  3. **The lit segments are the reported windows.** `nav.windows` comes from `/api/navigation`'s
//     `windows` list. One segment appears on this server because one window is reported, not
//     because one is assumed; the same code draws N when N are reported (docs/api.md, `windows`).
//  4. **Each bar touches one axis.** The vertical bar's overview is scoped to the selected
//     frequency range and its gestures move only the time cursor; the horizontal bar's content is
//     the survey across the spectrum and its gestures move only the frequency view. Neither reads
//     the other's gesture, and neither writes the other's slice.
//  5. **(T-368) Grey means genuinely unobserved.** The frequency bar's survey strip is the backend
//     coverage map (`GET /api/coverage`), not an energy picture: over the device-available spectrum
//     most cells were never tuned to, and shading them from energy alone would paint never-observed
//     spectrum as *quiet*. `state` decides grey and nothing else does; a `shade` of zero is a
//     measured quiet cell and is drawn on the ramp, not grey.
//  6. **(T-376) The frequency bar has a viewport of its own**, opening centred on the current tune
//     rather than on the middle of a 1 MHz–6 GHz range (where a 2.4 MHz window is invisible, not
//     off-centre). The wheel zooms that viewport about the pointer with the waterfall's own
//     `wheelFactor`; an untouched viewport follows the tune, a user-framed one stays put. No wheel
//     and no pan on either bar reaches the device — property 1 is unchanged by it.
//
// Everything below is placement, gesture and styling. Which states are achievable, which windows
// are active and what the capture window spans are all backend answers (`ui/src/navigators.ts` is
// the pure arithmetic over them).
import * as ax from "../../axis";
import { EDGE_OFFER_FRAC } from "../../controls/gestures";
import {
  detailLabel, snapState, snapTimeCell, type DetailSource, type HistoryTier, type NavigationGrid,
} from "../../navigation";
import {
  activeWindows, bandKey, clockRangeText, clockText, coverageRequest, litSegments, placeOn,
  regionFromDrag, spanOf, spectrumExtent, surveyCells, surveyViewport, timeAtFraction, timeExtent,
  timelineRequest, unobservedCount, valueAt, zoomWithin, type Band, type CoverageCell,
  type CoverageResponse, type Range,
} from "../../navigators";
import {
  captureWindow, currentSpan, durationText, overviewShade,
  type CaptureWindow, type OverviewResponse, type TimelineResponse,
} from "../capture/timeline";
import type { AppContext } from "../context";
import { h } from "../dom";
// T-379: the survey strip is a view over the one (time × frequency) window, so it reads the same
// window the inventory lists do rather than defaulting to the server's live edge.
import { viewWindow } from "../explore/inventory";
import { startPoll } from "../net";
import { sameCursor } from "./review-render";
import { goLive, reviewAt, setNavigation, toast } from "../state";
import { applyDeviceAction, mayRetune, retuneAction, retuneLabel, setLiveView, setRetuneOffer } from "./view";

/** A range's edges in MHz, at a resolution taken from the range's own width — so the readout's
 * precision follows what is being shown and no frequency constant appears in this file. */
const fmtEdges = (lo: number, hi: number) => {
  const res = Math.max(1, (hi - lo) / 100);
  return `${ax.fmtMHz(lo, res)}–${ax.fmtMHz(hi, res)}`;
};

/** Pointer travel (px) that makes a press a deliberate drag rather than a click (overlays.ts). */
export const DRAG_PX = 6;

/** Cells the time navigator asks the backend to fold the capture window onto: what it draws, one
 * to one. Time runs down, so the long axis is the column count. */
const TIME_COLUMNS = 160;
const TIME_ROWS = 6;

/** How long the selected frequency range must hold still before the overview is re-asked for it
 * (T-367). A drag moves the view on every pointer event; this coalesces the storm into one request
 * without the bar lagging behind a settled selection. */
export const BAND_SETTLE_MS = 250;

/** Cells the frequency navigator asks the coverage map to fold the spectrum onto (T-368): what the
 * survey strip draws, one cell to one pixel column, upscaled but never smoothed. */
export const SURVEY_CELLS = 512;

// ---------------------------------------------------------------------------
// Frequency navigator — pure decisions
// ---------------------------------------------------------------------------

/** The main view panned by `deltaFrac` of the navigator's whole extent, clamped to the tuned band.
 *
 * The bar spans the device-available spectrum while the view can only move inside the tuned
 * window, so a small drag here is a large frequency step — which is the point of the bar, and the
 * reason the overshoot is reported rather than acted on. */
export function freqPan(g: ax.Geometry, v: ax.View, ext: Range, deltaFrac: number): { view: ax.View; overflowHz: number } {
  const d = Number.isFinite(deltaFrac) ? deltaFrac * spanOf(ext) : 0;
  return ax.panView(g, v, d);
}

/** What a region dragged on the frequency navigator resolves to. */
export type FreqZoom =
  /** Inside the tuned band: a display zoom over live IQ, and no device is involved. */
  | { kind: "view"; view: ax.View; source: DetailSource }
  /** Outside it: showing this needs the radio moved, so it is **offered**, never performed. */
  | { kind: "offer"; centerHz: number; view: ax.View; source: DetailSource }
  /** Nothing to do: no geometry and no live device (a replay), or a region with no width. */
  | { kind: "none" };

/**
 * The region `[lo, hi]` a drag selected, resolved against the achievable grid (T-341).
 *
 * A region that fits inside the tuned band is a **view change** — the waterfall already holds that
 * IQ. A region outside it cannot be shown without retuning, so this returns an `offer` carrying
 * the **snapped** centre (`snapState`; `null` step → the region's own centre, and then nothing
 * claims the device sits exactly there). `source` is the detail claim the resulting view would
 * carry, so the bar can say "survey overview" before the zoom happens rather than after.
 */
export function freqZoomTarget(
  grid: NavigationGrid, g: ax.Geometry | null, region: Range | null, live: boolean,
): FreqZoom {
  if (!region || !(region.hi > region.lo)) return { kind: "none" };
  const centre = (region.lo + region.hi) / 2, span = region.hi - region.lo;
  const snapped = snapState(grid, centre, span);
  if (g) {
    const full = ax.fullView(g);
    if (region.lo >= full.loHz && region.hi <= full.hiHz) {
      return { kind: "view", view: ax.zoomTo(g, region.lo, region.hi), source: snapped.source };
    }
  }
  if (!live) return { kind: "none" };
  return {
    kind: "offer",
    centerHz: snapped.centerHz ?? centre,
    view: { loHz: region.lo, hiHz: region.hi },
    source: snapped.source,
  };
}

/** The store, as both navigators take it. */
type AppStore = AppContext["store"];

/**
 * Applies what a **frequency** gesture resolved to.
 *
 * Every write here lands in the `live` slice (plus the toast, which belongs to no axis). Nothing in
 * this function can reach the time cursor, which is why a horizontal drag leaves `state.time` the
 * same object it was — the "bit-identical other axis" control in `navigators.test.ts`.
 */
export function applyFreqZoom(store: AppStore, target: FreqZoom): void {
  if (target.kind === "view") {
    store.set(setLiveView(target.view));
    store.set(toast(`Zoomed to ${fmtEdges(target.view.loHz, target.view.hiHz)} MHz · ${detailLabel(target.source)}`));
  } else if (target.kind === "offer") {
    store.set(setRetuneOffer({ centerHz: target.centerHz, view: target.view }));
    store.set(toast(`Outside the tuned window — ${retuneLabel(target.centerHz)} to see it (${detailLabel(target.source)}).`));
  }
}

// ---------------------------------------------------------------------------
// What the bars say under the cursor (T-393)
// ---------------------------------------------------------------------------
//
// One readout per bar, on that bar's own axis, shown **only while the pointer is on it** — no
// always-present ticks, and nothing added to the resting UI. Both are descriptions of state that is
// already in hand (the extent the bar is laid out on, the grid `/api/navigation` reported): no
// measurement is made here and no route is called.
//
// The frequency readout's rule, and the reason it is derived from `freqZoomTarget` rather than
// written beside it: **it must describe what selecting actually does in this build.** Today a region
// inside the tuned window is a view zoom and one outside it leaves a *retune offer* the user then
// presses (T-343). A readout that promised a retune the code does not perform would be exactly the
// kind of lie the honesty principle forbids, so the branch is taken from the same function the
// gesture takes it from — the two cannot disagree.

/** The resolution to print a frequency at, taken from the width being described — so no frequency
 * constant appears here (the same rule as `fmtEdges`). */
const resOf = (span: number) => Math.max(1, span / 100);

/** "centre C · span S (sample rate) · live IQ": the capture state a selection resolves to, snapped
 * to the achievable grid (T-341's `snapState`), and what each axis being unknown means. A live
 * window's span *is* its sample rate (`navigation.ts`), which is why the third field can name one. */
function configText(grid: NavigationGrid, centreHz: number, spanHz: number): string {
  const s = snapState(grid, centreHz, spanHz);
  const res = resOf(spanHz);
  // A null centre is "the source states no tuning step", never "anywhere is reachable" (rule 1 in
  // navigation.ts): nothing here may claim the device can sit exactly where the user pointed.
  const centre = s.centerHz === null ? "centre not on any stated grid" : `centre ${ax.fmtMHz(s.centerHz, res)} MHz`;
  const span = s.spanHz === null ? "span unknown" : `span ${ax.fmtBandwidth(s.spanHz)}`;
  const rate = s.source === "live-iq" && s.spanHz !== null ? " (sample rate)" : "";
  return `${centre} · ${span}${rate} · ${detailLabel(s.source)}`;
}

/**
 * Hover on the **frequency** bar: the frequency under the cursor, and the capture state selecting
 * there would set.
 *
 * `spanHz` is the width the selection would keep — a hover is a point, and a point has no width of
 * its own to snap. With no span known the readout says only where the cursor is, rather than
 * inventing a window around it.
 */
export function freqHoverText(grid: NavigationGrid, ext: Range | null, frac: number, spanHz: number | null): string {
  if (!ext) return "";
  const hz = valueAt(ext, frac);
  const where = `${ax.fmtMHz(hz, resOf(spanOf(ext)))} MHz`;
  return spanHz !== null && spanHz > 0 ? `${where} · ${configText(grid, hz, spanHz)}` : where;
}

/**
 * Drag on the **frequency** bar: the range selected, and what selecting it does — taken from
 * `freqZoomTarget`, so the sentence and the gesture are the same decision.
 */
export function freqSelectText(
  grid: NavigationGrid, g: ax.Geometry | null, region: Range | null, live: boolean,
): string {
  const t = freqZoomTarget(grid, g, region, live);
  if (!region || t.kind === "none") return "";
  const edges = `${fmtEdges(region.lo, region.hi)} MHz`;
  if (t.kind === "view") return `${edges} · zooms the view, the radio stays put · ${detailLabel(t.source)}`;
  // The offer branch, said as an offer: pressing the button is what moves the front end (T-343).
  return `${edges} · outside the tuned window — offers "${retuneLabel(t.centerHz)}" · ${configText(grid, t.centerHz, spanOf(region))}`;
}

// ---------------------------------------------------------------------------
// Time navigator — pure decisions
// ---------------------------------------------------------------------------

/** What a region dragged on the time navigator resolves to: review the span ending at `tS`. */
export interface TimeZoom { tS: number; spanS: number; tier: HistoryTier | null }

/** The time cursor a gesture on the vertical bar resolves to — a time and a span, never a
 * frequency. The type is the constraint: there is no shape here that could move the other axis. */
export type TimeTarget = { live: true } | { live: false; tS: number; spanS: number | null };

/** The reviewed instant and span a cursor is at, with the live edge standing in for "now". */
type TimeCursorNow = { live: boolean; tS: number; spanS: number | null };

const cursorAt = (ext: Range, c: TimeCursorNow) => (c.live ? ext.hi : c.tS);

/** Narrowest span a time zoom resolves to. A view floor, not a claim about resolution: which cells
 * can answer a span is `snapTimeCell`'s answer, and it errs coarser (T-334). */
export const MIN_ZOOM_S = 1e-3;

/**
 * The cursor a **pan** of `deltaFrac` of the bar moves to: the reviewed instant slides along the
 * capture window, keeping whatever span is being reviewed, and reaching the newest edge is *live*.
 */
export function timePanTarget(ext: Range, cur: TimeCursorNow, deltaFrac: number): TimeTarget {
  const d = Number.isFinite(deltaFrac) ? deltaFrac * spanOf(ext) : 0;
  const next = Math.min(ext.hi, Math.max(ext.lo, cursorAt(ext, cur) + d));
  return next >= ext.hi ? { live: true } : { live: false, tS: next, spanS: cur.live ? null : cur.spanS };
}

/**
 * The cursor a **wheel** zoom of `factor` (> 1 zooms in) moves to: the reviewed *span* shrinks or
 * grows about the cursor, bounded by the capture window and never below `MIN_ZOOM_S`.
 *
 * With no span being reviewed yet, the first notch takes a fraction of the window rather than a
 * duration: a constant here would be a span nobody asked for.
 */
export function timeWheelTarget(ext: Range, cur: TimeCursorNow, factor: number): TimeTarget {
  if (!(factor > 0) || !Number.isFinite(factor)) return cur.live ? { live: true } : { live: false, tS: cur.tS, spanS: cur.spanS };
  const span = (cur.live ? 0 : (cur.spanS ?? 0)) || spanOf(ext) / 8;
  const next = Math.min(spanOf(ext), Math.max(MIN_ZOOM_S, span / factor));
  return { live: false, tS: Math.min(ext.hi, Math.max(ext.lo + next, cursorAt(ext, cur))), spanS: next };
}

/**
 * Applies what a **time** gesture resolved to — the one store write the vertical bar makes.
 *
 * `goLive` and `reviewAt` patch the `time` key and only that (`app/capture/slice.ts`), so a
 * vertical drag leaves `state.live` the same object it was: the frequency axis cannot move under a
 * time gesture, and the test asserts that by object identity rather than by eye.
 */
export function applyTimeTarget(store: AppStore, t: TimeTarget): void {
  store.set(t.live ? goLive : reviewAt(t.tS, t.spanS));
}

/**
 * The time zoom a dragged region asks for, and the pyramid tier that would answer it.
 *
 * `rows` is the rows the waterfall will draw, so `spanS / rows` is the cell the view wants;
 * `snapTimeCell` errs **coarser**, never finer (T-334), and the tier it names is what the bar
 * reports. Nothing here reduces or interpolates — the backend serves the span at the tier's own
 * resolution and the view replicates.
 */
export function timeZoomTarget(grid: NavigationGrid, region: Range | null, rows: number): TimeZoom | null {
  if (!region || !(region.hi > region.lo)) return null;
  const spanS = region.hi - region.lo;
  const want = rows > 0 ? spanS / rows : spanS;
  return { tS: region.hi, spanS, tier: snapTimeCell(grid.time, want) };
}

/**
 * Hover on the **time** bar: the clock time under the cursor, **on the capture clock**.
 *
 * `ext` is `GET /api/timeline`'s window, so the instant printed is an absolute capture time and the
 * browser's clock is not reachable from here. See the note above `clockText` in `navigators.ts` for
 * why that is stated so loudly, and `navigators.test.ts` for the assertion that pins it.
 */
export function timeHoverText(ext: Range | null, frac: number): string {
  return ext ? clockText(timeAtFraction(ext, frac)) : "";
}

/** Drag on the **time** bar: the selected time range and how long it is — "find me that window
 * twenty minutes ago" said in the terms the user is looking for. */
export function timeDragText(ext: Range | null, a: number, b: number): string {
  const r = regionFromDrag(ext, a, b);
  return r ? `${clockRangeText(r.lo, r.hi)} · ${durationText(spanOf(r))}` : timeHoverText(ext, b);
}

/**
 * What the time bar's **Live** control does (T-395, moved here from the Capture panel).
 *
 * Following the live edge is a *time-axis* decision — the same one a pan to the newest end of the
 * bar makes — so the control sits on the time navigator and goes through `applyTimeTarget` like
 * every other gesture on that bar. Moving the button moved no state: this dispatches the identical
 * `goLive` patch the Capture panel's pill used to, which is why the presence push (T-388), the view
 * window (T-379) and the live-only notes (T-387) are untouched by the relocation.
 */
export function goLiveFromNav(store: AppStore): void {
  applyTimeTarget(store, { live: true });
  store.set(toast("Back to live."));
}

/** The tier line beside a time zoom: which cells answer it, never a claim of live-IQ detail. */
export function timeDetailText(z: TimeZoom | null): string {
  if (!z) return "";
  const cell = z.tier ? ` · ${durationText(z.tier.t_cell_s)} cells` : "";
  return `${durationText(z.spanS)} · ${detailLabel("spectrum-history")}${cell}`;
}

// ---------------------------------------------------------------------------
// Shared pointer plumbing
// ---------------------------------------------------------------------------

interface BarDrag {
  /** Fraction along the bar, 0 at its `lo` end. */
  frac(e: PointerEvent): number;
  onPan(deltaFrac: number): void;
  onPanEnd(): void;
  onRegion(a: number, b: number, done: boolean): void;
  /** Whether the press started on the marker showing the current view (pan) or the bar (region). */
  onMarker(e: PointerEvent): boolean;
}

/** Drag-to-pan on the view marker, drag-a-region anywhere else. Both are view gestures; neither
 * has any path to the control API. */
function attachBar(el: HTMLElement, d: BarDrag) {
  let mode: "pan" | "region" | null = null, startFrac = 0, lastFrac = 0, downPx = 0, moved = false;
  const axisPx = (e: PointerEvent) => e.clientX + e.clientY; // only differences are used
  el.addEventListener("pointerdown", (e) => {
    if ((e.target as Element | null)?.closest?.("button, a, input, select")) return;
    if (e.pointerType === "mouse" && e.button !== 0) return;
    mode = d.onMarker(e) ? "pan" : "region";
    startFrac = lastFrac = d.frac(e);
    downPx = axisPx(e);
    moved = false;
    el.setPointerCapture(e.pointerId);
    el.classList.add("dragging");
    e.preventDefault();
  });
  el.addEventListener("pointermove", (e) => {
    if (!mode) return;
    const f = d.frac(e);
    if (Math.abs(axisPx(e) - downPx) >= DRAG_PX) moved = true;
    if (mode === "pan") { d.onPan(f - lastFrac); lastFrac = f; }
    else if (moved) d.onRegion(startFrac, f, false);
  });
  const end = (e: PointerEvent) => {
    if (!mode) return;
    el.classList.remove("dragging");
    if (mode === "pan") d.onPanEnd();
    else if (moved && e.type === "pointerup") d.onRegion(startFrac, d.frac(e), true);
    else d.onRegion(startFrac, startFrac, true); // a click: clear any draft, select nothing
    mode = null;
  };
  el.addEventListener("pointerup", end);
  el.addEventListener("pointercancel", end);
}

const pctStyle = (e: HTMLElement, p: { startPct: number; sizePct: number }, vertical: boolean) => {
  e.style[vertical ? "top" : "left"] = `${p.startPct}%`;
  e.style[vertical ? "height" : "width"] = `${p.sizePct}%`;
};

// ---------------------------------------------------------------------------
// The frequency navigator: a horizontal bar along the bottom
// ---------------------------------------------------------------------------

function mountFreqNav(el: HTMLElement, ctx: AppContext) {
  const { store, client } = ctx;
  el.replaceChildren();

  // T-368: the survey strip. Its cells are the backend's coverage map, so the bar can show what
  // was actually sampled across the spectrum and grey only what nothing ever looked at.
  const strip = h("canvas", {
    class: "fn-survey", role: "img",
    "aria-label": "Survey coverage across the device-available spectrum; grey is never observed",
  }) as HTMLCanvasElement;
  const litLayer = h("div", { class: "fn-lit-layer" });
  const marker = h("div", { class: "fn-view", title: "The frequency window on screen — drag to pan it" });
  const draft = h("div", { class: "fn-draft", hidden: true });
  const label = h("span", { class: "fn-label" });
  // T-393: a static name for the axis this bar controls. The two bars look alike and do different
  // things, and the user asked for each to say which axis is its own.
  const axisLabel = h("span", { class: "fn-axis-label" }, "FREQ");
  // T-393: the hover/drag readout. Present only while the pointer is on the bar — a cursor-following
  // line, never a row of always-on ticks.
  const readout = h("div", { class: "fn-readout" });
  readout.hidden = true;
  const offerBtn = h("button", { class: "fn-offer", type: "button" }, "");
  offerBtn.hidden = true;
  const track = h("div", { class: "fn-track" }, strip, litLayer, marker, draft, readout, offerBtn);
  el.replaceChildren(track, axisLabel, label);

  // T-376: the bar has a viewport of its own. `bounds` is the whole device-available spectrum as
  // the front end reported it (the union of `ranges_hz`); `extent` is the slice of it on the bar.
  const bounds = (): Range | null => spectrumExtent(store.get().navGrid.grid?.frequency ?? null);
  /** The viewport the user last zoomed to, and whether they ever did. An untouched viewport
   * follows the tune centre; a touched one is left where they put it (see `surveyViewport`). */
  let viewport: Range | null = null;
  let viewportTouched = false;
  const tuned = () => store.get().navGrid.grid?.frequency?.current ?? null;
  /** The live window's geometry, or null when nothing is tuned. */
  const geom = (): ax.Geometry | null => {
    const l = store.get().live;
    return l.centerHz !== null && l.bandwidthHz !== null && l.bins !== null
      ? { centerHz: l.centerHz, bandwidthHz: l.bandwidthHz, bins: l.bins } : null;
  };
  /** T-393: the text a drag in flight is showing, and `null` when no drag owns the readout. */
  let dragText: string | null = null;
  const extent = (): Range | null => {
    const t = tuned();
    viewport = surveyViewport(bounds(), viewport, viewportTouched, t?.center_hz, t?.span_hz);
    return viewport;
  };

  /** The strip the backend last served, and the extent it was served for — so a readout never
   * describes a spectrum range the drawn pixels are not of. */
  let survey: CoverageCell[] = [];
  /** The extent the drawn strip was asked for, so a viewport change re-asks rather than restretching
   * cells of one range across another. */
  let surveyFor = "";

  const drawSurvey = () => {
    const c = strip.getContext("2d");
    if (!c) return;
    if (survey.length === 0) {
      // Not asked yet, or nothing served: draw nothing. An empty strip is not a strip of
      // unobserved cells — the bar has been told neither that something looked nor that nothing did.
      strip.width = strip.height = 1;
      c.clearRect(0, 0, 1, 1);
      return;
    }
    strip.width = survey.length;
    strip.height = 1;
    const img = c.createImageData(survey.length, 1);
    for (let i = 0; i < survey.length; i++) {
      const cell = survey[i];
      const p = i * 4;
      const v = cell.state === "observed" ? cell.shade : undefined;
      if (cell.state !== "observed") {
        // Never observed: grey. Never the low end of the ramp — nothing looked here, which is not
        // the same finding as looking and seeing nothing.
        img.data[p] = img.data[p + 1] = img.data[p + 2] = 110;
        img.data[p + 3] = 70;
      } else if (v === null || v === undefined) {
        // Sampled, but no level retained for it: a flat tint, distinct from grey and from the ramp.
        img.data[p] = 55; img.data[p + 1] = 85; img.data[p + 2] = 95; img.data[p + 3] = 150;
      } else {
        img.data[p] = Math.round(20 + 40 * v);
        img.data[p + 1] = Math.round(120 + 110 * v);
        img.data[p + 2] = Math.round(130 + 90 * v);
        img.data[p + 3] = Math.round(70 + 185 * v);
      }
    }
    c.putImageData(img, 0, 0);
  };

  offerBtn.addEventListener("click", () => {
    const offer = store.get().live.retuneOffer;
    // The one explicit user action on this bar that reaches the front end (T-343).
    if (offer) void applyDeviceAction(ctx, retuneAction(offer.centerHz, "navigator", offer.view));
  });

  const render = () => {
    const s = store.get();
    const ext = extent();
    // Lit segments come from the reported list. With none reported the bar says so rather than
    // drawing the tuned state as if it were an enumeration of front ends.
    litLayer.replaceChildren(...litSegments(s.navGrid.windows, ext).map((seg) => {
      const e = h("div", {
        class: "fn-lit",
        // Label resolution comes from the window's own width, never from a frequency constant.
        title: `Capture window${seg.deviceId ? ` on ${seg.deviceId}` : ""}: ${fmtEdges(seg.loHz, seg.hiHz)} MHz`,
      });
      pctStyle(e, seg, false);
      return e;
    }));

    const v = s.live.view;
    const p = v ? placeOn(ext, v.loHz, v.hiHz) : null;
    marker.hidden = !p;
    if (p) pctStyle(marker, p, false);

    const offer = s.live.retuneOffer;
    offerBtn.hidden = !offer;
    if (offer) {
      offerBtn.textContent = retuneLabel(offer.centerHz);
      const on = s.device.deviceId;
      offerBtn.title = `Moves the radio${on ? ` on ${on}` : ""}. A retune can briefly interrupt capture; panning and zooming do not.`;
      const op = placeOn(ext, offer.centerHz, offer.centerHz, 0);
      offerBtn.style.left = `${op ? Math.min(88, op.startPct) : 4}%`;
    }

    const n = s.navGrid.windows.length;
    // T-368: how much of the spectrum was never looked at, from the served coverage. `null` (not
    // asked yet) says so rather than reporting nothing observed.
    const grey = unobservedCount(survey);
    // T-379: the strip is stale if either half of the window moved — the frequency viewport under
    // it, or the time range it is of. Both are in the key, so a scrub says "coverage unknown" until
    // the answer for the scrubbed window is in, rather than showing the live edge's cells as if
    // they were of the window on screen.
    const coverage = grey === null || surveyFor !== surveyKey(ext)
      ? "coverage unknown"
      : `${grey} of ${survey.length} never observed`;
    label.textContent = ext
      ? `${fmtEdges(ext.lo, ext.hi)} MHz · ${s.navGrid.loaded ? `${n} active capture window${n === 1 ? "" : "s"}` : "capture windows unknown"} · ${coverage}`
      : s.navGrid.loaded ? "no navigable spectrum reported (no live front end)" : "navigation grid not loaded";
  };

  attachBar(track, {
    frac: (e) => ax.pointerFrac(e.clientX, track.getBoundingClientRect()),
    onMarker: (e) => !marker.hidden && (e.target === marker || marker.contains(e.target as Node)),
    // Panning moves the view inside the tuned band and stops at its edges. No branch of this
    // reaches the control API, at any pan distance — that is the T-343 property, restated here.
    onPan: (df) => {
      const s = store.get(), ext = extent(), g = geom();
      if (!g || !s.live.view || !ext) return;
      const r = freqPan(g, s.live.view, ext, df);
      panOverflow = r.overflowHz;
      store.set(setLiveView(r.view));
    },
    onPanEnd: () => {
      const s = store.get(), v = s.live.view;
      const w = v ? v.hiHz - v.loHz : 0;
      // Past the edge by more than the axis strip's own offer threshold: offer a retune. A store
      // write; the radio moves only when the button above is pressed.
      if (v && w > 0 && Math.abs(panOverflow) > EDGE_OFFER_FRAC * w && mayRetune(s.device)) {
        store.set(setRetuneOffer({
          centerHz: ax.panRetuneCenter(v, panOverflow),
          view: { loHz: v.loHz + panOverflow, hiHz: v.hiHz + panOverflow },
        }));
      }
      panOverflow = 0;
    },
    onRegion: (a, b, done) => {
      const s = store.get(), ext = extent();
      const region = regionFromDrag(ext, a, b);
      const p = region ? placeOn(ext, region.lo, region.hi, 0) : null;
      draft.hidden = !p || done;
      if (p && !done) pctStyle(draft, p, false);
      const g = geom();
      // T-393: while the drag is in flight the readout says what selecting *this* region does,
      // taken from the same `freqZoomTarget` the release will apply.
      dragText = done ? null : freqSelectText(s.navGrid.grid ?? { frequency: null, time: null }, g, region, mayRetune(s.device));
      showReadout(dragText ?? "", b);
      if (!done) return;
      applyFreqZoom(store, freqZoomTarget(s.navGrid.grid ?? { frequency: null, time: null }, g, region, mayRetune(s.device)));
    },
  });
  let panOverflow = 0;

  // ---- T-393: the hover/drag readout ----
  //
  // Hover says where the cursor is and **what selecting there would set** (centre / span / sample
  // rate, snapped to the achievable grid); a drag says the range and what selecting it does. Both
  // are shown only while the pointer is on the bar, and both are arithmetic over state already in
  // hand — the viewport extent, and the grid `/api/navigation` reported.
  function showReadout(text: string, frac: number) {
    readout.textContent = text;
    readout.hidden = !text;
    if (!text) return;
    // Kept inside the track so the line never hangs off the (overflow-hidden) bar.
    readout.style.left = `${Math.min(92, Math.max(8, frac * 100))}%`;
  }
  track.addEventListener("pointermove", (e) => {
    if (dragText !== null) return; // a drag in flight owns the line
    const s = store.get(), v = s.live.view;
    const frac = ax.pointerFrac(e.clientX, track.getBoundingClientRect());
    showReadout(
      freqHoverText(s.navGrid.grid ?? { frequency: null, time: null }, extent(), frac, v ? v.hiHz - v.loHz : null),
      frac,
    );
  });
  track.addEventListener("pointerleave", () => { dragText = null; readout.hidden = true; });

  // T-376: the wheel zooms **this bar's own viewport** about the pointer, with the same
  // `ax.wheelFactor` the waterfall uses, so the gesture matches. Zooming about the pointer pans as
  // it scales, which is how the bar is moved along the spectrum. It writes no store slice and
  // reaches no device route — a survey frame is not a tune (T-343), and T-340's control still
  // holds: no pan and no wheel on either bar can move the radio.
  track.addEventListener("wheel", (e) => {
    const b = bounds();
    const ext = extent();
    if (!b || !ext || e.deltaY === 0) return;
    e.preventDefault();
    const at = ax.pointerFrac(e.clientX, track.getBoundingClientRect());
    // Never narrower than one capture window: a survey frame inside the live window would claim to
    // resolve the spectrum more finely than the front end can open it (T-341's rule, on this axis).
    const floor = tuned()?.span_hz ?? spanOf(b) / SURVEY_CELLS;
    viewport = zoomWithin(b, ext, Math.min(1, Math.max(0, at)), ax.wheelFactor(e.deltaY, e.deltaMode), floor);
    viewportTouched = true;
    render();
    void refreshSurvey();
  }, { passive: false });

  /** Which (frequency viewport × time window) the drawn cells are of. Both halves, so neither can
   * move without the readout noticing (T-368 viewport, T-379 window). */
  const surveyKey = (ext: Range | null) => {
    const w = viewWindow(store.get());
    return `${ext ? `${ext.lo}:${ext.hi}` : ""}@${w ? `${w.t0}:${w.t1}` : ""}`;
  };

  /** Re-asks for the strip when the viewport it is drawn on, **or the window it is of**, has moved
   * (T-379: the survey is a view over the one (time × frequency) window, not over the live edge). */
  async function refreshSurvey() {
    const ext = extent();
    const key = surveyKey(ext);
    const path = coverageRequest(ext, SURVEY_CELLS, viewWindow(store.get()));
    if (!path) return;
    const body = await client.get<CoverageResponse>(path).catch(() => null);
    if (!body) return;
    survey = surveyCells(body);
    surveyFor = key;
    drawSurvey();
    render();
  }

  store.select((s) => s.navGrid, render, { immediate: true });
  store.select((s) => s.live.view, render);
  store.select((s) => s.live.retuneOffer, render);
  store.select((s) => s.device.deviceId, render);
  // The tune moved: an untouched viewport follows it, so the strip it is drawn on has to follow too.
  store.select((s) => s.navGrid.grid?.frequency?.current?.center_hz ?? null, () => {
    if (!viewportTouched) void refreshSurvey();
  });
  // T-379: scrubbing moves the time half of the window, so the strip is re-asked for that window —
  // the bug this closes is a bottom bar that kept answering about the live edge while the waterfall
  // above it showed an hour ago, greying bands that had in fact been observed then.
  store.select((s) => s.time, () => { render(); void refreshSurvey(); }, { eq: sameCursor });

  // The one poll that fills the navigation slice, read by both navigators.
  startPoll(async () => {
    const body = await client.get<NavigationGrid & Parameters<typeof activeWindows>[0]>("/api/navigation").catch(() => null);
    if (!body) return;
    store.set(setNavigation({ frequency: body.frequency ?? null, time: body.time ?? null }, activeWindows(body)));
  }, 15_000);

  // T-368: the survey strip's own poll. It asks about the **viewport** the bar is showing, so the
  // cells it draws are of exactly the range under them — zooming out re-asks over the wider range
  // rather than restretching one range's cells across another. Whether a cell is observed is
  // decided by the route from the tune history; nothing here infers it from the values it holds.
  startPoll(refreshSurvey, 15_000);
}

// ---------------------------------------------------------------------------
// The time navigator: a vertical bar on the side, oldest at the top
// ---------------------------------------------------------------------------

function mountTimeNav(el: HTMLElement, ctx: AppContext) {
  const { store, client } = ctx;
  el.replaceChildren();

  const canvas = h("canvas", {
    class: "tn-overview", role: "img",
    "aria-label": "Overview of the retained capture window, for the selected frequency range",
  }) as HTMLCanvasElement;
  const marker = h("div", { class: "tn-view", title: "The time span on screen — drag to move it" });
  const draft = h("div", { class: "tn-draft", hidden: true });
  // T-393: the static name of this bar's axis, and the cursor-following readout (clock time on
  // hover, the selected range during a drag).
  const axisLabel = h("div", { class: "tn-axis-label" }, "TIME");
  const readout = h("div", { class: "tn-readout" });
  readout.hidden = true;
  // T-395: **Live** lives here, at the foot of the time navigator, because following the live edge
  // is a time-axis choice — the same one a pan to the newest end of this bar makes. It moved off
  // the Capture panel, whose subject is the recording, not the view's time window.
  const liveBtn = h("button", {
    class: "tn-live", type: "button",
    title: "Follow the live edge. Capture, the ring and detection are always on — this moves the view, not the radio.",
  }, "● LIVE");
  const track = h("div", { class: "tn-track" }, canvas, marker, draft, readout);
  el.replaceChildren(axisLabel, track, liveBtn);
  liveBtn.addEventListener("click", () => goLiveFromNav(store));

  // The capture window this bar is laid out on (T-338): `GET /api/timeline`'s window, which is the
  // IQ ring's configured retention. `null` = not answered, or this server has no capture window;
  // the bar then draws nothing rather than inventing a span.
  let win: CaptureWindow | null = null;
  const extent = (): Range | null => timeExtent(win);

  // The frequency range this bar's overview is *of* (T-367): the range the main view is on, never
  // the whole spectrum. Reading it is not setting it — no gesture below writes a frequency, and the
  // bar follows the horizontal axis rather than steering it.
  const selectedBand = (): Band | null => {
    const s = store.get();
    return currentSpan({ live: s.live.view, device: s.device });
  };
  /** The band the drawn picture was actually asked for, so the readout cannot claim a range the
   * canvas is not showing (a poll in flight when the selection moves). */
  let drawn: Band | null = null;

  const rowsOnScreen = () => Math.max(1, Math.round(track.clientHeight || 256));

  const renderOverview = (grid: OverviewResponse | null) => {
    const c = canvas.getContext("2d");
    if (!c) return;
    if (!grid || grid.nt <= 0 || grid.nf <= 0) {
      canvas.width = canvas.height = 1;
      c.clearRect(0, 0, 1, 1);
      return;
    }
    // Time runs down, so the grid's time axis is the canvas's height.
    canvas.width = grid.nf;
    canvas.height = grid.nt;
    const img = c.createImageData(grid.nf, grid.nt);
    for (let t = 0; t < grid.nt; t++) {
      for (let f = 0; f < grid.nf; f++) {
        const v = overviewShade(grid, t * grid.nf + f);
        const p = (t * grid.nf + f) * 4;
        if (v === null) {
          // Not observed: grey, never the colour scale's low end — a gap is not a quiet band.
          img.data[p] = img.data[p + 1] = img.data[p + 2] = 110;
          img.data[p + 3] = 70;
        } else {
          img.data[p] = Math.round(20 + 40 * v);
          img.data[p + 1] = Math.round(120 + 110 * v);
          img.data[p + 2] = Math.round(130 + 90 * v);
          img.data[p + 3] = Math.round(70 + 185 * v);
        }
      }
    }
    c.putImageData(img, 0, 0);
  };

  const render = () => {
    const ext = extent();
    const t = store.get().time;
    // T-395: the control shows which of the two states the view is in, exactly as the pill it
    // replaced did, and it says so even before a capture window has been answered — whether the
    // view is following is view state, not a fact about the server. It never disables itself:
    // pressing LIVE while live is a harmless no-op, and a disabled control would read as "this bar
    // cannot follow the live edge".
    liveBtn.classList.toggle("following", t.live);
    liveBtn.setAttribute("aria-pressed", String(t.live));
    if (!ext) { marker.hidden = true; track.title = "No capture window on this server"; return; }
    // Both halves of what this bar is, said together: how long it spans, and which frequencies the
    // picture on it is of. The second half is the T-367 correction made visible.
    track.title = `Capture window: ${durationText(spanOf(ext))} of retained IQ · ${
      drawn ? `${fmtEdges(drawn.loHz, drawn.hiHz)} MHz` : "no frequency range selected"}`;
    // Live follows the newest edge; reviewing marks the span the waterfall is showing.
    const tHi = t.live ? ext.hi : t.tS;
    const spanS = t.live ? 0 : (t.spanS ?? 0);
    const p = placeOn(ext, tHi - spanS, tHi, 1.5);
    marker.hidden = !p;
    if (p) pctStyle(marker, p, true);
    track.classList.toggle("reviewing", !t.live);
  };

  attachBar(track, {
    frac: (e) => {
      const r = track.getBoundingClientRect();
      return r.height > 0 ? Math.min(1, Math.max(0, (e.clientY - r.top) / r.height)) : 1;
    },
    onMarker: (e) => !marker.hidden && (e.target === marker || marker.contains(e.target as Node)),
    // Panning the time bar moves the reviewed instant, and only that. It touches the store's time
    // cursor and nothing else: capture, the ring and detection are always-on (CLAUDE.md, "Pause
    // freezes the view, not the capture"), no time gesture has ever had a path to a device route,
    // and — T-367 — none has a path to the frequency view either.
    onPan: (df) => {
      const ext = extent();
      if (!ext) return;
      applyTimeTarget(store, timePanTarget(ext, cursorNow(), df));
    },
    onPanEnd: () => { /* a time pan settles where it is: nothing to offer, nothing to command */ },
    onRegion: (a, b, done) => {
      const ext = extent();
      const region = regionFromDrag(ext, a, b);
      const p = region ? placeOn(ext, region.lo, region.hi, 0) : null;
      draft.hidden = !p || done;
      if (p && !done) pctStyle(draft, p, true);
      // T-393: mid-drag the readout is the selected time RANGE, on the capture clock, so a user can
      // stop the drag on the window they were looking for.
      dragText = done ? null : timeDragText(ext, a, b);
      showReadout(dragText ?? "", b);
      if (!done || !region) return;
      const z = timeZoomTarget(store.get().navGrid.grid ?? { frequency: null, time: null }, region, rowsOnScreen());
      if (!z) return;
      applyTimeTarget(store, { live: false, tS: z.tS, spanS: z.spanS });
      store.set(toast(`Zoomed to ${timeDetailText(z)}`));
    },
  });

  // ---- T-393: the hover/drag readout ----
  //
  // Hover prints **the clock time at the cursor**; a drag prints the selected range and its length.
  // Both come off `timeExtent(win)` — `GET /api/timeline`'s window — so every instant shown is an
  // absolute *capture* time. Nothing here can reach the browser's clock, which is the point.
  let dragText: string | null = null;
  function showReadout(text: string, frac: number) {
    readout.textContent = text;
    readout.hidden = !text;
    if (!text) return;
    // Time runs down, so the line follows the pointer on the vertical axis, kept inside the track.
    readout.style.top = `${Math.min(94, Math.max(2, frac * 100))}%`;
  }
  track.addEventListener("pointermove", (e) => {
    if (dragText !== null) return; // a drag in flight owns the line
    const r = track.getBoundingClientRect();
    const frac = r.height > 0 ? Math.min(1, Math.max(0, (e.clientY - r.top) / r.height)) : 1;
    showReadout(timeHoverText(extent(), frac), frac);
  });
  track.addEventListener("pointerleave", () => { dragText = null; readout.hidden = true; });

  // Wheel zooms the reviewed span about the pointer, on the time axis only.
  track.addEventListener("wheel", (e) => {
    const ext = extent();
    if (!ext || e.deltaY === 0) return;
    e.preventDefault();
    applyTimeTarget(store, timeWheelTarget(ext, cursorNow(), ax.wheelFactor(e.deltaY, e.deltaMode)));
  }, { passive: false });

  /** The time cursor as the gesture functions take it; `tS` is unused while live. */
  function cursorNow() {
    const t = store.get().time;
    return t.live ? { live: true, tS: 0, spanS: null } : { live: false, tS: t.tS, spanS: t.spanS ?? null };
  }

  store.select((s) => s.time, render, { immediate: true });

  // ---- the picture: the capture window folded over the SELECTED FREQUENCY RANGE (T-367) ----
  //
  // Before this the request carried no `f_lo`/`f_hi`, and `/api/timeline` answers a `null` grid
  // with no region — so the bar drew nothing at all. It now asks over the range the main view is
  // on, and re-asks when that range moves, which is the property: *the time navigator's overview
  // changes when the selected frequency range changes*.
  //
  // `seq` is the ordering guard. A band change and the 60 s poll can be in flight together, and
  // drawing whichever answered last would put one frequency range's energy on a bar labelled with
  // another's — the same class of lie as an unscoped overview.
  let seq = 0, settle = 0;
  const fetchOverview = async () => {
    const band = selectedBand();
    const mine = ++seq;
    const tl = await client.get<TimelineResponse>(timelineRequest(band, TIME_COLUMNS, TIME_ROWS)).catch(() => null);
    if (mine !== seq) return;
    win = captureWindow(tl);
    drawn = tl?.grid ? band : null;
    renderOverview(tl?.grid ?? null);
    render();
  };

  // Follow the frequency selection. Coalesced: a frequency pan moves the view every pointer event,
  // and one request per event would ask the pyramid to re-fold the window a hundred times a drag.
  store.select((s) => bandKey(currentSpan({ live: s.live.view, device: s.device })), () => {
    clearTimeout(settle);
    settle = window.setTimeout(() => { void fetchOverview(); }, BAND_SETTLE_MS);
  });

  startPoll(fetchOverview, 60_000);
}

export const navigatorMounts = { freqnav: mountFreqNav, timenav: mountTimeNav };
