// T-1042 / LSR-1: the live ring's bookkeeping — where a published row lands, and what the tile lane
// may therefore leave alone.
//
// The claims, each against the implementation that would otherwise pass:
//
//  1. **A row is placed by its own capture time**, and a run is laid out at the cadence measured
//     across that run — never at a nominal rate, never at an index counted from mount.
//  2. **A gap is a gap.** A dropped row splits the ring into runs; the newest run is the only extent
//     the tile lane is allowed to skip, and the period is a MEDIAN so one long gap does not stretch
//     every row's extent (the degenerate version — a mean — is a one-word change and is caught here).
//  3. **A retune empties the ring and moves its epoch**, so no row of the old band can be drawn under
//     the new one, and a texture built from it is rebuilt rather than patched.
//  4. **The wrap is a run boundary**, because two ends of a ring are not one quad.
//  5. **The cover is tested against what the pane SHOWS of a tile** — a live tile's extent runs past
//     the live edge into rows that do not exist, so full containment would exclude nothing, which is
//     the whole mechanism failing quietly.
//  6. **Zoomed out past the rows' own resolution the ring stands aside**: one row under `MIN_ROW_PX`
//     would be a NEAREST pick of one row in N shown as the picture.
import test from "node:test";
import assert from "node:assert/strict";
import { CELL } from "../src/surface/cellrule";
import {
  GAP_FACTOR, LiveRing, MIN_ROW_PX, ringCovers, ringMaxHoldColumns, ringPlan, ringRowAt, ringSliceColumns,
  sampleRingRow, type RingFrame,
} from "../src/surface/livering";
import type { Box } from "../src/surface/lattice";

const MS = 1e6;
const PERIOD = 40 * MS;           // the production display row period (docs/api.md: 40.1 ms)
const T0 = 1_789_300_000 * 1e9;   // an absolute capture time, as the stream reports it
const BAND = { f0Hz: 99.6e6, f1Hz: 102e6 };
const NF = 8;

/** One published row: `nf` bins at `db`, at capture time `tNs`. */
const row = (tNs: number, db = -70, band = BAND) => ({
  ...band, db: new Float32Array(NF).fill(db), tNs,
});

/** `n` rows at the nominal period from `t`, pushed in order. */
function fill(ring: LiveRing, n: number, t = T0, db = -70): number {
  for (let i = 0; i < n; i++) ring.push(row(t + i * PERIOD, db));
  return t + n * PERIOD;
}

/**
 * Times are compared with a tolerance, and that is a fact about the axis rather than a softening.
 * An absolute capture time in ns is ~1.8e18 — past `Number.MAX_SAFE_INTEGER` — so a double's step
 * there is 256 ns. That is the resolution the whole canvas has always laid rows out at (`edgeNs` is
 * `tS * 1e9` everywhere in this client), and it is nine orders below one row.
 */
const NS_EPS = 4096;
const sameNs = (a: number, b: number, what: string) =>
  assert.ok(Math.abs(a - b) <= NS_EPS, `${what}: ${a} is not ${b} (within ${NS_EPS} ns)`);

const frameOf = (ring: LiveRing): RingFrame => {
  const f = ring.frame();
  assert.ok(f, "the ring reported no frame");
  return f;
};

test("LSR-1: a row is placed by its own capture time, and a run is laid out at its measured cadence", () => {
  const ring = new LiveRing({ rows: 16 });
  assert.equal(ring.frame(), null, "one row states no cadence, so nothing may be placed from it");
  ring.push(row(T0));
  assert.equal(ring.frame(), null, "a single row is not a measurement of the row period");
  fill(ring, 7, T0 + PERIOD);
  const f = frameOf(ring);
  assert.equal(f.writes, 8);
  sameNs(f.rowPeriodNs, PERIOD, "the period is measured off the rows, not defaulted");
  assert.deepEqual(f.spans.map((s) => [s.row0, s.rows]), [[0, 8]]);
  sameNs(f.spans[0].t0Ns, T0, "the run starts at the first row's own time");
  sameNs(f.spans[0].t1Ns, T0 + 8 * PERIOD, "the run ends one measured cadence past its last row");
  assert.equal(f.live, f.spans[0]);
  // The rows are in the planes in arrival order, one row of `nf` cells each, all OBSERVED.
  assert.equal(f.nf, NF);
  assert.equal(f.capacity, 16);
  assert.equal(f.value[7 * NF], -70);
  assert.equal(f.state[7 * NF], CELL.OBSERVED);
  // A slot no row was written into is UNKNOWN: not grey (the radio never looked) and not a level.
  assert.equal(f.state[8 * NF], CELL.UNKNOWN);
});

