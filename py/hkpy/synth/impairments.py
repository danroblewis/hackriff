"""Receiver impairments, composable on any scenario (applied after the scenario builds its signals).

Order, which follows the signal path (the cubic nonlinearity commutes with the LO terms because it
depends only on |x|):

1. ``blocker_dbfs``: two strong CW tones, then a memoryless cubic front end ``y = x(1 + c|x|^2)``
   with ``c = im3_coeff``. Produces third-order intermodulation (IM3) at ``2f1-f2`` and ``2f2-f1``
   with amplitude exactly ``|c|·A1²·A2``, compression of the tones, and an average small-signal
   gain change (desensitisation) of ``20log10|1 + 2c(A1² + A2²)|`` on everything else. Push
   ``blocker_dbfs`` near 0 to clip the ADC (``overload``).
2. ``lo_ppm``: LO frequency error. Every signal appears shifted by ``-lo_ppm·1e-6·center``;
   annotation boxes move with it, ``rf_center_hz`` does not.
3. ``phase_noise_linewidth_hz``: Wiener LO phase noise with that Lorentzian linewidth.
4. ``iq_gain_db`` / ``iq_phase_deg``: ``y = μx + νx*``. Image rejection ``|μ|²/|ν|²``; images of
   emissions above the floor are annotated.
5. ``dc_offset_dbfs`` (+ ``dc_phase_deg``): an additive DC term.
6. ``spur_dbfs``: internal CW spurs at every multiple of ``spur_step_hz`` (default 10 MHz) that falls
   inside the capture, relative to the tuner centre (``core:frequency``). Not subject to LO error.
7. ``adc_gain_db``: gain in front of the ADC (all power truth moves with it). Use it to drive the
   ADC into clipping (``overload``).
8. ADC: 8-bit quantisation (``ci8``) and clipping at full scale, done by ``Scene.write``.

The cubic saturates at its peak (``|x| = sqrt(-1/(3c))`` for ``c < 0``) instead of folding back; the
IM3 truth is exact only while the input stays below that point (``im3_truth_exact``). Scenario-level
summaries (``hackriff:truth`` of role ``scenario``) describe the emitters before impairments; the
per-annotation truth is updated by every impairment and is authoritative.
"""

from __future__ import annotations

import math
from typing import Any

import numpy as np

from hkpy.synth.scene import CaptureSeg, Scene, db, undb

IMPAIRMENT_DEFAULTS: dict[str, Any] = {
    "blocker_dbfs": None,
    "blocker_offsets_hz": [],
    "im3_coeff": -0.3,
    "lo_ppm": 0.0,
    "phase_noise_linewidth_hz": 0.0,
    "iq_gain_db": 0.0,
    "iq_phase_deg": 0.0,
    "dc_offset_dbfs": None,
    "dc_phase_deg": 45.0,
    "spur_dbfs": None,
    "spur_step_hz": 10e6,
    "adc_gain_db": 0.0,
}

#: An artefact is annotated only when it would stand this far above the floor in its bandwidth.
VISIBLE_SNR_DB = 3.0


def apply(scene: Scene, p: dict[str, Any]) -> None:
    for cap in scene.captures:
        if p["blocker_dbfs"] is not None:
            _blocker(scene, cap, p)
        if p["lo_ppm"]:
            _lo_error(scene, cap, float(p["lo_ppm"]))
        if p["phase_noise_linewidth_hz"]:
            _phase_noise(scene, cap, float(p["phase_noise_linewidth_hz"]))
        if p["iq_gain_db"] or p["iq_phase_deg"]:
            _iq_imbalance(scene, cap, float(p["iq_gain_db"]), float(p["iq_phase_deg"]))
        if p["dc_offset_dbfs"] is not None:
            _dc(scene, cap, float(p["dc_offset_dbfs"]), float(p["dc_phase_deg"]))
        if p["spur_dbfs"] is not None:
            _spurs(scene, cap, float(p["spur_dbfs"]), float(p["spur_step_hz"]))
        if p["adc_gain_db"]:
            _adc_gain(scene, cap, float(p["adc_gain_db"]))


def _segment(scene: Scene, cap: CaptureSeg) -> tuple[slice, np.ndarray]:
    sl = slice(cap.sample_start, cap.sample_start + cap.sample_count)
    t = np.arange(cap.sample_count) / scene.sample_rate
    return sl, t


def _record(scene: Scene, cap: CaptureSeg, kind: str, **fields: Any) -> None:
    scene.impairments.append({"kind": kind, "capture_center_hz": cap.center_hz, **fields})


