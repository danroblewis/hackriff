// T-1039: tile loading survives an unstable network — end to end through the REAL production
// wiring (`TileCache` over `batchedTileSource`, exactly as `../src/surface/preview.ts` builds it),
// not just at the `TileCache` unit level `surface-cache.test.ts` already covers.
//
// The claim under test is the ticket's own sentence: **a failed or slow batch never clears what is
// drawn, no PENDING appears over previously drawn tiles, and requests in flight are coalesced per
// address.** Jittered backoff itself, and the stale marker's own threshold, are unit-tested directly
// against `TileCache` in `surface-cache.test.ts` (T-1039); this file is the one place that proves the
// three claims hold when a real batch is the thing failing or stalling.

import test from "node:test";
import assert from "node:assert/strict";

import { keyOf, type TileAddr } from "../src/surface/lattice";
import { batchedTileSource } from "../src/surface/tilebatch";
import { TileCache } from "../src/surface/tilecache";
import { buildOfflineBatch } from "../src/sw/tile-cache-logic";

const addr = (fIndex: number, over: Partial<TileAddr> = {}): TileAddr => ({
  device: "any", scheme: "view", levelF: 0, levelT: 0, fIndex, tIndex: 7, cells: 256, ...over,
});

function tileBody(n = 2): unknown {
  const cells = n * n;
  return {
    key: { level_f: 0, level_t: 0, f_index: 0, t_index: 7, cells: n },
    extent: { nf: n, nt: n, f_lo_hz: 0, f_hi_hz: 100, f_cell_hz: 50, t0_s: 0, t1_s: 1, t_cell_s: 0.5 },
    axes: { frequency: { cell_hz: 50, levels: 1, max_level: 0, tile_hz: 100 },
            time: { cell_s: 0.5, levels: 1, max_level: 0, tile_s: 1 } },
    grid: { nf: n, nt: n, cells, observed_cells: cells, encoding: { planes: "json" },
            max_db: Array.from({ length: cells }, () => -80) },
    coverage: { states: ["observed"], grid: { nf: n, nt: n }, planes: [{ cells, runs: [0, cells] }],
                any: { plane: 0 }, selected: { plane: 0, present: true } },
    resolution: { source: "live-iq", live: true, answered: 0, statement: "…",
                  fold: { frequency: { direction: "exact" }, time: { direction: "exact" } } },
    cost: { build_ms: 1, source_cells: 4, chunks: 1 },
  };
}

const flush = () => new Promise((r) => setImmediate(r));

const LAT = { scheme: "view", cells: 256, f0Hz: 6250, t0Ns: 1e9, levelsF: 20, levelsT: 15 } as const;
const FULL_BOX = { f0Hz: 0, f1Hz: 1e12, t0Ns: 0, t1Ns: 1e15 };

function harness(clock: { t: number }) {
  const calls: string[][] = []; // one entry per HTTP request, listing the addresses it carried
  let downUntilNextCall = false;
  const fetchFn = async (url: string) => {
    const spellings = new URLSearchParams(url.split("?")[1]).get("addresses")!.split(",");
    calls.push(spellings);
    if (downUntilNextCall) throw new Error("network drop");
    return {
      ok: true, status: 200, statusText: "OK",
      json: async () => ({
        tiles: spellings.map((s) => ({ address: { spelling: s }, status: 200, tile: tileBody() })),
        truncated: false, remaining: [],
      }),
    };
  };
  const source = batchedTileSource("tok", fetchFn as never, { schedule: (fn) => void Promise.resolve().then(fn) });
  const cache = new TileCache<{ id: number }>(
    { upload: () => ({ id: 0 }), destroy: () => {} },
    source,
    // staleAfterMs well above LAT's own level-0 cadence margin (STALE_CADENCE_MARGIN x 1 s = 2 s),
    // so it is the flat default doing the work in these tests, not the per-tile cadence floor
    // (which `surface-cache.test.ts`'s own "COARSE level" test covers directly).
    { now: () => clock.t, random: () => 0, staleAfterMs: 5000 },
  );
  return { cache, calls, setDown: (v: boolean) => { downUntilNextCall = v; } };
}

