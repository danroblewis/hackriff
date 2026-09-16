"""LoRa CSS (chirp spread spectrum) modulation and a LoRa-shaped bit chain (T-255).

**Why this module exists.** CLAUDE.md invariant 1 says a signal is a time-frequency region with a
time extent, and that it "needs no carrier and no stable frequency". A LoRa up-chirp is the
canonical case: its instantaneous frequency sweeps the whole channel every symbol, so there is no
stable frequency to find, by construction. Everything else in ``hkpy.synth`` sits on a carrier.

**What is exact here, and what is not.** Following the precedent set by :mod:`hkpy.synth.trunking`
(T-267), the honest split is stated up front:

- **The waveform is real LoRa CSS and is exact.** ``2**SF`` chips per symbol, symbol duration
  ``2**SF / BW``, chirp rate ``BW**2 / 2**SF`` Hz/s, a symbol of value ``s`` being the base
  up-chirp cyclically shifted to start at ``(s / 2**SF - 1/2) * BW``, continuous phase throughout,
  a preamble of up-chirps, two sync-word symbols and a 2.25-symbol down-chirp SFD. This is the part
  a chirp detector sees, and :func:`demodulate_symbols` recovers the transmitted symbols from it by
  the standard dechirp-and-FFT method, written independently of the modulator.
- **The bit layer is LoRa-*shaped*, not a bit-exact SX127x reproduction.** The chain is
  payload -> CRC-16 -> nibbles -> Hamming(4, 4+CR) -> diagonal interleave -> Gray -> symbols, which
  is the real chain's shape and makes the coding rate a parameter that genuinely changes the
  encoding and the symbol count. The *conventions* inside it (parity-bit order, interleave
  direction, Gray polarity) are defined here and are invertible; they are not claimed to match a
  Semtech radio, and no whitening, explicit header or LoRaWAN MAC layer is implemented. Nothing
  here should be read as a standards-compliant LoRa encoder.

The Hamming code is not merely "defined": :func:`hamming_encode` at CR 3 and 4 is a genuine (7,4)
Hamming and (8,4) SECDED code respectively, which ``py/tests/test_synth.py`` checks as a minimum-
distance property rather than taking it on trust.
"""

from __future__ import annotations

import math
from typing import Any

import numpy as np

#: Spreading factors LoRa defines. SF 6 is an implicit-header special case and is excluded.
SF_RANGE = (7, 12)
#: Coding-rate index ``cr``: the code is 4/(4+cr), so cr 1..4 means 4/5..4/8.
CR_RANGE = (1, 4)
#: LoRaWAN's public sync word. Two symbols, ``(sw >> 4) << 3`` and ``(sw & 0xF) << 3``.
PUBLIC_SYNC_WORD = 0x34
#: Up-chirps in a standard preamble.
DEFAULT_PREAMBLE_SYMBOLS = 8
#: Down-chirp symbols in the start-of-frame delimiter (2 full + a quarter).
SFD_SYMBOLS = 2.25

#: The payload CRC. LoRa uses the CCITT polynomial with a **zero** seed, unlike CRC-16/CCITT-FALSE.
CRC16_SPEC: dict[str, Any] = {
    "algorithm": "CRC-16/XMODEM",
    "poly": "0x1021",
    "init": "0x0000",
    "refin": False,
    "refout": False,
    "xorout": "0x0000",
    "check_123456789": "0x31C3",
    "covers": "payload bytes",
}

