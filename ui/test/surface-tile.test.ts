// T-440: decoding one `/api/tiles` answer into the two planes, and the route's `503` as an answer
// rather than an error.
//
// The assertions that carry weight are the ones about which of **five** things a cell is.
// `state: "unobserved"` is "nothing ever looked"; `state: "unknown"` is "the record that would say
// is gone"; and `null` in `grid.max_db` over an observed cell splits in two on `grid.frames`
// (T-441) — zero frames folded is *nothing has been written here yet*, the normal state of a live
// edge, while frames folded with no level is *no level is in hand*. Collapsing any pair of those is
// the defect docs/16 §4 exists to forbid, and T-413 met it from the other side.

import { test } from "node:test";
import assert from "node:assert/strict";
import { CELL } from "../src/surface/cellrule";
import {
  BYTES_PER_CELL, capFromRefusal, decodeTile, fetchTile, TileBusyError, TileDecodeError,
  type TileResponse,
} from "../src/surface/tile";
import type { TileAddr } from "../src/surface/lattice";

const ADDR: TileAddr = { device: "any", scheme: "view", levelF: 1, levelT: 2, fIndex: 3, tIndex: 4, cells: 2 };

/** The alphabet the route serves beside its planes (T-467), in the route's own order. */
const STATES = ["unobserved", "observed", "unknown", "excluded"];

/** Run-length encodes a list of state names exactly as `hk-api`'s `rle` does. */
function runsOf(cells: readonly string[]): number[] {
  const runs: number[] = [];
  for (const s of cells) {
    const code = STATES.indexOf(s);
    assert.ok(code >= 0, `test fixture used a state the route cannot spell: ${s}`);
    if (runs.length >= 2 && runs[runs.length - 2] === code) runs[runs.length - 1] += 1;
    else runs.push(code, 1);
  }
  return runs;
}

/**
 * The plane table, built the way the route builds it: **distinct** planes once, with `any` and each
 * device naming the index that is theirs. A device whose plane equals the union shares its index —
 * the duplication T-467 removed.
 */
function coverage(
  any: readonly string[],
  opts: {
    grid?: { nt: number; nf: number };
    devices?: readonly { device: string; cells: readonly string[] }[];
    selected?: { device: string; named: boolean; present: boolean };
  } = {},
): NonNullable<TileResponse["coverage"]> {
  const planes: { runs: number[]; cells: number; uniform: string | null }[] = [];
  const intern = (cells: readonly string[]): number => {
    const runs = runsOf(cells);
    const at = planes.findIndex((p) => JSON.stringify(p.runs) === JSON.stringify(runs));
    if (at >= 0) return at;
    planes.push({ runs, cells: cells.length, uniform: cells.length > 0 && cells.every((x) => x === cells[0]) ? cells[0] : null });
    return planes.length - 1;
  };
  const anyPlane = intern(any);
  const devices = (opts.devices ?? []).map((d) => ({ device: d.device, plane: intern(d.cells) }));
  const sel = opts.selected ?? { device: "any", named: false, present: true };
  return {
    grid: opts.grid ?? { nt: 2, nf: 2 },
    states: STATES,
    planes,
    any: { plane: anyPlane },
    devices,
    selected: sel,
  };
}

function resp(over: Partial<TileResponse> = {}): TileResponse {
  return {
    key: { device: "any", scheme: "view", level_f: 1, level_t: 2, f_index: 3, t_index: 4, cells: 2 },
    extent: { nt: 2, nf: 2 },
    axes: { frequency: { levels: 20, cell_hz: 12_500 }, time: { levels: 15, cell_s: 4 } },
    grid: { nt: 2, nf: 2, max_db: [-90, null, -70, -60], range_db: { lo: -90, hi: -60 } },
    coverage: coverage(["observed", "observed", "unobserved", "unknown"]),
    resolution: { source: "spectrum-history", answered: { level: 2 }, fold: { frequency: { direction: "folded" }, time: { direction: "replicated" } } },
    cost: { in_flight_limit: 4 },
    ...over,
  };
}

