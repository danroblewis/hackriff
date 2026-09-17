// T-388 (the user's bug from live testing, 2026-09-16): a live signal's box extended UP slowly.
//
// The measured chain was two lazy links in series — the backend committed an open track's presence
// every 5 s (`LIVE_OFFER_NS`) and the UI polled `/api/inventory` every 5 s — so a box top sat
// between 5 s and 10 s behind the live edge. The fix is a push, and these tests are about the two
// halves of "fast, without inventing anything":
//
//   a. **latency**: an extension applied to a row moves its box top to the pushed instant, with no
//      poll involved — asserted against the ~1 s target with numbers;
//   b. **the control that matters**: a signal that stops stops extending. A push that keeps a box
//      growing after an emission ended is exactly the fabrication the honesty constraint forbids,
//      and a latency test alone does not catch it.
//
// Plus the refusals: no row, no interval, a reordered record, and a record that would bridge a
// silence. Every one of them leaves the row exactly as the poll served it.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { extendPresence, parsePresenceExtension, PRESENCE_EXTENSION_KIND } from "../src/presence";
import type { Presence, Row } from "../src/inventory";

/** One `presence` record as `hk-pipeline::presence` publishes it (docs/stream-contract.md §15). */
const record = (emitter: string, t0: number, t1: number, open = true) => JSON.stringify({
  type: "message", seq: 3, t_ns: Math.round(t1 * 1e9), emitter_id: emitter,
  content_class: "unrestricted", gated: false, frame_model: PRESENCE_EXTENSION_KIND,
  metadata: { kind: PRESENCE_EXTENSION_KIND, last_interval: { t_start_s: t0, t_end_s: t1, open } },
});

const presence = (t0: number, t1: number, open = true): Presence => ({
  intervals: 1, on_air_s: t1 - t0, last_interval: { t_start_s: t0, t_end_s: t1, open },
  liveness: open ? "live" : "ended", ended_t_s: open ? null : t1,
});

const row = (p: Presence | undefined): Pick<Row, "presence"> => ({ presence: p });

// ---------------------------------------------------------------------------
// (a) latency: the box top reaches the pushed instant without a poll
// ---------------------------------------------------------------------------

test("T-388: an extension moves the box top to the observed live edge, within ~1 s and with no poll", () => {
  // The band's live edge is t = 1000.0; the last poll was a while ago and left the box at 995.0.
  const LIVE_EDGE_S = 1000.0, POLLED_END_S = 995.0;
  const r = row(presence(990.0, POLLED_END_S));
  assert.equal(
    LIVE_EDGE_S - r.presence!.last_interval!.t_end_s, 5.0,
    "before the push, the box top lags by a whole inventory poll",
  );

  // The backend publishes how far presence has now been OBSERVED. Detection runs per frame and the
  // detect reader flushes every 0.5 s, so the newest observed end trails the live edge by at most
  // one flush — this stands in for that: 1000.0 minus a 0.4 s-old measurement.
  const OBSERVED_END_S = 999.6;
  const ext = parsePresenceExtension(record("e1", 992.0, OBSERVED_END_S))!;
  const after = extendPresence(r, ext)!;

  assert.equal(after.last_interval!.t_end_s, OBSERVED_END_S);
  const lag = LIVE_EDGE_S - after.last_interval!.t_end_s;
  assert.ok(lag <= 1.0, `box top must track the live edge within ~1 s, lags ${lag} s`);
  assert.ok(lag < 5.0 / 4, `and by a large margin over the 5 s poll it replaces (${lag} s)`);
  // The older edge is untouched: an extension is news about the newest edge only. The row's
  // interval may have begun before the track the extension came from.
  assert.equal(after.last_interval!.t_start_s, 990.0, "t_start_s is the row's, not the record's");
  assert.equal(after.last_interval!.open, true);
});

test("T-388: a run of extensions grows the box monotonically, each to its own observed end", () => {
  let p: Presence | null = presence(990.0, 995.0);
  const ends = [996.5, 997.0, 998.25, 999.5];
  for (const t1 of ends) {
    p = extendPresence(row(p!), parsePresenceExtension(record("e1", 992.0, t1))!);
    assert.ok(p, `extension to ${t1} must apply`);
    assert.equal(p!.last_interval!.t_end_s, t1);
  }
  assert.equal(p!.last_interval!.t_start_s, 990.0, "the box grew downward in time only");
});

// ---------------------------------------------------------------------------
// (b) the control that matters: a signal that STOPS stops extending
// ---------------------------------------------------------------------------

