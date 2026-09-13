"""Figures for REPORT.md (reads cache/ and results/; run after analyze_iq.py,
sweep_survey.py, imd_control.py, synth_check.py)."""
import csv, json, os
import numpy as np
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt

HERE = os.path.dirname(os.path.abspath(__file__))
RES = os.path.join(HERE, "results")
CACHE = os.path.join(HERE, "cache")
FIG = os.path.join(HERE, "figs")
os.makedirs(FIG, exist_ok=True)
plt.rcParams.update({"font.size": 8, "axes.grid": True, "grid.alpha": 0.3})
CAT_COL = {"raster_station": "#1b7837", "station_part": "#7fbf7b", "comb": "#e08214", "unexplained": "#b2182b",
           "unverified": "#762a83", "suppressed": "#4d4d4d"}


def db(x):
    return 10 * np.log10(np.maximum(x, 1e-30))


def emitters(name):
    out = []
    for r in csv.DictReader(open(os.path.join(RES, f"emitters_{name}.csv"))):
        flags = r["flags"].split(";") if r["flags"] else []
        if "edge" in flags:
            continue
        sup = any(f in flags for f in ("spur_candidate", "image_candidate", "dc", "lo_relative", "suspect_imd"))
        out.append((float(r["f_center_mhz"]), "suppressed" if sup else (r["category"] or "unverified"), flags))
    return out


