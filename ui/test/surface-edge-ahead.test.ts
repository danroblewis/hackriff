// T-890: a pane FOLLOWING LIVE drew nothing at its live edge for 8-10+ s whenever its tile set
// changed, because the live edge crossed into a new time row.
//
// Observed in a gate and reproduced alone: "2 tiles … drawn to 2.3 s short of the top", then
// "0 tiles · 2 coarse stand-ins · 0 pending · 2 behind the edge · 2 drew nothing" for over ten
// seconds, meanLuma 0.0. The pane's span (2.8 s) was shorter than a level-0 tile (10.24 s), so once
// its old row scrolled out the only thing that could put rows on it was the NEW row's tile — and
// that was not asked for until the edge was already inside it, as a cold miss behind the parent pins
// and everything else the view wanted. The live edge was gated on a tile fetch.
//
// What these assert is the cache's policy, driven the way `Surface.render` + `SurfacePreview`
// drive it each frame (acquire the box's tiles, set the viewports, end the frame, refresh the edge),
// against a route where an ordinary miss is SLOW and a lone revalidation is fast — the shape of a
// busy box: a batch answers when its slowest member does and waits its turn on the history lock,
// while the refresh lane rides alone (T-573). "Drawn" is the renderer's own rule
// (`Surface.drawUpToHorizon`): a resident copy shows rows only up to its horizon `asOfNs`.

import { test } from "node:test";
import assert from "node:assert/strict";
import { CELL } from "../src/surface/cellrule";
import { extentOf, keyOf, tTileNs, tilesFor, type Box, type Lattice, type TileAddr } from "../src/surface/lattice";
import { TileCache, type Viewport } from "../src/surface/tilecache";
import type { TileData } from "../src/surface/tile";

/** The production time floor (T-501): 40 ms cells, so a level-0 tile is 10.24 s. */
const LAT: Lattice = { scheme: "view", cells: 256, f0Hz: 6250, t0Ns: 40e6, levelsF: 20, levelsT: 15 };
const TILE_NS = tTileNs(LAT, 0);
const TILE_HZ = 6250 * 256;
/** The observed pane: shorter than a tile, so it holds ONE row most of the time. */
const SPAN_NS = 2.8e9;
/** The row the run starts in; its start is where the edge begins, plus [[START_INTO_ROW_NS]]. */
const ROW = 170_000_000;
const START_INTO_ROW_NS = 0.2e9;
/** An ordinary (batched) miss on a busy box — the 7.8 s measured alone was worse than this. */
const COLD_MS = 4000;
/** A lone revalidation: the live-edge lane's own cost, which T-491 measures as its minimum. */
const SOLO_MS = 200;
const FRAME_MS = 100;

interface Pending { addr: TileAddr; due: number; issuedEdgeNs: number; resolve: (d: TileData) => void }

/** A tile answered at `asOfNs`: the route's coverage horizon at the instant it was built. */
function tile(a: TileAddr, asOfNs: number): TileData {
  const ext = extentOf(LAT, a);
  return {
    addr: a, key: keyOf(a), nf: 2, nt: 2, t1Ns: ext.t1Ns, asOfNs,
    value: new Float32Array([-90, -80, -70, -60]),
    state: new Uint8Array([CELL.OBSERVED, CELL.OBSERVED, CELL.OBSERVED, CELL.OBSERVED]),
    tier: "live-iq", answeredLevel: 0, fold: { frequency: "exact", time: "exact" },
    measured: { nf: 2, nt: 2 }, rangeDb: { lo: -100, hi: -60 }, bytes: 1024,
    serverInFlightLimit: null, serverInFlightShare: null,
  };
}

/**
 * Follow the live edge for `ms` of simulated time, one frame per [[FRAME_MS]], and report per frame
 * whether the pane drew anything at all and whether the row the edge is in was in hand.
 */
async function follow(ms: number) {
  const clock = { t: 0 };
  const edgeAt = (t: number) => ROW * TILE_NS + START_INTO_ROW_NS + t * 1e6;
  const waiting: Pending[] = [];
  const asked: { key: string; at: number; edgeNs: number; solo: boolean }[] = [];
  const cache = new TileCache<{ id: number }>(
    { upload: () => ({ id: 0 }), destroy: () => {} },
    (addr, _signal, hint) => new Promise<TileData>((resolve) => {
      const solo = hint?.solo === true;
      asked.push({ key: keyOf(addr), at: clock.t, edgeNs: edgeAt(clock.t), solo });
      waiting.push({ addr, due: clock.t + (solo ? SOLO_MS : COLD_MS), issuedEdgeNs: edgeAt(clock.t), resolve });
    }),
    { inFlight: 4, now: () => clock.t, serverMsGuess: COLD_MS },
  );
  const frames: { t: number; edgeNs: number; drew: boolean; edgeRowInHand: boolean }[] = [];
  for (let step = 0; step * FRAME_MS < ms; step++) {
    clock.t = step * FRAME_MS;
    const edgeNs = edgeAt(clock.t);
    const box: Box = { f0Hz: 0.25 * TILE_HZ, f1Hz: 0.75 * TILE_HZ, t0Ns: edgeNs - SPAN_NS, t1Ns: edgeNs };
    const view: Viewport = { box, levelF: 0, levelT: 0 };
    // The renderer's pass: every address in the pane's box, drawn up to each copy's horizon.
    cache.beginFrame();
    let drew = false, edgeRowInHand = false;
    for (const a of tilesFor(LAT, box, 0, 0)) {
      const r = cache.acquire(a);
      if (r.kind !== "resident") continue;
      const ext = extentOf(LAT, a);
      const asOf = r.entry.data.asOfNs ?? ext.t1Ns;
      if (Math.min(asOf, ext.t1Ns) > Math.max(ext.t0Ns, box.t0Ns)) drew = true;
      if (ext.t0Ns <= edgeNs && edgeNs < ext.t1Ns) edgeRowInHand = true;
    }
    cache.setViewports(LAT, [view]);
    cache.endFrame();
    cache.refreshEdge(LAT, edgeNs, [view]);
    frames.push({ t: clock.t, edgeNs, drew, edgeRowInHand });
    await new Promise((r) => setImmediate(r));
    // Answer everything due, each at the horizon the route had when it was asked (under-stating
    // what it holds, as T-495 says a client must assume).
    for (let i = waiting.length - 1; i >= 0; i--) {
      const w = waiting[i];
      if (w.due > clock.t) continue;
      waiting.splice(i, 1);
      w.resolve(tile(w.addr, w.issuedEdgeNs));
      await new Promise((r) => setImmediate(r));
    }
  }
  return { frames, asked, cache };
}

