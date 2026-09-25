// **T-916 — a last-known (shadow) cell says WHICH search it came from, and the pane says it too.**
//
// T-911 made a recently-departed band's shadow the very cell its last live row was drawn with, by
// searching the tile's own level first. Its residue is the case that search cannot reach: a band
// that left longer ago than `PINNED_REACH_BLOCKS` (≈4 h at scheme 1's level 0) is answered by the
// spectrum-history **ladder** instead, whose cells are coarser in both axes — measured over the
// stale-departure fixture in `crates/hk-api/tests/tile_cost.rs` as **50 kHz × 3600 s against the
// tile's own 6.25 kHz × 1 s** — and a max-hold over a box that much larger reads at or *hotter*
// than the row the band was last live on (10–15 dB on the mock SDR's FM band, T-911).
//
// Both answers are honest last-known values; neither is grey; and they are **not the same
// resolution**. This surface's standing rule is that a pane states the level it was actually drawn
// at (CLAUDE.md: *"a wide or deep zoom shows overview rather than upscaled detail presented as
// measurement"*), so the resolution of the shadow is stated the same way. That is what this file
// asserts, end to end and in the direction the data flows:
//
//  1. `decodeTile` reads the `sources` table beside the runs and counts own-level / ladder /
//     unstated, with the coarsest coarse cell — and an answer that labels nothing says `unstated`,
//     never "own level", because the claim that says least is the one that gets made by default.
//  2. `Surface.render` counts it over the tiles a pane actually DREW, stand-ins included.
//  3. `paneStatuses` turns it into one clause, and only for the coarser case — an own-level shadow
//     IS the pane's own cell, so the level line already speaks for it.
//  4. `readoutOf` carries that clause, and marks the row so a test reads state, not prose.
//
// **The contrast half of T-916 is here too** (`contrast.ts`'s header): a shadow never sets the
// display range in any mode, and it is deliberately NOT pinned when a tracking mode re-scales.
// `surface-vscale.test.ts` covers `viewport`'s exclusion; what is asserted below is `auto`'s, which
// works for a different reason — a departed tile carries no `range_db` at all.

import { test } from "node:test";
import assert from "node:assert/strict";
import { autoContrastButton } from "../src/surface/contrast";
import { readoutOf } from "../src/surface/chrome";
import { CELL } from "../src/surface/cellrule";
import { keyOf, type Box, type Lattice, type TileAddr } from "../src/surface/lattice";
import { PaneModel, paneStatuses } from "../src/surface/panes";
import { FALLBACK_RANGE, Surface, type PaneView } from "../src/surface/surface";
import { TileCache } from "../src/surface/tilecache";
import { decodeTile, type TileData, type TileResponse } from "../src/surface/tile";
import { stubGl } from "./surface-glstub";

const LAT: Lattice = { scheme: "view", cells: 256, f0Hz: 6250, t0Ns: 1e9, levelsF: 20, levelsT: 15 };
const S = 1e9;
const T0 = 1_700_000_000 * S;
const W = 1200, H = 800;
const RECT = { x: 0, y: 0, w: W, h: H };
const ADDR: TileAddr = { device: "any", scheme: "view", levelF: 0, levelT: 0, fIndex: 4, tIndex: 9, cells: 2 };
const flush = () => new Promise((r) => setImmediate(r));

// ——— the wire, shaped as docs/api.md's `shadow` block serves it ———

type Src = { search?: string; fHz?: number; tS?: number };

/** A two-by-two answer whose whole grid is unobserved, with one shadow run per column. */
function answer(sources: readonly Src[], srcOf: readonly number[]): TileResponse {
  return {
    key: { device: "any", scheme: "view", level_f: 0, level_t: 0, f_index: 4, t_index: 9, cells: 2 },
    extent: { nf: 2, nt: 2 },
    axes: { frequency: { levels: 20, cell_hz: 6250 }, time: { levels: 15, cell_s: 1 } },
    grid: { nf: 2, nt: 2, uniform: { max_db: null, frames: 3 } },
    coverage: { states: ["unobserved"], planes: [{ runs: [0, 4], cells: 4 }], any: { plane: 0 },
      selected: { device: "any", named: false, present: true, plane: 0 } },
    resolution: { source: "spectrum-history", answered: { level: 0 } },
    shadow: {
      encoding: "column-runs", runs: srcOf.length,
      f: srcOf.map((_, i) => i % 2),
      row: srcOf.map(() => 0),
      rows: srcOf.map(() => 2),
      last_db: srcOf.map(() => -101.5),
      last_t_s: srcOf.map(() => 1_700_000_000),
      src: [...srcOf],
      sources: [
        { from: "this-tile", level: 0 },
        ...sources.map((s) => ({
          from: "before-tile", store: "spectrum-history", level: 3,
          ...(s.search === undefined ? {} : { search: s.search }),
          ...(s.fHz === undefined ? {} : { f_cell_hz: s.fHz }),
          ...(s.tS === undefined ? {} : { t_cell_s: s.tS }),
        })),
      ],
    },
  } as unknown as TileResponse;
}