def save(fig, name):
    p = os.path.join(FIG, name)
    fig.savefig(p, dpi=90)
    plt.close(fig)
    print(name, os.path.getsize(p) // 1024, "KB")


def fig_fm():
    caps = [("urban_98M_20M_l8g10a0", "98 MHz, LNA 8 / VGA 10 / amp off (quantisation-limited)"),
            ("urban_98M_20M_l24g20a0", "98 MHz, LNA 24 / VGA 20 / amp off"),
            ("urban_98M_20M_l32g30a1", "98 MHz, LNA 32 / VGA 30 / amp on (1.2% samples clipped)")]
    fig, ax = plt.subplots(3, 1, figsize=(11, 8), sharex=True)
    for a, (n, title) in zip(ax, caps):
        c = np.load(os.path.join(CACHE, n + ".npz"))
        f = c["f_abs"] / 1e6
        a.plot(f, db(c["Pmean"]), lw=0.4, color="#2166ac", label="4 s mean PSD")
        a.plot(f, db(c["floor_fcme"]), lw=0.8, color="k", label="FCME floor")
        top = db(c["Pmean"]).max()
        seen = set()
        for fc, cat, flags in emitters(n):
            lab = cat if cat not in seen else None
            seen.add(cat)
            a.axvline(fc, color=CAT_COL[cat], lw=0.8, alpha=0.8, ymin=0.9, ymax=1.0, label=lab)
        a.set_title(title)
        a.set_ylabel("dBFS/Hz")
        a.axvspan(88, 90, color="0.9")
        a.axvspan(106, 108, color="0.9")
        a.legend(loc="lower left", ncol=4, fontsize=6)
    ax[-1].set_xlabel("MHz (grey: band-edge zone excluded)")
    fig.tight_layout()
    save(fig, "fig1_fm_three_gains.png")


def fig_gainstep():
    rows = [r for r in csv.DictReader(open(os.path.join(RES, "gainstep_rows.csv"))) if r["step"] == "98 mid->high"]
    ctl = json.load(open(os.path.join(RES, "imd_control.json")))
    fig, ax = plt.subplots(1, 2, figsize=(11, 4))
    col = {"linear": "#1b7837", "compressed": "#2166ac", "suspect_imd": "#b2182b", "inconclusive_weak": "0.6",
           "inconclusive_bursty": "#e08214"}
    for v in col:
        s = [r for r in rows if r["verdict"] == v]
        if s:
            ax[0].scatter([float(r["snr_hi_db"]) for r in s], [float(r["dsnr_db"]) for r in s], s=14, color=col[v], label=f"{v} ({len(s)})")
    ax[0].axhline(2.5 + 6, color="k", ls="--", lw=0.8, label="flag threshold (bound + 6 dB)")
    ax[0].set_xlabel("SNR at high gain (dB)")
    ax[0].set_ylabel("SNR change mid -> high (dB)")
    ax[0].set_title("Real 98 MHz captures: mid -> high gain (+33 dB measured)")
    ax[0].legend(fontsize=6)
    for r in ctl:
        if r["a"] in (0.9,):
            g = r["ghosts"]
            ax[1].scatter([x["snr_hi"] for x in g], [x["dsnr"] for x in g], color="#b2182b", s=18, label=f"a={r['a']}: flagged ghosts ({len(g)})")
            for x in g:
                ax[1].annotate(f"{x['f_mhz']:.1f}", (x["snr_hi"], x["dsnr"]), fontsize=6)
    ax[1].axhline(6, color="k", ls="--", lw=0.8, label="flag threshold (bound 0 + 6 dB)")
    ax[1].axhline(20, color="#b2182b", ls=":", lw=0.8, label="ideal IM3 (+2x10 dB)")
    ax[1].axhline(0, color="#1b7837", ls=":", lw=0.8, label="ideal linear (0 dB)")
    ax[1].set_xlabel("SNR at +10 dB drive (dB)")
    ax[1].set_title("Semi-synthetic control: injected 95.1/97.7 MHz + cubic, 10 dB step")
    ax[1].legend(fontsize=6)
    fig.tight_layout()
    save(fig, "fig2_gainstep_test.png")


def fig_calib():
    m = json.load(open(os.path.join(RES, "metrics.json")))["false_alarm"]
    s = json.load(open(os.path.join(RES, "synth_check.json")))
    fig, ax = plt.subplots(1, 2, figsize=(11, 4))
    design = [1e-2, 1e-3, 1e-4, 1e-5, 1e-6]
    for a, branch, title in ((ax[0], "os", "OS-CFAR branch (N=32, G=4, k=24, n=10)"), (ax[1], "floor", "Floor-referenced branch (FCME floor)")):
        a.loglog(design, design, "k--", lw=0.8, label="design")
        for key, lab, st in (("98M low", "98M low", "o-"), ("98M mid", "98M mid (all frames)", "s-"), ("98M high (clipped)", "98M high clipped", "^-"),
                             ("99M mid (retune)", "99M mid", "v-"), ("915M high", "915M high", "d-")):
            y = [m[key]["cell_calibration"][str(p)][branch + "_all"] for p in design]
            a.loglog(design, np.maximum(y, 1e-7), st, ms=3, lw=0.8, label=lab)
        y = [m["98M mid"]["cell_calibration"][str(p)][branch + "_non_impulsive"] for p in design]
        a.loglog(design, np.maximum(y, 1e-7), "s:", ms=3, lw=0.8, label="98M mid, impulsive frames removed")
        if branch == "os":
            q = s["pfa_quantised"]["0.5"]
            a.loglog([float(k) for k in q], list(q.values()), "x-", color="0.4", lw=0.8, label="synthetic 8-bit noise (0.5 code)")
        a.set_xlabel("design per-cell Pfa")
        a.set_ylabel("measured exceedance in quiet spans")
        a.set_title(title)
        a.legend(fontsize=6)
    fig.tight_layout()
    save(fig, "fig3_cfar_calibration.png")


def fig_impulses():
    fig, ax = plt.subplots(1, 1, figsize=(11, 3))
    for n, lab in (("urban_98M_20M_l8g10a0", "98M low"), ("urban_98M_20M_l24g20a0", "98M mid"),
                   ("urban_98M_20M_l32g30a1", "98M high"), ("urban_915M_20M_l32g30a1", "915M high")):
        c = np.load(os.path.join(CACHE, n + ".npz"))
        fm = c["fmed"]
        t = np.arange(fm.size) * 4096 * 10 / 20e6
        ax.plot(t, db(fm / np.median(fm)), lw=0.5, label=lab)
    ax.axhline(0.5, color="k", ls="--", lw=0.8, label="impulsive-frame threshold 0.5 dB")
    ax.set_xlabel("s")
    ax.set_ylabel("band-median P/floor re time median (dB)")
    ax.set_title("Broadband impulsive frames (2.05 ms): present at mid gain / 915M, not overload-driven")
    ax.legend(fontsize=6, ncol=5)
    fig.tight_layout()
    save(fig, "fig4_impulsive_frames.png")


def fig_sweeps():
    c = np.load(os.path.join(CACHE, "sweeps.npz"))
    fig, ax = plt.subplots(3, 1, figsize=(11, 8))
    F = c["F"] / 1e6
    ax[0].plot(F, db(c["lo_P"]), lw=0.3, color="#2166ac", label="LNA24/VGA20/amp0")
    ax[0].plot(F, db(c["hi_P"]) - 38.1, lw=0.3, color="#e08214", alpha=0.8, label="LNA32/VGA40/amp1, shifted by measured linear gain (-38.1 dB)")
    ax[0].plot(c["hi_imd"] / 1e6, np.full(c["hi_imd"].size, -40), "rv", ms=4, label="gain-step suspect IMD")
    ax[0].set_xlim(1, 1000)
    ax[0].set_title("hackrf_sweep 1-1000 MHz, 20-sweep mean, two gains")
    ax[0].legend(fontsize=6)
    ax[0].set_ylabel("dB (hackrf_sweep units)")
    for a, lo_, hi_ in ((ax[1], 80, 112),):
        s = (F >= lo_) & (F <= hi_)
        a.plot(F[s], db(c["lo_P"][s]), lw=0.7, color="#2166ac")
        a.plot(F[s], db(c["hi_P"][s]) - 38.1, lw=0.7, color="#e08214")
        a.plot(F[s], db(c["lo_fl"][s]), lw=0.7, color="k")
        for d in c["lo_det"]:
            if lo_ <= d / 1e6 <= hi_:
                a.axvline(d / 1e6, color="#1b7837", lw=0.5, ymin=0.92)
        a.set_title("80-112 MHz detail (green ticks: low-gain detections)")
    F6 = c["F6"] / 1e6
    ax[2].plot(F6, db(c["u_P"]), lw=0.3, color="#762a83")
    ax[2].plot(F6, db(c["u_fl"]), lw=0.7, color="k")
    ax[2].set_title("hackrf_sweep 1-6 GHz, LNA32/VGA30/amp1 (single gain: no ghost test possible)")
    ax[2].set_xlabel("MHz")
    fig.tight_layout()
    save(fig, "fig5_sweeps.png")


if __name__ == "__main__":
    fig_fm()
    fig_gainstep()
    fig_calib()
    fig_impulses()
    fig_sweeps()
