"""Independent FLEX pager sync-word oracle for fixture ground truth (T-949).

Same discipline as ``rds_ref.py`` and ``fsk_ref.py``: an oracle built from the public FLEX
description (sigidwiki, multimon-ng's ``flex.c`` sync constant), never from the explorer agent's
own claim, so it can *disagree* with a truth file rather than rubber-stamp it.

Chain: FM discriminator on a channel already mixed to baseband and low-pass filtered -> a fixed
1600 baud symbol clock (FLEX's Frame Information Word/sync is **always** sent 2-level at 1600 bps,
even on channels whose data portion later goes 4-level at 3200 bps: sigidwiki "FLEX") -> a 2-level
slicer at the timing phase that gives the cleanest eye -> a sliding 32-bit correlation against the
FLEX frame-sync word ``0xA6C6AAAA``, tried in both bit orders and both polarities (FSK sign and
bit-order convention are not pinned down by the discriminator alone). A window within
``SYNC_MAX_HAMMING`` bits of the pattern counts as a sync; adjacent windows within one symbol are
one event, not several.

Only PHY-level facts (sync count, position, bit rate, FSK level count/deviation) leave this
module: no payload bits are decoded or returned (unidentified third-party paging traffic; CLAUDE.md
legal guardrails).
"""

from __future__ import annotations

import math
from typing import Any

import numpy as np
from scipy import signal

FLEX_SYNC = 0xA6C6AAAA
SYNC_BITS = 32
SYNC_RATE_BD = 1600.0
#: Bits allowed to differ from ``FLEX_SYNC`` for a window to still count as a sync.
SYNC_MAX_HAMMING = 3


def fm_discriminate(x: np.ndarray, fs: float) -> np.ndarray:
    return np.angle(x[1:] * np.conj(x[:-1])) * fs / (2 * math.pi)


def _bits_at_rate(freq: np.ndarray, fsz: float, rate: float) -> tuple[np.ndarray, float]:
    """2-level slice of the instantaneous-frequency trace at ``rate`` baud: returns (bits, offset)
    for the timing phase (searched over 16 sub-symbol offsets) with the largest mean |symbol
    energy|, i.e. the clearest eye."""
    sps = fsz / rate
    csum = np.concatenate([[0.0], np.cumsum(freq)])

    def integrate(a, b):
        ia = np.clip(np.round(a).astype(int), 0, len(freq))
        ib = np.clip(np.round(b).astype(int), 0, len(freq))
        return csum[ib] - csum[ia]

    n_sym = int(len(freq) / sps) - 1
    best = None
    for off in np.arange(0, sps, sps / 16):
        s0 = off + np.arange(n_sym) * sps
        sym = integrate(s0, s0 + sps) / sps
        metric = float(np.mean(np.abs(sym)))
        if best is None or metric > best[0]:
            best = (metric, sym, off)
    _, sym, off = best
    return (sym > 0).astype(np.uint8), off


def _popcount32(x: np.ndarray) -> np.ndarray:
    y = x - ((x >> 1) & 0x55555555)
    y = (y & 0x33333333) + ((y >> 2) & 0x33333333)
    y = (y + (y >> 4)) & 0x0F0F0F0F
    return (y * 0x01010101) >> 24 & 0xFF


def _windows32(bits: np.ndarray) -> np.ndarray:
    """The 32-bit word (MSB = earliest bit) starting at every bit position."""
    n = len(bits) - SYNC_BITS + 1
    w = np.zeros(max(n, 0), dtype=np.uint32)
    for k in range(SYNC_BITS):
        w = (w << np.uint32(1)) | bits[k : k + n].astype(np.uint32)
    return w


def _reverse32(x: int) -> int:
    b = f"{x:032b}"[::-1]
    return int(b, 2)


def find_syncs(bits: np.ndarray) -> list[dict]:
    """Every position (in bit indices) within ``SYNC_MAX_HAMMING`` of ``FLEX_SYNC``, across both
    bit orders and both polarities, merged so windows within one symbol of each other are one
    event (the strongest -- lowest Hamming distance -- of the cluster wins)."""
    words = _windows32(bits)
    inv_words = _windows32(1 - bits)
    variants = {
        "natural": words,
        "natural_inv": inv_words,
        "reversed": np.array([_reverse32(int(w)) for w in words], dtype=np.uint32),
        "reversed_inv": np.array([_reverse32(int(w)) for w in inv_words], dtype=np.uint32),
    }
    hits: list[dict] = []
    for order, w in variants.items():
        if len(w) == 0:
            continue
        dist = _popcount32((w ^ np.uint32(FLEX_SYNC)).astype(np.uint32))
        for pos in np.nonzero(dist <= SYNC_MAX_HAMMING)[0]:
            hits.append({"bit": int(pos), "order": order, "hamming": int(dist[pos])})
    hits.sort(key=lambda h: h["bit"])
    merged: list[dict] = []
    for h in hits:
        if merged and h["bit"] - merged[-1]["bit"] <= 1:
            if h["hamming"] < merged[-1]["hamming"]:
                merged[-1] = h
        else:
            merged.append(dict(h))
    return merged


