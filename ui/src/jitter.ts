// Audio jitter buffer (T-043). Pure: runs inside the AudioWorklet and in the ScriptProcessor
// fallback, unit-tested in ui/test/listen.test.ts.
//
// - Prebuffers `targetMs` before playing; an empty buffer while playing is an underrun (counted
//   once per event, silence until the buffer refills to the target).
// - Bounded latency: when more than `maxMs` is queued the oldest audio is dropped back to the
//   target (counted as an overflow).
// - Rate conversion from the stream rate to the AudioContext rate by linear interpolation, with
//   a ±0.2 % playback-rate nudge that keeps the fill near the target despite clock drift.
// - Channels (T-874): `channels` interleaved samples make one frame; fill, latency and rate
//   conversion count frames, so every channel stays in step. `pull` fills one output per channel;
//   a single output from a multi-channel buffer gets the channels' average.

export interface JitterOptions {
  /** Stream sample rate, Hz. */
  inputRate: number;
  /** Output (AudioContext) rate, Hz. */
  outputRate: number;
  /** Prebuffer and steady-state fill, ms. */
  targetMs?: number;
  /** Largest fill before dropping, ms. */
  maxMs?: number;
  /** Interleaved channels per frame (default 1). */
  channels?: number;
}

export interface JitterStats {
  underruns: number;
  overflows: number;
  droppedSamples: number;
  bufferedMs: number;
  playing: boolean;
}

export class JitterBuffer {
  private buf: Float32Array;
  private readonly ch: number;
  private readonly frames: number; // capacity in frames
  private w = 0; // total frames written
  private r = 0; // total frames consumed (integer part of the read position)
  private frac = 0;
  private playing = false;
  private readonly ratio: number;
  readonly target: number;
  readonly max: number;
  underruns = 0;
  overflows = 0;
  droppedSamples = 0;

  constructor(private o: JitterOptions) {
    this.ratio = o.inputRate / o.outputRate;
    this.target = Math.round(((o.targetMs ?? 150) / 1000) * o.inputRate);
    this.max = Math.max(this.target + 1, Math.round(((o.maxMs ?? 600) / 1000) * o.inputRate));
    this.ch = Math.max(1, Math.floor(o.channels ?? 1));
    this.frames = this.max + 8192;
    this.buf = new Float32Array(this.frames * this.ch);
  }

  /** Interleaved channels per frame. */
  get channels() { return this.ch; }

  /** Frames queued. */
  get available() { return this.w - this.r; }

  stats(): JitterStats {
    return { underruns: this.underruns, overflows: this.overflows, droppedSamples: this.droppedSamples,
      bufferedMs: (this.available / this.o.inputRate) * 1000, playing: this.playing };
  }

  /** Forgets everything queued (e.g. a new stream). */
  reset() { this.w = this.r = 0; this.frac = 0; this.playing = false; }

  /** Queues stream samples (`channels` interleaved values per frame; a partial frame is ignored). */
  push(samples: Float32Array) {
    const ch = this.ch;
    const total = Math.floor(samples.length / ch);
    const n = Math.min(total, this.frames);
    if (this.available + n > this.max) {
      const newR = this.w + n - this.target;
      if (newR > this.r) { this.droppedSamples += newR - this.r; this.r = newR; this.frac = 0; this.overflows++; }
    }
    const cap = this.frames;
    for (let f = total - n; f < total; f++) {
      const at = (this.w++ % cap) * ch;
      for (let c = 0; c < ch; c++) this.buf[at + c] = samples[f * ch + c];
    }
  }

  /** Fills `outs` (one per channel; one output from a multi-channel buffer gets their average) at
   * the output rate, all the same length (silence while prebuffering or after an underrun). */
  pull(...outs: Float32Array[]) {
    const cap = this.frames, ch = this.ch;
    const len = outs.length ? outs[0].length : 0;
    const mix = outs.length === 1 && ch > 1;
    const silence = (i: number) => { for (const o of outs) o[i] = 0; };
    for (let i = 0; i < len; i++) {
      if (!this.playing) {
        if (this.available >= this.target) this.playing = true;
        else { silence(i); continue; }
      }
      if (this.available < 2) {
        this.playing = false;
        this.underruns++;
        silence(i);
        continue;
      }
      const ra = (this.r % cap) * ch, rb = ((this.r + 1) % cap) * ch;
      const at = (c: number) => { const a = this.buf[ra + c], b = this.buf[rb + c]; return a + (b - a) * this.frac; };
      if (mix) {
        let acc = 0;
        for (let c = 0; c < ch; c++) acc += at(c);
        outs[0][i] = acc / ch;
      } else {
        for (let k = 0; k < outs.length; k++) outs[k][i] = at(Math.min(k, ch - 1));
      }
      const avail = this.available;
      const nudge = avail > 1.5 * this.target ? 1.002 : avail < 0.5 * this.target ? 0.998 : 1;
      this.frac += this.ratio * nudge;
      const whole = Math.floor(this.frac);
      this.r += whole;
      this.frac -= whole;
    }
  }
}
