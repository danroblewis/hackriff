// Capture timeline mount (ADR-0013 §3.3, §4.4, §8). Owner: T-150. Always-on, scrubbable capture
// band; the time cursor drives T-152's review render and T-151's inventory queries. "Record IQ" is
// the interim label for "Export clip from the buffer" (it records forward from now, not from the
// retained past).
//
// T-338 — the band is `GET /api/timeline`: its **span** is the IQ ring's configured retention (the
// capture window, ADR-0014), and its **content** is a compressed sideways overview waterfall the
// backend measured and folded. Neither is decided here. Before this, the span was a hard-coded 48 h
// and the content a client-side max over every frequency cell of `/api/history` — a scrubber that
// offered times the ring had overwritten, filled with a reduction the client had no business making.
import type { AppContext, AreaMounts } from "../context";
import { h } from "../dom";
import { startPoll } from "../net";
import { selectionStoreFor } from "../explore/selections";
import { focusSelection } from "../explore/slice";
import { goLive, reviewAt, setCaptureWindow, toast, type AppState } from "../state";
import {
  DRAG_PX, agoText, bufferedSpan, captureWindow, coverageText, currentSpan, durationText, eventMarkTitle,
  eventMarks, observedFraction, overviewShade, pctForAgo, scrubDataNote, scrubToTime, selectionSpans, timeRegionName,
  timeWindowFromScrub, type CaptureWindow, type CoverageGap, type EventRow, type OverviewResponse,
  type TimelineResponse,
} from "./timeline";

/** Columns and frequency rows the band asks the backend to fold the capture window onto: the cells
 * it will draw, one to one. The backend serves exactly this shape (docs/api.md `/api/timeline`), so
 * nothing is reduced or interpolated here. */
const COLUMNS = 192;
const ROWS = 16;

interface HistoryResponse {
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

  // The band is itself a data display (the user's invariant): a compressed sideways overview
  // waterfall of the retained capture, time along the long axis. It is drawn at the served grid's
  // own cell count and stretched up by CSS — upscaling repeats a measured value across pixels,
  // which is the honest direction; smoothing is off so the browser never invents one between them.
  const canvas = h("canvas", { class: "cap-overview", role: "img", "aria-label": "Overview of the retained capture window" }) as HTMLCanvasElement;
  const selLayer = h("div", { class: "cap-sel-layer" });
  // T-263 (ADR-0017 TM-7): the past events a user scrubs *to*, and the IQ ring that still backs a
  // scrub-back (ADR-0014). Both are placed from timespans the API served; neither is drawn at all
  // until its poll has answered, so "not asked yet" never renders as "nothing happened".
  const marksLayer = h("div", { class: "cap-marks" });
  const ringTrack = h("div", { class: "cap-ring", hidden: true });
  const playhead = h("div", { class: "playhead" });
  const livePill = h("button", { class: "live-pill", type: "button" }, "● LIVE");
  const band = h("div", { class: "cap-band" }, canvas, marksLayer, ringTrack, selLayer, playhead, livePill);

  el.replaceChildren(head, band);

  // The capture window the whole band is laid out on, from the backend. `null` = not answered, or
  // this server has no capture window: the band then scrubs nothing rather than inventing a span.
  let win: CaptureWindow | null = null;

