"""Scenario builders. Each takes a :class:`Ctx` and returns ``(scenes, extra_files)``.

Impairments (``hkpy.synth.impairments``) are applied afterwards by :func:`hkpy.synth.generate`.
"""

from __future__ import annotations

import datetime as _dt
import math
from dataclasses import dataclass
from typing import Any

import numpy as np
from scipy import signal

from hkpy.synth import acars, adsb, fsk, pocsag, rds
from hkpy.synth.scene import (
    Scene,
    complex_noise,
    db,
    format_utc,
    parse_utc,
    undb,
)

DEFAULT_START_UTC = "2026-09-13T12:00:00Z"


@dataclass
class Ctx:
    scenario: str
    seed: int
    params: dict[str, Any]
    datatype: str
    use_cases: list[str]

    def scene(self, name: str, sample_rate: float, n_samples: int, description: str) -> Scene:
        return Scene(name=name, scenario=self.scenario, seed=self.seed, params=self.params,
                     sample_rate=float(sample_rate), n_samples=int(n_samples),
                     datatype=self.datatype, description=description, use_cases=self.use_cases)


def utc_plus(start_utc: str, seconds: float) -> str:
    return format_utc(parse_utc(start_utc) + _dt.timedelta(seconds=seconds))


def _n(p: dict[str, Any], key: str = "duration_s") -> int:
    return int(round(float(p[key]) * float(p["sample_rate"])))


def _noise_capture(scene: Scene, p: dict[str, Any], center_hz: float, k_db: float) -> None:
    """One capture covering the whole scene with white noise at ``noise_dbfs`` and a floor annotation."""
    fs = scene.sample_rate
    cap = scene.add_capture(0, scene.n_samples, center_hz, utc_plus(p["start_utc"], 0.0),
                            calibration_k_db=k_db)
    cap.floor_dbfs_per_hz = float(p["noise_dbfs"]) - db(fs)
    scene.add_samples(0, complex_noise(scene.rng("noise"), scene.n_samples, float(p["noise_dbfs"])))
    scene.add_floor(0, scene.n_samples, cap.floor_dbfs_per_hz)


# ---------------------------------------------------------------------------------------------
# tone
# ---------------------------------------------------------------------------------------------

TONE_DEFAULTS: dict[str, Any] = {
    "sample_rate": 1e6,
    "center_hz": 100e6,
    "offset_hz": 100e3,
    "power_dbfs": -20.0,
    "noise_dbfs": -40.0,
    "duration_s": 0.05,
    "calibration_k_db": -70.0,
    "start_utc": DEFAULT_START_UTC,
}


def tone(ctx: Ctx) -> tuple[list[Scene], dict[str, Any]]:
    p = ctx.params
    scene = ctx.scene("tone", p["sample_rate"], _n(p), "hkpy.synth tone: CW in white noise")
    _noise_capture(scene, p, p["center_hz"], p["calibration_k_db"])
    cap = scene.captures[0]
    off, power = float(p["offset_hz"]), float(p["power_dbfs"])
    amp = math.sqrt(undb(power))
    phase = float(scene.rng("tone").uniform(0, 2 * math.pi))
    t = scene.time(0, scene.n_samples)
    scene.add_samples(0, amp * np.exp(1j * (2 * math.pi * off * t + phase)))
    f = cap.center_hz + off
    scene.annotate(0, scene.n_samples, f, f, "tone",
                   scene.emission_truth(cap, off, 0.0, power, kind="cw", modulation="cw",
                                        amplitude=amp, phase_rad=phase,
                                        snr_db_per_hz=power - cap.floor_dbfs_per_hz))
    return [scene], {}


# ---------------------------------------------------------------------------------------------
# fsk_burst_train (AWARE-036)
# ---------------------------------------------------------------------------------------------

FSK_DEFAULTS: dict[str, Any] = {
    "sample_rate": 500e3,
    "center_hz": 433.92e6,
    "channel_offset_hz": 50e3,
    "cfo_hz": 3000.0,
    "symbol_rate_bd": 4800.0,
    "deviation_hz": 9600.0,
    "bt": 0.0,
    "snr_db": 20.0,
    "noise_dbfs": -40.0,
    "duration_s": 0.6,
    "first_burst_s": 0.03,
    "period_s": 0.12,
    "jitter_s": 0.01,
    "preamble_bits": 32,
    "sync_hex": "2dd4",
    "sensor_id": 0x5A3C,
    "calibration_k_db": -70.0,
    "start_utc": DEFAULT_START_UTC,
}

FSK_LAYOUT = [
    {"field": "preamble", "bits": "preamble_bits", "value": "1010..."},
    {"field": "sync", "bits": "16 (sync_hex)"},
    {"field": "sensor_id", "bits": 16},
    {"field": "seq", "bits": 8},
    {"field": "temperature_dC", "bits": 12, "signed": True},
    {"field": "humidity_pct", "bits": 8},
    {"field": "flags", "bits": 4},
    {"field": "crc16", "bits": 16, "covers": "sensor_id..flags (6 bytes)"},
]


