// T-440: the shared tile LRU — the reason the surface is ONE WebGL2 context.
//
// The load-bearing assertion is the first one: a tile wanted by eight panes is fetched once and
// uploaded once. T-437 measured that on real WebGL2 (95 distinct keys -> 95 uploads, 18.68 MB
// shared against 149.44 MB per-pane); this is the same claim as a property of the code, so it
// cannot quietly stop being true.

import { test } from "node:test";
import assert from "node:assert/strict";
import { CELL } from "../src/surface/cellrule";
import { keyOf, tileUrl, type Lattice, type TileAddr } from "../src/surface/lattice";
import { RECOVER_AFTER, TileCache, parseKey, type TileCacheOptions, type Viewport } from "../src/surface/tilecache";
import { TileBusyError, type TileData } from "../src/surface/tile";

const LAT: Lattice = { scheme: "view", cells: 256, f0Hz: 6250, t0Ns: 1e9, levelsF: 20, levelsT: 15 };
const BYTES = 192 * 1024; // one 256^2 tile: R16F measurement + R8 state

const addr = (fIndex: number, tIndex = 0, levelF = 0, levelT = 0): TileAddr =>
  ({ device: "any", scheme: "view", levelF, levelT, fIndex, tIndex, cells: 256 });

function data(a: TileAddr, bytes = BYTES): TileData {
  return {
    addr: a, key: keyOf(a), nf: 2, nt: 2,
    value: new Float32Array([-90, NaN, -70, NaN]),
    state: new Uint8Array([CELL.OBSERVED, CELL.UNOBSERVED, CELL.OBSERVED, CELL.UNKNOWN]),
    tier: "spectrum-history", answeredLevel: 1, fold: { frequency: "exact", time: "exact" },
    measured: { nf: 2, nt: 2 },
    rangeDb: { lo: -100, hi: -60 }, bytes, serverInFlightLimit: null,
  };
}

const flush = () => new Promise((r) => setImmediate(r));

function harness(opts: TileCacheOptions = {}) {
  const calls: string[] = [];
  /** The URL each request would actually be sent to — assert on what is REQUESTED (T-442/T-454). */
  const urls: string[] = [];
  const waiting = new Map<string, { resolve: (d: TileData) => void; reject: (e: unknown) => void }>();
  let uploads = 0, destroys = 0;
  const cache = new TileCache<{ id: number }>(
    { upload: () => ({ id: uploads++ }), destroy: () => { destroys++; } },
    // The signal is honoured, because the whole of T-454 is what an abort does and does not do.
    (a, signal) => new Promise<TileData>((resolve, reject) => {
      calls.push(keyOf(a));
      urls.push(tileUrl(a));
      waiting.set(keyOf(a), { resolve, reject });
      signal?.addEventListener("abort", () => {
        waiting.delete(keyOf(a));
        reject(Object.assign(new Error("aborted"), { name: "AbortError" }));
      });
    }),
    { now: () => 0, ...opts },
  );
  return {
    cache, calls, urls, waiting,
    uploads: () => uploads,
    destroys: () => destroys,
    async settle(a: TileAddr, bytes = BYTES) {
      waiting.get(keyOf(a))!.resolve(data(a, bytes));
      waiting.delete(keyOf(a));
      await flush();
    },
    async fail(a: TileAddr, e: unknown) {
      waiting.get(keyOf(a))!.reject(e);
      waiting.delete(keyOf(a));
      await flush();
    },
  };
}

test("a tile wanted by eight panes is fetched ONCE and uploaded ONCE", async () => {
  const h = harness({ inFlight: 8 });
  const a = addr(1);
  h.cache.beginFrame();
  for (let pane = 0; pane < 8; pane++) assert.equal(h.cache.acquire(a).kind, "pending");
  h.cache.endFrame();
  await flush();
  assert.deepEqual(h.calls, [keyOf(a)]);
  await h.settle(a);
  assert.equal(h.uploads(), 1);
  h.cache.beginFrame();
  for (let pane = 0; pane < 8; pane++) assert.equal(h.cache.acquire(a).kind, "resident");
  assert.equal(h.cache.stats.hits, 8);
  assert.equal(h.uploads(), 1, "a second upload means the panes are not sharing one cache");
});

test("the budget is BYTES, because `cells` makes tiles non-uniform", async () => {
  const h = harness({ budgetBytes: 3 * BYTES, inFlight: 8 });
  // A small tile costs a small part of the budget; a count would treat it as a whole one.
  h.cache.beginFrame();
  for (let i = 0; i < 4; i++) h.cache.acquire(addr(i));
  h.cache.endFrame();
  await flush();
  for (let i = 0; i < 4; i++) await h.settle(addr(i), BYTES / 16);
  assert.equal(h.cache.residentTiles, 4, "four eighth-sized tiles fit a three-tile budget");
  assert.equal(h.cache.residentBytes, 4 * (BYTES / 16));
  assert.equal(h.cache.stats.evictions, 0);
});

