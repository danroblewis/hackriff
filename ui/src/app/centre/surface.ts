// **The Explore centre, after the cutover** (T-445, docs/16 §8.5).
//
// What used to be here: a live WebGL waterfall (`ui/src/waterfall.ts`), a DOM overlay layer for its
// boxes (`overlays.ts`), a second render path that swapped the same pane to `GET /api/history` when
// the time cursor moved (`review-render.ts`), a frequency axis strip (`axis-view.ts`) and two
// bespoke edge scrubbers (`navigators.ts`, 1 528 lines). Five surfaces, four of them drawing
// spectrum, each with its own idea of the mapping from data to pixels.
//
// What is here now: **one viewport onto the one surface**, mounted through the *same*
// `SurfacePreview` host the `/surface` page mounts (`ui/src/surface/preview.ts`), with a live edge
// reported in. Live is not a mode — it is the finest growing edge of the same pyramid (T-439), so
// there is no live-versus-history seam left to keep consistent.
//
// ## Why each retired defect is unreachable rather than merely absent
//
// - **T-420, sliver-of-data.** `historyRows` emitted one texture row per *served* cell into a
//   512-row ring whose whole height was drawn, so a 20 s window lit 4 % of the pane. There is no
//   row ring here: a pane names a box and `Surface` draws the tiles that intersect it, so the
//   drawn extent *is* the asked-for extent. Nothing can serve fewer rows than the pane draws,
//   because nothing serves rows.
// - **T-388, box-jump.** A DOM overlay laid out on the 1 s inventory poll against a per-frame
//   scroll. The boxes are now `./marks.ts` quads computed inside `SurfaceView.frame()` from the
//   very `PaneView` the data pass was handed, through the same `toClip`. There is no poll cadence
//   in the layout path to race.
// - **T-397/T-411, fill and resolution.** The pane resolves `(level_f, level_t)` from its own box
//   and viewport (`levelsFor`), and the level it *states* in the chrome is read back off the
//   `PaneReport` the renderer actually drew with — not a second calculation beside it.
// - **T-397, axis and colormap divergence.** The ramp is `CMAP_GLSL`, once, in the one shader.
//   `review/history.ts` carried a second hand-written LUT that stopped at cyan; it is gone, and
//   `overlay.ts`'s program has no sampler and no ramp, so no overlay can express a data colour.
// - **T-412, wheel-zoom mismatch.** One wheel handler (`ui/src/surface/input.ts`) for both mounts
//   of this surface. The bespoke per-widget handlers that disagreed went with their widgets.
//
// ## What this file is allowed to decide
//
// Nothing about signals. It converts pointer events into viewport arithmetic, mirrors the viewport
// into `state.live.view`/`state.time` so the inventory lists stay scoped to what is on screen
// (CLAUDE.md's whole-UI window rule), and renders the retune control T-444 computes — persistent
// and per-pane since T-476. The only device route it can reach is through `acceptPaneRetune` →
// `applyDeviceAction`, T-343's one gate, on an explicit button press.
import { activeWindows, timeExtent, type ActiveWindow } from "../../navigators";
import type { NavigationGrid } from "../../navigation";
import { newClientId, setTileClientId } from "../../surface/clientid";
import { markSurface } from "../../surface/mounted";
import { attachSurfaceInput, type GlPoint } from "../../surface/input";
import {
  GENERALIZE_BELOW_CSS_PX, markAt, markQuads, measurementMarkBoxes, normalizeRegion, pendingMarkBox, pointOn,
  selectionMarkBoxes, signalMarkBoxes,
  type MarkBox, type MarkMeasurement, type MarkRegion, type MarkRow, type MarkSelection,
} from "../../surface/marks";
import { fmtMeasureReadout, measureReadout } from "../../surface/measure";
import { annotationAt, annotationLabels, annotationQuads, type MarkAnnotation } from "../../surface/annotations";
import { PinLayer, detectionPins, isUnexplained, layoutPanePins, pinTipLines, type PlacedPin } from "../../surface/pins";
import type { Box } from "../../surface/lattice";
import { TIME_LABEL_BOX_CSS, type HudReserve } from "../../surface/hud";
import type { RowAction, WidthAction } from "../../surface/chrome";
import { loadRangeMode, saveRangeMode, scaleMode, scaleRows } from "../../surface/contrast";
import { fogKeyEntries, markKeyEntries, rangeLabel } from "../../surface/legend";
import { SurfacePreview, clampToRect, isBackpressure, probeSurface, refreshOrientationNote } from "../../surface/preview";
import { loadShadowGain, shadowGainWheelHandler } from "../../surface/shadow-gain";
import { wsRowOpener } from "../../surface/rowfeed";
import {
  acceptPaneRetune, acceptPaneWidth, coveringWindow, goToSpanHz, offerAcceptable, offerLabel, paneRetuneOffer, paneWidthOffer,
  widthOfferAcceptable, widthOfferLabel, acceptGoLive, goLiveOfferLabel, isGoLiveOffer, GO_LIVE_LABEL,
  type PaneRetuneOffer, type PaneWidthOffer,
} from "../../surface/retune";
import {
  commitRetuneMode, isRetuneKey, retuneModeLabel, retuneModeTarget, RetuneModeController, type RetuneModeTarget,
} from "../../surface/retune-mode";
import {
  ANY_DEVICE, devicePill, deviceRows, splitPerDeviceOffer, type AttachedDevice,
} from "../../surface/panedevice";
import type { PaneRect, PaneReport, PaneView, RangeMode } from "../../surface/surface";
import {
  GLOW_PX, HOLD_INK, HOLD_PX, SHADOW_PX, SLICE_PX, TRACE_COLUMNS, liveFrameFits, maxHoldColumns,
  afterglowAbsence, peakOf, persistenceShortTiles, persistenceSlices, sampleFrame, sliceColumns, sliceWindow, tracePaths, type TracePath,
} from "../../surface/trace";
import { ringMaxHoldColumns, ringRowAt, sampleRingRow } from "../../surface/livering";
import type { OverlayQuad } from "../../surface/minimap";
import { boxOf, type PaneState } from "../../surface/panes";
import { parsePaths, pathQuads, pathsRequest, type MarkPath } from "../../surface/paths";
// T-898: the device's OWN route through frequency (`GET /api/tune-history`), drawn like a
// directions line — one per front end, in the same render pass as the tiles.
import {
  parseTuneHistory, tuneHistoryRequest, tuneKeyEntries, tuneQuads, type TunePath,
} from "../../surface/tunepath";
// T-981: front-end events (`GET /api/frontend/events`) — clipped whole-span rows marked as the
// radio's own energy, in the same render pass as the tiles.
import {
  frontEndKeyEntries, frontEndQuads, frontEndRequest, parseFrontEndEvents, type FrontEndEvent,
} from "../../surface/frontend";
import { flags } from "../../flags";
import { liveRing, liveRow } from "./live-edge";
import { recordIqButton, startCaptureClock } from "./capture-clock";
import { durationText, iqBackingAt, iqNote, ringRuleQuads, ringRules } from "./capture-window";
import { captureBanner } from "./capture-state";
import type { AppContext, AreaMounts } from "../context";
import { h } from "../dom";
import { openSelectionMenu, openSignalMenu } from "../menu";
import { boxActivity, type BoxActivity } from "../dock/activity";
import { startPoll } from "../net";
import { commitRegion } from "../explore/region";
import { commitMeasurement, type MeasureView } from "../explore/measure";
import { boxRequest, commitAnnotation, fetchAnnotations, normLabel, pointRequest, type AnnotationRequest } from "../explore/annotate";
import { focusSelection, focusSignal, paneRows, setInventoryPanes } from "../explore/slice";
import type { PaneWindowSpec } from "../explore/pane-window";
import { gotoTimeWindow, gotoWindow, requestGoto, reviewAt, setNavigation, toast, type AppState } from "../state";
import { mountMapControls, paneActions, type LayerMenu, type MapControlHost } from "../chrome/map-controls";
import { activePaneName, isTypingTarget, outlineBox, paneKeyIntent, stepPane } from "./active-pane";
import { PaneLiveLayer, paneLiveActions, type ChromeRect } from "./pane-live";
import { mountSplitChrome } from "./split-chrome";
import { trackOverlay } from "../chrome/dismiss";
import { PEEK_PX } from "../chrome/sheet";
import {
  BASE_STYLES, COLLECTION_Z, PLANE_ORDER, composeOverlays, defaultPaneLayers, isLayerVisible, layerDef, loadPaneLayers, paintOrder, savePaneLayers, withLayer,
  type LayerId, type OverlayLayerFn, type PaneLayers,
} from "../../surface/layers";
import { artifactLinkQuads, artifactLinks } from "../../surface/artifacts";
import { densityQuads } from "../../surface/density";
import { DENSITY_POLL_MS, DensityPoll } from "./density-poll";
import { dropPaneLayers, inheritPane, paneLayersOf, setPaneBase, setPaneLayer } from "../map/layers-slice";
import { PriorLabelLayer, parsePriors, priorLabels, priorQuads, priorsPath, type PriorsAnswer } from "../../surface/priors";
import { scanPlanQuads } from "../../surface/scanplan";
import { ScanController } from "../map/scan-overlay";
import {
  addResearchAnnotation, collectionLayer, collectionVisibleOn, parseColor, researchMarkBoxes, researchRows, rowKey, selectResearch, setResearchOpen,
  type Collection, type ResearchRow, type ResearchSlice,
} from "../map/research-slice";

const S_TO_NS = 1e9;
/** The map strip along the bottom of the canvas, device px. **0: the minimap is retired** (T-995,
 * user 2026-09-25: "there is never a 'whole world' minimap" — the whole 1 MHz–6 GHz range is reached
 * by zooming a pane out, Google-Maps style). Its per-SDR active-capture segments are drawn in the
 * panes instead (`SurfaceView.frame`), and survey/sweep coverage is the panes' coverage fog. */
const MINIMAP_PX = 0;
/** A pane frozen within this of the edge still counts as showing the growing edge, for the retune
 * control's `"past"` block (T-444/T-476). One frame at 60 Hz, generously. */
const EDGE_GRACE_NS = 0.25 * S_TO_NS;
/** Height of the spectrum-trace band over each pane's top rows, device px (T-457, T-1041: a
 * layer over the waterfall, not a strip carved off it — no space is reserved either way). */
const TRACE_PX = 96;
/** The capture-width presets offered directly (T-496) — round numbers a HackRF-class front end
 * commonly captures at. A host decision, not an RF fact: `retune.ts` re-derives none of its own
 * (`ui/test/surface-retune.test.ts` asserts that), so which spans to offer as buttons lives here,
 * beside "Retune"'s own label text. An unachievable one is still offered, stated and disabled. */
const WIDTH_PRESETS_HZ: readonly number[] = [500e3, 2e6, 5e6, 10e6, 20e6];
/** T-947: the Go-to offer's span when nothing is currently tuned — the device's default working
 * span, never the pane's view width. `gotoOffer` below prefers the front end's OWN current window
 * (`grid.current.span_hz`, T-947's "or keeps the current tuned span") when one exists; this is only
 * the fallback for a pane that has never been tuned. Picked from `WIDTH_PRESETS_HZ` rather than a
 * new RF constant, for the same reason `retune.ts` names none of its own. */
const GOTO_DEFAULT_SPAN_HZ = WIDTH_PRESETS_HZ[1];
/** T-522: the found-signal overlay's shown/hidden preference, kept in `localStorage` the same way
 * `shell.ts`'s `PREFS_KEY` is — a per-viewer convenience, wrapped in try/catch so the page works
 * with storage unavailable, and never anything the backend needs to know about. */
const SHOW_SIGNALS_KEY = "hk-mui-show-signals";

/** Read the persisted preference. Defaults to shown — absent, empty or thrown all mean shown. */
export function readShowSignals(): boolean {
  try {
    return localStorage.getItem(SHOW_SIGNALS_KEY) !== "false";
  } catch {
    return true;
  }
}

export function writeShowSignals(shown: boolean): void {
  try { localStorage.setItem(SHOW_SIGNALS_KEY, shown ? "true" : "false"); } catch { /* storage unavailable */ }
}

/**
 * The rectangles one pane draws, T-522's gate included. Kept as its own pure function — rather than
 * inline in the frame callback — so a test can assert on exactly what the render path is asked to
 * draw for a given `showSignals`, not on a boolean read off to the side. The toggle changes only
 * this composition: `rows`, `sels` and `pendingRegion` are unchanged, so nothing about what is
 * fetched, polled or detected moves when it flips.
 */
export function paneMarkBoxes(
  rows: readonly MarkRow[], focusId: string | null,
  sels: readonly MarkSelection[], selId: string | null, paneBox: Box,
  pendingRegion: MarkRegion | null, showSignals: boolean,
  measurements: readonly MarkMeasurement[] = [], measureId: string | null = null,
  pendingMeasure: MarkRegion | null = null,
  /** T-1004: this pane does not own the selection, so a focused mark is drawn here as the
   * selection's linked ghost rather than as the selection itself. */
  linkedFocus = false,
): MarkBox[] {
  return [
    ...(showSignals ? signalMarkBoxes(rows, focusId, undefined, undefined, linkedFocus) : []),
    ...selectionMarkBoxes(sels, selId, paneBox, linkedFocus),
    ...measurementMarkBoxes(measurements, measureId),
    ...pendingMarkBox(pendingRegion ?? pendingMeasure),
  ];
}

/** The HUD ticks' ink while the chrome is faded (docs/23 §10.2's ~35 %, a touch brighter so the
 * ruler stays readable against the ramp). The labels fade by CSS on the same `chrome-idle` class. */
const HUD_IDLE_ALPHA = 0.45;
/**
 * T-997: the floating chrome's TOP-LEFT column, measured against the canvas, so the time ruler's
 * labels are dropped rather than printed underneath it (`surface/hud.ts`'s `HudReserve`). The
 * cluster's children are absolutely placed by `map-controls.css`, and the ones that matter are the
 * ones that reach into the ruler's own band down the left edge — Go-to, the nudge row, the
 * inventory pills, the retune offer, and at phone width the status pill. Whichever they are, this
 * asks the layout rather than repeating the CSS's numbers: one rect read per child, once per frame,
 * BEFORE any DOM write of that frame (`view.ts` calls it above `HudAxes.update`), so it costs at
 * most one layout and never a read-write thrash.
 */
const RULER_BAND_CSS = TIME_LABEL_BOX_CSS.left + TIME_LABEL_BOX_CSS.width; // where a time label prints.
function chromeReserve(canvas: HTMLCanvasElement, ctl: HTMLElement | null): HudReserve | null {
  if (!ctl) return null;
  const base = canvas.getBoundingClientRect();
  let left = Infinity, right = -Infinity, bottom = -Infinity;
  for (const child of Array.from(ctl.children)) {
    const r = child.getBoundingClientRect();
    if (r.width <= 0 || r.height <= 0) continue;
    const x0 = r.left - base.left, x1 = r.right - base.left, y1 = r.bottom - base.top;
    // Only what reaches into the band the time labels print in, and only above the fold: the zoom
    // stack, the FAB and the top-right cluster are nowhere near the left ruler.
    if (x0 > RULER_BAND_CSS) continue;
    left = Math.min(left, x0); right = Math.max(right, x1); bottom = Math.max(bottom, y1);
  }
  return bottom > -Infinity ? { left, right, bottom } : null;
}

/** The surface's tool modes (docs/23 §10.4's table columns). */
type ToolMode = "navigate" | "measure" | "annotate" | "pin";
/** The first pane's id (`PaneModel`'s default `pane` prefix + 1): which registry the toolbar
 * describes before the surface has booted. Used for nothing once `preview.activePane` exists. */
const FIRST_PANE = "pane1";
/** What the active pane says while its coverage fog is hidden (T-807). */
const FOG_HIDDEN_TEXT = "Coverage fog hidden on this pane: never-observed and unknown spectrum are drawn as bare ground, "
  + "not grey — nothing about what was sampled changed. Layers menu to show it.";
/** The phosphor base style's one ink (T-806): a P31-style green, off the amplitude ramp. */
const PHOSPHOR_INK: readonly [number, number, number] = [0.35, 1, 0.45];

