// T-153 (ADR-0013 §4.6, §8): Decode workbench pure view-model functions, plot data mapping, the
// status-feed subscription lifecycle and params rendering helpers. No DOM under node:test (as
// app-shell.test.ts notes), so only pure functions and the socket-level status feed are exercised
// here; layout is checked by app-shell.test.ts's slot/CSS scan.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { ControlClient, type FetchFn } from "../src/controls/client";
import type { AppContext } from "../src/app/context";
import { createStore } from "../src/app/store";
import { initialState } from "../src/app/state";
import {
  autoDecode, findBlock, findNode, mergeCreated, pipelineChannelText, pipelineStatusChip, primaryOutput, publishPipeline,
  resolveTarget, startPipeline, subscribeDecodeFeed, type BlockDescriptor, type DecodeFeed, type Pipeline,
} from "../src/app/decode/pipelines";
import { nodeChip } from "../src/app/decode/stages";
import {
  CAPTURE_FRAME_LIMIT, capFrames, captureFor, captureFramesPath, decimate, decodeBits, decodeIq,
  decodeReal, parseBinaryRecord, pickPlots, tallyChannels, tallyCrcStatus, withinWindow,
} from "../src/app/decode/plots";
import {
  applyFragment, applyParam, assistRouteFor, coerceParamValue, nextNodeId, qualityTiles, recordsPerSecond,
} from "../src/app/decode/params";
import { subscribePipelineFeed } from "../src/app/decode/status-feed";
import { OUTPUTS_SUBJECT, PIPELINES_SUBJECT, STAGE_STATUS_SUBJECT, liveOnlyNote } from "../src/app/live-only";

// ---- fixtures ----

function mkPipeline(over: Partial<Pipeline> = {}): Pipeline {
  return {
    id: "p1", recipe_id: "rds", recipe_version: 1, edit_rev: 0, state: "running", end_reason: null,
    target: {}, channel: { center_hz: 101_300_000, bandwidth_hz: 200_000, sample_rate_hz: 200_000 },
    content_class: "unrestricted", emitter_id: "e1", started: 0,
    nodes: [{ id: "fm", block: "fm_demod", outputs: ["real"] }, { id: "sync", block: "sync_search", outputs: ["frames"] }],
    outputs: [{ id: "insp", kind: "inspector", stream_id: "inspector/p1/insp" }],
    status: {}, stats: { samples: 0, chunks: 0, frames: 0, gaps: 0, discontinuities: 0, skipped_samples: 0, edits: 0, status_ticks: 0, decodes: 0, decodes_dropped: 0 },
    warnings: [], follow_hops: null,
    ...over,
  };
}

function mkBlock(over: Partial<BlockDescriptor> = {}): BlockDescriptor {
  return { name: "fm_demod", version: 1, group: "iq", doc: "FM demodulator", inputs: [], outputs: [], params: [], params_pinned: true, ...over };
}

// ---- pipelines.ts ----

test("pipelineStatusChip: running, hopping and ended", () => {
  assert.deepEqual(pipelineStatusChip(mkPipeline()), { text: "running", cls: "run" });
  assert.deepEqual(pipelineStatusChip(mkPipeline({ follow_hops: { channels: [], channel_source: "list", channel_bandwidth_hz: 1e3, max_channels: 4 } })), { text: "hopping", cls: "hop" });
  assert.deepEqual(pipelineStatusChip(mkPipeline({ state: "ended", end_reason: "stopped" })), { text: "ended: stopped", cls: "end" });
});

test("pipelineChannelText: single channel vs. a hop set", () => {
  assert.equal(pipelineChannelText(mkPipeline()), "101.300 MHz · 1 channel");
  const p = mkPipeline({ follow_hops: { channels: [{ index: 0, center_hz: 152_480_000, bandwidth_hz: 1e4 }, { index: 1, center_hz: 157_740_000, bandwidth_hz: 1e4 }], channel_source: "list", channel_bandwidth_hz: 1e4, max_channels: 4 } });
  assert.equal(pipelineChannelText(p), "152.480 / 157.740 MHz");
});

