// T-440: the de-welded addressing, and the request the client builds for it.
//
// The second half matters as much as the first. `just gate`'s one blind spot is a client that asks
// the backend for the WRONG THING — T-367's time navigator requested `/api/timeline` with no band
// and drew an empty canvas while every suite stayed green — so these tests assert the URL, not only
// the arithmetic behind it.

import { test } from "node:test";
import assert from "node:assert/strict";
import {
  ancestor, ancestorsOf, extentOf, fCellHz, fTileHz, intersects, keyOf, latticeFrom,
  levelForHzPerPx, levelForNsPerPx, levelsFor, tCellNs, tierFor, tilesFor, tileUrl,
  VIEWPORT_TILE_BUDGET, type Lattice, type LatticeSet,
} from "../src/surface/lattice";

// The view lattice's floor is the store's own level-0 cell (T-438's fix for T-437 F1), not
// docs/16 §6.2's 100 kHz x 128 s.
const LAT: Lattice = { scheme: "view", cells: 256, f0Hz: 6250, t0Ns: 1e9, levelsF: 20, levelsT: 15 };

test("the two ladders are separate, and docs/api.md's own example lands on its own numbers", () => {
  // docs/api.md: level_f 3 -> f_cell_hz 50000, level_t 5 -> t_cell_s 32, f_index 139 -> 1779200000 Hz.
  assert.equal(fCellHz(LAT, 3), 50_000);
  assert.equal(tCellNs(LAT, 5) / 1e9, 32);
  assert.equal(fTileHz(LAT, 3), 12_800_000);
  const e = extentOf(LAT, { device: "any", scheme: "view", levelF: 3, levelT: 5, fIndex: 139, tIndex: 0, cells: 256 });
  assert.equal(e.f0Hz, 1_779_200_000);
  assert.equal(e.f1Hz, 1_792_000_000);
});

test("level_f and level_t are independent: coarsening one moves only its own axis", () => {
  const box = { f0Hz: 100e6, f1Hz: 102e6, t0Ns: 1_000e9, t1Ns: 1_060e9 };
  const a = tilesFor(LAT, box, 4, 6);
  const b = tilesFor(LAT, box, 7, 6); // frequency 8x coarser, time untouched
  assert.equal(tCellNs(LAT, 6), tCellNs(LAT, 6));
  // The time cell is bit-identical, and so is the set of time indices covered.
  const tIdx = (xs: typeof a) => [...new Set(xs.map((x) => x.tIndex))].sort();
  assert.deepEqual(tIdx(a), tIdx(b), "a frequency level change moved the time axis: the axes are welded again");
  assert.notDeepEqual([...new Set(a.map((x) => x.fIndex))], [...new Set(b.map((x) => x.fIndex))]);
});

test("a pane resolves its two levels from its two pixel densities, separately", () => {
  // 100 MHz across 800 px = 125 kHz/px -> the finest level whose cell is at least a pixel.
  const span = 100e6;
  const box = { f0Hz: 100e6, f1Hz: 100e6 + span, t0Ns: 0, t1Ns: 6000e9 };
  const { levelF, levelT } = levelsFor(LAT, box, 800, 600);
  assert.equal(levelF, levelForHzPerPx(LAT, span / 800));
  assert.equal(levelT, levelForNsPerPx(LAT, 6000e9 / 600));
  assert.ok(levelF > 0 && levelT > 0);
  assert.ok(fCellHz(LAT, levelF) >= span / 800, "a cell finer than a pixel asks for tiles nothing can show");
  assert.ok(fCellHz(LAT, levelF - 1) < span / 800, "…and a coarser level than needed throws away detail that exists");
  // Widening the pane in x alone must not move the time level.
  assert.equal(levelsFor(LAT, box, 1600, 600).levelT, levelT);
  assert.equal(levelsFor(LAT, box, 1600, 600).levelF, levelF - 1);
  // …nor may a taller pane move the frequency level.
  assert.equal(levelsFor(LAT, box, 800, 1200).levelF, levelF);
  assert.equal(levelsFor(LAT, box, 800, 1200).levelT, levelT - 1);
});

test("a level off the end of an axis clamps into the lattice rather than addressing a 404", () => {
  assert.equal(levelForHzPerPx(LAT, 1e12), LAT.levelsF - 1);
  assert.equal(levelForNsPerPx(LAT, 1e21), LAT.levelsT - 1);
  assert.equal(levelForHzPerPx(LAT, 0), 0);
  assert.equal(levelForNsPerPx(LAT, -5), 0);
});

