// T-1042 / LSR-1: the renderer draws a following pane's live edge from the **published rows**, and
// the tile lane leaves that extent alone.
//
// What is asserted here is **pixels and requests**, not flags — the two things the user's complaint
// and the tile route's cost are actually about:
//
//  1. **The rows are on the screen.** The newest strip rasterises to the ring's own dB on the one
//     ramp, at the `live-iq` tier (no stipple), through the same program and the same display range
//     as a tile. A test that read `ringRows` off the report would prove the renderer counted.
//  2. **The tile under the rows is never asked for.** The exclusion is measured on the fetch path:
//     the addresses the cache requested. This is the cost half of LSR-1 — the live-edge tile is the
//     one the route re-produces most often.
//  3. **Without the ring the same pane is the old pane**: it asks for that tile, and no pixel of the
//     rows' colour is anywhere on it. This is the control that makes (1) and (2) mean anything.
//  4. **Rows at the edge, tiles below** — the milestone's own words — in one pane, in one frame.
//  5. **A tile only partly under the rows keeps its measurement**: it is still asked for and still
//     drawn, and the rows are drawn over it, so nothing is ever blanked by the exclusion.
//  6. **Zoomed out past the rows' resolution the ring stands aside** rather than painting a NEAREST
//     pick of one row in N, and the tile lane is not excluded then either.
//  7. **A retune rebuilds the ring's texture and releases the old one**, so one band's rows cannot be
//     patched into another's, and a withdrawn source frees the textures.
import test from "node:test";
import assert from "node:assert/strict";
import { cmap } from "../src/cmap";
import { CELL } from "../src/surface/cellrule";
import { LiveRing, type RingFrame } from "../src/surface/livering";
import { extentOf, keyOf, oneTier, type Lattice, type TileAddr } from "../src/surface/lattice";
import { SurfacePreview, type SurfaceProbe } from "../src/surface/preview";
import { Surface, type PaneView } from "../src/surface/surface";
import { TileCache } from "../src/surface/tilecache";
import type { TileData } from "../src/surface/tile";
import { stubGl } from "./surface-glstub";
import { countColour, rasterize, type Rect } from "./surface-raster";

/** 16-cell tiles on the production floor (40 ms × 6250 Hz cells): one tile is 640 ms × 100 kHz. */
const LAT: Lattice = { scheme: "view", cells: 16, f0Hz: 6250, t0Ns: 40e6, levelsF: 20, levelsT: 15 };
const TILE_NS = LAT.t0Ns * LAT.cells;
const TILE_HZ = LAT.f0Hz * LAT.cells;
const F_INDEX = 1000;                       // 100.0–100.1 MHz
const T_INDEX = 2_795_781_250;              // ≈ 1 789 300 000 s, an absolute capture time
const PERIOD = LAT.t0Ns;                    // one published row per finest time cell (T-484)
const NF = 8;                               // the front end's bins across the tuned band

const RING_DB = -65, TILE_DB = -95, LO = -100, HI = -60;
const RING_X = (RING_DB - LO) / (HI - LO), TILE_X = (TILE_DB - LO) / (HI - LO);
const RING_COLOUR = cmap(RING_X);           // `live-iq` has no tier pattern: the bare ramp
const TILE_COLOUR = cmap(TILE_X);

const W = 128, H = 256;
const flush = () => new Promise((r) => setImmediate(r));

/** A fully observed tile at [[TILE_DB]], with no stated horizon, so it draws as served. */
function tile(a: TileAddr): TileData {
  const n = a.cells * a.cells;
  return {
    addr: a, key: keyOf(a), nf: a.cells, nt: a.cells, t1Ns: extentOf(LAT, a).t1Ns, asOfNs: null,
    value: new Float32Array(n).fill(TILE_DB), state: new Uint8Array(n).fill(CELL.OBSERVED),
    tier: "spectrum-history", answeredLevel: a.levelT, fold: { frequency: "exact", time: "exact" },
    measured: { nf: a.cells, nt: a.cells }, rangeDb: { lo: LO, hi: HI }, bytes: n * 3,
    serverInFlightLimit: null, serverInFlightShare: null,
  };
}