test("resolveTarget prefers focused signal, then selection, then the view band, else null", () => {
  assert.deepEqual(resolveTarget({ kind: "signal", id: "e1" }, null), { target: { emitter_id: "e1" }, label: "the focused signal" });
  assert.deepEqual(resolveTarget({ kind: "selection", id: "s1" }, { loHz: 1e6, hiHz: 2e6 }), { target: { selection_id: "s1" }, label: "the focused selection" });
  assert.deepEqual(resolveTarget({ kind: "none" }, { loHz: 1e6, hiHz: 2e6 }), { target: { band: { f_lo: 1e6, f_hi: 2e6 } }, label: "the current view" });
  assert.equal(resolveTarget({ kind: "none" }, null), null);
});

test("findNode / findBlock / primaryOutput", () => {
  const p = mkPipeline();
  assert.equal(findNode(p, "sync")?.block, "sync_search");
  assert.equal(findNode(p, "missing"), null);
  assert.equal(findNode(p, null), null);
  const blocks = [mkBlock({ name: "fm_demod" }), mkBlock({ name: "sync_search" })];
  assert.equal(findBlock(blocks, "sync_search")?.group, "iq");
  assert.equal(findBlock(blocks, "nope"), null);
  assert.equal(primaryOutput(p)?.id, "insp");
  assert.equal(primaryOutput(mkPipeline({ outputs: [] })), null);
});

// ---- T-944: a created pipeline is listed from the moment the create returns ----

type Call = { method: string; url: string; body: unknown };
/** A fetch double: `routes` answers by "METHOD path"; a route mapped to a deferred promise holds its
 * answer until the test resolves it (a poll in flight across the create). */
function routedCtx(routes: Record<string, () => Promise<unknown> | unknown>): { ctx: AppContext; calls: Call[] } {
  const calls: Call[] = [];
  const fetchFn: FetchFn = async (url, init) => {
    const method = String(init.method);
    calls.push({ method, url, body: init.body ? JSON.parse(String(init.body)) : undefined });
    const r = routes[`${method} ${url}`];
    if (!r) return { ok: false, status: 404, statusText: "", json: () => Promise.resolve({ error: "no route", code: "not_found" }) };
    const v = await r();
    return { ok: true, status: method === "POST" ? 201 : 200, statusText: "", json: () => Promise.resolve(v) };
  };
  return { ctx: { store: createStore(initialState()), client: new ControlClient("tok", fetchFn), token: "tok" }, calls };
}
function deferred<T>() { let resolve!: (v: T) => void; const p = new Promise<T>((r) => { resolve = r; }); return { p, resolve }; }
const tick = () => new Promise((r) => setTimeout(r, 0));
function installWindow() { (globalThis as unknown as { window: unknown }).window = globalThis; }

test("mergeCreated keeps a pipeline created after the poll was asked, and defers to a poll asked after it", () => {
  const p1 = mkPipeline({ id: "p1" });
  const created = new Map([["p1", { p: p1, seq: 2 }]]);
  assert.deepEqual(mergeCreated([], created, 1).map((p) => p.id), ["p1"], "a stale poll does not drop it");
  assert.deepEqual(mergeCreated([], created, 3), [], "a poll asked after the create is authoritative (a stop)");
  const server = mkPipeline({ id: "p1", state: "ended", end_reason: "stopped" });
  assert.equal(mergeCreated([server], created, 1)[0].state, "ended", "the server's copy wins");
});

test("THE T-944 RACE: a poll in flight across the create cannot hide the created pipeline", async () => {
  installWindow();
  const poll = deferred<unknown>();
  const created = mkPipeline({ id: "p1", state: "running" });
  let firstPoll = true;
  const { ctx } = routedCtx({
    "GET /api/pipelines": () => (firstPoll ? (firstPoll = false, poll.p) : { pipelines: [created] }),
    "GET /api/recipes": () => ({ recipes: {} }),
    "GET /api/blocks": () => ({ blocks: [] }),
    "POST /api/pipelines": () => created,
  });
  const seen: DecodeFeed[] = [];
  const stop = subscribeDecodeFeed(ctx, (f) => seen.push(f));
  try {
    await tick(); // the first poll is now in flight, asked before the create
    const p = await startPipeline(ctx, { recipe_id: "rds", target: { emitter_id: "e1" } });
    assert.equal(p.id, "p1");
    assert.deepEqual(seen.at(-1)!.pipelines.map((x) => [x.id, x.state]), [["p1", "running"]], "listed the moment the create returns");
    assert.equal(ctx.store.get().decode.pipelineId, "p1", "and selected in the workbench");
    poll.resolve({ pipelines: [] }); // the stale answer, asked before the create
    await tick(); await tick();
    assert.deepEqual(seen.at(-1)!.pipelines.map((x) => x.id), ["p1"], "the stale [] does not hide it");
  } finally { stop(); }
});