test("tilesFor covers the box, drops negative indices, and orders centre-outwards in frequency", () => {
  const tw = tCellNs(LAT, 0) * 256;
  const box = { f0Hz: 0, f1Hz: fTileHz(LAT, 0) * 3, t0Ns: -tw, t1Ns: tw * 2 };
  const ts = tilesFor(LAT, box, 0, 0);
  assert.ok(ts.every((t) => t.fIndex >= 0 && t.tIndex >= 0), "a negative index would address a different tile after clamping");
  assert.deepEqual([...new Set(ts.map((t) => t.fIndex))].sort((a, b) => a - b), [0, 1, 2]);
  // Least-wanted first, because the queue is LIFO: the LAST tile emitted is the first fetched, and
  // it must be the centre of the newest row.
  const last = ts[ts.length - 1];
  assert.equal(last.tIndex, 1, "the newest time row should be fetched first");
  assert.equal(last.fIndex, 1, "the centre column should be fetched first");
  assert.equal(ts[0].tIndex, 0);
});

test("an ancestor halves the index on each axis it coarsens, and only on that axis", () => {
  const a = { device: "any", scheme: "view", levelF: 2, levelT: 3, fIndex: 7, tIndex: 9, cells: 256 };
  assert.deepEqual(ancestor(LAT, a, 1, 0), { ...a, levelF: 3, fIndex: 3 });
  assert.deepEqual(ancestor(LAT, a, 0, 2), { ...a, levelT: 5, tIndex: 2 });
  const parent = ancestor(LAT, a, 1, 1);
  const pe = extentOf(LAT, parent), ae = extentOf(LAT, a);
  assert.ok(pe.f0Hz <= ae.f0Hz && pe.f1Hz >= ae.f1Hz && pe.t0Ns <= ae.t0Ns && pe.t1Ns >= ae.t1Ns,
    "a fallback must CONTAIN the tile it stands in for, or it is showing a different place");
  const anc = ancestorsOf(LAT, a, 2);
  assert.ok(anc.length > 0);
  for (let i = 1; i < anc.length; i++) {
    const d = (x: typeof a) => x.levelF - a.levelF + (x.levelT - a.levelT);
    assert.ok(d(anc[i]) >= d(anc[i - 1]), "nearest-resolution fallback must be tried first");
  }
});

test("ancestorsOf never leaves the lattice", () => {
  const top = { device: "any", scheme: "view", levelF: LAT.levelsF - 1, levelT: LAT.levelsT - 1, fIndex: 0, tIndex: 0, cells: 256 };
  assert.deepEqual(ancestorsOf(LAT, top, 4), []);
});

test("the key carries device, scheme and cells — the parts §8.3 left out", () => {
  const a = { device: "any", scheme: "view", levelF: 1, levelT: 2, fIndex: 3, tIndex: 4, cells: 256 };
  assert.notEqual(keyOf(a), keyOf({ ...a, device: "hackrf:0001" }));
  assert.notEqual(keyOf(a), keyOf({ ...a, scheme: "1" }));
  assert.notEqual(keyOf(a), keyOf({ ...a, cells: 64 }));
});

test("the request the client builds is the route's own spelling", () => {
  assert.equal(
    tileUrl({ device: "any", scheme: "view", levelF: 3, levelT: 5, fIndex: 139, tIndex: 218427, cells: 256 }),
    "/api/tiles?level_f=3&level_t=5&f_index=139&t_index=218427",
  );
  assert.equal(
    tileUrl({ device: "hackrf:abc", scheme: "1", levelF: 0, levelT: 0, fIndex: 0, tIndex: 0, cells: 64 }),
    "/api/tiles?level_f=0&level_t=0&f_index=0&t_index=0&scheme=1&device=hackrf%3Aabc&cells=64",
  );
});

test("intersects is what cancels a tile for a viewport the user has left", () => {
  const a = { device: "any", scheme: "view", levelF: 0, levelT: 0, fIndex: 10, tIndex: 3, cells: 256 };
  const e = extentOf(LAT, a);
  assert.ok(intersects(LAT, a, e));
  assert.ok(!intersects(LAT, a, { ...e, f0Hz: e.f1Hz, f1Hz: e.f1Hz * 2 }), "touching at the edge is not overlapping");
  assert.ok(!intersects(LAT, a, { ...e, t0Ns: e.t1Ns, t1Ns: e.t1Ns + 1 }));
});

