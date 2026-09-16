// MUI centre live view (T-152; ADR-0013 §3.3, §4.3). One WebGL canvas (ui/src/waterfall.ts: the
// spectrum trace in its top part, the waterfall below), fed from the first remote-permitted
// `spectrum` stream in `GET /api/streams` with reconnect and backoff. DOM overlays on top:
// - brackets for candidate rows, and one full-height yellow box per Confirmed row spanning both
//   panes, with draggable left/right edges that commit a user band override (T-193, overlays.ts);
// - selection boxes, and the DC notch mask (see overlays.ts for GAP 10);
// - a hover crosshair and readout;
// - drag to select (`POST /api/selections`), click to focus, wheel or two-finger pinch to zoom;
// - the capture-timeline review render (`GET /api/history`) while `time` is not live.
//
// Performance: spectrum rows go straight to `Waterfall.push` and never enter the store. Hover and
// drag feedback write DOM only. Overlays re-render at most once per animation frame, and only when
// inventory, focus, selections, the view or the DC mask change. `localStorage["hk-perf"] = "1"`
// shows fps and row-upload / frame CPU ms (`Waterfall.uploadMs`, `frameMs`) for the manual check.
import * as ax from "../../axis";
import { attachWheelZoom } from "../../controls/gestures";
import { inspectHalfWidthHz } from "../../inspect";
import type { NewSelection } from "../../selections";
import { MARK_DROP, MARK_GATED, Waterfall } from "../../waterfall";
import type { AppContext } from "../context";
import { h } from "../dom";
import { setUserBand } from "../explore/inventory";
import { selectionStoreFor } from "../explore/selections";
import { focusSelection, focusSignal, patchInventoryRow } from "../explore/slice";
import { bindContextTrigger, openSelectionMenu, openSignalMenu } from "../menu";
import { apiConnFor, backoffMs, openStream, parseSpectrumRecord, type StreamSocket } from "../net";
import { toast } from "../shell-slice";
import {
  MIN_BRACKET_FRAC, addModeActive, assumedDc, bracketLayout, clickTarget, confirmedBands, confirmedEdgeAt,
  dcFromHeader, dcFromObservations, dcQuery, dragBandEdge, dragSelection, draftBox, effectiveBand,
  hoverText, isDrag, levelU, placeExtent, presenceBoxes, selectionBoxes, selectionLabel, timeScaleText, tipOnLeft,
  type BandEdge, type DcMask, type DragPoint, type RowClock, type Span,
} from "./overlays";
import { historyMaxCells, historyQuery, historyRows, historyWindow, parseHistory, sameCursor } from "./review-render";
import { applyDeviceAction, geometryOfLive, gotoDecision, mayRetune, nextView, NOT_LIVE_TEXT, retuneAction, setLiveView, viewHooks } from "./view";

interface StreamInfo { stream_id: string; kind: string; remote_permitted: boolean }

const clamp01 = (x: number) => Math.min(1, Math.max(0, x));
const hms = (tS: number) => `${new Date(tS * 1000).toISOString().slice(11, 19)}Z`;
const place = (e: HTMLElement, s: Span) => { e.style.left = `${s.leftPct}%`; e.style.width = `${s.widthPct}%`; };
const perfOn = () => { try { return localStorage.getItem("hk-perf") === "1"; } catch { return false; } };
/** Just the focused row (or none): T-261/ADR-0017 TM-4 restricts the full-height Confirmed
 * box/edge-drag (T-193) to the focused row — every other row draws the time-extent presence box
 * instead (overlays.ts `presenceBoxes`). */
const focusedRowOnly = <T extends { id: string }>(rows: Readonly<Record<string, T>>, focusedId: string | null): T[] => {
  const r = focusedId ? rows[focusedId] : undefined;
  return r ? [r] : [];
};