interface Harness {
  readonly g: ReturnType<typeof stubGl>;
  readonly surface: Surface;
  readonly asked: TileAddr[];
  /** Render `n` frames, letting the cache's answers land between them. */
  drive(panes: readonly PaneView[], n?: number): Promise<void>;
}

function harness(): Harness {
  const g = stubGl(W, H);
  const asked: TileAddr[] = [];
  const surface = new Surface(
    g.canvas, LAT,
    (tex) => new TileCache(tex, (a) => { asked.push(a); return Promise.resolve(tile(a)); }, { now: () => 0 }),
    { pinParents: false },
  );
  surface.setScale(LO, HI, "test");
  return {
    g, surface, asked,
    async drive(panes, n = 4) { for (let i = 0; i < n; i++) { surface.render(panes); await flush(); } },
  };
}

/** A ring of `rows` published rows ending at `endNs`, over the pane's own band. */
function ringOf(rows: number, endNs: number, band: { f0Hz: number; f1Hz: number }, db = RING_DB): RingFrame {
  const ring = new LiveRing({ rows });
  for (let i = 0; i < rows; i++) {
    ring.push({ ...band, db: new Float32Array(NF).fill(db), tNs: endNs - (rows - i) * PERIOD });
  }
  const f = ring.frame();
  assert.ok(f, "the ring reported no frame");
  return f;
}

const source = (f: RingFrame | null) => ({ ringFor: () => f });

/** The extent of the level-0 tile at `(F_INDEX, t)`. */
const extent = (t: number) => extentOf(LAT, { device: "any", scheme: "view", levelF: 0, levelT: 0, fIndex: F_INDEX, tIndex: t, cells: LAT.cells });

const BAND = { f0Hz: extent(T_INDEX).f0Hz, f1Hz: extent(T_INDEX).f1Hz };
const EDGE = extent(T_INDEX).t1Ns;

/** The `tIndex`es the cache was asked for. */
const askedRows = (asked: readonly TileAddr[]) => [...new Set(asked.map((a) => a.tIndex))].sort();

test("LSR-1: a following pane's live edge is painted from the rows, and the tile under them is never asked for", async () => {
  const h = harness();
  const box = { ...BAND, t0Ns: EDGE - TILE_NS, t1Ns: EDGE };
  const pane: PaneView[] = [{ id: "p1", rect: { x: 0, y: 0, w: W, h: H }, box }];
  h.surface.setLiveRings(source(ringOf(LAT.cells, EDGE, BAND)));
  await h.drive(pane);
  h.g.reset();
  const [report] = h.surface.render(pane);

  // 2. the cost claim: the pane is entirely under the rows, so the tile lane asked for NOTHING.
  assert.deepEqual(h.asked, [], `the ring-covered tile was still requested: ${h.asked.map(keyOf).join(", ")}`);
  assert.ok(report.ringTiles > 0, "no address was excluded, so the ring's extent reached the fetch path not at all");
  assert.equal(report.ringRows, LAT.cells);
  assert.ok(report.ringRowPx >= 1, `one row was ${report.ringRowPx} px`);
  assert.equal(report.tiles, 0);
  assert.equal(report.shortNs, 0, "the pane did not report reaching its own live edge");

  // 1. the picture claim: the pane is the rows' colour, on the one ramp, with no tier stipple.
  const fb = rasterize(h.g.ops, W, H);
  const all: Rect = { x: 0, y: 0, w: W, h: H };
  assert.equal(countColour(fb, RING_COLOUR, all), W * H, "the pane was not painted from the rows");
  assert.equal(countColour(fb, TILE_COLOUR, all), 0);
});

