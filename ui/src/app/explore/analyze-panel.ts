// T-862 (ADR-0015 §5, MAUTO M-11): the Analyze section of the output panel — job progress, the
// ranked results, each result's evidence ladder, "start as pipeline" and "save template". A thin
// client over docs/api.md's `/api/analyze`: every number, verdict and summary is the backend's;
// this module only formats and wires buttons. The pure view-model helpers are unit-tested without
// a DOM (ui/test/app-analyze-panel.test.ts).
import type { ControlClient } from "../../controls/client";
import { ControlError } from "../../controls/client";
import type { AppContext } from "../context";
import { h } from "../dom";
import { startPoll } from "../net";
import { toast } from "../state";
import { watchAnalyzeJob } from "./analyze-slice";

// ---- wire types (docs/api.md "AnalyzeJob") ----

export type JobState = "queued" | "acquiring" | "searching" | "refining" | "validating" | "throttled" | "done" | "cancelled" | "failed";
export interface StageEvidence { stage: string; node: string; metric: string; raw: number; n: number; bits: number; quality: number }
export interface PipelineResult {
  rank: number; verdict: string; summary: string; recipe: unknown;
  template: { id: string; version: number } | null;
  stage_reached: string; stages: readonly StageEvidence[];
  evidence_bits: number; prior_bits: number; analytic_holdout_bits?: number | null;
}
export interface AnalyzeJob {
  id: string; state: JobState; end_reason: string | null;
  error: { code: string; message: string } | null;
  target: Readonly<Record<string, unknown>>; profile?: string;
  budget?: { wall_s?: number; max_evaluations?: number; max_proposal_calls?: number };
  used?: { wall_s?: number; evaluations?: number; proposal_calls?: number };
  progress?: { stage_max?: string; beam?: number; pruned?: number; tried?: number; not_tried?: number; evaluations?: number; proposal_calls?: number };
  results: readonly PipelineResult[];
  warnings?: readonly unknown[];
  /** ADR-0021 §7A.2, present whenever no result reached `solved`; rendered by the trace panel
   * (T-570), never by this module. */
  resolution?: Readonly<Record<string, unknown>> | null;
  /** ADR-0021 §4.1; the trace panel shows `nodes_elided` beside the list (T-570 item 5). */
  trace_summary?: { nodes_elided?: number; nodes_recorded?: number; truncated?: boolean } | null;
}

// ---- pure view-model ----

const ACTIVE: readonly JobState[] = ["queued", "acquiring", "searching", "refining", "validating", "throttled"];
export const jobIsActive = (j: AnalyzeJob): boolean => ACTIVE.includes(j.state);

/** "searching · deepest S3" — the state and how deep the search has reached. */
export function jobStateText(j: AnalyzeJob): string {
  const parts: string[] = [j.state];
  if (j.progress?.stage_max) parts.push(`deepest ${j.progress.stage_max}`);
  if (!jobIsActive(j) && j.end_reason) parts.push(j.end_reason);
  if (j.state === "failed" && j.error) parts.push(j.error.code);
  return parts.join(" · ");
}

/** Budget use as the server counts it: proposal calls first (they dominate wall time, T-552), wall
 * time as the backstop. Null when the job carries no budget to compare against. */
export function budgetText(j: AnalyzeJob): string | null {
  const b = j.budget, u = j.used;
  if (!b || !u) return null;
  const bits: string[] = [];
  if (b.max_proposal_calls != null && u.proposal_calls != null) bits.push(`${u.proposal_calls}/${b.max_proposal_calls} proposals`);
  if (b.max_evaluations != null && u.evaluations != null) bits.push(`${u.evaluations}/${b.max_evaluations} evaluations`);
  if (b.wall_s != null && u.wall_s != null) bits.push(`${u.wall_s.toFixed(1)}/${b.wall_s.toFixed(0)} s`);
  return bits.length ? bits.join(" · ") : null;
}

/** Fraction of the proposal budget spent (0..1), falling back to wall time; null when unknown. */
export function budgetFraction(j: AnalyzeJob): number | null {
  const b = j.budget, u = j.used;
  if (!b || !u) return null;
  const f = b.max_proposal_calls && u.proposal_calls != null ? u.proposal_calls / b.max_proposal_calls
    : b.wall_s && u.wall_s != null ? u.wall_s / b.wall_s : null;
  return f == null ? null : Math.max(0, Math.min(1, f));
}

/** Ranked results in rank order (the server ranks; this only sorts defensively and caps at 10). */
export const rankedResults = (j: AnalyzeJob): PipelineResult[] => [...j.results].sort((a, b) => a.rank - b.rank).slice(0, 10);

/** One ladder rung: "S2 · clock-recovery — 12.4 bits (n=210)". Bits are the backend's. */
export function rungText(s: StageEvidence): string {
  return `${s.stage} · ${s.node} · ${s.metric} — ${s.bits.toFixed(1)} bits (n=${s.n})`;
}

/** Bar width 0..1 for a rung, relative to the ladder's strongest rung, so the ladder reads as a shape. */
export function ladderWidths(stages: readonly StageEvidence[]): number[] {
  const max = Math.max(0, ...stages.map((s) => s.bits));
  return stages.map((s) => (max > 0 ? Math.max(0, s.bits) / max : 0));
}

export function resultHeadline(r: PipelineResult): string {
  return `#${r.rank} ${r.verdict} · ${r.stage_reached} · ${r.evidence_bits.toFixed(1)} bits`;
}

/** A finished job with no result says so; it never renders as an empty success. */
export function resultsEmptyText(j: AnalyzeJob): string | null {
  if (j.results.length) return null;
  if (jobIsActive(j)) return "searching — no result yet";
  if (j.state === "failed") return `failed: ${j.error?.message ?? "no reason served"}`;
  return "no decoder found for this window";
}

