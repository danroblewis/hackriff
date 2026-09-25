// T-570 review round 2: the trace panel's mount, driven over a minimal fake DOM with a scripted
// client and a manual timer, so the poll ORDER the review named can be replayed exactly:
//  - the resolution headline must appear when the `final` trace was fetched with a job copy from
//    before the hand-back and the resolution only arrives on a later same-id job poll;
//  - a failed trace fetch is stated plainly, never "loading trace…" forever, and the poll backs off;
//  - `job.trace_summary.nodes_elided` is shown beside the list.
// T-930 adds the REAL server shape (a job that ends without ever producing a trace serves
// `final: false` with empty nodes while it runs, then `final: true` when it has ended) and the
// filter label the results carry under a failed fetch's error line.
// Imports only `mountTracePanel` (plus the client's own error type), so this file also builds
// against the pre-fix module — which is how its red-on-old proof was taken.
import { test } from "node:test";
import assert from "node:assert/strict";
import { mountTracePanel, type TraceFetch } from "../src/app/explore/analyze-trace-panel";
import type { AnalyzeJob } from "../src/app/explore/analyze-panel";
import type { AppContext } from "../src/app/context";
import { ControlError } from "../src/controls/client";

class FakeEl {
  children: FakeEl[] = [];
  attrs: Record<string, string> = {};
  text = "";
  hidden = false;
  onkeydown: unknown = null;
  value = "";
  constructor(public tag: string) {}
  append(...c: (FakeEl | string)[]) { for (const x of c) if (typeof x === "string") this.text += x; else this.children.push(x); }
  replaceChildren(...c: FakeEl[]) { this.children = [...c]; this.text = ""; }
  setAttribute(k: string, v: string) { this.attrs[k] = v; }
  addEventListener() {}
  get textContent(): string { return this.text + this.children.map((c) => c.textContent).join(" "); }
  findAll(cls: string): FakeEl[] {
    const out: FakeEl[] = [];
    for (const c of this.children) {
      if ((c.attrs.class ?? "").split(" ").includes(cls)) out.push(c);
      out.push(...c.findAll(cls));
    }
    return out;
  }
}

const flush = async () => { for (let i = 0; i < 10; i++) await Promise.resolve(); };

interface Pending { path: string; resolve(v: unknown): void; reject(e: unknown): void }

function harness() {
  const g = globalThis as Record<string, unknown>;
  const saved = { document: g.document, window: g.window };
  const timers: Array<{ fn: () => void; ms: number }> = [];
  g.document = { createElement: (t: string) => new FakeEl(t) };
  g.window = { setTimeout: (fn: () => void, ms: number) => { timers.push({ fn, ms }); return timers.length; } };
  (g as Record<string, unknown>).clearTimeout ??= () => {};
  const calls: string[] = [];
  const pending: Pending[] = [];
  const client = {
    get: (path: string) => new Promise((resolve, reject) => { calls.push(`GET ${path}`); pending.push({ path, resolve, reject }); }),
    post: (path: string) => { calls.push(`POST ${path}`); return Promise.resolve({}); },
  };
  const ctx = { client, store: { set() {} } } as unknown as AppContext;
  const root = new FakeEl("section");
  const panel = mountTracePanel(root as unknown as HTMLElement, ctx);
  return {
    root, panel, calls, pending, timers,
    restore() { g.document = saved.document; g.window = saved.window; },
  };
}

const job = (over: Partial<AnalyzeJob> = {}): AnalyzeJob => ({
  id: "a1", state: "searching", end_reason: null, error: null, target: { emitter_id: "e1" }, results: [], ...over,
});
const trace = (over: Partial<TraceFetch> = {}): TraceFetch => ({
  job_id: "a1", state: "failed", engine: "hk-synth", final: true, replay_key: null,
  bounds: { max_nodes: 512, max_bytes: 262144, truncated: false },
  nodes: [{
    id: "n1", parent: null, stage: "S0", hypothesis: { skeleton: "s", slot: "S0", choice: "c" },
    seed_source: "estimate", prior_bits: 0, measured: null, evidence_bits: null, outcome: "unsupported",
    tried: false, evaluations: 0, cpu_ms: 0, summary: "not tried: no evaluator in this build",
  }],
  elided: [], ...over,
});
const NOT_SEARCHED = { kind: "not-searched", summary: "not searched: this build has no evaluator", reason: "no_evaluator" };

