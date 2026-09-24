"""N3 -- out-of-catalogue structure populations (T-623, ADR-0021 section 8.4, docs/22 section 4.3).

"REAL structure, no block": emissions with genuine structure the recipe catalogue has no block
for. The engine is expected to MEASURE the structure, name it and its ``missing_block``, and never
report ``solved``. CSS already exists (``lora_ism_burst``); this module adds the other three:

- ``ofdm_nonstandard_cp`` -- OFDM, 128-point FFT, 64 QPSK subcarriers, a real cyclic prefix of 19
  samples (ratio 0.148: not 1/4, 1/8, 1/16 or 1/32 of any standard).
- ``dsss_m_sequence`` -- BPSK data spread by a 31-chip m-sequence (x^5 + x^3 + 1), 5 samples/chip.
- ``qam16_unframed`` -- RRC-shaped Gray 16-QAM, no preamble, sync word or CRC.

Hidden truth records the structure kind and its parameters only (``negative_population``); nothing
in it is something a blind engine may read. G1 (docs/22 6.3): this is a held-out set of KNOWN
unknowns, not a sample of the unknown.
"""

from __future__ import annotations

import math
from typing import Any

import numpy as np

from hkpy.synth.scenarios import Ctx, DEFAULT_START_UTC, utc_plus
from hkpy.synth.scene import Scene, complex_noise, db, undb

_COMMON: dict[str, Any] = {
    "sample_rate": 500e3,
    "center_hz": 433.92e6,
    "duration_s": 0.4,
    "offset_hz": 0.0,
    "snr_db": 20.0,
    "noise_dbfs": -40.0,
    "start_s": 0.05,
    "calibration_k_db": -70.0,
    "start_utc": DEFAULT_START_UTC,
}

OFDM_DEFAULTS: dict[str, Any] = {**_COMMON, "fft_size": 128, "used_subcarriers": 64, "cp_samples": 19}
DSSS_DEFAULTS: dict[str, Any] = {**_COMMON, "offset_hz": 50e3, "chips_per_bit": 31,
                                 "samples_per_chip": 5, "n_bits": 400}
QAM16_DEFAULTS: dict[str, Any] = {**_COMMON, "offset_hz": -40e3, "symbol_rate_bd": 50e3,
                                  "rolloff": 0.35, "rrc_span_symbols": 10}

#: Gray map for one 16-QAM axis: 2 bits -> level.
_GRAY4 = {0b00: -3, 0b01: -1, 0b11: 1, 0b10: 3}


def m_sequence_31() -> np.ndarray:
    """One period of the length-31 m-sequence from x^5 + x^3 + 1, as +/-1 chips."""
    reg = [1, 0, 0, 0, 0]
    out = []
    for _ in range(31):
        out.append(reg[-1])
        fb = reg[-1] ^ reg[2]
        reg = [fb] + reg[:-1]
    return 1.0 - 2.0 * np.array(out, dtype=float)


