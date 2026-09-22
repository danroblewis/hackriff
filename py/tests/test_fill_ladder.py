"""ADC fill: the bucket rule, the recorded provenance field and the ladder sweep (T-625).

The stratification key of the MAUTO corpus (docs/22 §2 axis A3, §4.4), conditioned exactly as
ADR-0015 §13.3 fixes it: **two buckets**, because σ = 0.5 LSB is the only boundary docs/21 §3's
sweep supports, and **missing σ is `under_filled`, not `nominal`**.

Assertions here are counts and states, never wall-clock.
"""

from __future__ import annotations

import json
import math
import re
from pathlib import Path

import numpy as np

from hkpy import sigmf
from hkpy.synth import fill, generate

REPO = Path(__file__).resolve().parents[2]
RUST_PROVENANCE = REPO / "crates" / "hk-model" / "src" / "provenance.rs"

LADDER_PARAMS = {
    "duration_s": 0.2,
    "sample_rate": 200e3,
    "n_weak_signals": 0,
    "step_db": 0.0,
    "t0_s": 0.1,
}


def test_an_unmeasured_fill_is_under_filled_and_never_nominal() -> None:
    """The fail-closed direction, which is the whole point of recording the field."""
    assert fill.fill_bucket(None, 0.0) == "under_filled"
    assert fill.fill_bucket(None, None) == "under_filled"
    assert fill.fill_bucket(float("nan"), 0.0) == "under_filled"
    assert not fill.has_calibration_table(fill.fill_bucket(None, 0.0))
    # A clip fraction alone can never promote a window to a bucket that has a table.
    assert fill.fill_bucket(None, 0.0) != "nominal"


def test_the_boundary_is_the_only_one_the_measurement_supports() -> None:
    assert fill.fill_bucket(fill.UNDER_FILL_SIGMA_LSB, 0.0) == "nominal"
    assert fill.fill_bucket(fill.UNDER_FILL_SIGMA_LSB - 1e-3, 0.0) == "under_filled"
    assert fill.fill_bucket(77.0, fill.OVER_CLIP_FRACTION) == "nominal"
    assert fill.fill_bucket(77.0, fill.OVER_CLIP_FRACTION + 1e-3) == "over_clipped"
    assert fill.fill_bucket(77.0, float("nan")) == "over_clipped"
    # docs/21 §3: 0.57 → 77 LSB sit within 0.34 bits of each other, so they are ONE bucket.
    interior = {fill.fill_bucket(s, 0.0) for s in (0.57, 1.0, 2.0, 16.0, 32.0, 77.0)}
    assert interior == {"nominal"}


def test_the_six_level_ladder_classifies_into_exactly_two_buckets() -> None:
    """Six rungs, two buckets — and no rung reaches `over_clipped`, which is a finding, not a gap:
    the measurement stopped at 28.4 % clipped and the boundary is 30 %."""
    assert len(fill.FILL_LADDER) == 6
    buckets = [fill.fill_bucket(s, 0.0) for s in fill.FILL_LADDER]
    assert buckets.count("under_filled") == 1
    assert buckets.count("nominal") == 5
    assert buckets.count("over_clipped") == 0


def test_bucket_constants_match_the_rust_data_model() -> None:
    """One definition, two languages: a generator that strata differently from the engine that
    reads the recordings would silently mis-label every cell of the corpus."""
    src = RUST_PROVENANCE.read_text()
    sigma = re.search(r"pub const UNDER_FILL_SIGMA_LSB: f32 = ([0-9.]+);", src)
    clip = re.search(r"pub const OVER_CLIP_FRACTION: f64 = ([0-9.]+);", src)
    assert sigma and clip, "hk_model no longer declares the fill constants"
    assert float(sigma.group(1)) == fill.UNDER_FILL_SIGMA_LSB
    assert float(clip.group(1)) == fill.OVER_CLIP_FRACTION


def test_measure_fill_ignores_dc_and_counts_rails() -> None:
    rng = np.random.default_rng(625)
    iq = np.clip(np.rint(rng.normal(0.0, 2.0, size=(4096, 2))), -128, 127).astype(np.int8)
    sigma, clip_fraction = fill.measure_fill(iq)
    assert sigma is not None and abs(sigma - 2.0) < 0.2
    assert clip_fraction == 0.0
    offset = np.clip(iq.astype(np.int16) + 40, -128, 127).astype(np.int8)
    shifted, _ = fill.measure_fill(offset)
    assert shifted is not None and abs(shifted - sigma) < 0.05
    railed = iq.copy()
    railed[:1024, 0] = 127
    _, railed_fraction = fill.measure_fill(railed)
    assert railed_fraction == 0.25
    assert fill.measure_fill(np.zeros((0, 2), dtype=np.int8)) == (None, 0.0)


