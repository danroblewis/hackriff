"""Independent CTCSS (sub-audible tone) reference oracle for fixture ground truth (T-985).

Same discipline as ``rds_ref.py``/``p25_ref.py``/``flex_ref.py``: built from the public EIA/TIA
RS-220 standard tone table (sigidwiki "CTCSS"), never from the explorer agent's own
``tools/nbfm3.py`` claim, so it can *disagree* with a truth file rather than rubber-stamp it.

Chain, exactly as specified: **NBFM discriminator**, then a **low-pass filter below 300 Hz** (the
CTCSS band proper is 67-254.1 Hz; 300 Hz gives margin without admitting voice, which for NBFM sits
above ~300 Hz), then the **tone frequency estimated to 0.1 Hz** (a Welch-averaged, zero-padded power
spectrum + parabolic peak interpolation, the same interpolation ``rds_ref.measure_pilot`` uses
for the 19 kHz pilot; ``measure_tone``'s docstring explains why the averaging is needed) and
**matched to the EIA 50-tone table** (nearest table entry, only if within ``MAX_MATCH_DELTA_HZ``
of it -- an unmatched measurement is reported as a raw frequency with no CTCSS label, not forced
onto the nearest tone).

A **noise guard** (``MIN_SNR_DB``) is required before any tone is reported at all: the ratio of the
band's peak bin to its own median is the corroboration, following the same "don't let a loose
tolerance always fire" discipline as ``p25_ref``/``flex_ref``'s sync-search guards -- a smooth
near-zero-mean noise floor should not report a confident CTCSS match. DCS (digital) sub-audible
signalling is out of scope for this oracle (the task's decode chain names CTCSS/the 50-tone table
only); a DCS oracle would need its own bit-level design.

Only the measured tone frequency, its SNR and (if matched) the nearest standard tone leave this
module -- no payload/voice content is touched (CLAUDE.md legal guardrails: this is sub-audible
signalling metadata, not intercepted voice).
"""

from __future__ import annotations

import math

import numpy as np
from scipy import signal

#: EIA/TIA RS-220 standard CTCSS tone set (50 tones, Hz), label == its own frequency string.
EIA_CTCSS_HZ = {
    f"{f:g}": f for f in (
        67.0, 69.3, 71.9, 74.4, 77.0, 79.7, 82.5, 85.4, 88.5, 91.5, 94.8, 97.4, 100.0, 103.5,
        107.2, 110.9, 114.8, 118.8, 123.0, 127.3, 131.8, 136.5, 141.3, 146.2, 151.4, 156.7, 159.8,
        162.2, 165.5, 167.9, 171.3, 173.8, 177.3, 179.9, 183.5, 186.2, 189.9, 192.8, 196.6, 199.5,
        203.5, 206.5, 210.7, 218.1, 225.7, 229.1, 233.6, 241.8, 250.3, 254.1,
    )
}
#: The sub-audible search band: wide enough to bracket every standard tone with margin.
BAND_LO_HZ = 55.0
BAND_HI_HZ = 270.0
#: The specified low-pass cutoff (module docstring): keeps NBFM voice content out.
LOWPASS_CUTOFF_HZ = 300.0
#: A measured peak must be within this much of a table entry to be reported as that tone.
MAX_MATCH_DELTA_HZ = 1.5
#: Peak-to-median-of-band ratio (dB) required before any tone is reported (noise guard).
MIN_SNR_DB = 8.0


def fm_discriminate(x: np.ndarray, fs: float) -> np.ndarray:
    return np.angle(x[1:] * np.conj(x[:-1])) * fs / (2 * math.pi)


