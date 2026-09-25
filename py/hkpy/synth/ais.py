"""AIS (ITU-R M.1371) framing: the common navigation block for Class A position reports
(message types 1/2/3), standard HDLC bit-oriented framing (0x7E flags, zero-bit destuffing) and
FCS = CRC-16/X-25 (a.k.a. CRC-16/IBM-SDLC, already in :data:`hkpy.synth.fsk.CRC_CATALOGUE`).

Field layout (168 bits, MSB first, matching ``recipes/ais.recipe.json``'s ``ais_position_report``
field map exactly, cross-checked against ITU-R M.1371-5 table 45/table 76): message type (6),
repeat indicator (2), MMSI (30), navigational status (4), rate of turn (8, signed), speed over
ground (10, x0.1 kt), position accuracy (1), longitude (28, signed, x1/600000 deg), latitude (27,
signed, x1/600000 deg), course over ground (12, x0.1 deg), true heading (9), UTC second (6),
manoeuvre indicator (2), spare (3), RAIM (1), communication state (19).

The on-air bit chain (mirroring ``crates/hk-blocks/src/blocks/symbol/{bitstuff,line}.rs`` and the
recipe's ``sync``/``crc`` nodes) is: payload || FCS -> HDLC zero-bit stuff -> flag || stuffed ||
flag -> NRZI encode (0 = transition) -> GMSK (h = 0.5, BT = 0.4) at 9600 Bd.
"""

from __future__ import annotations

import numpy as np

from hkpy.synth import fsk

#: The common navigation block, bits.
POSITION_REPORT_BITS = 168
#: HDLC flag octet.
FLAG = 0x7E
#: 0 stuffed after this many consecutive ones (ISO/IEC 13239 section 4.4.2).
STUFF_AFTER = 5


def _field(out: list[int], value: int, width: int) -> None:
    """Appends `value`'s low `width` bits, MSB first (two's-complement wrap for negatives)."""
    v = value & ((1 << width) - 1)
    out.extend((v >> k) & 1 for k in range(width - 1, -1, -1))


def build_position_report(
    mmsi: int,
    *,
    msg_type: int = 1,
    repeat: int = 0,
    nav_status: int = 0,
    rot: int = -128,  # -128: not available (ITU-R M.1371 table 46)
    sog_kt: float = 0.0,
    accuracy: int = 0,
    lon_deg: float = 181.0,  # 181: not available
    lat_deg: float = 91.0,  # 91: not available
    cog_deg: float = 360.0,  # 360: not available (0.1 deg units, 3600 raw)
    heading_deg: int = 511,  # 511: not available
    timestamp_s: int = 60,  # 60: not available
    maneuver: int = 0,
    raim: int = 0,
    comm_state: int = 0,
) -> np.ndarray:
    """The 168-bit (uint8, MSB first) common navigation block for message types 1/2/3."""
    if msg_type not in (1, 2, 3):
        raise ValueError("msg_type must be 1, 2 or 3 (Class A position report)")
    bits: list[int] = []
    _field(bits, msg_type, 6)
    _field(bits, repeat, 2)
    _field(bits, mmsi, 30)
    _field(bits, nav_status, 4)
    _field(bits, rot, 8)
    _field(bits, round(sog_kt / 0.1), 10)
    _field(bits, accuracy, 1)
    _field(bits, round(lon_deg / (1.0 / 600000.0)), 28)
    _field(bits, round(lat_deg / (1.0 / 600000.0)), 27)
    _field(bits, round(cog_deg / 0.1), 12)
    _field(bits, heading_deg, 9)
    _field(bits, timestamp_s, 6)
    _field(bits, maneuver, 2)
    _field(bits, 0, 3)  # spare
    _field(bits, raim, 1)
    _field(bits, comm_state, 19)
    assert len(bits) == POSITION_REPORT_BITS, len(bits)
    return np.array(bits, dtype=np.uint8)


