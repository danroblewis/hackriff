// **T-480: no address outside the lattice can be produced.**
//
// The defect was a canvas asking `/api/tiles` for a node it had no reason to believe in — seen in
// the wild as `level_f=10, t_index=218471` — and the route answering `4xx` while the console filled
// up (T-479 stops the flood; this is the cause). It is CLAUDE.md's discretized-navigation invariant
// broken in the client: *"zoom/pan and region-select resolve only to realizable configurations and
// snap to the nearest one."*
//
// ——— WHAT THE EVIDENCE BELOW IS A PROPERTY **OF** ———
//
// This repo keeps shipping sound proofs of the adjacent question (T-454's counter measured a pump,
// T-448's gate guaranteed every counter except the asserted one, T-441 verified a shader for a
// module that could not load). The trap here is specific and worth naming: **an address being
// clamped is not the same as no bad address being produced.** Asserting `clampAddr(a)` is in the
// lattice proves a fact about `clampAddr`; it says nothing about whether the *derivation* ever
// hands `clampAddr` the address in the first place.
//
// So the quantification is over **the inputs, not the outputs**: a grid across the whole zoom/pan
// range — every pane size, every centre, every span from the zoom floor to the whole surface, on
// both axes — is pushed through the derivation the renderer actually uses (`levelsFor` → `tilesFor`,
// plus the parent row `surface.ts` pins at `levelF + 1` and every fallback `ancestorsOf` offers),
// and **every address that comes out** is checked against `inLattice`, which is written from the
// lattice's declared axes and not from the clamp. A clamp that agreed with itself would fail this.
//
// Non-vacuity is asserted in the file rather than claimed in a comment: the sweep records how many
// grid points DEMAND a level past the ceiling (`levelDemandF`/`levelDemandT` — what the zoom asks
// for before clamping), and the test fails if that count is small. A range that never reaches the
// bound would pass every assertion here while proving nothing.
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  ADDRESSABLE_HZ, ADDRESSABLE_NS, ancestor, ancestorsOf, clampAddr, inLattice, levelCapF, levelCapT,
  levelDemandF, levelDemandT, levelsFor, maxFIndex, maxTIndex, tilesFor,
  type Box, type Lattice, type TileAddr,
} from "../src/surface/lattice";
import { PaneModel } from "../src/surface/panes";

/**
 * The lattice the shipped server actually serves, measured rather than invented: `hk serve` over
 * the e2e fixture answers `axes.frequency = {cell_hz: 6250, levels: 12}` and
 * `axes.time = {cell_s: 1, levels: 15}` at `cells = 256` (T-480, `GET /api/tiles`).
 */
const LAT: Lattice = { scheme: "view", cells: 256, f0Hz: 6250, t0Ns: 1e9, levelsF: 12, levelsT: 15 };

/**
 * The surface's extent with no front end and no surviving tune record reported — `surfaceBounds`'s
 * own two fallbacks: the view lattice's whole frequency axis, and one tile of the coarsest time
 * level (~48 days). Deliberately the widest surface the client can open on, because that is the
 * corner the wild defect came from: the whole device range across a 1280 px pane resolves to
 * `level_f = 10`, which is exactly what the user's console reported.
 */
const NOW_NS = 1_789_297_834_000_000_000;
const BOUNDS: Box = {
  f0Hz: 0,
  f1Hz: LAT.f0Hz * 2 ** (LAT.levelsF - 1) * LAT.cells * 2,
  t0Ns: NOW_NS - LAT.t0Ns * 2 ** (LAT.levelsT - 1) * LAT.cells,
  t1Ns: NOW_NS,
};

/** Pane sizes across the range a browser window spans, including the awkward small ones. */
const SIZES = [[320, 240], [800, 600], [1280, 720], [1920, 1080], [2560, 1440], [3840, 2160]] as const;

/**
 * Every address the renderer would derive for one pane box at one viewport size — the same three
 * calls `Surface.render` makes, in the same order.
 *
 * `surface.ts` is not imported (it is another ticket's file this week, and it needs a GL context);
 * the calls it makes are reproduced, which is the part under test. The `levelF + 1` parent row is
 * included because that is precisely the kind of derived-somewhere-else arithmetic a fetch-time
 * guard would have hidden.
 */