#: Bit rates the level slicer is tried at (sync is always 2-level 1600 Bd; FLEX's faster variants
#: carry 4-level payload at 3200 Bd -- sigidwiki "FLEX"). A window is re-sliced at *each* rate
#: because slicing 3200 Bd data at the 1600 Bd clock averages pairs of symbols together and
#: smears distinct levels into a continuum (found by inspecting a real capture for T-949).
LEVEL_RATES_BD = (1600.0, 3200.0)
#: Seconds of data after each sync to examine for level clustering (one FLEX data block, roughly).
LEVEL_WINDOW_S = 2.0


def _clean_peaks(vals: np.ndarray, min_frac: float = 0.05, min_dist_hz: float = 700.0,
                 nbins: int = 48) -> list[float]:
    """Peaks in a lightly-smoothed histogram of ``vals`` that each hold at least ``min_frac`` of
    the samples -- a real, populated FSK level, as opposed to the smooth near-zero-mean continuum
    a discriminator produces on noise/silence between bursts."""
    if len(vals) == 0:
        return []
    span = float(np.max(np.abs(vals))) or 1.0
    hist, edges = np.histogram(vals, bins=nbins, range=(-span * 1.05, span * 1.05))
    kernel = np.array([1.0, 2.0, 1.0])
    kernel /= kernel.sum()
    smoothed = np.convolve(hist.astype(float), kernel, mode="same")
    bin_hz = edges[1] - edges[0]
    min_dist_bins = max(1, int(round(min_dist_hz / bin_hz)))
    idx, _ = signal.find_peaks(smoothed, height=min_frac * len(vals), distance=min_dist_bins)
    return sorted(float((edges[p] + edges[p + 1]) / 2) for p in idx)


def level_count(freq: np.ndarray, syncs: list[dict], off: float, sps_1600: float) -> dict:
    """Per candidate rate (``LEVEL_RATES_BD``): re-slices ``LEVEL_WINDOW_S`` seconds of data
    following each sync at that rate's symbol clock, and reports how many clean FSK levels show up
    (``_clean_peaks``) -- 2 for 2-level FSK, 4 for FLEX's 4-level payload, fewer if the window is
    noise/silence or timed at the wrong rate for this channel. Reported per rate rather than
    collapsed to one verdict: which rate is "the" data rate is exactly what this is measuring, so
    forcing a single number would hide a real disagreement rather than surface it. Rough, for the
    fixture's ``kind`` label, not a calibrated measurement."""
    csum = np.concatenate([[0.0], np.cumsum(freq)])
    out: dict[str, Any] = {}
    for rate in LEVEL_RATES_BD:
        sps = sps_1600 * (1600.0 / rate)
        n_syms = int(round(LEVEL_WINDOW_S * rate))
        vals_parts = []
        for s in syncs:
            start = off + s["bit"] * sps_1600
            s0 = start + np.arange(n_syms) * sps
            ia = np.clip(np.round(s0).astype(int), 0, len(freq))
            ib = np.clip(np.round(s0 + sps).astype(int), 0, len(freq))
            good = ib > ia
            vals_parts.append((csum[ib[good]] - csum[ia[good]]) / sps)
        vals = np.concatenate(vals_parts) if vals_parts else np.array([])
        peaks = _clean_peaks(vals)
        out[f"{int(rate)}bd"] = {"n_levels": len(peaks), "level_centres_hz": [round(p, 1) for p in peaks],
                                 "n_symbols": int(len(vals))}
    return out


def decode(x: np.ndarray, fs: float) -> dict:
    """Decodes FLEX sync events from complex baseband ``x`` already centred on the channel and
    low-passed to about its occupied bandwidth. ``fs`` should be >= ~16 kSps (>=10 samples/symbol
    at 1600 Bd)."""
    freq = fm_discriminate(x, fs)
    bits, off = _bits_at_rate(freq, fs, SYNC_RATE_BD)
    syncs = find_syncs(bits)
    sps = fs / SYNC_RATE_BD
    levels = level_count(freq, syncs, off, sps)
    return {
        "n_syncs": len(syncs),
        "syncs": syncs,
        "sync_rate_bd": SYNC_RATE_BD,
        "sync_hex": f"{FLEX_SYNC:08X}",
        "max_hamming": SYNC_MAX_HAMMING,
        "n_bits": int(len(bits)),
        "levels": levels,
        "decoder": "py/fixtures/flex_ref.py (independent oracle: FM discriminator + 1600 Bd "
                   "2-level sync correlation (both bit orders/polarities), then a 1600/3200 Bd "
                   "level-count re-slice of the data following each sync)",
    }


def decode_ci8(path: str, fs: float, offset_hz: float, start_s: float = 0.0,
               duration_s: float | None = None, half_bw_hz: float = 12_500.0,
               fs_out: float = 32_000.0, chunk_s: float = 2.0) -> dict:
    """Reads a ci8 file, mixes ``offset_hz`` to 0 Hz, filters to +-``half_bw_hz``, decimates to
    about ``fs_out``, and decodes."""
    raw = np.memmap(path, dtype=np.int8, mode="r")
    n_total = len(raw) // 2
    s0 = int(round(start_s * fs))
    s1 = n_total if duration_s is None else min(n_total, s0 + int(round(duration_s * fs)))
    dec = max(1, int(fs // fs_out))
    taps = signal.firwin(129, half_bw_hz, fs=fs)
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
    return res
