// T-814 (MAP-14): the Explore drawer's pure model + thin-client guard (AWARE-042).
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { drawerScope, foldSurveyRecords, groupItems, itemsInScope, peekLine, quietItems, scopeKey, scopeLine, strongestItem, surveyItems, pastSurveyItems, surveyWindowItems, neverLookedItems, SurveyLog, unknownItems, type DrawerItem, type EventsResp } from "../src/app/chrome/explore-drawer";
import { PaneModel } from "../src/surface/panes";
import { mountExploreDrawer } from "../src/app/chrome/explore-drawer";
import type { SchedulerResponse } from "../src/scheduler";
import type { Selection } from "../src/selections";
import type { AppContext } from "../src/app/context";
import { createStore } from "../src/app/store";
import { gotoTimeWindow, gotoWindow, initialState, requestGoto, type AppState } from "../src/app/state";

test("unknown emitters lead, newest first, one per emitter; known ones are not listed", () => {
  const r: EventsResp = {
    events: [
      { emitter_id: "a", t_start_s: 10, t_end_s: null, open: true, count: 1 },
      { emitter_id: "b", t_start_s: 20, t_end_s: 21, open: false, count: 3 },
      { emitter_id: "c", t_start_s: 30, t_end_s: 31, open: false, count: 1 },
      { emitter_id: "b", t_start_s: 5, t_end_s: 6, open: false, count: 1 },
    ],
    emitters: [
      { id: "a", state: "candidate", f_center_hz: 105.59e6, bandwidth_hz: 90e3, known_status: "unknown", explanations: [] },
      { id: "b", state: "candidate", f_center_hz: 92.15e6, bandwidth_hz: 12e3, known_status: "unknown", explanations: [{ label: "FM" }] },
      { id: "c", state: "confirmed", f_center_hz: 98e6, bandwidth_hz: 180e3, known_status: "known", explanations: [] },
    ],
  };
  const it = unknownItems(r);
  assert.deepEqual(it.map((i) => i.hz), [92.15e6, 105.59e6]);
  assert.match(it[0].tag, /burst ×3/);
  assert.match(it[1].tag, /on air/);
});

test("strongest, quiet-but-active and survey runs come from the backend's own figures", () => {
  assert.equal(strongestItem({ found: false }).length, 0);
  assert.equal(strongestItem({ found: true, f_center_hz: 101.3e6, max_db: -71.2 })[0].hz, 101.3e6);
  const sch = { poi: [
    { f_lo: 1e6, f_hi: 2e6, observed_cells: 0, observed_fraction: 0, poi: [{ p_poi: 0.9 }] },
    { f_lo: 88e6, f_hi: 108e6, observed_cells: 4, observed_fraction: 1, poi: [{ p_poi: 0.4 }] },
  ] } as unknown as SchedulerResponse;
  assert.equal(quietItems(sch).length, 1, "an unobserved band is never suggested as active");
  const s = surveyItems({ window: { t0_s: 0, t1_s: 100 }, grid: { cells: 5, f_lo_hz: 100e6, f_cell_hz: 1e6 },
    any: { cells: [{ state: "unobserved" }, { state: "observed" }, { state: "observed" }, { state: "unobserved" }, { state: "excluded" }] } });
  assert.deepEqual(s.map((i) => i.title), ["101.000 MHz – 103.000 MHz", "104.000 MHz – 105.000 MHz"]);
  assert.deepEqual(s[0].time, { t0: 0, t1: 100 });
  assert.deepEqual(groupItems([...s, ...strongestItem({ found: true, f_center_hz: 1e8 })]).map((g) => g.group), ["strongest", "surveys"]);
  assert.match(peekLine([]), /nothing to suggest/);
});

