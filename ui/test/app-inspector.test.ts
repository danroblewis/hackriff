// T-154 (ADR-0013 §4.7, §8): packet inspector pure logic — ring buffer, live frame view model, hex
// formatting, byte colouring, served address, and linked selection (field -> bytes, byte -> field)
// mapped through the same values the M1 inspector (T-090, frame-inspector.ts) already uses. No DOM
// under node:test (see ui/test/inventory.test.ts's note); only pure exports are tested here.
import { test } from "node:test";
import assert from "node:assert/strict";
import type { FrameView, LayerTree } from "../src/frame-inspector";
import { cycleLeafAt, nodeById } from "../src/frame-inspector";
import {
  byteColorClass, byteOwner, findInspectorStream, frameListEmptyText, frameRowsVM, frameViewFromLive,
  hexRows, inspectorNoteText, loadFrameBackfill, pushRingFrame, resolveFrameList, resolveSelectedFrame,
  selectedByteRange, servedAddressText, unplaceableFrames, windowFrames,
  type RingFrame, type StreamsResponse,
} from "../src/app/decode/inspector";
import { FALLBACK_ROWS, emptyListText, viewWindow, windowKey } from "../src/app/explore/inventory";
import { decodeEmptyText } from "../src/app/explore/output-panel";


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

// ---- T-387: the packet inspector is a view over the one window ------------------------------
//
// The prior question T-387 had to settle first: *should this surface re-derive at all?* Of the four
// T-384 left on the live edge, three describe the run (pipelines list, stage status, outputs dock)
// and are honestly live-only. This one is not: packets are **data about the air**, they carry
// capture-clock `t_ns`, and their past-window form already exists in the pipeline's capture. So it
// re-derives — through `GET /api/captures/{id}/frames?from_t&to_t`, a route that already had the
// window. No stream-contract change was needed, and none was made.

/** The capture clock these fixtures run on, deliberately far from any wall clock — the 3.5-day gap
 * behind T-379 is what makes a `Date.now()` window select nothing while the capture window holds
 * five frames. */
const CAP_EDGE_S = 1_789_297_847;
const SPAN_S = FALLBACK_ROWS / 25;

function winState(over: {
  view?: { loHz: number; hiHz: number } | null; rowRateHz?: number | null; edgeTS?: number | null;
  time?: { live: boolean; tS?: number; spanS?: number | null };
} = {}) {
  return {
    live: {
      view: over.view === undefined ? { loHz: 99.6e6, hiHz: 102e6 } : over.view,
      rowRateHz: over.rowRateHz ?? 25,
      edgeTS: over.edgeTS === undefined ? CAP_EDGE_S : over.edgeTS,
    },
    device: { rowsPerS: null },
    time: over.time ?? { live: true },
    captureWindow: null,
  };
}

/** A frame at `tS` on the capture clock, with the pipeline's own frame number as its id. */
const frameAt = (index: number, tS: number): FrameView => ({ index, timeS: tS, bitLen: 32, fit: "ok" });
const ringOf = (...views: FrameView[]): RingFrame[] => views.map((view) => ({ view, fresh: false }));

/** A client that records every path, serves one capture for `p1` and the frames a test names. */
function framesClient(
  stored: readonly { index: number; tS: number }[],
  opts: { coverage?: "observed" | "unobserved" | "none"; captures?: boolean } = {},
) {
  const paths: string[] = [];
  const client = {
    get: async <T,>(path: string): Promise<T> => {
      paths.push(path);
      if (path.startsWith("/api/coverage")) {
        const c = opts.coverage ?? "observed";
        return (c === "none" ? { any: { cells: [] } } : { any: { cells: [{ state: c }] } }) as T;
      }
      if (path === "/api/captures") {
        return (opts.captures === false
          ? { captures: [] }
          : { captures: [{ id: "cap1", pipeline_id: "p1", t_last: CAP_EDGE_S }] }) as T;
      }
      return {
        frames: stored.map((s) => ({
          type: "frame", gated: false, t_ns: s.tS * 1e9, metadata: { frame: s.index, bit_len: 32, fit: "ok" },
          content: { hex: "16940a00" },
        })),
      } as T;
    },
  };
  return { client, paths };
}

