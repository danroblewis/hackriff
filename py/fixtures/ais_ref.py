"""Independent reference AIS (ITU-R M.1371) decoder for fixture ground truth (T-963,
SIGNAL-015), in the style of ``rds_ref.py``/``flex_ref.py``: written from scratch against the
public spec, not against ``crates/hk-blocks``, and validated on the synthetic ``ais_vessels``
scene (``hkpy.synth.ais_scene``) in ``py/tests/test_fixture_tooling.py``. Filed because the
explorer (2026-09-25, AIS 161.975/162.025 MHz) had no validated AIS oracle to cross-check a
capture against — see ``~/.hackriff-ops/explorer/journal-20260925.md``, section 'AIS'.

Chain: burst detection (envelope threshold) -> GMSK FM discriminator -> best-phase symbol slicing
at 9600 Bd -> NRZI decode (0 = transition; a *global* level inversion cancels in this step, so
there is no discriminator-polarity ambiguity to resolve) -> HDLC zero-bit destuffing -> every span
strictly between two ``0x7E`` flags -> keep the ones whose trailing 16 bits are a valid
CRC-16/X-25 (a.k.a. CRC-16/IBM-SDLC) FCS over the rest, read as ISO/IEC 13239 section 4.3
transmits a reflected CRC: low-order octet first -> the common navigation block (message type,
repeat indicator, MMSI, and the Class A position-report fields for types 1/2/3).

Only decoded fields leave this module (never raw IQ or bit dumps) — same discipline as
``fsk_ref.py``.
"""

from __future__ import annotations

import numpy as np
from scipy import ndimage, signal

#: HDLC flag octet.
FLAG = 0x7E
SYMBOL_RATE_BD = 9600.0


def fm_discriminate(x: np.ndarray, fs: float) -> np.ndarray:
    return np.angle(x[1:] * np.conj(x[:-1])) * fs / (2 * np.pi)


def _lowpass(x: np.ndarray, fs: float, cutoff: float) -> np.ndarray:
    cutoff = min(cutoff, 0.45 * fs)
    ntaps = int(min(513, max(31, 6 * fs / cutoff))) | 1
    if len(x) <= 3 * ntaps:
        return x
    h = signal.firwin(ntaps, cutoff, fs=fs)
    return signal.filtfilt(h, 1.0, x)


def _moving_avg(v: np.ndarray, length: int) -> np.ndarray:
    length = max(1, int(length))
    return v if length == 1 else np.convolve(v, np.ones(length) / length, mode="same")


def find_bursts(x: np.ndarray, fs: float, min_len_s: float = 0.015,
                guard_s: float = 0.001) -> list[tuple[int, int]]:
    """Contiguous (start, stop) sample ranges whose envelope is well above the median (a robust
    stand-in for a noise floor), at least `min_len_s` long — one AIS burst is typically ~21 ms."""
    env = np.abs(x)
    if len(env) < 8:
        return []
    med = np.median(env)
    mad = np.median(np.abs(env - med)) + 1e-12
    on = env > med + 6 * mad
    k = max(1, int(round(guard_s * fs)))
    on = ndimage.binary_closing(on, structure=np.ones(2 * k + 1, dtype=bool))
    lab, _ = ndimage.label(on)
    out = []
    for sl in ndimage.find_objects(lab):
        (s,) = sl
        if s.stop - s.start >= int(min_len_s * fs):
            out.append((int(s.start), int(s.stop)))
    return out


#: Every AIS burst opens with an alternating-bit training sequence (ITU-R M.1371-5 section
#: 3.3.4); NRZI-encoded (0 = transition), that is a period-4 square wave, level held for 2
#: symbols each time: -1,-1,+1,+1,-1,-1,+1,+1,... Correlating against it is both a realistic
#: synchronisation method (real receivers use the same training sequence) and, empirically, far
#: more reliable than picking the phase with the widest spread of values: that criterion chose a
#: phase 3+ samples off the true one on a real destuffed payload, corrupting the frame (T-963).
_TRAINING_SYMBOLS = 16
_TRAINING_PATTERN = np.where(np.arange(_TRAINING_SYMBOLS) % 4 < 2, -1.0, 1.0)


