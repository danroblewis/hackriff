"""Trunking control-channel scenario (T-267, C23): a continuous C4FM CC hidden among bursty NBFM.

The scene exists to separate two things a frequency-channel-occupancy (FCO) measure cannot:

- a **control channel** — continuous C4FM on the 12.5 kHz LMR raster, carrying a real frame sync
  and CRC-valid blocks;
- a **continuous data emitter** — equally continuous, equally on-raster, equally 4FSK, carrying
  neither.

Both sit at 100 % FCO on the raster, so both are *candidates*. Only the first should ever be
confirmed. That decoy is the point of the fixture: C23's named pitfall is "false CCs: continuous
data emitters pass the FCO test. Require sync plus CRC."

Bursty NBFM neighbours fill the rest of the raster so the CC has to be found among traffic
rather than in an otherwise-empty band.
"""

from __future__ import annotations

import math
from typing import Any

import numpy as np

from hkpy.synth import trunking as tk
from hkpy.synth.scenarios import Ctx, DEFAULT_START_UTC, utc_plus
from hkpy.synth.scene import Scene, complex_noise, db, undb

TRUNK_CC_DEFAULTS: dict[str, Any] = {
    "sample_rate": 500e3,
    # 800 MHz public-safety trunking (docs/04 §4: 851-869 MHz, control channels 100 % duty).
    "center_hz": 851.0125e6,
    "duration_s": 1.0,
    "raster_hz": tk.LMR_RASTER_HZ,
    # Raster channel indices, relative to center_hz.
    "cc_channel": 3,
    "decoy_channel": -5,
    "nbfm_channels": [-2, 7, 11, -9],
    "cc_snr_db": 20.0,
    "decoy_snr_db": 20.0,
    "nbfm_snr_db": 18.0,
    "symbol_rate_bd": tk.C4FM_SYMBOL_RATE_BD,
    "cc_cfo_hz": 0.0,
    "fm_deviation_hz": 2500.0,
    "audio_tone_hz": 1000.0,
    "burst_mean_on_s": 0.08,
    "burst_mean_off_s": 0.12,
    "noise_dbfs": -40.0,
    "calibration_k_db": -70.0,
    "start_utc": DEFAULT_START_UTC,
}


def _n(p: dict[str, Any]) -> int:
    return int(round(float(p["duration_s"]) * float(p["sample_rate"])))


def _power(cap: Any, snr_db: float, bw_hz: float) -> float:
    """Absolute dBFS for a wanted SNR in ``bw_hz`` against the capture's floor density."""
    return float(snr_db) + cap.floor_dbfs_per_hz + db(bw_hz)


def _on_off(rng: np.random.Generator, span_s: float, mean_on: float, mean_off: float
            ) -> list[tuple[float, float]]:
    """Two-state on/off intervals over ``span_s`` (exponential dwell times)."""
    out: list[tuple[float, float]] = []
    t = float(rng.exponential(mean_off))
    while t < span_s:
        on = float(rng.exponential(mean_on))
        end = min(span_s, t + on)
        if end > t:
            out.append((t, end))
        t = end + float(rng.exponential(mean_off))
    return out


