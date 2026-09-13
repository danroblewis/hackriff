"""Spike S4 shared library: PSD/STFT, noise-floor estimators, OS-CFAR, hysteresis,
masks and cross-capture ghost tests.  Throwaway research code (Python is allowed in
spikes only; the product implementation is Rust in hk-dsp / hk-detect)."""
import json
import numpy as np
import scipy.fft as sfft
from scipy import special, ndimage, optimize, signal

STORE = "/Users/daniellewis/hackriff/fixtures/store/2026-09-13/"

# --------------------------------------------------------------------------- IO

def load_meta(name):
    m = json.load(open(STORE + name + ".sigmf-meta"))
    g = m["global"]
    prov = g["hackriff:provenance"]
    t = prov["tune"]
    return dict(name=name, fs=float(g["core:sample_rate"]),
                fc=float(m["captures"][0]["core:frequency"]),
                lna=t["lna_db"], vga=t["vga_db"], amp=bool(t["amp_on"]),
                clip_count=prov["clip_count"], datetime=m["captures"][0]["core:datetime"])


def stft_power(name, nfft=4096, n_avg=10, max_frames=None, workers=8, transform=None):
    """Hann-windowed, non-overlapping FFT frames averaged n_avg at a time.
    Returns dict with P (frames x bins, mean |X|^2 PSD in FS^2/Hz, fftshifted),
    S2 (sum of |X|^4 normalised the same way, for spectral kurtosis),
    clip (clip fraction per output frame), f (baseband Hz), dt (s/frame).
    transform(iq_chunk, first_sample_index) -> iq_chunk lets a caller modify the IQ
    (used only by the semi-synthetic IMD control)."""
    meta = load_meta(name)
    fs = meta["fs"]
    raw = np.memmap(STORE + name + ".sigmf-data", dtype=np.int8, mode="r")
    ns = raw.size // 2
    nout = ns // (nfft * n_avg)
    if max_frames:
        nout = min(nout, max_frames)
    w = signal.windows.hann(nfft, sym=False).astype(np.float32)
    norm = np.float32(fs * np.sum(w.astype(np.float64) ** 2))
    P = np.empty((nout, nfft), np.float32)
    S2 = np.empty((nout, nfft), np.float32)
    clip = np.empty(nout, np.float32)
    B = 64
    for o0 in range(0, nout, B):
        o1 = min(nout, o0 + B)
        s0, s1 = o0 * n_avg * nfft, o1 * n_avg * nfft
        r = np.asarray(raw[2 * s0:2 * s1])
        ri, rq = r[0::2], r[1::2]
        c = ((ri == 127) | (ri == -128) | (rq == 127) | (rq == -128))
        clip[o0:o1] = c.reshape(o1 - o0, -1).mean(1)
        iq = np.empty(ri.size, np.complex64)
        iq.real = ri.astype(np.float32) / 128
        iq.imag = rq.astype(np.float32) / 128
        if transform is not None:
            iq = transform(iq, s0).astype(np.complex64)
        X = sfft.fft(iq.reshape(-1, nfft) * w, axis=1, workers=workers)
        p = (X.real ** 2 + X.imag ** 2) / norm
        p = p.reshape(o1 - o0, n_avg, nfft)
        P[o0:o1] = sfft.fftshift(p.mean(1), axes=-1)
        S2[o0:o1] = sfft.fftshift((p ** 2).sum(1), axes=-1)
    f = sfft.fftshift(sfft.fftfreq(nfft, 1 / fs))
    return dict(meta=meta, P=P, S2=S2, clip=clip, f=f, df=fs / nfft,
                dt=nfft * n_avg / fs, n_avg=n_avg, nfft=nfft)


def spectral_kurtosis(P, S2, n_avg, t0=0, t1=None, b0=0, b1=None):
    """SK over frames [t0,t1) for bins [b0,b1): M = frames*n_avg FFTs.
    S1 = sum|X|^2 = P*n_avg.  SK = (M+1)/(M-1) * (M*S2/S1^2 - 1). ~1 noise, <1 CW, >1 bursty."""
    S1 = P[t0:t1, b0:b1].astype(np.float64).sum(0) * n_avg
    s2 = S2[t0:t1, b0:b1].astype(np.float64).sum(0)
    M = (P[t0:t1].shape[0]) * n_avg
    sk = (M + 1) / (M - 1) * (M * s2 / S1 ** 2 - 1)
    return sk


# ----------------------------------------------------------------- statistics

def gamma_T(n, pfa):
    """Threshold multiplier (relative to the mean) for a mean of n exponential bins."""
    return special.gammaincinv(n, 1 - pfa) / n


