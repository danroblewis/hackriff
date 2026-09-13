"""Spike S4 — detection quality in urban overload, IQ captures.

Per capture: STFT (4096 bins = 4.88 kHz, 10 FFTs/frame = 2.05 ms), four noise-floor
estimators, a frame-level 2-D OS-CFAR detector (hysteresis + min duration) and an
emitter-level detector on the 4 s integrated spectrum.  In-capture flags: DC, band
edge, n x 10 MHz reference harmonic, IQ image (mirror about centre), clipping, marginal,
harmonic comb, impulsive (broadband) frames.
Cross-capture tests: gain step (SNR invariance: real signals keep SNR when the floor is
analog-noise-limited, IM3 gains ~2 dB SNR per dB of gain), retune 98 -> 99 MHz (real
signals keep absolute frequency; images move 2*delta; LO-relative artefacts move +-delta),
IM3 product prediction.  Quiet-span false-alarm statistics.

Writes results/*.csv, results/metrics.json and cache/*.npz (for plots.py)."""
import csv, json, os, time
import numpy as np
from scipy import stats, ndimage
import s4lib as L

HERE = os.path.dirname(os.path.abspath(__file__))
RES = os.path.join(HERE, "results")
CACHE = os.path.join(HERE, "cache")
os.makedirs(RES, exist_ok=True)
os.makedirs(CACHE, exist_ok=True)

CAPS = ["urban_98M_20M_l8g10a0", "urban_98M_20M_l24g20a0", "urban_98M_20M_l32g30a1",
        "urban_99M_20M_l24g20a0", "urban_915M_20M_l8g10a0", "urban_915M_20M_l32g30a1"]
SHORT = {"urban_98M_20M_l8g10a0": "98M low", "urban_98M_20M_l24g20a0": "98M mid",
         "urban_98M_20M_l32g30a1": "98M high (clipped)", "urban_99M_20M_l24g20a0": "99M mid (retune)",
         "urban_915M_20M_l8g10a0": "915M low", "urban_915M_20M_l32g30a1": "915M high"}
KNOWN_FM = [93.3e6, 94.9e6, 96.5e6, 98.9e6, 101.3e6, 104.5e6, 106.9e6]
AMP_NOMINAL_DB = 11.0   # HackRF One RX amp, nominal; not verified on this unit
ON_DB, OFF_DB = 6.0, 3.0  # integrated (emitter-level) detector
MARGINAL_DB = 10.0
EDGE_HZ = 8.0e6
IMD_MARGIN_DB = 6.0
BURST_SPREAD_DB = 6.0
IMPULSE_DB = 0.5          # frame whose band-median P/floor rises > 0.5 dB above its time median
MEAS_LIMIT_DB = 10 * np.log10(10 ** (0.5 / 10) - 1)  # 0.5 dB excess ~ floor uncertainty -> -9.1 dB SNR
SUPPRESS = ("spur_candidate", "image_candidate", "dc", "lo_relative", "suspect_imd")
CONFIGS = {
    "or_on1e-6_off1e-3_min3": dict(L.DEFAULT),
    "or_on1e-6_off1e-3_min2": dict(L.DEFAULT, min_frames=2),
    "or_on1e-7_off1e-3_min2": dict(L.DEFAULT, pfa=1e-7, floor_pfa=1e-7, min_frames=2),
    "os_only_on1e-6_off1e-3_min3": dict(L.DEFAULT, mode="os"),
    "floor_only_on1e-6_off1e-3_min3": dict(L.DEFAULT, mode="floor"),
    "or_on1e-6_hyst3dB_min2": dict(L.DEFAULT, hyst_db=3.0, min_frames=2),
    "or_perframe_floor_on1e-6_off1e-3_min3": dict(L.DEFAULT, floor2d=True),
}
PRIMARY = "or_on1e-6_off1e-3_min3"


def nominal_gain(m):
    return m["lna"] + m["vga"] + (AMP_NOMINAL_DB if m["amp"] else 0.0)


def db(x):
    return 10 * np.log10(np.maximum(x, 1e-30))


# ------------------------------------------------------------------ per capture

