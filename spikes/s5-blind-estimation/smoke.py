"""Quick regression check of the C13/C14 prototype on a few synthetic positives and
negatives (runs in ~1 min). Flags any TRUSTED-WRONG rate or non-unknown negative.
usage: python3 smoke.py
"""
import sys

import numpy as np

sys.path.insert(0, __file__.rsplit("/", 1)[0])
import s5lib as L
import synth as S
import synth_sweep as W


def run(y, fs, obw, pad, fs_out_min=None):
    b = dict(t0=pad, t1=len(y) / fs - pad, f_lo=-obw / 2, f_hi=obw / 2)
    x, fso, _, np_ = L.extract_burst(None, fs, b, pad_s=pad * 0.9, bw_pad=2.0, x_full=y, fs_out_min=fs_out_min)
    noise = x[: max(64, np_ - int(0.0005 * fso))]
    p = L.c13_params(x[np_:len(x) - np_], fso, noise)
    if not p.get("obw99_hz"):
        return dict(family="unknown", reasons=["low_snr"], rate=None, rate_trusted=False)
    return L.classify_and_estimate(x[np_ // 2:len(x) - np_ // 2], fso, noise, p["obw99_hz"], p["snr_db"],
                                   cfo_hz=p["cfo_hz"])


bad = 0
for name in W.CASES:
    gen, fs, fam = W.CASES[name]
    obw = W.clean_obw(name)
    for snr in (12, 20, 30):
        for seed in (3, 4, 5):
            S.rng = np.random.default_rng(seed)
            sig, R, dev = gen(fs)
            y, npad = S.embed(sig, fs, snr, obw, noise_rms=3.21, pad_s=0.004, cfo_hz=300.0)
            r = run(y, fs, obw, npad / fs)
            ok = r["rate"] is not None and abs(r["rate"] / R - 1) < 0.01
            flag = ""
            if r["rate_trusted"] and not ok:
                flag = "TRUSTED-WRONG"
            if r["family"] not in (fam, "unknown"):
                flag += " WRONG-FAMILY"
            bad += bool(flag)
            print(f"{name:14s} {snr:2d} s{seed} fam {r['family']:7s} rate {r['rate'] and round(r['rate'], 1)} "
                  f"ok {ok} trusted {r['rate_trusted']} dev {r.get('deviation_hz') and round(r['deviation_hz'])}/{dev} "
                  f"{flag} {r['reasons']}")
fs = 1e6
for kind in ("chirp", "nbfm", "cw", "noise"):
    for seed in (4, 5, 6, 7, 8):
        S.rng = np.random.default_rng(seed)
        if kind == "chirp":
            sig, obw = S.gen_chirp(125e3, 7, 8, fs), 125e3
        elif kind == "nbfm":
            sig, obw = S.gen_nbfm_voice(0.1, fs), 8e3
        elif kind == "cw":
            sig, obw = np.exp(2j * np.pi * 1000 * np.arange(20000) / fs), 2e3
        else:
            sig, obw = np.zeros(20000, complex), 20e3
        y, npad = S.embed(sig, fs, 30, obw, noise_rms=3.21, pad_s=0.004)
        r = run(y, fs, max(obw, 20e3), npad / fs, 0)
        flag = ("LABELLED " if r["family"] != "unknown" else "") + ("TRUSTED" if r["rate_trusted"] else "")
        bad += bool(flag)
        print(f"NEG {kind:6s} s{seed} fam {r['family']:7s} rate {r['rate'] and round(r['rate'])} "
              f"trusted {r['rate_trusted']} {flag} {r['reasons']}")
print("problems:", bad)