def os_cfar_alpha(N, k, n, pfa):
    """Scale alpha so that P(CUT > alpha * Z_(k)) = pfa, with CUT and the N reference
    cells iid Gamma(n) (mean of n exponential bins) and Z_(k) the k-th smallest.
    Numerical integral over the order-statistic density."""
    lo = special.gammaincinv(n, 1e-14)
    hi = special.gammaincinv(n, 1 - 1e-14)
    y = np.geomspace(lo, hi, 20000)
    F = special.gammainc(n, y)
    logf = (n - 1) * np.log(y) - y - special.gammaln(n)
    logc = special.gammaln(N + 1) - special.gammaln(k) - special.gammaln(N - k + 1)
    with np.errstate(divide="ignore"):
        fk = np.exp(logc + (k - 1) * np.log(F) + (N - k) * np.log1p(-F) + logf)

    def pf(la):
        return np.trapezoid(fk * special.gammaincc(n, np.exp(la) * y), y)

    la = optimize.brentq(lambda la: np.log(pf(la)) - np.log(pfa), np.log(1.0001), np.log(1e4))
    return float(np.exp(la))


def os_cfar_alpha_exact_n1(N, k, pfa):
    """Closed form for n=1 (exponential): Pfa = prod_{i=0}^{k-1} (N-i)/(N-i+alpha)."""
    f = lambda a: np.sum(np.log((N - np.arange(k)) / (N - np.arange(k) + a))) - np.log(pfa)
    return optimize.brentq(f, 1e-6, 1e6)


# ------------------------------------------------------------ floor estimators

def fcme(v, n, pfa=1e-3, init_frac=0.1, iters=50):
    """Forward consecutive mean excision along the last axis.
    v: linear power, each value a mean of n exponential bins.  Returns (floor, clean_fraction).
    The retained set is noise below T*mean; the truncated-mean bias is corrected."""
    s = np.sort(v, axis=-1).astype(np.float64)
    L = s.shape[-1]
    cs = np.cumsum(s, axis=-1)
    T = gamma_T(n, pfa)
    m = max(1, int(init_frac * L))
    cnt = np.full(s.shape[:-1], m)
    mean = cs[..., m - 1] / m
    for _ in range(iters):
        thr = T * mean
        new = np.clip((s < thr[..., None]).sum(-1), m, L)
        mean = np.take_along_axis(cs, (new - 1)[..., None], -1)[..., 0] / new
        if np.array_equal(new, cnt):
            break
        cnt = new
    # E[X | X < T mu] / mu for Gamma(n, mu/n)
    corr = special.gammainc(n + 1, n * T) / special.gammainc(n, n * T)
    return mean / corr, cnt / L


def percentile_floor(v, n, p=0.5):
    """p-quantile across the last axis, bias-corrected for Gamma(n) (median/ln2 for n=1)."""
    q = np.quantile(v, p, axis=-1)
    return q / (special.gammaincinv(n, p) / n)


def blockwise(fn, P, block=256, hop=64, **kw):
    """Apply a floor estimator on overlapping frequency blocks of P (... x bins) and
    linearly interpolate block-centre values back to every bin (in dB)."""
    nb = P.shape[-1]
    starts = np.arange(0, nb - block + 1, hop)
    centres = starts + block / 2 - 0.5
    idx = starts[:, None] + np.arange(block)[None, :]
    blocks = P[..., idx]  # (..., nblocks, block)
    out = fn(blocks, **kw)
    fl = out[0] if isinstance(out, tuple) else out
    lf = 10 * np.log10(fl)
    x = np.arange(nb)
    if lf.ndim == 1:
        return 10 ** (np.interp(x, centres, lf) / 10)
    res = np.empty(P.shape, np.float32)
    for i in range(lf.shape[0]):
        res[i] = 10 ** (np.interp(x, centres, lf[i]) / 10)
    return res


def minstat_floor(P, n, win=256, smooth=0.7, bias_mc=4000, rng=0):
    """Minimum statistics per bin across time: first-order recursive smoothing (alpha =
    smooth), sliding minimum over `win` frames, bias-compensated by Monte Carlo on Gamma(n)."""
    S = signal.lfilter([1 - smooth], [1, -smooth], P.astype(np.float64), axis=0)
    S[: min(20, len(S))] = S[min(20, len(S))]  # drop filter start-up transient
    Mn = ndimage.minimum_filter1d(S, size=win, axis=0, mode="nearest")
    g = np.random.default_rng(rng).gamma(n, 1.0 / n, size=(win * 4, bias_mc))
    gs = signal.lfilter([1 - smooth], [1, -smooth], g, axis=0)[40:40 + win]
    bias = gs.min(0).mean()
    return (Mn / bias).astype(np.float32), bias


# -------------------------------------------------------------------- detector

