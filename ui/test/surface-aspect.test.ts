// T-472: **a plain wheel locks the aspect ratio.**
//
// The reported defect: zooming out past the end of the record pins the time axis at the retained
// window while the frequency axis keeps widening, so a gesture that promised to scale both equally
// scales one — the picture's proportions drift, the view jumps, and the user shift-scrolls the
// frequency axis back by hand after every wheel.
//
// ——— WHAT THE EVIDENCE BELOW IS A PROPERTY OF ———
//
// This repo keeps shipping sound proofs of *adjacent* claims: T-441 verified a shader for a module a
// browser could not load, T-454's counter measured a pump rather than a cancellation, T-448's gate
// guaranteed every counter except the asserted one. The adjacent claim here is **"the zoom was
// clamped"**, which is not the claim at all — the old code clamped too, on each axis, and that is
// exactly how the ratio drifted. So what is measured below is the **ratio itself**, `spanHz /
// spanNs`, before and after every wheel; and not at a few sample points but over a grid of surfaces,
// starting windows, cursor anchors and every factor a wheel can produce, applied repeatedly so the
// walk enters both bounds on both axes and then keeps wheeling while it sits there.
//
// The second thing asserted is what the fix could most easily break: **the lock constrains the
// GESTURE, not the pyramid.** T-434 de-welded the two axes in the store, T-438 addresses them
// independently and T-440 resolves them per pane; a fix that collapsed them onto one level to make
// the pixels square would undo that milestone. So the axes are checked to be *still* clamped
// separately and anchored separately, `shift` and `alt` are checked to still change the ratio on
// purpose, and the lock is checked to **preserve whatever ratio a pane has** rather than to impose
// one — two panes that start at different ratios must still differ after the same gesture.
//
// ——— NON-VACUITY ———
//
// Zero violations proves nothing unless the same grid can produce violations. It can: `walk`'s
// `per-axis` mode runs the identical grid through **exactly what `SurfacePreview.wheel` used to do**
// — `zoomFreq(factor)` then `zoomTime(factor)`, each clamping on its own. That is not a mutant
// invented to fail; it is the shipped code this ticket replaces.

import { test } from "node:test";
import assert from "node:assert/strict";
import type { Lattice } from "../src/surface/lattice";
import { PaneModel } from "../src/surface/panes";
import { zoomFactor } from "../src/surface/preview";

const S = 1e9;
const T1 = 1_789_300_920 * S; // an absolute capture instant: the surface's clock, not a wall clock
const LAT: Lattice = { scheme: "view", cells: 256, f0Hz: 6250, t0Ns: 1e9, levelsF: 20, levelsT: 15 };
// The zoom floors this lattice implies, at `PaneModel`'s default `minCells` of 16.
const FLOOR_HZ = LAT.f0Hz * 16, FLOOR_NS = LAT.t0Ns * 16;

/**
 * The surfaces the grid runs over — chosen so that **each axis is the one that saturates first** in
 * some of them, because a lock tested only where time runs out would say nothing about the case
 * where frequency does.
 */
const SURFACES = [
  { name: "6 GHz × a day of record (time has ~12 levels of room, frequency ~15)",
    bounds: { f0Hz: 1e6, f1Hz: 6e9, t0Ns: T1 - 86_400 * S, t1Ns: T1 } },
  { name: "6 GHz × 40 s of record (time saturates first — the reported case)",
    bounds: { f0Hz: 1e6, f1Hz: 6e9, t0Ns: T1 - 40 * S, t1Ns: T1 } },
  { name: "6 GHz × 18 s of record (barely one zoom floor of time: pinned both ways)",
    bounds: { f0Hz: 1e6, f1Hz: 6e9, t0Ns: T1 - 18 * S, t1Ns: T1 } },
  { name: "2.4 MHz × a day (frequency saturates first — the mirror image)",
    bounds: { f0Hz: 100e6, f1Hz: 100e6 + 2.4e6, t0Ns: T1 - 86_400 * S, t1Ns: T1 } },
];