test("LSR-1: without the ring, the same pane asks for that tile and paints no row (the control)", async () => {
  const h = harness();
  const box = { ...BAND, t0Ns: EDGE - TILE_NS, t1Ns: EDGE };
  const pane: PaneView[] = [{ id: "p1", rect: { x: 0, y: 0, w: W, h: H }, box }];
  await h.drive(pane);
  h.g.reset();
  const [report] = h.surface.render(pane);
  assert.equal(askedRows(h.asked).length, 1, "the pane drew no tile at all, so the control proves nothing");
  assert.equal(report.ringRows, 0);
  assert.equal(report.ringTiles, 0);
  assert.equal(report.ringRowPx, 0);
  const fb = rasterize(h.g.ops, W, H);
  const all: Rect = { x: 0, y: 0, w: W, h: H };
  assert.equal(countColour(fb, RING_COLOUR, all), 0, "a pane with no ring drew the rows' colour");
  assert.ok(countColour(fb, TILE_COLOUR, all) > 0, "the tile did not draw");
});

test("LSR-1: rows at the edge, tiles below — in one pane, in one frame", async () => {
  const h = harness();
  // Two tiles of time: the ring holds the newest one's worth of rows, the pyramid answers the rest.
  const box = { ...BAND, t0Ns: EDGE - 2 * TILE_NS, t1Ns: EDGE };
  const pane: PaneView[] = [{ id: "p1", rect: { x: 0, y: 0, w: W, h: H }, box }];
  h.surface.setLiveRings(source(ringOf(LAT.cells, EDGE, BAND)));
  await h.drive(pane);
  h.g.reset();
  const [report] = h.surface.render(pane);

  // The older tile was asked for; the live one — wholly under the rows — was not.
  assert.deepEqual(askedRows(h.asked), [T_INDEX - 1], `asked: ${h.asked.map(keyOf).join(", ")}`);
  assert.equal(report.ringTiles, 1);
  assert.equal(report.ringRows, LAT.cells);
  assert.equal(report.tiles, 1);
  assert.equal(report.shortNs, 0);

  // Time runs UP the pane (newest at the top, GL's origin at the bottom): the top half is rows, the
  // bottom half is the tile, and neither colour is in the other's half.
  const fb = rasterize(h.g.ops, W, H);
  const top: Rect = { x: 0, y: H / 2, w: W, h: H / 2 };
  const bottom: Rect = { x: 0, y: 0, w: W, h: H / 2 };
  assert.equal(countColour(fb, RING_COLOUR, top), W * (H / 2), "the newest half was not painted from rows");
  assert.equal(countColour(fb, RING_COLOUR, bottom), 0, "the rows were painted over history");
  assert.ok(countColour(fb, TILE_COLOUR, bottom) > 0, "the history half lost its tile");
  assert.equal(countColour(fb, TILE_COLOUR, top), 0);
});

test("LSR-1: a tile only PARTLY under the rows is still asked for, still drawn, and the rows are drawn over it", async () => {
  const h = harness();
  const box = { ...BAND, t0Ns: EDGE - TILE_NS, t1Ns: EDGE };
  const pane: PaneView[] = [{ id: "p1", rect: { x: 0, y: 0, w: W, h: H }, box }];
  // Only the newest quarter of the pane is in the ring.
  h.surface.setLiveRings(source(ringOf(LAT.cells / 4, EDGE, BAND)));
  await h.drive(pane);
  h.g.reset();
  const [report] = h.surface.render(pane);
  assert.deepEqual(askedRows(h.asked), [T_INDEX], "the partly-covered tile was excluded: its lower rows would be blank");
  assert.equal(report.ringTiles, 0);
  assert.equal(report.tiles, 1);
  assert.equal(report.ringRows, LAT.cells / 4);
  const fb = rasterize(h.g.ops, W, H);
  const top: Rect = { x: 0, y: (3 * H) / 4, w: W, h: H / 4 };
  const below: Rect = { x: 0, y: 0, w: W, h: (3 * H) / 4 };
  assert.equal(countColour(fb, RING_COLOUR, top), W * (H / 4), "the rows did not win where they overlap the tile");
  assert.equal(countColour(fb, RING_COLOUR, below), 0);
  assert.ok(countColour(fb, TILE_COLOUR, below) > 0, "the part the rows do not reach lost its measurement");
});

