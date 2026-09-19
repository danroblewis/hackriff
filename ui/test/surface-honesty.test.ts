// T-440 / T-437 finding **F3**: *an undersized client tile budget makes the surface LIE.*
//
// The spike measured the cliff on real WebGL2 — below `budget ≈ working set`, 101–198 tiles per
// frame rendered **grey** — and grey is this surface's load-bearing claim that the radio never
// looked there. So the whole of this file is one question asked several ways: **can a memory
// pressure event produce a grey pixel?**
//
// The answer is made structural rather than careful, and each test below pins one part of it:
//   - the grey constant occurs exactly ONCE in the shader, inside the branch that tests the cell's
//     *coverage state*, and that state comes from the server;
//   - a pane's ground is cleared to PENDING, never grey (the spike cleared panes to grey — that one
//     line IS F3);
//   - a missing tile is drawn as an upscaled ancestor, *marked*, or as PENDING — a different draw
//     with a different uniform, never a cell state;
//   - and running the same panes with the budget far below the working set changes the count of
//     pending draws and nothing about grey.

import { test } from "node:test";
import assert from "node:assert/strict";
import { BACKDROP, CELL, CELL_MARKS, CELL_RULE_GLSL, GREY, PENDING, REFUSED_MARK, SHADOW_MARK, cellPixel, markFor, patternHit, type Rgb } from "../src/surface/cellrule";
import { cmap } from "../src/cmap";
import { keyOf, type Lattice, type TileAddr } from "../src/surface/lattice";
import { Surface, type PaneView } from "../src/surface/surface";
import { TileCache } from "../src/surface/tilecache";
import type { TileData } from "../src/surface/tile";
import { stubGl } from "./surface-glstub";

const LAT: Lattice = { scheme: "view", cells: 256, f0Hz: 6250, t0Ns: 1e9, levelsF: 20, levelsT: 15 };
const TILE_HZ = LAT.f0Hz * LAT.cells;   // 1.6 MHz
const TILE_NS = LAT.t0Ns * LAT.cells;   // 256 s
const BYTES = 192 * 1024;
const W = 800, H = 600;

/** Every tile carries one unobserved cell and one unknown cell, at the same place, always. So any
 * change in what is drawn grey across the runs below would have to come from the renderer. */
function data(a: TileAddr): TileData {
  return {
    addr: a, key: keyOf(a), nf: 2, nt: 2,
    value: new Float32Array([-90, NaN, -70, NaN]),
    state: new Uint8Array([CELL.OBSERVED, CELL.UNOBSERVED, CELL.OBSERVED, CELL.UNKNOWN]),
    tier: "spectrum-history", answeredLevel: 1, fold: { frequency: "exact", time: "exact" },
    measured: { nf: 2, nt: 2 },
    rangeDb: { lo: -100, hi: -60 }, bytes: BYTES, serverInFlightLimit: null,
  };
}

const flush = () => new Promise((r) => setImmediate(r));

function harness(budgetBytes: number) {
  const g = stubGl(W, H);
  let fetches = 0;
  const surface = new Surface(
    g.canvas, LAT,
    (tex) => new TileCache(tex, (a) => { fetches++; return Promise.resolve(data(a)); }, { budgetBytes, inFlight: 64, now: () => 0 }),
    { pinParents: false },
  );
  surface.setScale(-100, -60);
  return { g, surface, fetches: () => fetches };
}

const pane = (id: string, f0: number, tiles = 3): PaneView => ({
  id, rect: { x: 0, y: 0, w: W, h: H },
  box: { f0Hz: f0, f1Hz: f0 + tiles * TILE_HZ, t0Ns: 0, t1Ns: TILE_NS },
});

const near = (a: readonly number[], b: readonly number[]) => a.length >= b.length && b.every((v, i) => Math.abs(a[i] - v) < 1e-6);

test("the grey constant lives in exactly one place in the shader: the coverage-state branch", () => {
  const g = stubGl();
  new Surface(g.canvas, LAT, (tex) => new TileCache(tex, () => Promise.reject(new Error("unused")), {}));
  const fs = g.shaders.find((s) => s.includes("cellMark"))!;
  const lit = `vec3(${GREY.join(",")})`;
  assert.equal(fs.split(lit).length - 1, 1, `the grey must appear exactly once in the fragment shader (found in: ${fs})`);
  // ...and that once is inside the rule generated from CELL_MARKS, on the unobserved branch.
  assert.ok(CELL_RULE_GLSL.includes(`if (s == ${CELL.UNOBSERVED}) return ${lit};`));
  assert.equal(CELL_RULE_GLSL.split(lit).length - 1, 1);
  // Neither of the two marks that are NOT claims about the radio may be the grey.
  assert.notDeepEqual([...PENDING], [...GREY]);
  assert.notDeepEqual([...BACKDROP], [...GREY]);
});