test("a pipeline created with no decode panel mounted seeds the feed the panel opens with", async () => {
  installWindow();
  const created = mkPipeline({ id: "p7" });
  const { ctx } = routedCtx({ "GET /api/pipelines": () => new Promise(() => {}), "GET /api/recipes": () => ({ recipes: {} }), "GET /api/blocks": () => ({ blocks: [] }) });
  publishPipeline(ctx, created);
  let feed: DecodeFeed | null = null;
  const stop = subscribeDecodeFeed(ctx, (f) => { feed = f; });
  assert.deepEqual(feed!.pipelines.map((x) => x.id), ["p7"], "not 'no pipelines running' while the first poll is out");
  stop();
});

test("autoDecode starts the backend's best-ranked recipe on the emitter, and asks for exactly that", async () => {
  installWindow();
  const created = mkPipeline({ id: "p2", recipe_id: "rds" });
  const { ctx, calls } = routedCtx({
    "GET /api/recipes/match?emitter=e%201": () => ({ recipes: [
      { id: "rds", version: 3, name: "RDS: data on FM", score: 0.9, outcome: "fit" },
      { id: "wfm", version: 1, name: "Analog WFM", score: 0.7, outcome: "partial" },
    ] }),
    "POST /api/pipelines": () => created,
  });
  const res = await autoDecode(ctx, "e 1");
  assert.ok(res.ok);
  assert.equal(res.pipeline.id, "p2");
  const post = calls.find((c) => c.method === "POST")!;
  assert.deepEqual(post.body, { recipe_id: "rds", version: 3, target: { emitter_id: "e 1" } }, "the first choice, on the emitter");
  assert.equal(ctx.store.get().decode.pipelineId, "p2");
});

test("autoDecode with nothing offered starts nothing and says to pick from the list", async () => {
  const { ctx, calls } = routedCtx({ "GET /api/recipes/match?emitter=e1": () => ({ recipes: [], outcome: "none" }) });
  const res = await autoDecode(ctx, "e1");
  assert.deepEqual(res.ok ? null : res.reason, "no_match");
  assert.equal(calls.filter((c) => c.method === "POST").length, 0);
});

// ---- stages.ts ----

test("nodeChip reads <node>.lock / .quality / .error_rate, honestly falling back to em-dash", () => {
  assert.deepEqual(nodeChip({ "n.lock": "locked", "n.quality": 0.93 }, "n"), { text: "93%", cls: "ok" });
  assert.deepEqual(nodeChip({ "n.lock": "locked", "n.quality": 0.2 }, "n"), { text: "20%", cls: "warn" });
  assert.deepEqual(nodeChip({ "n.lock": "lost" }, "n"), { text: "lost", cls: "bad" });
  assert.deepEqual(nodeChip({ "n.error_rate": 0.2 }, "n"), { text: "20.0% err", cls: "bad" });
  assert.deepEqual(nodeChip({}, "n"), { text: "—", cls: "" });
});

// ---- plots.ts ----

test("pickPlots: a frames output picks tallies; hop sets add a channel map; otherwise sync-search is gap 6", () => {
  const framesBlock = mkBlock({ name: "sync_search", outputs: [{ name: "frames", types: ["frames"], diagnostic: false }] });
  const [a1, b1] = pickPlots({ id: "sync", block: "sync_search", outputs: ["frames"] }, framesBlock, false);
  assert.equal(a1.kind, "frame-tally-crc");
  assert.equal(b1.kind, "gap");
  assert.equal((b1 as { gap: number }).gap, 6);
  const [, b2] = pickPlots({ id: "sync", block: "sync_search", outputs: ["frames"] }, framesBlock, true);
  assert.equal(b2.kind, "channel-map");
});