test("LSR-1: a faster or slower stream is laid out at the cadence it actually delivered", () => {
  const ring = new LiveRing({ rows: 64 });
  const slow = 25 * MS;
  for (let i = 0; i < 10; i++) ring.push(row(T0 + i * slow));
  const f = frameOf(ring);
  sameNs(f.rowPeriodNs, slow, "the delivered cadence");
  sameNs(f.spans[0].t1Ns, T0 + 10 * slow, "the run's end");
});

test("LSR-1: the ring keeps the newest `capacity` rows, and the wrap is a run boundary", () => {
  const ring = new LiveRing({ rows: 4 });
  fill(ring, 6);
  const f = frameOf(ring);
  assert.equal(f.writes, 6);
  // Rows 2..5 survive: 2,3 in slots 2,3 and 4,5 in slots 0,1. Two runs, because slot 3 → slot 0 is
  // not one quad — and their times say which is which.
  assert.deepEqual(f.spans.map((s) => [s.row0, s.rows]), [[2, 2], [0, 2]]);
  sameNs(f.spans[0].t0Ns, T0 + 2 * PERIOD, "the older run's first row");
  sameNs(f.spans[1].t0Ns, T0 + 4 * PERIOD, "the newer run's first row");
  assert.equal(f.live!.row0, 0, "the newest run is the one holding the newest row");
  sameNs(f.live!.t1Ns, T0 + 6 * PERIOD, "the newest run's end");
});

test("LSR-1: a dropped row is a gap — the runs split there, and the period is unstretched", () => {
  const ring = new LiveRing({ rows: 32 });
  const after = fill(ring, 6);
  // Six rows' worth of silence, then the stream resumes.
  ring.push(row(after + 6 * PERIOD));
  fill(ring, 5, after + 7 * PERIOD);
  const f = frameOf(ring);
  sameNs(f.rowPeriodNs, PERIOD, "one long delta moved the MEASURED period: a mean, not a median");
  assert.equal(f.spans.length, 2, `a gap of ${6 / GAP_FACTOR}× the period did not split the run`);
  assert.equal(f.spans[0].rows, 6);
  sameNs(f.spans[0].t1Ns, after, "the first run was stretched over rows that never arrived");
  assert.equal(f.spans[1].rows, 6);
  assert.equal(f.live, f.spans[1]);

  // …and the tile lane keeps the gap: the cover is the newest run only.
  const box: Box = { ...BAND, t0Ns: T0, t1Ns: f.live!.t1Ns };
  const plan = ringPlan(f, box, 600);
  assert.equal(plan.draws.length, 2, "the ring paints every run it holds");
  assert.deepEqual([plan.cover!.t0Ns, plan.cover!.t1Ns], [f.spans[1].t0Ns, f.spans[1].t1Ns]);
  // A tile over the gap is therefore NOT excluded: its rows are the pyramid's to answer.
  const gapTile: Box = { ...BAND, t0Ns: after, t1Ns: after + 6 * PERIOD };
  assert.equal(ringCovers(plan.cover!, gapTile, box), false);
});

test("LSR-1: a retune empties the ring and moves its epoch", () => {
  const ring = new LiveRing({ rows: 16 });
  fill(ring, 8);
  const before = frameOf(ring).epoch;
  ring.clear();
  assert.equal(ring.frame(), null, "rows of the band that ended survived the retune");
  fill(ring, 8, T0 + 100 * PERIOD);
  const f = frameOf(ring);
  assert.ok(f.epoch > before, "the epoch did not move, so a texture would be patched across bands");
  assert.equal(f.writes, 8);
  sameNs(f.spans[0].t0Ns, T0 + 100 * PERIOD, "the new band's first row");
});

test("LSR-1: a row of a different band empties the ring rather than joining it", () => {
  const ring = new LiveRing({ rows: 16 });
  fill(ring, 8);
  const was = frameOf(ring);
  const other = { f0Hz: 433e6, f1Hz: 435.4e6 };
  for (let i = 0; i < 4; i++) ring.push(row(T0 + (8 + i) * PERIOD, -60, other));
  const f = frameOf(ring);
  assert.ok(f.epoch > was.epoch);
  assert.deepEqual([f.f0Hz, f.f1Hz], [other.f0Hz, other.f1Hz]);
  assert.equal(f.writes, 4, "the old band's rows are still in the ring under the new band's geometry");
});

