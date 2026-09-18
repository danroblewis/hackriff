// T-441: **the three honesty tiers, the five cell states and the stand-in, asserted as PIXELS.**
//
// docs/16 §8.3: *"the honesty tiers stay visually distinct — `live-iq`, `spectrum-history`,
// `survey-overview` — so a wide or deep zoom never fakes resolution the hardware did not capture.
// §4's rule is unchanged and now has one place to be enforced instead of three."* §4's own line is
// *"never imply resolution the front end did not capture"*.
//
// **Why every assertion here is over a rasterised frame.** A test that reads `uTier` off a draw
// proves a flag was passed; it cannot tell whether two tiers ever *look* different, which is the
// whole of the claim. So each test renders through the real `Surface` into the recording stub, and
// `rasterize` replays the recorded ops — clears, viewports, uniforms, and the bytes actually
// uploaded to the samplers — into pixels through `cellPixel`, the CPU half of the same table the
// fragment shader is generated from.
//
// The four claims:
//   1. three tiers → three different pictures of the *same* measurements;
//   2. and yet **the ramp never moves**: an unmarked pixel is `cmap(x)` in every tier, because a
//      tier that changed a colour would be T-342 again (one energy, two strengths, one screen);
//   3. grey appears **iff** the cell's coverage state is `unobserved` — the fourth and fifth states
//      are their own marks, and neither is grey nor a level;
//   4. a zoom past what the front end measured draws the **survey lattice at the measured pitch**,
//      so replication is declared rather than upscaled into apparent detail.

import { test } from "node:test";
import assert from "node:assert/strict";
import { cmap } from "../src/cmap";
import {
  CELL, CELL_MARKS, FALLBACK_MARK, GREY, PATTERNS, PENDING, TIER, TIERS, tierByte,
  cellPixel, patternHit,
} from "../src/surface/cellrule";
import { keyOf, type Lattice, type TileAddr } from "../src/surface/lattice";
import { Surface, sourceCellPx, type PaneView } from "../src/surface/surface";
import { TileCache } from "../src/surface/tilecache";
import type { Tier, TileData } from "../src/surface/tile";
import { stubGl } from "./surface-glstub";
import { colourKey, countColour, rasterize, type Rect } from "./surface-raster";

const LAT: Lattice = { scheme: "view", cells: 256, f0Hz: 6250, t0Ns: 1e9, levelsF: 20, levelsT: 15 };
const TILE_HZ = LAT.f0Hz * LAT.cells;
const TILE_NS = LAT.t0Ns * LAT.cells;
const W = 480, H = 240;
const LO = -100, HI = -60;

interface Spec {
  readonly tier?: Tier;
  /** Cell states, row-major over an `nf × nt` grid. */
  readonly state?: number[];
  readonly value?: number[];
  readonly nf?: number;
  readonly nt?: number;
  /** How many cells along each axis were really measured (a replicated axis measured fewer). */
  readonly measured?: { nf: number; nt: number };
}

function tile(a: TileAddr, s: Spec = {}): TileData {
  const nf = s.nf ?? 2, nt = s.nt ?? 2;
  const state = s.state ?? Array(nf * nt).fill(CELL.OBSERVED);
  const value = s.value ?? state.map((_, i) => LO + ((i % 7) + 1) * 4);
  return {
    addr: a, key: keyOf(a), nf, nt,
    value: Float32Array.from(value),
    state: Uint8Array.from(state),
    tier: s.tier ?? "spectrum-history",
    answeredLevel: 1,
    fold: { frequency: "exact", time: "exact" },
    measured: s.measured ?? { nf, nt },
    rangeDb: null, bytes: 4096, serverInFlightLimit: null,
  };
}

const flush = () => new Promise((r) => setImmediate(r));

/** Render `panes` until every tile is resident, then rasterise the last frame. */
async function frameOf(spec: (a: TileAddr) => TileData, panes: readonly PaneView[], opts = { pinParents: false }) {
  const g = stubGl(W, H);
  const surface = new Surface(g.canvas, LAT, (tex) => new TileCache(tex, (a) => Promise.resolve(spec(a)), { now: () => 0 }), opts);
  surface.setScale(LO, HI);
  for (let i = 0; i < 4; i++) { surface.render(panes); await flush(); }
  g.reset();
  const reports = surface.render(panes);
  return { fb: rasterize(g.ops, W, H), reports, shaders: g.shaders, ops: g.ops };
}