test("pickPlots: soft -> tap + eye/timing gap 5; iq -> tap + spectrum gap 4; bits -> tap + gap 5", () => {
  const soft = mkBlock({ outputs: [{ name: "soft", types: ["soft"], diagnostic: false }] });
  const [a, b] = pickPlots({ id: "clk", block: "clock_recovery", outputs: ["soft"] }, soft, false);
  assert.deepEqual(a, { kind: "tap", port: "soft", portType: "soft", title: "Soft symbol values", note: "soft", caption: "raw samples from the soft port" });
  assert.equal((b as { gap: number }).gap, 5);

  const iq = mkBlock({ outputs: [{ name: "iq", types: ["iq"], diagnostic: false }] });
  const [, b2] = pickPlots({ id: "mix", block: "mix", outputs: ["iq"] }, iq, false);
  assert.equal((b2 as { gap: number }).gap, 4);

  const bits = mkBlock({ outputs: [{ name: "bits", types: ["bits"], diagnostic: false }] });
  const [a3, b3] = pickPlots({ id: "b", block: "fsk_demod", outputs: ["bits"] }, bits, false);
  assert.equal(a3.kind, "tap");
  assert.equal((b3 as { gap: number }).gap, 5);
});

test("pickPlots: no node/block, and a block with no output port, are honest placeholders", () => {
  const [a, b] = pickPlots(null, null, false);
  assert.equal(a.kind, "gap"); assert.equal(a.caption, "select a stage");
  const noOut = mkBlock({ outputs: [] });
  const [a2, b2] = pickPlots({ id: "x", block: "x", outputs: [] }, noOut, false);
  assert.equal(a2.caption, "this block has no diagnostic port");
  assert.equal(b2.caption, "this block has no diagnostic port");
});

function binaryRecord(tNs: bigint, floats: number[]): ArrayBuffer {
  const buf = new ArrayBuffer(32 + floats.length * 4);
  const dv = new DataView(buf);
  dv.setUint8(0, 1); // data record
  dv.setBigInt64(16, tNs, true);
  floats.forEach((v, i) => dv.setFloat32(32 + i * 4, v, true));
  return buf;
}

test("parseBinaryRecord + decode{Iq,Real,Bits}: header parsing and payload decoding", () => {
  const rec = parseBinaryRecord(binaryRecord(1_000_000_000n, [0.5, -0.25, 0.1, 0.2]));
  assert.ok(rec);
  assert.equal(rec!.tS, 1);
  assert.equal(rec!.gated, false);
  const round = (p: { x: number; y: number }) => ({ x: Math.round(p.x * 100) / 100, y: Math.round(p.y * 100) / 100 });
  assert.deepEqual(decodeIq(rec!.payload).map(round), [{ x: 0.5, y: -0.25 }, { x: 0.1, y: 0.2 }]);
  assert.deepEqual(decodeReal(rec!.payload).map((v) => Math.round(v * 100) / 100), [0.5, -0.25, 0.1, 0.2]);
  assert.equal(parseBinaryRecord(new ArrayBuffer(8)), null);
});

test("decodeBits reads one byte per bit", () => {
  const buf = new ArrayBuffer(32 + 3);
  new Uint8Array(buf, 32, 3).set([1, 0, 1]);
  assert.deepEqual(decodeBits(buf.slice(32)), [1, 0, 1]);
});

test("decimate keeps at most `max` evenly spaced points, and returns short arrays unchanged", () => {
  assert.deepEqual(decimate([1, 2, 3], 10), [1, 2, 3]);
  assert.equal(decimate(Array.from({ length: 1000 }, (_, i) => i), 100).length, 100);
});

test("tallyCrcStatus and tallyChannels: sorted descending, honest 'unknown' bucket", () => {
  const frames = [
    { crc_status: "valid", metadata: { channel_hz: 101_300_000 } },
    { crc_status: "valid", metadata: { channel_hz: 101_300_000 } },
    { crc_status: "invalid", metadata: { channel_hz: 102_100_000 } },
    { crc_status: undefined, metadata: {} },
  ];
  assert.deepEqual(tallyCrcStatus(frames), [{ label: "valid", count: 2 }, { label: "invalid", count: 1 }, { label: "unknown", count: 1 }]);
  assert.deepEqual(tallyChannels(frames), [{ label: "101.300 MHz", count: 2 }, { label: "102.100 MHz", count: 1 }, { label: "unknown", count: 1 }]);
});

