// Listen (T-043): click a signal → the server estimates mode, bandwidth, squelch and AGC and
// streams audio over the authenticated `/ws/open/listen` WebSocket → Web Audio. There is no mode
// control: the page shows what was chosen. Restricted classes are refused server-side; the
// refusal reason is shown. Relative URLs (ws/wss follows the page), so it works through the
// tunnel. Text goes in via textContent.
import { fmtBandwidth } from "./axis";
import { type AudioHeader, type AudioStatus, SeqTracker, audioHeaderProblem, parseRecord, parseText } from "./audio-frames";
import { JitterBuffer, type JitterStats } from "./jitter";

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;

/** What to listen to: an inventory emitter, or a frequency extent. */
export type ListenTarget = { label: string } & ({ emitter: string } | { f_lo: number; f_hi: number });

/** The query string for a target (the token is appended separately). */
export function listenQuery(t: ListenTarget): string {
  const q = new URLSearchParams();
  if ("emitter" in t) q.set("emitter", t.emitter);
  else { q.set("f_lo", String(Math.round(t.f_lo))); q.set("f_hi", String(Math.round(t.f_hi))); }
  return q.toString();
}

/** Row shape the inspect panel reports (a subset of the inventory row). */
export interface ShownEmitter { id: string; f_center_hz: number }

/**
 * Wires Listen into the page: the inspect panel's Listen button (enabled once a click was
 * looked up) and a selection action. Returns the inspect-panel hook and the selection action.
 */
export function installListen(token: string) {
  const listener = new Listener(token);
  let target: ListenTarget | null = null;
  const button = $<HTMLButtonElement>("listen");
  button.addEventListener("click", () => { if (target) listener.start(target); });
  return {
    onShown: (r: ShownEmitter | null, hz: number, half: number) => {
      target = r ? { label: `emitter ${(r.f_center_hz / 1e6).toFixed(4)} MHz`, emitter: r.id }
        : { label: `${(hz / 1e6).toFixed(4)} MHz`, f_lo: hz - half, f_hi: hz + half };
      button.disabled = false;
    },
    selection: (s: { name: string; f_lo: number; f_hi: number }) => listener.start({ label: s.name, f_lo: s.f_lo, f_hi: s.f_hi }),
  };
}

type Output = { node: AudioNode; post: (pcm: Float32Array) => void; reset: () => void; stats: () => JitterStats | null };

export class Listener {
  private ctx: AudioContext | null = null;
  private gain: GainNode | null = null;
  private out: Output | null = null;
  private ws: WebSocket | null = null;
  private header: AudioHeader | null = null;
  private status: AudioStatus | null = null;
  private seq = new SeqTracker();
  private workletStats: JitterStats | null = null;
  private timer = 0;
  private note = "";

  constructor(private token: string) {
    $("player-stop").addEventListener("click", () => this.stop("stopped"));
    $<HTMLInputElement>("player-volume").addEventListener("input", () => this.applyVolume());
  }

  /** Starts listening. Call from the click/tap handler: the AudioContext is created and resumed
   * synchronously inside the gesture, which unlocks audio on mobile browsers. */
  start(target: ListenTarget) {
    this.stop("");
    if (!this.ctx) {
      try { this.ctx = new AudioContext({ latencyHint: "interactive", sampleRate: 48_000 }); }
      catch { this.ctx = new AudioContext({ latencyHint: "interactive" }); }
      this.gain = this.ctx.createGain();
      this.gain.connect(this.ctx.destination);
    }
    void this.ctx.resume();
    this.applyVolume();
    $("player").hidden = false;
    $("player-target").textContent = target.label;
    this.note = "estimating mode…";
    this.render();
    clearInterval(this.timer);
    this.timer = window.setInterval(() => this.render(), 250);
    void this.connect(target);
  }

  /** Stops listening (closing the socket detaches the server's chain). */
  stop(note = "stopped") {
    const ws = this.ws;
    this.ws = null;
    if (ws) { ws.onmessage = ws.onclose = null; ws.close(); }
    this.out?.reset();
    this.header = null;
    this.status = null;
    this.seq = new SeqTracker();
    if (note) { this.note = note; this.render(); clearInterval(this.timer); }
  }

  private applyVolume() {
    if (this.gain) this.gain.gain.value = Number($<HTMLInputElement>("player-volume").value);
  }

