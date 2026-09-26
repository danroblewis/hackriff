// T-1038: **the child level for the centre third is resident before a one-level zoom-in**, so the
// zoom draws with no PENDING quad and asks the route for nothing under the middle of the view.
//
// User, 2026-09-25: "we don't prefetch the multiple levels of the pyramid ... so much UI jankiness".
// Asserted on requests (a spy source) and on PaneReport / rasterised PENDING pixels.

import test from "node:test";
import assert from "node:assert/strict";

import { CELL, PENDING } from "../src/surface/cellrule";
import { extentOf, keyOf, oneTier, tierFor, tilesFor, type Box, type Lattice, type TileAddr } from "../src/surface/lattice";
import { Surface, type PaneView } from "../src/surface/surface";
import { TileCache } from "../src/surface/tilecache";
import type { TileData } from "../src/surface/tile";
import { stubGl } from "./surface-glstub";
import { countColour, rasterize } from "./surface-raster";

const LAT: Lattice = { scheme: "view", cells: 256, f0Hz: 6250, t0Ns: 1e9, levelsF: 20, levelsT: 15 };
const TILE_HZ = LAT.f0Hz * LAT.cells;
const TILE_NS = LAT.t0Ns * LAT.cells;
const W = 1280, H = 800;
const LO = -100, HI = -60;

function tile(a: TileAddr): TileData {
  return {
    addr: a, key: keyOf(a), nf: 2, nt: 2, t1Ns: null, asOfNs: null,
    value: Float32Array.from({ length: 4 }, (_, i) => LO + (i + 1) * 5),
    state: Uint8Array.from({ length: 4 }, () => CELL.OBSERVED),
    tier: "spectrum-history", answeredLevel: a.levelF,
    fold: { frequency: "exact", time: "exact" },
    measured: { nf: 2, nt: 2 }, rangeDb: null, bytes: 4096,
    serverInFlightLimit: null, serverInFlightShare: null,
  };
}
const flush = (): Promise<void> => new Promise((r) => { setImmediate(r); });

const pane = (box: Box): PaneView => ({ id: "p", rect: { x: 0, y: 0, w: W, h: H }, box });
const centreThird = (b: Box): Box => {
  const df = (b.f1Hz - b.f0Hz) / 3, dt = (b.t1Ns - b.t0Ns) / 3;
  return { f0Hz: b.f0Hz + df, f1Hz: b.f1Hz - df, t0Ns: b.t0Ns + dt, t1Ns: b.t1Ns - dt };
};

function rig(pinParents = true) {
  const g = stubGl(W, H);
  const asked: TileAddr[] = [];
  const surface = new Surface(
    g.canvas, LAT,
    (tex) => new TileCache(tex, (a) => { asked.push(a); return Promise.resolve(tile(a)); }, { now: () => 0 }),
    { pinParents },
  );
  surface.setScale(LO, HI);
  return { g, surface, asked };
}

async function warm(r: ReturnType<typeof rig>, p: PaneView) {
  for (let i = 0; i < 10; i++) { r.surface.render([p]); await flush(); }
}

// A box wide enough that its level is > 0 on both axes, so a child level exists.
const BOX: Box = { f0Hz: 0, f1Hz: 16 * TILE_HZ, t0Ns: 0, t1Ns: 12 * TILE_NS };
const ZOOMED: Box = { // one level in: half the span on both axes, about the centre
  f0Hz: 4 * TILE_HZ, f1Hz: 12 * TILE_HZ, t0Ns: 3 * TILE_NS, t1Ns: 9 * TILE_NS,
};

test("a one-level zoom-in on a warm view asks for nothing under the centre third and draws no PENDING", async () => {
  const r = rig();
  await warm(r, pane(BOX));
  const before = tierFor(oneTier(LAT), BOX, W, H), after = tierFor(oneTier(LAT), ZOOMED, W, H);
  assert.equal(before.levelF - after.levelF, 1, "the zoom is exactly one level in frequency");
  assert.ok(before.levelF > 0);

  const n = r.asked.length;
  r.g.reset();
  const reports = r.surface.render([pane(ZOOMED)]);
  const fb = rasterize(r.g.ops, W, H);
  for (let i = 0; i < 4; i++) { await flush(); r.surface.render([pane(ZOOMED)]); }
  const centre = centreThird(ZOOMED);
  const fresh = r.asked.slice(n).filter((a) => {
    const e = extentOf(LAT, a);
    return a.levelF === after.levelF && a.levelT === after.levelT &&
      e.f0Hz < centre.f1Hz && e.f1Hz > centre.f0Hz && e.t0Ns < centre.t1Ns && e.t1Ns > centre.t0Ns;
  });
  assert.deepEqual(fresh.map(keyOf), [], "centre third was already resident: 0 requests");
  assert.equal(reports[0].pending, 0);
  assert.equal(countColour(fb, PENDING, { x: 0, y: 0, w: W, h: H }), 0, "no PENDING pixel on the first zoomed frame");
  // sanity: the zoomed centre-third tiles exist at the child level
  assert.ok(tilesFor(LAT, centre, after.levelF, after.levelT).length > 0);
});

test("RED PROOF: with prefetch off (pinParents false) the same zoom does ask for the centre", async () => {
  const r = rig(false);
  await warm(r, pane(BOX));
  const n = r.asked.length;
  const after = tierFor(oneTier(LAT), ZOOMED, W, H);
  for (let i = 0; i < 4; i++) { r.surface.render([pane(ZOOMED)]); await flush(); }
  const centre = centreThird(ZOOMED);
  const fresh = r.asked.slice(n).filter((a) => {
    const e = extentOf(LAT, a);
    return a.levelF === after.levelF && a.levelT === after.levelT &&
      e.f0Hz < centre.f1Hz && e.f1Hz > centre.f0Hz && e.t0Ns < centre.t1Ns && e.t1Ns > centre.t0Ns;
  });
  assert.ok(fresh.length > 0);
});
