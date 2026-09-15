// T-043 Listen: the audio stream framing parser and the jitter buffer.
import { test } from "node:test";
import assert from "node:assert/strict";
import { SeqTracker, audioHeaderProblem, parseRecord, parseText, type AudioHeader } from "../src/audio-frames";
import { JitterBuffer } from "../src/jitter";
import { Listener, clampBox, clickTarget, closeOnPageExit, listenQuery, peakBinIndex, resolveTarget, selectionTarget, strongestInView } from "../src/listen";
import { rowListenTarget } from "../src/inventory";

function record(type: number, flags: number, seq: number, payload: Uint8Array, sampleIndex = 0): ArrayBuffer {
  const buf = new ArrayBuffer(32 + payload.length);
  const dv = new DataView(buf);
  dv.setUint8(0, type);
  dv.setUint8(1, flags);
  dv.setUint32(4, payload.length, true);
  dv.setBigUint64(8, BigInt(seq), true);
  dv.setBigInt64(16, 1_789_297_800_500_000_000n, true);
  dv.setBigUint64(24, BigInt(sampleIndex), true);
  new Uint8Array(buf, 32).set(payload);
  return buf;
}

function pcm(values: number[]): Uint8Array {
  const b = new Uint8Array(values.length * 2);
  const dv = new DataView(b.buffer);
  values.forEach((v, i) => dv.setInt16(2 * i, v, true));
  return b;
}

test("PCM, gated, dropped and status records parse; malformed ones are rejected", () => {
  const r = parseRecord(record(1, 2, 7, pcm([0, 32767, -32767, 16384]), 960));
  assert.ok(r && r.type === "pcm");
  assert.equal(r.seq, 7);
  assert.equal(r.sampleIndex, 960);
  assert.equal(r.discontinuity, true);
  assert.equal(r.tS, 1_789_297_800.5);
  assert.deepEqual(Array.from(r.samples, (x) => Math.round(x * 1000) / 1000), [0, 1, -1, 0.5]);

  const gated = new ArrayBuffer(32);
  new DataView(gated).setUint8(0, 1);
  new DataView(gated).setUint8(1, 1);
  assert.deepEqual(parseRecord(gated), { type: "gated", seq: 0 });

  const count = new Uint8Array(8);
  new DataView(count.buffer).setBigUint64(0, 5n, true);
  const d = parseRecord(record(2, 2, 10, count));
  assert.ok(d && d.type === "dropped" && d.count === 5 && !d.gated);

  const s = parseRecord(record(3, 0, 11, new TextEncoder().encode('{"level_dbfs":-31.5,"squelch_open":true}')));
  assert.ok(s && s.type === "status");
  assert.equal(s.status.level_dbfs, -31.5);

  assert.equal(parseRecord(new ArrayBuffer(12)), null, "short");
  assert.equal(parseRecord(record(1, 0, 1, new Uint8Array(3))), null, "odd PCM payload");
  assert.equal(parseRecord(record(3, 0, 1, new TextEncoder().encode("{nope"))), null, "bad status JSON");
  assert.equal(parseRecord(record(9, 0, 4, new Uint8Array(0)))?.type, "unknown", "newer record types are skipped");
});

test("text messages: header vs refusal; header validation", () => {
  const h: AudioHeader = { schema: "hackriff.stream", version: "1.1", stream_id: "listen/1", kind: "audio", content_class: "unrestricted",
    datatype: "ri16_le", sample_rate_hz: 48000 };
  const m = parseText(JSON.stringify(h));
  assert.equal(m.kind, "header");
  assert.equal(audioHeaderProblem(h), null);
  assert.match(audioHeaderProblem({ ...h, kind: "spectrum" })!, /not an audio/);
  assert.match(audioHeaderProblem({ ...h, datatype: "rf32_le" })!, /datatype/);
  assert.match(audioHeaderProblem({ ...h, version: "2.0" })!, /version/);
  const refused = parseText('{"type":"refused","status":403,"code":"restricted-class","reason":"paging band","content_class":"restricted-paging"}');
  assert.ok(refused.kind === "refused" && refused.refusal.status === 403);
  assert.equal(parseText("not json").kind, "other");
});

test("sequence gaps and drop markers are counted", () => {
  const t = new SeqTracker();
  t.push({ type: "pcm", seq: 0, tS: 0, sampleIndex: 0, discontinuity: false, samples: new Float32Array() });
  t.push({ type: "dropped", seq: 1, count: 3, gated: false });
  t.push({ type: "status", seq: 4, status: {} as never });
  t.push({ type: "pcm", seq: 7, tS: 0, sampleIndex: 0, discontinuity: true, samples: new Float32Array() });
  assert.equal(t.dropped, 3);
  assert.equal(t.lost, 2);
});