def trunk_control_channel(ctx: Ctx) -> tuple[list[Scene], dict[str, Any]]:
    p = ctx.params
    fs = float(p["sample_rate"])
    n = _n(p)
    scene = ctx.scene(
        "trunk_control_channel", fs, n,
        "hkpy.synth trunk_control_channel: continuous C4FM control channel, a continuous 4FSK "
        "decoy with no framing, and bursty NBFM on the 12.5 kHz LMR raster",
    )

    # Noise floor over the whole scene.
    cap = scene.add_capture(0, n, float(p["center_hz"]), utc_plus(p["start_utc"], 0.0),
                            calibration_k_db=float(p["calibration_k_db"]))
    cap.floor_dbfs_per_hz = float(p["noise_dbfs"]) - db(fs)
    scene.add_samples(0, complex_noise(scene.rng("noise"), n, float(p["noise_dbfs"])))
    scene.add_floor(0, n, cap.floor_dbfs_per_hz)

    raster = float(p["raster_hz"])
    rate = float(p["symbol_rate_bd"])
    # C4FM occupies a 12.5 kHz channel; Carson on the outer deviation plus the symbol rate.
    c4fm_bw = 2 * (1800.0 + rate / 2)
    t_all = scene.time(0, n)

    def place_c4fm(channel: int, dibits: np.ndarray, snr_db: float, cfo_hz: float) -> tuple[float, float]:
        off = channel * raster + cfo_hz
        if abs(off) + c4fm_bw / 2 > fs / 2:
            raise ValueError(f"channel {channel} does not fit inside the sample rate")
        power = _power(cap, snr_db, c4fm_bw)
        iq = tk.c4fm(dibits, fs, rate)[:n]
        phase0 = float(scene.rng("phase", channel).uniform(0, 2 * math.pi))
        scene.add_samples(0, math.sqrt(undb(power)) * iq
                          * np.exp(1j * (2 * math.pi * off * t_all[: len(iq)] + phase0)))
        return off, power

    # --- The control channel: continuous C4FM, real frame sync, CRC-valid blocks.
    frames_needed = int(math.ceil(n * rate / fs / tk.FRAME_DIBITS)) + 1
    cc_dibits, cc_frames = tk.control_channel_dibits(scene.rng("cc"), frames_needed)
    cc_ch = int(p["cc_channel"])
    cc_off, cc_power = place_c4fm(cc_ch, cc_dibits, float(p["cc_snr_db"]), float(p["cc_cfo_hz"]))
    cc_f = cap.center_hz + cc_off
    scene.annotate(
        0, n, cc_f - c4fm_bw / 2, cc_f + c4fm_bw / 2, "trunk-control-channel",
        scene.emission_truth(
            cap, cc_off, c4fm_bw, cc_power,
            kind="trunk-control-channel", modulation="c4fm", levels=4,
            symbol_rate_bd=rate, duty_cycle=1.0, fco=1.0,
            raster_hz=raster, raster_channel=cc_ch,
            nominal_center_hz=cap.center_hz + cc_ch * raster,
            cfo_hz=float(p["cc_cfo_hz"]),
            is_control_channel=True, confirmable=True,
            frame=tk.CC_FRAME_SPEC,
            n_frames=len(cc_frames),
            frames=cc_frames[:8],
            sync_hex=tk.P25_FRAME_SYNC_HEX,
        ),
    )

    # --- The decoy: continuous, on-raster, 4FSK, and unframed. Must never be confirmed.
    n_dibits = int(math.ceil(n * rate / fs)) + 1
    decoy_dibits = tk.continuous_data_dibits(scene.rng("decoy"), n_dibits)
    dc_ch = int(p["decoy_channel"])
    dc_off, dc_power = place_c4fm(dc_ch, decoy_dibits, float(p["decoy_snr_db"]), 0.0)
    dc_f = cap.center_hz + dc_off
    scene.annotate(
        0, n, dc_f - c4fm_bw / 2, dc_f + c4fm_bw / 2, "continuous-data",
        scene.emission_truth(
            cap, dc_off, c4fm_bw, dc_power,
            kind="continuous-data", modulation="c4fm", levels=4,
            symbol_rate_bd=rate, duty_cycle=1.0, fco=1.0,
            raster_hz=raster, raster_channel=dc_ch,
            nominal_center_hz=cap.center_hz + dc_ch * raster,
            is_control_channel=False, confirmable=False,
            why_not="continuous 4FSK with no frame sync and no CRC-valid block: passes FCO "
                    "candidacy, fails sync+CRC confirmation",
        ),
    )

    # --- Bursty NBFM neighbours.
    dev, tone = float(p["fm_deviation_hz"]), float(p["audio_tone_hz"])
    nbfm_bw = 2 * (dev + tone)
    span_s = n / fs
    n_bursts = 0
    for ch in [int(c) for c in p["nbfm_channels"]]:
        off = ch * raster
        if abs(off) + nbfm_bw / 2 > fs / 2:
            raise ValueError(f"nbfm channel {ch} does not fit inside the sample rate")
        power = _power(cap, float(p["nbfm_snr_db"]), nbfm_bw)
        amp = math.sqrt(undb(power))
        r = scene.rng("nbfm", ch)
        for b0, b1 in _on_off(r, span_s, float(p["burst_mean_on_s"]), float(p["burst_mean_off_s"])):
            i0, i1 = int(round(b0 * fs)), min(n, int(round(b1 * fs)))
            if i1 <= i0:
                continue
            phase0, audio_phase = (float(v) for v in r.uniform(0, 2 * math.pi, 2))
            tt = scene.time(i0, i1 - i0)
            phase = (phase0 + 2 * math.pi * off * tt
                     + (dev / tone) * np.sin(2 * math.pi * tone * (tt - b0) + audio_phase))
            scene.add_samples(i0, amp * np.exp(1j * phase))
            f = cap.center_hz + off
            scene.annotate(
                i0, i1 - i0, f - nbfm_bw / 2, f + nbfm_bw / 2, "nbfm-burst",
                scene.emission_truth(
                    cap, off, nbfm_bw, power, kind="nbfm-burst", modulation="nbfm",
                    deviation_hz=dev, audio_tone_hz=tone,
                    raster_hz=raster, raster_channel=ch,
                    is_control_channel=False, confirmable=False,
                    burst_start_s=b0, burst_duration_s=b1 - b0,
                ),
            )
            n_bursts += 1

    scene.scenario_truth["trunking"] = {
        "raster_hz": raster,
        "raster_origin_hz": cap.center_hz,
        "control_channel": {
            "rf_center_hz": cc_f,
            "raster_channel": cc_ch,
            "modulation": "c4fm",
            "symbol_rate_bd": rate,
            "bandwidth_hz": c4fm_bw,
            "duty_cycle": 1.0,
            "sync_hex": tk.P25_FRAME_SYNC_HEX,
            "n_frames": len(cc_frames),
            "expected_confirmed": True,
        },
        "continuous_decoy": {
            "rf_center_hz": dc_f,
            "raster_channel": dc_ch,
            "modulation": "c4fm",
            "symbol_rate_bd": rate,
            "bandwidth_hz": c4fm_bw,
            "duty_cycle": 1.0,
            "expected_confirmed": False,
            "why_not": "no frame sync, no CRC-valid block",
        },
        "nbfm_channels": [int(c) for c in p["nbfm_channels"]],
        "n_nbfm_bursts": n_bursts,
        "frame": tk.CC_FRAME_SPEC,
    }
    return [scene], {}
