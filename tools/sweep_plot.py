#!/usr/bin/env python3
"""Plot hackrf_sweep CSV output as spectrum + waterfall, and list the strongest signals.

  hackrf_sweep -f 88:108 -w 50000 -N 30 -l 24 -g 20 -r fm.csv
  python3 tools/sweep_plot.py fm.csv            # writes fm.png
"""
import csv
import sys
from pathlib import Path

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
import numpy as np


def load_sweeps(path):
    rows = [(int(r[2]), float(r[4]), [float(v) for v in r[6:]])
            for r in csv.reader(open(path)) if len(r) > 6]
    if not rows:
        sys.exit(f"{path}: no sweep rows")
    start = min(lo for lo, _, _ in rows)
    sweeps, cur = [], {}
    for lo, width, dbs in rows:
        if lo == start and cur:            # rows within a sweep aren't sorted; a sweep restarts at the lowest hz_low
            sweeps.append(cur)
            cur = {}
        for i, v in enumerate(dbs):
            cur[lo + (i + 0.5) * width] = v
    sweeps.append(cur)
    full = max(len(s) for s in sweeps)
    sweeps = [s for s in sweeps if len(s) == full]   # drop a partial sweep cut off by Ctrl-C
    if len(sweeps) > 2:
        sweeps = sweeps[1:]                          # first sweep reads low while gain/PLL settle
    freqs = np.array(sorted(sweeps[0]))
    return freqs, np.array([[s[f] for f in freqs] for s in sweeps])


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    path = Path(sys.argv[1])
    out = Path(sys.argv[2]) if len(sys.argv) > 2 else path.with_suffix(".png")
    freqs, db = load_sweeps(path)
    mhz = freqs / 1e6
    mean = db.mean(axis=0)
    floor = np.median(mean)

    sep = max(3 * (freqs[1] - freqs[0]), (freqs[-1] - freqs[0]) / 150)
    peaks = []
    for i in np.argsort(mean)[::-1]:
        if all(abs(freqs[i] - freqs[j]) > sep for j in peaks):
            peaks.append(i)
        if len(peaks) == 15:
            break

    print(f"{len(db)} sweep(s), {mhz[0]:.3f}-{mhz[-1]:.3f} MHz, {len(freqs)} bins, noise floor {floor:.1f} dB")
    print("strongest signals:")
    for i in sorted(peaks):
        print(f"  {mhz[i]:10.3f} MHz  {mean[i]:6.1f} dB  (+{mean[i] - floor:4.1f} over floor)")

    fig, axes = plt.subplots(2 if len(db) > 1 else 1, 1, figsize=(14, 8 if len(db) > 1 else 4),
                             sharex=True, squeeze=False)
    ax = axes[0, 0]
    ax.plot(mhz, mean, lw=0.7, label="mean")
    ax.plot(mhz, db.max(axis=0), lw=0.5, alpha=0.5, label="max hold")
    ax.set_ylabel("dB")
    ax.grid(alpha=0.3)
    ax.legend(loc="upper right")
    ax.set_title(f"{path.name}: {len(db)} sweep(s)")
    if len(db) > 1:
        wf = axes[1, 0]
        wf.imshow(db, aspect="auto", origin="upper", cmap="viridis",
                  extent=[mhz[0], mhz[-1], len(db), 0],
                  vmin=np.percentile(db, 5), vmax=np.percentile(db, 99.7))
        wf.set_ylabel("sweep #")
    axes[-1, 0].set_xlabel("MHz")
    fig.tight_layout()
    fig.savefig(out, dpi=120)
    print(f"wrote {out}")


if __name__ == "__main__":
    main()