test("T-495: `extent.t1_s` is carried through, and a route that does not state it says NOTHING", () => {
  // The discriminator between a tile that can still be written and one that is finished, which the
  // cache must read from the ANSWER rather than infer from the address or from how long ago the copy
  // was taken. Seconds on the wire, ns in this client.
  assert.equal(decodeTile(ADDR, resp({ extent: { nt: 2, nf: 2, t0_s: 1789300736, t1_s: 1789300992 } })).t1Ns,
    1789300992e9);
  // **`null` is not "sealed".** An answer without the field made no claim, and the cache falls back
  // to the same number computed from the address rather than treating silence as permission to stop
  // asking — the `BiasTee::Unknown` is not `Off` direction, applied to freshness.
  assert.equal(decodeTile(ADDR, resp()).t1Ns, null);
  assert.equal(decodeTile(ADDR, resp({ extent: { nt: 2, nf: 2, t1_s: Number.NaN } })).t1Ns, null);
});

test("the fifth state: observed, zero frames folded — NOT unknown, NOT grey, NOT 'level not retained'", () => {
  // The live edge, as T-446 measured it post-fix: the radio is demonstrably tuned here (duty up to
  // 1.0) and the pyramid has written nothing yet. Cell 0 has a level; cell 1 was folded from two
  // frames and kept none; cell 2 has had nothing folded into it at all.
  const t = decodeTile(ADDR, resp({
    grid: { nt: 2, nf: 2, max_db: [-90, null, null, null], frames: [7, 2, 0, 0] },
    coverage: coverage(["observed", "observed", "observed", "unobserved"]),
  }));
  assert.deepEqual([...t.state], [CELL.OBSERVED, CELL.NO_LEVEL, CELL.AWAITING, CELL.UNOBSERVED]);
  assert.ok(Number.isNaN(t.value[2]), "an awaiting cell must carry no number anything could colour");

  // **Coverage still decides everything above the measurement.** Zero frames over an unobserved or
  // unknown cell is not the fifth state — the fifth state's whole content is *we know we looked*.
  const nothingFolded = decodeTile(ADDR, resp({
    grid: { nt: 2, nf: 2, max_db: [null, null, null, null], frames: [0, 0, 0, 0] },
    coverage: coverage(["unobserved", "unknown", "observed", "observed"]),
  }));
  assert.deepEqual([...nothingFolded.state], [CELL.UNOBSERVED, CELL.UNKNOWN, CELL.AWAITING, CELL.AWAITING]);
});

test("without per-cell frame counts the decoder claims LESS, not more", () => {
  // No `grid.frames` at all: every observed-and-null cell is NO_LEVEL, never AWAITING. The more
  // specific state is granted only on positive evidence — `BiasTee::Unknown` is not `Off`.
  const t = decodeTile(ADDR, resp({ grid: { nt: 2, nf: 2, max_db: [-90, null, null, null] } }));
  assert.equal(t.state[1], CELL.NO_LEVEL);
  // …and a frames array of the wrong length is not evidence either.
  const short = decodeTile(ADDR, resp({ grid: { nt: 2, nf: 2, max_db: [-90, null, null, null], frames: [0, 0] } }));
  assert.equal(short.state[1], CELL.NO_LEVEL);
});

test("`measured` is how many cells were really measured, so a replicated axis cannot pass as detail", () => {
  const t = decodeTile(ADDR, resp({
    resolution: {
      source: "survey-overview",
      answered: { level: 4 },
      fold: {
        frequency: { direction: "folded", source_cells: 512, served: 2 },
        time: { direction: "replicated", source_cells: 1, served: 2 },
      },
    },
  }));
  assert.deepEqual(t.measured, { nf: 2, nt: 1 }, "a folded axis measured every served cell; a replicated one measured fewer");
  // Absent fold info: the tile's own grid, because claiming LESS measured detail than we can show
  // would be its own invention.
  assert.deepEqual(decodeTile(ADDR, resp()).measured, { nf: 2, nt: 2 });
});

test("T-595: `excluded` draws its measurement and is never grey", () => {
  // The DC notch past the IQ-ring horizon: the radio sampled it, the pyramid holds rows across it,
  // and the observation log says only that the ANALYSIS skipped it. T-588 measured 1 212 such cells
  // holding a measurement and reading `unobserved`.
  const t = decodeTile(ADDR, resp({
    grid: { nt: 2, nf: 2, max_db: [-90, -70, null, -60], frames: [4, 4, 0, 4] },
    coverage: coverage(["observed", "excluded", "excluded", "unobserved"]),
  }));
  assert.deepEqual([...t.state], [CELL.OBSERVED, CELL.EXCLUDED, CELL.AWAITING, CELL.UNOBSERVED]);
  // The level is carried, undimmed and unaltered: it is a real measurement of THIS cell.
  assert.equal(t.value[1], -70);
  // With no level in hand the marks that claim least still answer — an exclusion is not a licence
  // to invent a level, and it is never grey either way.
  assert.ok(Number.isNaN(t.value[2]));

  // A client that has not learned the word falls through to drawing the measurement, never to grey:
  // the safe direction, because the level is real. (This is the old alphabet, verbatim.)
  const old = decodeTile(ADDR, resp({
    grid: { nt: 2, nf: 2, max_db: [-90, -70, -80, -60] },
    coverage: { ...coverage(["observed", "excluded", "excluded", "unobserved"]), states: ["unobserved", "observed", "unknown", "something-new"] },
  }));
  assert.deepEqual([...old.state], [CELL.OBSERVED, CELL.OBSERVED, CELL.OBSERVED, CELL.UNOBSERVED]);
});

