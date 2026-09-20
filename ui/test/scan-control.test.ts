// T-452: the in-app survey sweep's client side. The backend owns every decision about what a sweep
// costs and who wins when it and the user both want the radio; this asserts the two things the
// client is actually responsible for — **the request it builds** (the T-367 lesson: a contract test
// proves the server serves a route, never that the client calls it correctly) and **what it shows
// the user before they commit the radio for the next hour**.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import {
  type ControlState, type ScanBudget, type ScanState, commitmentText, scanPanelModel,
  scanPreviewPath, scanQueryFrom,
} from "../src/controls/model";
import replayState from "./control_state_replay.json";

const budget = (over: Partial<ScanBudget> = {}): ScanBudget => ({
  steps: 401, dwell_s: 12, pass_s: 4812, revisit_s: 4812, span_hz: 5.999e9, step_span_hz: 15e6,
  duty: 12 / 4812, statement: "iterative scan: 401 steps × 12.0 s = a 4812.0 s pass …", ...over,
});

const scan = (over: Partial<ScanState> = {}): ScanState => ({
  state: "idle", available: true, unavailable_reason: null,
  plan: null, budget: null, progress: null, yielded: null, ...over,
});

const live = (s: ScanState | null): ControlState =>
  ({ ...(replayState as unknown as ControlState), live: true, audit: true, scan: s });

// ---- the request the client builds ----

test("the sweep request: MHz in the boxes, Hz on the wire, and a half-stated range sends neither end", () => {
  assert.deepEqual(scanQueryFrom({ loMHz: "88", hiMHz: "108", dwellS: "12" }), {
    f_lo_hz: 88e6, f_hi_hz: 108e6, dwell_s: 12,
  });
  // A half-stated range is not a range. The client must not invent the other end — the server has
  // to see the half request and refuse it, or see none at all.
  assert.deepEqual(scanQueryFrom({ loMHz: "88", hiMHz: "", dwellS: "12" }), { dwell_s: 12 });
  assert.deepEqual(scanQueryFrom({ loMHz: "", hiMHz: "108", dwellS: "12" }), { dwell_s: 12 });
  // Blank boxes are a real answer — "everything this front end can tune" — not a missing one.
  assert.deepEqual(scanQueryFrom({ loMHz: "", hiMHz: "", dwellS: "" }), {});
  // Nonsense is dropped rather than sent as NaN.
  assert.deepEqual(scanQueryFrom({ loMHz: "x", hiMHz: "y", dwellS: "-3" }), {});
});

test("the preview asks the server to price it, and never to start it", () => {
  assert.equal(
    scanPreviewPath(scanQueryFrom({ loMHz: "88", hiMHz: "108", dwellS: "12" })),
    "/api/control/scan?f_lo_hz=88000000&f_hi_hz=108000000&dwell_s=12",
  );
  assert.equal(scanPreviewPath({}), "/api/control/scan");
  assert.equal(scanPreviewPath({ dwell_s: 15 }), "/api/control/scan?dwell_s=15");
});

// ---- what the user sees before they commit ----

test("the commitment line states the pass length and the duty, in words a person reads", () => {
  const t = commitmentText(budget());
  assert.match(t, /401 steps × 12 s/);
  assert.match(t, /80 min per pass/, "4812 s is 80 minutes, and that is the number being committed to");
  assert.match(t, /duty 0\.249 %/, "a sub-1% duty keeps its precision instead of rounding to 0.0 %");
  // A short pass reads in seconds, not as "0.0 h".
  assert.match(commitmentText(budget({ steps: 4, dwell_s: 12, pass_s: 48, revisit_s: 48, duty: 0.25 })), /48 s per pass/);
});

test("nothing is priced until the server has priced it — the line is never a guess", () => {
  const m = scanPanelModel(live(scan()), null);
  assert.equal(m.commitment, "", "no proposal, no claim about what a sweep costs");
  assert.equal(m.statement, "");
  const priced = scanPanelModel(live(scan()), {
    plan: { f_lo_hz: 1e6, f_hi_hz: 6e9, dwell_s: 12, recommended_dwell: true, sample_rate_hz: 20e6, steps: 401, warnings: [] },
    budget: budget(),
  });
  assert.match(priced.commitment, /401 steps/);
  assert.equal(priced.statement, budget().statement, "T-406's own sentence, verbatim");
});

