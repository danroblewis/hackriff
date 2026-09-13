"""Regenerates the tiny CW-tone fixture used by `just replay` smoke tests.

    uv run --project py python fixtures/tiny/make_tone.py

The output is deterministic: a noiseless complex tone quantised to ci8 (HackRF-native IQ).
"""

from pathlib import Path

import numpy as np

from hkpy import sigmf

HERE = Path(__file__).resolve().parent
SAMPLE_RATE = 100_000.0
CENTER_HZ = 100e6
OFFSET_HZ = 10_000.0
AMPLITUDE = 100  # int8 counts, ~0.79 of full scale
N = 4096

t = np.arange(N) / SAMPLE_RATE
iq = AMPLITUDE * np.exp(2j * np.pi * OFFSET_HZ * t)
interleaved = np.empty(2 * N, dtype=np.int8)
interleaved[0::2] = np.round(iq.real).astype(np.int8)
interleaved[1::2] = np.round(iq.imag).astype(np.int8)

meta = sigmf.new_meta(
    "ci8",
    SAMPLE_RATE,
    description="hackriff tiny fixture: noiseless CW tone at +10 kHz (T-001 smoke test)",
    author="hackriff",
    recorder="hkpy fixtures/tiny/make_tone.py",
    provenance=sigmf.provenance(
        "synthetic:hkpy",
        center_hz=CENTER_HZ,
        sample_rate_hz=SAMPLE_RATE,
        lna_db=0,
        vga_db=0,
        amp_on=False,
        bandwidth_hz=SAMPLE_RATE,
        timestamp_method="synthetic",
        timestamp_error_budget_ns=0,
    ),
)
sigmf.add_capture(meta, 0, frequency=CENTER_HZ, datetime="2026-09-13T00:00:00Z")
sigmf.add_annotation(
    meta,
    0,
    sample_count=N,
    freq_lower_edge=CENTER_HZ + OFFSET_HZ,
    freq_upper_edge=CENTER_HZ + OFFSET_HZ,
    label="tone",
    truth={"kind": "cw", "frequency_hz": CENTER_HZ + OFFSET_HZ, "amplitude_counts": AMPLITUDE},
)

sigmf.write_meta(meta, HERE / "tone.sigmf-meta")
(HERE / "tone.sigmf-data").write_bytes(interleaved.tobytes())
print(f"wrote {HERE / 'tone.sigmf-meta'} and {2 * N} bytes of ci8 data")
