// T-043 Listen: the audio stream framing parser and the jitter buffer.
import { test } from "node:test";
import assert from "node:assert/strict";
import { SeqTracker, audioHeaderProblem, parseRecord, parseText, type AudioHeader } from "../src/audio-frames";
import { JitterBuffer } from "../src/jitter";
import { listenQuery } from "../src/listen";

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