def hdlc_stuff(bits: np.ndarray) -> np.ndarray:
    """Bit-by-bit HDLC zero insertion: a 0 after every `STUFF_AFTER` consecutive 1s."""
    out: list[int] = []
    ones = 0
    for b in bits:
        b = int(b)
        out.append(b)
        if b == 1:
            ones += 1
            if ones == STUFF_AFTER:
                out.append(0)
                ones = 0
        else:
            ones = 0
    return np.array(out, dtype=np.uint8)


def fcs(payload_bits: np.ndarray) -> np.ndarray:
    """The 16-bit CRC-16/X-25 FCS over `payload_bits` (a whole number of bytes): standard HDLC
    transmits a reflected CRC's low-order octet first (ISO/IEC 13239 section 4.3), so this
    returns the low byte's bits (MSB first within it), then the high byte's."""
    if len(payload_bits) % 8 != 0:
        raise ValueError("payload_bits must be a whole number of bytes")
    data = np.packbits(payload_bits).tobytes()
    crc = fsk.crc_generic(data, *fsk.CRC_CATALOGUE["CRC-16/IBM-SDLC"])
    lo, hi = crc & 0xFF, (crc >> 8) & 0xFF
    bits = [(lo >> k) & 1 for k in range(7, -1, -1)] + [(hi >> k) & 1 for k in range(7, -1, -1)]
    return np.array(bits, dtype=np.uint8)


def frame_bits(payload_bits: np.ndarray) -> np.ndarray:
    """One flag-delimited, zero-stuffed HDLC frame: flag || stuff(payload || FCS) || flag."""
    flag = np.array([(FLAG >> k) & 1 for k in range(7, -1, -1)], dtype=np.uint8)
    on_frame = np.concatenate([payload_bits, fcs(payload_bits)])
    return np.concatenate([flag, hdlc_stuff(on_frame), flag])


def nrzi_encode(bits: np.ndarray) -> np.ndarray:
    """NRZI line coding: 0 = transition (ISO/IEC 13239 section 4.4.1, the `nrzi` block default).
    The first output level is arbitrary (1); a receiver locks polarity from the flag pattern."""
    level = 1
    out = np.empty(len(bits), dtype=np.uint8)
    for i, b in enumerate(bits):
        if int(b) == 0:
            level ^= 1
        out[i] = level
    return out


def modulate(
    line_bits: np.ndarray,
    sample_rate: float,
    *,
    symbol_rate_bd: float = 9600.0,
    deviation_hz: float | None = None,
    bt: float = 0.4,
    phase0: float = 0.0,
) -> np.ndarray:
    """GMSK (h = 0.5, `deviation_hz` default `symbol_rate_bd / 4`) at complex baseband."""
    dev = deviation_hz if deviation_hz is not None else symbol_rate_bd / 4.0
    return fsk.cpfsk(line_bits, sample_rate, symbol_rate_bd, dev, bt=bt, phase0=phase0)


def burst_iq(
    payload_bits: np.ndarray,
    sample_rate: float,
    *,
    symbol_rate_bd: float = 9600.0,
    bt: float = 0.4,
    preamble_bits: int = 24,
    phase0: float = 0.0,
) -> np.ndarray:
    """One AIS transmission burst at complex baseband: an alternating-bit training sequence
    (ramp-up, ITU-R M.1371 section 3.3.4's 24-bit example) then the flag-delimited HDLC frame,
    GMSK modulated. No power ramp (the mock SDR chain does not model transmitter ramp shape)."""
    training = np.array([i % 2 for i in range(preamble_bits)], dtype=np.uint8)
    line = nrzi_encode(np.concatenate([training, frame_bits(payload_bits)]))
    return modulate(line, sample_rate, symbol_rate_bd=symbol_rate_bd, bt=bt, phase0=phase0)


def burst_duration_s(payload_bits: int, symbol_rate_bd: float = 9600.0, preamble_bits: int = 24) -> float:
    """An upper bound on a burst's air time (stuffing can only add bits), for scene layout."""
    n = preamble_bits + 8 + payload_bits + 16 + 8
    n += n // STUFF_AFTER + 1  # worst-case stuffing overhead
    return n / symbol_rate_bd
