"""Narrowband spectrogram survey of a ci8 capture.

Computes a |STFT|^2 with nfft bins, averaged over `avg` frames, estimates a per-bin
noise floor (median over time), and reports time/frequency cells exceeding it.
Also saves a max-hold and mean PSD and a decimated spectrogram PNG.

usage: python3 specsurvey.py <file.sigmf-data> <fs> <fc> <outprefix> [nfft=4096] [avg=8] [thr_db=8]
"""
import sys
import numpy as np
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt

fn, fs, fc, out = sys.argv[1], float(sys.argv[2]), float(sys.argv[3]), sys.argv[4]
nfft = int(sys.argv[5]) if len(sys.argv) > 5 else 4096
avg = int(sys.argv[6]) if len(sys.argv) > 6 else 8
thr_db = float(sys.argv[7]) if len(sys.argv) > 7 else 8.0
raw = np.memmap(fn, dtype=np.int8, mode="r")
n = len(raw) // 2
hop = nfft * avg
nfr = n // hop
win = np.hanning(nfft).astype(np.float32)
S = np.empty((nfr, nfft), dtype=np.float32)
chunk = 256
for i in range(0, nfr, chunk):
    j = min(nfr, i + chunk)
    a = raw[2 * i * hop:2 * j * hop].astype(np.float32).reshape(-1, 2)
    x = (a[:, 0] + 1j * a[:, 1]).astype(np.complex64).reshape(j - i, avg, nfft)
    X = np.fft.fftshift(np.fft.fft(x * win, axis=-1), axes=-1)
    S[i:j] = (np.abs(X) ** 2).mean(1)
f = (np.arange(nfft) - nfft // 2) * fs / nfft
noise = np.median(S, axis=0)
R = 10 * np.log10(S / noise[None, :])
tfr = hop / fs
np.save(out + "_maxhold.npy", np.stack([f, 10 * np.log10(S.max(0)), 10 * np.log10(noise)]))
# exclude DC +-3 bins
dc = slice(nfft // 2 - 3, nfft // 2 + 4)
R[:, dc] = 0
hit = R > thr_db
print(f"dur {n/fs:.1f}s frames {nfr} frame {tfr*1e3:.1f}ms bin {fs/nfft:.0f}Hz hits {hit.sum()}")
# cluster by frequency: count frames with hits per 25 kHz slot
col = hit.any(0)
fb = np.round(f / 25e3) * 25e3
for fv in np.unique(fb[col]):
    m = (fb == fv)
    rows = hit[:, m].any(1)
    d = np.diff(np.r_[0, rows.astype(int), 0])
    st = np.where(d == 1)[0]
    en = np.where(d == -1)[0]
    pk = R[:, m].max()
    print(f"{(fc+fv)/1e6:10.4f} MHz  frames {rows.sum():6d}  events {len(st):4d}  peak +{pk:5.1f} dB  first {st[0]*tfr:7.2f}s")
dec = max(1, nfr // 1500)
Sd = R[: nfr // dec * dec].reshape(-1, dec, nfft).max(1)
fdec = max(1, nfft // 1024)
Sd = Sd[:, : nfft // fdec * fdec].reshape(Sd.shape[0], -1, fdec).max(2)
plt.figure(figsize=(8, 6))
plt.imshow(Sd, aspect="auto", origin="lower", cmap="viridis", vmin=0, vmax=25,
           extent=[(fc - fs / 2) / 1e6, (fc + fs / 2) / 1e6, 0, n / fs])
plt.xlabel("MHz"); plt.ylabel("s"); plt.colorbar(label="dB over per-bin median")
plt.title(out.split("/")[-1])
plt.tight_layout(); plt.savefig(out + "_spectrogram.png", dpi=70)
