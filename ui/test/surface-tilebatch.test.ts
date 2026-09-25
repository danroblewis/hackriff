// T-573: one request per viewport, not one per tile — **asserted on the REQUEST the client
// builds**, not only on the response it renders. No gate covers a client asking the backend for
// the wrong thing (T-367 asked `/api/timeline` with no band and drew an empty canvas while every
// suite stayed green), so the counts and the URL are the assertions here.

import test from "node:test";
import assert from "node:assert/strict";

import { TILES_BATCH_MAX_ADDRESSES, addrSpelling, batchGroupKey, tilesBatchUrl, type TileAddr } from "../src/surface/lattice";
import { batchedTileSource } from "../src/surface/tilebatch";
import { TileBusyError } from "../src/surface/tile";
import { setTileClientId } from "../src/surface/clientid";

const addr = (fIndex: number, over: Partial<TileAddr> = {}): TileAddr => ({
  device: "any", scheme: "view", levelF: 0, levelT: 0, fIndex, tIndex: 7, cells: 256, ...over,
});

/** A tile body complete enough for `decodeTile`, with one observed cell per `nf * nt`. */
function tileBody(n = 2): unknown {
  const cells = n * n;
  return {
    key: { level_f: 0, level_t: 0, f_index: 0, t_index: 7, cells: n },
    extent: { nf: n, nt: n, f_lo_hz: 0, f_hi_hz: 100, f_cell_hz: 50, t0_s: 0, t1_s: 1, t_cell_s: 0.5 },
    axes: { frequency: { cell_hz: 50, levels: 1, max_level: 0, tile_hz: 100 },
            time: { cell_s: 0.5, levels: 1, max_level: 0, tile_s: 1 } },
    grid: {
      nf: n, nt: n, cells, observed_cells: cells,
      encoding: { planes: "json" },
      max_db: Array.from({ length: cells }, () => -80),
    },
    coverage: {
      states: ["observed"],
      grid: { nf: n, nt: n },
      planes: [{ cells, runs: [0, cells] }],
      any: { plane: 0 },
      selected: { plane: 0, present: true },
    },
    resolution: { source: "live-iq", live: true, answered: 0, statement: "…",
                  fold: { frequency: { direction: "exact" }, time: { direction: "exact" } } },
    cost: { build_ms: 1, source_cells: 4, chunks: 1 },
  };
}

/** A fetch spy that answers a batch with one entry per requested address. */
function spy(over: (spellings: string[]) => unknown = () => null) {
  const urls: string[] = [];
  const fetchFn = async (url: string, _init: RequestInit) => {
    urls.push(url);
    const spellings = new URLSearchParams(url.split("?")[1]).get("addresses")!.split(",");
    const custom = over(spellings);
    const body = custom ?? {
      requested: spellings.length, returned: spellings.length, truncated: false, remaining: [],
      tiles: spellings.map((s) => ({ address: { spelling: s }, status: 200, tile: tileBody() })),
    };
    return { ok: true, status: 200, statusText: "OK", json: async () => body };
  };
  return { urls, fetchFn };
}

test("the batch request the client builds is the route's own spelling", () => {
  assert.equal(
    tilesBatchUrl([addr(3), addr(4, { levelF: 1, levelT: 2, tIndex: 9 })]),
    "/api/tiles/batch?addresses=0.0.3.7%2C1.2.4.9&planes=compact",
  );
  // The viewport's own parameters ride once, beside the addresses — never per address.
  assert.equal(
    tilesBatchUrl([addr(0, { device: "hackrf:abc", scheme: "overview", cells: 64 })]),
    "/api/tiles/batch?addresses=0.0.0.7&scheme=overview&device=hackrf%3Aabc&cells=64&planes=compact",
  );
  assert.equal(addrSpelling(addr(12, { levelF: 3, levelT: 5, tIndex: 218427 })), "3.5.12.218427");
  // What may share one request is exactly what the route takes once.
  assert.equal(batchGroupKey(addr(1)), batchGroupKey(addr(2)));
  assert.notEqual(batchGroupKey(addr(1)), batchGroupKey(addr(1, { cells: 64 })));
  assert.notEqual(batchGroupKey(addr(1)), batchGroupKey(addr(1, { scheme: "overview" })));
});