DEFAULT = dict(pfa=1e-6, off_pfa=1e-3, hyst_db=None, N=32, G=4, k=24, guard_db=3.0,
               min_frames=3, gap_frames=2, merge_bins=0, mode="or", floor_pfa=1e-6)


def detect(st, floor, cfg=DEFAULT, alpha=None, Z=None):
    """2-D (time x frequency) detection on STFT frames.
    seed   = (P > a_on *Z_os AND P > guard*floor)  OR  (P > T_on *floor)      [mode 'or']
    region = (P > a_off*Z_os AND P > guard*floor)  OR  (P > T_off*floor)
    a_on/T_on from pfa (per cell); a_off/T_off from off_pfa (or a fixed hyst_db below on).
    4-connected components of the *raw* region that contain a seed and span >= min_frames
    frames survive; only then are survivors merged across <= gap_frames gaps (closing in
    time) and turned into boxes.  (Closing before the duration test lets noise satisfy it.)"""
    P = st["P"]
    n = st["n_avg"]
    N, G, k = cfg["N"], cfg["G"], cfg["k"]
    if alpha is None:
        alpha = os_cfar_alpha(N, k, n, cfg["pfa"])
    if cfg.get("hyst_db") is not None:
        h = 10 ** (-cfg["hyst_db"] / 10)
        a_off = alpha * h
        T_off = gamma_T(n, cfg["floor_pfa"]) * h
    else:
        a_off = os_cfar_alpha(N, k, n, cfg["off_pfa"])
        T_off = gamma_T(n, cfg["off_pfa"])
    if Z is None:
        half = N // 2
        fp = np.ones(2 * (G + half) + 1, bool)
        fp[half:half + 2 * G + 1] = False
        Z = ndimage.rank_filter(P, k - 1, footprint=fp[None, :], mode="mirror")
    guard = 10 ** (cfg["guard_db"] / 10)
    Tf = gamma_T(n, cfg["floor_pfa"])
    fl = floor if floor.ndim == 2 else floor[None, :]
    g_ok = P > guard * fl
    os_on = (P > alpha * Z) & g_ok
    os_off = (P > a_off * Z) & g_ok
    fl_on = P > Tf * fl
    fl_off = P > T_off * fl
    if cfg["mode"] == "os":
        seed, region = os_on, os_off
    elif cfg["mode"] == "floor":
        seed, region = fl_on, fl_off
    else:
        seed, region = os_on | fl_on, os_off | fl_off
    region |= seed
    s4 = ndimage.generate_binary_structure(2, 1)
    lab, nl = ndimage.label(region, structure=s4)
    keep = np.zeros(nl + 1, bool)
    keep[np.unique(lab[seed])] = True
    keep[0] = False
    for i, sl in enumerate(ndimage.find_objects(lab), start=1):
        if sl is not None and keep[i] and (sl[0].stop - sl[0].start) < cfg["min_frames"]:
            keep[i] = False
    kept = keep[lab]
    merged = kept
    if cfg["gap_frames"]:
        merged = ndimage.binary_closing(kept, structure=np.ones((cfg["gap_frames"] + 1, 1), bool)) | kept
    if cfg["merge_bins"]:
        merged = ndimage.binary_closing(merged, structure=np.ones((1, cfg["merge_bins"] + 1), bool)) | merged
    lab2, _ = ndimage.label(merged, structure=s4)
    boxes = []
    for i, sl in enumerate(ndimage.find_objects(lab2), start=1):
        if sl is None:
            continue
        boxes.append((i, sl[0].start, sl[0].stop, sl[1].start, sl[1].stop))
    return dict(boxes=boxes, lab=lab2, kept=kept, seed=seed, region=region, Z=Z, alpha=alpha,
                a_off=a_off, Tf=Tf, T_off=T_off)