#: How the bit layer is built, recorded in truth so a reader never has to guess the conventions.
CODING_SPEC: dict[str, Any] = {
    "chain": "payload -> CRC-16 -> MSB-first nibbles -> Hamming(4, 4+cr) -> diagonal interleave "
             "-> Gray -> symbol values",
    "hamming_parity": {
        "4/5": ["d0^d1^d2^d3"],
        "4/6": ["d0^d1^d2^d3", "d0^d1^d3"],
        "4/7": ["d0^d2^d3", "d0^d1^d3", "d1^d2^d3"],
        "4/8": ["d0^d2^d3", "d0^d1^d3", "d1^d2^d3", "d0^d1^d2"],
        "order": "codeword = d3 d2 d1 d0 then the parities above, MSB first",
        "distance": {"4/5": 2, "4/6": 2, "4/7": 3, "4/8": 4},
        "note": "the parity set depends on the rate, as LoRa's does: 4/5 is a single overall "
                "parity (detection only), 4/7 a true (7,4) Hamming, 4/8 its SECDED extension",
    },
    "interleave": "block of sf codewords of (4+cr) bits -> (4+cr) symbols of sf bits; "
                  "symbol_bit[i][j] = codeword_bit[(i+j) % sf][(4+cr)-1-i]",
    "gray": "symbol = w ^ (w >> 1)",
    "standards_note": "LoRa-shaped, not bit-exact SX127x: no whitening, no explicit header, and "
                      "the parity/interleave/Gray conventions are this module's own",
}


# ---- geometry ---------------------------------------------------------------------------------


def n_chips(sf: int) -> int:
    """Chips (and distinct symbol values) per symbol: ``2**SF``."""
    return 1 << int(sf)


def symbol_duration_s(sf: int, bandwidth_hz: float) -> float:
    """``2**SF / BW`` seconds."""
    return n_chips(sf) / float(bandwidth_hz)


def chirp_rate_hz_per_s(sf: int, bandwidth_hz: float) -> float:
    """Sweep slope ``BW / T_sym = BW**2 / 2**SF``, Hz per second."""
    return float(bandwidth_hz) ** 2 / n_chips(sf)


def instantaneous_bandwidth_hz(sf: int, bandwidth_hz: float, frame_s: float) -> float:
    """How much of the channel one chirp actually occupies within an analysis frame of ``frame_s``.

    This is the number that makes ADR-0017 §1.3's bounding-box limitation concrete: a chirp's
    detection box is ``bandwidth_hz`` wide, but at any instant the emission is only this wide.
    """
    return min(float(bandwidth_hz), chirp_rate_hz_per_s(sf, bandwidth_hz) * float(frame_s))


def symbol_count(payload_bytes: int, sf: int, cr: int) -> int:
    """Payload symbols (excluding preamble, sync and SFD) for a payload of that many bytes."""
    nibbles = 2 * (payload_bytes + 2)  # + the 2 CRC bytes
    blocks = math.ceil(nibbles / sf)
    return blocks * (4 + cr)


# ---- the bit layer ----------------------------------------------------------------------------


def crc16_xmodem(data: bytes) -> int:
    """CRC-16 with poly 0x1021 and a zero seed (see :data:`CRC16_SPEC`)."""
    crc = 0x0000
    for byte in data:
        crc ^= byte << 8
        for _ in range(8):
            crc = ((crc << 1) ^ 0x1021) & 0xFFFF if crc & 0x8000 else (crc << 1) & 0xFFFF
    return crc


def hamming_encode(nibble: int, cr: int) -> int:
    """``nibble`` (0..15) as a ``4 + cr`` bit codeword, data bits first, MSB first.

    The parity set depends on the rate, as LoRa's does. At cr 1 it is a single overall parity, so
    the code detects one error and corrects none; at cr 3 the four data columns and three parity
    columns are seven distinct non-zero syndromes, so it is a true (7,4) Hamming code; at cr 4 the
    minimum weight is 4, so it is SECDED. ``py/tests/test_synth.py`` measures all four minimum
    distances rather than taking any of this on trust.
    """
    d0, d1, d2, d3 = (nibble >> 0) & 1, (nibble >> 1) & 1, (nibble >> 2) & 1, (nibble >> 3) & 1
    if int(cr) == 1:
        parity = [d0 ^ d1 ^ d2 ^ d3]
    elif int(cr) == 2:
        parity = [d0 ^ d1 ^ d2 ^ d3, d0 ^ d1 ^ d3]
    else:
        parity = [d0 ^ d2 ^ d3, d0 ^ d1 ^ d3, d1 ^ d2 ^ d3]
        if int(cr) == 4:
            parity.append(d0 ^ d1 ^ d2)
    word = (d3 << 3) | (d2 << 2) | (d1 << 1) | d0
    for p in parity:
        word = (word << 1) | p
    return word


