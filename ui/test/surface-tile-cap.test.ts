// **Every tile read takes a slot, and the cap the tab reports is never smaller than what it holds**
// (T-1079).
//
// The release candidate of 2026-09-26 went red in `ui/e2e/surface-contention.e2e.mjs` on the first
// tab's own readout: `2+0/1 in flight (share 1` — two reads out against an operating cap of one.
// Two separate things can put that line on the screen, and only one of them is a client asking for
// more than it may:
//
//  1. **A lane that issues without taking a slot.** `pumpChanged` (T-1040's `coverage_changed`
//     re-ask) issued its whole pending set with no budget test at all, on the reading that a batch
//     costs one route slot. It does not: `tiles_batch_json` takes a slot *per member*, under this
//     client's own share, so a move over five resident tiles put five reads on a route that had
//     told this client it may have one. That is a real contention defect and the first test here.
//  2. **A cap that fell under reads already out.** The route states a smaller share because another
//     tab arrived, or a refusal halves the cap — and the reads admitted a moment earlier are
//     already on the route. They cannot be recalled (an abort reaches the browser, not `hk-api`,
//     which goes on producing the tile and holding the slot), so the only way to release one is to
//     let it finish. Reporting a cap *below* what is provably out states an impossibility and hides
//     case 1 inside it; the operating cap therefore drains to the new one by attrition, while the
//     issuing budget drops at once. The second and third tests are that pair.
//
// Asserted on the addresses actually on the wire and on the numbers the readout is built from
// (`inFlightCount`, `abandonedSlots`, `inFlightLimit` — `surface/preview-main.ts` prints exactly
// these three), never on "it did not throw".

import test from "node:test";
import assert from "node:assert/strict";

import { addrSpelling, type Lattice, type TileAddr } from "../src/surface/lattice";
import { batchedTileSource } from "../src/surface/tilebatch";
import { TileCache } from "../src/surface/tilecache";

const LAT: Lattice = { scheme: "view", cells: 256, f0Hz: 6250, t0Ns: 1e9, levelsF: 20, levelsT: 15 };
const F_TILE = 6250 * 256;
const T_TILE = 256e9;

const addr = (fIndex: number, tIndex = 7): TileAddr =>
  ({ device: "any", scheme: "view", levelF: 0, levelT: 0, fIndex, tIndex, cells: 256 });

/** A tile body complete enough for `decodeTile`, optionally stating this client's share. */
function tileBody(share?: number): unknown {
  const n = 2, cells = n * n;
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
    cost: { build_ms: 1, source_cells: 4, chunks: 1, in_flight_limit: 4, ...(share === undefined ? {} : { in_flight_share: share }) },
  };
}

const flush = async () => { for (let i = 0; i < 4; i++) await new Promise((r) => setImmediate(r)); };

/** The addresses one request named, and the hook that answers it — so a test decides when a slot
 * is released, which is the only way to see how many were held at once. */
interface Held {
  readonly spellings: string[];
  answer(share?: number): void;
}

