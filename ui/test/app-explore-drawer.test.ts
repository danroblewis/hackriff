// T-814 (MAP-14): the Explore drawer's pure model + thin-client guard (AWARE-042).
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { groupItems, peekLine, quietItems, strongestItem, surveyItems, unknownItems, type EventsResp } from "../src/app/chrome/explore-drawer";
import { mountExploreDrawer } from "../src/app/chrome/explore-drawer";
import type { SchedulerResponse } from "../src/scheduler";
import type { AppContext } from "../src/app/context";
import { createStore } from "../src/app/store";
import { initialState, type AppState } from "../src/app/state";

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

async function mountedDrawer() {
  const g = globalThis as Record<string, unknown>;
  // The fake document stays installed: row clicks re-render. Nothing else in this file needs a DOM.
  const saved = { setInterval: g.setInterval };
  g.document = { createElement: (t: string) => new FakeEl(t) };
  g.setInterval = () => 0; // the 30 s refresh must not keep node alive
  const calls: string[] = [];
  const replies: Record<string, unknown> = {
    "/api/events": {
      events: [{ emitter_id: "u1", t_start_s: 90, t_end_s: null, open: true, count: 1 }],
      emitters: [{ id: "u1", state: "candidate", f_center_hz: 433.92e6, bandwidth_hz: 50e3, known_status: "unknown", explanations: [] }],
    },
    "/api/coverage": { window: { t0_s: 40, t1_s: 100 }, grid: { cells: 2, f_lo_hz: 400e6, f_cell_hz: 1e6 }, any: { cells: [{ state: "observed" }, { state: "unobserved" }] } },
  };
  const client = { get: async (path: string) => { calls.push(path); const r = replies[path.split("?")[0]]; if (!r) throw new Error("none"); return r; } };
  const store = createStore(initialState());
  store.set(() => ({ live: { ...store.get().live, edgeTS: 100, view: { loHz: 400e6, hiHz: 500e6 } } }));
  const el = new FakeEl("div");
  try {
    mountExploreDrawer(el as unknown as HTMLElement, { store, client, token: "t" } as unknown as AppContext);
    for (let i = 0; i < 10; i++) await new Promise((r) => setImmediate(r));
  } finally { g.setInterval = saved.setInterval; }
  const rows = el.all().filter((e) => e.className === "row");
  const gos = el.all().filter((e) => e.className === "go");
  return { store, calls, el, rows, gos };
}

/** Everything the surface reads to move a pane's view (centre/span/time window) or offer a retune. */
const viewOf = (s: AppState) => JSON.stringify({ nav: s.nav, time: s.time, view: s.live.view, pending: s.live.pendingView, offer: s.live.retuneOffer });

test("P4: a bare click on a drawer row selects (focuses the emitter's box) and never moves the view", async () => {
  const { store, rows, gos, el } = await mountedDrawer();
  assert.equal(rows.length, 2, "one unknown emitter and one survey run");
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
  assert.equal(store.get().time.live, true, "a live emitter's go-to does not freeze the time window");
  gos[1].fire("click");
  assert.equal(store.get().nav.gotoHz, 400.5e6);
  assert.deepEqual(store.get().time, { live: false, tS: 100, spanS: 60 }, "a past survey window is reviewed");
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
