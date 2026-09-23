#!/usr/bin/env python3
"""T-556 spike: synthesise a LoRa (SIGNAL-053) cf32 fixture with a hidden truth list.

Uses gr-lora_sdr's own TX chain (so the modulation is the reference implementation's), one
frame at a time, then places each frame at a random offset in AWGN with a random CFO. Writes
<out>.cf32 (complex64 LE, the host's channel datatype) and <out>.truth.json. Run under GNU
Radio's Python (see build.sh / env.sh).
"""
import json, sys
import numpy as np
from gnuradio import gr, blocks
import gnuradio.lora_sdr as lora_sdr

SF, BW, OS = 7, 125_000, 2
FS = BW * OS


def modulate(payload: str) -> np.ndarray:
    tb = gr.top_block()
    src = blocks.vector_source_b(list((payload + ",").encode()), False)
    # The lora_tx hier block only takes a message port; build its chain with a stream source.
    chain = [lora_sdr.whitening(False, False, ",", ""), lora_sdr.header(False, True, 1),
             lora_sdr.add_crc(True), lora_sdr.hamming_enc(1, SF),
             lora_sdr.interleaver(1, SF, 2, BW), lora_sdr.gray_demap(SF),
             lora_sdr.modulate(SF, FS, BW, [0x12], 0, 8)]
    sink = blocks.vector_sink_c()
    tb.connect(src, *chain, sink)
    tb.run()
    return np.array(sink.data(), dtype=np.complex64)


def main(out: str, n_frames: int = 12, seed: int = 556, snr_db: float = 10.0) -> None:
    rng = np.random.default_rng(seed)
    frames = []
    for i in range(n_frames):
        payload = f"hk{i:02d}-" + "".join(rng.choice(list("abcdefghijklmnop"), size=int(rng.integers(4, 24))))
        frames.append((payload, modulate(payload)))
    gaps = rng.integers(int(0.05 * FS), int(0.4 * FS), size=n_frames + 1)
    total = int(sum(len(w) for _, w in frames) + gaps.sum())
    noise_amp = 10 ** (-snr_db / 20) / np.sqrt(2)
    x = (rng.standard_normal(total) + 1j * rng.standard_normal(total)).astype(np.complex64) * noise_amp
    truth, pos = [], int(gaps[0])
    for i, (payload, w) in enumerate(frames):
        cfo_hz = float(rng.uniform(-3000, 3000))
        n = np.arange(len(w))
        x[pos:pos + len(w)] += w * np.exp(2j * np.pi * cfo_hz * n / FS).astype(np.complex64)
        truth.append({"start_sample": pos, "end_sample": pos + len(w), "payload": payload,
                      "cfo_hz": round(cfo_hz, 1)})
        pos += len(w) + int(gaps[i + 1])
    x.astype("<c8").tofile(out + ".cf32")
    meta = {"sample_rate_hz": FS, "sf": SF, "bw_hz": BW, "cr": 1, "snr_db": snr_db,
            "samples": total, "duration_s": total / FS, "frames": truth}
    json.dump(meta, open(out + ".truth.json", "w"), indent=1)
    print(f"{out}: {total} samples ({total / FS:.2f} s), {n_frames} frames", file=sys.stderr)


if __name__ == "__main__":
    main(sys.argv[1], *(int(a) for a in sys.argv[2:4]), *(float(a) for a in sys.argv[4:5]))
