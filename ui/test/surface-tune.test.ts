// T-898 (docs/23 §10.6 rule 2): the `tune` layer — **where the radio has been**, traced on the map
// as a directions line, one per front end, from the recorded tune intervals the backend serves.
//
// The claims, each against the degenerate implementation that would otherwise pass:
//  1. **The request the client builds** is one `GET /api/tune-history` over the union of the boxes
//     of the panes showing the layer, with all four bounds — and NO request when no pane shows it.
//     Nothing here derives a tune history: the vertices are the backend's.
//  2. **A dwell is a vertical run and a retune is a vertex**, placed by the tiles' own `toClip`.
//  3. **No drift under scroll or zoom** (the shared-time-axis invariant): inside
//     `SurfaceView.frame`, every on-screen vertex lies on the stroke at exactly the y the data pass
//     puts the tile row of that capture time. A control lays the same route out against the
//     PREVIOUS frame's box (the T-388 defect) and shows it misses.
//  4. **One line per front end, labelled by `device_id`**: two radios get two inks and two key
//     rows; a broken route (a gap no record covers) is never joined up.
//  5. **Stroke-only and toggleable per pane**: the data pass is byte-identical with the layer on
//     and off, and the layer is in the registry between the measured paths and the research marks.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { CELL } from "../src/surface/cellrule";
import { keyOf, type Lattice, type TileAddr } from "../src/surface/lattice";
import { composeOverlays, defaultPaneLayers, isLayerVisible, layerDef, withLayer } from "../src/surface/layers";
import type { OverlayQuad } from "../src/surface/minimap";
import { toClip, type PaneRect } from "../src/surface/surface";
import { TileCache } from "../src/surface/tilecache";
import type { TileData } from "../src/surface/tile";
import {
  TUNE_INKS, parseTuneHistory, tuneDevices, tuneHistoryRequest, tuneInk, tuneKeyEntries, tuneQuads,
  type TunePath,
} from "../src/surface/tunepath";
import { SurfaceView } from "../src/surface/view";
import { stubGl } from "./surface-glstub";

const S = 1e9;
const T0 = 1_700_000_000 * S;
const PANE = { f0Hz: 99.6e6, f1Hz: 102e6, t0Ns: T0 - 20 * S, t1Ns: T0 };
const RECT: PaneRect = { x: 0, y: 0, w: 1000, h: 500 };

/** A scripted route: three dwells inside the pane, two retunes — a staircase, two vertices a leg. */
const legs: [number, number, number][] = [
  [T0 - 18 * S, T0 - 12 * S, 100.0e6],
  [T0 - 12 * S, T0 - 6 * S, 101.0e6],
  [T0 - 6 * S, T0 - 1 * S, 100.4e6],
];
const ROUTE: TunePath = {
  id: "tune:hackrf-1:1", device: "hackrf-1", deviceNamed: true, retunes: 2,
  vertices: legs.flatMap(([a, b, f]) => [{ tNs: a, fHz: f }, { tNs: b, fHz: f }]),
};
/** A second radio parked on one frequency the whole time (the several-SDRs rule). */
const OTHER: TunePath = {
  id: "tune:rtl-2:1", device: "rtl-2", deviceNamed: true, retunes: 0,
  vertices: [{ tNs: T0 - 19 * S, fHz: 101.6e6 }, { tNs: T0 - 1 * S, fHz: 101.6e6 }],
};

const inside = (q: OverlayQuad, x: number, y: number, slack = 1e-9) =>
  x >= q.clip[0] - slack && x <= q.clip[2] + slack && y >= q.clip[1] - slack && y <= q.clip[3] + slack;

// ---------------------------------------------------------------------------
// 1. The request the client builds
// ---------------------------------------------------------------------------