test("an unusual dwell and a clipped range are said, not silently accepted", () => {
  const m = scanPanelModel(live(scan()), {
    plan: {
      f_lo_hz: 0, f_hi_hz: 8e9, dwell_s: 2, recommended_dwell: false, sample_rate_hz: 20e6, steps: 400,
      warnings: ["part of that range is outside the front end's tunable ranges and was clipped; the scan covers only what the device can reach"],
    },
    budget: budget({ dwell_s: 2 }),
  });
  assert.equal(m.notes.length, 2, m.notes.join(" | "));
  assert.match(m.notes.join(" "), /clipped/);
  assert.match(m.notes.join(" "), /outside the 10-30 s/, "a dwell outside the range still runs and says so — never clamped");
});

// ---- the arbitration, as the user sees it ----

test("a yield is never silent: the panel says what took the radio and offers to resume", () => {
  const yielded = scan({
    state: "yielded",
    plan: { f_lo_hz: 88e6, f_hi_hz: 108e6, dwell_s: 12, recommended_dwell: true, sample_rate_hz: 2.4e6, steps: 12, warnings: [] },
    budget: budget({ steps: 12, pass_s: 144, revisit_s: 144, duty: 1 / 12 }),
    progress: { step: 3, steps: 12, pass: 0, steps_done: 3, center_hz: 96.1e6, started_s: 1, step_started_s: 2, next_step_in_s: 4 },
    yielded: { to: "retune", at_s: 9, step: 3, detail: "an explicit retune took the front end; the sweep stopped at step 4 of 12 and kept its place" },
  });
  const m = scanPanelModel(live(yielded));
  assert.equal(m.state, "yielded");
  assert.match(m.yieldText, /an explicit retune took the front end/);
  assert.match(m.yieldText, /Resume to continue/, "a yielded sweep is resumable, and the panel says how");
  assert.deepEqual(
    { label: m.primary.label, action: m.primary.action, enabled: m.primary.enabled },
    { label: "Resume sweep", action: "resume", enabled: true },
  );
  assert.equal(m.stopEnabled, true);
  // While it is stopped it still shows the pass it is part-way through, not the idle nothing.
  assert.match(m.commitment, /12 steps/);
});

test("a running sweep shows where it is, and its primary button cannot start a second one", () => {
  const running = scan({
    state: "running",
    plan: { f_lo_hz: 88e6, f_hi_hz: 108e6, dwell_s: 12, recommended_dwell: true, sample_rate_hz: 2.4e6, steps: 12, warnings: [] },
    budget: budget({ steps: 12, pass_s: 144, revisit_s: 144, duty: 1 / 12 }),
    progress: { step: 5, steps: 12, pass: 1, steps_done: 17, center_hz: 99.7e6, started_s: 1, step_started_s: 2, next_step_in_s: 7 },
  });
  const m = scanPanelModel(live(running));
  assert.match(m.progressText, /step 6 of 12/, "1-based for a person, 0-based on the wire");
  assert.match(m.progressText, /pass 2/);
  assert.match(m.progressText, /tuned 99\.700 MHz/);
  assert.equal(m.primary.enabled, false, "two sweeps over one front end are two policies fighting");
  assert.equal(m.stopEnabled, true, "stopping must always be possible");
  assert.equal(m.yieldText, "");
});

test("a source that cannot be swept disables the control WITH ITS REASON, never hiding it", () => {
  // A replay: the server sends no `scan` at all.
  const replay = scanPanelModel({ ...(replayState as unknown as ControlState), scan: null });
  assert.equal(replay.gate.enabled, false);
  assert.match(replay.gate.reason, /not_live/);
  // A live source that states no tunable range: the server's own reason is shown verbatim.
  const reason = "this front end states no tunable range, so there is nothing to sweep";
  const m = scanPanelModel(live(scan({ available: false, unavailable_reason: reason })));
  assert.equal(m.gate.enabled, false);
  assert.equal(m.gate.reason, reason);
  assert.equal(m.primary.enabled, false);
  assert.equal(m.stopEnabled, false);
});

// ---- it is mounted, and it holds no signal logic ----