test("five cell states, and none of them collapses into another", () => {
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
    coverage: coverage(["unobserved", "observed", "unobserved", "observed"]),
  }));
  assert.deepEqual([...t.state], [CELL.UNOBSERVED, CELL.OBSERVED, CELL.UNOBSERVED, CELL.OBSERVED]);
  assert.ok(Number.isNaN(t.value[0]), "an unobserved cell must not carry the number the grid happened to hold");
});

test("a response that cannot be read is NOT a coverage answer: it throws, so the place stays pending", () => {
  for (const bad of [
    resp({ coverage: undefined }),
    resp({ grid: { nt: 2, nf: 2, max_db: [-90, -80] } }),
    // A plane whose cell count is not the grid it claims: unreadable, so not an answer.
    resp({ coverage: coverage(["observed"]) }),
    resp({ resolution: { source: "guessed" } }),
    // T-467's own failure modes. Each one throws instead of resolving to a state, because a plane
    // we cannot read is not a coverage answer — and the direction that matters is that none of them
    // may come out `observed` or `unobserved` by default.
    // ...runs that do not cover the plane,
    resp({ coverage: { ...coverage(["observed", "observed", "observed", "observed"]), planes: [{ runs: [1, 3], cells: 4 }] } }),
    // ...runs that overrun it,
    resp({ coverage: { ...coverage(["observed", "observed", "observed", "observed"]), planes: [{ runs: [1, 9], cells: 4 }] } }),
    // ...an odd run list,
    resp({ coverage: { ...coverage(["observed", "observed", "observed", "observed"]), planes: [{ runs: [1, 3, 0], cells: 4 }] } }),
    // ...a code outside the served alphabet,
    resp({ coverage: { ...coverage(["observed", "observed", "observed", "observed"]), planes: [{ runs: [7, 4], cells: 4 }] } }),
    // ...and no alphabet at all, which would leave every code meaning whatever the client assumed.
    resp({ coverage: { ...coverage(["observed", "observed", "observed", "observed"]), states: undefined } }),
  ]) {
    assert.throws(() => decodeTile(ADDR, bad), TileDecodeError);
  }
});

test("T-467: the three states survive the run-length encoding, and a run is not a licence to merge them", () => {
  // The compression this ticket bought must not buy it by spelling two states as one. A plane with
  // all three, in runs, decodes back to all three — and `unobserved` is still the ONLY grey.
  const t = decodeTile(ADDR, resp({
    grid: { nt: 2, nf: 2, max_db: [null, null, null, null], frames: [0, 0, 0, 0] },
    coverage: coverage(["unobserved", "unknown", "observed", "observed"]),
  }));
  assert.deepEqual([...t.state], [CELL.UNOBSERVED, CELL.UNKNOWN, CELL.AWAITING, CELL.AWAITING]);

  // The mutation that makes that load-bearing: collapse `unknown` onto `unobserved` in the
  // alphabet — exactly the compression defect the ticket forbids — and the decode changes.
  const collapsed = decodeTile(ADDR, resp({
    grid: { nt: 2, nf: 2, max_db: [null, null, null, null], frames: [0, 0, 0, 0] },
    coverage: { ...coverage(["unobserved", "unknown", "observed", "observed"]), states: ["unobserved", "observed", "unobserved"] },
  }));
  assert.notDeepEqual([...collapsed.state], [...t.state], "the honest decode must differ from the collapsed one");
  assert.equal(collapsed.state[1], CELL.UNOBSERVED, "…and the collapse is what turns `unknown` grey");

  // Observed-and-quiet is a different thing from unobserved, and stays one: same measurement plane
  // (no level anywhere), different coverage, different cell states.
  const quiet = decodeTile(ADDR, resp({
    grid: { nt: 2, nf: 2, max_db: [null, null, null, null], frames: [3, 3, 3, 3] },
    coverage: coverage(["observed", "observed", "observed", "observed"]),
  }));
  const never = decodeTile(ADDR, resp({
    grid: { nt: 2, nf: 2, max_db: [null, null, null, null], frames: [3, 3, 3, 3] },
    coverage: coverage(["unobserved", "unobserved", "unobserved", "unobserved"]),
  }));
  assert.deepEqual([...quiet.state], [CELL.NO_LEVEL, CELL.NO_LEVEL, CELL.NO_LEVEL, CELL.NO_LEVEL]);
  assert.deepEqual([...never.state], [CELL.UNOBSERVED, CELL.UNOBSERVED, CELL.UNOBSERVED, CELL.UNOBSERVED]);
});

