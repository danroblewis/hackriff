// T-149 (ADR-0013 §3.2, §5): MUI transport helpers, the control-state → device reduction, and the
// app page's slots/breakpoints. No DOM under node:test, so layout checks read the HTML/CSS as text
// (the technique ui/test/inventory.test.ts uses); cwd is ui/ (justfile test-ui).
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { ControlError } from "../src/controls/client";
import type { ControlState } from "../src/controls/model";
import { apiConnFor, backoffMs, parseSpectrumRecord, wsUrl } from "../src/app/net";
import { deviceFrom } from "../src/app/shell";

const html = readFileSync("src/app/index.html", "utf8");
const css = readFileSync("src/app/app.css", "utf8");
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
  for (const s of ["inventory", "selections", "live", "axis", "capture", "focus", "pipelines", "stages", "plots", "inspector", "params", "outputs", "review"]) {
    assert.equal(html.split(`data-slot="${s}"`).length - 1, 1, `slot ${s}`);
  }
  assert.match(html, /data-mode="explore" aria-pressed="true"/);
  assert.match(html, /data-mode="decode" aria-pressed="false"/);
  assert.match(html, /id="view-decode" hidden/);
  assert.doesNotMatch(html, /fonts\.googleapis|<script[^>]+https?:/, "CSP is default-src 'self'");
});

test("app CSS keeps the mockup breakpoints, both themes and no page-wide horizontal scroll", () => {
  assert.match(css, /@media \(max-width: 1150px\)/);
  assert.match(css, /@media \(max-width: 900px\)[\s\S]*overflow-x: hidden/);
  assert.match(css, /@media \(prefers-color-scheme: light\)[\s\S]*:root:not\(\[data-theme="dark"\]\)/);
  assert.match(css, /:root\[data-theme="light"\]/);
  assert.doesNotMatch(css, /min-width: *[4-9]\d\dpx|min-width: *\d{4}px/, "no fixed min-width wider than a phone");
});
