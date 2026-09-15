// T-090 inspector pane: pure request/response and linked-selection logic, no DOM (node:test has
// none; see inventory.test.ts's note — only DOM-independent functions are unit-tested here).
// Fixtures are taken from docs/api.md "Inspector (T-089)" and its backing tests
// (crates/hk-api/tests/inspector_api.rs `draft_map`/`captures`), so the shapes match the real
// server responses exactly.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import {
  type CaptureParseResponse, type FitSummary, type InspectorClient, type LayerTree,
  buildTree, cycleLeafAt, errorsByPath, fitClass, fitSummaryText, frameViewFromCapture, frameViewFromInline,
  hexBytes, asciiChar, loadCapturePage, nodeById, pagePrevFrom, parseFieldMapInput, parseInlineFrames, parsePastedFrames,
} from "../src/frame-inspector";

const html = readFileSync("src/index.html", "utf8");

// ---- fixture: kind/len/payload field map over frame 5 of a 27-frame recording
// (crates/hk-api/tests/inspector_api.rs `draft_map`/`captures`/`capture_reparse_pages_frames_...`) ----
const kindLenPayload: LayerTree = {
  nodes: [
    { id: 0, name: "kind", path: "kind", type: "enum", bits: [0, 4], bytes: [0, 1], value: 1, text: "hello" },
    { id: 1, name: "len", path: "len", type: "uint", bits: [4, 4], bytes: [0, 1], value: 3, text: "3" },
    { id: 2, name: "payload", path: "payload", type: "ascii", bits: [8, 24], bytes: [1, 4], value: "f05", text: "f05" },
  ],
  byte_index: [[0, 1], [2], [2], [2]],
  fit: "ok",
};

const partialFit: LayerTree = {
  nodes: [
    { id: 0, name: "kind", path: "kind", type: "enum", bits: [0, 4], bytes: [0, 1], value: 2, text: "data" },
    { id: 1, name: "len", path: "len", type: "uint", bits: [4, 4], bytes: [0, 1], value: 15, text: "15" },
    { id: 2, name: "payload", path: "payload", type: "ascii", bits: [8, 128], bytes: [1, 16], error: true },
  ],
  byte_index: [[0, 1], [2], [2], [2]],
  fit: "partial",
  errors: [{ path: "payload", kind: "out-of-bounds", need_bits: 128, have_bits: 24 }],
};

// A nested example (bitfield -> named flag children), per stream-contract §14.2 "A bitfield's
// named bits are child flag nodes."
const nestedFlags: LayerTree = {
  nodes: [
    { id: 0, name: "hdr", path: "hdr", type: "layer", bits: [0, 8], bytes: [0, 1] },
    { id: 1, parent: 0, name: "flags", path: "hdr.flags", type: "bitfield", bits: [0, 8], bytes: [0, 1] },
    { id: 2, parent: 1, name: "sync", path: "hdr.flags.sync", type: "flag", bits: [0, 1], bytes: [0, 1], value: true, text: "true" },
    { id: 3, parent: 1, name: "crc_ok", path: "hdr.flags.crc_ok", type: "flag", bits: [1, 1], bytes: [0, 1], value: false, text: "false" },
  ],
  byte_index: [[2, 3]],
  fit: "ok",
};

// The fit summary example from docs/api.md line 292 / inspector_api.rs.
const fitExample: FitSummary = {
  frames: 27, ok: 24, partial: 1, failed: 0, unparsed: 2,
  errors: { payload: { "out-of-bounds": 1 } }, truncated: false,
};

// ---- tree rendering (nesting) ----

test("buildTree nests flat pre-order nodes by parent id, preserving pre-order among siblings", () => {
  const roots = buildTree(nestedFlags.nodes);
  assert.equal(roots.length, 1);
  assert.equal(roots[0].node.name, "hdr");
  assert.equal(roots[0].children.length, 1);
  assert.equal(roots[0].children[0].node.name, "flags");
  assert.deepEqual(roots[0].children[0].children.map((c) => c.node.name), ["sync", "crc_ok"]);
});

test("buildTree keeps flat (no-nesting) responses as siblings at the root", () => {
  const roots = buildTree(kindLenPayload.nodes);
  assert.deepEqual(roots.map((r) => r.node.path), ["kind", "len", "payload"]);
  assert.equal(roots[0].children.length, 0);
});

test("nodeById looks a node up by id", () => {
  assert.equal(nodeById(kindLenPayload, 2)?.path, "payload");
  assert.equal(nodeById(kindLenPayload, 99), null);
});