def fsk_burst_train(ctx: Ctx) -> tuple[list[Scene], dict[str, Any]]:
    p = ctx.params
    fs = float(p["sample_rate"])
    scene = ctx.scene("fsk_burst_train", fs, _n(p),
                      "hkpy.synth fsk_burst_train: periodic 2-FSK sensor bursts with CRC-16")
    _noise_capture(scene, p, p["center_hz"], p["calibration_k_db"])
    cap = scene.captures[0]
    rate, dev, bt = float(p["symbol_rate_bd"]), float(p["deviation_hz"]), float(p["bt"])
    bw = 2 * dev + rate  # Carson's rule with f_m = Rs/2
    power = float(p["snr_db"]) + cap.floor_dbfs_per_hz + db(bw)
    amp = math.sqrt(undb(power))
    off = float(p["channel_offset_hz"]) + float(p["cfo_hz"])
    if abs(off) + bw / 2 > fs / 2:
        raise ValueError("fsk channel does not fit inside the sample rate")
    sync = bytes.fromhex(p["sync_hex"])
    preamble = np.array([(i + 1) % 2 for i in range(int(p["preamble_bits"]))], dtype=np.uint8)
    sensor_id = int(p["sensor_id"]) & 0xFFFF
    sched = scene.rng("schedule")
    payload_rng = scene.rng("payload")
    temp_dc = int(payload_rng.integers(100, 250))
    humidity = int(payload_rng.integers(30, 70))
    k = 0
    while True:
        jitter = float(p["jitter_s"])
        t = float(p["first_burst_s"]) + k * float(p["period_s"])
        t += float(sched.uniform(-jitter, jitter)) if jitter > 0 else 0.0
        temp_dc += int(payload_rng.integers(-3, 4))
        flags = 0b0001
        payload_int = (sensor_id << 32) | ((k & 0xFF) << 24) | ((temp_dc & 0xFFF) << 12) \
            | ((humidity & 0xFF) << 4) | flags
        payload = payload_int.to_bytes(6, "big")
        crc = fsk.crc16_ccitt_false(payload)
        bits = np.concatenate([preamble, fsk.bytes_to_bits(sync), fsk.bytes_to_bits(payload),
                               fsk.bytes_to_bits(crc.to_bytes(2, "big"))])
        iq = fsk.cpfsk(bits, fs, rate, dev, bt=bt, phase0=float(sched.uniform(0, 2 * math.pi)))
        start = max(0, int(round(t * fs)))
        if start + len(iq) > scene.n_samples:
            break
        tt = scene.time(start, len(iq))
        scene.add_samples(start, amp * iq * np.exp(2j * math.pi * off * tt))
        truth = scene.emission_truth(
            cap, off, bw, power, kind="fsk-burst", modulation="2fsk", levels=2,
            symbol_rate_bd=rate, deviation_hz=dev, mod_index=2 * dev / rate, bt=bt,
            nominal_center_hz=cap.center_hz + float(p["channel_offset_hz"]), cfo_hz=float(p["cfo_hz"]),
            burst_index=k, bit_order="msb-first", mapping="bit 1 = +deviation_hz",
            frame={
                "n_bits": int(len(bits)),
                "bits_hex": fsk.bits_to_hex(bits),
                "preamble_bits": int(len(preamble)),
                "preamble_hex": fsk.bits_to_hex(preamble),
                "sync_hex": sync.hex(),
                "payload_hex": payload.hex(),
                "crc_hex": f"{crc:04x}",
                "layout": FSK_LAYOUT,
            },
            payload_fields={"sensor_id": sensor_id, "seq": k & 0xFF, "temperature_dC": temp_dc,
                            "humidity_pct": humidity, "flags": flags},
            crc={**fsk.CRC16_SPEC, "value": f"0x{crc:04X}", "valid": True},
            identity={"type": "sensor_id", "value": f"{sensor_id:04x}"},
        )
        f = cap.center_hz + off
        scene.annotate(start, len(iq), f - bw / 2, f + bw / 2, "fsk-burst", truth)
        k += 1
    scene.scenario_truth["emitter"] = {
        "identity": {"type": "sensor_id", "value": f"{sensor_id:04x}"},
        "expected_known_status": "unknown",
        "modulation": "2fsk",
        "rf_center_hz": cap.center_hz + off,
        "bandwidth_hz": bw,
        "symbol_rate_bd": rate,
        "deviation_hz": dev,
        "period_s": float(p["period_s"]),
        "jitter_s": float(p["jitter_s"]),
        "n_bursts": k,
        "preamble_bits": int(len(preamble)),
        "sync_hex": sync.hex(),
        "crc": fsk.CRC16_SPEC,
    }
    return [scene], {}


# ---------------------------------------------------------------------------------------------
# noise_floor_rise (AWARE-006)
# ---------------------------------------------------------------------------------------------