/** A fetch spy that holds every request open until the test answers it. */
function heldSpy() {
  const held: Held[] = [];
  /** Every address this spy has been asked for, in order — what the client actually put on the wire. */
  const wire: string[] = [];
  let live = 0, peak = 0;
  const fetchFn = async (url: string, _init: RequestInit) => {
    const q = new URLSearchParams(url.split("?")[1]);
    const spellings = url.startsWith("/api/tiles/batch")
      ? q.get("addresses")!.split(",")
      : [`${q.get("level_f")}.${q.get("level_t")}.${q.get("f_index")}.${q.get("t_index")}`];
    wire.push(...spellings);
    live += spellings.length;
    peak = Math.max(peak, live);
    return await new Promise<{ ok: boolean; status: number; statusText: string; json: () => Promise<unknown> }>((resolve) => {
      held.push({
        spellings,
        answer(share?: number) {
          live -= spellings.length;
          const body = url.startsWith("/api/tiles/batch")
            ? { requested: spellings.length, returned: spellings.length, truncated: false, remaining: [],
                tiles: spellings.map((s) => ({ address: { spelling: s }, status: 200, tile: tileBody(share) })) }
            : tileBody(share);
          resolve({ ok: true, status: 200, statusText: "OK", json: async () => body });
        },
      });
    });
  };
  return {
    fetchFn, held, wire,
    get peak() { return peak; },
    /** Answer exactly what is outstanding now — not what the answers then cause, which is the next
     * round and is where the budget is tested again. */
    async answerRound(share?: number) {
      for (const h of held.splice(0, held.length)) h.answer(share);
      await flush();
    },
    /** Answer until nothing is outstanding. */
    async answerAll(share?: number) {
      for (let i = 0; i < 16 && held.length; i++) {
        for (const h of held.splice(0, held.length)) h.answer(share);
        await flush();
      }
    },
  };
}

function makeCache(inFlight: number, s: ReturnType<typeof heldSpy>) {
  let uploads = 0;
  const cache = new TileCache<{ id: number }>(
    { upload: () => ({ id: uploads++ }), destroy: () => {} },
    batchedTileSource("tok", s.fetchFn),
    { inFlight, now: () => 0 },
  );
  return cache;
}

/** What the status line is built from (`surface/preview-main.ts`): reads out, and the cap. */
const out = (c: TileCache<{ id: number }>) => c.inFlightCount + c.abandonedSlots;
const statusLine = (c: TileCache<{ id: number }>) =>
  `${c.inFlightCount}+${c.abandonedSlots}/${c.inFlightLimit} in flight (share ${c.inFlightCeiling})`;

/** Fill `tiles` into the cache, answering as fast as the cap lets them out. */
async function fill(cache: TileCache<{ id: number }>, s: ReturnType<typeof heldSpy>, tiles: readonly TileAddr[]) {
  for (let i = 0; i < 12 && cache.residentTiles < tiles.length; i++) {
    cache.beginFrame();
    for (const a of tiles) cache.acquire(a);
    cache.endFrame();
    await flush();
    await s.answerAll();
  }
  assert.equal(cache.residentTiles, tiles.length, "every tile resident before the move");
}

test("a coverage_changed over more tiles than the cap re-asks on the budget, never past it", async () => {
  const CAP = 2;
  const MOVED = [addr(60), addr(61), addr(62), addr(63), addr(64)];
  const s = heldSpy();
  const cache = makeCache(CAP, s);
  await fill(cache, s, MOVED);
  const peakBefore = s.peak, from = s.wire.length;

  // The retune: every resident tile moved, so every one of them is re-asked (T-1040).
  assert.equal(cache.coverageChanged([LAT], { fLoHz: 60 * F_TILE + 1, fHiHz: 65 * F_TILE - 1, tNs: 7.5 * T_TILE }),
    MOVED.length, "the change names every resident tile");
  await flush();

  // **The defect this test exists for**: the re-ask used to go out whole, in one unbudgeted batch.
  assert.ok(out(cache) <= CAP,
    `the re-ask put ${out(cache)} reads on the route against an operating cap of ${CAP}: "${statusLine(cache)}"`);
  assert.ok(s.peak <= Math.max(peakBefore, CAP),
    `${s.peak} addresses were on the wire at once against a cap of ${CAP}`);

  // …and budget-limited is not dropped: as slots free, the rest follow, and every moved tile is
  // re-asked exactly once.
  for (let i = 0; i < 12 && cache.stats.coverageRefetches < MOVED.length; i++) {
    await s.answerRound();
    assert.ok(out(cache) <= CAP, `over the cap mid-drain: "${statusLine(cache)}"`);
  }
  await s.answerAll();
  assert.deepEqual([...s.wire.slice(from)].sort(), MOVED.map(addrSpelling).sort(),
    "every moved tile re-asked, exactly once, across the turns the budget took");
  assert.equal(cache.stats.coverageRefetches, MOVED.length);
  assert.equal(cache.residentTiles, MOVED.length, "nothing dropped");
});