test("eviction is LRU over the budget, and never touches what this frame is drawing", async () => {
  const h = harness({ budgetBytes: 2 * BYTES, inFlight: 8 });
  for (let i = 0; i < 3; i++) {
    h.cache.beginFrame();
    h.cache.acquire(addr(i));
    h.cache.endFrame();
    await flush();
    await h.settle(addr(i));
  }
  assert.equal(h.cache.residentBytes <= 2 * BYTES, true);
  assert.equal(h.cache.stats.evictions, 1);
  assert.equal(h.destroys(), 1, "an evicted tile must free its textures, or the budget is fiction");
  // The oldest went; the two most recent are still there.
  h.cache.beginFrame();
  assert.equal(h.cache.acquire(addr(0)).kind, "pending");
  assert.equal(h.cache.acquire(addr(2)).kind, "resident");
});

test("pins win over the budget: a starved frame keeps drawing rather than dropping what is on screen", async () => {
  const h = harness({ budgetBytes: BYTES, inFlight: 8 });
  h.cache.beginFrame();
  for (let i = 0; i < 3; i++) h.cache.acquire(addr(i));
  h.cache.endFrame();
  await flush();
  for (let i = 0; i < 3; i++) await h.settle(addr(i));
  // All three were acquired on the current frame, so all three are pinned.
  h.cache.beginFrame();
  for (let i = 0; i < 3; i++) assert.equal(h.cache.acquire(addr(i)).kind, "resident");
  h.cache.endFrame();
  assert.equal(h.cache.residentTiles, 3);
  assert.ok(h.cache.stats.overBudgetFrames > 0, "being over budget on pins alone must be COUNTED, not silently obeyed");
});

test("the queue is LIFO: the last tile wanted is the first fetched", async () => {
  const h = harness({ inFlight: 1 });
  h.cache.beginFrame();
  h.cache.acquire(addr(1));
  h.cache.acquire(addr(2));
  h.cache.acquire(addr(3));
  h.cache.endFrame();
  await flush();
  assert.deepEqual(h.calls, [keyOf(addr(3))], "FIFO here is what makes a fast pan deliver tiles for where the user WAS");
  await h.settle(addr(3));
  assert.deepEqual(h.calls, [keyOf(addr(3)), keyOf(addr(2))]);
});

test("a viewport change cancels the tiles it left, and only those", async () => {
  const h = harness({ inFlight: 1 });
  h.cache.beginFrame();
  for (let i = 0; i < 4; i++) h.cache.acquire(addr(i));
  h.cache.endFrame();
  await flush();
  const tileHz = 6250 * 256;
  h.cache.setViewports(LAT, [{ box: { f0Hz: 0, f1Hz: tileHz, t0Ns: 0, t1Ns: 1e9 * 256 }, levelF: 0, levelT: 0 }]);
  assert.equal(h.cache.queueDepth, 1, "only tile 0 intersects the new viewport");
  assert.ok(h.cache.stats.cancelled >= 2);
});

test("a viewport is a box AND its levels: a coarse viewport over everything does not keep fine tiles alive", async () => {
  // T-443: the minimap is another viewport spanning nearly the whole surface, so an extent-only
  // predicate made every fine tile intersect something forever and cancellation stopped cancelling.
  const h = harness({ inFlight: 1 });
  h.cache.beginFrame();
  for (let i = 0; i < 4; i++) h.cache.acquire(addr(i, 0, 0, 0)); // a pane's own, fine level
  h.cache.endFrame();
  await flush();
  const whole = { f0Hz: 0, f1Hz: 6e9, t0Ns: 0, t1Ns: 1e9 * 4096 };
  h.cache.setViewports(LAT, [{ box: whole, levelF: 8, levelT: 6 }]); // the map, and only the map
  assert.equal(h.cache.queueDepth, 0, "a fine tile no viewport is drawing at is not wanted, however wide the map is");
  // …and the map's own tiles, plus the parent it pins one level coarser, survive.
  h.cache.beginFrame();
  h.cache.acquire(addr(0, 0, 8, 6));
  h.cache.prefetch(addr(0, 0, 9, 7));
  h.cache.endFrame();
  h.cache.setViewports(LAT, [{ box: whole, levelF: 8, levelT: 6 }]);
  assert.ok(h.cache.queueDepth + h.cache.inFlightCount >= 2, "the viewport's own level and its pinned parent must both survive");
});

