// Listen (T-043, made discoverable in T-069): a toolbar above the waterfall picks a target (the
// last click, else the most recent selection, else the strongest signal in view) and the server
// estimates mode, bandwidth, squelch and AGC and streams audio over the authenticated
// `/ws/open/listen` WebSocket -> Web Audio. There is no mode control: the page shows what was
// chosen. Restricted classes are refused server-side; the refusal reason is shown in the status
// line. Relative URLs (ws/wss follows the page), so it works through the tunnel. Text goes in via
// textContent.
import { fmtBandwidth } from "./axis";
import { type AudioHeader, type AudioStatus, SeqTracker, audioHeaderProblem, parseRecord, parseText } from "./audio-frames";
import { JitterBuffer, type JitterStats } from "./jitter";
import type { SelectionStore } from "./selections";

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

// --- Toolbar target resolution (T-069), pure so it's unit-tested without a DOM -----------------

/** A frequency window, Hz. */
export interface Extent { loHz: number; hiHz: number }

/** The signal last clicked or tapped (an emitter when the inspect lookup found one). */
export interface ClickState { emitterId: string | null; hz: number }

/** A peak found in the latest spectrum row. */
export interface PeakBox { hz: number; f_lo: number; f_hi: number }

/** Clamps [lo, hi] inside [min, max], shrinking it only if it doesn't already fit. */
export function clampBox(lo: number, hi: number, min: number, max: number): [number, number] {
  const w = Math.min(hi - lo, Math.max(0, max - min));
  let l = Math.max(min, lo);
  if (l + w > max) l = max - w;
  return [l, l + w];
}

/** ±25 kHz around a clicked frequency, clamped to the current view. */
export function clickTarget(hz: number, view: Extent | null, halfWidthHz = 25_000): { f_lo: number; f_hi: number } {
  let lo = hz - halfWidthHz, hi = hz + halfWidthHz;
  if (view) [lo, hi] = clampBox(lo, hi, view.loHz, view.hiHz);
  return { f_lo: lo, f_hi: hi };
}

/** A selection's band, clamped to `maxSpanHz` around its centre if wider. */
export function selectionTarget(s: { f_lo: number; f_hi: number }, maxSpanHz = 1_000_000): { f_lo: number; f_hi: number } {
  if (s.f_hi - s.f_lo <= maxSpanHz) return { f_lo: s.f_lo, f_hi: s.f_hi };
  const c = (s.f_lo + s.f_hi) / 2;
  return { f_lo: c - maxSpanHz / 2, f_hi: c + maxSpanHz / 2 };
}

// Peak-picking over the spectrum ("what's the strongest signal in view") moved server-side in
// T-079 (`GET /api/analysis/strongest?f_lo&f_hi`, `hk_api::query::strongest_json`): it is signal
// analysis, not presentation, and the backend can look at its own measured history rather than
// just the one row the client happens to have decoded. See `Live.strongestSignal` in main.ts.

export type TargetSource = "clicked" | "selection" | "strongest";
export type ResolvedTarget = ListenTarget & { source: TargetSource };

const mhz = (hz: number) => (hz / 1e6).toFixed(4);

/** Chooses what the toolbar's Listen button would listen to: the last click, else the most
 * recent selection (clamped to 1 MHz), else the strongest signal in view. Null with none of those. */
export function resolveTarget(input: {
  click: ClickState | null;
  selections: readonly { f_lo: number; f_hi: number }[];
  view: Extent | null;
  strongest: () => PeakBox | null;
}): ResolvedTarget | null {
  const c = input.click;
  if (c) {
    if (c.emitterId) return { source: "clicked", label: `${mhz(c.hz)} MHz (clicked)`, emitter: c.emitterId };
    const box = clickTarget(c.hz, input.view);
    return { source: "clicked", label: `${mhz(c.hz)} MHz (clicked)`, f_lo: box.f_lo, f_hi: box.f_hi };
  }
  if (input.selections.length) {
    const s = input.selections[input.selections.length - 1];
    const box = selectionTarget(s);
    return { source: "selection", label: `${mhz((box.f_lo + box.f_hi) / 2)} MHz (selection)`, f_lo: box.f_lo, f_hi: box.f_hi };
  }
  const p = input.strongest();
  if (p) return { source: "strongest", label: `${mhz(p.hz)} MHz (strongest)`, f_lo: p.f_lo, f_hi: p.f_hi };
  return null;
}