// ---- T-384: the plots are a view over the same window as the waterfall ----

/** The capture clock these fixtures run on, deliberately far from any wall clock — the fixture
 * that exposed T-379 sat 3.5 days from `Date.now()`, and these tests must fail if a browser clock
 * creeps back into the plots. */
const CAPTURE_EDGE_S = 1_789_297_847;

test("withinWindow keeps the frames inside the view window, on the capture clock", () => {
  const at = (s: number) => ({ t_ns: (CAPTURE_EDGE_S + s) * 1e9, crc_status: "valid" });
  const frames = [at(-70), at(-30), at(0)];
  assert.equal(withinWindow(frames, { t0: CAPTURE_EDGE_S - 60, t1: CAPTURE_EDGE_S }).length, 2);
  // Closed on both ends, and the window's *start* excludes as much as its end does — the old
  // signature had only a trailing edge, so a scrubbed-back window could not exclude newer frames.
  assert.equal(withinWindow(frames, { t0: CAPTURE_EDGE_S - 70, t1: CAPTURE_EDGE_S - 30 }).length, 2);
  assert.equal(withinWindow(frames, { t0: CAPTURE_EDGE_S - 50, t1: CAPTURE_EDGE_S - 40 }).length, 0);
});

test("THE DISTINGUISHING TEST: the same frames tally on the capture clock and vanish on the browser's", () => {
  // This is the bug, reproduced: `withinWindow(frames, Date.now() * 1e6, 60)` compared a wall-clock
  // instant against capture-clock `t_ns`. On the replay behind T-379 the two sat 306,315 s apart,
  // so a "last 60 s" tally of frames that had certainly arrived rendered permanently empty — and on
  // a source whose clock runs ahead instead, every frame passes and the 60 s tally silently becomes
  // an all-time one. Either way the plot is not a view of the window it claims.
  const frames = [
    { t_ns: (CAPTURE_EDGE_S - 5) * 1e9, crc_status: "valid" },
    { t_ns: (CAPTURE_EDGE_S - 3) * 1e9, crc_status: "valid" },
    { t_ns: (CAPTURE_EDGE_S - 1) * 1e9, crc_status: "invalid" },
  ];
  const capture = withinWindow(frames, { t0: CAPTURE_EDGE_S - 20, t1: CAPTURE_EDGE_S });
  assert.deepEqual(
    tallyCrcStatus(capture),
    [{ label: "valid", count: 2 }, { label: "invalid", count: 1 }],
    "THE CONTROL: a window that DOES hold frames tallies them, by label and count",
  );

  const wallNowS = CAPTURE_EDGE_S + 306_315; // the measured offset from T-379's fixture
  const wall = withinWindow(frames, { t0: wallNowS - 20, t1: wallNowS });
  assert.deepEqual(tallyCrcStatus(wall), [], "the browser's clock selects none of them");
  assert.notDeepEqual(tallyCrcStatus(capture), tallyCrcStatus(wall), "the two clocks are not interchangeable");
});

test("capFrames bounds the live buffer by count, never by a clock", () => {
  // A time-trimmed buffer drops live frames while the view is scrubbed back, so returning to Live
  // would find the plot empty of frames it had already received.
  const frames = Array.from({ length: 10 }, (_, i) => ({ t_ns: (CAPTURE_EDGE_S + i) * 1e9 }));
  assert.deepEqual(capFrames(frames, 20), frames, "under the cap, unchanged");
  const kept = capFrames(frames, 3);
  assert.equal(kept.length, 3);
  assert.deepEqual(kept.map((f) => f.t_ns), frames.slice(7).map((f) => f.t_ns), "the newest are kept");
});

