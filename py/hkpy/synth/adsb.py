"""Mode S DF17 extended squitter encoding (CRC-24, CPR, identification, velocity) and 1090 MHz PPM."""

from __future__ import annotations

import math
from fractions import Fraction

import numpy as np
from scipy import signal

CRC24_GENERATOR = 0x1FFF409  # x^24 + ... ; the "0xFFF409" Mode S parity polynomial
CRC24_SPEC = {"algorithm": "Mode S CRC-24", "poly": "0xFFF409", "covers": "first 88 bits",
              "parity_field": "PI (last 24 bits); zero remainder over all 112 bits"}
CHARSET = "#ABCDEFGHIJKLMNOPQRSTUVWXYZ##### ###############0123456789######"
#: Preamble in 0.5 µs half-chips: pulses at 0, 1.0, 3.5 and 4.5 µs over 8 µs.
PREAMBLE = (1, 0, 1, 0, 0, 0, 0, 1, 0, 1, 0, 0, 0, 0, 0, 0)
MESSAGE_S = 120e-6  # 8 µs preamble + 112 µs data
NZ = 15


def crc24(data: bytes) -> int:
    crc = 0
    for byte in data:
        crc ^= byte << 16
        for _ in range(8):
            crc <<= 1
            if crc & 0x1000000:
                crc ^= CRC24_GENERATOR
    return crc & 0xFFFFFF


def cpr_nl(lat: float) -> int:
    if lat == 0:
        return 59
    if abs(lat) == 87:
        return 2
    if abs(lat) > 87:
        return 1
    a = 1 - math.cos(math.pi / (2 * NZ))
    b = math.cos(math.pi / 180 * abs(lat)) ** 2
    return int(math.floor(2 * math.pi / math.acos(1 - a / b)))


def cpr_encode(lat: float, lon: float, odd: bool) -> tuple[int, int]:
    """Airborne CPR (17-bit) encoding."""
    i = 1 if odd else 0
    dlat = 360.0 / (4 * NZ - i)
    yz = math.floor(2**17 * ((lat % dlat) / dlat) + 0.5)
    rlat = dlat * (yz / 2**17 + math.floor(lat / dlat))
    dlon = 360.0 / max(cpr_nl(rlat) - i, 1)
    xz = math.floor(2**17 * ((lon % dlon) / dlon) + 0.5)
    return yz & 0x1FFFF, xz & 0x1FFFF


def _pack(fields: list[tuple[int, int]]) -> int:
    v = 0
    for value, width in fields:
        if value < 0 or value >= 1 << width:
            raise ValueError(f"field value {value} does not fit {width} bits")
        v = (v << width) | value
    return v


def me_identification(callsign: str, category: int = 0) -> int:
    cs = callsign.upper().ljust(8)[:8]
    fields = [(4, 5), (category, 3)] + [(CHARSET.index(ch), 6) for ch in cs]
    return _pack(fields)


def encode_altitude(alt_ft: float) -> int:
    n = int(round((alt_ft + 1000) / 25))
    return ((n >> 4) << 5) | 0x10 | (n & 0xF)


def me_airborne_position(alt_ft: float, lat: float, lon: float, odd: bool, tc: int = 11) -> tuple[int, int, int]:
    ylat, xlon = cpr_encode(lat, lon, odd)
    me = _pack([(tc, 5), (0, 2), (0, 1), (encode_altitude(alt_ft), 12), (0, 1), (int(odd), 1),
                (ylat, 17), (xlon, 17)])
    return me, ylat, xlon


def me_velocity(ew_kt: int, ns_kt: int, vrate_fpm: int) -> int:
    vr = min(abs(vrate_fpm) // 64 + 1, 511)
    return _pack([(19, 5), (1, 3), (0, 1), (0, 1), (0, 3),
                  (int(ew_kt < 0), 1), (min(abs(ew_kt) + 1, 1023), 10),
                  (int(ns_kt < 0), 1), (min(abs(ns_kt) + 1, 1023), 10),
                  (0, 1), (int(vrate_fpm < 0), 1), (vr, 9), (0, 2), (0, 1), (0, 7)])


def df17(icao: int, me: int, ca: int = 5) -> bytes:
    head = bytes([(17 << 3) | ca]) + icao.to_bytes(3, "big") + me.to_bytes(7, "big")
    return head + crc24(head).to_bytes(3, "big")


def ppm_envelope(message: bytes, sample_rate: float, margin: int = 32) -> np.ndarray:
    """Real envelope of one squitter at ``sample_rate``, band-limited by resampling from an
    oversampled rectangular-pulse rendering. Sample ``margin`` is the preamble start."""
    half_chip_s = 0.5e-6
    up = next(k for k in range(4, 64) if (Fraction(sample_rate) * k * Fraction(1, 2_000_000)).denominator == 1)
    per_half = int(Fraction(sample_rate) * up * Fraction(1, 2_000_000))
    assert per_half == round(sample_rate * up * half_chip_s)
    bits = np.unpackbits(np.frombuffer(message, dtype=np.uint8))
    chips = np.concatenate([np.array(PREAMBLE, dtype=np.float64),
                            np.stack([bits, 1 - bits], axis=1).reshape(-1).astype(np.float64)])
    hi = np.concatenate([np.zeros(margin * up), np.repeat(chips, per_half), np.zeros(margin * up)])
    return signal.resample_poly(hi, 1, up)
