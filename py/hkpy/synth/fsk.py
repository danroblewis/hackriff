"""2-FSK sensor framing and continuous-phase FSK modulation."""

from __future__ import annotations

import math

import numpy as np

#: CRC-16/CCITT-FALSE: poly 0x1021 (x^16 + x^12 + x^5 + 1), init 0xFFFF, no reflection, xorout 0.
CRC16_SPEC = {
    "algorithm": "CRC-16/CCITT-FALSE",
    "poly": "0x1021",
    "init": "0xFFFF",
    "refin": False,
    "refout": False,
    "xorout": "0x0000",
    "check_123456789": "0x29B1",
    "covers": "payload bytes",
}


def crc16_ccitt_false(data: bytes) -> int:
    crc = 0xFFFF
    for byte in data:
        crc ^= byte << 8
        for _ in range(8):
            crc = ((crc << 1) ^ 0x1021) & 0xFFFF if crc & 0x8000 else (crc << 1) & 0xFFFF
    return crc


def bytes_to_bits(data: bytes) -> np.ndarray:
    """MSB-first bits as uint8."""
    return np.unpackbits(np.frombuffer(data, dtype=np.uint8))


def bits_to_hex(bits: np.ndarray) -> str:
    """MSB-first hex, zero-padded at the end to a whole byte."""
    return np.packbits(np.asarray(bits, dtype=np.uint8)).tobytes().hex()


def cpfsk(bits: np.ndarray, sample_rate: float, symbol_rate: float, deviation_hz: float,
          *, bt: float = 0.0, phase0: float = 0.0) -> np.ndarray:
    """Continuous-phase 2-FSK: bit 1 -> +deviation, bit 0 -> -deviation. Optional Gaussian BT."""
    n = int(math.ceil(len(bits) * sample_rate / symbol_rate))
    sym = np.minimum((np.arange(n) * symbol_rate / sample_rate).astype(np.int64), len(bits) - 1)
    freq = deviation_hz * (2.0 * np.asarray(bits, dtype=np.float64)[sym] - 1.0)
    if bt > 0:
        sps = sample_rate / symbol_rate
        sigma = math.sqrt(math.log(2)) / (2 * math.pi * bt) * sps
        half = int(math.ceil(3 * sigma))
        k = np.arange(-half, half + 1)
        h = np.exp(-0.5 * (k / sigma) ** 2)
        freq = np.convolve(freq, h / h.sum(), mode="same")
    phase = phase0 + 2 * math.pi * np.cumsum(freq) / sample_rate
    return np.exp(1j * phase)