test("the route's 503 is an ANSWER: adopt the cap it names, back off, keep wanting the tile", async () => {
  let clock = 0;
  const h = harness({ inFlight: 4, busyBackoffMs: 50, now: () => clock });
  h.cache.beginFrame();
  h.cache.acquire(addr(1));
  h.cache.endFrame();
  await flush();
  await h.fail(addr(1), new TileBusyError(2, "too many tile reads in flight (limit 2)"));
  assert.equal(h.cache.inFlightLimit, 2, "the server owns this cap: it is ingest backpressure, not a browser connection limit");
  assert.equal(h.cache.stats.busyRefusals, 1);
  assert.equal(h.cache.stats.failures, 0, "a refusal naming its cap is not a failure");
  assert.equal(h.cache.queueDepth, 1, "the tile is still wanted");
  // Backoff holds the queue, then it runs again.
  h.cache.beginFrame(); h.cache.endFrame();
  assert.equal(h.calls.length, 1);
  clock = 100;
  h.cache.beginFrame(); h.cache.endFrame();
  assert.equal(h.calls.length, 2);
});

test("a real failure is counted and does not wedge the queue", async () => {
  const h = harness({ inFlight: 1 });
  h.cache.beginFrame();
  h.cache.acquire(addr(1));
  h.cache.acquire(addr(2));
  h.cache.endFrame();
  await flush();
  await h.fail(addr(2), new Error("boom"));
  assert.equal(h.cache.stats.failures, 1);
  assert.deepEqual(h.calls, [keyOf(addr(2)), keyOf(addr(1))]);
});

test("the residency answer is never a cell state", async () => {
  const h = harness();
  h.cache.beginFrame();
  const r = h.cache.acquire(addr(9));
  assert.equal(r.kind, "pending");
  // `pending` is not in CELL, and CELL's codes are not residencies. The type system says so; this
  // says it again where a future edit would have to notice.
  assert.ok(!Object.values(CELL).some((v) => String(v) === r.kind));
});

// ——— T-454: the cap did not hold, and none of these would have caught it ———

const TILE_HZ = 6250 * 256, TILE_NS = 1e9 * 256;
const paneView = (from: number, to: number): Viewport =>
  ({ box: { f0Hz: from * TILE_HZ, f1Hz: to * TILE_HZ, t0Ns: 0, t1Ns: TILE_NS }, levelF: 0, levelT: 0 });

test("an abort does NOT hand the route its slot back, so a drag cannot pump requests", async () => {
  // The mechanism behind T-450's 26 resident against 17 268 cancelled. `hk-api` serves on blocking
  // threads: it discovers the closed socket when it writes the response, so an aborted read keeps
  // its TileSlot and the history lock until it finishes. Freeing the client's slot at the abort let
  // every frame of a drag issue a fresh request against slots the server was still holding — four
  // locally, dozens on the route, and the refusal the user saw.
  let clock = 0;
  const h = harness({ inFlight: 2, now: () => clock, serverMsGuess: 100 });
  h.cache.beginFrame();
  h.cache.acquire(addr(0));
  h.cache.acquire(addr(1));
  h.cache.setViewports(LAT, [paneView(0, 2)]);
  h.cache.endFrame();
  await flush();
  assert.deepEqual(h.urls, [tileUrl(addr(1)), tileUrl(addr(0))], "two slots, two requests");

  // The user pans. Both in-flight tiles are left behind — and the route is still making them.
  h.cache.beginFrame();
  h.cache.acquire(addr(8));
  h.cache.acquire(addr(9));
  h.cache.setViewports(LAT, [paneView(8, 10)]);
  h.cache.endFrame();
  await flush();
  assert.equal(h.cache.stats.abandoned, 2);
  assert.equal(h.cache.abandonedSlots, 2, "still costing the route, so still costing the budget");
  assert.equal(h.urls.length, 2,
    "asking for two more here is exactly how four server slots became forty — and then a 503");

  // Once they are expected to have finished, the tiles the user is actually looking at go out.
  clock = 101;
  h.cache.beginFrame();
  h.cache.acquire(addr(8));
  h.cache.acquire(addr(9));
  h.cache.setViewports(LAT, [paneView(8, 10)]);
  h.cache.endFrame();
  await flush();
  assert.deepEqual(h.urls.slice(2), [tileUrl(addr(9)), tileUrl(addr(8))],
    "and only the still-visible ones are retried");
});

