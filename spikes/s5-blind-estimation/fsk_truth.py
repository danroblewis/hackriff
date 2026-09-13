"""Decoder-independent truth for the 902-928 MHz FHSS bursts.

The 802.15.4g SUN-FSK trial decode (sunfsk_truth.py) found no SFD/CRC, but the strong
bursts all demodulate at exactly 100 or 150 kbit/s into a long 0101 preamble followed by
the same sync bits 0000110001011111 (0x0C5F). Truth for a burst = trial demodulation at a
*fixed standard rate* R in {50,100,150,200,300} kbit/s (no blind estimator involved) finds
>= 24 alternating preamble bits immediately followed by that sync (<= 1 bit error), at
exactly one R. Rate truth is then R nominal (+-50 ppm tx, +-20 ppm HackRF: << 1%).

Deviation reference: median |IF - centre| at symbol centres inside runs of >= 3 equal bits
(full GFSK deviation reached), measured on this fixed-rate demod path. It is a
self-consistent reference, not an independent spec value (protocol unidentified).

Only PHY metadata is kept (rate, preamble length, sync position, deviation, SNR).

usage: python3 fsk_truth.py <bursts.json> <out.json> [min_peak_db=12]
"""
import json
import sys

import numpy as np

sys.path.insert(0, __file__.rsplit("/", 1)[0])
import s5lib as L

RATES = [50e3, 100e3, 150e3, 200e3, 300e3]
SYNC = "0000110001011111"


def demod_fixed(x, fs, R, npad):
    sps = fs / R
    xf = L.lowpass(x, fs, 1.0 * R)
    fi = L.inst_freq(xf, fs)
    v = L.moving_avg(fi, max(1, int(round(sps * 0.6))))
    nz = np.sqrt(np.mean(np.abs(xf[: max(16, npad // 2)]) ** 2))
    best = None
    for ph in np.linspace(0, sps, 16, endpoint=False):
        idx = (ph + np.arange(int((len(v) - 1 - ph) / sps)) * sps).astype(int)
        s = v[idx]
        on = np.abs(xf[idx]) > 3 * nz
        if on.sum() < 32:
            continue
        q, _ = L.kmeans_1d(s[on], 2)
        mid = q.mean()
        sc = np.mean(np.abs(s[on] - mid))
        if best is None or sc > best[0]:
            best = (sc, ph, s, on, mid, idx)
    return best, fi


def find_sync(bits):
    bs = "".join(map(str, bits))
    for pol in (0, 1):
        b = bs if pol == 0 else bs.translate(str.maketrans("01", "10"))
        k = b.find("01" * 12)
        while k >= 0:
            j = k
            while b[j:j + 2] == "01":
                j += 2
            for jj in (j, j - 1, j + 1):
                w = b[jj:jj + 16]
                if len(w) == 16 and sum(c1 != c2 for c1, c2 in zip(w, SYNC)) <= 1:
                    return dict(polarity=pol, preamble_bits=jj - k, sync_bit=jj)
            k = b.find("01" * 12, j + 2)
    return None


def truth_for(x, fs, npad):
    hits = []
    for R in RATES:
        if fs / R < 3:
            continue
        best, fi = demod_fixed(x, fs, R, npad)
        if best is None:
            continue
        _, ph, s, on, mid, idx = best
        st = np.where(on)[0]
        bits = (s > mid).astype(int)[st[0]:st[-1] + 1]
        sy = find_sync(bits)
        if sy:
            # deviation reference from runs of >= 3 equal bits
            b = bits
            on_sub = on[st[0]:st[-1] + 1]
            full = np.zeros(len(b), bool)
            for i in range(1, len(b) - 1):
                full[i] = b[i - 1] == b[i] == b[i + 1] and on_sub[i - 1] and on_sub[i] and on_sub[i + 1]
            vals = s[st[0]:st[-1] + 1][full] - mid
            dev_ref = float(np.median(np.abs(vals))) if len(vals) > 8 else None
            hits.append(dict(rate=R, n_bits=int(len(bits)), dev_ref_hz=dev_ref, **sy))
    return hits


if __name__ == "__main__":
    B = json.load(open(sys.argv[1]))
    out = sys.argv[2]
    minpk = float(sys.argv[3]) if len(sys.argv) > 3 else 12.0
    res = {}
    for b in B["bursts"]:
        if b["peak_db"] < minpk:
            continue
        x, fs, fcen, npad = L.extract_burst(B["data"], B["fs"], b, pad_s=0.002, bw_pad=2.0, fs_out_min=1.2e6)
        hits = truth_for(x, fs, npad)
        rates = sorted({h["rate"] for h in hits})
        res[b["id"]] = dict(rf_hz=b["rf_hz"], hits=hits, truth_rate=rates[0] if len(rates) == 1 else None,
                            ambiguous=len(rates) > 1)
    json.dump(res, open(out, "w"), indent=1)
    tr = [v for v in res.values() if v["truth_rate"]]
    from collections import Counter
    print(f"bursts examined {len(res)}; with unique-rate sync truth {len(tr)}; ambiguous "
          f"{sum(v['ambiguous'] for v in res.values())}; rates {Counter(v['truth_rate'] for v in tr)}")
