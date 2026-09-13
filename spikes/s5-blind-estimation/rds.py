"""S5 RDS: blind symbol-rate estimation on the 57 kHz RDS subcarrier of a real FM capture.

Truth: RDS data rate = 57000/48 = 1187.5 bd, locked to the 19 kHz pilot (x3 = 57 kHz).
The pilot is measured in the capture so the truth is expressed in *our* sample clock:
Rs_true = f_pilot_measured / 16 (removes HackRF crystal ppm). Biphase chip rate = 2 Rs.

Paths:
  real       : capture -> FM discriminator -> subcarrier (mixed at a deliberately wrong
               56.7 kHz; C13 must recover the offset) -> C13/C14 per window length
  real+noise : real subcarrier baseband + added white noise to sweep in-band SNR
  synthetic  : matched MPX (pilot, mono/stereo noise audio, RDS at the measured injection)
               -> FM at 2.4 Msps, +500 kHz, RF noise matched to the measured CNR, scaled to
               the real ADC rms and 8-bit quantised -> the identical pipeline

usage: python3 rds.py <fm.sigmf-data> <outdir> [seconds=30]
"""
import json
import sys

import numpy as np
from scipy import signal

sys.path.insert(0, __file__.rsplit("/", 1)[0])
import s5lib as L

RDS_RATE = 1187.5
F_GUESS = 56700.0
FS_BB = 12000.0
rng = np.random.default_rng(5)


def fm_mpx(fs, f_off, dec=10, fn=None, x=None, seconds=None):
    taps = signal.firwin(129, 110e3, fs=fs)
    n = (L.n_samples(fn) if x is None else len(x))
    if seconds:
        n = min(n, int(seconds * fs))
    zi = np.zeros(len(taps) - 1, complex)
    out, prev, pos = [], None, 0
    blk = int(fs) - int(fs) % dec
    while pos < n:
        m = min(blk, n - pos)
        m -= m % dec
        if m == 0:
            break
        c = (L.load_ci8(fn, pos, m) if x is None else x[pos:pos + m]).astype(np.complex128)
        c *= np.exp(-2j * np.pi * f_off * (np.arange(m) + pos) / fs)
        y, zi = signal.lfilter(taps, 1.0, c, zi=zi)
        y = y[::dec]
        if prev is not None:
            y = np.r_[prev, y]
            d = np.angle(y[1:] * np.conj(y[:-1]))
        else:
            d = np.r_[0.0, np.angle(y[1:] * np.conj(y[:-1]))]
        out.append(d.astype(np.float32))
        prev = y[-1:]
        pos += m
    return np.concatenate(out).astype(np.float64) * (fs / dec) / (2 * np.pi), fs / dec