test("THE CONTROL: a window that DOES hold frames renders them, by id, from both sources", async () => {
  // Two frames the live socket carried, three more only the capture holds — every one inside the
  // window. Without this control every other assertion here is satisfiable by a panel that renders
  // nothing and explains itself beautifully.
  const ring = ringOf(frameAt(41, CAP_EDGE_S - 2), frameAt(40, CAP_EDGE_S - 3));
  const { client, paths } = framesClient([
    { index: 37, tS: CAP_EDGE_S - 9 }, { index: 38, tS: CAP_EDGE_S - 7 }, { index: 39, tS: CAP_EDGE_S - 5 },
  ]);
  const w = { t0: CAP_EDGE_S - SPAN_S, t1: CAP_EDGE_S };
  const backfill = await loadFrameBackfill(client, "p1", w);
  const view = await resolveFrameList(client, winState(), ring, backfill);

  assert.equal(view.kind, "frames");
  assert.deepEqual(
    (view as { kind: "frames"; frames: RingFrame[] }).frames.map((r) => r.view.index),
    [41, 40, 39, 38, 37],
    "every frame of the window, newest first, live and recorded together — by id",
  );
  assert.deepEqual(paths, ["/api/captures", `/api/captures/cap1/frames?from_t=${w.t0}&to_t=${w.t1}&limit=500`]);
  assert.ok(!paths.some((p) => p.startsWith("/api/coverage")), "a non-empty window asks no coverage question");

  // And the rows a list would render carry those same ids: the control reaches the view model too.
  assert.deepEqual(
    frameRowsVM((view as { kind: "frames"; frames: RingFrame[] }).frames, null).map((r) => r.seq),
    [41, 40, 39, 38, 37],
  );
});

test("THE FOURTH TIME THIS BUG WOULD HAVE BEEN FOUND: the window is the capture clock, not Date.now()", () => {
  // T-379 (Candidate list, 306,315 s out), T-384 (three sites in plots.ts), T-389 (the Confirmed
  // query) — the same bug three times. These frames sit on a capture clock 3.5 days from any
  // browser instant, so a wall-clock window selects none of them while the real window selects all.
  const ring = ringOf(frameAt(2, CAP_EDGE_S - 1), frameAt(1, CAP_EDGE_S - 4));
  const capture = { t0: CAP_EDGE_S - SPAN_S, t1: CAP_EDGE_S };
  const wall = { t0: Date.now() / 1000 - SPAN_S, t1: Date.now() / 1000 };
  assert.deepEqual(windowFrames(ring, [], capture).map((r) => r.view.index), [2, 1]);
  assert.deepEqual(windowFrames(ring, [], wall).map((r) => r.view.index), []);
});

test("NEVER WIDEN: frames outside the window are not listed, and an empty window is not refilled", () => {
  const ring = ringOf(frameAt(9, CAP_EDGE_S - 1), frameAt(8, CAP_EDGE_S - SPAN_S - 30));
  const w = { t0: CAP_EDGE_S - SPAN_S, t1: CAP_EDGE_S };
  assert.deepEqual(windowFrames(ring, [], w).map((r) => r.view.index), [9], "the older frame is outside");
  // A window entirely before every frame lists nothing rather than relaxing to the nearest frames.
  const past = { t0: CAP_EDGE_S - 4000, t1: CAP_EDGE_S - 3000 };
  assert.deepEqual(windowFrames(ring, [], past), []);
});

test("a window straddling the live edge lists each frame once: the recorded copy is dropped, the live one kept", () => {
  const w = { t0: CAP_EDGE_S - SPAN_S, t1: CAP_EDGE_S };
  const live = [{ view: frameAt(12, CAP_EDGE_S - 2), fresh: true }];
  const stored = [frameAt(12, CAP_EDGE_S - 2), frameAt(11, CAP_EDGE_S - 6)];
  const out = windowFrames(live, stored, w);
  assert.deepEqual(out.map((r) => r.view.index), [12, 11]);
  assert.equal(out[0].fresh, true, "the live record wins, so the just-arrived flash survives the merge");
});

test("a frame carrying no t_ns is never claimed for the window — it is counted and disclosed", () => {
  const ring: RingFrame[] = [
    { view: { index: 5, bitLen: 32, fit: "ok" }, fresh: false },
    { view: frameAt(4, CAP_EDGE_S - 1), fresh: false },
  ];
  const w = { t0: CAP_EDGE_S - SPAN_S, t1: CAP_EDGE_S };
  assert.deepEqual(windowFrames(ring, [], w).map((r) => r.view.index), [4]);
  assert.equal(unplaceableFrames(ring), 1);
  assert.match(
    inspectorNoteText({ kind: "frames", frames: windowFrames(ring, [], w) }, 1, "live", "tcp://x y"),
    /carry no time and cannot be placed/,
  );
});

test("NO WINDOW: nothing is asked at all, and the panel says so rather than showing an unbounded ring", async () => {
  const ring = ringOf(frameAt(3, CAP_EDGE_S - 1));
  const { client, paths } = framesClient([]);
  const view = await resolveFrameList(client, winState({ edgeTS: null }), ring, null);
  assert.deepEqual(view, { kind: "no-window" });
  assert.deepEqual(paths, [], "an invented window returns an honest zero frames, which reads as a finding");
  assert.equal(frameListEmptyText(view), "Waiting for the capture window…");
});

test("THE GENUINELY-EMPTY CONTROL: an empty window asks about its coverage, for exactly that window", async () => {
  const { client, paths } = framesClient([], { coverage: "unobserved" });
  const view = await resolveFrameList(client, winState(), [], null);
  assert.deepEqual(view, { kind: "empty", coverage: "unobserved" });
  const cov = new URLSearchParams(paths[0].slice(paths[0].indexOf("?") + 1));
  assert.equal(Number(cov.get("t1")), CAP_EDGE_S, "about exactly this panel's window, not another");
  assert.equal(Number(cov.get("t0")), CAP_EDGE_S - SPAN_S);
  assert.equal(cov.get("f_lo"), String(99.6e6));
});