// --- Playback (unchanged from T-043) ------------------------------------------------------------

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
  private _active = false;
  /** Bumped by every start/stop: a connect still awaiting the audio output opens no socket once
   * it is stale (T-066: a quick second Listen used to orphan the first socket). */
  private gen = 0;
  /** Called whenever [[active]] changes (T-069: drives the toolbar's Listen/Stop button and hint). */
  onActiveChange: (active: boolean) => void = () => {};

  constructor(private token: string) {
    $<HTMLInputElement>("player-volume").addEventListener("input", () => this.applyVolume());
  }

  /** Whether a listen session is starting or playing (a socket is open). */
  get active(): boolean { return this._active; }

  private setActive(a: boolean) {
    if (a === this._active) return;
    this._active = a;
    this.onActiveChange(a);
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
    this.setActive(true);
    $("player-target").textContent = target.label;
    this.note = "estimating mode…";
    this.render();
    clearInterval(this.timer);
    this.timer = window.setInterval(() => this.render(), 250);
    void this.connect(target, this.gen);
  }

  /** Stops listening (closing the socket detaches the server's chain). A start still waiting for
   * the audio output opens no socket afterwards. */
  stop(note = "stopped") {
    this.gen++;
    const ws = this.ws;
    this.ws = null;
    if (ws) { ws.onmessage = ws.onclose = null; ws.close(); }
    this.out?.reset();
    this.header = null;
    this.status = null;
    this.seq = new SeqTracker();
    this.setActive(false);
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

  private async connect(target: ListenTarget, gen: number) {
    let out: Output;
    try { out = await this.output(); }
    catch (e) { if (gen === this.gen) { this.note = `audio output: ${(e as Error).message}`; this.render(); } return; }
    if (gen !== this.gen) return; // stopped or restarted while the output was being set up
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
      this.setActive(false);
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

// --- Page lifecycle (T-066) ---------------------------------------------------------------------

/** A hidden page keeps listening this long (background listening is fine), then stops. */
export const HIDDEN_CLOSE_MS = 10 * 60_000;

/** The part of `window`/`document` the lifecycle hook uses. */
export interface LifecycleTarget { addEventListener(type: string, fn: () => void): void }

/**
 * Stops listening (closing the socket, so the server's chain detaches at once instead of after
 * its peer timeout) when the page goes away (`pagehide`, `beforeunload`) or stays hidden for
 * `hiddenMs`.
 */
export function closeOnPageExit(
  listener: { readonly active: boolean; stop(note?: string): void },
  win: LifecycleTarget,
  doc: LifecycleTarget & { readonly visibilityState: string },
  hiddenMs = HIDDEN_CLOSE_MS,
  timers: { set(fn: () => void, ms: number): unknown; clear(t: unknown): void } = {
    set: (fn, ms) => setTimeout(fn, ms),
    clear: (t) => clearTimeout(t as ReturnType<typeof setTimeout>),
  },
) {
  const close = (note: string) => { if (listener.active) listener.stop(note); };
  win.addEventListener("pagehide", () => close("stopped (page closed)"));
  win.addEventListener("beforeunload", () => close("stopped (page closed)"));
  let pending: unknown = null;
  doc.addEventListener("visibilitychange", () => {
    if (pending !== null) { timers.clear(pending); pending = null; }
    if (doc.visibilityState === "hidden") {
      pending = timers.set(() => { pending = null; close("stopped (page hidden)"); }, hiddenMs);
    }
  });
}

// --- Wiring (T-069: toolbar + inspect-panel button + selection/inventory actions) ---------------

export interface ListenHooks {
  selections: SelectionStore;
  /** The current view (Hz), or null before the first stream header. */
  view: () => Extent | null;
  /** The strongest signal in the current view, from the backend's spectrum-history analysis
   * (`GET /api/analysis/strongest`, T-079); null if none or the request fails. */
  strongest: () => Promise<PeakBox | null>;
}

/**
 * Wires Listen into the page: the always-visible toolbar above the waterfall (target resolution,
 * Listen/Stop, hint), the inspect panel's own Listen button (the exact clicked/inspected target),
 * and the selection and inventory-row actions. Returns the inspect-panel hook and those actions.
 */
export function installListen(token: string, hooks: ListenHooks) {
  const listener = new Listener(token);
  closeOnPageExit(listener, window, document); // T-066
  let click: ClickState | null = null;
  // Polled from the backend (below), not fetched inline: `listener.start()` must run synchronously
  // inside the click handler to unlock audio on mobile browsers, so `resolve()` stays synchronous
  // and reads the last poll instead of awaiting a fetch.
  let cachedStrongest: PeakBox | null = null;

  // The inspect panel's own button: listens to exactly what's shown there.
  let inspectTarget: ListenTarget | null = null;
  const inspectButton = $<HTMLButtonElement>("listen");
  inspectButton.addEventListener("click", () => { if (inspectTarget) listener.start(inspectTarget); });

  // The toolbar: resolves a target from click > selection > strongest-in-view.
  const goButton = $<HTMLButtonElement>("listen-go");
  const targetLabel = $("player-target");
  const hint = $("listen-hint");
  const resolve = () => resolveTarget({ click, selections: hooks.selections.list(), view: hooks.view(), strongest: () => cachedStrongest });

  const refreshLabel = () => {
    if (listener.active) return; // playing: start() already set the label to the locked-in target
    const t = resolve();
    targetLabel.textContent = t ? t.label : "no signal chosen";
    goButton.textContent = "Listen";
    goButton.disabled = !t;
  };

  // Only asked when it could matter: resolveTarget short-circuits on a click or selection before
  // ever reaching `strongest`, so there is no point polling the endpoint while either is set.
  const refreshStrongest = () => {
    if (click || hooks.selections.list().length) return;
    void hooks.strongest().then((p) => { cachedStrongest = p; refreshLabel(); }).catch(() => { cachedStrongest = null; });
  };

  goButton.addEventListener("click", () => {
    if (listener.active) { listener.stop(); return; }
    const t = resolve();
    if (t) listener.start(t);
  });

  listener.onActiveChange = (active) => {
    goButton.textContent = active ? "Stop" : "Listen";
    goButton.disabled = false;
    hint.hidden = active;
    if (!active) refreshLabel();
  };

  hooks.selections.subscribe(refreshLabel);
  refreshLabel();
  refreshStrongest();
  window.setInterval(() => { refreshStrongest(); refreshLabel(); }, 1000); // catches panning/zooming while idle

  return {
    /** From the inspect panel (T-044 click-to-inspect, plus T-069 inventory-row inspect): the
     * emitter shown (null if none) and the clicked frequency. Becomes the toolbar's top-priority
     * target too. */
    onShown: (r: ShownEmitter | null, hz: number, half: number) => {
      click = { emitterId: r ? r.id : null, hz };
      inspectTarget = r ? { label: `emitter ${(r.f_center_hz / 1e6).toFixed(4)} MHz`, emitter: r.id }
        : { label: `${(hz / 1e6).toFixed(4)} MHz`, f_lo: hz - half, f_hi: hz + half };
      inspectButton.disabled = false;
      refreshLabel();
    },
    /** A selection's own Listen action. */
    selection: (s: { name: string; f_lo: number; f_hi: number }) => listener.start({ label: s.name, f_lo: s.f_lo, f_hi: s.f_hi }),
    /** An inventory row's own Listen button: listens by emitter id directly. */
    rowListen: (id: string, label: string) => listener.start({ label, emitter: id }),
  };
}