test("the lattice is read off a response, never chosen by the client", () => {
  const lat = latticeFrom({
    key: { scheme: "view", level_f: 3, level_t: 5, cells: 256 },
    axes: { frequency: { levels: 20, cell_hz: 50_000 }, time: { levels: 15, cell_s: 32 } },
  });
  assert.equal(lat.f0Hz, 6250);
  assert.equal(lat.t0Ns, 1e9, "node (0,0) is the open pyramid's own level-0 cell (T-438's F1 fix)");
  assert.equal(lat.levelsF, 20);
  assert.equal(lat.levelsT, 15);
});

// T-501: THE GUARD THAT WOULD HAVE CAUGHT T-484.
//
// `just gate`'s one blind spot is a client that asks the backend for the WRONG THING, and the
// wrong thing here is not a wrong URL — it is a right URL asked ten thousand times. `tilesFor` has
// no budget: `Surface.render` walks every address it returns, every frame, and `TileCache` queues
// every miss behind a four-slot in-flight cap. So the number of addresses a viewport enumerates is
// a **product property**, and it is the number that went dark.
//
// **The ceiling is a bound on level INDEX, and it does not move with the floor.** Measured on both
// geometries with `hk_api::tiles::readable_ceiling` (T-501, a depth sweep over
// `f_levels x t_levels` in 2..=10): the answer is identical at a 6250 Hz x 1 s floor and at a
// 585.9375 Hz x 40.106667 ms one — 4x4 declares (9, 1) for both — and the best `level_f + level_t`
// anywhere in that sweep is **11**, past which (F + T >= 13) the ceiling collapses to (0, 0). A
// floor N doublings finer therefore shrinks the coarsest ADDRESSABLE tile by exactly 2^N, and no
// lattice depth gives it back.
//
// The numbers below are that statement in tiles.
const surfaceViewports = (lat: Lattice, spanS: number) => {
  const nowNs = 1_789_300_000e9;
  const t0Ns = nowNs - spanS * 1e9;
  const n = (box: { f0Hz: number; f1Hz: number; t0Ns: number; t1Ns: number }, w: number, h: number) => {
    const { levelF, levelT } = levelsFor(lat, box, w, h);
    return tilesFor(lat, box, levelF, levelT).length;
  };
  return {
    // The tuned pane, opened on the observed extent (`surfaceBounds`), 1600 x 800.
    pane: n(PANE(t0Ns, nowNs), 1600, 800),
    // The minimap: the whole device range over the whole record horizon, a 1600 x 120 strip.
    minimap: n(MINIMAP(t0Ns, nowNs), 1600, 120),
  };
};

const NOW_NS = 1_789_300_000e9;
const PANE = (t0Ns: number, t1Ns: number) => ({ f0Hz: 99.6e6, f1Hz: 102.0e6, t0Ns, t1Ns });
const MINIMAP = (t0Ns: number, t1Ns: number) => ({ f0Hz: 0, f1Hz: 6e9, t0Ns, t1Ns });

/** The shipped view lattice: node (0, 0) 6250 Hz x 1 s, ceiling (9, 1). */
const SHIPPED: Lattice = { scheme: "view", cells: 256, f0Hz: 6250, t0Ns: 1e9, levelsF: 12, levelsT: 15, maxLevelF: 9, maxLevelT: 1 };
/** T-484's floor: the display STFT's own bin and row at 2.4 Msps, at the SAME declared ceiling. */
const T484: Lattice = { ...SHIPPED, f0Hz: 585.9375, t0Ns: 40_106_667, levelsF: 16, levelsT: 19 };
/**
 * **T-505's overview tier**: the same de-welded construction anchored on the SPECTRUM-HISTORY
 * pyramid, which is a different store and therefore a different ceiling. Measured on real pyramids
 * by `hk_api::tiles::the_overview_tier_reaches_past_the_whole_surface_whatever_the_view_floor_is`:
 * floor 6250 Hz x 1 s, ceiling **(11, 14)** — a coarsest addressable tile of 3276.8 MHz x 48.5
 * days, against the view pyramid's 819.2 MHz x 512 s and T-484's 76.8 MHz x 20.5 s.
 *
 * **It is the same numbers whichever floor the view pyramid has**, which is the entire point: it is
 * anchored on a store whose cells do not move when the display's do.
 */
const OVERVIEW: Lattice = { scheme: "overview", cells: 256, f0Hz: 6250, t0Ns: 1e9, levelsF: 20, levelsT: 20, maxLevelF: 11, maxLevelT: 14 };