def _visible(cap: CaptureSeg, power_dbfs: float, bandwidth_hz: float, fs: float) -> bool:
    if cap.floor_dbfs_per_hz is None:
        return True
    bw = max(bandwidth_hz, fs / 4096)
    return power_dbfs - (cap.floor_dbfs_per_hz + db(bw)) > VISIBLE_SNR_DB


def _blocker(scene: Scene, cap: CaptureSeg, p: dict[str, Any]) -> None:
    fs = scene.sample_rate
    offsets = [float(f) for f in p["blocker_offsets_hz"]] or [0.30 * fs, 0.36 * fs]
    if len(offsets) != 2:
        raise ValueError("blocker_offsets_hz needs exactly two offsets")
    a = math.sqrt(undb(float(p["blocker_dbfs"])))
    c = float(p["im3_coeff"])
    sl, t = _segment(scene, cap)
    rng = scene.rng("blocker", cap.sample_start)
    phases = rng.uniform(0, 2 * math.pi, 2)
    tones = sum(a * np.exp(1j * (2 * math.pi * f * t + ph)) for f, ph in zip(offsets, phases, strict=True))
    scene.x[sl] += tones

    # Everything already in the capture sees the average small-signal gain of the blocker.
    desense_db = 20 * math.log10(abs(1 + 2 * c * (2 * a * a)))
    scene.scale_powers(cap, desense_db)
    x = scene.x[sl]
    r = np.abs(x)
    r_peak = math.sqrt(-1 / (3 * c)) if c < 0 else math.inf
    r_c = np.minimum(r, r_peak)
    scale = np.ones_like(r)
    nz = r > 0
    scale[nz] = (r_c[nz] / r[nz]) * (1 + c * r_c[nz] ** 2)
    scene.x[sl] = x * scale

    tone_out = 20 * math.log10(abs(a * (1 + c * (a * a + 2 * a * a))))
    n = cap.sample_count
    for i, f in enumerate(offsets):
        scene.annotate(cap.sample_start, n, cap.center_hz + f, cap.center_hz + f, "blocker",
                       scene.emission_truth(cap, f, 0.0, tone_out, kind="blocker", modulation="cw",
                                            input_power_dbfs=float(p["blocker_dbfs"]), tone_index=i))
    im3_db = 20 * math.log10(abs(c) * a**3) if c else None
    products = []
    for f1, f2 in ((offsets[0], offsets[1]), (offsets[1], offsets[0])):
        f = 2 * f1 - f2
        products.append(f)
        if im3_db is None or abs(f) >= fs / 2:
            continue
        scene.annotate(cap.sample_start, n, cap.center_hz + f, cap.center_hz + f, "im3",
                       {"role": "artefact", "kind": "im3", "order": 3,
                        "center_hz": cap.center_hz + f, "offset_hz": f, "bandwidth_hz": 0.0,
                        "power_dbfs": im3_db, "power_dbm": im3_db + cap.calibration_k_db,
                        "products_of_hz": [cap.center_hz + f1, cap.center_hz + f2]})
    _record(scene, cap, "blocker-im3", blocker_dbfs=float(p["blocker_dbfs"]),
            offsets_hz=offsets, im3_coeff=c, im3_power_dbfs=im3_db, im3_offsets_hz=products,
            desensitisation_db=desense_db, im3_truth_exact=bool(2 * a <= r_peak),
            model="y = x(1 + c|x|^2), saturating at its peak; IM3 amplitude |c|*A1^2*A2; "
                  "small-signal gain 1 + 2c(A1^2 + A2^2)")


def _adc_gain(scene: Scene, cap: CaptureSeg, gain_db: float) -> None:
    sl, _ = _segment(scene, cap)
    scene.x[sl] *= 10 ** (gain_db / 20)
    scene.scale_powers(cap, gain_db)
    _record(scene, cap, "adc-gain", gain_db=gain_db)


def _lo_error(scene: Scene, cap: CaptureSeg, ppm: float) -> None:
    offset = -ppm * 1e-6 * cap.center_hz
    sl, t = _segment(scene, cap)
    scene.x[sl] *= np.exp(2j * math.pi * offset * t)
    for ann in scene.annotations_in(cap, ("emission", "artefact")):
        ann["f_lo"] += offset
        ann["f_hi"] += offset
        tr = ann["truth"]
        for key in ("center_hz", "offset_hz"):
            if key in tr:
                tr[key] += offset
        tr["lo_offset_hz"] = offset
    _record(scene, cap, "lo-error", lo_ppm=ppm, apparent_shift_hz=offset)


