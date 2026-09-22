"""2-FSK sensor framing and continuous-phase FSK modulation."""

from __future__ import annotations

import math
from typing import Any

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


def reflect(value: int, width: int) -> int:
    """Bit-reverses the low `width` bits of `value`."""
    r = 0
    for _ in range(width):
        r = (r << 1) | (value & 1)
        value >>= 1
    return r


def crc_generic(data: bytes, width: int, poly: int, init: int, refin: bool, refout: bool,
                 xorout: int) -> int:
    """A CRC in the RevEng parameter model (width, poly, init, refin, refout, xorout), any width
    8..32, matching `crates/hk-estimate/src/framing/crc.rs`'s `CrcCore` bit for bit (verified
    against every catalogue entry's ASCII-`"123456789"` check value in `test_synth.py`)."""
    mask = (1 << width) - 1
    top = 1 << (width - 1)
    reg = init & mask
    for byte in data:
        b = reflect(byte, 8) if refin else byte
        reg ^= (b << (width - 8)) & mask
        for _ in range(8):
            reg = ((reg << 1) ^ poly) & mask if reg & top else (reg << 1) & mask
    if refout:
        reg = reflect(reg, width)
    return (reg ^ xorout) & mask


#: RevEng-model catalogue (name -> width, poly, init, refin, refout, xorout), mirroring the
#: Rust `CATALOGUE` in `crates/hk-estimate/src/framing/crc.rs`. Widths 8/16/32 only - the Rust
#: catalogue carries no width-24 entry, so every width-24 check is off-catalogue by construction.
CRC_CATALOGUE: dict[str, tuple[int, int, int, bool, bool, int]] = {
    "CRC-16/CCITT-FALSE": (16, 0x1021, 0xFFFF, False, False, 0),
    "CRC-16/KERMIT": (16, 0x1021, 0x0000, True, True, 0),
    "CRC-16/XMODEM": (16, 0x1021, 0x0000, False, False, 0),
    "CRC-16/ARC": (16, 0x8005, 0x0000, True, True, 0),
    "CRC-16/DNP": (16, 0x3D65, 0x0000, True, True, 0xFFFF),
    "CRC-32/ISO-HDLC": (32, 0x04C1_1DB7, 0xFFFF_FFFF, True, True, 0xFFFF_FFFF),
    "CRC-16/CMS": (16, 0x8005, 0xFFFF, False, False, 0),
    "CRC-16/EN-13757": (16, 0x3D65, 0x0000, False, False, 0xFFFF),
    "CRC-16/IBM-SDLC": (16, 0x1021, 0xFFFF, True, True, 0xFFFF),
    "CRC-16/GENIBUS": (16, 0x1021, 0xFFFF, False, False, 0xFFFF),
    "CRC-16/MCRF4XX": (16, 0x1021, 0xFFFF, True, True, 0),
    "CRC-16/SPI-FUJITSU": (16, 0x1021, 0x1D0F, False, False, 0),
    "CRC-16/MODBUS": (16, 0x8005, 0xFFFF, True, True, 0),
    "CRC-16/UMTS": (16, 0x8005, 0x0000, False, False, 0),
    "CRC-16/MAXIM-DOW": (16, 0x8005, 0x0000, True, True, 0xFFFF),
    "CRC-16/USB": (16, 0x8005, 0xFFFF, True, True, 0xFFFF),
    "CRC-32/BZIP2": (32, 0x04C1_1DB7, 0xFFFF_FFFF, False, False, 0xFFFF_FFFF),
    "CRC-32/MPEG-2": (32, 0x04C1_1DB7, 0xFFFF_FFFF, False, False, 0),
    "CRC-32/ISCSI": (32, 0x1EDC_6F41, 0xFFFF_FFFF, True, True, 0xFFFF_FFFF),
    "CRC-32/JAMCRC": (32, 0x04C1_1DB7, 0xFFFF_FFFF, True, True, 0),
    "CRC-32/CKSUM": (32, 0x04C1_1DB7, 0x0000_0000, False, False, 0xFFFF_FFFF),
    "CRC-8/SMBUS": (8, 0x07, 0x00, False, False, 0),
    "CRC-8/MAXIM-DOW": (8, 0x31, 0x00, True, True, 0),
    "CRC-8/I-432-1": (8, 0x07, 0x00, False, False, 0x55),
    "CRC-8/ROHC": (8, 0x07, 0xFF, True, True, 0),
    "CRC-8/CDMA2000": (8, 0x9B, 0xFF, False, False, 0),
    "CRC-8/DVB-S2": (8, 0xD5, 0x00, False, False, 0),
    "CRC-8/AUTOSAR": (8, 0x2F, 0xFF, False, False, 0xFF),
    "CRC-8/SAE-J1850": (8, 0x1D, 0xFF, False, False, 0xFF),
}

#: Canonical, template-fixed defaults per width (init = all-ones, no reflection, no xorout - the
#: same style as the existing CRC-16/CCITT-FALSE default), used when a scenario asks for a width
#: without naming a polynomial. The width-32 default (Castagnoli) is deliberately not
#: `CRC-32/ISO-HDLC`'s polynomial so picking a width alone never silently lands on a catalogue
#: name; `check_poly_hex` is the explicit, documented way to land off-catalogue on purpose.
CRC_WIDTH_DEFAULT_POLY: dict[int, int] = {
    8: 0x07,
    16: 0x1021,
    24: 0x864C_FB,
    32: 0x1EDC_6F41,
}


def crc_catalogue_name(width: int, poly: int, init: int, refin: bool, refout: bool,
                        xorout: int) -> str | None:
    """The catalogue name matching these exact RevEng parameters, or None (off-catalogue)."""
    key = (width, poly, init, refin, refout, xorout)
    for name, params in CRC_CATALOGUE.items():
        if params == key:
            return name
    return None


def crc_params_for(width: int, poly_hex: str | None) -> dict[str, Any]:
    """Template-fixed RevEng CRC parameters for a scenario: the canonical default polynomial for
    `width` unless `poly_hex` overrides it, init = all-ones, no reflection, no xorout."""
    if width not in (8, 16, 24, 32):
        raise ValueError(f"check_width must be one of 8/16/24/32, got {width}")
    poly = int(poly_hex, 16) if poly_hex is not None else CRC_WIDTH_DEFAULT_POLY[width]
    mask = (1 << width) - 1
    if poly & ~mask:
        raise ValueError(f"check_poly_hex 0x{poly:x} does not fit in {width} bits")
    return {"width": width, "poly": poly, "init": mask, "refin": False, "refout": False,
            "xorout": 0}


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
