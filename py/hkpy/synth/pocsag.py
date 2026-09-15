"""POCSAG (British Telecom POCSAG / ITU-R M.584, "Radio paging code no. 1") framing:
BCH(31,21)+even-parity codewords, numeric and alphanumeric message packing.

Constants and packing rules are cross-checked against the public-domain ``gen_pocsag.c`` /
``bch.c`` reference generator and decoder bundled with `multimon-ng
<https://github.com/EliasOenal/multimon-ng>`_ (Unlicense), which this project uses as the POCSAG
oracle (T-098). The BCH generator polynomial 0x769 round-trips the standard sync codeword
``0x7CD215D8`` and idle codeword ``0x7A89C197`` exactly (see ``py/tests/test_synth.py``).
"""

from __future__ import annotations

import numpy as np

#: BCH(31,21) generator polynomial (systematic, MSB-first division), from ITU-R M.584 / multimon-ng.
BCH_POLY = 0x769
#: Standard 32-bit sync codeword that opens every batch.
SYNC_CODEWORD = 0x7CD215D8
#: Standard 32-bit idle/filler codeword (a valid BCH codeword with data field 0b1_0101_0001_0011_1000).
IDLE_CODEWORD = 0x7A89C197
PREAMBLE_BITS = 576
CODEWORDS_PER_BATCH = 16
FRAMES_PER_BATCH = 8
#: Numeric nibble -> character (POCSAG "digit" charset); ``char_to_bcd`` below is its inverse.
NUMERIC_CHARSET = "084 2.6]195-3U7["
BCH_SPEC = {
    "algorithm": "BCH(31,21) + even parity",
    "generator_poly": f"0x{BCH_POLY:03X}",
    "codeword_bits": 32,
    "layout": "bit31 msg-flag, bits30-11 data(20), bits10-1 BCH parity(10), bit0 even parity",
    "corrects": "up to 2 bit errors (multimon-ng default -b 2)",
}


def _parity32(x: int) -> int:
    p = 0
    while x:
        p ^= x & 1
        x >>= 1
    return p


def bch_parity(data21: int) -> int:
    """10-bit BCH parity of a 21-bit data field, by polynomial division with :data:`BCH_POLY`."""
    reg = (data21 & 0x1FFFFF) << 10
    for i in range(20, -1, -1):
        if reg & (1 << (i + 10)):
            reg ^= BCH_POLY << i
    return reg & 0x3FF


def bch_encode(data21: int) -> int:
    """32-bit POCSAG codeword: 21-bit data, 10-bit BCH parity, 1 even-parity bit."""
    d = data21 & 0x1FFFFF
    codeword = (d << 11) | (bch_parity(d) << 1)
    return codeword | _parity32(codeword)


def address_codeword(address: int, function: int) -> int:
    """Address codeword. ``address``'s low 3 bits select the frame (0-7); only the top 18 bits and
    the 2-bit ``function`` are carried in the data field (bit20=0 marks it an address codeword)."""
    if not 0 <= address <= 0x1FFFFF:
        raise ValueError(f"POCSAG address must be 0..2097151 (21 bits), got {address}")
    data = ((address >> 3) << 2) | (function & 3)
    return bch_encode(data)


def message_codeword(data20: int) -> int:
    """Message codeword (bit20=1 marks it a message codeword, carrying 20 bits of payload)."""
    return bch_encode((1 << 20) | (data20 & 0xFFFFF))


def char_to_bcd(c: str) -> int:
    try:
        return NUMERIC_CHARSET.index(c)
    except ValueError:
        return NUMERIC_CHARSET.index(" ")


def encode_numeric(text: str) -> list[int]:
    """5 BCD digits (space-padded) packed per 20-bit message codeword."""
    codewords = []
    for i in range(0, len(text), 5):
        chunk = text[i : i + 5].ljust(5)
        data = 0
        for c in chunk:
            data = (data << 4) | char_to_bcd(c)
        codewords.append(message_codeword(data))
    return codewords or [message_codeword(0x33333)]  # empty message: five spaces


def _rev7(b: int) -> int:
    r = 0
    for i in range(7):
        if b & (1 << i):
            r |= 1 << (6 - i)
    return r


def encode_alpha(text: str) -> list[int]:
    """Bit-reversed 7-bit ASCII, packed MSB-first, 5 nibbles (20 bits) per message codeword."""
    bits = np.zeros(((len(text) * 7 + 19) // 20) * 20 + 20, dtype=np.uint8)  # generous, trimmed below
    pos = 0
    for ch in text:
        v = _rev7(ord(ch) & 0x7F)
        for b in range(6, -1, -1):
            bits[pos] = (v >> b) & 1
            pos += 1
    bits = bits[:pos]
    pad = (-len(bits)) % 20
    bits = np.concatenate([bits, np.zeros(pad, dtype=np.uint8)])
    codewords = []
    for i in range(0, len(bits), 20):
        data = 0
        for b in bits[i : i + 20]:
            data = (data << 1) | int(b)
        codewords.append(message_codeword(data))
    return codewords or [message_codeword(0)]


def build_batches(address: int, function: int, message_codewords: list[int]) -> list[int]:
    """Preamble (576 alternating bits, as 18 codeword-sized filler words is *not* used here -- see
    :func:`build_bits`) plus enough batches to carry the address codeword and the message, padded
    with :data:`IDLE_CODEWORD`. Mirrors multimon-ng's ``gen_pocsag.c``."""
    frame_position = address & 7
    slots_needed = 1 + len(message_codewords) + 1  # address + message + >=1 idle trailer
    slots_in_first_batch = CODEWORDS_PER_BATCH - frame_position * 2
    if slots_needed <= slots_in_first_batch:
        batch_count = 1
    else:
        batch_count = 1 + -(-(slots_needed - slots_in_first_batch) // CODEWORDS_PER_BATCH)

    out: list[int] = []
    addr_cw = address_codeword(address, function)
    address_sent = False
    msg_idx = 0
    for _ in range(batch_count):
        out.append(SYNC_CODEWORD)
        for frame in range(FRAMES_PER_BATCH):
            for cw in range(2):
                if not address_sent and frame == frame_position and cw == 0:
                    out.append(addr_cw)
                    address_sent = True
                elif address_sent and msg_idx < len(message_codewords):
                    out.append(message_codewords[msg_idx])
                    msg_idx += 1
                else:
                    out.append(IDLE_CODEWORD)
    return out


def build_bits(address: int, function: int, message_codewords: list[int]) -> np.ndarray:
    """Full transmitted bitstream: 576-bit alternating preamble, then sync+batch codewords MSB-first."""
    preamble = np.array([1 - (i & 1) for i in range(PREAMBLE_BITS)], dtype=np.uint8)
    words = build_batches(address, function, message_codewords)
    body = np.concatenate([[(w >> b) & 1 for b in range(31, -1, -1)] for w in words]).astype(np.uint8)
    return np.concatenate([preamble, body])