test("T-467: two devices that genuinely differ get two planes, and a named device never gets the union's", () => {
  // The multi-SDR case the plane table exists for. `mock:a` looked at the low half, `mock:b` at the
  // high half; the union saw both. Three distinct planes, and each device reads its own.
  const cov = coverage(["observed", "observed", "observed", "observed"], {
    devices: [
      { device: "mock:a", cells: ["observed", "unobserved", "observed", "unobserved"] },
      { device: "mock:b", cells: ["unobserved", "observed", "unobserved", "observed"] },
    ],
    selected: { device: "mock:a", named: true, present: true },
  });
  assert.equal(cov.planes!.length, 3, "three genuinely different planes cost three entries");
  const a = decodeTile({ ...ADDR, device: "mock:a" }, resp({ coverage: cov }));
  const b = decodeTile({ ...ADDR, device: "mock:b" }, resp({ coverage: cov }));
  // `a` looked at the even cells, `b` at the odd ones, and the measurement plane is the same for
  // both: what differs is entirely which cells each radio is entitled to show.
  assert.deepEqual([...a.state], [CELL.OBSERVED, CELL.UNOBSERVED, CELL.OBSERVED, CELL.UNOBSERVED]);
  assert.deepEqual([...b.state], [CELL.UNOBSERVED, CELL.NO_LEVEL, CELL.UNOBSERVED, CELL.OBSERVED]);
  // And the union is neither of them: a merged plane must never wear one radio's identity.
  const union = decodeTile(ADDR, resp({ coverage: cov }));
  assert.notDeepEqual([...union.state], [...a.state]);
  assert.notDeepEqual([...union.state], [...b.state]);

  // The one-device case, which is what the demo backend actually runs: the device's plane IS the
  // union, so the table holds ONE plane and both indices point at it. That is the duplication.
  const one = coverage(["observed", "observed", "unobserved", "unobserved"], {
    devices: [{ device: "mock:a", cells: ["observed", "observed", "unobserved", "unobserved"] }],
    selected: { device: "mock:a", named: true, present: true },
  });
  assert.equal(one.planes!.length, 1, "an identical plane is never repeated");
  assert.equal(one.any!.plane, one.devices![0].plane);
  assert.deepEqual(
    [...decodeTile({ ...ADDR, device: "mock:a" }, resp({ coverage: one })).state],
    [...decodeTile(ADDR, resp({ coverage: one })).state],
    "sharing an index must not change what either reads",
  );
});

test("a named device with no plane is unobserved FOR THAT DEVICE — a coverage answer, not a missing one", () => {
  const addr = { ...ADDR, device: "hackrf:abc" };
  const all = ["observed", "observed", "observed", "observed"];
  const t = decodeTile(addr, resp({
    coverage: coverage(all, {
      devices: [{ device: "mock:1", cells: all }],
      selected: { device: "hackrf:abc", named: true, present: false },
    }),
  }));
  assert.deepEqual([...t.state], [CELL.UNOBSERVED, CELL.UNOBSERVED, CELL.UNOBSERVED, CELL.UNOBSERVED]);
  // And the union's plane is NOT what a named device gets: a merged plane must never wear one
  // radio's identity.
  assert.throws(() => decodeTile(addr, resp({
    coverage: coverage(all, { selected: { device: "hackrf:abc", named: true, present: true } }),
  })), TileDecodeError);
});