test("T-916: a shadow run carries which search found it, and the coarsest ladder cell behind it", () => {
  // Column 0 was answered at the tile's own level (T-911's case); column 1 by the ladder, from a
  // 50 kHz × 3600 s cell — the pair `tile_cost.rs`'s stale-departure fixture actually produces.
  const both = decodeTile(ADDR, answer(
    [{ search: "own-level", fHz: 6250, tS: 1 }, { search: "ladder", fHz: 50000, tS: 3600 }],
    [1, 2],
  ));
  assert.deepEqual(both.shadowSource, {
    carried: 2, ownLevel: 1, ladder: 1, unstated: 0, coarsest: { fHz: 50000, tS: 3600 },
  });
  // Every column at the tile's own level: nothing coarser is on screen, so there is nothing to say.
  const own = decodeTile(ADDR, answer([{ search: "own-level", fHz: 6250, tS: 1 }], [1, 1]));
  assert.deepEqual(own.shadowSource, { carried: 2, ownLevel: 2, ladder: 0, unstated: 0, coarsest: null });
  // A run out of the tile's OWN grid (`src` 0 — the carry, T-519, and the backward fill, T-527) is
  // not carried in from anywhere: its cell is this tile's, and it makes no claim about an older one.
  const carry = decodeTile(ADDR, answer([{ search: "ladder", fHz: 50000, tS: 3600 }], [0, 0]));
  assert.equal(carry.shadowSource, null, "the tile's own cells are not a source claim");
});

test("T-916: a source the answer did not label is UNSTATED, and unstated is stated — never read as own-level", () => {
  // A pre-T-911 server labels no search at all. Reading that as "the tile's own level" would let a
  // 6.25 kHz × 1 s max-hold pass for a 586 Hz × 40 ms one — the exact confusion T-911 closed — so
  // the unlabelled case is counted with the ladder and the pane still says something.
  const old = decodeTile(ADDR, answer([{ fHz: 12500, tS: 60 }], [1, 1]));
  assert.deepEqual(old.shadowSource, { carried: 2, ownLevel: 0, ladder: 0, unstated: 2, coarsest: { fHz: 12500, tS: 60 } });
  // And a source table that is missing entirely is still not "own level": the cell is unknown, so
  // `coarsest` states nothing rather than inventing a number to print.
  const bare = answer([], [1, 1]);
  delete (bare.shadow as { sources?: unknown }).sources;
  const none = decodeTile(ADDR, bare);
  assert.deepEqual(none.shadowSource, { carried: 2, ownLevel: 0, ladder: 0, unstated: 2, coarsest: { fHz: 0, tS: 0 } });
});

test("T-916: an unreadable source table does not take the shadow plane off the screen", () => {
  // The source table is a statement ABOUT the shadow, not the shadow. `shadowCells` still holds the
  // plane itself to every rule it had (a run over a measured cell, two runs over one cell, …), but
  // a nonsense `src` index must not turn a legitimate last-known cell into pending or grey.
  const v = answer([{ search: "ladder", fHz: 50000, tS: 3600 }], [7, 1]);
  const d = decodeTile(ADDR, v);
  assert.equal(d.state[0], CELL.SHADOW, "the plane still drew");
  assert.deepEqual(d.shadowSource, { carried: 2, ownLevel: 0, ladder: 1, unstated: 1, coarsest: { fHz: 50000, tS: 3600 } });
});

// ——— 2–4: the count reaches the pane, and the pane says it ———

/** A tile drawn from `ladder`-sourced shadows, exactly as the stale-departure fixture serves them. */
function ladderTile(a: TileAddr): TileData {
  return {
    addr: a, key: keyOf(a), nf: 2, nt: 2, t1Ns: null, asOfNs: null,
    value: new Float32Array([-101.5, -101.5, -101.5, -101.5]),
    state: new Uint8Array([CELL.SHADOW, CELL.SHADOW, CELL.SHADOW, CELL.SHADOW]),
    tier: "spectrum-history", answeredLevel: 0, fold: { frequency: "exact", time: "exact" },
    measured: { nf: 2, nt: 2 },
    // A tile that is ALL last-known has no measured range at all — which is exactly why `auto`
    // cannot be moved by one (T-916's contrast decision; the route serves `range_db: null` for it).
    rangeDb: null, bytes: 192 * 1024, serverInFlightLimit: null, serverInFlightShare: null,
    shadowSource: { carried: 2, ownLevel: 0, ladder: 2, unstated: 0, coarsest: { fHz: 50000, tS: 3600 } },
  };
}

