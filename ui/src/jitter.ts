// Audio jitter buffer (T-043). Pure: runs inside the AudioWorklet and in the ScriptProcessor
// fallback, unit-tested in ui/test/listen.test.ts.
//
// - Prebuffers `targetMs` before playing; an empty buffer while playing is an underrun (counted
//   once per event, silence until the buffer refills to the target).
// - Bounded latency: when more than `maxMs` is queued the oldest audio is dropped back to the
//   target (counted as an overflow).
// - Rate conversion from the stream rate to the AudioContext rate by linear interpolation, with
//   a ±0.2 % playback-rate nudge that keeps the fill near the target despite clock drift.

export interface JitterOptions {
  /** Stream sample rate, Hz. */
  inputRate: number;
  /** Output (AudioContext) rate, Hz. */
  outputRate: number;
  /** Prebuffer and steady-state fill, ms. */
  targetMs?: number;
  /** Largest fill before dropping, ms. */
  maxMs?: number;
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
  private w = 0; // total samples written
  private r = 0; // total samples consumed (integer part of the read position)
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
    this.buf = new Float32Array(this.max + 8192);
  }

  /** Samples queued. */
  get available() { return this.w - this.r; }

  stats(): JitterStats {
    return { underruns: this.underruns, overflows: this.overflows, droppedSamples: this.droppedSamples,
      bufferedMs: (this.available / this.o.inputRate) * 1000, playing: this.playing };
  }

  /** Forgets everything queued (e.g. a new stream). */
  reset() { this.w = this.r = 0; this.frac = 0; this.playing = false; }

  /** Queues stream samples. */
  push(samples: Float32Array) {
    const n = Math.min(samples.length, this.buf.length);
    if (this.available + n > this.max) {
      const newR = this.w + n - this.target;
      if (newR > this.r) { this.droppedSamples += newR - this.r; this.r = newR; this.frac = 0; this.overflows++; }
    }
    const cap = this.buf.length;
    for (let i = samples.length - n; i < samples.length; i++) this.buf[this.w++ % cap] = samples[i];
  }

  /** Fills `out` at the output rate (silence while prebuffering or after an underrun). */
  pull(out: Float32Array) {
    const cap = this.buf.length;
    for (let i = 0; i < out.length; i++) {
      if (!this.playing) {
        if (this.available >= this.target) this.playing = true;
        else { out[i] = 0; continue; }
      }
      if (this.available < 2) {
        this.playing = false;
        this.underruns++;
        out[i] = 0;
        continue;
      }
      const a = this.buf[this.r % cap], b = this.buf[(this.r + 1) % cap];
      out[i] = a + (b - a) * this.frac;
      const avail = this.available;
      const nudge = avail > 1.5 * this.target ? 1.002 : avail < 0.5 * this.target ? 0.998 : 1;
      this.frac += this.ratio * nudge;
      const whole = Math.floor(this.frac);
      this.r += whole;
      this.frac -= whole;
    }
  }
}
