"""Matched synthetic bursts for S5: same family / rate / deviation / duration as the real
signals, embedded in complex white noise at a stated in-band SNR (C13 definition:
S / (N0 * OBW99)), noise scaled to the real ADC noise rms, 8-bit quantised, then run through
the *same* decimation + C13 + C14 chain as the real bursts.

Also generates negatives that must come back unknown / untrusted: noise only, CW tone,
analog NBFM (voice-like), LoRa-like chirp.
"""
import numpy as np
from scipy import signal

import s5lib as L

rng = np.random.default_rng(55)


def gaussian_taps(bt, sps, span=4):
    t = np.arange(-span * sps, span * sps + 1) / sps
    a = np.sqrt(np.log(2) / 2) / bt
    h = np.sqrt(np.pi) / a * np.exp(-(np.pi * t / a) ** 2)
    return h / h.sum()


def gen_fsk(rate, dev, nsym, fs, bt=0.5, preamble=32):
    sps = fs / rate
    bits = np.r_[np.tile([0, 1], preamble // 2), rng.integers(0, 2, nsym - preamble)]
    n = int(np.ceil(nsym * sps))
    idx = np.minimum((np.arange(n) / sps).astype(int), nsym - 1)
    nrz = 2.0 * bits[idx] - 1
    if bt:
        nrz = np.convolve(nrz, gaussian_taps(bt, sps), mode="same")
    ph = 2 * np.pi * np.cumsum(dev * nrz) / fs
    return np.exp(1j * ph), bits


def gen_ook(rate, nsym, fs, preamble=16):
    sps = fs / rate
    bits = np.r_[np.tile([1, 0], preamble // 2), rng.integers(0, 2, nsym - preamble)]
    n = int(np.ceil(nsym * sps))
    idx = np.minimum((np.arange(n) / sps).astype(int), nsym - 1)
    env = bits[idx].astype(float)
    env = np.convolve(env, np.ones(max(1, int(sps / 8))) / max(1, int(sps / 8)), mode="same")
    return env.astype(complex), bits


def gen_bpsk(rate, nsym, fs, alpha=0.35):
    sps = int(round(fs / rate))
    sym = 2.0 * rng.integers(0, 2, nsym) - 1
    up = np.zeros(nsym * sps)
    up[::sps] = sym
    t = (np.arange(-6 * sps, 6 * sps + 1)) / sps
    with np.errstate(divide="ignore", invalid="ignore"):
        h = np.sinc(t) * np.cos(np.pi * alpha * t) / (1 - (2 * alpha * t) ** 2)
    h[~np.isfinite(h)] = np.pi / 4 * np.sinc(1 / (2 * alpha))
    return np.convolve(up, h, mode="same").astype(complex), fs / sps


def gen_nbfm_voice(dur, fs, dev=2500):
    n = int(dur * fs)
    audio = signal.lfilter(signal.firwin(255, [300, 3000], fs=fs, pass_zero=False), 1, rng.normal(size=n))
    audio /= np.max(np.abs(audio)) + 1e-9
    return np.exp(2j * np.pi * np.cumsum(dev * audio) / fs)


def gen_chirp(bw, sf, nsym, fs):
    T = 2 ** sf / bw
    n = int(T * fs)
    t = np.arange(n) / fs
    up = np.exp(2j * np.pi * (-bw / 2 * t + bw / (2 * T) * t ** 2))
    return np.tile(up, nsym)


def embed(sig, fs, snr_db, obw_hz, noise_rms=3.3, pad_s=0.004, cfo_hz=0.0):
    """Burst in noise at in-band SNR, noise rms in LSB (complex magnitude), 8-bit quantised."""
    npad = int(pad_s * fs)
    sigma2 = noise_rms ** 2
    N0 = sigma2 / fs
    p_sig = np.mean(np.abs(sig) ** 2) if np.any(sig) else 1.0
    A = np.sqrt(10 ** (snr_db / 10) * N0 * obw_hz / p_sig)
    x = np.zeros(len(sig) + 2 * npad, complex)
    x[npad:npad + len(sig)] = A * sig
    x *= np.exp(2j * np.pi * cfo_hz * np.arange(len(x)) / fs)
    x += (rng.normal(size=len(x)) + 1j * rng.normal(size=len(x))) * np.sqrt(sigma2 / 2)
    return L.quantize_ci8(x), npad


def run_chain(xq, fs, dec, npad):
    """Same as the real path: decimate, noise from the leading pad, C13 then C14."""
    x = xq.astype(np.complex128)
    if dec > 1:
        x = signal.decimate(x, dec, ftype="fir", zero_phase=True) if dec <= 13 else signal.resample_poly(x, 1, dec)
    fso = fs / dec
    npd = npad // dec
    noise = x[: max(64, npd - int(0.001 * fso))]
    p = L.c13_params(x[npd:len(x) - npd], fso, noise)
    r = L.classify_and_estimate(x[npd // 2:len(x) - npd // 2], fso, noise, p["obw99_hz"], p["snr_db"])
    r["c13"] = p
    return r
