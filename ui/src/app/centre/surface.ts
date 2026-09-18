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
import { SurfacePreview, isBackpressure, probeSurface } from "../../surface/preview";
import {
  acceptPaneRetune, offerAcceptable, offerLabel, paneRetuneOffer, type PaneRetuneOffer,
} from "../../surface/retune";
import type { PaneView } from "../../surface/surface";
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

function mount(el: HTMLElement, ctx: AppContext) {
  const { store, client } = ctx;

  const canvas = h("canvas", { class: "sf-canvas", "aria-label": "The spectrum surface: frequency across, time down, with the whole-surface map below" }) as HTMLCanvasElement;
  const stage = h("div", { class: "sf-stage" }, canvas);
  const chrome = h("div", { class: "sf-chrome", "aria-label": "Per-viewport level readout" });
  const hoverEl = h("div", { class: "sf-hover", role: "status" });
  const note = h("div", { class: "sf-note", role: "status" });
  const offerEl = h("div", { class: "sf-offer", hidden: true });
  const liveBtn = h("button", { class: "mini sf-live", type: "button" }, "Live");
  const actions = h("div", { class: "sf-actions" }, liveBtn,
    h("button", { class: "mini", type: "button", title: "Two viewports onto the same surface, side by side. They show the identical box until one is moved.", onclick: () => preview?.split("columns") }, "Split ⇔"),
    h("button", { class: "mini", type: "button", title: "Close the active viewport. The last one never closes.", onclick: () => preview?.closeActive() }, "Close"),
    h("button", { class: "mini", type: "button", title: "Zoom the active viewport out to the device-available spectrum over the whole record horizon.", onclick: () => preview?.fitToSurface() }, "Whole surface"),
    offerEl);
  el.replaceChildren(h("div", { class: "sf-bar" }, actions, hoverEl), stage, chrome, note);

  let preview: SurfacePreview | null = null;
  let windows: ActiveWindow[] = [];
  let detach: (() => void) | null = null;
  /** The region stroke in progress (T-458), in surface coordinates, or `null`. Read inside the
   * frame callback like the marks are, never mirrored into the store: it is pointer state for the
   * duration of one gesture, and the store is where things that outlive a gesture live. */
  let pending: { pane: string; region: MarkRegion } | null = null;

  const say = (text: string) => { note.textContent = text; note.hidden = !text; };

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
  const paneUnder = (x: number, y: number) => {
    const f = preview?.lastFrame;
    if (!f) return null;
    for (const v of f.views) {
      if (v.id === preview?.view.minimap.id) continue;
      if (x >= v.rect.x && x < v.rect.x + v.rect.w && y >= v.rect.y && y < v.rect.y + v.rect.h) return v;
    }
    return null;
  };
  const fmtHz = (hz: number) => `${(hz / 1e6).toFixed(hz < 1e9 ? 4 : 6)} MHz`;
  // The CAPTURE clock's own instant, rendered as UTC (the `hms` idiom the retired live view used).
  // Not `toLocaleTimeString`: T-393's guard forbids every browser clock in these modules, and a
  // formatter that reaches for the host's timezone is one keystroke from a fallback that reaches
  // for the host's *time* — which on a replay or a time-compressed scene is not this data's time.
  const at = (ns: number) => `${new Date(ns / 1e6).toISOString().slice(11, 19)}Z`;

  function hitAt(x: number, y: number): { pane: PaneView; mark: MarkBox | null; fHz: number; tNs: number } | null {
    const v = paneUnder(x, y);
    if (!v) return null;
    const { fHz, tNs } = pointOn(v.box, v.rect, x, y);
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
