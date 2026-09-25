"""T-863 (MAUTO M-12): the ADR-0015 section 7 generic FSK/OOK sweep generator.

The acceptance suite (`tests/e2e/tests/acceptance/mauto_eval.rs`) trusts three things about this
generator, and each is checked here rather than assumed: (1) every draw lies inside section 7's
population (rate, sync length, catalogue CRC-8/16, CFO bound); (2) the truth is *correct* - the
frame bits annotated are what was modulated, and each frame's check verifies under the
parameters truth names; (3) `deepest_achievable` follows its a-priori rule.
"""
import json
import math

import numpy as np
import pytest

from hkpy.synth import SCENARIOS, fsk, generate, generic_fsk
from hkpy.synth.scenarios import Ctx


def _truth(tmp_path, seed, **params):
    out = tmp_path / f"s{seed}-{abs(hash(tuple(sorted(params.items()))))}"
    manifest = json.loads(generate("generic_fsk_sweep", seed, out, params).read_text())
    meta = json.loads((out / manifest["recordings"][0]).read_text())
    scenario = next(a for a in meta["annotations"] if a["core:label"] == "scenario")
    frames = [a for a in meta["annotations"] if a["core:label"] == "generic-fsk-frame"]
    return meta, scenario["hackriff:truth"]["generic_fsk"], frames


def _hex_bits(hex_text, n):
    raw = np.unpackbits(np.frombuffer(bytes.fromhex(hex_text), dtype=np.uint8))
    return raw[:n]


def test_registered_with_its_use_cases():
    spec = SCENARIOS["generic_fsk_sweep"]
    assert "RESEARCH-002" in spec.use_cases and "SIGNAL-052" in spec.use_cases


def test_draws_stay_inside_the_section_7_population(tmp_path):
    seen_mod = set()
    for seed in range(1, 41):
        rng = np.random.default_rng(seed)
        d = generic_fsk.draw(rng, dict(generic_fsk.GENERIC_FSK_DEFAULTS))
        seen_mod.add(d["modulation"])
        assert generic_fsk.RATE_MIN_BD <= d["symbol_rate_bd"] <= generic_fsk.RATE_MAX_BD
        assert generic_fsk.SYNC_BITS_MIN <= d["sync_bits"] <= generic_fsk.SYNC_BITS_MAX
        assert abs(d["cfo_hz"]) <= generic_fsk.CFO_FRAC_MAX * d["bandwidth_hz"] + 1e-9
        assert d["check"] == "catalogue" and d["check_width"] in (8, 16)
    assert seen_mod == {"2fsk", "ook"}


def test_same_seed_is_the_same_scene_and_snr_changes_only_the_level(tmp_path):
    m1, t1, _ = _truth(tmp_path, 7, snr_db=20.0)
    _, t2, _ = _truth(tmp_path, 7, snr_db=20.0)
    _, t6, _ = _truth(tmp_path, 7, snr_db=6.0)
    assert t1 == t2
    # A paired design: the SNR axis moves nothing else, so a rate difference between SNR levels
    # is the SNR's and not a different population's.
    for k in ("modulation", "symbol_rate_bd", "sync_hex", "check", "cfo_hz", "n_frames"):
        assert t1[k] == t6[k], k
    assert t6["snr_db"] == 6.0


def test_the_hidden_frames_are_what_was_modulated_and_each_check_verifies(tmp_path):
    for seed in (1, 2, 3, 4, 5, 6):
        _, t, frames = _truth(tmp_path, seed)
        assert len(frames) == t["n_frames"] >= 1
        chk = t["check"]
        width = chk["width"]
        params = {"width": width, "poly": int(chk["poly"], 16), "init": int(chk["init"], 16),
                  "refin": chk["refin"], "refout": chk["refout"], "xorout": int(chk["xorout"], 16)}
        assert chk["in_reveng_catalogue"]
        assert fsk.crc_catalogue_name(**params) == chk["catalogue_name"]
        payloads = set()
        for a in frames:
            fr = a["hackriff:truth"]["frame"]
            bits = _hex_bits(fr["bits_hex"], fr["n_bits"])
            pre, sync = t["preamble_bits"], t["sync_bits"]
            assert "".join(map(str, bits[pre:pre + sync])) == t["sync_bitstring"]
            payload = bytes.fromhex(fr["payload_hex"])
            payloads.add(payload)
            body = bits[pre + sync:pre + sync + 8 * len(payload)]
            assert np.packbits(body).tobytes() == payload
            check = int("".join(map(str, bits[-width:])), 2)
            assert check == fsk.crc_generic(payload, **params)
        # Every frame is a distinct piece of evidence (ADR-0022 section 4.2).
        assert len(payloads) == len(frames)


