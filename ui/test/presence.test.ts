// T-410 (ADR-0019): the presence stream carries an interval's ENDPOINTS, and the box runs to the
// live edge until an END caps it.
//
// T-388 built this under contract A — presence as an accumulation of observations, one record per
// open emitter per tick pushing the measured top forward. The user replaced it with contract B:
// presence is an interval with endpoints, so the measurement is the START plus the ABSENCE of an
// END, and a continuing interval says nothing at all.
//
// What these assert:
//
//   a. **an opening record opens a box that runs to the live edge**, and a continuing interval
//      produces no record to apply — the per-poll bump is gone, not merely slower;
//   b. **the control that matters, moved**: under contract A it was "a stopped emission stops
//      extending"; under contract B the box would over-claim to the live edge, so it is now "an END
//      caps the box AT THE MEASURED END" — the box retracts to the truth rather than stopping
//      wherever the assumption had reached;
//   c. the refusals ADR-0019 §7 keeps (no interval conjures no box; nothing shortens the measured
//      extent) and the one it replaces (a record after a silence no longer waits for the poll — it
//      REOPENS, as its own box, with the silence drawn as a gap).
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import {
  applyPresenceEvent, parsePresenceEvent,
  PRESENCE_END_KIND, PRESENCE_REOPEN_KIND, PRESENCE_REVOKE_KIND, PRESENCE_START_KIND,
} from "../src/presence";
import type { Presence, Row } from "../src/inventory";

/** One `presence` record as `hk-pipeline::presence` publishes it (docs/stream-contract.md §15). */
const record = (emitter: string, kind: string, t0: number, t1: number, revoked_s = 0) => {
  const open = kind !== PRESENCE_END_KIND;
  const opening = kind === PRESENCE_START_KIND || kind === PRESENCE_REOPEN_KIND;
  return JSON.stringify({
    type: "message", seq: 3, t_ns: Math.round((opening ? t0 : t1) * 1e9), emitter_id: emitter,
    content_class: "unrestricted", gated: false, frame_model: kind,
    metadata: { kind, last_interval: { t_start_s: t0, t_end_s: t1, open, revoked_s } },
  });
};
const start = (e: string, t0: number, t1: number) => record(e, PRESENCE_START_KIND, t0, t1);
const reopen = (e: string, t0: number, t1: number) => record(e, PRESENCE_REOPEN_KIND, t0, t1);
const end = (e: string, t0: number, t1: number) => record(e, PRESENCE_END_KIND, t0, t1);
const revoke = (e: string, t0: number, t1: number, revoked_s = 1.4) =>
  record(e, PRESENCE_REVOKE_KIND, t0, t1, revoked_s);

const presence = (t0: number, t1: number, open = true): Presence => ({
  intervals: 1, on_air_s: t1 - t0, last_interval: { t_start_s: t0, t_end_s: t1, open },
  liveness: open ? "live" : "ended", ended_t_s: open ? null : t1,
});

const row = (p: Presence | undefined): Pick<Row, "presence"> => ({ presence: p });

// ---------------------------------------------------------------------------
// (a) endpoints only: an opening record, then silence while the interval runs
// ---------------------------------------------------------------------------

test("T-410: an opening record leaves the interval OPEN, which is what makes the box run to the live edge", () => {
  const r = row(presence(990.0, 995.0, false));
  const after = applyPresenceEvent(r, parsePresenceEvent(reopen("e1", 1010.0, 1010.2))!)!;
  assert.equal(after.last_interval!.open, true, "open is the claim the box is drawn from");
  assert.equal(after.last_interval!.t_start_s, 1010.0, "a new interval brings its own start");
  assert.equal(after.last_interval!.t_end_s, 1010.2, "and its measured end, where the open cap begins");
});

test("T-410: a continuing interval produces nothing to apply — there is no per-tick bump left", () => {
  // Contract A pushed one record per tick for as long as the signal stayed on the air, and every
  // one of them moved the box. Contract B says nothing: the box is already at the live edge.
  const p = presence(990.0, 995.0);
  for (let i = 0; i < 40; i++) { // ten seconds of ticks at the 250 ms push period
    assert.equal(
      applyPresenceEvent(row(p), parsePresenceEvent(start("e1", 990.0, 995.0 + i * 0.25))!), null,
      "an opening record for the interval already on screen is a replay, not news",
    );
  }
  assert.equal(p.last_interval!.t_end_s, 995.0, "and the row is exactly as the poll served it");
});

// ---------------------------------------------------------------------------
// (b) the control that matters, moved: an END caps the box at the MEASURED end
// ---------------------------------------------------------------------------

