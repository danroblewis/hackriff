// T-570 (ADR-0021 §6A, ADR-0015 §10 M-11): "why not PSK", answered from the backend's search trace
// with two visual registers (tried/not-tried) and no signal logic in the client. Every number and
// every sentence here is the backend's `summary`/`reasoning`; this module only fetches
// `GET /api/analyze/{id}/trace`, groups the served nodes by the served `stage` (in the backend's
// own order) and formats. The pure view-model helpers are unit-tested without a DOM
// (ui/test/app-analyze-trace-panel.test.ts), which also pins the request the client builds
// (the T-367 guard) and that no trace interaction reaches a device route.
import type { AppContext } from "../context";
import { h } from "../dom";
import { startPoll } from "../net";
import { toast } from "../state";
import { ControlError } from "../../controls/client";
import { jobIsActive, type AnalyzeJob } from "./analyze-panel";

// ---- wire types (docs/api.md "The trace (`GET /api/analyze/{id}/trace`)", ADR-0021 §2) ----

export interface Measured {
  metric: string; raw: number; n: number; bits: number; quality: number;
  floor_bits?: number | null; look_elsewhere_bits: number;
}
export interface TraceHypothesis {
  skeleton: string; slot: string; choice: string; family?: string | null;
  params?: Readonly<Record<string, unknown>>; swept?: readonly unknown[];
}
export interface TraceNode {
  id: string; parent: string | null; stage: string; hypothesis: TraceHypothesis;
  seed_source: string; prior_bits: number; measured: Measured | null; evidence_bits: number | null;
  outcome: string; outcome_detail?: Readonly<Record<string, unknown>>; tried: boolean;
  evaluations: number; cpu_ms: number; nondeterministic?: boolean; summary: string;
}
export interface ElidedBucket {
  stage: string; family?: string | null; outcome: string;
  count: number; bits_max: number; bits_min: number; evaluations: number;
}
export interface TraceFetch {
  job_id: string; state: string; engine: string; final: boolean; replay_key: unknown;
  bounds: { max_nodes: number; max_bytes: number; truncated: boolean };
  nodes: readonly TraceNode[]; elided: readonly ElidedBucket[];
}
export interface TraceFilter { stage?: string; outcome?: string; family?: string; tried?: boolean; limit?: number }

export interface Explanation {
  source: string; identity: string; score: number; distance_hz?: number | null;
  status: string; data_age_days?: number | null; reasoning: string;
}
export interface Resolution {
  kind: string; deepest_verdict?: string | null; reason?: string | null;
  summary: string; explanations?: readonly Explanation[] | null;
}

// ---- pure query building (asserted directly in tests: the T-367 guard) ----

/** `GET /api/analyze/{id}/trace` with the served filter params — the only request this module
 * builds, and never a device route. */
export function tracePath(jobId: string, f: TraceFilter = {}): string {
  const p = new URLSearchParams();
  if (f.stage) p.set("stage", f.stage);
  if (f.outcome) p.set("outcome", f.outcome);
  if (f.family) p.set("family", f.family);
  if (f.tried !== undefined) p.set("tried", String(f.tried));
  if (f.limit !== undefined) p.set("limit", String(f.limit));
  const q = p.toString();
  return `/api/analyze/${encodeURIComponent(jobId)}/trace${q ? `?${q}` : ""}`;
}

// ---- pure view-model ----

/** Rows grouped per stage, preserving the backend's own node order within and across groups
 * (acceptance item 1): no re-sorting by bits or anything else the client would have to compute. */
export function groupByStage(nodes: readonly TraceNode[]): Array<{ stage: string; nodes: TraceNode[] }> {
  const order: string[] = [];
  const by = new Map<string, TraceNode[]>();
  for (const n of nodes) {
    if (!by.has(n.stage)) { by.set(n.stage, []); order.push(n.stage); }
    by.get(n.stage)!.push(n);
  }
  return order.map((stage) => ({ stage, nodes: by.get(stage)! }));
}

