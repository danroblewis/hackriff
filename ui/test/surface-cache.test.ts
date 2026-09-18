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
import { RECOVER_AFTER, REFRESH_DUTY, TileCache, parseKey, type TileCacheOptions, type Viewport } from "../src/surface/tilecache";
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

// ——— T-460: the live edge was frozen HERE, and this is the guard that would have seen it ———
//
// The defect was one line: `acquire` answers a resident tile unconditionally, and the only path that
// could drop one was the retune. So a live-edge tile was fetched ONCE and frozen until the pane
// scrolled into a new address — at `level_t = 0`, once every 256 seconds — while the backend served
// the rows the whole time. The user's words: *"the live view still does not add waterfall rows for
// recent samples."*
//
// **What these assert is a property of the CACHE, not of the screen.** The pixels are
// `ui/e2e/live-edge.e2e.mjs`'s subject, deliberately: a tile being re-fetched is not a row appearing,
// and this file cannot tell the difference. What it can pin down is the policy — that a live-edge
// tile is never served from cache indefinitely, that the re-ask is bounded by the period the data
// can change in, and that it can never take a slot from something the user is waiting for.

/** A live-edge tile: its extent straddles `EDGE_NS`, so `atEdge` holds. */
const EDGE_NS = 200e9;
const edgeTile = (fIndex = 0, levelT = 0) => addr(fIndex, 0, 0, levelT);
const edgeView = (levelT = 0): Viewport =>
  ({ box: { f0Hz: 0, f1Hz: TILE_HZ, t0Ns: EDGE_NS - 100e9, t1Ns: EDGE_NS }, levelF: 0, levelT });

/**
 * Run `ms` of simulated time in 100 ms steps, drawing a frame each step and answering every fetch
 * at once. `refresh` is what separates the guard from its own control: with it, the policy runs;
 * without it, this is exactly the loop that produced the defect.
 */
async function live(h: ReturnType<typeof harness>, clock: { t: number }, ms: number,
  { refresh = true, levelT = 0, tiles = [0] }: { refresh?: boolean; levelT?: number; tiles?: number[] } = {}) {
  let everPending = 0;
  for (let step = 0; step < ms / 100; step++) {
    clock.t += 100;
    h.cache.beginFrame();
    for (const f of tiles) if (h.cache.acquire(edgeTile(f, levelT)).kind === "pending") everPending++;
    h.cache.setViewports(LAT, [edgeView(levelT)]);
    h.cache.endFrame();
    if (refresh) h.cache.refreshEdge(LAT, EDGE_NS, [edgeView(levelT)]);
    await flush();
    for (const [key, w] of [...h.waiting]) {
      w.resolve(data(parseKey(key)!));
      h.waiting.delete(key);
      await flush();
    }
  }
  return { everPending };
}

test("a live-edge tile is NEVER served from cache indefinitely", async () => {
  const clock = { t: 0 };
  const h = harness({ inFlight: 4, now: () => clock.t, serverMsGuess: 20 });
  const a = edgeTile();
  // Ten simulated seconds of a following pane on the growing edge.
  const { everPending } = await live(h, clock, 10_000);

  const asked = h.calls.filter((k) => k === keyOf(a)).length;
  assert.ok(asked > 1,
    `the live-edge tile was fetched ${asked} time(s) in 10 s — this is T-460: the rows were recorded ` +
    "and served, and the client never asked for them again");
  assert.ok(h.cache.stats.edgeRefreshApplied > 0,
    "asking is not enough: the answer has to REPLACE the resident tile, or nothing new is drawn");
  assert.equal(h.cache.stats.edgeRefreshes, asked - 1, "every re-ask is accounted for as a refresh");

  // And it never flashes: the stale copy stays resident and drawn for the whole revalidation, so a
  // following pane never goes back to `pending` for a tile it already had.
  assert.equal(everPending, 1, "the edge tile went pending again — a refresh must not drop what is drawn");
  assert.equal(h.cache.residentTiles, 1);
  // One texture in hand, not a leak per refresh.
  assert.equal(h.uploads() - h.destroys(), 1, "each refresh must destroy the texture it replaces");
});