test("a viewport must be drawable in a bounded number of tiles, at every horizon", () => {
  // 100 tiles is generous: it is ~3x what the shipped floor needs for the deepest viewport the
  // surface can open, and well inside a 96 MB LRU and a four-slot in-flight cap.
  const BUDGET = VIEWPORT_TILE_BUDGET;
  assert.equal(BUDGET, 100);
  for (const spanS of [20, 60, 300, 1200, 1800]) {
    const v = surfaceViewports(SHIPPED, spanS);
    assert.ok(v.pane <= BUDGET, `pane at ${spanS}s asks for ${v.pane} tiles`);
    assert.ok(v.minimap <= BUDGET, `minimap at ${spanS}s asks for ${v.minimap} tiles`);
  }
});

test("T-484's floor blows that budget by two orders of magnitude, and this is why the map went dark", () => {
  // Non-vacuity, and the measurement itself. Exact counts, so neither the defect nor a repair of it
  // can move unnoticed. The pane is survivable at a live edge and hopeless once the surface opens
  // on twenty minutes of observed extent; the minimap is hopeless from ~100 s of capture onward,
  // which is why every suite that runs against a seconds-old server stayed green.
  assert.deepEqual(surfaceViewports(T484, 20), { pane: 15, minimap: 158 });
  assert.deepEqual(surfaceViewports(T484, 100), { pane: 30, minimap: 474 });
  assert.deepEqual(surfaceViewports(T484, 1200), { pane: 300, minimap: 4740 });
  assert.deepEqual(surfaceViewports(T484, 1800), { pane: 445, minimap: 7031 });
  // The same viewports on the shipped floor, for the ratio.
  assert.deepEqual(surfaceViewports(SHIPPED, 1800), { pane: 8, minimap: 32 });
});

// ——— T-505: the tiers, measured. ———
//
// **The harness gap this section exists to close.** The surface opens on the OBSERVED EXTENT, so a
// server that has existed for seconds opens on a seconds-wide window and asks for a handful of
// tiles — and every suite runs against exactly such a server. The defect is a function of ELAPSED
// CAPTURE TIME, which no suite varies. So every count below is taken at an explicit horizon, thirty
// minutes included, against a faked observed extent rather than a real server's age.

/** Every tier a viewport could be drawn from, and what each would cost. */
const tiers = (set: LatticeSet, spanS: number) => {
  const t0Ns = NOW_NS - spanS * 1e9;
  const pane = tierFor(set, PANE(t0Ns, NOW_NS), 1600, 800);
  const minimap = tierFor(set, MINIMAP(t0Ns, NOW_NS), 1600, 120);
  return {
    pane: { tier: pane.tier, tiles: pane.addrs.length },
    minimap: { tier: minimap.tier, tiles: minimap.addrs.length },
  };
};

test("T-505: THE BUDGET HOLDS AT A THIRTY-MINUTE HORIZON, at the fidelity floor that broke it", () => {
  // The fidelity floor T-501 needs back, with the overview tier in place. Every count is a count
  // for the SAME viewports the test above measures at 445 and 7031.
  const set: LatticeSet = { detail: T484, overview: OVERVIEW };
  for (const spanS of [20, 60, 100, 300, 1200, 1800]) {
    const v = tiers(set, spanS);
    assert.ok(v.pane.tiles <= VIEWPORT_TILE_BUDGET, `pane at ${spanS}s: ${v.pane.tiles} tiles from the ${v.pane.tier} tier`);
    assert.ok(v.minimap.tiles <= VIEWPORT_TILE_BUDGET, `minimap at ${spanS}s: ${v.minimap.tiles} tiles from the ${v.minimap.tier} tier`);
  }
  // The exact numbers, and WHICH TIER each came from — the measurement this ticket was filed for.
  // 7031 -> 4 for the minimap, 445 -> 6 for a half-hour scrub of the tuned band.
  assert.deepEqual(tiers(set, 1800), {
    pane: { tier: "overview", tiles: 6 },
    minimap: { tier: "overview", tiles: 4 },
  });
  assert.deepEqual(tiers(set, 1200), {
    pane: { tier: "overview", tiles: 6 },
    minimap: { tier: "overview", tiles: 4 },
  });
  // **And the live/tuned window stays on the DETAIL tier**, which is the whole reason the fidelity
  // floor exists: what T-483 measured as missing is delivered exactly where it is looked at.
  assert.deepEqual(tiers(set, 20), {
    pane: { tier: "detail", tiles: 15 },
    // The MINIMAP leaves the detail tier even at twenty seconds, because 6 GHz is past that
    // lattice's frequency ceiling whatever the horizon: 158 addresses against 4.
    minimap: { tier: "overview", tiles: 4 },
  });
  assert.deepEqual(tiers(set, 100).pane, { tier: "detail", tiles: 30 });
  assert.deepEqual(tiers(set, 300).pane, { tier: "detail", tiles: 80 },
    "five minutes of the tuned band is still inside the budget on the detail tier");
});