test("listen queries carry a target but never a mode", () => {
  assert.equal(listenQuery({ label: "x", emitter: "abc" }), "emitter=abc");
  assert.equal(listenQuery({ label: "x", f_lo: 101.2e6, f_hi: 101.4e6 }), "f_lo=101200000&f_hi=101400000");
});

// --- T-069: toolbar target selection and clamping -----------------------------------------------

test("clampBox: inside stays put; off an edge shifts without changing width; wider than the range shrinks", () => {
  assert.deepEqual(clampBox(10, 20, 0, 100), [10, 20], "already inside");
  assert.deepEqual(clampBox(-5, 5, 0, 100), [0, 10], "off the left: shifted right, width kept");
  assert.deepEqual(clampBox(95, 105, 0, 100), [90, 100], "off the right: shifted left, width kept");
  assert.deepEqual(clampBox(-50, 150, 0, 100), [0, 100], "wider than the range: shrunk to fit");
});

test("clickTarget: ±25 kHz around the click, clamped to the view", () => {
  assert.deepEqual(clickTarget(101.3e6, null), { f_lo: 101.275e6, f_hi: 101.325e6 }, "no view: unclamped");
  assert.deepEqual(clickTarget(101.3e6, { loHz: 101.29e6, hiHz: 102e6 }),
    { f_lo: 101.29e6, f_hi: 101.34e6 }, "clamped at the view's low edge, width kept");
  assert.deepEqual(clickTarget(100e6, { loHz: 99e6, hiHz: 100.01e6 }, 25e3),
    { f_lo: 99.96e6, f_hi: 100.01e6 }, "clamped at the view's high edge, width kept");
});

test("selectionTarget: kept as-is up to 1 MHz, else clamped around its centre", () => {
  assert.deepEqual(selectionTarget({ f_lo: 101.2e6, f_hi: 101.4e6 }), { f_lo: 101.2e6, f_hi: 101.4e6 });
  // Centre (100e6 + 103e6)/2 = 101.5e6; clamped to ±0.5 MHz around it.
  assert.deepEqual(selectionTarget({ f_lo: 100e6, f_hi: 103e6 }), { f_lo: 101e6, f_hi: 102e6 });
});

test("peakBinIndex: the strongest finite bin within a range, else null", () => {
  const row = [-90, -80, -95, -60, -70, -85];
  assert.equal(peakBinIndex(row, 0, 6e6, 0, 6e6), 3, "whole row: bin 3 (-60)");
  assert.equal(peakBinIndex(row, 0, 6e6, 0, 2e6), 1, "restricted to bins 0-1");
  assert.equal(peakBinIndex([NaN, -Infinity], 0, 2e6, 0, 2e6), null, "nothing finite");
  assert.equal(peakBinIndex([], 0, 1, 0, 1), null, "empty row");
});

test("strongestInView: boxes the peak by its local -10 dB width, capped at 200 kHz total", () => {
  // 1000 bins over 10 MHz (10 kHz/bin): a peak at bin 500 with a narrow 1-bin-wide skirt each side.
  const n = 1000, fullLo = 0, fullHi = 10e6, df = (fullHi - fullLo) / n;
  const row = new Array(n).fill(-100);
  row[500] = -50;
  row[499] = -55; row[501] = -55; // within 10 dB of the peak; bins 498/502 (-100) are not
  const box = strongestInView(row, fullLo, fullHi, fullLo, fullHi)!;
  assert.ok(box);
  assert.equal(box.hz, fullLo + 500.5 * df);
  assert.deepEqual([box.f_lo, box.f_hi], [fullLo + 499 * df, fullLo + 502 * df], "boxed to the skirt (bins 499-501), not the whole band");

  // A wide skirt (or a flat plateau) is capped at 200 kHz total, not left to grow to the view's edges.
  const wide = new Array(n).fill(-100);
  wide[500] = -50;
  for (let d = 1; d <= 30; d++) { wide[500 - d] = -55; wide[500 + d] = -55; } // a 61-bin (610 kHz) skirt
  const capped = strongestInView(wide, fullLo, fullHi, fullLo, fullHi)!;
  assert.equal(capped.f_hi - capped.f_lo, 200e3, "capped at 200 kHz total");
  assert.ok(capped.f_lo < capped.hz && capped.hz < capped.f_hi);

  assert.equal(strongestInView(new Array(n).fill(NaN), fullLo, fullHi, fullLo, fullHi), null, "nothing finite: no target");
});