/** Where the pane starts: a fraction of each extent, where in it, and whether it follows the edge. */
const STARTS = [
  { fSpan: 1, tSpan: 1, fAt: 0.5, tAt: 0.5, live: true },
  { fSpan: 1, tSpan: 1, fAt: 0.5, tAt: 0.5, live: false },
  { fSpan: 0.5, tSpan: 0.5, fAt: 0.1, tAt: 0.9, live: false },
  { fSpan: 0.25, tSpan: 0.9, fAt: 0.9, tAt: 0.1, live: true },
  { fSpan: 0.01, tSpan: 0.02, fAt: 0.5, tAt: 0.5, live: false },
  { fSpan: 1e-4, tSpan: 1e-3, fAt: 0.3, tAt: 0.7, live: false },
  { fSpan: 0.7, tSpan: 0.05, fAt: 0.0, tAt: 0.0, live: true },
  { fSpan: 0.05, tSpan: 0.7, fAt: 1.0, tAt: 1.0, live: false },
  { fSpan: 1e-9, tSpan: 1e-9, fAt: 0.42, tAt: 0.42, live: false }, // both pinned at their floors
];

/** Where the cursor was, as `[anchorF, anchorT]` — including all four corners of the pane. */
const ANCHORS: readonly (readonly [number, number])[] = [[0.5, 1], [0, 0], [1, 1], [0.25, 0.75], [1, 0]];

/**
 * The wheel deltas. Spans the whole range `zoomFactor` can produce, **including both ends of its own
 * clamp** (±4000 px saturate at 4 and 0.25) and `0`, which is a zoom of exactly nothing.
 */
const DELTAS = [
  -4000, -2000, -1200, -800, -600, -480, -360, -240, -180, -120, -60, -30, -10,
  0, 10, 30, 60, 120, 180, 240, 360, 480, 600, 800, 1200, 2000, 4000,
];

/** Consecutive wheels per cell: enough to walk into a bound and then keep wheeling while there. */
const REPEATS = 4;

const WIDTH = 1200, HEIGHT = 800;

function paneAt(bounds: { f0Hz: number; f1Hz: number; t0Ns: number; t1Ns: number }, s: typeof STARTS[number]) {
  const fullF = bounds.f1Hz - bounds.f0Hz, fullT = bounds.t1Ns - bounds.t0Ns;
  const m = new PaneModel({
    bounds, lattice: LAT, width: WIDTH, height: HEIGHT,
    freq: { centerHz: bounds.f0Hz + fullF * s.fAt, spanHz: fullF * s.fSpan },
    spanNs: fullT * s.tSpan,
  });
  const id = m.list()[0].id;
  // A frozen pane and a following one clamp time differently (`normalise` has a centre to clamp for
  // one and not the other), so both arms of the union are walked.
  if (!s.live) m.goTo(id, bounds.t0Ns + fullT * s.tAt);
  return { m, id };
}

interface Tally {
  steps: number;
  /** Wheels after which `spanHz / spanNs` was not what it was before. The property under test. */
  ratio: number;
  /** Wheels that moved exactly one of the two spans. The user-visible shape of the same defect. */
  alone: number;
  /** The lock refused the gesture outright: an axis was already at a bound. */
  stopped: number;
  /** The lock shortened the gesture: an axis would have hit a bound part-way through. */
  reduced: number;
  /** The lock did nothing: both axes had room for the whole factor. */
  full: number;
  worst: number;
}

/**
 * Walk the grid. `mode` selects the gesture:
 *  - `locked` — today's `PaneModel.zoomBoth`, which `SurfacePreview.wheel` calls for a plain wheel.
 *  - `per-axis` — yesterday's, kept here as the non-vacuity control: the same factor handed to each
 *    axis, each clamping alone. Nothing about it is contrived; it is the code being replaced.
 */
