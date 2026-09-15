"""VHF ACARS (ARINC 618-style) framing: AM carrier with a 2400 Bd MSK-like tone subcarrier,
SYN/SOH..ETX character framing, CRC-16 block check.

**Synthetic, self-consistent, not oracle-validated.** ``acarsdec`` (the usual reference decoder)
needs ``libacars`` plus an SDR input driver and was not a cheap Homebrew install (T-098), so this
module's framing is this project's own best-effort reading of the public ACARS descriptions, not
verified against a third-party decoder. It is internally consistent: ``build_frame`` encodes and an
independent bit-level MSK/CRC decoder in ``py/tests/test_synth.py`` decodes, and the two agree. Do
not treat exact field placement (e.g. the ACK/NAK filler byte) as authoritative for a real receiver
without checking against ``acarsdec`` or a captured message.
"""

from __future__ import annotations

from dataclasses import dataclass

import numpy as np

BAUD = 2400.0
MARK_HZ = 2400.0
SPACE_HZ = 1200.0
SYN = 0x16
SOH = 0x01
STX = 0x02
ETX = 0x03
#: No-acknowledgement-requested filler for the technical ack byte (downlink messages carry no ack).
NAK_FILLER = "\\"
CRC_POLY = 0x1021
FRAMING_NOTE = (
    "32 alternating clock-sync bits, then SYN SYN (0x16 0x16), then SOH (0x01) + mode(1) + "
    "reg(7) + ack(1) + label(2) + block_id(1) + STX (0x02) + text + ETX (0x03), each byte sent "
    "MSB-first as 7 data bits + 1 odd-parity bit; CRC-16 (poly 0x1021, init 0, over the 7-bit "
    "values from SOH..ETX inclusive) sent as two raw (unparitied) bytes, MSB first"
)


def char_with_parity(b7: int) -> int:
    """Sets bit 7 so the byte has odd parity over all 8 bits."""
    b7 &= 0x7F
    ones = bin(b7).count("1")
    return b7 | (0x80 if ones % 2 == 0 else 0x00)


def crc16_acars(data: bytes) -> int:
    """CRC-16, poly 0x1021, init 0, MSB-first, no reflection, no xorout."""
    crc = 0x0000
    for byte in data:
        crc ^= byte << 8
        for _ in range(8):
            crc = ((crc << 1) ^ CRC_POLY) & 0xFFFF if crc & 0x8000 else (crc << 1) & 0xFFFF
    return crc


CRC_SPEC = {
    "algorithm": "CRC-16 (ACARS block check, this project's convention)",
    "poly": f"0x{CRC_POLY:04X}",
    "init": "0x0000",
    "refin": False,
    "refout": False,
    "xorout": "0x0000",
    "covers": "7-bit values (parity stripped) from SOH through ETX inclusive",
}


@dataclass
class Frame:
    mode: str
    reg: str
    label: str
    block_id: str
    text: str
    chars: bytes  # logical body bytes, SOH..ETX, 7-bit values (no parity, no CRC)
    crc: int
    bits: np.ndarray  # full transmitted bitstream: clock-sync preamble + SYN..CRC, MSB-first
    crc_spec: dict


def build_frame(mode: str, reg: str, label: str, block_id: str, text: str) -> Frame:
    mode_b = ord((mode or "2")[0]) & 0x7F
    reg8 = reg.ljust(7)[:7]
    label2 = label.ljust(2)[:2]
    block_b = ord((block_id or "1")[0]) & 0x7F
    body = (
        bytes([SOH, mode_b])
        + reg8.encode("ascii")
        + NAK_FILLER.encode("ascii")
        + label2.encode("ascii")
        + bytes([block_b, STX])
        + text.encode("ascii")
        + bytes([ETX])
    )
    crc = crc16_acars(body)  # body bytes are already 7-bit clean (<=0x7F)
    tx = bytes([SYN, SYN]) + bytes(char_with_parity(b) for b in body) + bytes([(crc >> 8) & 0xFF, crc & 0xFF])
    data_bits = np.unpackbits(np.frombuffer(tx, dtype=np.uint8))  # MSB-first
    presync = np.array([i % 2 for i in range(32)], dtype=np.uint8)
    bits = np.concatenate([presync, data_bits])
    return Frame(mode=chr(mode_b), reg=reg8, label=label2, block_id=chr(block_b), text=text,
                chars=body, crc=crc, bits=bits, crc_spec=CRC_SPEC)


def msk_baseband(bits: np.ndarray, sample_rate: float, *, prekey_s: float = 0.15) -> np.ndarray:
    """Real audio-domain waveform: ``prekey_s`` of silence (unmodulated carrier), then a
    continuous-phase tone at :data:`MARK_HZ` (bit 1) / :data:`SPACE_HZ` (bit 0), amplitude 1.0."""
    n = int(round(len(bits) * sample_rate / BAUD))
    sym = np.minimum((np.arange(n) * BAUD / sample_rate).astype(np.int64), len(bits) - 1)
    freq = np.where(bits[sym] == 1, MARK_HZ, SPACE_HZ)
    phase = 2 * np.pi * np.cumsum(freq) / sample_rate
    tone = np.sin(phase)
    n_prekey = int(round(prekey_s * sample_rate))
    return np.concatenate([np.zeros(n_prekey), tone])
