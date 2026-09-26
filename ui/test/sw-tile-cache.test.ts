// T-1039: the pure logic behind `../src/sw/tiles-sw.ts` — the Service Worker cache of SEALED,
// immutable tiles that gives a reload an instant first paint offline. Node cannot construct a
// `ServiceWorkerGlobalScope`, so everything the worker's own `fetch` handler decides is factored
// into `../src/sw/tile-cache-logic.ts` and tested here directly; the worker file is thin wiring
// around it (see its own header comment).

import test from "node:test";
import assert from "node:assert/strict";

import {
  buildOfflineBatch, parseBatchUrl, sealedCacheUrl, sealedEntries,
  type BatchEntryLike,
} from "../src/sw/tile-cache-logic";
import { tilesBatchUrl, type TileAddr } from "../src/surface/lattice";

const addr = (fIndex: number, over: Partial<TileAddr> = {}): TileAddr => ({
  device: "any", scheme: "view", levelF: 0, levelT: 0, fIndex, tIndex: 7, cells: 256, ...over,
});

const sealed = (spelling: string, extra: Record<string, unknown> = {}): BatchEntryLike =>
  ({ address: { spelling }, status: 200, tile: { sealed: true, max_db: -80, ...extra } });
const live = (spelling: string): BatchEntryLike =>
  ({ address: { spelling }, status: 200, tile: { sealed: false, max_db: -60 } });
const refused = (spelling: string): BatchEntryLike => ({ address: { spelling }, status: 400, error: "bad" });

test("parseBatchUrl reads the client's own request shape, and is null for anything else", () => {
  const url = tilesBatchUrl([addr(3), addr(4, { levelF: 1, levelT: 2, tIndex: 9 })]);
  const parsed = parseBatchUrl(`https://example.test${url}`);
  assert.deepEqual(parsed, {
    device: "any", scheme: "view", cells: 256, planes: "compact",
    addresses: ["0.0.3.7", "1.2.4.9"],
  });
  const withParams = parseBatchUrl(
    `https://example.test${tilesBatchUrl([addr(0, { device: "hackrf:abc", scheme: "overview", cells: 64 })])}`,
  );
  assert.equal(withParams?.device, "hackrf:abc");
  assert.equal(withParams?.scheme, "overview");
  assert.equal(withParams?.cells, 64);
  // A single-tile `/api/tiles` request is left alone — the browser's own HTTP cache already
  // handles a sealed one (T-574's ETag/immutable contract), and this worker must not shadow it.
  assert.equal(parseBatchUrl("https://example.test/api/tiles?level_f=0&level_t=0&f_index=0&t_index=0"), null);
  // No addresses at all: nothing to reconstruct, nothing to cache.
  assert.equal(parseBatchUrl("https://example.test/api/tiles/batch?addresses="), null);
  assert.equal(parseBatchUrl("not a url"), null);
});

test("sealedCacheUrl is a synthetic key that can never collide with a real route, and differs per group and address", () => {
  const p = { device: "any", scheme: "view", cells: 256, planes: "compact" };
  const a = sealedCacheUrl(p, "0.0.3.7");
  const b = sealedCacheUrl(p, "0.0.4.7");
  assert.notEqual(a, b, "different addresses key differently");
  assert.ok(!a.includes("/api/"), "never a real route — a request for it must never reach the wire");
  const c = sealedCacheUrl({ ...p, cells: 64 }, "0.0.3.7");
  assert.notEqual(a, c, "a different batch group (cells) must not collide with another's cache");
});

test("sealedEntries keeps ONLY status-200 answers whose own tile states sealed:true — never a live tile, a refusal, or an unaddressed one", () => {
  const entries: BatchEntryLike[] = [
    sealed("0.0.1.7"), live("0.0.2.7"), refused("0.0.3.7"),
    { address: {}, status: 200, tile: { sealed: true } }, // no spelling: cannot be looked up again
    { address: { spelling: "0.0.5.7" }, status: 200 }, // no tile body at all
  ];
  const kept = sealedEntries(entries);
  assert.deepEqual(kept.map((e) => e.address!.spelling), ["0.0.1.7"],
    "the live tile, the refusal, the spelling-less entry and the body-less entry are all excluded");
});