def gray(value: int) -> int:
    """Binary to Gray."""
    return value ^ (value >> 1)


def ungray(value: int, sf: int) -> int:
    """Gray to binary, over ``sf`` bits."""
    out = value
    shift = 1
    while shift < sf:
        out ^= out >> shift
        shift <<= 1
    return out & (n_chips(sf) - 1)


def _nibbles(data: bytes) -> list[int]:
    out: list[int] = []
    for b in data:
        out.append((b >> 4) & 0xF)
        out.append(b & 0xF)
    return out


def encode(payload: bytes, sf: int, cr: int) -> tuple[np.ndarray, dict[str, Any]]:
    """Encodes ``payload`` to LoRa symbol values. Returns ``(symbols, detail)``.

    ``detail`` records every intermediate stage so the truth annotation can carry the hidden
    parameters without the pipeline ever seeing them.
    """
    if not SF_RANGE[0] <= sf <= SF_RANGE[1]:
        raise ValueError(f"sf must be {SF_RANGE[0]}..{SF_RANGE[1]}, got {sf}")
    if not CR_RANGE[0] <= cr <= CR_RANGE[1]:
        raise ValueError(f"coding_rate must be {CR_RANGE[0]}..{CR_RANGE[1]} (4/5..4/8), got {cr}")
    crc = crc16_xmodem(payload)
    framed = payload + crc.to_bytes(2, "big")
    nibbles = _nibbles(framed)
    pad = (-len(nibbles)) % sf
    nibbles += [0] * pad
    cw_bits = 4 + cr
    symbols: list[int] = []
    words: list[int] = []
    for base in range(0, len(nibbles), sf):
        block = [hamming_encode(n, cr) for n in nibbles[base : base + sf]]
        for i in range(cw_bits):
            word = 0
            for j in range(sf):
                bit = (block[(i + j) % sf] >> (cw_bits - 1 - i)) & 1
                word = (word << 1) | bit
            words.append(word)
            symbols.append(gray(word))
    detail = {
        "payload_hex": payload.hex(),
        "payload_bytes": len(payload),
        "crc_hex": f"{crc:04x}",
        "crc": {**CRC16_SPEC, "value": f"0x{crc:04X}", "valid": True},
        "n_nibbles": len(nibbles),
        "pad_nibbles": pad,
        "n_blocks": len(nibbles) // sf,
        "codeword_bits": cw_bits,
        "interleaved_words": words[:16],
        "n_symbols": len(symbols),
        "coding": CODING_SPEC,
    }
    return np.array(symbols, dtype=np.int64), detail


def decode(symbols: np.ndarray, sf: int, cr: int, payload_bytes: int) -> tuple[bytes, int, bool]:
    """Inverts :func:`encode`. Returns ``(payload, crc, crc_valid)``.

    Kept here beside the encoder because the *test* writes its own demodulator (the hard,
    convention-free half) and only needs the bit chain undone; a second copy of the interleave
    conventions in the test file would test nothing but copy-paste.
    """
    cw_bits = 4 + cr
    nibbles: list[int] = []
    for base in range(0, len(symbols) - cw_bits + 1, cw_bits):
        block = [ungray(int(s), sf) for s in symbols[base : base + cw_bits]]
        cws = [0] * sf
        for i in range(cw_bits):
            for j in range(sf):
                bit = (block[i] >> (sf - 1 - j)) & 1
                cws[(i + j) % sf] |= bit << (cw_bits - 1 - i)
        nibbles.extend((c >> cr) & 0xF for c in cws)
    data = bytearray()
    for i in range(0, len(nibbles) - 1, 2):
        data.append((nibbles[i] << 4) | nibbles[i + 1])
    payload = bytes(data[:payload_bytes])
    crc = int.from_bytes(bytes(data[payload_bytes : payload_bytes + 2]), "big")
    return payload, crc, crc == crc16_xmodem(payload)


