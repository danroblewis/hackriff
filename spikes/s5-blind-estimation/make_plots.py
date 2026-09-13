"""Report figures (small PNGs) from the result JSONs.
usage: python3 make_plots.py <results_dir> <png_dir>
"""
import json
import sys

import numpy as np
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt

res, png = sys.argv[1], sys.argv[2]


def curve(rows, key, lo=-3, hi=33, step=3):
    xs, ok, tw, tr = [], [], [], []
    for a in np.arange(lo, hi, step):
        rr = [r for r in rows if r.get(key) is not None and a <= r[key] < a + step]
        if len(rr) < 5:
            continue
        xs.append(a + step / 2)
        good = [r["rate"] is not None and abs(r["rate"] / r["truth_rate"] - 1) < 0.01 for r in rr]
        ok.append(np.mean(good))
        tr.append(np.mean([r["trusted"] for r in rr]))
        tw.append(np.mean([r["trusted"] and not g for r, g in zip(rr, good)]))
    return np.array(xs), np.array(ok), np.array(tr), np.array(tw)


E = json.load(open(res + "/eval915_results.json"))
real_t = [r for r in E["real"] if r["truth_rate"]]
fig, ax = plt.subplots(1, 2, figsize=(10, 3.8))
for rows, lab, c in ((real_t, "real (all truth bursts)", "C0"), (E["degraded"], "real + added noise", "C1"),
                     (E["synth"], "matched synthetic", "C2")):
    x, ok, tr, tw = curve(rows, "snr_db")
    ax[0].plot(x, ok, "-o", color=c, ms=3, label=lab)
    ax[1].plot(x, tr, "-o", color=c, ms=3, label=lab + ": trusted")
    ax[1].plot(x, tw, "--x", color=c, ms=4, label=lab + ": trusted & wrong")
ax[0].set_title("915 MHz FSK bursts: symbol rate within 1%")
ax[1].set_title("trusted fraction / trusted-and-wrong")
for a in ax:
    a.set_xlabel("in-band SNR (C13), dB"); a.grid(alpha=0.3); a.set_ylim(-0.02, 1.02)
ax[0].legend(fontsize=7); ax[1].legend(fontsize=6)
fig.tight_layout(); fig.savefig(png + "/s5_915_rate_vs_snr.png", dpi=80); plt.close(fig)

try:
    fig, ax = plt.subplots(1, 2, figsize=(10, 3.6))
    for lab, c in (("real", "C0"), ("synthetic", "C2")):
        R = json.load(open(f"{res}/rds_{lab}.json"))
        W = sorted(R["windows"], key=float)
        ax[0].semilogy([float(w) for w in W], [max(R["windows"][w]["median_abs_err"] or 1, 1e-9) for w in W],
                       "-o", color=c, ms=3, label=f"{lab} (subcarrier SNR {R['params']['snr_db']:.1f} dB)")
        sw = R["snr_sweep"]
        xs = sorted(sw, key=float)
        ax[1].plot([float(s) for s in xs], [sw[s]["0.5"]["within1pct"] if "0.5" in sw[s] else sw[s][0.5]["within1pct"]
                                            for s in xs], "-o", color=c, ms=3, label=f"{lab}: 0.5 s windows")
    ax[0].axhline(0.01, color="k", lw=0.8, ls="--")
    ax[0].set_xscale("log"); ax[0].set_xlabel("window length, s"); ax[0].set_ylabel("median |rate error| vs 2*Rs")
    ax[0].set_title("RDS biphase symbol rate (truth = pilot/8)"); ax[0].legend(fontsize=7)
    ax[1].set_xlabel("in-band SNR after added noise, dB"); ax[1].set_title("RDS: within 1%")
    ax[1].set_ylim(-0.02, 1.02); ax[1].legend(fontsize=7)
    for a in ax:
        a.grid(alpha=0.3)
    fig.tight_layout(); fig.savefig(png + "/s5_rds.png", dpi=80); plt.close(fig)
except FileNotFoundError:
    pass

try:
    Sw = json.load(open(res + "/synth_sweep_results.json"))
    fig, ax = plt.subplots(1, 2, figsize=(10, 3.8))
    for case in sorted({t["case"] for t in Sw["table"]}):
        tt = [t for t in Sw["table"] if t["case"] == case]
        ax[0].plot([t["snr"] for t in tt], [t["fam_ok"] for t in tt], "-o", ms=3, label=case)
        ax[1].plot([t["snr"] for t in tt], [t["rate_ok"] for t in tt], "-o", ms=3, label=case)
    ax[0].set_title("synthetic: family correct"); ax[1].set_title("synthetic: rate within 1%")
    for a in ax:
        a.set_xlabel("in-band SNR, dB"); a.grid(alpha=0.3); a.set_ylim(-0.02, 1.02)
    ax[0].legend(fontsize=7)
    fig.tight_layout(); fig.savefig(png + "/s5_synth_family_rate.png", dpi=80); plt.close(fig)
except FileNotFoundError:
    pass