def test_the_emission_is_where_truth_says_at_the_stated_snr(tmp_path):
    for seed, mod in ((11, "2fsk"), (12, "ook")):
        meta, t, frames = _truth(tmp_path, seed, modulation=mod, snr_db=20.0)
        a = frames[0]
        assert a["core:freq_lower_edge"] < t["rf_center_hz"] < a["core:freq_upper_edge"]
        assert a["hackriff:truth"]["snr_db"] == pytest.approx(20.0, abs=1e-6)
        expected_bw = (2 * t["deviation_hz"] + t["symbol_rate_bd"] if mod == "2fsk"
                       else 2 * t["symbol_rate_bd"])
        assert t["bandwidth_hz"] == pytest.approx(expected_bw)


def test_ook_is_on_off_keyed():
    bits = np.array([1, 0, 1, 1, 0], dtype=np.uint8)
    iq = generic_fsk.ook(bits, 10_000.0, 1_000.0)
    assert len(iq) == 50
    assert np.all(np.abs(iq[10:20]) == 0) and np.all(np.abs(iq[20:40]) == 1)


def test_sync_words_are_findable():
    rng = np.random.default_rng(3)
    for n in (16, 20, 24, 32):
        for _ in range(20):
            w = generic_fsk.draw_sync(rng, n)
            assert len(w) == n
            assert abs(2 * int(w.sum()) - n) <= n // 4
            assert generic_fsk.autocorrelation_sidelobe(w) <= n // 3
            assert not np.all(w[1:8] != w[:7])
    with pytest.raises(ValueError):
        generic_fsk.draw_sync(rng, 12)


def test_random_poly_is_off_catalogue_and_none_has_no_check(tmp_path):
    _, t, _ = _truth(tmp_path, 21, check="random-poly")
    assert t["check_kind"] == "random-poly" and not t["check"]["in_reveng_catalogue"]
    poly = int(t["check"]["poly"], 16)
    assert poly & 1
    assert not any(v[0] == t["check"]["width"] and v[1] == poly for v in fsk.CRC_CATALOGUE.values())
    _, tn, frames = _truth(tmp_path, 22, check="absent")
    assert tn["check"] is None and tn["deepest_achievable"] == "framed"
    assert all(a["hackriff:truth"]["frame"]["check_hex"] is None for a in frames)


def test_deepest_achievable_follows_the_a_priori_rule(tmp_path):
    # A slow rate cannot fit three whole frames into the hold-out: framed, and truth says why.
    _, slow, _ = _truth(tmp_path, 31, symbol_rate_bd=300.0)
    assert slow["holdout_frames"] < generic_fsk.MIN_HOLDOUT_FRAMES
    assert slow["deepest_achievable"] == "framed"
    assert "hold-out" in slow["deepest_achievable_reason"]
    _, fast, _ = _truth(tmp_path, 31, symbol_rate_bd=20_000.0)
    assert fast["holdout_frames"] >= generic_fsk.MIN_HOLDOUT_FRAMES
    assert fast["deepest_achievable"] == "solved"
    holdout_from = generic_fsk.SEARCH_FRACTION * generic_fsk.GENERIC_FSK_DEFAULTS["duration_s"]
    assert fast["holdout_frames"] == sum(1 for f in fast["frames"] if f["t_start_s"] >= holdout_from)


def test_the_scene_fits_one_analyze_window():
    # hk_pipeline::synth::jobs::MAX_WINDOW_NS = 2 s: a job about the emitter reads all of it.
    assert generic_fsk.GENERIC_FSK_DEFAULTS["duration_s"] <= 2.0
    for rate in (300.0, 1_000.0, 50_000.0):
        pb, n, period = generic_fsk.frame_plan(rate, 2.0, 10, 0.35, 16 + 32 + 16, 8)
        assert 2 <= pb <= 8 and n >= 1
        assert n * period <= 2.0 + 1e-9
        assert (16 + 32 + 16 + 8 * pb) / rate <= period


def test_bad_parameters_are_refused():
    ctx_params = dict(generic_fsk.GENERIC_FSK_DEFAULTS)
    with pytest.raises(ValueError):
        generic_fsk.draw(np.random.default_rng(0), {**ctx_params, "modulation": "psk"})
    with pytest.raises(ValueError):
        generic_fsk.draw(np.random.default_rng(0), {**ctx_params, "check": "bch"})
    ctx = Ctx("generic_fsk_sweep", 1, {**ctx_params, "channel_offset_frac": 0.49,
                                       "symbol_rate_bd": 50_000.0, "modulation": "ook"},
              "ci8", [])
    with pytest.raises(ValueError):
        generic_fsk.generic_fsk_sweep(ctx)
    assert math.isfinite(generic_fsk.RATE_MAX_BD)
