// T-149 (ADR-0013 §3.2, §5): MUI transport helpers, the control-state → device reduction, and the
// app page's slots/breakpoints. No DOM under node:test, so layout checks read the HTML/CSS as text
// (the technique ui/test/inventory.test.ts uses); cwd is ui/ (justfile test-ui).
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { ControlError } from "../src/controls/client";
import type { ControlState } from "../src/controls/model";
import { mounts as centre } from "../src/app/centre";
import { mounts as decode } from "../src/app/decode";
import { mounts as dock } from "../src/app/dock";
import { mounts as explore } from "../src/app/explore";
import { mounts as history } from "../src/app/history";
import { mounts as review } from "../src/app/review";
import { apiConnFor, backoffMs, openStream, parseSpectrumRecord, STREAM_SCHEMA, wsUrl } from "../src/app/net";
import { deviceFrom } from "../src/app/shell";

const html = readFileSync("src/app/index.html", "utf8");
const entryCss = readFileSync("src/app/app.css", "utf8");
const cssImports = [...entryCss.matchAll(/@import "\.\/([^"]+)";/g)].map((m) => m[1]);
const css = cssImports.map((f) => readFileSync(`src/app/${f}`, "utf8")).join("\n");
// T-445's cutover replaced four centre slots — "live" (the waterfall), "axis" (the frequency tick
// strip) and the two edge navigators "timenav"/"freqnav" — with ONE: "surface".
// T-409's "nudge" stays: the tuning-nudge buttons in the top bar, beside the Go to control they sit
// next to and the Centre readout they change.
const SLOTS = ["inventory", "selections", "surface", "nudge", "focus", "pipelines", "stages", "plots", "inspector", "params", "outputs", "review", "catalogue"];
const replayState = JSON.parse(readFileSync("test/control_state_replay.json", "utf8")) as ControlState;

test("backoff doubles from 250 ms and caps at 10 s", () => {
  assert.deepEqual([0, 1, 2, 3].map((n) => backoffMs(n)), [250, 500, 1000, 2000]);
  assert.equal(backoffMs(30), 10_000);
  assert.equal(backoffMs(-1), 250);
});

test("ws URLs follow the page protocol and append the token once", () => {
  assert.equal(wsUrl({ protocol: "https:", host: "x.example" }, "/ws/spectrum/live", "a b"), "wss://x.example/ws/spectrum/live?token=a%20b");
  assert.equal(wsUrl({ protocol: "http:", host: "127.0.0.1:8787" }, "/ws/open/listen?emitter=e1", "t"), "ws://127.0.0.1:8787/ws/open/listen?emitter=e1&token=t");
});

test("API errors map onto the connection state", () => {
  assert.equal(apiConnFor(new ControlError(401, "unauthorized", "no")).api, "unauthorized");
  assert.equal(apiConnFor(new TypeError("fetch failed")).api, "offline");
  assert.equal(apiConnFor(new ControlError(503, "unavailable", "x")).api, "offline");
  assert.equal(apiConnFor(new ControlError(400, "invalid", "x")).api, "ok");
});

function record(type: number, flags: number, seq: number, tNs: bigint, payload: number[] | bigint | null): ArrayBuffer {
  const extra = payload === null ? 0 : typeof payload === "bigint" ? 8 : payload.length * 4;
  const buf = new ArrayBuffer(32 + extra);
  const dv = new DataView(buf);
  dv.setUint8(0, type); dv.setUint8(1, flags);
  dv.setBigUint64(8, BigInt(seq), true); dv.setBigInt64(16, tNs, true);
  if (typeof payload === "bigint") dv.setBigUint64(32, payload, true);
  else if (payload) payload.forEach((v, i) => dv.setFloat32(32 + 4 * i, v, true));
  return buf;
}

test("spectrum records: data, gated, dropped, unknown, short", () => {
  const d = parseSpectrumRecord(record(1, 2, 7, 1_789_300_800_500_000_000n, [-100, -90.5]));
  assert.ok(d && d.type === "data");
  assert.equal(d.seq, 7);
  assert.equal(d.tS, 1_789_300_800.5);
  assert.equal(d.discontinuity, true);
  assert.deepEqual(Array.from(d.row!), [-100, -90.5]);
  const g = parseSpectrumRecord(record(1, 1, 8, 0n, null));
  assert.ok(g && g.type === "data" && g.gated && g.row === null);
  const x = parseSpectrumRecord(record(2, 0, 9, 0n, 5n));
  assert.deepEqual(x, { type: "dropped", seq: 9, count: 5, gated: false });
  assert.deepEqual(parseSpectrumRecord(record(3, 0, 10, 0n, null)), { type: "other", seq: 10 });
  assert.equal(parseSpectrumRecord(new ArrayBuffer(8)), null);
});