const paneAt = (id: string, rect: Rect, tiles = 1): PaneView => ({
  id, rect: { x: rect.x, y: rect.y, w: rect.w, h: rect.h },
  box: { f0Hz: 0, f1Hz: tiles * TILE_HZ, t0Ns: 0, t1Ns: tiles * TILE_NS },
});

// ---------------------------------------------------------------------------------------------

test("the three honesty tiers are three different pictures of the SAME measurements", async () => {
  // One pane per tier, side by side, over identical cells. Anything that differs between the three
  // rectangles came from the tier and from nothing else.
  const rects: Rect[] = [
    { x: 0, y: 0, w: 160, h: H },
    { x: 160, y: 0, w: 160, h: H },
    { x: 320, y: 0, w: 160, h: H },
  ];
  const pixels: Map<string, number>[] = [];
  for (const t of TIERS) {
    const { fb } = await frameOf((a) => tile(a, { tier: t, nf: 4, nt: 4 }), [paneAt(t, rects[0])]);
    pixels.push(fb.histogram(rects[0]));
  }

  const asText = pixels.map((h) => [...h.entries()].sort().map(([k, n]) => `${k}:${n}`).join(" "));
  for (let i = 0; i < TIERS.length; i++) {
    for (let j = i + 1; j < TIERS.length; j++) {
      assert.notEqual(asText[i], asText[j], `${TIERS[i]} and ${TIERS[j]} rendered the same frame — a wide zoom would fake resolution and nothing on screen would say so`);
    }
  }

  // live-iq is the unmarked tier: marks are qualifications, so the tier with nothing to qualify
  // carries none. The other two must each add ink the live tier does not have.
  const live = pixels[TIER.LIVE_IQ];
  assert.equal(live.size, new Set([...live.keys()]).size);
  for (const t of [TIER.SPECTRUM_HISTORY, TIER.SURVEY_OVERVIEW]) {
    const extra = [...pixels[t].keys()].filter((k) => !live.has(k));
    assert.ok(extra.length > 0, `${TIERS[t]} drew no colour live-iq did not: the qualification is invisible`);
  }
  // …and the two qualified tiers ink different amounts of the pane, so they are not each other.
  const inked = (i: number) => [...pixels[i].entries()].filter(([k]) => !live.has(k)).reduce((n, [, c]) => n + c, 0);
  assert.notEqual(inked(TIER.SPECTRUM_HISTORY), inked(TIER.SURVEY_OVERVIEW));
});

test("…and yet the ramp never moves: an unmarked pixel is cmap(x) in EVERY tier", async () => {
  // T-342's defect was the same energy reading as two strengths on one screen. A tier that washed
  // the whole cell would re-import it with a new cause, so a tier mark may only ink the pixels it
  // draws on — the rest of the cell stays exactly the measurement's colour.
  const rect: Rect = { x: 0, y: 0, w: W, h: H };
  const v = -72;
  const want = colourKey(cmap((v - LO) / (HI - LO)));
  for (const t of TIERS) {
    const { fb } = await frameOf(
      (a) => tile(a, { tier: t, nf: 1, nt: 1, value: [v] }),
      [{ id: t, rect, box: { f0Hz: 0, f1Hz: TILE_HZ, t0Ns: 0, t1Ns: TILE_NS } }],
    );
    const h = fb.histogram(rect);
    const top = [...h.entries()].sort((a, b) => b[1] - a[1])[0];
    assert.equal(top[0], want, `${t}: the commonest colour over one measured cell must BE the measurement's colour`);
    assert.ok(top[1] > 0.5 * W * H, `${t}: a tier mark may qualify a measurement, never repaint it`);
  }
});