/** The two visual registers (acceptance item 2, ADR-0021 §6A): decided by the served `tried`
 * boolean alone — never by inspecting `measured` or parsing the outcome string. */
export const nodeRegister = (n: TraceNode): "tried" | "not-tried" => (n.tried ? "tried" : "not-tried");

/** Whether a not-tried row offers "re-run at deep" (ADR-0021 §6A's table): only the two outcomes a
 * bigger budget would actually reach. Checked against the served `outcome` name, never inferred
 * from a number. */
export function offersRerun(n: TraceNode): boolean {
  return !n.tried && (n.outcome === "deferred_budget" || n.outcome === "deferred_prior");
}

/** `POST /api/analyze` body to re-run the job's own target at `deep` (ADR-0015 §3.3's deepest
 * budget) — the action ADR-0021 §6A's table offers for `deferred_budget`/`deferred_prior`. */
export function rerunDeepBody(target: Readonly<Record<string, unknown>>): Record<string, unknown> {
  return { ...target, profile: "deep" };
}

/** The elided tail row (acceptance item 5): "412 more psk hypotheses pruned floor at S3, best 4.6
 * bits". Every number is the backend's; this only assembles the sentence. */
export function elidedText(e: ElidedBucket): string {
  const fam = e.family ? `${e.family} ` : "";
  return `${e.count} more ${fam}hypotheses ${e.outcome.replace(/_/g, " ")} at ${e.stage}, best ${e.bits_max.toFixed(1)} bits`;
}

/** The truncation note beside the list (acceptance item 5); null when the trace is complete, so a
 * shorter list never reads as a smaller search with no note. */
export function truncationNote(t: Pick<TraceFetch, "bounds">): string | null {
  return t.bounds.truncated ? `list truncated to the retained bound (${t.bounds.max_nodes} nodes) — counts below are complete, detail is not` : null;
}

/** An empty response is never the answer (acceptance item 4): the backend's own `not_applicable`
 * root node already carries the sentence for a family outside the skeleton set, so ordinarily
 * `nodes` is never empty for a real filter. This only fires if even that is missing, and it still
 * reads as an explicit statement rather than a blank box. */
/** `job.trace_summary.nodes_elided` beside the list (acceptance item 5): the session-level count of
 * decisions the recorded trace dropped, stated even when the fetch itself is not truncated. Null
 * when the job has no summary yet or nothing was elided. */
export function nodesElidedNote(job: Pick<AnalyzeJob, "trace_summary">): string | null {
  const n = job.trace_summary?.nodes_elided;
  return typeof n === "number" && n > 0 ? `${n} decision${n === 1 ? "" : "s"} elided from the recorded trace (nodes_elided)` : null;
}

/** A failed trace fetch, stated plainly (a trace expires with its job; a bad filter is a 400) —
 * never "loading" forever. */
export function traceErrorText(e: unknown): string {
  if (e instanceof ControlError) return `trace fetch failed: HTTP ${e.status} ${e.code} — ${e.message}`;
  return `trace fetch failed: ${e instanceof Error ? e.message : String(e)}`;
}

/** The trace poll may stop only when both the trace is `final` AND the job copy the panel holds is
 * the handed-back one: it carries its `resolution`, or has ended (a solved job has none). The
 * backend sets both in the same hand-back, so a `final` trace can arrive before the job poll has
 * brought the resolution — stopping then would lose the headline (review FAIL, T-570). */
export function tracePollDone(trace: Pick<TraceFetch, "final">, job: AnalyzeJob): boolean {
  return trace.final && (job.resolution != null || !jobIsActive(job));
}

export function emptyTraceText(t: Pick<TraceFetch, "nodes" | "elided">): string | null {
  return t.nodes.length || t.elided.length ? null : "no trace nodes recorded for this filter";
}