export function mountLiveSpectrum(el: HTMLElement, ctx: AppContext) {
  const { store } = ctx;
  let canvas = h("canvas", { class: "live-canvas", "aria-label": "Spectrum and waterfall" });
  const brackets = h("div", { class: "c-brackets" });
  const specLayer = h("div", { class: "c-spec" }, brackets);
  const wfLayer = h("div", { class: "c-wf" });
  const bandLayer = h("div", { class: "c-bands" }); // T-193: full-height, spans both panes
  const presenceLayer = h("div", { class: "c-presence" }); // T-261: time-extent boxes, non-focused rows
  const draftLabel = h("span");
  const draft = h("div", { class: "c-drag", hidden: true }, draftLabel);
  const cross = h("div", { class: "c-cross", hidden: true });
  const tip = h("div", { class: "c-tip", hidden: true });
  const hint = h("div", { class: "c-hint" });
  const addToggle = h("button", {
    type: "button", class: "c-addmode", "aria-pressed": "false",
    title: "Add mode: keep every selection when you drag another region (Shift also works while dragging)",
  }, "+ Add");
  const scale = h("div", { class: "c-time" });
  const badge = h("div", { class: "c-review", role: "status", hidden: true });
  const perf = h("div", { class: "c-perf", hidden: true });
  const note = h("div", { class: "live-note", role: "status" });
  el.replaceChildren(canvas, specLayer, wfLayer, bandLayer, presenceLayer, draft, cross, tip, hint, addToggle, scale, badge, perf, note);

  let wf: Waterfall | null = null, sock: StreamSocket | null = null, attempt = 0, lastSeq = -1;
  let dc: DcMask | null = null, dcAsk = false;
  let reviewPeriodS: number | null = null, reviewSeq = 0, reviewTimer = 0;
  const bracketEls = new Map<string, HTMLElement>();

  // ---- Confirmed-signal band boxes (T-193): draggable edges, pixel-snapped, min width, revert on
  // a refused PUT. `bandOverride` is the optimistic in-progress/pending value so the box tracks the
  // drag (and survives until the commit resolves) without waiting on the next inventory poll. ----
  let bandDrag: { id: string; edge: BandEdge; startLoHz: number; startHiHz: number; pointerId: number } | null = null;
  const bandOverride = new Map<string, { loHz: number; hiHz: number }>();

  async function commitBand(id: string) {
    const band = bandOverride.get(id);
    if (!band) return;
    const res = await setUserBand(ctx.client, id, band.loHz, band.hiHz);
    if (res.ok) {
      store.set(patchInventoryRow(id, { user_band: res.entry.user_band }));
      store.set(toast(`Band set: ${ax.fmtMHz(band.loHz, 1e3)}–${ax.fmtMHz(band.hiHz, 1e3)} MHz`));
    } else {
      store.set(toast(`Band not saved: ${res.message}`));
    }
    bandOverride.delete(id); // success: the patched/reloaded row now carries it; failure: revert
    schedule();
  }

  // ---- multi-band select (T-194): a plain drag replaces only the one selection this tool itself
  // last made; add mode (the toggle, or Shift) keeps building a set instead. ----
  let addMode = false;
  let lastAutoId: string | null = null;
  const renderAddUi = () => {
    addToggle.setAttribute("aria-pressed", String(addMode));
    addToggle.classList.toggle("on", addMode);
    addToggle.textContent = addMode ? "Adding…" : "+ Add";
    hint.textContent = addMode
      ? "Add mode: drag to add another region · tap + Add to stop"
      : "Click a signal · drag to select a region · Shift or + Add keeps several";
  };
  renderAddUi();
  addToggle.addEventListener("click", () => { addMode = !addMode; renderAddUi(); });

  const say = (text: string) => { note.textContent = text; note.hidden = !text; };
  const review = (text: string) => { badge.textContent = text; badge.hidden = !text; };
  const geom = () => geometryOfLive(store.get().live);
  const livePeriodS = () => { const s = store.get(); return 1 / Math.max(1e-3, s.live.rowRateHz ?? s.device.rowsPerS ?? 25); };
  const rowPeriodS = () => reviewPeriodS ?? livePeriodS();
  // The one canonical time axis for this view (T-337): `timeAt`/`rowsBackAt` are the waterfall's
  // own per-row capture times and their exact inverse, so rows, presence boxes, selections and the
  // drag draft are all laid out through the same mapping and move together. `rowPeriodS` rides
  // along as a *duration* only (the time-scale label, the newest row's half-open end cap).
  const clock = (): RowClock | null => {
    const w = wf;
    return w
      ? { specFrac: w.specFrac, rows: w.rows, timeAt: (n) => w.timeAt(n), rowsBackAt: (t) => w.rowsBackAt(t), rowPeriodS: rowPeriodS() }
      : null;
  };

  // ---- overlays (coalesced to one render per animation frame) ----

  let queued = 0;
  const schedule = () => { if (!queued) queued = requestAnimationFrame(() => { queued = 0; render(); }); };

  function render() {
    const s = store.get(), v = s.live.view, g = geometryOfLive(s.live);
    const specPct = (wf?.specFrac ?? 0.35) * 100;
    specLayer.style.height = `${specPct}%`;
    wfLayer.style.top = `${specPct}%`;
    badge.style.top = `calc(${specPct}% + 8px)`;
    scale.textContent = wf ? timeScaleText(wf.rows, rowPeriodS(), wf) : "";
    if (!v || !g) {
      for (const e of bracketEls.values()) e.remove();
      bracketEls.clear();
      wfLayer.replaceChildren();
      bandLayer.replaceChildren();
      presenceLayer.replaceChildren();
      return;
    }
    const focusSig = s.focus.kind === "signal" ? s.focus.id : null;
    const focusSel = s.focus.kind === "selection" ? s.focus.id : null;
    const seen = new Set<string>();
    for (const b of bracketLayout(Object.values(s.inventory.rows), v, el.clientWidth, focusSig)) {
      let e = bracketEls.get(b.id);
      if (!e) {
        e = h("div", { "data-id": b.id }, h("span"));
        bracketEls.set(b.id, e);
        brackets.append(e);
      }
      e.className = `bk ${b.state}${b.active ? " active" : ""}${b.narrow ? " narrow" : ""}`;
      place(e, b);
      e.title = `${b.label} MHz · ${b.state}`;
      e.firstElementChild!.textContent = b.label;
      seen.add(b.id);
    }
    for (const [id, e] of bracketEls) if (!seen.has(id)) { e.remove(); bracketEls.delete(id); }

    // Confirmed signals: the full-height yellow box (T-193), spanning both panes, now drawn only
    // for the *focused* row (T-261/ADR-0017 TM-4: "keep the existing bracket for the focused row so
    // T-149's drag-to-adjust-band is unaffected"). Every other Confirmed row gets the time-extent
    // presence box below instead. A faint tick marks a measured edge the user band has moved off;
    // the box itself is pointer-events: none (edge dragging is resolved by pixel math in the
    // pointerdown/hover handlers below, same as click-to-focus resolves a click by frequency rather
    // than by DOM hit).
    const bandEls: HTMLElement[] = [];
    for (const b of confirmedBands(focusedRowOnly(s.inventory.rows, focusSig), v, focusSig, bandOverride)) {
      const e = h("div", { class: `c-band${b.active ? " active" : ""}`, "data-id": b.id, title: `${b.label} MHz · confirmed${b.hasUserBand ? " · user band" : ""}` });
      place(e, b);
      bandEls.push(e);
      if (b.measuredLeftPct !== null) bandEls.push(h("div", { class: "c-band-tick", style: `left:${b.measuredLeftPct}%` }));
      if (b.measuredRightPct !== null) bandEls.push(h("div", { class: "c-band-tick", style: `left:${b.measuredRightPct}%` }));
    }
    bandLayer.replaceChildren(...bandEls);

    // T-261 (ADR-0017 TM-4): a time-extent box per non-focused Candidate/Confirmed row, spanning
    // trace and waterfall. Growth needs no per-frame work — it falls out of re-reading
    // `presence.last_interval.t_end_s` on the next inventory poll and redrawing (overlays.ts
    // `presenceBoxes`). Skipped without a waterfall (no clock to place a time extent against yet).
    const rc = clock();
    presenceLayer.replaceChildren(...(rc ? presenceBoxes(Object.values(s.inventory.rows), v, rc, focusSig).map((b) => {
      const e = h("div", {
        class: `c-presence-box ${b.state}${b.open ? " open" : ""}${b.chirp ? " chirp" : ""}`,
        "data-id": b.id,
        title: `${b.label} MHz · ${b.state}${b.open ? " · on air" : ""}${b.chirp ? " · bounding box (chirp: a swept carrier drawn as its extent, not its sweep)" : ""}`,
      });
      e.style.top = `${b.topPct}%`;
      e.style.height = `${b.heightPct}%`;
      place(e, b);
      return e;
    }) : []));

    const layer: HTMLElement[] = [];
    const m = dc ? placeExtent(v, dc.loHz, dc.hiHz, 0.002) : null;
    if (dc && m) {
      const e = h("div", {
        class: `c-dc${dc.assumed ? " assumed" : ""}`,
        title: dc.assumed ? "DC notch width assumed (±15 kHz, docs/api.md): not served on the live geometry yet (API GAP 10)" : "DC notch (observation log)",
      }, h("span", {}, dc.assumed ? "DC · assumed" : "DC · masked"));
      place(e, m);
      layer.push(e);
    }
    // Selections come straight from the shared SelectionStore (T-194); `data-id` (T-192) lets the
    // context menu (menu/) resolve which selection a right-click/long-press landed on. A selection
    // with a time extent is placed through the same row clock as the rows and the presence boxes
    // (T-337), so it scrolls with the energy it selected; one without stays full height.
    for (const b of selectionBoxes(s.selections.list, v, focusSel, undefined, rc)) {
      const e = h("div", { class: `c-sel${b.active ? " active" : ""}${b.pending ? " pending" : ""}`, "data-id": b.id });
      place(e, b);
      e.style.top = `${b.topPct}%`;
      e.style.bottom = "auto";
      e.style.height = `${b.heightPct}%`;
      layer.push(e);
    }
    const fr = focusSig ? s.inventory.rows[focusSig] : undefined;
    const fp = fr ? placeExtent(v, fr.f_lo_hz, fr.f_hi_hz, MIN_BRACKET_FRAC) : null;
    if (fp) { const e = h("div", { class: "c-colhi" }); place(e, fp); layer.push(e); }
    wfLayer.replaceChildren(...layer);
  }

  // ---- pointer: hover readout, drag to select, click to focus, pinch zoom ----

  const pts = new Map<number, number>(); // pointerId → clientX
  let drag: { id: number; cx: number; cy: number; a: DragPoint; moved: boolean } | null = null;
  let pinch: { view: ax.View; dist: number; mid: number } | null = null;

  const locate = (e: { clientX: number; clientY: number }) => {
    const r = el.getBoundingClientRect();
    return { x: ax.pointerFrac(e.clientX, r), y: r.height > 0 ? clamp01((e.clientY - r.top) / r.height) : 0, heightPx: r.height };
  };

  function hideHover() { cross.hidden = true; tip.hidden = true; }

  function hover(e: PointerEvent) {
    const v = store.get().live.view, g = geom(), w = wf;
    if (!v || !g || !w) return;
    const p = locate(e), hz = ax.fracToHz(v, p.x);
    const hit = ax.yHit(p.y, w.specFrac, w.rows);
    tip.textContent = hoverText(g, hz, w.levelAt(levelU(g, hz)), hit.area === "waterfall" ? w.timeAt(hit.rowsBack) : NaN);
    cross.style.left = `${p.x * 100}%`;
    if (tipOnLeft(p.x)) { tip.style.left = "auto"; tip.style.right = `calc(${(1 - p.x) * 100}% + 10px)`; }
    else { tip.style.right = "auto"; tip.style.left = `calc(${p.x * 100}% + 10px)`; }
    cross.hidden = false;
    tip.hidden = false;
    hint.hidden = true;
    if (!drag && !pinch && !bandDrag) {
      const focus0 = store.get().focus; const focusSig = focus0.kind === "signal" ? focus0.id : null;
      const edge = confirmedEdgeAt(focusedRowOnly(store.get().inventory.rows, focusSig), v, el.clientWidth, p.x * el.clientWidth, focusSig, bandOverride);
      el.style.cursor = edge ? "ew-resize" : "";
    }
  }

  function drawDraft(a: DragPoint, b: { x: number; y: number; heightPx: number }) {
    const v = store.get().live.view, g = geom();
    const sel = v && g ? dragSelection(v, a, b, b.heightPx, clock()) : null;
    if (!sel || !g) { draft.hidden = true; return; }
    const box = draftBox(a, b, sel.t_lo !== undefined);
    draft.style.left = `${box.leftPct}%`;
    draft.style.width = `${box.widthPct}%`;
    draft.style.top = `${box.topPct}%`;
    draft.style.height = `${box.heightPct}%`;
    draftLabel.textContent = selectionLabel(sel, ax.binWidthHz(g));
    draft.hidden = false;
  }

  function click(hz: number) {
    const s = store.get(), g = geom(), v = s.live.view;
    if (!g || !v) return;
    const t = clickTarget(Object.values(s.inventory.rows), s.selections.list, ax.snapHz(g, hz), inspectHalfWidthHz(v.hiHz - v.loHz, ax.binWidthHz(g)));
    if (t?.kind === "signal") store.set(focusSignal(t.id));
    else if (t?.kind === "selection") store.set(focusSelection(t.id));
  }

  /** Commits a drag-made selection through the shared `SelectionStore` (optimistic; synced in the
   * background). `useAdd`: keep every earlier one (and stop tracking a "solo" replaceable id) when
   * building a multi-band set; otherwise replace only the one this tool itself last made. */
  function createSelection(ns: NewSelection, useAdd: boolean) {
    const shared = selectionStoreFor(ctx);
    if (!useAdd && lastAutoId) shared.remove(lastAutoId);
    try {
      const sel = shared.add(ns);
      lastAutoId = useAdd ? null : sel.id;
      if (!useAdd) store.set(focusSelection(sel.id));
      store.set(toast(`Selected ${sel.name}`));
    } catch (e) {
      store.set(toast(`Selection not saved: ${e instanceof Error ? e.message : String(e)}`));
    }
  }

  const pinchXs = () => [...pts.values()].slice(0, 2);
  function startPinch() {
    const v = store.get().live.view, xs = pinchXs();
    pinch = v && xs.length === 2 ? { view: v, dist: Math.abs(xs[0] - xs[1]), mid: ax.pointerFrac((xs[0] + xs[1]) / 2, el.getBoundingClientRect()) } : null;
  }

  el.addEventListener("pointerdown", (e) => {
    if (e.pointerType === "mouse" && e.button !== 0) return;
    if ((e.target as Element | null)?.closest?.(".c-addmode")) return;
    const bk = (e.target as Element | null)?.closest?.(".bk") as HTMLElement | null;
    if (bk?.dataset.id) { store.set(focusSignal(bk.dataset.id)); return; }
    // T-193: a Confirmed band's edge hit zone takes priority over region-select/click, but only on
    // that zone (a few px either side of the drawn edge) — everywhere else a drag still selects.
    const v0 = store.get().live.view;
    if (v0 && pts.size === 0) {
      const focus0 = store.get().focus; const focusSig = focus0.kind === "signal" ? focus0.id : null;
      const p0 = locate(e);
      const edge = confirmedEdgeAt(focusedRowOnly(store.get().inventory.rows, focusSig), v0, el.clientWidth, p0.x * el.clientWidth, focusSig, bandOverride);
      const row = edge ? store.get().inventory.rows[edge.id] : null;
      const cur = edge ? (bandOverride.get(edge.id) ?? (row ? effectiveBand(row) : null)) : null;
      if (edge && cur) {
        try { el.setPointerCapture(e.pointerId); } catch { /* synthetic pointer */ }
        e.preventDefault();
        bandDrag = { id: edge.id, edge: edge.edge, startLoHz: cur.loHz, startHiHz: cur.hiHz, pointerId: e.pointerId };
        store.set(focusSignal(edge.id));
        hideHover();
        el.style.cursor = "ew-resize";
        return;
      }
    }
    pts.set(e.pointerId, e.clientX);
    try { el.setPointerCapture(e.pointerId); } catch { /* synthetic pointer */ }
    e.preventDefault();
    if (pts.size >= 2) { drag = null; draft.hidden = true; hideHover(); startPinch(); return; }
    const p = locate(e);
    drag = { id: e.pointerId, cx: e.clientX, cy: e.clientY, a: { x: p.x, y: p.y }, moved: false };
    hover(e);
  });
  el.addEventListener("pointermove", (e) => {
    if (bandDrag && bandDrag.pointerId === e.pointerId) {
      const v = store.get().live.view;
      if (v) {
        bandOverride.set(bandDrag.id, dragBandEdge(v, el.clientWidth, bandDrag.edge, bandDrag.startLoHz, bandDrag.startHiHz, ax.pointerFrac(e.clientX, el.getBoundingClientRect())));
        schedule();
      }
      return;
    }
    if (pts.has(e.pointerId)) pts.set(e.pointerId, e.clientX);
    if (pinch) {
      const g = geom(), xs = pinchXs();
      if (g && xs.length === 2 && pinch.dist > 0) store.set(setLiveView(ax.zoomAt(g, pinch.view, pinch.mid, Math.max(1, Math.abs(xs[0] - xs[1])) / pinch.dist)));
      return;
    }
    hover(e);
    if (!drag || drag.id !== e.pointerId) return;
    if (!drag.moved && !isDrag(e.clientX - drag.cx, e.clientY - drag.cy)) return;
    drag.moved = true;
    drawDraft(drag.a, locate(e));
  });
  const end = (e: PointerEvent) => {
    if (bandDrag && bandDrag.pointerId === e.pointerId) {
      const id = bandDrag.id;
      bandDrag = null;
      el.style.cursor = "";
      if (e.type === "pointerup") void commitBand(id);
      else bandOverride.delete(id); // cancelled: drop the optimistic preview, back to the last-known band
      schedule();
      return;
    }
    if (!pts.delete(e.pointerId)) return;
    if (pinch) { if (pts.size < 2) pinch = null; return; } // a pinch never selects or focuses
    const d = drag;
    drag = null;
    draft.hidden = true;
    if (e.pointerType !== "mouse") hideHover();
    if (!d || d.id !== e.pointerId || e.type !== "pointerup") return;
    const v = store.get().live.view;
    if (!v) return;
    const p = locate(e);
    if (!d.moved) { click(ax.fracToHz(v, p.x)); return; }
    const sel = dragSelection(v, d.a, p, p.heightPx, clock());
    if (sel) createSelection(sel, addModeActive(addMode, e.shiftKey));
  };
  el.addEventListener("pointerup", end);
  el.addEventListener("pointercancel", end);
  el.addEventListener("pointerleave", (e) => { if (e.pointerType === "mouse" && !drag) hideHover(); });
  attachWheelZoom(el, viewHooks(ctx));

  // Right-click / long-press on a confirmed/candidate bracket or a selection box opens the
  // T-192 context menu (Listen, Decode, Analyze, Record/Export, Stream out, Promote, Delete,
  // Adjust band); empty waterfall/spectrum space opens nothing.
  bindContextTrigger(el, (mx, my, target) => {
    drag = null; // a long-press or right-click never also starts/finishes a drag-select
    draft.hidden = true;
    const bk = target.closest<HTMLElement>(".bk");
    if (bk?.dataset.id) {
      const r = store.get().inventory.rows[bk.dataset.id];
      if (r) openSignalMenu(ctx, r, mx, my);
      return;
    }
    const box = target.closest<HTMLElement>(".c-sel");
    if (box?.dataset.id) {
      const sel = store.get().selections.list.find((x) => x.id === box.dataset.id);
      if (sel) openSelectionMenu(ctx, sel, mx, my);
      return;
    }
    // Confirmed-signal boxes (T-193) are pointer-events: none — edge dragging is resolved by pixel
    // math, not DOM hit-testing — so a right-click/long-press over one never lands on an element
    // `.closest()` finds above; resolve it the same way a plain click does instead.
    const v = store.get().live.view, g = geom();
    if (!v || !g) return;
    const hz = ax.snapHz(g, ax.fracToHz(v, ax.pointerFrac(mx, el.getBoundingClientRect())));
    const t = clickTarget(Object.values(store.get().inventory.rows), store.get().selections.list, hz, inspectHalfWidthHz(v.hiHz - v.loHz, ax.binWidthHz(g)));
    if (t?.kind === "signal") { const r = store.get().inventory.rows[t.id]; if (r) openSignalMenu(ctx, r, mx, my); }
    else if (t?.kind === "selection") { const sel = store.get().selections.list.find((x) => x.id === t.id); if (sel) openSelectionMenu(ctx, sel, mx, my); }
  });

  // ---- spectrum stream ----

  const retry = (reason: string) => {
    store.set((s) => ({ conn: { ...s.conn, spectrum: "reconnecting", message: reason } }));
    say(reason);
    window.setTimeout(() => void connect(), backoffMs(attempt++));
  };

  const onHeader = (hd: Record<string, unknown>) => {
    const g = ax.geometryOf(hd as { center_hz?: number; bandwidth_hz?: number; fft_size?: number });
    if (hd.kind !== "spectrum" || hd.datatype !== "rf32_le" || !g) { say(`unsupported spectrum header (${String(hd.kind)}/${String(hd.datatype)})`); return; }
    attempt = 0;
    lastSeq = -1;
    const rate = typeof hd.sample_rate_hz === "number" ? hd.sample_rate_hz : 25;
    let fresh = false;
    if (!wf || wf.bins !== g.bins) {
      if (wf) {
        wf.destroy(); // a lost WebGL context cannot be reused: fresh canvas
        const c = canvas.cloneNode(false) as HTMLCanvasElement;
        canvas.replaceWith(c);
        canvas = c;
      }
      try {
        wf = new Waterfall(canvas, g.bins, rate);
        fresh = true;
      } catch (e) {
        wf = null;
        say(`waterfall unavailable: ${(e as Error).message}`);
        return;
      }
    }
    const prev = geom();
    const retuned = !prev || prev.centerHz !== g.centerHz || prev.bandwidthHz !== g.bandwidthHz || prev.bins !== g.bins;
    store.set((s) => ({
      conn: { ...s.conn, spectrum: "live", message: "" },
      live: {
        streamId: String(hd.stream_id ?? ""), centerHz: g.centerHz, bandwidthHz: g.bandwidthHz, bins: g.bins, rowRateHz: rate,
        view: nextView(g, s.live.view, retuned ? s.live.pendingView : null), pendingView: retuned ? null : s.live.pendingView,
        // A new geometry answers whatever the last pan asked about, so the standing offer is stale
        // (T-343): never leave a button that would retune to where the radio already is.
        retuneOffer: retuned ? null : s.live.retuneOffer,
      },
    }));
    wf.setView(...ax.textureWindow(g, store.get().live.view ?? ax.fullView(g)));
    if (retuned) {
      const fromHeader = dcFromHeader(hd, g);
      if (fromHeader) {
        dc = fromHeader;
        dcAsk = false; // the header is authoritative: no need to poll the observation log
      } else {
        dc = assumedDc(g);
        dcAsk = true;
      }
      if (!fresh) wf.reset(); // rows of the previous tune don't line up with the new axis
    }
    if (!store.get().time.live && (retuned || fresh)) queueReview();
    say("");
    schedule();
  };

  async function loadDc(g: ax.Geometry, tS: number) {
    try {
      const m = dcFromObservations(await ctx.client.get(dcQuery(g, tS)), g);
      if (m && geom()?.centerHz === g.centerHz) { dc = m; schedule(); }
    } catch {
      // 503 without an observation log, or offline: keep the assumed notch (GAP 10 interim).
    }
  }

  const onBinary = (buf: ArrayBuffer) => {
    const r = parseSpectrumRecord(buf);
    if (!r || !wf) return;
    if (r.type === "dropped") { wf.mark(r.gated ? MARK_GATED : MARK_DROP); lastSeq = r.seq + r.count - 1; return; }
    if (r.type !== "data") return;
    if (dcAsk && Number.isFinite(r.tS)) { dcAsk = false; const g = geom(); if (g) void loadDc(g, r.tS); }
    if (lastSeq >= 0 && r.seq > lastSeq + 1) wf.mark(MARK_DROP);
    lastSeq = r.seq;
    if (r.gated) { wf.mark(MARK_GATED); return; }
    if (r.discontinuity) wf.mark(MARK_DROP);
    if (r.row && store.get().time.live) wf.push(r.row, r.tS); // reviewing: the history render instead
  };

  async function connect() {
    sock?.close();
    store.set((s) => ({ conn: { ...s.conn, spectrum: "connecting" } }));
    let streams: StreamInfo[];
    try {
      ({ streams } = await ctx.client.get<{ streams: StreamInfo[] }>("/api/streams"));
    } catch (e) {
      const c = apiConnFor(e);
      store.set((s) => ({ conn: { ...s.conn, ...c } }));
      if (c.api === "unauthorized") return; // the shell asks for the token; reload resumes
      retry(`API: ${c.message}`);
      return;
    }
    const s = streams.find((x) => x.kind === "spectrum" && x.remote_permitted);
    if (!s) { store.set((st) => ({ conn: { ...st.conn, spectrum: "unavailable" } })); retry("no spectrum stream"); return; }
    sock = openStream(`/ws/${s.stream_id}`, ctx.token, {
      onHeader, onBinary,
      // A stream that was live and ended (a new replay pass, a re-plumb) reconnects at once.
      onClose: (wasLive) => { if (wasLive) attempt = 0; retry(wasLive ? "stream ended; reconnecting" : "disconnected; retrying"); },
    });
  }

  // ---- capture-timeline cursor: LIVE vs reviewing (ADR-0013 §3.3) ----

  function queueReview() {
    const t = store.get().time;
    if (t.live) return;
    clearTimeout(reviewTimer);
    reviewTimer = window.setTimeout(() => void loadReview(t.tS), 200); // scrubbing: fetch once it settles
  }

  async function loadReview(tS: number) {
    const g = geom(), w = wf;
    if (!g || !w) { review(`reviewing ${hms(tS)} · waiting for the live band`); return; }
    const seq = ++reviewSeq;
    const full = ax.fullView(g), { t0, t1 } = historyWindow(tS, w.rows, livePeriodS());
    review(`reviewing ${hms(tS)} · loading history…`);
    try {
      // T-334: ask in this view's own terms — `max_t` rows, `max_f` texels — so the backend serves
      // the span at a matched resolution rather than a grid this has to reduce (or truncate).
      const grid = parseHistory(await ctx.client.get(
        historyQuery(full, t0, t1, historyMaxCells(w.texWidth, w.rows), w.rows, w.texWidth),
      ));
      if (seq !== reviewSeq || store.get().time.live || wf !== w) return;
      if (!grid) { review(`reviewing ${hms(tS)} · unexpected history response`); return; }
      const out = historyRows(grid, full, w.texWidth, w.rows);
      w.setRows(out.rows, out.times);
      reviewPeriodS = grid.t_cell_s;
      review(out.rows.length ? `reviewing ${hms(tS)} · history (grey = not observed)` : `reviewing ${hms(tS)} · no history here`);
      schedule();
    } catch (e) {
      if (seq === reviewSeq) review(`reviewing ${hms(tS)} · history unavailable: ${apiConnFor(e).message}`);
    }
  }

  store.select((s) => s.time, (t) => {
    if (t.live) {
      reviewSeq++;
      clearTimeout(reviewTimer);
      reviewPeriodS = null;
      wf?.reset();
      review("");
      el.classList.remove("reviewing");
      schedule();
      return;
    }
    el.classList.add("reviewing");
    queueReview();
  }, { eq: sameCursor });

  // ---- store hooks ----

  store.select((s) => s.live.view, (v) => {
    const g = geom();
    if (wf && v && g) wf.setView(...ax.textureWindow(g, v));
    schedule();
  });
  store.select((s) => s.inventory.rows, schedule);
  store.select((s) => s.focus, schedule);
  store.select((s) => s.selections.list, schedule);
  store.select((s) => s.nav.seq, () => {
    const s = store.get();
    if (s.nav.gotoHz === null) return;
    const d = gotoDecision(geom(), s.live.view, s.nav.gotoHz, mayRetune(s.device));
    if (d?.kind === "pan") store.set(setLiveView(d.view));
    // Go to is an explicit user request (a frequency typed and submitted), so it may build a
    // DeviceAction; a gesture may not.
    else if (d?.kind === "retune") void applyDeviceAction(ctx, retuneAction(d.centerHz, "goto"));
    else if (d?.kind === "not_live") store.set(toast(NOT_LIVE_TEXT));
  });
  if (typeof ResizeObserver !== "undefined") new ResizeObserver(schedule).observe(el);
  if (perfOn()) {
    perf.hidden = false;
    window.setInterval(() => {
      if (wf) perf.textContent = `${wf.fps.toFixed(0)} fps · upload ${wf.uploadMs.toFixed(2)} ms · frame ${wf.frameMs.toFixed(2)} ms · skipped ${wf.skipped}`;
    }, 500);
  }

  void connect();
}