test("T-815: past surveys are observed-then windows from the log, merged per band, distinct from never-looked", () => {
  const rec = (lo: number, hi: number, a: number, b: number) => ({ record: "dwell", window: { usable: { lo_hz: lo, hi_hz: hi } }, observed: { start_ns: a * 1e9, end_ns: b * 1e9 } });
  const s = pastSurveyItems({ records: [rec(430e6, 440e6, 100, 160), rec(430e6, 440e6, 200, 260), rec(900e6, 910e6, 5000, 5060), { record: "sweep" } as never] }, 6000);
  assert.equal(s.length, 2);
  assert.deepEqual(s[0].time, { t0: 5000, t1: 5060 }, "newest first");
  assert.deepEqual(s[1].time, { t0: 100, t1: 260 }, "touching dwells of one band merge");
  assert.ok(s.every((i) => i.tag === "survey · observed then" && i.group === "surveys"));
  const n = neverLookedItems({ window: { t0_s: 0, t1_s: 1 }, grid: { cells: 4, f_lo_hz: 100e6, f_cell_hz: 1e6 },
    any: { cells: [{ state: "observed" }, { state: "unobserved" }, { state: "unobserved" }, { state: "observed" }] } });
  assert.equal(n[0].tag, "survey · never looked");
  assert.equal(n[0].time, undefined, "a gap has no window to review");
  assert.deepEqual(pastSurveyItems(null, 0), []);
});

/** A fake `GET /api/observations` over an oldest-first log, with the route's own paging rules
 * (docs/api.md: records overlapping the box, in log order; `limit` ≤ 10000; `next_cursor` offset). */
type Rec = { record: string; window: { usable: { lo_hz: number; hi_hz: number } }; observed: { start_ns: number; end_ns: number } };
function fakeObsServer(log: Rec[]) {
  const calls: { t0: number; t1: number; cursor: number; limit: number }[] = [];
  const serve = (path: string) => {
    const q = new URLSearchParams(path.split("?")[1]);
    const t0 = Number(q.get("t0")), t1 = Number(q.get("t1")), cursor = Number(q.get("cursor") ?? 0);
    const limit = Math.min(Number(q.get("limit") ?? 1000), 10_000);
    calls.push({ t0, t1, cursor, limit });
    const hit = log.filter((r) => r.observed.end_ns / 1e9 > t0 && r.observed.start_ns / 1e9 < t1);
    const next = cursor + limit < hit.length ? cursor + limit : null;
    return { records: hit.slice(cursor, cursor + limit), next_cursor: next };
  };
  return { calls, serve };
}
/** An iterative scan at a 10 s dwell (docs/api.md: ~8,600 records/day): one band per hour, five bands
 * in rotation, so every band-hour is its own survey window. */
const T0 = 1_789_000_000;
function scanLog(fromS: number, toS: number): Rec[] {
  const out: Rec[] = [];
  for (let t = fromS; t < toS; t += 10) {
    const lo = 100e6 + (Math.floor((t - T0) / 3600) % 5) * 10e6;
    out.push({ record: "dwell", window: { usable: { lo_hz: lo, hi_hz: lo + 2e6 } }, observed: { start_ns: t * 1e9, end_ns: (t + 10) * 1e9 } });
  }
  return out;
}

test("T-815 review: with a 7-day, 60k-record oldest-first log the NEWEST surveys are listed, newest first", async () => {
  const edge = T0 + 7 * 86400;
  const log = scanLog(T0, edge);
  assert.ok(log.length >= 60_000);
  const srv = fakeObsServer(log);
  const { store, el } = await mountedDrawer({ edge, obs: srv.serve });
  const survey = el.all().filter((e) => e.tag === "li" && e.children[0]?.children[0]?.textContent === "survey · observed then");
  assert.equal(survey.length, 4);
  const got: number[] = [];
  for (const li of survey) { li.children[1].fire("click"); got.push(gotoTimeWindow(store.get().nav)!.tS); }
  // Each row is a clean 3600 s band-hour; `gotoTimeWindow`'s `tS` is the window's MIDPOINT (T-999
  // review fix), so it sits 1800 s before each window's end.
  assert.deepEqual(got, [edge - 1800, edge - 5400, edge - 9000, edge - 12600],
    "the four newest band-hours, newest first — never the oldest pages' windows");
  assert.deepEqual(gotoTimeWindow(store.get().nav), { tS: edge - 12600, spanS: 3600 });
  assert.ok(srv.calls.every((c) => c.limit === 10_000), "the documented max page");
  assert.equal(srv.calls[0].t1, edge, "the newest slice is read first");
  assert.ok(!el.all().some((e) => e.textContent === "survey · not fully loaded"), "a complete read claims no truncation");
});