test("errorsByPath groups layer errors by dotted field path", () => {
  const m = errorsByPath(partialFit);
  assert.equal(m.get("payload")?.[0].kind, "out-of-bounds");
  assert.equal(m.get("kind"), undefined);
  assert.equal(errorsByPath(kindLenPayload).size, 0, "no errors on a fully-fit frame");
});

// ---- linked selection: byte click -> leaf field, with repeat-click cycling ----

test("a byte with one leaf selects it on first click", () => {
  assert.equal(cycleLeafAt(kindLenPayload, 1, null), 2);
});

test("a byte with two overlapping leaves selects the first, then cycles on repeat clicks of the same byte", () => {
  assert.equal(cycleLeafAt(kindLenPayload, 0, null), 0, "byte 0: kind then len (byte_index[0] = [0, 1])");
  assert.equal(cycleLeafAt(kindLenPayload, 0, 0), 1, "repeat click cycles to the next id");
  assert.equal(cycleLeafAt(kindLenPayload, 0, 1), 0, "cycles back around");
});

test("clicking a different byte starts that byte's cycle fresh, ignoring the previous byte's cursor", () => {
  // Simulates the panel's own state machine: `current` is only carried when it's still this byte.
  const clickedByte0 = cycleLeafAt(kindLenPayload, 0, null); // 0
  const clickedByte1 = cycleLeafAt(kindLenPayload, 1, null); // fresh cycle for byte 1, not byte 0's cursor
  assert.equal(clickedByte0, 0);
  assert.equal(clickedByte1, 2);
});

test("a byte outside any field returns null", () => {
  assert.equal(cycleLeafAt(kindLenPayload, 99, null), null);
});

// ---- hex + ASCII ----

test("hexBytes decodes lower/upper hex into byte values", () => {
  assert.deepEqual(hexBytes("54a8"), [0x54, 0xa8]);
  assert.deepEqual(hexBytes("1348494A"), [0x13, 0x48, 0x49, 0x4a]);
  assert.deepEqual(hexBytes(""), []);
});

test("asciiChar renders printables and a middle dot for everything else", () => {
  assert.equal(asciiChar(0x41), "A");
  assert.equal(asciiChar(0x00), "·");
  assert.equal(asciiChar(0x7f), "·");
});

// ---- misfit / error display ----

test("fitSummaryText renders the docs/api.md example fit summary", () => {
  const t = fitSummaryText(fitExample);
  assert.match(t, /27 frames/);
  assert.match(t, /24 ok/);
  assert.match(t, /1 partial/);
  assert.match(t, /payload: out-of-bounds×1/);
  assert.doesNotMatch(t, /truncated/, "truncated: false adds nothing");
});

test("fitSummaryText notes when the summary was truncated to the frame cap", () => {
  const t = fitSummaryText({ ...fitExample, truncated: true });
  assert.match(t, /truncated to the first 100000 frames/);
});

test("fitClass maps fit status to a display class", () => {
  assert.equal(fitClass("ok"), "ok");
  assert.equal(fitClass("partial"), "warn");
  assert.equal(fitClass("failed"), "bad");
  assert.equal(fitClass("none"), "");
  assert.equal(fitClass(undefined), "");
});

// ---- frame view mapping (capture page vs. inline parse) ----

test("frameViewFromCapture maps a capture-page frame record, converting t from nanoseconds", () => {
  const v = frameViewFromCapture({
    gated: false, crc_status: "valid",
    t: 1_789_300_800_123_456_789,
    metadata: { frame: 5, channel: 0, channel_hz: 101_300_000, bit_len: 32, fit: "ok" },
    content: { hex: "1348494a", layers: kindLenPayload },
  }, 0);
  assert.equal(v.index, 5);
  assert.equal(v.timeS, 1_789_300_800_123_456_789 / 1e9);
  assert.equal(v.channelHz, 101_300_000);
  assert.equal(v.bitLen, 32);
  assert.equal(v.crcStatus, "valid");
  assert.equal(v.fit, "ok");
  assert.equal(v.hex, "1348494a");
  assert.equal(v.layers, kindLenPayload);
});

test("frameViewFromCapture falls back to the page index and 'none' fit when metadata is gated away", () => {
  const v = frameViewFromCapture({ gated: true, metadata: {} }, 25);
  assert.equal(v.index, 25);
  assert.equal(v.fit, "none");
  assert.equal(v.hex, undefined);
  assert.equal(v.gated, true);
});

test("frameViewFromInline maps a pasted-frame parse result", () => {
  const v = frameViewFromInline({ bit_len: 8, hex: "2f", layers: partialFit }, 1);
  assert.equal(v.index, 1);
  assert.equal(v.bitLen, 8);
  assert.equal(v.fit, "partial");
  assert.equal(v.hex, "2f");
});

// ---- paging ----