test("buildOfflineBatch answers EVERY address, one way or the other — never `remaining` (T-1039 review fix 2/2)", () => {
  // The first cut put an uncached address in `remaining`, which `tilebatch.ts` requeues on the next
  // microtask with NO delay — a tight retry loop against a server that is still down. `remaining`
  // must therefore always come back empty, and every address answered as its own tiles entry.
  const cached = new Map<string, unknown>([
    ["0.0.1.7", { sealed: true, max_db: -80 }],
    ["0.0.3.7", { sealed: true, max_db: -70 }],
  ]);
  const out = buildOfflineBatch(["0.0.1.7", "0.0.2.7", "0.0.3.7"], cached);
  assert.deepEqual(out.remaining, [], "remaining is always empty — nothing here is a truncation");
  assert.equal(out.tiles.length, 3, "every requested address gets its own entry, cached or not");
  assert.deepEqual(out.tiles[0], { address: { spelling: "0.0.1.7" }, status: 200, tile: { sealed: true, max_db: -80 } });
  // The uncached address is a real per-entry FAILURE (502, "a proxy that could not reach the
  // origin"), which `tilebatch.ts` rejects the caller with — reaching `TileCache.failed()`'s own
  // silent-failure backoff, rather than skipping it entirely via `remaining`.
  assert.equal(out.tiles[1].address?.spelling, "0.0.2.7");
  assert.equal(out.tiles[1].status, 502);
  assert.equal(out.tiles[1].tile, undefined, "a miss carries no tile body to be mistaken for one");
  assert.deepEqual(out.tiles[2], { address: { spelling: "0.0.3.7" }, status: 200, tile: { sealed: true, max_db: -70 } });
});

test("buildOfflineBatch with nothing cached at all still answers every address, each a miss — never `remaining`", () => {
  const out = buildOfflineBatch(["0.0.1.7", "0.0.2.7"], new Map());
  assert.deepEqual(out.remaining, []);
  assert.equal(out.tiles.length, 2);
  for (const t of out.tiles) { assert.equal(t.status, 502); assert.equal(t.tile, undefined); }
  assert.deepEqual(out.tiles.map((t) => t.address?.spelling), ["0.0.1.7", "0.0.2.7"]);
});

test("end to end: a batch answer mixing sealed and live tiles caches only the sealed one, and offline reconstruction never resurrects the live one", () => {
  const p = { device: "any", scheme: "view", cells: 256, planes: "compact" };
  const answer: BatchEntryLike[] = [sealed("0.0.1.7"), live("0.0.2.7")];
  const toStore = sealedEntries(answer);
  const cache = new Map<string, unknown>();
  for (const e of toStore) cache.set(sealedCacheUrl(p, e.address!.spelling!), e.tile);
  // Simulate the worker's own lookup: it keys by `sealedCacheUrl`, exactly as `storeSealed` wrote.
  const found = new Map<string, unknown>();
  for (const spelling of ["0.0.1.7", "0.0.2.7"]) {
    const hit = cache.get(sealedCacheUrl(p, spelling));
    if (hit !== undefined) found.set(spelling, hit);
  }
  const offline = buildOfflineBatch(["0.0.1.7", "0.0.2.7"], found);
  assert.deepEqual(offline.tiles.map((t) => t.address!.spelling), ["0.0.1.7", "0.0.2.7"]);
  assert.equal(offline.tiles[0].status, 200, "only the sealed tile is served as a hit");
  assert.equal(offline.tiles[1].status, 502,
    "the live tile is a per-entry MISS, never served stale from a cache it was never allowed into, and never `remaining` either");
  assert.deepEqual(offline.remaining, []);
});
