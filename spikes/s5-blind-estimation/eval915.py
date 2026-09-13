"""S5 evaluation: blind C13/C14 on the real 902-928 MHz FHSS FSK bursts vs sync truth,
noise-degraded real bursts, matched synthetic twins, and negative controls.

usage: python3 eval915.py <scratch_dir_with_b915_truth915_b433.json> <results_dir> [workers=20]
"""
import json
import sys
from multiprocessing import Pool

import numpy as np
from scipy import signal

HERE = __file__.rsplit("/", 1)[0]
sys.path.insert(0, HERE)
import s5lib as L
import synth as S

STORE = "/Users/daniellewis/hackriff/fixtures/store/2026-09-13/"
FN915 = STORE + "ism_915M_10M_l24g30a1.sigmf-data"
FN433 = STORE + "ism_433p62M_2M_l24g30a1.sigmf-data"
FNFM = STORE + "fm_100p8M_2p4M_l32g30a1.sigmf-data"
FS915, FS433, FSFM = 10e6, 2e6, 2.4e6
NOISE_RMS_915 = 3.21
PAD = 0.002
FS_OUT_MIN = 1.25e6
G = {}


def chain(b, fs, fn=None, x_full=None, pad=PAD, fs_out_min=FS_OUT_MIN, noise_override=None):
    x, fso, fcen, npad = L.extract_burst(fn, fs, b, pad_s=pad, bw_pad=2.0, fs_out_min=fs_out_min, x_full=x_full)
    noise = noise_override if noise_override is not None else x[: max(64, npad - int(0.0005 * fso))]
    body = x[npad:len(x) - npad]
    p = L.c13_params(body, fso, noise)
    if p.get("obw99_hz") is None:
        return dict(family="unknown", rate=None, rate_trusted=False, snr_db=None, reasons=["low_snr"],
                    deviation_hz=None), p
    r = L.classify_and_estimate(x[npad // 2:len(x) - npad // 2], fso, noise, p["obw99_hz"], p["snr_db"],
                                cfo_hz=p["cfo_hz"])
    return r, p


def slim(r, p, **kw):
    c = r.get("consensus") or {}
    return dict(snr_box_db=p.get("snr_db"), snr_db=r.get("snr_ext_db", p.get("snr_db")),
                obw_hz=p.get("obw99_hz"), cfo_hz=p.get("cfo_hz"),
                family=r["family"], family_conf=r.get("family_conf"), rate=r["rate"],
                trusted=bool(r["rate_trusted"]), cons_trusted=bool(r.get("rate_consensus_trusted")),
                dev=r.get("deviation_hz"), h=r.get("mod_index_h"),
                reasons=r.get("reasons"), n_methods=c.get("n_methods"), ls=bool(c.get("ls")), **kw)


def init(scr):
    G["B"] = json.load(open(scr + "/b915.json"))
    G["T"] = json.load(open(scr + "/truth915.json"))


def w_real(bid):
    b = G["B"]["bursts"][bid]
    r, p = chain(b, FS915, fn=FN915)
    t = G["T"].get(str(bid)) or {}
    hit = (t.get("hits") or [{}])[0] if t.get("truth_rate") else {}
    return slim(r, p, id=bid, truth_rate=t.get("truth_rate"), dev_ref=hit.get("dev_ref_hz"),
                n_bits=hit.get("n_bits"), preamble_bits=hit.get("preamble_bits"), peak_db=b["peak_db"])


def local_burst(b):
    dur = b["t1"] - b["t0"]
    return dict(t0=PAD * 2, t1=PAD * 2 + dur, f_lo=b["f_lo"], f_hi=b["f_hi"]), dur


def w_degraded(args):
    bid, target, snr_real, seed = args
    rng = np.random.default_rng(seed)
    b = G["B"]["bursts"][bid]
    lb, dur = local_burst(b)
    s0 = int((b["t0"] - PAD * 2) * FS915)
    x = L.load_ci8(FN915, s0, int((dur + PAD * 4) * FS915)).astype(np.complex128)
    g2 = 10 ** ((target - snr_real) / 10)
    sigma2 = NOISE_RMS_915 ** 2
    w = (rng.normal(size=len(x)) + 1j * rng.normal(size=len(x))) * np.sqrt(sigma2 * (1 - g2) / 2)
    y = L.quantize_ci8(np.sqrt(g2) * x + w)
    r, p = chain(lb, FS915, x_full=y)
    t = G["T"][str(bid)]
    return slim(r, p, id=bid, target_snr=target, truth_rate=t["truth_rate"], dev_ref=t["hits"][0]["dev_ref_hz"])


def w_synth(args):
    bid, target, obw, seed = args
    S.rng = np.random.default_rng(seed)
    t = G["T"][str(bid)]
    h = t["hits"][0]
    R, dev = t["truth_rate"], h["dev_ref_hz"]
    pre = max(24, (h["preamble_bits"] // 2) * 2)
    nb = max(h["n_bits"], pre + 32)
    sig, _ = S.gen_fsk(R, dev, nb, FS915, bt=0.5, preamble=pre)
    cfo = S.rng.uniform(-10e3, 10e3)
    y, npad = S.embed(sig, FS915, target, obw, noise_rms=NOISE_RMS_915, pad_s=PAD * 2, cfo_hz=cfo)
    dur = len(sig) / FS915
    lb = dict(t0=PAD * 2, t1=PAD * 2 + dur, f_lo=-obw / 2, f_hi=obw / 2)
    r, p = chain(lb, FS915, x_full=y)
    return slim(r, p, id=bid, target_snr=target, truth_rate=R, dev_ref=dev)


def w_noise915(args):
    t, f = args
    b = dict(t0=t, t1=t + 0.005, f_lo=f - 150e3, f_hi=f + 150e3)
    r, p = chain(b, FS915, fn=FN915)
    return slim(r, p, kind="noise915", t=t, f=f)


def w_433(bid):
    B4 = json.load(open(G["scr"] + "/b433.json")) if "B4" not in G else G["B4"]
    G["B4"] = B4
    b = B4["bursts"][bid]
    r, p = chain(b, FS433, fn=FN433, pad=0.004, fs_out_min=100e3)
    return slim(r, p, kind="433det", id=bid, rf_hz=b["rf_hz"])


def w_fm(args):
    t, f_noise = args
    b = dict(t0=t, t1=t + 0.02, f_lo=400e3, f_hi=600e3)
    nb = dict(t0=t, t1=t + 0.02, f_lo=f_noise - 100e3, f_hi=f_noise + 100e3)
    xn, _, _, _ = L.extract_burst(FNFM, FSFM, nb, pad_s=0.0, bw_pad=2.0, fs_out_min=FS_OUT_MIN / 2)
    r, p = chain(b, FSFM, fn=FNFM, pad=0.0, fs_out_min=FS_OUT_MIN / 2, noise_override=xn)
    return slim(r, p, kind="fm_broadcast_analog", t=t)


def w_synneg(args):
    kind, snr, seed = args
    S.rng = np.random.default_rng(seed)
    fs = 1e6
    if kind == "nbfm_voice":
        sig, obw = S.gen_nbfm_voice(0.1, fs, dev=2500), 8e3
    elif kind == "lora_chirp":
        sig, obw = S.gen_chirp(125e3, 7, 8, fs), 125e3
    elif kind == "cw":
        sig, obw = np.exp(2j * np.pi * 1000 * np.arange(int(0.02 * fs)) / fs), 2e3
    else:
        sig, obw = np.zeros(int(0.02 * fs), complex), 20e3
    y, npad = S.embed(sig, fs, snr, obw, noise_rms=NOISE_RMS_915, pad_s=0.004)
    dur = len(sig) / fs
    lb = dict(t0=0.004, t1=0.004 + dur, f_lo=-max(obw, 20e3) / 2, f_hi=max(obw, 20e3) / 2)
    r, p = chain(lb, fs, x_full=y, pad=0.004, fs_out_min=0)
    return slim(r, p, kind=kind, target_snr=snr)


def init_all(scr):
    init(scr)
    G["scr"] = scr


def summarize(rows, key_snr="snr_db", bins=(-99, 3, 6, 9, 12, 15, 20, 25, 99), dev_tol=0.10):
    out = []
    for lo, hi in zip(bins[:-1], bins[1:]):
        rr = [r for r in rows if r.get(key_snr) is not None and lo <= r[key_snr] < hi]
        if not rr:
            continue
        ok = [r["rate"] is not None and abs(r["rate"] / r["truth_rate"] - 1) < 0.01 for r in rr]
        tr = [r["trusted"] for r in rr]
        tw = sum(1 for o, t in zip(ok, tr) if t and not o)
        trr = sum(1 for o, t in zip(ok, tr) if t and o)
        fam = sum(r["family"] == "FSK" for r in rr)
        wrongfam = sum(r["family"] not in ("FSK", "unknown") for r in rr)
        devs = [abs(r["dev"] / r["dev_ref"] - 1) for r in rr if r.get("dev") and r.get("dev_ref")]
        out.append(dict(snr_bin=f"[{lo},{hi})", n=len(rr), rate_within1pct=round(np.mean(ok), 3),
                        trusted=round(np.mean(tr), 3), trusted_right=trr, trusted_wrong=tw,
                        family_fsk=round(fam / len(rr), 3), family_wrong_label=wrongfam,
                        dev_n=len(devs), dev_within10pct=round(float(np.mean(np.array(devs) < dev_tol)), 3) if devs else None,
                        dev_median_abs_err=round(float(np.median(devs)), 3) if devs else None))
    return out


def print_table(name, tab):
    print(f"\n== {name}")
    print(" snr_bin     n  rate<1%  trusted  t_right t_wrong  FSK   wrongfam  dev_n dev<10% dev_med")
    for t in tab:
        print(f" {t['snr_bin']:9s} {t['n']:4d}  {t['rate_within1pct']:.3f}   {t['trusted']:.3f}   {t['trusted_right']:5d} "
              f"{t['trusted_wrong']:5d}  {t['family_fsk']:.3f}  {t['family_wrong_label']:5d}   {t['dev_n']:4d} "
              f"{t['dev_within10pct']}  {t['dev_median_abs_err']}")


if __name__ == "__main__":
    scr, outdir = sys.argv[1], sys.argv[2]
    nw = int(sys.argv[3]) if len(sys.argv) > 3 else 20
    B = json.load(open(scr + "/b915.json"))
    T = json.load(open(scr + "/truth915.json"))
    rng = np.random.default_rng(915)
    with Pool(nw, initializer=init_all, initargs=(scr,)) as pool:
        real = pool.map(w_real, range(len(B["bursts"])), chunksize=4)
        truthed = [r for r in real if r["truth_rate"]]
        untruthed = [r for r in real if not r["truth_rate"]]
        print_table("REAL bursts with sync truth (C13 SNR bins)", summarize(truthed))
        # untruthed: what does the estimator claim?
        ut = [r for r in untruthed if r["trusted"]]
        print(f"\n== REAL bursts without truth: {len(untruthed)}; trusted rate on {len(ut)}; values near 100k/150k: "
              f"{sum(1 for r in ut if min(abs(r['rate']/1e5-1), abs(r['rate']/1.5e5-1)) < 0.01)}; "
              f"families { {f: sum(r['family']==f for r in untruthed) for f in set(r['family'] for r in untruthed)} }; "
              f"SNR median {np.median([r['snr_db'] for r in untruthed if r['snr_db'] is not None]):.1f} dB")
        # degraded real and matched synthetic twins from strong truth bursts
        strong = [r for r in truthed if r["snr_db"] and r["snr_db"] >= 20 and r["dev_ref"]][:40]
        targets = [18, 15, 12, 10, 8, 6, 4, 2]
        deg_args = [(r["id"], t, r["snr_db"], 1000 + i * 17 + int(t)) for i, r in enumerate(strong) for t in targets]
        deg = pool.map(w_degraded, deg_args, chunksize=2)
        syn_args = [(r["id"], t, r["obw_hz"], 5000 + i * 31 + int(t)) for i, r in enumerate(strong) for t in targets]
        syn = pool.map(w_synth, syn_args, chunksize=2)
        syn_hi = pool.map(w_synth, [(r["id"], r["snr_db"], r["obw_hz"], 9000 + i) for i, r in enumerate(strong)])
        print_table("REAL strong bursts at their own SNR", summarize(strong))
        print_table("SYNTH twins at the strong bursts' own SNR", summarize(syn_hi))
        print_table("REAL degraded (bin by measured C13 SNR)", summarize(deg))
        print_table("SYNTH matched twins (bin by measured C13 SNR)", summarize(syn))
        # negatives
        bl = B["bursts"]
        noise_args = []
        while len(noise_args) < 200:
            t = float(rng.uniform(0.01, 44.9))
            f = float(rng.uniform(-4.3e6, 4.3e6))
            if abs(f) < 200e3:
                continue
            if any(b["t0"] - 0.012 < t < b["t1"] + 0.002 and b["f_lo"] - 300e3 < f < b["f_hi"] + 300e3 for b in bl):
                continue
            noise_args.append((t, f))
        neg915 = pool.map(w_noise915, noise_args, chunksize=4)
        B4 = json.load(open(scr + "/b433.json"))
        neg433 = pool.map(w_433, range(len(B4["bursts"])), chunksize=4)
        # FM analog: quiet reference 200 kHz window
        x = L.load_ci8(FNFM, 0, int(2 * FSFM))
        f, P = signal.welch(x, FSFM, nperseg=4096, return_onesided=False)
        cand = np.arange(-1.0e6, 1.0e6, 50e3)
        pw = [P[np.abs(f - c) < 100e3].mean() for c in cand]
        fq = float(cand[int(np.argmin(pw))])
        negfm = pool.map(w_fm, [(float(t), fq) for t in rng.uniform(0.1, 29.8, 60)])
        synneg = pool.map(w_synneg, [(k, s, 700 + i) for i, (k, s) in enumerate(
            (k, s) for k in ("nbfm_voice", "lora_chirp", "cw", "noise") for s in (10, 20, 30) for _ in range(10))])
    print(f"\nFM quiet reference offset {fq/1e3:+.0f} kHz")
    negs = dict(noise915=neg915, det433=neg433, fm_broadcast_analog=negfm)
    for k in ("nbfm_voice", "lora_chirp", "cw", "noise"):
        negs["synth_" + k] = [r for r in synneg if r["kind"] == k]
    print("\n== NEGATIVES (should be unknown / untrusted)")
    print(" set                    n  labelled(non-unknown)  trusted_rate  labels                reasons(top)")
    neg_summary = {}
    for k, rows in negs.items():
        lab = [r["family"] for r in rows if r["family"] != "unknown"]
        tr = sum(r["trusted"] for r in rows)
        reasons = {}
        for r in rows:
            for z in r.get("reasons") or []:
                reasons[z] = reasons.get(z, 0) + 1
        top = sorted(reasons.items(), key=lambda kv: -kv[1])[:3]
        labs = {l: lab.count(l) for l in set(lab)}
        neg_summary[k] = dict(n=len(rows), labelled=len(lab), trusted=tr, labels=labs, reasons=top)
        print(f" {k:20s} {len(rows):4d}  {len(lab):5d}               {tr:5d}        {str(labs):20s}  {top}")
    json.dump(dict(real=real, degraded=deg, synth=syn, synth_own_snr=syn_hi, strong_ids=[r["id"] for r in strong],
                   negatives=negs, neg_summary=neg_summary,
                   tables=dict(real=summarize(truthed), degraded=summarize(deg), synth=summarize(syn),
                               strong=summarize(strong), synth_own=summarize(syn_hi))),
              open(outdir + "/eval915_results.json", "w"), indent=1, default=float)