test("…and the CONTROL: without the policy, the very same loop asks exactly once (the defect)", async () => {
  // Non-vacuity. The loop above draws frames, moves the clock, pumps the queue and reports
  // viewports — everything except the one call this ticket adds. If the guard above could pass
  // without `refreshEdge`, it would be measuring the harness.
  const clock = { t: 0 };
  const h = harness({ inFlight: 4, now: () => clock.t, serverMsGuess: 20 });
  await live(h, clock, 10_000, { refresh: false });
  assert.deepEqual(h.calls, [keyOf(edgeTile())],
    "one fetch in ten seconds: that is the frozen live edge, reproduced");

  // The same is true for a pane that is FROZEN rather than following, and that is not a bug: a
  // frozen pane is a view over data that cannot change, so refreshing for it is pure cost.
  const frozen = harness({ inFlight: 4, now: () => clock.t, serverMsGuess: 20 });
  for (let step = 0; step < 100; step++) {
    clock.t += 100;
    frozen.cache.beginFrame();
    frozen.cache.acquire(edgeTile());
    frozen.cache.endFrame();
    frozen.cache.refreshEdge(LAT, EDGE_NS, []); // no viewport is following
    await flush();
    for (const [key, w] of [...frozen.waiting]) { w.resolve(data(parseKey(key)!)); frozen.waiting.delete(key); await flush(); }
  }
  assert.deepEqual(frozen.calls, [keyOf(edgeTile())], "a frozen viewport refreshes nothing");
});

test("the refresh is bounded by the period the data can change in, per level", async () => {
  // A tile's newest row is one cell tall, so a re-ask inside `tCellNs(level_t)` cannot return a row
  // the copy in hand does not already have. That makes the cadence a function of the ZOOM, with no
  // policy number to tune — which is what keeps this from being the poll the ticket forbids.
  const fine = { t: 0 };
  const hFine = harness({ inFlight: 4, now: () => fine.t, serverMsGuess: 20 });
  await live(hFine, fine, 10_000);                     // level_t 0: a 1 s cell
  const nFine = hFine.cache.stats.edgeRefreshes;
  assert.ok(nFine >= 8 && nFine <= 10, `level 0 refreshed ${nFine} times in 10 s; expected ~one per 1 s cell`);

  const coarse = { t: 0 };
  const hCoarse = harness({ inFlight: 4, now: () => coarse.t, serverMsGuess: 20 });
  await live(hCoarse, coarse, 10_000, { levelT: 5 });  // a 32 s cell
  assert.equal(hCoarse.cache.stats.edgeRefreshes, 0,
    "a 32 s cell re-asked inside 10 s would spend a 19 MB tile to learn nothing");
});

test("the refresh can never take a slot from a tile the user is waiting for", async () => {
  const clock = { t: 0 };
  const h = harness({ inFlight: 4, now: () => clock.t, serverMsGuess: 20 });
  // One edge tile resident and long overdue a refresh…
  h.cache.beginFrame();
  h.cache.acquire(edgeTile(0));
  h.cache.endFrame();
  await flush();
  await h.settle(edgeTile(0));
  clock.t += 5_000;

  // …and now four MISSES appear: a pan, a split, a zoom. The refresh is revalidation of something
  // already on screen, so it may not be the reason any of these is not being fetched.
  h.cache.beginFrame();
  h.cache.acquire(edgeTile(0));
  for (let i = 1; i <= 4; i++) h.cache.acquire(addr(i));
  h.cache.setViewports(LAT, [{ box: { f0Hz: 0, f1Hz: 5 * TILE_HZ, t0Ns: EDGE_NS - 100e9, t1Ns: EDGE_NS }, levelF: 0, levelT: 0 }]);
  h.cache.endFrame();
  h.cache.refreshEdge(LAT, EDGE_NS, [edgeView()]);
  await flush();

  assert.equal(h.cache.stats.edgeRefreshes, 0, "a refresh was issued while four misses were outstanding");
  assert.ok(h.cache.inFlightCount <= 4, `in flight ${h.cache.inFlightCount} > the route's cap`);
  assert.deepEqual(h.calls.slice(1).sort(), [1, 2, 3, 4].map((i) => keyOf(addr(i))).sort(),
    "the four visible misses are what went out");
  assert.ok(h.cache.refreshDepth > 0, "the refresh is not dropped — it waits for the cache to be idle");

  // Once they land the cache is idle again, and the refresh goes out then.
  for (let i = 1; i <= 4; i++) await h.settle(addr(i));
  assert.equal(h.cache.stats.edgeRefreshes, 1, "and it is not forgotten either");
});