function ownLevelTile(a: TileAddr): TileData {
  return { ...ladderTile(a), shadowSource: { carried: 2, ownLevel: 2, ladder: 0, unstated: 0, coarsest: null } };
}

function harness(data: (a: TileAddr) => TileData) {
  const g = stubGl(W, H);
  const surface = new Surface(
    g.canvas, LAT,
    (tex) => new TileCache(tex, (a) => Promise.resolve(data(a)), { inFlight: 64, now: () => 0 }),
    { pinParents: false },
  );
  return { g, surface };
}

const BOX: Box = { f0Hz: 100e6, f1Hz: 100e6 + 1.6e6, t0Ns: T0, t1Ns: T0 + 200 * S };
const pane = (id: string, box: Box): PaneView => ({ id, rect: RECT, box });

async function settle(surface: Surface, box: Box, frames = 6) {
  let reports = surface.render([pane("a", box)]);
  for (let i = 1; i < frames; i++) {
    await flush();
    reports = surface.render([pane("a", box)]);
  }
  return reports;
}

test("T-916: a pane counts the tiles whose last-known cells came from the coarser source, and only those", async () => {
  const coarse = await settle(harness(ladderTile).surface, BOX);
  assert.ok(coarse[0].shadowLadder > 0, "a pane drawn entirely from ladder-sourced shadows counted none");
  assert.equal(coarse[0].shadowCellHz, 50000);
  assert.equal(coarse[0].shadowCellS, 3600);

  const own = await settle(harness(ownLevelTile).surface, BOX);
  assert.equal(own[0].shadowLadder, 0, "an own-level shadow is the pane's OWN cell: nothing extra to state");
  assert.deepEqual([own[0].shadowCellHz, own[0].shadowCellS], [0, 0]);
});

/** The pane statuses for a surface drawn over `data`, through the real pane model. */
async function statuses(data: (a: TileAddr) => TileData) {
  const { surface } = harness(data);
  const m = new PaneModel({
    bounds: { f0Hz: 1e6, f1Hz: 6e9, t0Ns: T0 - 86_400 * S, t1Ns: T0 },
    lattice: LAT, width: W, height: H, freq: { centerHz: 100.8e6, spanHz: 1.6e6 }, spanNs: 200 * S,
  });
  let reports = surface.render(m.views(T0));
  for (let i = 0; i < 5; i++) { await flush(); reports = surface.render(m.views(T0)); }
  return paneStatuses(m.list(), reports, T0, m.rects());
}

test("T-916: the pane READOUT names the coarser last-known source, its cell, and which way it leans", async () => {
  const [coarse] = await statuses(ladderTile);
  const label = coarse.shadowLabel;
  assert.ok(label, "a pane drawn over ladder-sourced shadows said nothing about the resolution it drew at");
  assert.match(label, /ladder/, `the source must be named: ${label}`);
  assert.match(label, /50\.0 kHz|50 kHz/, `the cell must be stated: ${label}`);
  assert.match(label, /hotter/, `the direction of the difference is the point: ${label}`);

  const [own] = await statuses(ownLevelTile);
  assert.equal(own.shadowLabel, null, "an own-level shadow needs no second sentence: it IS the stated level");

  // And it reaches the readout, marked on the row as state rather than only as prose — the same
  // discipline as `data-tier`: a test (and a stylesheet) must not have to parse a sentence.
  const row = readoutOf(await statuses(ladderTile)).rows[0];
  assert.equal(row.shadow, label);
  const clean = readoutOf(await statuses(ownLevelTile)).rows[0];
  assert.equal(clean.shadow, null);
});

// ——— the contrast decision (T-916 (2)) ———

test("T-916: `auto` contrast cannot be moved by a departed band — an all-shadow tile carries no range", async () => {
  const { surface } = harness(ladderTile);
  surface.setRangeMode("auto");
  const before = { lo: surface.lo, hi: surface.hi };
  assert.deepEqual(before, { lo: FALLBACK_RANGE.lo, hi: FALLBACK_RANGE.hi });
  await settle(surface, BOX, 8);
  assert.deepEqual({ lo: surface.lo, hi: surface.hi }, before,
    "a tile of remembered cells set the display range: the live cells beside it would be scaled to a time that is over");
});

test("T-916: the tracking modes SAY that a shadow moves with everything else, rather than leaving it to be discovered", () => {
  const on = autoContrastButton("auto");
  assert.equal(on.pressed, true);
  assert.match(on.title, /shadow/, `the tracking mode's own text must place the last-known tier: ${on.title}`);
  assert.match(on.title, /never set it|does not set|never sets/, "a shadow is excluded from the measurement, and the text says so");
  assert.match(on.title, /anchored/, "and it names the mode a viewer wants when they want the colour held");
});
