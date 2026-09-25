// T-821 (MAP-21, RESEARCH-003): the Research panel — collections as overlay layers, every mark also
// a row. The pure model both surfaces read (rows, filter, sort, the canvas boxes), the selection
// round trip (row → mark, mark → row), and the thin-client guards: selecting, toggling a layer and
// the per-row Go reach no device route, and loading reads only the documented research routes.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import {
  BOOKMARKS_ID, MINE_MARK, RESEARCH_SELECTED_MARK, collectionLayer, collectionVisibleOn, filterRows, parseColor,
  researchMarkBoxes, researchRows, rowKey, selectResearch, setCollectionVisible, setResearchData, setResearchOpen, sortRows,
  type Annotation, type Collection, type Marker,
} from "../src/app/map/research-slice";
import { exportFilename, exportPath, exportText, goRow, loadResearch, placeText, renameBody, rowPath, viewFilterWindow } from "../src/app/map/research";
import { markAt } from "../src/surface/marks";
import { defaultPaneLayers, withLayer } from "../src/surface/layers";
import { createStore } from "../src/app/store";
import { initialState, putPaneLayers } from "../src/app/state";

const coll = (id: string, name: string, extra: Partial<Collection> = {}): Collection =>
  ({ id, name, note: null, color: null, visible: true, reserved: false, member_count: 0, ...extra });
const marker = (id: string, cid: string, f: number, bw: number | null, t: number | null, d: number | null): Marker => ({
  id, collection_id: cid, name: `m ${id}`, note: null, f_center_hz: f, bandwidth_hz: bw,
  f_lo_hz: bw === null ? f : f - bw / 2, f_hi_hz: bw === null ? f : f + bw / 2,
  t_center_s: t, duration_s: d,
  t_start_s: t === null ? null : d === null ? t : t - d / 2, t_end_s: t === null ? null : d === null ? t : t + d / 2,
  provenance: { tier: "live-iq" },
});
const ann = (id: string, cid: string | null, f0: number, f1: number, t0: number, t1: number, label = `a ${id}`): Annotation =>
  ({ id, collection_id: cid, kind: "box", f_lo_hz: f0, f_hi_hz: f1, t0_s: t0, t1_s: t1, label, body: "off raster", provenance: { tier: "spectrum-history" } });

const DATA = {
  collections: [coll(BOOKMARKS_ID, "Bookmarks", { reserved: true }), coll("c1", "FM survey", { color: "#ff0000" }), coll("c2", "Airband", { visible: false })],
  markers: [
    marker("pin", BOOKMARKS_ID, 101.3e6, null, null, null),
    marker("pt", "c1", 88.5e6, null, 1000, null),
    marker("bx", "c2", 118.1e6, 8e3, 1200, 3),
  ],
  annotations: [ann("n1", "c1", 105.54e6, 105.64e6, 900, 1100), ann("n2", null, 433.9e6, 433.95e6, 1300, 1310)],
};

test("every mark is also a row: markers (pin/point/box) and annotations, named by their collection", () => {
  const rows = researchRows(DATA);
  assert.deepEqual(rows.map((r) => r.key), ["marker:pin", "marker:pt", "marker:bx", "annotation:n1", "annotation:n2"]);
  assert.deepEqual(rows.map((r) => r.shape), ["pin", "point", "box", "box", "box"]);
  assert.deepEqual(rows.map((r) => r.collectionName), ["Bookmarks", "FM survey", "Airband", "FM survey", "Unfiled"]);
  const pin = rows[0];
  assert.equal(pin.tStartS, null, "a pin has no time — never a fabricated one");
  assert.equal(placeText(pin).t, "all time (pin)");
  // The extents are the server's own fields, carried through untouched.
  assert.equal(rows[2].fLoHz, DATA.markers[2].f_lo_hz);
  assert.equal(rows[2].tEndS, DATA.markers[2].t_end_s);
});