function walk(mode: "locked" | "per-axis"): Tally {
  const t: Tally = { steps: 0, ratio: 0, alone: 0, stopped: 0, reduced: 0, full: 0, worst: 0 };
  for (const surf of SURFACES) {
    for (const s of STARTS) {
      for (const [af, at] of ANCHORS) {
        for (const d of DELTAS) {
          const { m, id } = paneAt(surf.bounds, s);
          const factor = zoomFactor(d);
          for (let k = 0; k < REPEATS; k++) {
            const before = m.get(id)!;
            const locked = m.lockedZoomFactor(id, factor);
            if (mode === "locked") m.zoomBoth(id, factor, af, at);
            else { m.zoomFreq(id, factor, af); m.zoomTime(id, factor, at); }
            const after = m.get(id)!;

            t.steps++;
            const r0 = before.freq.spanHz / before.time.spanNs;
            const r1 = after.freq.spanHz / after.time.spanNs;
            const err = Math.abs(r1 / r0 - 1);
            if (err > 1e-9) t.ratio++;
            t.worst = Math.max(t.worst, err);
            const movedF = after.freq.spanHz !== before.freq.spanHz;
            const movedT = after.time.spanNs !== before.time.spanNs;
            if (movedF !== movedT) t.alone++;

            // The population this grid actually reached, so the zero above is known not to be zero
            // because the bounds were never approached. Judged from the locked factor, which is a
            // read either way, so both modes report the same population.
            if (factor !== 1) {
              if (locked === 1) t.stopped++;
              else if (Math.abs(locked / factor - 1) > 1e-12) t.reduced++;
              else t.full++;
            }
          }
        }
      }
    }
  }
  return t;
}

const show = (t: Tally) =>
  `${t.steps} wheels · ${t.ratio} changed the aspect ratio (worst ${(t.worst * 100).toFixed(3)} %) · ` +
  `${t.alone} moved one axis alone · population: ${t.full} unclamped, ${t.reduced} shortened, ${t.stopped} refused`;

test("T-472: a plain wheel never changes the aspect ratio, and never moves one axis alone", (ctx) => {
  const locked = walk("locked");
  const perAxis = walk("per-axis");
  ctx.diagnostic(`locked (today):   ${show(locked)}`);
  ctx.diagnostic(`per-axis (T-456): ${show(perAxis)}`);

  // ——— the premise: the grid really did reach the bounds ———
  // Without this the zeroes below would be a statement about a walk that never left the middle of
  // the surface, which is where the old code was correct too.
  assert.ok(locked.stopped > 0,
    "no wheel in the whole grid was refused outright, so the bound this ticket is about was never reached");
  assert.ok(locked.reduced > 0,
    "no wheel was ever shortened by the lock — the grid never crossed a bound part-way through a step");
  assert.ok(locked.full > 0,
    "every wheel in the grid was clamped, so a lock that simply froze the view would pass this test");

  // ——— the claim ———
  assert.equal(locked.ratio, 0,
    `${locked.ratio} of ${locked.steps} plain wheels changed the pane's aspect ratio (worst ` +
    `${(locked.worst * 100).toFixed(3)} %). A uniform zoom must scale spanHz and spanNs by the SAME ` +
    "number at every point of the gesture, bounds included — that is what the user means by the view " +
    "not jumping, and it is why only shift and alt may change the ratio.");
  assert.equal(locked.alone, 0,
    `${locked.alone} of ${locked.steps} plain wheels moved exactly one of the two spans. A plain ` +
    "wheel moves both axes or neither; one axis alone is shift's job, or alt's.");

  // ——— non-vacuity: the same grid, through the code this replaces ———
  // Stated as a fraction of the walk rather than as a tuned constant: the defect is not a corner
  // case, it is what happens to roughly a fifth of every wheel once a bound is in reach.
  assert.ok(perAxis.ratio > locked.steps / 20,
    `the per-axis gesture changed the ratio on only ${perAxis.ratio} of ${perAxis.steps} wheels, so ` +
    "this grid does not exercise the defect and the zero above is not evidence of anything");
  assert.ok(perAxis.alone > locked.steps / 20,
    `the per-axis gesture moved one axis alone on only ${perAxis.alone} wheels — same problem`);
  assert.ok(perAxis.worst > 0.5,
    `the worst per-axis ratio drift over the whole grid was ${(perAxis.worst * 100).toFixed(1)} %, ` +
    "which is not the visible jump the user reported: the control is not reproducing the bug");
});

