"""Independent P25 (C4FM) frame-sync oracle for fixture ground truth (T-975).

Same discipline as ``rds_ref.py``/``flex_ref.py``: built from the public P25 description
(TIA-102.BAAA; sigidwiki "P25"), never from the explorer agent's own claim, so it can *disagree*
with a truth file rather than rubber-stamp it. It also does not share a line with the native
``hk-detect`` P25 decoder (T-544's oracle gap): the app's decoder is checked against this, not
against a scene either wrote.

Chain: FM discriminator on a channel already mixed to baseband and low-pass filtered -> a fixed
4800 Bd symbol clock (C4FM is always clocked at 4800 symbols/s, 2 bits/symbol = 9600 bps) -> the
frame sync ``0x5575F5FF77FF`` is 48 *bits* but only **24 symbols** (2 bits/dibit), and TIA-102
sends it using only the two *outer* C4FM deviation levels (+-1.8 kHz nominal) -- so each dibit's
sign (its first bit: ``01``/``00`` -> the positive outer level, ``10``/``11`` -> the negative one)
is recoverable by a plain **2-level sign slice** of the discriminator output, one decision per
*symbol*, without needing correct 4-level threshold placement first (this mirrors FLEX's
binary-sync-in-a-multilevel-alphabet trick, ``flex_ref.py``). Getting this wrong -- e.g. sign
slicing at 48 independent bit positions instead of the 24 real symbol periods -- silently searches
for the sync at twice its actual duration and never finds it; a real capture caught exactly that
bug during T-975 (see ``SYNC_SYMS``). A sliding 24-symbol correlation against the derived sign
pattern, tried in both symbol orders and both polarities (order and FM sign convention are not
pinned down by the discriminator alone), tolerates a few symbol errors. A window within
``SYNC_MAX_HAMMING`` symbols counts as a sync; adjacent windows within one symbol are one event,
not several.

A separate 4-level (C4FM dibit) re-slice of the payload following each sync reports how many
clean FSK levels show up, the same histogram-peak heuristic as ``flex_ref.level_count``, for the
fixture's diagnostic ``kind`` label -- not a calibrated measurement, and not a TSBK/NID decode.

Only PHY-level facts (sync count, position, bit rate, level count) leave this module: no payload
bits are decoded (unidentified public-safety traffic; CLAUDE.md legal guardrails, and TSBK/voice
decode is a native ``hk-detect`` concern, not this oracle's).
"""

from __future__ import annotations

import math

import numpy as np
from scipy import signal

#: TIA-102.BAAA / sigidwiki "P25" frame sync, as 48 bits / 24 dibit symbols.
P25_SYNC_HEX = "5575F5FF77FF"


def _sync_symbol_signs(hexs: str) -> list[int]:
    """Each dibit's C4FM deviation sign: ``01``/``00`` (the outer levels' MSB=0) -> +1, ``10``/
    ``11`` -> -1 -- the sync uses only the two *outer* deviation levels, so the sign already
    identifies the transmitted symbol without needing the inner-level thresholds."""
    bits = bin(int(hexs, 16))[2:].zfill(len(hexs) * 4)
    dibits = [bits[i : i + 2] for i in range(0, len(bits), 2)]
    sign = {"01": 1, "00": 1, "10": -1, "11": -1}
    return [sign[d] for d in dibits]


#: 24 +-1 values, one per sync symbol (not 48 independent bits -- see the module docstring).
SYNC_SYMS = _sync_symbol_signs(P25_SYNC_HEX)
#: The sign pattern packed as an int (MSB = earliest symbol), what ``find_syncs`` correlates
#: against.
P25_SYNC = int("".join("1" if s > 0 else "0" for s in SYNC_SYMS), 2)
SYNC_BITS = len(SYNC_SYMS)  # 24 symbols
SYNC_RATE_BD = 4800.0
#: Symbols allowed to differ from ``P25_SYNC`` for a window to still count as a sync (the
#: explorer's own oracle used <=1; a little looser here so this independent oracle is not tuned to
#: match it exactly, following flex_ref's ``SYNC_MAX_HAMMING`` style).
SYNC_MAX_HAMMING = 2

#: Window examined for the payload-level diagnostic re-slice, after each sync.
LEVEL_WINDOW_S = 0.18  # about one P25 NID (64 bits at 4800 Bd)


def fm_discriminate(x: np.ndarray, fs: float) -> np.ndarray:
    return np.angle(x[1:] * np.conj(x[:-1])) * fs / (2 * math.pi)


#: Sub-symbol timing phases tried per symbol period (both by ``_symbol_values_at_offset`` and by
#: the sync search below).
N_TIMING_PHASES = 16
#: Minimum number of the ``N_TIMING_PHASES`` phases a merged hit must be seen at to be reported as
#: a sync rather than a chance noise alignment -- see ``find_syncs_any_phase``.
MIN_PHASE_CORROBORATION = 6


