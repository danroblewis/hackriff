// T-1043 (LSR-2): the client half of `GET /ws/spectrum/rows` — **the request a pane builds** and the
// decode of the binary blocks it gets back.
//
// The request half is the guard CLAUDE.md asks for: contract tests assert the *server* serves a route
// correctly, never that the client calls it correctly (T-367 asked `/api/timeline` for no band at all
// and drew an empty canvas with every suite green). So these assert the URL, and they fail if a pane
// subscription could be spelled as "now".
//
// The decode half builds blocks **byte by byte** here, to the layout `docs/stream-contract.md` §17
// states, rather than reusing the encoder — a test that shares the producer's code cannot catch the
// producer disagreeing with the document.
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  BLOCK_HEADER_BYTES, COVERAGE_STATES, decodePaneBlock, FLAG_DISCONTINUITY, FLAG_FINAL, isObserved,
  KIND_ROWS, KIND_UNOBSERVED, panePath, parsePaneHeader, TRAILER_RUN8, VALUES_F16_LE,
  type PaneBlock, type PaneHeader, type PaneWindow,
} from "../src/surface/panerows";
import { TileDecodeError } from "../src/surface/tile";

const NF = 8;
const T_CELL_NS = 40_106_667;
const pane: PaneWindow = { fLoHz: 88e6, fHiHz: 108e6, nf: NF };

const headerJson = (over: Record<string, unknown> = {}) => JSON.stringify({
  type: "subscribed",
  pane: {
    f_lo_hz: 88e6, f_hi_hz: 108e6, nf: NF, f_cell_hz: 20e6 / NF,
    device: "any", scheme: "view", level_t: 0, t_cell_s: T_CELL_NS / 1e9,
  },
  range: { t_from: 400, t_to: null, t0_s: 4e-7, t1_s: null, row0: 0, open: true },
  record: {
    contract: "docs/stream-contract.md#17", byte_order: "little-endian",
    header_bytes: BLOCK_HEADER_BYTES,
    kinds: { rows: KIND_ROWS, unobserved: KIND_UNOBSERVED },
    values: { code: VALUES_F16_LE, type: "f16", absent: "nan" },
    coverage: { encoding: TRAILER_RUN8, name: "run8", states: [...COVERAGE_STATES] },
  },
  epoch: 3, rows_per_block: 64, store: "view-lattice",
  ...over,
});

const header = (): PaneHeader => parsePaneHeader(headerJson());

/** binary16 of `x`, by the rule the server encodes with (NaN stays NaN). */
function f16(x: number): number {
  if (Number.isNaN(x)) return 0x7e00;
  const f = new Float32Array(1), u = new Uint32Array(f.buffer);
  f[0] = x;
  const bits = u[0], sign = (bits >>> 16) & 0x8000, exp = ((bits >>> 23) & 0xff) - 127 + 15;
  const mant = bits & 0x7fffff;
  if (exp >= 31) return sign | 0x7c00;
  if (exp <= 0) return sign;
  return sign | (exp << 10) | (mant >>> 13);
}

/** One block, assembled to the documented layout. `values` is `rows × nf`; `runs` is `[cells, state]`. */
function blockBytes(o: {
  kind?: number; flags?: number; values?: (number | null)[]; rows: number; nf: number;
  row0: number; epoch?: number; level?: number; tier?: number; fold?: number;
  runs?: [number, number][]; observedCells?: number; valueCode?: number;
}): ArrayBuffer {
  const kind = o.kind ?? KIND_ROWS;
  const cells = o.values ?? [];
  const runs = o.runs ?? [];
  // Sized from what the caller gave, not from the kind — so a malformed block (a grey stretch with
  // a payload) can be built here and refused by the decoder rather than by this builder.
  const payload = cells.length * 2;
  const trailer = runs.length ? 8 + runs.length * 8 : 0;
  const buf = new ArrayBuffer(BLOCK_HEADER_BYTES + payload + trailer);
  const v = new DataView(buf), b = new Uint8Array(buf);
  b[0] = kind;
  b[1] = o.flags ?? 0;
  b[2] = kind === KIND_ROWS ? (o.valueCode ?? VALUES_F16_LE) : 0;
  b[3] = o.level ?? 0;
  b[4] = o.tier ?? 1;
  b[5] = o.fold ?? 0;
  v.setUint16(6, kind === KIND_ROWS ? o.nf : 0, true);
  v.setUint32(8, o.rows, true);
  v.setUint32(12, o.epoch ?? 0, true);
  v.setBigInt64(16, BigInt(o.row0) * BigInt(T_CELL_NS), true);
  v.setBigInt64(24, BigInt(T_CELL_NS), true);
  v.setBigInt64(32, BigInt(o.row0), true);
  v.setUint32(40, trailer, true);
  v.setUint32(44, o.observedCells ?? cells.filter((c) => c !== null).length, true);
  cells.forEach((c, i) => v.setUint16(BLOCK_HEADER_BYTES + i * 2, f16(c ?? NaN), true));
  if (trailer) {
    v.setUint8(BLOCK_HEADER_BYTES + payload, TRAILER_RUN8);
    v.setUint8(BLOCK_HEADER_BYTES + payload + 1, COVERAGE_STATES.length);
    v.setUint8(BLOCK_HEADER_BYTES + payload + 2, 0b11); // present, aligned
    v.setUint32(BLOCK_HEADER_BYTES + payload + 4, runs.length, true);
    runs.forEach(([n, state], r) => {
      const at = BLOCK_HEADER_BYTES + payload + 8 + r * 8;
      v.setUint32(at, n, true);
      v.setUint8(at + 4, state);
    });
  }
  return buf;
}