test("a coverage plane at a different resolution resamples within the same extent", () => {
  const t = decodeTile(ADDR, resp({
    coverage: coverage(["unobserved"], { grid: { nt: 1, nf: 1 } }),
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

test("T-461: a tile the coverage map answered states its one cell, and it reads as unobserved — not as zeroes", () => {
  // The short-circuited answer: no per-cell arrays at all, one stated cell, and a coverage plane
  // that is uniformly `unobserved`. The renderer must get grey, and must get NO number.
  const t = decodeTile(ADDR, resp({
    grid: { nt: 2, nf: 2, uniform: { max_db: null, frames: 0 }, range_db: null },
    coverage: coverage(["unobserved", "unobserved", "unobserved", "unobserved"]),
  }));
  assert.deepEqual([...t.state], [CELL.UNOBSERVED, CELL.UNOBSERVED, CELL.UNOBSERVED, CELL.UNOBSERVED]);
  assert.ok([...t.value].every(Number.isNaN), "an unobserved tile must carry no number anything could colour");
  assert.equal(t.rangeDb, null, "no scale from nothing");

  // **The trap, made into a test.** If the same body arrived with `max_db: 0` — a level of zero
  // rather than the absence of one — the decoder would colour it, because the coverage plane is the
  // only thing standing between the two. So the states stay grey ONLY because coverage says so, and
  // the stated cell contributes nothing: swap coverage to observed and the same uniform grid reads
  // as "we looked and nothing has been folded here yet", never as a measurement.
  const awaiting = decodeTile(ADDR, resp({
    grid: { nt: 2, nf: 2, uniform: { max_db: null, frames: 0 } },
    coverage: coverage(["observed", "observed", "observed", "observed"]),
  }));
  assert.deepEqual([...awaiting.state], [CELL.AWAITING, CELL.AWAITING, CELL.AWAITING, CELL.AWAITING]);
  assert.ok([...awaiting.value].every(Number.isNaN));
  assert.notDeepEqual([...awaiting.state], [...t.state], "observed-and-empty is not unobserved");

  // And a grid that is neither an array of the right length nor a stated uniform cell is
  // unreadable — it does not fall back to either spelling.
  assert.throws(() => decodeTile(ADDR, resp({ grid: { nt: 2, nf: 2 } })), TileDecodeError);
  assert.throws(() => decodeTile(ADDR, resp({ grid: { nt: 2, nf: 2, uniform: undefined, max_db: [-90] } })), TileDecodeError);
});

// ——— T-520: the last-known (shadow) tier ———
//
// Wire fixtures shaped exactly as docs/api.md's `shadow` block serves them (T-519): column runs as
// parallel arrays, a `sources` table, `edge_s` and a `search` block — the fields the client does not
// read are present anyway, so a decoder that tripped on them would fail here and not in a browser.

type Shadow = NonNullable<TileResponse["shadow"]>;

function shadowOf(runs: readonly { f: number; row: number; rows: number; db: number; t?: number; src?: number }[]): Shadow {
  return {
    encoding: "column-runs",
    runs: runs.length,
    f: runs.map((r) => r.f),
    row: runs.map((r) => r.row),
    rows: runs.map((r) => r.rows),
    last_db: runs.map((r) => r.db),
    last_t_s: runs.map((r) => r.t ?? 1789300620.0),
    src: runs.map((r) => r.src ?? 1),
    // Served, not read — carried so the fixture is the real shape.
    ...({
      sources: [
        { from: "this-tile", level: 2, statement: "this tile's own grid" },
        { from: "before-tile", store: "spectrum-history", level: 1, f_cell_hz: 12500.0, t_cell_s: 60.0 },
      ],
      edge_s: 1789309800.5,
      search: { store: "spectrum-history", before_s: 1789300736.0, searched_from_s: 1788912000.0, columns_found: 2,
        unsearched: [], stages: [], source_cells: 5120, chunks: 3, build_ms: 0.8, rule: "…" },
      rule: "the LAST-KNOWN tier (docs/adr/0020), NOT a measurement of the row it is drawn on.",
    } as object),
  } as Shadow;
}

/** A departed band on a 2×2 grid: column 1 swept earlier, now unobserved; column 0 never swept. */
const DEPARTED = (): TileResponse => resp({
  grid: { nt: 2, nf: 2, max_db: [null, null, null, null], frames: [0, 0, 0, 0] },
  coverage: coverage(["unobserved", "unobserved", "unobserved", "unobserved"]),
  shadow: shadowOf([{ f: 1, row: 0, rows: 2, db: -96.5 }]),
});

test("swept then departed = SHADOW carrying the last level; never swept = THE grey, with no number", () => {
  const t = decodeTile(ADDR, DEPARTED());
  // Row-major [t * nf + f]: column 1 is cells 1 and 3.
  assert.deepEqual([...t.state], [CELL.UNOBSERVED, CELL.SHADOW, CELL.UNOBSERVED, CELL.SHADOW]);
  assert.equal(t.value[1], -96.5);
  assert.equal(t.value[3], -96.5);
  assert.ok(Number.isNaN(t.value[0]) && Number.isNaN(t.value[2]), "grey must carry no number anything could colour");
});

test("the shadow is served on the coverage short-circuit too (T-461's uniform grid), and decodes there", () => {
  const t = decodeTile(ADDR, resp({
    grid: { nt: 2, nf: 2, uniform: { max_db: null, frames: 0 }, range_db: null },
    coverage: coverage(["unobserved", "unobserved", "unobserved", "unobserved"]),
    shadow: shadowOf([{ f: 0, row: 1, rows: 1, db: -101.2 }]),
  }));
  assert.deepEqual([...t.state], [CELL.UNOBSERVED, CELL.UNOBSERVED, CELL.SHADOW, CELL.UNOBSERVED]);
  assert.equal(t.value[2], Math.fround(-101.2), "the level, at the Float32 the GPU plane holds");
});

test("coverage alone decides grey: a run never overrides observed, unknown, awaiting or no-level", () => {
  // The route's rule is that runs cover only unobserved cells; if one ever reaches a cell coverage
  // calls something else, the coverage state wins and the shadow value is not drawn.
  const t = decodeTile(ADDR, resp({
    grid: { nt: 2, nf: 2, max_db: [null, null, null, null], frames: [0, 3, 0, 0] },
    coverage: coverage(["observed", "observed", "unknown", "unobserved"]),
    shadow: shadowOf([{ f: 0, row: 0, rows: 2, db: -90 }, { f: 1, row: 0, rows: 2, db: -80 }]),
  }));
  assert.deepEqual([...t.state], [CELL.AWAITING, CELL.NO_LEVEL, CELL.UNKNOWN, CELL.SHADOW]);
  assert.ok(Number.isNaN(t.value[0]) && Number.isNaN(t.value[1]) && Number.isNaN(t.value[2]));
  assert.equal(t.value[3], -80);
});

test("no shadow block, a null one, or zero runs: every unobserved cell stays THE grey", () => {
  for (const shadow of [undefined, null, shadowOf([])]) {
    const t = decodeTile(ADDR, resp({ ...DEPARTED(), shadow }));
    assert.deepEqual([...t.state], [CELL.UNOBSERVED, CELL.UNOBSERVED, CELL.UNOBSERVED, CELL.UNOBSERVED], JSON.stringify(shadow));
  }
});

test("a shadow never replaces a measurement: a run over a measured cell is a contract break, and throws", () => {
  // Defensive: T-519 guarantees no run covers a row where `grid` holds a value. A response that does
  // has confused which number is THIS cell's measurement, and the client does not get to pick.
  assert.throws(() => decodeTile(ADDR, resp({
    grid: { nt: 2, nf: 2, max_db: [-70, null, null, null] },
    coverage: coverage(["unobserved", "unobserved", "unobserved", "unobserved"]),
    shadow: shadowOf([{ f: 0, row: 0, rows: 2, db: -96 }]),
  })), /covers a measured cell/);
});

test("an unreadable shadow is not 'no shadow': the tile stays pending rather than painting grey", () => {
  const bad = (mut: (s: Shadow) => void) => {
    const s = shadowOf([{ f: 1, row: 0, rows: 2, db: -96.5 }]);
    mut(s);
    return () => decodeTile(ADDR, resp({ ...DEPARTED(), shadow: s }));
  };
  assert.throws(bad((s) => { s.encoding = "plane"; }), TileDecodeError);
  assert.throws(bad((s) => { s.runs = 2; }), TileDecodeError, "arrays shorter than runs");
  assert.throws(bad((s) => { s.f = [2]; }), TileDecodeError, "column outside the grid");
  assert.throws(bad((s) => { s.rows = [3]; }), TileDecodeError, "rows past the grid");
  assert.throws(bad((s) => { s.rows = [0]; }), TileDecodeError, "an empty run");
  assert.throws(bad((s) => { (s.last_db as unknown[]) = [null]; }), TileDecodeError, "a run with no level");
  assert.throws(bad((s) => { s.last_t_s = [Number.NaN]; }), TileDecodeError, "a level with no age is not last-known");
  // Two runs over one cell.
  assert.throws(() => decodeTile(ADDR, resp({ ...DEPARTED(), shadow: shadowOf([{ f: 1, row: 0, rows: 2, db: -1 }, { f: 1, row: 1, rows: 1, db: -2 }]) })),
    /overlap/);
});
