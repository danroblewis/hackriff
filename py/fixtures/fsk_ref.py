"""Decoder-independent PHY truth for the 902-928 MHz FHSS 2-FSK bursts (promoted from spike S5:
``spikes/s5-blind-estimation/{s5lib,fsk_truth,run_detect,eval915}.py``; same parameters).

1. Detect bursts: STFT (nfft 1024, 4 averages), per-bin median floor, 12 dB threshold,
   dilation, connected components (>= 3 frames) -> time/frequency boxes.
2. For each burst, demodulate at *fixed standard rates* R in {50,100,150,200,300} kbit/s
   (channel filter, discriminator, best eye phase; none of the blind estimators). R is truth iff
   >= 24 alternating preamble bits are immediately followed by sync ``0000110001011111`` (<= 1 bit
   error) at exactly one R.
3. Deviation reference: median |IF - centre| at symbol centres inside runs of >= 3 equal bits on
   that fixed-rate path (self-consistent, not a spec value).
4. In-band SNR over the box: noise-subtracted Welch OBW99, N0 = mean density of the 2 ms pad
   before the burst (S5 C13 definition, ``snr_box_db``).

Only PHY metadata leaves this module: rate, preamble length, sync bit position, deviation,
centre frequency, SNR. **No payload bits are returned or stored** (unidentified third-party
traffic; CLAUDE.md legal guardrails).
"""

from __future__ import annotations

import numpy as np
from scipy import ndimage, signal

RATES = (50e3, 100e3, 150e3, 200e3, 300e3)
SYNC = "0000110001011111"
PAD_S = 0.002


def load_ci8(fn, start=0, count=None):
    raw = np.memmap(fn, dtype=np.int8, mode="r")
    n = len(raw) // 2
    if count is None:
        count = n - start
    count = max(0, min(count, n - start))
    a = np.asarray(raw[2 * start : 2 * (start + count)], dtype=np.float32).reshape(-1, 2)
    return (a[:, 0] + 1j * a[:, 1]).astype(np.complex64)


def detect_bursts(fn, fs, nfft=1024, avg=4, thr_db=12.0, min_frames=3, dc_guard_bins=2,
                  chunk_frames=4096):
    n = len(np.memmap(fn, dtype=np.int8, mode="r")) // 2
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
    R[:, c - dc_guard_bins : c + dc_guard_bins + 1] = 0.0
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
                           f_hi=(fsl.stop - c) * fbin, peak_db=float(sub.max()),
                           frame_start=int(tsl.start), frame_stop=int(tsl.stop), hop=hop))
    return bursts


def decimate(x, dec):
    if dec <= 1:
        return x
    return signal.decimate(x, dec, ftype="fir", zero_phase=True) if dec <= 13 else \
        signal.resample_poly(x, 1, dec)


