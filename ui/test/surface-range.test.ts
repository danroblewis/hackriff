// **T-470 — the colour scale is a property of the region, not of the viewport.**
//
// The user's report was *"colours animate and shift when I zoom"*, and their reading of it was
// right: `/api/tiles` returns a `range_db` computed per request from the tiles asked for, and
// `Surface.render` adopted it every frame from the tiles that happened to be on screen. Zoom changes
// which tiles those are, so it changed `(uLo, uHi)`, so **the same measured dB re-coloured**.
//
// What is asserted here, and what is deliberately *not*:
//
//  - The claim that the same dB is the same colour **across zooms, in a browser, read as pixels** is
//    `ui/e2e/surface-colour.e2e.mjs`. It has to be, for T-450's reason: this tier has no CSP, no GPU
//    and no compositor, and a module can pass every assertion here and not run in the product.
//  - What this tier can state exactly is the **mechanism**: that the default range is not a function
//    of what is drawn, that the opt-in one still is, that the anchor comes off a backend answer this
//    client already fetches, and that no range can put a weak measurement on the same pixel as no
//    measurement. The browser tier reads the consequence; this one pins the cause.
//
// The guard is written so it FAILS if the default is ever made viewport-dependent again — which is
// a one-line regression (`autoScale = true`), so the test drives that exact fault itself, below, and
// shows the assertion catching it. A guard nobody has seen go red is a guard nobody has tested.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { cmap } from "../src/cmap";
import { CELL, GREY, cellPixel } from "../src/surface/cellrule";
import { shadeRange } from "../src/surface/bootstrap";
import { keyOf, type Box, type Lattice, type TileAddr } from "../src/surface/lattice";
import { legendEntries, rangeEntry, rangeLabel } from "../src/surface/legend";
import { anchorOf, probeSurface, refreshOrientationNote } from "../src/surface/preview";
import { ANCHOR_SPAN_DB, FALLBACK_RANGE, FALLBACK_RANGE_SOURCE, Surface, type PaneView } from "../src/surface/surface";
import { TileCache } from "../src/surface/tilecache";
import type { TileData } from "../src/surface/tile";
import { stubGl } from "./surface-glstub";

const LAT: Lattice = { scheme: "view", cells: 256, f0Hz: 6250, t0Ns: 1e9, levelsF: 20, levelsT: 15 };
const S = 1e9;
const T0 = 1_700_000_000 * S;
const W = 1200, H = 800;
const RECT = { x: 0, y: 0, w: W, h: H };

const flush = () => new Promise((r) => setImmediate(r));

/**
 * A tile whose reported `range_db` depends on **where it is**: the low half of the surface is quiet,
 * the high half is loud. That is the whole premise of the defect — two viewports over one surface
 * see two different `range_db`s — so the fixture has to carry it or the guard is vacuous.
 */
function data(a: TileAddr): TileData {
  const loud = a.fIndex >= 64;
  return {
    addr: a, key: keyOf(a), nf: 2, nt: 2,
    value: new Float32Array([-90, -80, -70, -60]),
    state: new Uint8Array([CELL.OBSERVED, CELL.OBSERVED, CELL.OBSERVED, CELL.OBSERVED]),
    tier: "spectrum-history", answeredLevel: 1, fold: { frequency: "exact", time: "exact" },
    measured: { nf: 2, nt: 2 },
    rangeDb: loud ? { lo: -40, hi: -5 } : { lo: -130, hi: -105 },
    bytes: 4096, serverInFlightLimit: null, serverInFlightShare: null,
  };
}

function harness() {
  const g = stubGl(W, H);
  const surface = new Surface(
    g.canvas, LAT,
    (tex) => new TileCache(tex, (a) => Promise.resolve(data(a)), { inFlight: 64, now: () => 0 }),
    { pinParents: false },
  );
  return { g, surface };
}

/** A pane over `box`, filling the canvas. */
const pane = (id: string, box: Box): PaneView => ({ id, rect: RECT, box });