test("a dropped batch never clears the resident tile it was trying to revalidate", async () => {
  const clock = { t: 0 };
  const h = harness(clock);
  h.cache.setViewports(LAT, [{ box: FULL_BOX, levelF: 0, levelT: 0 }]);
  h.cache.beginFrame(); h.cache.acquire(addr(1)); h.cache.endFrame();
  await flush();
  assert.equal(h.cache.acquire(addr(1)).kind, "resident");
  // The network drops, and a revalidation goes out and fails through the real batching layer.
  h.setDown(true);
  clock.t = 5000; // staleAfterMs: the copy is now reportable as stale, but must still be ON SCREEN
  h.cache.refreshEdge(LAT, 0, [{ box: FULL_BOX, levelF: 0, levelT: 0 }]);
  await flush();
  const during = h.cache.acquire(addr(1));
  assert.equal(during.kind, "resident", "the last good tile stays resident WHILE the batch is failing");
  assert.equal(h.cache.isStale(addr(1)), true, "…and the honesty tier says it is unconfirmed");
  h.setDown(false);
  const after = h.cache.acquire(addr(1));
  assert.equal(after.kind, "resident", "and after the failure lands, still nothing was cleared");
  assert.notEqual(after.kind, "pending", "no PENDING is ever drawn over what was already on screen");
});

test("a batch failure backs off — the retry does not re-hammer the route on the very next frame", async () => {
  const clock = { t: 0 };
  const h = harness(clock);
  h.cache.setViewports(LAT, [{ box: FULL_BOX, levelF: 0, levelT: 0 }]);
  h.setDown(true);
  h.cache.beginFrame();
  h.cache.acquire(addr(2));
  h.cache.endFrame();
  await flush();
  assert.equal(h.calls.length, 1, "the first ask goes out and fails");
  // The renderer asks every frame regardless (`acquire` is called for every place drawn, every
  // frame) — the backoff's job is to keep that from reaching the wire, not to stop it being asked.
  h.cache.beginFrame(); h.cache.acquire(addr(2)); h.cache.endFrame();
  await flush();
  assert.equal(h.calls.length, 1, "the very next frame does not retry: the silence gate is armed");
  clock.t = 600; // past the (unjittered, random()=0) first rung of the ladder
  h.setDown(false);
  h.cache.beginFrame(); h.cache.acquire(addr(2)); h.cache.endFrame();
  await flush();
  assert.equal(h.calls.length, 2, "and it reopens once the backoff elapses");
  assert.equal(h.cache.acquire(addr(2)).kind, "resident", "the retry through the real batch layer lands the tile");
});

test("two panes wanting the same address in one frame coalesce to ONE request through the batch layer", async () => {
  const clock = { t: 0 };
  const h = harness(clock);
  h.cache.setViewports(LAT, [{ box: FULL_BOX, levelF: 0, levelT: 0 }]);
  const a = addr(3);
  h.cache.beginFrame();
  assert.equal(h.cache.acquire(a).kind, "pending"); // pane 1
  assert.equal(h.cache.acquire(a).kind, "pending"); // pane 2, same address, same frame
  h.cache.endFrame();
  await flush();
  assert.equal(h.calls.length, 1, "one HTTP request, not two");
  assert.deepEqual(h.calls[0], [keyIndexOf(a)], "carrying the address once, not twice");
  h.cache.beginFrame();
  assert.equal(h.cache.acquire(a).kind, "resident");
  assert.equal(h.cache.acquire(a).kind, "resident");
});

/** The batch route's own spelling for one address (`level_f.level_t.f_index.t_index`), read back
 * off the same key the cache uses internally, so this test does not hand-derive it twice. */
function keyIndexOf(a: TileAddr): string {
  const [, , levelF, levelT, fIndex, tIndex] = keyOf(a).split("|");
  return `${levelF}.${levelT}.${fIndex}.${tIndex}`;
}

