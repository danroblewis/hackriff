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
// Eight properties this file exists to hold, each with a test:
//
//  1. **No *continuous* gesture moves the radio; one discrete one does (T-392).** Panning and
//     wheel-zooming either bar changes what is drawn and nothing else — a pan past the band edge
//     leaves a `RetuneOffer` in the store for the user to press, which is T-340's control (a ±1.0
//     drag of the whole 6 GHz bar reaching no route) and is unchanged. The single exception is
//     **region-select on the frequency bar**, which the user made *"the one navigator action that
//     commands the radio"*: unlike time, which is always a view over already-captured data, a
//     frequency outside the current window can only be reached by tuning there, so the drag
//     computes the covering capture config and applies it. It fires **on release**, with no
//     confirmation step in front of it — the user asked for the button removed from this gesture —
//     so the device action is built after the mount's `if (!done) return;` guard and nowhere else.
//     It still goes through `view.ts`'s gated, typed `applyDeviceAction` with `source: "navigator"`
//     and the device's `device_id` recorded; nothing here names a device route, which
//     `app-centre.test.ts` asserts against the source of every file under `src/`.
//
//     **The pan keeps its offer**, deliberately. A pan has no target region to interpret — the user
//     drags and stops — so there is nothing to fire on, and retuning at the end of a pan that ran
//     past the band edge would make the radio move as a side effect of scrolling. The offer is not
//     an orphan either: the waterfall's own edge pan (`viewHooks().edgeOffer`) and the axis strip's
//     button are the same mechanism. So "no confirmation" applies to the gesture that names a
//     destination, and the gesture that does not still asks.
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
//  7. **(T-397, T-411) Each bar's render buffer is its own size, and both draw in the waterfall's
//     colours.** The two strips asked the backend for grids sized by constants picked when the bars
//     were ~12 px slivers — 160 × **6** for the time bar, and a **single row** for the frequency bar
//     — and CSS then stretched those few cells across the widened bars. That is upsampling in the
//     client, which is exactly what made the left overview blocky and the bottom strip one smeared
//     spectrum. Both now ask `stripCells(px, cap)` for one cell per pixel on each axis, so every
//     drawn cell is a cell the backend folded; where the pyramid's own tier is coarser it replicates
//     there and `resolution` says so, which is the direction that repeats a measurement instead of
//     interpolating one. **The bottom bar is a mini-waterfall**, not a stretched row: the same
//     `/api/timeline` fold the left bar uses, with the axes swapped over — the left bar collapses
//     frequency over the selected range, this one collapses nothing and lays the survey range
//     across its width. And both paint through `ui/src/cmap.ts`, the one place the ramp lives, so a
//     peak is yellow here and yellow in the waterfall rather than cyan here and yellow there.
//  8. **(T-405) The frequency bar has no view box; the *undimmed* region is the tuned window.**
//     The user removed `.fn-view` — *"I rarely slide the view within the tuned range"* — so what
//     used to be a rectangle drawn over the reported windows is now their **complement**, shaded
//     down (`dimSegments`). It cannot disagree with the lit windows because it is computed from
//     them, and with no window reported the whole bar dims rather than one being assumed.
//     Drag-to-retune (property 1) is untouched; the transient `.fn-draft` still shows it in flight.
//  9. **(T-412) On the time axis the wheel PANS; span-zoom is the secondary gesture. (T-407) Touch
//     resolves to the same arithmetic as the mouse.** The user's correction: *"the TIME axis wheel
//     should PAN / SCROLL THROUGH TIME — wheel up/down moves the viewed window earlier/later,
//     keeping the duration/span FIXED — NOT zoom the time window … the user scrolls through time
//     constantly and rarely resizes."* So the plain wheel on the vertical bar is `timeWheelPan`,
//     which is `timePanTarget` — **the same pan a drag runs** — with a notch converted to a
//     distance; zoom moved to ctrl/cmd + wheel, a pinch, and the region drag that always did it.
//     The frequency bar's wheel is unchanged and still zooms.
//
//     **Panning at the ends keeps the span.** `timePanTarget` now clamps the whole reviewed
//     *window* through `clampInto` (which shifts and never resizes) rather than clamping its end
//     instant, so a pan into the oldest edge stops with the window inside the capture and the span
//     comes out the number it went in as. A pan that silently resized was the bug in the other
//     direction, and the mirror of it is the one this must not introduce.
//
//     **One gesture, several input devices.** A pinch is the touch spelling of the wheel and a
//     touch drag is the existing pan, so neither forks: `pinchFactor` returns a factor in
//     `ax.wheelFactor`'s own units, and both feed `zoomTimeBy` / `zoomFreqBy`, one per bar, which
//     each bar's wheel handler also calls. Touch usability is the rest: hit targets grown for a
//     finger (`grabTolerancePx` on the view marker, `@media (pointer: coarse)` on the controls),
//     a drag threshold that is the *input device's* (`dragThresholdPx`) measured along the **bar's
//     own axis**, and `touch-action: none` scoped to `.freqnav` / `.timenav` so a drag on a bar
//     does not scroll the page while every other surface scrolls normally.
//
//     **And changing what a gesture means changed nothing about what it can reach.** Property 1
//     holds unaltered on every new input: the committed region select on the frequency bar is
//     still the only path to the radio, and a tap, a cross-axis stroke and a pinch all miss it by
//     construction — a tap and a stroke never pass the threshold *along the bar*, and a pinch is
//     cancelled rather than committed (`onCancel`).
//
// Everything below is placement, gesture and styling. Which states are achievable, which windows
// are active and what the capture window spans are all backend answers (`ui/src/navigators.ts` is
// the pure arithmetic over them).
import * as ax from "../../axis";
import { EDGE_OFFER_FRAC } from "../../controls/gestures";
import {
  detailLabel, retunePlan, snapState, snapTimeCell,
  type DetailSource, type FrequencyGrid, type HistoryTier, type NavigationGrid, type RetuneRefusal,
} from "../../navigation";
import { cmapBytes } from "../../cmap";
import {
  DRAG_PX, TOUCH_DRAG_PX, activeWindows, bandKey, clampInto, clockRangeText, clockText,
  coverageRequest, dimSegments,
  dragThresholdPx, grabTolerancePx, grabsMarker, litSegments, pinchFactor, pinchSpread, placeOn,
  regionFromDrag, spanOf, spectrumExtent, stripCells, surveyCells, surveyViewport, timeAtFraction,
  timeExtent, timelineRequest, unobservedCount, valueAt, wheelPanFrac, zoomWithin, type Band,
  type CoverageCell, type CoverageResponse, type Range,
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
import {
  applyDeviceAction, mayRetune, retuneAction, retuneLabel, setLiveView, setRetuneOffer, NOT_LIVE_TEXT,
} from "./view";

/** A range's edges in MHz, at a resolution taken from the range's own width — so the readout's
 * precision follows what is being shown and no frequency constant appears in this file. */
const fmtEdges = (lo: number, hi: number) => {
  const res = Math.max(1, (hi - lo) / 100);
  return `${ax.fmtMHz(lo, res)}–${ax.fmtMHz(hi, res)}`;
};

/** Pointer travel (px) that makes a press a deliberate drag rather than a click (overlays.ts), and
 * the touch-sized threshold beside it. One definition, in `ui/src/navigators.ts` with the rest of
 * the input arithmetic, re-exported here for the callers that had it from this module. */
export { DRAG_PX, TOUCH_DRAG_PX, dragThresholdPx };

/** The most cells either strip will ask for on either axis — the caps `GET /api/timeline` states
 * for `columns` (time) and `rows` (frequency). Both bars now size their request from their own
 * pixels (`stripCells`), and these only stop a very tall or very wide layout asking for more than
 * the route will answer. */
const MAX_TIME_COLUMNS = 4096;
const MAX_FREQ_ROWS = 512;

/** How long the selected frequency range must hold still before the overview is re-asked for it
 * (T-367). A drag moves the view on every pointer event; this coalesces the storm into one request
 * without the bar lagging behind a settled selection. */
export const BAND_SETTLE_MS = 250;

/** Cells the frequency navigator asks the coverage map to fold the spectrum onto (T-368): what the
 * survey strip draws, one cell to one pixel column, upscaled but never smoothed. */
export const SURVEY_CELLS = 512;

// ---------------------------------------------------------------------------
// Painting a strip cell (T-397): one ramp, three states
// ---------------------------------------------------------------------------
//
// Both bars draw the same three-valued thing and used to do it twice, with a hand-written
// cyan-only ramp that had nothing to do with the waterfall's. They now share this, and the ramp
// itself comes from `ui/src/cmap.ts` — the same stops the WebGL waterfall's shader is generated
// from, so a peak is the same colour on the strip as it is in the picture beside it.
//
// The three states are the backend's, not this file's:
//
//  - **a value** — the position on the served scale (`/api/coverage`'s `shade`, normalised over
//    `shade.range_db`; `/api/timeline`'s cell through `overviewShade`, normalised over the grid's
//    own `range_db`). Nothing here decides what 0 and 1 mean.
//  - **observed, no value** — the radio was here but the history keeps no level: a flat tint,
//    distinct from both the ramp's bottom and grey.
//  - **never observed** — grey, and only ever this. The max of nothing is unknown, not zero and not
//    the bottom of the scale (T-368/T-397), so a fold must never turn this into a dark ramp cell.

/** Writes one cell at byte offset `p`. `shade` is the served 0…1 position, or `null` when there is
 * no level for the cell; `observed` then decides tint-versus-grey. */
function paintCell(data: Uint8ClampedArray, p: number, shade: number | null, observed: boolean) {
  if (shade !== null) {
    const [r, g, b] = cmapBytes(shade);
    data[p] = r; data[p + 1] = g; data[p + 2] = b; data[p + 3] = 255;
  } else if (observed) {
    data[p] = 55; data[p + 1] = 85; data[p + 2] = 95; data[p + 3] = 150;
  } else {
    data[p] = data[p + 1] = data[p + 2] = 110; data[p + 3] = 70;
  }
}

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
  /** Outside it: the one navigator gesture that **commands the radio** (T-392). Carries the whole
   * capture configuration — centre *and* the smallest span that covers the selection. */
  | { kind: "retune"; centerHz: number; spanHz: number; view: ax.View; source: DetailSource }
  /** No achievable configuration can capture it, or there is no front end to command. */
  | { kind: "error"; reason: RetuneRefusal | "not_live"; text: string }
  /** Nothing to do: a region with no width. */
  | { kind: "none" };

/** Why a region cannot be captured, in words. The extents come from the grid on the wire — the
 * device's own bands, never a constant in this file. */
function refusalText(g: FrequencyGrid | null, reason: RetuneRefusal, live: boolean): string {
  const where = live ? "the front end can tune" : "this recording covers";
  if (reason === "center_out_of_range") {
    const ext = spectrumExtent(g);
    return ext
      ? `Outside what ${where}: ${fmtEdges(ext.lo, ext.hi)} MHz.`
      : `Outside what ${where}.`;
  }
  if (reason === "span_too_wide") {
    const max = g?.max_live_span_hz ?? null;
    return max === null
      ? "Wider than one capture window can hold."
      : `Wider than one capture window: at most ${ax.fmtMHz(max, Math.max(1, max / 100))} MHz at once.`;
  }
  return "No navigable spectrum reported — nothing to tune.";
}

/**
 * The region `[lo, hi]` a drag selected, resolved against the achievable grid (T-341).
 *
 * **The boundary is the whole design.** The same gesture means two different things:
 *
 *  - a region **inside the tuned window** is "look closer" — the waterfall already holds that IQ,
 *    so it is a pure view zoom and no device call happens on any path;
 *  - a region **outside it** is "go there", and that is only reachable by tuning there. Unlike time
 *    (always a view over already-captured data), no amount of view state can show a frequency the
 *    front end is not on. So this resolves to a `retune` — the config, computed and applied, not
 *    offered (CLAUDE.md: *"this is the one navigator action that commands the radio"*).
 *
 * It refuses **only** when nothing can capture the region: a centre outside the tunable range, a
 * span wider than one live window, or — on a replay — outside the recording's extent, which is the
 * same `ranges_hz` check because a replay reports the recording as its band. Anything else retunes;
 * "outside the tuned window, retune yourself" is exactly the behaviour this replaced.
 */
export function freqZoomTarget(
  grid: NavigationGrid, g: ax.Geometry | null, region: Range | null, live: boolean,
): FreqZoom {
  if (!region || !(region.hi > region.lo)) return { kind: "none" };
  if (g) {
    const full = ax.fullView(g);
    // INSIDE: a display zoom. Nothing below this line can be reached for such a region.
    if (region.lo >= full.loHz && region.hi <= full.hiHz) {
      const span = snapState(grid, (region.lo + region.hi) / 2, region.hi - region.lo);
      return { kind: "view", view: ax.zoomTo(g, region.lo, region.hi), source: span.source };
    }
  }
  // OUTSIDE: the config that would cover it, or the one reason none can.
  const fg = grid.frequency;
  const plan = retunePlan(fg, region.lo, region.hi);
  if (!plan.ok) return { kind: "error", reason: plan.reason, text: refusalText(fg, plan.reason, live) };
  // A replay's band is the recording, so a region achievable against it is inside the recording —
  // but there is still no radio to move, and saying so beats a retune the server answers 409 to.
  if (!live) return { kind: "error", reason: "not_live", text: NOT_LIVE_TEXT };
  return {
    kind: "retune",
    centerHz: plan.centerHz,
    spanHz: plan.spanHz,
    view: { loHz: region.lo, hiHz: region.hi },
    source: plan.source,
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
 *
 * T-392: the `retune` branch is the **one** place a navigator gesture reaches the front end, and it
 * does so through the same gated typed `DeviceAction` the offer button always used — the path is
 * not new, the trigger is. Pan and wheel still resolve to `view` or to nothing, so T-340's control
 * (a ±1.0 drag of the whole bar reaching no route) is untouched by it.
 */
export async function applyFreqZoom(ctx: AppContext, target: FreqZoom): Promise<void> {
  const { store } = ctx;
  if (target.kind === "view") {
    store.set(setLiveView(target.view));
    store.set(toast(`Zoomed to ${fmtEdges(target.view.loHz, target.view.hiHz)} MHz · ${detailLabel(target.source)}`));
  } else if (target.kind === "error") {
    store.set(toast(target.text));
  } else if (target.kind === "retune") {
    // A region-select supersedes any pending pan-to-the-edge offer: the user has just said where
    // they want the radio, so a stale button proposing somewhere else is noise.
    store.set(setRetuneOffer(null));
    await applyDeviceAction(ctx, retuneAction(target.centerHz, "navigator", target.view, target.spanHz));
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

/** "centre C · span S (sample rate) · live IQ": one already-resolved capture state in words. A live
 * window's span *is* its sample rate (`navigation.ts`), which is why the third field can name one.
 * Nothing is decided here — the caller brings the config, from `snapState` or from `retunePlan`. */
function stateText(centerHz: number | null, spanHz: number | null, source: DetailSource): string {
  const res = resOf(spanHz ?? 1);
  // A null centre is "the source states no tuning step", never "anywhere is reachable" (rule 1 in
  // navigation.ts): nothing here may claim the device can sit exactly where the user pointed.
  const centre = centerHz === null ? "centre not on any stated grid" : `centre ${ax.fmtMHz(centerHz, res)} MHz`;
  const span = spanHz === null ? "span unknown" : `span ${ax.fmtBandwidth(spanHz)}`;
  const rate = source === "live-iq" && spanHz !== null ? " (sample rate)" : "";
  return `${centre} · ${span}${rate} · ${detailLabel(source)}`;
}

/** The state a *hover* would resolve to: a point has no width, so the current window's span is
 * carried through T-341's `snapState` to the achievable grid. */
function configText(grid: NavigationGrid, centreHz: number, spanHz: number): string {
  const s = snapState(grid, centreHz, spanHz);
  return stateText(s.centerHz, s.spanHz, s.source);
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
 *
 * **This readout was rewritten when T-392 landed, and that is the point of taking the branch from
 * the gesture.** Before it, a region outside the tuned window left a *retune offer* the user then
 * pressed, and the line said "offers". T-392 made the same gesture command the radio on release, so
 * the line now says it retunes — and says a refusal in the refusal's **own** `text`, rather than a
 * second wording of the same reason that could drift from it. (The frequency bar's *pan* still
 * leaves an offer, deliberately: a pan names no destination to fire on. That wording lives in
 * `onPanEnd`, not here.)
 */
export function freqSelectText(
  grid: NavigationGrid, g: ax.Geometry | null, region: Range | null, live: boolean,
): string {
  const t = freqZoomTarget(grid, g, region, live);
  if (!region || t.kind === "none") return "";
  const edges = `${fmtEdges(region.lo, region.hi)} MHz`;
  if (t.kind === "view") return `${edges} · zooms the view, the radio stays put · ${detailLabel(t.source)}`;
  // No achievable configuration covers it (or there is no radio to move): the honest line is the
  // one `freqZoomTarget` already wrote, naming the device's own bounds from the served grid.
  if (t.kind === "error") return `${edges} · cannot capture this — ${t.text}`;
  // The config `retunePlan` computed, not a second opinion about it: the centre the radio will sit
  // on after the snap, and the smallest span that still covers the selection from there.
  return `${edges} · retunes the radio on release: ${stateText(t.centerHz, t.spanHz, t.source)}`;
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
 * The cursor a **pan** of `deltaFrac` of the bar moves to: the reviewed **window** slides along the
 * capture window, keeping the span it is reviewing, and reaching the newest edge is *live*.
 *
 * **The clamp is the whole reviewed window, not its end (T-412).** This is the one gesture every
 * input device's pan resolves to — a drag on the bar, a finger, and now the wheel — so "panning at
 * the ends must not resize" has to hold here or it holds nowhere. Clamping only the end instant
 * would leave a 20 s window hanging half off the oldest edge of a capture that has nothing there:
 * the stored `spanS` would still say 20 s while the view showed ten of data and ten of nothing,
 * which is the same lie as a pan that silently resized. `clampInto` **shifts and never resizes**,
 * so the window stops with all of itself inside the retained capture and the span comes out the
 * number it went in as. A span wider than the whole capture window degenerates to the window, which
 * is `live`.
 */
export function timePanTarget(ext: Range, cur: TimeCursorNow, deltaFrac: number): TimeTarget {
  const d = Number.isFinite(deltaFrac) ? deltaFrac * spanOf(ext) : 0;
  const end = cursorAt(ext, cur) + d;
  // A live cursor is reviewing no span yet, so there is no window to keep inside — only the instant.
  const span = cur.live ? 0 : Math.max(0, cur.spanS ?? 0);
  const next = clampInto(ext, { lo: end - span, hi: end }).hi;
  return next >= ext.hi ? { live: true } : { live: false, tS: next, spanS: cur.live ? null : cur.spanS };
}

/**
 * The pan a **wheel notch** on the time bar asks for (T-412) — the primary time gesture.
 *
 * The user's correction: *"the TIME axis wheel should PAN / SCROLL THROUGH TIME — wheel up/down
 * moves the viewed window earlier/later, keeping the duration/span FIXED — NOT zoom the time
 * window. The user scrolls through time constantly and rarely resizes."* So the wheel resolves to
 * the **existing pan**, not to a second one: this converts a notch to a distance and hands it to
 * `timePanTarget`, which is where the fixed span and the edge clamp already live. A touch drag is
 * that same pan, and span-zoom is the secondary gesture (ctrl/cmd + wheel, a pinch, or a dragged
 * region) through `timeWheelTarget`.
 *
 * A notch moves a fraction of **what is on screen**, not of the whole retention: scrolling through
 * a 20 s window inside a 10 minute capture must step in 20-second-sized amounts or every notch is a
 * jump to somewhere unrelated. With no span being reviewed yet the fallback is `timeWheelTarget`'s,
 * for the same reason — a duration constant here would be a step nobody asked for.
 */
export function timeWheelPan(ext: Range, cur: TimeCursorNow, deltaY: number, deltaMode = 0): TimeTarget {
  const span = (cur.live ? 0 : (cur.spanS ?? 0)) || spanOf(ext) / 8;
  const whole = spanOf(ext);
  const frac = whole > 0 ? wheelPanFrac(deltaY, deltaMode) * (span / whole) : 0;
  return timePanTarget(ext, cur, frac);
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
  /** The pointer's position along **this bar's own axis**, in client pixels. The frequency bar is
   * horizontal and the time bar vertical, and only differences are used. */
  axisPx(e: PointerEvent): number;
  onPan(deltaFrac: number): void;
  onPanEnd(): void;
  onRegion(a: number, b: number, done: boolean): void;
  /** Whether the press started on the marker showing the current view (pan) or the bar (region). */
  onMarker(e: PointerEvent): boolean;
  /** Abandon whatever this press was becoming, committing nothing: a second finger arrived, so the
   * gesture is a pinch and **a pinch never selects** (`live-spectrum.ts` settled the same rule for
   * the waterfall). On the frequency bar this is load-bearing — a committed region select retunes
   * the radio on release (T-392), so a pinch that fell through to `onRegion(…, done)` would tune
   * wherever the fingers happened to be. */
  onCancel(): void;
  /** A two-finger zoom, as a factor in `axis.wheelFactor`'s units (> 1 zooms in) about fraction
   * `at` of the bar. The same number a wheel produces, handed to the same zoom. */
  onPinch(factor: number, at: number): void;
}

/**
 * Drag-to-pan on the view marker, drag-a-region anywhere else, two fingers to zoom. Every one of
 * them is a view gesture; none has any path to the control API (T-340), and the one gesture that
 * *does* reach it — a committed region select on the frequency bar — is reached only from the
 * `done` branch below, which a pinch and a tap both miss.
 *
 * **T-407, the touch half.** Three things separate a finger from a mouse here, and each is the
 * difference between a usable bar and an accidental command:
 *
 *  - **Travel is measured along the bar's own axis.** It was `clientX + clientY`, which counts a
 *    swipe *across* the bar as travel *along* it — so a finger stroked down the (horizontal)
 *    frequency bar registered as a region select of nearly zero width, and a page-scroll reflex on
 *    a bar became a gesture. Only the bar's own axis counts now, so a cross-axis stroke stays a tap.
 *  - **The threshold is the input device's** (`dragThresholdPx`): a finger lands over several pixels
 *    and wobbles as it lifts, and at the mouse's 6 px that wobble is a drag.
 *  - **A second pointer ends the drag and starts a pinch**, committing nothing.
 */
function attachBar(el: HTMLElement, d: BarDrag) {
  let mode: "pan" | "region" | "pinch" | null = null, startFrac = 0, lastFrac = 0;
  let downPx = 0, moved = false, threshold = DRAG_PX;
  /** Live pointers, by id: position along the bar's axis, and the fraction there. */
  const pts = new Map<number, number>();
  /** The spread the pinch began at, and the factor already delivered from it. The **absolute**
   * factor is always measured from `pinchFrom` — a ratio of successive frames would accumulate
   * rounding — but what is handed on is the **increment** since the last frame, because the zoom it
   * feeds reads the bar's current state, exactly as a wheel notch does. That is what makes the
   * pinch and the wheel one call rather than two that agree. */
  let pinchFrom = 0, pinchLast = 1, pinchAt = 0.5;

  const beginPinch = () => {
    const xs = [...pts.values()];
    if (xs.length < 2) return;
    if (mode && mode !== "pinch") d.onCancel(); // a pinch never selects, and never pans
    pinchFrom = pinchSpread(xs[0], xs[1]);
    pinchLast = 1;
    pinchAt = (startFrac + lastFrac) / 2;
    mode = "pinch";
  };

  el.addEventListener("pointerdown", (e) => {
    if ((e.target as Element | null)?.closest?.("button, a, input, select")) return;
    if (e.pointerType === "mouse" && e.button !== 0) return;
    pts.set(e.pointerId, d.axisPx(e));
    el.setPointerCapture(e.pointerId);
    el.classList.add("dragging");
    e.preventDefault();
    if (pts.size > 1) { lastFrac = d.frac(e); beginPinch(); return; }
    mode = d.onMarker(e) ? "pan" : "region";
    startFrac = lastFrac = d.frac(e);
    downPx = d.axisPx(e);
    threshold = dragThresholdPx(e.pointerType);
    moved = false;
  });
  el.addEventListener("pointermove", (e) => {
    if (!mode || !pts.has(e.pointerId)) return;
    pts.set(e.pointerId, d.axisPx(e));
    if (mode === "pinch") {
      const xs = [...pts.values()];
      if (xs.length < 2 || !(pinchFrom > 0)) return;
      const abs = pinchFactor(pinchFrom, pinchSpread(xs[0], xs[1]));
      const step = abs / pinchLast;
      pinchLast = abs;
      if (step !== 1) d.onPinch(step, pinchAt);
      return;
    }
    const f = d.frac(e);
    if (Math.abs(d.axisPx(e) - downPx) >= threshold) moved = true;
    if (mode === "pan") { d.onPan(f - lastFrac); lastFrac = f; }
    else if (moved) d.onRegion(startFrac, f, false);
  });
  const end = (e: PointerEvent) => {
    const had = pts.delete(e.pointerId);
    if (pts.size === 0) el.classList.remove("dragging");
    if (!had || !mode) return;
    if (pts.size > 0) {
      // A finger of a pinch lifted. With two or more left the pinch continues, re-anchored on the
      // fingers that remain — measuring on from a spread one of them is no longer part of would
      // jump the zoom. With one left the gesture is over as far as committing goes: the remaining
      // finger must NOT become a region select that retunes where the pinch happened to finish.
      if (mode === "pinch" && pts.size > 1) { beginPinch(); return; }
      if (mode === "pinch") d.onCancel();
      mode = null;
      return;
    }
    if (mode === "pinch") d.onCancel();
    else if (mode === "pan") d.onPanEnd();
    // The only path to a committed selection, and on the frequency bar the only path to the radio:
    // one pointer, travel past its device's threshold along the bar's own axis, released on the bar.
    else if (moved && e.type === "pointerup") d.onRegion(startFrac, d.frac(e), true);
    else d.onRegion(startFrac, startFrac, true); // a tap: clear any draft, select nothing
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
    "aria-label": "Recent history across the device-available spectrum: time down, frequency across; grey is never observed",
  }) as HTMLCanvasElement;
  // T-405: the dim layer, and what replaced the view box. There is no `.fn-view` rectangle any
  // more — the bar shades everywhere the radio is *not* capturing, so the region left at full
  // strength IS the tuned window. One less overlay, and it cannot disagree with the lit windows
  // because it is computed as their complement (`dimSegments`).
  const dimLayer = h("div", { class: "fn-dim-layer" });
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
  const track = h("div", { class: "fn-track" }, strip, dimLayer, draft, readout, offerBtn);
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

  /** The coverage cells the backend last served, and the extent they were served for — so a readout
   * never describes a spectrum range the drawn pixels are not of. */
  let survey: CoverageCell[] = [];
  /** The recent-history grid over the same extent: `nt` time rows × `nf` frequency cells, `nf`
   * matching `survey.length` so column `f` of one is column `f` of the other. `null` until asked,
   * or when no history answered for a viewport this wide (and then the strip falls back to the
   * single coverage row rather than inventing rows). */
  let tl: OverviewResponse | null = null;
  /** The extent the drawn strip was asked for, so a viewport change re-asks rather than restretching
   * cells of one range across another. */
  let surveyFor = "";
  /** Frequency cells the bar asks for: one per pixel of its own width (T-397). */
  const surveyCellCount = () => stripCells(track.clientWidth, SURVEY_CELLS);
  /** Time rows the bar asks for: one per pixel of its own height. */
  const surveyRowCount = () => stripCells(track.clientHeight, MAX_TIME_COLUMNS);

  /**
   * The bottom bar as a **mini-waterfall** (T-397): time down, frequency across the survey range,
   * as many rows as fit the bar at one row per pixel.
   *
   * It drew a single row stretched to the bar's full height — one spectrum scaled up, which is not a
   * spectrogram of anything. It now draws the `nt × nf` grid `GET /api/timeline` folds over the same
   * capture window the *time* bar spans, so the two edge navigators are two projections of one
   * pyramid: the left bar folds that window over the selected frequency range, this one folds it
   * over the whole survey range. Nothing is upsampled here — every row drawn is a row the backend
   * folded, and where its own tier is coarser it replicates on that side and says so in
   * `resolution`.
   *
   * **Grey still comes from the coverage map and nothing else.** `/api/coverage` answers per
   * frequency cell over the window, so it decides a *column*; the history grid decides a *cell*.
   * Where the grid holds a value the value is drawn — measured data is never hidden by a coverage
   * answer about a different window. Where it holds none, the column's coverage state chooses the
   * flat "sampled, no level" tint or grey. Per-(time, frequency) coverage is the second axis
   * `docs/16` names as the one real gap; until it exists this is the honest composition of what is
   * served, not a guess at it.
   */
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
    const nf = survey.length;
    // Rows only when the grid is over these very cells; a mismatched grid is stale (a viewport
    // moved under a poll) and one coverage row beats another range's rows.
    const grid = tl && tl.nf === nf && tl.nt > 0 ? tl : null;
    const nt = grid ? grid.nt : 1;
    strip.width = nf;
    strip.height = nt;
    const img = c.createImageData(nf, nt);
    for (let f = 0; f < nf; f++) {
      const cell = survey[f];
      const observed = cell.state === "observed";
      // The column's own level, for the no-rows fallback: what the bar drew before T-397.
      const flat = observed && cell.shade !== null && cell.shade !== undefined ? cell.shade : null;
      for (let t = 0; t < nt; t++) {
        const i = t * nf + f;
        paintCell(img.data, i * 4, grid ? overviewShade(grid, i) : flat, observed);
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
    // T-405: the inverse of what this layer used to draw. The reported capture windows are left at
    // full strength and everything else is shaded down, so *the normal-coloured region is the
    // current tuned window* and there is no second rectangle claiming to be it. The segments still
    // come from the reported list — with none reported the whole bar dims, which is the honest
    // picture of a server capturing nowhere, not an assumption that there is one window.
    const windows = litSegments(s.navGrid.windows, ext);
    dimLayer.replaceChildren(...dimSegments(s.navGrid.windows, ext).map((seg) => {
      const e = h("div", { class: "fn-dim" });
      pctStyle(e, seg, false);
      return e;
    }));
    // The windows themselves stay hoverable so the bar can still say which front end is where;
    // nothing is painted over them.
    track.title = windows.length === 0
      ? ""
      : windows
        // Label resolution comes from the window's own width, never from a frequency constant.
        .map((w) => `Capture window${w.deviceId ? ` on ${w.deviceId}` : ""}: ${fmtEdges(w.loHz, w.hiHz)} MHz`)
        .join("\n");

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

  /**
   * **The one frequency-bar zoom (T-407).** A wheel notch and a two-finger pinch are two ways of
   * naming the same factor (`ax.wheelFactor` / `pinchFactor`, both > 1 to zoom in), so they meet
   * here rather than each growing their own copy of the viewport arithmetic. It writes no store
   * slice and reaches no device route — a survey frame is not a tune (T-343), and T-340's control
   * still holds on both inputs: no zoom and no pan on either bar can move the radio.
   */
  const zoomFreqBy = (factor: number, at: number) => {
    const b = bounds(), ext = extent();
    if (!b || !ext) return;
    // Never narrower than one capture window: a survey frame inside the live window would claim to
    // resolve the spectrum more finely than the front end can open it (T-341's rule, on this axis).
    const floor = tuned()?.span_hz ?? spanOf(b) / SURVEY_CELLS;
    viewport = zoomWithin(b, ext, Math.min(1, Math.max(0, at)), factor, floor);
    viewportTouched = true;
    render();
    void refreshSurvey();
  };

  attachBar(track, {
    frac: (e) => ax.pointerFrac(e.clientX, track.getBoundingClientRect()),
    // The frequency bar is horizontal, so travel along it is travel in x. A stroke *down* the bar
    // (a page-scroll reflex) is therefore not a region select, which on this bar is the gesture
    // that commands the radio.
    axisPx: (e) => e.clientX,
    onCancel: () => { draft.hidden = true; dragText = null; readout.hidden = true; },
    onPinch: zoomFreqBy,
    // T-405: the view box that was the pan handle is gone, so there is nothing on this bar to grab
    // for a pan and every press is a region select — the gesture the user kept ("drag-to-retune,
    // transient draft only"). T-412 answered the question T-405 left open by moving this bar's
    // travel onto the wheel and the pinch (`zoomFreqBy` pans as it scales about the pointer), so no
    // invisible handle is needed; the pan handlers below stay reachable through `attachBar`'s
    // contract and keep the edge offer they always had.
    onMarker: () => false,
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
      // T-392: the one gesture on either bar that may command the radio. Inside the tuned window it
      // is a view zoom and reaches nothing; outside it, it retunes to cover the selection.
      void applyFreqZoom(ctx, freqZoomTarget(s.navGrid.grid ?? { frequency: null, time: null }, g, region, mayRetune(s.device)));
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
  // it scales, which is how the bar is moved along the spectrum. **T-412 leaves this bar's wheel a
  // zoom** — only the *time* axis's primary wheel gesture changed — and T-407 gives the same zoom
  // its touch spelling by routing a pinch into the identical `zoomFreqBy`.
  track.addEventListener("wheel", (e) => {
    if (!bounds() || !extent() || e.deltaY === 0) return;
    e.preventDefault();
    zoomFreqBy(ax.wheelFactor(e.deltaY, e.deltaMode), ax.pointerFrac(e.clientX, track.getBoundingClientRect()));
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
    const cells = surveyCellCount();
    const path = coverageRequest(ext, cells, viewWindow(store.get()));
    if (!path) return;
    const mine = ++surveySeq;
    // Both halves of the picture over the same extent and the same cell count, asked together: the
    // coverage map decides grey per frequency cell, and the timeline folds the capture window onto
    // the rows (T-397). `rows` is the timeline route's *frequency* axis, which is why it carries
    // the cell count and `columns` carries the time rows this bar draws.
    const band: Band | null = ext ? { loHz: ext.lo, hiHz: ext.hi } : null;
    const [body, grid] = await Promise.all([
      client.get<CoverageResponse>(path).catch(() => null),
      client
        .get<TimelineResponse>(timelineRequest(band, surveyRowCount(), Math.min(cells, MAX_FREQ_ROWS)))
        .catch(() => null),
    ]);
    // An answer for a viewport the bar has already left must not repaint it.
    if (mine !== surveySeq || !body) return;
    survey = surveyCells(body);
    // A grid that does not answer for these cells is no grid: the strip then draws the coverage row
    // it does have, rather than stretching another range's rows across this one.
    tl = grid?.grid ?? null;
    surveyFor = key;
    drawSurvey();
    render();
  }
  let surveySeq = 0;

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

  /**
   * The grid this bar asks for: **one cell per pixel of the bar, on both axes** (T-411).
   *
   * It asked for a fixed 160 × 6 — six frequency cells, chosen when the bar was a ~12 px track.
   * The bar is now 80 px wide, so CSS stretched six cells across it and the "compressed history
   * waterfall" became thirteen-pixel blocks of colour. Nothing was wrong with the data; the buffer
   * was a thirteenth of the picture. Asking for the cells is the fix — the backend folds the
   * pyramid onto whatever grid it is given and replicates where its own tier is coarser, which is
   * the direction that repeats a measured value instead of inventing one between two.
   *
   * `columns` is the timeline route's **time** axis (the long one, running down this bar) and
   * `rows` its **frequency** axis (across its width).
   */
  const gridRequest = () => ({
    columns: stripCells(track.clientHeight, MAX_TIME_COLUMNS),
    rows: stripCells(track.clientWidth, MAX_FREQ_ROWS),
  });

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
    for (let i = 0; i < grid.nt * grid.nf; i++) {
      // A cell nothing was folded into is `null` here — never observed over this bar's own window,
      // so grey, and never the ramp's low end. Everything else goes through the waterfall's ramp
      // against the range the backend measured for the grid (T-397).
      paintCell(img.data, i * 4, overviewShade(grid, i), false);
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

  /**
   * **The one time-axis zoom (T-412 / T-407).** Span-zoom is now the *secondary* time gesture — the
   * user scrolls through time constantly and rarely resizes — so it is reached deliberately:
   * ctrl/cmd + wheel (which is also what a trackpad pinch sends), a two-finger pinch, or a dragged
   * region. All of them are one factor into `timeWheelTarget`, the arithmetic the plain wheel used
   * to run. The gesture's *meaning* moved; the arithmetic did not, and neither did what it can
   * reach — a time gesture writes the time cursor and nothing else.
   */
  const zoomTimeBy = (factor: number) => {
    const ext = extent();
    if (ext) applyTimeTarget(store, timeWheelTarget(ext, cursorNow(), factor));
  };

  attachBar(track, {
    frac: (e) => {
      const r = track.getBoundingClientRect();
      return r.height > 0 ? Math.min(1, Math.max(0, (e.clientY - r.top) / r.height)) : 1;
    },
    // Time runs down this bar, so travel along it is travel in y — a stroke *across* the bar is not
    // a time region select.
    axisPx: (e) => e.clientY,
    onCancel: () => { draft.hidden = true; dragText = null; readout.hidden = true; },
    onPinch: (factor) => zoomTimeBy(factor),
    // T-407, "larger hit targets": the view marker can be a few pixels of a long bar, which a mouse
    // can hit and a finger cannot. The grab is decided from the marker's measured extent plus the
    // tolerance the input device earns (`grabTolerancePx`: none for a mouse, a fingertip for touch),
    // so the handle grows for touch without the drawn marker changing at all.
    onMarker: (e) => {
      if (marker.hidden) return false;
      const r = marker.getBoundingClientRect();
      return grabsMarker(r.top, r.bottom, e.clientY, grabTolerancePx(e.pointerType));
    },
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

  /**
   * **T-412: the time wheel PANS.** *"Wheel up/down moves the viewed window earlier/later, keeping
   * the duration/span FIXED — NOT zoom the time window. The user scrolls through time constantly
   * and rarely resizes."* So the plain wheel is the pan — scroll up goes back toward older, which
   * is the direction this bar's own axis runs (oldest at the top) and the direction the waterfall's
   * time axis runs — and the span-zoom it replaced is now reached deliberately, with ctrl/cmd held.
   *
   * ctrl/cmd + wheel is not an extra gesture invented for this: it is what a **trackpad pinch**
   * already sends, so the secondary zoom and the two-finger zoom are literally the same event on a
   * laptop, and `zoomTimeBy` is the single place either lands. Both are still time-axis-only and
   * still reach nothing but the time cursor.
   */
  track.addEventListener("wheel", (e) => {
    const ext = extent();
    if (!ext || e.deltaY === 0) return;
    e.preventDefault();
    if (e.ctrlKey || e.metaKey) zoomTimeBy(ax.wheelFactor(e.deltaY, e.deltaMode));
    else applyTimeTarget(store, timeWheelPan(ext, cursorNow(), e.deltaY, e.deltaMode));
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
    const { columns, rows } = gridRequest();
    const tl = await client.get<TimelineResponse>(timelineRequest(band, columns, rows)).catch(() => null);
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