test("the rule in TypeScript and the rule in GLSL are the same rule, because one generates the other", () => {
  assert.deepEqual(markFor(CELL.UNOBSERVED), { kind: "flat", rgb: GREY });
  assert.deepEqual(markFor(CELL.OBSERVED), { kind: "ramp" });
  // SIX states, six marks (T-413/T-441/T-520): any two of them drawn alike is the collapse this
  // refuses — "looked and it was quiet" spelled as "never looked", in either direction.
  const marks = CELL_MARKS.map((m) => JSON.stringify(m));
  assert.equal(new Set(marks).size, CELL_MARKS.length, `two cell states share a mark: ${marks}`);
  assert.equal(CELL_MARKS.length, Object.keys(CELL).length, "a state with no mark falls through to grey");
  // …and no mark that is not the unobserved one may use the grey, in ANY of its parts.
  for (const [s, m] of CELL_MARKS.entries()) {
    if (s === CELL.UNOBSERVED) continue;
    for (const c of m.kind === "flat" ? [m.rgb] : m.kind === "pattern" ? [m.rgb, m.ink] : m.kind === "shadow" ? [m.ink] : []) {
      assert.notDeepEqual([...c], [...GREY], `state ${s} draws THE grey`);
    }
  }
});

// ——— T-520: the last-known (shadow) tier ———
//
// Swept then departed = shadow; never swept = grey; re-swept = full brightness. The shadow is a real
// measurement drawn where the radio is NOT looking, so it has two ways to lie and each gets a test:
// read as grey (the radio never looked — false, it did) or read as live (the radio is looking — false,
// it left). Defended by a hard brightness ceiling (upper ramp) and a texture no live cell carries
// (lower ramp, where the ceiling cannot help).

const lum = (c: readonly number[]) => 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
const shadowPx = (x: number, px: { x: number; y: number }) =>
  cellPixel({ state: CELL.SHADOW, x, px, tier: 0, srcPx: { x: 1, y: 1 }, fallback: false });

test("the shadow is not THE grey — at any level, on any pixel — and grey is still only the unobserved branch", () => {
  for (let i = 0; i <= 200; i++) {
    for (let y = 0; y < SHADOW_MARK.pitchPx; y++) {
      assert.ok(!near(shadowPx(i / 200, { x: 0.5, y: y + 0.5 }), [...GREY]), `shadow at x=${i / 200} row ${y} is THE grey`);
    }
  }
  assert.notDeepEqual([...SHADOW_MARK.ink], [...GREY]);
  // The generated shader still holds exactly one grey, and the shadow branch is generated from the
  // table — cmap scaled by the table's gain, not hand-written arithmetic.
  assert.equal(CELL_RULE_GLSL.split(`vec3(${GREY.join(",")})`).length - 1, 1, "the shadow branch added a second grey");
  assert.ok(CELL_RULE_GLSL.includes(`cmap(x) * ${SHADOW_MARK.gain}`), "the shadow ground must be the ONE ramp, dimmed");
  assert.ok(CELL_RULE_GLSL.includes(`pat_${SHADOW_MARK.pattern}(px`), "the shadow texture must be a generated pattern");
});

test("the shadow's HARD CEILING: no shadow pixel is as bright as the live ramp from a third of the way up", () => {
  // Every pixel a shadow can produce, at every level.
  let brightest = 0;
  for (let i = 0; i <= 400; i++) {
    for (let y = 0; y < SHADOW_MARK.pitchPx; y++) {
      const c = shadowPx(i / 400, { x: 0.5, y: y + 0.5 });
      assert.ok(Math.max(...c) <= SHADOW_MARK.gain + 1e-9, `a shadow channel exceeds the ceiling at x=${i / 400}`);
      brightest = Math.max(brightest, lum(c));
    }
  }
  assert.ok(brightest <= SHADOW_MARK.gain + 1e-9);
  // The live ramp clears that ceiling from here up, and stays above it.
  const FROM = 0.3;
  for (let i = Math.round(FROM * 400); i <= 400; i++) {
    assert.ok(lum(cmap(i / 400)) > brightest, `live cmap(${i / 400}) is no brighter than the brightest shadow — a remembered signal could read as live`);
  }
  // …and the shadow of a strong signal really is far down from that signal live.
  assert.ok(lum(shadowPx(1, { x: 0.5, y: 3.5 })) < 0.3 * lum(cmap(1)));
});

