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
  levelForHzPerPx, levelForNsPerPx, levelsFor, tCellNs, tilesFor, tileUrl, type Lattice,
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
