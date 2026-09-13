"""Reference RDS PI/PS decoder for fixture ground truth (promoted from the T-023 pytest decoder in
``py/tests/test_synth.py::decode_rds``).

Same chain as the test decoder: FM discriminator -> 57 kHz downconversion -> BPSK phase from the
squared signal -> biphase symbol timing search -> differential decode -> block sync on the
EN 50067 offset-word syndromes. Changes for real captures:

- the symbol rate and the 57 kHz subcarrier come from the *measured* 19 kHz pilot (RDS is locked
  to it: 57 kHz = 3 x pilot, 1187.5 Bd = pilot / 16), so the receiver's clock error cancels;
- the subcarrier phase is tracked over sliding windows instead of one global estimate;
- PI and PS are majority votes over CRC-valid groups, and decode statistics are reported.

Broadcast RDS only (PI, PS, PTY, TP: public station identity). Research/tooling code, never the
real-time path.
"""

from __future__ import annotations

import math
from collections import Counter

import numpy as np
from scipy import signal

RDS_OFFSETS = {"A": 0x0FC, "B": 0x198, "C": 0x168, "C'": 0x350, "D": 0x1B4}
#: EN 50067 Annex B parity-check matrix; row k applies to bit 25-k of the 26-bit block (MSB first).
RDS_H = (0x200, 0x100, 0x080, 0x040, 0x020, 0x010, 0x008, 0x004, 0x002, 0x001, 0x2DC, 0x16E, 0x0B7,
         0x287, 0x39F, 0x313, 0x355, 0x376, 0x1BB, 0x201, 0x3DC, 0x1EE, 0x0F7, 0x2A7, 0x38F, 0x31B)
#: Syndromes of a correctly received block carrying each offset word.
RDS_SYNDROMES = {"A": 0x3D8, "B": 0x3D4, "C": 0x25C, "C'": 0x3CC, "D": 0x258}

GROUP_BITS = 104
BLOCK_BITS = 26


def syndromes(bits: np.ndarray) -> np.ndarray:
    """Syndrome of the 26-bit word starting at every bit position."""
    n = len(bits) - BLOCK_BITS + 1
    s = np.zeros(max(n, 0), dtype=np.int64)
    for k, row in enumerate(RDS_H):
        s ^= bits[k : k + n].astype(np.int64) * row
    return s


def words(bits: np.ndarray) -> np.ndarray:
    """The 16-bit information word starting at every bit position."""
    n = len(bits) - BLOCK_BITS + 1
    w = np.zeros(max(n, 0), dtype=np.int64)
    for k in range(16):
        w = (w << 1) | bits[k : k + n].astype(np.int64)
    return w


def fm_discriminate(x: np.ndarray, fs: float) -> np.ndarray:
    return np.angle(x[1:] * np.conj(x[:-1])) * fs / (2 * math.pi)


