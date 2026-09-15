// T-151 (ADR-0013 §4.5): pure formatting helpers for the Explore focus panel. Every case checks
// that the text is built only from fields the API already serves — nothing here invents a signal
// fact. No DOM under node:test.
import { test } from "node:test";
import assert from "node:assert/strict";
import { apiErrorText, explanationWhy, fmtBandwidth, fmtMHz, fmtPct, rasterText, refinedNote } from "../src/app/explore/format";
import type { ExplanationEvidence, RefinedTuning } from "../src/app/explore/inventory";

test("fmtMHz / fmtBandwidth / fmtPct", () => {
  assert.equal(fmtMHz(101_300_000), "101.3000");
  assert.equal(fmtMHz(101_300_000, 2), "101.30");
  assert.equal(fmtBandwidth(183_000), "183 kHz");
  assert.equal(fmtBandwidth(2_400_000), "2.40 MHz");
  assert.equal(fmtBandwidth(12_400_000), "12.4 MHz");
  assert.equal(fmtPct(0.42), "42%");
});

test("apiErrorText: server code appended, else the plain message", () => {
  assert.equal(apiErrorText({ code: "not_found", message: "no such entry" }), "no such entry (not_found)");
  assert.equal(apiErrorText(new Error("boom")), "boom");
  assert.equal(apiErrorText("boom"), "boom");
});

const bandPlan = (reason: string): ExplanationEvidence => ({ kind: "band-plan", status: "known", prior_ref: "band-plan/us-fm@1", reason });
const raster = (offsetHz: number, onRaster: boolean): ExplanationEvidence => ({
  kind: "raster", raster_hz: 200_000, nearest_channel_hz: 101_300_000, offset_hz: offsetHz, tolerance_hz: 5_000, on_raster: onRaster, source: "us-fm", center_source: "detected",
});
const family = (confidence: number): ExplanationEvidence => ({ kind: "family", family: "wfm-broadcast", model_version: "v1", confidence, mapping_confidence: 0.9 });

test("explanationWhy: joins only the evidence's own fields", () => {
  assert.equal(explanationWhy([bandPlan("on FM broadcast allocation")]), "on FM broadcast allocation");
  assert.equal(explanationWhy([raster(0, true)]), "on the 200 kHz raster");
  assert.equal(explanationWhy([raster(49_000, false)]), "49 kHz off the 200 kHz raster");
  assert.equal(explanationWhy([family(0.83)]), "classifier confidence 83%");
  assert.equal(explanationWhy([bandPlan("on FM broadcast allocation"), raster(0, true)]), "on FM broadcast allocation; on the 200 kHz raster");
  assert.equal(explanationWhy([]), "");
});

test("rasterText: on/off raster, and '—' without raster evidence", () => {
  assert.equal(rasterText([raster(0, true)]), "on raster");
  assert.equal(rasterText([raster(-49_000, false)]), "49 kHz off raster");
  assert.equal(rasterText([bandPlan("x")]), "—");
  assert.equal(rasterText([]), "—");
});

const refined = (over: Partial<RefinedTuning> = {}): RefinedTuning => ({
  emitter_id: "e1", provenance: "refined by output analysis", objective: "hk-demod/wfm-output@1",
  mode: "wfm", source: "listen", center_hz: 101_300_400, bandwidth_hz: 183_000,
  start_center_hz: 101_300_000, start_bandwidth_hz: 150_000, detected_center_hz: 101_300_000,
  detected_bandwidth_hz: 150_000, objective_value: 0.9, locked: true, converged: true, t_s: 1_789_300_900,
  ...over,
});

test("refinedNote: interim text before a refinement, and a plain fact after one", () => {
  assert.equal(refinedNote(null), "Centre from detection; not refined yet");
  assert.equal(refinedNote(refined()), "Refined from the wfm output");
  assert.equal(refinedNote(refined({ converged: false })), "Refined from the wfm output (not yet converged)");
});