test("T-472: the lock is on the GESTURE — the axes stay independently levelled and shift/alt still skew", () => {
  // The reported surface: 6 GHz of spectrum over a short record, where time runs out first.
  const bounds = { f0Hz: 1e6, f1Hz: 6e9, t0Ns: T1 - 40 * S, t1Ns: T1 };
  const fresh = () => {
    const m = new PaneModel({ bounds, lattice: LAT, width: WIDTH, height: HEIGHT,
      freq: { centerHz: 100.8e6, spanHz: 2.4e6 }, spanNs: 20 * S });
    return { m, id: m.list()[0].id };
  };

  // ——— 1. at the bound a plain wheel moves NEITHER axis, and leaves the record untouched ———
  {
    const { m, id } = fresh();
    for (let i = 0; i < 40; i++) m.zoomBoth(id, zoomFactor(240), 0.5, 1); // wheel out until it stops
    const at = m.get(id)!;
    assert.equal(m.lockedZoomFactor(id, zoomFactor(240)), 1, "the walk did not actually reach a bound");
    m.zoomBoth(id, zoomFactor(240), 0.5, 1);
    assert.equal(m.get(id), at,
      "a plain wheel at the bound replaced the pane's record. It must be a no-op — not a recomputation " +
      "that happens to land on the same numbers, because (c + k) − k is not always c");

    // …and it stopped because TIME ran out, with frequency still far from the surface's edge. This
    // is the whole point: the old code would have carried on widening frequency to the full 6 GHz.
    assert.ok(at.time.spanNs >= bounds.t1Ns - bounds.t0Ns - 1, "time is not at the record's extent");
    assert.ok(at.freq.spanHz < (bounds.f1Hz - bounds.f0Hz) / 2,
      `the frequency axis reached ${(at.freq.spanHz / 1e6).toFixed(1)} MHz of the surface's 6 GHz — ` +
      "it was not stopped by the lock, so this case is testing the wrong bound");

    // ——— 2. …but shift and alt still move one axis each, from that very state ———
    const skew = m.get(id)!;
    m.zoomFreq(id, zoomFactor(240), 0.5);
    assert.ok(m.get(id)!.freq.spanHz > skew.freq.spanHz, "shift must still widen frequency at the lock");
    assert.equal(m.get(id)!.time.spanNs, skew.time.spanNs, "shift moved the time axis: the axes are welded");
    const afterShift = m.get(id)!;
    m.zoomTime(id, zoomFactor(-240), 1);
    assert.ok(m.get(id)!.time.spanNs < afterShift.time.spanNs, "alt must still narrow time at the lock");
    assert.equal(m.get(id)!.freq.spanHz, afterShift.freq.spanHz, "alt moved the frequency axis: welded");
  }

  // ——— 3. the same at the other end: zoom IN until time hits its floor ———
  {
    const { m, id } = fresh();
    for (let i = 0; i < 40; i++) m.zoomBoth(id, zoomFactor(-240), 0.5, 1);
    const at = m.get(id)!;
    assert.equal(at.time.spanNs, FLOOR_NS, "time should be pinned at the 16-cell zoom floor");
    assert.ok(at.freq.spanHz > FLOOR_HZ * 4,
      `frequency was dragged down to ${at.freq.spanHz} Hz against a ${FLOOR_HZ} Hz floor — the lock ` +
      "let the uniform zoom run on past the axis that stopped first");
    assert.equal(m.lockedZoomFactor(id, zoomFactor(-240)), 1);
  }

  // ——— 4. the lock PRESERVES a ratio, it does not IMPOSE one ———
  // The failure this rules out is the tempting one: making the pixels square by welding the levels.
  // Two panes with different aspect ratios, the same gesture, still different afterwards — and each
  // still exactly its own ratio.
  {
    const { m, id: a } = fresh();
    const b = m.split(a, "columns")!;
    m.zoomFreq(b, 64, 0.5);           // b is now 64× wider in frequency than a, same time span
    const r = (id: string) => m.get(id)!.freq.spanHz / m.get(id)!.time.spanNs;
    const r0a = r(a), r0b = r(b);
    assert.ok(Math.abs(r0b / r0a - 64) < 1e-6, "the two panes did not start at different ratios");
    for (let i = 0; i < 6; i++) { m.zoomBoth(a, zoomFactor(-120), 0.5, 1); m.zoomBoth(b, zoomFactor(-120), 0.5, 1); }
    assert.ok(Math.abs(r(a) / r0a - 1) < 1e-9, "pane a's own ratio was not preserved");
    assert.ok(Math.abs(r(b) / r0b - 1) < 1e-9, "pane b's own ratio was not preserved");
    assert.ok(Math.abs(r(b) / r(a) - 64) < 1e-6,
      "the two panes' ratios converged: the lock is imposing an aspect ratio rather than preserving " +
      "each pane's own, which is the re-welding T-434 undid wearing a different hat");
  }
});
