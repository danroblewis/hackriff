// T-874 (ADR-0015 §12.13, LP-10): the Listen client and stereo — the request the dock builds (it
// opts in with channels=2; the TCP one-liner it shows does not), headers with 1 or 2 channels,
// the two-channel jitter buffer keeping L and R in step, the scope's mono mix, and a sub-line that
// says "stereo" only when the server's status reports the pilot locked.
import { test } from "node:test";
import assert from "node:assert/strict";
import { type AudioHeader, audioHeaderProblem, headerChannels, mixToMono, parseRecord, RECORD_HEADER_LEN } from "../src/audio-frames";
import { JitterBuffer } from "../src/jitter";
import { listenSocketQuery } from "../src/app/dock/audio-session";
import { audioSubText, listenTcpTarget } from "../src/app/dock/outputs";

const header = (channels?: number): AudioHeader => ({
  schema: "hackriff.stream", version: "1.5", stream_id: "listen/1", kind: "audio", content_class: "unrestricted",
  datatype: "ri16_le", sample_rate_hz: 48_000,
  audio: channels === undefined ? undefined : {
    channels, frame_samples: 960, mode: "wfm", mode_confidence: 0.9, params: {},
    squelch: { open_snr_db: 10, hysteresis_db: 3 }, agc: { enabled: false, target_dbfs: -20, max_gain_db: 40 },
  },
});

test("the dock's socket asks for stereo; the TCP target it shows stays the plain mono request", () => {
  const band = { kind: "band" as const, fLoHz: 101_200_000, fHiHz: 101_400_000, label: "" };
  assert.equal(listenSocketQuery(band), "f_lo=101200000&f_hi=101400000&channels=2");
  assert.equal(listenSocketQuery({ kind: "emitter", emitterId: "e7", label: "" }), "emitter=e7&channels=2");
  assert.equal(listenTcpTarget(band), "open/listen?f_lo=101200000&f_hi=101400000");
});

test("headers: 1 or 2 channels play, anything else is refused client-side", () => {
  assert.equal(audioHeaderProblem(header(1)), null);
  assert.equal(audioHeaderProblem(header(2)), null);
  assert.equal(audioHeaderProblem(header()), null, "a profile-less header is mono");
  assert.equal(headerChannels(header()), 1);
  assert.equal(headerChannels(header(2)), 2);
  assert.match(audioHeaderProblem(header(3)) ?? "", /channel count 3/);
});

test("a stereo record parses as interleaved samples and mixes to mono for the scope", () => {
  const n = 4; // two L/R frames
  const buf = new ArrayBuffer(RECORD_HEADER_LEN + 2 * n);
  const dv = new DataView(buf);
  dv.setUint8(0, 1);
  dv.setUint32(4, 2 * n, true);
  [16383, -16383, 32767, 0].forEach((v, i) => dv.setInt16(RECORD_HEADER_LEN + 2 * i, v, true));
  const r = parseRecord(buf);
  assert.ok(r && r.type === "pcm");
  assert.equal(r.samples.length, 4);
  const mono = mixToMono(r.samples, 2);
  assert.equal(mono.length, 2);
  assert.ok(Math.abs(mono[0]) < 1e-6 && Math.abs(mono[1] - 0.5) < 1e-4);
  assert.equal(mixToMono(r.samples, 1), r.samples, "mono passes through untouched");
});

test("a two-channel jitter buffer keeps L and R in step and counts frames, not values", () => {
  const jb = new JitterBuffer({ inputRate: 48_000, outputRate: 48_000, targetMs: 10, channels: 2 });
  assert.equal(jb.channels, 2);
  const frames = 960;
  const pcm = new Float32Array(2 * frames);
  for (let i = 0; i < frames; i++) { pcm[2 * i] = i / frames; pcm[2 * i + 1] = -i / frames; }
  jb.push(pcm);
  assert.equal(jb.available, frames, "one frame per L/R pair");
  const l = new Float32Array(128), r = new Float32Array(128);
  jb.pull(l, r);
  for (let i = 0; i < l.length; i++) assert.equal(r[i], -l[i], `frame ${i}: R is L's own partner`);
  assert.ok(l[l.length - 1] > 0);
  // One output from a stereo buffer is the channels' average (here L + R = 0).
  const one = new Float32Array(64);
  jb.pull(one);
  assert.ok(one.every((v) => Math.abs(v) < 1e-6));
  // Mono stays as it was: one value per frame.
  const mono = new JitterBuffer({ inputRate: 48_000, outputRate: 48_000, targetMs: 10 });
  mono.push(new Float32Array(960).fill(0.25));
  assert.equal(mono.available, 960);
  const m = new Float32Array(32);
  mono.pull(m);
  assert.ok(m.every((v) => v === 0.25));
});

test("the sub-line says stereo only when the status reports the pilot locked", () => {
  assert.equal(audioSubText("wfm", 48_000), "WFM audio · 48 kHz", "mono is unchanged");
  assert.equal(audioSubText("wfm", 48_000, 2), "WFM audio · 48 kHz · 2 ch");
  assert.equal(audioSubText("wfm", 48_000, 2, true), "WFM audio · 48 kHz · stereo");
  assert.equal(audioSubText("wfm", 48_000, 2, false), "WFM audio · 48 kHz · mono · no pilot lock");
  assert.equal(audioSubText("nbfm", 48_000, 1, true), "NBFM audio · 48 kHz", "a mono stream never says stereo");
});