test("T-410: an END caps the box at the measured end, not at the instant it was decided", () => {
  // The emission stopped at 999.5. The end detector needs one idle gap (1 s under a live dwell) of
  // observed silence to say so, so the END arrives around 1000.5 — but it names 999.5, so the box
  // RETRACTS to where the detector last heard it instead of keeping the second it had assumed.
  const STOPPED_AT_S = 999.5;
  const p = presence(990.0, 999.5);
  const after = applyPresenceEvent(row(p), parsePresenceEvent(end("e1", 990.0, STOPPED_AT_S))!)!;
  assert.equal(after.last_interval!.open, false, "closed: the box stops running to the live edge");
  assert.equal(after.last_interval!.t_end_s, STOPPED_AT_S, "at the measured end, never at now");
  // Replayed or duplicated, it changes nothing further.
  assert.equal(applyPresenceEvent(row(after), parsePresenceEvent(end("e1", 990.0, STOPPED_AT_S))!), null);
});

test("T-410: nothing may shorten the measured extent — refusal 2, restated for endpoints", () => {
  const p = presence(990.0, 999.0);
  // An END naming an earlier end than the one already measured would pull the box's measured edge
  // backwards. A reordered or replayed record cannot do that.
  assert.equal(applyPresenceEvent(row(p), parsePresenceEvent(end("e1", 990.0, 997.0))!), null);
  // An opening record for an interval starting at or before the one on screen cannot move its
  // start later either.
  assert.equal(applyPresenceEvent(row(p), parsePresenceEvent(start("e1", 985.0, 999.0))!), null);
  // But an END at or past the measured end applies, and CAPPING is not shortening: the span above
  // it was assumption standing in for this very measurement.
  assert.ok(applyPresenceEvent(row(p), parsePresenceEvent(end("e1", 990.0, 999.0))!));
});

test("T-410: a record after a silence REOPENS as its own box — it never bridges the gap", () => {
  // T-388's third refusal dropped this on the floor and waited for the poll. ADR-0019 §7 replaces
  // it: the returning signal gets its own interval immediately, and because `last_interval` is
  // REPLACED rather than stretched, no box ever spans the silence.
  const p = presence(990.0, 995.0, false);
  const after = applyPresenceEvent(row(p), parsePresenceEvent(reopen("e1", 1010.0, 1012.0))!)!;
  const iv = after.last_interval!;
  assert.equal(iv.t_start_s, 1010.0, "the new interval starts where the signal came back");
  assert.equal(iv.t_end_s, 1012.0);
  assert.ok(iv.t_start_s > 995.0, "and the 15 s of silence is between two boxes, not inside one");
});

test("T-410: a row with no interval in the window gets no box conjured for it — refusal 1, verbatim", () => {
  const none: Presence = { intervals: 0, on_air_s: 0, last_interval: null, liveness: "absent", ended_t_s: null };
  assert.equal(applyPresenceEvent(row(none), parsePresenceEvent(start("e1", 990.0, 999.0))!), null);
  assert.equal(applyPresenceEvent(row(undefined), parsePresenceEvent(start("e1", 990.0, 999.0))!), null);
  assert.equal(applyPresenceEvent(row(none), parsePresenceEvent(end("e1", 990.0, 999.0))!), null);
});

// ---------------------------------------------------------------------------
// parsing: only a presence endpoint is one
// ---------------------------------------------------------------------------

test("T-410: only a well-formed endpoint record parses; everything else is nothing to apply", () => {
  for (const good of [start("e1", 1, 2), reopen("e1", 1, 2), end("e1", 1, 2), revoke("e1", 1, 2)]) {
    assert.ok(parsePresenceEvent(good), good);
  }
  const meta = (m: unknown) => JSON.stringify({ type: "message", emitter_id: "e1", metadata: m });
  const bad = [
    '{"type":"dropped","first_seq":4,"count":2,"t_ns":1}',
    meta({ kind: "dwell" }),
    meta({ kind: "presence-extension", last_interval: { t_start_s: 1, t_end_s: 2, open: true } }),
    JSON.stringify({ type: "message", metadata: { kind: PRESENCE_START_KIND, last_interval: { t_start_s: 1, t_end_s: 2, open: true } } }),
    meta({ kind: PRESENCE_START_KIND, last_interval: { t_start_s: 1, t_end_s: 2 } }),
    meta({ kind: PRESENCE_START_KIND, last_interval: { t_start_s: 1, t_end_s: "soon", open: true } }),
    // A record that disagrees with itself: the kind says closed, the interval says open.
    meta({ kind: PRESENCE_END_KIND, last_interval: { t_start_s: 1, t_end_s: 2, open: true } }),
    meta({ kind: PRESENCE_START_KIND, last_interval: { t_start_s: 1, t_end_s: 2, open: false } }),
    // A revocation re-opens the interval it capped, so it can never state `open: false`.
    meta({ kind: PRESENCE_REVOKE_KIND, last_interval: { t_start_s: 1, t_end_s: 2, open: false } }),
    "not json at all",
  ];
  for (const b of bad) assert.equal(parsePresenceEvent(b), null, b);
});

/** The thin-client rule (CLAUDE.md), and the honesty constraint sharpened by ADR-0019: the box now
 * runs to the live edge, so it matters more than ever that the live edge is the render pass's own
 * newest row and never a clock read here. `Date.now` is named explicitly. */