test("LSR-1: a row whose time does not move forward is refused, not placed below the edge", () => {
  const ring = new LiveRing({ rows: 16 });
  fill(ring, 4);
  ring.push(row(T0 + 2 * PERIOD));
  ring.push(row(T0 + 3 * PERIOD));
  assert.equal(ring.dropped, 2);
  const f = frameOf(ring);
  assert.equal(f.writes, 4);
  sameNs(f.live!.t1Ns, T0 + 4 * PERIOD, "a repeated row rewound the ring's edge");
  // Neither is a row with no time, nor an empty row, ever filed.
  ring.push({ ...BAND, db: new Float32Array(NF), tNs: Number.NaN });
  ring.push({ ...BAND, db: new Float32Array(0), tNs: T0 + 5 * PERIOD });
  assert.equal(frameOf(ring).writes, 4);
});

test("LSR-1: the cover is what the pane SHOWS of a tile, so a live tile past the edge is excluded", () => {
  const ring = new LiveRing({ rows: 256 });
  const end = fill(ring, 256);
  const f = frameOf(ring);
  // A pane over the newest 20.48 s, of which the ring holds the newest half: the shape a following
  // pane is in — rows at the edge, history below them.
  const box: Box = { ...BAND, t0Ns: end - 512 * PERIOD, t1Ns: end };
  const plan = ringPlan(f, box, 600);
  assert.ok(plan.cover, "the ring covered nothing over its own rows");
  sameNs(plan.cover!.t0Ns, end - 256 * PERIOD, "the cover reaches back exactly as far as the rows do");
  sameNs(plan.cover!.t1Ns, end, "the cover reaches the pane's live edge");
  // A level-0 tile whose extent runs 5 s PAST the live edge — the normal shape of the tile the ring
  // replaces — IS excluded, because what the pane shows of it is all rows.
  const live: Box = { ...BAND, t0Ns: end - 4e9, t1Ns: end + 5e9 };
  assert.equal(ringCovers(plan.cover!, live, box), true,
    "a tile reaching past the live edge was not excluded: the test is against the STRIP the pane shows");
  // A tile reaching back before the oldest row in the ring is not: those rows are history, and
  // history is the pyramid's.
  const older: Box = { ...BAND, t0Ns: box.t0Ns + 1e9, t1Ns: end - 200 * PERIOD };
  assert.equal(ringCovers(plan.cover!, older, box), false);
  // Neither is a tile beside the tuned band: the ring says nothing about spectrum it never held.
  const beside: Box = { f0Hz: BAND.f1Hz, f1Hz: BAND.f1Hz + 1e6, t0Ns: end - 4e9, t1Ns: end };
  assert.equal(ringCovers(plan.cover!, beside, box), false);
  // A tile only PARTLY under the rows keeps its measurement: it is still drawn, and still asked for.
  const straddles: Box = { ...BAND, t0Ns: end - 300 * PERIOD, t1Ns: end };
  assert.equal(ringCovers(plan.cover!, straddles, box), false);
});

test("LSR-1: zoomed out past the rows' own resolution the ring stands aside and says why", () => {
  const ring = new LiveRing({ rows: 256 });
  const end = fill(ring, 256);
  const f = frameOf(ring);
  const hPx = 600;
  // A window so wide that one 40 ms row is a fraction of a pixel: the pyramid's max-folds are the
  // honest answer there, and a NEAREST pick of one row in N is not.
  const wide: Box = { ...BAND, t0Ns: end - 3600e9, t1Ns: end };
  const out = ringPlan(f, wide, hPx);
  assert.ok(out.rowPx < MIN_ROW_PX, `rowPx ${out.rowPx}`);
  assert.equal(out.cover, null, "a ring that draws nothing must not exclude a tile either");
  assert.deepEqual(out.draws, []);
  // Zoomed in far enough that a row has a pixel, it draws.
  const close: Box = { ...BAND, t0Ns: end - hPx * PERIOD, t1Ns: end };
  const inside = ringPlan(f, close, hPx);
  assert.ok(inside.rowPx >= MIN_ROW_PX && Math.abs(inside.rowPx - 1) < 1e-9, `rowPx ${inside.rowPx}`);
  assert.equal(inside.draws.length, 1);
  sameNs(inside.draws[0].region.t0Ns, Math.max(close.t0Ns, f.live!.t0Ns), "the quad's start");
  sameNs(inside.draws[0].region.t1Ns, f.live!.t1Ns, "the quad's end");
});