FLOOR_RISE_DEFAULTS: dict[str, Any] = {
    "sample_rate": 2e6,
    "center_hz": 1575.42e6,
    "duration_s": 0.2,
    "noise_dbfs": -40.0,
    "step_db": 10.0,
    "t0_s": 0.1,
    "rise_bandwidth_hz": 0.0,
    "rise_offset_hz": 0.0,
    "n_weak_signals": 2,
    "weak_power_dbfs": -50.0,
    "calibration_k_db": -70.0,
    "start_utc": DEFAULT_START_UTC,
    "lat": 52.2,
    "lon": 0.12,
}


def noise_floor_rise(ctx: Ctx) -> tuple[list[Scene], dict[str, Any]]:
    p = ctx.params
    fs = float(p["sample_rate"])
    scene = ctx.scene("noise_floor_rise", fs, _n(p),
                      "hkpy.synth noise_floor_rise: GNSS L1 band with a broadband floor step")
    n = scene.n_samples
    fc = float(p["center_hz"])
    cap = scene.add_capture(0, n, fc, utc_plus(p["start_utc"], 0.0),
                            calibration_k_db=float(p["calibration_k_db"]))
    floor = float(p["noise_dbfs"]) - db(fs)
    cap.floor_dbfs_per_hz = floor
    scene.add_samples(0, complex_noise(scene.rng("noise"), n, float(p["noise_dbfs"])))
    s0 = int(round(float(p["t0_s"]) * fs))
    if not 0 < s0 < n:
        raise ValueError("t0_s must fall inside the recording")
    step = float(p["step_db"])
    extra_per_hz = floor + db(undb(step) - 1) if step > 0 else None
    bw = float(p["rise_bandwidth_hz"])
    full = bw <= 0 or bw >= fs
    rise_off = 0.0 if full else float(p["rise_offset_hz"])
    if full:
        bw = fs
        if extra_per_hz is not None:
            scene.add_samples(s0, complex_noise(scene.rng("rise"), n - s0, extra_per_hz + db(fs)))
    elif extra_per_hz is not None:
        taps = signal.firwin(1023, bw / 2, fs=fs)
        white = complex_noise(scene.rng("rise"), n - s0 + len(taps) - 1, 0.0)
        shaped = np.convolve(white, taps, mode="valid")
        target = undb(extra_per_hz) * bw
        shaped *= math.sqrt(target / float(np.sum(taps**2)))
        scene.add_samples(s0, shaped * np.exp(2j * math.pi * rise_off * scene.time(s0, n - s0)))
    lo, hi = fc + rise_off - bw / 2, fc + rise_off + bw / 2
    scene.add_floor(0, s0, floor, label="noise-floor-before")
    scene.add_floor(s0, n - s0, floor + max(step, 0.0), lo, hi, label="noise-floor-after")
    if not full:
        if lo > fc - fs / 2:
            scene.add_floor(s0, n - s0, floor, fc - fs / 2, lo, label="noise-floor-after-outside")
        if hi < fc + fs / 2:
            scene.add_floor(s0, n - s0, floor, hi, fc + fs / 2, label="noise-floor-after-outside")
    t0_utc = utc_plus(p["start_utc"], s0 / fs)
    k = cap.calibration_k_db
    location = {"lat": float(p["lat"]), "lon": float(p["lon"])}
    scene.annotate(s0, n - s0, lo, hi, "noise-floor-rise", {
        "role": "event", "kind": "noise-floor-rise", "expected_anomaly_kind": "noise-floor-rise",
        "band": "GNSS L1", "center_hz": fc + rise_off, "bandwidth_hz": bw, "step_db": step,
        "t0_s": s0 / fs, "t0_utc": t0_utc,
        "floor_before_dbfs_per_hz": floor, "floor_after_dbfs_per_hz": floor + step,
        "floor_before_dbm_per_hz": floor + k, "floor_after_dbm_per_hz": floor + step + k,
        "location": location, "cause_hint": "synthetic broadband jammer (band-limited Gaussian noise)",
    })
    rng = scene.rng("weak")
    t = scene.time(0, n)
    for i in range(int(p["n_weak_signals"])):
        off = float(rng.uniform(fs / 50, 0.4 * fs)) * (1 if rng.uniform() < 0.5 else -1)
        power = float(p["weak_power_dbfs"])
        scene.add_samples(0, math.sqrt(undb(power)) * np.exp(1j * (2 * math.pi * off * t + rng.uniform(0, 6.28))))
        scene.annotate(0, n, fc + off, fc + off, "weak-cw",
                       scene.emission_truth(cap, off, 0.0, power, kind="cw", modulation="cw", index=i))
    scene.scenario_truth.update({"location": location, "t0_utc": t0_utc, "step_db": step})
    return [scene], {}


# ---------------------------------------------------------------------------------------------
# injected_floor (SPACE-050)
# ---------------------------------------------------------------------------------------------

