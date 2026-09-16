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


# ---------------------------------------------------------------------------------------------
# TSBK content (T-268)
# ---------------------------------------------------------------------------------------------
#
# The block above is TSBK-*shaped*; these builders fill it with real P25 Phase 1 message fields so
# a decoder has something to read. Field layout and units, all verified against an independent
# reference before use (see crates/hk-detect/src/trunk/tsbk.rs for the verification notes):
#
#   TSBK, 12 bytes: LB(1) P(1) opcode(6) | MFID(8) | 8 argument bytes | CRC-16
#   IDEN_UP args, 64 bits: iden(4) bandwidth(9) offset-sign(1) offset-magnitude(8) spacing(10)
#                          base(32); base in units of 5 Hz, spacing in units of 125 Hz
#   GRP_VCH_GRANT args, 64 bits: service options(8) channel(16) group(16) source(24)
#   A 16-bit channel number is iden(4) then channel(12); f = base + spacing x channel
#
# STILL NOT STANDARDS-COMPLIANT, deliberately and in the same four ways T-267 recorded: no
# rate-1/2 trellis code, no interleaving, no status symbols, and CRC-16/CCITT-FALSE where real P25
# uses the augmented CRC-CCITT (init 0, final XOR 0xFFFF). A real off-air capture needs all four.

#: TSBK opcodes (verified).
TSBK_OP_GRP_VCH_GRANT = 0x00
TSBK_OP_GRP_VCH_GRANT_UPDATE = 0x02
TSBK_OP_IDEN_UP = 0x3D

# --- Service options (T-270) ---------------------------------------------------------------
#
# The encryption ("protected") bit of a grant's service-options octet, argument byte 0. VERIFIED
# three independent ways: SDRTrunk's ServiceOptions.java names ENCRYPTION_FLAG = 0x40 behind
# isEncrypted(); dsd-fme documents the same test as `svc & 0x40`; and a TIA-102.AABC-B-referenced
# field description gives bit 6 as "protected".
#
# The remaining bits (emergency 0x80, duplex 0x20, mode 0x10, priority 0x07) are UNVERIFIED for
# this project's purposes and the decoder maps nothing through them, so the generator sets none of
# them: a fixture must not depend on a field nothing is entitled to read.
#
# NOTE: a real GRP_VCH_GRANT_UPDATE (0x02) carries no service-options octet at all -- its standard
# argument layout is four 16-bit channel/group fields. This generator keeps T-268's simplified
# layout for 0x02 (see the module docstring's list of deliberate deviations); what matters for
# T-270 is that the DECODER reads a service-options octet only from opcode 0x00, so a channel
# announced only by an update can never have an encryption state, which is the late-entry case.
SVC_ENCRYPTED = 0x40

#: Base-frequency unit, Hz (verified: field 0x09157562 x 5 Hz = 762,006,250 Hz).
IDEN_BASE_UNIT_HZ = 5.0
#: Channel-spacing unit, Hz (verified: spacing field x 125 Hz).
IDEN_SPACING_UNIT_HZ = 125.0
#: Transmit-offset unit, Hz. UNVERIFIED encoding; nothing maps a downlink through it.
IDEN_OFFSET_UNIT_HZ = 250_000.0
#: Bandwidth unit, Hz. UNVERIFIED units.
IDEN_BANDWIDTH_UNIT_HZ = 125.0


def tsbk(opcode: int, args: bytes, *, last_block: bool = False, protected: bool = False,
         mfid: int = 0) -> bytes:
    """One 12-byte TSBK: header, 8 argument bytes, and a CRC over the 10 that precede it."""
    if len(args) != 8:
        raise ValueError("TSBK arguments are exactly 8 bytes")
    head = (0x80 if last_block else 0) | (0x40 if protected else 0) | (opcode & 0x3F)
    data = bytes([head, mfid & 0xFF]) + args
    return data + crc16_ccitt_false(data).to_bytes(TSBK_CRC_BYTES, "big")


def iden_up_args(iden: int, base_hz: float, spacing_hz: float, *, tx_offset_hz: float = 0.0,
                 bandwidth_hz: float = 0.0) -> bytes:
    """Pack an IDEN_UP argument field. Raises if a value does not encode exactly."""
    base = round(base_hz / IDEN_BASE_UNIT_HZ)
    spacing = round(spacing_hz / IDEN_SPACING_UNIT_HZ)
    bw = round(bandwidth_hz / IDEN_BANDWIDTH_UNIT_HZ)
    mag = round(abs(tx_offset_hz) / IDEN_OFFSET_UNIT_HZ)
    if base * IDEN_BASE_UNIT_HZ != base_hz:
        raise ValueError(f"base {base_hz} Hz is not a whole number of {IDEN_BASE_UNIT_HZ} Hz steps")
    if spacing * IDEN_SPACING_UNIT_HZ != spacing_hz:
        raise ValueError(f"spacing {spacing_hz} Hz is not a whole number of "
                         f"{IDEN_SPACING_UNIT_HZ} Hz steps")
    if not 0 <= iden <= 0xF or not 0 <= spacing <= 0x3FF or not 0 <= base <= 0xFFFF_FFFF:
        raise ValueError("an IDEN_UP field is out of range")
    if not 0 <= bw <= 0x1FF or not 0 <= mag <= 0xFF:
        raise ValueError("an IDEN_UP field is out of range")
    v = ((iden & 0xF) << 60) | (bw << 51) | ((1 if tx_offset_hz >= 0 else 0) << 50) \
        | (mag << 42) | (spacing << 32) | base
    return v.to_bytes(8, "big")


def grant_args(channel: int, talkgroup: int, *, source: int = 0, service_options: int = 0) -> bytes:
    """Pack a group voice channel grant argument field."""
    if not 0 <= channel <= 0xFFFF or not 0 <= talkgroup <= 0xFFFF:
        raise ValueError("a grant field is out of range")
    v = ((service_options & 0xFF) << 56) | (channel << 40) | (talkgroup << 24) \
        | (source & 0xFF_FFFF)
    return v.to_bytes(8, "big")


def channel_number(iden: int, channel: int) -> int:
    """The 16-bit channel number a grant carries: iden in the top 4 bits, channel in the low 12."""
    if not 0 <= iden <= 0xF or not 0 <= channel <= 0xFFF:
        raise ValueError("a channel number is iden(4) + channel(12)")
    return (iden << 12) | channel


def frames_from_blocks(blocks: list[bytes], n_frames: int) -> np.ndarray:
    """``n_frames`` frames of (frame sync + block), cycling through ``blocks``."""
    sync = sync_dibits()
    out: list[np.ndarray] = []
    for i in range(n_frames):
        block = blocks[i % len(blocks)]
        if len(block) != TSBK_BYTES:
            raise ValueError(f"a frame block is {TSBK_BYTES} bytes, got {len(block)}")
        out.append(sync)
        out.append(bytes_to_dibits(block))
    return np.concatenate(out).astype(np.uint8)


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