def tone(mpx, fs, f0, bw=40.0, off=50.0):
    """Precise tone frequency: mix to +off Hz (not DC: the line finder removes the mean),
    decimate to 1 kHz, interpolated FFT peak within off +- bw."""
    t = np.arange(len(mpx)) / fs
    b = signal.resample_poly(mpx * np.exp(-2j * np.pi * (f0 - off) * t), 1, int(fs // 1000))
    fsb = fs / int(fs // 1000)
    f, s = L.spectral_line(b, fsb, off - bw, off + bw, zero_pad=4)
    amp = 2 * np.abs(np.mean(b * np.exp(-2j * np.pi * f * np.arange(len(b)) / fsb)))
    return f0 - off + f, s, amp


def to_bb(mpx, fs, f_mix, cutoff):
    t = np.arange(len(mpx)) / fs
    dec = int(round(fs / FS_BB))
    taps = signal.firwin(801, cutoff, fs=fs)
    y = signal.lfilter(taps, 1.0, mpx * np.exp(-2j * np.pi * f_mix * t))[400::dec]
    return y


def sub_params(bb_narrow, bb_wide, fs):
    """C13 on the subcarrier: N0 from the upper guard band (+3.4..+5.5 kHz from the guess),
    noise-subtracted OBW99, centroid CFO refined by the x^2 conjugate line, in-band SNR."""
    f, P = signal.welch(bb_wide, fs, nperseg=1024, return_onesided=False)
    N0 = float(np.median(P[(f > 3400) & (f < 5500)]))
    f, P = signal.welch(bb_narrow, fs, nperseg=1024, return_onesided=False)
    f, P = np.fft.fftshift(f), np.fft.fftshift(P)
    band = np.abs(f) < 3200
    Ps = np.clip(P - N0, 0, None) * band
    c = np.cumsum(Ps) / Ps.sum()
    lo, hi = f[np.searchsorted(c, 0.005)], f[np.searchsorted(c, 0.995)]
    df = f[1] - f[0]
    inb = (f >= lo) & (f <= hi)
    snr = 10 * np.log10(Ps[inb].sum() * df / (N0 * (hi - lo + df)))
    cfo_c = float((f[inb] * Ps[inb]).sum() / Ps[inb].sum())
    c2, f2, _ = L.carrier_lines(bb_narrow[: int(4 * fs)], fs, 2)
    return dict(obw99_hz=float(hi - lo + df), cfo_centroid_hz=cfo_c, cfo_x2_hz=f2, x2_coh=c2,
                snr_db=float(snr), N0=N0)


def windows_eval(bb, fs, obw, snr, truth, lengths, noise_var, max_windows=40):
    """Score each window against the biphase *chip* rate 2*Rs (the physical symbol rate C14
    must find; data rate = chip/2 is a line-code decision for C21) and record whether the
    data rate Rs is offered as the harmonic alternative."""
    chip = 2 * truth
    res = {}
    for W in lengths:
        n = int(W * fs)
        errs, fams, trusted, raw, alt_ok, data_ok = [], [], [], [], [], []
        for k in range(min(max_windows, len(bb) // n)):
            xw = bb[k * n:(k + 1) * n]
            nx = (rng.normal(size=4096) + 1j * rng.normal(size=4096)) * np.sqrt(noise_var / 2)
            r = L.classify_and_estimate(xw, fs, nx, obw, snr, rs_max=6000, rate_fmin=300)
            fams.append(r["family"])
            trusted.append(bool(r["rate_trusted"]))
            errs.append(None if r["rate"] is None else r["rate"] / chip - 1)
            data_ok.append(r["rate"] is not None and abs(r["rate"] / truth - 1) < 0.01)
            alts = (r.get("consensus") or {}).get("harmonic_alternatives", [])
            alt_ok.append(any(abs(a / truth - 1) < 0.01 for a in alts))
            raw.append({m["method"]: (round(m["value"], 2) if m.get("value") else None, round(m.get("sig_db", 0), 1))
                        for m in r.get("rate_methods", [])})
        e = np.array([v for v in errs if v is not None])
        ok = np.array([v is not None and abs(v) < 0.01 for v in errs])
        tr = np.array(trusted)
        res[W] = dict(n=len(errs), within1pct=float(ok.mean()) if len(ok) else 0.0,
                      trusted_frac=float(tr.mean()) if len(tr) else 0.0,
                      trusted_and_wrong=int(np.sum(tr & ~ok)),
                      trusted_and_right=int(np.sum(tr & ok)),
                      data_rate_top1=int(np.sum(data_ok)), data_rate_as_alt=int(np.sum(alt_ok)),
                      median_abs_err=float(np.median(np.abs(e))) if len(e) else None,
                      fam={f: fams.count(f) for f in set(fams)}, example_methods=raw[0] if raw else None)
    return res


def analyse(mpx, fs, label, out):
    fp, sp, ap = tone(mpx, fs, 19000.0)
    truth = fp / 16.0
    print(f"[{label}] pilot {fp:.3f} Hz (sig {sp:.0f} dB, dev {ap:.0f} Hz) -> Rs truth {truth:.4f} bd "
          f"(clock {(fp/19000-1)*1e6:+.1f} ppm)")
    bb_w = to_bb(mpx, fs, F_GUESS, 5800)
    bb = to_bb(mpx, fs, F_GUESS, 3300)
    p = sub_params(bb, bb_w, FS_BB)
    cfo = p["cfo_x2_hz"]
    print(f"[{label}] subcarrier: OBW99 {p['obw99_hz']:.0f} Hz, CFO centroid {p['cfo_centroid_hz']:+.1f} "
          f"x2 {cfo:+.2f} Hz (true {fp*3-F_GUESS:+.2f}), SNR {p['snr_db']:.1f} dB, x2coh {p['x2_coh']:.2f}")
    t = np.arange(len(bb)) / FS_BB
    bbc = bb * np.exp(-2j * np.pi * cfo * t)
    noise_var = p["N0"] * 6600
    lengths = [0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0]
    res = dict(label=label, pilot_hz=fp, truth_bd=truth, params=p, cfo_true_hz=fp * 3 - F_GUESS,
               windows=windows_eval(bbc, FS_BB, p["obw99_hz"], p["snr_db"], truth, lengths, noise_var))
    for W, r in res["windows"].items():
        print(f"   W {W:5.2f}s n {r['n']:3d} within1% {r['within1pct']:.2f} trusted {r['trusted_frac']:.2f} "
              f"trusted&wrong {r['trusted_and_wrong']} med|err| {r['median_abs_err']} fam {r['fam']}")
    print("   methods (first 1 s window):", res["windows"][1.0]["example_methods"])
    # SNR sweep by adding noise to the subcarrier baseband (same band-limit as the signal path)
    S = (10 ** (p["snr_db"] / 10)) * p["N0"] * p["obw99_hz"]
    sweep = {}
    taps = signal.firwin(201, 3300, fs=FS_BB)
    for target in [20, 15, 12, 10, 8, 6, 4, 2, 0, -3, -6, -9, -12]:
        if target >= p["snr_db"] - 0.5:
            continue
        N0_add = S / (p["obw99_hz"] * 10 ** (target / 10)) - p["N0"]
        w = (rng.normal(size=len(bbc)) + 1j * rng.normal(size=len(bbc))) * np.sqrt(N0_add * FS_BB / 2)
        xn = bbc + signal.lfilter(taps, 1.0, w)
        nv = (p["N0"] + N0_add) * 6600
        sweep[target] = windows_eval(xn, FS_BB, p["obw99_hz"], target, truth, [0.5, 2.0], nv, max_windows=20)
        s05, s2 = sweep[target][0.5], sweep[target][2.0]
        print(f"   SNR {target:+3d} dB: W0.5 within1% {s05['within1pct']:.2f} trusted {s05['trusted_frac']:.2f} "
              f"t&w {s05['trusted_and_wrong']} fam {s05['fam']} | W2 within1% {s2['within1pct']:.2f} "
              f"trusted {s2['trusted_frac']:.2f} t&w {s2['trusted_and_wrong']} fam {s2['fam']}")
    res["snr_sweep"] = sweep
    json.dump(res, open(f"{out}/rds_{label}.json", "w"), indent=1, default=float)
    return res, bbc


def rf_cnr(fn, fs, f_off):
    x = L.load_ci8(fn, 0, int(2 * fs))
    f, P = signal.welch(x, fs, nperseg=8192, return_onesided=False)
    N0 = np.percentile(P, 20)
    ch = np.abs(f - f_off) < 100e3
    df = f[1] - f[0]
    Pch = (P[ch] - N0).clip(0).sum() * df
    return float(10 * np.log10(Pch / (N0 * 200e3))), float(np.sqrt(np.mean(np.abs(x) ** 2))), float(N0)


def synth_capture(mpx_real, fs_mpx, fs, f_off, seconds, cnr_db, rms, pilot_dev, rds_dev):
    """Matched synthetic FM+RDS capture, 8-bit quantised ci8-like complex array."""
    n = int(seconds * fs_mpx)
    t = np.arange(n) / fs_mpx
    # audio: coloured noise with the real MPX band powers
    f, P = signal.welch(mpx_real[: int(10 * fs_mpx)], fs_mpx, nperseg=4096)
    def band_pow(a, b):
        m = (f >= a) & (f < b)
        return P[m].sum() * (f[1] - f[0])
    mono = signal.lfilter(signal.firwin(301, [50, 15000], fs=fs_mpx, pass_zero=False), 1, rng.normal(size=n))
    mono *= np.sqrt(band_pow(50, 15000) / np.var(mono))
    st = signal.lfilter(signal.firwin(301, 15000, fs=fs_mpx), 1, rng.normal(size=n)) * np.cos(2 * np.pi * 38000 * t)
    st *= np.sqrt(max(band_pow(23000, 53000), 1e-9) / np.var(st))
    pilot = pilot_dev * np.cos(2 * np.pi * 19000 * t)
    # RDS: differential + biphase at 1187.5 bd, cos shaping, DSB-SC at 57 kHz
    fsg = RDS_RATE * 200
    nb = int(seconds * RDS_RATE) + 4
    bits = rng.integers(0, 2, nb)
    diff = np.bitwise_xor.accumulate(bits)
    sym = 2.0 * diff - 1
    imp = np.zeros(nb * 200)
    imp[0::200] = sym
    imp[100::200] = -sym
    ff = np.linspace(0, fsg / 2, 512)
    H = np.where(ff <= 2 * RDS_RATE, np.cos(np.pi * ff / (4 * RDS_RATE)), 0.0)
    shp = signal.firwin2(1201, ff, H, fs=fsg)
    base = signal.lfilter(shp, 1, imp)
    base = signal.resample_poly(base, 96, 95)[:n]
    base *= rds_dev / np.sqrt(np.mean(base ** 2))  # rms of the RDS baseband waveform (Hz dev)
    rds = base * np.cos(2 * np.pi * 57000 * t[: len(base)])
    m = mono + st + pilot
    m[: len(rds)] += rds
    up = int(round(fs / fs_mpx))
    mu = signal.resample_poly(m, up, 1)
    ph = 2 * np.pi * np.cumsum(mu) / fs
    tt = np.arange(len(mu)) / fs
    s = np.exp(1j * (ph + 2 * np.pi * f_off * tt))
    # noise density for the CNR in 200 kHz; total rms scaled to the real ADC rms
    N0 = 1.0 / (10 ** (cnr_db / 10) * 200e3)
    w = (rng.normal(size=len(s)) + 1j * rng.normal(size=len(s))) * np.sqrt(N0 * fs / 2)
    y = s + w
    y *= rms / np.sqrt(np.mean(np.abs(y) ** 2))
    return L.quantize_ci8(y)


if __name__ == "__main__":
    fn, out = sys.argv[1], sys.argv[2]
    secs = float(sys.argv[3]) if len(sys.argv) > 3 else 30.0
    meta = json.load(open(fn.replace(".sigmf-data", ".sigmf-meta")))
    fs = meta["global"]["core:sample_rate"]
    f_off = 101.3e6 - meta["captures"][0]["core:frequency"]
    cnr, rms, _ = rf_cnr(fn, fs, f_off)
    print(f"real RF: CNR(200k) {cnr:.1f} dB, ADC rms {rms:.1f} LSB, station offset {f_off/1e3:+.0f} kHz")
    mpx, fsm = fm_mpx(fs, f_off, fn=fn, seconds=secs)
    real, _ = analyse(mpx, fsm, "real", out)
    _, _, pdev = tone(mpx, fsm, 19000.0)
    # complex baseband of A*d(t)*cos(57k) is A*d/2, so bb power S -> waveform rms 2*sqrt(S);
    # then calibrate twice on 5 s so the synthetic subcarrier SNR matches the real one
    S_bb = 10 ** (real["params"]["snr_db"] / 10) * real["params"]["N0"] * real["params"]["obw99_hz"]
    rds_dev = 2 * np.sqrt(S_bb)
    for it in range(2):
        m5, _ = fm_mpx(fs, f_off, x=synth_capture(mpx, fsm, fs, f_off, 5.0, cnr, rms, pdev, rds_dev))
        ps = sub_params(to_bb(m5, fsm, F_GUESS, 3300), to_bb(m5, fsm, F_GUESS, 5800), FS_BB)
        print(f"  calib {it}: RDS rms dev {rds_dev:.0f} Hz -> synthetic subcarrier SNR {ps['snr_db']:.1f} dB "
              f"(real {real['params']['snr_db']:.1f})")
        rds_dev *= 10 ** ((real["params"]["snr_db"] - ps["snr_db"]) / 20)
    print(f"matched synth: pilot dev {pdev:.0f} Hz, RDS rms dev {rds_dev:.0f} Hz, CNR {cnr:.1f} dB")
    ys = synth_capture(mpx, fsm, fs, f_off, min(secs, 20.0), cnr, rms, pdev, rds_dev)
    mpx_s, _ = fm_mpx(fs, f_off, x=ys)
    analyse(mpx_s, fsm, "synthetic", out)
