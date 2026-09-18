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
// (CLAUDE.md's whole-UI window rule), and renders the retune offer T-444 computes. The only device
// route it can reach is through `acceptPaneRetune` → `applyDeviceAction`, T-343's one gate, on an
// explicit button press.
import { activeWindows, type ActiveWindow } from "../../navigators";
import type { NavigationGrid } from "../../navigation";
import { attachSurfaceInput, type GlPoint } from "../../surface/input";
import {
  markAt, markQuads, normalizeRegion, pendingMarkBox, pointOn, selectionMarkBoxes, signalMarkBoxes,
  type MarkBox, type MarkRegion,
} from "../../surface/marks";
import { SurfacePreview, clampToRect, isBackpressure, probeSurface } from "../../surface/preview";
import {
  acceptPaneRetune, offerAcceptable, offerLabel, paneRetuneOffer, type PaneRetuneOffer,
} from "../../surface/retune";
import type { PaneRect, PaneReport, PaneView } from "../../surface/surface";
import {
  HOLD_INK, SLICE_INK, TRACE_COLUMNS, liveFrameFits, maxHoldColumns, peakOf, sampleFrame, sliceColumns,
  sliceWindow, traceQuads,
} from "../../surface/trace";
import type { OverlayQuad } from "../../surface/minimap";
import { liveRow } from "./live-edge";
import type { AppContext, AreaMounts } from "../context";
import { h } from "../dom";
import { openSelectionMenu, openSignalMenu } from "../menu";
import { startPoll } from "../net";
import { commitRegion } from "../explore/region";
import { focusSelection, focusSignal } from "../explore/slice";
import { reviewAt, setNavigation, toast } from "../state";

const S_TO_NS = 1e9;
/** The map strip along the bottom of the canvas, device px. */
const MINIMAP_PX = 110;
/** A pane frozen within this of the edge still counts as showing the growing edge, for the retune
 * offer's "not showing the live edge" refusal (T-444). One frame at 60 Hz, generously. */
const EDGE_GRACE_NS = 0.25 * S_TO_NS;
/** Height of the spectrum-trace strip above each pane, device px (T-457). */
const TRACE_PX = 96;