def _phase_noise(scene: Scene, cap: CaptureSeg, linewidth_hz: float) -> None:
    sl, _ = _segment(scene, cap)
    rng = scene.rng("phase-noise", cap.sample_start)
    step = math.sqrt(2 * math.pi * linewidth_hz / scene.sample_rate)
    phase = np.cumsum(step * rng.standard_normal(cap.sample_count))
    scene.x[sl] *= np.exp(1j * phase)
    _record(scene, cap, "phase-noise", linewidth_hz=linewidth_hz, model="Wiener (Lorentzian lineshape)")


def _iq_imbalance(scene: Scene, cap: CaptureSeg, gain_db: float, phase_deg: float) -> None:
    g = 10 ** (gain_db / 20)
    phi = math.radians(phase_deg)
    mu = (1 + g * np.exp(-1j * phi)) / 2
    nu = (1 - g * np.exp(1j * phi)) / 2
    irr_db = db(abs(mu) ** 2 / abs(nu) ** 2) if abs(nu) > 0 else math.inf
    sl, _ = _segment(scene, cap)
    x = scene.x[sl]
    scene.x[sl] = mu * x + nu * np.conj(x)
    sources = scene.annotations_in(cap, ("emission", "artefact"))
    scene.scale_powers(cap, db(abs(mu) ** 2))
    for ann in sources:
        tr = ann["truth"]
        if tr.get("power_dbfs") is None or tr["kind"] in ("iq-image", "overload"):
            continue
        img_power = tr["power_dbfs"] - irr_db
        bw = float(tr.get("bandwidth_hz", 0.0))
        if not _visible(cap, img_power, bw, scene.sample_rate):
            continue
        f_img = 2 * cap.center_hz - tr["center_hz"]
        scene.annotate(ann["sample_start"], ann["sample_count"], f_img - bw / 2, f_img + bw / 2,
                       "iq-image",
                       {"role": "artefact", "kind": "iq-image", "center_hz": f_img,
                        "offset_hz": f_img - cap.center_hz, "bandwidth_hz": bw,
                        "power_dbfs": img_power, "power_dbm": img_power + cap.calibration_k_db,
                        "image_of_hz": tr["center_hz"], "image_of_kind": tr["kind"]})
    _record(scene, cap, "iq-imbalance", gain_db=gain_db, phase_deg=phase_deg,
            image_rejection_db=irr_db, model="y = mu*x + nu*conj(x)")


def _dc(scene: Scene, cap: CaptureSeg, power_dbfs: float, phase_deg: float) -> None:
    dc = math.sqrt(undb(power_dbfs)) * np.exp(1j * math.radians(phase_deg))
    sl, _ = _segment(scene, cap)
    scene.x[sl] += dc
    scene.annotate(cap.sample_start, cap.sample_count, cap.center_hz, cap.center_hz, "dc-offset",
                   {"role": "artefact", "kind": "dc-offset", "center_hz": cap.center_hz,
                    "offset_hz": 0.0, "bandwidth_hz": 0.0, "power_dbfs": power_dbfs,
                    "power_dbm": power_dbfs + cap.calibration_k_db,
                    "i_offset": float(dc.real), "q_offset": float(dc.imag)})
    _record(scene, cap, "dc-offset", power_dbfs=power_dbfs, phase_deg=phase_deg)


def _spurs(scene: Scene, cap: CaptureSeg, power_dbfs: float, step_hz: float) -> None:
    fs = scene.sample_rate
    lo = math.ceil((cap.center_hz - 0.49 * fs) / step_hz)
    hi = math.floor((cap.center_hz + 0.49 * fs) / step_hz)
    sl, t = _segment(scene, cap)
    rng = scene.rng("spur", cap.sample_start)
    amp = math.sqrt(undb(power_dbfs))
    found = []
    for n in range(lo, hi + 1):
        f_abs = n * step_hz
        offset = f_abs - cap.center_hz
        scene.x[sl] += amp * np.exp(1j * (2 * math.pi * offset * t + rng.uniform(0, 2 * math.pi)))
        found.append(f_abs)
        scene.annotate(cap.sample_start, cap.sample_count, f_abs, f_abs, "spur",
                       {"role": "artefact", "kind": "spur", "center_hz": f_abs, "offset_hz": offset,
                        "bandwidth_hz": 0.0, "power_dbfs": power_dbfs,
                        "power_dbm": power_dbfs + cap.calibration_k_db, "harmonic_n": n,
                        "spur_step_hz": step_hz, "tuner_center_hz": cap.center_hz})
    _record(scene, cap, "internal-spur", power_dbfs=power_dbfs, step_hz=step_hz,
            tuner_center_hz=cap.center_hz, spurs_hz=found)