def lowpass_subaudible(freq: np.ndarray, fs: float, cutoff: float = LOWPASS_CUTOFF_HZ,
                       fs_out: float = 1000.0) -> tuple[np.ndarray, float]:
    """Zero-phase low-pass below ``cutoff`` Hz (``filtfilt``: no group delay to correct for a
    tone-frequency measurement), then decimated to about ``fs_out`` Hz -- comfortably above twice
    the highest standard CTCSS tone (254.1 Hz)."""
    ntaps = min(401, len(freq) - 1 if len(freq) % 2 == 0 else len(freq))
    ntaps = max(ntaps - (1 - ntaps % 2), 5)  # odd length for firwin
    taps = signal.firwin(ntaps, cutoff, fs=fs)
    y = signal.filtfilt(taps, [1.0], freq)
    dec = max(1, int(fs // fs_out))
    return y[::dec], fs / dec


#: Welch segment length for the noise guard below.
SEGMENT_S = 1.0


def measure_tone(sub: np.ndarray, fs_sub: float, band: tuple[float, float] = (BAND_LO_HZ, BAND_HI_HZ),
                 segment_s: float = SEGMENT_S) -> dict:
    """Welch-averaged, zero-padded power spectrum peak in ``band``, refined to sub-bin (about
    0.1 Hz or better) by parabolic interpolation of the log-power around the peak bin -- the same
    interpolation ``rds_ref.measure_pilot`` uses for the 19 kHz pilot.

    A single zero-padded FFT over the *whole* window (tried first, and much simpler) turned out to
    be a broken noise guard: zero-padding only interpolates, it does not reduce the (~2-DOF, high
    variance) per-bin noise power, and a huge padded FFT has enough bins that pure noise's random
    peak-to-median ratio regularly exceeded 8 dB by chance (measured false "tone" matches on
    noise-only synthetic input). Averaging several non-overlapping ``segment_s``-long Welch
    segments (each still zero-padded for the fine grid) drives that variance down -- worst
    observed noise SNR over 30 synthetic noise-only seeds dropped to ~5.9 dB against a genuine
    tone's ~25 dB, which is what ``MIN_SNR_DB`` sits between."""
    n = len(sub)
    if n < 8:
        return {"freq_hz": None, "snr_db": None, "bin_hz": None}
    nperseg = max(8, min(n, int(round(segment_s * fs_sub))))
    nfft = 1 << int(math.ceil(math.log2(max(nperseg * 32, 4096))))
    f, pxx = signal.welch(sub, fs=fs_sub, window="hann", nperseg=nperseg, noverlap=0, nfft=nfft,
                          detrend=False)
    band_idx = np.where((f >= band[0]) & (f <= band[1]))[0]
    if len(band_idx) < 3:
        return {"freq_hz": None, "snr_db": None, "bin_hz": float(f[1] - f[0]) if len(f) > 1 else None}
    k = band_idx[np.argmax(pxx[band_idx])]
    if k <= 0 or k >= len(pxx) - 1:
        peak_f, peak_pow = float(f[k]), float(pxx[k])
    else:
        a, c, e = np.log(pxx[k - 1 : k + 2] + 1e-30)
        denom = a - 2 * c + e
        frac = 0.5 * (a - e) / denom if denom != 0 else 0.0
        peak_f = float(f[k] + frac * (f[1] - f[0]))
        peak_pow = float(pxx[k])
    median_pow = float(np.median(pxx[band_idx])) + 1e-30
    snr_db = float(10 * np.log10(peak_pow / median_pow)) if peak_pow > 0 else None
    return {"freq_hz": peak_f, "snr_db": snr_db, "bin_hz": float(f[1] - f[0])}


def nearest_ctcss(freq_hz: float, max_delta_hz: float = MAX_MATCH_DELTA_HZ) -> dict | None:
    label, table_hz = min(EIA_CTCSS_HZ.items(), key=lambda kv: abs(kv[1] - freq_hz))
    delta = freq_hz - table_hz
    if abs(delta) > max_delta_hz:
        return None
    return {"label": label, "table_hz": table_hz, "delta_hz": round(delta, 3)}


def decode(x: np.ndarray, fs: float, min_snr_db: float = MIN_SNR_DB) -> dict:
    """Decodes a CTCSS tone (if any, above the noise guard) from complex baseband ``x`` already
    centred on the NBFM channel and low-passed to about its occupied bandwidth. ``fs`` should be
    a few kSps or more (the discriminator + subaudible low-pass need only resolve up to ~300 Hz,
    but a higher input rate keeps the discriminator itself well-conditioned)."""
    freq = fm_discriminate(x, fs)
    sub, fs_sub = lowpass_subaudible(freq, fs)
    tone = measure_tone(sub, fs_sub)
    ctcss = None
    if tone["freq_hz"] is not None and tone["snr_db"] is not None and tone["snr_db"] >= min_snr_db:
        ctcss = nearest_ctcss(tone["freq_hz"])
    return {
        "tone_hz_measured": tone["freq_hz"],
        "tone_snr_db": tone["snr_db"],
        "tone_bin_hz": tone["bin_hz"],
        "min_snr_db": min_snr_db,
        "ctcss": ctcss,
        "table": "EIA/TIA RS-220 50-tone CTCSS set",
        "decoder": "py/fixtures/ctcss_ref.py (independent oracle: NBFM discriminator -> "
                   "zero-phase low-pass < 300 Hz -> Welch-averaged zero-padded tone estimate "
                   "to ~0.1 Hz -> nearest EIA 50-tone match)",
    }


def decode_ci8(path: str, fs: float, offset_hz: float, start_s: float = 0.0,
               duration_s: float | None = None, half_bw_hz: float = 7_500.0,
               fs_out: float = 48_000.0, chunk_s: float = 2.0, min_snr_db: float = MIN_SNR_DB
               ) -> dict:
    """Reads a ci8 file, mixes ``offset_hz`` to 0 Hz, filters to +-``half_bw_hz`` (an NBFM
    channel's nominal occupied bandwidth), decimates to about ``fs_out``, and decodes."""
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
    res = decode(x, fs / dec, min_snr_db=min_snr_db)
    res["span"] = {"start_s": start_s, "duration_s": (s1 - s0) / fs}
    return res
