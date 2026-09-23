// T-822 / MAP-22: the measurement tool's live readout is pure arithmetic over a `MarkRegion`
// (already-ordered surface coordinates) — no DOM, no fetch, nothing about signals. This file holds
// that arithmetic to the numbers `compute_measurement` (`crates/hk-model/src/repo/measurements.rs`)
// answers with, so the number shown while dragging is never surprised by the number the server
// saves.
import { test } from "node:test";
import assert from "node:assert/strict";
import { fmtMeasureReadout, measureReadout } from "../src/surface/measure";
import type { MarkRegion } from "../src/surface/marks";

const S_TO_NS = 1e9;

test("measureReadout: Δf and Δt are the ordered region's own spans, in Hz and seconds", () => {
  const region: MarkRegion = { f0Hz: 100e6, f1Hz: 100.5e6, t0Ns: 10 * S_TO_NS, t1Ns: 13 * S_TO_NS };
  const r = measureReadout(region);
  assert.equal(r.fLoHz, 100e6);
  assert.equal(r.fHiHz, 100.5e6);
  assert.equal(r.deltaFHz, 500e3);
  assert.equal(r.deltaTS, 3);
  // bandwidth/duration are the SAME numbers under docs/25 §4's other names — one arithmetic, two
  // labels, so a caller cannot show a bandwidth that disagrees with the Δf beside it.
  assert.equal(r.bandwidthHz, r.deltaFHz);
  assert.equal(r.durationS, r.deltaTS);
});

test("measureReadout: a stroke with no extent on one axis still reads a zero, not a missing field", () => {
  const vertical: MarkRegion = { f0Hz: 433.9e6, f1Hz: 433.9e6, t0Ns: 0, t1Ns: 2 * S_TO_NS };
  assert.equal(measureReadout(vertical).deltaFHz, 0);
  assert.equal(measureReadout(vertical).deltaTS, 2);

  const horizontal: MarkRegion = { f0Hz: 100e6, f1Hz: 100.1e6, t0Ns: 5 * S_TO_NS, t1Ns: 5 * S_TO_NS };
  assert.equal(measureReadout(horizontal).deltaFHz, 100e3);
  assert.equal(measureReadout(horizontal).deltaTS, 0);
});

test("fmtMeasureReadout: one line, MHz/kHz/Hz scaled, seconds to three decimals", () => {
  assert.equal(
    fmtMeasureReadout(measureReadout({ f0Hz: 0, f1Hz: 6.6e6, t0Ns: 0, t1Ns: 3 * S_TO_NS })),
    "Δf 6.600 MHz · Δt 3.000 s",
  );
  assert.equal(
    fmtMeasureReadout(measureReadout({ f0Hz: 0, f1Hz: 12e3, t0Ns: 0, t1Ns: S_TO_NS / 2 })),
    "Δf 12.000 kHz · Δt 0.500 s",
  );
  assert.equal(
    fmtMeasureReadout(measureReadout({ f0Hz: 0, f1Hz: 40, t0Ns: 0, t1Ns: 0 })),
    "Δf 40.0 Hz · Δt 0.000 s",
  );
});