def measure_pilot(mpx: np.ndarray, fs: float, nominal: float = 19000.0) -> tuple[float, float]:
    """Pilot frequency (Hz, zero-padded interpolated FFT peak within +-50 Hz) and peak deviation."""
    dec = int(fs // 1000)
    t = np.arange(len(mpx)) / fs
    b = signal.resample_poly(mpx * np.exp(-2j * math.pi * (nominal - 100.0) * t), 1, dec)
    fsb = fs / dec
    nfft = 1 << int(math.ceil(math.log2(len(b) * 8)))
    spec = np.abs(np.fft.fft(b * np.hanning(len(b)), nfft))
    f = np.fft.fftfreq(nfft, 1 / fsb)
    band = np.where((f > 50.0) & (f < 150.0))[0]
    k = band[np.argmax(spec[band])]
    a, c, e = np.log(spec[k - 1 : k + 2] + 1e-30)
    frac = 0.5 * (a - e) / (a - 2 * c + e)
    f_pilot = nominal - 100.0 + (f[k] + frac * (f[1] - f[0]))
    dev = 2 * abs(np.mean(mpx * np.exp(-2j * math.pi * f_pilot * t)))
    return float(f_pilot), float(dev)


def decode(x: np.ndarray, fs: float, phase_window_s: float = 0.25) -> dict:
    """Decodes RDS from complex baseband ``x`` already centred on the station and low-passed to
    about +-100 kHz. ``fs`` should be >= 150 kSps."""
    mpx = fm_discriminate(x, fs)
    f_pilot, pilot_dev = measure_pilot(mpx, fs)
    f_sub, rate = 3.0 * f_pilot, f_pilot / 16.0
    t = np.arange(len(mpx)) / fs
    z = mpx * np.exp(-2j * math.pi * f_sub * t)
    dec = max(1, int(fs // 24000))
    z = signal.lfilter(signal.firwin(801, 2400, fs=fs), 1.0, z)[400::dec]
    fsz = fs / dec
    # Subcarrier phase from the squared signal over sliding windows (BPSK: x^2 removes the data).
    win = max(1, int(phase_window_s * fsz))
    z2 = np.convolve(z**2, np.ones(win), mode="same")
    phase = np.unwrap(np.angle(z2)) / 2
    r = np.real(z * np.exp(-1j * phase))
    sps = fsz / rate
    csum = np.concatenate([[0.0], np.cumsum(r)])

    def integrate(a, b):
        ia = np.clip(np.round(a).astype(int), 0, len(r))
        ib = np.clip(np.round(b).astype(int), 0, len(r))
        return csum[ib] - csum[ia]

    n_sym = int(len(r) / sps) - 2
    best = None
    for off in np.arange(0, sps, sps / 32):
        s0 = off + np.arange(n_sym) * sps
        diff = integrate(s0, s0 + sps / 2) - integrate(s0 + sps / 2, s0 + sps)
        metric = float(np.sum(np.abs(diff)))
        if best is None or metric > best[0]:
            best = (metric, diff, off)
    _, diff, sym_off = best
    d = (diff > 0).astype(np.uint8)
    bits = d[1:] ^ d[:-1]
    return _frames(bits, fs, dec, sym_off, sps) | {
        "carrier_offset_hz": float(np.mean(mpx)),
        "pilot_hz": f_pilot,
        "pilot_deviation_hz": pilot_dev,
        "clock_ppm_from_pilot": (f_pilot / 19000.0 - 1) * 1e6,
        "bitrate_bd_in_sample_clock": rate,
        "n_bits": int(len(bits)),
    }


def _frames(bits: np.ndarray, fs: float, dec: int, sym_off: float, sps: float) -> dict:
    syn = syndromes(bits)
    info = words(bits)
    n = len(syn)
    ok_a = syn == RDS_SYNDROMES["A"]
    starts = [
        p for p in np.nonzero(ok_a[: max(n - 78, 0)])[0]
        if syn[p + 26] == RDS_SYNDROMES["B"]
        and syn[p + 52] in (RDS_SYNDROMES["C"], RDS_SYNDROMES["C'"])
        and syn[p + 78] == RDS_SYNDROMES["D"]
    ]
    pis: Counter = Counter()
    ps_votes: dict[int, Counter] = {k: Counter() for k in range(4)}
    group_types: Counter = Counter()
    pty: Counter = Counter()
    tp: Counter = Counter()
    for p in starts:
        b2, b4 = int(info[p + 26]), int(info[p + 78])
        version_b = (b2 >> 11) & 1
        # Version B puts PI in block 3 (offset C'); A uses C. Mismatch means a bad sync.
        if (syn[p + 52] == RDS_SYNDROMES["C'"]) != bool(version_b):
            continue
        pis[int(info[p])] += 1
        gtype = f"{b2 >> 12}{'B' if version_b else 'A'}"
        group_types[gtype] += 1
        pty[(b2 >> 5) & 0x1F] += 1
        tp[(b2 >> 10) & 1] += 1
        if b2 >> 12 == 0:
            ps_votes[b2 & 3][chr(b4 >> 8) + chr(b4 & 0xFF)] += 1

    # Block error rate on the group lattice between the first and last valid group, re-anchoring
    # on each valid group (absorbs bit slips).
    valid = sorted(starts)
    blocks_total = blocks_ok = 0
    if valid:
        pos, last, vi = valid[0], valid[-1], 0
        while pos <= last:
            while vi < len(valid) and valid[vi] < pos - 2:
                vi += 1
            if vi < len(valid) and abs(valid[vi] - pos) <= 2:
                pos = valid[vi]
            for k, names in enumerate((("A",), ("B",), ("C", "C'"), ("D",))):
                q = pos + 26 * k
                if q < n:
                    blocks_total += 1
                    blocks_ok += int(any(syn[q] == RDS_SYNDROMES[m] for m in names))
            pos += GROUP_BITS
    # PS frames: segments 0,1,2,3 received in order, each within 2 s of the previous. Stations
    # with dynamic (scrolling) PS send several texts, so PS is the set of complete frames.
    ps_frames: Counter = Counter()
    seq: list[tuple[int, int, str]] = []
    for p in valid:
        b2, b4 = int(info[p + 26]), int(info[p + 78])
        if b2 >> 12 == 0 and ((syn[p + 52] == RDS_SYNDROMES["C'"]) == bool((b2 >> 11) & 1)):
            seq.append((p, b2 & 3, chr(b4 >> 8) + chr(b4 & 0xFF)))
    max_gap_bits = int(2.0 * 1187.5)
    buf: list[str] = []
    frame_log: list[dict] = []
    frame_start = None
    prev_p = prev_seg = None
    for p, seg, txt in seq:
        if seg == 0:
            buf, frame_start = [txt], p
        elif buf and seg == prev_seg + 1 and p - prev_p <= max_gap_bits:
            buf.append(txt)
            if seg == 3:
                text = "".join(buf)
                ps_frames[text] += 1
                frame_log.append({"t_s": float((sym_off + (frame_start + 1) * sps) * dec / fs),
                                  "ps": text})
                buf = []
        else:
            buf = []
        prev_p, prev_seg = p, seg
    ps = ps_frames.most_common(1)[0][0] if ps_frames else None
    pi = pis.most_common(1)[0][0] if pis else None
    first_bit_s = None
    if valid:
        # bit i is decided by symbols i and i+1; symbol j starts at sym_off + j*sps (decimated)
        first_bit_s = float((sym_off + (valid[0] + 1) * sps) * dec / fs)
    return {
        "pi": pi,
        "pi_hex": None if pi is None else f"{pi:04X}",
        "pi_votes": {f"{k:04X}": v for k, v in pis.items()},
        "ps": ps,
        "ps_frames": dict(ps_frames.most_common()),
        "ps_frame_log": frame_log,
        "ps_dynamic": len(ps_frames) > 1,
        "ps_segment_votes": {str(k): dict(v) for k, v in ps_votes.items()},
        "pty": pty.most_common(1)[0][0] if pty else None,
        "tp": bool(tp.most_common(1)[0][0]) if tp else None,
        "group_types": dict(sorted(group_types.items())),
        "groups_decoded": int(sum(group_types.values())),
        "groups_on_lattice": int(blocks_total // 4) if blocks_total else 0,
        "blocks_on_lattice": int(blocks_total),
        "blocks_ok": int(blocks_ok),
        "block_error_rate": None if not blocks_total else 1.0 - blocks_ok / blocks_total,
        "first_valid_group_s": first_bit_s,
    }


def decode_ci8(path: str, fs: float, offset_hz: float, start_s: float = 0.0,
               duration_s: float | None = None, chunk_s: float = 2.0) -> dict:
    """Reads a ci8 file, mixes ``offset_hz`` to 0 Hz, filters to +-140 kHz, decimates to ~240 kSps,
    and decodes."""
    raw = np.memmap(path, dtype=np.int8, mode="r")
    n_total = len(raw) // 2
    s0 = int(round(start_s * fs))
    s1 = n_total if duration_s is None else min(n_total, s0 + int(round(duration_s * fs)))
    dec = max(1, int(fs // 240000))
    taps = signal.firwin(129, 140e3, fs=fs)
    zi = np.zeros(len(taps) - 1, dtype=complex)
    out = []
    step = int(chunk_s * fs) // dec * dec
    for a in range(s0, s1, step):
        b = min(s1, a + step)
        iq = np.asarray(raw[2 * a : 2 * b], dtype=np.float32).reshape(-1, 2)
        c = (iq[:, 0] + 1j * iq[:, 1]) * np.exp(-2j * math.pi * offset_hz * np.arange(a, b) / fs)
        y, zi = signal.lfilter(taps, 1.0, c, zi=zi)
        out.append(y[(-(a - s0)) % dec :: dec] if dec > 1 else y)
    x = np.concatenate(out)
    res = decode(x, fs / dec)
    res["span"] = {"start_s": start_s, "duration_s": (s1 - s0) / fs}
    if res["first_valid_group_s"] is not None:
        res["first_valid_group_s"] += start_s + len(taps) // 2 / fs
    return res