test("resolveTarget: click beats selection beats strongest-in-view; each short-circuits the rest", () => {
  const noStrongest = () => { throw new Error("must not be called"); };

  // 1. An emitter under the last click wins outright.
  const clicked = resolveTarget({ click: { emitterId: "e1", hz: 101.2577e6 }, selections: [{ f_lo: 90e6, f_hi: 91e6 }], view: null, strongest: noStrongest });
  assert.deepEqual(clicked, { source: "clicked", label: "101.2577 MHz (clicked)", emitter: "e1" });

  // A click with no matched emitter falls back to a ±25 kHz box, clamped to the view.
  const clickedBox = resolveTarget({ click: { emitterId: null, hz: 101.3e6 }, selections: [], view: { loHz: 101.29e6, hiHz: 102e6 }, strongest: noStrongest });
  assert.deepEqual(clickedBox, { source: "clicked", label: "101.3000 MHz (clicked)", f_lo: 101.29e6, f_hi: 101.34e6 });

  // 2. With no click, the most recent selection wins (clamped to 1 MHz around its centre:
  // (100e6 + 103e6)/2 = 101.5e6, so ±0.5 MHz around that).
  const sel = resolveTarget({
    click: null,
    selections: [{ f_lo: 88e6, f_hi: 89e6 }, { f_lo: 100e6, f_hi: 103e6 }],
    view: null, strongest: noStrongest,
  });
  assert.deepEqual(sel, { source: "selection", label: "101.5000 MHz (selection)", f_lo: 101e6, f_hi: 102e6 });

  // 3. With neither, the strongest signal in view.
  const strong = resolveTarget({ click: null, selections: [], view: { loHz: 0, hiHz: 1 }, strongest: () => ({ hz: 5e6, f_lo: 4.9e6, f_hi: 5.1e6 }) });
  assert.deepEqual(strong, { source: "strongest", label: "5.0000 MHz (strongest)", f_lo: 4.9e6, f_hi: 5.1e6 });

  // Nothing at all: no target.
  assert.equal(resolveTarget({ click: null, selections: [], view: null, strongest: () => null }), null);
});

test("an inventory row's Listen affordance (button or row click) targets its emitter id", () => {
  const row = { id: "e-101p2577", f_center_hz: 101.2577e6 };
  assert.deepEqual(rowListenTarget(row), { emitter: "e-101p2577", label: "101.2577 MHz" }, "the row's own Listen button");

  // A row click opens the inspect panel for the row (Inspector.showKnown), which reports the same
  // shape onShown gets from a canvas click-to-inspect: {emitterId: row.id, hz: row.f_center_hz}.
  // The toolbar resolves that into the emitter target, exactly like the per-row button.
  const fromClick = resolveTarget({ click: { emitterId: row.id, hz: row.f_center_hz }, selections: [], view: null, strongest: () => null });
  assert.deepEqual(fromClick, { source: "clicked", label: "101.2577 MHz (clicked)", emitter: row.id });
});

const ramp = (n: number, start = 0) => Float32Array.from({ length: n }, (_, i) => start + i);

test("jitter buffer prebuffers to the target, then plays in order", () => {
  const jb = new JitterBuffer({ inputRate: 1000, outputRate: 1000, targetMs: 100, maxMs: 500 });
  const out = new Float32Array(50);
  jb.push(ramp(60));
  jb.pull(out);
  assert.ok(out.every((v) => v === 0), "silent while below the 100-sample target");
  jb.push(ramp(60, 60));
  jb.pull(out);
  assert.deepEqual(Array.from(out.slice(0, 5)), [0, 1, 2, 3, 4]);
  assert.equal(jb.stats().playing, true);
  assert.equal(jb.underruns, 0);
});

test("jitter buffer counts an underrun once and re-prebuffers", () => {
  const jb = new JitterBuffer({ inputRate: 1000, outputRate: 1000, targetMs: 20, maxMs: 200 });
  jb.push(ramp(30));
  const out = new Float32Array(100);
  jb.pull(out);
  assert.equal(jb.underruns, 1);
  assert.equal(jb.stats().playing, false);
  assert.ok(out.slice(40).every((v) => v === 0), "silence after the underrun");
  jb.push(ramp(10));
  jb.pull(new Float32Array(10));
  assert.equal(jb.underruns, 1, "prebuffering again is not another underrun");
});

test("jitter buffer bounds latency by dropping the oldest audio back to the target", () => {
  const jb = new JitterBuffer({ inputRate: 1000, outputRate: 1000, targetMs: 100, maxMs: 300 });
  jb.push(ramp(250));
  assert.equal(jb.overflows, 0);
  jb.push(ramp(100, 250));
  assert.equal(jb.overflows, 1);
  assert.equal(jb.available, 100);
  assert.equal(jb.droppedSamples, 250);
  const out = new Float32Array(1);
  jb.pull(out);
  assert.equal(out[0], 250, "the newest audio survives");
});