def slice_symbols(disc: np.ndarray, fs: float,
                  symbol_rate_bd: float = SYMBOL_RATE_BD) -> np.ndarray | None:
    """Symbol-sliced levels of a discriminator trace, starting at the training sequence: a 2-D
    search over the sub-symbol sampling phase *and* which symbol the training pattern starts at
    (both unknown — the caller's burst window carries margin of unknown length, e.g. to let a
    channel filter settle before the burst's own first sample), maximising correlation with the
    known training pattern. Returns the slice from the best-correlating symbol onward (the margin
    before it dropped); `None` when there are too few symbols to judge.

    The threshold is the *fixed* 0 Hz centre of the +-deviation design, not the samples' own
    median: an AIS payload is not necessarily bit-balanced, and Gaussian (BT = 0.4) ISI already
    leaves an isolated bit's peak excursion well under the deviation a run of same bits reaches —
    close enough to 0 that even a few-Hz median skew from unbalanced content mis-slices it (found
    empirically: a real payload's median sat ~1.8 kHz off 0 and flipped 3 of 224 bits that the
    fixed 0 Hz threshold gets exactly right, noiseless)."""
    sps = fs / symbol_rate_bd
    smooth = _moving_avg(disc, max(1, int(round(sps * 0.6))))
    best = None
    for ph in np.linspace(0, sps, 64, endpoint=False):
        idx = (ph + np.arange(int((len(smooth) - 1 - ph) / sps)) * sps).astype(int)
        idx = idx[(idx >= 0) & (idx < len(smooth))]
        if len(idx) < _TRAINING_SYMBOLS + 8:
            continue
        s = smooth[idx]
        for start in range(len(s) - _TRAINING_SYMBOLS):
            head = s[start : start + _TRAINING_SYMBOLS]
            corr = float(np.dot(head, _TRAINING_PATTERN)) / (np.linalg.norm(head) + 1e-30)
            if best is None or corr > best[0]:
                best = (corr, s, start)
    if best is None:
        return None
    _, s, start = best
    return (s[start:] > 0.0).astype(np.uint8)


def nrzi_decode(bits: np.ndarray) -> np.ndarray:
    """NRZI: 0 = transition, so a data bit is 1 exactly where the level did *not* change
    (ISO/IEC 13239 section 4.4.1). Invariant to inverting every level (a global sign flip cancels
    in the XOR below), so there is no line-polarity ambiguity to try both ways of."""
    out = np.empty(len(bits), dtype=np.uint8)
    out[0] = 1  # arbitrary; only matters before the first flag is found
    out[1:] = 1 - (bits[1:] ^ bits[:-1])
    return out


def hdlc_destuff(bits: np.ndarray) -> np.ndarray:
    """Removes the 0 stuffed after every 5 consecutive 1s (ISO/IEC 13239 section 4.4.2); a flag's
    own 6-one run is untouched, matching `crates/hk-blocks/src/blocks/symbol/bitstuff.rs`."""
    out: list[int] = []
    ones = 0
    for b in bits:
        b = int(b)
        if b == 1:
            ones += 1
            out.append(1)
        else:
            if ones == 5:
                ones = 0
                continue  # the stuffed zero: dropped, not emitted
            ones = 0
            out.append(0)
    return np.array(out, dtype=np.uint8)


def crc16_x25(data: bytes) -> int:
    """CRC-16/X-25 a.k.a. CRC-16/IBM-SDLC: poly 0x1021 (reflected 0x8408), init/xorout 0xFFFF."""
    crc = 0xFFFF
    for byte in data:
        crc ^= byte
        for _ in range(8):
            crc = (crc >> 1) ^ 0x8408 if crc & 1 else crc >> 1
    return crc ^ 0xFFFF


def _bits_to_str(bits: np.ndarray) -> str:
    return "".join(str(int(b)) for b in bits)


def find_flags(bits: np.ndarray) -> list[int]:
    """Bit positions where the 8-bit flag `0x7E` starts (overlaps count: consecutive flags share
    bits, per HDLC's shared-flag convention)."""
    s = _bits_to_str(bits)
    pat = "".join(str((FLAG >> k) & 1) for k in range(7, -1, -1))
    out = []
    k = s.find(pat)
    while k != -1:
        out.append(k)
        k = s.find(pat, k + 1)
    return out


def frames_between_flags(destuffed_bits: np.ndarray) -> list[np.ndarray]:
    """Every non-empty span strictly between two (not necessarily adjacent) flag occurrences."""
    flags = find_flags(destuffed_bits)
    out = []
    for i, a in enumerate(flags):
        for b in flags[i + 1 :]:
            if b > a + 8:
                out.append(destuffed_bits[a + 8 : b])
    return out