// T-417: a stream id outlives its publishers. A retune (or a re-plumb) finishes the publisher and
// offers the next one under the same id, and the bridge carries the connection across rather than
// dropping the browser — so the client must read a LATER header on the same socket as a header.
// Reading it as a record would be worse than the disconnect it replaces: rows of the new window
// would be drawn against the old geometry.
test("a later stream header on a live socket is a new header, not a record", () => {
  class FakeWs {
    static last: FakeWs | null = null;
    binaryType = "";
    onmessage: ((ev: { data: unknown }) => void) | null = null;
    onclose: (() => void) | null = null;
    closed = false;
    constructor(readonly url: string) { FakeWs.last = this; }
    close() { this.closed = true; }
  }
  const g = globalThis as unknown as Record<string, unknown>;
  const prevWs = g.WebSocket, prevLoc = g.location;
  g.WebSocket = FakeWs;
  g.location = { protocol: "http:", host: "127.0.0.1:8789" };
  try {
    const headers: Record<string, unknown>[] = [], texts: string[] = [];
    let closes = 0;
    openStream("/ws/spectrum/live", "tok", {
      onHeader: (h) => headers.push(h),
      onText: (t) => texts.push(t),
      onClose: () => { closes++; },
    });
    const ws = FakeWs.last!;
    const header = (centerHz: number) => JSON.stringify({ schema: STREAM_SCHEMA, stream_id: "spectrum/live", kind: "spectrum", center_hz: centerHz });
    ws.onmessage!({ data: header(100.8e6) });
    // A messages stream's records are text too, and are NOT headers: no record carries `schema`.
    ws.onmessage!({ data: `${JSON.stringify({ t: 1, metadata: { kind: "END" }, content: null })}\n` });
    // The seam: the producer re-offered the stream at the new centre, on this same connection.
    ws.onmessage!({ data: header(101.8e6) });
    assert.equal(headers.length, 2);
    assert.equal(headers[1].center_hz, 101.8e6);
    assert.deepEqual(texts.length, 1, "the record went to onText, not onHeader");
    assert.equal(closes, 0, "a re-plumb is not a disconnect");
    assert.equal(ws.closed, false);
  } finally {
    g.WebSocket = prevWs;
    g.location = prevLoc;
  }
});

test("device slice from a replay control state", () => {
  const d = deviceFrom(replayState);
  assert.equal(d.loaded, true);
  assert.equal(d.live, false);
  assert.equal(d.centerHz, 100_800_000);
  assert.equal(d.sampleRateHz, 2_400_000);
  assert.equal(d.rowsPerS, 25);
  assert.equal(d.recording, false);
});

test("the app page has every panel slot once, and the mode toggle", () => {
  for (const s of SLOTS) {
    assert.equal(html.split(`data-slot="${s}"`).length - 1, 1, `slot ${s}`);
  }
  assert.match(html, /data-mode="explore" aria-pressed="true"/);
  assert.match(html, /data-mode="decode" aria-pressed="false"/);
  assert.match(html, /id="view-decode" hidden/);
  // T-264 (ADR-0017 TM-8): History is a surface beside Explore and Decode, hidden until asked for.
  assert.match(html, /data-mode="history" aria-pressed="false"/);
  assert.match(html, /id="view-history"[^>]*hidden/);
  assert.doesNotMatch(html, /fonts\.googleapis|<script[^>]+https?:/, "CSP is default-src 'self'");
});

test("every panel slot is mounted by exactly one area index", () => {
  const names = [explore, centre, decode, dock, review, history].flatMap((m) => Object.keys(m));
  assert.deepEqual([...names].sort(), [...SLOTS].sort());
});

test("app.css is an import list: base first, then one file per area", () => {
  assert.deepEqual(cssImports, ["base.css", "explore/explore.css", "centre/centre.css", "dock/dock.css", "decode/decode.css", "decode/inspector.css", "explore/output-panel.css", "review/review.css", "history/history.css", "menu/menu.css"]);
  assert.doesNotMatch(entryCss.replace(/\/\*[\s\S]*?\*\//g, "").replace(/@import "[^"]+";/g, ""), /\S/, "no rules in app.css itself");
});

test("app CSS keeps the mockup breakpoints, both themes and no page-wide horizontal scroll", () => {
  assert.match(css, /@media \(max-width: 1150px\)/);
  assert.match(css, /@media \(max-width: 900px\)[\s\S]*overflow-x: hidden/);
  assert.match(css, /@media \(prefers-color-scheme: light\)[\s\S]*:root:not\(\[data-theme="dark"\]\)/);
  assert.match(css, /:root\[data-theme="light"\]/);
  assert.doesNotMatch(css, /min-width: *[4-9]\d\dpx|min-width: *\d{4}px/, "no fixed min-width wider than a phone");
});
