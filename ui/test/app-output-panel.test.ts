// T-195 (ADR-0013 §4.5/§4.7, docs/14-ui-rewrite.md "Added scope from docs/15 §7"): per-signal
// output panels — which signal's panel a tab strip should bind (digital pipeline vs. audio Listen
// stream, and tab selection), the RDS decode-fields view-model, and the audio scope's
// sample->pixel mapping. All pure; no DOM under node:test (see ui/test/inventory.test.ts's note).
import { test } from "node:test";
import assert from "node:assert/strict";
import type { OutputEntry } from "../src/app/state";
import {
  collectPanelSources, nextPanelTab, panelsEmptyText, rdsViewModel, scopePoints, trimRds, ScopeBuffer,
  type DecodeRow, type PanelSource, type PipelineLite,
} from "../src/app/explore/output-panel";

function audioOutput(over: Partial<OutputEntry> = {}): OutputEntry {
  return {
    id: "listen1", kind: "audio", label: "101.3 MHz", sub: "", state: "live", tcpTarget: null,
    muted: false, levelDbfs: -20, recordsPerS: null, emitterId: "em-fm", pipelineId: null, message: null,
    ...over,
  };
}

function pipeline(over: Partial<PipelineLite> = {}): PipelineLite {
  return { id: "p1", emitter_id: "em-digi", state: "running", outputs: [{ id: "o1", kind: "inspector", stream_id: "inspector/p1/o1" }], ...over };
}

// ---- collectPanelSources / nextPanelTab ----

test("collectPanelSources: a running pipeline with an inspector output makes a digital panel", () => {
  const sources = collectPanelSources([], [pipeline()]);
  assert.deepEqual(sources, [{ emitterId: "em-digi", kind: "digital", pipelineId: "p1" }]);
});

test("collectPanelSources: an ended pipeline, or one with no inspector output, contributes nothing", () => {
  assert.deepEqual(collectPanelSources([], [pipeline({ state: "ended" })]), []);
  assert.deepEqual(collectPanelSources([], [pipeline({ outputs: [{ id: "o1", kind: "messages", stream_id: "x" }] })]), []);
  assert.deepEqual(collectPanelSources([], [pipeline({ emitter_id: null })]), []);
});

test("collectPanelSources: a live or opening Listen entry makes an audio panel; other states don't", () => {
  assert.deepEqual(collectPanelSources([audioOutput()], []), [{ emitterId: "em-fm", kind: "audio", outputId: "listen1" }]);
  assert.deepEqual(collectPanelSources([audioOutput({ state: "opening" })], []), [{ emitterId: "em-fm", kind: "audio", outputId: "listen1" }]);
  assert.deepEqual(collectPanelSources([audioOutput({ state: "refused" })], []), []);
  assert.deepEqual(collectPanelSources([audioOutput({ state: "ended" })], []), []);
  assert.deepEqual(collectPanelSources([{ ...audioOutput(), kind: "records" }], []), []);
});

test("collectPanelSources: a digital pipeline wins over an audio entry for the same emitter, and order is pipelines-then-dock", () => {
  const sources = collectPanelSources([audioOutput({ emitterId: "em-digi" }), audioOutput({ id: "listen2", emitterId: "em-fm" })], [pipeline()]);
  assert.deepEqual(sources, [
    { emitterId: "em-digi", kind: "digital", pipelineId: "p1" },
    { emitterId: "em-fm", kind: "audio", outputId: "listen2" },
  ]);
});

test("nextPanelTab keeps the current tab while it's still available, else the first available, else none", () => {
  const sources: PanelSource[] = [{ emitterId: "a", kind: "audio", outputId: "o" }, { emitterId: "b", kind: "digital", pipelineId: "p" }];
  assert.equal(nextPanelTab("b", sources), "b");
  assert.equal(nextPanelTab("gone", sources), "a");
  assert.equal(nextPanelTab(null, sources), "a");
  assert.equal(nextPanelTab("a", []), null);
});

