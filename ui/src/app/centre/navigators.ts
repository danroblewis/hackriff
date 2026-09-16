// The two edge navigators (T-340), one parallel to each waterfall axis.
//
// **The user's invariant** (CLAUDE.md, "Time, the waterfall, and the live view"): *time runs down
// the waterfall and frequency across it, so the time navigator is a vertical bar on the side (an
// overview of the retained capture window) and the frequency navigator is a horizontal bar along
// the bottom (spanning the whole surveyed / device-available spectrum, setting the centre). Each
// navigator pans and zooms its own axis; a dragged region on either zooms the main view to it. The
// frequency navigator shows every currently-active capture window as a lit segment.*
//
// Three properties this file exists to hold, each with a test:
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
  activeWindows, litSegments, placeOn, regionFromDrag, spanOf, spectrumExtent, timeExtent,
  type Range,
} from "../../navigators";
import { captureWindow, durationText, overviewShade, type CaptureWindow, type OverviewResponse, type TimelineResponse } from "../capture/timeline";
import type { AppContext } from "../context";
import { h } from "../dom";
import { startPoll } from "../net";
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

// ---------------------------------------------------------------------------
// Time navigator — pure decisions
// ---------------------------------------------------------------------------

/** What a region dragged on the time navigator resolves to: review the span ending at `tS`. */
export interface TimeZoom { tS: number; spanS: number; tier: HistoryTier | null }

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

  const litLayer = h("div", { class: "fn-lit-layer" });
  const marker = h("div", { class: "fn-view", title: "The frequency window on screen — drag to pan it" });
  const draft = h("div", { class: "fn-draft", hidden: true });
  const label = h("span", { class: "fn-label" });
  const offerBtn = h("button", { class: "fn-offer", type: "button" }, "");
  offerBtn.hidden = true;
  const track = h("div", { class: "fn-track" }, litLayer, marker, draft, offerBtn);
  el.replaceChildren(track, label);

  const extent = (): Range | null => spectrumExtent(store.get().navGrid.grid?.frequency ?? null);

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
    label.textContent = ext
      ? `${fmtEdges(ext.lo, ext.hi)} MHz · ${s.navGrid.loaded ? `${n} active capture window${n === 1 ? "" : "s"}` : "capture windows unknown"}`
      : s.navGrid.loaded ? "no navigable spectrum reported (no live front end)" : "navigation grid not loaded";
  };

  attachBar(track, {
    frac: (e) => ax.pointerFrac(e.clientX, track.getBoundingClientRect()),
    onMarker: (e) => !marker.hidden && (e.target === marker || marker.contains(e.target as Node)),
    // Panning moves the view inside the tuned band and stops at its edges. No branch of this
    // reaches the control API, at any pan distance — that is the T-343 property, restated here.
    onPan: (df) => {
      const s = store.get(), ext = extent();
      const g = s.live.centerHz !== null && s.live.bandwidthHz !== null && s.live.bins !== null
        ? { centerHz: s.live.centerHz, bandwidthHz: s.live.bandwidthHz, bins: s.live.bins } : null;
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
      if (!done) return;
      const g = s.live.centerHz !== null && s.live.bandwidthHz !== null && s.live.bins !== null
        ? { centerHz: s.live.centerHz, bandwidthHz: s.live.bandwidthHz, bins: s.live.bins } : null;
      const target = freqZoomTarget(s.navGrid.grid ?? { frequency: null, time: null }, g, region, mayRetune(s.device));
      if (target.kind === "view") {
        store.set(setLiveView(target.view));
        store.set(toast(`Zoomed to ${fmtEdges(target.view.loHz, target.view.hiHz)} MHz · ${detailLabel(target.source)}`));
      } else if (target.kind === "offer") {
        store.set(setRetuneOffer({ centerHz: target.centerHz, view: target.view }));
        store.set(toast(`Outside the tuned window — ${retuneLabel(target.centerHz)} to see it (${detailLabel(target.source)}).`));
      }
    },
  });
  let panOverflow = 0;

  // Wheel zooms the main view's frequency axis about the pointer. A view gesture; the front end's
  // own span is a device action and lives in the SDR control panel.
  track.addEventListener("wheel", (e) => {
    const s = store.get();
    const g = s.live.centerHz !== null && s.live.bandwidthHz !== null && s.live.bins !== null
      ? { centerHz: s.live.centerHz, bandwidthHz: s.live.bandwidthHz, bins: s.live.bins } : null;
    const ext = extent();
    if (!g || !s.live.view || !ext || e.deltaY === 0) return;
    e.preventDefault();
    const hz = ext.lo + ax.pointerFrac(e.clientX, track.getBoundingClientRect()) * spanOf(ext);
    const at = ax.hzToFrac(s.live.view, hz);
    store.set(setLiveView(ax.zoomAt(g, s.live.view, Math.min(1, Math.max(0, at)), ax.wheelFactor(e.deltaY, e.deltaMode))));
  }, { passive: false });

  store.select((s) => s.navGrid, render, { immediate: true });
  store.select((s) => s.live.view, render);
  store.select((s) => s.live.retuneOffer, render);
  store.select((s) => s.device.deviceId, render);

  // The one poll that fills the navigation slice, read by both navigators.
  startPoll(async () => {
    const body = await client.get<NavigationGrid & Parameters<typeof activeWindows>[0]>("/api/navigation").catch(() => null);
    if (!body) return;
    store.set(setNavigation({ frequency: body.frequency ?? null, time: body.time ?? null }, activeWindows(body)));
  }, 15_000);
}