  private async output(): Promise<Output> {
    if (this.out) return this.out;
    const ctx = this.ctx!;
    if (ctx.audioWorklet) {
      try {
        await ctx.audioWorklet.addModule("audio-worklet.js");
        const node = new AudioWorkletNode(ctx, "hk-listen", { numberOfInputs: 0, outputChannelCount: [1] });
        node.port.onmessage = (e) => { this.workletStats = e.data as JitterStats; };
        node.connect(this.gain!);
        this.out = {
          node,
          post: (pcm) => node.port.postMessage({ pcm }, [pcm.buffer]),
          reset: () => node.port.postMessage({ reset: true }),
          stats: () => this.workletStats,
        };
        return this.out;
      } catch { /* insecure context or no worklet support: fall back */ }
    }
    // ScriptProcessor fallback (deprecated but universal, e.g. plain http on a LAN address).
    const jb = new JitterBuffer({ inputRate: 48_000, outputRate: ctx.sampleRate, targetMs: 200 });
    const node = ctx.createScriptProcessor(2048, 0, 1);
    node.onaudioprocess = (e) => jb.pull(e.outputBuffer.getChannelData(0));
    node.connect(this.gain!);
    this.out = { node, post: (pcm) => jb.push(pcm), reset: () => jb.reset(), stats: () => jb.stats() };
    return this.out;
  }

  private async connect(target: ListenTarget) {
    let out: Output;
    try { out = await this.output(); }
    catch (e) { this.note = `audio output: ${(e as Error).message}`; this.render(); return; }
    out.reset();
    const proto = location.protocol === "https:" ? "wss" : "ws";
    const ws = new WebSocket(`${proto}://${location.host}/ws/open/listen?${listenQuery(target)}&token=${encodeURIComponent(this.token)}`);
    ws.binaryType = "arraybuffer";
    this.ws = ws;
    ws.onmessage = (ev) => {
      if (typeof ev.data === "string") {
        const m = parseText(ev.data);
        if (m.kind === "refused") this.note = `refused (${m.refusal.status}): ${m.refusal.reason}`;
        else if (m.kind === "header") {
          const problem = audioHeaderProblem(m.header);
          if (problem) { this.note = problem; ws.close(); } else { this.header = m.header; this.note = ""; }
        }
        this.render();
        return;
      }
      if (!this.header) return;
      const r = parseRecord(ev.data as ArrayBuffer);
      if (!r) return;
      this.seq.push(r);
      if (r.type === "pcm") out.post(r.samples);
      else if (r.type === "status") this.status = r.status;
    };
    ws.onclose = (ev) => {
      if (this.ws !== ws) return;
      this.ws = null;
      if (ev.code >= 4000 && !this.note.startsWith("refused")) this.note = `refused (${ev.code - 4000}): ${ev.reason}`;
      else if (!this.note.startsWith("refused")) this.note = this.header ? "stream ended" : "could not start";
      this.header = null;
      this.render();
    };
  }

  private render() {
    const h = this.header, a = h?.audio, s = this.status, js = this.out?.stats();
    const parts: string[] = [];
    if (this.note) parts.push(this.note);
    if (a && h) {
      parts.push(`${a.mode.toUpperCase()} (${Math.round(100 * a.mode_confidence)} %)`);
      if (h.center_hz) parts.push(`${(h.center_hz / 1e6).toFixed(4)} MHz`);
      if (h.bandwidth_hz) parts.push(fmtBandwidth(h.bandwidth_hz));
    }
    if (s) {
      parts.push(`level ${s.level_dbfs.toFixed(1)} dBFS`);
      if (s.snr_db !== undefined) parts.push(`SNR ${s.snr_db.toFixed(0)} dB`);
      parts.push(s.squelch_open ? "squelch open" : "squelch closed");
      if (a?.agc.enabled) parts.push(`AGC ${s.agc_gain_db.toFixed(0)} dB`);
      parts.push(`latency ${s.latency_ms.toFixed(0)} ms`);
    }
    if (js && h) {
      parts.push(`buffer ${js.bufferedMs.toFixed(0)} ms`);
      const drops = this.seq.dropped + this.seq.lost + js.overflows;
      parts.push(`drops ${drops} · underruns ${js.underruns}`);
    }
    $("player-status").textContent = parts.join(" · ");
  }
}