def process(name, transform=None, tag=None, meta_over=None, configs=None, save=True):
    t0 = time.time()
    tag = tag or name
    configs = configs or CONFIGS
    st = L.stft_power(name, transform=transform)
    if meta_over:
        st["meta"] = dict(st["meta"], **meta_over)
    P = st["P"]
    Ffc = L.blockwise(L.fcme, P, 256, 64, n=10, pfa=1e-3)
    floors = dict(fcme=np.median(Ffc, 0),
                  pct50=np.median(L.blockwise(L.percentile_floor, P, 256, 64, n=10, p=0.5), 0),
                  pct20=np.median(L.blockwise(L.percentile_floor, P, 256, 64, n=10, p=0.2), 0))
    ms, _ = L.minstat_floor(P, 10, win=256)
    floors["minstat"] = np.median(ms, 0)
    fl = floors["fcme"]
    Pmean = P.mean(0)
    K = 8
    nb = P.shape[0] // K
    Pblocks = P[:nb * K].reshape(K, nb, -1).mean(1)
    fmed = np.median(P / fl[None, :], axis=1)
    impulsive = fmed > np.median(fmed) * 10 ** (IMPULSE_DB / 10)
    m = st["meta"]
    em = L.detect_integrated(Pmean, fl, st["f"], m["fc"], st["df"], ON_DB, OFF_DB)
    dets = {}
    Z = None
    for cname, cfg in configs.items():
        d = L.detect(st, Ffc if cfg.get("floor2d") else fl, cfg, Z=Z)
        Z = d["Z"]  # N, G, k identical across configs
        recs = L.box_records(st, d, fl) if cname == PRIMARY else None
        dets[cname] = dict(boxes=d["boxes"], kept=d["lab"] > 0, recs=recs, alpha=d["alpha"],
                           Z=Z if cname == PRIMARY else None)
    cap = dict(name=tag, st=st, meta=m, floors=floors, floor=fl, Pmean=Pmean, Pblocks=Pblocks, em=em, dets=dets,
               impulsive=impulsive, clip_frac=float(st["clip"].mean()), clip_max=float(st["clip"].max()),
               f_abs=m["fc"] + st["f"], df=st["df"], dt=st["dt"], nframes=P.shape[0])
    if save and transform is None:
        np.savez_compressed(os.path.join(CACHE, name + ".npz"), f_abs=cap["f_abs"], Pmean=Pmean,
                            clip=st["clip"], impulsive=impulsive, fmed=fmed,
                            **{"floor_" + k: v for k, v in floors.items()},
                            kept_primary=np.packbits(dets[PRIMARY]["kept"], axis=None),
                            kept_shape=np.array(dets[PRIMARY]["kept"].shape),
                            P_first=P[:400].astype(np.float16))
    print(f"{tag}: {time.time() - t0:.1f}s, {len(em)} emitters, {len(dets[PRIMARY]['boxes'])} boxes, "
          f"impulsive frames {impulsive.mean():.3f}", flush=True)
    return cap


def bins_of(cap, f_lo, f_hi):
    b0 = int(np.searchsorted(cap["f_abs"], f_lo))
    b1 = int(np.searchsorted(cap["f_abs"], f_hi))
    return max(0, b0), min(len(cap["f_abs"]), max(b1, b0 + 1))


def measure(cap, f_lo, f_hi):
    """Integrated peak SNR, excess level, frame-level duty and the spread (max-min, dB) of
    the peak-bin SNR across 8 x 0.5 s blocks (burstiness / stability)."""
    b0, b1 = bins_of(cap, f_lo, f_hi)
    P, fl = cap["Pmean"][b0:b1], cap["floor"][b0:b1]
    r = P / fl
    pk = int(np.argmax(r))
    snr = float(db(max(r[pk] - 1, 10 ** (-3))))
    lvl = float(db(np.sum(np.maximum(P - fl, 0)) * cap["df"]))
    duty = float(cap["dets"][PRIMARY]["kept"][:, b0:b1].any(1).mean())
    lo, hi = max(0, pk - 1), min(b1 - b0, pk + 2)
    rb = cap["Pblocks"][:, b0 + lo:b0 + hi].mean(1) / fl[lo:hi].mean()
    sb = db(np.maximum(rb - 1, 10 ** (-3)))
    spread = float(sb.max() - sb.min())
    return snr, lvl, duty, spread