test("T-630: a named page's batch carries `client` once, as its single-tile requests do", () => {
  // One batch is one asker: every address in it takes its slot under that asker's share, so a
  // batch that dropped the name would put a named tab back in the anonymous bucket.
  setTileClientId("tab-two");
  try {
    assert.equal(
      tilesBatchUrl([addr(3), addr(4)]),
      "/api/tiles/batch?addresses=0.0.3.7%2C0.0.4.7&client=tab-two&planes=compact",
    );
  } finally {
    setTileClientId("");
  }
  assert.equal(tilesBatchUrl([addr(3)]), "/api/tiles/batch?addresses=0.0.3.7&planes=compact");
});

test("COUNT: a viewport's eight addresses cost ONE request, not eight", async () => {
  const h = spy();
  const source = batchedTileSource("tok", h.fetchFn);
  // Issued the way `TileCache.pump()` issues them: synchronously, in one turn.
  const all = await Promise.all(Array.from({ length: 8 }, (_, i) => source(addr(i))));
  assert.equal(h.urls.length, 1, `eight addresses cost ${h.urls.length} requests`);
  assert.equal(all.length, 8);
  const asked = new URLSearchParams(h.urls[0].split("?")[1]).get("addresses")!.split(",");
  assert.deepEqual(asked, Array.from({ length: 8 }, (_, i) => `0.0.${i}.7`));
  // Every address got its OWN tile back, keyed to its own address.
  assert.deepEqual(all.map((t) => t.addr.fIndex), [0, 1, 2, 3, 4, 5, 6, 7]);
});

test("COUNT: mixed viewports are one request per group, still a small constant", async () => {
  const h = spy();
  const source = batchedTileSource("tok", h.fetchFn);
  await Promise.all([
    source(addr(0)), source(addr(1)),
    source(addr(2, { scheme: "overview" })), source(addr(3, { scheme: "overview" })),
  ]);
  assert.equal(h.urls.length, 2, "one pane and one minimap: two requests, not four");
  assert.equal(h.urls.filter((u) => u.includes("scheme=overview")).length, 1);
});

test("COUNT: the client splits at the route's cap rather than being refused", async () => {
  const h = spy();
  const source = batchedTileSource("tok", h.fetchFn);
  const n = TILES_BATCH_MAX_ADDRESSES + 5;
  await Promise.all(Array.from({ length: n }, (_, i) => source(addr(i))));
  assert.equal(h.urls.length, 2, "two requests, not a 400 and a retry");
  const counts = h.urls.map((u) => new URLSearchParams(u.split("?")[1]).get("addresses")!.split(",").length);
  assert.deepEqual(counts, [TILES_BATCH_MAX_ADDRESSES, 5]);
});

test("a per-address refusal fails only its own address", async () => {
  const h = spy((s) => ({
    tiles: [
      { address: { spelling: s[0] }, status: 200, tile: tileBody() },
      { address: { spelling: s[1] }, status: 503, error: "too many tile reads in flight (limit 4)" },
    ],
    truncated: false, remaining: [],
  }));
  const source = batchedTileSource("tok", h.fetchFn);
  const [ok, bad] = await Promise.allSettled([source(addr(0)), source(addr(1))]);
  assert.equal(ok.status, "fulfilled");
  assert.equal(bad.status, "rejected");
  assert.ok((bad as PromiseRejectedResult).reason instanceof TileBusyError,
    "a 503 in a batch is the same backoff signal it is on its own");
  assert.equal(h.urls.length, 1);
});

test("T-630: a per-address refusal carries the SHARE it names, as a single-tile refusal does", async () => {
  // Since T-573 nearly every tile read is a batch, so a batch member's `503` is the main way a tab
  // learns another client arrived. Dropping its `share` here (only `limit` was parsed) left a tab
  // told "share 2" still claiming 4 on its status line — `ui/e2e/surface-contention.e2e.mjs`'s red
  // under load, whenever that tab's reads after the newcomer registered were all refused.
  const message = "too many tile reads in flight (limit 4, share 2) — 2 client(s) are reading tiles, " +
    "so your share is 2 of them: tile production takes the history lock";
  const h = spy((s) => ({
    tiles: [{ address: { spelling: s[0] }, status: 503, error: message }],
    truncated: false, remaining: [],
  }));
  const source = batchedTileSource("tok", h.fetchFn);
  await assert.rejects(() => source(addr(0)), (e: unknown) => {
    assert.ok(e instanceof TileBusyError);
    assert.equal(e.limit, 4);
    assert.equal(e.share, 2, "the batch path must hand the cache the share the route stated");
    return true;
  });
  // And a pre-T-630 route that names no share says nothing, exactly as on the single-tile path.
  const old = spy((s) => ({
    tiles: [{ address: { spelling: s[0] }, status: 503, error: "too many tile reads in flight (limit 4)" }],
    truncated: false, remaining: [],
  }));
  await assert.rejects(() => batchedTileSource("tok", old.fetchFn)(addr(0)),
    (e: unknown) => e instanceof TileBusyError && e.share === null && e.limit === 4);
});