def check_crc(frame_bits: np.ndarray) -> tuple[np.ndarray | None, bool]:
    """(payload_bits, ok): the FCS is the trailing 16 bits, ISO/IEC 13239 section 4.3's
    low-octet-first order (little-endian bytes of the CRC-16/X-25 register)."""
    if len(frame_bits) < 24 or len(frame_bits) % 8 != 0:
        return None, False
    data_bits, check_bits = frame_bits[:-16], frame_bits[-16:]
    data = np.packbits(data_bits).tobytes()
    want = crc16_x25(data)
    lo = int(_bits_to_str(check_bits[:8]), 2)
    hi = int(_bits_to_str(check_bits[8:]), 2)
    return data_bits, (lo | (hi << 8)) == want


def _uint(bits: np.ndarray) -> int:
    return int(_bits_to_str(bits), 2) if len(bits) else 0


def _int(bits: np.ndarray) -> int:
    v = _uint(bits)
    return v - (1 << len(bits)) if len(bits) and (v >> (len(bits) - 1)) else v


def parse_common_block(data_bits: np.ndarray) -> dict:
    """Message type, repeat indicator and MMSI (every AIS message), plus the Class A
    position-report fields for types 1/2/3 (ITU-R M.1371-5 table 45)."""
    out = {
        "message_type": _uint(data_bits[0:6]),
        "repeat_indicator": _uint(data_bits[6:8]),
        "mmsi": _uint(data_bits[8:38]),
    }
    if out["message_type"] in (1, 2, 3) and len(data_bits) >= 168:
        out.update(
            nav_status=_uint(data_bits[38:42]),
            rot=_int(data_bits[42:50]),
            sog_kt=_uint(data_bits[50:60]) * 0.1,
            position_accuracy=_uint(data_bits[60:61]),
            longitude_deg=_int(data_bits[61:89]) / 600000.0,
            latitude_deg=_int(data_bits[89:116]) / 600000.0,
            cog_deg=_uint(data_bits[116:128]) * 0.1,
            heading_deg=_uint(data_bits[128:137]),
            timestamp_s=_uint(data_bits[137:143]),
            maneuver=_uint(data_bits[143:145]),
            raim=_uint(data_bits[147:148]),
            comm_state=_uint(data_bits[148:167]),
        )
    return out


def decode_channel(x: np.ndarray, fs: float,
                   symbol_rate_bd: float = SYMBOL_RATE_BD) -> list[dict]:
    """Every CRC-16/X-25-valid AIS frame in `x` (complex baseband, one channel already isolated
    and centred — the mixing/filtering a caller does before this, as `rds_ref.decode` expects an
    already-downconverted MPX). Burst-detects, demodulates and frames each burst independently
    (a fixed global symbol-timing phase would not track independent transmissions' own bit
    boundaries)."""
    out = []
    sps = fs / symbol_rate_bd
    for s0, s1 in find_bursts(x, fs):
        # A margin either side, so the channel filter has settled by the burst's own first
        # sample (`filtfilt` on a segment that starts mid rising-edge distorts its own start),
        # and the envelope-threshold boundary (which can land a fraction of a symbol short of the
        # true edge) never truncates the closing flag — then trimmed back to ~2 symbols of real
        # margin *after* discriminating: the outer part of the pad's near-zero-amplitude samples
        # give a wildly unstable discriminator (dividing by ~0 amplitude) that would otherwise
        # swamp `slice_symbols`'s phase search with huge spurious excursions.
        pad = int(round(6 * sps))
        seg = x[max(0, s0 - pad) : min(len(x), s1 + pad)]
        xf = _lowpass(seg, fs, 1.3 * symbol_rate_bd)
        disc = fm_discriminate(xf, fs)
        trim = max(0, pad - int(round(2 * sps)))
        disc = disc[trim : len(disc) - trim] if len(disc) > 2 * trim else disc
        line = slice_symbols(disc, fs, symbol_rate_bd)
        if line is None or len(line) < 40:
            continue
        destuffed = hdlc_destuff(nrzi_decode(line))
        for frame in frames_between_flags(destuffed):
            data_bits, ok = check_crc(frame)
            if ok:
                out.append(parse_common_block(data_bits))
    # A frame can validate from more than one flag-pair span inside the same burst (a longer
    # inter-flag scan that happens to also satisfy CRC by chance is vanishingly unlikely, but a
    # repeated *exact* hit is deduplicated).
    seen = set()
    uniq = []
    for m in out:
        key = (m["message_type"], m["mmsi"], m.get("timestamp_s"))
        if key not in seen:
            seen.add(key)
            uniq.append(m)
    return uniq
