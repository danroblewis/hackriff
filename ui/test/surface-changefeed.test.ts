// T-1040: `coverage_changed` on `/ws/tiles/changes` — a retune re-lays the fog and re-fetches
// EXACTLY the moved tiles, in ONE batch. Asserted on the REQUESTS the client builds (the T-367
// rule): the socket path it opens, and the one `/api/tiles/batch` URL the event causes, with the
// addresses it names — no more, no fewer.

import test from "node:test";
import assert from "node:assert/strict";

import { addrSpelling, keyOf, type Lattice, type TileAddr } from "../src/surface/lattice";
import { batchedTileSource } from "../src/surface/tilebatch";
import { TileCache } from "../src/surface/tilecache";
import {
  CHANGE_FEED_PATH, CoverageChangeFeed, parseChangeMessage, type ChangeOpener, type CoverageChange,
} from "../src/surface/changefeed";

// Level-0 tiles are 6250 Hz × 256 = 1.6 MHz wide and 1 s × 256 = 256 s tall.
const LAT: Lattice = { scheme: "view", cells: 256, f0Hz: 6250, t0Ns: 1e9, levelsF: 20, levelsT: 15 };
const F_TILE = 6250 * 256;
const T_TILE = 256e9;

const addr = (fIndex: number, tIndex: number, levelF = 0, levelT = 0): TileAddr =>
  ({ device: "any", scheme: "view", levelF, levelT, fIndex, tIndex, cells: 256 });

/** A tile body complete enough for `decodeTile`. */
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

/** A fetch spy answering every batch whole, recording each URL and the addresses it named. */
function spy() {
  const urls: string[] = [];
  const batches: string[][] = [];
  const fetchFn = async (url: string, _init: RequestInit) => {
    urls.push(url);
    // The initial fill (no viewports yet) asks one tile at a time; answer it as the route does.
    if (!url.startsWith("/api/tiles/batch")) {
      return { ok: true, status: 200, statusText: "OK", json: async () => tileBody() };
    }
    const spellings = new URLSearchParams(url.split("?")[1]).get("addresses")!.split(",");
    batches.push(spellings);
    const body = {
      requested: spellings.length, returned: spellings.length, truncated: false, remaining: [],
      tiles: spellings.map((s) => ({ address: { spelling: s }, status: 200, tile: tileBody() })),
    };
    return { ok: true, status: 200, statusText: "OK", json: async () => body };
  };
  return { urls, batches, fetchFn };
}

function residentCache(tiles: readonly TileAddr[]) {
  const s = spy();
  let uploads = 0;
  const cache = new TileCache<{ id: number }>(
    { upload: () => ({ id: uploads++ }), destroy: () => {} },
    batchedTileSource("tok", s.fetchFn),
    { inFlight: 16, now: () => 0 },
  );
  return {
    cache, s, uploads: () => uploads,
    async fill() {
      cache.beginFrame();
      for (const a of tiles) cache.acquire(a);
      cache.endFrame();
      await flush(); await flush();
      assert.equal(cache.residentTiles, tiles.length, `every tile resident before the move: ${JSON.stringify(cache.stats)} ${s.urls.join(" ")}`);
    },
  };
}

// The move: the radio left 99.2–100.8 MHz for 100.8–102.4 MHz at t = 7.5 tiles in.
const T_MOVE = 7.5 * T_TILE;
const MOVE = { fLoHz: 62 * F_TILE + 1, fHiHz: 64 * F_TILE - 1, tNs: T_MOVE };
const MOVED = [addr(62, 7), addr(63, 7), addr(31, 7, 1, 0)];
const UNMOVED = [
  addr(60, 7), // another band
  addr(66, 7), // another band
  addr(62, 6), // this band, but wholly before the move
];

test("the change socket is ONE path with no range: the event is about the tune record", () => {
  const opened: string[] = [];
  const opener: ChangeOpener = (path) => { opened.push(path); return { close() {} }; };
  const feed = new CoverageChangeFeed(opener, () => {});
  feed.keep(); feed.keep(); feed.keep();
  assert.deepEqual(opened, ["/ws/tiles/changes"], "opened once and kept");
  assert.deepEqual(feed.requests, [CHANGE_FEED_PATH]);
});

