// T-1048 / LSR-7: sample→pixel latency and per-row fold cost, measured — never assumed.
//
// The claims, each against the implementation that would otherwise pass:
//
//  1. **A rolling window reports over exactly what it retains** — the most recent `capacity`
//     samples, oldest evicted first — never a lifetime average that would smooth an ongoing stall
//     out of the mean.
//  2. **`n === 0` is the honest empty state**: no samples means "no samples yet", never a guessed
//     0 ms.
//  3. **`timed()` measures the wall-clock cost of exactly the call it wraps**, and returns the
//     call's own result unchanged.
//  4. **`paintLatencyMs` is "row t vs rAF"**: `paint` minus `arrival`, ONE clock (`performance.now()`)
//     read twice — never the row's own backend capture time, which is not wall-clock-comparable on a
//     replay (this module's header records the bug that shipped once already).
//  5. **`LastArrival` is the single shared "when did the newest row arrive" holder**, `null` until
//     set.
//  6. **The dashboard tile's text never states a number for an empty window.**
import test from "node:test";
import assert from "node:assert/strict";
import { fmtLiveMetrics, LastArrival, LiveMetrics, paintLatencyMs, RollingStat, timed } from "../src/surface/livemetrics";

test("RollingStat: n=0 is the honest empty state, never a guessed 0 ms", () => {
  const s = new RollingStat(8);
  const snap = s.snapshot();
  assert.equal(snap.n, 0);
  assert.equal(snap.meanMs, 0);
  assert.equal(snap.p95Ms, 0);
  assert.equal(snap.maxMs, 0);
});

test("RollingStat: mean/p95/max over the samples actually pushed", () => {
  const s = new RollingStat(100);
  for (let i = 1; i <= 100; i++) s.push(i); // 1..100
  const snap = s.snapshot();
  assert.equal(snap.n, 100);
  assert.equal(snap.meanMs, 50.5);
  assert.equal(snap.maxMs, 100);
  // p95 of 1..100 (ceil(0.95*100) - 1 = 94, zero-indexed) is the 95th smallest value, 95.
  assert.equal(snap.p95Ms, 95);
});

test("RollingStat: retains only the most recent `capacity` samples — an old stall does not linger forever", () => {
  const s = new RollingStat(4);
  for (const v of [1, 2, 3, 4, 100, 100, 100, 100]) s.push(v); // the first four evicted
  const snap = s.snapshot();
  assert.equal(snap.n, 4);
  assert.equal(snap.meanMs, 100);
  assert.equal(snap.maxMs, 100);
});

test("RollingStat: a non-finite duration is refused rather than corrupting the window", () => {
  const s = new RollingStat(4);
  s.push(NaN);
  s.push(Infinity);
  s.push(5);
  assert.equal(s.snapshot().n, 1);
  assert.equal(s.snapshot().meanMs, 5);
});

test("RollingStat: reset() forgets every sample", () => {
  const s = new RollingStat(4);
  s.push(1);
  s.push(2);
  s.reset();
  assert.equal(s.snapshot().n, 0);
});

test("timed(): measures exactly the wrapped call, and returns its result unchanged", () => {
  const s = new RollingStat(4);
  let clock = 0;
  const now = () => clock;
  const out = timed(s, () => { clock += 3.5; return "row filed"; }, now);
  assert.equal(out, "row filed");
  const snap = s.snapshot();
  assert.equal(snap.n, 1);
  assert.equal(snap.meanMs, 3.5);
});

test("timed(): a zero-cost call still records a (zero) sample — the window is never silently skipped", () => {
  const s = new RollingStat(4);
  timed(s, () => 1, () => 10);
  assert.equal(s.snapshot().n, 1);
  assert.equal(s.snapshot().meanMs, 0);
});

test("paintLatencyMs: row t vs rAF — the paint clock minus the arrival clock, one clock read twice", () => {
  assert.equal(paintLatencyMs(1042, 1000), 42);
});

test("paintLatencyMs: a paint that (implausibly) preceded the arrival reads negative, not clamped", () => {
  // The arithmetic states what it measured; clamping a negative to 0 would hide a clock skew.
  assert.equal(paintLatencyMs(995, 1000), -5);
});

test("LastArrival: null until set, and holds exactly the last value written", () => {
  const a = new LastArrival();
  assert.equal(a.get(), null);
  a.set(123.5);
  assert.equal(a.get(), 123.5);
  a.set(456);
  assert.equal(a.get(), 456);
});

test("LastArrival.mark(): reads its OWN clock rather than taking one from its caller — the one place " +
  "allowed to read the wall clock for this ticket's question", () => {
  const a = new LastArrival();
  let clock = 10;
  a.mark(() => clock);
  assert.equal(a.get(), 10);
  clock = 20;
  a.mark(() => clock);
  assert.equal(a.get(), 20);
});

test("LiveMetrics: latency and fold are independent windows, both reset together", () => {
  const m = new LiveMetrics();
  m.latency.push(10);
  m.fold.push(0.5);
  const snap = m.snapshot();
  assert.equal(snap.latency.n, 1);
  assert.equal(snap.fold.n, 1);
  assert.equal(snap.latency.meanMs, 10);
  assert.equal(snap.fold.meanMs, 0.5);
  m.reset();
  const empty = m.snapshot();
  assert.equal(empty.latency.n, 0);
  assert.equal(empty.fold.n, 0);
});

test("fmtLiveMetrics: states 'no samples yet' rather than a guessed number when a window is empty", () => {
  const m = new LiveMetrics();
  const text = fmtLiveMetrics(m.snapshot());
  assert.match(text, /fold no samples yet/);
  assert.match(text, /latency no samples yet/);
});

test("fmtLiveMetrics: with samples, states mean/p95/max and the sample count for each window", () => {
  const m = new LiveMetrics();
  m.fold.push(0.02);
  m.fold.push(0.04);
  m.latency.push(30);
  m.latency.push(50);
  const text = fmtLiveMetrics(m.snapshot());
  assert.match(text, /fold 0\.03 ms mean, 0\.04 ms p95, 0\.04 ms max \(n=2\)/);
  assert.match(text, /latency 40\.00 ms mean, 50\.00 ms p95, 50\.00 ms max \(n=2\)/);
});
