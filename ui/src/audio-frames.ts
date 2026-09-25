// Listen stream framing (T-043; docs/stream-contract.md §10 bridge mapping and §12 audio profile).
// Pure (no DOM): unit-tested in ui/test/listen.test.ts.
//
// Over `/ws/open/listen` the first text message is either the stream header (kind `audio`,
// datatype `ri16_le`, 48 kS/s, `audio` profile with `channels` 1 — or 2, interleaved L, R, only
// when the client asked with `channels=2`, T-874) or a refusal `{"type":"refused",...}`
// followed by a close with code 4000 + status. Every later message is one binary record: a
// 32-byte little-endian header (type, flags, reserved, payload length, seq, t ns, sample index)
// and its payload. Types: 1 PCM data, 2 "dropped N" marker, 3 status JSON (metadata only).

export const RECORD_HEADER_LEN = 32;
export const REC_DATA = 1, REC_DROPPED = 2, REC_STATUS = 3;
export const FLAG_GATED = 1, FLAG_DISCONTINUITY = 2;

export interface AudioParams { bandwidth_hz?: number; cfo_hz?: number; deviation_hz?: number; pilot_hz?: number }
export interface AudioInfo {
  /** 1 (mono) or 2 (interleaved L, R — only on a stream that asked for stereo). */
  channels: number;
  /** Sample frames per record (a frame is one sample of every channel). */
  frame_samples: number; mode: string; mode_confidence: number; mode_rules?: string;
  params: AudioParams; snr_db?: number;
  squelch: { open_snr_db: number; hysteresis_db: number; noise_dbfs?: number };
  agc: { enabled: boolean; target_dbfs: number; max_gain_db: number };
  deemphasis_s?: number; demod?: string;
}
export interface AudioHeader {
  schema: string; version: string; stream_id: string; kind: string; content_class: string;
  datatype?: string; sample_rate_hz?: number; center_hz?: number; bandwidth_hz?: number; audio?: AudioInfo;
}
export interface Refusal { type: "refused"; status: number; code: string; reason: string; content_class?: string | null }
export interface AudioStatus {
  level_dbfs: number; snr_db?: number; squelch_open: boolean; agc_gain_db: number; frames: number;
  squelched_frames: number; lost_samples: number; latency_ms: number; backlog_s: number;
  /** Two-channel streams only (T-874): L−R is decoded now (pilot locked); false = both channels mono. */
  stereo?: boolean;
  /** Two-channel streams only: pilot lock losses since the stream began. */
  stereo_lock_losses?: number;
  /** NBFM streams only (T-988): the backend's blind CTCSS/DCS answer. */
  subaudible?: "measuring" | "ctcss" | "tone" | "dcs" | "none";
  subaudible_s?: number; ctcss_hz?: number; tone_hz?: number; tone_snr_db?: number; tone2_hz?: number;
  dcs_code?: string; dcs_polarity?: "normal" | "inverted"; dcs_alias?: string;
}

/** Channels a playable header carries (1 when the profile omits it). */
export function headerChannels(h: AudioHeader): number {
  return h.audio?.channels ?? 1;
}

/** Averages interleaved `channels`-channel samples to mono (presentation: a scope trace). */
export function mixToMono(samples: Float32Array, channels: number): Float32Array {
  if (channels <= 1) return samples;
  const out = new Float32Array(Math.floor(samples.length / channels));
  for (let i = 0; i < out.length; i++) {
    let acc = 0;
    for (let c = 0; c < channels; c++) acc += samples[i * channels + c];
    out[i] = acc / channels;
  }
  return out;
}

export type TextMessage = { kind: "header"; header: AudioHeader } | { kind: "refused"; refusal: Refusal } | { kind: "other" };

/** Classifies a text message. */
export function parseText(text: string): TextMessage {
  let v: unknown;
  try { v = JSON.parse(text); } catch { return { kind: "other" }; }
  if (!v || typeof v !== "object") return { kind: "other" };
  const o = v as Record<string, unknown>;
  if (o.type === "refused") return { kind: "refused", refusal: o as unknown as Refusal };
  if (o.schema === "hackriff.stream") return { kind: "header", header: o as unknown as AudioHeader };
  return { kind: "other" };
}

/** Why a header is not a playable listen stream, or null. */
export function audioHeaderProblem(h: AudioHeader): string | null {
  if (!h.version?.startsWith("1.")) return `unsupported contract version ${h.version}`;
  if (h.kind !== "audio") return `not an audio stream (${h.kind})`;
  if (h.datatype !== "ri16_le") return `unsupported datatype ${h.datatype}`;
  if (!(Number(h.sample_rate_hz) > 0)) return "missing sample rate";
  const channels = headerChannels(h);
  if (channels !== 1 && channels !== 2) return `unsupported channel count ${channels}`;
  return null;
}

export type AudioRecord =
  | { type: "pcm"; seq: number; tS: number; sampleIndex: number; discontinuity: boolean; samples: Float32Array }
  | { type: "gated"; seq: number }
  | { type: "dropped"; seq: number; count: number; gated: boolean }
  | { type: "status"; seq: number; status: AudioStatus }
  | { type: "unknown"; seq: number };

const u64 = (dv: DataView, at: number) => Number(dv.getBigUint64(at, true));

/** Parses one binary record; null when malformed. */
export function parseRecord(buf: ArrayBuffer): AudioRecord | null {
  if (buf.byteLength < RECORD_HEADER_LEN) return null;
  const dv = new DataView(buf);
  const type = dv.getUint8(0), flags = dv.getUint8(1), len = dv.getUint32(4, true), seq = u64(dv, 8);
  const tS = Number(dv.getBigInt64(16, true) / 1000n) / 1e6, sampleIndex = u64(dv, 24);
  const payload = buf.byteLength - RECORD_HEADER_LEN;
  if (type === REC_DATA) {
    if (flags & FLAG_GATED) return { type: "gated", seq };
    if (payload !== len || payload % 2) return null;
    const samples = new Float32Array(payload / 2);
    for (let i = 0; i < samples.length; i++) samples[i] = dv.getInt16(RECORD_HEADER_LEN + 2 * i, true) / 32767;
    return { type: "pcm", seq, tS, sampleIndex, discontinuity: (flags & FLAG_DISCONTINUITY) !== 0, samples };
  }
  if (type === REC_DROPPED) {
    if (payload < 8) return null;
    return { type: "dropped", seq, count: u64(dv, RECORD_HEADER_LEN), gated: (flags & FLAG_GATED) !== 0 };
  }
  if (type === REC_STATUS) {
    try {
      const status = JSON.parse(new TextDecoder().decode(new Uint8Array(buf, RECORD_HEADER_LEN))) as AudioStatus;
      return { type: "status", seq, status };
    } catch { return null; }
  }
  return { type: "unknown", seq };
}

/** Counts loss from sequence numbers: gaps and "dropped N" markers. */
export class SeqTracker {
  lastSeq = -1;
  /** Records never seen (sequence gaps). */
  lost = 0;
  /** Records the server dropped for this consumer (markers). */
  dropped = 0;

  push(r: AudioRecord) {
    const count = r.type === "dropped" ? r.count : 1;
    if (this.lastSeq >= 0 && r.seq > this.lastSeq + 1) this.lost += r.seq - this.lastSeq - 1;
    if (r.type === "dropped") this.dropped += r.count;
    this.lastSeq = Math.max(this.lastSeq, r.seq + count - 1);
  }
}