// ---- panel ----

function nodeRow(job: AnalyzeJob, n: TraceNode, ctx: AppContext): HTMLElement {
  const rerun = offersRerun(n)
    ? h("button", { class: "mini", type: "button", onclick: () => {
        ctx.client.post("/api/analyze", rerunDeepBody(job.target))
          .then(() => ctx.store.set(toast(`re-run at deep: requested`)))
          .catch((e) => ctx.store.set(toast(`re-run at deep: ${e instanceof Error ? e.message : String(e)}`)));
      } }, "Re-run at deep")
    : undefined;
  return h("div", { class: `tr-node tr-${nodeRegister(n)}` },
    h("span", { class: "tr-summary" }, n.summary),
    rerun,
  );
}

function resolutionBlock(r: Resolution): HTMLElement {
  // `not-searched` and `unknown` are visually distinct (acceptance item 6); the class carries it.
  const explanations = r.explanations ?? [];
  return h("div", { class: `tr-resolution tr-res-${r.kind}` },
    h("div", { class: "tr-res-headline" }, r.summary),
    explanations.length
      ? h("div", { class: "tr-explanations" }, ...explanations.map((x) =>
          h("div", { class: "tr-explain" }, `${x.reasoning} (${x.source}, ${x.status})`)))
      : undefined,
  );
}

/** The results body only (headline/resolution, stage groups, elided tail, truncation note) — never
 * the filter row. Split out so a redraw (every trace fetch, and only a trace fetch — see
 * `mountTracePanel` below) cannot touch the family `<input>` a user may be mid-keystroke in
 * (found in review: rebuilding the whole panel on every ~1 s job poll wiped focus and typed text,
 * so "Why not PSK?" could not be used at all). */
function renderTraceResults(el: HTMLElement, ctx: AppContext, job: AnalyzeJob, trace: TraceFetch | null, error: string | null = null): void {
  const kids: Array<Node | undefined> = [];
  if (error) kids.push(h("div", { class: "tr-error" }, error));
  if (!trace) {
    if (!error) kids.push(h("div", { class: "empty" }, "loading trace…"));
  } else {
    const resolution = job.resolution as Resolution | undefined;
    if (resolution) kids.push(resolutionBlock(resolution));
    const empty = emptyTraceText(trace);
    if (empty) kids.push(h("div", { class: "empty" }, empty));
    for (const g of groupByStage(trace.nodes)) {
      kids.push(h("div", { class: "tr-stage" },
        h("div", { class: "tr-stage-h" }, g.stage),
        ...g.nodes.map((n) => nodeRow(job, n, ctx)),
      ));
    }
    for (const e of trace.elided) kids.push(h("div", { class: "tr-elided" }, elidedText(e)));
    const note = truncationNote(trace);
    if (note) kids.push(h("small", { class: "tr-trunc" }, note));
    const elidedNote = nodesElidedNote(job);
    if (elidedNote) kids.push(h("small", { class: "tr-trunc" }, elidedNote));
  }
  el.replaceChildren(...kids.filter((k): k is Node => k !== undefined));
}

/** Mounts the trace panel into `el`. The caller (`mountAnalyzeSection`, via its own `AnalyzeJob`
 * poll) drives `setJob`; this module makes no `GET /api/analyze/{id}` call of its own, only the
 * trace fetch, at 2 s while a job is watched (stopped per `tracePollDone`, restarted on the next
 * job; a failed fetch is stated and rethrown so `startPoll` backs off).
 *
 * The filter row (the family `<input>` and its buttons) is built exactly once per watched job and
 * never rebuilt by a results redraw — the fix for the review finding above. `renderTraceResults`
 * runs on every trace fetch, and on a same-id `setJob` (the 1 s `AnalyzeJob` poll) only when that
 * job brings a new or changed `resolution`/`trace_summary` — so the headline appears even when the
 * `final` trace arrived before the job poll carried the resolution. The poll stops only when the
 * trace is final AND the job copy is the handed-back one (`tracePollDone`). A monotonic request
 * sequence discards a stale trace response that resolves after a newer one (e.g. the unfiltered fetch still in flight when "why not psk" is asked) rather than letting it
 * overwrite the answer to a later question. */