test("the cap in force is never exceeded, even when the budget is shared with another client", async () => {
  // The route's cap is SERVER-WIDE (hk-api's TileSlot on ApiState): a second tab holding two of the
  // four leaves two, and a client counting only its own four is over the budget while believing it
  // is under it. The refusal is the only thing that says so, so it must be acted on, not displayed.
  const FREE = 2;
  let open = 0, maxOpen = 0, refusals = 0;
  const done: (() => void)[] = [];
  let clock = 0;
  const cache = new TileCache<{ id: number }>(
    { upload: () => ({ id: 0 }), destroy: () => {} },
    (a) => {
      if (open >= FREE) {
        refusals++;
        return Promise.reject(new TileBusyError(4, "too many tile reads in flight (limit 4)"));
      }
      open++;
      maxOpen = Math.max(maxOpen, open);
      return new Promise<TileData>((resolve) => done.push(() => { open--; resolve(data(a)); }));
    },
    { inFlight: 4, budgetBytes: 64 * BYTES, busyBackoffMs: 10, now: () => clock, serverMsGuess: 20 },
  );

  for (let frame = 0; frame < 40; frame++) {
    clock += 16; // one animation frame
    cache.beginFrame();
    for (let i = 0; i < 8; i++) cache.acquire(addr(i));
    cache.setViewports(LAT, [paneView(0, 8)]);
    cache.endFrame();
    await flush();
    done.shift()?.(); // the route finishes one tile
    await flush();
  }
  assert.ok(maxOpen <= FREE + 1,
    `the client kept ${maxOpen} reads open against a route with ${FREE} free slots`);
  assert.ok(cache.inFlightLimit <= FREE + 1, `converged to ${cache.inFlightLimit}, not to its own guess of 4`);
  const before = refusals;
  for (let frame = 0; frame < 20; frame++) {
    clock += 16;
    cache.beginFrame();
    for (let i = 0; i < 8; i++) cache.acquire(addr(i));
    cache.setViewports(LAT, [paneView(0, 8)]);
    cache.endFrame();
    await flush();
    done.shift()?.();
    await flush();
  }
  assert.equal(refusals, before, "once converged the client stops being refused at all");
});

test("recovery is EARNED: the halved cap climbs back, and never past the number the route named", async () => {
  const h = harness({ inFlight: 4, busyBackoffMs: 0 });
  h.cache.beginFrame();
  h.cache.acquire(addr(0));
  h.cache.endFrame();
  await flush();
  await h.fail(addr(0), new TileBusyError(4, "too many tile reads in flight (limit 4)"));
  assert.equal(h.cache.inFlightLimit, 2,
    "the route's 4 is EVERYONE's budget — adopting it as this client's allowance was `min(4, 4)`, a no-op");
  assert.equal(h.cache.stats.busyRefusals, 1);
  assert.equal(h.cache.stats.failures, 0);

  // The refusal re-queued the tile and the pump reissued it; that plus RECOVER_AFTER-1 more
  // completions is one run of successes, and buys back exactly one slot.
  await h.settle(addr(0));
  for (let i = 1; i < RECOVER_AFTER; i++) {
    h.cache.beginFrame();
    h.cache.acquire(addr(i));
    h.cache.endFrame();
    await flush();
    await h.settle(addr(i));
  }
  assert.equal(h.cache.inFlightLimit, 3, "one slot per run of successes, not a jump back to the guess");
  assert.ok(h.cache.inFlightLimit <= 4);
});

test("a coarse viewport cannot hold the whole budget while the panes wait", async () => {
  // T-450 measured the map's level-10 tile at 5.2 s and 9.5 MB. LIFO alone says "the most recently
  // wanted tile", and the map's are enqueued last, so both slots went to five-second reads.
  const h = harness({ inFlight: 2 });
  const map: Viewport = { box: { f0Hz: 0, f1Hz: 2e9, t0Ns: 0, t1Ns: 2e13 }, levelF: 8, levelT: 6 };
  h.cache.beginFrame();
  for (let i = 0; i < 4; i++) h.cache.acquire(addr(i, 0, 0, 0)); // the pane's own tiles
  for (let i = 0; i < 4; i++) h.cache.acquire(addr(i, 0, 8, 6)); // the map's, wanted last
  h.cache.setViewports(LAT, [paneView(0, 4), map]);
  h.cache.endFrame();
  await flush();
  assert.deepEqual(h.urls, [tileUrl(addr(3, 0, 8, 6)), tileUrl(addr(3, 0, 0, 0))],
    "one slot each: the map is a viewport, not a priority");
});

test("keys round-trip, so an in-flight request can be tested against a viewport", () => {
  const a = addr(7, 3, 2, 5);
  assert.deepEqual(parseKey(keyOf(a)), a);
  assert.equal(parseKey("nonsense"), null);
});