function mount(el: HTMLElement, ctx: AppContext) {
  const { store, client } = ctx;

  const canvas = h("canvas", { class: "sf-canvas", "aria-label": "The spectrum surface: frequency across, time down, with the whole-surface map below" }) as HTMLCanvasElement;
  const stage = h("div", { class: "sf-stage" }, canvas);
  const chrome = h("div", { class: "sf-chrome", "aria-label": "Per-viewport level readout" });
  const hoverEl = h("div", { class: "sf-hover", role: "status" });
  const note = h("div", { class: "sf-note", role: "status" });
  const offerEl = h("div", { class: "sf-offer", hidden: true });
  const traceEl = h("div", { class: "sf-trace", role: "status" });
  const liveBtn = h("button", { class: "mini sf-live", type: "button" }, "Live");
  const traceBtn = h("button", {
    class: "mini sf-tracebtn on", type: "button", "aria-pressed": "true",
    title: "The spectrum trace above each viewport: the slice across frequency at that viewport's own time position, and the max-hold over its whole window. Both are drawn on the surface's one measured dB range.",
  }, "Trace");
  const actions = h("div", { class: "sf-actions" }, liveBtn, traceBtn,
    h("button", { class: "mini", type: "button", title: "Two viewports onto the same surface, side by side. They show the identical box until one is moved.", onclick: () => preview?.split("columns") }, "Split ⇔"),
    h("button", { class: "mini", type: "button", title: "Close the active viewport. The last one never closes.", onclick: () => preview?.closeActive() }, "Close"),
    h("button", { class: "mini", type: "button", title: "Zoom the active viewport out to the device-available spectrum over the whole record horizon.", onclick: () => preview?.fitToSurface() }, "Whole surface"),
    offerEl);
  el.replaceChildren(h("div", { class: "sf-bar" }, actions, hoverEl), stage, traceEl, chrome, note);

  let preview: SurfacePreview | null = null;
  let windows: ActiveWindow[] = [];
  let detach: (() => void) | null = null;
  /** The region stroke in progress (T-458), in surface coordinates, or `null`. Read inside the
   * frame callback like the marks are, never mirrored into the store: it is pointer state for the
   * duration of one gesture, and the store is where things that outlive a gesture live. */
  let pending: { pane: string; region: MarkRegion } | null = null;

  const say = (text: string) => { note.textContent = text; note.hidden = !text; };
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

  // ---- the marks: per frame, from the state they describe ----
  // Deliberately reading the store *inside* the frame callback rather than subscribing: a
  // subscription would re-derive on the poll's cadence, which is T-388 exactly.
  const boxesFor = (pane: PaneView): MarkBox[] => {
    const s = store.get();
    const focusId = s.focus.kind === "signal" ? s.focus.id : null;
    const selId = s.focus.kind === "selection" ? s.focus.id : null;
    return [
      ...signalMarkBoxes(Object.values(s.inventory.rows), focusId),
      ...selectionMarkBoxes(s.selections.list, selId, pane.box),
      // The rubber band goes through the same pass on the same frame as everything else it is being
      // drawn over, and only on the pane it is being stroked on (T-458).
      ...pendingMarkBox(pending && pending.pane === pane.id ? pending.region : null),
    ];
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
  // **Why there is no manual dB range.** The old waterfall carried `setScale(auto, lo, hi)` and
  // **nothing ever called it** — a repo-wide search at the cutover commit finds the definition and no
  // caller, so the cutover retired an unreachable control rather than a feature in use. And the range
  // it would have overridden is measured: the tiles report their own `range_db` and the surface
  // tracks it. Letting a hand-set pair of numbers stand in for that is a user overriding a
  // measurement, which is the move this product declines by default. The honest control is to *say*
  // the range, which the readout below does, so a surprising picture is diagnosable instead of
  // paintable-over.
  let traceOn = true;
  const fmtDb = (db: number) => `${db.toFixed(1)} dB`;
  const fmtDur = (s: number) => (s < 1 ? `${(s * 1000).toFixed(0)} ms` : s < 90 ? `${s.toFixed(1)} s` : `${(s / 60).toFixed(1)} min`);
  const traceFor = (pane: PaneView, _edgeNs: number, report: PaneReport, strip: PaneRect): OverlayQuad[] => {
    const p = preview;
    if (!p) return [];
    const s = p.view.surface;
    const dev = pane.device ?? "any";
    const n = Math.max(16, Math.min(TRACE_COLUMNS, Math.floor(strip.w)));
    const out: OverlayQuad[] = [];

    // The max-hold, over this viewport's WHOLE window, from the tiles it just drew at the level it
    // drew them. Not an accumulator: the pyramid's cells ARE max-holds (hk-api's `MAX_HOLD_RULE`),
    // so panning to an hour ago shows that hour's peak instead of restarting from nothing.
    const hold = maxHoldColumns(s.lat, s.cache, pane.box, report.levelF, report.levelT, dev, n);
    out.push(...traceQuads(hold, pane.box, strip, s.lo, s.hi, HOLD_INK, "trace-hold", pane.id));

    // The slice, at this viewport's own time position. The live row is preferred only where it is
    // genuinely finer — inside the cell the slice is asking about — and the pyramid answers
    // everywhere else, which is what makes a scrubbed pane show the spectrum of *then*.
    const tAtNs = pane.box.t1Ns;
    const win = sliceWindow(s.lat, report.levelT, tAtNs);
    const fr = liveRow.get();
    const live = liveFrameFits(fr, win);
    const slice = live && fr
      ? sampleFrame(fr, pane.box, n)
      : sliceColumns(s.lat, s.cache, pane.box, report.levelF, report.levelT, dev, n, tAtNs);
    out.push(...traceQuads(slice, pane.box, strip, s.lo, s.hi, SLICE_INK, "trace-slice", pane.id));

    // The readout, for the pane gestures apply to. Written here rather than on the poll for the same
    // reason the quads are: it describes the frame that was just drawn. It names the SOURCE, because
    // "a live frame" and "a 1.0 s cell" are different claims about the same picture and the coarser
    // one must not be passed off as an instant.
    if (pane.id === p.activePane) {
      const slicePk = peakOf(slice, pane.box);
      const holdPk = peakOf(hold, pane.box);
      const spanS = (pane.box.t1Ns - pane.box.t0Ns) / S_TO_NS;
      const src = live ? "live frame" : `${fmtDur((win.t1Ns - win.t0Ns) / S_TO_NS)} cell`;
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
          ? `slice ${at(live && fr ? fr.tNs : tAtNs)} (${src}) · peak ${fmtDb(slicePk.db)} at ${fmtHz(slicePk.hz)}`
          : `slice ${at(tAtNs)} (${src}) — ${empty}`,
        holdPk
          ? `max-hold over ${fmtDur(spanS)} · peak ${fmtDb(holdPk.db)} at ${fmtHz(holdPk.hz)}`
          : `max-hold over ${fmtDur(spanS)} — ${empty}`,
        `scale ${fmtDb(s.lo)} … ${fmtDb(s.hi)}, measured from the served tiles and shared with the ramp`,
      ].join(" · "));
    }
    return out;
  };
  traceBtn.addEventListener("click", () => {
    traceOn = !traceOn;
    if (preview) preview.view.tracePx = traceOn ? TRACE_PX : 0;
    traceBtn.classList.toggle("on", traceOn);
    traceBtn.setAttribute("aria-pressed", String(traceOn));
    traceEl.hidden = !traceOn;
    if (!traceOn) traceEl.textContent = "";
  });

  // ---- mirror the active viewport into the app's one window (CLAUDE.md's whole-UI window rule) ----
  // The inventory lists, the focus panel and the decode captures all scope themselves through
  // `state.live.view` + `state.time` (`explore/inventory.ts`'s `viewWindow`). Writing the viewport
  // there is what keeps them answering about what is on screen; it is the same state the capture
  // band's scrub writes, so there is one time cursor with two editors, not two cursors.
  let lastMirror = "";
  function mirror(): void {
    const p = preview;
    if (!p) return;
    const pane = p.view.panes.get(p.activePane);
    if (!pane) return;
    const loHz = pane.freq.centerHz - pane.freq.spanHz / 2, hiHz = pane.freq.centerHz + pane.freq.spanHz / 2;
    const spanS = pane.time.spanNs / S_TO_NS;
    const key = `${loHz}|${hiHz}|${pane.time.live}|${spanS}|${pane.time.live ? "" : pane.time.centerNs}`;
    if (key === lastMirror) return;
    lastMirror = key;
    store.set((s) => ({ live: { ...s.live, view: { loHz, hiHz } } }));
    if (pane.time.live) store.set((s) => (s.time.live && s.time.spanS === spanS ? {} : { time: { live: true, spanS } }));
    else store.set(reviewAt(pane.time.centerNs / S_TO_NS + spanS / 2, spanS));
  }

  // ---- the retune offer (T-444). A pan produces an offer; only this button moves the radio. ----
  let offer: PaneRetuneOffer | null = null;
  const offerNow = (paneId: string): PaneRetuneOffer | null => {
    const p = preview;
    const pane = p?.view.panes.get(paneId);
    if (!p || !pane) return null;
    return paneRetuneOffer(pane, windows, store.get().navGrid.grid?.frequency ?? null, p.edgeNs, EDGE_GRACE_NS);
  };
  const acceptBtn = h("button", { class: "mini", type: "button" }, "Retune");
  const offerText = h("span", {});
  offerEl.replaceChildren(offerText, acceptBtn);
  acceptBtn.addEventListener("click", () => {
    const p = preview, o = offer;
    if (!p || !o) return;
    void acceptPaneRetune(ctx, {
      offerNow,
      // T-437 §5.2: the growing edge's tiles were computed from the tuning that has just ended, so
      // a cached one is an observation claim about a tuning that no longer exists.
      invalidateEdge: () => p.view.surface.cache.invalidateEdge(p.view.surface.lat, p.edgeNs),
    }, o).then((r) => {
      if (!r.ok && r.reason === "moved") store.set(toast("The viewport moved: the offer was for where it was. Press again."));
    });
  });

  function renderOffer(): void {
    const p = preview;
    offer = p ? offerNow(p.activePane) : null;
    offerEl.hidden = !offer;
    if (!offer) return;
    offerText.textContent = offerLabel(offer);
    acceptBtn.disabled = !offerAcceptable(offer);
  }

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

  function hitAt(x: number, y: number): { pane: PaneView; mark: MarkBox | null; fHz: number; tNs: number } | null {
    const hit = paneUnder(x, y);
    if (!hit) return null;
    const { view: v, at } = hit;
    const { fHz, tNs } = pointOn(v.box, v.rect, at.x, at.y);
    return { pane: v, fHz, tNs, mark: markAt(boxesFor(v), preview!.edgeNs, fHz, tNs) };
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


  // ---- boot ----
  void (async () => {
    let probe;
    try {
      probe = await probeSurface((path) => client.get(path));
    } catch (e) {
      say(isBackpressure(e)
        ? "The tile route is busy producing for another viewport. Nothing is wrong with the surface — it will come back."
        : `The surface could not be addressed: ${e instanceof Error ? e.message : String(e)}. GET /api/tiles is what states the view lattice, and a client that guessed one would be addressing a pyramid that does not exist.`);
      return;
    }
    try {
      preview = new SurfacePreview({
        canvas, probe, token: ctx.token, fetchFn: (u, i) => fetch(u, i),
        chrome, minimapPx: MINIMAP_PX,
        edge: () => edgeNs() || probe.origin.edgeNs,
        windows: () => windows,
        marks: (pane, edge) => markQuads(boxesFor(pane), edge, pane.box, pane.rect),
        trace: traceFor, tracePx: TRACE_PX,
      });
    } catch (e) {
      say(`WebGL2 is unavailable in this browser: ${e instanceof Error ? e.message : String(e)}`);
      return;
    }
    say(probe.note);

    detach = attachSurfaceInput(canvas, preview, {
      onView: () => { mirror(); renderOffer(); },
      onHover: (p) => {
        if (!p) { hoverEl.textContent = ""; return; }
        const hit = hitAt(p.x, p.y);
        hoverEl.textContent = hit ? `${fmtHz(hit.fHz)} · ${at(hit.tNs)}${hit.mark ? ` · ${hit.mark.kind === "signal-box" ? "signal" : "selection"} ${hit.mark.id.slice(0, 8)}` : ""}` : "";
      },
      onClick: (p) => {
        const hit = hitAt(p.x, p.y);
        if (!hit?.mark) return;
        store.set(hit.mark.kind === "signal-box" ? focusSignal(hit.mark.id) : focusSelection(hit.mark.id));
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
      onContext: (p, e) => {
        const hit = hitAt(p.x, p.y);
        if (!hit?.mark) return;
        const s = store.get();
        if (hit.mark.kind === "signal-box") {
          const row = s.inventory.rows[hit.mark.id];
          if (row) openSignalMenu(ctx, row, e.clientX, e.clientY);
        } else {
          const sel = s.selections.list.find((x) => x.id === hit.mark!.id);
          if (sel) openSelectionMenu(ctx, sel, e.clientX, e.clientY);
        }
      },
    });

    const fit = () => {
      const r = stage.getBoundingClientRect();
      preview?.resize(r.width, r.height, window.devicePixelRatio || 1);
    };
    fit();
    const ro = typeof ResizeObserver === "function" ? new ResizeObserver(fit) : null;
    ro?.observe(stage);
    window.addEventListener("resize", fit);
    preview.start();

    // Once a second, not per frame: the offer depends on the reported windows and the grid, both of
    // which arrive on a poll. The *boxes* are per frame; this is chrome.
    startPoll(async () => { mirror(); renderOffer(); }, 1000);

    // ---- Go to / bookmarks: a frequency request moves the viewport (T-152's `nav.gotoHz`) ----
    store.select((s) => s.nav.gotoHz, (hz) => {
      const p = preview;
      if (!p || hz === null || !Number.isFinite(hz)) return;
      const pane = p.view.panes.get(p.activePane);
      if (!pane) return;
      p.view.panes.setFreq(p.activePane, hz, pane.freq.spanHz);
      mirror();
      renderOffer();
    });

    liveBtn.addEventListener("click", () => {
      const p = preview;
      if (!p) return;
      const following = p.view.panes.isFollowing(p.activePane);
      // T-442: freezing is a COORDINATE change, not a mode change — `pause` writes down the window
      // the pane was already showing, so the frame you pause on is identical to the one before it.
      p.view.panes.setFollowing(p.activePane, !following);
      p.view.minimap.setFollowing(!following);
      // `mirror` writes the cursor for whichever arm we are now on, so there is no separate
      // `goLive` write here: one place publishes the window, and it is the viewport's own state.
      lastMirror = ""; // the arm of the cursor changed, so re-publish it even if the numbers match
      mirror();
      renderLive();
    });
    const renderLive = () => {
      const on = !!preview && preview.view.panes.isFollowing(preview.activePane);
      liveBtn.textContent = on ? "Live" : "Paused";
      liveBtn.classList.toggle("on", on);
      liveBtn.title = on
        ? "This viewport follows the growing edge. Pausing freezes the view only — capture, the ring and detection never stop."
        : "This viewport is frozen on a past window. Capture never stopped.";
    };
    renderLive();
    store.select((s) => s.time.live, renderLive);
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