test("T-388: a stopped emission stops extending — the box top stays at the last OBSERVED end", () => {
  // The emitter goes off the air at 999.5. The tracker stops advancing its end, so every later
  // record repeats that same instant (hk-pipeline's `LiveExtent::t_end_ns` is `t_last_end`, never a
  // clock read). The box must not move again, however long the stream stays open.
  const STOPPED_AT_S = 999.5;
  let p: Presence = extendPresence(row(presence(990.0, 995.0)),
    parsePresenceExtension(record("e1", 992.0, STOPPED_AT_S))!)!;
  assert.equal(p.last_interval!.t_end_s, STOPPED_AT_S);

  for (let i = 0; i < 40; i++) { // ten seconds of ticks at the 250 ms push period
    const again = extendPresence(row(p), parsePresenceExtension(record("e1", 992.0, STOPPED_AT_S))!);
    assert.equal(again, null, "a repeated end is not an extension");
  }
  assert.equal(p.last_interval!.t_end_s, STOPPED_AT_S, "the box stops where the evidence stops");

  // And once the track closes, the backend unbinds it and publishes nothing for it at all; the next
  // poll carries the closed interval. Nothing here has to know that — there is simply no record.
});

test("T-388: an out-of-order or replayed record never shortens a box", () => {
  const p = presence(990.0, 999.0);
  assert.equal(extendPresence(row(p), parsePresenceExtension(record("e1", 992.0, 997.0))!), null);
  assert.equal(extendPresence(row(p), parsePresenceExtension(record("e1", 992.0, 999.0))!), null);
});

test("T-388: an extension that would bridge a silence is refused — the box waits for the poll", () => {
  // A new track bound to the same emitter after a gap: its span starts AFTER the end on screen.
  // Stretching the box across that gap would assert the emitter transmitted through it.
  const p = presence(990.0, 995.0);
  assert.equal(extendPresence(row(p), parsePresenceExtension(record("e1", 1010.0, 1012.0))!), null);
  // Contiguous (the span overlaps the end on screen) is the case that does apply.
  assert.ok(extendPresence(row(p), parsePresenceExtension(record("e1", 994.5, 996.0))!));
});

test("T-388: a row with no interval in the window gets no box conjured for it", () => {
  const none: Presence = { intervals: 0, on_air_s: 0, last_interval: null, liveness: "absent", ended_t_s: null };
  assert.equal(extendPresence(row(none), parsePresenceExtension(record("e1", 990.0, 999.0))!), null);
  assert.equal(extendPresence(row(undefined), parsePresenceExtension(record("e1", 990.0, 999.0))!), null);
});

// ---------------------------------------------------------------------------
// parsing: only a presence extension is one
// ---------------------------------------------------------------------------

test("T-388: only a well-formed presence-extension record parses; everything else is nothing to apply", () => {
  assert.ok(parsePresenceExtension(record("e1", 1, 2)));
  const bad = [
    '{"type":"dropped","first_seq":4,"count":2,"t_ns":1}',
    JSON.stringify({ type: "message", emitter_id: "e1", metadata: { kind: "dwell" } }),
    JSON.stringify({ type: "message", metadata: { kind: PRESENCE_EXTENSION_KIND, last_interval: { t_start_s: 1, t_end_s: 2, open: true } } }),
    JSON.stringify({ type: "message", emitter_id: "e1", metadata: { kind: PRESENCE_EXTENSION_KIND, last_interval: { t_start_s: 1, t_end_s: 2 } } }),
    JSON.stringify({ type: "message", emitter_id: "e1", metadata: { kind: PRESENCE_EXTENSION_KIND, last_interval: { t_start_s: 1, t_end_s: "soon", open: true } } }),
    "not json at all",
  ];
  for (const b of bad) assert.equal(parsePresenceExtension(b), null, b);
});

/** The thin-client rule (CLAUDE.md), and the honesty constraint this task exists for: the fast path
 * must not be where a clock, a rate or a signal constant sneaks in. `Date.now` is named explicitly —
 * a box drawn to the browser's idea of now is the client-side extrapolation the push replaces. */