test("panelsEmptyText: a quiet empty-state string with no sources, null once any panel exists", () => {
  assert.match(panelsEmptyText([]) ?? "", /no active outputs/i);
  assert.equal(panelsEmptyText([{ emitterId: "a", kind: "audio", outputId: "o" }]), null);
});

// ---- rdsViewModel ----

function decodeRow(over: Partial<DecodeRow> = {}): DecodeRow {
  return { decoder: "hk-rds", recipe_id: null, frame_model: "rds-pi", at: 1789300820.5, fields: {}, crc: { valid: true }, source_session: null, ...over };
}

test("rdsViewModel is null with no RDS-family decode rows (a signal with no decode yet)", () => {
  assert.equal(rdsViewModel([]), null);
  assert.equal(rdsViewModel([decodeRow({ frame_model: "adsb-icao", fields: { icao: "A1B2C3" } })]), null);
});

test("rdsViewModel merges PI/PS/RT across their separate rows into one model", () => {
  const rows: DecodeRow[] = [
    decodeRow({ frame_model: "rds-ps", at: 3, fields: { ps: "KROQ    " } }),
    decodeRow({ frame_model: "rds-rt", at: 2, fields: { rt: "Now playing" } }),
    decodeRow({ frame_model: "rds-pi", at: 1, fields: { pi: "C0DE", pty: 10, tp: false, ta: true } }),
  ];
  assert.deepEqual(rdsViewModel(rows), { ps: "KROQ    ", rt: "Now playing", pi: "C0DE", pty: 10, tp: false, ta: true, updatedAtS: 3 });
});

test("rdsViewModel: the first (newest, per the API's newest-first order) row wins when a field repeats", () => {
  const rows: DecodeRow[] = [
    decodeRow({ frame_model: "rds-ps", at: 5, fields: { ps: "NEW STN " } }),
    decodeRow({ frame_model: "rds-ps", at: 1, fields: { ps: "OLD STN " } }),
  ];
  const vm = rdsViewModel(rows);
  assert.equal(vm?.ps, "NEW STN ");
  assert.equal(vm?.updatedAtS, 5);
});

test("rdsViewModel reads exactly the named fields as committed, never inventing a missing one", () => {
  const vm = rdsViewModel([decodeRow({ fields: { pi: "C0DE" } })]);
  assert.deepEqual(vm, { ps: null, rt: null, pi: "C0DE", pty: null, tp: null, ta: null, updatedAtS: 1789300820.5 });
});

test("trimRds trims padding, and a blank/absent field is null (not an empty string)", () => {
  assert.equal(trimRds("KROQ    "), "KROQ");
  assert.equal(trimRds("        "), null);
  assert.equal(trimRds(null), null);
});

// ---- audio scope ----

test("ScopeBuffer keeps the most recent `capacity` samples, oldest dropped first", () => {
  const b = new ScopeBuffer(4);
  b.push([1, 2]);
  assert.deepEqual(Array.from(b.snapshot()), [1, 2]);
  b.push([3, 4, 5]); // now 6 pushed total, capacity 4 -> [2,3,4,5]
  assert.deepEqual(Array.from(b.snapshot()), [2, 3, 4, 5]);
});

test("ScopeBuffer: a single push longer than capacity keeps only its own tail", () => {
  const b = new ScopeBuffer(3);
  b.push([1, 2, 3, 4, 5]);
  assert.deepEqual(Array.from(b.snapshot()), [3, 4, 5]);
});

test("scopePoints maps one x per pixel column, nearest sample, y about the vertical centre", () => {
  const pts = scopePoints([1, 0, -1, 0], 4, 10);
  assert.equal(pts, "0,0.0 1,5.0 2,10.0 3,5.0");
});

test("scopePoints clamps out-of-range samples to +-1, and is empty with no samples or no room", () => {
  assert.equal(scopePoints([2, -2], 2, 10), "0,0.0 1,10.0");
  assert.equal(scopePoints([], 10, 10), "");
  assert.equal(scopePoints([1], 0, 10), "");
});
