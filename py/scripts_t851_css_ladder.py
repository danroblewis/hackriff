"""T-851: CSS (LoRa) calibrated-bit discount, the T-547/T-619 method in numpy.

Single-use measurement script (docs/21 §11). css_demod has no implementation, so the ladder is the
block's own first stage: dechirp + FFT (fold to BW), metrics peak_ratio and bin_run.
"""
import json, sys
import numpy as np

FS, BW = 500e3, 125e3
rng = np.random.default_rng(851)
GAINS = [-11.1, -5.1, 0.9, 6.9, 13.0, 19.0, 25.0, 31.1, 34.6, 37.1, 40.0]  # dB, as §1 grid


def chirp_syms(sf, bw, nsym, fs=FS):
    n = int(round(fs * (2 ** sf) / bw))
    t = np.arange(n) / fs
    k = bw / (2 ** sf / bw)
    ph = 2 * np.pi * (-0.5 * bw * t + 0.5 * k * t * t)
    return np.tile(np.exp(1j * ph), nsym), n


def fsk(nsym, n, rate=2400.0):
    sps = FS / rate
    bits = rng.integers(0, 2, int(nsym * n / sps) + 2)
    f = np.repeat(np.where(bits, 1, -1) * 1800.0, int(sps))[: nsym * n]
    return np.exp(1j * 2 * np.pi * np.cumsum(f) / FS)


def cn(shape):
    return (rng.normal(size=shape) + 1j * rng.normal(size=shape)) / np.sqrt(2)


def adc(x, gain_db):
    if gain_db is None:
        return x
    g = 10 ** (gain_db / 20)
    q = lambda v: np.clip(np.round(v * g), -128, 127)
    return q(x.real) + 1j * q(x.imag)


def metrics(x, sf, bw, nsym):
    """x: (W, nsym*n) complex. Returns peak_ratio, bin_run (both high = evidence)."""
    n = int(round(FS * (2 ** sf) / bw))
    W = x.shape[0]
    s = x[:, : nsym * n].reshape(W, nsym, n)
    t = np.arange(n) / FS
    k = bw / (2 ** sf / bw)
    down = np.exp(-1j * 2 * np.pi * (-0.5 * bw * t + 0.5 * k * t * t))
    X = np.abs(np.fft.fft(s * down, axis=2)) ** 2
    m = int(FS / bw)  # fold oversampling
    X = X[:, :, : n // 2 * 0 + n]  # full
    # fold the +-bw/2 band: bins [0,n/m) and [n-n/m,n)
    b = n // m
    F = X[:, :, :b] + X[:, :, n - b:]
    pk = F.max(2)
    ratio = (pk / (F.mean(2) + 1e-30)).mean(1)
    arg = F.argmax(2)
    run = np.zeros(W)
    for w in range(W):
        run[w] = np.bincount(arg[w]).max()
    return ratio, run


def sigma(x):
    return float(np.std(x.real))


def build(kind, sf, nsym, W, snr_db):
    n = int(round(FS * (2 ** sf) / BW))
    L = nsym * n
    noise = cn((W, L))
    if kind == "noise":
        return noise
    if kind == "wrong_sf":   # true SF9 read at other hypothesis
        c, _ = chirp_syms(9, BW, int(np.ceil(L / (512 * 4))) + 1)
    elif kind == "wrong_bw":
        c, _ = chirp_syms(sf, 250e3, int(np.ceil(L / (n // 2))) + 1)
    elif kind == "fsk":
        c = fsk(int(np.ceil(L / n)) + 1, n)
    else:  # matched
        c, _ = chirp_syms(sf, BW, nsym + 1)
    c = c[:L]
    # random phase / start offset a symbol fraction is not needed: chirp folds
    p = 10 ** (snr_db / 10)
    a = np.sqrt(p) * np.exp(2j * np.pi * rng.random((W, 1)))
    return noise + a * c[None, :]


def tail_thr(vals, b):
    return np.quantile(vals, 1 - 2.0 ** -b)


def realised(vals, thr):
    p = max(np.mean(vals > thr), 0.5 / len(vals))
    return -np.log2(p)


def run(sf, nsym, W):
    out = []
    for name in ("peak_ratio", "bin_run"):
        pass
    kinds = {"noise": 0.0, "wrong_sf": 6.0, "wrong_bw": 6.0, "fsk": 6.0, "match": 6.0}
    # NB: matched at SNR 6 dB per-sample (chirp is spread; processing gain 27 dB @ SF9)
    raw = {k: build(k, sf, nsym, W if k == "noise" else W // 4, s) for k, s in kinds.items()}
    for gname in ["float"] + GAINS:
        g = None if gname == "float" else gname
        st = {k: metrics(adc(v, g), sf, BW, nsym) for k, v in raw.items()}
        sg = sigma(adc(raw["noise"], g)) if g is not None else float("nan")
        clip = float(np.mean(np.abs(adc(raw["noise"], g).real) >= 127)) if g is not None else 0.0
        clips = {k: (float(np.mean(np.abs(adc(v, g).real) >= 127)) if g is not None else 0.0) for k, v in raw.items()}
        out.append((gname, sg, clips, st))
    return out, {k: metrics(v, sf, BW, nsym) for k, v in raw.items()}


def main():
    res = {}
    for nsym in (8, 32):
        W = 2000 if nsym == 8 else 600
        sf = 9
        rows, fl = run(sf, nsym, W)
        rec = []
        for mi, mname in enumerate(("peak_ratio", "bin_run")):
            for b in (4, 6, 8):
                # table: tail of float noise (Null A), as ADR-0015 §2.2
                thr = tail_thr(fl["noise"][mi], b)
                for gname, sg, clip, st in rows:
                    nulls = {k: realised(st[k][mi], thr) for k in ("noise", "wrong_sf", "wrong_bw", "fsk")}
                    # per-null table (each calibrated on its own float): delta = b - realised
                    per = {}
                    for k in ("noise", "wrong_sf", "wrong_bw", "fsk"):
                        t2 = tail_thr(fl[k][mi], b)
                        per[k] = b - realised(st[k][mi], t2)
                    rec.append(dict(nsym=nsym, metric=mname, b=b, gain=gname, sigma=sg, clip=clip, clips=clip,
                                    recall=float(np.mean(st["match"][mi] > thr)),
                                    delta=per, distinct=int(len(np.unique(fl["noise"][mi])))))
        res[nsym] = rec
    json.dump(res, open("/tmp/t851/out.json", "w"))
    BUCKETS = {"none": (0, 1.1), "nominal s>=.5 clip<=30": (0.5, 0.30), "tight s>=1 clip<=10": (1.0, 0.10), "s>=2 clip<=10": (2.0, 0.10)}
    for nsym, rec in res.items():
        print("nsym", nsym)
        for m in ("peak_ratio", "bin_run"):
            for b in (4, 6, 8):
                for bn, (smin, cmax) in BUCKETS.items():
                    ds = []; rcs = []
                    for r in rec:
                        if r["metric"] != m or r["b"] != b or r["gain"] == "float" or r["sigma"] < smin:
                            continue
                        for k, d in r["delta"].items():
                            c = r["clips"] if k == "noise" else r["clips"]
                            if c.get(k, 0) <= cmax:
                                ds.append(d)
                        if all(c <= cmax for c in r["clips"].values()) or True:
                            rcs.append(r["recall"])
                    if ds:
                        print(f"  {m:10s} b={b} {bn:24s} worst d {max(ds):+.2f}  min recall {min(rcs):.2f}")

main()