/** A quiet window (low frequency) and a loud one (high), disjoint in the tile fixture above. */
const QUIET: Box = { f0Hz: 10 * 1.6e6, f1Hz: 12 * 1.6e6, t0Ns: T0, t1Ns: T0 + 200 * S };
const LOUD: Box = { f0Hz: 100 * 1.6e6, f1Hz: 102 * 1.6e6, t0Ns: T0, t1Ns: T0 + 200 * S };
/** The loud window again, zoomed in 4× about its centre — the gesture the user reported. */
const LOUD_ZOOMED: Box = {
  f0Hz: (LOUD.f0Hz + LOUD.f1Hz) / 2 - (LOUD.f1Hz - LOUD.f0Hz) / 8,
  f1Hz: (LOUD.f0Hz + LOUD.f1Hz) / 2 + (LOUD.f1Hz - LOUD.f0Hz) / 8,
  t0Ns: (LOUD.t0Ns + LOUD.t1Ns) / 2 - (LOUD.t1Ns - LOUD.t0Ns) / 8,
  t1Ns: (LOUD.t0Ns + LOUD.t1Ns) / 2 + (LOUD.t1Ns - LOUD.t0Ns) / 8,
};

/** Draw `box` until its tiles are resident and the range has had every chance to move. */
async function settle(surface: Surface, id: string, box: Box, frames = 6): Promise<void> {
  for (let i = 0; i < frames; i++) {
    surface.render([pane(id, box)]);
    await flush();
  }
}

// ——— 1. the default is anchored, and nothing drawn can move it ———

test("a Surface opens ANCHORED to a stated range, not tracking whatever is on screen", () => {
  const { surface } = harness();
  assert.equal(surface.autoScale, false, "the default mapping is viewport-dependent again");
  assert.deepEqual({ lo: surface.lo, hi: surface.hi }, { lo: FALLBACK_RANGE.lo, hi: FALLBACK_RANGE.hi });
  assert.equal(surface.range.mode, "anchored");
  assert.equal(surface.range.source, FALLBACK_RANGE_SOURCE,
    "a range with no provenance is a number the legend cannot make honest");
});

/**
 * **The guard, driven through the DEFAULT path.**
 *
 * The first draft of this called `setScale` first — which sets `autoScale = false` itself, so it
 * proved "the anchored mode is anchored" and went green with `autoScale = true` restored as the
 * default. That is the adjacent-question failure this repo keeps hitting, caught by running the
 * fault: the subject is *what a Surface does when nobody has said anything*, so nothing here says
 * anything. `withAnchor` below re-runs the identical drive after a host has anchored it, which is
 * the other half and a different claim.
 */
async function guardDrive(surface: Surface): Promise<void> {
  const before = { ...surface.range };

  // A quiet band, a loud band, and the loud band zoomed 4× — three viewports whose tiles report
  // `range_db` spanning -130…-5 between them. Under the defect each of these moves the scale.
  await settle(surface, "a", QUIET);
  assert.deepEqual({ lo: surface.lo, hi: surface.hi }, { lo: before.lo, hi: before.hi });
  await settle(surface, "a", LOUD);
  assert.deepEqual({ lo: surface.lo, hi: surface.hi }, { lo: before.lo, hi: before.hi });
  await settle(surface, "a", LOUD_ZOOMED);
  assert.deepEqual({ lo: surface.lo, hi: surface.hi }, { lo: before.lo, hi: before.hi },
    "zooming changed the display range: the same measured dB now renders as a different colour");
  assert.deepEqual(surface.range, before, "the mode or the provenance drifted under navigation");

  // Two panes on one screen at two zooms — the split case, where the defect is visible side by side.
  surface.render([pane("a", LOUD), pane("b", LOUD_ZOOMED)]);
  await flush();
  surface.render([pane("a", LOUD), pane("b", LOUD_ZOOMED)]);
  assert.deepEqual({ lo: surface.lo, hi: surface.hi }, { lo: before.lo, hi: before.hi },
    "one pane's zoom re-coloured the other pane, which is showing the same data it was");
}

test("THE GUARD: a Surface nobody configured does not let the viewport decide the colours", async () => {
  const { surface } = harness();
  await guardDrive(surface);
  assert.deepEqual({ lo: surface.lo, hi: surface.hi }, { lo: FALLBACK_RANGE.lo, hi: FALLBACK_RANGE.hi });
});

test("THE GUARD, with a host's measured anchor in force", async () => {
  const { surface } = harness();
  surface.setScale(-95, -45, "measured once over the observed region");
  await guardDrive(surface);
  assert.deepEqual({ lo: surface.lo, hi: surface.hi }, { lo: -95, hi: -45 });
});

