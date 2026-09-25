// T-570 (ADR-0021 §6A): the trace panel's pure view-model and the requests it builds. No DOM under
// node:test; assertions are on the request the client builds (the T-367 guard), not only the
// response it renders, and that no trace interaction reaches a device route.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import {
  elidedText, emptyTraceText, groupByStage, nodeRegister, offersRerun, rerunDeepBody,
  filterLabel, tracePath, truncationNote, type ElidedBucket, type TraceFetch, type TraceNode,
} from "../src/app/explore/analyze-trace-panel";

const node = (over: Partial<TraceNode> = {}): TraceNode => ({
  id: "n1", parent: null, stage: "S1",
  hypothesis: { skeleton: "generic-fsk-framed@1", slot: "S1", choice: "fsk", family: "fsk" },
  seed_source: "estimate", prior_bits: 0, measured: null, evidence_bits: null,
  outcome: "pruned_floor", tried: true, evaluations: 1, cpu_ms: 1, summary: "measured 3.2 bits",
  ...over,
});

test("the trace fetch is GET /api/analyze/{id}/trace with the served filter params, never a device route", () => {
  assert.equal(tracePath("a1"), "/api/analyze/a1/trace");
  assert.equal(tracePath("a1", { family: "psk" }), "/api/analyze/a1/trace?family=psk");
  assert.equal(tracePath("a1", { stage: "S3", outcome: "pruned_floor", family: "psk", tried: false, limit: 50 }),
    "/api/analyze/a1/trace?stage=S3&outcome=pruned_floor&family=psk&tried=false&limit=50");
  assert.equal(tracePath("job with space"), "/api/analyze/job%20with%20space/trace");
  for (const p of [tracePath("a1"), tracePath("a1", { family: "psk" })]) {
    assert.ok(!p.startsWith("/api/control"), p);
    assert.ok(!p.startsWith("/ws/"), p);
  }
});

test("guard: the trace panel module names no device route and no browser clock", () => {
  const src = readFileSync("src/app/explore/analyze-trace-panel.ts", "utf8");
  for (const w of ["/api/control", "/ws/open", "DeviceAction", "Date.now", "performance.now"]) {
    assert.ok(!src.includes(w), `analyze-trace-panel.ts names ${w}`);
  }
});

test("stage grouping preserves the backend's own node order, never re-sorted", () => {
  const nodes = [
    node({ id: "n1", stage: "S2" }),
    node({ id: "n2", stage: "S1" }),
    node({ id: "n3", stage: "S1" }),
    node({ id: "n4", stage: "S2" }),
  ];
  const groups = groupByStage(nodes);
  assert.deepEqual(groups.map((g) => g.stage), ["S2", "S1"]); // first-seen order, not S1-first
  assert.deepEqual(groups[0].nodes.map((n) => n.id), ["n1", "n4"]);
  assert.deepEqual(groups[1].nodes.map((n) => n.id), ["n2", "n3"]);
});

test("the two visual registers are decided by the served `tried` boolean alone", () => {
  assert.equal(nodeRegister(node({ tried: true, outcome: "pruned_floor", measured: { metric: "m", raw: 1, n: 1, bits: 3.2, quality: 0.5, look_elsewhere_bits: 0 } })), "tried");
  assert.equal(nodeRegister(node({ tried: false, outcome: "unsupported", measured: null })), "not-tried");
  // Even a tried node with a low bits value still reads "tried" — the register never inspects
  // `measured` or parses the outcome string, only the served `tried` flag.
  assert.equal(nodeRegister(node({ tried: true, outcome: "evaluated_worse", measured: { metric: "m", raw: 0, n: 1, bits: 0.1, quality: 0.1, look_elsewhere_bits: 0 } })), "tried");
});

test("re-run at deep is offered only for deferred_budget/deferred_prior, never for unsupported or pruned_floor", () => {
  assert.equal(offersRerun(node({ tried: false, outcome: "deferred_budget" })), true);
  assert.equal(offersRerun(node({ tried: false, outcome: "deferred_prior" })), true);
  assert.equal(offersRerun(node({ tried: false, outcome: "unsupported" })), false);
  assert.equal(offersRerun(node({ tried: true, outcome: "pruned_floor" })), false);
  assert.deepEqual(rerunDeepBody({ emitter_id: "e1" }), { emitter_id: "e1", profile: "deep" });
});

