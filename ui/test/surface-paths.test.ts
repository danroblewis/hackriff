// T-897 (docs/23 §10.6 rule 2, ADR-0023 §2): the `paths` layer — traced (t, f) routes on the map.
//
// The claims, each against the degenerate implementation that would otherwise pass:
//  1. **The request the client builds** is one `GET /api/paths` over the union of the boxes of the
//     panes showing the layer, with all four bounds — and NO request when no pane shows it.
//  2. **A vertex is placed by the tiles' own `toClip`**, and the stroke passes through it.
//  3. **No drift under scroll or zoom** (the shared-time-axis invariant): inside `SurfaceView.frame`,
//     every on-screen vertex lies on the stroke at exactly the y the data pass puts the tile row of
//     that capture time — frame after frame as the edge advances and the pane zooms. A control lays
//     the same path out against the PREVIOUS frame's box (the T-388 defect: per-poll layout against
//     a per-frame scroll) and shows it misses, so the test can tell drift from agreement.
//  4. **Stroke-only**: the data pass is byte-identical with the layer on and off; everything the
//     layer submits is a `path-stroke` rectangle a few px thick, off the pane nothing.
//  5. **Toggleable per pane** from the registry, default on, between detections and research.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { CELL } from "../src/surface/cellrule";
import { keyOf, type Lattice, type TileAddr } from "../src/surface/lattice";
import { composeOverlays, defaultPaneLayers, isLayerVisible, layerDef, withLayer } from "../src/surface/layers";
import type { OverlayQuad } from "../src/surface/minimap";
import {
  MAX_PATH_QUADS, PATH_MARK, parsePaths, pathQuads, pathsRequest, type MarkPath,
} from "../src/surface/paths";
import { toClip, type PaneRect } from "../src/surface/surface";
import { TileCache } from "../src/surface/tilecache";
import type { TileData } from "../src/surface/tile";
import { SurfaceView } from "../src/surface/view";
import { stubGl } from "./surface-glstub";

const S = 1e9;
const T0 = 1_700_000_000 * S;
const PANE = { f0Hz: 99.6e6, f1Hz: 102e6, t0Ns: T0 - 20 * S, t1Ns: T0 };
const RECT: PaneRect = { x: 0, y: 0, w: 1000, h: 500 };

/** A chirp from 100.0 to 101.5 MHz over 12 s, ending 4 s before the edge; a hop net beside it. */
const CHIRP: MarkPath = {
  id: "chirp:a", kind: "chirp",
  vertices: [0, 3, 6, 9, 12].map((k) => ({ tNs: T0 - 16 * S + k * S, fHz: 100.0e6 + k * 0.125e6 })),
};
const HOP: MarkPath = {
  id: "hop:b", kind: "hop",
  vertices: [100.2e6, 100.6e6, 100.4e6, 100.8e6, 100.2e6].flatMap((f, k) => [
    { tNs: T0 - 10 * S + k * 0.5 * S, fHz: f }, { tNs: T0 - 10 * S + (k + 1) * 0.5 * S, fHz: f },
  ]),
};

/** A clip-space point is inside a quad, with `slack` clip units of tolerance. */
const inside = (q: OverlayQuad, x: number, y: number, slack = 1e-9) =>
  x >= q.clip[0] - slack && x <= q.clip[2] + slack && y >= q.clip[1] - slack && y <= q.clip[3] + slack;

// ---------------------------------------------------------------------------
// 1. The request the client builds
// ---------------------------------------------------------------------------

test("MAP paths: one GET /api/paths over the union of the showing panes' boxes, all four bounds", () => {
  const a = { f0Hz: 100e6, f1Hz: 101e6, t0Ns: T0 - 20 * S, t1Ns: T0 };
  const b = { f0Hz: 99e6, f1Hz: 100.5e6, t0Ns: T0 - 60 * S, t1Ns: T0 - 30 * S };
  assert.equal(pathsRequest([a, b]), `/api/paths?f_lo=99000000&f_hi=101000000&t0=${(T0 - 60 * S) / S}&t1=${T0 / S}`);
  assert.equal(pathsRequest([a]), `/api/paths?f_lo=100000000&f_hi=101000000&t0=${(T0 - 20 * S) / S}&t1=${T0 / S}`);
  // No pane shows the layer → no request at all.
  assert.equal(pathsRequest([]), null);
  // No live edge yet (a following pane before the first row) and degenerate boxes ask nothing.
  assert.equal(pathsRequest([{ f0Hz: 100e6, f1Hz: 101e6, t0Ns: -20 * S, t1Ns: 0 }]), null);
  assert.equal(pathsRequest([{ ...a, f1Hz: a.f0Hz }]), null);
  // The centre surface builds it from exactly the panes whose `paths` layer is on.
  const src = readFileSync("src/app/centre/surface.ts", "utf8");
  assert.match(src, /pathsRequest\(pv\.view\.panes\.list\(\)\s*\.filter\(\(x\) => isLayerVisible\(layersFor\(x\.id\), "paths"\)\)/);
  // The paths renderer is an entry of the overlay table (asserted on the entry, not the table's
  // literal, so another layer's renderer beside it does not break this check).
  const fns = /overlayFns: Partial<Record<LayerId, OverlayLayerFn>> = \{([^}]*)\}/.exec(src);
  assert.ok(fns, "the overlay renderer table");
  assert.match(fns[1], /\bpaths: pathQuadsFn\b/);
});

