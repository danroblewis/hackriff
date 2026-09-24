"""C4FM burst train with the check parameterisation T-622 gave 2-FSK (T-850, docs/22 A7).

The trunk scenes carry only the fixed P25 framing. This generator is the generic 4-level
counterpart of ``fsk_burst_train``: periodic C4FM bursts of preamble, sync, a 6-byte payload
and a check chosen by ``check_kind`` / ``crc_source`` / ``check_width`` / ``check_poly_hex`` /
``constant_payload``. Everything the system under test must not see is in the truth annotations.
"""

from __future__ import annotations

import math
from typing import Any

import numpy as np

from hkpy.synth import fsk, pocsag
from hkpy.synth import trunking as tk
from hkpy.synth.scenarios import Ctx, DEFAULT_START_UTC, _n, _noise_capture
from hkpy.synth.scene import db, undb

C4FM_DEFAULTS: dict[str, Any] = {
    "sample_rate": 500e3,
    "center_hz": 851.0125e6,
    "channel_offset_hz": 50e3,
    "cfo_hz": 0.0,
    "symbol_rate_bd": tk.C4FM_SYMBOL_RATE_BD,
    "snr_db": 20.0,
    "noise_dbfs": -40.0,
    "duration_s": 0.6,
    "first_burst_s": 0.03,
    "period_s": 0.12,
    "jitter_s": 0.01,
    "preamble_dibits": 16,
    "sync_hex": "5575f5ff77ff",
    "sensor_id": 0x5A3C,
    "calibration_k_db": -70.0,
    "start_utc": DEFAULT_START_UTC,
    "check_kind": "crc",       # "crc" | "bch" (POCSAG BCH(31,21)+parity, two codewords) | "nocheck"
    "check_width": 16,         # crc only: 8 / 16 / 24 / 32
    "crc_source": "template",  # "template": fixed default poly (or `check_poly_hex`);
                               # "searched": a random RevEng-catalogue entry of that width
                               #   (in-catalogue, parameters unknown to the receiver);
                               # "random": a random off-catalogue polynomial
    "check_poly_hex": None,
    "constant_payload": False,
}


def _crc_params(p: dict[str, Any], rng: np.random.Generator) -> dict[str, Any]:
    width, source = int(p["check_width"]), str(p["crc_source"])
    if source == "template":
        return fsk.crc_params_for(width, p.get("check_poly_hex"))
    if source == "searched":
        names = sorted(n for n, v in fsk.CRC_CATALOGUE.items() if v[0] == width)
        if not names:
            raise ValueError(f"no catalogue CRC of width {width} to search")
        w, poly, init, refin, refout, xorout = fsk.CRC_CATALOGUE[names[int(rng.integers(len(names)))]]
        return {"width": w, "poly": poly, "init": init, "refin": refin, "refout": refout,
                "xorout": xorout}
    if source == "random":
        while True:
            poly = int(rng.integers(1, 1 << width)) | 1
            params = {"width": width, "poly": poly, "init": (1 << width) - 1, "refin": False,
                      "refout": False, "xorout": 0}
            if fsk.crc_catalogue_name(**params) is None:
                return params
    raise ValueError(f"crc_source must be template/searched/random, got {source!r}")