test("T-388: no clock, no signal logic and no RF constant in the presence modules", () => {
  for (const f of ["src/presence.ts", "src/app/explore/presence-stream.ts"]) {
    const src = readFileSync(f, "utf8").replace(/\/\*[\s\S]*?\*\//g, "").replace(/^\s*\/\/.*$/gm, "");
    for (const word of ["_db", "dbfs", "snr", "occupancy", "noise", "e6", "e9", "Date.now", "performance.now", "rowRate", "rowsPerS"]) {
      assert.ok(!src.toLowerCase().includes(word.toLowerCase()), `${f} must not contain "${word}"`);
    }
    assert.ok(!/\b\d{6,}(\.\d+)?\b/.test(src), `${f} must not contain a hard-coded frequency`);
  }
});

// ---------------------------------------------------------------------------
// the paused control: a frozen view is untouched while pushes arrive
// ---------------------------------------------------------------------------
//
// The scope rule (T-388): the live/following view only. A paused or scrubbed view answers about a
// fixed past window — which T-379/T-384 wired to the view window — and a push has nothing to offer
// there. So these drive the real socket module through a WebSocket stub and assert the pair:
// following applies a record, paused does not subscribe at all and its rows do not move.

import { mountPresenceStream } from "../src/app/explore/presence-stream";
import { goLive, reviewAt } from "../src/app/capture/slice";
import { setInventoryRows } from "../src/app/explore/slice";
import { createStore } from "../src/app/store";
import { initialState } from "../src/app/state";
import type { AppContext } from "../src/app/context";
import type { Row as ExploreRow } from "../src/app/explore/inventory";

class FakeSocket {
  static open: FakeSocket[] = [];
  binaryType = "";
  onmessage: ((ev: { data: string | ArrayBuffer }) => void) | null = null;
  onclose: (() => void) | null = null;
  closed = false;
  constructor(readonly url: string) { FakeSocket.open.push(this); }
  close() { this.closed = true; this.onclose?.(); }
  /** The header, then one record — what `openStream` expects in that order. */
  deliver(text: string) { this.onmessage?.({ data: text }); }
}

function harness(live: boolean) {
  FakeSocket.open = [];
  (globalThis as Record<string, unknown>).WebSocket = FakeSocket;
  (globalThis as Record<string, unknown>).location = { protocol: "http:", host: "h" };
  (globalThis as Record<string, unknown>).window = globalThis;
  const store = createStore(initialState());
  store.set(live ? goLive() : reviewAt(900));
  const e1 = {
    id: "e1", state: "confirmed", f_center_hz: 1, bandwidth_hz: 1, f_lo_hz: 1, f_hi_hz: 2,
    first_seen_s: 990, last_seen_s: 995, count: 2, known_status: "unknown", status: null,
    tags: [], family: null, identity_scheme: null, identity_class: null, withheld: false,
    recurrence: null, classification: null, explanations: [], refined: null, cluster_id: null,
    presence: presence(990.0, 995.0),
  } as unknown as ExploreRow;
  store.set(setInventoryRows({ e1 }, 1));
  let gets = 0;
  const client = {
    get: async () => { gets++; return { streams: [{ stream_id: "presence", kind: "messages", remote_permitted: true }] }; },
  } as unknown as AppContext["client"];
  const ctx = { store, client, token: "t" } as AppContext;
  const stop = mountPresenceStream(ctx);
  return { store, stop, gets: () => gets };
}

const endOf = (store: { get(): { inventory: { rows: Record<string, unknown> } } }) =>
  (store.get().inventory.rows.e1 as { presence: Presence }).presence.last_interval!.t_end_s;

test("T-388: a FOLLOWING view subscribes and a pushed extension moves the box top", async () => {
  const h = harness(true);
  await new Promise((r) => setTimeout(r, 0));
  assert.equal(h.gets(), 1, "discovery asked once");
  const sock = FakeSocket.open[0];
  assert.ok(sock && sock.url.includes("/ws/presence"), "subscribed to the presence stream");
  sock.deliver(JSON.stringify({ schema: "hackriff.stream", stream_id: "presence" })); // header
  sock.deliver(record("e1", 992.0, 999.6));
  assert.equal(endOf(h.store), 999.6);
  h.stop();
});

test("T-388: a PAUSED view never subscribes, and its rows are unchanged while pushes arrive", async () => {
  const h = harness(false);
  await new Promise((r) => setTimeout(r, 0));
  assert.equal(h.gets(), 0, "a frozen view asks for no stream at all");
  assert.equal(FakeSocket.open.length, 0, "and opens no socket");
  assert.equal(endOf(h.store), 995.0, "its window's rows stay exactly as the poll served them");
  h.stop();
});

test("T-388: pausing a following view closes the socket; going live re-subscribes", async () => {
  const h = harness(true);
  await new Promise((r) => setTimeout(r, 0));
  const sock = FakeSocket.open[0];
  sock.deliver(JSON.stringify({ schema: "hackriff.stream", stream_id: "presence" }));
  h.store.set(reviewAt(900));
  assert.ok(sock.closed, "paused: the push stops arriving, and the poll owns the window again");
  // A record that somehow lands after the pause is still not applied.
  sock.deliver(record("e1", 992.0, 999.9));
  assert.equal(endOf(h.store), 995.0);
  h.store.set(goLive());
  await new Promise((r) => setTimeout(r, 0));
  assert.equal(FakeSocket.open.length, 2, "following again: subscribed again");
  h.stop();
});
