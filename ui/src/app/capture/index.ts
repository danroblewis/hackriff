// Capture timeline mount (ADR-0013 §3.3, §4.4, §8). Owner: T-150. Always-on, scrubbable activity
// band over `GET /api/history`; the time cursor drives T-152's review render and T-151's inventory
// queries. API GAP 1 (no rolling-buffer status/clip-export route yet): shows an honest coverage
// summary instead of invented buffered-hours/quota numbers, and "Record IQ" is the interim label
// for "Export clip from the buffer" (it records forward from now, not from the retained past).
import type { AppContext, AreaMounts } from "../context";
import { h } from "../dom";
import { startPoll } from "../net";
import { selectionStoreFor } from "../explore/selections";
import { focusSelection } from "../explore/slice";
import { goLive, reviewAt, toast, type AppState } from "../state";
import {
  DRAG_PX, WINDOW_S, agoText, coverageText, currentSpan, pctForAgo, reduceActivity, scrubToTime, selectionSpans, timeRegionName,
  timeWindowFromScrub, type HistoryGrid,
} from "./timeline";

const COLUMNS = 96;
const SVG_NS = "http://www.w3.org/2000/svg";
const svgRect = () => document.createElementNS(SVG_NS, "rect");

interface HistoryResponse extends HistoryGrid { coverage_summary?: { observed_fraction: number } }
interface RecordSession { id: string; active: boolean; elapsed_s: number; max_s: number }

function spanOf(s: AppState) { return currentSpan({ live: s.live.view, device: s.device }); }

