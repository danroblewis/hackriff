"""N5 — the mismatched-hypothesis negative population (T-626, docs/22 §4.3).

What these tests establish, blind: the scene really does present a **real, framed, CRC-valid**
emitter whose parameters no proposal-grid hypothesis can reach, so the engine necessarily scores
wrong-parameter hypotheses; the adjacent variant really does leak into a box it never occupies;
and the truth carries enough for an assert harness to tell a **mismatch** from a **miss** without
the system ever seeing a truth value.

Everything measured here is measured from the samples with an independent reference (a
discriminator for the symbol rate, `binascii.crc_hqx` for the CRC, Welch for the leak), never
read out of the truth block and re-asserted against itself. Counts and states, never wall-clock.
"""

from __future__ import annotations

import binascii
import json
from pathlib import Path

import numpy as np
import pytest
from scipy import signal

from hkpy import sigmf
from hkpy.synth import generate
from hkpy.synth import mismatch as mm

SMALL = {"duration_s": 0.3}


def load(manifest: Path):
    man = json.loads(manifest.read_text())
    meta_path = manifest.parent / man["recordings"][0]
    meta = sigmf.read_meta(meta_path)
    raw = np.frombuffer(sigmf.data_path(meta_path).read_bytes(), dtype=np.int8).astype(np.float64)
    a = raw / 127.0
    return meta, a[0::2] + 1j * a[1::2]


def scenario_truth(meta):
    return next(
        a[sigmf.TRUTH_KEY]
        for a in meta["annotations"]
        if a[sigmf.TRUTH_KEY]["role"] == "scenario"
    )


def emissions(meta, kind):
    return [a for a in meta["annotations"] if a[sigmf.TRUTH_KEY].get("kind") == kind]


@pytest.fixture(scope="module")
def off_grid(tmp_path_factory):
    d = tmp_path_factory.mktemp("n5_off_grid")
    return load(generate("mismatched_hypothesis", 626, d, SMALL, "ci8"))


@pytest.fixture(scope="module")
def adjacent(tmp_path_factory):
    d = tmp_path_factory.mktemp("n5_adjacent")
    return load(
        generate(
            "mismatched_hypothesis", 626, d, {**SMALL, "population": "adjacent_leakage"}, "ci8"
        )
    )


# ---- the population is off the grid, by construction and by measurement ----------------------


def test_no_grid_hypothesis_can_reach_the_true_parameters(off_grid) -> None:
    """The defining property. Counted over the whole grid, not argued from the nearest point:
    **zero** of the proposal rates and **zero** of the proposal indices are within reach."""
    meta, _ = off_grid
    n5 = scenario_truth(meta)["negative_population"]
    rate = n5["off_grid"]["symbol_rate_bd"]
    h = n5["off_grid"]["mod_index"]

    reachable_rates = [
        g for g in mm.PROPOSAL_SYMBOL_RATES_BD if abs(rate - g) / g <= mm.OFF_GRID_MIN_DISTANCE
    ]
    reachable_h = [
        g for g in mm.PROPOSAL_MOD_INDICES if abs(h - g) / g <= mm.OFF_GRID_MIN_DISTANCE
    ]
    assert len(reachable_rates) == 0, reachable_rates
    assert len(reachable_h) == 0, reachable_h
    assert len(mm.PROPOSAL_SYMBOL_RATES_BD) == 8 and len(mm.PROPOSAL_MOD_INDICES) == 3
    assert n5["off_grid"]["symbol_rate_grid_distance"] > mm.OFF_GRID_MIN_DISTANCE
    assert n5["off_grid"]["mod_index_grid_distance"] > mm.OFF_GRID_MIN_DISTANCE
    assert n5["id"] == "N5" and n5["shape"] == "off_grid"


