// T-154 (ADR-0013 §4.7, §8): packet inspector pure logic — ring buffer, live frame view model, hex
// formatting, byte colouring, served address, and linked selection (field -> bytes, byte -> field)
// mapped through the same values the M1 inspector (T-090, frame-inspector.ts) already uses. No DOM
// under node:test (see ui/test/inventory.test.ts's note); only pure exports are tested here.
import { test } from "node:test";
import assert from "node:assert/strict";
import type { LayerTree } from "../src/frame-inspector";
import { cycleLeafAt, nodeById } from "../src/frame-inspector";
import {
  byteColorClass, byteOwner, findInspectorStream, frameRowsVM, frameViewFromLive,
  hexRows, pushRingFrame, resolveSelectedFrame, selectedByteRange, servedAddressText,
  type RingFrame, type StreamsResponse,
} from "../src/app/decode/inspector";

// ---- fixture: a small RDS-shaped frame record + layer tree, shaped exactly as
// docs/stream-contract.md §14.2's example (bytes 0x16 0x94 0x0A 0x00). ----

const TREE: LayerTree = {
  nodes: [
    { id: 0, name: "pi", path: "pi", type: "uint", bits: [0, 16], bytes: [0, 2], value: 0x1694, text: "0x1694" },
    { id: 1, name: "group", path: "group", type: "enum", bits: [16, 8], bytes: [2, 3], value: "0A", text: "0A" },
    { id: 2, name: "status", path: "status", type: "flag", bits: [24, 8], bytes: [3, 4], value: false, text: "CRC ✗", error: true },
  ],
  byte_index: [[0], [0], [1], [2]],
  fit: "partial",
  errors: [{ path: "status", kind: "parity" }],
};

function liveFrame(seq: number, frame: number, gated = false) {
  // `t_ns` stays a small, exactly representable nanosecond count (frame + 0.5 s): the huge
  // Unix-epoch nanosecond timestamps stream-contract §14.2 shows exceed float64 integer precision
  // regardless of this UI layer, so the fixture avoids asserting on a value never exact on the wire.
  return {
    type: "frame", seq, t_ns: frame * 1_000_000_000 + 500_000_000, content_class: "unrestricted", gated,
    crc_status: gated ? undefined : (frame % 2 === 0 ? "valid" : "invalid"), decoder: "recipe:rds@1", frame_model: "rds",
    metadata: { frame, channel: 0, channel_hz: 101_300_000, bit_len: 32, fit: gated ? undefined : "partial" },
    content: gated ? undefined : { hex: "16940a00", layers: TREE },
  };
}

// ---- ring buffer ----

test("pushRingFrame prepends newest-first, resets older fresh flags, and caps", () => {
  let ring: RingFrame[] = [];
  ring = pushRingFrame(ring, { index: 1, bitLen: 8, fit: "ok" });
  ring = pushRingFrame(ring, { index: 2, bitLen: 8, fit: "ok" });
  ring = pushRingFrame(ring, { index: 3, bitLen: 8, fit: "ok" });
  assert.deepEqual(ring.map((r) => r.view.index), [3, 2, 1]);
  assert.deepEqual(ring.map((r) => r.fresh), [true, false, false]);

  let capped: RingFrame[] = [];
  for (let i = 0; i < 5; i++) capped = pushRingFrame(capped, { index: i, bitLen: 8, fit: "ok" }, 3);
  assert.deepEqual(capped.map((r) => r.view.index), [4, 3, 2]); // 0 and 1 fell off the tail (oldest)
});

test("resolveSelectedFrame keeps an explicit selection while it's still in the ring, else the newest", () => {
  let ring: RingFrame[] = [];
  ring = pushRingFrame(ring, { index: 1, bitLen: 8, fit: "ok" });
  ring = pushRingFrame(ring, { index: 2, bitLen: 8, fit: "ok" });
  ring = pushRingFrame(ring, { index: 3, bitLen: 8, fit: "ok" });
  assert.equal(resolveSelectedFrame(ring, 2)?.index, 2);
  assert.equal(resolveSelectedFrame(ring, 99)?.index, 3); // not found -> newest
  assert.equal(resolveSelectedFrame(ring, null)?.index, 3);
  assert.equal(resolveSelectedFrame([], 1), null);
});

// ---- live frame view model ----

test("frameViewFromLive reuses frameViewFromCapture's mapping for a live inspector-stream record", () => {
  const rec = {
    type: "frame", seq: 41, t_ns: 41_500_000_000, content_class: "unrestricted", gated: false,
    crc_status: "valid", decoder: "recipe:rds@1", frame_model: "rds",
    metadata: { frame: 41, channel: 0, channel_hz: 101_300_000, bit_len: 32, fit: "partial" },
    content: { hex: "16940a00", layers: TREE },
  };
  const v = frameViewFromLive(rec, 0);
  assert.equal(v.index, 41);
  assert.equal(v.timeS, 41.5);
  assert.equal(v.crcStatus, "valid");
  assert.equal(v.fit, "partial");
  assert.equal(v.hex, "16940a00");
  assert.equal(v.layers, TREE);
  assert.equal(v.gated, false);
});

test("frameViewFromLive: a gated record carries no bytes or layers", () => {
  const v = frameViewFromLive(liveFrame(9, 9, true), 0);
  assert.equal(v.gated, true);
  assert.equal(v.hex, undefined);
  assert.equal(v.layers, undefined);
});