test("a window the live tap never carried is fetched from the pipeline's capture, over exactly that window", () => {
  // The rule's positive half: frames for a scrubbed window exist (the pipeline records them), so
  // they must be shown. The socket has no history form, so the capture is where they come from.
  const captures = [
    { id: "cap-old", pipeline_id: "p1", t_last: CAPTURE_EDGE_S - 600 },
    { id: "cap-new", pipeline_id: "p1", t_last: CAPTURE_EDGE_S },
    { id: "cap-other", pipeline_id: "p2", t_last: CAPTURE_EDGE_S },
  ];
  assert.equal(captureFor(captures, "p1")?.id, "cap-new", "the most recently written capture of that pipeline");
  assert.equal(captureFor(captures, "p3"), null, "and null rather than another pipeline's");

  const w = { t0: CAPTURE_EDGE_S - 20, t1: CAPTURE_EDGE_S };
  const q = new URLSearchParams(captureFramesPath("cap/new", w).split("?")[1]);
  assert.ok(captureFramesPath("cap/new", w).startsWith("/api/captures/cap%2Fnew/frames?"), "the id is escaped");
  assert.equal(Number(q.get("from_t")), w.t0, "from_t/to_t are Unix seconds on the capture clock");
  assert.equal(Number(q.get("to_t")), w.t1);
  assert.equal(Number(q.get("limit")), CAPTURE_FRAME_LIMIT, "the route's documented maximum, so a dense window truncates rather than widens");
});

// ---- params.ts ----

test("nextNodeId walks the pipeline's node order and stops at the end", () => {
  const p = mkPipeline();
  assert.equal(nextNodeId(p, "fm"), "sync");
  assert.equal(nextNodeId(p, "sync"), null);
  assert.equal(nextNodeId(p, "missing"), null);
});

test("applyParam / applyFragment deep-clone and never mutate the source recipe", () => {
  const recipe = { schema: "hackriff.recipe", schema_version: 1, id: "rds", version: 1, name: "RDS", input: { port: "iq" as const }, nodes: [{ id: "sync", block: "sync_search", params: { period_bits: 26 } }] };
  const withParam = applyParam(recipe, "sync", "period_bits", 32);
  assert.equal(withParam.nodes[0].params.period_bits, 32);
  assert.equal(recipe.nodes[0].params.period_bits, 26); // source untouched
  const withFragment = applyFragment(recipe, "sync", { "sync-word": "0x0FC" });
  assert.deepEqual(withFragment.nodes[0].params, { period_bits: 26, "sync-word": "0x0FC" });
});

test("coerceParamValue: format checks only, never a semantic judgement", () => {
  assert.deepEqual(coerceParamValue({ name: "n", type: "bool", required: false, hot: true, doc: "" }, "true"), { ok: true, value: true });
  assert.equal(coerceParamValue({ name: "n", type: "bool", required: false, hot: true, doc: "" }, "yes").ok, false);
  assert.deepEqual(coerceParamValue({ name: "n", type: "int", required: false, hot: true, doc: "" }, "12"), { ok: true, value: 12 });
  assert.equal(coerceParamValue({ name: "n", type: "int", required: false, hot: true, doc: "" }, "1.5").ok, false);
  assert.deepEqual(coerceParamValue({ name: "n", type: "float", required: false, hot: true, doc: "" }, "1187.5"), { ok: true, value: 1187.5 });
  assert.deepEqual(coerceParamValue({ name: "n", type: "hex", max_bits: 16, required: false, hot: true, doc: "" }, "0x5B9"), { ok: true, value: "0x5B9" });
});

test("recordsPerSecond: only forward, never-decreasing counters", () => {
  assert.equal(recordsPerSecond(100, 0, 150, 10), 5);
  assert.equal(recordsPerSecond(100, 0, 100, 10), 0);
  assert.equal(recordsPerSecond(100, 0, 50, 10), null); // a reset counter is not a rate
  assert.equal(recordsPerSecond(100, 10, 150, 10), null); // no elapsed time
});

test("qualityTiles: honest em-dash when a status key is absent", () => {
  const p = mkPipeline({ status: { "sync.lock": "locked", "sync.error_rate": 0.02 } });
  const tiles = qualityTiles(p, "sync", 3.5);
  assert.deepEqual(tiles.map((t) => t.label), ["Check pass rate", "Records / s", "Lock"]);
  assert.equal(tiles[0].value, "98.0%");
  assert.equal(tiles[1].value, "3.5");
  assert.equal(tiles[2].value, "locked");
  assert.equal(qualityTiles(p, null, null)[1].value, "—");
});