def in_capture_flags(cap):
    m, df = cap["meta"], cap["df"]
    fc = m["fc"]
    clipped = cap["clip_max"] > 1e-4
    nfft = len(cap["f_abs"])
    rdb = db(cap["Pmean"] / cap["floor"])
    for e in cap["em"]:
        fl = {}
        near, covers_, h = L.spur_mask_hits(e["f_lo"], e["f_hi"], e["f_center"], e["bw"])
        fl["spur_candidate"] = near
        fl["spur_overlap"] = covers_
        fl["dc"] = L.dc_hit(e["f_lo"], e["f_hi"], fc) and e["bw"] <= 40e3
        fl["dc_overlap"] = L.dc_hit(e["f_lo"], e["f_hi"], fc) and not fl["dc"]
        fl["edge"] = L.edge_hit(e["f_center"], fc, EDGE_HZ)
        fl["clipped"] = clipped
        fl["marginal"] = e["peak_snr_db"] < MARGINAL_DB
        mirror = 2 * fc - e["f_center"]
        src = [s for s in cap["em"] if s is not e and s["f_lo"] - df <= mirror <= s["f_hi"] + df]
        fl["image_candidate"] = False
        e["image_irr_db"] = None
        e["image_shape_corr"] = None
        if src and not fl["dc"] and abs(e["f_center"] - fc) > 20e3:
            s = max(src, key=lambda s: s["peak_exc_dbfs"])
            irr = s["peak_exc_dbfs"] - e["peak_exc_dbfs"]
            b = np.arange(e["b0"], e["b1"])
            mb = nfft - b
            ok = (mb >= 0) & (mb < nfft)
            corr = float(np.corrcoef(rdb[b[ok]], rdb[mb[ok]])[0, 1]) if ok.sum() >= 3 else None
            e["image_irr_db"] = float(irr)
            e["image_shape_corr"] = corr
            if irr >= 20.0 and (corr is None or corr > 0.5 or e["nbins"] <= 4):
                fl["image_candidate"] = True
                e["image_of"] = s["f_center"]
        fl["suspect_imd"] = False
        fl["lo_relative"] = False
        fl["compressed"] = False
        fl["comb_candidate"] = False
        e["flags"] = fl
        e["snr_db"], e["level_meas_dbfs"], e["duty"], e["spread_db"] = measure(cap, e["f_lo"], e["f_hi"])


def find_comb(freqs, span, tol=1.5e3, dmin=100e3, dmax=2e6, mmax=40):
    """Best arithmetic comb among narrow-line frequencies.  Returns (members, spacing, mask)."""
    fs = np.sort(np.asarray(freqs, float))
    n = len(fs)
    if n < 3:
        return 0, None, np.zeros(n, bool)
    cands = []
    for i in range(n):
        for j in range(i + 1, n):
            for mm in range(1, mmax + 1):
                d = (fs[j] - fs[i]) / mm
                if dmin <= d <= dmax:
                    cands.append((i, d))
    if not cands:
        return 0, None, np.zeros(n, bool)
    I = np.array([c[0] for c in cands])
    D = np.array([c[1] for c in cands])
    k = (fs[None, :] - fs[I][:, None]) / D[:, None]
    mem = np.abs(k - np.round(k)) * D[:, None] <= tol
    cnt = mem.sum(1)
    h = int(np.argmax(cnt))
    mask = mem[h]
    # refine spacing by least squares on members and recount
    kk = np.round(k[h][mask])
    if mask.sum() >= 3:
        d = np.polyfit(kk, fs[mask], 1)[0]
        k2 = (fs - fs[mask][0]) / d
        mask = np.abs(k2 - np.round(k2)) * d <= tol
    else:
        d = D[h]
    order = np.argsort(np.argsort(np.asarray(freqs, float)))
    return int(mask.sum()), float(d), mask[order]


def comb_flags(cap, min_members=6, trials=200, rng=0):
    nar = [e for e in cap["em"] if e["bw"] <= 20e3 and not e["flags"]["edge"] and not e["flags"]["dc"]
           and not e["flags"]["spur_candidate"]]
    if len(nar) < min_members:
        return dict(narrow_lines=len(nar), members=0, spacing_khz=None, chance=None)
    fr = [e["f_center"] for e in nar]
    cnt, d, mask = find_comb(fr, 2 * EDGE_HZ)
    g = np.random.default_rng(rng)
    lo, hi = cap["meta"]["fc"] - EDGE_HZ, cap["meta"]["fc"] + EDGE_HZ
    chance = np.mean([find_comb(g.uniform(lo, hi, len(fr)), 2 * EDGE_HZ)[0] >= cnt for _ in range(trials)])
    if cnt >= min_members and chance < 0.05:
        for e, mk in zip(nar, mask):
            if mk:
                e["flags"]["comb_candidate"] = True
    return dict(narrow_lines=len(nar), members=cnt, spacing_khz=(d / 1e3 if d else None), chance=float(chance))


# ----------------------------------------------------------------- cross tests

def overlaps(a, e):
    return a["f_lo"] <= e["f_center"] <= a["f_hi"] or e["f_lo"] <= a["f_center"] <= e["f_hi"]