test("MAP paths: the wire answer parses to ns vertices; malformed paths and vertices are dropped", () => {
  const got = parsePaths({
    paths: [
      { id: "chirp:1", kind: "chirp", vertices: [{ t_s: 1700000000.5, f_hz: 1e8 }, { t_s: 1700000001.5, f_hz: 1.001e8 }] },
      { id: "hop:2", kind: "radar", vertices: [{ t_s: 1, f_hz: 1 }, { t_s: 2, f_hz: 2 }] },
      { id: "sweep:3", kind: "sweep", vertices: [{ t_s: 1, f_hz: 1 }, { t_s: null, f_hz: 2 }] },
      { kind: "hop", vertices: [] },
    ],
  });
  assert.equal(got.length, 1);
  assert.equal(got[0].kind, "chirp");
  assert.deepEqual(got[0].vertices.map((v) => v.fHz), [1e8, 1.001e8]);
  assert.ok(Math.abs(got[0].vertices[0].tNs - 1700000000.5e9) < 1e3);
  for (const junk of [null, {}, { paths: "x" }, "nope"]) assert.deepEqual(parsePaths(junk), []);
});

// ---------------------------------------------------------------------------
// 2. Placement
// ---------------------------------------------------------------------------

test("MAP paths: every vertex is placed by the tiles' own toClip, and the stroke passes through it", () => {
  const q = pathQuads([CHIRP, HOP], PANE, RECT);
  for (const p of [CHIRP, HOP]) {
    const mine = q.filter((x) => x.id === p.id);
    assert.ok(mine.length > 0, p.id);
    for (const v of p.vertices) {
      const [x, y] = toClip({ f0Hz: v.fHz, f1Hz: v.fHz, t0Ns: v.tNs, t1Ns: v.tNs }, PANE);
      assert.ok(mine.some((m) => inside(m, x, y)), `${p.id} vertex at (${x}, ${y}) is not on its stroke`);
    }
  }
  const src = readFileSync("src/surface/paths.ts", "utf8");
  assert.match(src, /import \{ toClip/, "and it imports the tiles' mapping rather than reimplementing it");
});

test("MAP paths: a stroke is thin — every rectangle hugs the route, a dwell or a jump is ONE rectangle", () => {
  const q = pathQuads([HOP], PANE, RECT, { strokePx: 2 });
  // 5 dwells (constant f) + 4 jumps between them (constant t): nine axis-aligned segments.
  assert.equal(q.length, 9, "an axis-aligned segment is one exact rectangle");
  const chirp = pathQuads([CHIRP], PANE, RECT, { strokePx: 2, stepPx: 3 });
  for (const r of chirp) {
    const wPx = ((r.clip[2] - r.clip[0]) / 2) * RECT.w, hPx = ((r.clip[3] - r.clip[1]) / 2) * RECT.h;
    // One step of 3 px along the major axis, plus the stroke either side.
    assert.ok(Math.min(wPx, hPx) <= 3 + 2 * 2 + 1e-6, `a ${wPx}×${hPx} px rectangle is a wash, not a stroke`);
    assert.equal(r.kind, "path-stroke");
    assert.deepEqual(r.rgba, PATH_MARK);
  }
});

test("MAP paths: off the pane draws nothing; partly on draws only what is on; one path is bounded", () => {
  const away = { ...PANE, f0Hz: 200e6, f1Hz: 201e6 };
  assert.deepEqual(pathQuads([CHIRP, HOP], away, RECT), []);
  const half = { ...PANE, f0Hz: 100.75e6, f1Hz: 102e6 };
  const q = pathQuads([CHIRP], half, RECT);
  assert.ok(q.length > 0);
  for (const r of q) for (const c of r.clip) assert.ok(c >= -1 && c <= 1, "nothing outside the pane");
  // A route a million px long still costs at most the budget.
  const long: MarkPath = { id: "x", kind: "chirp", vertices: [
    { tNs: PANE.t0Ns, fHz: PANE.f0Hz }, { tNs: PANE.t1Ns, fHz: PANE.f1Hz },
  ] };
  const big = { ...RECT, w: 400_000, h: 400_000 };
  assert.ok(pathQuads([long], PANE, big).length <= MAX_PATH_QUADS);
});

// ---------------------------------------------------------------------------
// 3. No drift: inside the frame, on the tiles' rows, under scroll and zoom
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

function view(withPaths: boolean, seen?: { box: typeof PANE; rect: PaneRect; quads: OverlayQuad[] }[]) {
  const g = stubGl(1200, 600);
  let reg = defaultPaneLayers("pane1");
  if (!withPaths) reg = withLayer(reg, "paths", false);
  const v = new SurfaceView({
    canvas: g.canvas, lattice: LAT, bounds: BOUNDS, minimapPx: 150,
    cache: (tex) => new TileCache(tex, (a) => Promise.resolve(tile(a)), { inFlight: 64, now: () => 0 }),
    surface: { pinParents: false },
    freq: { centerHz: 100.8e6, spanHz: 2.4e6 }, spanNs: 20 * S,
    // The centre surface's own composition: the pane's registry, into the one marks hook.
    marks: (pane, edgeNs) => {
      const quads = composeOverlays(reg, { paths: (p) => pathQuads([CHIRP, HOP], p.box, p.rect) }, pane, edgeNs);
      seen?.push({ box: pane.box as typeof PANE, rect: pane.rect, quads });
      return quads;
    },
  });
  return { g, v };
}

test("MAP paths: NO DRIFT — each frame, every vertex sits on its stroke at the tile row of its own capture time, under scroll and zoom", () => {
  const seen: { box: typeof PANE; rect: PaneRect; quads: OverlayQuad[] }[] = [];
  const { v } = view(true, seen);
  const id = v.panes.list()[0].id;
  // Scroll: the pane follows an edge that advances 0.4 s a frame. Zoom: halfway through, the time
  // span and the frequency span both change.
  const edges = [0, 0.4, 0.8, 1.2, 1.6, 2.0, 2.4, 2.8].map((k) => T0 + k * S);
  let checked = 0;
  edges.forEach((edge, i) => {
    if (i === 4) { v.panes.zoomTime(id, 0.6); v.panes.zoomFreq(id, 0.7); }
    const f = v.frame(edge, []);
    const drawn = f.views.find((x) => x.id === id)!;
    const s = seen[seen.length - 1];
    assert.deepEqual(s.box, drawn.box, "the layer was handed the box the data pass drew with, this frame");
    const strokes = s.quads.filter((q) => q.kind === "path-stroke");
    for (const p of [CHIRP, HOP]) {
      for (const vx of p.vertices) {
        // Where the data pass puts the tile row at this vertex's capture time, and this frequency.
        const row = toClip({ f0Hz: vx.fHz, f1Hz: vx.fHz, t0Ns: vx.tNs, t1Ns: vx.tNs }, drawn.box);
        if (Math.abs(row[0]) > 1 || Math.abs(row[1]) > 1) continue; // scrolled or zoomed off
        assert.ok(strokes.some((q) => q.id === p.id && inside(q, row[0], row[1])),
          `frame ${i}: ${p.id} vertex drifted off the row of its capture time`);
        checked++;
      }
    }
  });
  assert.ok(checked > 40, `the test must actually check on-screen vertices (${checked})`);

  // CONTROL — the T-388 defect: lay the path out against the previous frame's box (a poll's view
  // of the pane) while the tiles scroll on. The vertices land off their rows by the scroll delta.
  const prev = seen[seen.length - 2].box, now = seen[seen.length - 1].box;
  const stale = pathQuads([CHIRP], prev, seen[seen.length - 1].rect);
  const missed = CHIRP.vertices.filter((vx) => {
    const row = toClip({ f0Hz: vx.fHz, f1Hz: vx.fHz, t0Ns: vx.tNs, t1Ns: vx.tNs }, now);
    return Math.abs(row[1]) <= 1 && !stale.some((q) => inside(q, row[0], row[1]));
  });
  assert.ok(missed.length > 0, "the control must fail: a stale box is exactly the drift this test exists to catch");
  v.dispose();
});

// ---------------------------------------------------------------------------
// 4. Stroke-only: the data pass cannot be reached
// ---------------------------------------------------------------------------

test("MAP paths: the data pass is byte-identical with the paths layer on and off", () => {
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

// ---------------------------------------------------------------------------
// 5. The registry
// ---------------------------------------------------------------------------

test("MAP paths: an overlay layer, on by default, between detections and research, toggleable per pane", () => {
  const d = layerDef("paths")!;
  assert.equal(d.plane, "overlay");
  assert.equal(d.visibleByDefault, true);
  assert.ok(layerDef("detections")!.z < d.z && d.z < layerDef("research")!.z);
  const a = defaultPaneLayers("p1");
  const b = withLayer(defaultPaneLayers("p2"), "paths", false);
  assert.equal(isLayerVisible(a, "paths"), true);
  assert.equal(isLayerVisible(b, "paths"), false);
  const fns = { paths: () => pathQuads([CHIRP], PANE, RECT) };
  const pv = { id: "p", box: PANE, rect: RECT } as never;
  assert.ok(composeOverlays(a, fns, pv, T0).length > 0);
  assert.deepEqual(composeOverlays(b, fns, pv, T0), []);
});

test("MAP paths: no signal logic and no clock in the geometry module", () => {
  const src = readFileSync("src/surface/paths.ts", "utf8").replace(/^\s*\/\/.*$/gm, "");
  for (const word of ["Date.now", "performance.now", "fetch(", "client.", "DeviceAction"]) {
    assert.ok(!src.includes(word), `paths.ts must not contain "${word}"`);
  }
});