test("T-815 review: a refresh reads only what is new since the last read, not the whole 7 days", async () => {
  const edge = T0 + 7 * 86400;
  let log = scanLog(T0, edge);
  const calls: string[] = [];
  const m = await mountedDrawer({ edge, obs: (p) => { calls.push(p); return fakeObsServer(log).serve(p); } });
  const first = calls.length;
  log = scanLog(T0, edge + 30);
  m.store.set(() => ({ live: { ...m.store.get().live, edgeTS: edge + 30 } }));
  await m.tick();
  const again = calls.slice(first).map((p) => new URLSearchParams(p.split("?")[1]));
  assert.equal(again.length, 1, "one request for the new 30 s");
  assert.equal(Number(again[0].get("t1")), edge + 30);
  assert.equal(Number(again[0].get("t0")), edge - 120, "from the last edge, less a short re-read margin");
});

test("T-815 review: a log too dense for the page budget is STATED as truncated, and the newest still leads", async () => {
  const edge = T0 + 3 * 86400;
  const srv = fakeObsServer(scanLog(T0, edge));
  const log = new SurveyLog(3, 5000); // 3 pages of 5000: the newest day (8,640) fits, the next does not
  await log.refresh(async <T,>(p: string) => srv.serve(p) as T, edge);
  assert.equal(log.readThrough, edge);
  assert.equal(log.truncatedBefore, edge - 86400, "the second-newest slice was cut");
  const items = surveyWindowItems(log.wins, edge, 4, log.truncatedBefore);
  assert.equal(items[0].time?.t1, edge, "newest first even when truncated");
  const note = items.at(-1)!;
  assert.equal(note.tag, "survey · not fully loaded");
  assert.equal(note.note, true, "a statement, with no go-to");
  assert.match(note.why, /may be missing/);
});

