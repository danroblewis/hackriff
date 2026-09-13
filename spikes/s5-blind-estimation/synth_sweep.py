"""Synthetic family / rate sweep through the same 8-bit chain: OOK, 2-FSK, GFSK, BPSK, QPSK
vs in-band SNR. Gives the family confusion, trusted-and-wrong counts and the SNR floor.

usage: python3 synth_sweep.py <results_dir> [trials=20] [workers=20]
"""
import json
import sys
from multiprocessing import Pool

import numpy as np

sys.path.insert(0, __file__.rsplit("/", 1)[0])
import s5lib as L
import synth as S

SNRS = [0, 3, 6, 9, 12, 15, 20, 25, 30]
NOISE_RMS = 3.21


def gen_qpsk(rate, nsym, fs, alpha=0.35):
    i, _ = S.gen_bpsk(rate, nsym, fs, alpha)
    q, r = S.gen_bpsk(rate, nsym, fs, alpha)
    return (i.real + 1j * q.real) / np.sqrt(2), r


CASES = {
    # name: (generator(fs) -> (sig, true_rate, true_dev), fs, family)
    "OOK_4k": (lambda fs: (S.gen_ook(4000, 200, fs)[0], 4000, None), 250e3, "OOK"),
    "FSK_9k6_h1": (lambda fs: (S.gen_fsk(9600, 4800, 400, fs, bt=None)[0], 9600, 4800), 250e3, "FSK"),
    "GFSK_50k_h05": (lambda fs: (S.gen_fsk(50e3, 12.5e3, 500, fs, bt=0.5)[0], 50e3, 12.5e3), 1e6, "FSK"),
    "BPSK_25k_a035": (lambda fs: (lambda s: (s[0], s[1], None))(S.gen_bpsk(25e3, 500, fs)), 250e3, "BPSK"),
    "QPSK_25k_a035": (lambda fs: (lambda s: (s[0], s[1], None))(gen_qpsk(25e3, 500, fs)), 250e3, "QPSK"),
}


def clean_obw(name):
    gen, fs, _ = CASES[name]
    S.rng = np.random.default_rng(1)
    sig, _, _ = gen(fs)
    p = L.c13_params(sig * 30, fs, (np.random.default_rng(2).normal(size=4096) * 1e-3).astype(complex))
    return p["obw99_hz"]


def work(args):
    name, snr, obw, seed = args
    gen, fs, fam = CASES[name]
    S.rng = np.random.default_rng(seed)
    sig, R, dev = gen(fs)
    cfo = S.rng.uniform(-0.05, 0.05) * obw
    y, npad = S.embed(sig, fs, snr, obw, noise_rms=NOISE_RMS, pad_s=0.004 if fs < 5e5 else 0.002, cfo_hz=cfo)
    pad = npad / fs
    b = dict(t0=pad, t1=pad + len(sig) / fs, f_lo=-obw / 2, f_hi=obw / 2)
    x, fso, _, np_ = L.extract_burst(None, fs, b, pad_s=pad * 0.9, bw_pad=2.0, x_full=y)
    noise = x[: max(64, np_ - int(0.0005 * fso))]
    p = L.c13_params(x[np_:len(x) - np_], fso, noise)
    if p.get("obw99_hz") is None:
        return dict(name=name, snr=snr, fam_true=fam, family="unknown", rate_ok=False, trusted=False, dev_err=None,
                    snr_meas=None)
    r = L.classify_and_estimate(x[np_ // 2:len(x) - np_ // 2], fso, noise, p["obw99_hz"], p["snr_db"], cfo_hz=p["cfo_hz"])
    ok = r["rate"] is not None and abs(r["rate"] / R - 1) < 0.01
    de = abs(r["deviation_hz"] / dev - 1) if (dev and r.get("deviation_hz")) else None
    return dict(name=name, snr=snr, snr_meas=p["snr_db"], fam_true=fam, family=r["family"], rate=r["rate"],
                rate_ok=bool(ok), trusted=bool(r["rate_trusted"]), cons_trusted=bool(r.get("rate_consensus_trusted")),
                dev_err=de, reasons=r.get("reasons"))


if __name__ == "__main__":
    out = sys.argv[1]
    trials = int(sys.argv[2]) if len(sys.argv) > 2 else 20
    nw = int(sys.argv[3]) if len(sys.argv) > 3 else 20
    obws = {n: clean_obw(n) for n in CASES}
    print("clean OBW99:", {k: round(v) for k, v in obws.items()})
    args = [(n, s, obws[n], 10000 * (i + 1) + s * 100 + t) for i, n in enumerate(CASES) for s in SNRS for t in range(trials)]
    with Pool(nw) as pool:
        rows = pool.map(work, args, chunksize=4)
    print(f"\ncase            SNR  fam_ok  fam_wrong  rate<1%  trusted  t_wrong  dev<10%  (n={trials})")
    table = []
    for n in CASES:
        for s in SNRS:
            rr = [r for r in rows if r["name"] == n and r["snr"] == s]
            fam_ok = np.mean([r["family"] == r["fam_true"] for r in rr])
            fam_wrong = sum(r["family"] not in (r["fam_true"], "unknown") for r in rr)
            rate_ok = np.mean([r["rate_ok"] for r in rr])
            tr = np.mean([r["trusted"] for r in rr])
            tw = sum(r["trusted"] and not r["rate_ok"] for r in rr)
            devs = [r["dev_err"] for r in rr if r["dev_err"] is not None]
            d10 = float(np.mean(np.array(devs) < 0.1)) if devs else None
            table.append(dict(case=n, snr=s, fam_ok=float(fam_ok), fam_wrong=int(fam_wrong), rate_ok=float(rate_ok),
                              trusted=float(tr), trusted_wrong=int(tw), dev_within10=d10, dev_n=len(devs)))
            print(f"{n:15s} {s:3d}   {fam_ok:.2f}   {fam_wrong:3d}       {rate_ok:.2f}    {tr:.2f}    {tw:3d}     "
                  f"{'-' if d10 is None else f'{d10:.2f}'}")
    json.dump(dict(obw=obws, table=table, rows=rows), open(out + "/synth_sweep_results.json", "w"), indent=1, default=float)
