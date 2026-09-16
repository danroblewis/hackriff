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
  DRAG_PX, WINDOW_S, agoText, coverageText, currentSpan, eventMarkTitle, eventMarks, pctForAgo, reduceActivity,
  ringSpan, scrubDataNote, scrubToTime, selectionSpans, timeRegionName, timeWindowFromScrub,
  type CoverageGap, type EventRow, type HistoryGrid, type RingStatus,
} from "./timeline";

const COLUMNS = 96;
const SVG_NS = "http://www.w3.org/2000/svg";
const svgRect = () => document.createElementNS(SVG_NS, "rect");

interface HistoryResponse extends HistoryGrid {
  coverage_summary?: { observed_fraction: number; gaps?: CoverageGap[] };
}
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
  // T-263 (ADR-0017 TM-7): the past events a user scrubs *to*, and the IQ ring that still backs a
  // scrub-back (ADR-0014). Both are placed from timespans the API served; neither is drawn at all
  // until its poll has answered, so "not asked yet" never renders as "nothing happened".
  const marksLayer = h("div", { class: "cap-marks" });
  const ringTrack = h("div", { class: "cap-ring", hidden: true });
  const playhead = h("div", { class: "playhead" });
  const livePill = h("button", { class: "live-pill", type: "button" }, "● LIVE");
  const band = h("div", { class: "cap-band" }, svg, marksLayer, ringTrack, selLayer, playhead, livePill);

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
  // `null` = not answered yet, which is "unknown" and never "nothing" (T-164/T-207/T-284).
  let coverageGaps: CoverageGap[] | null = null;
  let ring: RingStatus | null = null;
  let eventRows: EventRow[] | null = null;

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
    // T-263: what the scrubbed window is actually backed by. "Nothing was on the air" and "no data
    // for this window" are different claims, and the note says which one applies.
    const data = t.live ? "" : scrubDataNote(t.tS, false, ring, coverageGaps);
    noteRest.textContent = ` · ${coverageText(coverageFraction)}${data ? ` · ${data}` : ""} · press LIVE to return`;
  };
  store.select((s) => s.time, renderNote, { immediate: true });

  // T-263: past events and the retained ring. A mark is one timespan a row reported; it carries no
  // liveness, because a `recurrence` appearance never measured one (see timeline.ts).
  const renderMarks = () => {
    const now = Date.now() / 1000;
    marksLayer.replaceChildren(...(eventRows === null ? [] : eventMarks(eventRows, now).map((m) => {
      const e = h("div", { class: `cap-mark ${m.state}`, title: eventMarkTitle(m, now) });
      e.style.left = `${m.leftPct}%`;
      e.style.width = `${m.widthPct}%`;
      return e;
    })));
    const rs = ringSpan(ring, now);
    ringTrack.hidden = rs === null;
    if (rs) {
      ringTrack.style.left = `${rs.leftPct}%`;
      ringTrack.style.width = `${rs.widthPct}%`;
    }
  };

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
      // T-334: ask for the band's own time resolution — `max_t = COLUMNS`, one time cell per drawn
      // bar. The old `max_cells = COLUMNS * 16` product budget bound first and pulled the *day*
      // level (2 cells for 96 bars over 48 h), which is the "truncates rather than re-scales"
      // failure in miniature; the axis budget pulls the hour level (48 cells) instead. `max_cells`
      // is left at its default so the product no longer decides the time axis.
      `/api/history?f_lo=${span.loHz}&f_hi=${span.hiHz}&t0=${now - WINDOW_S}&t1=${now}&max_t=${COLUMNS}`,
    );
    renderBand(grid);
    coverageFraction = grid.coverage_summary?.observed_fraction ?? null;
    coverageGaps = grid.coverage_summary?.gaps ?? null;
    // T-263: the ring's own span, and the events over the retained window. Each is asked for
    // separately and fails separately — a server with no buffer answers 503, and that leaves `ring`
    // null, which reads as "IQ coverage unknown" rather than as "the ring holds nothing".
    ring = await client.get<RingStatus>("/api/iqbuffer").catch(() => null);
    const inv = await client
      .get<{ entries: EventRow[] }>(
        `/api/inventory?f_lo=${span.loHz}&f_hi=${span.hiHz}&t0=${now - WINDOW_S}&t1=${now}&limit=200`,
      )
      .catch(() => null);
    if (inv) eventRows = inv.entries;
    renderMarks();
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
