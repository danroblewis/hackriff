// T-996: the scale bar's arithmetic (`src/surface/scale.ts`), which is the whole claim it makes.
//
// The bar says "this length is that much spectrum / that much time". The test of that is not what it
// looks like but whether the two agree: `px * perPx === value` at whatever zoom, and the label is
// the value in words. Everything else here is the choice of a ROUND value — the reason a map shows
// "200 kHz" and not "187.4 kHz".
import test from "node:test";
import assert from "node:assert/strict";

import { fmtScaleHz, fmtScaleS, freqScaleBar, niceBar, paneScale, scaleBudgetPx, scaleMarkOf, timeScaleBar } from "../src/surface/scale";

test("T-996: the bar's length times the pane's Hz/px IS the quantity it states", () => {
  for (const spanHz of [20e6, 2e6, 937e3, 125e3, 4e3, 300]) {
    for (const widthPx of [1280, 800, 400, 320]) {
      const budget = scaleBudgetPx(widthPx);
      const bar = freqScaleBar(spanHz, widthPx, budget);
      assert.ok(bar, `no bar for ${spanHz} Hz across ${widthPx} px`);
      const hzPerPx = spanHz / widthPx;
      assert.ok(Math.abs(bar.px * hzPerPx - bar.value) < 1e-6 * bar.value,
        `${bar.label}: ${bar.px} px × ${hzPerPx} Hz/px ≠ ${bar.value} Hz`);
      assert.ok(bar.px > 0 && bar.px <= budget + 1e-9,
        `${bar.label} is ${bar.px.toFixed(1)} px, over the ${budget} px budget`);
      assert.equal(bar.label, fmtScaleHz(bar.value));
    }
  }
});

test("T-996: the time bar is the same claim on the time axis, in clock units", () => {
  for (const spanS of [600, 120, 30, 10, 2, 0.4]) {
    for (const heightPx of [760, 400, 240]) {
      const budget = scaleBudgetPx(heightPx);
      const bar = timeScaleBar(spanS, heightPx, budget);
      assert.ok(bar, `no bar for ${spanS} s down ${heightPx} px`);
      const sPerPx = spanS / heightPx;
      assert.ok(Math.abs(bar.px * sPerPx - bar.value) < 1e-9 * Math.max(1, bar.value),
        `${bar.label}: ${bar.px} px × ${sPerPx} s/px ≠ ${bar.value} s`);
      assert.ok(bar.px <= budget + 1e-9);
      assert.equal(bar.label, fmtScaleS(bar.value));
    }
  }
});

test("T-996: the value is always a round one a reader recognises", () => {
  const seenHz = new Set<string>();
  for (let widthPx = 300; widthPx <= 1600; widthPx += 37) {
    for (const spanHz of [6e9, 20e6, 1.234e6, 91e3, 512]) {
      const bar = freqScaleBar(spanHz, widthPx, scaleBudgetPx(widthPx));
      if (!bar) continue;
      seenHz.add(bar.label);
      const mantissa = bar.value / Math.pow(10, Math.floor(Math.log10(bar.value) + 1e-9));
      assert.ok([1, 2, 5].some((m) => Math.abs(m - mantissa) < 1e-6),
        `${bar.value} Hz is not a 1/2/5 step (mantissa ${mantissa})`);
    }
  }
  // Not one label for every zoom: a scale bar that never changed its words would not be a scale.
  assert.ok(seenHz.size > 4, `only ${seenHz.size} distinct frequency labels across the sweep`);
});

test("T-996: zooming in shrinks the quantity the same length stands for", () => {
  // The invariant a user reads off it: halve the span, and the bar states at most what it did.
  let prev = Infinity;
  for (let spanHz = 20e6; spanHz > 100; spanHz /= 2) {
    const bar = freqScaleBar(spanHz, 1000, 96);
    assert.ok(bar);
    assert.ok(bar.value <= prev, `${bar.value} > ${prev} after a zoom IN`);
    prev = bar.value;
  }
});

test("T-996: a degenerate pane gets no bar rather than a false one", () => {
  assert.equal(freqScaleBar(0, 800, 96), null, "a zero span invented a scale");
  assert.equal(freqScaleBar(NaN, 800, 96), null);
  assert.equal(freqScaleBar(20e6, 0, 96), null, "a zero-width pane invented a scale");
  assert.equal(timeScaleBar(-1, 400, 96), null);
  // Below the smallest step on the ladder there is nothing honest to say: 0.4 Hz over 96 px cannot
  // be labelled 1 Hz without the bar being 2.4× its own claim.
  assert.equal(freqScaleBar(0.4, 96, 96), null);
  assert.equal(timeScaleBar(0.0005, 96, 96), null, "half a millisecond across the whole budget invented a bar");
});

