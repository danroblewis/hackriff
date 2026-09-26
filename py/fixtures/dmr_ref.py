"""Independent DMR (4FSK, ETSI TS 102 361-1) frame-sync oracle for fixture ground truth (T-985).

Same discipline as ``p25_ref.py``/``flex_ref.py``: built from the public standard (ETSI TS 102
361-1 §9.1.1's SYNC pattern table; sigidwiki "DMR"), never from the explorer agent's own
``tools/dmrsync.py`` claim, so it can *disagree* with a truth file rather than rubber-stamp it. It
also shares no line with the native ``hk-detect`` DMR decoder (there isn't one yet): this is the
oracle a future decoder would be checked against, not a scene either wrote.

Chain: FM discriminator -> a fixed 4800 sym/s (2400 baud, 2 bits/symbol = 9600 bps) symbol clock,
searched over ``N_TIMING_PHASES`` sub-symbol phases (DMR bursts are intermittent -- 12 % duty in
this fixture's own capture -- so a single whole-clip-best phase is dominated by the noise/silence
majority, exactly the P25 finding this reuses; see ``p25_ref.find_syncs_any_phase``) -> a 4-level
(dibit) slice at the standard deviations (data-plane values differ from voice by design, but the
*symbol* mapping is fixed: dibit ``01``/``00``/``10``/``11`` -> deviation ``+3``/``+1``/``-1``/
``-3`` half-steps, i.e. the outer levels are 3x the inner ones -- ``tools/dmrsync.py`` used the
same convention independently, itself following the ETSI symbol map) -> a sliding 48-bit (24
dibit) correlation against each of the standard 12-nibble SYNC patterns, tried across both bit
orders and both polarities, merged the same way as ``p25_ref.find_syncs``, then filtered by
timing-phase corroboration exactly as P25's intermittent bursts needed.

Unlike P25's frame sync (sent using only the two *outer* C4FM deviation levels, so a 2-level sign
slice recovers it without correct 4-level threshold placement), DMR's SYNC patterns use the full
4-level alphabet -- so this oracle needs an actual 4-level threshold, not a sign slice. The
threshold sits at the geometric... no, arithmetic midpoint of the inner and outer deviation
magnitudes (``INNER_DEV_HZ``, ``OUTER_DEV_HZ`` below): this fixture's own histogram (``level_count``
diagnostic re-slice) puts the real peaks near +-0.6/+-1.8 kHz, a 1:3 ratio, matching the standard's
equally-spaced 4FSK levels.

Only PHY-level facts (sync count, position, pattern name, bit rate, level count) leave this module:
no payload bits are decoded (unidentified land-mobile traffic; CLAUDE.md legal guardrails).
"""

from __future__ import annotations

import math

import numpy as np
from scipy import signal

#: ETSI TS 102 361-1 Table 9.2 SYNC patterns, 48 bits (24 dibit symbols) each, as transmitted.
DMR_SYNC_PATTERNS = {
    "BS_voice": 0x755FD7DF75F7,
    "BS_data": 0xDFF57D75DF5D,
    "MS_voice": 0x7F7D5DD57DFD,
    "MS_data": 0xD5D7F77FD757,
}

#: Standard 4FSK deviations (Hz): inner dibits +-1, outer dibits +-3 half-steps (1:3 ratio).
INNER_DEV_HZ = 648.0
OUTER_DEV_HZ = 1944.0
#: Midpoint threshold between the inner and outer deviation magnitudes, for the 4-level slice.
LEVEL_THRESHOLD_HZ = (INNER_DEV_HZ + OUTER_DEV_HZ) / 2.0  # 1296.0


def _sync_symbol_dibits(hexs: str) -> list[str]:
    """Each SYNC pattern's 24 two-bit symbols (MSB first), from its 48-bit hex form."""
    bits = bin(int(hexs, 16))[2:].zfill(len(hexs) * 4)
    return [bits[i : i + 2] for i in range(0, len(bits), 2)]


#: Dibit -> signed symbol level (+3/+1/-1/-3 half-steps), the ETSI/``tools/dmrsync.py`` map.
DIBIT_TO_LEVEL = {"01": 3, "00": 1, "10": -1, "11": -3}
SYNC_SYMS = {name: [DIBIT_TO_LEVEL[d] for d in _sync_symbol_dibits(f"{v:012X}")]
             for name, v in DMR_SYNC_PATTERNS.items()}
