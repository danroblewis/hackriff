// T-443, the zoomable minimap. Four claims, and the first one is the design:
//
//  1. **It is another viewport, not a fifth widget.** It goes into the same `Surface.render` call as
//     the panes, at its own level, and reads the same tiles from the same LRU. So there is no
//     private fetch path to drift — the thing that kept failing about the old navigators was keeping
//     a second implementation in step with the main view.
//  2. **Pane rectangles are re-derived every frame from pane state**, through the renderer's own
//     `toClip`. No change event, because re-laying-out on one against a per-frame scroll is T-388's
//     box-jump.
//  3. **The overlay pass cannot tint a measurement.** The T-437 spike's own minimap comparison
//     failed because a translucent wash was drawn over the sample point; here the data draws are
//     byte-identical with overlays on and off, and every overlay quad is a stroke, not a wash.
//  4. **The level is stated per viewport** (docs/16 §8.5a), the minimap included, because a 6 GHz
//     map is nearly always at a different level from a pane and the difference is legitimate.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { CELL } from "../src/surface/cellrule";
import { keyOf, type Lattice, type TileAddr } from "../src/surface/lattice";
import {
  Minimap, activeWindows, coverageDeviceOf, liveSegmentQuads, paneOutlineQuads, quadSizePx,
  type ActiveWindow,
} from "../src/surface/minimap";
import { activeWindows as navigatorsActiveWindows } from "../src/navigators";
import { readoutOf } from "../src/surface/chrome";
import { Surface } from "../src/surface/surface";
import { TileCache } from "../src/surface/tilecache";
import type { TileData } from "../src/surface/tile";
import { SurfaceView } from "../src/surface/view";
import { stubGl, type GlOp } from "./surface-glstub";

const LAT: Lattice = { scheme: "view", cells: 256, f0Hz: 6250, t0Ns: 1e9, levelsF: 20, levelsT: 15 };
const S = 1e9;
const T0 = 1_700_000_000 * S;
const BOUNDS = { f0Hz: 1e6, f1Hz: 6e9, t0Ns: T0 - 86_400 * S, t1Ns: T0 };
const W = 1200, H = 600, MAP = 150;

const flush = () => new Promise((r) => setImmediate(r));

function data(a: TileAddr): TileData {
  return {
    addr: a, key: keyOf(a), nf: 2, nt: 2,
    value: new Float32Array([-90, -80, -70, -60]),
    state: new Uint8Array([CELL.OBSERVED, CELL.OBSERVED, CELL.UNOBSERVED, CELL.OBSERVED]),
    tier: "survey-overview", answeredLevel: 1, fold: { frequency: "exact", time: "exact" },
    rangeDb: { lo: -100, hi: -60 }, bytes: 192 * 1024, serverInFlightLimit: null,
  };
}

function harness(overlays = true) {
  const g = stubGl(W, H);
  const asked: TileAddr[] = [];
  const view = new SurfaceView({
    canvas: g.canvas, lattice: LAT, bounds: BOUNDS,
    cache: (tex) => new TileCache(tex, (a) => { asked.push(a); return Promise.resolve(data(a)); }, { inFlight: 64, now: () => 0 }),
    minimapPx: MAP, overlays,
    surface: { pinParents: false },
    freq: { centerHz: 100.8e6, spanHz: 2.4e6 }, spanNs: 20 * S,
  });
  view.surface.setScale(-100, -60);
  return { g, view, asked };
}

/** Two frames: the first schedules tiles, the second draws them. Exactly what a real loop does. */
async function warm(h: ReturnType<typeof harness>, edgeNs = T0, windows: readonly ActiveWindow[] = []) {
  h.view.frame(edgeNs, windows);
  await flush();
  h.g.reset();
  return h.view.frame(edgeNs, windows);
}