export function mountTracePanel(el: HTMLElement, ctx: AppContext): { setJob(j: AnalyzeJob | null): void } {
  let job: AnalyzeJob | null = null;
  let filter: TraceFilter = {};
  let stopPoll: (() => void) | null = null;
  let seq = 0;

  const familyInput = h("input", { type: "text", class: "mono", placeholder: "family (e.g. psk)", "aria-label": "Why not this family?" }) as HTMLInputElement;
  const ask = () => { filter = { ...filter, family: familyInput.value.trim() || undefined }; fetchQuiet(); };
  familyInput.onkeydown = (e: KeyboardEvent) => { if (e.key === "Enter") { e.preventDefault(); ask(); } };
  const goBtn = h("button", { class: "mini", type: "button", onclick: ask }, "Why not?");
  const clearBtn = h("button", { class: "mini", type: "button", onclick: () => { familyInput.value = ""; filter = { ...filter, family: undefined }; fetchQuiet(); } }, "Clear");
  const resultsEl = h("div", { class: "tr-results" });
  el.replaceChildren(h("div", { class: "section-h" }, "Search trace"), h("div", { class: "tr-filter" }, familyInput, goBtn, clearBtn), resultsEl);

  // The last trace drawn, so a job poll bringing a new `resolution` (or trace_summary) can redraw
  // the headline without refetching, and a failed fetch keeps what was already shown.
  let lastTrace: TraceFetch | null = null;
  let lastError: string | null = null;
  const redraw = () => { if (job) renderTraceResults(resultsEl, ctx, job, lastTrace, lastError); };

  /** One trace fetch. Draws with the LATEST job (never a copy saved before the await: the backend
   * hands the resolution and the final trace back together, so a saved copy can predate it).
   * Rethrows a failure after stating it, so the poll's back-off applies. */
  const fetchTrace = async () => {
    if (!job) return;
    const mySeq = ++seq;
    const id = job.id;
    let trace: TraceFetch;
    try {
      trace = await ctx.client.get<TraceFetch>(tracePath(id, filter));
    } catch (e) {
      if (seq !== mySeq || job?.id !== id) return;
      lastError = traceErrorText(e);
      redraw();
      throw e;
    }
    if (seq !== mySeq || job?.id !== id) return; // superseded by a later filter, or the job changed
    lastTrace = trace; lastError = null;
    redraw();
    if (tracePollDone(trace, job) && stopPoll) { stopPoll(); stopPoll = null; }
  };
  const fetchQuiet = () => { fetchTrace().catch(() => {}); }; // a click's failure is already on screen

  return {
    setJob(j) {
      const prev = job;
      const changed = j?.id !== prev?.id;
      job = j;
      if (!j) {
        if (stopPoll) { stopPoll(); stopPoll = null; }
        el.hidden = true;
        return;
      }
      el.hidden = false;
      if (!changed) {
        // The 1 s AnalyzeJob poll for the SAME job: the filter row is never touched, and the
        // results redraw only when the served resolution or trace_summary actually changed.
        if (lastTrace && (JSON.stringify(prev?.resolution ?? null) !== JSON.stringify(j.resolution ?? null)
          || JSON.stringify(prev?.trace_summary ?? null) !== JSON.stringify(j.trace_summary ?? null))) redraw();
        return;
      }
      lastTrace = null; lastError = null;
      filter = {}; familyInput.value = "";
      resultsEl.replaceChildren(h("div", { class: "empty" }, "loading trace…"));
      if (stopPoll) stopPoll();
      stopPoll = startPoll(fetchTrace, 2000);
    },
  };
}