test("jitter buffer converts 48 kS/s to the output rate", () => {
  const jb = new JitterBuffer({ inputRate: 48000, outputRate: 44100, targetMs: 50, maxMs: 2000 });
  const tone = Float32Array.from({ length: 48000 }, (_, i) => Math.sin((2 * Math.PI * 1000 * i) / 48000));
  jb.push(tone);
  const out = new Float32Array(44100 - 4000);
  jb.pull(out);
  // Consumed ≈ out.length × 48000/44100 input samples (small drift nudge allowed).
  const consumed = 48000 - jb.available;
  assert.ok(Math.abs(consumed - out.length * (48000 / 44100)) < 0.01 * consumed, `consumed ${consumed}`);
  let crossings = 0;
  for (let i = 1; i < out.length; i++) if (out[i - 1] < 0 && out[i] >= 0) crossings++;
  const hz = crossings / (out.length / 44100); // already above the target: no prebuffer silence
  assert.ok(Math.abs(hz - 1000) < 30, `tone at ${hz} Hz`);
});

// --- T-066: the listen socket closes on Stop and when the page goes away ------------------------

class FakeWs {
  static all: FakeWs[] = [];
  binaryType = "";
  onmessage: unknown = null;
  onclose: unknown = null;
  closed = false;
  constructor(public url: string) { FakeWs.all.push(this); }
  close() { this.closed = true; }
}

class FakeTarget {
  visibilityState = "visible";
  private handlers = new Map<string, (() => void)[]>();
  addEventListener(type: string, fn: () => void) { this.handlers.set(type, [...(this.handlers.get(type) ?? []), fn]); }
  fire(type: string) { for (const fn of this.handlers.get(type) ?? []) fn(); }
}

/** Just enough DOM, Web Audio and WebSocket for Listener in node. */
function stubBrowser() {
  const g = globalThis as Record<string, unknown>;
  g.document = { getElementById: () => ({ value: "1", textContent: "", addEventListener() {} }) };
  g.window = { setInterval: () => 0 };
  g.location = { protocol: "http:", host: "127.0.0.1:8787" };
  g.WebSocket = FakeWs;
  g.AudioContext = class {
    sampleRate = 48000;
    destination = {};
    resume() { return Promise.resolve(); }
    createGain() { return { connect() {}, gain: { value: 1 } }; }
    createScriptProcessor() { return { connect() {}, onaudioprocess: null }; }
  };
  FakeWs.all = [];
}

const tick = () => new Promise((r) => setTimeout(r, 0));

test("Stop closes the listen socket, and a superseded or stopped start opens none", async () => {
  stubBrowser();
  const l = new Listener("tok");
  l.start({ label: "a", f_lo: 1e6, f_hi: 1.01e6 });
  l.start({ label: "b", f_lo: 2e6, f_hi: 2.01e6 }); // before the first start's audio output is ready
  await tick();
  assert.equal(FakeWs.all.length, 1, "only the latest start opens a socket");
  assert.match(FakeWs.all[0].url, /f_lo=2000000/);
  assert.ok(l.active);
  l.stop();
  assert.equal(FakeWs.all[0].closed, true, "Stop closes the socket");
  assert.equal(l.active, false);
  l.start({ label: "c", f_lo: 3e6, f_hi: 3.01e6 });
  l.stop();
  await tick();
  assert.equal(FakeWs.all.length, 1, "a start stopped before its output was ready opens nothing");
});

test("pagehide, beforeunload and staying hidden close the listen socket", async () => {
  stubBrowser();
  const win = new FakeTarget(), doc = new FakeTarget();
  const clock = { fn: null as (() => void) | null, ms: 0 };
  const l = new Listener("tok");
  closeOnPageExit(l, win, doc, 1234, { set: (fn, ms) => { clock.fn = fn; clock.ms = ms; return 1; }, clear: () => { clock.fn = null; } });

  l.start({ label: "a", f_lo: 1e6, f_hi: 1.01e6 });
  await tick();
  win.fire("pagehide");
  assert.equal(FakeWs.all[0].closed, true, "pagehide closes the socket");
  assert.equal(l.active, false);

  l.start({ label: "b", f_lo: 1e6, f_hi: 1.01e6 });
  await tick();
  win.fire("beforeunload");
  assert.equal(FakeWs.all[1].closed, true, "beforeunload closes the socket");

  l.start({ label: "c", f_lo: 1e6, f_hi: 1.01e6 });
  await tick();
  doc.visibilityState = "hidden";
  doc.fire("visibilitychange");
  assert.equal(clock.ms, 1234);
  doc.visibilityState = "visible";
  doc.fire("visibilitychange");
  assert.equal(clock.fn, null, "coming back cancels the hidden timer");
  assert.equal(FakeWs.all[2].closed, false, "a short hide keeps listening");
  doc.visibilityState = "hidden";
  doc.fire("visibilitychange");
  clock.fn!();
  assert.equal(FakeWs.all[2].closed, true, "hidden for long closes the socket");
  assert.equal(l.active, false);
});
