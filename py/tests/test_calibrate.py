"""The `hackriff.calibration/1` generator (ADR-0015 §13.2, T-660 (b)).

Asserts the refusal is non-vacuous at the generation side too: an atom in the null distribution
must land in `unexpressible`, never rounded into `levels`, and the file the generator writes
parses as valid JSON matching the schema `crates/hk-synth/src/calibration.rs`'s loader reads.
"""

from __future__ import annotations

import json
import math

import numpy as np
import pytest

from hkpy import calibrate


def _noise_null(n: int, *, seed: int) -> np.ndarray:
    """A smooth, high-resolution null: every ask in `DEFAULT_ASK_BITS` up to the sample ceiling
    and the claim cap should be expressible."""
    rng = np.random.default_rng(seed)
    return rng.normal(loc=0.0, scale=1.0, size=n)


def test_a_smooth_null_expresses_every_level_up_to_its_two_ceilings() -> None:
    cell = calibrate.calibrate_cell(
        _noise_null(4096, seed=1), ask_bits=(1.0, 2.0, 3.0, 4.0, 6.0)
    )
    got_bits = {level["bits"] for level in cell["levels"]}
    assert got_bits == {1.0, 2.0, 3.0, 4.0, 6.0}
    assert cell["unexpressible"] == []
    assert cell["admissible_bits"] == 6.0
    # Levels ascend with threshold: more evidence needs a larger raw value.
    thresholds = [level["threshold"] for level in cell["levels"]]
    assert thresholds == sorted(thresholds)


def test_the_generator_refuses_an_atom_it_cannot_express_rather_than_rounding() -> None:
    # 4200 windows, all but 66 distinct values collapsed onto a handful of repeats -- the exact
    # defect docs/21 §5.2 measured for `eye_open` at n = 112: an atom sits on the mid-range
    # quantiles, so the 4- and 6-bit asks cannot land within tolerance.
    rng = np.random.default_rng(2)
    n = 4200
    fine = rng.normal(0.0, 1.0, size=n)
    # Collapse everything above the median onto one repeated value: every quantile deeper than
    # the median is now an atom, so a 4- or 6-bit ask (needing the top ~6% / ~1.5%) cannot be
    # expressed to within 0.25 bits.
    median = np.median(fine)
    atom_value = float(np.percentile(fine, 90))
    samples = np.where(fine > median, atom_value, fine)

    cell = calibrate.calibrate_cell(samples, ask_bits=(1.0, 2.0, 3.0, 4.0, 6.0))

    refused_bits = {u["bits"] for u in cell["unexpressible"]}
    assert 4.0 in refused_bits or 6.0 in refused_bits, cell
    for level in cell["levels"]:
        assert level["bits"] not in refused_bits
    for u in cell["unexpressible"]:
        assert u["reason"] in (
            "atom",
            "above_sample_ceiling",
            "above_calibrated_claim_cap",
        )
        # The realised significance recorded is never the claim itself when refused for being an
        # atom -- that would be silently granting the very level being refused.
        if u["reason"] == "atom":
            assert (
                abs(u["realised_bits"] - u["bits"])
                > calibrate.EXPRESSIBILITY_TOLERANCE_BITS
            )
    # admissible_bits never exceeds the best level actually published.
    assert cell["admissible_bits"] == max(
        (level["bits"] for level in cell["levels"]), default=0.0
    )


def test_a_level_above_the_calibrated_claim_cap_is_always_refused() -> None:
    # Even a perfectly smooth null (which could in principle express 8 or 10 bits at n = 4096)
    # must refuse anything above the 6-bit per-metric calibrated claim cap (§13.2 item 1).
    cell = calibrate.calibrate_cell(
        _noise_null(4096, seed=3), ask_bits=(6.0, 8.0, 10.0)
    )
    above_cap = [
        u
        for u in cell["unexpressible"]
        if u["bits"] > calibrate.CALIBRATED_CLAIM_CAP_BITS
    ]
    assert {u["bits"] for u in above_cap} == {8.0, 10.0}
    assert all(u["reason"] == "above_calibrated_claim_cap" for u in above_cap)
    assert cell["admissible_bits"] <= calibrate.CALIBRATED_CLAIM_CAP_BITS


