"""Retune-diversity scene (T-586, AWARE-011): the same region surveyed at several centres.

The physics this fixture exists to exercise is a two-line rule:

- a **real emission** sits at a fixed **absolute** frequency, so retuning the front end does not
  move it;
- a **receiver artefact** — DC/LO leakage, an LO-relative internal spur, an IQ image, an IM3
  product — is manufactured at a fixed offset from the **local oscillator**, so it moves with the
  centre, to a *new absolute frequency*, every time the radio is retuned.

Everything in the scene is therefore the same object to a single-capture detector: a narrow CW
line in white noise. Only the behaviour across centres separates the two classes, which is exactly
the property under test, and the reason nothing here is modulated — modulation would let a test
cheat by recognising the signal instead of measuring the invariant.

Layout (defaults). ``centers_hz`` are visited in order, one capture each, sample counters and
timestamps continuing across them as a real retune does:

- ``emitter_offsets_hz`` are **absolute** RF frequencies (badly named ``offsets`` only in the sense
  of the parameter list): each is injected into every capture at baseband ``f − centre``, so it
  stays put across the survey.
- the DC artefact comes from the shared ``dc_offset_dbfs`` impairment: one line at baseband 0, i.e.
  at the tuned centre, in every capture.
- ``lo_spur_offset_hz`` is an internal spur at a fixed **baseband** offset, injected here rather
  than by :mod:`hkpy.synth.impairments` (whose ``spur_dbfs`` spurs sit on an absolute reference
  raster, a different mechanism: those do *not* move with the LO).

The generator refuses a layout whose lines come closer than ``min_separation_hz`` **within one
capture**, so a detector merging two lines can never be mistaken for the invariant failing.
"""

from __future__ import annotations

import math
from typing import Any

import numpy as np

from hkpy.synth.scene import Scene, complex_noise, db, undb

RETUNE_DEFAULTS: dict[str, Any] = {
    "sample_rate": 4e6,
    # Three centres 500 kHz apart over one region: enough diversity that a fixed-offset artefact
    # and a fixed-frequency emission cannot be confused, and few enough to stay a fast fixture.
    "centers_hz": [100.0e6, 100.5e6, 101.0e6],
    # Absolute RF frequencies of the real emitters. Each is in every capture's passband.
    "emitter_offsets_hz": [99.78e6, 100.68e6, 101.18e6],
    "emitter_power_dbfs": -25.0,
    # Baseband offset of the internal LO-relative spur, so it appears at centre + this.
    "lo_spur_offset_hz": 370e3,
    "lo_spur_power_dbfs": -30.0,
    "noise_dbfs": -40.0,
    # The DC/LO-leakage artefact: one line at baseband 0 in every capture, i.e. always at the
    # tuned centre. Applied by hkpy.synth.impairments after the scene is built.
    "dc_offset_dbfs": -30.0,
    "dwell_s": 0.3,
    "calibration_k_db": -70.0,
    "start_utc": "2026-09-21T12:00:00Z",
    "min_separation_hz": 150e3,
    # Settle gap between captures, samples: the retune discontinuity, not a stream restart.
    "settle_samples": 0,
}


def _tone(scene: Scene, start: int, count: int, offset_hz: float, power_dbfs: float,
          rng_name: str) -> float:
    amp = math.sqrt(undb(power_dbfs))
    phase = float(scene.rng(rng_name, start).uniform(0, 2 * math.pi))
    t = scene.time(start, count)
    scene.add_samples(start, amp * np.exp(1j * (2 * math.pi * offset_hz * t + phase)))
    return phase


def retune_diversity(ctx: Any) -> tuple[list[Scene], dict[str, Any]]:
    from hkpy.synth.scenarios import utc_plus

    p = ctx.params
    fs = float(p["sample_rate"])
    centers = [float(c) for c in p["centers_hz"]]
    emitters = [float(f) for f in p["emitter_offsets_hz"]]
    spur_offset = float(p["lo_spur_offset_hz"])
    dwell = int(round(float(p["dwell_s"]) * fs))
    settle = int(p["settle_samples"])
    min_sep = float(p["min_separation_hz"])
    if len(centers) < 2:
        raise ValueError("retune_diversity needs at least two centres to be a retune at all")

    # A priori layout check: within one capture every line must be resolvable, so that a merged
    # pair can never masquerade as the invariant breaking.
    usable_half = 0.42 * fs
    for c in centers:
        lines = sorted([*emitters, c, c + spur_offset])
        for f in lines:
            if abs(f - c) > usable_half:
                raise ValueError(
                    f"line {f / 1e6:.4f} MHz is {abs(f - c) / 1e6:.4f} MHz from centre "
                    f"{c / 1e6:.4f} MHz, outside the usable +/-{usable_half / 1e6:.4f} MHz")
        gaps = [b - a for a, b in zip(lines, lines[1:], strict=False)]
        if gaps and min(gaps) < min_sep:
            raise ValueError(
                f"centre {c / 1e6:.4f} MHz: lines {min(gaps) / 1e3:.1f} kHz apart, under the "
                f"{min_sep / 1e3:.1f} kHz minimum separation")

    stride = dwell + settle
    n = stride * len(centers) - settle
    scene = ctx.scene(
        "retune_diversity", fs, n,
        "hkpy.synth retune_diversity: one region surveyed at several centres; fixed-frequency "
        "emitters and LO-relative receiver artefacts, all CW lines in white noise")
    floor_per_hz = float(p["noise_dbfs"]) - db(fs)
    scene.add_samples(0, complex_noise(scene.rng("noise"), n, float(p["noise_dbfs"])))

    for i, center in enumerate(centers):
        start = i * stride
        cap = scene.add_capture(start, dwell, center,
                                utc_plus(p["start_utc"], start / fs),
                                calibration_k_db=float(p["calibration_k_db"]))
        cap.floor_dbfs_per_hz = floor_per_hz
        scene.add_floor(start, dwell, floor_per_hz)

        for k, f_abs in enumerate(emitters):
            offset = f_abs - center
            power = float(p["emitter_power_dbfs"])
            phase = _tone(scene, start, dwell, offset, power, f"emitter{k}")
            scene.annotate(
                start, dwell, f_abs, f_abs, "cw-emitter",
                scene.emission_truth(
                    cap, offset, 0.0, power, kind="cw", modulation="cw",
                    phase_rad=phase, emitter_index=k,
                    # The invariant, stated in the truth: this frequency does not depend on the
                    # centre it was seen from.
                    lo_relative=False, capture_index=i, capture_center_hz=center,
                    snr_db_per_hz=power - floor_per_hz))

        f_spur = center + spur_offset
        power = float(p["lo_spur_power_dbfs"])
        _tone(scene, start, dwell, spur_offset, power, "lo-spur")
        scene.annotate(
            start, dwell, f_spur, f_spur, "lo-spur",
            {"role": "artefact", "kind": "lo-spur", "center_hz": f_spur,
             "offset_hz": spur_offset, "bandwidth_hz": 0.0, "power_dbfs": power,
             "power_dbm": power + cap.calibration_k_db, "lo_relative": True,
             "lo_slope": 1.0, "capture_index": i, "tuner_center_hz": center,
             "mechanism": "internal spur at a fixed offset from the LO"})

    scene.scenario_truth["retune_diversity"] = {
        "centers_hz": centers,
        "emitters_hz": emitters,
        "lo_relative_offsets_hz": [0.0, spur_offset],
        "rule": "a real emission keeps its absolute frequency across centres; a receiver artefact "
                "keeps its offset from the LO and so moves to a new absolute frequency",
    }
    return [scene], {}