SYNC_BITS = 48  # 24 symbols x 2 bits/symbol
SYNC_SYMBOLS = 24
SYNC_RATE_BD = 4800.0
#: Bits allowed to differ (out of 48) for a window to still count as a sync.
SYNC_MAX_HAMMING = 4

#: Window examined for the payload-level diagnostic re-slice, after each sync.
LEVEL_WINDOW_S = 0.10  # roughly one DMR slot's worth of following symbols at 4800 Bd


def fm_discriminate(x: np.ndarray, fs: float) -> np.ndarray:
    return np.angle(x[1:] * np.conj(x[:-1])) * fs / (2 * math.pi)


#: Sub-symbol timing phases tried per symbol period (DMR is intermittent, same reasoning as P25's
#: ``find_syncs_any_phase``).
N_TIMING_PHASES = 16
#: Minimum number of the ``N_TIMING_PHASES`` phases a merged hit must be seen at to be reported as
#: a sync rather than a chance noise alignment. DMR's SYNC uses the full 4-level alphabet (unlike
#: P25's 2-level sign-sliced sync), so a hard 4-level threshold is pickier about sub-symbol timing
#: and corroborates at fewer of the 16 phases even for a clean signal: measured on 20 seeds of 5 s
#: pure random-symbol "noise" (worst case 3/16 phases) against a clean synthetic sync (5/16) --
#: this sits at the noise ceiling plus one, the same margin discipline as ``p25_ref``'s threshold,
#: just a thinner margin because the underlying decision is genuinely harder.
MIN_PHASE_CORROBORATION = 4


def _symbol_values_at_offset(freq: np.ndarray, fsz: float, rate: float, off: float) -> np.ndarray:
    """Symbol-rate matched-filter (integrate-and-dump) sample of the instantaneous-frequency
    trace at ``rate`` baud, at one fixed timing phase ``off`` (in samples)."""
    sps = fsz / rate
    csum = np.concatenate([[0.0], np.cumsum(freq)])
    n_sym = int(len(freq) / sps) - 1
    s0 = off + np.arange(n_sym) * sps
    ia = np.clip(np.round(s0).astype(int), 0, len(freq))
    ib = np.clip(np.round(s0 + sps).astype(int), 0, len(freq))
    return (csum[ib] - csum[ia]) / sps


def _symbols_to_dibits(sym: np.ndarray) -> np.ndarray:
    """4-level slice at ``LEVEL_THRESHOLD_HZ``, mapped back to the two bits each level encodes
    (``DIBIT_TO_LEVEL``'s inverse), flattened to one bit array (MSB of each dibit first)."""
    level = np.where(sym > LEVEL_THRESHOLD_HZ, 3,
             np.where(sym > 0, 1,
             np.where(sym > -LEVEL_THRESHOLD_HZ, -1, -3)))
    inv = {3: (0, 1), 1: (0, 0), -1: (1, 0), -3: (1, 1)}
    bits = np.empty(2 * len(level), dtype=np.uint8)
    for lv, (b0, b1) in inv.items():
        mask = level == lv
        bits[0::2][mask] = b0
        bits[1::2][mask] = b1
    return bits


def _popcount64(x: np.ndarray) -> np.ndarray:
    x = x - ((x >> np.uint64(1)) & np.uint64(0x5555555555555555))
    x = (x & np.uint64(0x3333333333333333)) + ((x >> np.uint64(2)) & np.uint64(0x3333333333333333))
    x = (x + (x >> np.uint64(4))) & np.uint64(0x0F0F0F0F0F0F0F0F)
    return ((x * np.uint64(0x0101010101010101)) >> np.uint64(56)) & np.uint64(0xFF)


def _windows(bits: np.ndarray, width: int) -> np.ndarray:
    n = len(bits) - width + 1
    w = np.zeros(max(n, 0), dtype=np.uint64)
    for k in range(width):
        w = (w << np.uint64(1)) | bits[k : k + n].astype(np.uint64)
    return w


def _reverse_bits(x: int, width: int) -> int:
    b = f"{x:0{width}b}"[::-1]
    return int(b, 2)