test("the resolution headline appears when the final trace was drawn with a pre-hand-back job copy (review poll order)", async () => {
  const hx = harness();
  try {
    // t=0: the job poll, still searching, no resolution.
    hx.panel.setJob(job());
    assert.equal(hx.pending.length, 1, "the trace poll fetched once at mount");
    // The engine hands back (trace final + resolution together) — the trace fetch sees final:true,
    // but the panel's job copy is still the t=0 one.
    hx.pending[0].resolve(trace());
    await flush();
    assert.equal(hx.root.findAll("tr-res-headline").length, 0, "no resolution on the job copy yet");
    // t=1: the job poll brings the resolution, same id. No click.
    hx.panel.setJob(job({ state: "failed", end_reason: "no_evaluator", resolution: NOT_SEARCHED }));
    await flush();
    const head = hx.root.findAll("tr-res-headline");
    assert.equal(head.length, 1, "the headline is drawn without a user click");
    assert.equal(head[0].textContent, NOT_SEARCHED.summary);
    assert.equal(hx.root.findAll("tr-res-not-searched").length, 1);
    // Rows still in the backend's order with the served tried register.
    assert.equal(hx.root.findAll("tr-not-tried").length, 1);
    assert.ok(hx.calls.every((c) => c.startsWith("GET /api/analyze/a1/trace")), hx.calls.join(", "));
  } finally { hx.restore(); }
});

test("a job poll landing while the final trace fetch is in flight is the copy the headline is drawn with", async () => {
  const hx = harness();
  try {
    hx.panel.setJob(job());
    hx.panel.setJob(job({ state: "failed", resolution: NOT_SEARCHED })); // arrives mid-fetch
    hx.pending[0].resolve(trace());
    await flush();
    assert.equal(hx.root.findAll("tr-res-headline").length, 1, "drawn with the LATEST job, not the one saved at fetch start");
  } finally { hx.restore(); }
});

test("the trace poll keeps going after a final trace until the job itself carries its resolution", async () => {
  const hx = harness();
  try {
    hx.panel.setJob(job());
    hx.pending[0].resolve(trace());
    await flush();
    assert.equal(hx.timers.length, 1, "final trace but a pre-hand-back job copy: the poll is re-armed, not stopped");
    hx.panel.setJob(job({ state: "failed", resolution: NOT_SEARCHED }));
    hx.timers[0].fn();
    hx.pending[1].resolve(trace());
    await flush();
    assert.equal(hx.timers.length, 1, "final trace AND the handed-back job: the poll stops");
  } finally { hx.restore(); }
});

test("a failed trace fetch is stated plainly, never 'loading trace…' forever, and the poll backs off", async () => {
  const hx = harness();
  try {
    hx.panel.setJob(job({ state: "failed", resolution: NOT_SEARCHED }));
    const fail = (i: number) => hx.pending[i].reject(new ControlError(404, "not_found", "no trace for job a1 (expired with the job)"));
    fail(0);
    await flush();
    const text = hx.root.textContent;
    assert.doesNotMatch(text, /loading trace/, text);
    assert.match(text, /trace fetch failed: HTTP 404 not_found — no trace for job a1 \(expired with the job\)/);
    // Repeated failures lengthen the interval (startPoll's back-off): never a fixed 2 s hammer.
    for (let i = 1; i < 4; i++) { hx.timers[i - 1].fn(); fail(i); await flush(); }
    assert.equal(hx.timers.length, 4);
    assert.ok(hx.timers[3].ms > 2000, `after 4 failures the poll backs off (got ${hx.timers.map((t) => t.ms).join(",")})`);
    // A later success clears the error.
    hx.timers[3].fn(); hx.pending[4].resolve(trace()); await flush();
    assert.doesNotMatch(hx.root.textContent, /trace fetch failed/);
  } finally { hx.restore(); }
});

