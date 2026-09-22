// **T-532: a kept answer may not claim the radio never looked at time it never saw.**
//
// The defect, measured on the fidelity floor (T-501) against a real `hk serve` and a real browser:
// the live-edge zone of a following pane read **10–38 % THE grey** over a band the server reported
// fully observed, against **0.2 %** on the coarse-floor control. Per hop, the lag was:
//
//   recorded -> IQ ring          -0.05 s   (the ring is ahead of `navigation.time.latest_s`)
//   ring     -> folded + served  -0.03 s   (`/api/tiles`'s newest non-null row, median of 12)
//   served   -> RESIDENT         +1.3 s    (mean 0.8-2.0 s, worst 4.4 s, browser, 30 s window)
//
// So the rows exist, the route serves them, and the client's copy is a second or two old. That is
// ordinary and unavoidable — a tile is 1.2 MB and the route answers in ~90 ms — and it is NOT what
// made the pixels grey. What made them grey is that a tile's coverage plane is a positive
// `unobserved` claim about the **whole** tile, including rows recorded after the answer was built;
// held for 1.3 s at a 40 ms cell that is ~32 rows of grey at the live edge. At the old 1 s cell the
// same staleness hid inside the cell the edge was already in, which is why the control read 0.2 %
// and why nothing caught this before the floor moved.
//
// Two things are fixed here and this file pins both:
//   1. `/api/tiles` states `coverage.horizon.as_of_s` — how far forward its evidence reaches — and
//      the renderer draws **nothing** past it, leaving the pane's PENDING ground. (Tests 1–5.)
//   2. The live-edge refresh's duty gate is charged to a **pass over the edge**, not to each tile
//      on it, so the period stops scaling with how many tiles the edge happens to be cut into —
//      which is the thing T-501's floor changed (1 tile became 4). That half is a property of the
//      CACHE and is pinned in `surface-cache.test.ts`, next to T-460's own five.

import { test } from "node:test";
import assert from "node:assert/strict";
import { CELL, GREY, PENDING } from "../src/surface/cellrule";
import { keyOf, type Lattice, type TileAddr } from "../src/surface/lattice";
import { Surface, type PaneView } from "../src/surface/surface";
import { TileCache } from "../src/surface/tilecache";
import { decodeTile, type TileData, type TileResponse } from "../src/surface/tile";
import { stubGl } from "./surface-glstub";

const LAT: Lattice = { scheme: "view", cells: 256, f0Hz: 6250, t0Ns: 1e9, levelsF: 20, levelsT: 15 };
const TILE_HZ = LAT.f0Hz * LAT.cells;   // 1.6 MHz
const TILE_NS = LAT.t0Ns * LAT.cells;   // 256 s
const W = 800, H = 600;
const flush = () => new Promise((r) => setImmediate(r));
const near = (a: readonly number[], b: readonly number[]) => a.length >= b.length && b.every((v, i) => Math.abs(a[i] - v) < 1e-6);

/** One tile: half its cells observed, half never-looked-at, and a stated forward horizon. */
function data(a: TileAddr, asOfNs: number | null): TileData {
  return {
    addr: a, key: keyOf(a), nf: 2, nt: 2,
    t1Ns: (a.tIndex + 1) * TILE_NS,
    asOfNs,
    value: new Float32Array([-90, NaN, -70, NaN]),
    state: new Uint8Array([CELL.OBSERVED, CELL.UNOBSERVED, CELL.OBSERVED, CELL.UNOBSERVED]),
    tier: "live-iq", answeredLevel: 0, fold: { frequency: "exact", time: "exact" },
    measured: { nf: 2, nt: 2 }, rangeDb: { lo: -100, hi: -60 },
    bytes: 4 * 1024, serverInFlightLimit: null, serverInFlightShare: null,
  };
}

function harness(asOfNs: number | null) {
  const g = stubGl(W, H);
  const surface = new Surface(
    g.canvas, LAT,
    (tex) => new TileCache(tex, (a) => Promise.resolve(data(a, asOfNs)), { inFlight: 8, now: () => 0 }),
    { pinParents: false },
  );
  surface.setScale(-100, -60);
  return { g, surface };
}

/** One pane over exactly one tile, from the epoch to the tile's end. */
const pane = (): PaneView => ({
  id: "a", rect: { x: 0, y: 0, w: W, h: H },
  box: { f0Hz: 0, f1Hz: TILE_HZ, t0Ns: 0, t1Ns: TILE_NS },
});

/** `uRect` is `[x0, y0, x1, y1]` in the pane's clip space, so `y1` is where the draw stops. */
const tileDraws = (g: ReturnType<typeof stubGl>) =>
  g.draws().filter((d) => d.u?.uKind && d.u.uKind[0] === 0);

