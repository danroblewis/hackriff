// T-195 (ADR-0013 §4.5/§4.7, docs/14-ui-rewrite.md "Added scope from docs/15 §7"): per-signal
// output panels — which signal's panel a tab strip should bind (digital pipeline vs. audio Listen
// stream, and tab selection), the RDS decode-fields view-model, and the audio scope's
// sample->pixel mapping. All pure; no DOM under node:test (see ui/test/inventory.test.ts's note).
import { test } from "node:test";
import assert from "node:assert/strict";
import type { OutputEntry } from "../src/app/state";
import {
  collectPanelSources, nextPanelTab, panelsEmptyText, rdsFieldText, rdsIdentity, rdsViewModel, scopePoints,
  trimRds, ScopeBuffer,
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

// ---- RDS pipeline recognition (T-252: the rds.recipe.json messages outputs win over the raw
// packet inspector, so the accumulated view isn't hidden behind the per-frame "groups" stream) ----

function rdsPipeline(over: Partial<PipelineLite> = {}): PipelineLite {
  return {
    id: "p2", emitter_id: "em-fm", state: "running",
    outputs: [
      { id: "groups", kind: "inspector", stream_id: "inspector/p2/groups" },
      { id: "group-info", kind: "messages", stream_id: "decodes/p2/group-info" },
      { id: "station", kind: "messages", stream_id: "decodes/p2/station" },
      { id: "radiotext", kind: "messages", stream_id: "decodes/p2/radiotext" },
    ],
    ...over,
  };
}

test("collectPanelSources: a running RDS-recipe pipeline (messages output ids group-info/station/radiotext) gets an rds panel, not the raw inspector", () => {
  assert.deepEqual(collectPanelSources([], [rdsPipeline()]), [{ emitterId: "em-fm", kind: "rds", pipelineId: "p2" }]);
});

test("collectPanelSources: an rds panel still wins over an audio Listen entry on the same emitter", () => {
  const sources = collectPanelSources([audioOutput({ emitterId: "em-fm" })], [rdsPipeline()]);
  assert.deepEqual(sources, [{ emitterId: "em-fm", kind: "rds", pipelineId: "p2" }]);
});

test("collectPanelSources: a non-RDS pipeline with messages+inspector outputs (e.g. ADS-B) still gets the raw digital inspector", () => {
  const sources = collectPanelSources([], [rdsPipeline({
    outputs: [
      { id: "frames", kind: "inspector", stream_id: "inspector/p2/frames" },
      { id: "aircraft", kind: "messages", stream_id: "decodes/p2/aircraft" },
    ],
  })]);
  assert.deepEqual(sources, [{ emitterId: "em-fm", kind: "digital", pipelineId: "p2" }]);
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

// ---- rdsViewModel (T-252: over rds.recipe.json's actual three outputs — group-info/rds-group,
// station/rds-ps, radiotext/rds-rt — not the built-in hk-rds plugin's differently-shaped rows) ----

function decodeRow(over: Partial<DecodeRow> = {}): DecodeRow {
  return { decoder: "recipe:rds", recipe_id: "rds", frame_model: "rds-group", at: 1789300820.5, fields: {}, crc: { valid: true }, source_session: null, ...over };
}

test("rdsViewModel is null with no RDS recipe decode rows (a signal with no decode yet)", () => {
  assert.equal(rdsViewModel([]), null);
  assert.equal(rdsViewModel([decodeRow({ frame_model: "adsb-icao", fields: { icao: "A1B2C3" } })]), null);
  // A different decoder's own RDS frame model (e.g. the built-in hk-rds plugin's "rds-pi") is out
  // of this recipe-scoped view model's scope, not a fabricated merge of two unrelated shapes.
  assert.equal(rdsViewModel([decodeRow({ decoder: "hk-rds", recipe_id: null, frame_model: "rds-pi", fields: { pi: "C0DE" } })]), null);
});

test("rdsViewModel merges PS/RT/TP/PTY across their separate rows into one model, each read from its own row's actual field key", () => {
  const rows: DecodeRow[] = [
    decodeRow({ frame_model: "rds-ps", at: 3, fields: { text: "KROQ    " } }),
    decodeRow({ frame_model: "rds-rt", at: 2, fields: { text: "Now playing" } }),
    decodeRow({ frame_model: "rds-group", at: 1, fields: { group_type: 0, version: "A", pty: 10, tp: false } }),
  ];
  assert.deepEqual(rdsViewModel(rows), { ps: "KROQ    ", rt: "Now playing", tp: false, pty: 10, updatedAtS: 3 });
});

test("rdsViewModel: the first (newest, per the API's newest-first order) row wins when a field repeats", () => {
  const rows: DecodeRow[] = [
    decodeRow({ frame_model: "rds-ps", at: 5, fields: { text: "NEW STN " } }),
    decodeRow({ frame_model: "rds-ps", at: 1, fields: { text: "OLD STN " } }),
  ];
  const vm = rdsViewModel(rows);
  assert.equal(vm?.ps, "NEW STN ");
  assert.equal(vm?.updatedAtS, 5);
});

test("rdsViewModel reads exactly the named fields as committed, never inventing a missing one", () => {
  const vm = rdsViewModel([decodeRow({ frame_model: "rds-group", fields: { tp: true } })]);
  assert.deepEqual(vm, { ps: null, rt: null, tp: true, pty: null, updatedAtS: 1789300820.5 });
});

test("rdsViewModel: PI is never read off a decode row — the recipe only ever uses it as the row's identity, never a mapped field", () => {
  const vm = rdsViewModel([decodeRow({ frame_model: "rds-group", fields: { pi: "C0DE", tp: true } })]);
  // "pi" in fields (if a caller ever mistakenly put one there) is simply not part of this model.
  assert.deepEqual(vm, { ps: null, rt: null, tp: true, pty: null, updatedAtS: 1789300820.5 });
});

// ---- rdsIdentity: PI comes from the emitter row's own identity, not /decode ----

test("rdsIdentity is null-and-not-withheld with no row, or a row identified by something other than rds-pi", () => {
  assert.deepEqual(rdsIdentity(undefined), { value: null, withheld: false });
  assert.deepEqual(rdsIdentity({ identity_scheme: "adsb-icao", identity_value: "a1b2c3", withheld: false }), { value: null, withheld: false });
  assert.deepEqual(rdsIdentity({ identity_scheme: null, identity_value: undefined, withheld: false }), { value: null, withheld: false });
});

test("rdsIdentity reads the emitter's identity_value when the scheme is rds-pi", () => {
  assert.deepEqual(rdsIdentity({ identity_scheme: "rds-pi", identity_value: "C0DE", withheld: false }), { value: "C0DE", withheld: false });
});

test("rdsIdentity: withheld and not-yet-seen are kept distinct, never collapsed to the same null", () => {
  const withheld = rdsIdentity({ identity_scheme: "rds-pi", identity_value: undefined, withheld: true });
  assert.deepEqual(withheld, { value: null, withheld: true });
  assert.notDeepEqual(withheld, rdsIdentity(undefined)); // both have value:null, but withheld differs
});

// ---- trimRds / rdsFieldText: never render "not yet received" and "received but blank" the same way ----

test("trimRds trims padding, and a blank/absent field is null (not an empty string)", () => {
  assert.equal(trimRds("KROQ    "), "KROQ");
  assert.equal(trimRds("        "), null);
  assert.equal(trimRds(null), null);
});

test("rdsFieldText: null (never received) reads differently from a received-but-blank string", () => {
  assert.equal(rdsFieldText(null), "not yet received");
  assert.equal(rdsFieldText("KROQ    "), "KROQ");
  assert.equal(rdsFieldText("        "), "(blank)");
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