def _symbol_values_at_offset(freq: np.ndarray, fsz: float, rate: float,
                             off: float) -> np.ndarray:
    """Symbol-rate matched-filter (integrate-and-dump) sample of the instantaneous-frequency
    trace at ``rate`` baud, at one fixed timing phase ``off`` (in samples)."""
    sps = fsz / rate
    csum = np.concatenate([[0.0], np.cumsum(freq)])
    n_sym = int(len(freq) / sps) - 1
    s0 = off + np.arange(n_sym) * sps
    ia = np.clip(np.round(s0).astype(int), 0, len(freq))
    ib = np.clip(np.round(s0 + sps).astype(int), 0, len(freq))
    return (csum[ib] - csum[ia]) / sps


def _symbols_at_rate(freq: np.ndarray, fsz: float, rate: float) -> tuple[np.ndarray, float]:
    """Symbol-rate matched-filter sample of the instantaneous-frequency trace at ``rate`` baud:
    returns (raw symbol values, timing offset) for the phase (searched over
    ``N_TIMING_PHASES`` sub-symbol offsets) with the largest mean |symbol energy|, i.e. the
    clearest eye -- reasonable for a payload that fills the window, used here only for the
    diagnostic 4-level re-slice. **Not** used for sync search: see ``find_syncs_any_phase``, which
    a signal only intermittently present (P25's own "control vs conventional" case) needs, since a
    single global energy-best phase computed over mostly-noise/silence would be dominated by noise
    statistics rather than the rare real burst."""
    sps = fsz / rate
    best = None
    for off in np.arange(0, sps, sps / N_TIMING_PHASES):
        sym = _symbol_values_at_offset(freq, fsz, rate, off)
        metric = float(np.mean(np.abs(sym)))
        if best is None or metric > best[0]:
            best = (metric, sym, off)
    _, sym, off = best
    return sym, off


def _popcount64(x: np.ndarray) -> np.ndarray:
    x = x - ((x >> np.uint64(1)) & np.uint64(0x5555555555555555))
    x = (x & np.uint64(0x3333333333333333)) + ((x >> np.uint64(2)) & np.uint64(0x3333333333333333))
    x = (x + (x >> np.uint64(4))) & np.uint64(0x0F0F0F0F0F0F0F0F)
    return ((x * np.uint64(0x0101010101010101)) >> np.uint64(56)) & np.uint64(0xFF)


def _windows(bits: np.ndarray, width: int) -> np.ndarray:
    """The ``width``-bit word (MSB = earliest bit) starting at every bit position."""
    n = len(bits) - width + 1
    w = np.zeros(max(n, 0), dtype=np.uint64)
    for k in range(width):
        w = (w << np.uint64(1)) | bits[k : k + n].astype(np.uint64)
    return w


def _reverse_bits(x: int, width: int) -> int:
    b = f"{x:0{width}b}"[::-1]
    return int(b, 2)


def find_syncs(bits: np.ndarray) -> list[dict]:
    """Every position (in symbol indices) within ``SYNC_MAX_HAMMING`` of ``P25_SYNC``'s 24-symbol
    sign pattern, across both symbol orders and both polarities, merged so windows within one
    symbol of each other are one event (the strongest -- lowest Hamming distance -- of the cluster
    wins). Same method as ``flex_ref.find_syncs``, at 24 symbols instead of 32."""
    words = _windows(bits, SYNC_BITS)
    inv_words = _windows(1 - bits, SYNC_BITS)
    variants = {
        "natural": words,
        "natural_inv": inv_words,
        "reversed": np.array([_reverse_bits(int(w), SYNC_BITS) for w in words], dtype=np.uint64),
        "reversed_inv": np.array([_reverse_bits(int(w), SYNC_BITS) for w in inv_words], dtype=np.uint64),
    }
    hits: list[dict] = []
    for order, w in variants.items():
        if len(w) == 0:
            continue
        dist = _popcount64((w ^ np.uint64(P25_SYNC)).astype(np.uint64))
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


def find_syncs_any_phase(freq: np.ndarray, fsz: float, rate: float) -> list[dict]:
    """Runs ``find_syncs`` at every one of ``N_TIMING_PHASES`` sub-symbol timing phases, then
    keeps only the single symbol-order/polarity ("natural"/"natural_inv"/"reversed"/
    "reversed_inv") that occurs most often across all phases before merging into events.

    Needed because P25 is intermittent (a control-vs-conventional channel may carry a frame sync
    only a handful of times in a multi-second clip): a single timing phase chosen by maximum mean
    symbol energy over the *whole* clip (``_symbols_at_rate``) is dominated by the mostly
    noise/silence majority of the window, not the rare real burst, and reliably missed the sync on
    this fixture's real capture until this per-phase search was added. But trying every phase
    against all 4 orders multiplies the chance-alignment false-alarm rate (measured: ~20 hits on a
    5 s clip with no real signal in it at all, drowning the ~4 real hits this fixture's target
    channel actually carries). A real transmitter's FM sign and the receiver's bit order are fixed
    for the whole capture, so **locking onto whichever order the data agrees with most, then
    reporting only that order's hits**, is a legitimate consistency check, not a second tuning of
    the threshold -- and cuts the chance floor by ~4x (one order search space instead of four)
    without moving ``SYNC_MAX_HAMMING``."""
    sps = fsz / rate
    events: list[dict] = []
    for off in np.arange(0, sps, sps / N_TIMING_PHASES):
        sym = _symbol_values_at_offset(freq, fsz, rate, off)
        bits = (sym > 0).astype(np.uint8)
        for h in find_syncs(bits):
            events.append({**h, "off": float(off), "sample": off + h["bit"] * sps})
    if not events:
        return []
    counts: dict[str, int] = {}
    for e in events:
        counts[e["order"]] = counts.get(e["order"], 0) + 1
    best_order = max(counts, key=lambda o: (counts[o], -sum(e["hamming"] for e in events if e["order"] == o)))
    events = [e for e in events if e["order"] == best_order]
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
    # A real sync's correlation peak is wide enough to keep passing SYNC_MAX_HAMMING across many
    # of the N_TIMING_PHASES nearby timing phases at once; a chance alignment in noise almost
    # never does (measured on this fixture's own signal-free neighbouring channels: the strongest
    # noise-floor cluster there corroborated at 4 of 16 phases, while this fixture's real bursts
    # corroborated at 13-14). MIN_PHASE_CORROBORATION sits between those two clusters.
    return [m for m in merged if m["n_phases"] >= MIN_PHASE_CORROBORATION]


