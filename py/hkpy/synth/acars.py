"""VHF ACARS (ARINC 618) framing: AM carrier with a 2400 Bd MSK audio subcarrier (1800 Hz centre),
pre-key / bit-sync / SYN SYN SOH preamble, 7-bit ASCII + odd parity characters sent LSB first,
CRC-16/KERMIT block check over the transmitted characters.

**Convention source (T-108): acarsdec, not memory.** ``acarsdec`` (github.com/TLeconte/acarsdec,
commit 339f63e) is the reference open-source decoder; its receiver fixes every convention here:

- ``msk.c`` ``putbit`` (lines 53-63) shifts each bit in at the top of an 8-bit register, so the first
  bit on air is a character's LSB and the parity bit (bit 7) is last.
- ``msk.c`` ``demodMSK`` (lines 74-131) is a coherent MSK (offset-QPSK) demodulator: a VCO at
  1800 Hz stepping 3*pi/2 per bit, a half-sine matched filter, and the decision alternating
  Re, Im, -Re, -Im (``MskS``). Its decisions are the chips, and they are the data bits. The audio
  tone therefore follows the chip *transitions*: phase +pi/2 per bit (2400 Hz, mark) when a chip
  equals the previous one, -pi/2 (1200 Hz, space) when it differs. :func:`msk_tones` does this, and
  the wavecom description calls it "NRZI coded coherent MSK". A non-coherent receiver has to
  integrate tones back to chips, with a global polarity ambiguity; acarsdec also accepts ``~SYN``
  (``acars.c`` lines 252-277).
- ``acars.c`` ``decodeAcars`` (lines 246-371): SYN, SYN, SOH (SOH not stored), then characters as
  received (parity bit included) up to ETX ``0x83`` / ETB ``0x97`` (the parity-bearing values,
  lines 22-27), then two block-check bytes.
- ``acars.c`` lines 158-165 plus ``syndrom.h`` lines 15-49: ``crc = 0; update_crc`` over every
  stored character (mode..ETX, parity bits included), then over both BCS bytes, and valid means
  zero. ``update_crc`` is the reflected table form ``(crc >> 8) ^ table[(crc ^ c) & 0xff]`` with
  ``table[1] = 0x1189``: CRC-16/KERMIT (poly 0x1021 reflected, init 0, xorout 0). A zero residue
  means the BCS is sent low byte first.
- Pre-key (16 characters of binary ones), bit sync ``+ *`` and the DEL BCS suffix follow the
  ARINC 618 summary at cartoonman.github.io/WAVECOM/wavecomhtm/acars.htm. acarsdec's SYN search
  slides bit by bit, so it doesn't depend on them.

Still synthetic (no captured message, and acarsdec isn't installed here): field *content*, such as
the ack byte, is illustrative.
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
DEL = 0x7F
#: ARINC 618 pre-key: 16 characters of binary ones.
PREKEY_BITS = 128
#: ARINC 618 bit sync characters.
BIT_SYNC = b"+*"
#: Downlink technical-acknowledgement byte: NAK (acarsdec prints 0x15 as '!').
NAK = 0x15
CRC_POLY = 0x1021
FRAMING_NOTE = (
    "pre-key (128 one bits), '+' '*' SYN SYN SOH, then mode(1) + reg(7) + ack(1) + label(2) + "
    "block_id(1) + STX + text + ETX, each character 7-bit ASCII + odd parity in bit 7, sent LSB "
    "first; CRC-16/KERMIT over the transmitted characters after SOH through ETX (parity bits "
    "included) sent low byte first, then DEL. MSK tones follow chip transitions (mark = no change) "
    "as acarsdec's coherent demodulator implies"
)


def char_with_parity(b7: int) -> int:
    """Sets bit 7 so the byte has odd parity over all 8 bits."""
    b7 &= 0x7F
    ones = bin(b7).count("1")
    return b7 | (0x80 if ones % 2 == 0 else 0x00)


def crc16_kermit(data: bytes) -> int:
    """CRC-16/KERMIT: poly 0x1021 reflected (0x8408), init 0, refin/refout, xorout 0."""
    crc = 0x0000
    for byte in data:
        crc ^= byte
        for _ in range(8):
            crc = (crc >> 1) ^ 0x8408 if crc & 1 else crc >> 1
    return crc


#: Backwards-compatible name: ACARS's block check is CRC-16/KERMIT.
crc16_acars = crc16_kermit

CRC_SPEC = {
    "algorithm": "CRC-16/KERMIT (ARINC 618 block check, as acarsdec checks it)",
    "poly": f"0x{CRC_POLY:04X}",
    "init": "0x0000",
    "refin": True,
    "refout": True,
    "xorout": "0x0000",
    "covers": "transmitted characters after SOH through ETX/ETB inclusive, parity bits included",
    "byte_order": "little-endian (low byte first on air)",
}


@dataclass
class Frame:
    mode: str
    reg: str
    label: str
    block_id: str
    text: str
    chars: bytes  # transmitted characters after SOH through ETX, parity bit set (the CRC span)
    crc: int
    bits: np.ndarray  # chips on air: pre-key + '+*' SYN SYN SOH + chars + BCS + DEL, LSB first
    crc_spec: dict


def lsb_first_bits(data: bytes) -> np.ndarray:
    return np.unpackbits(np.frombuffer(data, dtype=np.uint8), bitorder="little")


def build_frame(mode: str, reg: str, label: str, block_id: str, text: str) -> Frame:
    mode_b = ord((mode or "2")[0]) & 0x7F
    reg8 = reg.ljust(7)[:7]
    label2 = label.ljust(2)[:2]
    block_b = ord((block_id or "1")[0]) & 0x7F
    body = (
        bytes([mode_b])
        + reg8.encode("ascii")
        + bytes([NAK])
        + label2.encode("ascii")
        + bytes([block_b, STX])
        + text.encode("ascii")
        + bytes([ETX])
    )
    chars = bytes(char_with_parity(b) for b in body)
    crc = crc16_kermit(chars)
    head = bytes(char_with_parity(b) for b in BIT_SYNC + bytes([SYN, SYN, SOH]))
    tx = head + chars + bytes([crc & 0xFF, (crc >> 8) & 0xFF, char_with_parity(DEL)])
    bits = np.concatenate([np.ones(PREKEY_BITS, dtype=np.uint8), lsb_first_bits(tx)])
    return Frame(mode=chr(mode_b), reg=reg8, label=label2, block_id=chr(block_b), text=text,
                 chars=chars, crc=crc, bits=bits, crc_spec=CRC_SPEC)


def msk_tones(chips: np.ndarray) -> np.ndarray:
    """Tone per bit (1 = mark 2400 Hz, 0 = space 1200 Hz): mark when a chip equals the previous
    one (the chip before the first is taken as 1, the pre-key value)."""
    c = np.asarray(chips, dtype=np.uint8) & 1
    prev = np.concatenate([[1], c[:-1]]).astype(np.uint8)
    return (c == prev).astype(np.uint8)


def msk_baseband(chips: np.ndarray, sample_rate: float, *, prekey_s: float = 0.15) -> np.ndarray:
    """Real audio-domain waveform: ``prekey_s`` of silence (unmodulated carrier), then continuous-
    phase MSK: tone :data:`MARK_HZ` / :data:`SPACE_HZ` per :func:`msk_tones`, amplitude 1.0."""
    tones = msk_tones(chips)
    n = int(round(len(tones) * sample_rate / BAUD))
    sym = np.minimum((np.arange(n) * BAUD / sample_rate).astype(np.int64), len(tones) - 1)
    freq = np.where(tones[sym] == 1, MARK_HZ, SPACE_HZ)
    phase = 2 * np.pi * np.cumsum(freq) / sample_rate
    tone = np.sin(phase)
    n_prekey = int(round(prekey_s * sample_rate))
    return np.concatenate([np.zeros(n_prekey), tone])