test("the shadow's TEXTURE: where the ceiling cannot help (a remembered noise floor), the scanlines can", () => {
  // At the bottom of the ramp a live cell and its shadow are both near-black. The scanline ink is on
  // no position of the ramp, so a ruled row can never be a live level…
  for (let i = 0; i <= 200; i++) {
    assert.ok(!near(cmap(i / 200), [...SHADOW_MARK.ink]), `the scanline ink is cmap(${i / 200})`);
  }
  // …it is clearly lighter than a dimmed floor, so the rules are visible exactly there…
  assert.ok(lum(SHADOW_MARK.ink) > 5 * lum(shadowPx(0, { x: 0.5, y: 3.5 })));
  // …and every pitch-high band of a shadow cell carries a rule, running along time only.
  for (let y0 = 0; y0 < 40; y0++) {
    let hits = 0;
    for (let y = y0; y < y0 + SHADOW_MARK.pitchPx; y++) if (patternHit(SHADOW_MARK.pattern, { x: 3.5, y: y + 0.5 }, { x: SHADOW_MARK.pitchPx, y: SHADOW_MARK.pitchPx })) hits++;
    assert.ok(hits >= 1 && hits < SHADOW_MARK.pitchPx, `band at ${y0}: ${hits} ruled rows`);
  }
  for (let x = 0; x < 40; x++) {
    assert.equal(patternHit(SHADOW_MARK.pattern, { x: x + 0.5, y: 0.5 }, { x: SHADOW_MARK.pitchPx, y: SHADOW_MARK.pitchPx }), true, "a rule is horizontal: constant along frequency");
  }
});

test("the shadow is not the unknown hatch, not the refused X, not pending, not any other mark's shape", () => {
  const others = CELL_MARKS.filter((_, s) => s !== CELL.SHADOW);
  for (const m of others) if (m.kind === "pattern") assert.notEqual(m.pattern, SHADOW_MARK.pattern, "the shadow shares a cell mark's shape");
  assert.notEqual(SHADOW_MARK.pattern, REFUSED_MARK.pattern);
  assert.notEqual(SHADOW_MARK.pattern, (CELL_MARKS[CELL.UNKNOWN] as { pattern: string }).pattern);
  for (const c of [REFUSED_MARK.ink, REFUSED_MARK.rgb, PENDING, BACKDROP, (CELL_MARKS[CELL.UNKNOWN] as { ink: Rgb }).ink]) {
    assert.ok(!near([...SHADOW_MARK.ink], [...c]), `the scanline ink is ${c}`);
  }
  // Not magenta, by the e2e's own predicate (magenta is `unknown`'s alone).
  const [r, g, b] = SHADOW_MARK.ink.map((v) => Math.round(v * 255));
  assert.ok(!(r > g + 20 && b > g + 20 && (r + b) / 2 > 60));
  // No honesty-tier wash reaches a shadow: it is not a measurement of this cell, so it has no
  // resolution to qualify — the same pixel whatever tier the tile was answered at.
  for (const tier of [0, 1, 2]) {
    assert.deepEqual(cellPixel({ state: CELL.SHADOW, x: 0.6, px: { x: 1.5, y: 3.5 }, tier, srcPx: { x: 2, y: 2 }, fallback: false }),
      shadowPx(0.6, { x: 1.5, y: 3.5 }));
  }
});

// ——— T-499: the mark for "asked, and no usable answer came back" ———
//
// The ticket's premise was that the user's purple is an out-of-range number reaching the ramp. It is
// not, and the measurement says what it is instead: `CELL_MARKS[CELL.UNKNOWN]`'s ink, rgb(112,77,133),
// drawn from a perfectly well-formed 200 whose `coverage` said every cell was `unknown` — which is
// what `hk-api` serves for EVERY row when no record survives anywhere (`unknown_rows` -> `g.nt`).
// Browser measurement, `hk serve` restarted under a live page with a fresh data dir: **54.3 % of the
// pane that ink at +3 s, 20.4 % at +8 s, 9.3 % at +15 s, 0 at +25 s** — decaying exactly as the new
// server re-accumulated records, which is the signature of a coverage answer and not of a ramp.
//
// So the ramp is innocent, and what this file gets from T-499 is the *other* half: the state the
// client itself is in when it cannot get an answer. That was drawn as PENDING — "loading" — which is
// a promise of arrival it cannot keep, and it is one mark away from the grey the whole surface rests
// on. It gets its own mark, in neither vocabulary.