def find_syncs(bits: np.ndarray) -> list[dict]:
    """Every position (in bit indices) within ``SYNC_MAX_HAMMING`` of any of ``DMR_SYNC_PATTERNS``'
    48-bit pattern, across both bit orders and both polarities, merged so windows within one
    dibit (2 bits) of each other are one event (the strongest -- lowest Hamming -- of the cluster
    wins). Same method as ``p25_ref.find_syncs``, at 48 bits and 4 candidate patterns instead of
    24 symbols and 1."""
    words = _windows(bits, SYNC_BITS)
    inv_words = _windows(1 - bits, SYNC_BITS)
    variants = {
        "natural": words,
        "natural_inv": inv_words,
        "reversed": np.array([_reverse_bits(int(w), SYNC_BITS) for w in words], dtype=np.uint64),
        "reversed_inv": np.array([_reverse_bits(int(w), SYNC_BITS) for w in inv_words], dtype=np.uint64),
    }
    hits: list[dict] = []
    for name, pattern in DMR_SYNC_PATTERNS.items():
        for order, w in variants.items():
            if len(w) == 0:
                continue
            dist = _popcount64((w ^ np.uint64(pattern)).astype(np.uint64))
            for pos in np.nonzero(dist <= SYNC_MAX_HAMMING)[0]:
                hits.append({"bit": int(pos), "order": order, "pattern": name, "hamming": int(dist[pos])})
    hits.sort(key=lambda h: h["bit"])
    merged: list[dict] = []
    for h in hits:
        if merged and h["bit"] - merged[-1]["bit"] <= 2:
            if h["hamming"] < merged[-1]["hamming"]:
                merged[-1] = h
        else:
            merged.append(dict(h))
    return merged


def find_syncs_any_phase(freq: np.ndarray, fsz: float, rate: float) -> list[dict]:
    """Runs ``find_syncs`` at every one of ``N_TIMING_PHASES`` sub-symbol timing phases, keeps
    only the single (bit order, pattern) combination that occurs most often across all phases,
    then merges into events -- the same discipline as ``p25_ref.find_syncs_any_phase``, needed
    because a real DMR repeater's bursts are a small fraction of a multi-second clip and a single
    whole-clip-best timing phase is dominated by the mostly-idle majority."""
    sps = fsz / rate
    events: list[dict] = []
    for off in np.arange(0, sps, sps / N_TIMING_PHASES):
        sym = _symbol_values_at_offset(freq, fsz, rate, off)
        bits = _symbols_to_dibits(sym)
        for h in find_syncs(bits):
            events.append({**h, "off": float(off), "sample": off + (h["bit"] / 2.0) * sps})
    if not events:
        return []
    counts: dict[tuple[str, str], int] = {}
    for e in events:
        key = (e["order"], e["pattern"])
        counts[key] = counts.get(key, 0) + 1
    best_key = max(counts, key=lambda k: (counts[k], -sum(e["hamming"] for e in events
                                                          if (e["order"], e["pattern"]) == k)))
    events = [e for e in events if (e["order"], e["pattern"]) == best_key]
    events.sort(key=lambda e: e["sample"])
    merged: list[dict] = []
    for e in events:
        if merged and e["sample"] - merged[-1]["sample"] <= sps:
            if e["hamming"] < merged[-1]["hamming"]:
                merged[-1]["hamming"] = e["hamming"]
                merged[-1]["bit"] = e["bit"]
                merged[-1]["off"] = e["off"]
                merged[-1]["sample"] = e["sample"]
            merged[-1]["n_phases"] += 1
        else:
            merged.append({**e, "n_phases": 1})
    return [m for m in merged if m["n_phases"] >= MIN_PHASE_CORROBORATION]


def _clean_peaks(vals: np.ndarray, min_frac: float = 0.05, min_dist_hz: float = 500.0,
                 nbins: int = 48) -> list[float]:
    """Same histogram-peak heuristic as ``p25_ref._clean_peaks``/``flex_ref._clean_peaks``."""
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


def level_count(freq: np.ndarray, syncs: list[dict], off: float, sps: float) -> dict:
    """4-level re-slice of ``LEVEL_WINDOW_S`` seconds following each sync -- diagnostic only, same
    discipline as ``p25_ref.level_count``, not a calibrated measurement."""
    n_syms = int(round(LEVEL_WINDOW_S * SYNC_RATE_BD))
    vals_parts = []
    for s in syncs:
        start = s.get("sample", off + s["bit"] / 2.0 * sps) + SYNC_SYMBOLS // 2 * sps
        idx = np.round(start + np.arange(n_syms) * sps).astype(int)
        idx = idx[(idx >= 0) & (idx < len(freq))]
        if len(idx) == 0:
            continue
        vals_parts.append(freq[idx])
    vals = np.concatenate(vals_parts) if vals_parts else np.array([])
    peaks = _clean_peaks(vals)
    return {"n_levels": len(peaks), "level_centres_hz": [round(p, 1) for p in peaks],
            "n_symbols": int(len(vals))}