def gain_step(A, B):
    """A = lower gain, B = higher gain, same centre frequency."""
    Gnom = nominal_gain(B["meta"]) - nominal_gain(A["meta"])
    ne = np.abs(B["f_abs"] - B["meta"]["fc"]) <= EDGE_HZ
    dfloor = float(np.median(db(B["floor"][ne] / A["floor"][ne])))
    rows = []
    for e in B["em"]:
        if e["flags"]["edge"]:
            continue
        sA, lA, duA, spA = measure(A, e["f_lo"], e["f_hi"])
        rows.append(dict(e=e, snrA=sA, snrB=e["snr_db"], lvlA=lA, lvlB=e["level_meas_dbfs"], dutyA=duA,
                         dutyB=e["duty"], spreadA=spA, spreadB=e["spread_db"],
                         detA=any(overlaps(a, e) for a in A["em"])))
    def clean(r):
        f = r["e"]["flags"]
        return not (f["spur_candidate"] or f["dc"] or f["image_candidate"])
    anchors = [r for r in rows if r["detA"] and r["snrA"] >= 10 and r["snrB"] >= 10 and r["e"]["bw"] >= 50e3 and clean(r)]
    if len(anchors) < 3:
        anchors = [r for r in rows if r["detA"] and r["snrA"] >= 3 and clean(r)]
    Glin = float(np.median([r["lvlB"] - r["lvlA"] for r in anchors])) if len(anchors) >= 3 else Gnom
    bound = max(0.0, Glin - dfloor)
    for r in rows:
        bursty = (r["spreadA"] > BURST_SPREAD_DB and r["snrA"] >= 0) or (r["spreadB"] > BURST_SPREAD_DB and r["snrB"] >= 0)
        if r["snrA"] >= MEAS_LIMIT_DB:
            dsnr, certain = r["snrB"] - r["snrA"], True
        else:
            dsnr, certain = r["snrB"] - MEAS_LIMIT_DB, False  # lower bound
        if bursty:
            verdict = "inconclusive_bursty"
        elif dsnr > bound + IMD_MARGIN_DB:
            verdict = "suspect_imd"
        elif not certain:
            verdict = "inconclusive_weak"
        elif r["snrA"] >= 10 and (r["lvlB"] - r["lvlA"]) < Glin - IMD_MARGIN_DB:
            verdict = "compressed"
        else:
            verdict = "linear"
        r.update(dsnr=dsnr, dsnr_certain=certain, dlvl=r["lvlB"] - r["lvlA"], verdict=verdict)
    return dict(A=A["name"], B=B["name"], G_nominal=Gnom, G_lin=Glin, n_anchors=len(anchors),
                dfloor=dfloor, dsnr_real_bound=bound, rows=rows)


def apply_gain_step(gs, A, B):
    key = SHORT.get(gs["A"], gs["A"]) + "->" + SHORT.get(gs["B"], gs["B"])
    for r in gs["rows"]:
        e = r["e"]
        e.setdefault("gain_tests", {})[key] = r["verdict"]
        if r["verdict"] == "suspect_imd":
            e["flags"]["suspect_imd"] = True
            for a in A["em"]:
                if overlaps(a, e):
                    a["flags"]["suspect_imd"] = True
                    a.setdefault("gain_tests", {})[key + " (step-up)"] = "suspect_imd"
        if r["verdict"] == "compressed":
            e["flags"]["compressed"] = True


def covers(ems, f, tol):
    return [x for x in ems if x["f_lo"] - tol <= f <= x["f_hi"] + tol]


def retune(A, B):
    """A and B: same gain, centres fcA, fcB.  Classify emitters in the common non-edge span."""
    d = B["meta"]["fc"] - A["meta"]["fc"]
    lo = max(A["meta"]["fc"], B["meta"]["fc"]) - EDGE_HZ
    hi = min(A["meta"]["fc"], B["meta"]["fc"]) + EDGE_HZ
    out = []
    for X, Y, sgn in ((A, B, 1), (B, A, -1)):
        dd = sgn * d
        for e in X["em"]:
            if not (lo <= e["f_center"] <= hi):
                continue
            tol = 10e3
            if covers(Y["em"], e["f_center"], tol):
                label = "stays"
            else:
                label = "not_reproduced"
                def similar(c):
                    return abs(c["peak_exc_dbfs"] - e["peak_exc_dbfs"]) <= 4 and 0.5 <= c["bw"] / e["bw"] <= 2
                for shift, lab in ((dd, "moves_with_LO(+d)"), (-dd, "moves_against_LO(-d)"), (2 * dd, "image_moves(2d)")):
                    if [x for x in covers(Y["em"], e["f_center"] + shift, tol) if similar(x)]:
                        label = lab
                        break
            e["retune"] = label
            if label.startswith("moves"):
                e["flags"]["lo_relative"] = True
            if label == "image_moves(2d)":
                e["flags"]["image_candidate"] = True
            out.append(dict(capture=X["name"], f=e["f_center"], label=label, snr=e["snr_db"],
                            flags=[k for k, v in e["flags"].items() if v]))
    return dict(delta=d, span=(lo, hi), rows=out)