test("a share that shrinks under reads already out never makes the tab report more than its cap", async () => {
  const s = heldSpy();
  const cache = makeCache(4, s);
  // Three cold misses, all admitted while this tab was alone and its share was the whole cap.
  const COLD = [addr(10), addr(11), addr(12)];
  cache.beginFrame();
  for (const a of COLD) cache.acquire(a);
  cache.endFrame();
  await flush();
  assert.equal(cache.inFlightCount, 3, "all three admitted under the cap of 4");

  // Another tab arrives: the route states this client's share is 1 in the answer to the first read.
  // Two reads are still out, and nothing this client can do returns those slots — `hk-api` is still
  // producing them. What it must not do is claim a cap smaller than what it provably holds.
  s.held.shift()!.answer(1);
  await flush();
  assert.equal(cache.inFlightCeiling, 1, "the stated share is adopted as the ceiling");
  assert.equal(cache.inFlightCount, 2, "two reads are still out");
  assert.ok(out(cache) <= cache.inFlightLimit,
    `the tab reports more reads out than its own operating cap: "${statusLine(cache)}"`);

  // And the ISSUING budget dropped at once: nothing new goes out while it is over the new cap, and
  // the operating cap follows the reads down rather than sitting at the old one.
  cache.beginFrame();
  for (const a of [addr(20), addr(21)]) cache.acquire(a);
  cache.endFrame();
  await flush();
  assert.equal(cache.inFlightCount, 2, "no new read while the old ones still hold the new cap");
  s.held.shift()!.answer(1);
  await flush();
  assert.equal(cache.inFlightCount, 1, "still nothing new: one read is the whole share");
  assert.equal(cache.inFlightLimit, 1, "the operating cap has drained to the stated share");
  s.held.shift()!.answer(1);
  await flush();
  assert.equal(cache.inFlightCount, 1, "and now the queue gets the one slot the share allows");
  assert.ok(out(cache) <= cache.inFlightLimit, `over the cap after draining: "${statusLine(cache)}"`);
});

test("the drained cap is not a credit: a lane that issues without a slot still reads as over the cap", async () => {
  // The guard on the guard. Charging what is out when the cap falls must not become a general
  // licence to be over it — or defect 1 above would have been invisible behind defect 2's fix. So:
  // shrink the cap under reads already out (the legitimate case), let it drain, and then have a
  // `coverage_changed` land on a cache whose budget is full. Every re-ask it makes is over the cap,
  // and the readout has to say so — which, with the budget test in `pumpChanged`, means it makes
  // none until a slot frees.
  const s = heldSpy();
  const cache = makeCache(4, s);
  const TILES = [addr(30), addr(31), addr(32), addr(33)];
  await fill(cache, s, TILES);
  // Two reads out, then the route states share 1: the legitimate overshoot, charged and draining.
  cache.beginFrame();
  for (const a of [addr(40), addr(41)]) cache.acquire(a);
  cache.endFrame();
  await flush();
  assert.equal(cache.inFlightCount, 2);
  s.held.shift()!.answer(1);
  await flush();
  assert.equal(cache.inFlightLimit, 1, "one read out, one slot of share: the charge has drained");

  // Now the retune, with that one read still out and the whole budget spent.
  assert.equal(cache.coverageChanged([LAT], { fLoHz: 30 * F_TILE + 1, fHiHz: 34 * F_TILE - 1, tNs: 7.5 * T_TILE }),
    TILES.length);
  await flush();
  assert.ok(out(cache) <= cache.inFlightLimit,
    `a coverage-change re-ask went out over a full budget and the readout absorbed it: "${statusLine(cache)}"`);
});
