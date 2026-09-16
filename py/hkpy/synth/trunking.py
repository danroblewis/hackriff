"""C4FM/4FSK trunking control-channel framing and modulation (T-267, C23).

A land-mobile-radio control channel transmits a **continuous** outbound data stream at 100 %
duty cycle on the 12.5 kHz LMR raster (docs/04 §8.1). That description alone does not identify
one: any continuous data emitter parked on the raster looks identical to an occupancy measure.
What separates a control channel from a continuous data emitter is *framing* — a known frame
sync pattern and payloads whose CRC checks out (docs/04 §8.4, C23 "Pitfalls").

This module therefore builds both, so a fixture can carry a real control channel **and** a
decoy that is continuous, on-raster and 4FSK but carries no recoverable framing.

**Scope.** Real P25 Phase 1 protects the TSBK with a rate-1/2 trellis code and interleaves it;
that coding is deliberately **not** implemented here. T-267 confirms *that* a control channel
exists (sync + CRC); T-268 decodes TSBKs and names the protocol. The frame below is the honest
minimum for that split: a real 48-bit P25 frame sync followed by a 12-byte block whose last two
bytes are a real CRC-16/CCITT-FALSE over the first ten. Nothing here should be read as a
standards-compliant P25 encoder.
"""

from __future__ import annotations

import math
from typing import Any

import numpy as np

from hkpy.synth.fsk import crc16_ccitt_false

#: P25 Phase 1 frame sync, 48 bits (docs/04 §7.3). MSB first.
P25_FRAME_SYNC_HEX = "5575f5ff77ff"
P25_FRAME_SYNC_BITS = 48
#: Dibits per frame sync (48 bits / 2 bits per C4FM symbol).
P25_FRAME_SYNC_DIBITS = P25_FRAME_SYNC_BITS // 2

#: P25 Phase 1 C4FM: 4800 symbols/s = 9600 bit/s (docs/04 §7.2).
C4FM_SYMBOL_RATE_BD = 4800.0
#: C4FM dibit -> frequency deviation, Hz (docs/04 §7.2: peaks at +/-600 and +/-1800 Hz).
C4FM_DEVIATIONS_HZ = {0b01: 1800.0, 0b00: 600.0, 0b10: -600.0, 0b11: -1800.0}

#: TSBK-shaped block: 10 data bytes + 2 CRC bytes.
TSBK_DATA_BYTES = 10
TSBK_CRC_BYTES = 2
TSBK_BYTES = TSBK_DATA_BYTES + TSBK_CRC_BYTES
#: Dibits in one frame: sync + block.
FRAME_DIBITS = P25_FRAME_SYNC_DIBITS + TSBK_BYTES * 4

#: The 12.5 kHz narrowband LMR raster (docs/04 §4: narrowbanding mandated 2013).
LMR_RASTER_HZ = 12_500.0

CC_FRAME_SPEC: dict[str, Any] = {
    "sync": "P25 Phase 1 frame sync, 48 bits, MSB first",
    "sync_hex": P25_FRAME_SYNC_HEX,
    "block": "12 bytes: 10 data + CRC-16/CCITT-FALSE over those 10",
    "symbol_rate_bd": C4FM_SYMBOL_RATE_BD,
    "modulation": "c4fm",
    "dibit_map": {f"{k:02b}": v for k, v in C4FM_DEVIATIONS_HZ.items()},
    "frame_dibits": FRAME_DIBITS,
    "coding": "none (T-267 confirms sync+CRC; the P25 1/2-rate trellis is T-268)",
}


def bytes_to_dibits(data: bytes) -> np.ndarray:
    """MSB-first dibits (values 0..3) of ``data``."""
    bits = np.unpackbits(np.frombuffer(data, dtype=np.uint8))
    return (bits[0::2] * 2 + bits[1::2]).astype(np.uint8)


def sync_dibits() -> np.ndarray:
    """The 24 dibits of the P25 frame sync."""
    return bytes_to_dibits(bytes.fromhex(P25_FRAME_SYNC_HEX))


def tsbk_block(rng: np.random.Generator) -> tuple[bytes, int]:
    """A 12-byte TSBK-shaped block with a valid CRC. Returns ``(block, crc)``."""
    data = bytes(int(v) for v in rng.integers(0, 256, TSBK_DATA_BYTES))
    crc = crc16_ccitt_false(data)
    return data + crc.to_bytes(TSBK_CRC_BYTES, "big"), crc


def control_channel_dibits(rng: np.random.Generator, n_frames: int) -> tuple[np.ndarray, list[dict[str, Any]]]:
    """``n_frames`` back-to-back frames of (frame sync + CRC-valid block) as dibits."""
    sync = sync_dibits()
    out: list[np.ndarray] = []
    frames: list[dict[str, Any]] = []
    for i in range(n_frames):
        block, crc = tsbk_block(rng)
        out.append(sync)
        out.append(bytes_to_dibits(block))
        frames.append({
            "index": i,
            "block_hex": block.hex(),
            "crc_hex": f"{crc:04x}",
            "crc_valid": True,
        })
    return np.concatenate(out).astype(np.uint8), frames


def continuous_data_dibits(rng: np.random.Generator, n_dibits: int) -> np.ndarray:
    """Unframed continuous 4FSK traffic: the decoy a pure FCO test cannot tell from a CC.

    Uniform random dibits carry no frame sync and no CRC-valid block, so this emitter is
    continuous, on-raster and 4FSK — it passes every candidacy test — and must still be
    rejected at confirmation.
    """
    return rng.integers(0, 4, int(n_dibits)).astype(np.uint8)


def c4fm(dibits: np.ndarray, sample_rate: float, symbol_rate: float = C4FM_SYMBOL_RATE_BD,
         *, deviations: dict[int, float] | None = None, phase0: float = 0.0,
         shape: bool = True) -> np.ndarray:
    """Continuous-phase 4FSK (C4FM) for ``dibits``, one complex sample per output sample.

    ``shape`` applies the raised-cosine-ish symbol smoothing a real C4FM modulator uses, which
    keeps the emission inside 12.5 kHz instead of splattering across neighbouring channels.
    """
    dev = deviations or C4FM_DEVIATIONS_HZ
    d = np.asarray(dibits, dtype=np.uint8)
    n = int(math.ceil(len(d) * sample_rate / symbol_rate))
    idx = np.minimum((np.arange(n) * symbol_rate / sample_rate).astype(np.int64), len(d) - 1)
    levels = np.array([dev[int(v)] for v in d], dtype=np.float64)
    freq = levels[idx]
    if shape:
        sps = sample_rate / symbol_rate
        # A **one-symbol-wide** raised cosine. The width is not cosmetic: a two-symbol-wide pulse
        # is not a Nyquist pulse, so it closes the eye at the symbol centres and no receiver can
        # read it — measured here at ~38 % symbol error rate against 0 % at this width. A fixture
        # that cannot be demodulated tests nothing, so the pulse has to be one a real C4FM
        # modulator would emit: narrow enough to keep the centres clean, wide enough to
        # band-limit the deviation steps into 12.5 kHz.
        w = max(int(sps / 2), 1)
        k = np.arange(-w, w + 1)
        h = 0.5 * (1.0 + np.cos(math.pi * k / w))
        freq = np.convolve(freq, h / h.sum(), mode="same")
    phase = phase0 + 2 * math.pi * np.cumsum(freq) / sample_rate
    return np.exp(1j * phase)