test("the refused mark is neither THE grey, nor on the ramp, nor the `unknown` hatch", () => {
  const parts: Rgb[] = [REFUSED_MARK.rgb, REFUSED_MARK.ink];
  for (const c of parts) {
    assert.notDeepEqual([...c], [...GREY],
      "'I asked and got nothing usable' must never be spelled as 'the radio never looked'");
    // Not anywhere on the measurement ramp, at any position: a refusal is not a level.
    for (let i = 0; i <= 200; i++) {
      assert.ok(!near(cmap(i / 200), [...c]), `the refused mark is cmap(${i / 200}) — it would read as a measurement`);
    }
    // Not magenta, by the e2e's own predicate: the one legitimately magenta mark here is `unknown`,
    // and a second one would make `canvas-journey`'s magenta count unable to tell them apart.
    const [r, g, b] = c.map((v) => Math.round(v * 255));
    assert.ok(!(r > g + 20 && b > g + 20 && (r + b) / 2 > 60),
      `the refused mark is magenta (${r},${g},${b}), which is what the user reported seeing`);
  }
  // …and it is visibly not PENDING either, or "wait" and "nothing is coming" are one mark again.
  assert.notDeepEqual([...REFUSED_MARK.ink], [...PENDING]);
  assert.ok(!CELL_MARKS.some((m) => m.kind === "pattern" && m.pattern === REFUSED_MARK.pattern),
    "no cell state may share the refused mark's shape: they can be on one screen at one time");
});

test("a place the route refused draws the refused mark, and NEVER grey", async () => {
  const g = stubGl(W, H);
  const surface = new Surface(
    g.canvas, LAT,
    (tex) => new TileCache(tex, () => Promise.reject(Object.assign(new Error("bad node"), { status: 400 })),
      { budgetBytes: 96 * 1024 * 1024, inFlight: 4, now: () => 0 }),
    { pinParents: false },
  );
  surface.setScale(-100, -60);
  surface.render([pane("a", 0)]);
  await flush();
  const reports = surface.render([pane("a", 0)]);
  assert.ok(reports[0].refused > 0, "a refused place must be counted as refused, not as pending");
  assert.equal(reports[0].pending, 0, "…and not as both");
  // Nothing in the frame is grey, and the refused draws carry no flat colour at all — they are a
  // generated pattern, so there is no constant here that could drift into being one.
  for (const c of g.clears()) assert.ok(!near(c.args, [...GREY]));
  for (const d of g.draws()) assert.ok(!(d.u?.uFlat && near(d.u.uFlat, [...GREY])));
  const fs = g.shaders.find((s) => s.includes("refusedMark"))!;
  assert.equal(fs.split(`vec3(${GREY.join(",")})`).length - 1, 1,
    "the refused branch added a second grey to the shader");
});

test("ONE canvas, ONE context — which is what makes the cache shareable at all", () => {
  const h = harness(96 * 1024 * 1024);
  h.surface.render([pane("a", 0), pane("b", 10 * TILE_HZ)]);
  assert.equal(h.g.contextCount(), 1);
});

test("a pane's ground is PENDING; nothing in a frame is ever cleared to grey", async () => {
  const h = harness(96 * 1024 * 1024);
  h.surface.render([pane("a", 0)]);
  const clears = h.g.clears();
  assert.ok(clears.length >= 2, "the canvas backdrop and then the pane");
  assert.ok(near(clears[0].args, [...BACKDROP]));
  assert.ok(near(clears[1].args, [...PENDING]), "a grey pane clear says 'never observed' about a pane that is merely empty");
  for (const c of clears) assert.ok(!near(c.args, [...GREY]));
});

test("a tile in two panes uploads ONCE — through the real renderer, not a bench", async () => {
  const h = harness(96 * 1024 * 1024);
  const panes = [
    { ...pane("a", 0), rect: { x: 0, y: 0, w: W / 2, h: H } },
    { ...pane("b", 0), rect: { x: W / 2, y: 0, w: W / 2, h: H } },
  ];
  h.surface.render(panes);
  await flush();
  h.surface.render(panes);
  const keys = h.surface.cache.stats.distinctKeys;
  assert.ok(keys > 0);
  assert.equal(h.surface.cache.stats.uploads, keys, "a second upload of a shared tile means one cache per pane");
  assert.equal(h.fetches(), keys, "and one request per key, however many panes want it");
});