/** What `POST /api/pipelines` gets to run result `r` on the job's own target. */
export const startBody = (j: AnalyzeJob, r: PipelineResult): { recipe: unknown; target: unknown } => ({ recipe: r.recipe, target: j.target });

/** Default id/name for "save template" — a suggestion the user's click accepts, from the job and rank. */
export const templateBody = (j: AnalyzeJob, r: PipelineResult): { id: string; name: string } =>
  ({ id: `discovered-${j.id}-r${r.rank}`, name: `Discovered ${j.id} #${r.rank} (${r.verdict})` });

/** A save-template refusal in words: the route is M-10's, so an older server answers 404/405. */
export function templateError(e: unknown): string {
  if (e instanceof ControlError && (e.status === 404 || e.status === 405)) return "save template: not available on this server yet";
  return `save template: ${e instanceof Error ? e.message : String(e)}`;
}

// ---- actions ----

export async function startAsPipeline(client: ControlClient, j: AnalyzeJob, r: PipelineResult): Promise<string> {
  const created = await client.post<{ id: string }>("/api/pipelines", startBody(j, r));
  return created.id;
}

export async function saveTemplate(client: ControlClient, j: AnalyzeJob, r: PipelineResult): Promise<string> {
  const body = templateBody(j, r);
  await client.post(`/api/analyze/${encodeURIComponent(j.id)}/results/${r.rank}/template`, body);
  return body.id;
}

// ---- panel ----

function resultCard(j: AnalyzeJob, r: PipelineResult, ctx: AppContext): HTMLElement {
  const widths = ladderWidths(r.stages);
  const ladder = h("div", { class: "an-ladder" }, ...r.stages.map((s, i) =>
    h("div", { class: "an-rung", title: `quality ${(s.quality * 100).toFixed(0)}%` },
      h("span", { class: "an-bar", style: `width:${(widths[i] * 100).toFixed(0)}%` }),
      h("span", { class: "an-rung-t" }, rungText(s)))));
  const start = h("button", { class: "mini", type: "button", onclick: () => {
    startAsPipeline(ctx.client, j, r)
      .then((id) => ctx.store.set(toast(`analyze #${r.rank}: started pipeline ${id}`)))
      .catch((e) => ctx.store.set(toast(`start as pipeline: ${e instanceof Error ? e.message : String(e)}`)));
  } }, "Start as pipeline");
  const save = h("button", { class: "mini", type: "button", onclick: () => {
    saveTemplate(ctx.client, j, r)
      .then((id) => ctx.store.set(toast(`saved template ${id}`)))
      .catch((e) => ctx.store.set(toast(templateError(e))));
  } }, "Save template");
  return h("div", { class: "an-result" },
    h("div", { class: "an-head" }, h("b", {}, resultHeadline(r))),
    h("div", { class: "an-summary" }, r.summary),
    ladder,
    r.analytic_holdout_bits != null ? h("small", {}, `hold-out ${r.analytic_holdout_bits.toFixed(1)} bits · prior ${r.prior_bits.toFixed(1)} (reported, never ranks)`) : null,
    h("div", { class: "an-actions" }, start, save),
  );
}

export function renderAnalyze(el: HTMLElement, j: AnalyzeJob | null, ctx: AppContext): void {
  if (!j) { el.hidden = true; el.replaceChildren(); return; }
  el.hidden = false;
  const frac = budgetFraction(j);
  const budget = budgetText(j);
  const stop = h("button", { class: "mini", type: "button", onclick: () => {
    ctx.client.del(`/api/analyze/${encodeURIComponent(j.id)}`).catch((e) => ctx.store.set(toast(`stop analyze: ${e instanceof Error ? e.message : String(e)}`)));
  } }, jobIsActive(j) ? "Stop" : "Dismiss");
  const empty = resultsEmptyText(j);
  const kids: Array<Node | undefined> = [
    h("div", { class: "section-h" }, `Analyze ${j.id}`),
    h("div", { class: "an-state" }, jobStateText(j), " ", stop),
    frac == null ? undefined : h("progress", { max: "1", value: String(frac), "aria-label": "analyze budget used" }),
    budget ? h("small", {}, budget) : undefined,
    empty ? h("div", { class: "empty" }, empty) : undefined,
    ...rankedResults(j).map((r) => resultCard(j, r, ctx)),
  ];
  el.replaceChildren(...kids.filter((k): k is Node => k !== undefined));
}

/** Mounts the Analyze section into `el`, polling the watched job (GET is authoritative, ADR-0015
 * §5.2) at 1 s while it runs and once it has finished. `onJob` (T-570) forwards every fetched job
 * to the trace panel, so it needs no `GET /api/analyze/{id}` poll of its own. */
export function mountAnalyzeSection(el: HTMLElement, ctx: AppContext, onJob?: (j: AnalyzeJob | null) => void): void {
  let job: AnalyzeJob | null = null;
  let watching: string | null = null;
  const draw = () => { renderAnalyze(el, job, ctx); onJob?.(job); };
  ctx.store.select((s) => s.analyze.jobId, (id) => {
    if (id === watching) return;
    watching = id; job = null; draw();
  });
  startPoll(async () => {
    const id = ctx.store.get().analyze.jobId;
    if (!id) return;
    try {
      const j = await ctx.client.get<AnalyzeJob>(`/api/analyze/${encodeURIComponent(id)}`);
      if (ctx.store.get().analyze.jobId !== id) return;
      job = j; draw();
    } catch (e) {
      if (e instanceof ControlError && (e.status === 404 || e.status === 410)) { ctx.store.set(watchAnalyzeJob(null)); return; }
      throw e;
    }
  }, 1000);
  draw();
}