test("LSR-1: the rows are drawn where the ring's BAND is, and grey is never reached", async () => {
  const h = harness();
  // A pane twice as wide as the tuned band, band on the left half: the rows may not spill, and the
  // spectrum the ring never held is answered by the pyramid like any other.
  const box = { f0Hz: BAND.f0Hz, f1Hz: BAND.f0Hz + 2 * TILE_HZ, t0Ns: EDGE - TILE_NS, t1Ns: EDGE };
  const pane: PaneView[] = [{ id: "p1", rect: { x: 0, y: 0, w: W, h: H }, box }];
  h.surface.setLiveRings(source(ringOf(LAT.cells, EDGE, BAND)));
  await h.drive(pane);
  h.g.reset();
  const [report] = h.surface.render(pane);
  assert.deepEqual(askedRows(h.asked), [T_INDEX], "the tile beside the band was excluded by a ring that says nothing about it");
  assert.equal(report.ringTiles, 1, "the tile INSIDE the band was not excluded");
  const fb = rasterize(h.g.ops, W, H);
  const left: Rect = { x: 0, y: 0, w: W / 2, h: H };
  const right: Rect = { x: W / 2, y: 0, w: W / 2, h: H };
  assert.equal(countColour(fb, RING_COLOUR, left), (W / 2) * H, "the band's half was not painted from rows");
  assert.equal(countColour(fb, RING_COLOUR, right), 0, "the rows spilled outside the tuned band");
  assert.ok(countColour(fb, TILE_COLOUR, right) > 0);
});

test("LSR-1: zoomed out past the rows' resolution the ring stands aside, and excludes nothing", async () => {
  const h = harness();
  // 512 tiles of time in 256 px: one 40 ms row is 1/128 of a pixel.
  const box = { ...BAND, t0Ns: EDGE - 512 * TILE_NS, t1Ns: EDGE };
  const pane: PaneView[] = [{ id: "p1", rect: { x: 0, y: 0, w: W, h: H }, box }];
  h.surface.setLiveRings(source(ringOf(LAT.cells, EDGE, BAND)));
  await h.drive(pane);
  h.g.reset();
  const [report] = h.surface.render(pane);
  assert.equal(report.ringRows, 0, "a row smaller than a pixel was drawn as the picture");
  assert.equal(report.ringTiles, 0, "the ring excluded a tile it did not draw");
  assert.ok(report.ringRowPx > 0 && report.ringRowPx < 1, `rowPx ${report.ringRowPx}`);
  assert.ok(h.asked.length > 0, "nothing was asked for either: the pane would be blank");
  const fb = rasterize(h.g.ops, W, H);
  assert.equal(countColour(fb, RING_COLOUR, { x: 0, y: 0, w: W, h: H }), 0);
});

test("LSR-1: a retune rebuilds the ring's texture and releases the old one; a withdrawn source frees them", async () => {
  const h = harness();
  const box = { ...BAND, t0Ns: EDGE - TILE_NS, t1Ns: EDGE };
  const pane: PaneView[] = [{ id: "p1", rect: { x: 0, y: 0, w: W, h: H }, box }];
  let frame = ringOf(LAT.cells, EDGE, BAND);
  h.surface.setLiveRings({ ringFor: () => frame });
  await h.drive(pane, 2);
  const planes = h.g.uploads.filter((u) => u.w === NF && u.h === LAT.cells).length;
  assert.equal(planes, 2, "the ring's value and state planes were not uploaded once each");
  const freed = h.g.deleted.length;

  // A retune: a new band, a new epoch. The texture is rebuilt rather than patched across bands.
  const other = { f0Hz: 433e6, f1Hz: 433e6 + TILE_HZ };
  const ring = new LiveRing({ rows: LAT.cells });
  for (let i = 0; i < LAT.cells; i++) {
    ring.push({ ...BAND, db: new Float32Array(NF).fill(RING_DB), tNs: EDGE - (LAT.cells - i) * PERIOD });
  }
  ring.clear();
  for (let i = 0; i < LAT.cells; i++) {
    ring.push({ ...other, db: new Float32Array(NF).fill(RING_DB), tNs: EDGE + (i + 1) * PERIOD });
  }
  frame = ring.frame()!;
  assert.ok(frame.epoch > 0);
  h.surface.render([{ id: "p1", rect: { x: 0, y: 0, w: W, h: H }, box: { ...other, t0Ns: EDGE, t1Ns: EDGE + TILE_NS } }]);
  assert.equal(
    h.g.uploads.filter((u) => u.w === NF && u.h === LAT.cells).length, planes + 2,
    "the new band's rows were patched into the old band's texture",
  );
  assert.ok(h.g.deleted.length > freed, "the old band's texture was leaked");

  // Withdrawing the source releases what is left: no ring, no texture.
  const before = h.g.deleted.length;
  h.surface.setLiveRings(null);
  assert.ok(h.g.deleted.length > before, "a withdrawn ring source left its textures behind");
});