test("T-505: on the SHIPPED floor the tiers change nothing — the prerequisite lands before the change that needs it", () => {
  const set: LatticeSet = { detail: SHIPPED, overview: OVERVIEW };
  for (const spanS of [20, 100, 1200, 1800]) {
    const v = tiers(set, spanS);
    assert.equal(v.pane.tier, "detail", `pane at ${spanS}s left the detail tier on the shipped floor`);
    assert.equal(v.minimap.tier, "detail", `minimap at ${spanS}s left the detail tier on the shipped floor`);
  }
  // Byte-for-byte the counts the shipped floor already produced: no viewport moved.
  assert.deepEqual(tiers(set, 1800), {
    pane: { tier: "detail", tiles: 8 },
    minimap: { tier: "detail", tiles: 32 },
  });
});

test("T-505: a coverage-first minimap alone is NOT enough — measured, which is why the tier exists", () => {
  // **Shape (1) of the ticket, evaluated before shape (2) was built.** `/api/coverage` first, and
  // request tiles only where something was observed: on a device-wide viewport almost all of
  // 1 MHz - 6 GHz has never been sampled, so it is a large constant factor.
  //
  // It is a FACTOR, NOT A BOUND, and the two viewports show why. The observed region is modelled
  // as the tuned band over the whole horizon — what the user's demo actually was.
  const observed = (box: { f0Hz: number; f1Hz: number; t0Ns: number; t1Ns: number }, lat: Lattice, w: number, h: number, band: { f0Hz: number; f1Hz: number }) => {
    const { levelF, levelT } = levelsFor(lat, box, w, h);
    return tilesFor(lat, box, levelF, levelT)
      .filter((a) => { const e = extentOf(lat, a); return e.f1Hz > band.f0Hz && e.f0Hz < band.f1Hz; })
      .length;
  };
  const band = { f0Hz: 99.6e6, f1Hz: 102.0e6 };
  const t0Ns = NOW_NS - 1800e9;
  // The minimap: 7031 addresses, of which 89 intersect anything the radio ever sampled. A 79x
  // saving — and still short of the budget, on its own.
  assert.equal(observed(MINIMAP(t0Ns, NOW_NS), T484, 1600, 120, band), 89);
  assert.ok(89 > VIEWPORT_TILE_BUDGET * 0.8, "close enough to the budget to fail on the next band tuned");
  // The tuned pane is the case it cannot touch: the surface OPENS on the observed extent, so
  // almost every address it enumerates is inside the observed band by construction. 445 -> 356 is
  // one column of tiles at the band's edge, and still THREE AND A HALF TIMES the budget.
  assert.equal(surfaceViewports(T484, 1800).pane, 445);
  assert.equal(observed(PANE(t0Ns, NOW_NS), T484, 1600, 800, band), 356);
  assert.ok(356 > VIEWPORT_TILE_BUDGET * 3, "coverage-first leaves the tuned-window viewport hopeless");
});

test("T-505: the tier is chosen by the BUDGET, and the choice is total", () => {
  const set: LatticeSet = { detail: T484, overview: OVERVIEW };
  const t0Ns = NOW_NS - 1800e9;
  const chosen = tierFor(set, MINIMAP(t0Ns, NOW_NS), 1600, 120);
  assert.equal(chosen.tier, "overview");
  assert.equal(chosen.lat.scheme, "overview", "the address carries the scheme, so the two tiers cannot share a cache key");
  assert.equal(chosen.addrs[0].scheme, "overview");
  assert.equal(chosen.clamped, false, "the overview tier answers this window at the level the screen asks for");
  // The detail tier's own verdict for the same viewport: clamped, and 7031 addresses.
  const dense = tierFor({ detail: T484, overview: T484 }, MINIMAP(t0Ns, NOW_NS), 1600, 120);
  assert.equal(dense.tier, "detail");
  assert.equal(dense.clamped, true, "it is asked for a level past its ceiling — the honest reason to leave it");
  assert.equal(dense.addrs.length, 7031);
  // Total: with no better tier available the cheaper one is still returned rather than nothing.
  assert.ok(dense.addrs.length > VIEWPORT_TILE_BUDGET);
});