def im3_products(ems, top=6):
    real = [e for e in ems if not any(e["flags"].get(k) for k in SUPPRESS) and e["bw"] >= 50e3 and not e["flags"]["edge"]]
    real = sorted(real, key=lambda e: -e["level_meas_dbfs"])[:top]
    fs = [e["f_center"] for e in real]
    return fs, sorted({2 * a - b for a in fs for b in fs if a != b})


def product_hits(ghost_freqs, prods, fc, tol=30e3):
    hits = sum(1 for g in ghost_freqs if any(abs(g - p) <= tol for p in prods))
    grid = np.linspace(fc - EDGE_HZ, fc + EDGE_HZ, 8192)
    cover = np.zeros(grid.size, bool)
    for p in prods:
        cover |= np.abs(grid - p) <= tol
    return hits, float(cover.mean())


def fm_raster(f):
    ch = np.round((f / 1e6 - 0.1) / 0.2) * 0.2 + 0.1
    return ch * 1e6, abs(f - ch * 1e6)


def fm_category(e, surv):
    if fm_raster(e["f_center"])[1] <= 40e3 and e["bw"] >= 40e3:
        return "raster_station"
    for s in surv:
        if s is not e and fm_raster(s["f_center"])[1] <= 40e3 and s["bw"] >= 40e3 \
                and abs(s["f_center"] - e["f_center"]) <= 250e3 and s["snr_db"] >= e["snr_db"] + 3:
            return "station_part"
    if e["flags"].get("comb_candidate"):
        return "comb"
    return "unexplained"


# ------------------------------------------------------------ quiet-span stats

def quiet_bins_abs(refs, others, lo, hi):
    """Absolute-frequency quiet spans: integrated excess < 1 dB in every reference capture,
    >= 50 kHz from any emitter (refs + others), >= 25 kHz from n*10 MHz and DC, >= 20 bins."""
    ref = refs[0]
    f = ref["f_abs"]
    ok = (f >= lo) & (f <= hi)
    for R in refs:
        ok &= np.interp(f, R["f_abs"], db(R["Pmean"] / R["floor"]), left=99, right=99) < 1.0
    for C in refs + others:
        for e in C["em"]:
            ok &= ~((f >= e["f_lo"] - 50e3) & (f <= e["f_hi"] + 50e3))
        ok &= np.abs(f - C["meta"]["fc"]) > 25e3
    ok &= np.abs(f - np.round(f / 10e6) * 10e6) > 25e3
    lab, nl = ndimage.label(ok)
    sizes = ndimage.sum(ok, lab, index=np.arange(1, nl + 1))
    good = np.isin(lab, 1 + np.flatnonzero(sizes >= 20))
    return [(f[s[0].start], f[s[0].stop - 1] + ref["df"]) for s in ndimage.find_objects(ndimage.label(good)[0])]


def poisson_up(k):
    return stats.chi2.ppf(0.95, 2 * k + 2) / 2


def fa_stats(cap, runs):
    f = cap["f_abs"]
    P = cap["st"]["P"]
    q = np.zeros(len(f), bool)
    interior = np.zeros(len(f), bool)
    cfg = L.DEFAULT
    reach = cfg["G"] + cfg["N"] // 2
    for lo, hi in runs:
        b0, b1 = bins_of(cap, lo, hi)
        q[b0:b1] = True
        if b1 - b0 > 2 * reach:
            interior[b0 + reach:b1 - reach] = True
    nq = int(q.sum())
    hours = cap["nframes"] * cap["dt"] / 3600
    out = dict(quiet_bins=nq, quiet_mhz=nq * cap["df"] / 1e6, interior_bins=int(interior.sum()),
               impulsive_frame_frac=float(cap["impulsive"].mean()))
    exposure = out["quiet_mhz"] * hours
    out["exposure_mhz_h"] = exposure
    Z = cap["dets"][PRIMARY]["Z"]
    nimp = ~cap["impulsive"]
    calib = {}
    for pfa in [1e-2, 1e-3, 1e-4, 1e-5, 1e-6]:
        a = L.os_cfar_alpha(cfg["N"], cfg["k"], 10, pfa)
        Tf = L.gamma_T(10, pfa)
        row = {}
        for lab, fr in (("all", slice(None)), ("non_impulsive", nimp)):
            Ps = P[fr]
            row["os_" + lab] = float(np.mean(Ps[:, interior] > a * Z[fr][:, interior])) if interior.any() else None
            row["floor_" + lab] = float(np.mean(Ps[:, q] > Tf * cap["floor"][q])) if nq else None
        row["os_cells"] = int(interior.sum()) * cap["nframes"]
        row["floor_cells"] = nq * cap["nframes"]
        calib[str(pfa)] = row
    out["cell_calibration"] = calib
    per_cfg = {}
    for cname, d in cap["dets"].items():
        fb = []
        for (i, t0, t1, b0, b1) in d["boxes"]:
            # power-weighted centre bin (a bounding-box centre can land in a gap between emitters)
            exc = np.maximum(P[t0:t1, b0:b1].mean(0) - cap["floor"][b0:b1], 0)
            c = b0 + int(round(np.sum(exc * np.arange(b1 - b0)) / max(exc.sum(), 1e-30)))
            if q[c]:
                fb.append((t0, t1, c, cap["impulsive"][t0:t1].mean() >= 0.5))
        k = len(fb)
        kn = sum(1 for x in fb if not x[3])
        # false "tracks": >= 2 non-impulsive boxes within +-1 bin of each other at different times
        cs = sorted(x[2] for x in fb if not x[3])
        tracks = 0
        i = 0
        while i < len(cs):
            j = i
            while j + 1 < len(cs) and cs[j + 1] - cs[i] <= 2:
                j += 1
            if j > i:
                tracks += 1
            i = j + 1
        per_cfg[cname] = dict(false_boxes=k, false_boxes_non_impulsive=kn, false_tracks=tracks,
                              rate_per_mhz_h=k / exposure, rate_non_impulsive=kn / exposure,
                              upper95_non_impulsive=poisson_up(kn) / exposure,
                              track_rate_upper95=poisson_up(tracks) / exposure)
    out["false_boxes"] = per_cfg
    out["runs_mhz"] = [(round(a / 1e6, 4), round(b / 1e6, 4)) for a, b in runs]
    return out