function addressesFor(lat: Lattice, box: Box, wPx: number, hPx: number): TileAddr[] {
  const { levelF, levelT } = levelsFor(lat, box, wPx, hPx);
  const out = [...tilesFor(lat, box, levelF, levelT)];
  out.push(...tilesFor(lat, box, levelF + 1, levelT + 1)); // the pinned parent row
  for (const a of out.slice()) out.push(...ancestorsOf(lat, a, 4));
  return out;
}

test("no address outside the lattice can be produced, over the whole zoom/pan range", () => {
  const fullF = BOUNDS.f1Hz - BOUNDS.f0Hz, fullT = BOUNDS.t1Ns - BOUNDS.t0Ns;
  let produced = 0, demandedPastCapF = 0, demandedPastCapT = 0, points = 0;
  const offenders: string[] = [];

  for (const [wPx, hPx] of SIZES) {
    // Spans from the zoom floor to the whole surface, doubling — the exact ladder a wheel walks.
    for (let fz = 0; fz <= 24; fz++) {
      const spanHz = Math.min(fullF, LAT.f0Hz * 16 * 2 ** fz);
      for (let tz = 0; tz <= 24; tz++) {
        const spanNs = Math.min(fullT, LAT.t0Ns * 16 * 2 ** tz);
        // Pans across the whole axis at that zoom, including both hard edges.
        for (const fp of [0, 0.5, 1]) {
          for (const tp of [0, 0.5, 1]) {
            points++;
            const centerHz = BOUNDS.f0Hz + spanHz / 2 + fp * (fullF - spanHz);
            const centerNs = BOUNDS.t0Ns + spanNs / 2 + tp * (fullT - spanNs);
            const box: Box = {
              f0Hz: centerHz - spanHz / 2, f1Hz: centerHz + spanHz / 2,
              t0Ns: centerNs - spanNs / 2, t1Ns: centerNs + spanNs / 2,
            };
            if (levelDemandF(LAT, spanHz / wPx) > levelCapF(LAT)) demandedPastCapF++;
            if (levelDemandT(LAT, spanNs / hPx) > levelCapT(LAT)) demandedPastCapT++;
            for (const a of addressesFor(LAT, box, wPx, hPx)) {
              produced++;
              if (!inLattice(LAT, a) && offenders.length < 5) {
                offenders.push(`${wPx}x${hPx} span ${spanHz} Hz / ${spanNs} ns -> ${JSON.stringify(a)}`);
              }
            }
          }
        }
      }
    }
  }

  assert.deepEqual(offenders, [], "an address off the lattice is the discretized-navigation invariant broken in the client");
  // Non-vacuity, in the file: a sweep that never reached the ceiling would pass the line above and
  // prove nothing about the clamp.
  assert.ok(produced > 100_000, `the sweep must actually produce addresses (got ${produced} over ${points} points)`);
  assert.ok(demandedPastCapF > 100, `the sweep must contain zooms that ASK for a level past the frequency ceiling (got ${demandedPastCapF})`);
  assert.ok(demandedPastCapT > 100, `…and past the time ceiling (got ${demandedPastCapT})`);
});

test("the same property through PaneModel, which is what a real gesture moves", () => {
  // The sweep above drives boxes directly. This one drives the object the wheel and the drag drive,
  // so a clamp that only held for hand-made boxes would not survive it.
  const model = new PaneModel({ bounds: BOUNDS, lattice: LAT, width: 1280, height: 720 });
  const id = model.list()[0].id;
  let produced = 0;
  const offenders: TileAddr[] = [];
  const check = () => {
    for (const v of model.views(NOW_NS, 1280, 720)) {
      for (const a of addressesFor(LAT, v.box, v.rect.w, v.rect.h)) {
        produced++;
        if (!inLattice(LAT, a)) offenders.push(a);
      }
    }
  };
  // Zoom all the way out on both axes, one wheel notch at a time, then all the way back in, panning
  // to each edge as we go. 40 notches at 1.25x is ~8600x — well past any bound either axis has.
  for (const factor of [1.25, 0.8]) {
    for (let i = 0; i < 40; i++) {
      model.zoomFreq(id, factor, i % 3 === 0 ? 0 : i % 3 === 1 ? 1 : 0.5);
      model.zoomTime(id, factor, i % 2);
      check();
      model.panFreq(id, i % 2 ? 1e12 : -1e12);
      model.panTime(id, i % 2 ? 1e15 : -1e15);
      check();
    }
  }
  assert.deepEqual(offenders.slice(0, 3), [], "a gesture must not be able to address a node that does not exist");
  assert.ok(produced > 10_000, `the gesture sweep must produce addresses (got ${produced})`);
});

