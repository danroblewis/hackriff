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
  findBlock, findNode, pipelineChannelText, pipelineStatusChip, primaryOutput, resolveTarget,
  type BlockDescriptor, type Pipeline,
} from "../src/app/decode/pipelines";
import { nodeChip } from "../src/app/decode/stages";
import {
  decimate, decodeBits, decodeIq, decodeReal, parseBinaryRecord, pickPlots, tallyChannels,
  tallyCrcStatus, withinWindow,
} from "../src/app/decode/plots";
import {
  applyFragment, applyParam, assistRouteFor, coerceParamValue, nextNodeId, qualityTiles, recordsPerSecond,
} from "../src/app/decode/params";
import { subscribePipelineFeed } from "../src/app/decode/status-feed";

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

test("withinWindow keeps only frames within the trailing window", () => {
  const now = 100 * 1e9;
  const frames = [{ t: now - 70 * 1e9 }, { t: now - 30 * 1e9 }, { t: now }];
  assert.equal(withinWindow(frames, now, 60).length, 2);
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
  sock.send({ type: "status", t: 5_000_000_000, metadata: { "sync.lock": "locked" } });
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
  FakeSocket.instances[0].send({ type: "frame", seq: 1, t: 1e9, metadata: { channel: 0 } });
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