test("NON-VACUITY: the same drive, with the defect restored, moves the range every time", async () => {
  // This is the fault the guard above exists for — `autoScale` left on by default — driven through
  // the identical sequence. If this test ever stops observing movement, the guard above is asserting
  // nothing and the fixture (not the product) is what changed.
  const { surface } = harness();
  surface.setScale(-95, -45, "anchored");
  surface.setAutoScale(true);

  await settle(surface, "a", QUIET);
  const quiet = { lo: surface.lo, hi: surface.hi };
  await settle(surface, "a", LOUD);
  const loud = { lo: surface.lo, hi: surface.hi };
  assert.ok(loud.lo > quiet.lo + 5 && loud.hi > quiet.hi + 5,
    `the viewport-tracking mode did not track the viewport: ${JSON.stringify(quiet)} → ${JSON.stringify(loud)}`);
  assert.equal(surface.range.mode, "auto");
  assert.notEqual(quiet.lo, FALLBACK_RANGE.lo);
});

test("the renderer reads a tile's own range_db ONLY inside the opt-in branch", () => {
  const src = readFileSync("src/surface/surface.ts", "utf8");
  const reads = src.split("\n").map((l, i) => [l, i] as const).filter(([l]) => /\.rangeDb/.test(l));
  assert.equal(reads.length, 1, `rangeDb is read on ${reads.length} lines; the guard below covers one`);
  const [, at] = reads[0];
  // The four lines above the read must contain the opt-in test. Written as a proximity check rather
  // than a parse because the failure it guards against is a one-line move of the read *out* of the
  // branch, which is exactly what a proximity check sees.
  const above = src.split("\n").slice(Math.max(0, at - 4), at).join("\n");
  assert.match(above, /if \(this\.autoScale\)/,
    "a tile's own range is read outside the auto-contrast branch: the default mapping is viewport-dependent again");
});

// ——— 2. the anchor is a backend measurement, and the client adds nothing to it ———

/**
 * **The top is measured, the span is stated — and the reason is the fold, not taste.**
 *
 * `max_db` folds by max-hold, so the two ends of a reported `range_db` are different kinds of
 * number: `hi` is a maximum of maxima and is therefore *exact at any resolution*, while `lo` is a
 * minimum of maxima that folding can only raise. The first draft of this fix adopted both, and the
 * browser tier measured what that costs: over the FM fixture the 128 × 32 coverage grid reports
 * −72 dBFS where the level-0 cells reach −86, so 14 dB of real measurement clipped to black and the
 * spectrum trace collapsed from 586 drawn columns to 44, every one pinned to the floor of its strip.
 */
test("the anchor takes the MEASURED top and a STATED span, because only the top survives the fold", () => {
  const r = shadeRange({ shade: { range_db: { lo: -76.33, hi: -50.42 } } }, "the observed region");
  assert.equal(r?.hi, -50.42, "the measured maximum is exact at any resolution; it is adopted as-is");
  assert.equal(r?.lo, -50.42 - ANCHOR_SPAN_DB,
    "the bottom must be the stated span below the peak — a reported `lo` is an upper bound on the floor, " +
    "so anchoring there clips real measurement to black");
  assert.equal(r!.hi - r!.lo, ANCHOR_SPAN_DB);
  assert.match(r!.source, /observed region/);
  assert.match(r!.source, /shade\.range_db/, "the provenance must name the answer it came from");
  assert.match(r!.source, /below the peak/, "…and must not read as if both ends were measured");
  // The reported floor is BELOW the anchored floor for this fixture's numbers, which is the whole
  // point: nothing the region contains is clipped.
  assert.ok(r!.lo < -76.33, "the anchored floor must reach below the coarse grid's reported minimum");
});

test("no range, no anchor: nothing measured one is not the same as one that happens to be zero", () => {
  for (const cov of [
    null, undefined, {}, { shade: null }, { shade: { range_db: null } },
    { shade: { range_db: { lo: -50, hi: -50 } } }, //     an empty span is not a scale
    { shade: { range_db: { lo: -20, hi: -60 } } }, //     inverted
    { shade: { range_db: { lo: Number.NaN, hi: -60 } } },
    { shade: { range_db: { hi: -60 } } },
  ]) {
    assert.equal(shadeRange(cov as never, "x"), null, `accepted ${JSON.stringify(cov)} as a display range`);
  }
});