def test_a_level_above_log2_windows_is_refused_as_the_sample_ceiling() -> None:
    # log2(64) = 6 exactly; asking for anything above the sample ceiling (here well below the
    # claim cap) must be attributed to the ceiling, not silently granted or mislabeled an atom.
    cell = calibrate.calibrate_cell(_noise_null(64, seed=4), ask_bits=(1.0, 5.0))
    assert math.isclose(cell["sample_ceiling_bits"], 6.0)
    ceiling_refusals = [
        u for u in cell["unexpressible"] if u["reason"] == "above_sample_ceiling"
    ]
    # 5 bits at n=64 needs ~2 windows in the tail; whether it lands in `levels` or is refused
    # depends on the exact draw, but nothing above the sample ceiling is ever published.
    for level in cell["levels"]:
        assert level["bits"] <= cell["sample_ceiling_bits"]
    for u in ceiling_refusals:
        assert u["bits"] > cell["sample_ceiling_bits"]


def test_calibrate_cell_refuses_fewer_than_two_windows() -> None:
    with pytest.raises(ValueError):
        calibrate.calibrate_cell([1.0])


def test_calibration_document_matches_the_rust_loaders_schema(tmp_path) -> None:
    cell = calibrate.calibrate_cell(_noise_null(4096, seed=5))
    table = {"metric": "eye_open", "null": "noise", "n": 112, **cell}
    doc = calibrate.calibration_document(
        block="clock_recovery@1", bucket="nominal", tables=[table]
    )
    assert doc["schema"] == "hackriff.calibration/1"
    assert doc["conditioning"]["bucket"] == "nominal"
    assert doc["conditioning"]["sigma_lsb_range"] == [0.5, 77.0]
    assert doc["tables"] == [table]
    assert "correlation" not in doc
    assert "groups" not in doc

    path = calibrate.write_calibration_file(tmp_path / "clock_recovery.json", doc)
    reloaded = json.loads(path.read_text())
    assert reloaded == doc


def test_declaring_groups_and_correlation_rides_along_when_given() -> None:
    doc = calibrate.calibration_document(
        block="fsk_demod@3",
        bucket="nominal",
        tables=[],
        correlation={
            "method": "spearman",
            "windows": 820,
            "metrics": ["snr", "evm"],
            "rho": [[1.0, 0.989], [0.989, 1.0]],
        },
        groups={"S2": {"soft_quality": ["snr", "evm"]}},
    )
    assert doc["correlation"]["metrics"] == ["snr", "evm"]
    assert doc["groups"]["S2"]["soft_quality"] == ["snr", "evm"]


def test_tight_nominal_bounds_are_available_but_not_the_default() -> None:
    # ADR-0015 §16.4 / docs/21 §10.10 (T-619): the tighter bucket is a deliberate opt-in via
    # `nominal_bounds=`, never a silent default the generator chooses on its own.
    doc = calibrate.calibration_document(
        block="am_demod@1", bucket="nominal", tables=[]
    )
    assert (
        doc["conditioning"]["sigma_lsb_range"][0]
        == calibrate.ADR_NOMINAL_BOUNDS["sigma_lsb_min"]
    )

    tight = calibrate.calibration_document(
        block="am_demod@1",
        bucket="nominal",
        tables=[],
        nominal_bounds=calibrate.TIGHT_NOMINAL_BOUNDS,
    )
    assert tight["conditioning"]["sigma_lsb_range"][0] == 1.0
    assert tight["conditioning"]["clip_fraction_max"] == 0.10


# --- T-853 (MAUTO M-2): draws -> document -------------------------------------------------------


