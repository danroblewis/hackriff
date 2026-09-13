"""S5 spike library: burst detection, parameter estimation (C13) and blind symbol
estimation (C14) prototypes. Research code only (numpy/scipy), not the real-time path.

Conventions: complex baseband, sample rate `fs`. Estimators return dicts with value,
significance/confidence and method name; `None` + reason when they decline to answer.
"""
from __future__ import annotations

import numpy as np
from scipy import ndimage, signal

# ----------------------------------------------------------------------------- IO

def load_ci8(fn: str, start: int = 0, count: int | None = None) -> np.ndarray:
    raw = np.memmap(fn, dtype=np.int8, mode="r")
    n = len(raw) // 2
    if count is None:
        count = n - start
    count = max(0, min(count, n - start))
    a = np.asarray(raw[2 * start:2 * (start + count)], dtype=np.float32).reshape(-1, 2)
    return (a[:, 0] + 1j * a[:, 1]).astype(np.complex64)


def n_samples(fn: str) -> int:
    return len(np.memmap(fn, dtype=np.int8, mode="r")) // 2


def quantize_ci8(x: np.ndarray) -> np.ndarray:
    """Emulate the HackRF 8-bit ADC: round and clip each rail to [-127, 127]."""
    i = np.clip(np.round(x.real), -127, 127)
    q = np.clip(np.round(x.imag), -127, 127)
    return (i + 1j * q).astype(np.complex64)

# ----------------------------------------------------------------------------- detection

def detect_bursts(fn, fs, nfft=1024, avg=4, thr_db=10.0, min_frames=3, max_seconds=None,
                  dc_guard_bins=2, chunk_frames=4096):
    """Energy detector in the time-frequency plane (per-bin median noise, threshold,
    dilation, connected components). Returns (bursts, noise_psd, frame_s)."""
    n = n_samples(fn)
    if max_seconds:
        n = min(n, int(max_seconds * fs))
    hop = nfft * avg
    nfr = n // hop
    win = np.hanning(nfft).astype(np.float32)
    S = np.empty((nfr, nfft), dtype=np.float32)
    for i in range(0, nfr, chunk_frames):
        j = min(nfr, i + chunk_frames)
        x = load_ci8(fn, i * hop, (j - i) * hop).reshape(j - i, avg, nfft)
        X = np.fft.fftshift(np.fft.fft(x * win, axis=-1), axes=-1)
        S[i:j] = (np.abs(X) ** 2).mean(1)
    noise = np.median(S, axis=0)
    R = 10 * np.log10(S / noise[None, :])
    c = nfft // 2
    R[:, c - dc_guard_bins:c + dc_guard_bins + 1] = 0.0
    hit = ndimage.binary_dilation(R > thr_db, structure=np.ones((3, 5), bool))
    lab, _ = ndimage.label(hit)
    bursts = []
    fbin, tfr = fs / nfft, hop / fs
    for k, sl in enumerate(ndimage.find_objects(lab)):
        tsl, fsl = sl
        if tsl.stop - tsl.start < min_frames:
            continue
        sub = R[sl] * (lab[sl] == k + 1)
        bursts.append(dict(t0=tsl.start * tfr, t1=tsl.stop * tfr, f_lo=(fsl.start - c) * fbin,
                           f_hi=(fsl.stop - c) * fbin, peak_db=float(sub.max())))
    return bursts, noise, tfr


def decimate(x, dec):
    if dec <= 1:
        return x
    return signal.decimate(x, dec, ftype="fir", zero_phase=True) if dec <= 13 else \
        signal.resample_poly(x, 1, dec)


