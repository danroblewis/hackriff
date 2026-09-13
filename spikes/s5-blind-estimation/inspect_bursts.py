"""Plot envelope / instantaneous frequency / PSD for chosen bursts and print C13/C14 output.
usage: python3 inspect_bursts.py <bursts.json> <outdir> id [id ...]
"""
import json
import sys

import numpy as np
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt

sys.path.insert(0, __file__.rsplit("/", 1)[0])
import s5lib as L

B = json.load(open(sys.argv[1]))
outdir = sys.argv[2]
ids = [int(v) for v in sys.argv[3:]]
fs0 = B["fs"]
for i in ids:
    b = B["bursts"][i]
    x, fs, fcen, npad = L.extract_burst(B["data"], fs0, b, pad_s=0.004, bw_pad=2.0)
    noise = x[: max(64, npad - int(0.001 * fs))]
    p = L.c13_params(x[npad:len(x) - npad], fs, noise)
    r = L.classify_and_estimate(x[npad // 2:len(x) - npad // 2], fs, noise, p["obw99_hz"], p["snr_db"],
                                cfo_hz=p["cfo_hz"])
    print(f"--- burst {i} rf {(B['fc']+fcen)/1e6:.4f} MHz fs_out {fs/1e3:.0f} kHz dur {(b['t1']-b['t0'])*1e3:.1f} ms")
    print("  C13:", {k: (round(v, 1) if isinstance(v, float) else v) for k, v in p.items() if k != "N0"})
    print("  family", r["family"], round(r["family_conf"], 2), "reasons", r["reasons"])
    print("  feats", {k: (round(v, 3) if isinstance(v, float) else v) for k, v in r.get("features", {}).items()})
    for m in r.get("rate_methods", []):
        print("   ", m["method"], m.get("value"), "sig", round(m.get("sig_db", 0), 1), m.get("reason", ""))
    c = r.get("consensus", {})
    ls = c.get("ls") or {}
    print("  rate", r["rate"], "trusted", r["rate_trusted"], "support", c.get("support"), "LS jit",
          ls.get("jitter_ui"), "odd", ls.get("odd_frac"), "dev", r.get("deviation_hz"), "h", r.get("mod_index_h"))
    t = np.arange(len(x)) / fs * 1e3
    fi = L.inst_freq(x, fs)
    fig, ax = plt.subplots(3, 1, figsize=(9, 7))
    ax[0].plot(t, np.abs(x), lw=0.5); ax[0].set_ylabel("|x|")
    ax[1].plot(t[1:], L.moving_avg(fi, 3) / 1e3, lw=0.4); ax[1].set_ylabel("IF kHz"); ax[1].set_ylim(-fs / 4e3, fs / 4e3)
    from scipy import signal
    f, P = signal.welch(x, fs, nperseg=min(1024, len(x)), return_onesided=False)
    ax[2].plot(np.fft.fftshift(f) / 1e3, 10 * np.log10(np.fft.fftshift(P))); ax[2].set_xlabel("kHz")
    fig.suptitle(f"burst {i}"); fig.tight_layout(); fig.savefig(f"{outdir}/burst_{i}.png", dpi=60); plt.close(fig)
