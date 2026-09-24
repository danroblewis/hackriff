"""T-623: N3 structure generators are genuinely structured and truth-annotated."""
import math

import numpy as np

from hkpy.synth import structures
from hkpy.synth.scenarios import Ctx  # noqa: F401
from hkpy.synth import SCENARIOS, generate


def test_registered_and_generate(tmp_path):
    for name in ("ofdm_nonstandard_cp", "dsss_m_sequence", "qam16_unframed"):
        assert name in SCENARIOS
        assert "SIGNAL-052" in SCENARIOS[name].use_cases
        generate(name, 1, tmp_path / name)
        assert list((tmp_path / name).glob("*.sigmf-meta"))


def test_ofdm_cp_is_real():
    rng = np.random.default_rng(0)
    p = structures.OFDM_DEFAULTS
    iq, d = structures._ofdm(rng, p, 20000)
    n, cp = d["fft_size"], d["cp_samples"]
    sym = iq[: n + cp]
    assert np.allclose(sym[:cp], sym[n:n + cp] * 1, atol=1e-9 * 100) or abs(
        np.vdot(sym[:cp], sym[n:]) ) / np.vdot(sym[:cp], sym[:cp]).real > 0.99
    assert d["cp_ratio"] not in (1 / 4, 1 / 8, 1 / 16, 1 / 32)


def test_dsss_despreads():
    rng = np.random.default_rng(0)
    iq, d = structures._dsss(rng, structures.DSSS_DEFAULTS)
    chips = iq.real[:: d["samples_per_chip"]]
    code = structures.m_sequence_31()
    got = [int(np.dot(chips[i * 31:(i + 1) * 31], code) < 0) for i in range(len(d["data_bits"]))]
    assert got == d["data_bits"]
    assert abs(np.dot(code, np.roll(code, 3))) == 1  # m-sequence autocorrelation


def test_qam16_has_16_levels():
    rng = np.random.default_rng(0)
    iq, d = structures._qam16(rng, structures.QAM16_DEFAULTS, 500e3)
    assert d["framing"] == "none"
    assert len(set(d["symbol_indices"])) == 16
    assert math.isfinite(float(np.mean(np.abs(iq))))
