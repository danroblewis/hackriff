"""RDS (IEC 62106 / EN 50067) group 0A encoding: blocks with offset words, differential and biphase
coding at 1187.5 bd for the 57 kHz subcarrier."""

from __future__ import annotations

import math

import numpy as np
from scipy import signal

BITRATE_BD = 1187.5
SUBCARRIER_HZ = 57_000.0
PILOT_HZ = 19_000.0
#: g(x) = x^10 + x^8 + x^7 + x^5 + x^4 + x^3 + 1
CHECK_POLY = 0x5B9
OFFSET_WORDS = {"A": 0x0FC, "B": 0x198, "C": 0x168, "C'": 0x350, "D": 0x1B4}
BASEBAND_CUTOFF_HZ = 2400.0
ENCODING_NOTE = (
    "blocks of 16 info + 10 check bits (g(x)=x^10+x^8+x^7+x^5+x^4+x^3+1, XOR offset word), MSB first; "
    "differential coding d[k] = d[k-1] XOR b[k] (d[-1] = 0); biphase symbol +,- for d=1 and -,+ for d=0; "
    "rectangular half-symbols low-pass filtered at 2.4 kHz (approximates the EN 50067 shaping); "
    "DSB-SC on sin(3*w_pilot*t), i.e. in phase with the third pilot harmonic"
)


def checkword(info: int, offset: str) -> int:
    reg = (info & 0xFFFF) << 10
    for bit in range(25, 9, -1):
        if reg & (1 << bit):
            reg ^= CHECK_POLY << (bit - 10)
    return (reg & 0x3FF) ^ OFFSET_WORDS[offset]


def block(info: int, offset: str) -> int:
    return ((info & 0xFFFF) << 10) | checkword(info, offset)


def group_0a(pi: int, ps: str, segment: int, *, pty: int, tp: bool, ta: bool, music: bool,
             di: int, af: tuple[int, int] = (0xE0, 0xCD)) -> list[int]:
    """Four 26-bit blocks of a type 0A group carrying PS segment ``segment`` (0..3).

    ``di`` packs d3 d2 d1 d0 (dynamic PTY, compressed, artificial head, stereo) as bits 3..0; the
    segment address selects which DI bit is sent (0 -> d3 ... 3 -> d0)."""
    ps = ps.ljust(8)[:8]
    di_bit = (di >> (3 - segment)) & 1
    b2 = (0 << 12) | (0 << 11) | (int(tp) << 10) | (pty << 5) | (int(ta) << 4) | (int(music) << 3) \
        | (di_bit << 2) | segment
    b3 = (af[0] << 8) | af[1]
    b4 = (ord(ps[2 * segment]) << 8) | ord(ps[2 * segment + 1])
    return [block(pi, "A"), block(b2, "B"), block(b3, "C"), block(b4, "D")]


def radiotext_codes(text: str) -> list[int]:
    """RadioText as sent: up to 64 ASCII characters, a 0x0D end marker when shorter, padded with
    spaces to whole 4-character segments."""
    codes = list(text[:64].encode("ascii"))
    if len(codes) < 64:
        codes.append(0x0D)
    return codes + [0x20] * (-len(codes) % 4)


def group_2a(pi: int, text: str, segment: int, *, pty: int, tp: bool, ab: int = 0) -> list[int]:
    """Four 26-bit blocks of a type 2A group carrying RadioText segment ``segment``: four
    characters of :func:`radiotext_codes` in blocks C and D."""
    c = radiotext_codes(text)[4 * segment:4 * segment + 4]
    b2 = (2 << 12) | (0 << 11) | (int(tp) << 10) | (pty << 5) | ((ab & 1) << 4) | segment
    return [block(pi, "A"), block(b2, "B"), block((c[0] << 8) | c[1], "C"),
            block((c[2] << 8) | c[3], "D")]


def blocks_to_bits(blocks: list[int]) -> np.ndarray:
    return np.array([(b >> (25 - i)) & 1 for b in blocks for i in range(26)], dtype=np.uint8)


def differential(bits: np.ndarray) -> np.ndarray:
    return np.bitwise_xor.accumulate(bits.astype(np.uint8))


def biphase_baseband(dbits: np.ndarray, sample_rate: float) -> np.ndarray:
    """Band-limited biphase baseband (±1 nominal) at ``sample_rate``; bit 0 starts at t = 0."""
    n = int(math.ceil(len(dbits) * sample_rate / BITRATE_BD))
    pos = np.arange(n) * BITRATE_BD / sample_rate
    k = np.minimum(pos.astype(np.int64), len(dbits) - 1)
    second_half = (np.floor(2 * pos).astype(np.int64) % 2).astype(np.float64)
    s = (2.0 * dbits[k] - 1.0) * (1.0 - 2.0 * second_half)
    taps = int(4 * sample_rate / BITRATE_BD) | 1
    h = signal.firwin(taps, BASEBAND_CUTOFF_HZ, fs=sample_rate)
    return np.convolve(s, h, mode="same")
