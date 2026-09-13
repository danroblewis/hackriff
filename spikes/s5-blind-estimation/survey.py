"""Quick 1 ms energy survey of a ci8 SigMF capture: noise median, active fraction, segments.

usage: python3 survey.py <file.sigmf-data> <fs> [thr_db=3] [max_list=60]
"""
import sys
import numpy as np

fn, fs = sys.argv[1], float(sys.argv[2])
thr_db = float(sys.argv[3]) if len(sys.argv) > 3 else 3.0
max_list = int(sys.argv[4]) if len(sys.argv) > 4 else 60
raw = np.memmap(fn, dtype=np.int8, mode="r")
n = len(raw) // 2
blk = int(fs * 0.001)
nb = n // blk
p = np.empty(nb)
for i in range(0, nb, 5000):
    j = min(nb, i + 5000)
    a = raw[2 * i * blk:2 * j * blk].astype(np.float32).reshape(-1, 2)
    p[i:j] = (a[:, 0] ** 2 + a[:, 1] ** 2).reshape(-1, blk).mean(1)
db = 10 * np.log10(p + 1e-9)
med = np.median(db)
print(f"dur {n/fs:.1f}s median {med:.2f} dB p99 {np.percentile(db,99):.2f} max {db.max():.2f}")
act = db > med + thr_db
d = np.diff(np.r_[0, act.astype(int), 0])
st, en = np.where(d == 1)[0], np.where(d == -1)[0]
print(f"active frac {act.mean():.4f} nseg {len(st)}")
for s, e in list(zip(st, en))[:max_list]:
    print(f"{s/1000:9.3f}s dur {e-s:5d}ms peak +{db[s:e].max()-med:5.1f}dB")