test("GREY appears if and only if the cell's coverage state is `unobserved`", async () => {
  // Every state on screen at once, in every tier, with a stand-in quad over part of it. If any of
  // those paths can reach the grey, this is where it shows.
  const states = [CELL.OBSERVED, CELL.UNOBSERVED, CELL.NO_LEVEL, CELL.UNKNOWN, CELL.AWAITING, CELL.OBSERVED, CELL.UNOBSERVED, CELL.AWAITING, CELL.UNKNOWN];
  for (const t of TIERS) {
    const rect: Rect = { x: 0, y: 0, w: W, h: H };
    const { fb } = await frameOf(
      (a) => tile(a, { tier: t, nf: 3, nt: 3, state: states }),
      [{ id: t, rect, box: { f0Hz: 0, f1Hz: TILE_HZ, t0Ns: 0, t1Ns: TILE_NS } }],
    );
    const grey = countColour(fb, GREY, rect);
    const cells = states.filter((s) => s === CELL.UNOBSERVED).length;
    // Two of nine cells are unobserved; every pixel of them is grey and no pixel outside them is.
    assert.ok(grey > 0, `${t}: an unobserved cell did not draw grey — grey is the claim, and it went missing`);
    const share = grey / (W * H);
    assert.ok(Math.abs(share - cells / 9) < 0.02, `${t}: ${(share * 100).toFixed(1)}% of the pane is grey, but ${cells}/9 cells are unobserved`);
  }
});

test("the fourth and fifth states are their own marks — not grey, not a level, not each other", async () => {
  // One cell per state across the pane, each in its own third, so each mark can be read alone.
  const rect: Rect = { x: 0, y: 0, w: W, h: H };
  const { fb } = await frameOf(
    (a) => tile(a, { tier: "live-iq", nf: 4, nt: 1, state: [CELL.UNOBSERVED, CELL.NO_LEVEL, CELL.UNKNOWN, CELL.AWAITING] }),
    [{ id: "p", rect, box: { f0Hz: 0, f1Hz: TILE_HZ, t0Ns: 0, t1Ns: TILE_NS } }],
  );
  const quarter = (i: number): Rect => ({ x: Math.round((i * W) / 4) + 4, y: 4, w: Math.round(W / 4) - 8, h: H - 8 });
  const hs = [0, 1, 2, 3].map((i) => fb.histogram(quarter(i)));

  // A flat claim is flat; a patterned claim is patterned. That is the difference a viewer reads.
  assert.equal(hs[0].size, 1, "unobserved is one flat colour — the grey");
  assert.equal(hs[1].size, 1, "no_level is one flat colour");
  assert.ok(hs[2].size >= 2, "the fourth state is HATCHED (T-413/T-423), not a flat fill");
  assert.ok(hs[3].size >= 2, "the fifth state is STIPPLED — an absence you can see, not a colour swatch");

  // No colour is shared between any two of the four.
  for (let i = 0; i < 4; i++) {
    for (let j = i + 1; j < 4; j++) {
      const shared = [...hs[i].keys()].filter((k) => hs[j].has(k));
      assert.deepEqual(shared, [], `states ${i} and ${j} share a colour: one of them is being read as the other`);
    }
  }
  // …and none of them, anywhere, is the "not loaded yet" mark. A memory fact is not a radio fact.
  assert.equal(countColour(fb, PENDING, rect), 0);
});

