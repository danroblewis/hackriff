// Listen audio output (T-043), built to dist/audio-worklet.js and loaded with
// audioWorklet.addModule (same origin, so the page's CSP allows it). Owns the jitter buffer;
// the page posts `{ pcm: Float32Array }` (transferred) and `{ reset: true }`; it posts stats back.
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
      const m = e.data as { pcm?: Float32Array; reset?: boolean };
      if (m.reset) this.jb.reset();
      if (m.pcm) this.jb.push(m.pcm);
    };
  }

  process(_inputs: Float32Array[][], outputs: Float32Array[][]): boolean {
    const out = outputs[0];
    if (out.length) {
      this.jb.pull(out[0]);
      for (let c = 1; c < out.length; c++) out[c].set(out[0]);
    }
    if (++this.blocks % 50 === 0) this.port.postMessage(this.jb.stats());
    return true;
  }
}

registerProcessor("hk-listen", ListenProcessor);