// ——— T-1039 review fix 2/2: the SW's OFFLINE reconstruction must not create a tight retry loop ———
//
// The trigger the review named exactly: the network is down, a batch asks for two addresses, one
// (A) is a sealed tile the Service Worker has cached and the other (B) is not. The worker (through
// `buildOfflineBatch`, exercised directly here — the same function `../src/sw/tiles-sw.ts` calls,
// which cannot itself run under `node:test`) must answer both, not put B in `remaining` (which
// `tilebatch.ts` requeues on the very next microtask with no delay at all).

test("an offline SW answer mixing a cached hit and an uncached miss backs the miss off — no tight retry loop", async () => {
  // The trigger, exactly: address A is a sealed tile the worker has cached, address B is not, and
  // the network is down. Under the bug the review found, B was carried in `remaining` — which
  // `tilebatch.ts` requeues on the very next MICROTASK with no delay at all, so every one of B's
  // "failures" bypassed `TileCache.failed()` entirely and the ladder this ticket adds never got a
  // chance to arm. Fixed, B is answered as its own 502 entry, which the client REJECTS the caller
  // with — reaching `failed()`'s real silent-failure backoff.
  //
  // (One nuance, not part of the bug: A's SUCCESS resolves in the same round trip as B's failure,
  // and `TileCache.succeeded()` — pre-existing, general T-499 behaviour, unrelated to this fix —
  // clears the WHOLE silence ladder on any completion at all, so B's very first backoff is
  // immediately un-armed by A's co-occurring hit and gets one extra, undelayed retry. That is not
  // the "for as long as the network is down" tight loop the review named: A is resident after round
  // 1 and never re-enters a request again, so from round 2 on nothing is left to reset the ladder
  // and it holds and escalates exactly as the equivalent all-miss case already does. Both facts are
  // asserted below rather than only the friendlier one.)
  const clock = { t: 0 };
  const calls: string[][] = [];
  const cachedSealed = new Map<string, unknown>([[keyIndexOf(addr(1)), tileBody()]]);
  const fetchFn = async (url: string) => {
    const spellings = new URLSearchParams(url.split("?")[1]).get("addresses")!.split(",");
    calls.push(spellings);
    const offline = buildOfflineBatch(spellings, cachedSealed);
    return { ok: true, status: 200, statusText: "OK", json: async () => offline };
  };
  const source = batchedTileSource("tok", fetchFn as never, { schedule: (fn) => void Promise.resolve().then(fn) });
  const cache = new TileCache<{ id: number }>(
    { upload: () => ({ id: 0 }), destroy: () => {} },
    source,
    { now: () => clock.t, random: () => 0 },
  );
  cache.setViewports(LAT, [{ box: FULL_BOX, levelF: 0, levelT: 0 }]);
  cache.beginFrame();
  cache.acquire(addr(1)); // A: cached sealed tile
  cache.acquire(addr(2)); // B: never cached
  cache.endFrame();
  await flush();
  await flush();
  assert.equal(calls.length, 1, "one batch request for both addresses");
  assert.equal(cache.acquire(addr(1)).kind, "resident", "A resolves from the worker's cached sealed tile");
  assert.equal(cache.stats.silentFailures, 1,
    "B's per-entry miss reached TileCache.failed() and armed the silent-failure backoff — the fix this test proves");
  // A is now resident and drops out of every further request, so from here B is the ONLY thing
  // being asked — this is the steady state a real outage actually looks like.
  cache.beginFrame(); cache.acquire(addr(2)); cache.endFrame();
  await flush();
  const afterFirstRetry = calls.length;
  assert.ok(afterFirstRetry <= 2,
    "at most one un-gated retry from A's co-occurring success (the noted nuance) — never an unbounded loop");
  // THE ASSERTION THAT MATTERS: with nothing left to reset the ladder, it now holds and escalates
  // exactly like the all-miss case (`"a batch failure backs off"`, above) — not another tight loop.
  cache.beginFrame(); cache.acquire(addr(2)); cache.endFrame();
  await flush();
  assert.equal(calls.length, afterFirstRetry,
    "the silence gate holds on the very next frame once nothing is left to reset it");
  clock.t = 600;
  cache.beginFrame(); cache.acquire(addr(2)); cache.endFrame();
  await flush();
  assert.equal(calls.length, afterFirstRetry + 1, "and it reopens once the backoff elapses — resilience, not a refusal");
});