def extract_burst(fn, fs, b, pad_s=PAD_S, bw_pad=2.0, fs_out_min=1.2e6):
    fcen = 0.5 * (b["f_lo"] + b["f_hi"])
    span = max(b["f_hi"] - b["f_lo"], 1.0)
    want = max(span * bw_pad * 2, fs_out_min or 0)
    dec = max(1, int(fs // want))
    s0 = max(0, int((b["t0"] - pad_s) * fs))
    s1 = int((b["t1"] + pad_s) * fs)
    x = load_ci8(fn, s0, s1 - s0).astype(np.complex128)
    x = x * np.exp(-2j * np.pi * fcen * (np.arange(len(x)) + s0) / fs)
    x = decimate(x, dec)
    return x.astype(np.complex128), fs / dec, fcen, int(pad_s * fs / dec)


def lowpass(x, fs, cutoff, ntaps=None):
    cutoff = min(cutoff, 0.45 * fs)
    ntaps = ntaps or (int(min(1025, max(31, 8 * fs / cutoff))) | 1)
    h = signal.firwin(ntaps, cutoff, fs=fs)
    return signal.filtfilt(h, 1.0, x) if len(x) > 3 * ntaps else x


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


def moving_avg(v, L):
    L = max(1, int(L))
    return v if L == 1 else np.convolve(v, np.ones(L) / L, mode="same")


def inst_freq(x, fs):
    return fs / (2 * np.pi) * np.angle(x[1:] * np.conj(x[:-1]))


def c13_snr(x, fs, noise_x):
    """In-band SNR over the box (dB), OBW99 and CFO; S5 ``c13_params`` with a noise pad."""
    n = len(x)
    nfft = int(2 ** np.clip(np.round(np.log2(max(n, 256) / 8)), 8, 14))
    f, P = signal.welch(x, fs, nperseg=min(nfft, n), return_onesided=False, scaling="density")
    f, P = np.fft.fftshift(f), np.fft.fftshift(P)
    _, Pn = signal.welch(noise_x, fs, nperseg=min(nfft, len(noise_x)), return_onesided=False)
    N0 = float(np.mean(Pn))
    Ps = np.convolve(np.clip(P - N0, 0, None), np.ones(5) / 5, mode="same")
    if Ps.sum() <= 0:
        return None
    c = np.cumsum(Ps) / Ps.sum()
    lo = f[np.searchsorted(c, 0.005)]
    hi = f[min(len(f) - 1, np.searchsorted(c, 0.995))]
    band = (f >= lo) & (f <= hi)
    df = f[1] - f[0]
    B = hi - lo + df
    Psig = (P[band] - N0).sum() * df
    snr = 10 * np.log10(max(Psig, 1e-12 * N0 * B) / (N0 * B))
    cfo = float((f[band] * Ps[band]).sum() / max(Ps[band].sum(), 1e-30))
    return dict(obw99_hz=float(B), cfo_hz=cfo, snr_db=float(snr))


def demod_fixed(x, fs, R, npad):
    sps = fs / R
    xf = lowpass(x, fs, 1.0 * R)
    fi = inst_freq(xf, fs)
    v = moving_avg(fi, max(1, int(round(sps * 0.6))))
    nz = np.sqrt(np.mean(np.abs(xf[: max(16, npad // 2)]) ** 2))
    best = None
    for ph in np.linspace(0, sps, 16, endpoint=False):
        idx = (ph + np.arange(int((len(v) - 1 - ph) / sps)) * sps).astype(int)
        s = v[idx]
        on = np.abs(xf[idx]) > 3 * nz
        if on.sum() < 32:
            continue
        q, _ = kmeans_1d(s[on], 2)
        mid = q.mean()
        sc = np.mean(np.abs(s[on] - mid))
        if best is None or sc > best[0]:
            best = (sc, ph, s, on, mid, idx)
    return best


def find_sync(bits):
    bs = "".join(map(str, bits))
    for pol in (0, 1):
        b = bs if pol == 0 else bs.translate(str.maketrans("01", "10"))
        k = b.find("01" * 12)
        while k >= 0:
            j = k
            while b[j : j + 2] == "01":
                j += 2
            for jj in (j, j - 1, j + 1):
                w = b[jj : jj + 16]
                if len(w) == 16 and sum(c1 != c2 for c1, c2 in zip(w, SYNC)) <= 1:
                    return dict(polarity=pol, preamble_bits=jj - k, sync_bit=jj,
                                sync_bit_errors=sum(c1 != c2 for c1, c2 in zip(w, SYNC)))
            k = b.find("01" * 12, j + 2)
    return None


def truth_for(x, fs, npad):
    """Fixed-rate trial demodulation. Returns PHY metadata hits only (bits are discarded here)."""
    hits = []
    for R in RATES:
        if fs / R < 3:
            continue
        best = demod_fixed(x, fs, R, npad)
        if best is None:
            continue
        _, ph, s, on, mid, idx = best
        st = np.where(on)[0]
        bits = (s > mid).astype(int)[st[0] : st[-1] + 1]
        sy = find_sync(bits)
        if not sy:
            continue
        on_sub = on[st[0] : st[-1] + 1]
        full = np.zeros(len(bits), bool)
        for i in range(1, len(bits) - 1):
            full[i] = bits[i - 1] == bits[i] == bits[i + 1] and on_sub[i - 1] and on_sub[i] and on_sub[i + 1]
        vals = s[st[0] : st[-1] + 1][full] - mid
        dev_ref = float(np.median(np.abs(vals))) if len(vals) > 8 else None
        # sample index (in x) of the first sync bit's centre
        sync_sample = int(idx[st[0] + sy["sync_bit"]]) if st[0] + sy["sync_bit"] < len(idx) else None
        hits.append(dict(rate=R, n_bits=int(len(bits)), dev_ref_hz=dev_ref, if_mid_hz=float(mid),
                         sync_sample_in_snippet=sync_sample, **sy))
        del bits
    return hits


def burst_truth(fn, fs, fc, b):
    """Truth dict for one detected burst (PHY metadata only)."""
    x, fso, fcen, npad = extract_burst(fn, fs, b)
    noise = x[: max(64, npad - int(0.0005 * fso))]
    body = x[npad : len(x) - npad]
    snr = c13_snr(body, fso, noise) if len(body) >= 256 else None
    hits = truth_for(x, fso, npad)
    rates = sorted({h["rate"] for h in hits})
    out = dict(box_center_offset_hz=float(fcen), snr=snr, hits=hits,
               truth_rate=rates[0] if len(rates) == 1 else None, ambiguous=len(rates) > 1,
               snippet_start_s=max(0.0, b["t0"] - PAD_S), snippet_fs=fso)
    return out