test("the refresh lane can never exceed a fixed share of MEASURED capacity", async () => {
  // The duty limit, and why it is a share of the measured service time rather than a frequency: at
  // ~19 MB a live tile, a fixed 1 Hz would spend the whole in-flight budget. Here every answer costs
  // 600 ms of the clock, the cache measures that, and the refresh rate is a quotient of it — so a
  // route that gets slower makes the refresh rarer with nothing retuned.
  const clock = { t: 0 };
  const SERVER_MS = 600;
  const h = harness({ inFlight: 4, now: () => clock.t, serverMsGuess: SERVER_MS });
  const wide: Viewport = { box: { f0Hz: 0, f1Hz: 3 * TILE_HZ, t0Ns: EDGE_NS - 100e9, t1Ns: EDGE_NS }, levelF: 0, levelT: 0 };
  for (let step = 0; step < 200; step++) {
    clock.t += 100;
    h.cache.beginFrame();
    for (const f of [0, 1, 2]) h.cache.acquire(edgeTile(f));
    h.cache.setViewports(LAT, [wide]);
    h.cache.endFrame();
    h.cache.refreshEdge(LAT, EDGE_NS, [wide]);
    await flush();
    for (const [key, w] of [...h.waiting]) {
      clock.t += SERVER_MS;
      w.resolve(data(parseKey(key)!));
      h.waiting.delete(key);
      await flush();
    }
  }
  const n = h.cache.stats.edgeRefreshes;
  assert.ok(n >= 2, `only ${n} refresh(es) in ${clock.t} ms — nothing to measure, the bound would be vacuous`);
  // At least the per-answer cost: a batch settled back-to-back on the same fake clock charges the
  // later ones for the earlier ones' wait, which is honest — the route really was busy that long.
  assert.ok(h.cache.serverEstimateMs >= SERVER_MS,
    `the estimate never learned the ${SERVER_MS} ms cost: ${h.cache.serverEstimateMs}`);
  // The whole guarantee, stated as a rate so it does not depend on when the test looked: at most one
  // refresh per REFRESH_DUTY service times over the whole run (+1 for the one issued at t = 0).
  const cap = clock.t / (REFRESH_DUTY * SERVER_MS) + 1;
  assert.ok(n <= cap,
    `${n} refreshes in ${clock.t} ms is above one per ${REFRESH_DUTY} x ${SERVER_MS} ms (cap ${cap.toFixed(1)})`);
  // Three edge tiles, all continuously due, and the lane still never ran more than one at a time.
  assert.ok(h.cache.stats.edgeRefreshApplied >= 2, "the refreshes landed rather than being cancelled");
});

test("a retune still wins over a refresh in flight", async () => {
  // `invalidateEdge` marks an in-flight key stale, and the data that then arrives describes a tuning
  // that no longer exists. A refresh must not be the one path that smuggles it back in: the tile is
  // dropped, the answer is discarded, and the address is asked for again.
  const clock = { t: 0 };
  const h = harness({ inFlight: 4, now: () => clock.t, serverMsGuess: 20 });
  await live(h, clock, 1_500);
  clock.t += 2_000;
  h.cache.refreshEdge(LAT, EDGE_NS, [edgeView()]);
  await flush();
  assert.equal(h.cache.inFlightCount, 1, "a refresh should be in flight to be overtaken");

  assert.equal(h.cache.invalidateEdge(LAT, EDGE_NS), 1, "the retune drops the resident edge tile");
  const uploads = h.uploads();
  await h.settle(edgeTile());
  assert.equal(h.uploads(), uploads, "the old tuning's tile was uploaded by the refresh path");
  await flush();
  assert.ok(h.waiting.has(keyOf(edgeTile())), "the address is asked for again after the retune");
});

// ——— T-479: a 4xx is TERMINAL, and "retryable" is the enumerated case ———
//
// What the user saw: the console flooding. The canvas asked for an out-of-range node (observed in
// the wild as `level_f=10` with `t_index=218471` — the address arithmetic is T-480's, not this
// file's), the route correctly answered **400**, and the client asked again on the very next frame,
// forever. `failed()` special-cased 503 and let everything else fall through to a default of *ask
// again*, so every unenumerated status was wrong; a fix that adds `400` beside `503` would leave the
// next one wrong too. The predicate is therefore inverted: retryable is enumerated, terminal is the
// default.