const isOverlay = (o: GlOp) => !!o.u && "uInk" in o.u;
const windowAt = (deviceId: string, centerHz: number, spanHz: number): ActiveWindow =>
  ({ deviceId, driver: "mock", centerHz, spanHz, loHz: centerHz - spanHz / 2, hiHz: centerHz + spanHz / 2 });

// ——— 1. another viewport: same context, same call, same LRU, no private fetch path ———

test("the minimap is drawn by the SAME render call, through the SAME context, at its OWN level", async () => {
  const h = harness();
  const f = await warm(h);
  assert.equal(h.g.contextCount(), 1, "a minimap with its own context would be a second cache and a second ramp");
  assert.equal(f.reports.length, 2, "one pane and the map, reported by one render()");
  const pane = f.reports[0], map = f.reports[1];
  assert.equal(map.id, h.view.minimap.id);
  assert.ok(map.levelF > pane.levelF, "a 6 GHz-wide viewport cannot be at the same frequency level as a 2.4 MHz one");
  assert.ok(map.levelT > pane.levelT, "…nor at the same time level as a 20 s window");
  // The map occupies a strip of the SAME drawing buffer, below the panes.
  assert.deepEqual(f.mapRect, { x: 0, y: 0, w: W, h: MAP });
  assert.ok(f.views[0].rect.y >= MAP, "the panes sit above the map on one canvas");
});