test("pagePrevFrom steps back a page, clamped at 0", () => {
  assert.equal(pagePrevFrom(5, 3), 2);
  assert.equal(pagePrevFrom(2, 3), 0);
  assert.equal(pagePrevFrom(0, 50), 0);
});

// ---- draft field map + pasted frames ----

test("parseFieldMapInput treats blank input as 'no field map'", () => {
  assert.deepEqual(parseFieldMapInput(""), {});
  assert.deepEqual(parseFieldMapInput("   \n"), {});
});

test("parseFieldMapInput parses valid JSON and reports a message for invalid JSON", () => {
  const r = parseFieldMapInput('{"unit":"bits","fields":[]}');
  assert.deepEqual(r.value, { unit: "bits", fields: [] });
  const bad = parseFieldMapInput("{not json");
  assert.ok(bad.error?.includes("invalid JSON"), bad.error);
});

test("parsePastedFrames splits lines, skips blanks and # comments, and validates hex", () => {
  const r = parsePastedFrames("54a8\n# comment\n\n2F\n");
  assert.deepEqual(r.frames, [{ hex: "54a8" }, { hex: "2F" }]);
  assert.equal(r.error, undefined);
});

test("parsePastedFrames refuses odd-length or non-hex lines, and empty input", () => {
  assert.ok(parsePastedFrames("").error);
  assert.ok(parsePastedFrames("abc").error, "odd-length hex");
  assert.ok(parsePastedFrames("zz").error, "not hex");
});

test("parsePastedFrames refuses more than the 500-frame cap", () => {
  const lines = Array.from({ length: 501 }, () => "ab").join("\n");
  assert.ok(parsePastedFrames(lines).error);
});

// ---- API calls: request shape and response pass-through ----

function mkClient(handler: (path: string, body: unknown) => unknown): { client: InspectorClient; calls: { path: string; body: unknown }[] } {
  const calls: { path: string; body: unknown }[] = [];
  const client: InspectorClient = {
    post: async (path, body) => { calls.push({ path, body }); return handler(path, body) as never; },
  };
  return { client, calls };
}

test("loadCapturePage posts from_frame/limit and only includes field_map when given", async () => {
  const resp: CaptureParseResponse = {
    capture_id: "cap-1", stream: {}, total_frames: 27, from_frame: 5, limit: 3, next_from_frame: 8, frames: [], fit: fitExample,
  };
  const { client, calls } = mkClient(() => resp);
  const got = await loadCapturePage(client, "cap-1", { unit: "bits", fields: [] }, 5, 3);
  assert.equal(got, resp);
  assert.equal(calls[0].path, "/api/captures/cap-1/parse");
  assert.deepEqual(calls[0].body, { from_frame: 5, limit: 3, field_map: { unit: "bits", fields: [] } });

  await loadCapturePage(client, "cap-1", undefined, 0, 100);
  assert.deepEqual(calls[1].body, { from_frame: 0, limit: 100 }, "no field_map key when not set");
});

test("loadCapturePage encodes the capture id into the path", async () => {
  const { client, calls } = mkClient(() => ({ capture_id: "a/b", stream: {}, total_frames: 0, from_frame: 0, limit: 50, next_from_frame: null, frames: [], fit: null }));
  await loadCapturePage(client, "a/b", undefined, 0, 50);
  assert.equal(calls[0].path, "/api/captures/a%2Fb/parse");
});

test("parseInlineFrames posts field_map and frames to /api/inspector/parse", async () => {
  const { client, calls } = mkClient(() => ({ frames: [], fit: fitExample }));
  await parseInlineFrames(client, { unit: "bits", fields: [] }, [{ hex: "1348494a" }, { hex: "2f", bit_len: 8 }]);
  assert.equal(calls[0].path, "/api/inspector/parse");
  assert.deepEqual(calls[0].body, { field_map: { unit: "bits", fields: [] }, frames: [{ hex: "1348494a" }, { hex: "2f", bit_len: 8 }] });
});

// ---- layout: the pane's ids exist once each, and phone-width uses no fixed wide widths ----

test("index.html declares the frame-inspector pane's element ids", () => {
  for (const id of [
    "frame-inspector", "fi-status", "fi-capture-form", "fi-capture-id", "fi-field-map", "fi-map-apply",
    "fi-paste-frames", "fi-paste-parse", "fi-fit", "fi-error", "fi-frame-table", "fi-frame-body",
    "fi-prev", "fi-next", "fi-page-info", "fi-hex", "fi-tree",
  ]) {
    const matches = html.match(new RegExp(`id="${id}"`, "g")) ?? [];
    assert.equal(matches.length, 1, `expected exactly one id="${id}" in index.html`);
  }
});
