// T-459: axis ticks for the chrome readout. A readout states two edges; a ruler adds intermediate
// marks between them. Four claims:
//
//  1. **Ticks land at nice (1/2/5 × 10ⁿ) steps inside the window**, not at values that divide the
//     span evenly (which would print unreadable fractions).
//  2. **The step never falls below the cell actually drawn** — honesty: a tick finer than the
//     pane's own resolution would claim the hardware resolved something it did not.
//  3. **A window too narrow, relative to its own cell, to offer an interior mark returns nothing** —
//     not a crash, not a single degenerate tick.
//  4. **`rulerLabel` composes both axes and is `null` only when BOTH are empty**, and the time axis
//     uses this surface's one time vocabulary (`LIVE`/`−<span>`), never a wall clock.

import { test } from "node:test";
import assert from "node:assert/strict";
import { freqTicksOf, joinTicks, niceStep, rulerLabel, timeTicksOf } from "../src/surface/ticks";

const S = 1e9;

test("niceStep rounds up to 1/2/5 × 10ⁿ, never below the floor", () => {
  assert.equal(niceStep(1), 1);
  assert.equal(niceStep(1.1), 2);
  assert.equal(niceStep(2), 2);
  assert.equal(niceStep(2.1), 5);
  assert.equal(niceStep(5), 5);
  assert.equal(niceStep(5.1), 10);
  assert.equal(niceStep(150), 200);
  assert.equal(niceStep(0.03), 0.05);
});

test("freqTicksOf: nice MHz steps, floored at the drawn cell, inside the window", () => {
  const ticks = freqTicksOf(100e6, 102e6, 1e3); // a fine cell: the span/target ratio should govern
  assert.ok(ticks.length >= 3 && ticks.length <= 7, `expected a handful of ticks, got ${ticks.length}`);
  for (const t of ticks) assert.ok(t.value > 100e6 && t.value < 102e6, "ticks fall strictly inside the window");
  // Values are nice: divisible by the same step, and that step reads as a round MHz number.
  const step = ticks.length > 1 ? ticks[1].value - ticks[0].value : null;
  if (step !== null) assert.ok([0.1, 0.2, 0.5, 1].some((mhz) => Math.abs(step - mhz * 1e6) < 1), `step ${step} Hz is not nice`);
});

test("freqTicksOf: honesty — the step never falls below the cell the pane actually drew", () => {
  // A cell coarser than the span/target ratio would want: the step floors at the cell even though
  // that means fewer ticks than the target — here, zero, because no >=5 MHz nice multiple lands
  // strictly inside a 2 MHz window that doesn't straddle one. Asserted directly against the
  // flooring rule (not a magic count), so this breaks if flooring regresses to the span-based step.
  assert.deepEqual(freqTicksOf(100e6, 102e6, 5e6), []);
});

test("freqTicksOf: a window narrower than its own cell offers nothing", () => {
  assert.deepEqual(freqTicksOf(100.0e6, 100.05e6, 1e6), [], "no interior mark fits inside one cell");
});

test("freqTicksOf: ticks never land exactly on an edge (that would restate headline, not add to it)", () => {
  // loHz is itself an exact multiple of the natural step, so a naive inclusive scan would print a
  // tick at 100.0 MHz — identical to what the row's headline already states.
  const ticks = freqTicksOf(100e6, 102e6, 1e3);
  for (const t of ticks) assert.ok(t.value > 100e6 && t.value < 102e6, `tick ${t.value} sits on an edge`);
});

test("freqTicksOf: degenerate windows never throw", () => {
  assert.deepEqual(freqTicksOf(100e6, 100e6, 1e3), []);
  assert.deepEqual(freqTicksOf(100e6, 99e6, 1e3), []);
  assert.deepEqual(freqTicksOf(100e6, 102e6, 0), []);
});

test("timeTicksOf: labels behind the live edge in this surface's vocabulary, never a wall clock", () => {
  const edgeNs = 1_700_000_000 * S;
  const ticks = timeTicksOf(edgeNs - 20 * S, edgeNs, 1, edgeNs);
  assert.ok(ticks.length > 0);
  for (const t of ticks) assert.ok(/^live edge$|^−/.test(t.label), `label "${t.label}" is not this surface's vocabulary`);
  // Strictly interior (see the edge-exclusion test below), so the tick nearest the edge is within
  // one step of it, never further — asserted against `niceStep` itself, the one source of the step.
  const step = niceStep(Math.max(1e9, (20 * S) / 6));
  const last = ticks[ticks.length - 1];
  assert.ok(edgeNs - last.value > 0 && edgeNs - last.value <= step, "the last tick should be within one step of the edge");
});

test("timeTicksOf: honesty — the step never falls below the cell actually drawn", () => {
  const edgeNs = 1_700_000_000 * S; // an exact multiple of 10 s, chosen so the arithmetic is exact
  // A 10 s cell over a 40 s window: the span/target ratio alone would ask for a finer step, so a
  // flat 10 s spacing between every consecutive tick is direct evidence the floor bound.
  const ticks = timeTicksOf(edgeNs - 40 * S, edgeNs, 10, edgeNs);
  assert.ok(ticks.length >= 2, `expected multiple ticks to compare spacing, got ${ticks.length}`);
  for (let i = 1; i < ticks.length; i++) {
    assert.equal(ticks[i].value - ticks[i - 1].value, 10 * S, "spacing must be the floored 10 s step");
  }
});

test("joinTicks: null for an empty list, comma-joined labels otherwise", () => {
  assert.equal(joinTicks([]), null);
  assert.equal(joinTicks([{ value: 1, label: "a" }, { value: 2, label: "b" }]), "a, b");
});

test("rulerLabel: composes both axes, null only when both are empty", () => {
  const edgeNs = 1_700_000_000 * S;
  const label = rulerLabel(100e6, 102e6, edgeNs - 20 * S, edgeNs, 1e3, 1, edgeNs);
  assert.ok(label && label.startsWith("freq ") && label.includes(" · time "), label ?? "null");

  // One axis too narrow to mark, the other fine: only the non-empty half appears, no stray "·".
  const oneAxis = rulerLabel(100.0e6, 100.05e6, edgeNs - 20 * S, edgeNs, 1e6, 1, edgeNs);
  assert.ok(oneAxis && oneAxis.startsWith("time ") && !oneAxis.includes("freq"), oneAxis ?? "null");

  // Both too narrow: null, not "freq  · time " or similar.
  assert.equal(rulerLabel(100.0e6, 100.05e6, edgeNs - 0.5, edgeNs, 1e6, 10, edgeNs), null);
});