#: ETSI TS 102 361-1 §4.2: a DMR burst (one TDMA timeslot) is **30 ms**, two of them make the
#: 60 ms TDMA frame, and every burst carries its SYNC (or embedded signalling) at the same place in
#: the middle. So a real DMR emission's syncs are spaced by whole multiples of 30 ms -- a structural
#: check a chance correlation in noise or voice cannot pass, independent of the sync count (T-986).
BURST_S = 0.030
#: How close a sync spacing must be to a whole number of bursts, s: ~2.4 symbols at 4800 Bd, far
#: wider than a clean sync's position error (under one symbol) and far narrower than the 30 ms grid.
SLOT_GRID_TOL_S = 0.5e-3
#: At least this many syncs before a slot-grid verdict is meaningful (7 spacings).
MIN_IDENT_SYNCS = 8
#: Share of consecutive sync spacings that must sit on the 30 ms grid to identify DMR.
MIN_SLOT_GRID_FRACTION = 0.9


def slot_grid_fraction(times_s: list[float]) -> float | None:
    """Share of consecutive sync spacings within ``SLOT_GRID_TOL_S`` of a whole multiple of
    ``BURST_S``; ``None`` with fewer than two syncs (no spacing to measure)."""
    t = np.sort(np.asarray(times_s, dtype=float))
    if len(t) < 2:
        return None
    d = np.diff(t)
    resid = np.abs(d - np.round(d / BURST_S) * BURST_S)
    return float(np.mean((resid <= SLOT_GRID_TOL_S) & (np.round(d / BURST_S) >= 1)))


def identify(times_s: list[float]) -> bool:
    """DMR identified from sync evidence alone: at least ``MIN_IDENT_SYNCS`` syncs, and at least
    ``MIN_SLOT_GRID_FRACTION`` of their spacings on the 30 ms burst grid."""
    frac = slot_grid_fraction(times_s)
    return len(times_s) >= MIN_IDENT_SYNCS and frac is not None and frac >= MIN_SLOT_GRID_FRACTION


def decode(x: np.ndarray, fs: float) -> dict:
    """Decodes DMR 4FSK frame-sync events from complex baseband ``x`` already centred on the
    channel and low-passed to about its occupied bandwidth (12.5 kHz nominal). ``fs`` should be
    >= ~20 kSps (a handful of samples/symbol at 4800 Bd)."""
    freq = fm_discriminate(x, fs)
    sps = fs / SYNC_RATE_BD
    syncs = find_syncs_any_phase(freq, fs, SYNC_RATE_BD)
    levels = level_count(freq, syncs, off=0.0, sps=sps)
    by_pattern: dict[str, int] = {}
    for s in syncs:
        by_pattern[s["pattern"]] = by_pattern.get(s["pattern"], 0) + 1
    times_s = [s["sample"] / fs for s in syncs]
    return {
        "n_syncs": len(syncs),
        "slot_grid_fraction": slot_grid_fraction(times_s),
        "identified_dmr": identify(times_s),
        "syncs": syncs,
        "syncs_by_pattern": by_pattern,
        "sync_rate_bd": SYNC_RATE_BD,
        "sync_patterns": {k: f"{v:012X}" for k, v in DMR_SYNC_PATTERNS.items()},
        "sync_bits": SYNC_BITS,
        "max_hamming": SYNC_MAX_HAMMING,
        "n_symbols": int(len(freq) / sps) - 1,
        "n_timing_phases": N_TIMING_PHASES,
        "levels": levels,
        "decoder": "py/fixtures/dmr_ref.py (independent oracle: FM discriminator + 4800 Bd "
                   "4-level-sliced 48-bit DMR SYNC-pattern correlation (both bit orders/"
                   "polarities, all four ETSI SYNC patterns), then a 4-level re-slice of the "
                   "data following each sync)",
    }


def decode_ci8(path: str, fs: float, offset_hz: float, start_s: float = 0.0,
               duration_s: float | None = None, half_bw_hz: float = 6_250.0,
               fs_out: float = 48_000.0, chunk_s: float = 2.0) -> dict:
    """Reads a ci8 file, mixes ``offset_hz`` to 0 Hz, filters to +-``half_bw_hz`` (DMR's nominal
    12.5 kHz channel), decimates to about ``fs_out``, and decodes."""
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
