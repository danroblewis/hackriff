"""``ais_vessels`` (SIGNAL-015, T-963): a synthetic AIS scene on the two fixed international
marine VHF channels — AIS1/Ch87B 161.975 MHz and AIS2/Ch88B 162.025 MHz, both inside one capture
centred between them — carrying several Class A position-report bursts (GMSK 9600 Bd, hidden
truth: MMSI, message type, navigation fields) built by :mod:`hkpy.synth.ais`. The scene exists so
`py/fixtures/ais_ref.py` (an independent reference decoder) has something validated to decode
before the next live-air explorer window, per the field-found gap in
``~/.hackriff-ops/explorer/journal-20260925.md`` (section 'AIS').
"""

from __future__ import annotations

import math
from typing import Any

import numpy as np

from hkpy.synth import ais, fsk
from hkpy.synth.scenarios import DEFAULT_START_UTC, utc_plus
from hkpy.synth.scene import Scene, complex_noise, db, undb

#: AIS1 (Ch87B) and AIS2 (Ch88B), Hz.
CHANNEL_HZ = (161_975_000.0, 162_025_000.0)

AIS_DEFAULTS: dict[str, Any] = {
    "sample_rate": 240_000.0,
    "center_hz": 162_000_000.0,  # midway between AIS1 and AIS2: both land at +-25 kHz
    "channel_offsets_hz": [-25_000.0, 25_000.0],
    "snr_db": 22.0,
    "noise_dbfs": -40.0,
    "start_s": 0.05,
    "margin_s": 0.05,
    "calibration_k_db": -70.0,
    "start_utc": DEFAULT_START_UTC,
    # Per-vessel truth: which channel (index into channel_offsets_hz), MMSI, message type
    # (1/2/3: Class A position report), key-up offset after start_s, then the navigation fields.
    "channels": [0, 1, 0],
    "mmsis": [366123456, 367654321, 366987000],
    "msg_types": [1, 1, 3],
    "start_offsets_s": [0.0, 0.03, 0.07],
    "sog_kt": [12.3, 0.0, 5.5],
    "lon_deg": [-122.4194, -122.4210, -122.4180],
    "lat_deg": [37.7749, 37.7780, 37.7720],
    "cog_deg": [45.6, 0.0, 271.0],
    "heading_deg": [50, 511, 270],
    "nav_status": [0, 5, 0],
}