/** The first frame whose edge is in the next row, and the longest run of frames that drew nothing
 * after the pane first drew anything. */
function measure(frames: Awaited<ReturnType<typeof follow>>["frames"]) {
  const crossing = frames.find((f) => f.edgeNs >= (ROW + 1) * TILE_NS)!;
  const firstDrawn = frames.findIndex((f) => f.drew);
  let worst = 0, run = 0;
  for (const f of frames.slice(Math.max(0, firstDrawn))) {
    run = f.drew ? 0 : run + FRAME_MS;
    worst = Math.max(worst, run);
  }
  return { crossing, firstDrawn, worstBlankMs: worst };
}

// The crossing is ~10 s in; follow it for another two spans and a cold miss's worth.
const RUN_MS = TILE_NS / 1e6 - START_INTO_ROW_NS / 1e6 + 2 * SPAN_NS / 1e6 + COLD_MS + 2000;

test("T-890: the row a following edge is about to enter is asked for BEFORE the edge enters it", async () => {
  const { frames, asked } = await follow(RUN_MS);
  const { crossing } = measure(frames);
  const nextRow = tilesFor(LAT, { f0Hz: 0.25 * TILE_HZ, f1Hz: 0.75 * TILE_HZ, t0Ns: (ROW + 1) * TILE_NS, t1Ns: (ROW + 1) * TILE_NS + 1e6 }, 0, 0);
  assert.equal(nextRow.length, 1, "the pane is one tile column wide");
  const first = asked.find((q) => q.key === keyOf(nextRow[0]));
  assert.ok(first, "the next row was never asked for at all");
  assert.ok(first.edgeNs < (ROW + 1) * TILE_NS,
    `the next row was first asked for ${((first.edgeNs - (ROW + 1) * TILE_NS) / 1e9).toFixed(2)} s AFTER the edge ` +
    "entered it — a cold miss issued at the crossing is what left the pane drawing nothing (T-890)");
  assert.ok(crossing.edgeRowInHand,
    `at the frame the edge entered row ${ROW + 1} (t=${crossing.t} ms) its tile was not in hand: the live edge ` +
    "waits on a tile fetch at every row boundary");
});

test("T-890: a following pane shorter than a tile NEVER draws nothing across a row boundary", async () => {
  const { frames } = await follow(RUN_MS);
  const { firstDrawn, worstBlankMs, crossing } = measure(frames);
  assert.ok(firstDrawn >= 0, "the pane never drew at all — the harness proves nothing");
  assert.ok(frames.indexOf(crossing) > firstDrawn, "the pane first drew after the crossing — the harness proves nothing");
  // One frame of slack: a revalidation's answer lands between frames.
  assert.ok(worstBlankMs <= FRAME_MS,
    `the pane drew NOTHING for ${worstBlankMs} ms at a stretch while following live (the crossing is at ` +
    `t=${crossing.t} ms): the rows it had scrolled out and the new row's tile was still a cold fetch — ` +
    "\"0 tiles · 2 coarse stand-ins · 2 drew nothing\"");
});

test("T-890: the look-ahead asks each next row ONCE, and never for a frozen pane", async () => {
  const { asked, cache } = await follow(RUN_MS);
  const nextKey = keyOf({ device: "any", scheme: "view", levelF: 0, levelT: 0, fIndex: 0, tIndex: ROW + 1, cells: 256 });
  const cold = asked.filter((q) => q.key === nextKey && !q.solo);
  assert.equal(cold.length, 1, `the next row was fetched ${cold.length} times as a miss; once is the budget`);
  assert.ok(cache.stats.aheadIssued >= 1);

  // Frozen: the same cache told nothing follows. A new row is never asked ahead of a frozen edge.
  const before = cache.stats.aheadIssued;
  const clock0 = asked.length;
  cache.refreshEdge(LAT, (ROW + 5) * TILE_NS - 1e6, []);
  assert.equal(cache.stats.aheadIssued, before);
  assert.equal(asked.length, clock0);
});