def test_the_emitter_is_real_framed_and_at_the_off_grid_rate(off_grid) -> None:
    """A mismatched-hypothesis null is only a null for the *parameters*: the signal itself is a
    genuine framed emission. Measured back out of the samples, not taken from the truth."""
    meta, x = off_grid
    n5 = scenario_truth(meta)["negative_population"]
    bursts = emissions(meta, "fsk-burst")
    assert len(bursts) == n5["n_bursts"] >= 2

    fs = meta["global"]["core:sample_rate"]
    ann = bursts[0]
    t = ann[sigmf.TRUTH_KEY]
    seg = x[ann["core:sample_start"] : ann["core:sample_start"] + ann["core:sample_count"]]
    # Mix to baseband and filter to the emission's own bandwidth before discriminating: the
    # discriminator of a 4 kHz signal in 500 kHz of noise measures the noise.
    seg = seg * np.exp(-2j * np.pi * t["offset_hz"] * np.arange(len(seg)) / fs)
    taps = signal.firwin(255, t["bandwidth_hz"] / 2, fs=fs)
    seg = signal.lfilter(taps, 1.0, seg)[len(taps) // 2 :]
    inst = np.angle(seg[1:] * np.conj(seg[:-1])) * fs / (2 * np.pi)
    # The preamble is 1010..., one transition per symbol, so zero crossings per second ARE the
    # symbol rate. Measured, not read out of the truth.
    sps = fs / t["symbol_rate_bd"]
    pre = inst[: int(round(24 * sps))]
    crossings = int(np.sum(np.diff(np.sign(pre)) != 0))
    measured_rate = crossings / (len(pre) / fs)
    measured_dev = float(np.percentile(np.abs(pre), 75))
    assert abs(measured_rate - t["symbol_rate_bd"]) / t["symbol_rate_bd"] < 0.02, measured_rate
    assert abs(measured_dev - t["deviation_hz"]) / t["deviation_hz"] < 0.10, measured_dev

    # The MEASURED parameters are themselves unreachable from the grid — the property is in the
    # samples, not only in the truth block.
    _, rate_distance = mm.grid_distance(measured_rate, mm.PROPOSAL_SYMBOL_RATES_BD)
    _, h_distance = mm.grid_distance(
        2 * measured_dev / measured_rate, mm.PROPOSAL_MOD_INDICES
    )
    assert rate_distance > mm.OFF_GRID_MIN_DISTANCE, rate_distance
    assert h_distance > mm.OFF_GRID_MIN_DISTANCE, h_distance

    # Real framing: the CRC in the truth is valid under an independent implementation.
    payload = bytes.fromhex(t["frame"]["payload_hex"])
    assert binascii.crc_hqx(payload, 0xFFFF) == int(t["frame"]["crc_hex"], 16)
    assert t["crc"]["valid"] is True and t["in_analysed_box"] is True


# ---- the adjacent-leakage shape ---------------------------------------------------------------


def test_the_adjacent_emitter_is_outside_the_box_and_measurably_inside_it(adjacent, off_grid) -> None:
    """The second half of N5: a strong neighbour the box never contains, whose skirts it does.
    Any parameter bound from it is a mismatch by construction."""
    meta, x = adjacent
    n5 = scenario_truth(meta)["negative_population"]
    box, leak = n5["analysed_box"], n5["adjacent_leakage"]
    assert n5["shape"] == "adjacent_leakage" and leak is not None

    # State: outside the box, and not merely by an edge.
    assert not (box["f_lo_hz"] <= leak["rf_center_hz"] <= box["f_hi_hz"])
    assert abs(leak["rf_center_hz"] - box["rf_center_hz"]) > 3 * box["bandwidth_hz"]
    interferers = emissions(meta, "adjacent-interferer")
    assert len(interferers) == 1
    assert interferers[0][sigmf.TRUTH_KEY]["in_analysed_box"] is False
    assert interferers[0][sigmf.TRUTH_KEY]["leaks_into_box"] is True

    # Measurement, as an A/B against the control: the same scene WITHOUT the neighbour is the
    # off-grid fixture, so the in-box level of one against the other is the leak and nothing else.
    # Taken in a quiet stretch before the first burst, so the target emitter is not in either.
    fs = meta["global"]["core:sample_rate"]
    centre = meta["captures"][0]["core:frequency"]

    def in_box_power(samples):
        f, pxx = signal.welch(
            samples[: int(0.02 * fs)], fs=fs, nperseg=4096, return_onesided=False, detrend=False
        )
        f, pxx = np.fft.fftshift(f), np.fft.fftshift(pxx)
        sel = (f + centre >= box["f_lo_hz"]) & (f + centre <= box["f_hi_hz"])
        assert sel.sum() > 4
        return float(np.mean(pxx[sel]))

    margin_db = 10 * np.log10(in_box_power(x) / in_box_power(off_grid[1]))
    assert margin_db > 6.0, f"the neighbour does not reach the box: {margin_db:.1f} dB"
    assert leak["leak_margin_over_floor_db"] > 6.0
    # The generator's own measurement and this independent A/B agree on the size of the leak.
    assert abs(margin_db - leak["leak_margin_over_floor_db"]) < 4.0, (
        margin_db,
        leak["leak_margin_over_floor_db"],
    )


def test_both_shapes_exist_and_differ_only_in_the_neighbour(off_grid, adjacent) -> None:
    a, b = scenario_truth(off_grid[0]), scenario_truth(adjacent[0])
    assert {a["negative_population"]["shape"], b["negative_population"]["shape"]} == set(
        mm.POPULATIONS
    )
    assert a["negative_population"]["off_grid"] == b["negative_population"]["off_grid"]
    assert a["negative_population"]["adjacent_leakage"] is None
    assert len(emissions(off_grid[0], "adjacent-interferer")) == 0
    assert len(emissions(adjacent[0], "adjacent-interferer")) == 1


# ---- the harness rule: a mismatch is not a miss ------------------------------------------------


def test_truth_tells_a_mismatch_from_a_miss_without_the_system_seeing_it(off_grid) -> None:
    """The assertion N5 exists to make: `unknown`/`tied` or a correct partial are fine; a label
    with wrong parameters is the over-claim, and a grid-snapped one is the aggravating case."""
    meta, _ = off_grid
    n5 = scenario_truth(meta)["negative_population"]
    true_rate = n5["off_grid"]["symbol_rate_bd"]

    # The engine answering the hypothesis it had rather than the signal it saw.
    snapped = mm.classify_outcome(
        {"verdict": "solved", "symbol_rate_bd": 2400.0,
         "resolution": {"kind": "solved", "reason": None}},
        n5,
    )
    assert snapped["outcome"] == "mismatch" and snapped["grid_snapped"] is True

    # Wrong, but not a grid point: still a mismatch, and the distinction is recorded.
    off = mm.classify_outcome({"verdict": "framed", "symbol_rate_bd": 3000.0}, n5)
    assert off["outcome"] == "mismatch" and off["grid_snapped"] is False

    # The right answer, and the near-miss inside tolerance that must still read as right.
    for rate in (true_rate, true_rate * 1.01):
        assert mm.classify_outcome({"verdict": "solved", "symbol_rate_bd": rate}, n5)[
            "outcome"
        ] == "correct"

    # The WANTED answers.
    assert mm.classify_outcome(
        {"verdict": "clocked", "resolution": {"kind": "unknown", "reason": "tied"}}, n5
    )["outcome"] == "abstain"
    assert mm.classify_outcome({"verdict": "clocked", "symbol_rate_bd": 2400.0}, n5)[
        "outcome"
    ] == "partial"
    assert mm.classify_outcome(None, n5)["outcome"] == "abstain"

    # Four outcomes, and a miss is never counted as a false label: N5 measures over-claim, not
    # recall, and conflating them is how a suite reports a comfortable margin by finding nothing.
    outcomes = {
        mm.classify_outcome(r, n5)["outcome"]
        for r in (
            {"verdict": "solved", "symbol_rate_bd": 2400.0},
            {"verdict": "solved", "symbol_rate_bd": true_rate},
            {"verdict": "energy"},
            {"verdict": "framed", "resolution": {"kind": "unknown", "reason": "tied"}},
        )
    }
    assert outcomes == {"mismatch", "correct", "partial", "abstain"}


def test_the_truth_block_states_what_the_population_is_for(off_grid) -> None:
    n5 = scenario_truth(off_grid[0])["negative_population"]
    assert "solved" in n5["must_never_return"]
    assert "unknown/tied" in n5["acceptable"]
    assert n5["mismatch_vs_miss"]["rule"] == "hkpy.synth.mismatch.classify_outcome"
    assert n5["mismatch_vs_miss"]["symbol_rate_tolerance_frac"] == mm.SYMBOL_RATE_TOLERANCE
    assert any("false_labels" in c for c in n5["counted_in"])


def test_an_adjacent_emitter_inside_the_box_is_refused(tmp_path) -> None:
    """A neighbour *inside* the box would be two overlapping emissions — the thing the signal
    model calls an error signal — not adjacent-channel leakage. The generator refuses it rather
    than quietly producing a different population."""
    with pytest.raises(ValueError, match="OUTSIDE the analysed box"):
        generate(
            "mismatched_hypothesis",
            626,
            tmp_path,
            {**SMALL, "population": "adjacent_leakage", "adjacent_offset_hz": -40e3},
            "ci8",
        )
    with pytest.raises(ValueError, match="population must be one of"):
        generate("mismatched_hypothesis", 626, tmp_path, {**SMALL, "population": "noise"}, "ci8")