def extract_burst(fn, fs, b, pad_s=0.01, bw_pad=1.5, fs_out_min=None, x_full=None):
    """Cut a burst, mix its span centre to 0 Hz, decimate (integer). Returns
    (x, fs_out, f_centre_hz, n_pad_samples_out). `x_full` replaces the file (synthetic)."""
    fcen = 0.5 * (b["f_lo"] + b["f_hi"])
    span = max(b["f_hi"] - b["f_lo"], 1.0)
    want = max(span * bw_pad * 2, fs_out_min or 0)
    dec = max(1, int(fs // want))
    s0 = max(0, int((b["t0"] - pad_s) * fs))
    s1 = int((b["t1"] + pad_s) * fs)
    if x_full is None:
        x = load_ci8(fn, s0, s1 - s0).astype(np.complex128)
    else:
        x = np.asarray(x_full[s0:s1], dtype=np.complex128)
    x = x * np.exp(-2j * np.pi * fcen * (np.arange(len(x)) + s0) / fs)
    x = decimate(x, dec)
    return x.astype(np.complex128), fs / dec, fcen, int(pad_s * fs / dec)


def lowpass(x, fs, cutoff, ntaps=None):
    """Zero-phase FIR low-pass (channel filter)."""
    cutoff = min(cutoff, 0.45 * fs)
    ntaps = ntaps or (int(min(1025, max(31, 8 * fs / cutoff))) | 1)
    h = signal.firwin(ntaps, cutoff, fs=fs)
    return signal.filtfilt(h, 1.0, x) if len(x) > 3 * ntaps else x

# ----------------------------------------------------------------------------- C13 params

def c13_params(x, fs, noise_x=None, nfft=None):
    """OBW99 (noise-subtracted), CFO (centroid within OBW), in-band SNR = S/(N0*OBW).

    N0 = mean Welch density of a signal-free segment `noise_x` (mean, not median: the
    median of a chi-square PSD is biased ~1.6 dB low and inflates SNR on noise-only
    snippets). Without `noise_x`, the lowest 20% of bins (biased low; flagged)."""
    n = len(x)
    if nfft is None:
        nfft = int(2 ** np.clip(np.round(np.log2(max(n, 256) / 8)), 8, 14))
    f, P = signal.welch(x, fs, nperseg=min(nfft, n), return_onesided=False, scaling="density")
    f, P = np.fft.fftshift(f), np.fft.fftshift(P)
    method = "welch-noise-subtracted"
    if noise_x is not None and len(noise_x) >= 128:
        _, Pn = signal.welch(noise_x, fs, nperseg=min(nfft, len(noise_x)), return_onesided=False)
        N0 = float(np.mean(Pn))
    else:
        N0 = float(np.mean(np.sort(P)[: max(3, len(P) // 5)]))
        method += "/N0-from-low-bins"
    Ps = np.convolve(np.clip(P - N0, 0, None), np.ones(5) / 5, mode="same")
    if Ps.sum() <= 0:
        return dict(obw99_hz=None, cfo_hz=None, snr_db=None, reason="low_snr", N0=N0)
    c = np.cumsum(Ps) / Ps.sum()
    lo = f[np.searchsorted(c, 0.005)]
    hi = f[min(len(f) - 1, np.searchsorted(c, 0.995))]
    band = (f >= lo) & (f <= hi)
    df = f[1] - f[0]
    B = hi - lo + df
    Psig = (P[band] - N0).sum() * df  # unclipped: zero-mean on pure noise
    snr = 10 * np.log10(max(Psig, 1e-12 * N0 * B) / (N0 * B))
    cfo = float((f[band] * Ps[band]).sum() / max(Ps[band].sum(), 1e-30))
    return dict(obw99_hz=float(B), cfo_hz=cfo, snr_db=float(snr), N0=N0, method=method)

# ----------------------------------------------------------------------------- spectral lines

def spectral_line(y, fs, fmin, fmax, zero_pad=4, whiten_bins=24):
    """Strongest discrete line of y in [fmin, fmax] -> (freq, significance dB).

    The periodogram (Blackman, zero-padded) is whitened by a local median over
    `whiten_bins` native bins (block medians, interpolated), so a line on a sloped
    continuum is judged against its neighbourhood, not the band. Peak frequency refined by
    log-parabolic interpolation. The mean is removed: a line at exactly 0 Hz is invisible.
    Max of whitened noise over ~1e4 bins is ~10 dB, so use >= 12 dB as 'significant'."""
    y = np.asarray(y)
    n = len(y)
    if n < 32:
        return None, 0.0
    y = y - y.mean()
    N = int(2 ** np.ceil(np.log2(n * zero_pad)))
    Y = np.fft.fftshift(np.abs(np.fft.fft(y * np.blackman(n), N)) ** 2)
    fr = np.fft.fftshift(np.fft.fftfreq(N, 1 / fs))
    blk = max(8, int(whiten_bins * N / n))
    nb = N // blk
    if nb < 3:
        return None, 0.0
    med = np.median(Y[: nb * blk].reshape(nb, blk), axis=1)
    loc = np.interp(np.arange(N), (np.arange(nb) + 0.5) * blk, med)
    R = Y / (loc + 1e-30)
    m = (fr >= fmin) & (fr <= fmax)
    if m.sum() < 5:
        return None, 0.0
    idx = np.where(m)[0]
    k = idx[np.argmax(R[idx])]
    if 0 < k < N - 1:
        a, b, c = np.log(Y[k - 1] + 1e-30), np.log(Y[k] + 1e-30), np.log(Y[k + 1] + 1e-30)
        den = a - 2 * b + c
        d = 0.5 * (a - c) / den if den != 0 else 0.0
    else:
        d = 0.0
    return float(fr[k] + d * fs / N), float(10 * np.log10(R[k]))


METHOD_GROUP = {"env|x|^2": "envelope", "env-diff": "envelope", "delay-mult": "phase", "IF-diff": "phase"}


def rate_line(y, fs, fmin, fmax, method):
    """Raw strongest cyclic line (harmonic resolution is done in `rate_consensus`)."""
    y = np.asarray(y)
    if np.iscomplexobj(y):
        f1, s1 = spectral_line(y.real, fs, fmin, fmax)
        f2, s2 = spectral_line(y.imag, fs, fmin, fmax)
        f, s = (f1, s1) if s1 >= s2 else (f2, s2)
    else:
        f, s = spectral_line(y, fs, fmin, fmax)
    if f is None:
        return dict(method=method, value=None, sig_db=0.0, reason="too_short")
    return dict(method=method, value=float(f), sig_db=float(s))

# ----------------------------------------------------------------------------- transitions

def transitions(v, thr):
    s = v > thr
    idx = np.where(s[1:] != s[:-1])[0]
    a, b = v[idx] - thr, v[idx + 1] - thr
    den = a - b
    frac = np.where(den != 0, a / np.where(den == 0, 1, den), 0.5)
    return idx + frac


def runlength_unit(v, fs, thr, fmax, valid=None):
    """rtl_433 -A style: shortest well-populated run between slicer transitions -> rate seed."""
    tau = transitions(v, thr)
    if valid is not None and len(tau):
        tau = tau[valid[np.clip(tau.astype(int), 0, len(valid) - 1)]]
    runs = np.diff(tau)
    runs = runs[runs > 0.4 * fs / fmax]
    if len(runs) < 8:
        return None
    p10 = np.percentile(runs, 10)
    T = float(np.median(runs[runs <= 1.5 * p10]))
    return fs / T if T > 0 else None


def rate_transitions_ls(v, fs, thr, T0, valid=None, min_trans=12):
    """Refine a clock period seeded at T0 (samples) from slicer transitions.

    Transitions outside `valid` (e.g. envelope-off gaps) are dropped; runs > 64 T0 split
    the burst into segments with their own timing offset and a common T. Integer run counts
    round(run/T), least squares tau = a_seg + T*n, outliers > 0.3 T dropped, iterated.
    ok <=> jitter < 0.12 UI, > 40% odd run counts (a 2x-rate seed gives ~0% odd; random NRZ
    ~67%, a 0101 preamble 100%), fit within 2% of the seed, >= 32 symbols."""
    tau = transitions(v, thr)
    rising = v[np.clip(tau.astype(int) + 1, 0, len(v) - 1)] > thr
    if valid is not None and len(tau):
        keep = valid[np.clip(tau.astype(int), 0, len(valid) - 1)]
        tau, rising = tau[keep], rising[keep]
    if len(tau) > 1:
        keep = np.r_[True, np.diff(tau) > 0.4 * T0]
        tau, rising = tau[keep], rising[keep]
    if len(tau) < min_trans:
        return dict(method="transition-LS", value=None, ok=False, reason="too_short", n_trans=int(len(tau)))
    runs = np.diff(tau)
    seg = np.r_[0, np.cumsum(runs > 64 * T0)]
    nseg = int(seg[-1]) + 1
    # separate timing offsets for rising and falling edges per segment: slicer / pulse-shape
    # asymmetry otherwise lets a non-integer sub-multiple of the clock fit (2.5x seeds)
    col = 2 * seg + rising.astype(int)
    T = float(T0)
    inc = np.ones(len(runs))
    res, nsym, coef = np.zeros(2), 0, np.zeros(2 * nseg + 1)
    kept_frac = 1.0
    for _ in range(5):
        inc = np.where(runs > 64 * T0, 0, np.maximum(1, np.round(runs / T)))
        n = np.r_[0, np.cumsum(inc)]
        A = np.zeros((len(tau), 2 * nseg + 1))
        A[np.arange(len(tau)), col] = 1.0
        A[:, -1] = n
        coef, *_ = np.linalg.lstsq(A, tau, rcond=None)
        r = tau - A @ coef
        keep = np.abs(r) < 0.3 * max(coef[-1], 1e-9)
        kept_frac = float(keep.mean())
        if keep.sum() >= min_trans:
            coef, *_ = np.linalg.lstsq(A[keep], tau[keep], rcond=None)
            res = tau[keep] - A[keep] @ coef
        else:
            res = r
        T = float(coef[-1])
        nsym = int(n[-1])
        if not np.isfinite(T) or T <= 0.25 * T0 or nsym < 2:
            return dict(method="transition-LS", value=None, ok=False, reason="degenerate_fit",
                        n_trans=int(len(tau)))
    counted = inc[inc > 0]
    # common factor: a seed at k x the true rate gives run counts that are all multiples of k
    factor = 1
    for k in (5, 4, 3, 2):
        if len(counted) >= 8 and np.mean(counted % k == 0) > 0.9:
            factor = k
            break
    if factor > 1:
        T *= factor
        counted = counted // factor
        nsym //= factor
    odd = float(np.mean((counted % 2) == 1)) if len(counted) else 0.0
    jit = float(np.std(res) / T)
    # a sliced tone / chirp has one run length everywhere: no data, not a symbol clock
    modal = float(np.max(np.bincount(counted.astype(int))) / len(counted)) if len(counted) else 1.0
    # outlier rejection must not hide a bad clock: a sub-multiple seed leaves ~half the
    # transitions at 0.5 UI and still fits the rest, so require most transitions kept
    ok = bool(jit < 0.12 and odd > 0.40 and abs(T / (T0 * factor) - 1) < 0.02 and nsym >= 32
              and modal <= 0.9 and kept_frac >= 0.85)
    return dict(method="transition-LS", value=float(fs / T), ok=ok, jitter_ui=jit, odd_frac=odd, kept_frac=kept_frac,
                n_trans=int(len(tau)), n_symbols=nsym, n_segments=nseg, factor=factor,
                phase=float(coef[0]), T_samples=T)


def kmeans_1d(v, k=2, iters=30):
    q = np.quantile(v, np.linspace(0.15, 0.85, k))
    for _ in range(iters):
        lab = np.argmin(np.abs(v[:, None] - q[None, :]), axis=1)
        nq = np.array([v[lab == i].mean() if np.any(lab == i) else q[i] for i in range(k)])
        if np.allclose(nq, q):
            break
        q = nq
    lab = np.argmin(np.abs(v[:, None] - q[None, :]), axis=1)
    return q, lab

# ----------------------------------------------------------------------------- C14

def moving_avg(v, L):
    L = max(1, int(L))
    if L == 1:
        return v
    return np.convolve(v, np.ones(L) / L, mode="same")


def inst_freq(x, fs):
    return fs / (2 * np.pi) * np.angle(x[1:] * np.conj(x[:-1]))


def burst_extent(x, fs, win_s, thr_db=6.0, noise_x=None):
    p = moving_avg(np.abs(x) ** 2, win_s * fs)
    if noise_x is not None and len(noise_x) > 64:
        nz = float(np.mean(np.abs(noise_x) ** 2))
    else:
        nz = float(np.percentile(p, 10))
    on = np.where(p > nz * 10 ** (thr_db / 10))[0]
    if len(on) < 8:
        return None
    return int(on[0]), int(on[-1]) + 1, nz


def carrier_lines(x, fs, power):
    """For x^p: coherence |max FFT| / sum|x|^p (~1 = pure carrier: OOK p=1, BPSK p=2,
    QPSK p=4), the carrier frequency /p, and the ratio of the second-largest line (outside
    +-3 native bins, circular) to the largest (~1 for MSK-like two-line x^2, small for BPSK)."""
    y = x.astype(np.complex128) ** power
    N = int(2 ** np.ceil(np.log2(len(y) * 2)))
    Y = np.abs(np.fft.fft(y, N))
    k = int(np.argmax(Y))
    guard = 3 * N // len(y) + 1
    dist = np.minimum(np.abs(np.arange(N) - k), N - np.abs(np.arange(N) - k))
    second = Y[dist > guard].max() if np.any(dist > guard) else 0.0
    return (float(Y[k] / (np.sum(np.abs(y)) + 1e-12)), float(np.fft.fftfreq(N, 1 / fs)[k] / power),
            float(second / (Y[k] + 1e-12)))


def symbol_centre_stats(fi, fs, c, valid):
    """IF sampled at symbol centres for a candidate rate c (phase = best eye of 8):
    2-means separation, Fisher ratio, occupancy and histogram valley depth."""
    T = fs / c
    if T < 2 or len(fi) < 16 * T:
        return None
    v = moving_avg(fi, max(1, 0.6 * T))
    best = None
    for ph in np.linspace(0, T, 8, endpoint=False):
        idx = (ph + np.arange(int((len(v) - 1 - ph) / T)) * T).astype(int)
        idx = idx[valid[np.clip(idx, 0, len(valid) - 1)]] if valid is not None else idx
        if len(idx) < 16:
            continue
        s = v[idx]
        eye = np.mean(np.abs(s - np.median(s)))
        if best is None or eye > best[0]:
            best = (eye, s)
    if best is None:
        return None
    s = best[1]
    q, lab = kmeans_1d(s, 2)
    sep = float(abs(q[1] - q[0]))
    within = float(np.sqrt(np.mean([np.var(s[lab == i]) for i in range(2) if np.any(lab == i)])))
    occ = float(min(np.mean(lab == 0), np.mean(lab == 1)))
    lo_, hi_ = min(q), max(q)
    edges = np.linspace(lo_ - 0.3 * sep, hi_ + 0.3 * sep, 25)
    hist, _ = np.histogram(s, bins=edges)
    ctr = 0.5 * (edges[1:] + edges[:-1])

    def dens(val):
        i = int(np.argmin(np.abs(ctr - val)))
        return hist[max(0, i - 1):i + 2].mean()

    valley = float(dens(0.5 * (lo_ + hi_)) / (0.5 * (dens(lo_) + dens(hi_)) + 1e-9))
    # data carries information: decisions of a tone / chirp sampled at a multiple of its
    # period repeat exactly with some small lag p (1..8); random data agrees ~50%
    d = s > 0.5 * (lo_ + hi_)
    periodic = max(float(np.mean(d[p:] == d[:-p])) for p in range(1, min(9, len(d) // 4)))
    return dict(rate=c, sep=sep, J=sep / (within + 1e-9), occ=occ, valley=valley, periodic=periodic, n=len(s))


def rate_consensus(lines, fmin, fmax, ls_fn=None, sig_min=12.0, seeds=()):
    """Harmonic-aware consensus over raw cyclic lines (+ optional run-length seeds).

    Candidates c = {f/4, f/3, f/2, f, 2f} of every line with significance >= sig_min, plus
    seeds. If transition-LS seeded at c is ok, the candidate becomes the LS rate (LS divides
    out a common run-count factor, so a 3x seed lands on the true rate). Support at a
    candidate = direct lines within 1%; with LS ok, lines at 2/3/4x also count (harmonics).
    Rank: (LS ok, #supporting methods, summed direct significance, lower frequency).
    Trusted iff direct lines >= 14 dB from >= 2 method groups (envelope / phase), or LS ok
    with >= 1 direct or harmonic line. For transition-structured signals (ls_fn given)
    without LS confirmation, >= 3 direct methods are required."""
    sig = [(m["value"], m["sig_db"], m["method"]) for m in lines if m.get("value") and m["sig_db"] >= sig_min]
    raw = sorted({f * k for f, _, _ in sig for k in (0.25, 1 / 3, 0.5, 1.0, 2.0) if fmin <= f * k <= fmax} |
                 {s for s in seeds if s and fmin <= s <= fmax})
    evaluated = []  # (effective rate, ls or None)
    ls_cache = {}
    for c in raw:
        ls = None
        if ls_fn is not None:
            key_c = round(c, -int(np.floor(np.log10(c))) + 2)
            if key_c not in ls_cache:
                ls_cache[key_c] = ls_fn(c)
            ls = ls_cache[key_c]
        ls_ok = bool(ls and ls.get("ok"))
        eff = ls["value"] if ls_ok else c
        if any(abs(eff / e - 1) < 0.01 and (bool(l) == ls_ok) for e, l in evaluated):
            continue
        evaluated.append((eff, ls if ls_ok else None))
    best = None
    for eff, ls in evaluated:
        direct, harm = {}, {}
        for f, s, mm in sig:
            if abs(f / eff - 1) < 0.01:
                direct[mm] = max(direct.get(mm, 0.0), s)
            elif ls is not None and any(abs(f / (k * eff) - 1) < 0.01 for k in (2, 3, 4)):
                harm[mm] = max(harm.get(mm, 0.0), s)
        if not direct and ls is None:
            continue
        key = (ls is not None, len(set(direct) | set(harm)) + int(ls is not None), sum(direct.values()), -eff)
        if best is None or key > best[0]:
            best = (key, eff, direct, harm, ls)
    if best is None:
        return dict(rate=None, trusted=False, reason="no_cyclic_line")
    _, c, direct, harm, ls = best
    if ls:
        val = ls["value"]
    else:
        dv = [(f, s) for f, s, mm in sig if abs(f / c - 1) < 0.01]
        val = float(np.average([f for f, _ in dv], weights=[10 ** (s / 10) for _, s in dv]))
    strong_groups = {METHOD_GROUP.get(m, m) for m, s in direct.items() if s >= 14.0}
    groups = {METHOD_GROUP.get(m, m) for m in direct}
    trusted = len(strong_groups) >= 2 or (ls is not None and (len(direct) >= 1 or len(harm) >= 1))
    if ls_fn is not None and ls is None and len(direct) < 3:
        trusted = False
    alts = [k * c for k in (0.5, 2.0) if fmin <= k * c <= fmax]
    return dict(rate=float(val), trusted=bool(trusted), direct=direct, harmonic_support=harm,
                groups=sorted(groups), ls=ls, n_methods=len(set(direct) | set(harm)) + (1 if ls else 0),
                harmonic_alternatives=alts, reason=None if trusted else "weak_rate_consensus")


def _longest_run(mask, min_gap):
    """Longest run of True in `mask` after closing gaps shorter than min_gap."""
    m = ndimage.binary_closing(mask, structure=np.ones(max(1, int(min_gap)), bool)) if min_gap > 1 else mask
    lab, n = ndimage.label(m)
    if n == 0:
        return 0, len(mask)
    sizes = ndimage.sum(m, lab, range(1, n + 1))
    k = int(np.argmax(sizes)) + 1
    idx = np.where(lab == k)[0]
    return int(idx[0]), int(idx[-1]) + 1


def classify_and_estimate(x, fs, noise_x=None, obw_hz=None, snr_db=None, cfo_hz=None,
                          rs_max=None, snr_floor_db=8.0, rate_fmin=None, rate_snr_floor_db=0.0):
    """Family + symbol rate + FSK deviation for one snippet, with an explicit unknown.

    1. recentre at the C13 CFO; channel filter +-0.75 OBW; extent = first..last sample above
       noise + 6 dB; SNR re-measured over the extent (snr_ext_db); multi-segment bursts
       (few envelope level changes, high contrast) -> analyse the longest on-segment
    2. independent raw cyclic lines: |x|^2 and |d envelope|^2 (envelope group), delay-
       multiply and |dIF|^2 (phase group); run-length seeds
    3. family scores in [0,1]: OOK (two envelope levels, low ~ noise, many changes),
       FSK (IF at symbol centres of the top rate candidates: bimodal with a valley),
       BPSK (single x^2 line, weak x line), QPSK (x^4 line, weak x^2); label iff
       conf >= 0.5 and SNR >= floor; bare carrier -> unknown/unmodulated_carrier
    4. rate consensus (+ transition-LS for FSK/OOK-like); rate trusted iff consensus trusted,
       digital structure (a family score >= 0.5 or LS ok) and SNR >= rate floor
    5. FSK deviation: symbol-centre IF of symbols whose neighbours share the same decision
    """
    out = dict(family="unknown", family_conf=0.0, reasons=[], rate=None, rate_trusted=False,
               rate_consensus_trusted=False, deviation_hz=None)
    if obw_hz is None or snr_db is None:
        p = c13_params(x, fs, noise_x)
        obw_hz, snr_db, cfo_hz = p.get("obw99_hz"), p.get("snr_db"), p.get("cfo_hz")
    if obw_hz is None or snr_db is None:
        out["reasons"].append("low_snr")
        return out
    out["obw_hz"], out["snr_db"] = obw_hz, snr_db
    if cfo_hz:
        x = x * np.exp(-2j * np.pi * cfo_hz * np.arange(len(x)) / fs)
    rs_max = rs_max or obw_hz * 1.2
    xf = lowpass(x, fs, 0.75 * obw_hz)
    nf = lowpass(noise_x, fs, 0.75 * obw_hz) if noise_x is not None and len(noise_x) > 64 else None
    win = 2.0 / max(obw_hz, 1.0)
    ext = burst_extent(xf, fs, win, 6.0, nf)
    if ext is None:
        out["reasons"] += ["no_extent"] + (["low_snr"] if snr_db < snr_floor_db else [])
        return out
    s0, s1, nz = ext
    if s1 - s0 < 64:
        out["reasons"].append("too_short")
        return out
    snr_ext = snr_db
    if noise_x is not None and len(noise_x) >= 128:
        pe = c13_params(x[s0:s1], fs, noise_x)
        if pe.get("snr_db") is not None:
            snr_ext = pe["snr_db"]
    out["snr_ext_db"] = float(snr_ext)
    if snr_ext < snr_floor_db:
        out["reasons"].append("low_snr")
    xe = xf[s0:s1]
    # --- envelope levels
    a = np.sqrt(moving_avg(np.abs(xe) ** 2, win * fs / 2))
    lv, lab = kmeans_1d(a, 2)
    ilo, ihi = int(np.argmin(lv)), int(np.argmax(lv))
    lo = float(np.median(a[lab == ilo])) if np.any(lab == ilo) else float(lv[ilo])
    hi = float(np.median(a[lab == ihi])) if np.any(lab == ihi) else float(lv[ihi])
    contrast_db = float(20 * np.log10(hi / max(lo, 1e-9)))
    labs = ndimage.median_filter(lab, size=5)
    n_env_tr = int(np.sum(labs[1:] != labs[:-1]))
    if contrast_db > 6 and n_env_tr < 12:
        out["reasons"].append("multi_segment")
        g0, g1 = _longest_run(labs == ihi, max(2, win * fs))
        if g1 - g0 >= 64:
            xe, a = xe[g0:g1], a[g0:g1]
            labs = np.full(len(a), ihi)
    p_lo = float(np.mean(labs == ilo))
    on = labs == ihi if (contrast_db > 6 and n_env_tr >= 12) else np.ones(len(a), bool)
    xon = xe[on] if on.sum() > 64 else xe
    env_cv = float(np.std(np.abs(xon)) / (np.mean(np.abs(xon)) + 1e-12))
    fi = inst_freq(xe, fs)
    fis = moving_avg(fi, max(1, fs / obw_hz / 2))
    on1 = on[1:]
    # --- raw cyclic lines
    fmin = rate_fmin or max(4 * fs / len(xe), obw_hz / 50)
    fmax = min(rs_max, fs / 2.5)
    D = max(1, int(round(fs / obw_hz / 2)))
    lines = [rate_line(np.abs(xe) ** 2, fs, fmin, fmax, "env|x|^2"),
             rate_line(np.abs(np.diff(a)) ** 2, fs, fmin, fmax, "env-diff"),
             rate_line(xe[D:] * np.conj(xe[:-D]), fs, fmin, fmax, "delay-mult"),
             rate_line(np.abs(np.diff(fis)) ** 2 * on1[1:], fs, fmin, fmax, "IF-diff")]
    out["rate_methods"] = lines
    # --- FSK: symbol-centre IF bimodality for the top candidates
    q_all, _ = kmeans_1d(fis[on1] if on1.sum() > 64 else fis, 2)
    mid_all = 0.5 * (q_all[0] + q_all[1])
    seed_fsk = runlength_unit(fis, fs, mid_all, fmax, valid=on1)
    cand = sorted([m for m in lines if m.get("value") and m["sig_db"] >= 12], key=lambda m: -m["sig_db"])[:3]
    cvals = [m["value"] for m in cand] + ([seed_fsk] if seed_fsk else [])
    cvals = [c for c in cvals if fmin <= c <= fmax]
    cstats = [s for s in (symbol_centre_stats(fi, fs, c, on1) for c in cvals) if s]
    fsk_best = max(cstats, key=lambda s: s["J"] * (1.0 if s["valley"] < 0.6 else 0.3), default=None)
    fsk_score = 0.0
    if fsk_best:
        fsk_score = min(1.0, max(0.0, (fsk_best["J"] - 2.5) / 2.0)) * (1.0 if fsk_best["occ"] > 0.1 else 0.3) * \
            (1.0 if fsk_best["sep"] > 0.1 * obw_hz else 0.2) * (1.0 if env_cv < 0.35 else 0.4) * \
            (1.0 if fsk_best["valley"] < 0.6 else 0.15) * \
            (0.1 if fsk_best["periodic"] > 0.95 else 1.0)  # tone/chirp: decisions carry no data
    # --- OOK (strong on/off evidence outranks an FSK reading of edge artefacts, and vice versa)
    ook_score = 0.0
    if 0.08 < p_lo < 0.92 and n_env_tr >= 12:
        ook_score = min(1.0, max(0.0, (contrast_db - 8) / 6)) * (1.0 if lo < 2.5 * np.sqrt(nz) else 0.4)
    if ook_score >= 0.8:
        fsk_score *= 0.3
    elif fsk_score >= 0.5:
        ook_score *= 0.3
    # --- PSK / carrier
    c1, _, _ = carrier_lines(xon, fs, 1)
    c2, f2, u2 = carrier_lines(xon, fs, 2)
    c4, _, _ = carrier_lines(xon, fs, 4)
    bpsk_score = min(1.0, max(0.0, (c2 - 0.25) / 0.25)) * (1.0 if c1 < 0.5 * c2 else 0.3) * (1.0 if u2 < 0.6 else 0.2)
    qpsk_score = min(1.0, max(0.0, (c4 - 0.2) / 0.2)) * (1.0 if c2 < 0.5 * c4 else 0.2)
    scores = dict(OOK=ook_score, FSK=fsk_score, BPSK=bpsk_score, QPSK=qpsk_score)
    out["features"] = dict(contrast_db=contrast_db, p_lo=p_lo, n_env_tr=n_env_tr, env_cv=env_cv,
                           fsk=fsk_best, c1=c1, c2=c2, x2_second_line=u2, c4=c4)
    out["scores"] = scores
    best = max(scores, key=scores.get)
    ordered = sorted(scores.values(), reverse=True)
    conf = float(ordered[0] * (1 - 0.7 * ordered[1]))
    out["family_conf"] = conf
    carrier_only = c1 > 0.7 and ook_score < 0.3
    digital = ordered[0] >= 0.5 and not carrier_only
    if carrier_only:
        out["reasons"].append("unmodulated_carrier")
    elif conf >= 0.5 and snr_ext >= snr_floor_db:
        out["family"] = best
    else:
        if conf < 0.5:
            out["reasons"].append("ambiguous_family")
        if env_cv < 0.25 and ordered[0] < 0.2:
            out["analog_likely"] = True
    fam = out["family"]
    # --- rate consensus
    ls_fn, seeds = None, []
    fsk_like = fam == "FSK" or (fam == "unknown" and fsk_score >= max(ook_score, 0.3))
    ook_like = fam == "OOK" or (fam == "unknown" and ook_score > max(fsk_score, 0.3))
    if fsk_like and not carrier_only:
        ls_fn = lambda c: rate_transitions_ls(moving_avg(fi, max(1, 0.4 * fs / c)), fs, mid_all, fs / c, valid=on1)
        seeds.append(seed_fsk)
    elif ook_like:
        thr = 0.5 * (lo + hi)
        ls_fn = lambda c: rate_transitions_ls(a, fs, thr, fs / c)
        seeds.append(runlength_unit(a, fs, thr, fmax))
    cons = rate_consensus(lines, fmin, fmax, ls_fn, seeds=seeds)
    out["consensus"] = cons
    out["rate"] = cons["rate"]
    ls = cons.get("ls")
    out["rate_consensus_trusted"] = bool(cons["trusted"] and not carrier_only)
    out["rate_trusted"] = bool(out["rate_consensus_trusted"] and (digital or ls is not None)
                               and snr_ext >= rate_snr_floor_db)
    if cons["reason"]:
        out["reasons"].append(cons["reason"])
    if cons["trusted"] and not (digital or ls is not None):
        out["reasons"].append("no_digital_structure")
    # --- FSK deviation at symbol centres with same-decision neighbours
    if fam == "FSK" and ls:
        T = ls["T_samples"]
        ph = ls["phase"] % T
        centres = np.arange(ph + T / 2, len(fi) - 1, T).astype(int)
        half = max(1, int(T / 4))
        keep = [c for c in centres if c + half < len(fi) and on[min(c, len(on) - 1)]]
        sm = np.array([fi[max(0, c - half):c + half + 1].mean() for c in keep])
        if len(sm) > 8:
            qq, _ = kmeans_1d(sm, 2)
            mid = 0.5 * (qq[0] + qq[1])
            dec = sm > mid
            full = np.r_[False, (dec[1:-1] == dec[:-2]) & (dec[1:-1] == dec[2:]), False]
            vals = np.abs(sm[full] - mid) if full.sum() >= 8 else np.abs(sm - mid)
            out["deviation_hz"] = float(np.median(vals))
            out["deviation_method"] = "run3-centres" if full.sum() >= 8 else "all-centres"
            out["cfo_fsk_hz"] = float(mid)
            out["mod_index_h"] = float(2 * out["deviation_hz"] / out["rate"])
    return out