test("MAP tune: one GET /api/tune-history over the union of the showing panes' boxes, all four bounds", () => {
  const a = { f0Hz: 100e6, f1Hz: 101e6, t0Ns: T0 - 20 * S, t1Ns: T0 };
  const b = { f0Hz: 99e6, f1Hz: 100.5e6, t0Ns: T0 - 60 * S, t1Ns: T0 - 30 * S };
  assert.equal(
    tuneHistoryRequest([a, b]),
    `/api/tune-history?f_lo=99000000&f_hi=101000000&t0=${(T0 - 60 * S) / S}&t1=${T0 / S}`,
  );
  // No pane shows the layer → no request at all; nor before the first row, nor for an empty box.
  assert.equal(tuneHistoryRequest([]), null);
  assert.equal(tuneHistoryRequest([{ f0Hz: 100e6, f1Hz: 101e6, t0Ns: -20 * S, t1Ns: 0 }]), null);
  assert.equal(tuneHistoryRequest([{ ...a, f1Hz: a.f0Hz }]), null);
  // The centre surface builds it from exactly the panes whose `tune` layer is on, and renders it
  // through the overlay table (asserted on the entry, not the table's literal).
  const src = readFileSync("src/app/centre/surface.ts", "utf8");
  assert.match(src, /tuneHistoryRequest\(pv\.view\.panes\.list\(\)\s*\.filter\(\(x\) => isLayerVisible\(layersFor\(x\.id\), "tune"\)\)/);
  const fns = /overlayFns: Partial<Record<LayerId, OverlayLayerFn>> = \{([\s\S]*?)\}/.exec(src);
  assert.ok(fns, "the overlay renderer table");
  assert.match(fns[1], /\btune: tuneQuadsFn\b/);
});

test("MAP tune: the wire answer parses to ns vertices; malformed routes and vertices are dropped", () => {
  const got = parseTuneHistory({
    paths: [
      { id: "tune:a:1", device: "hackrf-1", device_named: true, retunes: 1, vertices: [
        { t_s: 1700000000.5, f_hz: 1e8, at: "start" }, { t_s: 1700000001.5, f_hz: 1e8, at: "end" },
      ] },
      { id: "tune:b:1", vertices: [{ t_s: 1, f_hz: 1 }, { t_s: 2, f_hz: 2 }] }, // no device
      { id: "tune:c:1", device: "x", vertices: [{ t_s: 1, f_hz: 1 }, { t_s: null, f_hz: 2 }] }, // one usable vertex
      { device: "x", vertices: [] },
    ],
  });
  assert.equal(got.length, 1);
  assert.equal(got[0].device, "hackrf-1");
  assert.equal(got[0].retunes, 1);
  assert.ok(Math.abs(got[0].vertices[0].tNs - 1700000000.5e9) < 1e3);
  for (const junk of [null, {}, { paths: "x" }, "nope"]) assert.deepEqual(parseTuneHistory(junk), []);
});

// ---------------------------------------------------------------------------
// 2. A dwell is a vertical run; a retune is a vertex
// ---------------------------------------------------------------------------

test("MAP tune: a dwell draws as a vertical run and a retune as a jump, through the tiles' own toClip", () => {
  const q = tuneQuads([ROUTE], PANE, RECT);
  assert.ok(q.length > 0);
  for (const v of ROUTE.vertices) {
    const [x, y] = toClip({ f0Hz: v.fHz, f1Hz: v.fHz, t0Ns: v.tNs, t1Ns: v.tNs }, PANE);
    assert.ok(q.some((m) => inside(m, x, y)), `vertex at (${x}, ${y}) is not on the route`);
  }
  // Three dwells (constant frequency) + two retunes (constant time): five axis-aligned rectangles,
  // each exact. A dwell's rectangle is tall and thin; a retune's is wide and thin.
  assert.equal(q.length, 5, "an axis-aligned segment is one exact rectangle");
  const px = (r: OverlayQuad) => [((r.clip[2] - r.clip[0]) / 2) * RECT.w, ((r.clip[3] - r.clip[1]) / 2) * RECT.h];
  const dwells = q.filter((r) => px(r)[0] <= 2 * 2 + 1e-6);
  assert.equal(dwells.length, 3, "one vertical run per dwell");
  for (const r of dwells) assert.ok(px(r)[1] > 20, "a dwell's run spans its whole interval");
  assert.equal(q.filter((r) => px(r)[1] <= 2 * 2 + 1e-6).length, 2, "one jump per retune");
  for (const r of q) assert.equal(r.kind, "path-stroke");
  const src = readFileSync("src/surface/tunepath.ts", "utf8");
  assert.match(src, /import \{ pathQuads/, "it reuses the paths layer's stroking rather than a second one");
});

test("MAP tune: off the pane draws nothing; a broken route is never joined up", () => {
  assert.deepEqual(tuneQuads([ROUTE], { ...PANE, f0Hz: 200e6, f1Hz: 201e6 }, RECT), []);
  // The backend breaks a route at a gap no record covers: two paths, and nothing between them.
  const before: TunePath = { ...ROUTE, id: "tune:hackrf-1:1", vertices: [
    { tNs: T0 - 19 * S, fHz: 100e6 }, { tNs: T0 - 15 * S, fHz: 100e6 } ] };
  const after: TunePath = { ...ROUTE, id: "tune:hackrf-1:2", vertices: [
    { tNs: T0 - 5 * S, fHz: 100e6 }, { tNs: T0 - 1 * S, fHz: 100e6 } ] };
  const q = tuneQuads([before, after], PANE, RECT);
  const gapY = toClip({ f0Hz: 100e6, f1Hz: 100e6, t0Ns: T0 - 10 * S, t1Ns: T0 - 10 * S }, PANE);
  assert.ok(!q.some((r) => inside(r, gapY[0], gapY[1])),
    "nothing is drawn across an interval no tune record covers");
  assert.equal(q.filter((r) => r.id === before.id).length, 1);
  assert.equal(q.filter((r) => r.id === after.id).length, 1);
});

// ---------------------------------------------------------------------------
// 3. One line per front end, labelled by device_id
// ---------------------------------------------------------------------------

test("MAP tune: each front end gets its own ink and its own key row, named by device_id", () => {
  const paths = [ROUTE, OTHER];
  assert.deepEqual(tuneDevices(paths), ["hackrf-1", "rtl-2"]);
  assert.deepEqual(tuneInk("hackrf-1", tuneDevices(paths)), TUNE_INKS[0]);
  assert.deepEqual(tuneInk("rtl-2", tuneDevices(paths)), TUNE_INKS[1]);
  const q = tuneQuads(paths, PANE, RECT);
  const inkOf = (id: string) => q.filter((r) => r.id === id).map((r) => r.rgba);
  assert.ok(inkOf(ROUTE.id).every((c) => JSON.stringify(c) === JSON.stringify(TUNE_INKS[0])));
  assert.ok(inkOf(OTHER.id).every((c) => JSON.stringify(c) === JSON.stringify(TUNE_INKS[1])));
  assert.notDeepEqual(TUNE_INKS[0], TUNE_INKS[1], "two radios are never one colour");
  const key = tuneKeyEntries(paths);
  assert.deepEqual(key.map((e) => e.label), ["hackrf-1", "rtl-2"]);
  assert.match(key[0].note, /2 retunes in view/);
  assert.deepEqual(key[0].rgb, [TUNE_INKS[0][0], TUNE_INKS[0][1], TUNE_INKS[0][2]]);
  // A record that named no radio is its own route, and says so.
  const unknown = tuneKeyEntries([{ ...ROUTE, device: "unknown", deviceNamed: false }]);
  assert.match(unknown[0].label, /unknown \(no device named\)/);
  // No route in the window is stated, never drawn as a swatch for a line nobody can see.
  assert.match(tuneKeyEntries([])[0].note, /No tune record covers this window/);
  // The layers menu takes the key from those entries.
  assert.match(readFileSync("src/app/centre/surface.ts", "utf8"), /tuneKeyEntries\(tunePaths\)/);
});

// ---------------------------------------------------------------------------
// 4. No drift: inside the frame, on the tiles' rows, under scroll and zoom
// ---------------------------------------------------------------------------

const LAT: Lattice = { scheme: "view", cells: 256, f0Hz: 6250, t0Ns: 1e9, levelsF: 20, levelsT: 15 };
const BOUNDS = { f0Hz: 1e6, f1Hz: 6e9, t0Ns: T0 - 86_400 * S, t1Ns: T0 + 3_600 * S };

function tile(a: TileAddr): TileData {
  return {
    addr: a, key: keyOf(a), nf: 2, nt: 2,
    value: new Float32Array([-90, -80, -70, -60]),
    state: new Uint8Array([CELL.OBSERVED, CELL.OBSERVED, CELL.UNOBSERVED, CELL.OBSERVED]),
    tier: "survey-overview", answeredLevel: 1, fold: { frequency: "exact", time: "exact" },
    rangeDb: { lo: -100, hi: -60 }, bytes: 192 * 1024, serverInFlightLimit: null, serverInFlightShare: null,
  };
}

function view(withTune: boolean, seen?: { box: typeof PANE; rect: PaneRect; quads: OverlayQuad[] }[]) {
  const g = stubGl(1200, 600);
  let reg = defaultPaneLayers("pane1");
  if (!withTune) reg = withLayer(reg, "tune", false);
  const v = new SurfaceView({
    canvas: g.canvas, lattice: LAT, bounds: BOUNDS, minimapPx: 150,
    cache: (tex) => new TileCache(tex, (a) => Promise.resolve(tile(a)), { inFlight: 64, now: () => 0 }),
    surface: { pinParents: false },
    freq: { centerHz: 100.8e6, spanHz: 2.4e6 }, spanNs: 20 * S,
    marks: (pane, edgeNs) => {
      const quads = composeOverlays(reg, { tune: (p) => tuneQuads([ROUTE, OTHER], p.box, p.rect) }, pane, edgeNs);
      seen?.push({ box: pane.box as typeof PANE, rect: pane.rect, quads });
      return quads;
    },
  });
  return { g, v };
}

test("MAP tune: NO DRIFT — each frame, every vertex sits on its stroke at the tile row of its own capture time, under scroll and zoom", () => {
  const seen: { box: typeof PANE; rect: PaneRect; quads: OverlayQuad[] }[] = [];
  const { v } = view(true, seen);
  const id = v.panes.list()[0].id;
  const edges = [0, 0.4, 0.8, 1.2, 1.6, 2.0, 2.4, 2.8].map((k) => T0 + k * S);
  let checked = 0;
  edges.forEach((edge, i) => {
    if (i === 4) { v.panes.zoomTime(id, 0.6); v.panes.zoomFreq(id, 0.7); }
    const f = v.frame(edge, []);
    const drawn = f.views.find((x) => x.id === id)!;
    const s = seen[seen.length - 1];
    assert.deepEqual(s.box, drawn.box, "the layer was handed the box the data pass drew with, this frame");
    const strokes = s.quads.filter((q) => q.kind === "path-stroke");
    for (const p of [ROUTE, OTHER]) {
      for (const vx of p.vertices) {
        const row = toClip({ f0Hz: vx.fHz, f1Hz: vx.fHz, t0Ns: vx.tNs, t1Ns: vx.tNs }, drawn.box);
        if (Math.abs(row[0]) > 1 || Math.abs(row[1]) > 1) continue; // scrolled or zoomed off
        assert.ok(strokes.some((q) => q.id === p.id && inside(q, row[0], row[1])),
          `frame ${i}: ${p.id} vertex drifted off the row of its capture time`);
        checked++;
      }
    }
  });
  assert.ok(checked > 30, `the test must actually check on-screen vertices (${checked})`);

  // CONTROL — the T-388 defect: lay the route out against the previous frame's box while the tiles
  // scroll on. The vertices land off their rows by the scroll delta.
  const prev = seen[seen.length - 2].box, now = seen[seen.length - 1].box;
  const stale = tuneQuads([ROUTE], prev, seen[seen.length - 1].rect);
  const missed = ROUTE.vertices.filter((vx) => {
    const row = toClip({ f0Hz: vx.fHz, f1Hz: vx.fHz, t0Ns: vx.tNs, t1Ns: vx.tNs }, now);
    return Math.abs(row[1]) <= 1 && !stale.some((q) => inside(q, row[0], row[1]));
  });
  assert.ok(missed.length > 0, "the control must fail: a stale box is exactly the drift this test exists to catch");
  v.dispose();
});

// ---------------------------------------------------------------------------
// 5. Stroke-only, and toggleable per pane
// ---------------------------------------------------------------------------

test("MAP tune: the data pass is byte-identical with the retune layer on and off", () => {
  const run = (on: boolean) => {
    const { g, v } = view(on);
    const f = v.frame(T0, []);
    assert.equal(f.quads.some((q) => q.kind === "path-stroke"), on, "the toggle acts");
    const ops = g.ops.filter((o) => o.kind === "draw" && o.u && "uBox" in o.u);
    v.dispose();
    return JSON.stringify(ops);
  };
  assert.equal(run(true), run(false));
});

test("MAP tune: the layer is registered per pane, on by default, above the measured paths", () => {
  const d = layerDef("tune")!;
  assert.equal(d.plane, "overlay");
  assert.equal(d.visibleByDefault, true, "after tuning around, the route is there to see");
  assert.ok(d.z > layerDef("paths")!.z && d.z < layerDef("research")!.z);
  assert.equal(d.label, "Retune history");
  const reg = defaultPaneLayers("p1");
  assert.equal(isLayerVisible(reg, "tune"), true);
  assert.equal(isLayerVisible(withLayer(reg, "tune", false), "tune"), false);
  // Per pane: turning it off in one registry leaves another's alone.
  assert.equal(isLayerVisible(defaultPaneLayers("p2"), "tune"), true);
});