test("the table filters by kind, text, collection and the view window, and sorts stably", () => {
  const rows = researchRows(DATA);
  const all = { kind: "all" as const, text: "", collectionId: null, window: null };
  assert.equal(filterRows(rows, { ...all, kind: "annotation" }).length, 2);
  assert.deepEqual(filterRows(rows, { ...all, text: "OFF RASTER" }).map((r) => r.id), ["n1", "n2"]);
  assert.deepEqual(filterRows(rows, { ...all, collectionId: "c1" }).map((r) => r.id), ["pt", "n1"]);
  // Window: 100–110 MHz over [950, 1050] — the pin matches every time, the box note overlaps.
  assert.deepEqual(filterRows(rows, { ...all, window: { loHz: 100e6, hiHz: 110e6, t0S: 950, t1S: 1050 } }).map((r) => r.id), ["pin", "n1"]);
  assert.deepEqual(sortRows(rows, "freq", 1).map((r) => r.id), ["pt", "pin", "n1", "bx", "n2"]);
  assert.deepEqual(sortRows(rows, "freq", -1).map((r) => r.id), ["n2", "bx", "n1", "pin", "pt"]);
  assert.deepEqual(sortRows(rows, "time", 1).map((r) => r.id)[0], "pin", "a pin sorts as oldest");
  assert.deepEqual(sortRows(rows, "collection", 1).map((r) => r.collectionName),
    ["Airband", "Bookmarks", "FM survey", "FM survey", "Unfiled"]);
});

test("each collection is a layer: stored default, a pane's override, and the panel switch clears overrides", () => {
  const c2 = DATA.collections[2];
  const reg = defaultPaneLayers("p1");
  assert.equal(collectionVisibleOn(reg, c2), false, "the stored default applies when the pane never toggled it");
  const on = withLayer(reg, collectionLayer("c2"), true);
  assert.equal(collectionVisibleOn(on, c2), true, "the pane's layers-menu override wins");

  const store = createStore(initialState());
  store.set(setResearchData({ ...DATA }));
  store.set(putPaneLayers(on));
  store.set(setCollectionVisible("c2", false));
  const s = store.get();
  assert.equal(s.research.collections.find((c) => c.id === "c2")!.visible, false);
  assert.equal(s.layers.p1.layers.some((l) => l.id === collectionLayer("c2")), false, "the pane override was dropped");
  assert.equal(collectionVisibleOn(s.layers.p1, s.research.collections[2]), false);
});

const PANE = { f0Hz: 80e6, f1Hz: 120e6, t0Ns: 800e9, t1Ns: 1400e9 };
const RECT = { w: 1000, h: 600 };

test("the canvas boxes: a pin spans the pane's time, a point is floored to be visible, the selected mark is amber", () => {
  const rows = researchRows(DATA);
  const colors = new Map(DATA.collections.map((c) => [c.id, parseColor(c.color)] as const));
  const boxes = researchMarkBoxes(rows, colors, () => true, "marker:bx", PANE, RECT);
  const by = new Map(boxes.map((b) => [b.id, b]));
  const pin = by.get("marker:pin")!;
  assert.equal(pin.t0Ns, PANE.t0Ns);
  assert.equal(pin.t1Ns, PANE.t1Ns);
  const pt = by.get("marker:pt")!;
  assert.ok(pt.f1Hz - pt.f0Hz >= 8 * 40e6 / 1000 - 1, "a zero-width point is drawn a few px wide");
  assert.ok(pt.t1Ns! > pt.t0Ns, "and a few px tall");
  assert.ok(Math.abs((pt.t0Ns + pt.t1Ns!) / 2 - 1000e9) < 1, "widened about its own instant");
  assert.deepEqual(by.get("marker:bx")!.rgba, RESEARCH_SELECTED_MARK);
  assert.deepEqual(by.get("marker:pt")!.rgba, [1, 0, 0, 0.9], "a collection's colour is its ink");
  assert.deepEqual(by.get("marker:pin")!.rgba, MINE_MARK);
  assert.ok(boxes.every((b) => b.kind === "research-box"));
  // Visibility is the caller's per-row decision (the pane's layers).
  assert.equal(researchMarkBoxes(rows, colors, (r) => r.collectionId === "c1", null, PANE, RECT).length, 2);
});