// ---------------------------------------------------------------------------
// The time navigator: a vertical bar on the side, oldest at the top
// ---------------------------------------------------------------------------

function mountTimeNav(el: HTMLElement, ctx: AppContext) {
  const { store, client } = ctx;
  el.replaceChildren();

  const canvas = h("canvas", {
    class: "tn-overview", role: "img",
    "aria-label": "Overview of the retained capture window",
  }) as HTMLCanvasElement;
  const marker = h("div", { class: "tn-view", title: "The time span on screen — drag to move it" });
  const draft = h("div", { class: "tn-draft", hidden: true });
  const track = h("div", { class: "tn-track" }, canvas, marker, draft);
  el.replaceChildren(track);

  // The capture window this bar is laid out on (T-338): `GET /api/timeline`'s window, which is the
  // IQ ring's configured retention. `null` = not answered, or this server has no capture window;
  // the bar then draws nothing rather than inventing a span.
  let win: CaptureWindow | null = null;
  const extent = (): Range | null => timeExtent(win);

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
    if (!ext) { marker.hidden = true; track.title = "No capture window on this server"; return; }
    track.title = `Capture window: ${durationText(spanOf(ext))} of retained IQ`;
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
    // Panning the time bar moves the reviewed instant. It touches the store's time cursor and
    // nothing else: capture, the ring and detection are always-on (CLAUDE.md, "Pause freezes the
    // view, not the capture"), and no time gesture has ever had a path to a device route.
    onPan: (df) => {
      const ext = extent();
      const t = store.get().time;
      if (!ext) return;
      const cur = t.live ? ext.hi : t.tS;
      const next = Math.min(ext.hi, Math.max(ext.lo, cur + df * spanOf(ext)));
      store.set(next >= ext.hi ? goLive : reviewAt(next, t.live ? null : (t.spanS ?? null)));
    },
    onPanEnd: () => { /* a time pan settles where it is: nothing to offer, nothing to command */ },
    onRegion: (a, b, done) => {
      const ext = extent();
      const region = regionFromDrag(ext, a, b);
      const p = region ? placeOn(ext, region.lo, region.hi, 0) : null;
      draft.hidden = !p || done;
      if (p && !done) pctStyle(draft, p, true);
      if (!done || !region) return;
      const z = timeZoomTarget(store.get().navGrid.grid ?? { frequency: null, time: null }, region, rowsOnScreen());
      if (!z) return;
      store.set(reviewAt(z.tS, z.spanS));
      store.set(toast(`Zoomed to ${timeDetailText(z)}`));
    },
  });

  // Wheel zooms the reviewed span about the pointer, on the time axis only.
  track.addEventListener("wheel", (e) => {
    const ext = extent();
    const t = store.get().time;
    if (!ext || e.deltaY === 0) return;
    e.preventDefault();
    const cur = t.live ? ext.hi : t.tS;
    const span = (t.live ? 0 : (t.spanS ?? 0)) || spanOf(ext) / 8;
    const next = Math.min(spanOf(ext), Math.max(1e-3, span / ax.wheelFactor(e.deltaY, e.deltaMode)));
    store.set(reviewAt(Math.min(ext.hi, Math.max(ext.lo + next, cur)), next));
  }, { passive: false });

  store.select((s) => s.time, render, { immediate: true });

  startPoll(async () => {
    const tl = await client.get<TimelineResponse>(`/api/timeline?columns=${TIME_COLUMNS}&rows=${TIME_ROWS}`).catch(() => null);
    win = captureWindow(tl);
    renderOverview(tl?.grid ?? null);
    render();
  }, 60_000);
}

export const navigatorMounts = { freqnav: mountFreqNav, timenav: mountTimeNav };