test("thin client: the drawer source reaches no device route and only GETs", () => {
  const src = readFileSync("src/app/chrome/explore-drawer.ts", "utf8");
  assert.doesNotMatch(src, /\.post\(|\.put\(|\.del\(|\/api\/(device|retune|control)/);
  assert.match(src, /requestGoto/);
});

// ---- docs/23 §10.6 P4: a bare row click selects; only the small per-row "go to" button moves the
// view. Driven through the real mount over a minimal fake DOM (node has none). ----

type Handler = (ev: Record<string, unknown>) => void;
class FakeEl {
  children: FakeEl[] = [];
  attrs: Record<string, string> = {};
  className = ""; textContent = ""; type = "";
  classes = new Set<string>();
  classList = { add: (c: string) => this.classes.add(c), remove: (c: string) => this.classes.delete(c) };
  handlers: Record<string, Handler[]> = {};
  constructor(public tag: string) {}
  append(...c: FakeEl[]) { this.children.push(...c); }
  replaceChildren() { this.children = []; }
  setAttribute(k: string, v: string) { this.attrs[k] = v; }
  addEventListener(t: string, fn: Handler) { (this.handlers[t] ??= []).push(fn); }
  fire(t: string) { for (const fn of this.handlers[t] ?? []) fn({ preventDefault() {}, stopPropagation() {} }); }
  all(): FakeEl[] { return this.children.flatMap((c) => [c, ...c.all()]); }
}

async function mountedDrawer(opts: { edge?: number; obs?: (path: string) => unknown; region?: Selection; events?: unknown } = {}) {
  const g = globalThis as Record<string, unknown>;
  // The fake document stays installed: row clicks re-render. Nothing else in this file needs a DOM.
  const saved = { setInterval: g.setInterval };
  g.document = { createElement: (t: string) => new FakeEl(t) };
  let every: (() => void) | null = null;
  g.setInterval = (fn: () => void) => { every = fn; return 0; }; // the 30 s refresh must not keep node alive
  const calls: string[] = [];
  const replies: Record<string, unknown> = {
    "/api/events": {
      events: [{ emitter_id: "u1", t_start_s: 90, t_end_s: null, open: true, count: 1 }],
      emitters: [{ id: "u1", state: "candidate", f_center_hz: 433.92e6, bandwidth_hz: 50e3, known_status: "unknown", explanations: [] }],
    },
    "/api/coverage": { window: { t0_s: 40, t1_s: 100 }, grid: { cells: 2, f_lo_hz: 400e6, f_cell_hz: 1e6 }, any: { cells: [{ state: "observed" }, { state: "unobserved" }] } },
  };
  if (opts.obs) replies["/api/observations"] = opts.obs;
  const client = { get: async (path: string) => { calls.push(path); const f = replies[path.split("?")[0]]; const r = typeof f === "function" ? f(path) : f; if (!r) throw new Error("none"); return r; } };
  if (opts.events) replies["/api/events"] = opts.events;
  const store = createStore(initialState());
  store.set(() => ({ live: { ...store.get().live, edgeTS: opts.edge ?? 100, view: { loHz: 400e6, hiHz: 500e6 } } }));
  // T-943: a selected region is the drawer's scope, so a test can put one there.
  if (opts.region) store.set(() => ({ selections: { list: [opts.region!], sync: "" }, focus: { kind: "selection", id: opts.region!.id } }));
  const el = new FakeEl("div");
  try {
    mountExploreDrawer(el as unknown as HTMLElement, { store, client, token: "t" } as unknown as AppContext);
    for (let i = 0; i < 10; i++) await new Promise((r) => setImmediate(r));
  } finally { g.setInterval = saved.setInterval; }
  const rows = el.all().filter((e) => e.className === "row");
  const gos = el.all().filter((e) => e.className === "go");
  const flush = async () => { for (let i = 0; i < 20; i++) await new Promise((r) => setImmediate(r)); };
  const tick = async () => { every?.(); await flush(); };
  return { store, calls, el, rows, gos, tick };
}

/** Everything the surface reads to move a pane's view (centre/span/time window) or offer a retune. */
const viewOf = (s: AppState) => JSON.stringify({ nav: s.nav, time: s.time, view: s.live.view, pending: s.live.pendingView, offer: s.live.retuneOffer });

test("P4: a bare click on a drawer row selects (focuses the emitter's box) and never moves the view", async () => {
  const { store, rows, gos, el } = await mountedDrawer();
  assert.equal(rows.length, 3, "one unknown emitter, one survey run, one never-looked gap");
  assert.equal(gos.length, rows.length, "every row has its own go-to button");
  const before = viewOf(store.get());
  for (const r of rows) r.fire("click");
  assert.equal(viewOf(store.get()), before, "a row-body click wrote no requestGoto / reviewAt / view change");
  // Selecting the unknown emitter's row highlights its mark on the map (focus is view state only).
  const fresh = el.all().filter((e) => e.className === "row");
  fresh[0].fire("click");
  assert.deepEqual(store.get().focus, { kind: "signal", id: "u1" });
  assert.equal(viewOf(store.get()), before);
  assert.equal(el.all().filter((e) => e.className === "row")[0].attrs["aria-pressed"], "true");
});

test("P4: the small per-row go-to button is the one that jumps the view (view arithmetic only)", async () => {
  const { store, gos, calls } = await mountedDrawer();
  for (const g of gos) {
    assert.match(g.attrs["aria-label"], /^Go to /, "labelled for keyboard / screen reader");
    assert.equal(g.type, "button", "a real button: reachable by Tab, Enter activates it");
  }
  const seq0 = store.get().nav.seq;
  gos[0].fire("click");
  assert.equal(store.get().nav.gotoHz, 433.92e6);
  assert.equal(store.get().nav.seq, seq0 + 1);
  assert.equal(store.get().nav.gotoTS, null, "a live emitter's go-to names no time window");
  gos[1].fire("click");
  assert.equal(store.get().nav.gotoHz, 400.5e6);
  // T-999: a past survey's time rides the SAME `nav` request as its frequency, not a separate
  // `reviewAt` write — see `gotoTimeWindow` (`app/state.ts`) and the test below that this is what
  // the surface actually moves a pane with.
  // `tS` is the window's MIDPOINT (T-999 review fix: `goTo` takes a centre, so an end-time `tS`
  // left half the window off-screen), not its end: (40+100)/2 = 70.
  assert.deepEqual(gotoTimeWindow(store.get().nav), { tS: 70, spanS: 60 }, "a past survey window is named");
  assert.ok(calls.every((c) => !/\/api\/(device|retune|control)/.test(c)), "never a device route");
});

test("P4 CSS: the go-to target is at least 24 px; P1: the sheet has a visible, labelled dismiss", () => {
  const css = readFileSync("src/app/chrome/sheet.css", "utf8");
  const go = /\.explore-drawer li button\.go \{([^}]*)\}/.exec(css)?.[1] ?? "";
  for (const k of ["min-width", "min-height"]) assert.ok(Number(new RegExp(`${k}: (\\d+)px`).exec(go)?.[1]) >= 24, k);
  const sheet = readFileSync("src/app/chrome/sheet.ts", "utf8");
  assert.match(sheet, /sheet-close/);
  assert.match(sheet, /aria-label", `Close /);
});

// ---- T-906: the T-815 review's follow-ups ----

test("T-906: Go on a past survey restores its frequency SPAN as well as its centre and time window", async () => {
  const srv = fakeObsServer([{ record: "dwell", window: { usable: { lo_hz: 430e6, hi_hz: 440e6 } }, observed: { start_ns: 50e9, end_ns: 90e9 } }]);
  const { store, el } = await mountedDrawer({ obs: srv.serve });
  const li = el.all().find((e) => e.tag === "li" && e.children[0]?.children[0]?.textContent === "survey · observed then")!;
  li.children[1].fire("click");
  assert.deepEqual(store.get().nav.gotoHz, 435e6);
  assert.equal(store.get().nav.gotoSpanHz, 10e6, "the survey's band width, not the pane's old span");
  // T-999: the time half rides the same request — `gotoTimeWindow`, not a separate `reviewAt` write
  // that a later `mirror()` pass could overwrite before it ever reached a pane. `tS` is the MIDPOINT
  // of [50, 90], not the end: `PaneModel.goTo` takes a centre, so an end-time `tS` would land the
  // pane with the whole window in the older half of the frame (the review finding this fixes).
  assert.deepEqual(gotoTimeWindow(store.get().nav), { tS: 70, spanS: 40 });
  // The surface's own step: the request becomes a pane window, snapped by the real PaneModel — BOTH
  // axes, exactly as `centre/surface.ts`'s `store.select((s) => s.nav, ...)` applies them.
  // Bounds wide enough that a centre of 90 s with a 40 s span is not clamped against the capture
  // window's own edge (which would be a second effect on top of the one under test).
  const m = new PaneModel({ bounds: { f0Hz: 1e6, f1Hz: 6e9, t0Ns: 0, t1Ns: 1000e9 }, width: 1000, height: 600,
    freq: { centerHz: 100e6, spanHz: 2e6 }, minSpanHz: 1e3 });
  const id = m.list()[0].id;
  const w = gotoWindow(store.get().nav, m.get(id)!.freq.spanHz)!;
  m.setFreq(id, w.centerHz, w.spanHz);
  assert.deepEqual(m.get(id)!.freq, { centerHz: 435e6, spanHz: 10e6 });
  const t = gotoTimeWindow(store.get().nav)!;
  if (t.spanS !== null) m.zoomTime(id, (t.spanS * 1e9) / Math.max(1, m.get(id)!.time.spanNs), 0.5);
  m.goTo(id, t.tS * 1e9);
  assert.deepEqual(m.get(id)!.time, { live: false, centerNs: 70e9, spanNs: 40e9 },
    "the pane freezes CENTRED on the survey's own window ([50,90], centre 70), not its end (90) " +
    "and not the pane's previous (live) one");
  // A span beyond the device range is a view zoom snapped to the realizable extent, never a retune.
  m.setFreq(id, 3e9, 1e12);
  assert.deepEqual(m.get(id)!.freq, { centerHz: (1e6 + 6e9) / 2, spanHz: 6e9 - 1e6 });
  // A plain go-to (no span, no time) keeps the pane's span and leaves its time alone.
  store.set(requestGoto(101e6));
  assert.deepEqual(gotoWindow(store.get().nav, 2e6), { centerHz: 101e6, spanHz: 2e6 });
  assert.equal(gotoTimeWindow(store.get().nav), null);
  // The surface wires the request through gotoWindow/gotoTimeWindow on every request (keyed on the
  // request, so a second Go to the same centre with a different span still moves the pane).
  const surface = readFileSync("src/app/centre/surface.ts", "utf8");
  assert.match(surface, /store\.select\(\(s\) => s\.nav, \(nav\) =>/);
  assert.match(surface, /gotoWindow\(nav, pane\.freq\.spanHz\)/);
  assert.match(surface, /gotoTimeWindow\(nav\)/);
  assert.match(surface, /p\.view\.panes\.goTo\(p\.activePane, t\.tS \* S_TO_NS\)/);
});

test("T-906: survey sweep passes are listed as past surveys (one row per pass, visited hops only), stated as sweeps", () => {
  const geometries = [{ id: 7, hops: [
    { usable: { lo_hz: 100e6, hi_hz: 115e6 } }, { usable: { lo_hz: 115e6, hi_hz: 130e6 } }, { usable: { lo_hz: 130e6, hi_hz: 145e6 } },
  ] }];
  const sw = (a: number, b: number, visits: { hop: number; observed_ms: number }[], id = "s1") =>
    ({ record: "sweep", survey_id: id, geometry: 7, span: { start_ns: a * 1e9, end_ns: b * 1e9 }, visits });
  const r = { records: [
    sw(1000, 1060, [{ hop: 0, observed_ms: 50 }, { hop: 1, observed_ms: 50 }]),
    sw(1060, 1120, [{ hop: 1, observed_ms: 50 }, { hop: 2, observed_ms: 0 }]), // hop 2 was never heard
    { record: "sweep", survey_id: "lost", geometry: 99, span: { start_ns: 1e12, end_ns: 2e12 }, visits: [{ hop: 0, observed_ms: 5 }] },
  ], geometries } as never;
  const items = pastSurveyItems(r, 1200);
  assert.equal(items.length, 1, "one pass, one row; a sweep with no geometry on the page is skipped, not guessed");
  assert.equal(items[0].tag, "survey · swept then");
  assert.equal(items[0].title, "100.000 MHz – 130.000 MHz", "only the hops actually visited");
  assert.deepEqual(items[0].time, { t0: 1000, t1: 1120 });
  assert.equal(items[0].spanHz, 30e6);
  assert.match(items[0].why, /survey sweep/);
  assert.match(items[0].why, /each step was heard only during its own dwell/);
  // A dwell on the same band never merges into the sweep's row.
  const both = foldSurveyRecords([], [{ record: "dwell", window: { usable: { lo_hz: 100e6, hi_hz: 130e6 } }, observed: { start_ns: 1000e9, end_ns: 1100e9 } }, ...(r as { records: never[] }).records], 120, geometries);
  assert.equal(both.length, 2);
});

test("T-906: the drawer never claims nothing was looked at outside its rows", () => {
  const src = readFileSync("src/app/chrome/explore-drawer.ts", "utf8");
  assert.doesNotMatch(src, /nothing was looked at/);
});

test("T-906: an older slice that failed to load is retried on the next refresh, and the note clears", async () => {
  const edge = T0 + 3 * 86400;
  const srv = fakeObsServer(scanLog(T0, edge));
  let down = true;
  const get = async <T,>(p: string) => {
    const t1 = Number(new URLSearchParams(p.split("?")[1]).get("t1"));
    if (down && t1 < edge) return null; // the network drops every older slice once
    return srv.serve(p) as T;
  };
  const log = new SurveyLog();
  await log.refresh(get, edge);
  assert.equal(log.truncatedBefore, edge - 86400, "stated as not fully loaded");
  const oldest = Math.min(...log.wins.map((w) => w.t0));
  assert.ok(oldest >= edge - 86400 - 3600);
  down = false;
  await log.refresh(get, edge + 30);
  assert.equal(log.truncatedBefore, null, "the note does not stick until the day ages out");
  assert.equal(log.retryBefore, null);
  assert.ok(Math.min(...log.wins.map((w) => w.t0)) <= T0 + 10, "the older days are now listed");
  // A budget-bound truncation is not a failure and is NOT retried every refresh.
  const dense = new SurveyLog(3, 5000);
  const calls: string[] = [];
  await dense.refresh(async <T,>(p: string) => { calls.push(p); return srv.serve(p) as T; }, edge);
  assert.equal(dense.retryBefore, null);
  const n = calls.length;
  await dense.refresh(async <T,>(p: string) => { calls.push(p); return srv.serve(p) as T; }, edge + 30);
  assert.equal(calls.length - n, 1, "only the new 30 s is read");
});


// ---- T-943: the drawer shares the "Selected" sheet with the focus panel, so a selected REGION is
// the window its questions are about. The explorer's staging session (2026-09-25) had a region at
// 98.8226–99.0407 MHz headed by 107.816 / 106.997 / 106.159 MHz and "Strongest 106.166 MHz": the
// drawer had asked about the whole viewed span. ASSERT THE REQUEST THE CLIENT BUILDS (CLAUDE.md:
// "a client asking the backend for the wrong thing" is what no gate covers), not only the render. ----

/** The explorer's region (journal-20260925.md), with no time extent (a frequency-only stroke). */
const REGION: Selection = { id: "s1", name: "Region 1", f_lo: 98_822_600, f_hi: 99_040_700, tags: [], links: [], created: 1, updated: 1 };

test("T-943: a focused region is the drawer's scope — the view otherwise", () => {
  const base = { live: { edgeTS: 1000, view: { loHz: 88e6, hiHz: 108e6 } }, selections: { list: [REGION], sync: "" }, focus: { kind: "none" } };
  const view = drawerScope(base as never)!;
  assert.deepEqual([view.loHz, view.hiHz, view.t0, view.t1, view.region], [88e6, 108e6, 1000 - 1800, 1000, null]);
  const region = drawerScope({ ...base, focus: { kind: "selection", id: "s1" } } as never)!;
  assert.deepEqual([region.loHz, region.hiHz], [98_822_600, 99_040_700]);
  assert.equal(region.region?.id, "s1");
  assert.deepEqual([region.t0, region.t1], [1000 - 1800, 1000], "no time extent: the recent window stands");
  // A region WITH a time extent carries it (a stroke over the canvas makes one).
  const timed = drawerScope({ ...base, selections: { list: [{ ...REGION, t_lo: 500, t_hi: 600 }], sync: "" }, focus: { kind: "selection", id: "s1" } } as never)!;
  assert.deepEqual([timed.t0, timed.t1], [500, 600]);
  // A focused region the page no longer holds falls back to the view, never to nothing.
  assert.equal(drawerScope({ ...base, focus: { kind: "selection", id: "gone" } } as never)!.region, null);
  assert.notEqual(scopeKey(view), scopeKey(region), "a re-scope is visible to the mount");
  assert.match(scopeLine(region), /selected region 98\.823 MHz – 99\.041 MHz only/);
  assert.match(scopeLine(view), /viewed span/);
});

test("T-943: with a region selected, EVERY request the drawer builds carries the region's bounds", async () => {
  const { calls } = await mountedDrawer({ region: REGION, edge: 1000 });
  const band = (path: string) => {
    const q = new URLSearchParams(path.split("?")[1]);
    return [Number(q.get("f_lo")), Number(q.get("f_hi"))];
  };
  const asked = calls.filter((c) => /^\/api\/(events|analysis\/strongest|scheduler|coverage)/.test(c));
  assert.deepEqual(asked.length, 4, `all four questions asked, got ${JSON.stringify(calls)}`);
  for (const c of asked) {
    assert.deepEqual(band(c), [98_822_600, 99_040_700], `${c} was not scoped to the selected region`);
  }
  // The defect's shape: the viewed span (400–500 MHz here) is not what any request asked about.
  assert.ok(!asked.some((c) => /f_lo=400000000/.test(c)), "a request still asked about the whole viewed span");
});

test("T-943: selecting a region re-asks at once, and the drawer states the window it is showing", async () => {
  const m = await mountedDrawer({ edge: 1000 });
  const before = m.calls.length;
  assert.match(m.el.all().find((e) => e.className === "drawer-scope")!.textContent, /viewed span 400\.000 MHz – 500\.000 MHz/);
  m.store.set(() => ({ selections: { list: [REGION], sync: "" }, focus: { kind: "selection", id: "s1" } }));
  for (let i = 0; i < 20; i++) await new Promise((r) => setImmediate(r));
  assert.ok(m.calls.length > before, "the scope changed and nothing was re-asked until the 30 s poll");
  assert.ok(m.calls.slice(before).every((c) => !/f_lo=400000000/.test(c)));
  assert.match(m.el.all().find((e) => e.className === "drawer-scope")!.textContent, /selected region/);
});

test("T-943: a row outside the selected region is not listed, whatever the backend answers", async () => {
  // The server answers with the explorer's four far-away unknowns plus one inside the region; only
  // the inside one may be listed. (A route that ignores `f_lo`/`f_hi` is exactly this case.)
  const far = [107.816e6, 106.997e6, 106.159e6, 107.662e6];
  const events = {
    events: [...far, 98.9e6].map((hz, i) => ({ emitter_id: `e${i}`, t_start_s: 90 + i, t_end_s: null, open: true, count: 1 })),
    emitters: [...far, 98.9e6].map((hz, i) => ({ id: `e${i}`, state: "candidate", f_center_hz: hz, bandwidth_hz: 180e3, known_status: "unknown", explanations: [] })),
  };
  const { el } = await mountedDrawer({ region: REGION, edge: 1000, events });
  const titles = el.all().filter((e) => e.className === "f").map((e) => e.textContent);
  assert.deepEqual(titles, ["98.900 MHz · 180.0 kHz"], `only the region's own signal, got ${JSON.stringify(titles)}`);
  // Without a region the drawer is "places to go": a row in another band is the whole point of it.
  const elsewhere: DrawerItem[] = [{ group: "surveys", tag: "survey · observed then", title: "430–440 MHz", why: "", hz: 435e6, spanHz: 10e6 }];
  assert.equal(itemsInScope(elsewhere, drawerScope({ live: { edgeTS: 1000, view: { loHz: 88e6, hiHz: 108e6 } }, selections: { list: [], sync: "" }, focus: { kind: "none" } } as never)).length, 1);
});
