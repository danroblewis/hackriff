// T-341: snapping navigation to achievable capture states. The client half of the rule — the grid
// itself is the backend's (crates/hk-api/src/navigation.rs, asserted on the wire by
// crates/hk-cli/tests/api_contract.rs). These assert the arithmetic, and the two places it must
// refuse rather than guess.
import { strict as assert } from "node:assert";
import { test } from "node:test";

import {
  detailLabel, detailOf, snapCenter, snapSpan, snapState, snapTimeCell,
  type FrequencyGrid, type NavigationGrid, type TimeGrid,
} from "../src/navigation";

/** HackRF One's synthesiser grid: 30 MHz / 2^20. */
const STEP = 30e6 / 2 ** 20;

const HACKRF: FrequencyGrid = {
  device_id: "hackrf:0001", driver: "hackrf-one", controllable: true,
  ranges_hz: [[1e6, 6e9]],
  center_step: "uniform", center_step_hz: STEP,
  spans_hz: { min: 2e6, max: 20e6 },
  max_live_span_hz: 20e6,
  current: { center_hz: 100.8e6, span_hz: 2.4e6 },
};

/** A replay: it records the centre it was made at, never the grid of the device that made it. */
const REPLAY: FrequencyGrid = {
  ...HACKRF, device_id: null, driver: "sigmf-replay", controllable: false,
  center_step: "unknown", center_step_hz: null,
  spans_hz: { values: [2.4e6] }, max_live_span_hz: 2.4e6,
};

const TIME: TimeGrid = {
  tiers: [
    { level: 0, t_cell_s: 1, f_cell_hz: 6250, max_age_s: 3600 },
    { level: 1, t_cell_s: 60, f_cell_hz: 12500, max_age_s: null },
    { level: 2, t_cell_s: 900, f_cell_hz: 25000, max_age_s: null },
  ],
  min_t_cell_s: 1, max_t_cell_s: 900, latest_s: 1789300920,
};

test("an unknown tuning step snaps nothing, and is never read as 1 Hz", () => {
  assert.equal(snapCenter(REPLAY, 100.8e6), null);
  assert.equal(snapCenter(null, 100.8e6), null);
  assert.equal(snapCenter({ ranges_hz: [[1e6, 6e9]], center_step_hz: null }, 100.8e6), null);
  // The state it produces says so too: no centre, and the request is not echoed back as if it were
  // reachable.
  const s = snapState({ frequency: REPLAY, time: TIME }, 100.8e6, 2.4e6);
  assert.equal(s.centerHz, null);
  assert.deepEqual(s.snapped, []);
});

test("a centre snaps to the grid, within half a step, and stays inside the band", () => {
  const got = snapCenter(HACKRF, 100.8e6)!;
  assert.ok(Math.abs(got - 100.8e6) <= STEP / 2);
  assert.equal(Math.round(got / STEP) * STEP, got);
  // 1 Hz apart is the same achievable centre — which is why declaring a 1 Hz step would put 28
  // centres on the axis that do not exist.
  assert.equal(snapCenter(HACKRF, 100_000_000), snapCenter(HACKRF, 100_000_001));
  // Both band edges land inside the band, walked one step inward where the nearest point fell out.
  for (const hz of [1e6, 6e9, 0, 9e9]) {
    const v = snapCenter(HACKRF, hz)!;
    assert.ok(v >= 1e6 && v <= 6e9, `${hz} snapped to ${v}, outside the band`);
  }
});

test("a span snaps to a rate the device can open", () => {
  assert.equal(snapSpan(HACKRF, 2.4e6), 2.4e6);
  assert.equal(snapSpan(HACKRF, 40e6), 20e6, "clamped down to the widest window");
  assert.equal(snapSpan(HACKRF, 1e3), 2e6, "clamped up to the narrowest");
  const discrete: FrequencyGrid = { ...HACKRF, spans_hz: { values: [2.048e6, 8e6, 20e6] } };
  assert.equal(snapSpan(discrete, 7e6), 8e6, "nearest entry, not the floor");
  assert.equal(snapSpan(null, 2.4e6), null);
});

test("a time cell errs coarser, never finer", () => {
  assert.equal(snapTimeCell(TIME, 1)!.t_cell_s, 1);
  assert.equal(snapTimeCell(TIME, 30)!.t_cell_s, 1, "30 s is answered by the 1 s tier");
  assert.equal(snapTimeCell(TIME, 60)!.t_cell_s, 60);
  assert.equal(snapTimeCell(TIME, 1000)!.t_cell_s, 900);
  // Below every tier: the finest one answers, which is coarser than asked. It never invents a
  // 10 ms cell the pyramid does not hold.
  assert.equal(snapTimeCell(TIME, 0.01)!.t_cell_s, 1);
});

test("wider than the live window is overview, and the boundary is inside", () => {
  assert.equal(detailOf(HACKRF, 2.4e6), "live-iq");
  assert.equal(detailOf(HACKRF, 20e6), "live-iq", "exactly one window wide is one window");
  assert.equal(detailOf(HACKRF, 20e6 + 1), "survey-overview");
  assert.equal(detailOf(HACKRF, 100e6), "survey-overview");
  // Not knowing the window is not evidence the span fits inside it.
  assert.equal(detailOf({ ...HACKRF, max_live_span_hz: null }, 1e3), "survey-overview");
  assert.equal(detailOf(null, 1e3), "survey-overview");
});

test("snapState: the honesty test and its control", () => {
  const grid: NavigationGrid = { frequency: HACKRF, time: TIME };

  // Wider than the instantaneous bandwidth: overview, not live, with both axes named as moved.
  const wide = snapState(grid, 100.8e6, 40e6);
  assert.equal(wide.source, "survey-overview");
  assert.equal(wide.spanHz, 20e6);
  assert.equal(wide.matched, false);
  assert.deepEqual(wide.snapped, ["centerHz", "spanHz"]);

  // The control, without which "everything is overview" would pass: a request already inside the
  // achievable set keeps full fidelity, is marked live-IQ-backed, and reports nothing snapped.
  const onGrid = Math.round(100.8e6 / STEP) * STEP;
  const inside = snapState(grid, onGrid, 2.4e6);
  assert.equal(inside.source, "live-iq");
  assert.equal(inside.centerHz, onGrid);
  assert.equal(inside.spanHz, 2.4e6);
  assert.equal(inside.matched, true);
  assert.deepEqual(inside.snapped, []);

  // Asking for a history tier is asking the pyramid, so the claim drops even for a span that fits.
  const deep = snapState(grid, onGrid, 2.4e6, 0.01);
  assert.equal(deep.source, "spectrum-history");
  assert.equal(deep.tier!.t_cell_s, 1);
  assert.deepEqual(deep.snapped, ["tCellS"]);
  // A tier that does exist is served exactly.
  assert.deepEqual(snapState(grid, onGrid, 2.4e6, 60).snapped, []);
});

test("detailLabel is presentation only", () => {
  assert.equal(detailLabel("live-iq"), "live IQ");
  assert.equal(detailLabel("spectrum-history"), "history");
  assert.equal(detailLabel("survey-overview"), "survey overview");
});
