// Multi-stream Listen audio session for the Outputs dock (ADR-0013 §8, T-150). One shared
// `AudioContext` plays every live `/ws/open/listen` stream this page opened at once, each with its
// own `GainNode` (Mute) and jitter buffer, reusing the T-043 wire format (`../../audio-frames.ts`),
// jitter buffer (`../../jitter.ts`) and the built `dist/audio-worklet.js` (from
// `../../audio-worklet.ts`, unchanged — shared with the old page). No DOM; a thin client over the
// stream contract, not signal logic.
import { type AudioHeader, type AudioStatus, SeqTracker, audioHeaderProblem, parseRecord, parseText } from "../../audio-frames";
import { JitterBuffer } from "../../jitter";
import type { ListenTarget } from "./api";
import { listenQuery } from "./outputs";

type Output = { node: AudioNode; post: (pcm: Float32Array) => void; reset: () => void };

export interface AudioSessionEvents {
  /** The stream's header arrived: mode/bandwidth known, entry moves to `live`. */
  onHeader(id: string, header: AudioHeader): void;
  /** A refusal instead of a header, or a header this client can't play. */
  onRefused(id: string, status: number, reason: string): void;
  /** A status record (level/SNR/squelch), throttled by the server per the stream contract. */
  onStatus(id: string, status: AudioStatus): void;
  /** The socket closed, with or without ever getting a header. */
  onClosed(id: string, hadHeader: boolean, reason: string): void;
}

interface Entry { ws: WebSocket; gain: GainNode; out: Output }

/** Plays every live listen stream this page opened, over one shared `AudioContext`. */
export class AudioSession {
  private ctx: AudioContext | null = null;
  private entries = new Map<string, Entry>();
  // T-195: per-signal output panels tap the raw PCM a Listen stream already received, to draw a
  // scope — presentation only, the same samples `out.post` already sends to the speaker.
  private sampleSubs = new Map<string, Set<(pcm: Float32Array) => void>>();

  constructor(private token: string, private events: AudioSessionEvents) {}

  /** Subscribes to `id`'s raw PCM frames as they arrive (T-195's audio scope). Returns unsubscribe;
   * a no-op for an id with no live stream (a late subscriber just sees nothing until the next
   * frame). */
  onSamples(id: string, cb: (pcm: Float32Array) => void): () => void {
    let set = this.sampleSubs.get(id);
    if (!set) { set = new Set(); this.sampleSubs.set(id, set); }
    set.add(cb);
    return () => {
      const s = this.sampleSubs.get(id);
      if (!s) return;
      s.delete(cb);
      if (s.size === 0) this.sampleSubs.delete(id);
    };
  }

  /** Opens `id`'s stream. Call synchronously inside the click handler: creating/resuming the
   * `AudioContext` here unlocks audio on mobile browsers. Replaces any existing stream for `id`. */
  start(id: string, target: ListenTarget) {
    this.stop(id);
    if (!this.ctx) {
      try { this.ctx = new AudioContext({ latencyHint: "interactive", sampleRate: 48_000 }); }
      catch { this.ctx = new AudioContext({ latencyHint: "interactive" }); }
    }
    void this.ctx.resume();
    const gain = this.ctx.createGain();
    gain.connect(this.ctx.destination);
    void this.connect(id, target, gain);
  }

  setMuted(id: string, muted: boolean) {
    const e = this.entries.get(id);
    if (e) e.gain.gain.value = muted ? 0 : 1;
  }

  /** Closes `id`'s socket (a no-op for an unknown id). */
  stop(id: string) {
    const e = this.entries.get(id);
    if (!e) return;
    this.entries.delete(id);
    e.ws.onmessage = null;
    e.ws.onclose = null;
    e.ws.close();
    e.out.reset();
    e.gain.disconnect();
  }

  stopAll() { for (const id of [...this.entries.keys()]) this.stop(id); }

  private async output(): Promise<Output> {
    const ctx = this.ctx!;
    if (ctx.audioWorklet) {
      try {
        await ctx.audioWorklet.addModule("audio-worklet.js");
        const node = new AudioWorkletNode(ctx, "hk-listen", { numberOfInputs: 0, outputChannelCount: [1] });
        return { node, post: (pcm) => node.port.postMessage({ pcm }, [pcm.buffer]), reset: () => node.port.postMessage({ reset: true }) };
      } catch { /* insecure context or no worklet support: fall back */ }
    }
    const jb = new JitterBuffer({ inputRate: 48_000, outputRate: ctx.sampleRate, targetMs: 200 });
    const node = ctx.createScriptProcessor(2048, 0, 1);
    node.onaudioprocess = (e) => jb.pull(e.outputBuffer.getChannelData(0));
    return { node, post: (pcm) => jb.push(pcm), reset: () => jb.reset() };
  }

  private async connect(id: string, target: ListenTarget, gain: GainNode) {
    let out: Output;
    try { out = await this.output(); }
    catch (e) { this.events.onClosed(id, false, `audio output: ${(e as Error).message}`); gain.disconnect(); return; }
    if (!this.ctx) return; // stopAll() ran while the output was being set up
    out.node.connect(gain);
    const proto = location.protocol === "https:" ? "wss" : "ws";
    const ws = new WebSocket(`${proto}://${location.host}/ws/open/listen?${listenQuery(target)}&token=${encodeURIComponent(this.token)}`);
    ws.binaryType = "arraybuffer";
    const entry: Entry = { ws, gain, out };
    this.entries.set(id, entry);
    const seq = new SeqTracker();
    let header: AudioHeader | null = null;
    ws.onmessage = (ev) => {
      if (this.entries.get(id) !== entry) return;
      if (typeof ev.data === "string") {
        const m = parseText(ev.data);
        if (m.kind === "refused") this.events.onRefused(id, m.refusal.status, m.refusal.reason);
        else if (m.kind === "header") {
          const problem = audioHeaderProblem(m.header);
          if (problem) { this.events.onRefused(id, 0, problem); ws.close(); } else { header = m.header; this.events.onHeader(id, m.header); }
        }
        return;
      }
      if (!header) return;
      const r = parseRecord(ev.data as ArrayBuffer);
      if (!r) return;
      seq.push(r);
      if (r.type === "pcm") {
        // Notify scope subscribers before `out.post`: the AudioWorklet output path transfers
        // `r.samples.buffer` to the worklet thread (a zero-copy `postMessage` transfer), which
        // detaches it in this thread — reading it after that would see a zero-length array.
        for (const cb of this.sampleSubs.get(id) ?? []) cb(r.samples);
        out.post(r.samples);
      } else if (r.type === "status") this.events.onStatus(id, r.status);
    };
    ws.onclose = (ev) => {
      if (this.entries.get(id) !== entry) return;
      this.entries.delete(id);
      out.reset();
      gain.disconnect();
      const reason = ev.code >= 4000 ? `refused (${ev.code - 4000}): ${ev.reason}` : header ? "stream ended" : "could not start";
      this.events.onClosed(id, !!header, reason);
    };
  }
}