def ais_vessels(ctx: Any) -> tuple[list[Scene], dict[str, Any]]:
    p = ctx.params
    fs = float(p["sample_rate"])
    offsets = [float(v) for v in p["channel_offsets_hz"]]
    channels = [int(v) for v in p["channels"]]
    mmsis = [int(v) for v in p["mmsis"]]
    msg_types = [int(v) for v in p["msg_types"]]
    start_offsets = [float(v) for v in p["start_offsets_s"]]
    sog = [float(v) for v in p["sog_kt"]]
    lon = [float(v) for v in p["lon_deg"]]
    lat = [float(v) for v in p["lat_deg"]]
    cog = [float(v) for v in p["cog_deg"]]
    heading = [int(v) for v in p["heading_deg"]]
    nav_status = [int(v) for v in p["nav_status"]]
    n_vessels = len(mmsis)
    lens = {len(channels), len(msg_types), len(start_offsets), len(sog), len(lon), len(lat),
            len(cog), len(heading), len(nav_status)}
    if lens != {n_vessels}:
        raise ValueError("channels/mmsis/msg_types/.../nav_status must have the same length")
    if any(c not in (0, 1) for c in channels):
        raise ValueError("channels must index channel_offsets_hz (0 or 1)")

    payloads = [
        ais.build_position_report(
            mmsi=mmsis[i], msg_type=msg_types[i], nav_status=nav_status[i], sog_kt=sog[i],
            lon_deg=lon[i], lat_deg=lat[i], cog_deg=cog[i], heading_deg=heading[i],
        )
        for i in range(n_vessels)
    ]
    dur_s = ais.burst_duration_s(ais.POSITION_REPORT_BITS)
    starts_s = [float(p["start_s"]) + start_offsets[i] for i in range(n_vessels)]
    total_s = max(starts_s[i] + dur_s for i in range(n_vessels)) + float(p["margin_s"])
    n = int(round(total_s * fs))

    scene = ctx.scene(
        "ais_vessels", fs, n,
        "hkpy.synth ais_vessels: GMSK 9600 Bd AIS Class A position reports on the two fixed "
        "marine channels (AIS1 161.975 MHz, AIS2 162.025 MHz)",
    )
    cap = scene.add_capture(0, n, float(p["center_hz"]), utc_plus(p["start_utc"], 0.0),
                            calibration_k_db=float(p["calibration_k_db"]))
    cap.floor_dbfs_per_hz = float(p["noise_dbfs"]) - db(fs)
    scene.add_samples(0, complex_noise(scene.rng("noise"), n, float(p["noise_dbfs"])))
    scene.add_floor(0, n, cap.floor_dbfs_per_hz)

    bw = 4 * (9600.0 / 4.0) + 9600.0  # Carson's rule: 2*(dev + Rs/2), dev = Rs/4
    power = float(p["snr_db"]) + cap.floor_dbfs_per_hz + db(bw)
    amp = math.sqrt(undb(power))
    t_all = scene.time(0, n)

    truth_vessels = []
    for i in range(n_vessels):
        off = offsets[channels[i]]
        if abs(off) + bw / 2 > fs / 2:
            raise ValueError(f"vessel {i} at {off:+.0f} Hz does not fit inside the sample rate")
        phase0 = float(scene.rng("phase", i).uniform(0, 2 * math.pi))
        iq = ais.burst_iq(payloads[i], fs, phase0=phase0)
        s0 = int(round(starts_s[i] * fs))
        if s0 + len(iq) > n:
            raise ValueError(f"vessel {i} burst does not fit in the recording")
        tt = t_all[s0 : s0 + len(iq)]
        scene.add_samples(s0, amp * iq * np.exp(2j * math.pi * off * tt))

        f_center = cap.center_hz + off
        truth = scene.emission_truth(
            cap, off, bw, power,
            kind="ais-position-report", modulation="gmsk", levels=2,
            symbol_rate_bd=9600.0, deviation_hz=2400.0, mod_index=0.5, bt=0.4,
            line_coding="nrzi (0 = transition)", flag_hex="0x7e",
            fcs=fsk_ibm_sdlc_spec(),
            channel_index=channels[i], channel_hz=CHANNEL_HZ[channels[i]],
            mmsi=mmsis[i], msg_type=msg_types[i], repeat_indicator=0,
            nav_status=nav_status[i], sog_kt=sog[i], position_accuracy=0,
            longitude_deg=lon[i], latitude_deg=lat[i], cog_deg=cog[i], heading_deg=heading[i],
            payload_bits=int(ais.POSITION_REPORT_BITS),
            identity={"type": "mmsi", "value": str(mmsis[i])},
        )
        scene.annotate(s0, len(iq), f_center - bw / 2, f_center + bw / 2, "ais-position-report", truth)
        truth_vessels.append({
            "vessel": i, "channel": channels[i], "channel_hz": CHANNEL_HZ[channels[i]],
            "mmsi": mmsis[i], "msg_type": msg_types[i],
        })
    scene.scenario_truth["vessels"] = truth_vessels
    scene.scenario_truth["note"] = ("AIS1/AIS2 Class A position reports; py/fixtures/ais_ref.py "
                                    "recovers each vessel's MMSI and message type blind")
    return [scene], {}


def fsk_ibm_sdlc_spec() -> dict[str, Any]:
    """CRC-16/IBM-SDLC a.k.a. CRC-16/X-25 in the RevEng model, for truth (mirrors
    `hkpy.synth.fsk.CRC_CATALOGUE`)."""
    width, poly, init, refin, refout, xorout = fsk.CRC_CATALOGUE["CRC-16/IBM-SDLC"]
    return {
        "algorithm": "CRC-16/IBM-SDLC (CRC-16/X-25)",
        "width": width, "poly": f"0x{poly:04x}", "init": f"0x{init:04x}",
        "refin": refin, "refout": refout, "xorout": f"0x{xorout:04x}",
    }