# ---- the waveform -----------------------------------------------------------------------------


def _sweep(symbol: int, sf: int, bandwidth_hz: float, sample_rate: float, *,
           down: bool = False, fraction: float = 1.0) -> np.ndarray:
    """Instantaneous frequency, Hz relative to the channel centre, of one (part-)symbol."""
    n_total = n_chips(sf)
    n = int(round(fraction * symbol_duration_s(sf, bandwidth_hz) * sample_rate))
    t = np.arange(n) / sample_rate
    u = np.mod(symbol / n_total + t * bandwidth_hz / n_total, 1.0)
    f = (u - 0.5) * bandwidth_hz
    return -f if down else f


def packet_frequency(symbols: np.ndarray, sf: int, bandwidth_hz: float, sample_rate: float, *,
                     preamble_symbols: int = DEFAULT_PREAMBLE_SYMBOLS,
                     sync_word: int = PUBLIC_SYNC_WORD) -> np.ndarray:
    """The whole packet's instantaneous frequency: preamble, sync word, SFD, then the payload."""
    s1 = ((int(sync_word) >> 4) & 0xF) << 3
    s2 = (int(sync_word) & 0xF) << 3
    parts = [_sweep(0, sf, bandwidth_hz, sample_rate) for _ in range(int(preamble_symbols))]
    parts += [_sweep(s1, sf, bandwidth_hz, sample_rate), _sweep(s2, sf, bandwidth_hz, sample_rate)]
    parts += [_sweep(0, sf, bandwidth_hz, sample_rate, down=True) for _ in range(2)]
    parts.append(_sweep(0, sf, bandwidth_hz, sample_rate, down=True, fraction=0.25))
    parts += [_sweep(int(s), sf, bandwidth_hz, sample_rate) for s in symbols]
    return np.concatenate(parts)


def modulate(symbols: np.ndarray, sf: int, bandwidth_hz: float, sample_rate: float, *,
             preamble_symbols: int = DEFAULT_PREAMBLE_SYMBOLS,
             sync_word: int = PUBLIC_SYNC_WORD, phase0: float = 0.0) -> np.ndarray:
    """Unit-amplitude, continuous-phase LoRa CSS for one packet at baseband.

    Phase is the running integral of :func:`packet_frequency`, so it is continuous everywhere —
    including at the chirp's fold, where the *frequency* jumps by a full ``bandwidth_hz`` and the
    phase does not. That fold is what makes a chirp a chirp, and what a carrier-tracking estimator
    has nothing to lock to.
    """
    f = packet_frequency(symbols, sf, bandwidth_hz, sample_rate,
                         preamble_symbols=preamble_symbols, sync_word=sync_word)
    return np.exp(1j * (phase0 + 2 * math.pi * np.cumsum(f) / sample_rate))


def demodulate_symbols(x: np.ndarray, sf: int, bandwidth_hz: float, sample_rate: float,
                       n_symbols: int, *, start: int = 0) -> np.ndarray:
    """Standard dechirp-and-FFT LoRa demodulator, from ``start`` for ``n_symbols`` symbols.

    Written from the method, not from :func:`modulate`: multiply by the conjugate base up-chirp,
    take the FFT over the symbol, and fold it modulo ``2**SF`` (the oversampled aliases of the
    dechirped tone land on the same residue). The peak bin is the symbol value. This is the
    independent check that the waveform really is LoRa CSS.
    """
    total = n_chips(sf)
    sps = int(round(symbol_duration_s(sf, bandwidth_hz) * sample_rate))
    base = np.exp(-1j * 2 * math.pi * np.cumsum(_sweep(0, sf, bandwidth_hz, sample_rate))
                  / sample_rate)
    out = np.empty(n_symbols, dtype=np.int64)
    for k in range(n_symbols):
        i = start + k * sps
        seg = x[i : i + sps] * base[: sps]
        spec = np.abs(np.fft.fft(seg, sps)) ** 2
        folded = spec[: (len(spec) // total) * total].reshape(-1, total).sum(axis=0)
        out[k] = int(np.argmax(folded))
    return out
