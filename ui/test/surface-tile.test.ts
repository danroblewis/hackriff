// T-440: decoding one `/api/tiles` answer into the two planes, and the route's `503` as an answer
// rather than an error.
//
// The assertions that carry weight are the ones about which of four things a cell is. `null` in
// `grid.max_db` is "sampled, level not retained"; `state: "unobserved"` is "nothing ever looked";
// `state: "unknown"` is "the record that would say is gone". Collapsing any pair of those is the
// defect docs/16 §4 exists to forbid, and T-413 met it from the other side.

import { test } from "node:test";
import assert from "node:assert/strict";
import { CELL } from "../src/surface/cellrule";
import {
  BYTES_PER_CELL, capFromRefusal, decodeTile, fetchTile, TileBusyError, TileDecodeError,
  type TileResponse,
} from "../src/surface/tile";
import type { TileAddr } from "../src/surface/lattice";

const ADDR: TileAddr = { device: "any", scheme: "view", levelF: 1, levelT: 2, fIndex: 3, tIndex: 4, cells: 2 };

function resp(over: Partial<TileResponse> = {}): TileResponse {
  return {
    key: { device: "any", scheme: "view", level_f: 1, level_t: 2, f_index: 3, t_index: 4, cells: 2 },
    extent: { nt: 2, nf: 2 },
    axes: { frequency: { levels: 20, cell_hz: 12_500 }, time: { levels: 15, cell_s: 4 } },
    grid: { nt: 2, nf: 2, max_db: [-90, null, -70, -60], range_db: { lo: -90, hi: -60 } },
    coverage: {
      grid: { nt: 2, nf: 2 },
      any: { cells: [{ state: "observed" }, { state: "observed" }, { state: "unobserved" }, { state: "unknown" }] },
      selected: { device: "any", named: false, present: true },
    },
    resolution: { source: "spectrum-history", answered: { level: 2 }, fold: { frequency: { direction: "folded" }, time: { direction: "replicated" } } },
    cost: { in_flight_limit: 4 },
    ...over,
  };
}

test("four cell states, and none of them collapses into another", () => {
  const t = decodeTile(ADDR, resp());
  assert.deepEqual([...t.state], [CELL.OBSERVED, CELL.NO_LEVEL, CELL.UNOBSERVED, CELL.UNKNOWN]);
  assert.equal(t.value[0], -90);
  assert.ok(Number.isNaN(t.value[1]), "a cell with no retained level must not carry a number that could be coloured");
  assert.ok(Number.isNaN(t.value[2]) && Number.isNaN(t.value[3]));
  assert.equal(t.tier, "spectrum-history");
  assert.equal(t.answeredLevel, 2, "the tier that ANSWERED, not the one the address implies");
  assert.deepEqual(t.fold, { frequency: "folded", time: "replicated" });
  assert.equal(t.bytes, 4 * BYTES_PER_CELL);
  assert.equal(t.serverInFlightLimit, 4);
});

test("coverage decides grey, and the measurement only splits OBSERVED from NO_LEVEL", () => {
  // A cell the coverage plane calls unobserved stays unobserved even with a level beside it, and a
  // cell it calls observed never becomes grey for want of a level.
  const t = decodeTile(ADDR, resp({
    grid: { nt: 2, nf: 2, max_db: [-90, -80, -70, -60], range_db: null },
    coverage: {
      grid: { nt: 2, nf: 2 },
      any: { cells: [{ state: "unobserved" }, { state: "observed" }, { state: "unobserved" }, { state: "observed" }] },
      selected: { device: "any", named: false, present: true },
    },
  }));
  assert.deepEqual([...t.state], [CELL.UNOBSERVED, CELL.OBSERVED, CELL.UNOBSERVED, CELL.OBSERVED]);
  assert.ok(Number.isNaN(t.value[0]), "an unobserved cell must not carry the number the grid happened to hold");
});

test("a response that cannot be read is NOT a coverage answer: it throws, so the place stays pending", () => {
  for (const bad of [
    resp({ coverage: undefined }),
    resp({ grid: { nt: 2, nf: 2, max_db: [-90, -80] } }),
    resp({ coverage: { grid: { nt: 2, nf: 2 }, any: { cells: [{ state: "observed" }] }, selected: { device: "any", named: false, present: true } } }),
    resp({ resolution: { source: "guessed" } }),
  ]) {
    assert.throws(() => decodeTile(ADDR, bad), TileDecodeError);
  }
});

test("a named device with no plane is unobserved FOR THAT DEVICE — a coverage answer, not a missing one", () => {
  const addr = { ...ADDR, device: "hackrf:abc" };
  const t = decodeTile(addr, resp({
    coverage: {
      grid: { nt: 2, nf: 2 },
      any: { cells: [{ state: "observed" }, { state: "observed" }, { state: "observed" }, { state: "observed" }] },
      devices: [{ device: "mock:1", cells: [{ state: "observed" }, { state: "observed" }, { state: "observed" }, { state: "observed" }] }],
      selected: { device: "hackrf:abc", named: true, present: false },
    },
  }));
  assert.deepEqual([...t.state], [CELL.UNOBSERVED, CELL.UNOBSERVED, CELL.UNOBSERVED, CELL.UNOBSERVED]);
  // And the union's plane is NOT what a named device gets: a merged plane must never wear one
  // radio's identity.
  assert.throws(() => decodeTile(addr, resp({
    coverage: { grid: { nt: 2, nf: 2 }, any: { cells: [{ state: "observed" }, { state: "observed" }, { state: "observed" }, { state: "observed" }] }, selected: { device: "hackrf:abc", named: true, present: true } },
  })), TileDecodeError);
});

test("a coverage plane at a different resolution resamples within the same extent", () => {
  const t = decodeTile(ADDR, resp({
    coverage: { grid: { nt: 1, nf: 1 }, any: { cells: [{ state: "unobserved" }] }, selected: { device: "any", named: false, present: true } },
  }));
  assert.deepEqual([...t.state], [CELL.UNOBSERVED, CELL.UNOBSERVED, CELL.UNOBSERVED, CELL.UNOBSERVED]);
});

test("the 503 names its cap, and the client reads it", () => {
  assert.equal(capFromRefusal("too many tile reads in flight (limit 4): tile production takes the history lock"), 4);
  assert.equal(capFromRefusal("busy"), null);
});

test("fetchTile turns the route's backpressure into an answer, and everything else into an error", async () => {
  const seen: string[] = [];
  const fetchFn = (url: string, init: RequestInit) => {
    seen.push(url);
    assert.equal((init.headers as Record<string, string>).Authorization, "Bearer tok");
    return Promise.resolve({
      ok: false, status: 503, statusText: "",
      json: () => Promise.resolve({ error: "too many tile reads in flight (limit 4)", code: "http_503" }),
    });
  };
  await assert.rejects(() => fetchTile(ADDR, "tok", fetchFn), (e: unknown) => {
    assert.ok(e instanceof TileBusyError);
    assert.equal(e.limit, 4);
    return true;
  });
  assert.deepEqual(seen, ["/api/tiles?level_f=1&level_t=2&f_index=3&t_index=4&cells=2"]);

  const ok = await fetchTile(ADDR, "tok", () => Promise.resolve({ ok: true, status: 200, statusText: "", json: () => Promise.resolve(resp()) }));
  assert.equal(ok.key, "any|view|1|2|3|4|2");

  await assert.rejects(
    () => fetchTile(ADDR, "tok", () => Promise.resolve({ ok: false, status: 404, statusText: "", json: () => Promise.resolve({ error: "no such node", code: "not_found" }) })),
    /no such node/,
  );
});