test("probeSurface anchors from the REFINED coverage pass, and says which region it measured", async () => {
  const answers: Record<string, unknown> = {};
  const p = await probeSurface(async (path) => {
    if (path.startsWith("/api/tiles")) return tileProbe();
    if (path === "/api/navigation") return { time: { latest_s: 1_700_000_000 } };
    // The first coverage ask is the whole surface; the second is the refinement over the observed
    // box. They report DIFFERENT ranges, so which one was adopted is observable.
    const n = (answers.n = ((answers.n as number) ?? 0) + 1);
    return coverage(n === 1 ? { lo: -140, hi: -30 } : { lo: -76.33, hi: -50.42 });
  });
  assert.deepEqual({ lo: p.range.lo, hi: p.range.hi }, { lo: -50.42 - ANCHOR_SPAN_DB, hi: -50.42 },
    "the coarse whole-surface pass was adopted over the refined one measured on the data");
  assert.match(p.range.source, /observed region/);
});

test("with no coverage answer at all, the client states a FALLBACK and never dresses it as a measurement", async () => {
  const p = await probeSurface(async (path) => {
    if (path.startsWith("/api/tiles")) return tileProbe();
    if (path === "/api/navigation") return { time: { latest_s: 1_700_000_000 } };
    throw new Error("no coverage store");
  });
  assert.deepEqual({ lo: p.range.lo, hi: p.range.hi }, { lo: FALLBACK_RANGE.lo, hi: FALLBACK_RANGE.hi });
  assert.match(p.range.source, /not a measurement/);
  assert.equal(p.range.hi - p.range.lo, 60, "the stated fallback is the 60 dB working span");
});

test("anchorOf is total: a renderer may not have an input that blanks every pane", () => {
  for (const bad of [null, undefined, {}, { lo: 0, hi: 0 }, { lo: Number.NaN, hi: 1 }, { lo: 5, hi: -5 },
    { lo: Number.POSITIVE_INFINITY, hi: 1 }]) {
    const a = anchorOf(bad as never);
    assert.ok(Number.isFinite(a.lo) && Number.isFinite(a.hi) && a.hi > a.lo, `anchorOf(${JSON.stringify(bad)})`);
    assert.equal(a.source, FALLBACK_RANGE_SOURCE);
  }
  assert.deepEqual(anchorOf({ lo: -90, hi: -30, source: "s" }), { lo: -90, hi: -30, source: "s" });
});

// ——— 3. the honesty boundary a stable anchor makes reachable ———

test("the bottom of the ramp is NOT the grey: a weak measurement cannot read as no measurement", () => {
  const px = { x: 3, y: 3 }, srcPx = { x: 8, y: 8 };
  const weak = (x: number) => cellPixel({ state: CELL.OBSERVED, x, px, tier: 0, srcPx, fallback: false });
  const never = cellPixel({ state: CELL.UNOBSERVED, x: 0, px, tier: 0, srcPx, fallback: false });
  assert.deepEqual(never, [...GREY]);
  // Every position at or below the bottom of ANY anchored range — including one clipped far below
  // it, which is the new thing a fixed scale makes possible — stays far from the grey.
  for (const x of [-10, -1, -0.001, 0, 0.02, 0.05]) {
    const c = weak(x);
    const d = Math.abs(c[0] - never[0]) + Math.abs(c[1] - never[1]) + Math.abs(c[2] - never[2]);
    assert.ok(d > 0.25, `a measurement at ramp position ${x} is within ${d.toFixed(3)} of THE grey`);
  }
  // And the ramp's own floor is a colour the grey is not: near-black with a blue cast.
  assert.deepEqual(cmap(0), [0, 0, 0.04]);
});

// ——— 4. auto-contrast stays reachable, and the legend is accurate in BOTH modes ———

test("the toggle round-trips: back to the anchor, with its own provenance, not to a fallback", () => {
  const { surface } = harness();
  surface.setScale(-76.33, -50.42, "measured once over the observed region");
  const anchored = { ...surface.range };
  surface.setAutoScale(true);
  assert.equal(surface.range.mode, "auto");
  assert.match(surface.range.source, /on screen/);
  surface.setScale(anchored.lo, anchored.hi, anchored.source);
  assert.deepEqual(surface.range, anchored);
});