INJECTED_FLOOR_DEFAULTS: dict[str, Any] = {
    "sample_rate": 1e6,
    "segment_duration_s": 0.05,
    "segment_centers_hz": [10e6, 144e6, 433.92e6, 915e6, 2450e6, 5800e6],
    "floors_dbfs": [],
    "calibration_k_db": [],
    "n_cw_per_segment": 1,
    "cw_rel_db": -10.0,
    "lna_db": 24.0,
    "vga_db": 20.0,
    "start_utc": DEFAULT_START_UTC,
}


def _per_segment(values: list[float], n: int, rng: np.random.Generator, lo: float, hi: float,
                 name: str) -> list[float]:
    if not values:
        return [round(float(v), 1) for v in rng.uniform(lo, hi, n)]
    if len(values) == 1:
        return [float(values[0])] * n
    if len(values) != n:
        raise ValueError(f"{name} needs 0, 1 or {n} values")
    return [float(v) for v in values]


def injected_floor(ctx: Ctx) -> tuple[list[Scene], dict[str, Any]]:
    p = ctx.params
    fs = float(p["sample_rate"])
    centers = [float(c) for c in p["segment_centers_hz"]]
    m = _n(p, "segment_duration_s")
    scene = ctx.scene("injected_floor", fs, m * len(centers),
                      "hkpy.synth injected_floor: known calibrated floors in several band segments")
    floors = _per_segment(p["floors_dbfs"], len(centers), scene.rng("floors"), -42, -28, "floors_dbfs")
    ks = _per_segment(p["calibration_k_db"], len(centers), scene.rng("cal"), -80, -60, "calibration_k_db")
    segments = []
    for i, (fc, floor_dbfs, k) in enumerate(zip(centers, floors, ks, strict=True)):
        start = i * m
        cap = scene.add_capture(start, m, fc, utc_plus(p["start_utc"], start / fs), calibration_k_db=k,
                                lna_db=float(p["lna_db"]), vga_db=float(p["vga_db"]))
        cap.floor_dbfs_per_hz = floor_dbfs - db(fs)
        scene.add_samples(start, complex_noise(scene.rng("noise", i), m, floor_dbfs))
        ann = scene.add_floor(start, m, cap.floor_dbfs_per_hz, segment=i)
        rng = scene.rng("cw", i)
        t = scene.time(start, m)
        for j in range(int(p["n_cw_per_segment"])):
            off = float(rng.uniform(fs / 50, 0.35 * fs)) * (1 if rng.uniform() < 0.5 else -1)
            power = floor_dbfs + float(p["cw_rel_db"])
            scene.add_samples(start, math.sqrt(undb(power)) * np.exp(1j * (2 * math.pi * off * t + rng.uniform(0, 6.28))))
            scene.annotate(start, m, fc + off, fc + off, "cw",
                           scene.emission_truth(cap, off, 0.0, power, kind="cw", modulation="cw",
                                                segment=i, index=j))
        segments.append({"segment": i, "center_hz": fc, "sample_start": start, "sample_count": m,
                         "floor_dbfs": floor_dbfs, "floor_dbfs_per_hz": cap.floor_dbfs_per_hz,
                         "calibration_k_db": k, "floor_dbm": floor_dbfs + k,
                         "floor_dbm_per_hz": cap.floor_dbfs_per_hz + k,
                         "expected_floor_dbfs": ann["truth"]["expected_floor_dbfs"]})
    scene.scenario_truth["segments"] = segments
    return [scene], {}


# ---------------------------------------------------------------------------------------------
# fm_broadcast_rds (SIGNAL-062)
# ---------------------------------------------------------------------------------------------

FM_DEFAULTS: dict[str, Any] = {
    "sample_rate": 456e3,
    "center_hz": 99.5e6,
    "offset_hz": 0.0,
    "duration_s": 0.6,
    "power_dbfs": -20.0,
    "noise_dbfs": -45.0,
    "pi_hex": "C0DE",
    "ps": "HACKRIFF",
    "pty": 10,
    "tp": True,
    "ta": False,
    "music": True,
    "di": 1,
    "stereo": True,
    "left_tone_hz": 1000.0,
    "right_tone_hz": 400.0,
    "mono_deviation_hz": 30000.0,
    "stereo_deviation_hz": 30000.0,
    "pilot_deviation_hz": 6750.0,
    "rds_deviation_hz": 2000.0,
    # RadioText (group 2A, up to 64 characters; T-094). Empty: PS groups only.
    "radiotext": "",
    "calibration_k_db": -70.0,
    "start_utc": DEFAULT_START_UTC,
}