test("T-996: the words are the unit a reader would use", () => {
  assert.equal(fmtScaleHz(200e3), "200 kHz");
  assert.equal(fmtScaleHz(1e6), "1 MHz");
  assert.equal(fmtScaleHz(5e8), "500 MHz");
  assert.equal(fmtScaleHz(2e9), "2 GHz");
  assert.equal(fmtScaleHz(50), "50 Hz");
  assert.equal(fmtScaleS(0.5), "500 ms");
  assert.equal(fmtScaleS(10), "10 s");
  assert.equal(fmtScaleS(120), "2 min");
  assert.equal(fmtScaleS(1800), "30 min");
  assert.equal(fmtScaleS(7200), "2 h");
  assert.equal(fmtScaleS(86400), "1 d");
});

test("T-996: time steps climb through the clock's own ladder, not decimal seconds", () => {
  // 100 s and 1000 s are never shown: a map says "2 min", not "100 s".
  const labels = new Set<string>();
  for (let spanS = 1; spanS < 200000; spanS *= 1.35) {
    const bar = timeScaleBar(spanS, 800, 96);
    if (bar) labels.add(bar.label);
  }
  for (const bad of ["100 s", "200 s", "500 s", "1000 s"]) {
    assert.ok(!labels.has(bad), `the time bar offered ${bad}`);
  }
  assert.ok(labels.has("1 min") || labels.has("2 min"), `no minute-scale label in ${[...labels].join(", ")}`);
});

test("T-996: both bars come from ONE pane's own box and rect", () => {
  const s = paneScale(2e6, 60, 1000, 500, 96);
  assert.ok(s.freq && s.time);
  assert.ok(Math.abs(s.freq.px * (2e6 / 1000) - s.freq.value) < 1e-6 * s.freq.value);
  assert.ok(Math.abs(s.time.px * (60 / 500) - s.time.value) < 1e-9);
});

test("T-996: the budget stays small at a phone width (the 400 px case)", () => {
  assert.equal(scaleBudgetPx(1280), 96);
  assert.equal(scaleBudgetPx(400), 96);
  assert.equal(scaleBudgetPx(240), 60);
  assert.ok(scaleBudgetPx(120) <= 32 + 1);
  // ...and a bar chosen under it never runs past a quarter of the pane at that width.
  const bar = freqScaleBar(2e6, 400, scaleBudgetPx(400));
  assert.ok(bar && bar.px <= 100, `${bar?.px} px of a 400 px pane`);
});

test("T-996: niceBar refuses a ladder it cannot fit rather than rounding up", () => {
  assert.equal(niceBar(10, 1, [100, 200], String), null, "it took a step 10× its budget");
  const b = niceBar(1, 100, [1, 10, 100, 1000], String);
  assert.deepEqual(b, { px: 100, value: 100, label: "100" });
});

test("T-1006 x T-996: the pane's device pill rides its scale block, and the selector is STATE", () => {
  // T-996 retired the per-viewport row T-1006 stated the pill on; the pane's own scale block is the
  // per-pane "what am I looking at" place that remains, so the pill and `data-device` go there.
  const pane = { id: "p1", f0Hz: 100e6, f1Hz: 102e6, t0Ns: 0, t1Ns: 10e9, rect: { x: 0, y: 0, w: 800, h: 600 } };
  const base = { tier: "detail", following: true, tierLabel: "detail", levelLabel: "L0", freqLabel: "101 MHz", timeLabel: "LIVE", counts: "" };
  const pinned = scaleMarkOf(pane, { ...base, device: { device: "mock:a", label: "mock a", why: "mock:a's coverage alone", stale: false } }, 600, 1)!;
  assert.deepEqual(pinned.device, { device: "mock:a", label: "mock a", why: "mock:a's coverage alone", stale: false });
  assert.equal(pinned.state.device, "mock:a", "the selector must be on the element, not only in the sentence");
  assert.equal(pinned.state.deviceStale, "false");
  const stale = scaleMarkOf(pane, { ...base, device: { device: "mock:gone", label: "gone", why: "not held", stale: true } }, 600, 1)!;
  assert.equal(stale.state.deviceStale, "true", "a pin to a front end this run does not hold must be marked");
  const none = scaleMarkOf(pane, base, 600, 1)!;
  assert.equal(none.device, null, "a host that names no front end gets no pill, not an invented one");
  assert.equal(none.state.device, "");
});
