"""Monte Carlo false-component rate of the full frame-level chain on pure Gamma(10) noise
with a known floor (the ideal case: the real-data rate can only be worse).  Uses the same
frame geometry as the IQ analysis (4096 bins x 4.88 kHz, 2.05 ms frames).
Usage: python3 synth_chain_fa.py [chunks_per_config]   -> results/synth_chain_fa.json"""
import json, os, sys
from multiprocessing import Pool
import numpy as np
from scipy import stats
import s4lib as L

HERE = os.path.dirname(os.path.abspath(__file__))
CONFIGS = {
    "or_on1e-6_off1e-3_min3": dict(L.DEFAULT),
    "or_on1e-6_off1e-3_min2": dict(L.DEFAULT, min_frames=2),
    "or_on1e-7_off1e-3_min2": dict(L.DEFAULT, pfa=1e-7, floor_pfa=1e-7, min_frames=2),
    "os_only_on1e-6_off1e-3_min3": dict(L.DEFAULT, mode="os"),
    "or_on1e-6_hyst3dB_min2": dict(L.DEFAULT, hyst_db=3.0, min_frames=2),
    "or_on1e-4_off1e-3_min3": dict(L.DEFAULT, pfa=1e-4, floor_pfa=1e-4),
}
FRAMES, BINS, EDGE = 2000, 4096, 64


def work(args):
    cname, seed = args
    rng = np.random.default_rng(seed)
    P = rng.gamma(10, 0.1, size=(FRAMES, BINS)).astype(np.float32)
    d = L.detect(dict(P=P, n_avg=10), np.ones(BINS, np.float32), CONFIGS[cname])
    return cname, sum(1 for b in d["boxes"] if b[3] >= EDGE and b[4] <= BINS - EDGE)


def main():
    """argv: [chunks_per_config] [comma-separated config names] [output suffix]"""
    chunks = int(sys.argv[1]) if len(sys.argv) > 1 else 40
    names = sys.argv[2].split(",") if len(sys.argv) > 2 else list(CONFIGS)
    suffix = sys.argv[3] if len(sys.argv) > 3 else ""
    seed0 = 7919 if suffix else 0
    jobs = [(c, seed0 + 1000 * list(CONFIGS).index(c) + j) for c in names for j in range(chunks)]
    counts = {c: 0 for c in names}
    with Pool(min(24, os.cpu_count())) as pool:
        for c, k in pool.imap_unordered(work, jobs):
            counts[c] += k
    dt = BINS * 10 / 20e6
    mhz = (BINS - 2 * EDGE) * 20.0 / BINS
    exp = mhz * chunks * FRAMES * dt / 3600
    out = {}
    for c, k in counts.items():
        up = stats.chi2.ppf(0.95, 2 * k + 2) / 2
        out[c] = dict(false_boxes=k, mhz_hours=exp, rate_per_mhz_h=k / exp, upper95_per_mhz_h=up / exp)
        print(c, out[c], flush=True)
    json.dump(out, open(os.path.join(HERE, "results", f"synth_chain_fa{suffix}.json"), "w"), indent=1)


if __name__ == "__main__":
    main()