def test_generated_ci8_recordings_carry_their_measured_fill(tmp_path: Path) -> None:
    """The field is written where capture provenance is written — beside the gain state, the
    overload flag and the clip count — and it is the *measured* value, not the requested one."""
    manifest = json.loads(
        generate("noise_floor_rise", 625, tmp_path, LADDER_PARAMS, "ci8").read_text()
    )
    name = manifest["recordings"][0]
    meta = sigmf.read_meta(tmp_path / name)
    prov = meta["global"][sigmf.PROVENANCE_KEY]
    data = np.fromfile(sigmf.data_path(tmp_path / name), dtype=np.int8)
    measured, clip_fraction = fill.measure_fill(data)

    assert prov["noise_sigma_lsb"] == measured
    assert fill.fill_bucket(prov["noise_sigma_lsb"], clip_fraction) == "nominal"
    scenario = next(
        a["hackriff:truth"]
        for a in meta["annotations"]
        if a.get("hackriff:truth", {}).get("role") == "scenario"
    )
    assert scenario["noise_sigma_lsb"] == measured
    assert scenario["fill_bucket"] == "nominal"


def test_a_float_recording_has_no_adc_and_therefore_no_fill(tmp_path: Path) -> None:
    """T-547's control skipped the ADC; a float recording is that case. Nothing may invent a fill
    for it, and the absence classifies `under_filled` — the fail-closed direction."""
    manifest = json.loads(
        generate("noise_floor_rise", 625, tmp_path, LADDER_PARAMS, "cf32_le").read_text()
    )
    meta = sigmf.read_meta(tmp_path / manifest["recordings"][0])
    prov = meta["global"][sigmf.PROVENANCE_KEY]
    assert "noise_sigma_lsb" not in prov
    assert fill.fill_bucket(prov.get("noise_sigma_lsb"), 0.0) == "under_filled"


def test_the_fill_ladder_is_a_reproducible_sweep_of_one_variable(tmp_path: Path) -> None:
    """docs/22 §4.4's ladder: six rungs differing only in ADC fill, each recording carrying the
    fill it was actually written at, and the whole sweep collapsing to the two measured buckets."""
    manifest = fill.ladder("noise_floor_rise", 625, tmp_path, params=LADDER_PARAMS)

    assert len(manifest["rungs"]) == 6
    assert manifest["bucket_counts"] == {"nominal": 5, "under_filled": 1, "over_clipped": 0}
    assert [r["bucket"] for r in manifest["rungs"]][0] == "under_filled"
    assert sum(1 for r in manifest["rungs"] if r["has_calibration_table"]) == 5

    for rung in manifest["rungs"]:
        assert rung["recorded_sigma_lsb"] == rung["measured_sigma_lsb"]
        assert rung["bucket"] == fill.fill_bucket(
            rung["recorded_sigma_lsb"], rung["clip_fraction"]
        )
    # The sweep moves the variable it claims to move, monotonically, over three decades.
    measured = [r["measured_sigma_lsb"] for r in manifest["rungs"]]
    assert measured == sorted(measured)
    assert measured[-1] / measured[0] > 100.0
    # Only `adc_gain_db` differs between rungs; the scene, seed and every other parameter are one.
    gains = [r["adc_gain_db"] for r in manifest["rungs"]]
    assert gains == sorted(gains) and len(set(gains)) == 6
    # Heavy clipping is reached and is still nominal: docs/21 refuted clipping as the hazard.
    assert manifest["rungs"][-1]["clip_fraction"] > 0.1
    assert manifest["rungs"][-1]["bucket"] == "nominal"

    written = json.loads((tmp_path / "fill-ladder.json").read_text())
    assert written == manifest


def test_adc_gain_for_a_target_fill_is_a_closed_form(tmp_path: Path) -> None:
    """The rung is derived from the scene's own noise level, never searched."""
    # Complex noise at -40 dBFS: per-component sigma = sqrt(1e-4/2) * 127.
    assert math.isclose(fill.sigma_lsb_of_noise_dbfs(-40.0), math.sqrt(1e-4 / 2) * 127.0)
    assert math.isclose(fill.adc_gain_db_for(fill.sigma_lsb_of_noise_dbfs(-40.0), -40.0), 0.0)
    assert math.isclose(fill.adc_gain_db_for(2.0 * fill.sigma_lsb_of_noise_dbfs(-40.0), -40.0),
                        20.0 * math.log10(2.0))