def box_records(st, det, floor1d, clip_thr=1e-4):
    """Turn boxes into Detection-like records (docs/07 §2.9 fields)."""
    P, f, fc, df, dt = st["P"], st["f"], st["meta"]["fc"], st["df"], st["dt"]
    lab = det["lab"]
    recs = []
    for (i, t0, t1, b0, b1) in det["boxes"]:
        m = lab[t0:t1, b0:b1] == i
        sub = P[t0:t1, b0:b1]
        fl = floor1d[b0:b1][None, :].repeat(t1 - t0, 0)
        snr = 10 * np.log10(np.maximum(sub[m] / fl[m], 1e-12))
        # burst-gated PSD over the box
        on = m.any(1)
        bp = sub[on].mean(0)
        exc = np.maximum(bp - floor1d[b0:b1], 0)
        fcen = fc + np.sum(exc * f[b0:b1]) / max(np.sum(exc), 1e-30)
        pk = np.argmax(bp)
        # -10 dB bandwidth about the peak within the box
        above = bp >= bp[pk] / 10
        bw10 = above.sum() * df
        recs.append(dict(
            t_start=t0 * dt, t_end=t1 * dt, frames=int(t1 - t0), duty=float(on.mean()),
            f_lo=fc + f[b0], f_hi=fc + f[b1 - 1] + df, f_center=float(fcen),
            f_peak=fc + f[b0 + pk], bw_box=(b1 - b0) * df, bw_10db=float(bw10),
            nbins=int(b1 - b0), b0=int(b0), b1=int(b1),
            snr_peak=float(snr.max()), snr_mean=float(10 * np.log10(np.mean(10 ** (snr / 10)))),
            level_dbfs=float(10 * np.log10(max(np.sum(exc) * df, 1e-30))),
            sk=float(np.nanmedian(spectral_kurtosis(P, st["S2"], st["n_avg"], t0, t1, b0, b1))),
            clipped=bool(st["clip"][t0:t1].max() > clip_thr),
            clip_frac=float(st["clip"][t0:t1].max()),
        ))
    return recs


# -------------------------------------------------------------- masks / flags

def spur_mask_hits(f_lo, f_hi, f_center, bw, ref_hz=10e6, ppm=25.0, min_tol=10e3, narrow_hz=25e3):
    """n x 10 MHz reference-harmonic test: narrow detection whose centre is within
    max(min_tol, ppm*f) of a harmonic, or any detection whose extent covers one."""
    n = np.round(f_center / ref_hz)
    h = n * ref_hz
    tol = max(min_tol, ppm * 1e-6 * h)
    near = abs(f_center - h) <= tol and bw <= narrow_hz
    covers = (f_lo - tol <= h <= f_hi + tol)
    return bool(near), bool(covers and not near), float(h)


def dc_hit(f_lo, f_hi, fc, tol=15e3):
    return (f_lo - tol <= fc <= f_hi + tol)


def edge_hit(f_center, fc, edge_hz=8.0e6):
    return abs(f_center - fc) > edge_hz


def cluster_by_frequency(recs, pad_hz=0.0):
    """Union detections whose *core* extents (f_center +- max(bw_10db, 1 bin)/2) overlap
    into emitter candidates.  Bounding boxes chain across a dense band, cores do not."""
    def core(r):
        half = max(r["bw_10db"], 5e3) / 2
        return r["f_center"] - half, r["f_center"] + half
    order = sorted(range(len(recs)), key=lambda i: core(recs[i])[0])
    groups = []
    for i in order:
        lo, hi = core(recs[i])
        if groups and lo <= groups[-1]["f_hi"] + pad_hz:
            g = groups[-1]
            g["members"].append(i)
            g["f_hi"] = max(g["f_hi"], hi)
        else:
            groups.append(dict(members=[i], f_lo=lo, f_hi=hi))
    return groups


def detect_integrated(Pmean, floor1d, f, fc, df, on_db=6.0, off_db=3.0, min_bins=1):
    """Emitter-level detector on a long-integration spectrum (seconds of averaging).
    With n_eff ~ 10^4 the statistical threshold is ~0.2 dB, so the model-uncertainty
    guard dominates (SNR-wall argument, C08): seed at floor+on_db, extend to floor+off_db."""
    r = Pmean / floor1d
    region = r > 10 ** (off_db / 10)
    seed = r > 10 ** (on_db / 10)
    lab, nl = ndimage.label(region)
    keep = np.zeros(nl + 1, bool)
    keep[np.unique(lab[seed])] = True
    keep[0] = False
    out = []
    for i, sl in enumerate(ndimage.find_objects(lab), start=1):
        if sl is None or not keep[i]:
            continue
        b0, b1 = sl[0].start, sl[0].stop
        if b1 - b0 < min_bins:
            continue
        seg = Pmean[b0:b1]
        exc = np.maximum(seg - floor1d[b0:b1], 0)
        pk = int(np.argmax(seg))
        out.append(dict(f_lo=fc + f[b0], f_hi=fc + f[b1 - 1] + df,
                        f_center=fc + float(np.sum(exc * f[b0:b1]) / max(exc.sum(), 1e-30)),
                        f_peak=fc + f[b0 + pk], b0=int(b0), b1=int(b1), nbins=int(b1 - b0),
                        bw=(b1 - b0) * df,
                        peak_snr_db=float(10 * np.log10(r[b0 + pk])),
                        peak_exc_dbfs=float(10 * np.log10(max(seg[pk] - floor1d[b0 + pk], 1e-30) * df)),
                        level_dbfs=float(10 * np.log10(max(exc.sum() * df, 1e-30)))))
    return out