test("the clamp is per axis: clamping one level never moves the other", () => {
  // T-434 de-welded the axes in the store, T-438 on the wire, T-440 per pane. A clamp that coupled
  // them would undo a milestone, so the independence is asserted at the clamp itself.
  const a: TileAddr = { device: "any", scheme: "view", levelF: 99, levelT: 3, fIndex: 7, tIndex: 9, cells: 256 };
  const c = clampAddr(LAT, a);
  assert.equal(c.levelF, levelCapF(LAT));
  assert.equal(c.levelT, 3, "the time level was in range and must be untouched");
  assert.equal(clampAddr(LAT, { ...a, levelF: 2, levelT: 99 }).levelF, 2, "…and symmetrically");
  assert.equal(clampAddr(LAT, { ...a, levelF: 2, levelT: 99 }).levelT, levelCapT(LAT));
  // A ceiling on one axis does not pull the other down with it.
  const shallowF: Lattice = { ...LAT, maxLevelF: 4 };
  assert.equal(levelCapF(shallowF), 4);
  assert.equal(levelCapT(shallowF), LAT.levelsT - 1, "a frequency ceiling is not a time ceiling");
});

test("a stated `max_level` is the ceiling, and it is honoured by every derivation", () => {
  // The route may declare that it cannot READ as far up an axis as it can NAME (a store ladder
  // shallower than the address lattice). When it does, the ceiling is the smaller number, and the
  // whole derivation must obey it — not the fetch.
  const capped: Lattice = { ...LAT, maxLevelF: 9, maxLevelT: 6 };
  const box: Box = { f0Hz: 0, f1Hz: BOUNDS.f1Hz, t0Ns: NOW_NS - 30 * 86_400e9, t1Ns: NOW_NS };
  const { levelF, levelT } = levelsFor(capped, box, 1280, 720);
  assert.ok(levelDemandF(capped, (box.f1Hz - box.f0Hz) / 1280) > 9, "this zoom must genuinely ask for more than the ceiling");
  assert.equal(levelF, 9);
  assert.equal(levelT, 6);
  for (const a of addressesFor(capped, box, 1280, 720)) assert.ok(inLattice(capped, a), JSON.stringify(a));
  // …including the single-address fallback path.
  const top: TileAddr = { device: "any", scheme: "view", levelF: 9, levelT: 6, fIndex: 0, tIndex: 0, cells: 256 };
  assert.deepEqual(ancestorsOf(capped, top, 4), [], "there is nothing above the ceiling to fall back to");
  assert.equal(ancestor(capped, top, 3, 0).levelF, 9, "a coarsening request past the ceiling stops at it");
});

test("an index past the end of its axis is DROPPED, not clamped — a different index is a different place", () => {
  // The route refuses `f_index` whose tile leaves the addressable spectrum, and `t_index` whose tile
  // leaves the addressable time range. Clamping either would quietly serve a different place, so
  // `tilesFor` drops them; `clampAddr` exists for the single-address paths, where the caller has
  // already said which place it means.
  const lvl = 0;
  const fw = LAT.f0Hz * 2 ** lvl * LAT.cells, tw = LAT.t0Ns * 2 ** lvl * LAT.cells;
  assert.equal(maxFIndex(LAT, lvl), Math.floor(ADDRESSABLE_HZ / fw) - 1);
  assert.equal(maxTIndex(LAT, lvl), Math.floor(ADDRESSABLE_NS / tw) - 1);
  const past = maxFIndex(LAT, lvl);
  const box: Box = { f0Hz: past * fw, f1Hz: (past + 3) * fw, t0Ns: 0, t1Ns: tw };
  const got = tilesFor(LAT, box, lvl, lvl);
  assert.ok(got.length > 0, "the part of the box that IS addressable must still be asked for");
  assert.ok(got.every((a) => a.fIndex <= past), "an unaddressable column is dropped, never folded onto the last one");
  assert.ok(got.every((a) => inLattice(LAT, a)));
});