test("one coverage_changed causes EXACTLY one batch, for exactly the tiles the move rewrote", async () => {
  const h = residentCache([...MOVED, ...UNMOVED]);
  await h.fill();
  const before = h.s.urls.length;
  const up = h.uploads();

  // The server's message, through the feed, into the cache — the whole client path.
  let deliver: (text: string) => void = () => {};
  const feed = new CoverageChangeFeed((_p, onText) => { deliver = onText; return { close() {} }; },
    (c) => { h.cache.coverageChanged([LAT], c); });
  feed.keep();
  deliver(JSON.stringify({ type: "subscribed", tuned: [], tick_ms: 100, rule: "…" }));
  deliver(JSON.stringify({
    type: "coverage_changed", seq: 1, device: "mock:hackrf:0", t: MOVE.tNs / 1e9,
    f_lo: MOVE.fLoHz, f_hi: MOVE.fHiHz,
    departed: { f_lo: MOVE.fLoHz, f_hi: 63 * F_TILE }, arrived: { f_lo: 63 * F_TILE, f_hi: MOVE.fHiHz },
    as_of_s: MOVE.tNs / 1e9 + 0.2, source: "iq-ring",
  }));
  assert.equal(feed.received, 1);

  // While the re-ask is in flight the copies in hand stay drawn: nothing goes blank.
  h.cache.beginFrame();
  for (const a of MOVED) assert.equal(h.cache.acquire(a).kind, "resident", `${keyOf(a)} blanked`);
  h.cache.endFrame();
  await flush(); await flush();

  assert.equal(h.s.urls.length - before, 1, `one batch, not one per tile: ${h.s.urls.slice(before).join("\n")}`);
  assert.deepEqual(
    [...h.s.batches.at(-1)!].sort(),
    MOVED.map(addrSpelling).sort(),
    "exactly the moved tiles — none of the unmoved ones",
  );
  assert.equal(h.uploads() - up, MOVED.length, "each answer replaced its copy in place");
  assert.equal(h.cache.residentTiles, MOVED.length + UNMOVED.length, "nothing dropped");
  assert.equal(h.cache.stats.coverageChanges, 1);
  assert.equal(h.cache.stats.coverageRefetches, MOVED.length);

  // Stated once: nothing further is asked with no further event.
  h.cache.beginFrame();
  for (const a of [...MOVED, ...UNMOVED]) h.cache.acquire(a);
  h.cache.endFrame();
  await flush();
  assert.equal(h.s.urls.length - before, 1, "no follow-up requests");
});

test("a change that meets no resident tile asks for nothing", async () => {
  const h = residentCache(UNMOVED);
  await h.fill();
  const before = h.s.urls.length;
  assert.equal(h.cache.coverageChanged([LAT], { fLoHz: 200e6, fHiHz: 202e6, tNs: T_MOVE }), 0);
  await flush(); await flush();
  assert.equal(h.s.urls.length, before);
});

test("a malformed or refused message closes the socket and it is reopened after the retry", () => {
  assert.throws(() => parseChangeMessage(JSON.stringify({ type: "coverage_changed", seq: 1, t: 1, f_lo: 2, f_hi: 1, arrived: { f_lo: 1, f_hi: 2 }, departed: null })));
  assert.throws(() => parseChangeMessage(JSON.stringify({ type: "something_else" })));
  const ok = parseChangeMessage(JSON.stringify({
    type: "coverage_changed", seq: 3, device: "d", t: 2.5, f_lo: 1, f_hi: 4, departed: null, arrived: { f_lo: 1, f_hi: 4 },
  }));
  assert.equal(ok.kind, "change");
  const c = (ok as { change: CoverageChange }).change;
  assert.deepEqual([c.fLoHz, c.fHiHz, c.tNs, c.departed], [1, 4, 2.5e9, null]);

  let now = 0;
  let closes = 0;
  const opened: string[] = [];
  const feed = new CoverageChangeFeed((p, onText) => {
    opened.push(p);
    onText(JSON.stringify({ type: "refused", status: 503, reason: "cap" }));
    return { close() { closes++; } };
  }, () => assert.fail("a refusal is not an event"), { now: () => now, retryMs: 1000 });
  feed.keep();
  assert.equal(feed.isOpen, false);
  assert.equal(closes, 1);
  feed.keep();
  assert.equal(opened.length, 1, "not before the retry");
  now = 1000;
  feed.keep();
  assert.equal(opened.length, 2, "again after it");
});