test("a zoom past what the front end measured draws the SURVEY LATTICE at the measured pitch", async () => {
  // A tile whose time axis was replicated: 256 served time cells built from 4 source cells. Zoomed
  // to fill the pane, a smooth upscale would show 256 bands of detail that were never measured.
  // The survey-overview mark draws the lattice of the FOUR cells that were, so the picture states
  // its own resolution (docs/16 §4; T-342's rule, T-411's failure).
  const rect: Rect = { x: 0, y: 0, w: W, h: H };
  const nt = 32, srcT = 4;
  const spec = (t: Tier) => (a: TileAddr) => tile(a, {
    tier: t, nf: 1, nt,
    value: Array.from({ length: nt }, () => -72), // one level everywhere: the ONLY structure on
    measured: { nf: 1, nt: srcT },                // screen is whatever the tier mark draws
  });
  const box = { f0Hz: 0, f1Hz: TILE_HZ, t0Ns: 0, t1Ns: TILE_NS };
  const survey = await frameOf(spec("survey-overview"), [{ id: "zoom", rect, box }]);

  // Count the lattice rules down a column: runs of pixels darker than the cell's own colour.
  const rules = (fb: { at(x: number, y: number): [number, number, number] }) => {
    const col = W >> 1;
    const ref = fb.at(col, Math.round(H * 0.125)); // a row the pitch cannot land on
    let runs = 0, inRun = false;
    for (let y = 0; y < H; y++) {
      const c = fb.at(col, y);
      // A lattice pixel is the same colour, darker — the mark never invents a hue.
      const dark = c[0] + c[1] + c[2] < (ref[0] + ref[1] + ref[2]) * 0.98;
      if (dark && !inRun) runs++;
      inRun = dark;
    }
    return runs;
  };

  // What the renderer told the shader, read off the draw it actually submitted. The expectations
  // below come from THESE numbers rather than from the pane's size, because the level the pane
  // resolves to decides how much of the tile the quad covers — and the claim under test is about
  // the pitch, not about which level answered.
  const draw = survey.ops.find((o) => o.kind === "draw" && o.u?.uTier?.[0] === TIER.SURVEY_OVERVIEW)!;
  const [, quadPx] = draw.u!.uSizePx;
  const [, srcY] = draw.u!.uSrcPx;
  const [, v0] = draw.u!.uUv0;
  const [, v1] = draw.u!.uUv1;
  const servedY = quadPx / (nt * (v1 - v0)); // one SERVED cell, on screen
  assert.ok(Math.abs(srcY / servedY - nt / srcT) < 0.01,
    `the lattice pitch is ${(srcY / servedY).toFixed(1)} served cells; ${nt} served cells were built from ${srcT} measured ones, so it must be ${nt / srcT}`);

  const drawn = rules(survey.fb);
  const periods = H / srcY; // how many measured cells the pane can actually show
  assert.ok(drawn >= Math.floor(periods) && drawn <= Math.ceil(periods) + 1,
    `the lattice drew ${drawn} rules where ${periods.toFixed(1)} measured cells are on screen: the pitch is not the measurement's`);
  assert.ok(drawn * 4 < nt, `${nt} served cells drew ${drawn} rules — anything near ${nt} is the served grid pretending to be the measured one`);

  // The same data at the live tier draws no lattice at all: the mark is the *claim*, not decoration.
  const live = await frameOf(spec("live-iq"), [{ id: "zoom", rect, box }]);
  assert.equal(rules(live.fb), 0, "live-iq drew a survey lattice: the tiers are not distinguishable after all");
});

test("the stand-in hatch and the tier marks cannot be mistaken for one another", async () => {
  // A resident coarser ancestor standing in for a missing child is marked (docs/16 §5.5). Its hatch
  // runs down-right; `unknown`'s runs up-right at another pitch; the tiers are dots and an
  // axis-aligned lattice. No two marks that can share a screen share a geometry.
  const g = stubGl(W, H);
  const surface = new Surface(
    g.canvas, LAT,
    (tex) => new TileCache(tex, (a) => Promise.resolve(tile(a, { tier: "live-iq", nf: 2, nt: 2 })), { now: () => 0 }),
    { pinParents: false },
  );
  surface.setScale(LO, HI);
  const coarse: PaneView = { id: "c", rect: { x: 0, y: 0, w: W, h: H }, box: { f0Hz: 0, f1Hz: 8 * TILE_HZ, t0Ns: 0, t1Ns: 4 * TILE_NS } };
  for (let i = 0; i < 3; i++) { surface.render([coarse]); await flush(); }
  g.reset();
  const [r] = surface.render([paneAt("zoomed", { x: 0, y: 0, w: W, h: H })]);
  assert.ok(r.fallbacks > 0, "the zoom must land on stand-ins, or this proves nothing");
  const fb = rasterize(g.ops, W, H);
  const h = fb.histogram();
  assert.ok(h.size >= 2, "a stand-in must be VISIBLY a stand-in, not a clean upscale");
  assert.equal(countColour(fb, GREY), 0, "a tile that has not arrived is never the radio never looking");

  // The four pattern geometries are genuinely different functions, not four names for one.
  const names = Object.keys(PATTERNS) as (keyof typeof PATTERNS)[];
  const sig = (n: (typeof names)[number]) => {
    let s = "";
    for (let y = 0; y < 24; y++) for (let x = 0; x < 24; x++) s += patternHit(n, { x, y }, { x: 8, y: 8 }) ? "1" : "0";
    return s;
  };
  const sigs = names.map(sig);
  assert.equal(new Set(sigs).size, names.length, `two patterns are the same shape: ${names}`);
});