test("LSR-1: the plan is clipped to the pane, in both axes", () => {
  const ring = new LiveRing({ rows: 64 });
  const end = fill(ring, 64);
  const f = frameOf(ring);
  // A pane narrower than the band and shorter than the ring: the quad is the overlap, and nothing
  // outside the pane is claimed.
  const box: Box = {
    f0Hz: BAND.f0Hz + 0.6e6, f1Hz: BAND.f1Hz - 0.6e6,
    t0Ns: end - 10 * PERIOD, t1Ns: end - 2 * PERIOD,
  };
  const plan = ringPlan(f, box, 400);
  assert.equal(plan.draws.length, 1);
  assert.deepEqual(plan.draws[0].region, {
    f0Hz: box.f0Hz, f1Hz: box.f1Hz, t0Ns: box.t0Ns, t1Ns: box.t1Ns,
  });
  assert.deepEqual(plan.cover, plan.draws[0].region);
  // A pane entirely before the ring's oldest row draws nothing from it.
  const past: Box = { ...BAND, t0Ns: T0 - 100 * PERIOD, t1Ns: T0 - 10 * PERIOD };
  assert.deepEqual(ringPlan(f, past, 400).draws, []);
  assert.equal(ringPlan(f, past, 400).cover, null);
});

// T-1047 / LSR-6: the trace's slice and its ring-covered max-hold read the ring directly, so the
// newest few seconds — which the tile lane was told to leave alone — are not read back as a gap.

test("LSR-6: ringRowAt finds the row that CONTAINS an instant, and sampleRingRow pools it across frequency", () => {
  const ring = new LiveRing({ rows: 64 });
  fill(ring, 8, T0, -70);
  const f = frameOf(ring);
  // Half-way through row 3 (0-indexed): inside that row's own extent, nobody else's.
  const at = ringRowAt(f, T0 + 3 * PERIOD + PERIOD / 2);
  assert.ok(at, "no row found for an instant well inside the ring");
  sameNs(at!.tNs, T0 + 3 * PERIOD, "the row's own start time");
  const box: Box = { ...BAND, t0Ns: 0, t1Ns: 0 };
  const cols = sampleRingRow(f, at!.slot, box, 4);
  for (const v of cols) assert.equal(v, -70);
  // A column outside the ring's own band: NaN, never a floor and never the neighbour's value.
  const outside = sampleRingRow(f, at!.slot, { f0Hz: 200e6, f1Hz: 201e6, t0Ns: 0, t1Ns: 0 }, 4);
  for (const v of outside) assert.ok(Number.isNaN(v), `expected NaN outside the ring's band, got ${v}`);
  // Before the ring's oldest row, and past its newest extent: no row contains either instant.
  assert.equal(ringRowAt(f, T0 - PERIOD), null);
  assert.equal(ringRowAt(f, T0 + 100 * PERIOD), null);
});

test("LSR-6: ringSliceColumns answers only where the ring itself has a row, and falls through (null) elsewhere", () => {
  const ring = new LiveRing({ rows: 32 });
  const end = fill(ring, 16, T0, -50);
  const f = frameOf(ring);
  const box: Box = { ...BAND, t0Ns: 0, t1Ns: 0 };
  const mid = T0 + 8 * PERIOD + PERIOD / 2;
  const cols = ringSliceColumns(f, box, 4, mid);
  assert.ok(cols, "the ring holds a row at this instant");
  for (const v of cols!) assert.equal(v, -50);
  // Older than the ring's own oldest row, and newer than its live extent: the caller falls back to
  // the pyramid (`sliceColumns`) exactly as it did before the ring existed.
  assert.equal(ringSliceColumns(f, box, 4, T0 - PERIOD), null);
  assert.equal(ringSliceColumns(f, box, 4, end + 10 * PERIOD), null);
});

test("LSR-6: ringMaxHoldColumns is the column-wise max over every ring row in the window, across a gap", () => {
  const ring = new LiveRing({ rows: 64 });
  // A quiet run …
  const after = fill(ring, 6, T0, -80);
  // … a gap the ring itself splits into two spans …
  ring.push(row(after + 6 * PERIOD, -90));
  const end = fill(ring, 5, after + 7 * PERIOD, -90);
  // … with one loud row in the middle of the second run.
  ring.push(row(end, -30));
  const f = frameOf(ring);
  assert.equal(f.spans.length, 2, "the gap must still split the ring into two spans (LSR-1's rule)");
  const box: Box = { ...BAND, t0Ns: T0, t1Ns: end + PERIOD };
  const cols = ringMaxHoldColumns(f, box, 4);
  for (const v of cols) assert.equal(v, -30, "the loudest row anywhere in the window must win");
  // A window entirely before the ring's own data: every column stays NaN, not zero and not a floor.
  const before: Box = { ...BAND, t0Ns: T0 - 100 * PERIOD, t1Ns: T0 - 10 * PERIOD };
  for (const v of ringMaxHoldColumns(f, before, 4)) assert.ok(Number.isNaN(v));
});