test("assistRouteFor: only the blocks assist actually covers; everything else is gap 7a", () => {
  assert.equal(assistRouteFor("sync_search"), "/api/assist/sync");
  assert.equal(assistRouteFor("crc"), "/api/assist/crc");
  assert.equal(assistRouteFor("bch"), "/api/assist/crc");
  assert.equal(assistRouteFor("fields"), "/api/assist/fields");
  assert.equal(assistRouteFor("fm_demod"), null);
  assert.equal(assistRouteFor("clock_recovery"), null);
});

// ---- status-feed.ts: one ref-counted socket per pipeline ----

type Ev = { data: string | ArrayBuffer };
class FakeSocket {
  static instances: FakeSocket[] = [];
  url: string;
  binaryType = "";
  onmessage: ((ev: Ev) => void) | null = null;
  onclose: (() => void) | null = null;
  closed = false;
  constructor(url: string) { this.url = url; FakeSocket.instances.push(this); }
  close() { if (!this.closed) { this.closed = true; this.onclose?.(); } }
  header(h: Record<string, unknown> = { schema: "hackriff.stream/1" }) { this.onmessage?.({ data: JSON.stringify(h) }); }
  send(rec: Record<string, unknown>) { this.onmessage?.({ data: JSON.stringify(rec) }); }
}

function installFakeWs() {
  FakeSocket.instances = [];
  (globalThis as unknown as { WebSocket: unknown }).WebSocket = FakeSocket;
  (globalThis as unknown as { location: unknown }).location = { protocol: "http:", host: "test.local" };
}

function fakeCtx(): AppContext {
  const fetchFn: FetchFn = () => Promise.resolve({ ok: true, status: 200, statusText: "", json: () => Promise.resolve({}) });
  return { store: createStore(initialState()), client: new ControlClient("tok", fetchFn), token: "tok" };
}

test("subscribePipelineFeed: one socket per pipeline across subscribers, closed only when the last leaves", () => {
  installFakeWs();
  const ctx = fakeCtx();
  const a: unknown[] = [], b: unknown[] = [];
  const unsubA = subscribePipelineFeed(ctx, "p1", { status: (u) => a.push(u) });
  assert.equal(FakeSocket.instances.length, 1, "first subscriber opens one socket");
  const unsubB = subscribePipelineFeed(ctx, "p1", { status: (u) => b.push(u) });
  assert.equal(FakeSocket.instances.length, 1, "a second subscriber to the same pipeline opens no new socket");

  const sock = FakeSocket.instances[0];
  sock.header();
  sock.send({ type: "status", t_ns: 5_000_000_000, metadata: { "sync.lock": "locked" } });
  assert.equal(a.length, 1);
  assert.equal(b.length, 1, "both subscribers receive the same status record");
  assert.deepEqual(a[0], { tS: 5, values: { "sync.lock": "locked" } });

  unsubA();
  assert.equal(sock.closed, false, "the socket stays open while a subscriber remains");
  unsubB();
  assert.equal(sock.closed, true, "the last unsubscribe closes the socket");
});

test("subscribePipelineFeed: different pipelines get different sockets; a refusal is terminal (state 'closed', no reconnect)", () => {
  installFakeWs();
  const ctx = fakeCtx();
  const states: string[] = [];
  subscribePipelineFeed(ctx, "p1", { state: (s) => states.push(s) });
  subscribePipelineFeed(ctx, "p2", { state: () => {} });
  assert.equal(FakeSocket.instances.length, 2);

  FakeSocket.instances[0].onmessage?.({ data: JSON.stringify({ type: "refused", status: 404, code: "not_found", reason: "no such pipeline" }) });
  assert.deepEqual(states, ["connecting", "closed"]);
});