// ——— the host's half: WHICH panes have a ring ———

/**
 * A probe good enough to mount a preview: the bounds and the opening window are the only parts this
 * claim is about, and both are stated here rather than fetched (`probeSurface` is T-450's own test).
 */
function probeFor(): SurfaceProbe {
  const bounds = { f0Hz: 1e6, f1Hz: 6e9, t0Ns: EDGE - 3600e9, t1Ns: EDGE };
  return {
    lattice: LAT, lattices: oneTier(LAT),
    origin: { bounds, edgeNs: EDGE, provenance: { freq: "test", time: "test" } },
    census: { observed: 0, unobserved: 4096, unknown: 0, total: 4096, box: null },
    opening: {
      freq: { centerHz: (BAND.f0Hz + BAND.f1Hz) / 2, spanHz: TILE_HZ },
      centerNs: EDGE - TILE_NS / 2, spanNs: TILE_NS, onCoverage: true,
    },
    range: { lo: LO, hi: HI, source: "test" },
    note: "test", requests: [], degraded: [],
  };
}

test("LSR-1: only a FOLLOWING pane paints from the ring — pausing one hands it back to the pyramid", () => {
  const g = stubGl(W, 600);
  // A wide ring, so the pane's own window is inside it whatever the model snapped to.
  const wide = { f0Hz: BAND.f0Hz - TILE_HZ, f1Hz: BAND.f1Hz + TILE_HZ };
  const preview = new SurfacePreview({
    canvas: g.canvas, probe: probeFor(), token: "t", minimapPx: 0,
    // Every tile refused: this test is about the ring's lane, and a refused tile cannot be mistaken
    // for one (`drawRefused` is its own mark, never a measurement).
    fetchFn: async () => ({ ok: false, status: 503, statusText: "busy", json: async () => ({}) } as unknown as Response),
    edge: () => EDGE,
    liveRing: () => ringOf(LAT.cells, EDGE, wide),
  });
  const id = preview.activePane;
  assert.equal(preview.view.panes.isFollowing(id), true, "the opening pane does not follow, so the claim is untestable");
  const live = preview.frame().reports.find((r) => r.id === id)!;
  assert.ok(live.ringRows > 0, "a following pane painted no rows");
  assert.ok(live.ringTiles > 0, "a following pane's live-edge tile was still asked for");

  // Pausing a pane IS its time window (T-347/T-442): it becomes a view over recorded data, which the
  // pyramid answers — and a live row painted into it would be a live claim about a frozen window.
  preview.view.panes.pause(id, EDGE);
  assert.equal(preview.view.panes.isFollowing(id), false);
  const frozen = preview.frame().reports.find((r) => r.id === id)!;
  assert.equal(frozen.ringRows, 0, "a frozen pane was painted from live rows");
  assert.equal(frozen.ringTiles, 0, "a frozen pane still excluded a tile the ring is not drawing");
  preview.dispose();
});