/** Two rows of values whose hot column is the row's own index — the address is in the data. */
const twoRows = (row0: number): (number | null)[] => {
  const out: (number | null)[] = [];
  for (let r = row0; r < row0 + 2; r++) for (let f = 0; f < NF; f++) out.push(f === r % NF ? -40 : -100);
  return out;
};

test("the request is a pane and a range, and cannot be spelled as now", () => {
  assert.equal(
    panePath(pane, { tFromNs: 1_500_000_000, tToNs: null }),
    "/ws/spectrum/rows?f_lo_hz=88000000&f_hi_hz=108000000&nf=8&t_from=1500000000",
  );
  // A closed range, a coarser row period, a named device and a scheme all travel; the defaults do not.
  assert.equal(
    panePath(
      { ...pane, levelT: 2, scheme: "overview", device: "hackrf:abc" },
      { tFromNs: 10, tToNs: 20 },
    ),
    "/ws/spectrum/rows?f_lo_hz=88000000&f_hi_hz=108000000&nf=8&t_from=10&t_to=20"
      + "&level_t=2&scheme=overview&device=hackrf%3Aabc",
  );
  // No start, no subscription: there is no implicit now, and nothing invents one.
  assert.throws(() => panePath(pane, { tFromNs: NaN, tToNs: null }), /needs a start instant/);
  assert.throws(
    () => panePath(pane, { tFromNs: undefined as unknown as number, tToNs: null }),
    /needs a start instant/,
  );
  assert.throws(() => panePath(pane, { tFromNs: 10, tToNs: 10 }), /must be an instant after/);
  // A pane is a window and a column count, checked before anything is asked of the server.
  assert.throws(() => panePath({ ...pane, fHiHz: 88e6 }, { tFromNs: 0, tToNs: null }), /needs a window/);
  assert.throws(() => panePath({ ...pane, nf: 4 }, { tFromNs: 0, tToNs: null }), /nf must be/);
  assert.throws(() => panePath({ ...pane, nf: 8192 }, { tFromNs: 0, tToNs: null }), /nf must be/);
});

test("the header states the wire, and an unreadable one is refused rather than assumed", () => {
  const h = header();
  assert.equal(h.nf, NF);
  assert.equal(h.epoch, 3);
  assert.equal(h.rowsPerBlock, 64);
  assert.deepEqual([...h.states], [...COVERAGE_STATES]);
  const over = (record: Record<string, unknown>) =>
    headerJson({ record: { ...JSON.parse(headerJson()).record, ...record } });
  assert.throws(() => parsePaneHeader(over({ header_bytes: 32 })), TileDecodeError);
  assert.throws(() => parsePaneHeader(over({ values: { code: 2, type: "u8", absent: "zero" } })), TileDecodeError);
  assert.throws(() => parsePaneHeader(over({ coverage: { encoding: 7, states: [...COVERAGE_STATES] } })), TileDecodeError);
  // Two states is the honesty failure the four exist to prevent: refuse, never fold them together.
  assert.throws(
    () => parsePaneHeader(over({ coverage: { encoding: TRAILER_RUN8, states: ["unobserved", "observed"] } })),
    TileDecodeError,
  );
  assert.throws(() => parsePaneHeader(JSON.stringify({ type: "refused", status: 400 })), TileDecodeError);
});