test("no private fetch path: every tile the map wants goes through the one shared cache", async () => {
  const h = harness();
  await warm(h);
  // Everything asked for came from the TileCache's own source, and the map's addresses are in it.
  const mapLevel = h.view.frame(T0).reports[1];
  assert.ok(h.asked.some((a) => a.levelF === mapLevel.levelF && a.levelT === mapLevel.levelT),
    "the map's tiles were never requested — it is not reading the surface at all");
  assert.ok(h.asked.every((a) => a.scheme === LAT.scheme && a.cells === LAT.cells));
  // And the modules that own the map submit no request of their own.
  for (const f of ["src/surface/minimap.ts", "src/surface/view.ts", "src/surface/overlay.ts", "src/surface/chrome.ts"]) {
    const src = readFileSync(f, "utf8");
    assert.equal(/\bfetch\s*\(/.test(src), false, `${f} reaches the network; the minimap must read the shared LRU`);
    assert.equal(/tileUrl/.test(src), false, `${f} builds a tile URL of its own`);
  }
});

test("a whole frame — panes AND map — calls nothing on the wire", async () => {
  const calls: unknown[] = [];
  const g = globalThis as { fetch?: unknown };
  const real = g.fetch;
  g.fetch = (...args: unknown[]) => { calls.push(args); return Promise.reject(new Error("the surface must not reach the network")); };
  try {
    const h = harness();
    await warm(h, T0, [windowAt("hackrf-0", 100.8e6, 2.4e6)]);
    h.view.minimap.zoomFreq(0.25);
    h.view.minimap.panTime(-60 * S);
    h.view.frame(T0 + S);
  } finally { if (real) g.fetch = real; else delete g.fetch; }
  assert.deepEqual(calls, [], "the view reached the network directly; tiles are the cache's business and nothing else's");
});

test("a tile wanted by a pane AND the map at the same level is fetched ONCE", async () => {
  const g = stubGl(W, H);
  const asked: TileAddr[] = [];
  const surface = new Surface(
    g.canvas, LAT,
    (tex) => new TileCache(tex, (a) => { asked.push(a); return Promise.resolve(data(a)); }, { inFlight: 64, now: () => 0 }),
    { pinParents: false },
  );
  const map = new Minimap({ bounds: BOUNDS, lattice: LAT, freq: { centerHz: 100.8e6, spanHz: 2.4e6 }, spanNs: 20 * S });
  const rect = { x: 0, y: 0, w: 600, h: 300 };
  const pane = { id: "pane1", rect: { ...rect, x: 600 }, box: map.box(T0), device: "any" };
  surface.render([pane, map.view(rect, T0)]);
  await flush();
  assert.ok(asked.length > 0);
  assert.equal(new Set(asked.map(keyOf)).size, asked.length, "the same tile was fetched twice: the LRU is not shared");
});

// ——— 2. pane rectangles track pane state EVERY FRAME ———

test("pane rectangles are re-derived from pane state each frame, with no change event to subscribe to", async () => {
  const h = harness();
  await warm(h);
  const outlinesOf = (f: { quads: readonly { kind: string; id: string; clip: readonly number[] }[] }) =>
    f.quads.filter((q) => q.kind === "pane-outline");
  const a = h.view.panes.list()[0].id;
  const before = outlinesOf(h.view.frame(T0));
  assert.ok(before.length >= 3, "a pane inside the map must draw its edges");

  // Move the pane. Nothing is notified, nothing is re-registered: the next frame simply re-reads.
  h.view.panes.panFreq(a, 1.5e9);
  const after = outlinesOf(h.view.frame(T0));
  assert.notDeepEqual(after.map((q) => q.clip), before.map((q) => q.clip), "the rectangle did not follow the pane");

  // …and a pane that is following the live edge keeps its rectangle pinned to the top of the map as
  // the edge grows, because both are laid out through the same time mapping on the same frame.
  const tops = [0, 1, 2, 3].map((i) => {
    const f = h.view.frame(T0 + i * 10 * S);
    return outlinesOf(f).map((q) => q.clip[3]).reduce((m, v) => Math.max(m, v), -Infinity);
  });
  for (const t of tops) assert.ok(Math.abs(t - tops[0]) < 1e-9, "a following pane's rectangle drifted from the live edge");

  const src = readFileSync("src/surface/minimap.ts", "utf8") + readFileSync("src/surface/view.ts", "utf8");
  assert.equal(/addEventListener|subscribe\(|onChange/.test(src), false,
    "T-388: an overlay laid out on a change event drifts against a per-frame scroll, then snaps");
});

test("a pane outside the map's window draws NOTHING, and a straddling one draws only the edges that exist", async () => {
  const h = harness();
  await warm(h);
  const a = h.view.panes.list()[0].id;
  h.view.minimap.setFreq(3e9, 200e6);          // the map looks at a slice that holds no pane
  h.view.panes.setFreq(a, 100.8e6, 2.4e6);
  assert.deepEqual(h.view.frame(T0).quads.filter((q) => q.kind === "pane-outline"), [],
    "a rectangle was drawn for a pane that is not on the map");

  // Straddling: the pane covers the map's whole low edge, so its left edge is off-map and must not
  // be invented at the border.
  h.view.minimap.setFreq(3e9, 200e6);   // 2.90 … 3.10 GHz
  h.view.panes.setFreq(a, 2.85e9, 200e6); // 2.75 … 2.95 GHz: only its high edge is on the map
  const q = h.view.frame(T0).quads.filter((x) => x.kind === "pane-outline");
  assert.ok(q.length > 0 && q.length < 4, `a straddling pane drew ${q.length} edges; it has 4 only if both ends are on the map`);
  for (const e of q) {
    assert.ok(e.clip[0] >= -1 - 1e-9 && e.clip[2] <= 1 + 1e-9, "an edge was drawn outside the map");
  }
});

// ——— 3. the overlay pass cannot tint a measurement ———

test("the DATA pass is byte-identical with overlays on and off — the honesty comparison needs no flag", async () => {
  const on = harness(true), off = harness(false);
  await warm(on, T0, [windowAt("hackrf-0", 100.8e6, 2.4e6)]);
  await warm(off, T0, [windowAt("hackrf-0", 100.8e6, 2.4e6)]);
  const dataDraws = (h: ReturnType<typeof harness>) => h.g.draws().filter((o) => !isOverlay(o));
  assert.ok(dataDraws(on).length > 0);
  assert.deepEqual(dataDraws(on), dataDraws(off),
    "an overlay changed what the measurement pass submitted — the T-437 trap, one layer down");
  assert.ok(on.g.draws().some(isOverlay), "the overlay pass never ran, so the comparison proves nothing");
  assert.equal(off.g.draws().some(isOverlay), false);
  // The overlay draws come strictly AFTER the data draws: the tiles are submitted before the pass
  // that could sit on top of them even exists in the stream.
  const ops = on.g.draws();
  const firstOverlay = ops.findIndex(isOverlay);
  assert.ok(ops.slice(firstOverlay).every(isOverlay), "an overlay draw was interleaved into the data pass");
});

test("every overlay quad is a STROKE, never a wash over a measurement", async () => {
  const h = harness();
  const f = await warm(h, T0, [windowAt("hackrf-0", 100.8e6, 2.4e6), windowAt("hackrf-1", 2.45e9, 20e6)]);
  assert.ok(f.quads.length >= 5);
  for (const q of f.quads) {
    const { wPx, hPx } = quadSizePx(q, f.mapRect!);
    assert.ok(Math.min(Math.abs(wPx), Math.abs(hPx)) <= 4,
      `${q.kind} ${q.id} covers ${wPx.toFixed(1)}×${hPx.toFixed(1)} px of the map: a translucent wash over the point being measured is exactly what made the spike's own comparison fail`);
  }
});

test("the overlay program has no ramp and no cell state: it cannot express a measurement", () => {
  const h = harness();
  h.view.frame(T0, [windowAt("hackrf-0", 100.8e6, 2.4e6)]);
  const overlaySrc = h.g.shaders.filter((s) => /uInk|uQuad/.test(s)).join("\n");
  assert.ok(overlaySrc.length > 0, "the overlay shaders were never compiled");
  assert.equal(/sampler2D|cmap|cellMark/.test(overlaySrc), false, "the overlay pass grew a way to colour a cell");
});

test("two viewports at the SAME level submit identical tile uniforms — the spike's measurement, no overlays disabled", async () => {
  const g = stubGl(W, H);
  const surface = new Surface(
    g.canvas, LAT,
    (tex) => new TileCache(tex, (a) => Promise.resolve(data(a)), { inFlight: 64, now: () => 0 }),
    { pinParents: false },
  );
  surface.setScale(-100, -60);
  const map = new Minimap({ bounds: BOUNDS, lattice: LAT, freq: { centerHz: 100.8e6, spanHz: 2.4e6 }, spanNs: 20 * S });
  const rect = { x: 0, y: 0, w: 600, h: 300 };
  const views = [
    { id: "pane1", rect: { ...rect, x: 600 }, box: map.box(T0), device: "any" },
    map.view(rect, T0),
  ];
  surface.render(views);
  await flush();
  g.reset();
  const reports = surface.render(views);
  assert.equal(reports[0].levelF, reports[1].levelF);
  assert.equal(reports[0].levelT, reports[1].levelT);
  const draws = g.draws();
  const n = draws.length / 2;
  assert.ok(n >= 1 && Number.isInteger(n));
  assert.deepEqual(draws.slice(0, n).map((d) => d.u), draws.slice(n).map((d) => d.u),
    "§8.5a: at the SAME level two viewports sample the same pixels — same ramp, same scale");
});

// ——— per-SDR live segments: the backend's list, and the coverage identity the panes use ———

test("the lit segments read the SAME reported list the frequency navigator does", () => {
  assert.equal(activeWindows, navigatorsActiveWindows, "a second reader of `windows` is free to disagree with the first");
  // …and its rule is unchanged: a body with a tuned `current` but no enumeration lights nothing.
  assert.deepEqual(activeWindows({}), []);
});

test("one lit segment per REPORTED window, per device, carrying the identity the panes' coverage is keyed by", async () => {
  const h = harness();
  assert.deepEqual((await warm(h, T0, [])).quads.filter((q) => q.kind === "live-segment"), [],
    "a segment appeared with no window reported");

  const ws = [windowAt("hackrf-0", 100.8e6, 2.4e6), windowAt("hackrf-1", 2.45e9, 20e6)];
  const f = h.view.frame(T0, ws);
  const lit = f.quads.filter((q) => q.kind === "live-segment");
  assert.equal(lit.length, 2);
  assert.deepEqual(lit.map((q) => q.id), ["hackrf-0", "hackrf-1"], "in the order the backend listed them");
  assert.notDeepEqual(lit[0].rgba, lit[1].rgba, "two front ends must be tellable apart");
  assert.ok(lit[0].clip[0] < lit[1].clip[0], "2.45 GHz is to the right of 100.8 MHz on a frequency axis");

  // The device a segment names is the device a pane's tiles are keyed by — one identity, one
  // coverage plane, not two client-side notions of "which radio".
  h.view.panes.setDevice(h.view.panes.list()[0].id, coverageDeviceOf(ws[0]));
  h.asked.length = 0;
  h.view.frame(T0, ws);
  await flush();
  assert.ok(h.asked.some((a) => a.device === "hackrf-0"), "the pane's coverage is keyed by a different name than the segment");
  assert.equal(coverageDeviceOf({ ...ws[0], deviceId: null }), "any", "an unnamed front end falls back to the union");
});

test("a 2.4 MHz window on a 6 GHz map is still visible, and a window off the map is omitted rather than clamped", () => {
  const map = new Minimap({ bounds: BOUNDS, lattice: LAT });
  const rect = { x: 0, y: 0, w: W, h: MAP };
  const box = map.box(T0);
  const [seg] = liveSegmentQuads([windowAt("hackrf-0", 100.8e6, 2.4e6)], T0, box, rect);
  const { wPx } = quadSizePx(seg, rect);
  assert.ok(wPx >= 2 - 1e-9, `a 0.04 % window rounded away to ${wPx.toFixed(2)} px — the one thing this mark exists to show`);
  assert.ok(wPx <= 4, "the drawing floor must not become a visible span claim");
  // Nothing reads the drawn width back as a span: the measured numbers stay on the ActiveWindow.
  map.setFreq(100.8e6, 2.4e6);
  assert.deepEqual(liveSegmentQuads([windowAt("hackrf-1", 2.45e9, 20e6)], T0, map.box(T0), rect), [],
    "a capture was drawn where it is not");
});

test("a lit segment sits at the LIVE EDGE, so a map scrubbed into the past lights nothing", () => {
  const map = new Minimap({ bounds: BOUNDS, lattice: LAT, spanNs: 600 * S });
  const rect = { x: 0, y: 0, w: W, h: MAP };
  const ws = [windowAt("hackrf-0", 100.8e6, 2.4e6)];
  assert.equal(liveSegmentQuads(ws, T0, map.box(T0), rect).length, 1);
  map.panTime(-3600 * S); // scrub an hour back: "live" is no longer on this picture
  assert.deepEqual(liveSegmentQuads(ws, T0, map.box(T0), rect), [],
    "a bar pinned to the top of the pane regardless of time is the fixed-screen-coordinate overlay the invariant forbids");
});

// ——— 4. the level is stated, and the minimap is in the same list ———

test("the minimap STATES its level in the same readout as the panes, and the note explains the difference", async () => {
  const h = harness();
  const f = await warm(h);
  const map = f.statuses.find((s) => s.id === h.view.minimap.id)!;
  const pane = f.statuses.find((s) => s.id !== h.view.minimap.id)!;
  assert.ok(map.levelLabel.includes(`level ${map.levelF}/${map.levelT}`));
  assert.equal(map.levelF, f.reports.find((r) => r.id === map.id)!.levelF, "the stated level must be the DRAWN level");
  assert.deepEqual(map.differsFrom, [pane.id]);
  assert.ok(f.note && /different pyramid levels/.test(f.note) && /maximum over more cells/.test(f.note),
    "§8.5a: a 6 GHz map is nearly always at a different level, and hiding that is what invites the bug report");

  const r = readoutOf(f.statuses, h.view.minimap.id);
  assert.equal(r.rows.length, 2);
  assert.equal(r.rows.find((x) => x.id === map.id)!.viewport, "minimap");
  assert.equal(r.rows.find((x) => x.id === pane.id)!.viewport, "pane");
  assert.equal(r.note, f.note);
  const mapRow = r.rows.find((x) => x.id === map.id)!;
  assert.ok(/Hz/.test(mapRow.level) && /level \d+\/\d+/.test(mapRow.level), `the map must say its CELL: ${mapRow.level}`);
  assert.ok(/tiles/.test(mapRow.counts) && /pending/.test(mapRow.counts));
});

test("…and when a pane is zoomed out to exactly the map's window there is nothing left to explain", async () => {
  const h = harness();
  await warm(h);
  const a = h.view.panes.list()[0].id;
  // Give the pane the map's own window AND the map's aspect, so both resolve to the same levels.
  h.view.minimapPx = Math.floor(H / 2); // equal halves: same box AND same pixels => same levels
  const box = h.view.minimap.box(T0);
  h.view.panes.setFreq(a, (box.f0Hz + box.f1Hz) / 2, box.f1Hz - box.f0Hz);
  h.view.panes.setFollowing(a, true);
  h.view.panes.zoomTime(a, 1e9); // clamps to the whole retained window, like the map's
  const f = h.view.frame(T0);
  assert.equal(f.reports[0].levelF, f.reports[1].levelF);
  assert.equal(f.reports[0].levelT, f.reports[1].levelT);
  assert.equal(f.note, null, "a note with nothing to explain is noise");
  assert.deepEqual(readoutOf(f.statuses, h.view.minimap.id).note, null);
});

// ——— zoomable, because it is a pane ———

test("the map zooms and pans with the pane model's own arithmetic, clamped to the surface's extent", async () => {
  const h = harness();
  await warm(h);
  const wide = h.view.frame(T0).reports[1];
  h.view.minimap.zoomFreq(0.02);
  const tight = h.view.frame(T0).reports[1];
  assert.ok(tight.levelF < wide.levelF, "zooming the map must resolve it to a finer level — it is a viewport, not a picture");
  h.view.minimap.panFreq(1e12);
  const b = h.view.minimap.box(T0);
  assert.ok(b.f1Hz <= BOUNDS.f1Hz + 1e-6 && b.f0Hz >= BOUNDS.f0Hz - 1e-6, "the map wandered off the device's range");
  // And a point on it resolves against the box that was drawn.
  const mid = h.view.minimap.locate(0.5, 1, T0);
  assert.ok(Math.abs(mid.hz - (b.f0Hz + b.f1Hz) / 2) < 1e-6);
  assert.equal(mid.ns, b.t1Ns);
});

test("pane outlines and live segments are the only overlay kinds, and neither is a cell mark", () => {
  const map = new Minimap({ bounds: BOUNDS, lattice: LAT });
  const rect = { x: 0, y: 0, w: W, h: MAP };
  const box = map.box(T0);
  const quads = [
    ...paneOutlineQuads([{ id: "p", freq: { centerHz: 100.8e6, spanHz: 2.4e6 }, time: { live: true, spanNs: 20 * S }, device: "any" }], T0, box, rect),
    ...liveSegmentQuads([windowAt("hackrf-0", 100.8e6, 2.4e6)], T0, box, rect),
  ];
  assert.ok(quads.length >= 3);
  const cellMarks = readFileSync("src/surface/cellrule.ts", "utf8");
  for (const q of quads) {
    // Bright and saturated: an overlay mark must not be mistakable for a state the backend decided.
    assert.ok(Math.max(q.rgba[0], q.rgba[1], q.rgba[2]) > 0.6, "an overlay mark as dark as a cell mark");
    assert.equal(cellMarks.includes(q.rgba.slice(0, 3).join(", ")), false, "an overlay reused a cell-mark colour");
  }
});
