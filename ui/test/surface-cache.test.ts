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
import { TileBusyError, TileDecodeError, type TileData } from "../src/surface/tile";

const LAT: Lattice = { scheme: "view", cells: 256, f0Hz: 6250, t0Ns: 1e9, levelsF: 20, levelsT: 15 };
const BYTES = 192 * 1024; // one 256^2 tile: R16F measurement + R8 state

const addr = (fIndex: number, tIndex = 0, levelF = 0, levelT = 0): TileAddr =>
  ({ device: "any", scheme: "view", levelF, levelT, fIndex, tIndex, cells: 256 });

/** `t1Ns` is what the ROUTE said this tile's span ends at (`extent.t1_s`), and `null` — the default
 * here — is a route that did not say, which [[TileCache]] must fall back from rather than treat as
 * "sealed". Both arms are exercised by the T-495 guard below. */
function data(a: TileAddr, bytes = BYTES, t1Ns: number | null = null): TileData {
  return {
    addr: a, key: keyOf(a), nf: 2, nt: 2, t1Ns,
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

test("a real failure is counted and does not wedge the queue — it PACES it (T-499)", async () => {
  // **This test changed with T-499, and the change is the ticket.** It used to assert that the very
  // next pump issues the next queued tile, which is the same policy that asked a SIGKILLed server 56
  // times in five seconds and 55 in the next. The property it was written for — *the failure does not
  // wedge the queue* — is asserted here still, and more sharply: the queue drains, on a clock.
  let clock = 0;
  const h = harness({ inFlight: 1, now: () => clock });
  h.cache.beginFrame();
  h.cache.acquire(addr(1));
  h.cache.acquire(addr(2));
  h.cache.endFrame();
  await flush();
  await h.fail(addr(2), new Error("boom"));
  assert.equal(h.cache.stats.failures, 1);
  assert.equal(h.cache.stats.silentFailures, 1, "an error with no status is the server saying nothing");
  assert.deepEqual(h.calls, [keyOf(addr(2))],
    "the gate holds: the failure itself must not hand the next tile straight back to the wire");
  h.cache.beginFrame(); h.cache.acquire(addr(1)); h.cache.endFrame();
  await flush();
  assert.deepEqual(h.calls, [keyOf(addr(2))], "…nor does the next frame, while the backoff is armed");
  clock += 600; // past the first rung of the ladder
  h.cache.beginFrame(); h.cache.acquire(addr(1)); h.cache.endFrame();
  await flush();
  assert.deepEqual(h.calls, [keyOf(addr(2)), keyOf(addr(1))],
    "and once it opens the queue drains — a backoff is not a wedge");
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

/** A live-edge tile: its extent straddles `EDGE_NS`, so it is still being written. */
const EDGE_NS = 200e9;
/**
 * **The edge at a given point on the simulated clock: it GROWS, because a live edge does** (T-495).
 *
 * It was a constant, which was harmless while eligibility was "is the edge inside this tile" and is
 * not any more: since T-495 a copy is fresh once it was asked for at an edge that already reached
 * everything it could hold, so **an edge that never moves correctly produces no second refresh** —
 * there is provably no new row to fetch. A cadence measured against a frozen edge would therefore be
 * measuring the harness. One millisecond of clock is one millisecond of capture time.
 */
const edgeAt = (clockMs: number) => EDGE_NS + clockMs * 1e6;
const edgeTile = (fIndex = 0, levelT = 0) => addr(fIndex, 0, 0, levelT);
const edgeView = (levelT = 0, edgeNs = EDGE_NS): Viewport =>
  ({ box: { f0Hz: 0, f1Hz: TILE_HZ, t0Ns: edgeNs - 100e9, t1Ns: edgeNs }, levelF: 0, levelT });

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
    if (refresh) h.cache.refreshEdge(LAT, edgeAt(clock.t), [edgeView(levelT, edgeAt(clock.t))]);
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

// ——— T-495: a live tile panned OFF SCREEN and back, whose middle never fills ———
//
// The user's report, in their own six steps: *load tuned to 100.8 MHz; retune to 101.99; pan RIGHT so
// the new tile is in view, staying in Live; pan LEFT so it goes OFF-SCREEN, still Live; pan RIGHT so
// it is back in view. Rows from the first on-screen period are present, rows from the second are
// present, and the rows that arrived while it was off-screen are grey forever.*
//
// **It reads like a stale cache on re-entry and it is not.** Driven through this cache the client
// makes no request at all on re-entry — and would make two if the edge happened still to be inside
// the tile. The two arms are the whole finding, and they are the two runs below:
//
// ```
//                                          fetches of the tile under test
//                                    on screen   while away   AFTER RE-ENTRY
//   the edge is still inside it           3            0            2    <- T-460's case, fine
//   the edge LEFT it while it was away    3            0            0    <- grey forever
// ```
//
// Nothing about re-entry was broken. `atEdge` — "is the live edge still inside this tile" — went
// false permanently while nobody was looking, and no other path in `tilecache.ts` can ask again for
// a resident key. The copy in hand then holds rows only to wherever the edge was at its last
// on-screen refresh; the rest of its span was served as `unobserved`, which is THE grey, and is
// honest at the instant it is served. Measured against a real `hk serve`: a 32 s live tile read 2.5 s
// in answers three rows `observed` and **twenty-nine `unobserved`**.
//
// The tile ABOVE it in time is a different address, so on return it is an ordinary miss and arrives
// complete — which is exactly the present / grey / present sandwich the user described.

/** 1 s cells, 8 to a tile: an 8 s tile, so the edge can cross out of it inside a test. */
const SMALL: Lattice = { scheme: "view", cells: 8, f0Hz: 6250, t0Ns: 1e9, levelsF: 20, levelsT: 15 };
const SMALL_TILE_NS = 8e9, SMALL_TILE_HZ = 6250 * 8;
const smallTile = (fIndex: number, tIndex: number): TileAddr =>
  ({ device: "any", scheme: "view", levelF: 0, levelT: 0, fIndex, tIndex, cells: 8 });
/**
 * One pane, anchored at the edge. The time window is **deliberately taller than the tile**, so the
 * tile stays on screen for the whole run: the subject here is the cache's eligibility rule, and a
 * tile that simply scrolled out of the viewport would stop being asked for on a different rule
 * entirely and make the run vacuous. In the product the relation is the other way round — a level-0
 * tile is 256 s against a window of tens of seconds — which changes only how long the stale band is
 * visible, never whether it fills.
 */
const smallView = (fIndex: number, edgeNs: number): Viewport => ({
  box: { f0Hz: fIndex * SMALL_TILE_HZ, f1Hz: (fIndex + 1) * SMALL_TILE_HZ, t0Ns: edgeNs - 60e9, t1Ns: edgeNs },
  levelF: 0, levelT: 0,
});

/**
 * The user's six steps, as frames: on screen, away while the edge advances, back on screen.
 *
 * `stated` is whether the route's answer carries `extent.t1_s`. Both arms must behave identically —
 * the fallback computes the same number from the address, which is where the route computes it from
 * too — and that is asserted rather than assumed.
 */
async function offAndBack(awayFrames: number, stated: boolean) {
  const clock = { t: 0 };
  const h = harness({ inFlight: 4, now: () => clock.t, serverMsGuess: 20 });
  const UNDER_TEST = smallTile(3, 0);              // spans 0…8 s
  const ABOVE = smallTile(3, 1);                   // spans 8…16 s: the next address up
  const end = (a: TileAddr) => (a.tIndex + 1) * SMALL_TILE_NS;
  let edgeNs = 3e9;                                // 3 s in: the edge is inside UNDER_TEST
  const count = (a: TileAddr) => h.calls.filter((k) => k === keyOf(a)).length;

  const frames = async (n: number, fIndex: number, draw: TileAddr[]) => {
    for (let i = 0; i < n; i++) {
      clock.t += 400;
      edgeNs += 400e6;                             // a live edge GROWS; a still one has no new rows
      h.cache.beginFrame();
      for (const a of draw) h.cache.acquire(a);
      h.cache.setViewports(SMALL, [smallView(fIndex, edgeNs)]);
      h.cache.endFrame();
      h.cache.refreshEdge(SMALL, edgeNs, [smallView(fIndex, edgeNs)]);
      await flush();
      for (const [key, w] of [...h.waiting]) {
        const a = parseKey(key)!;
        w.resolve(data(a, BYTES, stated ? end(a) : null));
        h.waiting.delete(key);
        await flush();
      }
    }
  };

  await frames(8, 3, [UNDER_TEST]);                // steps 1–3: on screen, following
  const onScreen = count(UNDER_TEST);
  await frames(awayFrames, 40, []);                // step 4: off screen, rows still arriving
  const away = count(UNDER_TEST) - onScreen;
  const edgeOnReturn = edgeNs;                     // was the tile finished before the user came back?
  await frames(20, 3, [UNDER_TEST, ABOVE]);        // step 5: back, same window
  const back = count(UNDER_TEST) - onScreen - away;
  await frames(20, 3, [UNDER_TEST, ABOVE]);        // …and then left alone
  const settled = count(UNDER_TEST) - onScreen - away - back;
  return { h, onScreen, away, back, settled, edgeOnReturn, endNs: end(UNDER_TEST),
    residency: h.cache.acquire(UNDER_TEST, false).kind };
}

for (const stated of [true, false]) {
  const how = stated ? "with the route stating extent.t1_s" : "with the route stating no extent (the address stands in)";
  test(`a live tile panned off screen and back fills in — ${how}`, async () => {
    // **The subject: the edge leaves the tile while it is off screen.** 30 frames is 12 s of capture
    // over an 8 s tile, so it is finished by the time the user pans back and `atEdge` can never be
    // true for it again.
    const gone = await offAndBack(30, stated);
    assert.ok(gone.edgeOnReturn > gone.endNs,
      "the edge had not left the tile by the time the user panned back — this run has no subject");

    // The trap the ticket names: the stale tile IS resident, so residency proves nothing.
    assert.equal(gone.residency, "resident",
      "the tile was evicted, so this run measures the ordinary miss path and not T-495");
    // Nothing polls a tile nobody is looking at: that half of the policy is unchanged.
    assert.equal(gone.away, 0, "an off-screen tile was re-fetched — a refresh is for what is on screen");

    // **THE CLAIM.** Coming back into view, the copy in hand is missing every row recorded since it
    // was last asked for, and the client asks again. Before T-495 this was 0, for the rest of the
    // session.
    assert.ok(gone.back >= 1,
      `the tile was fetched ${gone.back} time(s) after coming back into view, with ` +
      `${(gone.edgeOnReturn - gone.endNs) / 1e9} s of capture past its own end while it was away. This is T-495: a ` +
      "resident live tile is not fresh just because it is resident — its freshness is its coverage " +
      "up to the live edge.");
    assert.ok(gone.h.cache.stats.edgeRefreshCompletions >= 1,
      "the completing re-ask was not counted, so the fix is not the one that ran");

    // **…and it SEALS.** The re-ask is taken at an edge past the tile's own end, so the copy now
    // covers everything it ever can and is never asked for again. That is what stops a fix for a
    // never-fills bug becoming the poll T-460 forbade: one extra request per tile, per lifetime.
    assert.equal(gone.settled, 0,
      `the tile was re-fetched ${gone.settled} more time(s) after it was complete — a sealed tile ` +
      "cannot change, and re-asking for it is pure waste");

    // **The control, and the reason the bug was invisible:** with the edge still inside the tile on
    // return, T-460's rule already covered it and the same six gestures look fine.
    const inside = await offAndBack(2, stated);
    assert.ok(inside.edgeOnReturn < inside.endNs,
      "the control's edge had left the tile by the time it came back — it is not a control");
    assert.ok(inside.back >= 1, "T-460's own case regressed: a tile still at the edge must re-ask");
  });
}

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
  h.cache.refreshEdge(LAT, edgeAt(clock.t), [edgeView(0, edgeAt(clock.t))]);
  await flush();
  assert.equal(h.cache.inFlightCount, 1, "a refresh should be in flight to be overtaken");

  assert.equal(h.cache.invalidateEdge(LAT, edgeAt(clock.t)), 1, "the retune drops the resident edge tile");
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

test("NO status the ROUTE can emit is ever asked twice — the enumeration, not a list of special cases", async () => {
  // The guard the ticket asked for, stated over the statuses rather than over the one that bit us.
  // T-523 narrows "every status" to "every status `/api/tiles` can answer with": every 4xx, plus
  // the three 5xx `hk-api` emits (500, the T-190 501, and T-454's 503 — which arrives as a
  // TileBusyError and is the test below). 501 is here because it is a real refusal that a proxy
  // could be mistaken for: it must stay terminal.
  for (const status of [400, 401, 403, 404, 405, 410, 413, 422, 429, 431, 500, 501]) {
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

  // A transport failure — the server said nothing at all — is not terminal either, and is asked
  // again **after the silence backoff** rather than on the next frame (T-499). "Paced by the render
  // loop" is what this line used to say, and a render loop is 60 Hz: that pacing is the loop the
  // user reported, so the clock that paces it now is this cache's own.
  let netClock = 0;
  const net = harness({ inFlight: 4, now: () => netClock });
  net.cache.beginFrame(); net.cache.acquire(addr(3)); net.cache.endFrame();
  await flush();
  await net.fail(addr(3), new TypeError("Failed to fetch"));
  assert.equal(net.cache.stats.terminalFailures, 0, "a disconnect must not blank the place for the session");
  assert.equal(net.cache.queueDepth, 0, "…and must not spin: nothing is re-queued by the failure itself");
  net.cache.beginFrame(); net.cache.acquire(addr(3)); net.cache.endFrame();
  await flush();
  assert.equal(net.calls.length, 1, "the next frame does NOT ask again — that was the loop");
  netClock += 600;
  net.cache.beginFrame(); net.cache.acquire(addr(3)); net.cache.endFrame();
  await flush();
  assert.equal(net.calls.length, 2, "…and once the backoff opens it does: a silence is not terminal");
});

test("a 5xx the ROUTE cannot emit is the route saying nothing: silence ladder, never terminal (T-523)", async () => {
  // The route emits exactly 500, 501 and 503; every other 5xx reached us from something between
  // this client and `hk serve`. The user's cloudflared tunnel answers 502 for a slow tile; a
  // Cloudflare edge in front of it would answer 520/522/524; nginx would answer 502/504. The rule
  // is over the route's vocabulary rather than over that open list, so a proxy nobody has met yet
  // is handled too — and 598/599 below are exactly such a case, invented by proxies and in no RFC.
  for (const status of [502, 504, 507, 508, 520, 521, 522, 524, 527, 530, 598, 599]) {
    let clock = 0;
    const h = harness({ inFlight: 4, now: () => clock });
    const a = addr(status % 7);
    h.cache.beginFrame(); h.cache.acquire(a); h.cache.endFrame();
    await flush();
    await h.fail(a, httpError(status));
    assert.equal(h.cache.terminalPlaces, 0, `HTTP ${status} from a gateway made the place terminal`);
    assert.equal(h.cache.silent, true, `HTTP ${status} should arm the silence backoff`);
    h.cache.beginFrame(); h.cache.acquire(a); h.cache.endFrame();
    await flush();
    assert.equal(h.calls.length, 1, "not re-asked at frame rate while the backoff is armed");
    clock += 600;
    h.cache.beginFrame(); h.cache.acquire(a); h.cache.endFrame();
    await flush();
    assert.equal(h.calls.length, 2, `HTTP ${status}: the place is asked again once the backoff opens`);
    await h.settle(a);
    assert.equal(h.cache.silent, false, "one answer clears the ladder");
    assert.equal(h.cache.residentTiles, 1);
  }
});

// ——— T-499: a dead server is asked at a DECAYING rate, and one answer clears the ladder ———
//
// What the user saw: "on stream loss some tiles render purple and keep re-rendering left to right in
// a loop", and a page refresh clearing it. The re-render is this: `acquire` runs for every place on
// every frame, T-479 left *the server said nothing* as the one retryable outcome, and nothing said
// when. Measured in a browser with `hk serve` SIGKILLed under a live page (`ui/e2e/canvas-journey`
// test 4, which is the same claim on the wire): **56 failed requests in the first 5 s and 55 in the
// second** — flat. The e2e is the demonstration; this is the property.

test("a dead server is asked at a DECAYING rate, not at frame rate", async () => {
  let clock = 0;
  const h = harness({ inFlight: 4, now: () => clock });
  const places = [addr(1), addr(2), addr(3), addr(4), addr(5), addr(6)];
  // Ten seconds of a 60 Hz render loop over a viewport of six places, with nothing answering.
  const perWindow = [0, 0];
  for (let frame = 0; frame < 600; frame++) {
    const before = h.calls.length;
    h.cache.beginFrame();
    for (const a of places) h.cache.acquire(a);
    h.cache.endFrame();
    await flush();
    for (const [key, w] of [...h.waiting]) { w.reject(new TypeError("Failed to fetch")); h.waiting.delete(key); }
    await flush();
    perWindow[clock < 5000 ? 0 : 1] += h.calls.length - before;
    clock += 1000 / 60;
  }
  assert.ok(perWindow[0] < 60,
    `${perWindow[0]} requests in the first 5 s: a ladder that starts at 500 ms cannot spend that many`);
  assert.ok(perWindow[1] <= perWindow[0] * 0.5 + 4,
    `the client is still hammering a dead server: ${perWindow[0]} requests in the first 5 s and ` +
    `${perWindow[1]} in the second. The second window is where a ladder shows and a loop does not.`);
  assert.equal(h.cache.silent, true, "…and it says so, rather than only behaving differently");
});

test("…and ONE answer clears the ladder: a server that comes back is served at once", async () => {
  let clock = 0;
  const h = harness({ inFlight: 4, now: () => clock });
  h.cache.beginFrame(); h.cache.acquire(addr(1)); h.cache.endFrame();
  await flush();
  await h.fail(addr(1), new TypeError("Failed to fetch"));
  for (let i = 0; i < 4; i++) {
    clock += 60_000;
    h.cache.beginFrame(); h.cache.acquire(addr(1)); h.cache.endFrame();
    await flush();
    if (i < 3) await h.fail(addr(1), new TypeError("Failed to fetch"));
  }
  // The fifth request is the one the server answers.
  await h.settle(addr(1));
  assert.equal(h.cache.silent, false, "an answer is proof the route is there; the gate must fall at once");
  const before = h.calls.length;
  h.cache.beginFrame(); h.cache.acquire(addr(2)); h.cache.acquire(addr(3)); h.cache.endFrame();
  await flush();
  assert.equal(h.calls.length, before + 2,
    "recovery must be immediate and at the full cap — a client that stays throttled after the server " +
    "is back needs a page reload, which is the defect one step on");
});

test("a place with no usable answer is told apart from one that is merely not loaded", async () => {
  let clock = 0;
  const h = harness({ inFlight: 4, now: () => clock });
  h.cache.beginFrame();
  assert.equal(h.cache.acquire(addr(1)).failed, undefined, "before anything fails, a miss is just a miss");
  h.cache.endFrame();
  await flush();
  await h.fail(addr(1), new TypeError("Failed to fetch"));
  h.cache.beginFrame();
  const res = h.cache.acquire(addr(7));
  h.cache.endFrame();
  assert.equal(res.kind, "pending", "…and it is still never a cell state, so it can never be grey");
  assert.equal(res.kind === "pending" && res.failed, true,
    "a place this client cannot get an answer for must not be drawn as 'loading': that is a progress " +
    "bar that never finishes, the same defect as AWAITING promising arrival (T-441)");
});

test("an unreadable 200 is an ANSWER: a decode failure is terminal too", async () => {
  // Found by merging T-467, not by argument. Its new coverage encoding made a stale fixture's `200`
  // responses undecodable; `TileDecodeError` carries no HTTP status, so the rule above read it as
  // "the server said nothing" and the place was re-asked on every frame — 157 times in 700 ms. The
  // server had said plenty. A body this client cannot read is an answer, and asking again gets the
  // same bytes back.
  const h = harness({ inFlight: 4 });
  const a = addr(2);
  h.cache.beginFrame(); h.cache.acquire(a); h.cache.endFrame();
  await flush();
  await h.fail(a, new TileDecodeError("tile …: coverage has no plane for device any"));
  for (let frame = 0; frame < 60; frame++) {
    h.cache.beginFrame(); h.cache.acquire(a); h.cache.endFrame();
    await flush();
  }
  assert.deepEqual(h.calls, [keyOf(a)], `an undecodable response was re-asked ${h.calls.length} times`);
  assert.equal(h.cache.stats.terminalFailures, 1);
  assert.match(h.cache.refusalFor(a) ?? "", /coverage has no plane/, "the decoder's own words survive");
});

// ——— T-490: the refresh lane is PER COST CLASS, so a cheap tenant is not charged an expensive one ———
//
// **The finding this exists to preserve, because it is not obvious and cost a day to find.**
// `ui/e2e/live-edge.e2e.mjs` passed on `main` for a reason unrelated to what it asserts: the
// minimap's address was a **permanent 400** (the view lattice named 12 x 15 levels over a 4 x 4
// store — T-482), so T-479's terminal rule asked it once and never again, and the refresh lane plus
// all four in-flight slots belonged to the live pane. Declare the ceiling, and the minimap becomes
// a servable, *following* viewport that queues here beside the pane. Measured in the browser, same
// gestures, same box, only the server binary differing:
//
//   main   0/0 x68 @69ms    1/1 x3 @513ms   11/0 x2 @153ms   (73 requests, 5/5 samples drawn)
//   before 9/0 x16 @725ms   0/0 x14 @183ms  9/1 x8 @398ms    (41 requests, 2/5 drawn — FLAT)
//   after  0/0 x45 @128ms   9/0 x18 @650ms  9/1 x8 @660ms    (74 requests, 5/5 drawn)
//
// So the next person to see the minimap go quiet should not conclude the lane is fine because this
// test is green: **check what the minimap's address is answering first.**
//
// The mechanism is T-460's own `serverMs` bug one level up, and its `done` handler already argues
// the principle: a mean that folds in the minimap's coarse read charges the live edge for a tile it
// is not. The fix there was to measure the lane's own cost on its own request — but the *lane* was
// still a mixture, so a 725 ms coarse revalidation set a 2.2 s gate on a 183 ms fine one.
//
// **Fairness is a sixth property, not a replacement.** T-460's five still hold and are still
// asserted above: only the live-edge address, only for a FOLLOWING viewport, only at the level it
// was drawn at, never inside one `tCellNs(level_t)`, and one revalidation in flight into a slot the
// ordinary queue could not use. This adds: and each cost class gets its own turn and its own clock.

/** A coarse live-edge tile and the viewport drawing it — the minimap's shape, at `level_f = 9`. */
const COARSE_LEVEL_F = 9;
const coarseTile = (fIndex = 0) => addr(fIndex, 0, COARSE_LEVEL_F, 0);
/** `wide` tiles across, because a minimap is many tiles and that is what a single FIFO starves on. */
const coarseView = (wide = 1, edgeNs = EDGE_NS): Viewport => ({
  box: { f0Hz: 0, f1Hz: LAT.f0Hz * 2 ** COARSE_LEVEL_F * LAT.cells * wide, t0Ns: edgeNs - 100e9, t1Ns: edgeNs },
  levelF: COARSE_LEVEL_F, levelT: 0,
});

/**
 * Run the live loop with `views` following, answering a fine tile at once and a coarse one only
 * after `coarseMs` of simulated time — the measured 37 ms against 246 ms, exaggerated so the effect
 * cannot be a rounding.
 *
 * Returns each level's **revalidations** — total fetches minus one first fetch per distinct address.
 * Counting raw fetches would fold in the ordinary queue's initial misses, which are a different
 * mechanism: eight coarse tiles cost eight misses whatever the lane does, and an assertion over the
 * sum reads as lane behaviour while measuring queue depth. (It did, on the first draft of this.)
 */
async function liveMixed(h: ReturnType<typeof harness>, clock: { t: number }, ms: number,
  views: Viewport[], coarseMs: number, coarseWide = 1) {
  const issuedAt = new Map<string, number>();
  const isCoarse = (k: string) => parseKey(k)!.levelF === COARSE_LEVEL_F;
  for (let step = 0; step < ms / 100; step++) {
    clock.t += 100;
    h.cache.beginFrame();
    for (const v of views) {
      if (v.levelF !== COARSE_LEVEL_F) { h.cache.acquire(edgeTile()); continue; }
      for (let i = 0; i < coarseWide; i++) h.cache.acquire(coarseTile(i));
    }
    h.cache.setViewports(LAT, views);
    h.cache.endFrame();
    // The edge grows; the boxes do not need to, because a viewport's overlap with these tiles is
    // what `drawnBy` reads and that is unchanged by where the newest row is.
    h.cache.refreshEdge(LAT, edgeAt(clock.t), views);
    await flush();
    for (const [key, w] of [...h.waiting]) {
      if (!issuedAt.has(key)) issuedAt.set(key, clock.t);
      if (isCoarse(key) && clock.t - issuedAt.get(key)! < coarseMs) continue;
      issuedAt.delete(key);
      w.resolve(data(parseKey(key)!));
      h.waiting.delete(key);
      await flush();
    }
  }
  const per = (lf: number) => {
    const at = h.calls.filter((k) => parseKey(k)!.levelF === lf);
    return at.length - new Set(at).size;
  };
  return { fine: per(0), coarse: per(COARSE_LEVEL_F) };
}

test("an expensive lane beside it does not slow the live edge's own refresh", async () => {
  const alone = { t: 0 };
  const h1 = harness({ inFlight: 4, now: () => alone.t, serverMsGuess: 20 });
  const solo = await liveMixed(h1, alone, 10_000, [edgeView()], 800);

  const together = { t: 0 };
  const h2 = harness({ inFlight: 4, now: () => together.t, serverMsGuess: 20 });
  const mixed = await liveMixed(h2, together, 10_000, [edgeView(), coarseView()], 800);

  // Non-vacuity: the expensive lane has to have actually run, or this measures nothing.
  assert.ok(mixed.coarse > 1, `the coarse lane was asked ${mixed.coarse} time(s) — it must refresh too`);
  // **The claim.** The live edge's cadence is a property of the live edge, not of what else is on
  // screen. Before T-490 one shared `refreshNextIssue` made `mixed.fine` a fraction of `solo.fine`.
  assert.ok(mixed.fine >= solo.fine * 0.6,
    `the live edge refreshed ${mixed.fine} times with an expensive lane beside it against ` +
    `${solo.fine} times alone — an unrelated viewport's cost must not set the live edge's cadence`);
});

test("…and the lanes take TURNS, so a WIDE coarse viewport cannot queue ahead of the live edge", async () => {
  // **Why this needs a wide coarse viewport, and what it caught.** Written with ONE coarse tile it
  // passed against a deliberately collapsed single lane — the cheap lane re-queues on its own
  // cadence, so `fine > coarse` came out true under FIFO too, and the test was a sound proof of an
  // adjacent claim. A minimap is not one tile; it is ~19 across the device range, which is exactly
  // what a single FIFO starves on: nineteen 650 ms entries ahead of the live edge is 12 s of
  // nothing. Round-robin is the property, and this is the shape that can see it.
  const run = async (wide: number) => {
    const clock = { t: 0 };
    const h = harness({ inFlight: 4, now: () => clock.t, serverMsGuess: 20 });
    return liveMixed(h, clock, 10_000, [edgeView(), coarseView(wide)], 400, wide);
  };
  const narrow = await run(1);
  const wide = await run(8);

  // Non-vacuity: the wide run must really have more coarse addresses in the lane.
  assert.ok(wide.coarse >= narrow.coarse,
    `coarse revalidations ${narrow.coarse} -> ${wide.coarse}: widening must not shrink the lane`);
  // **The claim, and the one FIFO cannot satisfy.** Under a single queue, eight coarse addresses sit
  // ahead of the live edge and it waits eight turns — at the measured 650 ms each that is 5 s of no
  // rows. Round-robin makes it wait ONE turn, so the live edge's rate is unmoved by how DEEP the
  // other lane is. A lane's rate is a share of its own cost, never of another lane's depth.
  assert.ok(wide.fine >= narrow.fine * 0.8,
    `the live edge revalidated ${narrow.fine} times beside one coarse tile and ${wide.fine} times ` +
    "beside eight — another viewport's queue depth must not set the live edge's cadence");
});

// ——— T-491: ONE contended completion must not buy the live edge seconds of silence ———
//
// T-490 left a named residual: in 3 of 13 browser runs the live-edge e2e's `t+8 s` sample was FLAT,
// always the FIRST sample. Its stated mechanism was that the minimap's ~19-miss initial fill holds
// `pumpRefresh`'s `inflight < limit` false. **Measured in a browser, that is not what happens** —
// instrumenting every `pumpRefresh` outcome over 16 runs, the "no slot" bail fired only while the
// refresh lanes were EMPTY (the live tile was not yet resident, so there was nothing to revalidate);
// not once did a queued revalidation fail for want of a slot. The slots are indeed full for the
// first ~2.5 s, and that window closes six seconds before the failing sample.
//
// What the initial fill actually does is **inflate the one sample the lane's clock is set from.**
// The route serialises tile reads on the history mutex, so the live lane's wall time is production
// plus whatever a neighbour's hold added: the same `0/0` address answered in 844 ms during the fill
// and in 80 ms after it, and `(REFRESH_DUTY - 1) x 844 ms` then bought 2.5 s of silence for an 80 ms
// tile. This is T-490's own finding surviving as a *sample* rather than as a mean.
//
// The control is the same run with that one answer served promptly. The claim is that the lane's
// cadence is the same either way — a neighbour's cost must not become the live edge's clock.

/**
 * Run the live loop, answering every fetch at once **except one**: the fetch issued at or after
 * `slowAfterMs` is held for `slowMs` of simulated time. Returns when that one landed, the gap to
 * the next completion, and how many completions there were.
 */
async function liveOneSlow(h: ReturnType<typeof harness>, clock: { t: number }, ms: number,
  slowAfterMs: number, slowMs: number) {
  const issuedAt = new Map<string, number>();
  let slowTag: string | null = null, slowDone = 0, slowSpent = 0, gapAfter = -1, n = 0;
  for (let step = 0; step < ms / 100; step++) {
    clock.t += 100;
    h.cache.beginFrame();
    h.cache.acquire(edgeTile());
    h.cache.setViewports(LAT, [edgeView()]);
    h.cache.endFrame();
    h.cache.refreshEdge(LAT, edgeAt(clock.t), [edgeView()]);
    await flush();
    for (const [key, w] of [...h.waiting]) {
      if (!issuedAt.has(key)) issuedAt.set(key, clock.t);
      const started = issuedAt.get(key)!;
      const tag = `${key}@${started}`;
      if (slowTag === null && started >= slowAfterMs) slowTag = tag;
      if (tag === slowTag && clock.t - started < slowMs) continue;
      if (tag === slowTag && !slowDone) { slowDone = clock.t; slowSpent = clock.t - started; }
      else if (slowDone && gapAfter < 0) gapAfter = clock.t - slowDone;
      issuedAt.delete(key);
      w.resolve(data(parseKey(key)!));
      h.waiting.delete(key);
      n++;
      await flush();
    }
  }
  return { slowDone, slowSpent, gapAfter, n };
}

test("ONE contended answer does not set the live lane's clock — the cadence survives it", async () => {
  const SLOW_MS = 800, AT = 3000, RUN = 9000;
  const c1 = { t: 0 };
  const clean = await liveOneSlow(harness({ inFlight: 4, now: () => c1.t, serverMsGuess: 20 }), c1, RUN, AT, 0);
  const c2 = { t: 0 };
  const contended = await liveOneSlow(harness({ inFlight: 4, now: () => c2.t, serverMsGuess: 20 }), c2, RUN, AT, SLOW_MS);

  // **Non-vacuity, both halves.** The lane has to have been running — a lane that never refreshed
  // has a gap of -1 and would sail through the claim — and the contended answer has to have actually
  // been contended, or the two runs are the same run twice. This is the check T-490's own depth
  // guard failed to make: it counted the ordinary queue's misses alongside revalidations and stayed
  // green with the lanes collapsed.
  assert.ok(clean.n >= 5 && contended.n >= 5,
    `only ${clean.n} / ${contended.n} completions — there is no cadence here to measure`);
  assert.ok(contended.slowSpent >= SLOW_MS,
    `the "slow" answer took ${contended.slowSpent} ms, not ${SLOW_MS} — the control and the subject are the same run`);
  assert.ok(clean.gapAfter >= 0 && contended.gapAfter >= 0,
    `no completion followed the marked one (clean ${clean.gapAfter}, contended ${contended.gapAfter})`);

  // **The claim.** The gap to the next revalidation is a property of what this lane costs, not of
  // how long a neighbour held the lock during one of its answers. Charged from that single sample it
  // is (REFRESH_DUTY - 1) x 800 = 2400 ms; charged from the lane's own recent minimum it is the
  // clean run's gap, within one 100 ms step of the loop.
  assert.ok(contended.gapAfter <= clean.gapAfter + 300,
    `after ONE ${contended.slowSpent} ms answer the live edge waited ${contended.gapAfter} ms for its ` +
    `next revalidation against ${clean.gapAfter} ms with the same answer served promptly — a ` +
    "neighbour's history-lock hold must not become the live edge's clock (T-491)");
  // And the bound it must not have traded away: a lane that is GENUINELY expensive still gets rarer
  // on its own. That is the run above ("a fixed share of MEASURED capacity"), where every answer
  // costs 600 ms, so the minimum over the window IS 600 ms and the duty cap still binds.
});

// ——— T-471: a bounded ring just outside the viewport, at the LOWEST priority in the file ———
//
// `prefetchRing` is its own lane, alongside the ordinary queue and the refresh lane. It is never
// called from `setViewports` — see the file header — so every test above this line, none of which
// calls it, is a control: it proves the ring changes nothing about the many behaviours already
// covered when nobody asks for it.

test("T-471: the ring around a lone viewport is exactly its one-tile border", () => {
  // paneView(0, 1) is one tile at level 0/0. The border just outside it, minus the tile itself, is
  // the three neighbours a one-tile-radius ring names: right, below, and the corner between them.
  // ONE in-flight slot, taken by the visible miss, so nothing here can pump straight out of the
  // queue and understate it — this test is about the SHAPE of the ring, not its issuance.
  const h = harness({ inFlight: 1 });
  const view = paneView(0, 1);
  h.cache.beginFrame();
  h.cache.acquire(addr(0));
  h.cache.setViewports(LAT, [view]);
  h.cache.endFrame(); // addr(0) takes the cache's one slot
  const n = h.cache.prefetchRing(LAT, [view]);
  assert.equal(n, 3, `expected the 3-tile border of one tile, got ${n}`);
  assert.equal(h.cache.prefetchDepth, 3);
});

test("T-471: the ring is issued only once the visible set it shares a budget with is satisfied", async () => {
  const h = harness({ inFlight: 1 }); // one slot: the sharpest test that the ring cannot take it
  const view = paneView(0, 1);
  h.cache.beginFrame();
  h.cache.acquire(addr(0));
  h.cache.setViewports(LAT, [view]);
  h.cache.endFrame();
  await flush();
  const n = h.cache.prefetchRing(LAT, [view]);
  await flush();
  assert.ok(n > 0, "the ring around a lone viewport must not be empty, or this proves nothing");
  assert.deepEqual(h.calls, [keyOf(addr(0))], "the one slot went to the visible miss, not the ring");
  assert.equal(h.cache.stats.prefetchIssued, 0, "no ring request went out while the visible one was outstanding");

  await h.settle(addr(0));
  assert.ok(h.cache.stats.prefetchIssued >= 1, "once the visible set is satisfied the ring gets its turn");
});

test("T-471: the ring — including its own in-flight fetch — is cancelled the instant the viewport moves", async () => {
  // A controllable clock: generation is throttled to once per PREFETCH_SCAN_MS, so a SECOND scan
  // for the moved viewport needs the clock to actually have advanced past it, same as production.
  let clock = 0;
  const h = harness({ inFlight: 4, now: () => clock });
  const view = paneView(0, 1);
  h.cache.beginFrame();
  h.cache.acquire(addr(0));
  h.cache.setViewports(LAT, [view]);
  h.cache.endFrame();
  await flush();
  await h.settle(addr(0));

  h.cache.prefetchRing(LAT, [view]);
  await flush();
  assert.ok(h.cache.stats.prefetchIssued >= 1, "the ring must have started fetching, or cancelling it tests nothing");
  const abandonedBefore = h.cache.stats.abandoned;

  clock += 300; // past PREFETCH_SCAN_MS, so the moved viewport's ring is actually (re-)generated
  const moved = paneView(50, 51);
  h.cache.beginFrame();
  h.cache.acquire(addr(50));
  h.cache.setViewports(LAT, [moved]);
  h.cache.endFrame();
  const n = h.cache.prefetchRing(LAT, [moved]);
  // Asserted BEFORE the flush: `abandon()` charges the budget synchronously inside `prefetchRing`
  // itself, and once the old aborted fetch's rejection is processed (below) it is allowed to free a
  // slot the new ring's own trailing `pump` may then use, which would change `prefetchDepth` out from
  // under an equality check taken after that happens.
  assert.ok(h.cache.stats.abandoned > abandonedBefore,
    "the abandoned in-flight ring fetch is charged to the budget exactly like any other abort (T-454)");
  assert.ok(h.cache.stats.prefetchCancelled >= 1, "the ring's own cancellations are counted apart from `cancelled`");
  assert.ok(n > 0, "the new viewport gets its own ring");
  assert.equal(h.cache.prefetchDepth, n, "nothing from the old viewport's ring survives the move");
  await flush();
});

test("T-471: an ORDINARY setViewports call does not abort an in-flight ring fetch — nothing moved", async () => {
  // The control for the test above. `Surface.render()` calls `setViewports` every frame regardless
  // of whether anything called `prefetchRing` at all, and its own predicate ("is this tile inside
  // some viewport's own box") is false for every ring address by construction — a ring tile is
  // defined as one that is NOT inside the box. Without an explicit exemption this call would abort
  // a ring fetch one frame after issuing it, every time, viewport unmoved.
  const h = harness({ inFlight: 4 });
  const view = paneView(0, 1);
  h.cache.beginFrame();
  h.cache.acquire(addr(0));
  h.cache.setViewports(LAT, [view]);
  h.cache.endFrame();
  await flush();
  await h.settle(addr(0));

  h.cache.prefetchRing(LAT, [view]);
  await flush();
  assert.ok(h.cache.stats.prefetchIssued >= 1, "the ring must have started fetching, or this proves nothing");
  const abandonedBefore = h.cache.stats.abandoned;

  // The next frame redraws the SAME viewport, exactly like `Surface.render()` does every tick.
  h.cache.beginFrame();
  h.cache.acquire(addr(0));
  h.cache.setViewports(LAT, [view]);
  h.cache.endFrame();
  await flush();

  assert.equal(h.cache.stats.abandoned, abandonedBefore,
    "an ordinary setViewports call must not abort a ring fetch just because it sits outside the box");
});

test("T-471: the ring asks for nothing while the cache is backing off from a 503", async () => {
  let clock = 0;
  const h = harness({ inFlight: 4, busyBackoffMs: 200, now: () => clock });
  const view = paneView(0, 1);
  h.cache.beginFrame();
  h.cache.acquire(addr(0));
  h.cache.setViewports(LAT, [view]);
  h.cache.endFrame();
  await flush();
  await h.fail(addr(0), new TileBusyError(4, "too many tile reads in flight (limit 4)"));
  const before = h.calls.length;

  const n = h.cache.prefetchRing(LAT, [view]);
  await flush();
  assert.ok(n > 0, "the ring still knows what it wants — it just must not ask for it");
  assert.equal(h.calls.length, before, "no ring request escaped while the client is backing off a 503");
  assert.equal(h.cache.stats.prefetchIssued, 0);
});

// ——— THE TWO RULES THE BROWSER TIER CAUGHT THE RING BREAKING ———
//
// The test above is real but narrow: with the clock pinned at 0 it never leaves the `busyBackoffMs`
// window, so it certifies "the ring is quiet for 200 ms after a 503" and nothing more. `main` failed
// `ui/e2e/surface-nav.e2e.mjs` on exactly the regime it does not reach — the 8 s AFTER that window,
// in which the operating cap is still halved. Measured there, three runs each, full-suite ordering:
// with the ring as merged, 27–30 tile requests and 2–3 refusals during the seconds in which NOTHING
// moved the view; with the ring disabled outright, 0 and 0. The two tests below are those two
// findings, each as the smallest fact that would have failed.

test("T-471: the ring stays silent AFTER the 503 backoff expires — until the cap is back at its ceiling", async () => {
  let clock = 0;
  const h = harness({ inFlight: 4, busyBackoffMs: 200, now: () => clock });
  const view = paneView(0, 1);
  h.cache.beginFrame();
  h.cache.acquire(addr(0));
  h.cache.setViewports(LAT, [view]);
  h.cache.endFrame();
  await flush();
  await h.fail(addr(0), new TileBusyError(4, "too many tile reads in flight (limit 4)"));

  // Well past the backoff window, so `pump` runs freely and only the OPERATING CAP is left to hold
  // the ring back. The requeued visible miss goes out on this pump; settling it leaves the cache
  // completely idle, which is what makes the assertion below about the cap and nothing else.
  clock = 5000;
  h.cache.prefetchRing(LAT, [view]);
  await flush();
  await h.settle(addr(0));
  assert.equal(h.cache.inFlightCount, 0, "the cache must be idle, or the budget — not the cap — is what is being tested");
  assert.equal(h.cache.inFlightLimit, 2, "the 503 must have halved the operating cap, or this tests nothing");
  assert.ok(h.cache.prefetchDepth > 0, "the ring must still know what it wants, or there is nothing to withhold");
  assert.equal(h.cache.stats.prefetchIssued, 0,
    "the ring asked while the client was still recovering from a refusal: speculative work must not " +
    "spend the share the visible path is trying to win back — and being refused for it resets the recovery");

  // …and it is a PAUSE, not a kill switch. `RECOVER_AFTER` successes buy a slot back, so 16 of them
  // take the cap 2 -> 3 -> 4, and at the ceiling the ring is allowed out again.
  for (let i = 0; i < 16; i++) {
    const a = addr(100 + i);
    h.cache.beginFrame();
    h.cache.acquire(a);
    h.cache.endFrame();
    await flush();
    await h.settle(a);
  }
  assert.equal(h.cache.inFlightLimit, 4, "the cap must be back at its ceiling, or the resume below proves nothing");
  h.cache.prefetchRing(LAT, [view]);
  await flush();
  assert.ok(h.cache.stats.prefetchIssued >= 1, "once recovered, the ring gets its turn again");
});

test("T-471: the ring asks for nothing while ANY request of this cache's is outstanding at the route", async () => {
  // **Four slots, deliberately** — not the one-slot harness the tests above use. With one slot
  // "under the budget" and "idle" are the same sentence, which is why the merged rule read as
  // correct: it said `>= effectiveLimit`, so with three slots free the ring took one and sat beside
  // work the next viewport change would abort. An abort does not give the route its slot back — it
  // charges `abandonedSlots` for a production `hk-api` carries on doing and throws away — so that is
  // how a ring tile turns into a refusal for somebody else. Measured on the wire: 46–53 aborts a run
  // with the ring issuing under the cap, 6–8 with it silent.
  const h = harness({ inFlight: 4 });
  const view = paneView(0, 1);
  h.cache.beginFrame();
  h.cache.acquire(addr(0));
  h.cache.setViewports(LAT, [view]);
  h.cache.endFrame();
  await flush();
  assert.equal(h.cache.inFlightCount, 1, "the visible miss must be outstanding, or this tests nothing");
  assert.ok(h.cache.inFlightCount < h.cache.inFlightLimit, "…with slots to spare, or the budget rule alone would explain the result");

  const n = h.cache.prefetchRing(LAT, [view]);
  await flush();
  assert.ok(n > 0, "the ring must want something, or there is nothing to withhold");
  assert.equal(h.cache.stats.prefetchIssued, 0, "the ring took a free slot beside work that can still be aborted");

  await h.settle(addr(0));
  assert.ok(h.cache.stats.prefetchIssued >= 1, "and it is not starved: once the cache is idle the ring goes out");
});

test("T-471: the ring also stops while the transport is silent (T-499)", async () => {
  const h = harness({ inFlight: 4 });
  const view = paneView(0, 1);
  h.cache.beginFrame();
  h.cache.acquire(addr(0));
  h.cache.setViewports(LAT, [view]);
  h.cache.endFrame();
  await flush();
  await h.fail(addr(0), new Error("boom"));
  assert.ok(h.cache.silent, "the harness's failure must arm the silence gate, or this tests nothing");
  const before = h.calls.length;

  h.cache.prefetchRing(LAT, [view]);
  await flush();
  assert.equal(h.calls.length, before, "no ring request escapes while the route is silent");
  assert.equal(h.cache.stats.prefetchIssued, 0);
});

test("T-471: the ring is bounded by BYTES, not by tile count", async () => {
  const wide = paneView(0, 20);
  // Control: the same 20-tile-wide viewport with a budget generous enough not to trim anything —
  // establishes the naive ring is bigger than the cap below, or the cap proves nothing.
  const unbounded = harness({ inFlight: 1, budgetBytes: 512 * BYTES, prefetchBudgetBytes: 512 * BYTES });
  unbounded.cache.beginFrame();
  for (let i = 0; i < 20; i++) unbounded.cache.acquire(addr(i));
  unbounded.cache.setViewports(LAT, [wide]);
  unbounded.cache.endFrame();
  const full = unbounded.cache.prefetchRing(LAT, [wide]);
  await flush();
  assert.ok(full > 4, `the naive ring around 20 tiles is only ${full} wide — too small to prove a bound`);

  // Subject: nothing resident yet, so the bound falls back to a conservative fraction of the whole
  // budget (`budgetBytes / 512`) rather than a guess biased toward "no limit at all".
  const h = harness({ inFlight: 1, budgetBytes: 512 * BYTES, prefetchBudgetBytes: 4 * BYTES });
  h.cache.beginFrame();
  for (let i = 0; i < 20; i++) h.cache.acquire(addr(i));
  h.cache.setViewports(LAT, [wide]);
  h.cache.endFrame();
  const n = h.cache.prefetchRing(LAT, [wide]);
  await flush();
  assert.ok(n <= 4, `queued ${n} ring tiles against a 4-tile (${4 * BYTES} B) budget at ${BYTES} B/tile`);
  assert.equal(h.cache.prefetchDepth, n);
});

test("T-471: the live-edge refresh still wins the slot over the ring", async () => {
  const clock = { t: 0 };
  const h = harness({ inFlight: 1, now: () => clock.t, serverMsGuess: 20 });
  h.cache.beginFrame();
  h.cache.acquire(edgeTile(0));
  h.cache.endFrame();
  await flush();
  await h.settle(edgeTile(0));
  clock.t += 5_000; // comfortably past the 1 s cadence at level_t 0

  h.cache.beginFrame();
  h.cache.acquire(edgeTile(0));
  h.cache.setViewports(LAT, [edgeView()]);
  h.cache.endFrame();
  // **Call order matches the real one** (`ui/src/surface/preview.ts`'s `frame()`): refresh first, so
  // its trailing `pump` gets the one slot before the ring's trailing `pump` ever sees it free.
  h.cache.refreshEdge(LAT, EDGE_NS, [edgeView()]);
  h.cache.prefetchRing(LAT, [edgeView()]);
  await flush();

  assert.ok(h.cache.prefetchDepth > 0, "the ring must actually have something queued, or this proves nothing");
  assert.equal(h.cache.stats.edgeRefreshes, 1, "the due refresh must have been issued");
  assert.equal(h.cache.stats.prefetchIssued, 0, "the one slot went to the live edge, not the ring");

  // And once the refresh lands, the slot is free again and the ring gets its turn.
  await h.settle(edgeTile(0));
  assert.ok(h.cache.stats.prefetchIssued >= 1, "the ring is not starved forever, only while the edge is due");
});
