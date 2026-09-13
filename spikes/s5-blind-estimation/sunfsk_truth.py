"""Independent truth for the 902-928 MHz bursts: trial-decode IEEE 802.15.4g SUN-FSK PHY
frames (the Wi-SUN FAN PHY) at the standard 2-FSK rates, *without* using the blind
estimator. A burst counts as truth only if SFD + PHR are found AND the PSDU FCS (CRC-16 or
CRC-32, with or without PN9 whitening) passes. Output is PHY metadata only (rate, SFD,
frame length, FCS type, CRC ok); payloads are never printed or stored.

usage: python3 sunfsk_truth.py <bursts.json> <out.json> [min_peak_db=12] [ids...]
"""
import json
import sys
import zlib

import numpy as np

sys.path.insert(0, __file__.rsplit("/", 1)[0])
import s5lib as L

RATES = [50e3, 100e3, 150e3, 200e3, 300e3]
SFDS = {0x904E: "uncoded-0", 0x7A0E: "uncoded-1", 0x6F4E: "coded-0", 0x632D: "coded-1"}


def pn9_bits(n):
    s = 0x1FF
    out = np.empty(n, np.uint8)
    for i in range(n):
        out[i] = s & 1
        nb = (s & 1) ^ ((s >> 5) & 1)
        s = (s >> 1) | (nb << 8)
    return out


PN9 = pn9_bits(2047 * 8 + 64)


def crc16_kermit(data: bytes) -> int:
    crc = 0
    for b in data:
        crc ^= b
        for _ in range(8):
            crc = (crc >> 1) ^ 0x8408 if crc & 1 else crc >> 1
    return crc


def bits_to_bytes_lsb(bits):
    n = len(bits) // 8
    b = bits[: n * 8].reshape(n, 8)
    return bytes((b * (1 << np.arange(8))).sum(1).astype(np.uint8))


def try_frame(bits, start):
    """bits after the SFD; returns dict if a PHR+PSDU with valid FCS is found."""
    if start + 16 > len(bits):
        return None
    phr = bits[start:start + 16]
    fcs_type = int(phr[3])
    dw = int(phr[4])
    lens = {"msb": int("".join(map(str, phr[5:16])), 2),
            "lsb": int("".join(map(str, phr[5:16][::-1])), 2)}
    for order, ln in lens.items():
        if not 3 <= ln <= 2047 or start + 16 + 8 * ln > len(bits):
            continue
        psdu = bits[start + 16:start + 16 + 8 * ln].copy()
        for white in ([True, False] if dw else [False, True]):
            p = psdu ^ PN9[: len(psdu)] if white else psdu
            data = bits_to_bytes_lsb(p)
            ok16 = crc16_kermit(data[:-2]) == int.from_bytes(data[-2:], "little")
            ok32 = (zlib.crc32(data[:-4]) & 0xFFFFFFFF) == int.from_bytes(data[-4:], "little")
            if ok16 or ok32:
                return dict(len_octets=ln, len_order=order, fcs=("crc32" if ok32 else "crc16"),
                            fcs_type_bit=fcs_type, dw_bit=dw, whitened=white)
    return None


def decode_burst(x, fs):
    best = None
    for R in RATES:
        sps = fs / R
        if sps < 3:
            continue
        # channel filter: +-(R/2 + deviation) with h <= 1  ->  cutoff ~ R
        fi = L.inst_freq(L.lowpass(x, fs, 1.0 * R), fs)
        v = L.moving_avg(fi, max(1, int(round(sps * 0.6))))
        cands = []
        for ph in np.linspace(0, sps, 12, endpoint=False):
            idx = (ph + np.arange(int((len(v) - 1 - ph) / sps)) * sps).astype(int)
            s = v[idx]
            cands.append((np.mean(np.abs(s - np.median(s))), ph, s))
        cands.sort(key=lambda t: -t[0])
        for _, ph, s in cands[:3]:
            mid = np.median(s)
            bits = (s > mid).astype(np.uint8)
            for pol in (0, 1):
                b = bits ^ pol
                bs = "".join(map(str, b))
                for sfd, name in SFDS.items():
                    for bo in ("msb", "rev"):
                        pat = format(sfd, "016b") if bo == "msb" else format(sfd, "016b")[::-1]
                        k = bs.find(pat)
                        while k >= 0:
                            pre = bs[max(0, k - 16):k]
                            if len(pre) == 16 and pre.count("01") + pre.count("10") >= 12:
                                fr = try_frame(b, k + 16)
                                if fr:
                                    fr.update(rate=R, sfd=name, sfd_order=bo, polarity=pol,
                                              sfd_symbol=int(k), sps=float(sps))
                                    return fr
                                if best is None:
                                    best = dict(rate=R, sfd=name, sfd_order=bo, polarity=pol,
                                                sfd_symbol=int(k), crc_ok=False)
                            k = bs.find(pat, k + 1)
    return best


if __name__ == "__main__":
    B = json.load(open(sys.argv[1]))
    out = sys.argv[2]
    minpk = float(sys.argv[3]) if len(sys.argv) > 3 else 12.0
    ids = [int(v) for v in sys.argv[4:]] or [b["id"] for b in B["bursts"] if b["peak_db"] >= minpk]
    res = {}
    n_crc = 0
    for i in ids:
        b = B["bursts"][i]
        x, fs, fcen, npad = L.extract_burst(B["data"], B["fs"], b, pad_s=0.002, bw_pad=2.0,
                                            fs_out_min=1.2e6)
        r = decode_burst(x, fs)
        if r and "len_octets" in r:
            r["crc_ok"] = True
            n_crc += 1
        res[i] = r
        if r:
            print(i, f"{b['rf_hz']/1e6:.4f}", {k: v for k, v in r.items() if k not in ("sps",)})
    json.dump(res, open(out, "w"), indent=1)
    print(f"CRC-verified frames: {n_crc} / {len(ids)} bursts; SFD-only: "
          f"{sum(1 for v in res.values() if v and not v.get('crc_ok'))}")