def c4fm_burst_train(ctx: Ctx) -> tuple[list[Any], dict[str, Any]]:
    p = ctx.params
    fs = float(p["sample_rate"])
    scene = ctx.scene("c4fm_burst_train", fs, _n(p),
                      "hkpy.synth c4fm_burst_train: periodic C4FM bursts with a parameterised check")
    _noise_capture(scene, p, p["center_hz"], p["calibration_k_db"])
    cap = scene.captures[0]
    rate = float(p["symbol_rate_bd"])
    bw = 2 * (1800.0 + rate / 2)
    power = float(p["snr_db"]) + cap.floor_dbfs_per_hz + db(bw)
    amp = math.sqrt(undb(power))
    off = float(p["channel_offset_hz"]) + float(p["cfo_hz"])
    if abs(off) + bw / 2 > fs / 2:
        raise ValueError("c4fm channel does not fit inside the sample rate")
    kind = str(p["check_kind"])
    if kind not in ("crc", "bch", "nocheck"):
        raise ValueError(f"check_kind must be crc/bch/nocheck, got {kind!r}")
    sync = bytes.fromhex(p["sync_hex"])
    preamble = np.array([(2 if i % 2 else 1) for i in range(int(p["preamble_dibits"]))], dtype=np.uint8)
    sensor_id = int(p["sensor_id"]) & 0xFFFF
    sched, payload_rng = scene.rng("schedule"), scene.rng("payload")
    temp_dc = int(payload_rng.integers(100, 250))
    humidity = int(payload_rng.integers(30, 70))
    constant = bool(p["constant_payload"])

    crc_params: dict[str, Any] | None = None
    catalogue_name = None
    width = int(p["check_width"]) if kind == "crc" else 0
    if kind == "crc":
        crc_params = _crc_params(p, scene.rng("crc-params"))
        catalogue_name = fsk.crc_catalogue_name(**crc_params)
    hexw = width // 4

    k = 0
    while True:
        jitter = float(p["jitter_s"])
        t = float(p["first_burst_s"]) + k * float(p["period_s"])
        t += float(sched.uniform(-jitter, jitter)) if jitter > 0 else 0.0
        if constant:
            seq = 0
        else:
            temp_dc += int(payload_rng.integers(-3, 4))
            seq = k & 0xFF
        flags = 0b0001
        payload_int = (sensor_id << 32) | (seq << 24) | ((temp_dc & 0xFFF) << 12) \
            | ((humidity & 0xFF) << 4) | flags
        payload = payload_int.to_bytes(6, "big")
        check: dict[str, Any] | None = None
        if kind == "crc":
            assert crc_params is not None
            crc = fsk.crc_generic(payload, **crc_params)
            tail = crc.to_bytes(width // 8, "big")
            check = {
                "kind": "crc", "algorithm": catalogue_name or f"CRC-{width}/CUSTOM",
                "source": str(p["crc_source"]), "width": width,
                "poly": f"0x{crc_params['poly']:0{hexw}x}", "init": f"0x{crc_params['init']:0{hexw}x}",
                "refin": crc_params["refin"], "refout": crc_params["refout"],
                "xorout": f"0x{crc_params['xorout']:0{hexw}x}",
                "in_reveng_catalogue": catalogue_name is not None, "catalogue_name": catalogue_name,
                "covers": "payload bytes", "covered_bits": 48, "value": f"0x{crc:0{hexw}x}",
                "valid": True,
            }
            body = payload + tail
        elif kind == "bch":
            d = payload_int >> 6  # top 42 bits -> two 21-bit data fields
            words = [pocsag.bch_encode((d >> 21) & 0x1FFFFF), pocsag.bch_encode(d & 0x1FFFFF)]
            body = b"".join(w.to_bytes(4, "big") for w in words)
            check = {"kind": "bch", "code": "BCH(31,21)+even-parity", "poly": f"0x{pocsag.BCH_POLY:x}",
                     "codewords_hex": [f"{w:08x}" for w in words], "covered_bits": 42,
                     "corrects_bits": 2, "valid": True}
        else:
            body = payload
        dibits = np.concatenate([preamble, tk.bytes_to_dibits(sync), tk.bytes_to_dibits(body)])
        iq = tk.c4fm(dibits, fs, rate, phase0=float(sched.uniform(0, 2 * math.pi)))
        start = max(0, int(round(t * fs)))
        if start + len(iq) > scene.n_samples:
            break
        tt = scene.time(start, len(iq))
        scene.add_samples(start, amp * iq * np.exp(2j * math.pi * off * tt))
        truth = scene.emission_truth(
            cap, off, bw, power, kind="c4fm-burst", modulation="c4fm", levels=4,
            symbol_rate_bd=rate, burst_index=k,
            nominal_center_hz=cap.center_hz + float(p["channel_offset_hz"]),
            cfo_hz=float(p["cfo_hz"]), constant_payload=constant,
            frame={"n_dibits": int(len(dibits)), "preamble_dibits": int(len(preamble)),
                   "sync_hex": sync.hex(), "payload_hex": payload.hex(), "body_hex": body.hex(),
                   "dibit_map": {f"{d:02b}": v for d, v in tk.C4FM_DEVIATIONS_HZ.items()}},
            payload_fields={"sensor_id": sensor_id, "seq": seq, "temperature_dC": temp_dc,
                            "humidity_pct": humidity, "flags": flags},
            check=check,
            identity={"type": "sensor_id", "value": f"{sensor_id:04x}"},
        )
        f = cap.center_hz + off
        scene.annotate(start, len(iq), f - bw / 2, f + bw / 2, "c4fm-burst", truth)
        k += 1
    scene.scenario_truth["emitter"] = {
        "identity": {"type": "sensor_id", "value": f"{sensor_id:04x}"},
        "expected_known_status": "unknown", "modulation": "c4fm", "rf_center_hz": cap.center_hz + off,
        "bandwidth_hz": bw, "symbol_rate_bd": rate, "n_bursts": k, "constant_payload": constant,
        "check_kind": kind, "crc_source": str(p["crc_source"]) if kind == "crc" else None,
        "crc": None if crc_params is None else {
            "width": width, "poly": f"0x{crc_params['poly']:0{hexw}x}",
            "init": f"0x{crc_params['init']:0{hexw}x}", "refin": crc_params["refin"],
            "refout": crc_params["refout"], "xorout": f"0x{crc_params['xorout']:0{hexw}x}",
            "in_reveng_catalogue": catalogue_name is not None, "catalogue_name": catalogue_name},
    }
    return [scene], {}
