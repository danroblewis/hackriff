// T-123: attention scheduler panel (docs/api.md "Attention scheduler"). No DOM under node:test:
// pure query/formatting functions and the thin GET wrappers are tested directly, including the
// "POI only with a span" query rule and the `scheduler: null` (no scheduler on this run) shape.
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  bandit_summaryText, loadArms, loadScheduler, schedulerQuery, sharesText,
  type ArmsResponse, type SchedulerClient, type SchedulerResponse,
} from "../src/scheduler";

// ---- query building ----

test("schedulerQuery with nothing given hits the bare status path", () => {
  assert.equal(schedulerQuery({}), "/api/scheduler");
});

test("schedulerQuery carries region, span and tau_s", () => {
  const q = schedulerQuery({ fLoHz: 433e6, fHiHz: 435e6, t0: 1789296400, t1: 1789300000, tauS: [0.1, 1, 10] });
  const p = new URLSearchParams(q.split("?")[1]);
  assert.equal(p.get("f_lo"), "433000000");
  assert.equal(p.get("f_hi"), "435000000");
  assert.equal(p.get("t0"), "1789296400");
  assert.equal(p.get("t1"), "1789300000");
  assert.equal(p.get("tau_s"), "0.1,1,10");
});

test("schedulerQuery omits t0/t1 when no span is given: a bare poll never asks for POI (docs/api.md)", () => {
  const q = schedulerQuery({ fLoHz: 433e6, fHiHz: 435e6 });
  assert.ok(!q.includes("t0="), q);
  assert.ok(!q.includes("t1="), q);
});

// ---- formatting ----

test("sharesText names every tier share", () => {
  const t = sharesText({ discovery: 150.2, exploit: 40.1, explore: 8.3, other: 0 });
  assert.match(t, /discovery 150\.2s/);
  assert.match(t, /exploit 40\.1s/);
  assert.match(t, /explore 8\.3s/);
  assert.match(t, /other 0\.0s/);
});

test("bandit_summaryText summarises arms/banned/pending/dwell/version", () => {
  const t = bandit_summaryText({
    provider_version: 7, arms: 12, active_arms: 9, pending_verifications: 0, banned: 1, total_dwell_s: 48.4,
    config: {}, counters: { repacks: 0, outcomes: 0, outcomes_unmatched: 0, exploit_dwells: 0, explore_dwells: 0, stale_forced: 0, beacon_dwells: 0, verifications_started: 0, verifications_dropped: 0, verifications_passed: 0, verifications_failed: 0, floor_deferrals: 0, arms_dropped: 0, suspect_wasted_s: 0 },
  });
  assert.match(t, /9\/12 active/);
  assert.match(t, /1 banned/);
  assert.match(t, /48\.4s total dwell/);
  assert.match(t, /v7/);
});

// ---- API calls, including the "scheduler: null" (no scheduler) shape ----

function mockSchedulerResponse(): SchedulerResponse {
  return {
    scheduler: {
      now: 1789300000.5, plan_version: 1, window_s: 600,
      shares_s: { discovery: 150.2, exploit: 40.1, explore: 8.3, other: 0 },
      sweep_floor: 0.25, sweep_floor_met: true, floor_violations: 0,
      interactive: false, leases: 0, scheduled: 0, low_power: false,
      bandit: { provider_version: 7, arms: 12, active_arms: 9, pending_verifications: 0, banned: 0, total_dwell_s: 48.4, config: {}, counters: { repacks: 7, outcomes: 31, outcomes_unmatched: 0, exploit_dwells: 20, explore_dwells: 9, stale_forced: 0, beacon_dwells: 2, verifications_started: 0, verifications_dropped: 0, verifications_passed: 0, verifications_failed: 0, floor_deferrals: 3, arms_dropped: 0, suspect_wasted_s: 0 } },
    },
    leases: [{ id: 1, kind: "user-pin", center_hz: 433920000, rate_hz: 2000000, duration_s: null }],
    observation_log: true,
    span: { t0: 1789296400.5, t1: 1789300000.5 },
    poi: [{ f_lo: 433000000, f_hi: 435000000, cell_hz: 1000000, cells: 2, observed_cells: 2, observed_fraction: 0.31, mean_revisit_s: 1.9, poi: [{ tau_s: 0.1, p_poi: 0.36, p_poi_min: 0.34 }], gap_threshold_s: 3.8, gaps: [{ f_lo: 433000000, f_hi: 434000000, t0: 1789296400.5, t1: 1789296900 }], gaps_truncated: false }],
    poi_truncated: false,
  };
}

test("loadScheduler GETs the built query and returns the response", async () => {
  const calls: string[] = [];
  const client: SchedulerClient = { get: async (p) => { calls.push(p); return mockSchedulerResponse(); } };
  const r = await loadScheduler(client, { t0: 1789296400.5, t1: 1789300000.5 });
  assert.equal(calls.length, 1);
  assert.ok(calls[0].includes("t0="));
  assert.equal(r.scheduler?.bandit?.arms, 12);
  assert.equal(r.poi[0].observed_fraction, 0.31);
});

test("loadScheduler tolerates scheduler: null (hk serve, no scheduler) without throwing", async () => {
  const noScheduler: SchedulerResponse = { scheduler: null, leases: [], observation_log: false, span: null, poi: [], poi_truncated: false };
  const client: SchedulerClient = { get: async () => noScheduler };
  const r = await loadScheduler(client, {});
  assert.equal(r.scheduler, null);
  assert.equal(r.span, null);
});

test("loadArms GETs /api/scheduler/arms", async () => {
  const calls: string[] = [];
  const resp: ArmsResponse = { scheduler: true, bandit: true, arms: [{ index: 0, key: {}, center_hz: 100e6, rate_hz: 2e6, active: true, exploration: false, on_dc: false, prior: 0, mean_reward: 0.1, dwell_s: 1, ucb: "inf", visits: 0, staleness_s: 0, suspect_fraction: 0, lead: null, members: 1, dwell_planned_s: 1, required_revisit_s: 1, complete_capture: false, last_reward: null }] };
  const client: SchedulerClient = { get: async (p) => { calls.push(p); return resp; } };
  const r = await loadArms(client);
  assert.equal(calls[0], "/api/scheduler/arms");
  assert.equal(r.arms.length, 1);
  assert.equal(r.arms[0].ucb, "inf");
});

// The Scheduler tab (status, leases, POI, the collapsed bandit-arms <details>) is now
// ui/src/app/review/scheduler.ts's SchedulerTab, built with h() rather than a static index.html:
// exercised manually against a running `hk serve`, like every other MUI DOM-wiring class (see
// ui/test/app-review.test.ts's top comment).
