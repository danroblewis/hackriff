// T-862 (ADR-0015 §5, MAUTO M-11): the Analyze section's pure view-model and the calls it builds.
// No DOM under node:test; assertions are on the requests the client builds (docs/api.md).
import { test } from "node:test";
import assert from "node:assert/strict";
import { ControlError } from "../src/controls/client";
import {
  budgetFraction, budgetText, jobStateText, ladderWidths, rankedResults, resultsEmptyText, rungText,
  saveTemplate, startAsPipeline, templateError, type AnalyzeJob, type PipelineResult,
} from "../src/app/explore/analyze-panel";
import { analyzeTarget } from "../src/app/menu/actions";

const result = (rank: number, over: Partial<PipelineResult> = {}): PipelineResult => ({
  rank, verdict: "clocked", summary: "2-FSK 9600 baud", recipe: { id: `r${rank}` }, template: null, stage_reached: "S3",
  stages: [
    { stage: "S1", node: "demod", metric: "snr", raw: 12, n: 100, bits: 20, quality: 0.9 },
    { stage: "S3", node: "clock", metric: "eye", raw: 0.5, n: 210, bits: 5, quality: 0.5 },
  ],
  evidence_bits: 25, prior_bits: 0, ...over,
});
const job = (over: Partial<AnalyzeJob> = {}): AnalyzeJob => ({
  id: "a1", state: "searching", end_reason: null, error: null, target: { emitter_id: "e1" }, results: [], ...over,
});

test("state text names the deepest stage and the end reason only once finished", () => {
  assert.equal(jobStateText(job({ progress: { stage_max: "S3" } })), "searching · deepest S3");
  assert.equal(jobStateText(job({ state: "done", end_reason: "plateau" })), "done · plateau");
});

test("budget counts proposal calls first, wall time as the backstop", () => {
  const j = job({ budget: { max_proposal_calls: 40, wall_s: 60 }, used: { proposal_calls: 10, wall_s: 30 } });
  assert.equal(budgetText(j), "10/40 proposals · 30.0/60 s");
  assert.equal(budgetFraction(j), 0.25);
  assert.equal(budgetFraction(job()), null);
});

test("results sort by rank; a finished empty job says so, never a blank success", () => {
  assert.deepEqual(rankedResults(job({ results: [result(2), result(1)] })).map((r) => r.rank), [1, 2]);
  assert.equal(resultsEmptyText(job()), "searching — no result yet");
  assert.equal(resultsEmptyText(job({ state: "done" })), "no decoder found for this window");
  assert.equal(resultsEmptyText(job({ state: "done", results: [result(1)] })), null);
});

test("ladder rungs carry the backend's bits, widths are relative to the strongest", () => {
  const r = result(1);
  assert.equal(rungText(r.stages[0]), "S1 · demod · snr — 20.0 bits (n=100)");
  assert.deepEqual(ladderWidths(r.stages), [1, 0.25]);
});

test("start as pipeline posts the result's recipe on the job's target; save template hits M-10's route", async () => {
  const calls: Array<[string, unknown]> = [];
  const client = { post: async (p: string, b: unknown) => { calls.push([p, b]); return { id: "p7" }; } };
  const j = job(), r = result(2);
  assert.equal(await startAsPipeline(client as never, j, r), "p7");
  await saveTemplate(client as never, j, r);
  assert.deepEqual(calls[0], ["/api/pipelines", { recipe: { id: "r2" }, target: { emitter_id: "e1" } }]);
  assert.equal(calls[1][0], "/api/analyze/a1/results/2/template");
  assert.equal((calls[1][1] as { id: string }).id, "discovered-a1-r2");
});

test("an older server's 404 on save-template reads as not available", () => {
  assert.match(templateError(new ControlError(404, "not_found", "x")), /not available/);
});

test("analyzeTarget hands back the started job's id for the panel to watch", async () => {
  const r = await analyzeTarget({ post: async () => ({ job: { id: "a3" } }) } as never, { kind: "emitter", id: "e1" });
  assert.deepEqual(r, { ok: true, message: "Analyze: requested", jobId: "a3" });
});

test("a watched job keeps the focus panel mounted even with nothing focused", async () => {
  const { analyzeWatched, watchAnalyzeJob } = await import("../src/app/explore/analyze-slice");
  assert.equal(analyzeWatched(null), false);
  assert.equal(analyzeWatched("a1"), true);
  assert.deepEqual(watchAnalyzeJob("a1")(), { analyze: { jobId: "a1" } });
});