test("the legend states the range and the trade-off it makes, in whichever mode is in force", () => {
  const anchored = rangeEntry({ lo: -76.3, hi: -50.4, mode: "anchored", source: "measured once over the observed region" });
  assert.match(anchored.label, /-76\.3 … -50\.4 dBFS/);
  assert.match(anchored.label, /\(26 dB\)/);
  assert.match(anchored.note, /same colour at every zoom/i);
  assert.match(anchored.note, /clipped/, "a fixed range that can clip must say so");
  assert.match(anchored.note, /observed region/, "the legend must name the measurement the scale is");

  const auto = rangeEntry({ lo: -120, hi: -60, mode: "auto", source: "the range of the tiles currently on screen" });
  assert.match(auto.label, /-120\.0 … -60\.0 dBFS/);
  assert.match(auto.note, /changes colour as you zoom/i, "auto-contrast must state ITS trade, not the other one");
  assert.ok(!/clipped/.test(auto.note), "auto-contrast does not clip; saying it does would be a second inaccuracy");

  // One line for a status bar, same facts.
  assert.match(rangeLabel({ lo: -76.3, hi: -50.4, mode: "anchored", source: "x" }), /-76\.3…-50\.4 dBFS/);
  assert.match(rangeLabel({ lo: -76.3, hi: -50.4, mode: "auto", source: "x" }), /auto-contrast/);

  // The scale row is painted by the same rule as every other row, so it cannot become a second ramp.
  assert.equal(anchored.pixel({ x: 2, y: 2 }, 0.7).join(","), cmap(0.7).join(","));
  assert.ok(!legendEntries().some((e) => e.key === "range"), "the scale row is live, so it is not in the static key");
});

// ——— fixtures for probeSurface ———

function tileProbe(): unknown {
  return {
    key: { device: "any", scheme: "view", level_f: 0, level_t: 0, f_index: 0, t_index: 0, cells: 8 },
    extent: { nf: 8, nt: 8 },
    axes: { frequency: { levels: 12, cell_hz: 6250 }, time: { levels: 15, cell_s: 1 } },
    grid: { nf: 8, nt: 8, max_db: Array<number | null>(64).fill(null) },
    coverage: { any: { cells: Array.from({ length: 64 }, () => ({ state: "unobserved" })) }, horizon: { oldest_record_s: 1_699_999_900 } },
    resolution: { source: "spectrum-history", answered: { level: 0 } },
  };
}

/** A coverage answer with one observed cell, so the refinement pass happens, and a stated scale. */
function coverage(range: { lo: number; hi: number }): unknown {
  const cells = Array.from({ length: 128 * 32 }, (_, i) => ({ state: i === 500 ? "observed" : "unobserved" }));
  return {
    grid: { cells: 128, rows: 32, f_lo_hz: 99.6e6, f_cell_hz: 18750, t0_s: 1_699_999_900, t_cell_s: 1 },
    any: { cells },
    shade: { range_db: range },
  };
}

test("T-946(b): the orientation note follows the backend's coverage as it grows, not the first paint", async () => {
  const lit = (n: number) => ({
    ...(coverage({ lo: -76, hi: -50 }) as object),
    any: { cells: Array.from({ length: 128 * 32 }, (_, i) => ({ state: i < n ? "observed" : "unobserved" })) },
  });
  let n = 1;
  let latest = 1_700_000_000;
  const coverageT1: number[] = [];
  const get = async (path: string) => {
    if (path.startsWith("/api/tiles")) return tileProbe();
    if (path === "/api/navigation") return { time: { latest_s: latest } };
    coverageT1.push(Number(new URL(path, "http://x").searchParams.get("t1") ?? NaN));
    return lit(n);
  };
  const p = await probeSurface(get);
  const first = p.note;
  n = 400;
  latest += 300; // the sweep lit cells AFTER first paint, past the probe's frozen box
  const later = await refreshOrientationNote(get, p);
  assert.notEqual(later, first, "the sentence stayed at its first-paint census after coverage grew");
  // T-964: the census is stated on both axes — 400 row-major cells is every one of the 128 frequency
  // cells (the survey's own share) and 9.8 % of the 4096 (time x frequency) cells the canvas draws.
  assert.match(later, /128 of 128 frequency cells ever sampled/);
  assert.match(later, /9\.8 % of its 4096 time × frequency cells/);
  const asked = coverageT1[coverageT1.length - 1];
  assert.ok(asked >= latest, `the refresh queried a box ending at ${asked}, before the newest capture ${latest}`);
});