function mount(el: HTMLElement, ctx: AppContext) {
  const { store, client } = ctx;

  const noteB = h("b", {}, "viewing live");
  const noteRest = document.createTextNode(" · coverage unknown");
  const recordBtn = h("button", { class: "mini", type: "button" }, "Record IQ");
  const head = h("div", { class: "section-h" },
    h("span", {}, "Capture · always recording"),
    h("em", { class: "cap-note" }, noteB, noteRest, " ", recordBtn));

  const svg = document.createElementNS(SVG_NS, "svg");
  svg.setAttribute("viewBox", "0 0 600 60");
  svg.setAttribute("preserveAspectRatio", "none");
  svg.setAttribute("role", "img");
  svg.setAttribute("aria-label", "Activity over the retained history");
  const selLayer = h("div", { class: "cap-sel-layer" });
  const playhead = h("div", { class: "playhead" });
  const livePill = h("button", { class: "live-pill", type: "button" }, "● LIVE");
  const band = h("div", { class: "cap-band" }, svg, selLayer, playhead, livePill);

  el.replaceChildren(head, band);

  // ---- scrub / LIVE ----
  let dragging = false, downX = 0, downPct = 0;
  const pctFromEvent = (e: PointerEvent) => {
    const r = band.getBoundingClientRect();
    return r.width > 0 ? ((e.clientX - r.left) / r.width) * 100 : 100;
  };
  const scrub = (e: PointerEvent) => {
    const res = scrubToTime(pctFromEvent(e), Date.now() / 1000);
    store.set(res.live ? goLive : reviewAt(res.tS));
  };
  band.addEventListener("pointerdown", (e) => {
    if (e.target === livePill) return;
    dragging = true;
    downX = e.clientX;
    downPct = pctFromEvent(e);
    band.setPointerCapture(e.pointerId);
    scrub(e);
  });
  band.addEventListener("pointermove", (e) => { if (dragging) scrub(e); });
  band.addEventListener("pointerup", (e) => {
    if (dragging && Math.abs(e.clientX - downX) >= DRAG_PX) commitTimeWindow(downPct, pctFromEvent(e));
    dragging = false;
  });
  livePill.addEventListener("click", () => { store.set(goLive); store.set(toast("Back to live.")); });

  // ---- time-window select (T-194): a deliberate drag sets t_lo/t_hi on the focused selection, or
  // makes a new one over the current view span. ----
  function commitTimeWindow(pctA: number, pctB: number) {
    const w = timeWindowFromScrub(pctA, pctB, Date.now() / 1000);
    if (!w) return;
    const shared = selectionStoreFor(ctx);
    const focus = store.get().focus;
    const cur = focus.kind === "selection" ? shared.get(focus.id) : undefined;
    if (cur) {
      if (shared.setExtent(cur.id, { t_lo: w.t_lo, t_hi: w.t_hi })) store.set(toast(`Time window set on ${cur.name}`));
      return;
    }
    const span = spanOf(store.get());
    if (!span) { store.set(toast("No tuned span to select yet.")); return; }
    const sel = shared.add({ name: timeRegionName(w.t_lo, w.t_hi), f_lo: span.loHz, f_hi: span.hiHz, t_lo: w.t_lo, t_hi: w.t_hi });
    store.set(focusSelection(sel.id));
    store.set(toast(`Selected ${sel.name}`));
  }

  const renderSelSpans = () => {
    const now = Date.now() / 1000, focus = store.get().focus;
    selLayer.replaceChildren(...selectionSpans(store.get().selections.list, now).map((sp) => {
      const e = h("div", { class: `cap-sel${focus.kind === "selection" && focus.id === sp.id ? " active" : ""}` });
      e.style.left = `${sp.leftPct}%`;
      e.style.width = `${sp.widthPct}%`;
      return e;
    }));
  };
  store.select((s) => s.selections.list, renderSelSpans, { immediate: true });
  store.select((s) => s.focus, renderSelSpans);

  let coverageFraction: number | null = null;
  const renderNote = () => {
    const t = store.get().time;
    band.classList.toggle("reviewing", !t.live);
    if (t.live) {
      noteB.textContent = "viewing live";
      playhead.style.left = "100%";
    } else {
      const agoS = Math.max(0, Date.now() / 1000 - t.tS);
      noteB.textContent = `reviewing ${agoText(agoS)} ago`;
      playhead.style.left = `${pctForAgo(agoS)}%`;
    }
    noteRest.textContent = ` · ${coverageText(coverageFraction)} · press LIVE to return`;
  };
  store.select((s) => s.time, renderNote, { immediate: true });

  // ---- activity band (GET /api/history, §4.4) ----
  const renderBand = (grid: HistoryGrid) => {
    const cols = reduceActivity(grid, COLUMNS);
    const W = 600, H = 60, gap = 1.5, bw = W / COLUMNS - gap;
    svg.replaceChildren(...cols.map((v, i) => {
      const r = svgRect();
      r.setAttribute("x", String(i * (bw + gap)));
      const height = v === null ? 3 : Math.max(2, v * (H - 8));
      r.setAttribute("y", String(H - height));
      r.setAttribute("width", String(bw));
      r.setAttribute("height", String(height));
      r.setAttribute("rx", "1");
      r.setAttribute("fill", v === null ? "var(--line)" : "var(--teal)");
      r.setAttribute("fill-opacity", v === null ? "0.4" : String(0.35 + 0.55 * v));
      return r;
    }));
  };

  startPoll(async () => {
    const span = spanOf(store.get());
    if (!span) return;
    const now = Date.now() / 1000;
    const grid = await client.get<HistoryResponse>(
      `/api/history?f_lo=${span.loHz}&f_hi=${span.hiHz}&t0=${now - WINDOW_S}&t1=${now}&max_cells=${COLUMNS * 16}`,
    );
    renderBand(grid);
    coverageFraction = grid.coverage_summary?.observed_fraction ?? null;
    renderNote();
    renderSelSpans();
  }, 60_000);

  // ---- Record IQ (GAP 1 interim for "Export clip from the buffer") ----
  let session: RecordSession | null = null;
  const renderRecordBtn = () => {
    recordBtn.textContent = session?.active ? `Stop (${session.elapsed_s.toFixed(0)} s)` : "Record IQ";
  };
  const pollSession = () => startPoll(async () => {
    if (!session) return;
    const r = await client.get<{ recordings: RecordSession[] }>("/api/outputs");
    const found = r.recordings.find((s) => s.id === session!.id) ?? null;
    session = found;
    renderRecordBtn();
    if (!found || !found.active) { store.set(toast("IQ recording finished.")); stopPoll?.(); }
  }, 2000);
  let stopPoll: (() => void) | null = null;

  recordBtn.addEventListener("click", () => {
    if (session?.active) {
      client.post<{ recording: RecordSession }>("/api/outputs/record/stop", { id: session.id })
        .then((r) => { session = r.recording; renderRecordBtn(); stopPoll?.(); })
        .catch(() => store.set(toast("Could not stop the recording.")));
      return;
    }
    const span = spanOf(store.get());
    if (!span) { store.set(toast("No tuned span to record yet.")); return; }
    client.post<{ recording: RecordSession }>("/api/outputs/record/start", { band: { f_lo: span.loHz, f_hi: span.hiHz }, kinds: ["iq"] })
      .then((r) => { session = r.recording; renderRecordBtn(); stopPoll = pollSession(); })
      .catch(() => store.set(toast("Could not start an IQ recording.")));
  });
  renderRecordBtn();
}

export const mounts: AreaMounts = { capture: mount };