# ------------------------------------------------------------------------ main

def main():
    caps = {n: process(n) for n in CAPS}
    for c in caps.values():
        in_capture_flags(c)
    low, mid, high, r99 = (caps[n] for n in CAPS[:4])
    ulow, uhigh = caps[CAPS[4]], caps[CAPS[5]]
    combs = {SHORT[n]: comb_flags(c) for n, c in caps.items()}
    gs = {"98 low->mid": gain_step(low, mid), "98 mid->high": gain_step(mid, high), "915 low->high": gain_step(ulow, uhigh)}
    apply_gain_step(gs["98 low->mid"], low, mid)
    apply_gain_step(gs["98 mid->high"], mid, high)
    apply_gain_step(gs["915 low->high"], ulow, uhigh)
    rt = retune(mid, r99)

    im3 = {}
    for key, ref, tgt in (("98 mid->high", mid, high), ("98 low->mid", mid, mid)):
        fs, prods = im3_products(ref["em"])
        ghosts = [r["e"]["f_center"] for r in gs[key]["rows"] if r["verdict"] == "suspect_imd"]
        hits, chance = product_hits(ghosts, prods, tgt["meta"]["fc"])
        im3[key] = dict(strong=[round(x / 1e6, 3) for x in fs], n_products=len(prods), ghosts=len(ghosts),
                        ghosts_on_im3=hits, chance_fraction=chance)

    runs_fm = quiet_bins_abs([mid, r99], [low, high], 90.5e6, 105.5e6)
    runs_915 = quiet_bins_abs([uhigh], [ulow], 915e6 - EDGE_HZ, 915e6 + EDGE_HZ)
    fa = {SHORT[c["name"]]: fa_stats(c, runs_fm) for c in (low, mid, high, r99)}
    fa.update({SHORT[c["name"]]: fa_stats(c, runs_915) for c in (ulow, uhigh)})

    metrics, known = {}, {}
    for n, c in caps.items():
        fc = c["meta"]["fc"]
        ems = [e for e in c["em"] if not e["flags"]["edge"]]
        span_mhz = 2 * EDGE_HZ / 1e6
        frac = lambda k: sum(1 for e in ems if e["flags"].get(k)) / max(len(ems), 1)
        surv = [e for e in ems if not any(e["flags"].get(k) for k in SUPPRESS)]
        is_fm = fc < 200e6
        cats = {}
        for e in surv:
            cat = fm_category(e, surv) if is_fm else ("comb" if e["flags"]["comb_candidate"] else "unverified")
            e["category"] = cat
            cats[cat] = cats.get(cat, 0) + 1
        boxes = c["dets"][PRIMARY]["recs"]
        nb_flag = dict(spur=0, image=0, imd=0, lo=0, dc=0, comb=0, clipped=0, edge=0, impulsive=0, any_suppress=0)
        for b in boxes:
            flags = set(k for e in c["em"] if e["f_lo"] <= b["f_center"] <= e["f_hi"] for k, v in e["flags"].items() if v)
            if b["clipped"]:
                flags.add("clipped")
            if L.edge_hit(b["f_center"], fc, EDGE_HZ):
                flags.add("edge")
            if L.spur_mask_hits(b["f_lo"], b["f_hi"], b["f_center"], b["bw_10db"])[0]:
                flags.add("spur_candidate")
            if L.dc_hit(b["f_lo"], b["f_hi"], fc) and b["bw_10db"] <= 40e3:
                flags.add("dc")
            t0, t1 = int(round(b["t_start"] / c["dt"])), int(round(b["t_end"] / c["dt"]))
            if c["impulsive"][t0:t1].mean() >= 0.5:
                flags.add("impulsive")
            b["flags"] = sorted(flags)
            for key, fk in (("spur", "spur_candidate"), ("image", "image_candidate"), ("imd", "suspect_imd"),
                            ("lo", "lo_relative"), ("dc", "dc"), ("comb", "comb_candidate"), ("clipped", "clipped"),
                            ("edge", "edge"), ("impulsive", "impulsive")):
                nb_flag[key] += fk in flags
            nb_flag["any_suppress"] += bool(flags & set(SUPPRESS))
        metrics[SHORT[n]] = dict(
            capture=n, fc_mhz=fc / 1e6, gain=f"LNA {c['meta']['lna']} / VGA {c['meta']['vga']} / amp {int(c['meta']['amp'])}",
            clip_frac=c["clip_frac"], impulsive_frame_frac=float(c["impulsive"].mean()),
            floor_fcme_dbfs_hz=float(np.median(db(c["floor"][np.abs(c['f_abs'] - fc) <= EDGE_HZ]))),
            emitters_total=len(c["em"]), emitters_nonedge=len(ems),
            frac_spur=frac("spur_candidate"), frac_dc=frac("dc"), frac_image=frac("image_candidate"),
            frac_lo_relative=frac("lo_relative"), frac_suspect_imd=frac("suspect_imd"), frac_marginal=frac("marginal"),
            frac_compressed=frac("compressed"), frac_clipped=frac("clipped"), frac_comb=frac("comb_candidate"),
            suppressed=len(ems) - len(surv), surviving=len(surv), surviving_categories=cats,
            unexplained_per_mhz=cats.get("unexplained", 0) / span_mhz,
            frame_boxes=len(boxes), frame_boxes_flag_counts=nb_flag,
            frame_boxes_by_config={k: len(v["boxes"]) for k, v in c["dets"].items()},
        )
        if is_fm:
            kk = {}
            for fst in KNOWN_FM:
                cand = [e for e in c["em"] if e["f_lo"] <= fst <= e["f_hi"] and abs(e["f_center"] - fst) <= 60e3]
                b0, b1 = bins_of(c, fst - 50e3, fst + 50e3)
                cov = {k: round(float(v["kept"][:, b0:b1].any(1).mean()), 3) for k, v in c["dets"].items()}
                if cand:
                    e = max(cand, key=lambda e: e["peak_snr_db"])
                    kk[f"{fst / 1e6:.1f}"] = dict(detected=True, snr_db=round(e["snr_db"], 1),
                                                  suppressed=any(e["flags"].get(k) for k in SUPPRESS),
                                                  flags=[k for k, v in e["flags"].items() if v], frame_coverage=cov)
                else:
                    kk[f"{fst / 1e6:.1f}"] = dict(detected=False, frame_coverage=cov)
            known[SHORT[n]] = kk

    floorcmp = {}
    for n, c in caps.items():
        fc = c["meta"]["fc"]
        ne = np.abs(c["f_abs"] - fc) <= EDGE_HZ
        occ = np.zeros(len(ne), bool)
        for e in c["em"]:
            occ[e["b0"]:e["b1"]] = True
        q = np.zeros(len(ne), bool)
        for lo, hi in (runs_fm if fc < 200e6 else runs_915):
            b0, b1 = bins_of(c, lo, hi)
            q[b0:b1] = True
        ref = db(c["floors"]["fcme"])
        row = {}
        for k, v in c["floors"].items():
            dv = db(v) - ref
            row[k] = dict(median_dbfs_hz=float(np.median(db(v)[ne])), minus_fcme_all=float(np.median(dv[ne])),
                          minus_fcme_on_emitter_bins=float(np.median(dv[ne & occ])) if (ne & occ).any() else None,
                          minus_quiet_psd=float(np.median(db(v)[q] - db(c["Pmean"])[q])) if q.any() else None)
        row["emitter_bin_fraction_nonedge"] = float(occ[ne].mean())
        floorcmp[SHORT[n]] = row

    for n, c in caps.items():
        with open(os.path.join(RES, f"emitters_{n}.csv"), "w", newline="") as fh:
            w = csv.writer(fh)
            w.writerow(["f_center_mhz", "f_lo_mhz", "f_hi_mhz", "bw_khz", "peak_snr_db", "level_dbfs", "duty",
                        "spread_db", "image_irr_db", "retune", "gain_tests", "category", "flags"])
            for e in sorted(c["em"], key=lambda e: e["f_center"]):
                w.writerow([f"{e['f_center'] / 1e6:.4f}", f"{e['f_lo'] / 1e6:.4f}", f"{e['f_hi'] / 1e6:.4f}",
                            f"{e['bw'] / 1e3:.1f}", f"{e['snr_db']:.1f}", f"{e['level_meas_dbfs']:.1f}", f"{e['duty']:.2f}",
                            f"{e['spread_db']:.1f}", "" if e.get("image_irr_db") is None else f"{e['image_irr_db']:.1f}",
                            e.get("retune", ""), ";".join(f"{k}:{v}" for k, v in e.get("gain_tests", {}).items()),
                            e.get("category", ""), ";".join(k for k, v in e["flags"].items() if v)])
    with open(os.path.join(RES, "gainstep_rows.csv"), "w", newline="") as fh:
        w = csv.writer(fh)
        w.writerow(["step", "f_center_mhz", "bw_khz", "snr_lo_db", "snr_hi_db", "dsnr_db", "dsnr_certain", "dlevel_db",
                    "spread_lo_db", "spread_hi_db", "verdict"])
        for k, g in gs.items():
            for r in sorted(g["rows"], key=lambda r: r["e"]["f_center"]):
                w.writerow([k, f"{r['e']['f_center'] / 1e6:.4f}", f"{r['e']['bw'] / 1e3:.1f}", f"{r['snrA']:.1f}",
                            f"{r['snrB']:.1f}", f"{r['dsnr']:.1f}", r["dsnr_certain"], f"{r['dlvl']:.1f}",
                            f"{r['spreadA']:.1f}", f"{r['spreadB']:.1f}", r["verdict"]])
    with open(os.path.join(RES, "frame_boxes_primary.csv"), "w", newline="") as fh:
        w = csv.writer(fh)
        w.writerow(["capture", "f_center_mhz", "t_start_s", "t_end_s", "bw_10db_khz", "snr_peak_db", "sk", "clip_frac", "flags"])
        for n, c in caps.items():
            for b in c["dets"][PRIMARY]["recs"]:
                w.writerow([n, f"{b['f_center'] / 1e6:.4f}", f"{b['t_start']:.3f}", f"{b['t_end']:.3f}",
                            f"{b['bw_10db'] / 1e3:.1f}", f"{b['snr_peak']:.1f}", f"{b['sk']:.2f}", f"{b['clip_frac']:.2e}",
                            ";".join(b["flags"])])
    gsum = {k: dict(G_nominal=g["G_nominal"], G_lin_measured=g["G_lin"], n_anchors=g["n_anchors"], dfloor_db=g["dfloor"],
                    dsnr_real_bound=g["dsnr_real_bound"],
                    verdicts={v: sum(1 for r in g["rows"] if r["verdict"] == v) for v in
                              ("linear", "compressed", "suspect_imd", "inconclusive_weak", "inconclusive_bursty")})
            for k, g in gs.items()}
    rsum = {}
    for r in rt["rows"]:
        rsum.setdefault(SHORT[r["capture"]], {}).setdefault(r["label"], 0)
        rsum[SHORT[r["capture"]]][r["label"]] += 1
    out = dict(metrics=metrics, known_fm=known, gain_step=gsum, combs=combs,
               retune=dict(summary=rsum, rows=[dict(capture=SHORT[r["capture"]], f_mhz=round(r["f"] / 1e6, 4),
                                                    label=r["label"], snr=round(r["snr"], 1), flags=r["flags"])
                                               for r in rt["rows"]]),
               im3=im3, false_alarm=fa, floor_compare=floorcmp,
               thresholds=dict(alpha_on_db=10 * np.log10(L.os_cfar_alpha(32, 24, 10, 1e-6)),
                               alpha_off_db=10 * np.log10(L.os_cfar_alpha(32, 24, 10, 1e-3)),
                               floor_on_db=10 * np.log10(L.gamma_T(10, 1e-6)), floor_off_db=10 * np.log10(L.gamma_T(10, 1e-3))),
               configs=CONFIGS)
    json.dump(out, open(os.path.join(RES, "metrics.json"), "w"), indent=1, default=float)
    print(json.dumps(dict(metrics=metrics, gain_step=gsum, retune=rsum, im3=im3, combs=combs), indent=1, default=float))


if __name__ == "__main__":
    main()
