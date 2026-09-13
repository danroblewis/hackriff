"""Semi-synthetic positive control for the gain-step ghost test.

The real captures contain no gain-dependent ghosts (stations are weak against the band
noise), so the test's ability to *catch* IMD is checked here: two strong FM-like carriers
(95.1 and 97.7 MHz, odd tenths, so their IM3 lands on the raster at 92.5 / 100.3 MHz like a
real ghost would) are added to the real 98 MHz mid-gain IQ, then a memoryless cubic
nonlinearity y = g x (1 - a |g x|^2) is applied at two drive levels 10 dB apart (the
external-noise-limited regime: noise scales with the drive).  No 8-bit requantisation.
a = 0 is the negative control.  Output: results/imd_control.json."""
import json, os
import numpy as np
import analyze_iq as A
import s4lib as L

NAME = "urban_98M_20M_l24g20a0"
FS = 20e6
FC = 98e6
CARRIERS = [(95.1e6, 0.1), (97.7e6, 0.1)]  # (frequency, amplitude): -20 dBFS each at drive 0 dB
TONES = [(400.0, 25e3, 0.3), (1130.0, 20e3, 1.7), (2710.0, 15e3, 4.1)]  # (audio Hz, peak dev Hz, phase)


def carriers(s0, n):
    t = (s0 + np.arange(n)) / FS
    out = np.zeros(n, np.complex128)
    for i, (f, amp) in enumerate(CARRIERS):
        ph = 2 * np.pi * (f - FC) * t
        for fa, dev, p in TONES:
            ph += (dev / fa) * np.sin(2 * np.pi * fa * t + p + i)
        out += amp * np.exp(1j * ph)
    return out


def make(g_db, a):
    g = 10 ** (g_db / 20)

    def tf(x, s0):
        xg = g * (x.astype(np.complex128) + carriers(s0, x.size))
        return xg * (1 - a * (xg.real ** 2 + xg.imag ** 2))
    return tf


def run(a):
    cfg = {A.PRIMARY: A.CONFIGS[A.PRIMARY]}
    lo = A.process(NAME, transform=make(0, a), tag=f"ctl_a{a}_drive0", meta_over=dict(lna=0, vga=0, amp=False), configs=cfg, save=False)
    hi = A.process(NAME, transform=make(10, a), tag=f"ctl_a{a}_drive10", meta_over=dict(lna=10, vga=0, amp=False), configs=cfg, save=False)
    for c in (lo, hi):
        A.in_capture_flags(c)
    gs = A.gain_step(lo, hi)
    A.apply_gain_step(gs, lo, hi)
    ghosts = [r for r in gs["rows"] if r["verdict"] == "suspect_imd"]
    fs, prods = A.im3_products(lo["em"], top=2)
    hits, chance = A.product_hits([r["e"]["f_center"] for r in ghosts], prods, FC)
    known_flagged = [f / 1e6 for f in A.KNOWN_FM if any(e["f_lo"] <= f <= e["f_hi"] and e["flags"]["suspect_imd"] for e in hi["em"])]
    raster_real = [e for e in lo["em"] if not e["flags"]["edge"] and A.fm_raster(e["f_center"])[1] <= 40e3 and e["bw"] >= 40e3
                   and e["snr_db"] >= 10 and all(abs(e["f_center"] - p) > 60e3 for p in prods)
                   and all(abs(e["f_center"] - f) > 100e3 for f, _ in CARRIERS)]
    raster_flagged = [e for e in raster_real if e["flags"]["suspect_imd"]]
    im3_expected = [p for p in prods if abs(p - FC) <= A.EDGE_HZ]
    im3_found = [p for p in im3_expected if any(e["f_lo"] - 30e3 <= p <= e["f_hi"] + 30e3 for e in hi["em"])]
    im3_flagged = [p for p in im3_expected if any(e["f_lo"] - 30e3 <= p <= e["f_hi"] + 30e3 and e["flags"]["suspect_imd"] for e in hi["em"])]
    return dict(a=a, G_lin=gs["G_lin"], dfloor=gs["dfloor"], bound=gs["dsnr_real_bound"],
                emitters_drive0=len(lo["em"]), emitters_drive10=len(hi["em"]),
                verdicts={v: sum(1 for r in gs["rows"] if r["verdict"] == v) for v in
                          ("linear", "compressed", "suspect_imd", "inconclusive_weak", "inconclusive_bursty")},
                ghosts=[dict(f_mhz=round(r["e"]["f_center"] / 1e6, 4), snr_lo=round(r["snrA"], 1), snr_hi=round(r["snrB"], 1),
                             dsnr=round(r["dsnr"], 1)) for r in ghosts],
                strong=[round(f / 1e6, 3) for f in fs], im3_expected_mhz=[round(p / 1e6, 3) for p in im3_expected],
                im3_found_at_drive10=len(im3_found), im3_flagged=len(im3_flagged),
                ghosts_on_predicted_im3=hits, chance_fraction=chance,
                known_fm_flagged=known_flagged, raster_real_stations=len(raster_real), raster_real_flagged=len(raster_flagged))


def main():
    res = [run(a) for a in (0.0, 0.9, 3.0)]
    for r in res:
        print(json.dumps(r, default=float))
    json.dump(res, open(os.path.join(A.RES, "imd_control.json"), "w"), indent=1, default=float)


if __name__ == "__main__":
    main()