test("selection round trip: a row selects its mark, and a click on the drawn mark selects that row", () => {
  const store = createStore(initialState());
  store.set(setResearchData({ ...DATA }));
  // Row → mark.
  store.set(selectResearch(rowKey("marker", "bx")));
  const rows = researchRows(store.get().research);
  const boxes = researchMarkBoxes(rows, new Map(), () => true, store.get().research.selected, PANE, RECT);
  assert.deepEqual(boxes.find((b) => b.id === "marker:bx")!.rgba, RESEARCH_SELECTED_MARK);
  // Mark → row: hit-test the same rectangles that were drawn (surface.ts's `hitAt` path).
  const hit = markAt(boxes, PANE.t1Ns, 105.6e6, 1000e9);
  assert.equal(hit?.id, "annotation:n1");
  store.set(selectResearch(hit!.id));
  store.set(setResearchOpen(true));
  assert.equal(store.get().research.selected, "annotation:n1");
  assert.equal(store.get().research.open, true);
  // A deleted mark takes its selection with it: no ghost highlight.
  store.set(setResearchData({ ...DATA, annotations: [] }));
  assert.equal(store.get().research.selected, null);
});

test("thin client: select, layer switch and Go reach NO route; loading reads only the research GETs", async () => {
  const calls: string[] = [];
  const store = createStore(initialState());
  store.set((s) => ({ live: { ...s.live, view: { loHz: 100e6, hiHz: 110e6 } } }));
  store.set(setResearchData({ ...DATA }));
  const rows = researchRows(store.get().research);
  store.set(selectResearch(rows[1].key));
  store.set(setCollectionVisible("c1", false));
  store.set(goRow(rows[2]));
  const s = store.get();
  assert.equal(s.nav.gotoHz, 118.1e6, "Go centres the view on the mark");
  assert.deepEqual(s.time, { live: false, tS: 1201.5, spanS: 3 }, "and reviews its time window");
  assert.equal(s.research.selected, rows[2].key);
  store.set(goRow(rows[0]));
  assert.equal(store.get().time.live, false, "a pin's Go keeps the pane's time");
  assert.deepEqual(viewFilterWindow(store.get()), { loHz: 100e6, hiHz: 110e6, t0S: 1198.5, t1S: 1201.5 });

  const get = <T,>(p: string): Promise<T> => {
    calls.push(p);
    const page = p.startsWith("/api/collections") ? { collections: DATA.collections, next_cursor: null }
      : p.startsWith("/api/markers") ? (p.includes("cursor=") ? { markers: [DATA.markers[2]], next_cursor: null } : { markers: DATA.markers.slice(0, 2), next_cursor: "2" })
        : { annotations: DATA.annotations, next_cursor: null };
    return Promise.resolve(page as T);
  };
  const d = await loadResearch(get);
  assert.equal(d.markers.length, 3, "paged to the last page by next_cursor");
  assert.equal(d.truncated, false);
  assert.ok(calls.every((c) => /^\/api\/(collections|markers|annotations)\?/.test(c)), calls.join(" "));
  assert.ok(calls.some((c) => c.startsWith("/api/annotations?f_lo=0&f_hi=6000000000&t0=0&t1=")), "annotations need a window");
  // Edits go to the documented authoring routes.
  assert.equal(rowPath(rows[0]), "/api/markers/pin");
  assert.equal(rowPath(rows[3]), "/api/annotations/n1");
  assert.deepEqual(renameBody(rows[0], "x"), { name: "x" });
  assert.deepEqual(renameBody(rows[3], "x"), { label: "x" });
});

test("guard: the panel names no device route and no browser clock", () => {
  for (const f of ["src/app/map/research.ts", "src/app/map/research-slice.ts"]) {
    const src = readFileSync(f, "utf8");
    for (const w of ["/api/control", "/ws/open", "DeviceAction", "Date.now", "performance.now", "toLocaleTimeString"]) {
      assert.ok(!src.includes(w), `${f} names ${w}`);
    }
  }
});

test("T-823 export: one read-only GET, optionally narrowed to a collection; the file is the server's bundle", () => {
  assert.equal(exportPath(null), "/api/research/export");
  assert.equal(exportPath("c 1"), "/api/research/export?collection=c%201");
  assert.equal(exportFilename(null), "hackriff-research.json");
  assert.equal(exportFilename("FM survey #2"), "hackriff-research-fm-survey-2.json");
  const bundle = { format: "hackriff-research-export@1", sigmf: { annotations: [] } };
  assert.deepEqual(JSON.parse(exportText(bundle)), bundle, "nothing recomputed or dropped");
  // Thin client: the export button names only the export route, never a device route.
  const src = readFileSync("src/app/map/research.ts", "utf8");
  assert.ok(src.includes("/api/research/export"));
});