test("the sweep control is mounted in the SDR panel, and prices through the server", () => {
  const src = readFileSync("src/app/review/device.ts", "utf8");
  // A control nobody can see is not a control (the T-409 rule).
  assert.match(src, /this\.scanFields/, "the fieldset must be in the panel's root");
  assert.match(src, /this\.deviceFields, this\.deviceReason, this\.scanFields/);
  // Start, resume and stop all go to the server's routes; nothing steps the tune from here.
  for (const r of ["/api/control/scan", "/api/control/scan/stop"]) {
    assert.ok(src.includes(r), `the panel must call ${r}`);
  }
  // The pass length, the hop tiling and the duty are the backend's; the client asks and renders.
  const model = readFileSync("src/controls/model.ts", "utf8");
  for (const f of ["usable_fraction", "region_dwell", "hop_", "IterativeScan"]) {
    assert.ok(!model.includes(f), `the client must not re-derive the sweep's plan (${f})`);
  }
});

// ---- T-517: the step width ----

test("the step width is on the wire exactly as chosen, on both the price and the start", () => {
  const coarse = scanQueryFrom({ loMHz: "", hiMHz: "", dwellS: "0.5", step: "coarse" });
  // The start POSTs this same object (device.ts `onScanStart` sends `this.scanQuery()`).
  assert.deepEqual(coarse, { dwell_s: 0.5, step: "coarse" });
  assert.equal(scanPreviewPath(coarse), "/api/control/scan?dwell_s=0.5&step=coarse");
  assert.deepEqual(
    scanQueryFrom({ loMHz: "88", hiMHz: "108", dwellS: "12", step: "fine" }),
    { f_lo_hz: 88e6, f_hi_hz: 108e6, dwell_s: 12, step: "fine" },
  );
  // Anything the server does not name is left to its default rather than sent.
  assert.deepEqual(scanQueryFrom({ loMHz: "", hiMHz: "", dwellS: "", step: "medium" }), {});
  assert.deepEqual(scanQueryFrom({ loMHz: "", hiMHz: "", dwellS: "" }), {});
});

test("the price line shows the chosen step, so coarse visibly = fast, at the same bin width", () => {
  const plan = (step: "fine" | "coarse", rate: number, steps: number) => ({
    f_lo_hz: 1e6, f_hi_hz: 6e9, dwell_s: 0.5, recommended_dwell: false, sample_rate_hz: rate,
    rate_in_force_hz: 2.4e6, changes_rate: rate !== 2.4e6, step, bin_hz: 4687.5, steps, warnings: [],
  });
  const fine = scanPanelModel(live(scan()), {
    plan: plan("fine", 2.4e6, 3334),
    budget: budget({ steps: 3334, dwell_s: 0.5, pass_s: 1667, revisit_s: 1667, step_span_hz: 1.8e6, duty: 1 / 3334 }),
  });
  const coarse = scanPanelModel(live(scan()), {
    plan: plan("coarse", 19.2e6, 418),
    budget: budget({ steps: 418, dwell_s: 0.5, pass_s: 209, revisit_s: 209, step_span_hz: 14.4e6, duty: 1 / 418 }),
  });
  assert.match(fine.commitment, /^fine steps of 1\.8 MHz \(bins 4\.69 kHz either way\): 3334 steps × 0\.5 s = 28 min per pass/);
  assert.match(coarse.commitment, /^coarse steps of 14\.4 MHz \(bins 4\.69 kHz either way\): 418 steps × 0\.5 s = 3 min per pass/);
  // A coarse step that changes the span says so before the button.
  assert.ok(coarse.notes.some((n) => /sets the span to 19\.2 MHz \(from 2\.4 MHz\)/.test(n)), coarse.notes.join(" | "));
  assert.ok(!fine.notes.some((n) => /sets the span/.test(n)));
});

test("the step control sits in the sweep fieldset beside From/To/Dwell and reprices on change", () => {
  const src = readFileSync("src/app/review/device.ts", "utf8");
  assert.match(src, /"Dwell ", this\.scanDwell\),\s*h\("label"[^\n]*"Step ", this\.scanStep\)/);
  assert.match(src, /scanStep = h\("select", \{ onchange: \(\) => void this\.priceScan\(\) \}/);
  assert.match(src, /step: \(this\.scanStep as HTMLSelectElement\)\.value/);
});
