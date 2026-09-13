"""Device truth via rtl_433: cut each detected burst (+-pad) to its own cs8 file at a
rtl_433-friendly rate and run rtl_433 on it. Writes a JSON map burst id -> decodes.
Scratch files go to <scratch>; never the fixture store.

usage: python3 rtl433_truth.py <bursts.json> <scratch_dir> <out.json> [fs_out=1000000] [pad_s=0.05] [ids...]
"""
import json
import os
import subprocess
import sys

import numpy as np
from scipy import signal

sys.path.insert(0, __file__.rsplit("/", 1)[0])
import s5lib as L

B = json.load(open(sys.argv[1]))
scratch, out = sys.argv[2], sys.argv[3]
fs_out = float(sys.argv[4]) if len(sys.argv) > 4 else 1e6
pad = float(sys.argv[5]) if len(sys.argv) > 5 else 0.05
ids = [int(v) for v in sys.argv[6:]] or list(range(len(B["bursts"])))
fs = B["fs"]
dec = int(round(fs / fs_out))
res = {}
os.makedirs(scratch, exist_ok=True)
for i in ids:
    b = B["bursts"][i]
    fcen = 0.5 * (b["f_lo"] + b["f_hi"])
    s0 = max(0, int((b["t0"] - pad) * fs))
    s1 = int((b["t1"] + pad) * fs)
    x = L.load_ci8(B["data"], s0, s1 - s0).astype(np.complex128)
    x *= np.exp(-2j * np.pi * fcen * (np.arange(len(x)) + s0) / fs)
    if dec > 1:
        x = signal.resample_poly(x, 1, dec)
    # keep level: scale so the peak uses ~100 LSB (rtl_433 levels are relative)
    x = x / (np.max(np.abs(x)) + 1e-9) * 100
    iq = np.empty(2 * len(x), np.int8)
    iq[0::2] = np.clip(np.round(x.real), -127, 127)
    iq[1::2] = np.clip(np.round(x.imag), -127, 127)
    fn = f"{scratch}/b{i:04d}_{(B['fc']+fcen)/1e6:.4f}M_{int(fs/dec)}sps.cs8"
    iq.tofile(fn)
    p = subprocess.run(["rtl_433", "-r", f"cs8:{fn}", "-s", str(int(fs / dec)), "-F", "json",
                        "-M", "protocol", "-M", "level", "-M", "bits"],
                       capture_output=True, text=True, timeout=120)
    decs = []
    for line in p.stdout.splitlines():
        line = line.strip()
        if line.startswith("{"):
            try:
                decs.append(json.loads(line))
            except json.JSONDecodeError:
                pass
    res[i] = dict(rf_hz=B["fc"] + fcen, file=fn, decodes=decs)
    os.remove(fn)
    if decs:
        print(i, [(d.get("model"), d.get("protocol"), d.get("mod"), d.get("freq")) for d in decs])
json.dump(res, open(out, "w"), indent=1)
print("bursts with decodes:", sum(1 for v in res.values() if v["decodes"]), "/", len(res))