test("the elided tail row and the truncation note read from served numbers only", () => {
  const bucket: ElidedBucket = { stage: "S3", family: "psk", outcome: "pruned_floor", count: 412, bits_max: 4.6, bits_min: 0.2, evaluations: 1648 };
  assert.equal(elidedText(bucket), "412 more psk hypotheses pruned floor at S3, best 4.6 bits");
  assert.equal(truncationNote({ bounds: { max_nodes: 512, max_bytes: 262144, truncated: true } }), "list truncated to the retained bound (512 nodes) — counts below are complete, detail is not");
  assert.equal(truncationNote({ bounds: { max_nodes: 512, max_bytes: 262144, truncated: false } }), null);
});

test("an empty response is never silently blank — but a served not_applicable node already prevents it", () => {
  const withNode: Pick<TraceFetch, "nodes" | "elided"> = { nodes: [node({ outcome: "not_applicable", tried: false })], elided: [] };
  assert.equal(emptyTraceText(withNode), null);
  const trulyEmpty: Pick<TraceFetch, "nodes" | "elided"> = { nodes: [], elided: [] };
  assert.equal(emptyTraceText(trulyEmpty), "no trace nodes recorded for this filter");
  const onlyElided: Pick<TraceFetch, "nodes" | "elided"> = { nodes: [], elided: [{ stage: "S1", outcome: "memoised", count: 1, bits_max: 0, bits_min: 0, evaluations: 1 }] };
  assert.equal(emptyTraceText(onlyElided), null);
});

test("nodes_elided, the fetch-failure sentence and the poll stop rule read only served fields", async () => {
  const { nodesElidedNote, traceErrorText, tracePollDone } = await import("../src/app/explore/analyze-trace-panel");
  const { ControlError } = await import("../src/controls/client");
  assert.equal(nodesElidedNote({ trace_summary: { nodes_elided: 412 } }), "412 decisions elided from the recorded trace (nodes_elided)");
  assert.equal(nodesElidedNote({ trace_summary: { nodes_elided: 0 } }), null);
  assert.equal(nodesElidedNote({}), null);
  assert.equal(traceErrorText(new ControlError(400, "invalid", "unknown outcome")), "trace fetch failed: HTTP 400 invalid — unknown outcome");
  assert.equal(traceErrorText(new Error("network down")), "trace fetch failed: network down");
  const j = { id: "a1", state: "searching", end_reason: null, error: null, target: {}, results: [] } as const;
  assert.equal(tracePollDone({ final: true }, { ...j }), false, "final trace, pre-hand-back job copy: keep polling");
  assert.equal(tracePollDone({ final: false }, { ...j, state: "failed", resolution: { kind: "not-searched" } }), false);
  assert.equal(tracePollDone({ final: true }, { ...j, state: "failed", resolution: { kind: "not-searched" } }), true);
  assert.equal(tracePollDone({ final: true }, { ...j, state: "done" }), true, "a solved job carries no resolution");
});

test("the list says which filter it answers (T-930): an error for one filter never labels another's results", () => {
  assert.equal(filterLabel({}), "showing: the whole trace (no filter)");
  assert.equal(filterLabel({ family: "psk" }), "showing: family psk");
  assert.equal(filterLabel({ stage: "S3", outcome: "pruned_floor" }), "showing: stage S3, outcome pruned_floor");
  assert.equal(filterLabel({ family: "psk", tried: false }), "showing: family psk, not-tried only");
  assert.equal(filterLabel({ tried: true }), "showing: tried only");
  // `limit` is a bound on the fetch, not a question about the trace: it never labels the list
  // (the truncation note already states a shortened list).
  assert.equal(filterLabel({ limit: 8 }), "showing: the whole trace (no filter)");
});

test("T-930: the poll stops for a job that ENDED without a trace — the served `final` decides", async () => {
  const { tracePollDone } = await import("../src/app/explore/analyze-trace-panel");
  const j = { id: "a1", state: "searching", end_reason: null, error: null, target: {}, results: [] } as const;
  // The real shape of a running job's fetch on this build: final:false, no nodes. Keep polling.
  assert.equal(tracePollDone({ final: false }, { ...j }), false);
  // The shape the backend now serves once the job has ended without ever producing a trace: a
  // failure, and a cancel of a job that was still queued. Both carry a not-searched resolution.
  const notSearched = { kind: "not-searched", summary: "not searched" };
  assert.equal(tracePollDone({ final: true }, { ...j, state: "failed", error: { code: "no_evaluator", message: "" }, resolution: notSearched }), true);
  assert.equal(tracePollDone({ final: true }, { ...j, state: "cancelled", resolution: notSearched }), true);
  // A cancelled job whose worker has not handed back yet still serves final:false (its partial
  // trace may still arrive), so the poll continues — the case the fix must not break.
  assert.equal(tracePollDone({ final: false }, { ...j, state: "cancelled", resolution: notSearched }), false);
});