test("a coverage answer that never came stays UNKNOWN rather than hardening into a measurement claim", async () => {
  const { client } = framesClient([], { coverage: "none" });
  assert.deepEqual(await resolveFrameList(client, winState(), [], null), { kind: "empty", coverage: null });
});

test("a pipeline with no capture leaves the live frames alone rather than emptying the window", async () => {
  const { client } = framesClient([], { captures: false });
  const w = { t0: CAP_EDGE_S - SPAN_S, t1: CAP_EDGE_S };
  const backfill = await loadFrameBackfill(client, "p1", w);
  assert.deepEqual(backfill, { w, frames: [] });
  const ring = ringOf(frameAt(6, CAP_EDGE_S - 1));
  const view = await resolveFrameList(client, winState(), ring, backfill);
  assert.deepEqual((view as { kind: "frames"; frames: RingFrame[] }).frames.map((r) => r.view.index), [6]);
});

test("a backfill fetched for another window is never tallied under this one", async () => {
  const { client } = framesClient([]);
  const stale = { w: { t0: CAP_EDGE_S - 900, t1: CAP_EDGE_S - 800 }, frames: [frameAt(1, CAP_EDGE_S - 850)] };
  const view = await resolveFrameList(client, winState(), [], stale);
  assert.equal(view.kind, "empty", "the stale set belongs to a different window and is not borrowed");
});

test("scrubbing back re-derives: the same ring answers about whichever window is on screen", () => {
  const ring = ringOf(
    frameAt(3, CAP_EDGE_S - 1), frameAt(2, CAP_EDGE_S - 100), frameAt(1, CAP_EDGE_S - 101),
  );
  const live = viewWindow(winState())!;
  const past = viewWindow(winState({ time: { live: false, tS: CAP_EDGE_S - 100, spanS: 5 } }))!;
  assert.deepEqual(windowFrames(ring, [], live).map((r) => r.view.index), [3]);
  assert.deepEqual(windowFrames(ring, [], past).map((r) => r.view.index), [2, 1]);
  // And the window key changes between them, which is what makes the panel re-ask at all.
  assert.notEqual(windowKey(winState()), windowKey(winState({ time: { live: false, tS: CAP_EDGE_S - 100, spanS: 5 } })));
});

test("THE DISTINGUISHING TEST: the four emptinesses produce four different sentences", () => {
  const sentences = [
    frameListEmptyText({ kind: "no-window" }),
    frameListEmptyText({ kind: "empty", coverage: "unobserved" }),
    frameListEmptyText({ kind: "empty", coverage: "observed" }),
    frameListEmptyText({ kind: "empty", coverage: null }),
  ];
  assert.equal(new Set(sentences).size, 4, `pairwise distinct: ${JSON.stringify(sentences)}`);
  assert.match(sentences[2], /No frames in this window\./, "only the third is a claim about the air");
});

test("ONE VOCABULARY: the two measurement claims are worded identically on every window-scoped surface", () => {
  // `Coverage` is the backend's (T-368) and the sentences that report it must not fork per panel:
  // "nothing ever looked here" said three slightly different ways is three vocabularies, and a
  // reader who learned one on the sidebar would read a different meaning into the inspector's.
  for (const v of [{ kind: "no-window" } as const, { kind: "empty", coverage: "unobserved" } as const]) {
    assert.equal(
      frameListEmptyText(v), decodeEmptyText(v),
      "the packet inspector and the decode panel make the same claim in the same words",
    );
    assert.equal(
      frameListEmptyText(v),
      emptyListText(v.kind === "no-window"
        ? { window: null, loadedAtS: 1, error: null }
        : { window: { coverage: "unobserved" }, loadedAtS: 1, error: null }),
      "…and so does the Candidate/Confirmed list",
    );
  }
  // The third state is the one that differs, because it is the only one about this surface's subject.
  assert.notEqual(
    frameListEmptyText({ kind: "empty", coverage: "observed" }),
    emptyListText({ window: { coverage: "observed" }, loadedAtS: 1, error: null }),
  );
});

test("the note keeps three different facts apart: the window, the tap and the served address", () => {
  const w = { t0: CAP_EDGE_S - SPAN_S, t1: CAP_EDGE_S };
  const full = inspectorNoteText({ kind: "frames", frames: windowFrames(ringOf(frameAt(1, CAP_EDGE_S - 1)), [], w) }, 0, "live", "tcp://a b");
  assert.equal(full, "1 frame in this window · live tap · tcp://a b");
  // A connected tap says nothing about whether the window on screen holds frames: an empty window
  // under a live tap must still read as empty, or the tap's state would be mistaken for an answer.
  const empty = inspectorNoteText({ kind: "empty", coverage: "unobserved" }, 0, "live", "tcp://a b");
  assert.match(empty, /^Nothing was observed in this window/);
  assert.match(empty, /live tap/);
});