def rrc_taps(beta: float, sps: int, span: int) -> np.ndarray:
    t = np.arange(-span * sps // 2, span * sps // 2 + 1) / sps
    h = np.empty_like(t)
    for i, x in enumerate(t):
        if abs(x) < 1e-12:
            h[i] = 1 + beta * (4 / math.pi - 1)
        elif beta > 0 and abs(abs(x) - 1 / (4 * beta)) < 1e-9:
            h[i] = beta / math.sqrt(2) * ((1 + 2 / math.pi) * math.sin(math.pi / (4 * beta))
                                          + (1 - 2 / math.pi) * math.cos(math.pi / (4 * beta)))
        else:
            h[i] = (math.sin(math.pi * x * (1 - beta)) + 4 * beta * x * math.cos(math.pi * x * (1 + beta))) \
                / (math.pi * x * (1 - (4 * beta * x) ** 2))
    return h / math.sqrt(np.sum(h ** 2))


def _ofdm(rng: np.random.Generator, p: dict[str, Any], n_out: int) -> tuple[np.ndarray, dict[str, Any]]:
    nfft, used, cp = int(p["fft_size"]), int(p["used_subcarriers"]), int(p["cp_samples"])
    bins = np.r_[np.arange(1, used // 2 + 1), np.arange(nfft - used // 2, nfft)]
    n_sym = -(-n_out // (nfft + cp))
    qpsk = np.array([1 + 1j, -1 + 1j, -1 - 1j, 1 - 1j]) / math.sqrt(2)
    syms = []
    for _ in range(n_sym):
        spec = np.zeros(nfft, dtype=complex)
        spec[bins] = qpsk[rng.integers(0, 4, len(bins))]
        body = np.fft.ifft(spec) * nfft / math.sqrt(used)
        syms.append(np.concatenate([body[-cp:], body]))
    iq = np.concatenate(syms)[:n_out]
    return iq / math.sqrt(np.mean(np.abs(iq) ** 2)), {
        "fft_size": nfft, "used_subcarriers": used, "cp_samples": cp,
        "cp_ratio": cp / nfft, "modulation_per_subcarrier": "qpsk",
        "subcarrier_spacing_hz": None, "n_symbols": n_sym,
        "occupied_fraction": used / nfft,
    }


def _dsss(rng: np.random.Generator, p: dict[str, Any]) -> tuple[np.ndarray, dict[str, Any]]:
    code = m_sequence_31()
    spc = int(p["samples_per_chip"])
    bits = rng.integers(0, 2, int(p["n_bits"]))
    chips = (np.repeat(1.0 - 2.0 * bits, len(code)) * np.tile(code, len(bits)))
    iq = np.repeat(chips, spc).astype(complex)
    return iq, {"code": "m-sequence x^5+x^3+1", "chips_per_bit": len(code),
                "code_chips": [int(c) for c in code], "samples_per_chip": spc,
                "data_modulation": "bpsk", "n_bits": len(bits), "data_bits": bits.tolist()}


def _qam16(rng: np.random.Generator, p: dict[str, Any], fs: float) -> tuple[np.ndarray, dict[str, Any]]:
    sps = int(round(fs / float(p["symbol_rate_bd"])))
    if abs(sps * float(p["symbol_rate_bd"]) - fs) > 1e-6:
        raise ValueError("sample_rate must be an integer multiple of symbol_rate_bd")
    n_sym = int(float(p["duration_s"]) * fs / sps) - 2 * int(p["rrc_span_symbols"])
    bits = rng.integers(0, 4, (n_sym, 2))
    sym = np.array([complex(_GRAY4[int(b[0])], _GRAY4[int(b[1])]) for b in bits]) / math.sqrt(10)
    up = np.zeros(n_sym * sps, dtype=complex)
    up[::sps] = sym
    iq = np.convolve(up, rrc_taps(float(p["rolloff"]), sps, int(p["rrc_span_symbols"])))
    return iq, {"constellation": "16-qam", "mapping": "gray, 2 bits per axis, I=high bits",
                "symbol_rate_bd": float(p["symbol_rate_bd"]), "rolloff": float(p["rolloff"]),
                "pulse": "root-raised-cosine", "n_symbols": n_sym, "framing": "none",
                "symbol_indices": (bits[:, 0] * 4 + bits[:, 1]).tolist()}


def _build(ctx: Ctx, name: str, kind: str, missing: str, gen: Any) -> tuple[list[Scene], dict[str, Any]]:
    p = ctx.params
    fs = float(p["sample_rate"])
    n = int(round(float(p["duration_s"]) * fs))
    scene = ctx.scene(name, fs, n, f"hkpy.synth {name}: N3 real-structure emission with no catalogue block ({kind})")
    cap = scene.add_capture(0, n, float(p["center_hz"]), utc_plus(p["start_utc"], 0.0),
                            calibration_k_db=float(p["calibration_k_db"]))
    cap.floor_dbfs_per_hz = float(p["noise_dbfs"]) - db(fs)
    scene.add_samples(0, complex_noise(scene.rng("noise"), n, float(p["noise_dbfs"])))
    scene.add_floor(0, n, cap.floor_dbfs_per_hz)

    iq, detail, bw = gen(scene, p, fs, n - int(round(float(p["start_s"]) * fs)) - 1)
    off = float(p["offset_hz"])
    if abs(off) + bw / 2 > fs / 2:
        raise ValueError(f"{name}: emission ({bw:.0f} Hz wide at {off:+.0f} Hz) leaves the window")
    start = int(round(float(p["start_s"]) * fs))
    iq = iq[: n - start]
    iq = iq / math.sqrt(np.mean(np.abs(iq) ** 2))
    power = float(p["snr_db"]) + cap.floor_dbfs_per_hz + db(bw)
    tt = scene.time(start, len(iq))
    scene.add_samples(start, math.sqrt(undb(power)) * iq * np.exp(2j * math.pi * off * tt))
    f = cap.center_hz + off
    truth = scene.emission_truth(
        cap, off, bw, power, kind=kind, modulation=kind, family_hint=kind,
        negative_population="N3", structure=detail, expected_verdict_ceiling="not-solved",
        missing_block=missing, in_catalogue=False)
    scene.annotate(start, len(iq), f - bw / 2, f + bw / 2, kind, truth)
    scene.scenario_truth["negative_population"] = {
        "population": "N3", "structure_kind": kind, "missing_block": missing,
        "parameters": {k: v for k, v in detail.items() if not isinstance(v, list)},
    }
    return [scene], {}


def ofdm_nonstandard_cp(ctx: Ctx) -> tuple[list[Scene], dict[str, Any]]:
    def gen(scene: Scene, p: dict[str, Any], fs: float, n: int):
        iq, d = _ofdm(scene.rng("ofdm"), p, n)
        d["subcarrier_spacing_hz"] = fs / int(p["fft_size"])
        return iq, d, fs * int(p["used_subcarriers"]) / int(p["fft_size"])
    return _build(ctx, "ofdm_nonstandard_cp", "ofdm", "ofdm-demod-nonstandard-cp", gen)


def dsss_m_sequence(ctx: Ctx) -> tuple[list[Scene], dict[str, Any]]:
    def gen(scene: Scene, p: dict[str, Any], fs: float, n: int):
        iq, d = _dsss(scene.rng("dsss"), p)
        d["chip_rate_hz"] = fs / int(p["samples_per_chip"])
        return iq, d, fs / int(p["samples_per_chip"])
    return _build(ctx, "dsss_m_sequence", "dsss", "dsss-despread", gen)


def qam16_unframed(ctx: Ctx) -> tuple[list[Scene], dict[str, Any]]:
    def gen(scene: Scene, p: dict[str, Any], fs: float, n: int):
        iq, d = _qam16(scene.rng("qam"), p, fs)
        return iq, d, float(p["symbol_rate_bd"]) * (1 + float(p["rolloff"]))
    return _build(ctx, "qam16_unframed", "qam16", "qam-demod-16", gen)