  // ---- scrub / LIVE ----
  let dragging = false, downX = 0, downPct = 0;
  const pctFromEvent = (e: PointerEvent) => {
    const r = band.getBoundingClientRect();
    return r.width > 0 ? ((e.clientX - r.left) / r.width) * 100 : 100;
  };
  const scrub = (e: PointerEvent) => {
    if (!win) return;
    // The live edge is the capture clock's, not the browser's: a replay runs on its own clock, and
    // anchoring the band to `Date.now()` would place its capture in the future.
    const res = scrubToTime(pctFromEvent(e), win.t1S, win.spanS);
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
    if (!win) return;
    const w = timeWindowFromScrub(pctA, pctB, win.t1S, win.spanS);
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
    const focus = store.get().focus;
    if (!win) { selLayer.replaceChildren(); return; }
    selLayer.replaceChildren(...selectionSpans(store.get().selections.list, win.t1S, win.spanS).map((sp) => {
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
  let eventRows: EventRow[] | null = null;

  const renderNote = () => {
    const t = store.get().time;
    band.classList.toggle("reviewing", !t.live);
    if (t.live || !win) {
      noteB.textContent = "viewing live";
      playhead.style.left = "100%";
    } else {
      const agoS = Math.max(0, win.t1S - t.tS);
      noteB.textContent = `reviewing ${agoText(agoS)} ago`;
      playhead.style.left = `${pctForAgo(agoS, win.spanS)}%`;
    }
    // T-263: what the scrubbed window is actually backed by. "Nothing was on the air" and "no data
    // for this window" are different claims, and the note says which one applies.
    const data = t.live ? "" : scrubDataNote(t.tS, false, win, coverageGaps);
    noteRest.textContent = ` · ${coverageText(coverageFraction, win?.spanS ?? null)}${data ? ` · ${data}` : ""} · press LIVE to return`;
  };
  store.select((s) => s.time, renderNote, { immediate: true });

  // T-263: past events and what the ring holds. A mark is one timespan a row reported; it carries
  // no liveness, because a `recurrence` appearance never measured one (see timeline.ts).
  const renderMarks = () => {
    marksLayer.replaceChildren(...(eventRows === null || win === null ? [] : eventMarks(eventRows, win.t1S, win.spanS).map((m) => {
      const e = h("div", { class: `cap-mark ${m.state}`, title: eventMarkTitle(m, win!.t1S) });
      e.style.left = `${m.leftPct}%`;
      e.style.width = `${m.widthPct}%`;
      return e;
    })));
    const rs = bufferedSpan(win);
    ringTrack.hidden = rs === null;
    if (rs) {
      ringTrack.style.left = `${rs.leftPct}%`;
      ringTrack.style.width = `${rs.widthPct}%`;
    }
  };

  // ---- the overview waterfall (GET /api/timeline) ----
  // One canvas pixel per served cell, time across, frequency up. Nothing is reduced here: the grid
  // arrives at exactly the shape asked for, and `overviewShade` normalises against the range the
  // backend measured rather than against the numbers that happen to be in hand.
  const renderBand = (grid: OverviewResponse | null) => {
    const ctx = canvas.getContext("2d");
    if (!ctx) return;
    if (!grid || grid.nt <= 0 || grid.nf <= 0) {
      canvas.width = 1;
      canvas.height = 1;
      ctx.clearRect(0, 0, 1, 1);
      return;
    }
    canvas.width = grid.nt;
    canvas.height = grid.nf;
    const img = ctx.createImageData(grid.nt, grid.nf);
    for (let t = 0; t < grid.nt; t++) {
      for (let f = 0; f < grid.nf; f++) {
        const v = overviewShade(grid, t * grid.nf + f);
        // Frequency runs up the short axis, so row 0 of the grid is the bottom of the canvas.
        const p = ((grid.nf - 1 - f) * grid.nt + t) * 4;
        if (v === null) {
          // Not observed: grey, and never the colour scale's low end — a gap is not a quiet band.
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
    ctx.putImageData(img, 0, 0);
  };

  startPoll(async () => {
    const span = spanOf(store.get());
    // The capture window is asked for even with nothing tuned: the band can say how long it spans
    // before it can say what was in it.
    const q = span ? `?f_lo=${span.loHz}&f_hi=${span.hiHz}&columns=${COLUMNS}&rows=${ROWS}` : "";
    const tl = await client.get<TimelineResponse>(`/api/timeline${q}`).catch(() => null);
    win = captureWindow(tl);
    // T-379: the window the band is laid out on is the *whole UI's* window, so it goes in the store
    // rather than staying local to this mount. Every other surface — the inventory lists, the
    // survey strip, the waterfall's backfill — reads its live edge from here, on the capture clock,
    // instead of each inventing one from `Date.now()`.
    store.set(setCaptureWindow(win));
    renderBand(tl?.grid ?? null);
    coverageFraction = observedFraction(tl?.grid);
    if (win && span) {
      // T-263: gaps and the events over the capture window — both asked for over the window the
      // backend just reported, so the marks and the band can never disagree about what it spans.
      const hist = await client
        .get<HistoryResponse>(`/api/history?f_lo=${span.loHz}&f_hi=${span.hiHz}&t0=${win.t0S}&t1=${win.t1S}&max_t=${COLUMNS}`)
        .catch(() => null);
      coverageGaps = hist?.coverage_summary?.gaps ?? null;
      const inv = await client
        .get<{ entries: EventRow[] }>(
          `/api/inventory?f_lo=${span.loHz}&f_hi=${span.hiHz}&t0=${win.t0S}&t1=${win.t1S}&limit=200`,
        )
        .catch(() => null);
      if (inv) eventRows = inv.entries;
    }
    renderMarks();
    renderNote();
    renderSelSpans();
    // The band's own label says how long it is, so a reconfigured retention is visible as a
    // different window rather than as the same box holding different data.
    band.title = win ? `Capture window: ${durationText(win.spanS)} of retained IQ` : "No capture window on this server";
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
