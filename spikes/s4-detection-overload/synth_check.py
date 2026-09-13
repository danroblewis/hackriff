"""Synthetic validation for S4: (1) OS-CFAR alpha vs closed form, (2) per-cell Pfa on
Gamma noise and on 8-bit-quantised low-level noise, (3) floor-estimator bias vs
occupancy, (4) false-component rate of the full chain on pure noise.
Writes results/synth_*.json."""
import json, os, time
import numpy as np
import scipy.fft as sfft
from scipy import signal
import s4lib as L

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(HERE, "results")
os.makedirs(OUT, exist_ok=True)
rng = np.random.default_rng(1)
res = {}

# (1) alpha
a_num = L.os_cfar_alpha(32, 24, 1, 1e-6)
a_ex = L.os_cfar_alpha_exact_n1(32, 24, 1e-6)
res["alpha_check_n1"] = dict(numeric=a_num, exact=a_ex)
res["alpha_n10"] = {str(p): L.os_cfar_alpha(32, 24, 10, p) for p in [1e-2, 1e-3, 1e-4, 1e-5, 1e-6]}
res["alpha_n10_dB"] = {k: 10 * np.log10(v) for k, v in res["alpha_n10"].items()}
res["floorT_n10_dB"] = {str(p): 10 * np.log10(L.gamma_T(10, p)) for p in [1e-3, 1e-4, 1e-5, 1e-6]}
print("alpha", res["alpha_check_n1"], res["alpha_n10_dB"], flush=True)


def os_pfa(P, pfa, N=32, G=4, k=24, n=10):
    from scipy import ndimage
    a = L.os_cfar_alpha(N, k, n, pfa)
    half = N // 2
    fp = np.ones(2 * (G + half) + 1, bool)
    fp[half:half + 2 * G + 1] = False
    Z = ndimage.rank_filter(P, k - 1, footprint=fp[None, :], mode="mirror")
    R = P / Z
    return float(np.mean(R[:, 64:-64] > a))


# (2a) ideal Gamma noise
P = rng.gamma(10, 0.1, size=(2500, 4096)).astype(np.float32)
res["pfa_gamma"] = {str(p): os_pfa(P, p) for p in [1e-2, 1e-3, 1e-4, 1e-5]}
print("pfa gamma", res["pfa_gamma"], flush=True)

# (2b) 8-bit quantised Gaussian noise at the level seen in the low-gain captures
# (std ~0.5 code per rail, DC offset ~(0.6, -2.1) codes), through the same STFT.
def quantised_frames(sigma_codes, frames=1500, nfft=4096, n_avg=10, dc=(0.57, -2.1)):
    w = signal.windows.hann(nfft, sym=False).astype(np.float32)
    out = np.empty((frames, nfft), np.float32)
    for i in range(frames):
        g = rng.normal(0, sigma_codes, size=(n_avg * nfft, 2))
        q = np.clip(np.round(g + np.array(dc)), -128, 127)
        x = (q[:, 0] + 1j * q[:, 1]).astype(np.complex64).reshape(n_avg, nfft)
        X = sfft.fft(x * w, axis=1)
        out[i] = sfft.fftshift(np.mean(np.abs(X) ** 2, 0))
    return out

res["pfa_quantised"] = {}
for sig in [0.35, 0.5, 1.2]:
    Q = quantised_frames(sig)
    Q = Q[:, np.r_[100:2000, 2100:4000]]  # drop DC region
    res["pfa_quantised"][str(sig)] = {str(p): os_pfa(Q, p) for p in [1e-2, 1e-3, 1e-4, 1e-5]}
print("pfa quantised", res["pfa_quantised"], flush=True)

# (3) floor-estimator bias vs occupancy (continuous signals and bursty signals)
def occupied(occ, frames=300, nb=4096, n=10, bursty=False):
    truth = 1.0
    P = rng.gamma(n, truth / n, size=(frames, nb))
    mask = np.zeros(nb, bool)
    tries = 0
    while mask.mean() < occ and tries < 10000:
        tries += 1
        wdt = int(rng.integers(5, 200))
        b = int(rng.integers(0, nb - wdt))
        if mask[b:b + wdt].any():
            continue
        snr = 10 ** (rng.uniform(3, 30) / 10)
        if bursty:
            on = rng.random(frames) < 0.3
        else:
            on = np.ones(frames, bool)
        P[on, b:b + wdt] += rng.gamma(n, snr / n, size=(on.sum(), wdt))
        mask[b:b + wdt] = True
    return P.astype(np.float32), mask.mean()

rows = []
for bursty in [False, True]:
    for occ in [0.0, 0.2, 0.4, 0.6, 0.8]:
        P, got = occupied(occ, bursty=bursty)
        e = {}
        fl = L.blockwise(L.fcme, P, 256, 64, n=10, pfa=1e-3)
        e["fcme_block256"] = float(np.median(10 * np.log10(np.median(fl, 0))))
        fl = L.blockwise(L.percentile_floor, P, 256, 64, n=10, p=0.5)
        e["pct50_block256"] = float(np.median(10 * np.log10(np.median(fl, 0))))
        fl = L.blockwise(L.percentile_floor, P, 256, 64, n=10, p=0.2)
        e["pct20_block256"] = float(np.median(10 * np.log10(np.median(fl, 0))))
        ms, _ = L.minstat_floor(P, 10, win=256)
        e["minstat_perbin_w256"] = float(np.median(10 * np.log10(ms[-1])))
        rows.append(dict(bursty=bursty, occupancy=round(got, 2), **e))
        print(rows[-1], flush=True)
res["floor_bias_db"] = rows
# (4) the chain false-component rate is in synth_chain_fa.py
json.dump(res, open(os.path.join(OUT, "synth_check.json"), "w"), indent=1, default=float)