test("frame list view model: time/channel/summary text, bad and fresh flags, and selection", () => {
  let ring: RingFrame[] = [];
  ring = pushRingFrame(ring, frameViewFromLive(liveFrame(1, 0), 0)); // crc valid (even)
  ring = pushRingFrame(ring, frameViewFromLive(liveFrame(2, 1), 0)); // crc invalid (odd) -> bad
  ring = pushRingFrame(ring, frameViewFromLive(liveFrame(3, 2, true), 0)); // gated
  const rows = frameRowsVM(ring, 1); // select the middle (bad) frame explicitly
  assert.deepEqual(rows.map((r) => r.seq), [2, 1, 0]);
  assert.equal(rows[0].fresh, true); // the newest push
  assert.equal(rows[1].fresh, false);
  assert.equal(rows.find((r) => r.seq === 1)!.bad, true); // frame 1: crc invalid
  assert.equal(rows.find((r) => r.seq === 1)!.summaryText, "invalid");
  assert.equal(rows.find((r) => r.seq === 0)!.bad, false); // frame 0: crc valid
  assert.equal(rows.find((r) => r.seq === 2)!.summaryText, "withheld");
  assert.equal(rows.find((r) => r.seq === 2)!.gated, true);
  assert.equal(rows.find((r) => r.seq === 1)!.selected, true);
  assert.equal(rows.find((r) => r.seq === 0)!.selected, false);
  assert.equal(rows.find((r) => r.seq === 1)!.channelText, "0");
});

// ---- served address (docs/api.md "GET /api/streams") ----

const STREAMS: StreamsResponse = {
  streams: [
    { stream_id: "spectrum/live", tcp_target: "spectrum/live" },
    { stream_id: "inspector/p1/frames", tcp_target: "inspector/p1/frames" },
  ],
  tcp: { addr: "127.0.0.1:8788" },
};

test("findInspectorStream picks the pipeline's inspector output, ignoring other streams", () => {
  assert.deepEqual(findInspectorStream(STREAMS, "p1"), { stream_id: "inspector/p1/frames", tcp_target: "inspector/p1/frames" });
  assert.equal(findInspectorStream(STREAMS, "p2"), null);
});

test("servedAddressText never includes the token and degrades when there's no TCP bridge or no stream yet", () => {
  const text = servedAddressText(STREAMS, "p1");
  assert.equal(text, "tcp://127.0.0.1:8788 inspector/p1/frames");
  assert.doesNotMatch(text, /token/i);
  assert.equal(servedAddressText(STREAMS, "p2"), "not streaming yet");
  assert.equal(servedAddressText({ streams: STREAMS.streams, tcp: null }, "p1"), "inspector/p1/frames (no TCP bridge on this server)");
});

// ---- byte colouring and hex/ASCII formatting ----

test("byteOwner and byteColorClass read a byte's owning leaf, colouring errors coral over type", () => {
  assert.equal(byteOwner(TREE, 0)?.name, "pi");
  assert.equal(byteColorClass(TREE, 0), "b-teal"); // uint
  assert.equal(byteColorClass(TREE, 2), "b-amber"); // enum
  assert.equal(byteColorClass(TREE, 3), "b-coral"); // flag, but error: true wins over b-lav
  assert.equal(byteOwner(TREE, 99), null);
  assert.equal(byteColorClass(TREE, 99), "b-mut");
});

test("hexRows chunks bytes, formats hex/ASCII, colours by owner, and marks a selection range", () => {
  const rows = hexRows([0x16, 0x94, 0x0a, 0x00], TREE, [2, 3]);
  assert.equal(rows.length, 1);
  assert.equal(rows[0].offset, "0000");
  assert.deepEqual(rows[0].cells.map((c) => c.hex), ["16", "94", "0A", "00"]);
  assert.deepEqual(rows[0].cells.map((c) => c.ascii), ["·", "·", "·", "·"]); // none of these bytes are printable
  assert.deepEqual(rows[0].cells.map((c) => c.colorClass), ["b-teal", "b-teal", "b-amber", "b-coral"]);
  assert.deepEqual(rows[0].cells.map((c) => c.selected), [false, false, true, false]);
});

test("hexRows chunks at 16 bytes per row by default and pages long frames", () => {
  const bytes = Array.from({ length: 20 }, (_, i) => i);
  const rows = hexRows(bytes, undefined, null);
  assert.equal(rows.length, 2);
  assert.equal(rows[0].cells.length, 16);
  assert.equal(rows[1].cells.length, 4);
  assert.equal(rows[1].offset, "0010");
  assert.ok(rows[0].cells.every((c) => c.colorClass === "b-mut")); // no tree -> no colouring
});

// ---- linked selection, both directions (stream-contract §14.2) ----

test("field click -> byte highlight: selectedByteRange is exactly the field's own bytes", () => {
  assert.deepEqual(selectedByteRange(TREE, 1), [2, 3]);
  assert.equal(selectedByteRange(TREE, null), null);
  assert.equal(selectedByteRange(undefined, 1), null);
  assert.equal(selectedByteRange(TREE, 999), null);
});

test("byte click -> field select lands on the same field a field click would highlight, both ways", () => {
  // Clicking byte 2 selects byte_index[2][0] (field 1, "group"); that field's own bytes cover byte 2.
  const fieldId = cycleLeafAt(TREE, 2, null);
  assert.equal(fieldId, 1);
  assert.deepEqual(selectedByteRange(TREE, fieldId), [2, 3]);
  assert.equal(nodeById(TREE, fieldId!)?.name, "group");
  // Repeat clicks on the same byte cycle through byte_index[b]; with one owner it comes back to itself.
  assert.equal(cycleLeafAt(TREE, 2, fieldId), 1);
});