test("job.trace_summary.nodes_elided is shown beside the list, with truncated and the elided buckets", async () => {
  const hx = harness();
  try {
    hx.panel.setJob(job({ state: "done", resolution: NOT_SEARCHED, trace_summary: { nodes_elided: 412, nodes_recorded: 512, truncated: true } }));
    hx.pending[0].resolve(trace({
      bounds: { max_nodes: 512, max_bytes: 262144, truncated: true },
      elided: [{ stage: "S3", family: "psk", outcome: "pruned_floor", count: 412, bits_max: 4.6, bits_min: 0.2, evaluations: 1648 }],
    }));
    await flush();
    const notes = hx.root.findAll("tr-trunc").map((e) => e.textContent);
    assert.ok(notes.some((t) => /list truncated/.test(t)), notes.join(" | "));
    assert.ok(notes.some((t) => /\b412\b.*nodes_elided/.test(t)), `nodes_elided shown: ${notes.join(" | ")}`);
    assert.equal(hx.root.findAll("tr-elided").length, 1);
  } finally { hx.restore(); }
});

test("T-930 real shape: final:false + empty nodes while it runs, final:true once it ended without a trace — and the poll stops there", async () => {
  const hx = harness();
  try {
    // Exactly what this build's server returns for a running job: no trace yet, so no nodes and
    // `final:false` — NOT the mount fixtures' final:true-with-a-node.
    const running = (): TraceFetch => ({
      job_id: "a1", state: "searching", engine: "hk-synth", final: false, replay_key: null,
      bounds: { max_nodes: 512, max_bytes: 262144, truncated: false }, nodes: [], elided: [],
    });
    hx.panel.setJob(job());
    hx.pending[0].resolve(running());
    await flush();
    assert.doesNotMatch(hx.root.textContent, /loading trace/, "the empty answer replaces 'loading'");
    assert.match(hx.root.textContent, /no trace nodes recorded for this filter/);
    assert.equal(hx.timers.length, 1, "not final: the poll is re-armed");

    // The engine hands back: the job failed with no trace at all, and the backend says `final`
    // (T-930) instead of leaving `final:false` forever.
    hx.panel.setJob(job({ state: "failed", end_reason: "no_evaluator", error: { code: "no_evaluator", message: "no evaluator in this build" }, resolution: NOT_SEARCHED }));
    hx.timers[0].fn();
    hx.pending[1].resolve({ ...running(), state: "failed", final: true });
    await flush();
    assert.equal(hx.root.findAll("tr-res-headline")[0]?.textContent, NOT_SEARCHED.summary, "the headline is the answer, not the empty list");
    assert.equal(hx.timers.length, 1, "ended without a trace: the poll STOPS — no 2 s fetch forever");
    assert.equal(hx.pending.length, 2, "and nothing else was fetched");
  } finally { hx.restore(); }
});

test("T-930: the results say which filter they answer, so a failed fetch's error never labels the previous filter's list", async () => {
  const hx = harness();
  try {
    hx.panel.setJob(job({ state: "done", resolution: NOT_SEARCHED }));
    hx.pending[0].resolve(trace()); // the unfiltered list
    await flush();
    const shown = () => hx.root.findAll("tr-shown").map((e) => e.textContent);
    assert.deepEqual(shown(), ["showing: the whole trace (no filter)"]);

    // Ask "why not psk" through the family input (Enter), and have THAT fetch fail. The previous
    // filter's rows stay on screen on purpose, so they must still read as the whole trace's
    // answer — never as psk's.
    const input = hx.root.children[1].children[0];
    const enter = () => (input.onkeydown as (e: { key: string; preventDefault(): void }) => void)({ key: "Enter", preventDefault() {} });
    input.value = "psk";
    enter();
    await flush();
    assert.equal(hx.calls[hx.calls.length - 1], "GET /api/analyze/a1/trace?family=psk", hx.calls.join(", "));
    hx.pending[1].reject(new ControlError(404, "not_found", "no trace for job a1 (expired with the job)"));
    await flush();
    assert.match(hx.root.textContent, /trace fetch failed: HTTP 404/);
    assert.deepEqual(shown(), ["showing: the whole trace (no filter)"], "the surviving list is labelled with ITS filter, not the failed one");
    assert.equal(hx.root.findAll("tr-not-tried").length, 1, "and the rows themselves are kept");

    // When the filtered fetch succeeds, the label moves to it.
    enter();
    await flush();
    hx.pending[2].resolve(trace());
    await flush();
    assert.deepEqual(shown(), ["showing: family psk"]);
    assert.doesNotMatch(hx.root.textContent, /trace fetch failed/);
  } finally { hx.restore(); }
});