def _draws(rho_pair: float, *, n: int = 256, min_n: int | None = None) -> dict:
    """Two S2 metrics declared in different groups, with a chosen dependence, over 4096 windows."""
    rng = np.random.default_rng(7)
    a = rng.normal(size=4096)
    b = rho_pair * a + math.sqrt(max(0.0, 1 - rho_pair**2)) * rng.normal(size=4096)
    return {
        "block": "clock_recovery@1",
        "corpus": "synthetic",
        "fill": {"sigma_lsb": 8.0, "clip_fraction": 0.0},
        "cells": [
            {
                "n": n,
                "min_n": n if min_n is None else min_n,
                "max_n": n + 3,
                "windows": 4096,
                "metrics": {
                    "eye_open": {
                        "stage": "S2",
                        "group": "eye",
                        "direction": "larger",
                        "raw": a.tolist(),
                    },
                    # Evidence when small: the generator negates it before calibrating.
                    "timing_var": {
                        "stage": "S2",
                        "group": "soft_quality",
                        "direction": "smaller",
                        "raw": (-b).tolist(),
                    },
                },
            }
        ],
    }


def test_independent_metrics_keep_their_declared_groups() -> None:
    doc = calibrate.document_from_draws(
        _draws(0.0), nominal_bounds=calibrate.TIGHT_NOMINAL_BOUNDS
    )
    assert doc["groups"] == {
        "S2": {"eye": ["eye_open"], "soft_quality": ["timing_var"]}
    }
    rho = doc["correlation"]["rho"]
    assert abs(rho[0][1]) < calibrate.GROUP_SPLIT_MAX_RHO
    # The tight bucket is stated in the file (ADR-0015 §16.4).
    assert doc["conditioning"]["sigma_lsb_range"][0] == 1.0
    assert doc["conditioning"]["clip_fraction_max"] == 0.10
    assert {t["metric"] for t in doc["tables"]} == {"eye_open", "timing_var"}
    assert all(t["null"] == "noise" and t["n"] == 256 for t in doc["tables"])


def test_dependent_metrics_are_merged_into_one_group_never_split() -> None:
    # Correlated in the evidence direction (timing_var negated back): one group.
    doc = calibrate.document_from_draws(
        _draws(0.9), nominal_bounds=calibrate.TIGHT_NOMINAL_BOUNDS
    )
    assert doc["correlation"]["rho"][0][1] >= calibrate.GROUP_SPLIT_MAX_RHO
    assert list(doc["groups"]["S2"].values()) == [["eye_open", "timing_var"]]


def test_a_cell_whose_draws_had_another_support_is_refused() -> None:
    with pytest.raises(ValueError, match="for a cell at n = 256"):
        calibrate.document_from_draws(
            _draws(0.0, min_n=250), nominal_bounds=calibrate.TIGHT_NOMINAL_BOUNDS
        )


def test_the_support_slack_mirrors_the_rust_scorer() -> None:
    # hk_synth::calibration::support_matches: n in [cell_n, cell_n + cell_n/50 + 16].
    assert calibrate.support_slack(256) == 21
    assert calibrate.support_slack(16384) == 343


def test_spearman_is_rank_based_and_a_constant_column_cannot_justify_a_split() -> None:
    x = np.arange(100.0)
    assert calibrate.spearman_matrix([x, x**3])[0][1] == 1.0
    assert calibrate.spearman_matrix([x, -x])[0][1] == -1.0
    assert calibrate.spearman_matrix([x, np.zeros(100)])[0][1] == 1.0


def test_the_shipped_tables_are_what_the_rust_loader_reads() -> None:
    from pathlib import Path

    root = Path(__file__).resolve().parents[2] / "synth" / "calibration"
    files = sorted(root.glob("*.json"))
    assert len(files) >= 11
    for f in files:
        doc = json.loads(f.read_text())
        assert doc["schema"] == calibrate.CALIBRATION_SCHEMA
        assert (
            doc["conditioning"]["sigma_lsb_range"][0]
            == calibrate.TIGHT_NOMINAL_BOUNDS["sigma_lsb_min"]
        )
        for t in doc["tables"]:
            assert t["windows"] >= calibrate.WINDOWS_PER_CELL_FLOOR
            assert t["admissible_bits"] <= calibrate.CALIBRATED_CLAIM_CAP_BITS
            assert all(
                abs(lv["realised_bits"] - lv["bits"])
                <= calibrate.EXPRESSIBILITY_TOLERANCE_BITS
                for lv in t["levels"]
            )