/** An error shaped like the one `tile.ts` builds from a non-503 response: it carries a status. */
const httpError = (status: number, msg = "bad node") =>
  Object.assign(new Error(msg), { status, code: "bad_request" });

test("a 400 is fetched at most ONCE per place, however many frames ask for it", async () => {
  const h = harness({ inFlight: 4 });
  const bad = addr(9, 218471, 10, 0);
  h.cache.beginFrame();
  h.cache.acquire(bad);
  h.cache.endFrame();
  await flush();
  await h.fail(bad, httpError(400));

  // Sixty frames — one second of a render loop — all asking for the same refused place.
  for (let frame = 0; frame < 60; frame++) {
    h.cache.beginFrame();
    const res = h.cache.acquire(bad);
    h.cache.endFrame();
    await flush();
    assert.equal(res.kind, "pending", "a refused place must never become a cell state");
    assert.equal(res.kind === "pending" && res.failed, true,
      "…and the caller is told WHICH not-loaded it is: asked and refused, not never-sampled");
  }
  assert.deepEqual(h.calls, [keyOf(bad)], `the 400 was re-asked ${h.calls.length - 1} more times`);
  assert.equal(h.cache.stats.terminalFailures, 1);
  assert.equal(h.cache.terminalPlaces, 1);
  assert.match(h.cache.refusalFor(bad) ?? "", /HTTP 400/, "the route's own words are kept for the readout");
});

test("NO non-503 status is ever asked twice — the enumeration, not a list of special cases", async () => {
  // The guard the ticket asked for, stated over the statuses rather than over the one that bit us.
  for (const status of [400, 401, 403, 404, 410, 413, 422, 500, 502, 504]) {
    const h = harness({ inFlight: 4 });
    const a = addr(status % 7);
    h.cache.beginFrame(); h.cache.acquire(a); h.cache.endFrame();
    await flush();
    await h.fail(a, httpError(status));
    for (let frame = 0; frame < 10; frame++) {
      h.cache.beginFrame(); h.cache.acquire(a); h.cache.endFrame();
      await flush();
    }
    assert.deepEqual(h.calls, [keyOf(a)], `HTTP ${status} was re-asked: terminal is supposed to be the DEFAULT`);
  }
});

test("…and 503 still retries with T-454's AIMD intact: the cap is discovery, not leakage", async () => {
  // The load-bearing property this change must not disturb. A refusal is not a failure, is not
  // terminal, halves the cap, backs off, and the place stays wanted.
  let clock = 0;
  const h = harness({ inFlight: 4, busyBackoffMs: 50, now: () => clock });
  h.cache.beginFrame(); h.cache.acquire(addr(0)); h.cache.endFrame();
  await flush();
  await h.fail(addr(0), new TileBusyError(4, "too many tile reads in flight (limit 4)"));
  assert.equal(h.cache.inFlightLimit, 2, "a 503 still halves the cap");
  assert.equal(h.cache.stats.busyRefusals, 1);
  assert.equal(h.cache.stats.terminalFailures, 0, "backpressure is a statement about NOW, never terminal");
  assert.equal(h.cache.terminalPlaces, 0);
  clock += 100;
  h.cache.beginFrame(); h.cache.acquire(addr(0)); h.cache.endFrame();
  await flush();
  assert.deepEqual(h.calls, [keyOf(addr(0)), keyOf(addr(0))], "the refused place is still wanted");
  await h.settle(addr(0));
  assert.equal(h.cache.residentTiles, 1);

  // A transport failure — the server said nothing at all — is not terminal either, and is re-asked
  // by the next frame rather than by a tight requeue cycle.
  const net = harness({ inFlight: 4 });
  net.cache.beginFrame(); net.cache.acquire(addr(3)); net.cache.endFrame();
  await flush();
  await net.fail(addr(3), new TypeError("Failed to fetch"));
  assert.equal(net.cache.stats.terminalFailures, 0, "a disconnect must not blank the place for the session");
  assert.equal(net.cache.queueDepth, 0, "…and must not spin: nothing is re-queued by the failure itself");
  net.cache.beginFrame(); net.cache.acquire(addr(3)); net.cache.endFrame();
  await flush();
  assert.equal(net.calls.length, 2, "the next frame asks again, paced by the render loop");
});