test("a resident tile is drawn only as far forward as its own answer reaches", async () => {
  // The answer's evidence stops three quarters of the way up the tile. Everything above it is time
  // the plane was written before, so the copy cannot speak about it at all.
  const h = harness(TILE_NS * 0.75);
  h.surface.render([pane()]);
  await flush();
  h.g.reset();
  const [report] = h.surface.render([pane()]);

  assert.equal(report.tiles, 1, "the tile is resident and must still be drawn");
  assert.equal(report.behind, 1, "a tile whose answer stops short of the pane's edge is BEHIND it");
  assert.equal(report.pending, 0,
    "`behind` is not `pending`: the tile ARRIVED, and a readout calling it pending would report the fetch as outstanding");

  const drawn = tileDraws(h.g);
  assert.equal(drawn.length, 1, `expected one tile draw, got ${drawn.length}`);
  // The pane spans [0, TILE_NS] over clip [-1, 1], so a horizon at 0.75 of it is y = 0.5.
  const y1 = drawn[0].u!.uRect[3];
  assert.ok(Math.abs(y1 - 0.5) < 1e-6,
    `the tile was drawn up to clip y ${y1}, not to its stated horizon (0.5): the renderer is showing a plane past the time it was written`);
});

test("THE GREY STOPS THERE TOO — the newest strip is the pane's PENDING ground, never grey", async () => {
  const h = harness(TILE_NS * 0.75);
  h.surface.render([pane()]);
  await flush();
  h.g.reset();
  h.surface.render([pane()]);

  // Nothing in the frame paints grey, and in particular nothing paints it over the strip: the
  // ground under that strip is the pane's clear, which is PENDING.
  for (const c of h.g.clears()) {
    assert.ok(!near(c.args, [...GREY]),
      "a pane cleared to grey claims the radio never looked at the whole pane (T-437 F3)");
  }
  for (const d of h.g.draws()) {
    assert.ok(!(d.u?.uFlat && near(d.u.uFlat, [...GREY])),
      "the strip past the answer's horizon was painted grey: that is the claim the radio never looked, over rows it recorded");
  }
  const cleared = h.g.clears().filter((c) => near(c.args, [...PENDING]));
  assert.ok(cleared.length > 0, "the pane's ground must be PENDING, which is what the strip falls through to");
});

test("a sealed tile is untouched: an answer that reaches past its own end clips nothing", async () => {
  // The ordinary case, and the one every other suite runs: the horizon is past the tile, so the
  // whole tile draws exactly as before. If this regressed, every historical pane would lose its
  // newest rows.
  const h = harness(TILE_NS * 4);
  h.surface.render([pane()]);
  await flush();
  h.g.reset();
  const [report] = h.surface.render([pane()]);
  assert.equal(report.behind, 0, "a tile whose answer covers its whole extent is not behind anything");
  const drawn = tileDraws(h.g);
  assert.equal(drawn.length, 1);
  assert.ok(Math.abs(drawn[0].u!.uRect[3] - 1) < 1e-6, "the whole tile must be drawn");
});

test("NO stated horizon draws the whole tile — a missing field may never blank a pane", async () => {
  // `as_of_s: null` is "no record touches this band", which is not "reaches everywhere": everything
  // there is honestly grey at every instant. And a server that predates the field says nothing,
  // which must also leave the answer standing — the standing rule that no absent input blanks a
  // pane (`surface-tiers`: "a tile with NO `measured` renders").
  for (const missing of [null, undefined]) {
    const h = harness(missing as null);
    h.surface.render([pane()]);
    await flush();
    h.g.reset();
    const [report] = h.surface.render([pane()]);
    assert.equal(report.behind, 0, `as_of ${String(missing)} must not be read as a horizon`);
    const drawn = tileDraws(h.g);
    assert.equal(drawn.length, 1, `as_of ${String(missing)}: the tile must still be drawn`);
    assert.ok(Math.abs(drawn[0].u!.uRect[3] - 1) < 1e-6,
      `as_of ${String(missing)} blanked the newest rows of a tile that says nothing about a horizon`);
  }
});

test("the horizon is read off the ANSWER, in the answer's own units", () => {
  const addr: TileAddr = { scheme: "view", cells: 2, levelF: 0, levelT: 0, fIndex: 0, tIndex: 0, device: "any" };
  const base = {
    key: { device: "any", scheme: "view", level_f: 0, level_t: 0, f_index: 0, t_index: 0, cells: 2 },
    extent: { nt: 1, nf: 1, t0_s: 10, t1_s: 20 },
    axes: { frequency: { levels: 4, cell_hz: 6250 }, time: { levels: 4, cell_s: 1 } },
    grid: { nt: 1, nf: 1, max_db: [-70] },
    resolution: { source: "live-iq" },
  };
  const withHorizon = (h: unknown): TileResponse =>
    ({ ...base, coverage: { states: ["unobserved", "observed", "unknown"], planes: [{ runs: [1, 1], cells: 1 }], any: { plane: 0 }, horizon: h } }) as unknown as TileResponse;

  assert.equal(decodeTile(addr, withHorizon({ as_of_s: 17.25 })).asOfNs, 17_250_000_000,
    "seconds on the wire, ns in the client — the same reading as `extent.t1_s`");
  for (const bad of [{ as_of_s: null }, { as_of_s: "soon" }, {}, undefined]) {
    assert.equal(decodeTile(addr, withHorizon(bad)).asOfNs, null,
      `an answer that states no readable horizon says NOTHING (${JSON.stringify(bad)}), and nothing said is not a horizon`);
  }
});
