"""Spike S4 — survey-level detection on hackrf_sweep output at two gains, 1 MHz-1 GHz
(LNA24/VGA20/amp0 vs LNA32/VGA40/amp1), plus the single-gain 1-6 GHz sweep.
Rows are single FFTs (measured n_eff ~1.2); the per-bin mean over the 20 (10) sweeps is
treated as Gamma(20) (Gamma(10)).  Writes results/sweep_metrics.json,
results/sweep_detections.csv and cache/sweeps.npz."""
import csv, json, os
import numpy as np
from scipy import ndimage
import s4lib as L

HERE = os.path.dirname(os.path.abspath(__file__))
RES = os.path.join(HERE, "results")
CACHE = os.path.join(HERE, "cache")
AMP_NOMINAL_DB = 11.0
KNOWN_FM = [93.3e6, 94.9e6, 96.5e6, 98.9e6, 101.3e6, 104.5e6, 106.9e6]
BANDS = [(1, 30), (30, 88), (88, 108), (108, 174), (174, 216), (216, 470), (470, 698), (698, 1001)]
BANDS_HI = [(1000, 2000), (2000, 3000), (3000, 4000), (4000, 5000), (5000, 6001)]


def load(fn):
    rows = {}
    for line in open(L.STORE + fn):
        p = [s.strip() for s in line.split(",")]
        rows.setdefault(float(p[2]), []).append((float(p[4]), np.array(p[6:], float)))
    freqs, mats = [], []
    for lo, v in sorted(rows.items()):
        bw = v[0][0]
        m = np.array([x[1] for x in v])
        freqs.append(lo + bw * np.arange(m.shape[1]) + bw / 2)
        mats.append(m)
    ns = min(m.shape[0] for m in mats)
    F = np.concatenate(freqs)
    M = np.concatenate([m[:ns] for m in mats], axis=1)  # sweeps x bins (dB)
    o = np.argsort(F)
    return F[o], 10 ** (M[:, o] / 10), bw, ns


def survey(fn, gain):
    F, lin, bw, ns = load(fn)
    Pm = lin.mean(0)
    fl = L.blockwise(L.fcme, Pm[None, :], 128, 32, n=ns, pfa=1e-3)[0]
    cfg = dict(L.DEFAULT, N=16, G=2, k=12, min_frames=1, gap_frames=0)
    d = L.detect(dict(P=Pm[None, :].astype(np.float32), n_avg=ns), fl, cfg)
    persist = (lin > fl[None, :] * 10 ** (6 / 10)).mean(0)
    dets = []
    for (i, t0, t1, b0, b1) in d["boxes"]:
        seg = Pm[b0:b1]
        exc = np.maximum(seg - fl[b0:b1], 0)
        pk = b0 + int(np.argmax(seg / fl[b0:b1]))
        fcn = float(np.sum(exc * F[b0:b1]) / max(exc.sum(), 1e-30))
        h = np.round(fcn / 10e6) * 10e6
        dets.append(dict(f_center=fcn, f_lo=F[b0] - bw / 2, f_hi=F[b1 - 1] + bw / 2, nbins=b1 - b0,
                         snr=float(10 * np.log10(max(Pm[pk] / fl[pk] - 1, 1e-3))),
                         level=float(10 * np.log10(max(exc.sum() * bw, 1e-30))), persist=float(persist[b0:b1].max()),
                         spur=bool(abs(fcn - h) <= bw and (b1 - b0) <= 3 and h > 0), b0=b0, b1=b1))
    return dict(fn=fn, gain=gain, F=F, Pm=Pm, fl=fl, bw=bw, ns=ns, dets=dets)


def measure(S, b0, b1):
    r = S["Pm"][b0:b1] / S["fl"][b0:b1]
    return float(10 * np.log10(max(r.max() - 1, 1e-3))), float(10 * np.log10(max(np.sum(np.maximum(S["Pm"][b0:b1] - S["fl"][b0:b1], 0)) * S["bw"], 1e-30)))


def band_counts(dets, bands, scale=1e6):
    return {f"{a}-{b} MHz": sum(1 for d in dets if a * scale <= d["f_center"] < b * scale) for a, b in bands}


