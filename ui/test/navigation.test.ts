// T-341: snapping navigation to achievable capture states. The client half of the rule — the grid
// itself is the backend's (crates/hk-api/src/navigation.rs, asserted on the wire by
// crates/hk-cli/tests/api_contract.rs). These assert the arithmetic, and the two places it must
// refuse rather than guess.
import { strict as assert } from "node:assert";
import { test } from "node:test";

import {
  containsCenter, dcOffsetHz, detailLabel, detailOf, detailPlan, retunePlan, smallestCoveringSpan,
  snapCenter, snapSpan, snapState, snapTimeCell,
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

// ---------------------------------------------------------------------------
// T-392: the capture configuration a region-select commands
// ---------------------------------------------------------------------------

test("T-392: containsCenter asks whether the grid COVERS a centre — snapCenter answers a different question", () => {
  // snapCenter walks an out-of-band request inward to the nearest reachable point, so "did it
  // snap?" calls every out-of-range centre achievable at the band edge. That is exactly the
  // over-permissive reading a refusal must not be built on.
  assert.equal(snapCenter(HACKRF, 7e9), 6e9 - (6e9 % STEP) + 0); // pulled inside the band
  assert.ok(snapCenter(HACKRF, 7e9)! <= 6e9);
  assert.equal(containsCenter(HACKRF, 7e9), false, "…but 7 GHz is not covered");
  assert.equal(containsCenter(HACKRF, 6e9), true, "the boundary is covered");
  assert.equal(containsCenter(HACKRF, 1e6), true);
  assert.equal(containsCenter(HACKRF, 0.9e6), false);
  assert.equal(containsCenter(null, 100e6), false);
  // Disjoint bands: the gap between them is not covered, however near either edge it is.
  const split = { ranges_hz: [[24e6, 1.766e9], [3e9, 6e9]] as [number, number][], center_step_hz: 1 };
  assert.equal(containsCenter(split, 2e9), false);
  assert.equal(containsCenter(split, 1.766e9), true);
});

test("T-392: smallestCoveringSpan takes the least entry that COVERS — never the nearest", () => {
  const ladder: FrequencyGrid = { ...HACKRF, spans_hz: { values: [20e6, 2e6, 10e6, 8e6] }, max_live_span_hz: 20e6 };
  // 3 MHz: 8, 10 and 20 all cover. The *nearest* entry is 2 MHz, which does not — picking it would
  // open a window that cuts the selection in half.
  assert.equal(smallestCoveringSpan(ladder, 3e6), 8e6);
  assert.equal(snapSpan(ladder, 3e6), 2e6, "the nearest entry is the wrong answer here, by construction");
  // The boundary covers: a window exactly as wide as the region contains it.
  assert.equal(smallestCoveringSpan(ladder, 8e6), 8e6);
  assert.equal(smallestCoveringSpan(ladder, 8e6 + 1), 10e6);
  // Narrower than every entry: the ladder's floor. Wider than every entry: nothing covers it.
  assert.equal(smallestCoveringSpan(ladder, 1), 2e6);
  assert.equal(smallestCoveringSpan(ladder, 20e6 + 1), null);
  // A continuous rate range takes the need itself, lifted to the floor.
  assert.equal(smallestCoveringSpan(HACKRF, 400e3), 2e6);
  assert.equal(smallestCoveringSpan(HACKRF, 5e6), 5e6);
  assert.equal(smallestCoveringSpan(HACKRF, 20e6), 20e6);
  assert.equal(smallestCoveringSpan(HACKRF, 20e6 + 1), null);
  assert.equal(smallestCoveringSpan(null, 1e6), null);
  assert.equal(smallestCoveringSpan(HACKRF, 0), null);
});

test("T-392: retunePlan computes the covering config, and refuses only when nothing can capture", () => {
  // The ordinary case: centre on the synthesiser grid, smallest covering span, live-IQ detail.
  const p = retunePlan(HACKRF, 432.0e6, 432.4e6);
  assert.equal(p.ok, true);
  if (!p.ok) return;
  assert.equal(p.spanHz, 2e6);
  assert.equal(p.source, "live-iq");
  assert.equal(p.snappedCenter, true);
  assert.ok(Math.abs(p.centerHz / STEP - Math.round(p.centerHz / STEP)) < 1e-6);
  // T-418: the centre is NOT the region's centre — that would put the target on the tuner's own
  // DC/LO leakage spike, which is the one part of the window it cannot be cleanly demodulated on.
  // It is `span/4` below it (`dcOffsetHz`), to within the snap.
  assert.ok(Math.abs(p.dcOffsetHz - p.spanHz / 4) <= STEP, `offset ${p.dcOffsetHz} is not span/4`);
  assert.ok(Math.abs(p.centerHz - (432.2e6 - p.spanHz / 4)) <= STEP);
  assert.equal(p.clearsDc, true, "DC must fall outside the selection");
  // The window it opens contains the whole selection — the point of "covering", and it still holds
  // with the selection slid off centre, which is the thing the offset must not break.
  assert.ok(p.centerHz - p.spanHz / 2 <= 432.0e6 && p.centerHz + p.spanHz / 2 >= 432.4e6);

  // Refusal 1: the centre is outside every band. Not "snap it to the edge and go".
  assert.deepEqual(retunePlan(HACKRF, 6.1e9, 6.2e9), { ok: false, reason: "center_out_of_range" });
  // Adjacent: a centre inside the range retunes, even when the window it opens runs past the edge.
  assert.equal(retunePlan(HACKRF, 6e9 - 0.2e6, 6e9).ok, true);

  // Refusal 2: wider than one live window.
  assert.deepEqual(retunePlan(HACKRF, 422e6, 462e6), { ok: false, reason: "span_too_wide" });
  // Adjacent: exactly one window wide is capturable, and the snap's fraction of a step does not
  // turn that into a refusal.
  const edge = retunePlan(HACKRF, 432e6 - 10e6, 432e6 + 10e6);
  assert.equal(edge.ok, true);
  assert.equal(edge.ok && edge.spanHz, 20e6);

  // Refusal 3 is the same check on a replay, whose band IS the recording's extent.
  const rec: FrequencyGrid = { ...REPLAY, ranges_hz: [[99e6, 101e6]] };
  assert.deepEqual(retunePlan(rec, 432e6, 432.4e6), { ok: false, reason: "center_out_of_range" });
  const inRec = retunePlan(rec, 99.5e6, 99.9e6);
  assert.equal(inRec.ok, true);
  // An unknown step snaps nothing, and the plan says so rather than claiming a grid point.
  assert.equal(inRec.ok && inRec.snappedCenter, false);
  assert.equal(inRec.ok && inRec.centerHz, 99.7e6);

  // No grid at all: nothing may be claimed achievable.
  assert.deepEqual(retunePlan(null, 100e6, 101e6), { ok: false, reason: "no_grid" });
  assert.deepEqual(retunePlan(HACKRF, 100e6, 100e6), { ok: false, reason: "no_grid" });

  // A coarse tuning grid moves the centre enough to matter, and the span is computed from where
  // the radio will actually sit — not from the centre that was asked for.
  const coarse: FrequencyGrid = { ...HACKRF, center_step_hz: 1e6, spans_hz: { min: 1, max: 20e6 } };
  const c = retunePlan(coarse, 432.4e6, 432.8e6);
  assert.equal(c.ok, true);
  if (!c.ok) return;
  assert.equal(c.centerHz, 433e6);
  assert.equal(c.spanHz, 1.2e6, "0.4 MHz would not reach the selection from 433 MHz");
  assert.ok(c.centerHz - c.spanHz / 2 <= 432.4e6 && c.centerHz + c.spanHz / 2 >= 432.8e6);
});

// ---------------------------------------------------------------------------
// T-418: a narrow selection raises RESOLUTION; it never asks for an impossible narrow capture.
// ---------------------------------------------------------------------------
//
// **The user's principle, verbatim:** *"Detail on a narrow view comes from resolution/decimation,
// never from an impossible narrow capture."*
//
// The device limit that forces it: a HackRF's minimum sample rate is 2 Msps, so **a sub-2-MHz
// window cannot be captured**. `smallestCoveringSpan` bottoming out at that floor is the right
// answer, not a gap — and it means every hertz of detail below the floor has to come from the
// transform instead.

/** The FFT ladder this repo's pipeline serves (`hk-pipeline`'s `DISPLAY_FFT_MIN`/`MAX`, reported
 * on the wire as `display_limits` — never hard-coded by the client, which is why it is a value
 * here and a parameter there). */
const FFT = { fft_size_min: 64, fft_size_max: 65_536 };

test("T-418 THE MEASUREMENT: a sub-2-MHz region already snapped to the 2 Msps floor — the missing half was resolution", () => {
  // Half one was never the gap. `smallestCoveringSpan` has answered 2 MHz for a narrow region since
  // T-392: a 200 kHz selection cannot be captured any more tightly, and the floor IS the answer.
  assert.equal(smallestCoveringSpan(HACKRF, 200e3), 2e6);
  assert.equal(smallestCoveringSpan(HACKRF, 20e3), 2e6, "even a 20 kHz selection opens the same window");
  assert.equal(smallestCoveringSpan(HACKRF, 2e3), 2e6);

  // So at a fixed transform, zooming in bought NOTHING: the same Hz/bin, drawn wider. This is the
  // bug as the user reported it — a coarse ~500 Hz/bin view that stayed coarse however far in they
  // selected. The three selections below differ by 100x in width and not at all in resolution.
  const fixed = 4096;
  for (const w of [200e3, 20e3, 2e3]) {
    assert.equal(smallestCoveringSpan(HACKRF, w)! / fixed, 2e6 / 4096);
  }

  // The fix: the transform moves instead, and it moves BECAUSE the window cannot.
  const wide = detailPlan(FFT, 2e6, 200e3, 1200)!;
  const tight = detailPlan(FFT, 2e6, 20e3, 1200)!;
  assert.ok(tight.binHz < wide.binHz, "a tighter selection must resolve finer, not merely draw wider");
  assert.ok(tight.fftSize > wide.fftSize, "…and it does so by measuring more, not by stretching less");
});

test("T-418 half one: the off-DC offset is DERIVED — span/4, bounded by the slack and by the snap", () => {
  // The ideal is the midpoint of the usable half-band: the LO spike sits at DC (0) and the
  // anti-alias roll-off at Nyquist (span/2), so the point maximally far from both is span/4. It is
  // reached whenever the slack allows it, at any span.
  assert.equal(dcOffsetHz(2e6, 200e3, null), 0.5e6);
  assert.equal(dcOffsetHz(20e6, 1e6, null), 5e6);
  assert.equal(dcOffsetHz(8e6, 10e3, null), 2e6);

  // Bound one: the selection must stay inside the window, so the offset cannot exceed the slack.
  // A 1.5 MHz selection in a 2 MHz window has only 250 kHz of room, well short of the 500 kHz ideal.
  assert.equal(dcOffsetHz(2e6, 1.5e6, null), 250e3);
  // At the extreme the window IS the selection and there is nothing to slide.
  assert.equal(dcOffsetHz(2e6, 2e6, null), 0);
  // Bound two: the snap afterwards moves the centre by up to half a step, which comes out of the
  // same slack — otherwise the selection's far edge leaves the window the snap just chose.
  assert.equal(dcOffsetHz(2e6, 1.5e6, 100e3), 250e3 - 50e3);
  // An unknown step subtracts nothing, because nothing is snapped then.
  assert.equal(dcOffsetHz(2e6, 1.5e6, null), 250e3);

  // Degenerate inputs offset nothing rather than inventing a placement.
  assert.equal(dcOffsetHz(0, 1e3, null), 0);
  assert.equal(dcOffsetHz(NaN, 1e3, null), 0);
});

test("T-418 half one: the narrowed window keeps the target OFF the DC spike, and says so when it cannot", () => {
  // T-409 arriving from the other direction: a signal at the exact centre lands on the tuner's
  // DC/LO leakage and cannot be cleanly demodulated (T-317/T-382 measured the spike; T-394 still
  // leaves the tuner-DC box above 20 dB). So a narrowing that centred the target perfectly would
  // put it on the WORST part of the band — the offset is correctness, not polish.
  const p = retunePlan(HACKRF, 101.2e6, 101.4e6);
  assert.equal(p.ok, true);
  if (!p.ok) return;
  assert.equal(p.spanHz, 2e6, "the narrowest window the device can open");
  assert.ok(Math.abs(p.dcOffsetHz - 0.5e6) <= STEP, "…opened deliberately lopsided");
  assert.equal(p.clearsDc, true);
  // DC is the window's centre, and it lands outside the selection by the offset less its half-width.
  assert.ok(p.centerHz < 101.2e6 || p.centerHz > 101.4e6);
  // And the selection is still wholly inside the window — the offset must not cost coverage.
  assert.ok(p.centerHz - p.spanHz / 2 <= 101.2e6 && p.centerHz + p.spanHz / 2 >= 101.4e6);

  // THE CONTROL: a selection too wide a fraction of the narrowest window has NO placement that
  // clears DC of it. The window is not widened to buy the dodge — that would throw away exactly the
  // resolution this path exists to gain — so the plan says `clearsDc: false` rather than implying a
  // clean window, and the readout tells the user.
  const wide = retunePlan(HACKRF, 100.5e6, 102.0e6);
  assert.equal(wide.ok, true);
  if (!wide.ok) return;
  assert.equal(wide.spanHz, 2e6, "still the narrowest covering window — never widened for the dodge");
  assert.equal(wide.clearsDc, false);
  assert.ok(wide.centerHz >= 100.5e6 && wide.centerHz <= 102.0e6, "DC is inside the selection, and admitted");

  // A recording has no oscillator of ours leaking into it, so there is nothing to dodge and the
  // centre is not moved. Derived from `controllable`, not from a special case for replays.
  const rec: FrequencyGrid = { ...REPLAY, ranges_hz: [[99e6, 101e6]] };
  const r = retunePlan(rec, 99.5e6, 99.9e6);
  assert.equal(r.ok && r.dcOffsetHz, 0);
  assert.equal(r.ok && r.centerHz, 99.7e6);
});

test("T-418 half two: finer Hz/bin is MEASURED — the smallest transform that gives a bin per pixel", () => {
  // Hz/bin is `span / fftSize`. With the span pinned at the device's floor, only the transform can
  // move — and a longer DFT resolves more because it integrated more samples, which is a
  // measurement. (Drawing the same bins wider is not, and is what the client would otherwise do:
  // `ui/src/waterfall.ts` nearest-repeats a short row across the texture.)
  const p = detailPlan(FFT, 2e6, 200e3, 1200)!;
  assert.equal(p.binHz, 2e6 / p.fftSize, "Hz/bin is the transform's, by construction");
  assert.ok(p.binsAcross >= 1200, "at least one measured bin per pixel of the selection");
  assert.equal(p.capped, false);
  // Smallest that suffices, like `smallestCoveringSpan`: one rung down would leave fewer bins than
  // pixels, so nothing shorter would have done.
  assert.ok((p.fftSize / 2) * (200e3 / 2e6) < 1200, `${p.fftSize / 2} would not have been enough`);
  assert.ok(Number.isInteger(Math.log2(p.fftSize)), "a power of two, as the route requires");

  // The narrower the slice, the longer the transform — this is the whole relationship the bug broke.
  const sizes = [400e3, 200e3, 100e3, 50e3].map((w) => detailPlan(FFT, 2e6, w, 1200)!.fftSize);
  for (let i = 1; i < sizes.length; i++) assert.ok(sizes[i] > sizes[i - 1], `${sizes}`);

  // THE HONESTY CONTROL: the ladder has a top. Past it the selection gets FEWER bins than pixels
  // and the client repeats them to fill the gap — so the plan says `capped`, and the readout says
  // it in words. A view never implies detail the front end did not capture.
  const tiny = detailPlan(FFT, 2e6, 1e3, 1200)!;
  assert.equal(tiny.fftSize, FFT.fft_size_max, "pinned at the longest transform on offer");
  assert.equal(tiny.capped, true);
  assert.ok(tiny.binsAcross < 1200, "fewer measured bins than pixels — repeated, and declared");
  // Capped still means *measured at the cap*, never interpolated: the bins it does report are real.
  assert.equal(tiny.binHz, 2e6 / FFT.fft_size_max);

  // Unknown bounds claim nothing — the same discipline as a null tuning step. Not knowing the
  // limit is not permission to invent one.
  assert.equal(detailPlan(null, 2e6, 200e3, 1200), null);
  assert.equal(detailPlan(FFT, 0, 200e3, 1200), null);
  // A slice wider than the window is the window: nothing outside it was captured.
  assert.equal(detailPlan(FFT, 2e6, 9e6, 1200)!.binsAcross, detailPlan(FFT, 2e6, 2e6, 1200)!.binsAcross);
});

test("T-418: BOTH halves together beat either alone, and the gain is real at every step", () => {
  // The reported situation: tuned at 101.3 MHz on a 2.4 MHz window with a 4096-point transform.
  const before = 2.4e6 / 4096;
  const p = retunePlan(HACKRF, 101.2e6, 101.4e6);
  assert.equal(p.ok, true);
  if (!p.ok) return;
  const d = detailPlan(FFT, p.spanHz, 200e3, 1200)!;

  // Half one alone (narrow the window, keep the transform) barely moves: 2.4 -> 2.0 MHz is 1.2x,
  // because the device floor is right there. This is why the task is not "just request a smaller
  // span" — there is no smaller span to request.
  assert.ok(before / (p.spanHz / 4096) < 1.25);
  // Half two is where the detail comes from, and together they are the answer the user asked for.
  assert.ok(d.binHz < before / 4, `${d.binHz} Hz/bin is not meaningfully finer than ${before}`);
});