test("subscribePipelineFeed: frames reach only the `frame` handler, and resubscribing after every unsubscribe opens a fresh socket", () => {
  installFakeWs();
  const ctx = fakeCtx();
  const frames: unknown[] = [];
  const unsub = subscribePipelineFeed(ctx, "p1", { frame: (f) => frames.push(f) });
  FakeSocket.instances[0].header();
  FakeSocket.instances[0].send({ type: "frame", seq: 1, t_ns: 1e9, metadata: { channel: 0 } });
  assert.equal(frames.length, 1);
  unsub();
  assert.equal(FakeSocket.instances[0].closed, true);

  subscribePipelineFeed(ctx, "p1", { frame: () => {} });
  assert.equal(FakeSocket.instances.length, 2, "a fresh subscribe after the last unsubscribe opens a new socket");
});

// ---- layout: narrow-width rules (ADR-0013 §5; the T-151/T-152 CSS-text technique) ----

test("decode.css: the plots grid collapses to one column at 900px, and no rule needs more than 400px", () => {
  const css = readFileSync("src/app/decode/decode.css", "utf8").replace(/\/\*[\s\S]*?\*\//g, "");
  assert.match(css, /@media \(max-width:\s*900px\)\s*\{\s*\.plots\s*\{[^}]*grid-template-columns:\s*1fr/);
  for (const m of css.matchAll(/min-width:\s*(\d+)px/g)) assert.ok(Number(m[1]) <= 400);
  // the node chain is inherently wide, so it gets its own horizontal scroller (base.css) rather
  // than collapsing; that's the ADR-0013 §5 "inherently wide" exception, not a layout bug.
  assert.match(readFileSync("src/app/base.css", "utf8"), /\.pipe\s*\{[^}]*overflow-x:\s*auto/);
});

test("inspector.css: the three-pane inspector stacks to one column with capped heights at 900px", () => {
  const css = readFileSync("src/app/decode/inspector.css", "utf8").replace(/\/\*[\s\S]*?\*\//g, "");
  assert.match(css, /@media \(max-width:\s*900px\)\s*\{[\s\S]*\.insp\s*\{[^}]*grid-template-columns:\s*1fr/);
  assert.match(css, /\.insp-frames\s*\{[^}]*max-height:\s*\d+px/);
  assert.match(css, /\.insp-bytes\s*\{[^}]*max-height:\s*\d+px/);
  assert.match(css, /\.insp-tree\s*\{[^}]*max-height:\s*\d+px/);
  for (const m of css.matchAll(/min-width:\s*(\d+)px/g)) assert.ok(Number(m[1]) <= 400);
});

// ---- T-387: the three surfaces that describe THE RUN, not THE AIR, and say so ----------------

test("THE PER-SURFACE DECISION: a live-only surface says it is live-only, and says it louder once scrubbed", () => {
  // The prior question T-387 settled before touching any contract: the pipelines list, the stage
  // status strip and the outputs dock describe the *run* — which decoder processes exist, how a
  // node is reading now, which sockets this page holds. None has a past-window form, and none
  // should: a socket this tab holds cannot exist in a window an hour ago. Being live-only is not
  // the bug; *looking* windowed while being live-only is, which is the same class of lie as the
  // focus panel's "no longer in the inventory" (T-385).
  for (const subject of [PIPELINES_SUBJECT, STAGE_STATUS_SUBJECT, OUTPUTS_SUBJECT]) {
    const live = liveOnlyNote(subject, false);
    const scrubbed = liveOnlyNote(subject, true);
    assert.match(live, /^live only — /, "the claim is made, not implied");
    assert.ok(live.includes(subject) && scrubbed.includes(subject));
    assert.notEqual(live, scrubbed, "the note changes when the view stops following the live edge");
    assert.match(scrubbed, /scrubbed back; this is not that window/);
  }
  // Three surfaces, three subjects — the reader is told *what* is live-only, not merely that
  // something is.
  assert.equal(new Set([PIPELINES_SUBJECT, STAGE_STATUS_SUBJECT, OUTPUTS_SUBJECT]).size, 3);
});

test("the live-only note never depends on whether the surface is empty", () => {
  // If it did, an empty live-only panel would read as an answer about the window — exactly the
  // confusion the note exists to prevent. Its only inputs are the subject and Play/Pause.
  assert.equal(liveOnlyNote(PIPELINES_SUBJECT, false), liveOnlyNote(PIPELINES_SUBJECT, false));
  assert.equal(liveOnlyNote.length, 2);
});