test("a truncated batch re-asks for exactly what `remaining` named", async () => {
  let call = 0;
  const h = spy((s) => {
    if (call++ > 0) return null; // the follow-up answers everything
    return { tiles: [{ address: { spelling: s[0] }, status: 200, tile: tileBody() }],
             truncated: true, remaining: s.slice(1) };
  });
  const source = batchedTileSource("tok", h.fetchFn);
  const all = await Promise.all([source(addr(0)), source(addr(1)), source(addr(2))]);
  assert.equal(all.length, 3, "the unreached addresses are re-asked, never failed");
  assert.equal(h.urls.length, 2);
  assert.equal(new URLSearchParams(h.urls[1].split("?")[1]).get("addresses"), "0.0.1.7,0.0.2.7");
});

test("an address neither answered nor listed stays PENDING, never grey", async () => {
  const h = spy(() => ({ tiles: [], truncated: false, remaining: [] }));
  const source = batchedTileSource("tok", h.fetchFn);
  await assert.rejects(source(addr(0)), /neither answered nor listed/);
});

test("a batch is abandoned only when EVERY member has been", async () => {
  const aborts: AbortSignal[] = [];
  const urls: string[] = [];
  const fetchFn = async (url: string, init: RequestInit) => {
    urls.push(url);
    if (init.signal) aborts.push(init.signal);
    const s = new URLSearchParams(url.split("?")[1]).get("addresses")!.split(",");
    return { ok: true, status: 200, statusText: "OK",
             json: async () => ({ tiles: s.map((x) => ({ address: { spelling: x }, status: 200, tile: tileBody() })), truncated: false, remaining: [] }) };
  };
  const source = batchedTileSource("tok", fetchFn);
  const a = new AbortController(), b = new AbortController();
  const p = Promise.allSettled([source(addr(0), a.signal), source(addr(1), b.signal)]);
  // One turn, so the batch has reached the wire and its controller exists.
  await Promise.resolve();
  await Promise.resolve();
  assert.equal(urls.length, 1);
  a.abort();
  assert.equal(aborts[0]?.aborted, false, "one viewport leaving must not cancel the other's tile");
  b.abort();
  assert.equal(aborts[0]?.aborted, true, "with nobody waiting, the request is abandoned");
  await p;
});

test("the live edge rides ALONE: a `solo` ask is its own single-tile request, never batched", async () => {
  // A batch answers when its slowest member does, so a live-edge revalidation coalesced with cold
  // tiles would hold the newest rows until those were built — the live edge gated on generation.
  // `TileCache` marks its refresh lane `solo`; this asserts the request that hint produces.
  const urls: string[] = [];
  const fetchFn = async (url: string, _init: RequestInit) => {
    urls.push(url);
    const q = new URLSearchParams(url.split("?")[1]);
    const body = url.startsWith("/api/tiles/batch")
      ? { requested: 2, returned: 2, truncated: false, remaining: [],
          tiles: q.get("addresses")!.split(",").map((s) => ({ address: { spelling: s }, status: 200, tile: tileBody() })) }
      : tileBody();
    return { ok: true, status: 200, statusText: "OK", json: async () => body };
  };
  const source = batchedTileSource("tok", fetchFn);
  const got = await Promise.all([source(addr(0)), source(addr(1)), source(addr(2), undefined, { solo: true })]);
  assert.equal(got.length, 3);
  const batch = urls.filter((u) => u.startsWith("/api/tiles/batch"));
  const single = urls.filter((u) => u.startsWith("/api/tiles?"));
  assert.equal(batch.length, 1, `requests: ${urls.join(" ")}`);
  assert.deepEqual(new URLSearchParams(batch[0].split("?")[1]).get("addresses")!.split(","), ["0.0.0.7", "0.0.1.7"],
    "the solo address must not ride in the batch");
  assert.equal(single.length, 1, `requests: ${urls.join(" ")}`);
  const q = new URLSearchParams(single[0].split("?")[1]);
  assert.equal(q.get("f_index"), "2");
  assert.equal(q.get("planes"), "compact");
});