function mount(el: HTMLElement, ctx: AppContext) {
  const { store, client } = ctx;

  const canvas = h("canvas", { class: "sf-canvas", "aria-label": "The spectrum surface: frequency across, time down, with the whole-surface map below" }) as HTMLCanvasElement;
  // T-508: capture state, stated OVER the picture — a frozen edge that looks live is the defect.
  const captureEl = h("div", { class: "sf-capture", role: "alert", hidden: true });
  // T-805 (MAP-05): the HUD axes' labels — band 2, screen-space DOM over the canvas, but placed
  // every render frame by `SurfaceView` from the same ruler its ticks were stroked from. Never read
  // by the pointer: a label must not steal a pan from the surface underneath it.
  const hudEl = h("div", { class: "sf-hud", "aria-hidden": "true" });
  // T-820 (MAP-20): the annotations' labels, placed per frame like the HUD's (band 2 DOM text over
  // the canvas, never read by the pointer). The tool-mode banner is the cluster's `.map-mode`.
  const annoEl = h("div", { class: "sf-annos", "aria-hidden": "true" });
  // T-812 (MAP-12): the band-plan priors' labels — band 2 like the HUD's, placed per frame from the
  // pane's own box, and pointer-transparent. The ranked reasoning itself is in `priorsEl` below.
  const priorsLabelEl = h("div", { class: "sf-priors-labels", "aria-hidden": "true" });
  // T-809 (MAP-09): the pins — band 1, DOM so each is keyboard-focusable with an accessible name,
  // laid out every render frame by the `dom` hook below from the same pane views the tiles were
  // drawn with. `pointer-events: none` throughout: the POINTER reaches a pin through the canvas's
  // own handler and `PinLayer.pick` (the quadtree), so a drag or wheel that starts on a pin still
  // pans and zooms — one gesture everywhere (docs/23 §3). The MapTip is placed in the same pass.
  const pinsEl = h("div", { class: "sf-pins", role: "group", "aria-label": "Signal markers: Tab to a marker for its summary, Enter to select it, arrow keys for its neighbours" });
  const tipEl = h("div", { class: "sf-maptip", role: "tooltip", id: "sf-maptip", hidden: true });
  // T-1000 (docs/23 §10.7): the ACTIVE pane's outline — the pane Go-to, zoom, the layers menu, the
  // follow-live FAB and the viewport menu act on. Placed every render frame from the pane rectangles
  // the frame was drawn with (and at once when the active pane changes), shown only while there are
  // two or more panes. Never takes the pointer: a press goes through it to the pane underneath.
  const activeEl = h("div", { class: "sf-active-pane", "aria-hidden": "true", hidden: true });
  // T-1005: the split's own chrome — a drag handle on every divider and a × on every pane — placed
  // per frame from the same layout the panes were drawn with (`split-chrome.ts`). Empty with one pane.
  const split = mountSplitChrome({ canvas, preview: () => preview, closePane: (id) => closeActive(id) });
  const splitEl = split.el;
  // T-1001 (MMAP split view): **each pane's own Live/Freeze button**, inside its own rectangle —
  // the retired follow-live FAB acted on the hidden active pane, so with two panes open it could
  // not say which one it froze. One button per pane, placed every render frame from that frame's
  // pane rectangles (`centre/pane-live.ts`). The container never takes the pointer; the buttons do.
  const liveEl = h("div", { class: "sf-pane-live", role: "group", "aria-label": "Per-viewport Live / Freeze" });
  const chrome = h("div", { class: "sf-chrome", "aria-label": "Per-viewport level readout" });
  const hoverEl = h("div", { class: "sf-hover", role: "status" });
  const note = h("div", { class: "sf-note", role: "status" });
  // T-1041: hidden until the trace layer is switched on (it is off by default).
  const traceEl = h("div", { class: "sf-trace", role: "status", hidden: true });
  // T-506: the IQ horizon and the retention bound, said in words beside the two rules that draw
  // them. The data-* attributes are the same numbers the rules were drawn from on the same frame,
  // so ui/e2e can check the pixels against them rather than against a second calculation.
  const ringEl = h("div", { class: "sf-ring", role: "status" });
  // T-807 (MAP-07): says so, in words, when the active pane's coverage fog is hidden — the bare
  // ground it then draws is a viewer's choice, and a choice about grey must never pass for a fact.
  const fogEl = h("div", { class: "sf-ring sf-fog", role: "status", hidden: true });
  // T-812 (MAP-12): the active pane's band-plan priors, ranked, each with the backend's own reason —
  // shown only while that pane's priors layer is on. Suggestions, never truth.
  const priorsEl = h("div", { class: "sf-priors", role: "status", hidden: true });
  // T-470/T-528: the display range, stated. Its CONTROL is the layers menu's "Colour scale" axis
  // (T-882); the sentence stays on the picture, because a fixed scale is honest only if it is quoted.
  const rangeEl = h("span", { class: "sf-range", role: "status" });
  // T-882: the retired toolbar row's two readouts, floated over the canvas's bottom-left above the
  // map strip (screen-space chrome, docs/23 §10.1 band 2). Status only: never takes the pointer.
  const readout = h("div", { class: "sf-readout", "data-band": "chrome" }, rangeEl, hoverEl);
  const stage = h("div", { class: "sf-stage" }, canvas, activeEl, splitEl, liveEl, pinsEl, hudEl, priorsLabelEl, annoEl, tipEl, captureEl);
  // T-522: the found-signal overlay (Candidate/Confirmed boxes) shown/hidden, remembered per viewer.
  // Pure client presentation — it changes only `paneMarkBoxes`'s composition below, never a fetch,
  // a poll or what is detected, and it touches neither `state.inventory` nor the lists that read it.
  // T-806 (MAP-06): per-pane layer registries (docs/24 §13). A pane with no entry in
  // `state.layers` yet starts from its SEED: what this viewer stored for that pane id, else the
  // defaults — with T-522's older found-signal preference applied, so a viewer who hid the boxes
  // does not get them back by an upgrade. A seed is computed once per pane id and never follows
  // another pane's edits (a split inherits explicitly, by value, below).
  const seeds = new Map<string, PaneLayers>();
  const seedFor = (paneId: string): PaneLayers => {
    let s = seeds.get(paneId);
    if (!s) {
      const d = defaultPaneLayers(paneId);
      s = loadPaneLayers(paneId) ?? (readShowSignals() ? d : withLayer(d, "detections", false));
      seeds.set(paneId, s);
    }
    return s;
  };
  const layersFor = (paneId: string): PaneLayers => paneLayersOf(store.get(), paneId, seedFor(paneId));
  /** Every registry write goes through here: the pane's entry in the store, and in this viewer's
   * storage under that pane's id. Presentation state only — no route, no poll. */
  const editLayers = (patch: (s: AppState) => Partial<AppState>, paneId: string) => {
    store.set(patch);
    savePaneLayers(layersFor(paneId));
    syncLayerControls();
  };
  // T-882: there is no toolbar row. Live is the follow-live FAB, Trace/Signals/Contrast are the
  // layers menu, Measure and pane management are the floating cluster's top-right (`map-controls`).
  // T-918: the canvas is full-bleed (docs/23 §10.1) — no row below the stage subtracts from it.
  // The statements that used to be those rows (trace, IQ ring, fog, priors, per-viewport level,
  // orientation) float bottom-left with the readout, above the map strip: screen-space chrome over
  // the canvas, never faded (docs/23 §10.2: honesty statements).
  //
  // T-919 (user P1, docs/23 §10.6 rule 1): T-918's stack could grow to ~560 × 184 px — a large
  // PERMANENT overlay over the waterfall, which is exactly what P1 forbids ("an overlay exists to
  // be closed"; a panel's default state is its smallest). It is a compact STATUS LINE now:
  //
  //  - **Collapsed (the default)** — one line: the per-viewport row (`.sf-chrome`, trimmed by CSS
  //    to where · level/tier and T-476's persistent Retune) beside the colour-scale sentence and
  //    the hover readout. The tier/level and the scale are honesty statements, so they are on the
  //    picture in every state: §10.2 says a statement about what the data *is* may not be made less
  //    legible, and that applies to hiding it behind a press as much as to fading it.
  //  - **Expanded** — adds the sentences that are a paragraph each: the spectrum trace, the IQ-ring
  //    rules in words (retention bound, oldest IQ, whether this viewport has IQ), the fog note, the
  //    ranked band-plan priors and T-450's orientation note.
  //
  // The dismiss (×, and Escape through the one overlay stack, T-900) returns it to the collapsed
  // line, never to nothing — a viewer can put a paragraph away, not switch an honesty statement off.
  const statusBody = h("div", { class: "sf-status-body", id: "sf-status-body", hidden: true },
    traceEl, ringEl, fogEl, priorsEl, note);
  const statusToggle = h("button", {
    class: "sf-status-toggle", type: "button", "aria-controls": "sf-status-body", "aria-expanded": "false",
    title: "The full status: spectrum trace, capture rules, coverage, priors and orientation",
  }, "More") as HTMLButtonElement;
  const statusClose = h("button", {
    class: "sf-status-close", type: "button", "aria-label": "Close the status detail", title: "Close", hidden: true,
  }, "×") as HTMLButtonElement;
  const statusLine = h("div", { class: "sf-status-line" },
    chrome, readout, h("div", { class: "sf-status-btns" }, statusToggle, statusClose));
  const statusEl = h("div", { class: "sf-status", "data-band": "chrome", "data-open": "false" }, statusBody, statusLine);
  const statusOverlay = trackOverlay("surface-status", () => setStatusOpen(false));
  function setStatusOpen(on: boolean): void {
    statusEl.dataset.open = on ? "true" : "false";
    statusBody.hidden = !on;
    statusClose.hidden = !on;
    statusToggle.setAttribute("aria-expanded", on ? "true" : "false");
    statusToggle.textContent = on ? "Less" : "More";
    statusOverlay.open(on);
  }
  statusToggle.addEventListener("click", () => setStatusOpen(statusEl.dataset.open !== "true"));
  statusClose.addEventListener("click", () => setStatusOpen(false));
  stage.append(statusEl);
  el.replaceChildren(stage);

  let preview: SurfacePreview | null = null;
  /** T-1001: the panes' own Live/Freeze buttons, built once the surface (and so the panes) exist.
   * The render frame's `dom` hook places them; until then there is nothing on screen to place. */
  let liveButtons: PaneLiveLayer | null = null;
  /** The floating cluster's boxes in CSS px from the canvas's top-left, so a pane's Live button can
   * be placed clear of whatever chrome is over its corner (`chromeClearance`). Measured when the
   * layout changes, never per frame. */
  let chromeBoxes: ChromeRect[] = [];
  /** Hooks into the floating cluster (T-802), no-ops until it is mounted after the surface boots. */
  let renderFollow: () => void = () => {};
  let viewMoved: () => void = () => {};
  let renderLive: () => void = () => {};
  let renderLayers: () => void = () => {};
  let renderMeasure: () => void = () => {};

  // Record IQ over this viewport's span (GAP-1 interim). Its home is the viewport menu (T-882); a
  // found signal's own clip is the detail sheet's "Record clip" (T-804).
  // T-1004: it is per-pane chrome (it lives in the viewport menu, named for the active pane since
  // T-1000), so it reports which pane it acts on and whether that pane is frozen — an IQ recording
  // runs forward from now, and a frozen viewport must not be left implying otherwise.
  const recordBtn = recordIqButton(ctx, () => {
    const p = preview;
    if (!p) return null;
    const ids = p.view.panes.list().map((x) => x.id);
    return { frozen: !p.view.panes.isFollowing(p.activePane), pane: activePaneName(ids, p.activePane)?.label ?? null };
  });
  recordBtn.classList.add("map-pane-item");
  recordBtn.dataset.paneAct = "record";
  /** Split: the new pane inherits the creating pane's registry by value (docs/24 §13.5). */
  const splitActive = (dir: "columns" | "rows" = "columns") => {
    const p = preview;
    if (!p) return;
    const from = p.activePane;
    p.split(dir);
    if (p.activePane !== from) {
      store.set(inheritPane(from, p.activePane, seedFor(from)));
      savePaneLayers(layersFor(p.activePane));
    }
    syncLayerControls();
  };
  /** T-1005: close pane `id` (the active one by default). The pane made active afterwards is the
   * one under the pointer, else the live one — `SurfacePreview.closePane`'s rule, fed the last
   * pointer position this page saw over the canvas. */
  const closeActive = (id?: string) => {
    const p = preview;
    if (!p) return;
    const gone = id ?? p.activePane;
    if (p.closePane(gone, split.pointerGl())) store.set(dropPaneLayers(gone));
    syncLayerControls();
  };
  /** An open layers menu describes the ACTIVE pane's registry (and the view-wide trace and colour
   * scale), so a pane switch or a toggle re-states it. Set-if-changed: the menu is not rebuilt per
   * frame. (T-882 retired the toolbar's `Signals` button, which used to mirror the same layer.) */
  let syncedFor = "";
  function syncLayerControls(): void {
    const p = preview;
    const reg = layersFor(p ? p.activePane : FIRST_PANE);
    const scale = p ? `${p.range.mode}|${rangeLabel(p.range)}` : "";
    const key = `${reg.paneId}|${reg.base}|${reg.layers.map((l) => +l.visible).join("")}|${+traceOn}|${scale}`;
    if (key !== syncedFor) { syncedFor = key; renderLayers(); }
  }
  let windows: ActiveWindow[] = [];
  let detach: (() => void) | null = null;
  /** The region stroke in progress (T-458), in surface coordinates, or `null`. Read inside the
   * frame callback like the marks are, never mirrored into the store: it is pointer state for the
   * duration of one gesture, and the store is where things that outlive a gesture live. */
  let pending: { pane: string; region: MarkRegion } | null = null;
  // ---- measurement mode (T-822 / MAP-22) ----
  // Whether a plain drag marks out a measurement instead of panning. Read live by
  // `attachSurfaceInput` through the getter below, exactly the pointer-state discipline `pending`
  // above already follows: not store state, because it is meaningless once the mode is off.
  /** The surface's tool mode (docs/23 §10.4): `navigate` is the default and binds nothing extra. */
  let tool = "navigate" as ToolMode;
  let pendingMeasure: { pane: string; region: MarkRegion } | null = null;
  /** Saved measurements, this session. A durable object once `POST /api/measurements` answers
   * (docs/25 §4/§10); kept here rather than in the store because no other surface reads it yet —
   * T-821's collections panel is where a shared, fetched, cross-window list belongs. */
  let measurements: MarkMeasurement[] = [];
  // ---- annotations (T-820 / MAP-20) ----
  /** The annotations in the viewed window, as `GET /api/annotations` last answered, plus any saved
   * since. Durable: they are read back from the store on every load, so a reload shows them. */
  let annotations: MarkAnnotation[] = [];
  /** The annotation box being stroked, or `null`. Pointer state, like `pending`. */
  let pendingAnnotate: { pane: string; region: MarkRegion } | null = null;

  const say = (text: string) => { note.textContent = text; note.hidden = !text; };
  store.select((s) => s.device, (d) => {
    const b = captureBanner(d);
    captureEl.hidden = !b;
    if (b) {
      if (captureEl.textContent !== b.text) captureEl.textContent = b.text;
      captureEl.dataset.state = b.state;
      stage.dataset.capture = b.state;
    } else {
      delete captureEl.dataset.state;
      delete stage.dataset.capture;
    }
  }, { immediate: true });
  /** Set-if-changed, so a per-frame readout does not rewrite the DOM sixty times a second. */
  const setText = (e: HTMLElement, text: string) => { if (e.textContent !== text) e.textContent = text; };

  // ---- the live edge, on the capture clock (T-379). Never `Date.now()`. ----
  // `live.edgeTS` is the newest spectrum row's own time (`live-edge.ts`); the capture window's end
  // stands in until one arrives; `null` is *unknown*, and then the surface keeps the edge the probe
  // resolved rather than inventing one.
  const edgeNs = (): number => {
    const s = store.get();
    const tS = s.live.edgeTS ?? s.captureWindow?.t1S ?? null;
    return tS === null ? 0 : tS * S_TO_NS;
  };

  // ---- the capture clock (T-379), re-homed from the retired Capture panel (T-506) ----
  // The only writer of `state.captureWindow`. Started with the mount, not after the surface boots:
  // the inventory lists and the History default period read it too, and a surface that failed to
  // address must not take their live edge down with it.
  startCaptureClock(ctx);

  // ---- the IQ horizon and the retention bound (T-506) ----
  //
  // Two rules across every pane, from the ring window `GET /api/timeline` reports, laid out through
  // the pane's own mapping on the same frame as the rows (`ringRuleQuads` → `toClip`). The edge is
  // the one the panes are drawn to, so the retention bound advances with the rows, not on the poll.
  // The readout says the same thing in words for the active pane, including which side of the IQ
  // horizon that pane's own time position is on — the playback invariant's "no audio past the
  // ring", stated where the user is looking rather than in a panel below it.
  const ringQuads = (pane: PaneView, edge: number): OverlayQuad[] => {
    const s = store.get();
    const rules = ringRules(s.captureWindow, edge > 0 ? edge / S_TO_NS : null);
    const p = preview;
    if (p && pane.id === p.activePane) {
      const following = p.view.panes.isFollowing(pane.id);
      const posS = pane.box.t1Ns / S_TO_NS;
      // T-464: the wider audio horizon (ring + recordings), from `state.iqAvailability`; `null` (not
      // polled yet) falls back inside `iqBackingAt` to the ring-only `rules` this pane already reads.
      const backing = iqBackingAt(posS, following, rules, s.iqAvailability);
      const w = s.captureWindow;
      const held = w?.buffered ? Math.max(0, w.buffered.t1S - (rules?.iqS ?? w.buffered.t0S)) : null;
      setText(ringEl, !rules
        ? "IQ ring: this server has not reported a capture window, so where raw IQ ends is unknown"
        : `IQ ring: ${held === null ? "holds nothing yet" : `holds ${durationText(held)}`} of a ${durationText(rules.spanS)} retention`
          + ` — green line: oldest IQ${rules.iqS === null ? " (none yet)" : ` ${clock(rules.iqS)}`}`
          + `; magenta dashes: retention bound ${clock(rules.retentionS)}`
          + ` · this viewport: ${iqNote(backing)}`);
      const scale = canvas.height > 0 ? canvas.clientHeight / canvas.height : 1;
      const d = ringEl.dataset;
      d.retentionS = rules ? String(rules.retentionS) : "";
      d.iqS = rules?.iqS != null ? String(rules.iqS) : "";
      // What those two rules were derived FROM, this frame: the edge the panes are drawn to and the
      // ring window as last polled. The server's ring moves on between that poll and any later
      // question, so a claim about the rules is only checkable against the snapshot they came from.
      d.edgeS = rules ? String(rules.retentionS + rules.spanS) : "";
      d.ringT0S = w?.buffered ? String(w.buffered.t0S) : "";
      d.ringT1S = w?.buffered ? String(w.buffered.t1S) : "";
      // T-845: the oldest sample the ring's scheduled drops leave, where one is applied this frame.
      d.dropT0S = rules?.dropT0S != null ? String(rules.dropT0S) : "";
      d.backing = backing;
      d.paneT0S = String(pane.box.t0Ns / S_TO_NS);
      d.paneT1S = String(pane.box.t1Ns / S_TO_NS);
      // The pane's rectangle in CSS px from the canvas's top-left (PaneRect is GL, bottom-left).
      d.paneTopPx = String((canvas.height - pane.rect.y - pane.rect.h) * scale);
      d.paneHPx = String(pane.rect.h * scale);
      d.paneLeftPx = String(pane.rect.x * scale);
      d.paneWPx = String(pane.rect.w * scale);
    }
    return ringRuleQuads(rules, pane.box, pane.rect);
  };

  // ---- the marks: per frame, from the state they describe ----
  // Deliberately reading the store *inside* the frame callback rather than subscribing: a
  // subscription would re-derive on the poll's cadence, which is T-388 exactly.
  const boxesFor = (pane: PaneView): MarkBox[] => {
    const s = store.get();
    const focusId = s.focus.kind === "signal" ? s.focus.id : null;
    const selId = s.focus.kind === "selection" ? s.focus.id : null;
    // The rubber band goes through the same pass on the same frame as everything else it is being
    // drawn over, and only on the pane it is being stroked on (T-458).
    const pendingRegion = pending && pending.pane === pane.id ? pending.region : null;
    const pendingMeasureRegion = pendingMeasure && pendingMeasure.pane === pane.id ? pendingMeasure.region
      : pendingAnnotate && pendingAnnotate.pane === pane.id ? pendingAnnotate.region : null;
    // The found-signal boxes are the `detections` LAYER now (T-806, below), so this composition —
    // selections, measurements and the in-progress gesture, which are the user's own interaction
    // and are always drawn — passes `false` for them.
    // T-1002: THIS pane's rows — the answer to its own (t, f) window, never the active pane's.
    // T-1004: a pane that does not own the selection draws it as a linked ghost.
    return paneMarkBoxes(Object.values(paneRows(s.inventory, pane.id)), focusId, s.selections.list, selId, pane.box, pendingRegion, false, measurements, null, pendingMeasureRegion, !ownsSelection(pane.id));
  };

  // ---- the overlay layers (T-806 / MAP-06, docs/24 §13.2) ----
  // Each is a pure `(pane, edge) => OverlayQuad[]` over store state, placed through the pane's own
  // box and rect. `composeOverlays` concatenates the pane's VISIBLE ones in ascending z into the one
  // `marks` hook — still one place overlay geometry is produced and one pass (`overlay.ts`: no
  // sampler, no ramp) that draws it. MAP-07…MAP-13 each add one entry here.
  // T-994: which boxes have an open output (Listen / stream-out / decode / recording), from the
  // backend's open-output records the Active-outputs mount polls (`dock/activity.ts`). Re-derived
  // only when those records change, and read INSIDE the frame like every other mark's state, so the
  // halo and the badge are laid out through the pane's own mapping on every frame (never on the
  // poll's cadence — T-388).
  let activitySrc: readonly [unknown, unknown] = [null, null];
  let activityMap: Map<string, BoxActivity> = new Map();
  const activityNow = (): ReadonlyMap<string, BoxActivity> => {
    const s = store.get();
    if (activitySrc[0] !== s.outputs || activitySrc[1] !== s.servedOutputs) {
      activitySrc = [s.outputs, s.servedOutputs];
      activityMap = boxActivity(s.outputs, s.servedOutputs.pipelines, s.servedOutputs.recordings);
    }
    return activityMap;
  };
  /**
   * T-1004: does this pane OWN the page's one selection? Selection is global state (`focus`) —
   * rightly, since the lists, the sheet and the canvas must agree about what is selected — but it
   * was MADE in one pane, and drawing it as *the* selection in every pane at once says the opposite.
   * The owner is the ACTIVE pane: a press on a feature makes its pane active in the same dispatch
   * (T-1000), so the pane that owns the selection is the pane the user last selected in. Everywhere
   * else the same feature is drawn as the selection's **linked ghost** (`marks.ts`'s faint brackets,
   * `pins.ts`'s `linked` class) — the link is visible and the selection stays one place.
   */
  const ownsSelection = (paneId: string): boolean => !preview || preview.activePane === paneId;
  const detectionQuads: OverlayLayerFn = (pane, edge) => {
    const s = store.get();
    const focusId = s.focus.kind === "signal" ? s.focus.id : null;
    // T-910: the features, in their class symbology, generalized to a symbol under ~6 CSS px in both
    // axes — by the same predicate the pin layer's hit areas are laid out by, on the same frame.
    // T-1002: the detections of THIS pane's window (`inventory.panes[pane.id]`), so a pane frozen
    // on a past signal and a pane at the live edge each draw their own — in the activity symbology
    // the box carries (T-1019's `activityNow`), which is a property of the row, not of the window.
    // T-1004: a pane that does not own the selection draws the focused signal as a linked ghost.
    const rows = Object.values(paneRows(s.inventory, pane.id));
    return markQuads(signalMarkBoxes(rows, focusId, isUnexplained, activityNow(), !ownsSelection(pane.id)), edge, pane.box, pane.rect,
      { dpr: window.devicePixelRatio || 1, generalizeBelowPx: GENERALIZE_BELOW_CSS_PX });
  };
  // T-812 (MAP-12): band-plan priors — each pane's own `GET /api/priors` answer, as dashed strokes
  // through the pane's own box. The frame only records which window the pane showed; the fetch is on
  // the poll below (`refreshPriors`), never in the frame.
  const priorsByPane = new Map<string, PriorsAnswer>();
  const priorBoxes = new Map<string, Box>();
  const priorsInflight = new Set<string>();
  let priorsError: string | null = null;
  const priorLayer = new PriorLabelLayer(priorsLabelEl);
  const priorsQuads: OverlayLayerFn = (pane) => {
    priorBoxes.set(pane.id, pane.box);
    const a = priorsByPane.get(pane.id);
    return a ? priorQuads(a.rows, pane.box, pane.rect) : [];
  };
  /** Ask for each priors-on pane's window when it changed (quantized by `priorsPath`). A `GET` of
   * reference data — never a device route — and only for panes whose layer is on. */
  const refreshPriors = () => {
    const p = preview;
    if (!p) return;
    const ids = new Set(p.view.panes.list().map((x) => x.id));
    priorLayer.retain(ids);
    for (const id of [...priorsByPane.keys()]) if (!ids.has(id)) priorsByPane.delete(id);
    for (const [id, box] of [...priorBoxes]) {
      if (!ids.has(id) || !isLayerVisible(layersFor(id), "priors")) { priorBoxes.delete(id); continue; }
      const path = priorsPath(box);
      if (!path || priorsByPane.get(id)?.path === path || priorsInflight.has(id)) continue;
      priorsInflight.add(id);
      client.get<unknown>(path)
        .then((body) => {
          const a = parsePriors(path, body);
          if (a) priorsByPane.set(id, a);
          priorsError = a ? null : "the server's answer was not a priors list";
        })
        .catch((e: unknown) => { priorsError = e instanceof Error ? e.message : String(e); })
        .finally(() => { priorsInflight.delete(id); });
    }
  };
  let priorsShown = "";
  /** The active pane's ranked priors in words, rebuilt only when what it would say changes. */
  const renderPriorsReadout = (paneId: string) => {
    const on = isLayerVisible(layersFor(paneId), "priors");
    if (priorsEl.hidden === on) priorsEl.hidden = !on;
    if (!on) { priorsShown = ""; return; }
    const a = priorsByPane.get(paneId);
    const key = a ? `${paneId}|${a.path}` : `${paneId}|${priorsError ?? "loading"}`;
    if (key === priorsShown) return;
    priorsShown = key;
    if (!a) {
      priorsEl.replaceChildren(h("b", {}, priorsError ? `Band-plan priors unavailable: ${priorsError}` : "Band-plan priors: loading…"));
      return;
    }
    const head = h("b", { title: a.statement }, "Band-plan priors — suggestions, never truth");
    if (a.rows.length === 0) {
      priorsEl.replaceChildren(head, h("span", {}, " · no allocation in the bundled table intersects this window."));
      return;
    }
    const items = a.rows.slice(0, 6).map((r) => h("li", { class: r.offRasterHz !== null ? "off-raster" : "" }, r.reason));
    const more = a.rows.length > items.length || a.truncated ? h("span", {}, ` · ${a.rows.length - items.length} more drawn, not listed`) : null;
    priorsEl.replaceChildren(head, ...(more ? [more] : []), h("ol", {}, ...items));
  };
  const artifactQuads: OverlayLayerFn = (pane, edge) =>
    artifactLinkQuads(artifactLinks(Object.values(paneRows(store.get().inventory, pane.id)), edge), pane.box, pane.rect);
  // T-810 (MAP-10): the coarse-zoom density layer — `GET /api/tiles/events` counts, laid out here
  // through the SAME `toClip` and drawn only where `isCoarseZoom` says this pane is genuinely
  // coarse-zoomed (`densityQuads`'s own gate). The poll below only refreshes each pane's tiles for
  // its CURRENT address, and only while that gate is true — it never positions anything (T-388's
  // rule, followed by every layer here) and never fetches what the frame would draw nothing with.
  // Per-pane, per-address copies and WHEN each is asked again — plus the read's deadline, the joined
  // request's edge stamp and the write-back against the CURRENT addresses — are `./density-poll.ts`'s
  // (T-927). Here the layer is only polled and drawn.
  const densityPoll = new DensityPoll((url, signal) => client.get<unknown>(url, { signal }));
  const densityQuadsFn: OverlayLayerFn = (pane) => {
    const lat = preview?.view.surface.lat;
    if (!lat) return [];
    return densityQuads(densityPoll.tilesFor(pane.id), pane.box, pane.rect, lat, { dpr: window.devicePixelRatio || 1 });
  };
  /** One density poll: each density-on, coarse-zoomed pane's tile(s) (`isCoarseZoom`, the same gate
   * `densityQuadsFn` draws by — asking for tiles a fine-zoomed pane would draw nothing with is a
   * request this layer has no use for). A `GET` of an inventory aggregate, never a device route. */
  const refreshDensity = () => {
    const p = preview;
    if (!p) return;
    const edgeNs = p.view.panes.lastEdgeNs;
    densityPoll.tick(
      p.view.surface.lat, p.view.panes.views(edgeNs), edgeNs,
      (id) => isLayerVisible(layersFor(id), "density"), (id) => p.view.panes.isFollowing(id),
    );
  };
  // T-897 (docs/23 §10.6 rule 2): the traced paths — chirps, sweeps, hop sequences — as the backend
  // derived them (`GET /api/paths`), laid out HERE, per frame, through the pane's own box like every
  // other layer. The poll below only refreshes the records; it never positions anything (T-388).
  let paths: MarkPath[] = [];
  const pathQuadsFn: OverlayLayerFn = (pane) => pathQuads(paths, pane.box, pane.rect);
  // T-898 (docs/23 §10.6 rule 2): the radio's own route — the recorded tune intervals as the
  // backend traced them (`GET /api/tune-history`), laid out HERE, per frame, through the pane's
  // own box. The poll below only refreshes the records; it never positions anything (T-388).
  let tunePaths: TunePath[] = [];
  const tuneQuadsFn: OverlayLayerFn = (pane) => tuneQuads(tunePaths, pane.box, pane.rect);
  // T-981: the front-end events as the backend judged them (`GET /api/frontend/events`), laid out
  // HERE, per frame, through the pane's own box. The poll below only refreshes the records.
  let frontEndEvents: FrontEndEvent[] = [];
  const frontEndQuadsFn: OverlayLayerFn = (pane) => frontEndQuads(frontEndEvents, pane.box, pane.rect);
  // T-1008: the scan plan — a survey sweep's region and the steps the engine will take (as served,
  // `plan.windows`; the controller never tiles a range itself), hatched over the canvas, grey cells
  // included, and its progress while it runs. Laid out HERE, per frame, through the pane's own box;
  // the controller only holds the served state. Its button sits in the Go-to cluster (below).
  const scanCtl = new ScanController({
    client,
    paneWindow: () => {
      const v = preview ? paneById(preview.activePane) : null;
      return v ? { f0Hz: v.box.f0Hz, f1Hz: v.box.f1Hz } : null;
    },
    toast: (text) => store.set(toast(text)),
  });
  store.select((s) => s.device, (d) => scanCtl.update(d.scan, d.loaded), { immediate: true });
  const scanQuadsFn: OverlayLayerFn = (pane) => {
    // Where the active pane is, stated beside the plan it draws (CSS px from the canvas's top-left,
    // and its frequency window), so a check can find a plan edge on screen without re-deriving the
    // pane layout. Set-if-changed; presentation only.
    if (preview && pane.id === preview.activePane && scanCtl.model()) {
      const k = canvas.height > 0 ? canvas.clientHeight / canvas.height : 1;
      const d = scanCtl.panel.dataset;
      const put = (key: string, v: number) => { const t = String(v); if (d[key] !== t) d[key] = t; };
      put("paneF0Hz", pane.box.f0Hz); put("paneF1Hz", pane.box.f1Hz);
      put("paneLeftPx", pane.rect.x * k); put("paneWPx", pane.rect.w * k);
      put("paneTopPx", (canvas.height - pane.rect.y - pane.rect.h) * k); put("paneHPx", pane.rect.h * k);
    }
    return scanPlanQuads(scanCtl.model(), pane.box, pane.rect);
  };
  const overlayFns: Partial<Record<LayerId, OverlayLayerFn>> = {
    rules: ringQuads, detections: detectionQuads, density: densityQuadsFn, artifacts: artifactQuads,
    paths: pathQuadsFn, tune: tuneQuadsFn, frontend: frontEndQuadsFn, scan: scanQuadsFn, priors: priorsQuads,
  };
  /** The layer ids this build draws — the menu offers only these (a switch that draws nothing lies).
   * `base` is the base-style axis, not a toggle. `research` (annotations filed in no collection) and
   * every `collection:*` layer are drawn by `researchBoxesFor` below. */
  const drawnLayers = new Set<LayerId>([...Object.keys(overlayFns) as LayerId[], "research", "pins"]);
  /** The `data`-plane layers this build draws (T-807). Each is a flag on the one cell rule, handed
   * to the data pass per pane — never a quad, so never an entry in `overlayFns`. */
  const dataLayers = new Set<LayerId>(["coverage"]);
  const fogShown = (paneId: string): boolean => isLayerVisible(layersFor(paneId), "coverage");

  // ---- the pins (T-809 / MAP-09, docs/24 §14) ----
  // Hover and keyboard focus are pointer/focus state, kept here like `pending` rather than in the
  // store; selection IS the store's one `focus`, so the canvas, the sheet and the lists agree.
  let hoveredPin: PlacedPin | null = null;
  let focusedPin: PlacedPin | null = null;
  const selectPin = (p: PlacedPin) => {
    // A detection marker selects its row: `focusSignal` is what raises the detail sheet (T-804).
    // A curated marker has no focus target until MAP-21's panel; selecting one does nothing rather
    // than mis-focusing a row.
    // T-1002: selecting a feature makes ITS pane active first (§10.7's rule for a press on a pane,
    // which a press on the pin layer's own button does not otherwise reach). Without it the lists,
    // the focus panel and the detail sheet — all scoped to the active pane — would be asked for a
    // row from a window they are not showing, and would find nothing.
    if (preview && p.paneId !== preview.activePane) preview.activePane = p.paneId;
    if (p.pin.source === "detection") store.set(focusSignal(p.pin.id));
  };
  const pinLayer = new PinLayer(pinsEl, {
    onFocus: (p) => { focusedPin = p; },
    onSelect: selectPin,
  });
  // T-994: the keyboard's menu key / Shift+F10 on a focused feature (the browser targets the focused
  // button with `contextmenu`) opens the same menu, at the feature.
  pinsEl.addEventListener("contextmenu", (e) => {
    const btn = (e.target as HTMLElement | null)?.closest<HTMLElement>(".sf-pin[data-pin]");
    // T-1002: the row as the pin's OWN pane knows it (`data-pane`, written with the button) — with a
    // split, the feature under the key is not necessarily on the pane the lists are scoped to.
    const row = btn ? paneRows(store.get().inventory, btn.dataset.pane ?? "")[btn.dataset.pin ?? ""] : undefined;
    if (!btn || !row) return;
    e.preventDefault();
    const r = btn.getBoundingClientRect();
    openSignalMenu(ctx, row, e.clientX || r.left + Math.min(r.width, 24), e.clientY || r.top + Math.min(r.height, 24));
  });
  /** The MapTip for whichever pin is focused (keyboard) or else hovered (pointer), re-placed on the
   * pin's position THIS frame so it moves with the pin. It reads loaded state only. */
  const placeTip = () => {
    const want = focusedPin ?? hoveredPin;
    const now = want ? pinLayer.pins.find((q) => q.paneId === want.paneId && q.pin.id === want.pin.id) ?? null : null;
    if (!now) {
      if (!tipEl.hidden) tipEl.hidden = true;
      return;
    }
    const text = pinTipLines(now.pin).join("\n");
    if (tipEl.textContent !== text) tipEl.textContent = text;
    if (tipEl.hidden) tipEl.hidden = false;
    tipEl.dataset.pin = now.pin.id;
    // Beside the pin, flipped to its left where it would run off the canvas's right edge.
    const w = tipEl.offsetWidth;
    // For a feature drawn as its box (no glyph), beside the box's edge rather than over it.
    const xr = now.area ? now.area.x1 : now.x, xl = now.area ? now.area.x0 : now.x;
    const x = xr + 14 + w > canvas.clientWidth ? xl - 14 - w : xr + 14;
    tipEl.style.transform = `translate(${x.toFixed(1)}px, ${(now.y + 10).toFixed(1)}px)`;
  };
  const pinsFrame = (panes: readonly PaneView[], edge: number, hPx: number, dpr: number) => {
    const s = store.get();
    // T-910: the feature layer (hit areas, focus, labels) is over the DETECTIONS it identifies, so a
    // pane that hides its detections has none of it either — no invisible target for an undrawn box.
    // T-1002: and over the detections THIS pane has, which are its own window's.
    const layouts = panes
      .filter((v) => isLayerVisible(layersFor(v.id), "pins") && isLayerVisible(layersFor(v.id), "detections"))
      .map((v) => layoutPanePins(detectionPins(Object.values(paneRows(s.inventory, v.id))), v.id, v.box, v.rect, hPx, dpr, edge));
    pinLayer.update(layouts, (focusedPin ?? hoveredPin)?.pin.id ?? null, s.focus.kind === "signal" ? s.focus.id : null,
      activityNow(), preview?.activePane ?? null);
    placeTip();
  };
  /** T-1000: outline the active pane, from pane rectangles in drawing-buffer px. Set-if-changed, like
   * every other per-frame placement here. The outline's own `data-*` state which pane it is on, so a
   * test compares it with the chrome's words and the pane's rectangle rather than parsing pixels. */
  let outlined = "";
  const placeActive = (panes: readonly PaneView[], hPx: number, dpr: number) => {
    const p = preview;
    const active = p ? p.activePane : null;
    // T-1004: the Record IQ item is per-pane chrome too, and what it records depends on whether the
    // active pane is frozen — which a scrub changes without any chrome event. Re-stated on the frame
    // (set-if-changed inside), like every other per-pane statement here.
    recordBtn.sync();
    const name = activePaneName(panes.map((v) => v.id), active);
    const v = name ? panes.find((x) => x.id === active) ?? null : null;
    const box = v ? outlineBox(v.rect, hPx, dpr) : null;
    const key = box && name ? `${v!.id}|${name.label}|${box.left}|${box.top}|${box.width}|${box.height}` : "";
    if (key === outlined) return;
    outlined = key;
    activeEl.hidden = !box;
    stage.dataset.activePane = name ? String(name.n) : "";
    if (!box || !name) { delete activeEl.dataset.pane; return; }
    activeEl.dataset.pane = String(name.n);
    activeEl.dataset.paneId = v!.id;
    activeEl.dataset.label = name.label;
    Object.assign(activeEl.style, {
      left: `${box.left}px`, top: `${box.top}px`, width: `${box.width}px`, height: `${box.height}px`,
    });
  };
  /** CSS px from the canvas's top-left — the coordinate the pins are laid out in. */
  const cssPoint = (e: MouseEvent) => {
    const r = canvas.getBoundingClientRect();
    return { x: e.clientX - r.left, y: e.clientY - r.top };
  };

  // ---- T-821 (MAP-21): collections as overlay layers; every mark also a row in the Research panel ----
  // One model (`state.research`) behind both views: the rows are derived once per data change and
  // placed per frame through the pane's own box, like every other mark. A collection's layer is on
  // for a pane when that pane's layers menu says so, else by the collection's stored default.
  // Collections sit at z 40 (`COLLECTION_Z`) and unfiled research (markers only, below) at z 30:
  // ABOVE rules/detections but BELOW artifacts (z 50) and priors (z 60). They are not routed through
  // the registry (which lists only the collections a menu has touched), so the `marks` hook splits
  // `composeOverlays` around `COLLECTION_Z` and draws these marks in the gap — paint order is
  // ascending z throughout.
  //
  // T-984: annotations are excluded here and drawn exactly ONCE, by `annotationQuads` below — always
  // visible, dashed, never a claim about the air (T-820) — whatever a pane's research/collection
  // layers show. Before this fix an unfiled annotation with the `research` layer on, or a filed one
  // with its collection on, drew a SECOND "research-box" on top of the always-on dashed one.
  let researchSrc: ResearchSlice | null = null;
  let researchRowsNow: ResearchRow[] = [];
  let researchColors = new Map<string, readonly [number, number, number, number]>();
  let researchColls = new Map<string, Collection>();
  const researchBoxesFor = (pane: PaneView): MarkBox[] => {
    const r = store.get().research;
    if (r !== researchSrc) {
      researchSrc = r;
      researchRowsNow = researchRows(r).filter((row) => row.kind !== "annotation");
      researchColls = new Map(r.collections.map((c) => [c.id, c]));
      researchColors = new Map(r.collections.map((c) => [c.id, parseColor(c.color)]));
    }
    if (researchRowsNow.length === 0) return [];
    const reg = layersFor(pane.id);
    const unfiledOn = isLayerVisible(reg, "research");
    const on = (row: ResearchRow) => {
      if (row.key === r.selected) return true;
      const c = row.collectionId === null ? undefined : researchColls.get(row.collectionId);
      return c ? collectionVisibleOn(reg, c) : unfiledOn;
    };
    return researchMarkBoxes(researchRowsNow, researchColors, on, r.selected, pane.box, pane.rect);
  };

  // ---- the spectrum trace (T-457, docs/16 §8.5b finding 1) ----
  //
  // **Why it exists as a strip and not as a zoom level.** The cutover's own reason for dropping it is
  // the reason it is back in this shape: the surface draws folded cells *over time*, and a trace is
  // one spectrum *across frequency*. Zooming in gives finer cells, never a single row — so the trace
  // gets a rectangle of its own, carved off the top of **each pane** by `SurfaceView.frame`.
  //
  // **It is time-addressable, and the time is the pane's** (user, 2026-09-17). The slice is taken at
  // `box.t1Ns`, the newest instant *this viewport* is showing: the live edge when the pane follows
  // it, last hour when the pane has been scrubbed to last hour. A split therefore gives two traces at
  // two different instants, because it gives two viewports at two different instants — and none of
  // that needs a clock here, only the pane's own box, which is the rule every other time-varying
  // thing on this surface already obeys.
  //
  // **What it shares, and what it must not.** It shares the two axes: x through the pane's own
  // `toClip`, y through `Surface.lo`/`hi`, the one *measured* display range the ramp is relative to.
  // It shares no renderer state: the quads go to `overlay.ts`, which has no sampler and no ramp.
  //
  // **Why there is still no manual dB range, and what T-470 changed.** The old waterfall carried
  // `setScale(auto, lo, hi)` and **nothing ever called it** — a repo-wide search at the cutover
  // commit finds the definition and no caller — so the cutover retired an unreachable control rather
  // than a feature in use. A hand-set pair of numbers standing in for a measured range is a user
  // overriding a measurement, and that is still declined.
  //
  // What T-470 corrected is *which* measurement. The range was tracked from the tiles **currently on
  // screen**, so navigating re-coloured measurements that had not changed ("colours animate and
  // shift when I zoom"). It is now a stated span below a peak measured once over the region, so the
  // same dB is the same colour at every zoom — and so is this trace's y axis, which reads that same
  // pair. `Auto-contrast` is the opt-in way back to tracking, and it is a *contrast* control rather
  // than a hand-set scale: it still colours from a measurement, just from the visible one. The honest
  // part is unchanged — the range is **said**, by the readout below and by the label in the bar, so a
  // surprising picture is diagnosable instead of paintable-over.
  // **T-1041: OFF by default.** The trace used to be on, and to pay for itself with a reserved
  // band above every pane — which is the black bar the user saw at the top of a full-bleed map
  // ("the top bar looks like it's actually the phosphor display … we can remove it entirely for
  // now", 2026-09-25). It is now a layer like any other: no reserved space, drawn over the pane's
  // top rows when it is on, and nothing at all when it is off. T-457's invariant — viewport-wide,
  // time-addressable, per pane — is a property of that layer, not of a band.
  let traceOn = false;
  const fmtDb = (db: number) => `${db.toFixed(1)} dB`;
  const fmtDur = (s: number) => (s < 1 ? `${(s * 1000).toFixed(0)} ms` : s < 90 ? `${s.toFixed(1)} s` : `${(s / 60).toFixed(1)} min`);
  /**
   * **What each pane's live lane did, this frame** (T-1042) — behind the same flag as the lane: rows
   * painted from the ring, tile addresses those rows made unnecessary, and one row's height in px.
   *
   * Read by `ui/e2e/live-ring.e2e.mjs`, which can see from the pixels that the newest rows are on
   * screen but cannot see *which lane* put them there — and "the edge kept up" is a claim about the
   * lane. Written from the `PaneReport` the data pass just produced, in the pass that produced it,
   * and only when the numbers change. Presentation metadata: commands nothing, and absent entirely
   * with the flag off.
   */
  let ringDiag = "";
  const ringReports = new Map<string, { rows: number; tiles: number; rowPx: number }>();
  const stateRing = (report: PaneReport) => {
    ringReports.set(report.id, { rows: report.ringRows, tiles: report.ringTiles, rowPx: Math.round(report.ringRowPx * 10) / 10 });
    const next = JSON.stringify([...ringReports].map(([id, r]) => ({ id, ...r })));
    if (next === ringDiag) return;
    ringDiag = next;
    stage.dataset.liveRing = next;
  };
  const traceFor = (pane: PaneView, _edgeNs: number, report: PaneReport, strip: PaneRect): TracePath[] => {
    const p = preview;
    if (!p) return [];
    if (flags().liveRing) stateRing(report);
    const s = p.view.surface;
    const dev = pane.device ?? "any";
    // **The pane's OWN lattice, off the report the data pass just produced** (T-505). Since the
    // surface draws from two tiers, `report.levelF/levelT` are indices into `report.lat` and mean
    // a different cell on the other one — reading them against the host's lattice would put the
    // trace on tiles the pane never drew, which is the T-388 family (two derivations of one
    // picture) with a scheme in it.
    const lat = report.lat;
    const n = Math.max(16, Math.min(TRACE_COLUMNS, Math.floor(strip.w)));
    const out: TracePath[] = [];

    // The max-hold, over this viewport's WHOLE window, from the tiles it just drew at the level it
    // drew them. Not an accumulator: the pyramid's cells ARE max-holds (hk-api's `MAX_HOLD_RULE`),
    // so panning to an hour ago shows that hour's peak instead of restarting from nothing.
    //
    // It keeps a FLAT ink (T-475), and that is the one place the ramp deliberately does not reach:
    // the max-hold answers a different question over a different interval than the slice, so giving
    // both the ramp would put two identically-coloured lines on one strip.
    const hold = maxHoldColumns(lat, s.cache, pane.box, report.levelF, report.levelT, dev, n);
    // **LSR-6 (T-1047): fold in the ring-covered max-hold.** T-1042 tells the tile lane to stand
    // aside over the ring's own extent (`report.ringCover`), so the pyramid has nothing requested
    // there at all — the ring is the only source that can answer for it. Folding is a plain max,
    // never a replacement: a max-hold is idempotent and associative, so taking the greater of the two
    // sources over the same cell is still a max-hold over their union, and the pyramid still answers
    // for the rest of the window exactly as before.
    if (report.ringFrame && report.ringCover) {
      const ringHold = ringMaxHoldColumns(report.ringFrame, report.ringCover, n);
      for (let c = 0; c < n; c++) {
        const v = ringHold[c];
        if (Number.isFinite(v) && !(hold[c] >= v)) hold[c] = v;
      }
    }
    out.push(...tracePaths(hold, pane.box, strip, s.lo, s.hi, "trace-hold", pane.id,
      { ink: [HOLD_INK[0], HOLD_INK[1], HOLD_INK[2]], alpha: HOLD_INK[3], widthPx: HOLD_PX }));

    // The slice, at this viewport's own time position. The live RING is preferred first — a row that
    // CONTAINS this instant is a strictly finer answer than any pyramid cell that merely spans it
    // (T-1047 / LSR-6) — then the single held live row for a band with no ring, then the pyramid
    // everywhere else, which is what makes a scrubbed pane show the spectrum of *then*.
    const tAtNs = pane.box.t1Ns;
    const win = sliceWindow(lat, report.levelT, tAtNs);
    const ringAt = report.ringFrame ? ringRowAt(report.ringFrame, tAtNs) : null;
    const fr = liveRow.get();
    const live = !ringAt && liveFrameFits(fr, win);
    let slice: Float32Array;
    let sliceSrc: string;
    let sliceAtNs: number;
    if (ringAt && report.ringFrame) {
      slice = sampleRingRow(report.ringFrame, ringAt.slot, pane.box, n);
      sliceSrc = "live ring row";
      sliceAtNs = ringAt.tNs;
    } else if (live && fr) {
      slice = sampleFrame(fr, pane.box, n);
      sliceSrc = "live frame";
      sliceAtNs = fr.tNs;
    } else {
      slice = sliceColumns(lat, s.cache, pane.box, report.levelF, report.levelT, dev, n, tAtNs);
      sliceSrc = `${fmtDur((win.t1Ns - win.t0Ns) / S_TO_NS)} cell`;
      sliceAtNs = tAtNs;
    }

    // **The afterglow** (T-475): the rows just before THIS pane's time position, oldest first so the
    // newest shadow sits on top of the older ones and the current slice on top of all of them. They
    // come from the pyramid at the level the pane drew — the same cells under the strip — so a pane
    // scrubbed into last hour glows with last hour, which is the whole point of deriving them from
    // the window instead of from a buffer of whatever the page received.
    const shadows = persistenceSlices(lat, s.cache, pane.box, report.levelF, report.levelT, dev, n, tAtNs);
    // T-806: the pane's BASE STYLE (docs/24 §4). `ramp` is T-475's look, unchanged: the slice's core
    // on the waterfall's ramp. `phosphor` draws the slice and its afterglow in one flat green ink,
    // off the ramp — so it is a look, not a second measurement colour, and the readout says which.
    const phosphor = layersFor(pane.id).base === "phosphor";
    for (const sh of [...shadows].reverse()) {
      out.push(...tracePaths(sh.cols, pane.box, strip, s.lo, s.hi, "trace-glow", pane.id,
        phosphor ? { alpha: sh.alpha, widthPx: SHADOW_PX, ink: PHOSPHOR_INK } : { alpha: sh.alpha, widthPx: SHADOW_PX, shade: "mono" }));
    }

    // The current slice, twice: a wide neutral bloom under a full-opacity ramp-coloured core. The
    // core's centre is exactly `cmap((db - lo) / (hi - lo))` — the same argument, the same ramp and
    // the same range as the cell at that dB below it — which is the equality
    // `ui/e2e/app-trace.e2e.mjs` reads off the framebuffer. The bloom is grey on purpose: it is a
    // glow, not a second reading, and keeping it achromatic leaves exactly one measurement colour on
    // the strip (see `TraceStyle.shade`).
    out.push(...tracePaths(slice, pane.box, strip, s.lo, s.hi, "trace-bloom", pane.id,
      { alpha: 0.18, widthPx: GLOW_PX, shade: "mono" }));
    out.push(...tracePaths(slice, pane.box, strip, s.lo, s.hi, "trace-slice", pane.id,
      phosphor ? { alpha: 1, widthPx: SLICE_PX, ink: PHOSPHOR_INK } : { alpha: 1, widthPx: SLICE_PX }));

    // The readout, for the pane gestures apply to. Written here rather than on the poll for the same
    // reason the quads are: it describes the frame that was just drawn. It names the SOURCE, because
    // "a live frame" and "a 1.0 s cell" are different claims about the same picture and the coarser
    // one must not be passed off as an instant.
    if (pane.id === p.activePane) {
      const slicePk = peakOf(slice, pane.box);
      const holdPk = peakOf(hold, pane.box);
      const spanS = (pane.box.t1Ns - pane.box.t0Ns) / S_TO_NS;
      const src = sliceSrc;
      // **A gap is not evidence of quiet, and "not loaded" is not "never observed."** The trace draws
      // nothing in either case, which is the safe direction — absence claims nothing. But the
      // sentence beside it must not turn a memory-and-latency fact into a statement about the radio,
      // which is exactly the distinction `cellrule.ts` keeps between PENDING and the one grey. The
      // `PaneReport` the renderer just produced is what knows which it is.
      const empty = report.tiles === 0
        ? `no tile in hand for this span yet (${report.pending} pending, ${report.fallbacks} coarse stand-in${report.fallbacks === 1 ? "" : "s"}) — not loaded is not unobserved`
        : "nothing observed across this span";
      setText(traceEl, [
        slicePk
          ? `slice ${at(sliceAtNs)} (${src}) · peak ${fmtDb(slicePk.db)} at ${fmtHz(slicePk.hz)}`
          : `slice ${at(tAtNs)} (${src}) — ${empty}`,
        ...(phosphor ? ["phosphor style: trace in one green ink, not on the ramp"] : []),
        holdPk
          ? `max-hold over ${fmtDur(spanS)} · peak ${fmtDb(holdPk.db)} at ${fmtHz(holdPk.hz)}`
          : `max-hold over ${fmtDur(spanS)} — ${empty}`,
        // **The afterglow says which instants it is**, because "several fading lines" is otherwise
        // unfalsifiable decoration: the sentence names the rows, and they are rows of THIS pane's
        // window, so a scrubbed viewport states past instants and a following one states recent
        // ones. `app-trace.e2e.mjs` reads this back while scrubbed, which is the demonstration the
        // ticket asks for and the one a live-only persistence buffer could not make.
        shadows.length
          ? `afterglow ${shadows.length} × ${fmtDur((win.t1Ns - win.t0Ns) / S_TO_NS)} back to ${at(shadows[shadows.length - 1].tAtNs - (win.t1Ns - win.t0Ns))}`
          : afterglowAbsence(report,
            persistenceShortTiles(lat, s.cache, pane.box, report.levelF, report.levelT, dev, tAtNs)),
        // T-470: one scale for the trace's y axis and the ramp, and it says which of the two ways it
        // was decided. It used to read "measured from the served tiles" — true of the viewport-
        // tracking range, and exactly what stopped being true when the scale stopped following the
        // viewport. `app-trace.e2e.mjs` asserts this sentence, and was updated with it.
        // T-528 adds the third case. The numbers are `s.lo`/`s.hi` — the pair the frame that is on
        // the screen was uploaded with, not the one the next frame will use (see `Surface.next`) —
        // so this sentence is true of the pixels beside it in every mode, which is T-475's rule and
        // is what a re-measured-per-frame scale makes load-bearing rather than incidental.
        `scale ${fmtDb(s.lo)} … ${fmtDb(s.hi)}, ${s.range.mode === "anchored"
          ? "measured over the region and anchored there"
          : s.range.mode === "viewport"
            ? "measured from the observed cells in this view, shadows excluded (viewport scale)"
            : "measured from the tiles on screen (auto-contrast)"}, shared with the ramp`,
      ].join(" · "));
      // **The bar's range line, on the FRAME, not on the 1 s chrome poll** (T-528). It was on the
      // poll because the anchored range never moves and `auto` creeps; a viewport-measured range
      // moves with every pan, and a bar quoting a second-old scale beside pixels drawn with the
      // current one is the readout-disagrees-with-the-pixels defect in miniature. Set-if-changed,
      // so the anchored mode still writes the DOM exactly once. The poll below stays as the path
      // for when the trace strip is switched off and this callback does not run.
      setText(rangeEl, rangeLabel(s.range));
    }
    return out;
  };
  // The trace layer's switch — the layers menu's "Every pane" row since T-882 (it was the toolbar's
  // `Trace` button). Presentation only: whether the layer is drawn and whether the readout is
  // written. Since T-1041 it moves no geometry: the pane is the same rectangle either way.
  const setTrace = (on: boolean) => {
    traceOn = on;
    if (preview) preview.view.tracePx = traceOn ? TRACE_PX : 0;
    traceEl.hidden = !traceOn;
    if (!traceOn) traceEl.textContent = "";
    syncLayerControls();
  };
  // T-522: the found-signal boxes' switch is the ACTIVE pane's `detections` layer (T-806) — the
  // layers menu's overlay row, since T-882 its only control. Presentation only — no route, no poll.
  // The frame reads the pane's registry fresh every frame, so the next frame just draws fewer boxes;
  // there is no cache or subscription to invalidate.
  const setSignals = (id: string, on: boolean) => {
    editLayers(setPaneLayer(id, "detections", on, seedFor(id)), id);
    writeShowSignals(on);
  };

  // ---- mirror the active viewport into the app's one window (CLAUDE.md's whole-UI window rule) ----
  // The inventory lists, the focus panel and the decode captures all scope themselves through
  // `state.live.view` + `state.time` (`explore/inventory.ts`'s `viewWindow`). Writing the viewport
  // there is what keeps them answering about what is on screen. Since T-506 retired the capture
  // band's scrub, the viewport is the time cursor's only editor.
  let lastMirror = "";
  function mirror(): void {
    const p = preview;
    if (!p) return;
    const pane = p.view.panes.get(p.activePane);
    if (!pane) return;
    publishPanes();
    const loHz = pane.freq.centerHz - pane.freq.spanHz / 2, hiHz = pane.freq.centerHz + pane.freq.spanHz / 2;
    const spanS = pane.time.spanNs / S_TO_NS;
    const key = `${loHz}|${hiHz}|${pane.time.live}|${spanS}|${pane.time.live ? "" : pane.time.centerNs}`;
    if (key === lastMirror) return;
    lastMirror = key;
    store.set((s) => ({ live: { ...s.live, view: { loHz, hiHz } } }));
    if (pane.time.live) store.set((s) => (s.time.live && s.time.spanS === spanS ? {} : { time: { live: true, spanS } }));
    else store.set(reviewAt(pane.time.centerNs / S_TO_NS + spanS / 2, spanS));
  }

  // ---- and publish EVERY pane's window, not only the active one (T-1002) ----
  //
  // The mirror above is what the chrome is scoped to; this is what the DETECTIONS are scoped to.
  // The inventory is time-scoped to the view (CLAUDE.md's signal model) and each pane is its own
  // view, so each pane's `(t, f)` window is published and answered separately — otherwise freezing
  // pane 1 on a past signal re-scopes pane 2's live boxes to pane 1's past window, which is exactly
  // the split view failing to be a split.
  //
  // View arithmetic only: a pane's own centre/span in both axes, in layout order, plus which one is
  // active. `setInventoryPanes` is a no-op when nothing moved, so this is safe on the frame hook —
  // and it has to be on one, because the active pane changes in a press's own dispatch (T-1000) and
  // the lists must follow it there, not a poll later.
  /** One pane's window as the inventory queries need it — the same arithmetic `mirror()` publishes
   * for the active pane, per pane. `tS` is the instant the pane's window ENDS at. */
  const paneSpec = (pane: PaneState, n: number): PaneWindowSpec => {
    const spanS = pane.time.spanNs / S_TO_NS;
    return {
      id: pane.id, n,
      loHz: pane.freq.centerHz - pane.freq.spanHz / 2,
      hiHz: pane.freq.centerHz + pane.freq.spanHz / 2,
      live: pane.time.live,
      tS: pane.time.live ? null : pane.time.centerNs / S_TO_NS + spanS / 2,
      spanS,
    };
  };
  function publishPanes(): void {
    const p = preview;
    if (!p) return;
    const list = p.view.panes.list();
    const specs = list.map((pane, i) => paneSpec(pane, i + 1));
    const i = list.findIndex((pane) => pane.id === p.activePane);
    const at = i < 0 ? 0 : i;
    const active = specs.length
      ? { id: specs[at].id, n: at + 1, count: specs.length, label: `pane ${at + 1} of ${specs.length}` }
      : null;
    store.set(setInventoryPanes(specs, active));
  }

  // ---- which front end a pane draws, and retunes (T-1006) ----
  //
  // docs/16 §8 gave every pane a `device` and said what it means: it *"only chooses whose coverage
  // decides its grey"*. With one radio the default `any` IS that radio and there was nothing to say;
  // with two (MSDR) the pane's grey, its retune's `device_id` and the Go-to offer all depend on it,
  // and nothing on screen said which. The pill on each pane's status row says it; the picker in the
  // viewport menu sets it; and `surface/panedevice.ts` owns every word and every decision — this
  // host only reads the list off the store and hands the strings on.
  /** Every live front end this run holds, off the `/api/control/state` poll. `[]` on a replay. */
  const attached = (): readonly AttachedDevice[] => store.get().device.devices;
  /** How the chrome names the pane a per-pane control acts on — `activePaneName`'s words, reused so
   * the viewport menu's device section names the pane exactly as its layers section does. */
  const paneMenuName = (): string => {
    const p = preview;
    if (!p) return "this pane";
    const ids = p.view.panes.list().map((x) => x.id);
    const n = ids.indexOf(p.activePane) + 1;
    return ids.length > 1 ? `pane ${n} of ${ids.length}` : "this pane";
  };

  // ---- the retune control (T-444, made persistent and per-pane by T-476) ----
  //
  // It used to be one control in the toolbar, shown only when `paneRetuneOffer` produced an offer —
  // which it did only for a viewport NOT contained in a tuned window. The user's report was that the
  // thing they wanted most appeared only sporadically, and the reason is that the containment
  // trigger hid it exactly where it is worth the most: a pane zoomed deep INSIDE the tuned window,
  // where a narrower capture is a finer look at what is already on screen.
  //
  // So it is now a slot on **every pane's chrome row**, asked for on every frame. Two consequences
  // worth stating, because both are load-bearing:
  //
  //  - **Per frame, not per poll.** The sentence names the viewport's current window, and a label
  //    produced on the 1 s poll while the window moves per frame names somewhere the pane is not.
  //  - **The offer that was PAINTED is the one that is pressed.** `lastPainted` records what each
  //    row's label was derived from, so `acceptPaneRetune`'s re-derivation at commit is comparing
  //    the destination the user read against the viewport as it is *now* — which is T-444's guard
  //    doing its job rather than comparing an offer to itself. Deriving fresh at press instead would
  //    close that window by making the guard vacuous, and would let a press land on a frequency the
  //    button had never displayed.
  const offerNow = (paneId: string): PaneRetuneOffer | null => {
    const p = preview;
    const pane = p?.view.panes.get(paneId);
    if (!p || !pane) return null;
    // T-1006: and WHICH front end. The offer names the `device_id` this pane's retune would carry,
    // or blocks when the pane's `device` cannot be resolved to one radio — read off the same 2 s
    // control-state poll the rest of the device slice comes from, never a second source.
    return paneRetuneOffer(pane, windows, store.get().navGrid.grid?.frequency ?? null, p.edgeNs, EDGE_GRACE_NS, attached());
  };
  /** What each row's label was last derived from — the target the user actually consented to. */
  const lastPainted = new Map<string, PaneRetuneOffer>();

  const chromeAction = (paneId: string): RowAction | null => {
    const o = offerNow(paneId);
    if (!o) {
      lastPainted.delete(paneId);
      return null;
    }
    lastPainted.set(paneId, o);
    return { label: "Retune", why: offerLabel(o), enabled: offerAcceptable(o) };
  };

  const pressRetune = (paneId: string): void => {
    const o = lastPainted.get(paneId);
    if (o) pressOffer(o);
  };
  /** Press a painted offer — the pane row's viewport-covering retune (T-802's floating Go-to has
   * had its own path since T-947, `pressGotoOffer` below, because it plans a named span rather than
   * the viewport). Takes the SAME object the row derived after the move, so this reaches the one
   * gate with the destination the user actually read, and refuses if the view moved since. */
  const pressOffer = (o: PaneRetuneOffer): void => {
    const p = preview;
    if (!p) return;
    void acceptPaneRetune(ctx, {
      offerNow,
      // T-437 §5.2: the growing edge's tiles were computed from the tuning that has just ended, so
      // a cached one is an observation claim about a tuning that no longer exists.
      invalidateEdge: () => p.view.surface.cache.invalidateEdge(p.view.surface.lat, p.edgeNs),
    }, o).then((r) => {
      if (!r.ok && r.reason === "moved") store.set(toast("The viewport moved: the offer was for where it was. Press again."));
    });
  };

  // ---- the capture-width presets (T-496) ----
  //
  // The width IS changeable today (zoom, then press Retune above), but only as a side effect of two
  // gestures — nothing on screen says so. This is a second, explicit way to ask for a width without
  // zooming first: a fixed list of round spans, each planned through `paneWidthOffer`, which reaches
  // `smallestCoveringSpan` through the SAME `retunePlan` the control above calls. Zoom-then-retune is
  // untouched — this adds a way to say the width, it does not replace the one that already works.
  //
  // Same painted-offer discipline as the retune control: each preset's offer is re-derived every
  // frame and recorded by (pane, span), so a press re-derives from the state that was ACTUALLY
  // painted and refuses if it moved (T-407) — comparing a fresh offer to itself would prove nothing.
  //
  // `chrome.ts` must not learn what a span is (it stays as ignorant of tuning as `RowAction` already
  // keeps it — `ui/test/surface-retune.test.ts` greps its source for "spanHz" and fails if it is
  // there), so each preset crosses into `WidthAction` as an opaque `key` — `String(spanHz)` — and is
  // parsed back only here, on this side of the boundary.
  const paintedKey = (paneId: string, spanHz: number) => `${paneId}:${spanHz}`;
  const lastPaintedWidth = new Map<string, PaneWidthOffer>();

  const widthOfferNow = (paneId: string, spanHz: number): PaneWidthOffer | null => {
    const p = preview;
    const pane = p?.view.panes.get(paneId);
    if (!p || !pane) return null;
    return paneWidthOffer(pane, spanHz, store.get().navGrid.grid?.frequency ?? null, p.edgeNs, EDGE_GRACE_NS, attached());
  };

  const widthLabel = (hz: number) => hz < 1e6 ? `${Math.round(hz / 1e3)} kHz` : `${(hz / 1e6).toFixed(hz % 1e6 === 0 ? 0 : 1)} MHz`;

  const widthActions = (paneId: string): WidthAction[] =>
    WIDTH_PRESETS_HZ.map((spanHz): WidthAction => {
      const o = widthOfferNow(paneId, spanHz);
      const key = String(spanHz);
      lastPaintedWidth.delete(paintedKey(paneId, spanHz));
      if (!o) return { label: widthLabel(spanHz), why: "No viewport to plan against.", enabled: false, key };
      lastPaintedWidth.set(paintedKey(paneId, spanHz), o);
      return { label: widthLabel(spanHz), why: widthOfferLabel(o), enabled: widthOfferAcceptable(o), key };
    });

  const pressWidth = (paneId: string, key: string): void => {
    const spanHz = Number(key);
    const p = preview, o = lastPaintedWidth.get(paintedKey(paneId, spanHz));
    if (!p || !o) return;
    void acceptPaneWidth(ctx, {
      offerNow: widthOfferNow,
      // T-437 §5.2, same as the retune control: the growing edge's tiles described the tuning that
      // has just ended.
      invalidateEdge: () => p.view.surface.cache.invalidateEdge(p.view.surface.lat, p.edgeNs),
    }, o).then((r) => {
      if (!r.ok && r.reason === "moved") store.set(toast("The viewport moved: the offer was for where it was. Press again."));
    });
  };

  // ---- the floating Go-to's offer (T-802, corrected by T-947) ----
  //
  // T-947: the offer used to be `offerNow`'s — the SAME retune control the pane row paints, whose
  // plan covers the pane's whole VIEWPORT. That is right for the row (the viewport is the region the
  // user is looking at and wants sharpened) and wrong here: a Go-to only names a CENTRE, and the
  // viewport it lands in is whatever the pane happened to be zoomed to before the jump — so a pane
  // left zoomed out proposed a multi-megahertz capture nobody asked for (a 15.8 MHz view spanned a
  // 15.819 MHz plan; found live 2026-09-25). What Go-to should plan for is a SPAN: the front end's own
  // current window when it has one (`grid.current.span_hz` — "keep the current tuned span"), else
  // `GOTO_DEFAULT_SPAN_HZ`. Built through `paneWidthOffer`/`acceptPaneWidth`, the exact path T-496
  // already uses to plan a NAMED span at a pane's own centre rather than its viewport — so this reuses
  // the width control's arithmetic instead of adding a second way to derive one.
  let lastPaintedGoto: PaneWidthOffer | null = null;
  const gotoSpanHz = (): number => goToSpanHz(store.get().navGrid.grid?.frequency ?? null, GOTO_DEFAULT_SPAN_HZ);
  const pressGotoOffer = (): void => {
    const p = preview;
    if (!p || !lastPaintedGoto) return;
    // T-1004: on a FROZEN pane the same offer is takeable as "go live at this frequency" — the pane
    // returns to the live edge (view state, `panes.follow`) and only then is the capture taken,
    // through the same gate. `acceptGoLive` owns the order and both guards; this supplies the one
    // thing it cannot do for itself, which is moving this host's pane and re-publishing the window.
    if (isGoLiveOffer(lastPaintedGoto)) {
      void acceptGoLive(ctx, {
        offerNow: widthOfferNow,
        invalidateEdge: () => p.view.surface.cache.invalidateEdge(p.view.surface.lat, p.edgeNs),
      }, lastPaintedGoto, (paneId) => {
        p.view.panes.follow(paneId);
        lastMirror = ""; mirror(); renderLive(); viewMoved();
      }).then((r) => {
        if (!r.ok && r.reason === "moved") store.set(toast("The viewport moved: the offer was for where it was. Press again."));
      });
      return;
    }
    void acceptPaneWidth(ctx, {
      // Re-derive at the SAME asked span the offer was painted for — `acceptPaneWidth` passes it
      // back in, exactly as T-407's guard requires (comparing a fresh offer to itself proves
      // nothing); if the current/default span has since changed that is `sameWidthTarget`'s job to
      // catch, not this function's.
      offerNow: widthOfferNow,
      invalidateEdge: () => p.view.surface.cache.invalidateEdge(p.view.surface.lat, p.edgeNs),
    }, lastPaintedGoto).then((r) => {
      if (!r.ok && r.reason === "moved") store.set(toast("The viewport moved: the offer was for where it was. Press again."));
    });
  };

  // ---- retune mode (T-1028): the one mode in which a gesture commands the radio ----
  //
  // The user's 2026-09-25 amendment to the navigation invariant. Everything that makes it safe is in
  // `surface/retune-mode.ts`; what lives here is the three seams a host owns: WHICH pane (the active
  // one, T-1000), WHEN a gesture settled (`attachSurfaceInput`'s `onGesture`), and HOW it is said on
  // screen (the chip, the banner and the pane's status line). The mode is off at mount and nothing
  // below runs while it is off, so `app-map-controls`/`surface-input`'s empty-call-list controls hold
  // exactly as they did.
  const retuneTargetNow = (paneId: string): RetuneModeTarget | null => {
    const p = preview;
    const pane = p?.view.panes.get(paneId);
    if (!p || !pane) return null;
    // The pane's OWN device would choose the grid here once a pane can name one (T-1006); until
    // then there is one front end and one grid, exactly as the Retune button's `offerNow` reads it.
    // T-1006: the settled-view retune is a path to the front end like any other, so it names the
    // pane's own radio — and refuses out loud when the pane's device does not resolve to one.
    return retuneModeTarget(pane, store.get().navGrid.grid?.frequency ?? null, p.edgeNs, EDGE_GRACE_NS, attached());
  };
  /** The last thing that happened to a pane in the mode, so the status line has something to say
   * after the retune landed rather than blanking the instant the request resolves. */
  let retuneSaid: { paneId: string; text: string } | null = null;
  const retuneMode = new RetuneModeController({
    commit: async (paneId) => {
      const p = preview;
      if (!p) return;
      const r = await commitRetuneMode(ctx, {
        targetNow: retuneTargetNow,
        // T-437 §5.2, as everywhere else: the growing edge's tiles describe the tuning that ended.
        invalidateEdge: () => p.view.surface.cache.invalidateEdge(p.view.surface.lat, p.edgeNs),
      }, paneId);
      // What the pane says afterwards. A refusal is said too: in this mode the user did not press a
      // button, so silence would read as "my pan did nothing" rather than "the front end said no" —
      // and `applyDeviceAction` has already toasted the reason.
      retuneSaid = r.target
        ? { paneId, text: r.ok ? retuneModeLabel(r.target) : `${retuneModeLabel(r.target)}${r.reason === "refused" ? " The front end refused it." : ""}` }
        : null;
    },
    onChange: () => { syncRetuneChip(); },
  });
  /** Set once the cluster is mounted; before that there is no chip to re-state. */
  let syncRetuneChip: () => void = () => {};
  /** The pane's own status line while the mode is acting on it (`chromeStatus`, per frame). */
  const retuneStatusFor = (paneId: string): string | null => {
    if (retuneMode.pendingPane === paneId) {
      const t = retuneTargetNow(paneId);
      const what = t ? retuneModeLabel(t) : "Retune mode: nothing to plan against yet.";
      return retuneMode.inFlight ? `${what} (asking the front end…)` : `${what} (settling…)`;
    }
    return retuneSaid && retuneSaid.paneId === paneId && retuneMode.on ? retuneSaid.text : null;
  };
  /** `R`: held for one gesture, tapped to latch (`isRetuneKey` says which events count). */
  const onRetuneKeyDown = (e: KeyboardEvent) => {
    if (!isRetuneKey(e, isTypingTarget) || e.repeat) return;
    e.preventDefault();
    retuneMode.keyDown();
  };
  const onRetuneKeyUp = (e: KeyboardEvent) => {
    if (!isRetuneKey(e, isTypingTarget)) return;
    e.preventDefault();
    retuneMode.keyUp();
  };

  // ---- pointer: hover readout, click to focus, right-click for the menu ----
  //
  // **One hit test, `preview.paneAt`, so hover and gesture cannot disagree about where a pointer is.**
  // This used to walk `lastFrame.views` itself — a second copy of the same arithmetic, which is how
  // T-457's trace strip became a hole that `input.ts` dropped gestures into while this file happily
  // read out a hover a few pixels away. The strip belongs to its pane (`paneAtPoint`), and the point
  // is clamped into the pane's own rectangle, so a hover over the strip reads the pane's **top edge**:
  // the same frequency, at the instant the strip is a spectrum of.
  const paneUnder = (x: number, y: number): { view: PaneView; at: { x: number; y: number } } | null => {
    const p = preview;
    const id = p?.paneAt({ x, y }) ?? null;
    const view = id ? p!.lastFrame?.views.find((v) => v.id === id) ?? null : null;
    return view ? { view, at: clampToRect(view.rect, { x, y }) } : null;
  };
  const fmtHz = (hz: number) => `${(hz / 1e6).toFixed(hz < 1e9 ? 4 : 6)} MHz`;
  // The CAPTURE clock's own instant, rendered as UTC (the `hms` idiom the retired live view used).
  // Not `toLocaleTimeString`: T-393's guard forbids every browser clock in these modules, and a
  // formatter that reaches for the host's timezone is one keystroke from a fallback that reaches
  // for the host's *time* — which on a replay or a time-compressed scene is not this data's time.
  const at = (ns: number) => `${new Date(ns / 1e6).toISOString().slice(11, 19)}Z`;
  const clock = (sec: number) => at(sec * S_TO_NS);

  function hitAt(x: number, y: number): { pane: PaneView; mark: MarkBox | null; fHz: number; tNs: number } | null {
    const hit = paneUnder(x, y);
    if (!hit) return null;
    const { view: v, at } = hit;
    const { fHz, tNs } = pointOn(v.box, v.rect, at.x, at.y);
    return { pane: v, fHz, tNs, mark: markAt([...boxesFor(v), ...researchBoxesFor(v)], preview!.edgeNs, fHz, tNs) };
  }

  // ---- shift+drag marks out a region (T-458) ----
  //
  // Two destinations, one gesture, and which one is not inferred: `explore.bandEdit` is armed by the
  // context menu's "Adjust band" and names a Confirmed row explicitly. Unarmed, a stroke is a new
  // selection. Nothing here reaches a device route — a region is `POST /api/selections` or
  // `PUT /api/inventory/{id}/band`, and neither is a tuning.
  const paneById = (id: string): PaneView | null =>
    preview?.lastFrame?.views.find((v) => v.id === id) ?? null;

  /** A stroke's two corners as a region of this pane's own window. The release corner is clamped to
   * the pane: `pointOn` extrapolates outside the rectangle, and a selection running past the edge of
   * the viewport would claim frequencies and times the user could not see to choose. */
  const regionOf = (r: { pane: string; a: GlPoint; b: GlPoint }): MarkRegion | null => {
    const v = paneById(r.pane);
    if (!v) return null;
    const clamp = (p: GlPoint) => pointOn(v.box, v.rect,
      Math.min(Math.max(p.x, v.rect.x), v.rect.x + v.rect.w),
      Math.min(Math.max(p.y, v.rect.y), v.rect.y + v.rect.h));
    return normalizeRegion(clamp(r.a), clamp(r.b));
  };

  // ---- measurement mode (T-822 / MAP-22) ----
  //
  // A toggle, not a modifier: `surface/input.ts` reads `measureMode` live (the getter below) at
  // every press, the same "decided once, at the press" discipline the shift+drag region above
  // already follows. Escape exits it, matching the mockup (`ui/mockups/map-ui-v1.html`'s `#mode`
  // banner) and every other modal affordance on this surface (the row/selection context menu).
  // T-820 (MAP-20): Measure, Annotate and Pin are ONE tool mode (docs/23 §10.4's table columns), so
  // a bare drag has exactly one meaning at any instant. Their buttons and the banner naming the mode
  // are the floating cluster's (`map-controls.ts`, T-882); this is the state they toggle.
  const setTool = (next: ToolMode) => {
    tool = next;
    stage.dataset.tool = tool;
    if (tool !== "measure") pendingMeasure = null;
    if (tool !== "annotate") pendingAnnotate = null;
    hoverEl.textContent = "";
    renderMeasure();
  };
  const setMeasureMode = (on: boolean) => setTool(on ? "measure" : "navigate");
  document.addEventListener("keydown", (e) => { if (e.key === "Escape" && tool !== "navigate") setTool("navigate"); });

  /** The `view` a measurement is stamped with (docs/25 §10.2): what the pane was showing at the
   * instant of the drag, read from the very frame the stroke landed on.
   *
   * `PaneReport.tier` (`surface/lattice.ts`'s `ViewTier`) is only `"detail"` or `"overview"` — the
   * live chain's own lattice versus the folded spectrum-history pyramid — because both `live-iq` and
   * `spectrum-history` answer from the SAME detail-tier lattice, differing only in *where on the
   * time axis* the pane sits (docs/16 §8's "live is the finest growing edge, not a separate mode").
   * So the three-way honesty tier `MeasurementProvenance` wants is `overview` → `survey-overview`,
   * and otherwise whether this pane is following the live edge right now
   * (`preview.view.panes.isFollowing`, the same call `renderLive` reads). No report yet (nothing
   * drawn) falls to the least detailed claim rather than the most, per the project's fail-closed
   * convention for an honesty tier — a measurement should never overstate detail it cannot show.
   */
  const measureViewOf = (paneId: string): MeasureView | null => {
    const v = paneById(paneId);
    const p = preview;
    if (!v || !p) return null;
    const report = p.lastFrame?.reports.find((r) => r.id === paneId) ?? null;
    const tier: MeasureView["tier"] = report === null
      ? "survey-overview"
      : report.tier === "overview" ? "survey-overview" : p.view.panes.isFollowing(paneId) ? "live-iq" : "spectrum-history";
    return {
      center_hz: (v.box.f0Hz + v.box.f1Hz) / 2,
      span_hz: v.box.f1Hz - v.box.f0Hz,
      t_capture: [v.box.t0Ns / S_TO_NS, v.box.t1Ns / S_TO_NS],
      tier,
      device_id: v.device ?? null,
    };
  };

  /** The plain annotation id of the currently-selected Research row, or `null` — the id
   * `annotationQuads` highlights, so selecting an annotation's row lights its (single) box. */
  const annotationFocusId = (): string | null => {
    const sel = store.get().research.selected;
    return sel !== null && sel.startsWith("annotation:") ? sel.slice("annotation:".length) : null;
  };

  /**
   * Keep a just-saved annotation on the canvas until the next windowed read includes it, AND put it
   * in the Research slice immediately (T-984) — the panel's own poll (`RESEARCH_REFRESH_MS`, 15 s)
   * would otherwise be the only way a fresh annotation reached it, so a panel opened right after
   * authoring showed nothing until that poll landed. `req` is the request just sent: it carries the
   * `view` the server used to stamp `provenance`, which this build never asks back for (the create
   * response is narrowed to `MarkAnnotation`) — the next poll overwrites this with the server's own
   * row regardless, so an approximate provenance here is corrected within one refresh cycle.
   */
  const keepAnnotation = (req: AnnotationRequest) => (a: MarkAnnotation | null) => {
    if (!a) return;
    if (!annotations.some((x) => x.id === a.id)) annotations = [...annotations, a];
    store.set(addResearchAnnotation({
      id: a.id, collection_id: null, kind: a.kind, f_lo_hz: a.f_lo_hz, f_hi_hz: a.f_hi_hz,
      t0_s: a.t0_s, t1_s: a.t1_s, label: a.label, body: null,
      provenance: { tier: req.view.tier, device_id: req.view.device_id ?? null },
    }));
  };
  /** The label spans, pooled per pane and reused frame to frame (set-if-changed, like the HUD's). */
  const labelPools = new Map<string, HTMLElement[]>();
  const placeAnnotationLabels = (pane: PaneView) => {
    const scale = canvas.height > 0 ? canvas.clientHeight / canvas.height : 1;
    const labels = annotationLabels(annotations, pane.box, pane.rect);
    let pool = labelPools.get(pane.id);
    if (!pool) { pool = []; labelPools.set(pane.id, pool); }
    while (pool.length < labels.length) { const e = h("div", { class: "sf-anno-label" }); pool.push(e); annoEl.append(e); }
    pool.forEach((e, i) => {
      const l = labels[i];
      e.hidden = !l;
      if (!l) return;
      setText(e, l.text);
      e.style.transform = `translate(${l.x * scale}px, ${(canvas.height - l.y) * scale}px)`;
    });
    // A pane closed by `closeActive` drops its pool, so its labels do not linger.
    const live = new Set(preview?.lastFrame?.views.map((v) => v.id) ?? []);
    for (const [id, p] of labelPools) if (id !== pane.id && live.size > 0 && !live.has(id)) { p.forEach((e) => e.remove()); labelPools.delete(id); }
  };

  // ---- boot ----
  void (async () => {
    // **Name this page before it asks for its first tile** (T-630): `GET /api/tiles` splits its
    // four in-flight slots between the clients asking for them, and an unnamed page shares the
    // anonymous bucket with every other unnamed caller. See `ui/src/surface/clientid.ts`.
    setTileClientId(newClientId());
    let probe;
    try {
      probe = await probeSurface((path) => client.get(path));
    } catch (e) {
      const why = isBackpressure(e)
        ? "The tile route is busy producing for another viewport. Nothing is wrong with the surface — it will come back."
        : `The surface could not be addressed: ${e instanceof Error ? e.message : String(e)}. GET /api/tiles is what states the view lattice, and a client that guessed one would be addressing a pyramid that does not exist.`;
      say(why);
      markSurface("failed", why);
      return;
    }
    try {
      preview = new SurfacePreview({
        canvas, probe, token: ctx.token, fetchFn: (u, i) => fetch(u, i),
        chrome, minimapPx: MINIMAP_PX,
        // The persistent per-pane retune control (T-476). Strings and a press: `preview.ts` and
        // `view.ts` carry them through without ever naming a tuning, which is what keeps
        // `retune.ts` out of the `/surface.html` preview's import graph.
        chromeAction, onChromeAction: pressRetune,
        widthActions, onWidthAction: pressWidth,
        // T-1028: one line per pane about a retune the mode has pending/in flight, per frame for the
        // same reason `chromeAction` is — it names the window the pane is showing NOW.
        chromeStatus: retuneStatusFor,
        // T-1006: the device pill on each pane's status row — whose coverage decides that pane's
        // grey. Strings and a bit, like the controls above, so `chrome.ts` still learns nothing about
        // `device_id`s and `panedevice.ts` stays out of the `/surface.html` preview's import graph.
        rowDevice: (paneId) => {
          const pane = preview?.view.panes.get(paneId);
          return pane ? devicePill(attached(), pane.device) : null;
        },
        edge: () => edgeNs() || probe.origin.edgeNs,
        // T-893: rows are pushed to the columns a following pane draws, as they are recorded.
        rows: wsRowOpener(ctx.token),
        // T-1042 / LSR-1, behind `?live-ring=1` (`src/flags.ts`): the published spectrum rows every
        // FOLLOWING pane paints its live edge from, read in the render pass. Off, this is `null` and
        // the surface is drawn from tiles exactly as before — one flag, one lane, no second picture.
        liveRing: flags().liveRing ? () => liveRing.frame() : null,
        // T-580: ask the coverage map FIRST, so never-sampled spectrum costs no tile request.
        survey: (path) => client.get(path),
        windows: () => windows,
        // T-806: the pane's visible overlay layers in z order (the ring rules first, so a signal box
        // or selection that crosses one is drawn over it), then the user's own interaction marks.
        // T-807: each pane's coverage-fog layer, a flag on the one cell rule (docs/24 §13.3).
        fog: fogShown,
        marks: (pane, edge) => {
          if (pane.id === preview?.activePane) {
            syncLayerControls();
            const fogHidden = !fogShown(pane.id);
            if (fogEl.hidden !== !fogHidden) fogEl.hidden = !fogHidden;
            if (fogHidden) setText(fogEl, FOG_HIDDEN_TEXT);
            // The readout beside the rules names two lines; when they are not drawn it says so
            // instead of describing strokes that are not on the screen.
            if (!isLayerVisible(layersFor(pane.id), "rules")) {
              setText(ringEl, "Capture rules (retention bound, oldest IQ) are hidden on this pane — Layers menu to show them");
            }
          }
          // Registry overlays below COLLECTION_Z, then research (z 30) and collection (z 40) marks,
          // then registry overlays above it (artifacts, priors), then the user's interaction marks.
          const reg = layersFor(pane.id);
          // T-812: the priors' labels, placed from this very frame's pane box (never on the poll).
          const priorsOn = isLayerVisible(reg, "priors");
          const pa = priorsOn ? priorsByPane.get(pane.id) : undefined;
          priorLayer.update(pane.id, pa ? priorLabels(pane.id, pa.rows, pane.box, pane.rect, canvas.height, window.devicePixelRatio || 1) : []);
          if (pane.id === preview?.activePane) renderPriorsReadout(pane.id);
          const band = (keep: (z: number) => boolean) => ({ ...reg, layers: reg.layers.filter((l) => keep(l.z)) });
          placeAnnotationLabels(pane);
          if (pane.id === preview?.activePane) {
            // T-984: how many of this frame's two annotation-drawing paths actually drew each
            // loaded annotation — read back by `app-annotate.e2e.mjs` so it can assert "drawn once"
            // from the page itself rather than re-deriving the fix's internal split. `dashed` is
            // `annotationQuads` (T-820, always on); `researchBox` is `researchBoxesFor` including it
            // as a second "research-box" (the T-984 defect, whichever research/collection layer was
            // on). A correct build's `researchBox` is always 0: annotations own exactly one path.
            // `researchBoxesFor` caches its own work keyed on the research slice's identity, so
            // reading it a second time here costs nothing extra most frames.
            const research = researchBoxesFor(pane);
            stage.dataset.annotationDraws = JSON.stringify(annotations.map((a) => ({
              id: a.id, label: a.label, dashed: 1, researchBox: research.some((b) => b.id === `annotation:${a.id}`) ? 1 : 0,
            })));
          }
          return [
            ...composeOverlays(band((z) => z < COLLECTION_Z), overlayFns, pane, edge),
            // T-820: the human-authored annotations, always on and DASHED (never a claim about the
            // air) — the SOLE place any annotation draws (T-984: `researchBoxesFor` excludes them,
            // so a filed or unfiled annotation is never also a second "research-box"). Highlighted
            // like a selected research row when its own Research-panel row is selected.
            ...annotationQuads(annotations, annotationFocusId(), pane.box, pane.rect),
            ...markQuads(researchBoxesFor(pane), edge, pane.box, pane.rect),
            ...composeOverlays(band((z) => z > COLLECTION_Z), overlayFns, pane, edge),
            ...markQuads(boxesFor(pane), edge, pane.box, pane.rect),
          ];
        },
        trace: traceFor, tracePx: traceOn ? TRACE_PX : 0,
        // The HUD rulers fade with the floating chrome: `chrome-idle` on <body> is the one idle
        // signal (docs/23 §10.2), and the labels' CSS reads the same class.
        hud: hudEl, hudAlpha: () => (document.body.classList.contains("chrome-idle") ? HUD_IDLE_ALPHA : 1),
        hudReserve: () => chromeReserve(canvas, stage.querySelector<HTMLElement>(".map-ctl")),
        dom: (panes, edge, hPx, dpr) => {
          pinsFrame(panes, edge, hPx, dpr);
          placeActive(panes, hPx, dpr);
          // T-1001: each pane's Live button, from THIS frame's rectangles — the same pass as the
          // data and the outline, never a poll (docs/16 §8's one shared mapping).
          liveButtons?.update(panes, hPx, dpr, chromeBoxes);
          // T-1005: the split's dividers and each pane's ×, from the same frame.
          split.place(panes, hPx, dpr);
        },
      });
    } catch (e) {
      const why = `WebGL2 is unavailable in this browser: ${e instanceof Error ? e.message : String(e)}`;
      say(why);
      markSurface("failed", why);
      return;
    }
    say(probe.note);
    // T-946(b): coverage grows while the view is open; the sentence follows the backend's census.
    startPoll(async () => {
      try { const t = await refreshOrientationNote((path) => client.get(path), probe); if (t !== note.textContent) say(t); } catch { /* keep the last sentence */ }
    }, 10_000);
    markSurface("mounted");

    // The shadow's brightness is a per-viewer display preference (T-526): loaded once here, never
    // fetched, and changed only by the Ctrl+Shift+wheel gesture `input.ts` claims before any zoom.
    preview.view.surface.setShadowGain(loadShadowGain());

    // T-506: the time extent always reaches the retained capture window (T-338's span), and further
    // back only where spectrum history exists. `timeExtent` is the ring's window and nothing else;
    // the floor only ever moves older, so a pane is never yanked by a poll.
    store.select((s) => s.captureWindow, (w) => {
      const ext = timeExtent(w);
      if (ext && preview) preview.extendTimeFloor(ext.lo * S_TO_NS);
    }, { immediate: true });

    // T-1008: the scan plan's region edges are dragged on the map, through the ONE input handler's
    // `grabHandle` — offered only where an edge of an editable (idle) plan is under the press in a
    // pane that shows the `scan` layer, so every other press keeps its meaning. The drag only moves
    // the region; the server re-prices it on release, and nothing reaches a device route (Start
    // does, on its own press).
    const scanEdgeUnder = (x: number, y: number) => {
      const hit = paneUnder(x, y);
      if (!hit || !isLayerVisible(layersFor(hit.view.id), "scan")) return null;
      const edge = scanCtl.edgeAt(hit.view.box, hit.view.rect, x, window.devicePixelRatio || 1);
      return edge ? { edge, pane: hit.view.id } : null;
    };
    const grabScanEdge = (p: GlPoint, paneId: string) => {
      const grab = scanEdgeUnder(p.x, p.y);
      if (!grab || grab.pane !== paneId) return null;
      return {
        move: (q: GlPoint) => {
          const v = paneById(grab.pane);
          if (!v) return;
          const x = Math.min(Math.max(q.x, v.rect.x), v.rect.x + v.rect.w);
          scanCtl.dragTo(grab.edge, pointOn(v.box, v.rect, x, v.rect.y).fHz);
        },
        end: () => scanCtl.endDrag(),
      };
    };

    detach = attachSurfaceInput(canvas, preview, {
      onShadowGain: shadowGainWheelHandler(preview.view.surface),
      onView: () => { mirror(); viewMoved(); },
      grabHandle: grabScanEdge,
      // T-1028: the only wire from a gesture to the front end, and it is inert while the mode is
      // off — `moved`/`settled` return immediately then, so with the mode off this handler is the
      // old rule byte for byte. A pinch and a drag report `ended` at their release (T-486's commit
      // point); a wheel has none and is settled by stillness inside the controller.
      onGesture: ({ pane, ended }) => { if (ended) retuneMode.settled(pane); else retuneMode.moved(pane); },
      onHover: (p, e) => {
        // A pin under the pointer wins the MapTip; the quadtree is the hit test (docs/24 §14.4).
        hoveredPin = p ? pinLayer.pick(cssPoint(e).x, cssPoint(e).y) : null;
        canvas.style.cursor = hoveredPin ? "pointer" : p && scanEdgeUnder(p.x, p.y) ? "ew-resize" : "";
        if (!p) { hoverEl.textContent = ""; return; }
        const hit = hitAt(p.x, p.y);
        const markLabel = hit?.mark
          ? hit.mark.kind === "signal-box" ? "signal"
            : hit.mark.kind === "measurement-box" ? "measurement"
              : hit.mark.kind === "research-box" ? "mark"
              : hit.mark.kind === "pending-region" ? null : "selection"
          : null;
        const anno = hit ? annotationAt(annotations, hit.pane.box, hit.pane.rect, p) : null;
        const annoLabel = anno ? ` · annotation (${anno.kind}): ${anno.label}` : "";
        hoverEl.textContent = hit ? `${fmtHz(hit.fHz)} · ${at(hit.tNs)}${markLabel ? ` · ${markLabel} ${hit.mark!.id.slice(0, 8)}` : ""}${annoLabel}` : "";
      },
      onClick: (p, e) => {
        const c = cssPoint(e);
        const pin = pinLayer.pick(c.x, c.y);
        if (pin) { selectPin(pin); return; }
        const hit = hitAt(p.x, p.y);
        if (!hit) return;
        if (hit.mark?.kind === "signal-box") { store.set(focusSignal(hit.mark.id)); return; }
        if (hit.mark?.kind === "selection-box") { store.set(focusSelection(hit.mark.id)); return; }
        if (hit.mark?.kind === "research-box") {
          // T-821: a collection mark selects its row in the Research panel (opened to show it).
          store.set(selectResearch(hit.mark.id));
          store.set(setResearchOpen(true));
          return;
        }
        // T-984: an annotation is drawn once, by `annotationQuads`, never as a `research-box` (see
        // `researchBoxesFor`) — its own hit test still selects its Research row, the same behaviour
        // a marker's box gets ("a mark clicked on the canvas selects its row", `research.ts`).
        const anno = annotationAt(annotations, hit.pane.box, hit.pane.rect, p);
        if (anno) { store.set(selectResearch(rowKey("annotation", anno.id))); store.set(setResearchOpen(true)); }
        // A measurement box has no focus target yet (T-821's collections panel is where a click
        // through to it belongs); a click on one does nothing rather than mis-focusing a selection.
      },
      onRegionDrag: (r) => {
        const region = r ? regionOf(r) : null;
        pending = r && region ? { pane: r.pane, region } : null;
      },
      onRegion: (r) => {
        pending = null;
        const region = regionOf(r);
        if (region) commitRegion(ctx, region, fmtHz);
      },
      get measureMode() { return tool === "measure"; },
      get annotateMode() { return tool === "annotate" || tool === "pin" ? tool : null; },
      onAnnotateDrag: (r) => {
        const region = r ? regionOf(r) : null;
        pendingAnnotate = r && region ? { pane: r.pane, region } : null;
      },
      onAnnotateBox: (r) => {
        pendingAnnotate = null;
        const region = regionOf(r);
        const view = region ? measureViewOf(r.pane) : null;
        if (!region || !view) return;
        const label = normLabel(window.prompt("Label for this annotation box:", ""));
        const body = label ? boxRequest(region, label, view) : null;
        if (body) void commitAnnotation(ctx, body).then(keepAnnotation(body));
      },
      onAnnotatePoint: (p, kind) => {
        const v = paneById(p.pane);
        const view = v ? measureViewOf(p.pane) : null;
        if (!v || !view) return;
        const q = clampToRect(v.rect, p.at);
        const label = normLabel(window.prompt(kind === "marker" ? "Label for this marker:" : "Text note:", kind === "marker" ? "marker" : ""));
        if (label) {
          const body = pointRequest(kind, pointOn(v.box, v.rect, q.x, q.y), label, view);
          void commitAnnotation(ctx, body).then(keepAnnotation(body));
        }
      },
      onMeasureDrag: (r) => {
        const region = r ? regionOf(r) : null;
        pendingMeasure = r && region ? { pane: r.pane, region } : null;
        if (region) hoverEl.textContent = fmtMeasureReadout(measureReadout(region));
      },
      onMeasure: (r) => {
        pendingMeasure = null;
        const region = regionOf(r);
        const view = region ? measureViewOf(r.pane) : null;
        if (region && view) {
          void commitMeasurement(ctx, region, view, (rg) => fmtMeasureReadout(measureReadout(rg))).then((saved) => {
            if (saved.length > 0) {
              measurements = [...measurements, {
                id: saved[0].id, f_lo_hz: region.f0Hz, f_hi_hz: region.f1Hz,
                t0_s: region.t0Ns / S_TO_NS, t1_s: region.t1Ns / S_TO_NS,
              }];
            }
          });
        }
        setMeasureMode(false);
      },
      onContext: (p, e) => {
        const s = store.get();
        // T-994: a feature on the map — a detection's box or its generalized symbol — is hit the way
        // a click hits it (the pin layer's polygon/quadtree pick), so right-click / long-press on ANY
        // box opens its menu. (The detections are the `detections` LAYER since T-806, so `hitAt`'s
        // marks — the user's own selections and measurements — no longer contain them.)
        const c = cssPoint(e);
        const pin = pinLayer.pick(c.x, c.y);
        if (pin && pin.pin.source === "detection") {
          const row = paneRows(s.inventory, pin.paneId)[pin.pin.id];
          if (row) { openSignalMenu(ctx, row, e.clientX, e.clientY); return; }
        }
        const hit = hitAt(p.x, p.y);
        if (!hit?.mark) return;
        if (hit.mark.kind === "signal-box") {
          // T-1002: the row as THIS pane knows it — the box was drawn from that pane's answer.
          const row = paneRows(s.inventory, hit.pane.id)[hit.mark.id];
          if (row) openSignalMenu(ctx, row, e.clientX, e.clientY);
        } else if (hit.mark.kind === "selection-box") {
          const sel = s.selections.list.find((x) => x.id === hit.mark!.id);
          if (sel) openSelectionMenu(ctx, sel, e.clientX, e.clientY);
        }
      },
    });

    // ---- the floating control cluster (T-802 / MAP-02), docked over the canvas's edges ----
    // Go-to, the layers button and zoom (T-1001: follow/freeze is each pane's own button, below).
    // All view arithmetic on the active
    // pane through `paneActions` (the code `ui/test/app-map-controls.test.ts` drives against a
    // fetch spy); the only press that can reach the radio is the Go-to's retune OFFER, which goes
    // through `pressOffer` above — the same gate as the pane row's Retune.
    const pv = preview;
    const acts = paneActions(pv.view.panes, () => pv.activePane);
    // T-1001: follow/freeze is PER PANE — every method below is told which pane it acts on, so a
    // press on pane 1's button cannot reach pane 2. Nothing here is a device call.
    // T-995: the minimap is retired, so a press has no second viewport to bring with it.
    // T-955: the states are relative to the TUNED window's live edge, and a press from anywhere
    // else brings the pane there (frequency too, only if it does not overlap) — the same
    // `frequency.current` the retune-offer span already reads (`goToSpanHz`), never a device call.
    // T-1006: **the PRESSED pane's own front end's window**, not a run-wide one. With two radios
    // `frequency.current` is the primary's tuned state (docs/api.md: the `frequency` block is one
    // device's, which `windows` is the enumeration of), so a pane pinned to the second radio was
    // being brought to the FIRST radio's live edge — the follow-live control moving a pane onto a
    // window its own device never looked through. A pane naming a device reads that device's entry
    // in `windows`; a pane on `any` keeps the run-wide answer, which is what `any` means. The pane
    // is the one whose button was pressed (T-1001), never the active one.
    const liveActs = paneLiveActions(pv.view.panes, (id) => {
      const pane = pv.view.panes.get(id);
      if (pane && pane.device !== ANY_DEVICE) {
        const w = windows.find((x) => x.deviceId === pane.device);
        return w ? { centerHz: w.centerHz, spanHz: w.spanHz } : null;
      }
      const cur = store.get().navGrid.grid?.frequency?.current;
      return cur ? { centerHz: cur.center_hz, spanHz: cur.span_hz } : null;
    });
    liveButtons = new PaneLiveLayer(liveEl, {
      state: (id) => liveActs.state(id),
      press: (id) => {
        liveActs.press(id);
        // A follow-live press can move the pane's frequency too, so a painted Go-to offer now
        // describes a window the pane has left — withdrawn, exactly as a zoom withdraws it.
        viewMoved();
        lastMirror = "";
        mirror();
        renderLive();
      },
      paneNumber: (id) => activePaneName(pv.view.panes.list().map((x) => x.id), id)?.n ?? null,
    });
    // The cluster's boxes, in the canvas's own CSS pixels: a pane at the top of the canvas puts its
    // Live button below whatever is over that corner. Measured when the layout changes (`fit`),
    // never per frame — `getBoundingClientRect` on every child of the cluster is a layout read.
    const measureChrome = () => {
      const ctl = stage.querySelector<HTMLElement>(".map-ctl");
      const base = canvas.getBoundingClientRect();
      chromeBoxes = ctl
        ? Array.from(ctl.children).map((c) => {
          const r = c.getBoundingClientRect();
          return {
            left: r.left - base.left, right: r.right - base.left,
            top: r.top - base.top, bottom: r.bottom - base.top,
            width: r.width, height: r.height,
          };
        })
        : [];
    };
    // A read-only statement of the overlay layers this build draws, each as its REGISTRY def
    // (plane, z, default) — never the menu's rendering of them — so a check can derive what the
    // layers menu must offer from the registry itself rather than a literal every new renderer
    // breaks. Each collection is a layer too (z COLLECTION_Z, default its stored `visible`), so the
    // statement is re-written whenever the collections change, exactly when the menu re-states.
    // Presentation metadata only: commands nothing.
    const stateOverlays = () => {
      stage.dataset.overlayLayers = JSON.stringify([
        ...[...drawnLayers].map((id) => layerDef(id)!)
          .map((d) => ({ id: d.id, plane: d.plane, z: d.z, visibleByDefault: d.visibleByDefault })),
        ...store.get().research.collections.map((c) => ({ id: collectionLayer(c.id), plane: "overlay", z: COLLECTION_Z, visibleByDefault: c.visible })),
      ]);
    };
    stateOverlays();
    const host: MapControlHost = {
      ...acts,
      // T-882: the retired toolbar row's controls, rehomed into the cluster. All view state.
      measuring: () => tool === "measure",
      setMeasuring: (on) => setMeasureMode(on),
      // T-1028: retune mode's chip. A mode, like Measure — but the one whose state decides whether
      // a settled gesture reaches the device, which is why it is stated twice (the lit chip and the
      // banner) and why the held key is marked apart from the latch.
      retuneMode: () => ({ on: retuneMode.on, held: retuneMode.isHeld }),
      setRetuneMode: (on) => retuneMode.setSticky(on),
      // T-820 (MAP-20): the Annotate and Pin tool modes, beside Measure in the cluster.
      annotating: () => (tool === "annotate" || tool === "pin" ? tool : null),
      setAnnotating: (mode) => setTool(mode ?? "navigate"),
      split: (dir) => splitActive(dir),
      closePane: () => closeActive(),
      splitDir: () => pv.view.panes.splitDirOf(pv.activePane),
      flipSplit: () => {
        const d = pv.view.panes.splitDirOf(pv.activePane);
        if (d) pv.view.panes.setSplitDir(pv.activePane, d === "rows" ? "columns" : "rows");
      },
      wholeSurface: () => { pv.fitToSurface(); lastMirror = ""; mirror(); renderLive(); },
      paneCount: () => pv.view.panes.list().length,
      paneMenuExtras: [recordBtn],
      // T-1006: the front-end picker for the active pane, and the "one viewport per front end"
      // split. Every word comes from `surface/panedevice.ts`; every press is a view change.
      deviceMenu: () => {
        const pane = pv.view.panes.get(pv.activePane);
        const dev = pane?.device ?? ANY_DEVICE;
        const list = attached();
        return {
          pane: paneMenuName(),
          rows: deviceRows(list, dev).map((r) => ({ id: r.id, label: r.label, hint: r.hint, on: r.on })),
          note: devicePill(list, dev).why,
          offer: splitPerDeviceOffer(list),
        };
      },
      setPaneDevice: (id) => {
        pv.view.panes.setDevice(pv.activePane, id);
        // The grey, the retune offer and the trace are all functions of the pane's device, so the
        // frame has to be re-derived — the same reason a layer toggle renders. No route is touched.
        lastMirror = "";
        mirror();
        renderLive();
      },
      splitPerDevice: () => {
        const offer = splitPerDeviceOffer(attached());
        if (!offer.enabled) return;
        // One pane per front end, the first being the pane already open. `splitActive` splits the
        // ACTIVE pane, so each new pane inherits the layers of the one before it (T-1000's rule) and
        // is then pinned; a run with more radios than the layout can hold simply stops when `split`
        // declines to add one, which is the pane model's own bound, not a second policy here.
        pv.view.panes.setDevice(pv.activePane, offer.devices[0]);
        for (const id of offer.devices.slice(1)) {
          const before = pv.activePane;
          splitActive();
          if (pv.activePane === before) break; // the layout took no more panes
          pv.view.panes.setDevice(pv.activePane, id);
        }
        lastMirror = "";
        mirror();
        renderLive();
      },
      goTo: (hz) => store.set(requestGoto(hz)),
      centreHz: () => store.get().device.centerHz,
      gotoOffer: () => {
        // T-947: the viewport-covered check still reads the VIEW's own box (what is on screen right
        // now), but the plan offered is `paneWidthOffer`'s — a NAMED span at the pane's centre, never
        // the viewport's width. See the block above `pressGotoOffer` for why.
        const o = offerNow(pv.activePane);
        // Only when the pane now shows spectrum no tuned window covers: inside one, panning already
        // reaches it and the pane row's persistent control is where a finer capture is offered.
        // T-955: a Go-to names a CENTRE (T-947), so "covered" is whether the tuned window holds the
        // pane's centre — not whether it holds the whole viewport, which a pane zoomed out past the
        // capture never is. Checked against `frequency.current` as well as the active windows, so a
        // retune by anyone withdraws the offer the moment the navigation poll reports it (the
        // explorer's 0428: "Retune to 162.2000 MHz" still painted with the radio at 162.2, then 144.6).
        const pane = pv.view.panes.get(pv.activePane);
        const cur = store.get().navGrid.grid?.frequency?.current ?? null;
        const c = pane?.freq.centerHz ?? NaN;
        const heldNow = !!pane && (coveringWindow(windows, c, c, pane.device) !== null
          || (!!cur && Math.abs(c - cur.center_hz) <= cur.span_hz / 2));
        if (!o || heldNow) { lastPaintedGoto = null; return null; }
        const wo = widthOfferNow(pv.activePane, gotoSpanHz());
        if (!wo) { lastPaintedGoto = null; return null; }
        lastPaintedGoto = wo;
        // T-1004: a frozen pane's offer is disabled as "past" — true of a retune, and a dead end for
        // the user who just typed a frequency into it. The same offer is takeable as a GO-LIVE, so it
        // is offered as one, named as one, and takes both acts in one press.
        return isGoLiveOffer(wo)
          ? { label: GO_LIVE_LABEL, why: goLiveOfferLabel(wo), enabled: true, press: pressGotoOffer }
          : { why: widthOfferLabel(wo), enabled: widthOfferAcceptable(wo), press: pressGotoOffer };
      },
      layerMenu: (): LayerMenu => {
        const reg = layersFor(pv.activePane);
        const ids = pv.view.panes.list().map((x) => x.id);
        const n = ids.indexOf(pv.activePane) + 1;
        return {
          pane: ids.length > 1 ? `pane ${n} of ${ids.length}` : "this pane",
          bases: BASE_STYLES.map((b) => ({ id: b.id, label: b.label, hint: b.hint, on: reg.base === b.id })),
          data: paintOrder(reg).filter((l) => l.plane === "data" && dataLayers.has(l.id)).map((l) => {
            const d = layerDef(l.id)!;
            return { id: l.id, label: d.label, hint: d.hint, on: l.visible, key: l.id === "coverage" ? fogKeyEntries() : undefined };
          }),
          // T-809: the `dom`-plane pins are an overlay-content toggle too (docs/24 §4's second axis).
          overlays: paintOrder(reg).filter((l) => (l.plane === "overlay" || l.plane === "dom") && drawnLayers.has(l.id)).map((l) => {
            const d = layerDef(l.id)!;
            // T-813: the detections layer's key — same symbology, quoted from `marks.ts`, `markKeyEntries` draws.
            // T-813: the detections key is `marks.ts`'s own symbology. T-898: the retune layer's
            // key is one row per front end, so the route on screen is labelled by its radio.
            const key = l.id === "detections"
              ? markKeyEntries()
              : l.id === "tune"
                ? tuneKeyEntries(tunePaths).map((e) => ({ key: e.key, label: e.label, note: e.note, pixel: () => e.rgb }))
                : l.id === "frontend"
                  ? frontEndKeyEntries(frontEndEvents).map((e) => ({ key: e.key, label: e.label, note: e.note, pixel: () => e.rgb }))
                  : undefined;
            return { plane: l.plane, z: l.z, row: { id: l.id, label: d.label, hint: d.hint, on: l.visible, key } };
          }).concat(store.get().research.collections.map((c) => ({ plane: "overlay" as const, z: COLLECTION_Z, row: {
            id: collectionLayer(c.id), label: c.name, hint: c.reserved ? "collection · bookmarks" : "my collection",
            on: collectionVisibleOn(reg, c),
            key: undefined,
          } }))).sort((a, b) => PLANE_ORDER.indexOf(a.plane) - PLANE_ORDER.indexOf(b.plane) || a.z - b.z).map((x) => x.row),
          viewWide: [{ id: "trace", label: "Spectrum trace", hint: "over every pane's top rows", on: traceOn }],
          scale: { rows: scaleRows(pv.range.mode), note: rangeLabel(pv.range) },
        };
      },
      setBase: (id) => {
        const b = BASE_STYLES.find((x) => x.id === id);
        if (b) editLayers(setPaneBase(pv.activePane, b.id, seedFor(pv.activePane)), pv.activePane);
      },
      toggleOverlay: (id) => {
        const lid = id as LayerId;
        const coll = lid.startsWith("collection:") ? store.get().research.collections.find((c) => collectionLayer(c.id) === lid) : undefined;
        if (!drawnLayers.has(lid) && !dataLayers.has(lid) && !coll) return;
        const reg = layersFor(pv.activePane);
        const on = coll ? !collectionVisibleOn(reg, coll) : !isLayerVisible(reg, lid);
        if (lid === "detections") setSignals(pv.activePane, on);
        else editLayers(setPaneLayer(pv.activePane, lid, on, seedFor(pv.activePane)), pv.activePane);
      },
      toggleViewWide: (id) => { if (id === "trace") setTrace(!traceOn); },
      setScale: (id) => { const m = scaleMode(id); if (m) setMode(m); },
      viewChanged: () => { lastMirror = ""; mirror(); renderLive(); },
      toast: (text) => store.set(toast(text)),
      research: {
        isOpen: () => store.get().research.open,
        toggle: () => store.set(setResearchOpen(!store.get().research.open)),
      },
      scan: { button: scanCtl.button, panel: scanCtl.panel },
      // T-1000: the per-pane chrome's name for the pane it acts on — the outline's own words.
      activeName: () => activePaneName(pv.view.panes.list().map((x) => x.id), pv.activePane),
    };
    const setMode = (mode: RangeMode) => {
      preview?.setRangeMode(mode);
      saveRangeMode(mode);
      renderRange();
    };
    const controls = mountMapControls(host);
    store.select((s) => s.research.open, () => controls.syncResearch());
    // A collection created, renamed or deleted in the panel re-states an open layers menu.
    store.select((s) => s.research.collections, () => { stateOverlays(); controls.syncLayers(); });
    stage.append(controls.el);
    // T-1001: "the follow state may have changed" is now each pane's own button re-stating itself.
    renderFollow = () => liveButtons?.sync();
    measureChrome();
    renderLayers = controls.syncLayers;
    renderMeasure = controls.syncMeasure;
    viewMoved = controls.viewMoved;
    // T-1028: the chip and its banner, re-stated whenever the mode changes or a retune becomes
    // pending. Wired here rather than passed in, because the controller exists before the cluster.
    syncRetuneChip = controls.syncRetuneMode;
    syncRetuneChip();
    // `R` held is the mode for one gesture; `R` tapped latches it. Registered on `document` like the
    // pane keys beside them, and refused for a key going into a text field or carrying a modifier.
    document.addEventListener("keydown", onRetuneKeyDown);
    document.addEventListener("keyup", onRetuneKeyUp);
    // A window that loses focus with `R` down never delivers the keyup, and the momentary mode would
    // stay on until `R` was pressed again — a mode that tunes the radio, left on by alt-tabbing away
    // from it. Dropping the hold is not the same act as a release (see `releaseHeld`): it must not
    // latch the mode, which is what a tap does.
    window.addEventListener("blur", () => retuneMode.releaseHeld());
    document.addEventListener("visibilitychange", () => { if (document.hidden) retuneMode.releaseHeld(); });
    // T-955: a retune (by anyone — this page, another client, the API) re-derives the painted Go-to
    // offer and the FAB's tuned-live-edge state against the tuned window the backend now reports.
    store.select((s) => {
      const c = s.navGrid.grid?.frequency?.current;
      return c ? `${c.center_hz}/${c.span_hz}` : "";
    }, () => { controls.tuningChanged(); liveButtons?.sync(); });

    // ---- the active pane, made visible (T-1000, docs/23 §10.7) ----
    // Whatever made a pane active — a press, a right-click, a wheel, a split, a close, a key — the
    // outline and every piece of chrome that names it move in the SAME call, not on the next frame
    // or poll. A Go-to offer on screen described the previous pane's window, so it is withdrawn.
    // Presentation only: changing the active pane moves no view and reaches no route.
    const activeChanged = () => {
      const f = pv.lastFrame;
      const dpr = canvas.clientWidth > 0 ? canvas.width / canvas.clientWidth : window.devicePixelRatio || 1;
      // The last frame's PANE rectangles (its `views` also carry the map strip's, which is not a
      // pane), when that frame drew the panes there are now; after a split or a close it did not, and
      // the very next frame (the `dom` hook) places the outline instead.
      const ids = new Set(pv.view.panes.list().map((x) => x.id));
      const drawn = f ? f.views.filter((v) => ids.has(v.id)) : [];
      if (drawn.length === ids.size && ids.has(pv.activePane)) placeActive(drawn, canvas.height, dpr);
      controls.syncActive();
      syncLayerControls();
      renderFollow();
      viewMoved();
      // T-1002: the lists follow the active pane, so they are re-scoped in the SAME dispatch as the
      // outline — a press on pane 1 shows pane 1's Candidates, named, not pane 2's for a poll.
      publishPanes();
    };
    pv.onActiveChange(activeChanged);
    // The pane keys: `[` / `]` step through the panes, `1`-`9` pick one, `L` toggles Live on the
    // active pane through the FAB's own press. `paneKeyIntent` refuses typing, modifiers and repeats.
    document.addEventListener("keydown", (e) => {
      const k = paneKeyIntent(e);
      if (!k) return;
      // `L` presses the ACTIVE pane's own Live button — the very element the user sees, so the key
      // and the button cannot do different things (T-1000's rule, kept with the per-pane control).
      if (k.kind === "live") { e.preventDefault(); liveButtons?.buttonFor(pv.activePane)?.click(); return; }
      const ids = pv.view.panes.list().map((x) => x.id);
      const next = k.kind === "step" ? stepPane(ids, pv.activePane, k.step) : ids[k.n - 1] ?? null;
      if (!next) return;
      e.preventDefault();
      pv.activePane = next;
    });

    const topBar = document.querySelector<HTMLElement>(".app > .bar");
    const fit = () => {
      const r = stage.getBoundingClientRect();
      const dpr = window.devicePixelRatio || 1;
      preview?.resize(r.width, r.height, dpr);
      // T-1001: the chrome a pane's Live button clears is measured, so it is re-measured whenever
      // the layout changes — here, not on the frame.
      measureChrome();
      // T-918: the canvas runs under the floating chrome at the bottom (full-bleed, docs/23 §10.1),
      // so the map strip is lifted clear of it — layout arithmetic over two measured boxes, as below.
      // T-994: the dock bar is retired; what can sit there now is the Active-outputs strip, and only
      // while an output is open (hidden — height 0, nothing to clear — otherwise).
      const dock = document.querySelector<HTMLElement>(".app > .out-strip");
      const dr = dock?.getBoundingClientRect();
      const dockUnder = dr && dr.height > 0 ? Math.max(0, Math.ceil(r.bottom - dr.top)) : 0;
      // T-933: the sheet's peek strip (`chrome/sheet.css`) floats ABOVE the dock even collapsed —
      // it is never hidden (T-803's rule) — and the panes' bottom edge spans the WHOLE canvas width,
      // so it always shares an x-range with the sheet: the panes must clear the peek strip too, not
      // just the dock. (This was the minimap strip's clearance until T-995 retired the minimap.)
      //
      // Anchored off the sheet's BOTTOM edge, never its live top or height: `sheet.css` pins
      // `bottom` (`--sheet-bottom`) and only the top edge moves as the sheet's height changes — a
      // drag toward full (`chrome/sheet.ts`'s pointermove sets `style.height` with `snap` still
      // "peek" until release) or the half/full <-> peek snap transition (`sheet.css`'s .28 s
      // height transition). Reading the live top/height, as an earlier version of this fix did,
      // made the bottom edge — and so every pane — follow the sheet up and down
      // on every drag and close (review finding on this ticket). The peek clearance itself is a
      // CONSTANT (`PEEK_PX`, `chrome/sheet.ts`), so this fixed-position rule (docs/23 §10.6 P3)
      // applies whether or not the sheet is currently at peek — it does not need `dataset.snap`.
      const sheet = document.querySelector<HTMLElement>(".sheet");
      const sr = sheet?.getBoundingClientRect();
      const sheetUnder = sr && sr.height > 0 ? Math.max(0, Math.ceil(r.bottom - (sr.bottom - PEEK_PX))) : 0;
      const under = Math.max(dockUnder, sheetUnder);
      const lift = under > 0 ? under + 8 : 0;
      stage.style.setProperty("--chrome-bottom", `${under}px`);
      // The FAB and the readouts dock above the panes' bottom inset, in CSS px (`--map-strip` keeps
      // its name; with the minimap retired, T-995, it is the lift alone).
      stage.style.setProperty("--map-strip", `${MINIMAP_PX / dpr + lift}px`);
      // T-882: how far the app's floating top bar reaches down over the stage (it wraps to several
      // rows on a narrow window — ~120 px at 420 px), so the cluster's top row starts below it
      // rather than under it. Layout arithmetic over two measured boxes; 0 where they do not meet.
      const over = topBar ? Math.max(0, Math.ceil(topBar.getBoundingClientRect().bottom - r.top)) : 0;
      stage.style.setProperty("--chrome-top", `${over}px`);
      // T-918: the panes' content stops short of both full-width bars (never of a closeable overlay).
      // Stated on the canvas (CSS px) so a test indexing pixels by pane reads the layout, not a copy.
      const top = over > 0 ? over + 8 : 0;
      canvas.dataset.insetTop = String(top);
      canvas.dataset.insetBottom = String(lift);
      if (preview) {
        preview.view.insetTopPx = Math.round(top * dpr);
        preview.view.insetBottomPx = Math.round(lift * dpr);
      }
    };
    fit();
    const ro = typeof ResizeObserver === "function" ? new ResizeObserver(fit) : null;
    ro?.observe(stage);
    if (topBar) ro?.observe(topBar);
    // T-933: `fit`'s sheet clearance is anchored to the sheet's fixed bottom edge (never its live
    // height, see above), so this observer is not about tracking drag/snap changes — it exists so
    // that a sheet mounted AFTER this first `fit()` call (the sheet is a separate area mount, T-803)
    // is still picked up once it appears, rather than the panes staying un-lifted until the next
    // stage resize.
    const sheetEl = document.querySelector<HTMLElement>(".sheet");
    if (sheetEl) ro?.observe(sheetEl);
    // T-994: the Active-outputs strip appears and disappears with the outputs; its box changing size
    // (0 while hidden) re-fits the map strip's clearance.
    const outStrip = document.querySelector<HTMLElement>(".app > .out-strip");
    if (outStrip) ro?.observe(outStrip);
    window.addEventListener("resize", fit);
    preview.start();

    // Once a second: the app's one (time, frequency) window has to track the active viewport so the
    // inventory lists stay scoped to it. The retune control is NOT on this cadence any more — it is
    // re-derived per frame through `chromeAction`, because its sentence names the window the pane is
    // showing *now* (T-476).
    startPoll(async () => { mirror(); refreshPriors(); refreshDensity(); }, DENSITY_POLL_MS);

    // T-897: the `paths` layer's records, for every pane that shows the layer — one read over the
    // union of their boxes (`pathsRequest`, asserted in `ui/test/surface-paths.test.ts`). A pane
    // with the layer off costs nothing; with it off everywhere there is no request at all. Read
    // only: a path is a view over stored detections and reaches no device.
    startPoll(async () => {
      const url = pathsRequest(pv.view.panes.list()
        .filter((x) => isLayerVisible(layersFor(x.id), "paths"))
        .map((x) => boxOf(x, pv.view.panes.lastEdgeNs)));
      if (!url) { paths = []; return; }
      const body = await client.get<unknown>(url).catch(() => null);
      if (body) paths = parsePaths(body);
    }, 2000);

    // T-898: the `tune` layer's records — the device's own retune route — on the same terms: one
    // read over the union of the boxes of the panes showing the layer, nothing when none does.
    startPoll(async () => {
      const url = tuneHistoryRequest(pv.view.panes.list()
        .filter((x) => isLayerVisible(layersFor(x.id), "tune"))
        .map((x) => boxOf(x, pv.view.panes.lastEdgeNs)));
      if (!url) { tunePaths = []; return; }
      const body = await client.get<unknown>(url).catch(() => null);
      if (body) tunePaths = parseTuneHistory(body);
    }, 2000);

    // T-981: the `frontend` layer's records — front-end events — on the same terms: one read over
    // the union of the time spans of the panes showing the layer, nothing when none does.
    startPoll(async () => {
      const url = frontEndRequest(pv.view.panes.list()
        .filter((x) => isLayerVisible(layersFor(x.id), "frontend"))
        .map((x) => boxOf(x, pv.view.panes.lastEdgeNs)));
      if (!url) { frontEndEvents = []; return; }
      const body = await client.get<unknown>(url).catch(() => null);
      if (body) frontEndEvents = parseFrontEndEvents(body);
    }, 2000);

    // T-820 / MAP-20: the annotations in view, read back from the store — which is what makes one
    // drawn before a reload visible after it. The window is the union of the panes' boxes as last
    // drawn; a GET is a read of research state and never reaches a device route.
    startPoll(async () => {
      const views = preview?.lastFrame?.views ?? [];
      if (views.length === 0) return;
      const w = {
        f0Hz: Math.min(...views.map((v) => v.box.f0Hz)), f1Hz: Math.max(...views.map((v) => v.box.f1Hz)),
        t0S: Math.min(...views.map((v) => v.box.t0Ns)) / S_TO_NS, t1S: Math.max(...views.map((v) => v.box.t1Ns)) / S_TO_NS,
      };
      annotations = await fetchAnnotations(ctx, w);
    }, 2000);

    // ---- Go to / bookmarks: a frequency request moves the viewport (T-152's `nav.gotoHz`) ----
    // T-906: keyed on the whole request (its `seq`), so a second Go to the same centre with a
    // different span still moves the pane; a request that names a span restores it (snapped by the
    // pane model), view arithmetic only.
    //
    // T-999: the SAME request can also name a time window (`gotoTimeWindow`) — a past survey's
    // capture time, from the drawer's "go to". Applied to the active pane exactly as `setWindow`
    // (`surface/preview.ts`) does it elsewhere: span first via `zoomTime`'s relative factor, THEN
    // `goTo`'s absolute centre, because `goTo` freezes the pane at whatever span it finds — asking
    // for the centre before the span would freeze it at the OLD span and only then resize around it,
    // landing off from where the row named. `goTo` itself freezes the pane (`live: false`), which is
    // the point: the past-survey jump must stick, not be read back to `live` on the next frame. Doing
    // this here, in the one place that already writes the pane from `nav`, is what makes the write
    // stick — `mirror()` below publishes the pane's OWN (now-moved) window afterwards, so there is no
    // separate `reviewAt` write for a later `mirror()` pass to overwrite.
    store.select((s) => s.nav, (nav) => {
      const p = preview;
      if (!p) return;
      const pane = p.view.panes.get(p.activePane);
      if (!pane) return;
      const w = gotoWindow(nav, pane.freq.spanHz);
      if (w) p.view.panes.setFreq(p.activePane, w.centerHz, w.spanHz);
      const t = gotoTimeWindow(nav);
      if (t) {
        if (t.spanS !== null) {
          const spanNs = t.spanS * S_TO_NS;
          p.view.panes.zoomTime(p.activePane, spanNs / Math.max(1, pane.time.spanNs), 0.5);
        }
        p.view.panes.goTo(p.activePane, t.tS * S_TO_NS);
      }
      if (!w && !t) return;
      mirror();
    });

    // Follow / freeze is the FAB (T-882 retired the toolbar's `Live` button): `paneActions`'
    // `followLive`/`pauseLive` move the pane and the map strip together, and `viewChanged` re-publishes
    // the cursor for whichever arm the pane is now on — one place publishes the window.
    renderLive = () => renderFollow();
    renderLive();
    store.select((s) => s.time.live, renderLive);

    // ---- the colour scale (T-470, T-528). A view control; it reaches no route and no device. ----
    //
    // ONE piece of state — the mode the surface is actually in — offered as the layers menu's
    // "Colour scale" radio group (T-882; it was two toolbar buttons), whose rows are derived from
    // the mode by `contrast.ts`'s `scaleRows`, so the menu cannot disagree with the surface. Every
    // press is persisted there; nothing here is fetched, polled or sent.
    const renderRange = () => {
      const p = preview;
      if (!p) return;
      setText(rangeEl, rangeLabel(p.range));
      syncLayerControls();
    };
    // The remembered mode, applied once the surface exists. `anchored` — the default — is what the
    // probe's anchor already put it in, so the common path sets nothing.
    const remembered = loadRangeMode();
    if (remembered !== "anchored") preview.setRangeMode(remembered);
    renderRange();
    // Auto-contrast moves the range every frame, so the statement follows it rather than only the
    // press: a label quoting a range the surface no longer draws with is worse than no label. At the
    // chrome cadence, not the frame's — it is a sentence, and the anchored mode never changes it.
    startPoll(async () => renderRange(), 1000);
  })();

  // The one poll that fills the navigation slice: the achievable-centre grid the retune offer plans
  // against, and the active capture windows the map lights as segments. Moved here verbatim from
  // the retired `navigators.ts`, which was its only reader.
  startPoll(async () => {
    const body = await client.get<NavigationGrid & Parameters<typeof activeWindows>[0]>("/api/navigation").catch(() => null);
    if (!body) return;
    windows = activeWindows(body);
    store.set(setNavigation({ frequency: body.frequency ?? null, time: body.time ?? null }, windows));
  }, 15_000);

  // The input handlers live as long as the mount does; the disposer is kept for symmetry with
  // `mountLiveEdge` and so a future teardown has one thing to call.
  void detach;
}

export const surfaceMounts: AreaMounts = { surface: mount };
