"""Detect bursts in a ci8 SigMF capture and write a JSON table.
usage: python3 run_detect.py <data> <out.json> [nfft] [avg] [thr_db] [min_frames]
"""
import json
import sys

import numpy as np

sys.path.insert(0, __file__.rsplit("/", 1)[0])
import s5lib as L

data, out = sys.argv[1], sys.argv[2]
meta = json.load(open(data.replace(".sigmf-data", ".sigmf-meta")))
fs = meta["global"]["core:sample_rate"]
fc = meta["captures"][0]["core:frequency"]
nfft = int(sys.argv[3]) if len(sys.argv) > 3 else 1024
avg = int(sys.argv[4]) if len(sys.argv) > 4 else 4
thr = float(sys.argv[5]) if len(sys.argv) > 5 else 10.0
minf = int(sys.argv[6]) if len(sys.argv) > 6 else 3
bursts, noise, tfr = L.detect_bursts(data, fs, nfft=nfft, avg=avg, thr_db=thr, min_frames=minf)
for i, b in enumerate(bursts):
    b["id"] = i
    b["rf_hz"] = fc + 0.5 * (b["f_lo"] + b["f_hi"])
json.dump(dict(data=data, fs=fs, fc=fc, frame_s=tfr, nfft=nfft, thr_db=thr, bursts=bursts),
          open(out, "w"), indent=1)
print(f"{len(bursts)} bursts, frame {tfr*1e3:.2f} ms, bin {fs/nfft:.0f} Hz")
dur = np.array([b["t1"] - b["t0"] for b in bursts])
bw = np.array([b["f_hi"] - b["f_lo"] for b in bursts])
pk = np.array([b["peak_db"] for b in bursts])
if len(bursts):
    print("dur ms pct 10/50/90:", np.percentile(dur * 1e3, [10, 50, 90]).round(2))
    print("span kHz pct 10/50/90:", np.percentile(bw / 1e3, [10, 50, 90]).round(1))
    print("peak dB pct 10/50/90:", np.percentile(pk, [10, 50, 90]).round(1))
    for b in sorted(bursts, key=lambda b: -b["peak_db"])[:25]:
        print(f"id {b['id']:4d} t {b['t0']:8.3f}-{b['t1']:8.3f}s  rf {b['rf_hz']/1e6:9.4f} MHz "
              f"span {(b['f_hi']-b['f_lo'])/1e3:7.1f} kHz dur {(b['t1']-b['t0'])*1e3:7.1f} ms pk {b['peak_db']:.1f}")
