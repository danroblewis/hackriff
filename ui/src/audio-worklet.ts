// Listen audio output (T-043), built to dist/audio-worklet.js and loaded with
// audioWorklet.addModule (same origin, so the page's CSP allows it). Owns the jitter buffer;
// the page posts `{ pcm: Float32Array }` (transferred), `{ reset: true }` and, once the stream
// header says how many, `{ channels: 1 | 2 }` (T-874: interleaved L, R; a new, empty buffer); it
// posts stats back. The node always has two outputs: mono plays on both.
import { JitterBuffer } from "./jitter";

declare const sampleRate: number;
declare function registerProcessor(name: string, ctor: unknown): void;
declare class AudioWorkletProcessor {
  readonly port: MessagePort;
}

class ListenProcessor extends AudioWorkletProcessor {
  private jb = new JitterBuffer({ inputRate: 48_000, outputRate: sampleRate });
  private blocks = 0;

  constructor() {
    super();
    this.port.onmessage = (e: MessageEvent) => {
      const m = e.data as { pcm?: Float32Array; reset?: boolean; channels?: number };
      if (m.channels && m.channels !== this.jb.channels) this.jb = new JitterBuffer({ inputRate: 48_000, outputRate: sampleRate, channels: m.channels });
      if (m.reset) this.jb.reset();
      if (m.pcm) this.jb.push(m.pcm);
    };
  }

  process(_inputs: Float32Array[][], outputs: Float32Array[][]): boolean {
    const out = outputs[0];
    if (out.length) {
      // As many buffer channels as the output has; mono is copied to every output channel.
      const n = Math.min(out.length, this.jb.channels);
      this.jb.pull(...out.slice(0, n));
      for (let c = n; c < out.length; c++) out[c].set(out[n - 1]);
    }
    if (++this.blocks % 50 === 0) this.port.postMessage(this.jb.stats());
    return true;
  }
}

registerProcessor("hk-listen", ListenProcessor);