test("the tier mark qualifies a MEASUREMENT and nothing else", () => {
  // A tier wash over an unobserved cell would make a second grey, which is the one thing the rule
  // module exists to prevent. So the tier branch in the shader is gated on the OBSERVED state, and
  // the CPU rule agrees: every non-observed state renders identically in all three tiers.
  const px = { x: 3, y: 5 }, srcPx = { x: 20, y: 20 };
  for (const s of [CELL.UNOBSERVED, CELL.NO_LEVEL, CELL.UNKNOWN, CELL.AWAITING]) {
    const cols = [0, 1, 2].map((tier) => cellPixel({ state: s, x: 0.5, px, tier, srcPx, fallback: false }).join(","));
    assert.equal(new Set(cols).size, 1, `state ${s} changed with the tier: a cell with no measurement has no resolution to overstate`);
  }
  // …and an observed one does: over one cell's worth of pixels the three tiers ink different sets.
  const inked = (tier: number) => {
    const out = new Set<string>();
    for (let y = 0; y < 24; y++) {
      for (let x = 0; x < 24; x++) out.add(`${x},${y}:${cellPixel({ state: CELL.OBSERVED, x: 0.5, px: { x, y }, tier, srcPx, fallback: false }).join(",")}`);
    }
    return out;
  };
  const [a, b, c] = [0, 1, 2].map(inked).map((s) => [...s].sort().join("|"));
  assert.notEqual(a, b);
  assert.notEqual(b, c);
  assert.notEqual(a, c);
});

test("the generated shader is the only implementation: one grey, one tier rule, one stand-in rule", () => {
  const g = stubGl();
  new Surface(g.canvas, LAT, (tex) => new TileCache(tex, () => Promise.reject(new Error("unused")), {}));
  const fs = g.shaders.find((s) => s.includes("cellMark"))!;

  // Every pattern expression appears exactly once, as the body of its generated predicate — so the
  // shader and `patternHit` cannot become two implementations of one shape (T-397's defect).
  for (const [name, expr] of Object.entries(PATTERNS)) {
    assert.equal(fs.split(expr).length - 1, 1, `pattern ${name} is written ${fs.split(expr).length - 1} times in the shader`);
  }
  // …and `patternHit` really computes that same expression. T-450 had to replace the module-scope
  // `new Function` that used to guarantee this by construction — `hk serve`'s own CSP has no
  // `unsafe-eval`, so evaluating it took the whole renderer down the first time a browser loaded
  // this module. The eval moved HERE, where it is allowed, and the guarantee became an assertion:
  // a drifting transcription now fails a test instead of being made impossible by a construct the
  // product cannot run.
  for (const [name, expr] of Object.entries(PATTERNS)) {
    const evaled = new Function("px", "p", "fract", `return (${expr});`) as
      (px: { x: number; y: number }, p: { x: number; y: number }, fract: (v: number) => number) => boolean;
    const fract = (v: number) => v - Math.floor(v);
    for (const pitch of [3, 5, 7, 8, 9, 10, 14.5]) {
      for (let y = -4; y < 32; y++) {
        for (let x = -4; x < 32; x++) {
          const px = { x: x + 0.5, y: y + 0.5 }, p = { x: pitch, y: pitch * 0.75 };
          assert.equal(patternHit(name as keyof typeof PATTERNS, px, p), evaled(px, p, fract),
            `pattern ${name} disagrees with its GLSL at (${px.x}, ${px.y}) pitch ${pitch}`);
        }
      }
    }
  }
  // The tier rule is generated, and `live-iq` really is the unmarked branch.
  assert.match(fs, /vec3 tierMark\(int t, vec3 col, vec2 px, vec2 srcPx\)/);
  assert.match(fs, new RegExp(`if \\(t == ${TIER.LIVE_IQ}\\) return col;`));
  // The tier branch is applied ONLY to an observed cell.
  assert.match(fs, new RegExp(`if \\(s == ${CELL.OBSERVED}\\) col = tierMark\\(uTier, col, px, uSrcPx\\);`));
  // The stand-in mark is generated too — no hand-written hatch left in the fragment shader.
  assert.match(fs, /vec3 fallbackMark\(vec3 col, vec2 px\)/);
  assert.equal(fs.split(`vec3(${GREY.join(",")})`).length - 1, 1, "a second grey");
  // A cell mark for every state, and a tier byte that is never permissive about an unknown tier.
  assert.equal(CELL_MARKS.length, 5);
  assert.equal(tierByte("live-iq"), TIER.LIVE_IQ);
  assert.equal(tierByte("something-new"), TIER.SURVEY_OVERVIEW, "an unrecognised tier must be the MOST qualified, never the most trusted");
  assert.ok(FALLBACK_MARK.pattern !== (CELL_MARKS[CELL.UNKNOWN] as { pattern: string }).pattern, "the stand-in and the fourth state must not share a hatch");
});