test("T-410: no clock, no signal logic and no RF constant in the presence modules", () => {
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
// The scope rule (unchanged by T-410): the live/following view only. A paused or scrubbed view answers about a
// fixed past window — which T-379/T-384 wired to the view window — and a push has nothing to offer
// there. So these drive the real socket module through a WebSocket stub and assert the pair:
// following applies a record, paused does not subscribe at all and its rows do not move.

import { mountPresenceStream } from "../src/app/explore/presence-stream";
import { goLive, reviewAt } from "../src/app/centre/capture-slice";
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
    recurrence: null, classification: null, explanations: [], refined: null, cluster_id: null, cluster_group: null,
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

test("T-410: a FOLLOWING view subscribes and a pushed END caps the box", async () => {
  const h = harness(true);
  await new Promise((r) => setTimeout(r, 0));
  assert.equal(h.gets(), 1, "discovery asked once");
  const sock = FakeSocket.open[0];
  assert.ok(sock && sock.url.includes("/ws/presence"), "subscribed to the presence stream");
  sock.deliver(JSON.stringify({ schema: "hackriff.stream", stream_id: "presence" })); // header
  sock.deliver(end("e1", 990.0, 999.6));
  assert.equal(endOf(h.store), 999.6, "the END capped the box at the measured end");
  h.stop();
});

test("T-410: a PAUSED view never subscribes, and its rows are unchanged while pushes arrive", async () => {
  const h = harness(false);
  await new Promise((r) => setTimeout(r, 0));
  assert.equal(h.gets(), 0, "a frozen view asks for no stream at all");
  assert.equal(FakeSocket.open.length, 0, "and opens no socket");
  assert.equal(endOf(h.store), 995.0, "its window's rows stay exactly as the poll served them");
  h.stop();
});

test("T-410: pausing a following view closes the socket; going live re-subscribes", async () => {
  const h = harness(true);
  await new Promise((r) => setTimeout(r, 0));
  const sock = FakeSocket.open[0];
  sock.deliver(JSON.stringify({ schema: "hackriff.stream", stream_id: "presence" }));
  h.store.set(reviewAt(900));
  assert.ok(sock.closed, "paused: the push stops arriving, and the poll owns the window again");
  // A record that somehow lands after the pause is still not applied.
  sock.deliver(end("e1", 990.0, 999.9));
  assert.equal(endOf(h.store), 995.0);
  h.store.set(goLive());
  await new Promise((r) => setTimeout(r, 0));
  assert.equal(FakeSocket.open.length, 2, "following again: subscribed again");
  h.stop();
});

// ---------------------------------------------------------------------------
// (T-413) the END is provisional: a resumption inside the window revokes it
// ---------------------------------------------------------------------------

test("T-413: a REVOKE re-opens the SAME interval — one box grows, a second never appears", () => {
  // The poll served the row capped at its measured end. The signal came back inside the revocation
  // window, so the end is nulled and the interval is the one it capped: same start, open again.
  const p = presence(990.0, 995.0, false);
  const after = applyPresenceEvent(row(p), parsePresenceEvent(revoke("e1", 990.0, 996.6, 1.4))!)!;
  const iv = after.last_interval!;
  assert.equal(iv.t_start_s, 990.0, "the interval the END capped, not a new one");
  assert.equal(iv.t_end_s, 996.6, "the measured edge the resumption has reached");
  assert.equal(iv.open, true, "open again: the box runs to the live edge");
  assert.equal(iv.revoked_s, 1.4, "and it carries the measured silence it was rejoined across");
});

test("T-413: the revoked silence only ever grows — the stream states a bound, the poll the whole gap", () => {
  // The stream can only show the silence it watched before deciding; the poll knows the rest. A
  // later record must never talk the figure back down and make the box look more continuous.
  const p = presence(990.0, 995.0, false);
  const streamed = applyPresenceEvent(row(p), parsePresenceEvent(revoke("e1", 990.0, 996.6, 2.0))!)!;
  const again = applyPresenceEvent(row(streamed), parsePresenceEvent(revoke("e1", 990.0, 997.0, 1.0))!)!;
  assert.equal(again.last_interval!.revoked_s, 2.0, "never revised downwards");
});

test("T-413: a REVOKE addressed to a later interval, or one that would shorten the measured end, is refused", () => {
  const p = presence(990.0, 995.0, false);
  // A later start is a different interval — this row is not holding it, so there is nothing here
  // to revoke and nothing is invented (refusal 1's reasoning).
  assert.equal(applyPresenceEvent(row(p), parsePresenceEvent(revoke("e1", 1010.0, 1012.0))!), null);
  // And nothing may pull the measured extent backwards (refusal 2).
  assert.equal(applyPresenceEvent(row(p), parsePresenceEvent(revoke("e1", 990.0, 994.0))!), null);
  // A row with no interval at all still gets no box conjured for it.
  const none: Presence = { intervals: 0, on_air_s: 0, last_interval: null, liveness: "absent", ended_t_s: null };
  assert.equal(applyPresenceEvent(row(none), parsePresenceEvent(revoke("e1", 990.0, 996.0))!), null);
});