def main():
    lo = survey("sweep_1-1000M_w100k_l24g20a0.csv", (24, 20, 0))
    hi = survey("sweep_1-1000M_w100k_l32g40a1.csv", (32, 40, 1))
    uh = survey("sweep_1000-6000M_w500k_l32g30a1.csv", (32, 30, 1))
    assert np.allclose(lo["F"], hi["F"])
    Gnom = (32 - 24) + (40 - 20) + AMP_NOMINAL_DB
    dfloor = float(np.median(10 * np.log10(hi["fl"] / lo["fl"])))
    meas_limit = 10 * np.log10(10 ** (0.5 / 10) - 1)
    rows = []
    for e in hi["dets"]:
        sA, lA = measure(lo, e["b0"], e["b1"])
        detA = any(a["f_lo"] <= e["f_center"] <= a["f_hi"] or e["f_lo"] <= a["f_center"] <= e["f_hi"] for a in lo["dets"])
        rows.append(dict(e=e, snrA=sA, lvlA=lA, detA=detA))
    anchors = [r for r in rows if r["detA"] and r["snrA"] >= 10 and r["e"]["snr"] >= 10 and not r["e"]["spur"] and r["e"]["persist"] >= 0.8]
    Glin = float(np.median([r["e"]["level"] - r["lvlA"] for r in anchors])) if len(anchors) >= 3 else Gnom
    bound = max(0.0, Glin - dfloor)
    for r in rows:
        e = r["e"]
        certain = r["snrA"] >= meas_limit
        dsnr = e["snr"] - (r["snrA"] if certain else meas_limit)
        if e["persist"] < 0.5 and e["snr"] < 15:
            v = "inconclusive_bursty"
        elif dsnr > bound + 6:
            v = "suspect_imd"
        elif not certain:
            v = "inconclusive_weak"
        elif r["snrA"] >= 10 and e["level"] - r["lvlA"] < Glin - 6:
            v = "compressed"
        else:
            v = "linear"
        r.update(dsnr=dsnr, verdict=v)
        e["verdict"] = v
    # IM2 / IM3 prediction from the strongest persistent low-gain detections
    strong = sorted([d for d in lo["dets"] if not d["spur"] and d["persist"] >= 0.8], key=lambda d: -d["level"])[:10]
    fs = [d["f_center"] for d in strong]
    prods = sorted({p for a in fs for b in fs if a != b for p in (2 * a - b, a + b, abs(a - b)) if 1e6 <= p <= 1001e6})
    ghosts = [r["e"] for r in rows if r["verdict"] == "suspect_imd"]
    tol = lo["bw"]
    hits = sum(1 for g in ghosts if any(abs(g["f_center"] - p) <= tol for p in prods))
    grid = np.linspace(1e6, 1001e6, 200000)
    cover = np.zeros(grid.size, bool)
    for p in prods:
        cover |= np.abs(grid - p) <= tol
    new_hi = [r for r in rows if not r["detA"]]
    known = {}
    for S, key in ((lo, "low"), (hi, "high")):
        known[key] = {f"{f / 1e6:.1f}": any(d["f_lo"] <= f <= d["f_hi"] for d in S["dets"]) for f in KNOWN_FM}
    fm_hi_flagged = [f / 1e6 for f in KNOWN_FM if any(d["f_lo"] <= f <= d["f_hi"] and d.get("verdict") == "suspect_imd" for d in hi["dets"])]

    def summary(S, bands, scale=1e6):
        n = len(S["dets"])
        return dict(detections=n, spur_flagged=sum(d["spur"] for d in S["dets"]),
                    persistent_ge_0p8=sum(d["persist"] >= 0.8 for d in S["dets"]),
                    floor_median_db=float(np.median(10 * np.log10(S["fl"]))), by_band=band_counts(S["dets"], bands, scale))
    spur_chance_1g = 100 * 2 * lo["bw"] / 1000e6
    spur_chance_6g = 500 * 2 * uh["bw"] / 5000e6
    out = dict(
        low=summary(lo, BANDS), high=summary(hi, BANDS), uhf_6g=summary(uh, BANDS_HI),
        spur_mask_chance_fraction=dict(sweep_1g=spur_chance_1g, sweep_6g=spur_chance_6g),
        gain_step=dict(G_nominal=Gnom, G_lin=Glin, n_anchors=len(anchors), dfloor=dfloor, dsnr_real_bound=bound,
                       verdicts={v: sum(1 for r in rows if r["verdict"] == v) for v in
                                 ("linear", "compressed", "suspect_imd", "inconclusive_weak", "inconclusive_bursty")},
                       suspect_imd_by_band=band_counts(ghosts, BANDS),
                       new_at_high_gain=len(new_hi),
                       new_at_high_gain_flagged=sum(1 for r in new_hi if r["verdict"] == "suspect_imd"),
                       new_by_band=band_counts([r["e"] for r in new_hi], BANDS)),
        im_products=dict(strong_mhz=[round(f / 1e6, 2) for f in fs], n_products=len(prods), ghosts=len(ghosts),
                         ghosts_on_products=hits, chance_fraction=float(cover.mean())),
        known_fm=known, known_fm_flagged_high=fm_hi_flagged)
    json.dump(out, open(os.path.join(RES, "sweep_metrics.json"), "w"), indent=1, default=float)
    with open(os.path.join(RES, "sweep_detections.csv"), "w", newline="") as fh:
        w = csv.writer(fh)
        w.writerow(["sweep", "f_center_mhz", "nbins", "snr_db", "level_db", "persist", "spur", "verdict"])
        for S, key in ((lo, "1G_low"), (hi, "1G_high"), (uh, "6G_high")):
            for d in S["dets"]:
                w.writerow([key, f"{d['f_center'] / 1e6:.3f}", d["nbins"], f"{d['snr']:.1f}", f"{d['level']:.1f}",
                            f"{d['persist']:.2f}", int(d["spur"]), d.get("verdict", "")])
    np.savez_compressed(os.path.join(CACHE, "sweeps.npz"), F=lo["F"], lo_P=lo["Pm"], lo_fl=lo["fl"], hi_P=hi["Pm"], hi_fl=hi["fl"],
                        F6=uh["F"], u_P=uh["Pm"], u_fl=uh["fl"],
                        lo_det=np.array([d["f_center"] for d in lo["dets"]]),
                        hi_det=np.array([d["f_center"] for d in hi["dets"]]),
                        hi_imd=np.array([d["f_center"] for d in ghosts]))
    print(json.dumps(out, indent=1, default=float))


if __name__ == "__main__":
    main()