test("a tile with NO `measured` renders — a renderer may not have an input that blanks every pane", async () => {
  // Caught on the merge with T-442, whose pane fixtures predate `measured`. `sourceCellPx` ran
  // inside `drawRegion`, so an unguarded dereference there did not spoil one tile: it threw out of
  // `render` and took **every pane on the screen** with it. The guard is the same default
  // `decodeTile` applies with no `fold` block — the served grid, i.e. "no replication reported".
  const rect: Rect = { x: 0, y: 0, w: W, h: H };
  const box = { f0Hz: 0, f1Hz: TILE_HZ, t0Ns: 0, t1Ns: TILE_NS };
  const bare = (a: TileAddr): TileData => {
    const { measured: _drop, ...rest } = tile(a, { tier: "survey-overview", nf: 4, nt: 4 });
    return rest as TileData;
  };
  const { fb, reports } = await frameOf(bare, [{ id: "bare", rect, box }]);
  assert.ok(reports[0].tiles > 0, "the frame must actually have drawn the tile, not merely survived");
  assert.ok(fb.histogram(rect).size > 1, "…and drawn something, not one flat colour");

  // The fallback is the served grid, so the pitch is the tile's own cells — never invented, and
  // never a pitch coarser than anything the tile reported.
  assert.deepEqual(sourceCellPx({ nf: 8, nt: 4 }, 1, 1, 640, 320), sourceCellPx({ nf: 8, nt: 4, measured: { nf: 8, nt: 4 } }, 1, 1, 640, 320));
  // Every degenerate shape a hand-built fixture can reach answers a number, not an exception.
  for (const d of [{ nf: 0, nt: 0 }, { nf: NaN, nt: 2 }, { nf: 2, nt: 2, measured: { nf: 0, nt: -1 } }]) {
    const [x, y] = sourceCellPx(d as { nf: number; nt: number }, 1, 1, 100, 100);
    assert.ok(Number.isFinite(x) && Number.isFinite(y) && x >= 2 && y >= 2, `sourceCellPx(${JSON.stringify(d)}) = ${x},${y}`);
  }
});

test("sourceCellPx: a stand-in shows the pitch of the part on screen, and never sub-pixel mush", () => {
  const data = { nf: 256, nt: 256, measured: { nf: 8, nt: 4 } };
  // A whole tile across 640 × 320 device px: 8 measured columns, 4 measured rows.
  assert.deepEqual(sourceCellPx(data, 1, 1, 640, 320), [80, 80]);
  // A quarter of the tile, drawn in the same rectangle: the same measured cells, four times as big.
  assert.deepEqual(sourceCellPx(data, 0.25, 0.25, 640, 320), [320, 320]);
  // Zoomed far out, the lattice floors rather than aliasing into noise.
  const [fx, fy] = sourceCellPx(data, 1, 1, 8, 4);
  assert.ok(fx >= 2 && fy >= 2);
});