def _clean_peaks(vals: np.ndarray, min_frac: float = 0.05, min_dist_hz: float = 500.0,
                 nbins: int = 48) -> list[float]:
    """Peaks in a lightly-smoothed histogram of ``vals`` that each hold at least ``min_frac`` of
    the samples -- a real, populated FSK/C4FM level, as opposed to the smooth near-zero-mean
    continuum a discriminator produces on noise/silence. Same heuristic as
    ``flex_ref._clean_peaks``."""
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
    """4-level (C4FM dibit) re-slice of ``LEVEL_WINDOW_S`` seconds following each sync, at the
    same 4800 Bd symbol clock the sync was found at -- reports how many clean levels show up (2 if
    only the outer deviations are populated in this window, 4 for a full C4FM payload, fewer if
    the window is noise/silence). Diagnostic only, for the fixture's ``kind`` label, not a
    calibrated measurement. Each ``syncs`` item may carry its own ``sample`` (the sync's own
    timing phase, from ``find_syncs_any_phase``); ``off``/``sps`` are the fallback for a plain
    ``{"bit": ...}`` item (as the unit test constructs)."""
    n_syms = int(round(LEVEL_WINDOW_S * SYNC_RATE_BD))
    vals_parts = []
    for s in syncs:
        start = s.get("sample", off + s["bit"] * sps) + SYNC_BITS // 2 * sps
        idx = np.round(start + np.arange(n_syms) * sps).astype(int)
        idx = idx[(idx >= 0) & (idx < len(freq))]
        if len(idx) == 0:
            continue
        # coarse per-symbol sample (matches the sync's timing phase closely enough for a level
        # histogram; a full re-integration isn't needed for a level *count*).
        vals_parts.append(freq[idx])
    vals = np.concatenate(vals_parts) if vals_parts else np.array([])
    peaks = _clean_peaks(vals)
    return {"n_levels": len(peaks), "level_centres_hz": [round(p, 1) for p in peaks],
            "n_symbols": int(len(vals))}


def decode(x: np.ndarray, fs: float) -> dict:
    """Decodes P25 C4FM frame-sync events from complex baseband ``x`` already centred on the
    channel and low-passed to about its occupied bandwidth (12.5 kHz nominal). ``fs`` should be
    >= ~20 kSps (a handful of samples/symbol at 4800 Bd)."""
    freq = fm_discriminate(x, fs)
    sps = fs / SYNC_RATE_BD
    syncs = find_syncs_any_phase(freq, fs, SYNC_RATE_BD)
    # every sync from find_syncs_any_phase carries its own "sample"; level_count's off/sps
    # fallback is exercised only by the unit test's plain {"bit": ...} items.
    levels = level_count(freq, syncs, off=0.0, sps=sps)
    return {
        "n_syncs": len(syncs),
        "syncs": syncs,
        "sync_rate_bd": SYNC_RATE_BD,
        "sync_hex": P25_SYNC_HEX.upper(),
        "sync_symbols": SYNC_BITS,
        "max_hamming": SYNC_MAX_HAMMING,
        "n_symbols": int(len(freq) / sps) - 1,
        "n_timing_phases": N_TIMING_PHASES,
        "levels": levels,
        "decoder": "py/fixtures/p25_ref.py (independent oracle: FM discriminator + 4800 Bd "
                   "sign-sliced 24-symbol sync correlation (both symbol orders/polarities), then "
                   "a 4-level C4FM dibit re-slice of the data following each sync)",
    }


def decode_ci8(path: str, fs: float, offset_hz: float, start_s: float = 0.0,
               duration_s: float | None = None, half_bw_hz: float = 6_250.0,
               fs_out: float = 48_000.0, chunk_s: float = 2.0) -> dict:
    """Reads a ci8 file, mixes ``offset_hz`` to 0 Hz, filters to +-``half_bw_hz`` (P25's nominal
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