test("each pane resolves its OWN two levels, and a frequency zoom does not move anyone's time level", () => {
  const h = harness(96 * 1024 * 1024);
  const wide = { ...pane("wide", 0, 64), rect: { x: 0, y: 0, w: W, h: H } };
  const narrow: PaneView = { id: "narrow", rect: { x: 0, y: 0, w: W, h: H }, box: { f0Hz: 0, f1Hz: TILE_HZ, t0Ns: 0, t1Ns: TILE_NS } };
  const [a, b] = h.surface.render([wide, narrow]);
  assert.notEqual(a.levelF, b.levelF, "two panes at different frequency zooms must not share a level");
  assert.equal(a.levelT, b.levelT, "…and their time levels must be untouched by that difference");
});

test("F3: starving the budget changes PENDING and never changes GREY", async () => {
  const generous = harness(96 * 1024 * 1024);
  const starved = harness(BYTES); // one tile, against a three-tile working set that keeps moving

  const run = async (h: ReturnType<typeof harness>) => {
    let pending = 0, fallbacks = 0, tiles = 0;
    const greys: string[] = [];
    for (let frame = 0; frame < 9; frame++) {
      // Three viewports, revisited in turn: a nine-tile working set that the generous budget holds
      // and the starved one cannot. Panning to somewhere NEW every frame would make both runs miss
      // equally and prove nothing — the budget only bites on the return.
      h.g.reset();
      for (const r of h.surface.render([pane("a", (frame % 3) * 3 * TILE_HZ)])) {
        pending += r.pending; fallbacks += r.fallbacks; tiles += r.tiles;
      }
      for (const c of h.g.clears()) if (near(c.args, [...GREY])) greys.push(`clear ${c.args}`);
      for (const d of h.g.draws()) if (d.u?.uFlat && near(d.u.uFlat, [...GREY])) greys.push(`flat draw ${d.u.uFlat}`);
      await flush();
    }
    return { pending, fallbacks, tiles, greys, uploads: h.surface.cache.stats.uploads, evictions: h.surface.cache.stats.evictions };
  };

  const a = await run(generous);
  const b = await run(starved);

  assert.deepEqual(a.greys, [], "the renderer painted grey with a budget it did not need");
  assert.deepEqual(b.greys, [], "A MEMORY BUDGET MANUFACTURED THE CLAIM THAT THE RADIO NEVER LOOKED (T-437 F3)");
  assert.ok(b.evictions > 0, "the starved run must actually have thrashed, or it proves nothing");
  assert.equal(a.evictions, 0, "the generous budget must NOT thrash, or the two runs are the same run");
  // The budget shows up where it belongs: as tiles that have not arrived.
  assert.ok(b.pending + b.fallbacks > a.pending + a.fallbacks, "a starved budget must be visible as PENDING, not as anything else");
});

test("a cold viewport fetches the PARENT first: coarse-first fill against an 11.4 ms tile", async () => {
  const g = stubGl(W, H);
  const asked: number[][] = [];
  const surface = new Surface(
    g.canvas, LAT,
    (tex) => new TileCache(tex, (a) => { asked.push([a.levelF, a.levelT]); return Promise.resolve(data(a)); }, { inFlight: 1, now: () => 0 }),
    { pinParents: true },
  );
  surface.render([pane("a", 0)]);
  assert.ok(asked.length > 0);
  assert.deepEqual(asked[0], [1, 1], "one parent covers four children's worth of screen; fetching a child first spends 11.4 ms on a quarter of it");
});

test("a missing tile with a resident ancestor draws the ancestor, marked — never grey, never silently", async () => {
  const h = harness(96 * 1024 * 1024);
  // First look at a coarse view so the parents become resident…
  const coarse: PaneView = { id: "c", rect: { x: 0, y: 0, w: W, h: H }, box: { f0Hz: 0, f1Hz: 16 * TILE_HZ, t0Ns: 0, t1Ns: 4 * TILE_NS } };
  for (let i = 0; i < 3; i++) { h.surface.render([coarse]); await flush(); }
  // …then zoom in. The finer tiles are not resident on this first frame.
  h.g.reset();
  const [r] = h.surface.render([pane("a", 0, 1)]);
  assert.equal(r.tiles, 0, "the finer tiles cannot be resident on the frame they were first asked for");
  assert.ok(r.fallbacks > 0, "a resident ancestor must be drawn rather than left empty (docs/16 §5.5)");
  const marked = h.g.draws().filter((d) => d.u?.uFallback && d.u.uFallback[0] === 1);
  assert.equal(marked.length, r.fallbacks, "every stand-in must carry the mark that says it is one");
  for (const d of h.g.draws()) assert.ok(!(d.u?.uFlat && near(d.u.uFlat, [...GREY])));
});