def fm_broadcast_rds(ctx: Ctx) -> tuple[list[Scene], dict[str, Any]]:
    p = ctx.params
    fs = float(p["sample_rate"])
    scene = ctx.scene("fm_broadcast_rds", fs, _n(p),
                      "hkpy.synth fm_broadcast_rds: stereo WFM with 19 kHz pilot and RDS group 0A")
    _noise_capture(scene, p, p["center_hz"], p["calibration_k_db"])
    cap = scene.captures[0]
    n = scene.n_samples
    t = scene.time(0, n)
    left = np.sin(2 * math.pi * float(p["left_tone_hz"]) * t)
    right = np.sin(2 * math.pi * float(p["right_tone_hz"]) * t)
    w_pilot = 2 * math.pi * rds.PILOT_HZ * t
    mpx = float(p["mono_deviation_hz"]) * (left + right) / 2
    if p["stereo"]:
        mpx += float(p["pilot_deviation_hz"]) * np.sin(w_pilot)
        mpx += float(p["stereo_deviation_hz"]) * (left - right) / 2 * np.sin(2 * w_pilot)
    pi = int(p["pi_hex"], 16)
    n_groups = int(math.ceil(n / fs * rds.BITRATE_BD / 104)) + 1
    # One cycle: PS segments 0-3 (0A), then every RadioText segment (2A) when there is text.
    rt = str(p["radiotext"] or "")
    cycle = [("0A", s) for s in range(4)] + [("2A", s) for s in range(len(rds.radiotext_codes(rt)) // 4 if rt else 0)]
    blocks: list[int] = []
    for g in range(n_groups):
        kind, seg = cycle[g % len(cycle)]
        if kind == "0A":
            blocks += rds.group_0a(pi, p["ps"], seg, pty=int(p["pty"]), tp=bool(p["tp"]),
                                   ta=bool(p["ta"]), music=bool(p["music"]), di=int(p["di"]))
        else:
            blocks += rds.group_2a(pi, rt, seg, pty=int(p["pty"]), tp=bool(p["tp"]))
    dbits = rds.differential(rds.blocks_to_bits(blocks))
    bb = rds.biphase_baseband(dbits, fs)[:n]
    mpx += float(p["rds_deviation_hz"]) * bb * np.sin(3 * w_pilot)
    off, power = float(p["offset_hz"]), float(p["power_dbfs"])
    phase0 = float(scene.rng("carrier").uniform(0, 2 * math.pi))
    phase = phase0 + 2 * math.pi * (np.cumsum(mpx) / fs + off * t)
    scene.add_samples(0, math.sqrt(undb(power)) * np.exp(1j * phase))
    peak_dev = float(np.max(np.abs(mpx)))
    bw = min(2 * (peak_dev + rds.SUBCARRIER_HZ + rds.BASEBAND_CUTOFF_HZ), fs)
    ps = p["ps"].ljust(8)[:8]
    rds_truth = {
        "pi_hex": f"{pi:04X}", "pi": pi, "ps": ps, "pty": int(p["pty"]), "tp": bool(p["tp"]),
        "ta": bool(p["ta"]), "music": bool(p["music"]), "di": int(p["di"]),
        "group_types": ["0A", "2A"] if rt else ["0A"],
        **({"radiotext": rt[:64]} if rt else {}),
        "n_groups": n_groups, "bitrate_bd": rds.BITRATE_BD, "subcarrier_hz": rds.SUBCARRIER_HZ,
        "deviation_hz": float(p["rds_deviation_hz"]), "first_bit_s": 0.0,
        "encoding": rds.ENCODING_NOTE, "check_poly": f"0x{rds.CHECK_POLY:03X}",
        "offset_words": {k: f"0x{v:03X}" for k, v in rds.OFFSET_WORDS.items()},
        "blocks_hex": [f"{b:07x}" for b in blocks],
    }
    truth = scene.emission_truth(
        cap, off, bw, power, kind="wfm-broadcast", modulation="wfm", peak_deviation_hz=peak_dev,
        bandwidth_rule="Carson: 2*(peak_deviation + 59.4 kHz)", preemphasis="none",
        stereo=bool(p["stereo"]),
        pilot={"present": bool(p["stereo"]), "frequency_hz": rds.PILOT_HZ,
               "deviation_hz": float(p["pilot_deviation_hz"]) if p["stereo"] else 0.0},
        audio={"left_tone_hz": float(p["left_tone_hz"]), "right_tone_hz": float(p["right_tone_hz"]),
               "mono_deviation_hz": float(p["mono_deviation_hz"]),
               "stereo_deviation_hz": float(p["stereo_deviation_hz"]) if p["stereo"] else 0.0},
        rds=rds_truth, identity={"type": "rds_pi", "value": f"{pi:04X}"}, label_expected=ps.strip(),
    )
    f = cap.center_hz + off
    scene.annotate(0, n, f - bw / 2, f + bw / 2, "wfm-rds", truth)
    return [scene], {}


# ---------------------------------------------------------------------------------------------
# pocsag_pagers (SIGNAL-062 M1 tutorial fixture, T-098): multi-channel, multi-baud POCSAG
# ---------------------------------------------------------------------------------------------

POCSAG_DEFAULTS: dict[str, Any] = {
    "sample_rate": 132300.0,  # 6 * 22050, so decimating to multimon-ng's rate is an integer /6
    "center_hz": 152.360e6,
    "deviation_hz": 4500.0,
    "snr_db": 22.0,
    "noise_dbfs": -40.0,
    "start_s": 0.05,
    "margin_s": 0.05,
    "channel_offsets_hz": [-40e3, 0.0, 40e3],
    "bauds_bd": [512.0, 1200.0, 2400.0],
    "rics": [1234567, 1876543, 654321],  # POCSAG address is 21 bits: max 2097151
    "functions": [0, 3, 3],
    "messages": ["911234", "STANDBY AT GATE 12", "HACKRIFF PAGE TEST"],
    "calibration_k_db": -70.0,
    "start_utc": DEFAULT_START_UTC,
}


def pocsag_pagers(ctx: Ctx) -> tuple[list[Scene], dict[str, Any]]:
    p = ctx.params
    fs = float(p["sample_rate"])
    offsets = [float(v) for v in p["channel_offsets_hz"]]
    bauds = [float(v) for v in p["bauds_bd"]]
    rics = [int(v) for v in p["rics"]]
    functions = [int(v) for v in p["functions"]]
    messages = [str(v) for v in p["messages"]]
    n_ch = len(offsets)
    if not (len(bauds) == len(rics) == len(functions) == len(messages) == n_ch):
        raise ValueError("channel_offsets_hz, bauds_bd, rics, functions and messages must have the same length")
    dev = float(p["deviation_hz"])
    start = float(p["start_s"])

    channels = []
    for i in range(n_ch):
        codewords = (pocsag.encode_numeric(messages[i]) if functions[i] == 0
                    else pocsag.encode_alpha(messages[i]))
        bits = pocsag.build_bits(rics[i], functions[i], codewords)
        channels.append({"bits": bits, "codewords": codewords, "duration_s": len(bits) / bauds[i]})
    total_s = start + max(c["duration_s"] for c in channels) + float(p["margin_s"])
    scene = ctx.scene("pocsag_pagers", fs, int(round(total_s * fs)),
                      "hkpy.synth pocsag_pagers: multi-channel 2-FSK POCSAG (512/1200/2400 Bd), "
                      "BCH(31,21)+parity, numeric and alphanumeric messages")
    _noise_capture(scene, p, p["center_hz"], p["calibration_k_db"])
    cap = scene.captures[0]

    truth_channels = []
    for i in range(n_ch):
        off, baud = offsets[i], bauds[i]
        bits, codewords = channels[i]["bits"], channels[i]["codewords"]
        bw = 2 * dev + baud
        power = float(p["snr_db"]) + cap.floor_dbfs_per_hz + db(bw)
        if abs(off) + bw / 2 > fs / 2:
            raise ValueError(f"pocsag channel {i} does not fit inside the sample rate")
        amp = math.sqrt(undb(power))
        phase0 = float(scene.rng("phase", i).uniform(0, 2 * math.pi))
        iq = fsk.cpfsk(1 - bits, fs, baud, dev, bt=0.0, phase0=phase0)  # invert: POCSAG bit1 -> -dev
        s0 = int(round(start * fs))
        tt = scene.time(s0, len(iq))
        scene.add_samples(s0, amp * iq * np.exp(2j * math.pi * off * tt))
        f = cap.center_hz + off
        text = messages[i]
        truth = scene.emission_truth(
            cap, off, bw, power, kind="pocsag-page", modulation="2fsk", levels=2,
            symbol_rate_bd=baud, deviation_hz=dev, mod_index=2 * dev / baud,
            preamble_bits=pocsag.PREAMBLE_BITS, sync_hex=f"{pocsag.SYNC_CODEWORD:08x}",
            idle_hex=f"{pocsag.IDLE_CODEWORD:08x}", bit_order="msb-first",
            mapping="bit 1 = -deviation_hz (multimon-ng gen_pocsag.c convention; polarity is "
                    "auto-detected on decode)",
            bch=pocsag.BCH_SPEC,
            frame={"n_bits": int(len(bits)), "n_codewords": len(codewords) + 2,
                  "n_batches": (len(bits) - pocsag.PREAMBLE_BITS) // (pocsag.CODEWORDS_PER_BATCH * 32 + 32)},
            ric=rics[i], address=rics[i], frame_position=rics[i] & 7, function=functions[i],
            message_kind="numeric" if functions[i] == 0 else "alphanumeric", message_text=text,
            message_codewords_hex=[f"{w:08x}" for w in codewords],
            identity={"type": "ric", "value": str(rics[i])},
        )
        scene.annotate(s0, len(iq), f - bw / 2, f + bw / 2, "pocsag-page", truth)
        truth_channels.append({"channel": i, "offset_hz": off, "rf_center_hz": f, "baud": baud,
                               "ric": rics[i], "function": functions[i], "message": text})
    scene.scenario_truth["channels"] = truth_channels
    scene.scenario_truth["note"] = ("multi-channel pager net for follow_hops: one RIC/message per "
                                    "channel offset, at a distinct baud each")
    return [scene], {}


# ---------------------------------------------------------------------------------------------
# acars_message (SIGNAL-062 M1 tutorial fixture, T-098): AM + MSK 2400 Bd VHF ACARS
# ---------------------------------------------------------------------------------------------

ACARS_DEFAULTS: dict[str, Any] = {
    "sample_rate": 96_000.0,
    "center_hz": 131.550e6,
    "am_depth": 0.7,
    "snr_db": 25.0,
    "noise_dbfs": -40.0,
    "prekey_s": 0.15,
    "start_s": 0.02,
    "margin_s": 0.02,
    "mode": "2",
    "reg": ".HKRF01",
    "label": "H1",
    "block_id": "1",
    "text": "HACKRIFF ACARS TUTORIAL FIXTURE TEST MESSAGE 001",
    "calibration_k_db": -70.0,
    "start_utc": DEFAULT_START_UTC,
}


def acars_message(ctx: Ctx) -> tuple[list[Scene], dict[str, Any]]:
    p = ctx.params
    fs = float(p["sample_rate"])
    frame = acars.build_frame(str(p["mode"]), str(p["reg"]), str(p["label"]), str(p["block_id"]),
                              str(p["text"]))
    mpx = acars.msk_baseband(frame.bits, fs, prekey_s=float(p["prekey_s"]))
    n = len(mpx)
    start = int(round(float(p["start_s"]) * fs))
    total = start + n + int(round(float(p["margin_s"]) * fs))
    scene = ctx.scene("acars_message", fs, total,
                      "hkpy.synth acars_message: AM-modulated VHF ACARS, MSK 2400 Bd, "
                      "SYN/SOH..ETX framing, CRC-16")
    _noise_capture(scene, p, p["center_hz"], p["calibration_k_db"])
    cap = scene.captures[0]
    bw = 2 * acars.MARK_HZ  # AM double-sideband around the MSK tone band
    power = float(p["snr_db"]) + cap.floor_dbfs_per_hz + db(bw)
    amp = math.sqrt(undb(power))
    depth = float(p["am_depth"])
    carrier = 1.0 + depth * mpx  # mpx in [-1,1] (0 during prekey) -> envelope in [1-depth,1+depth]
    phase0 = float(scene.rng("carrier").uniform(0, 2 * math.pi))
    scene.add_samples(start, amp * carrier * np.exp(1j * phase0))
    truth = scene.emission_truth(
        cap, 0.0, bw, power, kind="acars-message", modulation="am+msk", carrier_modulation="am",
        am_depth=depth, subcarrier_modulation="msk", symbol_rate_bd=acars.BAUD,
        mark_hz=acars.MARK_HZ, space_hz=acars.SPACE_HZ, char_bits="7 data (LSB-first) + odd parity",
        framing=acars.FRAMING_NOTE, crc=frame.crc_spec,
        fields={"mode": frame.mode, "reg": frame.reg, "label": frame.label, "block_id": frame.block_id,
               "text": frame.text}, text_expected=frame.text,
        frame={"n_bits": len(frame.bits), "chars_hex": frame.chars.hex(), "crc_hex": f"{frame.crc:04x}"},
        identity={"type": "acars_reg", "value": frame.reg.strip()},
    )
    f = cap.center_hz
    scene.annotate(start, n, f - bw / 2, f + bw / 2, "acars-message", truth)
    scene.scenario_truth["message"] = {"mode": frame.mode, "reg": frame.reg, "label": frame.label,
                                       "block_id": frame.block_id, "text": frame.text,
                                       "crc_hex": f"{frame.crc:04x}"}
    scene.scenario_truth["note"] = ("synthetic: acarsdec was not available to build (not in "
                                    "Homebrew, no cheap tap) so this fixture's truth is the "
                                    "generator's own frame, cross-checked by an independent "
                                    "MSK/CRC reference decoder in py/tests/test_synth.py, not "
                                    "acarsdec (T-098)")
    return [scene], {}


# ---------------------------------------------------------------------------------------------
# adsb_squitter (SIGNAL-001 fallback)
# ---------------------------------------------------------------------------------------------

ADSB_DEFAULTS: dict[str, Any] = {
    "sample_rate": 2.4e6,
    "center_hz": 1090e6,
    "duration_s": 0.3,
    "noise_dbfs": -35.0,
    "icao": ["a0b1c2", "4ca853", "3c6444", "c0ffee"],
    "callsigns": [],
    "messages_per_aircraft": 8,
    "power_dbfs_min": -15.0,
    "power_dbfs_max": -6.0,
    "cfo_max_hz": 50e3,
    "lat": 52.2,
    "lon": 0.12,
    "calibration_k_db": -60.0,
    "start_utc": DEFAULT_START_UTC,
}

ADSB_CYCLE = ("identification", "position-even", "position-odd", "velocity")


def adsb_squitter(ctx: Ctx) -> tuple[list[Scene], dict[str, Any]]:
    p = ctx.params
    fs = float(p["sample_rate"])
    scene = ctx.scene("adsb_squitter", fs, _n(p),
                      "hkpy.synth adsb_squitter: Mode S DF17 extended squitters (PPM, CRC-24)")
    _noise_capture(scene, p, p["center_hz"], p["calibration_k_db"])
    cap = scene.captures[0]
    icaos = [int(str(h), 16) for h in p["icao"]]
    callsigns = list(p["callsigns"]) or [f"HKRF{i + 1:02d}" for i in range(len(icaos))]
    if len(callsigns) != len(icaos):
        raise ValueError("callsigns must match icao in length")
    aircraft = []
    for i, (icao, cs) in enumerate(zip(icaos, callsigns, strict=True)):
        r = scene.rng("aircraft", i)
        aircraft.append({
            "icao": f"{icao:06x}", "callsign": str(cs).upper(),
            "power_dbfs": float(r.uniform(p["power_dbfs_min"], p["power_dbfs_max"])),
            "cfo_hz": float(r.uniform(-p["cfo_max_hz"], p["cfo_max_hz"])),
            "lat": float(p["lat"]) + float(r.uniform(-0.8, 0.8)),
            "lon": float(p["lon"]) + float(r.uniform(-1.2, 1.2)),
            "altitude_ft": int(round(r.uniform(3000, 39000) / 25) * 25),
            "ew_kt": int(r.integers(-450, 451)), "ns_kt": int(r.integers(-450, 451)),
            "vrate_fpm": int(r.integers(-32, 33)) * 64,
            "carrier_phase": float(r.uniform(0, 2 * math.pi)),
        })
    per = int(p["messages_per_aircraft"])
    total = per * len(aircraft)
    margin_s = 2e-3
    slot = (scene.n_samples / fs - 2 * margin_s) / max(total, 1)
    if slot < 250e-6:
        raise ValueError("too many messages for duration_s; messages would overlap")
    order = scene.rng("order").permutation(total)
    jitter = scene.rng("slot-jitter")
    msg_len = int(round(adsb.MESSAGE_S * fs))
    margin = 32
    for j, idx in enumerate(order):
        ac_i, m_i = divmod(int(idx), per)
        ac = aircraft[ac_i]
        kind = ADSB_CYCLE[m_i % len(ADSB_CYCLE)]
        fields: dict[str, Any]
        if kind == "identification":
            me = adsb.me_identification(ac["callsign"])
            tc, fields = 4, {"callsign": ac["callsign"]}
        elif kind == "velocity":
            me = adsb.me_velocity(ac["ew_kt"], ac["ns_kt"], ac["vrate_fpm"])
            tc, fields = 19, {"ew_velocity_kt": ac["ew_kt"], "ns_velocity_kt": ac["ns_kt"],
                              "vertical_rate_fpm": ac["vrate_fpm"]}
        else:
            odd = kind == "position-odd"
            me, ylat, xlon = adsb.me_airborne_position(ac["altitude_ft"], ac["lat"], ac["lon"], odd)
            tc, fields = 11, {"altitude_ft": ac["altitude_ft"], "lat": ac["lat"], "lon": ac["lon"],
                              "cpr_format": "odd" if odd else "even", "cpr_lat": ylat, "cpr_lon": xlon}
        msg = adsb.df17(int(ac["icao"], 16), me)
        t = margin_s + j * slot + float(jitter.uniform(0, slot - 150e-6))
        start = int(round(t * fs))
        env = adsb.ppm_envelope(msg, fs, margin)
        tt = scene.time(start - margin, len(env))
        amp = math.sqrt(undb(ac["power_dbfs"]))
        scene.add_samples(start - margin,
                          amp * env * np.exp(1j * (2 * math.pi * ac["cfo_hz"] * tt + ac["carrier_phase"])))
        bw = min(2e6, fs)
        f = cap.center_hz + ac["cfo_hz"]
        crc = msg[-3:].hex()
        scene.annotate(start, msg_len, f - bw / 2, f + bw / 2, "adsb-df17", scene.emission_truth(
            cap, ac["cfo_hz"], bw, ac["power_dbfs"], kind="adsb-df17", modulation="ppm",
            power_definition="pulse-on power", df=17, ca=5, icao=ac["icao"], tc=tc,
            message_kind=kind, message_hex=msg.hex(), crc_hex=crc,
            crc={**adsb.CRC24_SPEC, "value": f"0x{crc.upper()}", "valid": True},
            metadata=fields, identity={"type": "icao", "value": ac["icao"]},
        ))
    for ac in aircraft:
        ac["n_messages"] = per
        ac.pop("carrier_phase")
    scene.scenario_truth["aircraft"] = aircraft
    return [scene], {}