test("a block decodes to its rows, its coverage and the claims it makes about itself", () => {
  const h = header();
  const buf = blockBytes({
    rows: 2, nf: NF, row0: 41, values: twoRows(41), epoch: 3, level: 0, tier: 1,
    fold: 0b0001, flags: FLAG_FINAL, runs: [[2 * NF, COVERAGE_STATES.indexOf("observed")]],
  });
  const m = decodePaneBlock(h, buf);
  assert.equal(m.kind, "rows");
  const block = m as PaneBlock;
  assert.equal(block.row0, 41);
  assert.equal(block.rows, 2);
  assert.equal(block.nf, NF);
  // The instant is the header's exact i64, not a float reconstructed from seconds.
  assert.equal(block.t0Ns, 41 * T_CELL_NS);
  assert.equal(block.tCellNs, T_CELL_NS);
  assert.equal(block.epoch, 3);
  assert.equal(block.final, true);
  assert.equal(block.discontinuity, false);
  assert.equal(block.tier, "spectrum-history");
  assert.deepEqual(block.fold, { frequency: "folded", time: "exact" });
  assert.equal(block.observedCells, 2 * NF);
  // Each row's hot column is its own index: a row decoded at the wrong offset fails here.
  for (const r of [0, 1]) {
    const row = [...block.values.slice(r * NF, (r + 1) * NF)];
    assert.equal(row.indexOf(Math.max(...row)), (41 + r) % NF);
  }
  assert.ok(isObserved(block, 0));
  assert.equal(block.coverage.length, 2 * NF);
});

test("an absent cell is NaN, never a level — and never a zero", () => {
  const h = header();
  const values: (number | null)[] = [...twoRows(0)];
  values[3] = null;
  const block = decodePaneBlock(h, blockBytes({
    rows: 2, nf: NF, row0: 0, values, runs: [[2 * NF, COVERAGE_STATES.indexOf("observed")]],
  })) as PaneBlock;
  assert.ok(Number.isNaN(block.values[3]), "an absent cell decodes to NaN");
  assert.ok(!Number.isNaN(block.values[2]), "its neighbour is still a measurement");
  assert.notEqual(block.values[3], 0);
});

test("a grey stretch carries no payload, and is marked as a part the store did not have", () => {
  const h = header();
  const m = decodePaneBlock(h, blockBytes({
    kind: KIND_UNOBSERVED, rows: 4096, nf: 0, row0: 0, flags: FLAG_DISCONTINUITY,
  }));
  assert.equal(m.kind, "unobserved");
  assert.equal(m.rows, 4096);
  assert.equal(m.discontinuity, true, "the client has no values for it: fill from tiles");
  assert.equal(m.final, false);
});

test("DISCONTINUITY survives the decode on a measured block too", () => {
  const h = header();
  const block = decodePaneBlock(h, blockBytes({
    rows: 2, nf: NF, row0: 64, values: twoRows(64), flags: FLAG_DISCONTINUITY | FLAG_FINAL,
    runs: [[2 * NF, COVERAGE_STATES.indexOf("observed")]],
  })) as PaneBlock;
  assert.equal(block.discontinuity, true, "rows the store did not have are the client's to fill");
  assert.equal(block.final, true);
});

test("a block this client cannot read is refused, never rendered with a guessed meaning", () => {
  const h = header();
  const ok = { rows: 2, nf: NF, row0: 0, values: twoRows(0), runs: [[2 * NF, 1]] as [number, number][] };
  // An unknown kind, value encoding, tier, fold direction or flag bit.
  assert.throws(() => decodePaneBlock(h, blockBytes({ ...ok, kind: 9 })), TileDecodeError);
  assert.throws(() => decodePaneBlock(h, blockBytes({ ...ok, valueCode: 2 })), TileDecodeError);
  assert.throws(() => decodePaneBlock(h, blockBytes({ ...ok, tier: 7 })), TileDecodeError);
  assert.throws(() => decodePaneBlock(h, blockBytes({ ...ok, fold: 0b0011 })), TileDecodeError);
  assert.throws(() => decodePaneBlock(h, blockBytes({ ...ok, flags: 0b1000 })), TileDecodeError);
  // A pane width that is not the subscription's.
  assert.throws(() => decodePaneBlock(h, blockBytes({ ...ok, nf: NF / 2 })), TileDecodeError);
  // A length that does not add up, in either direction.
  const short = blockBytes(ok).slice(0, BLOCK_HEADER_BYTES + 4);
  assert.throws(() => decodePaneBlock(h, short), TileDecodeError);
  assert.throws(() => decodePaneBlock(h, new ArrayBuffer(12)), TileDecodeError);
  // A trailer that does not cover the block's cells, and a code outside the served alphabet.
  assert.throws(() => decodePaneBlock(h, blockBytes({ ...ok, runs: [[3, 1]] })), TileDecodeError);
  assert.throws(() => decodePaneBlock(h, blockBytes({ ...ok, runs: [[2 * NF, 9]] })), TileDecodeError);
  // A grey block with a payload is not a grey block.
  assert.throws(
    () => decodePaneBlock(h, blockBytes({ ...ok, kind: KIND_UNOBSERVED, runs: [] })),
    TileDecodeError,
  );
});
