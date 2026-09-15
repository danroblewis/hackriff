"""Self-consistency of hkpy.synth: files are valid SigMF, generation is deterministic, and the
``hackriff:truth`` annotations describe what is actually in the samples. Reference decoders here are
written independently of the generator's encoders where practical (stdlib CRC, own RDS block sync,
own PPM demodulator and CPR decoder)."""

from __future__ import annotations

import binascii
import json
import math
import re
import shutil
import subprocess
from collections import Counter
from pathlib import Path

import numpy as np
import pytest
from scipy import signal

from hkpy import sigmf
from hkpy.synth import SCENARIOS, ParamError, generate, resolve_params
from hkpy.synth import acars as acars_mod
from hkpy.synth import adsb as adsb_mod
from hkpy.synth import fsk as fsk_mod
from hkpy.synth import pocsag as pocsag_mod
from hkpy.synth.__main__ import main as cli_main

#: Small parameter sets so the suite stays fast.
SMALL: dict[str, dict] = {
    "tone": {},
    "fsk_burst_train": {"duration_s": 0.3},
    "noise_floor_rise": {"duration_s": 0.1, "t0_s": 0.05},
    "injected_floor": {"segment_duration_s": 0.02},
    "occupancy_multi_hour": {"hours": 1.0, "windows": 1, "window_duration_s": 0.1},
    "fm_broadcast_rds": {},
    "adsb_squitter": {"duration_s": 0.1, "messages_per_aircraft": 4},
    "pocsag_pagers": {},
    "acars_message": {"prekey_s": 0.02, "text": "TEST"},
}


# ---- helpers --------------------------------------------------------------------------------


def load(manifest: Path, index: int = 0):
    man = json.loads(manifest.read_text())
    meta_path = manifest.parent / man["recordings"][index]
    meta = sigmf.read_meta(meta_path)
    raw = sigmf.data_path(meta_path).read_bytes()
    if meta["global"]["core:datatype"] == "ci8":
        a = np.frombuffer(raw, dtype=np.int8).astype(np.float64) / 127.0
    else:
        a = np.frombuffer(raw, dtype="<f4").astype(np.float64)
    return man, meta, a[0::2] + 1j * a[1::2]


def truths(meta, role=None, kind=None):
    out = []
    for ann in meta["annotations"]:
        t = ann[sigmf.TRUTH_KEY]
        if (role is None or t["role"] == role) and (kind is None or t["kind"] == kind):
            out.append((ann, t))
    return out


def scenario_truth(meta):
    [(_, t)] = truths(meta, role="scenario")
    return t


def db(v):
    return 10 * math.log10(v)


def tone_power_dbfs(x, fs, offset_hz, start=0):
    """Power of a CW component at an exactly known frequency (coherent projection)."""
    t = (start + np.arange(len(x))) / fs
    return db(abs(np.mean(x * np.exp(-2j * math.pi * offset_hz * t))) ** 2)


def median_floor_dbfs(x, fs, nperseg=1024):
    """Total noise power over fs from the median Welch PSD bin (robust to narrowband signals)."""
    _, psd = signal.welch(x, fs=fs, nperseg=nperseg, return_onesided=False, scaling="density",
                          detrend=False)
    return db(np.median(psd) * fs)


def gen(tmp_path, scenario, seed=1, datatype="ci8", **params):
    return generate(scenario, seed, tmp_path / f"{scenario}-{seed}-{datatype}", {**SMALL[scenario], **params},
                    datatype)


# ---- structure, determinism, CLI ---------------------------------------------------------------


@pytest.mark.parametrize("scenario", sorted(SCENARIOS))
def test_scenario_writes_valid_sigmf_with_truth(tmp_path, scenario):
    manifest = gen(tmp_path, scenario)
    man = json.loads(manifest.read_text())
    assert man["scenario"] == scenario and man["recordings"]
    assert man["use_cases"] == list(SCENARIOS[scenario].use_cases)
    for i in range(len(man["recordings"])):
        _, meta, x = load(manifest, i)
        glob = meta["global"]
        assert glob[sigmf.PROVENANCE_KEY]["timestamp_method"] == "synthetic"
        st = scenario_truth(meta)
        assert st["n_samples"] == len(x)
        assert st["seed"] == 1 and st["scenario"] == scenario
        for ann, t in truths(meta):
            assert t["role"] in {"scenario", "emission", "artefact", "floor", "event"}
            assert ann["core:sample_start"] + ann["core:sample_count"] <= len(x)
            assert ann["core:freq_lower_edge"] <= ann["core:freq_upper_edge"]
        assert truths(meta, role="floor"), "every scenario states its noise floor"


def test_generation_is_deterministic(tmp_path):
    a = generate("fsk_burst_train", 7, tmp_path / "a", SMALL["fsk_burst_train"])
    b = generate("fsk_burst_train", 7, tmp_path / "b", SMALL["fsk_burst_train"])
    c = generate("fsk_burst_train", 8, tmp_path / "c", SMALL["fsk_burst_train"])
    name = "fsk_burst_train.sigmf-data"
    assert (a.parent / name).read_bytes() == (b.parent / name).read_bytes()
    assert (a.parent / "fsk_burst_train.sigmf-meta").read_text() == (b.parent / "fsk_burst_train.sigmf-meta").read_text()
    assert (a.parent / name).read_bytes() != (c.parent / name).read_bytes()


def test_cf32_matches_ci8_within_quantisation(tmp_path):
    _, _, xi = load(gen(tmp_path, "tone"))
    _, meta, xf = load(gen(tmp_path, "tone", datatype="cf32_le"))
    assert meta["global"]["core:datatype"] == "cf32_le"
    assert np.max(np.abs(xi.real - xf.real)) <= 0.5 / 127 + 1e-6
    assert scenario_truth(meta)["quantisation_noise_dbfs"] is None


def test_cli_coerces_params(tmp_path, capsys):
    out = tmp_path / "cli"
    rc = cli_main(["fsk_burst_train", "--seed", "3", "--out", str(out), "--param", "duration_s=0.2",
                   "--param", "sensor_id=0x1234", "--param", "dc_offset_dbfs=-30",
                   "--param", "blocker_offsets_hz=100e3,120e3", "--param", "spur_dbfs=none"])
    assert rc == 0
    man = json.loads(Path(capsys.readouterr().out.strip()).read_text())
    assert man["params"]["sensor_id"] == 0x1234
    assert man["params"]["blocker_offsets_hz"] == [100e3, 120e3]
    assert man["params"]["spur_dbfs"] is None
    assert resolve_params("fm_broadcast_rds", {"stereo": "false"})["stereo"] is False
    assert resolve_params("adsb_squitter", {"icao": "123456,abcdef"})["icao"] == ["123456", "abcdef"]
    with pytest.raises(ParamError, match="unknown parameter"):
        resolve_params("tone", {"nope": 1})
    assert cli_main(["tone", "--out", str(out), "--param", "nope=1"]) == 2


# ---- tone, floors, impairments ---------------------------------------------------------------


def test_tone_frequency_power_and_floor(tmp_path):
    _, meta, x = load(gen(tmp_path, "tone"))
    fs = meta["global"]["core:sample_rate"]
    fc = meta["captures"][0]["core:frequency"]
    [(_, t)] = truths(meta, kind="cw")
    n = len(x)
    spec = np.abs(np.fft.fft(x * np.hanning(n))) ** 2
    k = int(np.argmax(spec))
    a, b, c = np.log(spec[k - 1]), np.log(spec[k]), np.log(spec[(k + 1) % n])
    f_meas = (k + 0.5 * (a - c) / (a - 2 * b + c)) * fs / n
    f_meas = f_meas - fs if f_meas > fs / 2 else f_meas
    assert abs(fc + f_meas - t["center_hz"]) < 0.1 * fs / n
    assert abs(tone_power_dbfs(x, fs, t["offset_hz"]) - t["power_dbfs"]) < 0.2
    [(_, fl)] = truths(meta, role="floor")
    assert abs(median_floor_dbfs(x, fs) - fl["expected_floor_dbfs"]) < 0.3
    assert fl["floor_dbm"] == pytest.approx(fl["floor_dbfs"] + fl["calibration_k_db"])


def test_impairments_match_truth(tmp_path):
    manifest = gen(tmp_path, "tone", center_hz=100.2e6, offset_hz=150e3, spur_dbfs=-45.0,
                   dc_offset_dbfs=-38.0, iq_gain_db=0.5, iq_phase_deg=3.0, blocker_dbfs=-12.0,
                   lo_ppm=5.0, phase_noise_linewidth_hz=1.0, duration_s=0.1)
    _, meta, x = load(manifest)
    fs = meta["global"]["core:sample_rate"]
    fc = meta["captures"][0]["core:frequency"]
    kinds = {t["kind"] for _, t in truths(meta)}
    assert {"spur", "dc-offset", "iq-image", "im3", "blocker", "cw"} <= kinds

    [(_, spur)] = truths(meta, kind="spur")
    assert spur["center_hz"] == 100e6 and spur["offset_hz"] == pytest.approx(-200e3)
    assert abs(tone_power_dbfs(x, fs, spur["offset_hz"]) - spur["power_dbfs"]) < 0.5

    [(_, dc)] = truths(meta, kind="dc-offset")
    assert abs(db(abs(np.mean(x)) ** 2) - dc["power_dbfs"]) < 0.5

    [(_, cw)] = truths(meta, kind="cw")
    assert cw["center_hz"] == pytest.approx(cw["rf_center_hz"] - 5e-6 * fc)
    # Phase noise of 1 Hz linewidth over 0.1 s costs well under 0.5 dB of coherent power.
    assert abs(tone_power_dbfs(x, fs, cw["center_hz"] - fc) - cw["power_dbfs"]) < 0.5

    image = [t for _, t in truths(meta, kind="iq-image") if t["image_of_kind"] == "cw"][0]
    assert image["center_hz"] == pytest.approx(2 * fc - cw["center_hz"])
    assert abs(tone_power_dbfs(x, fs, image["offset_hz"]) - image["power_dbfs"]) < 1.0

    for _, im3 in truths(meta, kind="im3"):
        assert abs(tone_power_dbfs(x, fs, im3["offset_hz"]) - im3["power_dbfs"]) < 1.0


def test_overload_sets_provenance_and_annotations(tmp_path):
    _, meta, _ = load(gen(tmp_path, "tone", blocker_dbfs=-6.0, adc_gain_db=6.0))
    prov = meta["global"][sigmf.PROVENANCE_KEY]
    assert prov["overload"] is True
    assert meta["captures"][0]["hackriff:clip_count"] == scenario_truth(meta)["clip_count"] > 0
    assert truths(meta, kind="overload")
    assert scenario_truth(meta)["overload"] is True


def test_noise_floor_rise_step(tmp_path):
    _, meta, x = load(gen(tmp_path, "noise_floor_rise"))
    fs = meta["global"]["core:sample_rate"]
    [(ev_ann, ev)] = truths(meta, role="event")
    s0 = ev_ann["core:sample_start"]
    before = median_floor_dbfs(x[:s0], fs)
    after = median_floor_dbfs(x[s0:], fs)
    floors = {a["core:label"]: t for a, t in truths(meta, role="floor")}
    assert abs(before - floors["noise-floor-before"]["expected_floor_dbfs"]) < 0.3
    assert abs(after - floors["noise-floor-after"]["expected_floor_dbfs"]) < 0.3
    assert abs((after - before) - ev["step_db"]) < 0.4
    assert ev["center_hz"] == 1575.42e6 and ev["t0_utc"].startswith("2026-09-13T12:00:00.05")
    assert len(truths(meta, kind="cw")) == 2


def test_noise_floor_rise_partial_band(tmp_path):
    _, meta, x = load(gen(tmp_path, "noise_floor_rise", rise_bandwidth_hz=500e3, n_weak_signals=0))
    fs = meta["global"]["core:sample_rate"]
    [(ev_ann, ev)] = truths(meta, role="event")
    s0 = ev_ann["core:sample_start"]
    f, psd = signal.welch(x[s0:], fs=fs, nperseg=1024, return_onesided=False, detrend=False)
    inside = np.abs(f) < 200e3
    outside = np.abs(f) > 350e3
    step = db(np.median(psd[inside])) - db(np.median(psd[outside]))
    assert abs(step - ev["step_db"]) < 0.6


def test_injected_floor_recovered_within_1db(tmp_path):
    """SPACE-050 truth: the calibrated floor recovered from IQ matches the stated dBm within ±1 dB."""
    _, meta, x = load(gen(tmp_path, "injected_floor"))
    fs = meta["global"]["core:sample_rate"]
    segs = scenario_truth(meta)["segments"]
    assert len(meta["captures"]) == len(segs) == 6
    for cap, seg in zip(meta["captures"], segs, strict=True):
        assert cap[sigmf.PROVENANCE_KEY]["tune"]["center_hz"] == seg["center_hz"]
        s = cap["core:sample_start"]
        measured_dbfs = median_floor_dbfs(x[s : s + seg["sample_count"]], fs, nperseg=512)
        measured_dbm = measured_dbfs + seg["calibration_k_db"]
        assert abs(measured_dbfs - seg["expected_floor_dbfs"]) < 0.5
        assert abs(measured_dbm - seg["floor_dbm"]) < 1.0


# ---- occupancy -------------------------------------------------------------------------------


def test_occupancy_schedule_statistics(tmp_path):
    manifest = gen(tmp_path, "occupancy_multi_hour", hours=3.0, windows=2)
    sched = json.loads((manifest.parent / "schedule.json").read_text())
    p = json.loads(manifest.read_text())["params"]
    span = sched["span_s"]
    grid = np.arange(0, span, 0.01) + 0.005
    for ch in sched["stats"]["per_channel"]:
        mine = sorted((b for b in sched["bursts"] if b["channel"] == ch["channel"]), key=lambda b: b["start_s"])
        assert len(mine) == ch["n_bursts"]
        busy = np.zeros(grid.size, dtype=bool)
        for prev, b in zip([None, *mine], mine):
            assert p["burst_min_s"] - 1e-6 <= b["duration_s"] <= p["burst_max_s"] + 1e-6 or \
                b["start_s"] + b["duration_s"] >= span - 1e-6
            if prev is not None:
                assert b["start_s"] >= prev["start_s"] + prev["duration_s"] + p["min_gap_s"] - 1e-6
            busy[(grid >= b["start_s"]) & (grid < b["start_s"] + b["duration_s"])] = True
        assert abs(busy.mean() - ch["occupancy_fraction"]) < 2e-3
        hours = ch["window_hours"]
        assert sum(h["on_s"] for h in hours) == pytest.approx(ch["on_time_s"])
        assert sum(v["on_s"] for v in ch["utc_hour_of_day"].values()) == pytest.approx(ch["on_time_s"])
    assert sum(sched["stats"]["duration_histogram"]["counts"]) == len(sched["bursts"])


def test_occupancy_distribution_and_hour_profile(tmp_path):
    """Over a day, burst starts follow the hour-of-day profile and durations the log-normal median."""
    manifest = gen(tmp_path, "occupancy_multi_hour", hours=24.0, start_utc="2026-09-13T00:00:00Z",
                   rates_per_hour=[240.0], burst_median_s=0.5, min_gap_s=0.0, windows=1)
    sched = json.loads((manifest.parent / "schedule.json").read_text())
    durs = np.array([b["duration_s"] for b in sched["bursts"]])
    assert abs(np.median(durs) / 0.5 - 1) < 0.08
    counts = np.array(sched["stats"]["bursts_started_per_window_hour"], dtype=float)
    assert np.corrcoef(counts, sched["hour_profile"])[0, 1] > 0.95


def test_occupancy_window_render(tmp_path):
    manifest = gen(tmp_path, "occupancy_multi_hour", hours=1.0, windows=2)
    man = json.loads(manifest.read_text())
    sched = json.loads((manifest.parent / "schedule.json").read_text())
    for i, w in enumerate(sched["windows"]):
        _, meta, x = load(manifest, i)
        fs = meta["global"]["core:sample_rate"]
        fc = meta["captures"][0]["core:frequency"]
        bursts = truths(meta, kind="nbfm-burst")
        expected = [b["index"] for b in sched["bursts"]
                    if b["start_s"] < w["start_s"] + w["duration_s"] and b["start_s"] + b["duration_s"] > w["start_s"]]
        assert sorted(t["burst_index"] for _, t in bursts) == sorted(expected) and expected
        for ann, t in bursts:
            s, n = ann["core:sample_start"], ann["core:sample_count"]
            seg = x[s : s + n] * np.exp(-2j * math.pi * (t["center_hz"] - fc) * np.arange(s, s + n) / fs)
            inband = np.mean(np.abs(signal.lfilter(signal.firwin(129, 5e3, fs=fs), 1, seg)) ** 2)
            assert db(inband) > t["power_dbfs"] - 1.5
    # Rendering a window on demand reproduces the same bytes.
    w0 = sched["windows"][0]
    again = generate("occupancy_multi_hour", 1, tmp_path / "again",
                     {**SMALL["occupancy_multi_hour"], "hours": 1.0, "render_start_s": w0["start_s"]})
    assert (again.parent / "window_00.sigmf-data").read_bytes() == (manifest.parent / "window_00.sigmf-data").read_bytes()
    assert man["files"] == ["schedule.json"]


# ---- FSK -------------------------------------------------------------------------------------


@pytest.mark.parametrize("impaired", [False, True])
def test_fsk_bursts_demodulate_to_truth_bits_with_valid_crc(tmp_path, impaired):
    extra = {"dc_offset_dbfs": -35.0, "iq_gain_db": 0.3, "iq_phase_deg": 2.0, "lo_ppm": 2.0,
             "phase_noise_linewidth_hz": 5.0} if impaired else {}
    _, meta, x = load(gen(tmp_path, "fsk_burst_train", **extra))
    fs = meta["global"]["core:sample_rate"]
    fc = meta["captures"][0]["core:frequency"]
    st = scenario_truth(meta)
    bursts = truths(meta, kind="fsk-burst")
    assert len(bursts) == st["emitter"]["n_bursts"] >= 2
    starts = [t["t_start_s"] for _, t in bursts]
    for d in np.diff(starts):
        assert abs(d - st["emitter"]["period_s"]) <= 2 * st["emitter"]["jitter_s"] + 1e-6
    lp = signal.firwin(101, 20e3, fs=fs)
    for ann, t in bursts:
        s, n = ann["core:sample_start"], ann["core:sample_count"]
        seg = x[s : s + n] * np.exp(-2j * math.pi * (t["center_hz"] - fc) * np.arange(s, s + n) / fs)
        seg = np.convolve(seg, lp, mode="same")
        inst = np.angle(seg[1:] * np.conj(seg[:-1])) * fs / (2 * math.pi)
        sps = fs / t["symbol_rate_bd"]
        idx = ((np.arange(t["frame"]["n_bits"]) + 0.5) * sps).astype(int)
        bits = (inst[np.minimum(idx, len(inst) - 1)] > 0).astype(np.uint8)
        assert np.packbits(bits).tobytes().hex() == t["frame"]["bits_hex"]
        payload = bytes.fromhex(t["frame"]["payload_hex"])
        assert binascii.crc_hqx(payload, 0xFFFF) == int(t["frame"]["crc_hex"], 16)  # stdlib reference
        assert abs(np.median(np.abs(inst[idx[:-1]])) / t["deviation_hz"] - 1) < 0.08
        assert t["identity"]["value"] == f"{t['payload_fields']['sensor_id']:04x}"
        if not impaired:
            noise = 10 ** (st_floor(meta) / 10)
            assert abs(db(np.mean(np.abs(x[s : s + n]) ** 2) - noise) - t["power_dbfs"]) < 0.5


def st_floor(meta):
    [(_, fl)] = truths(meta, role="floor")
    return fl["expected_floor_dbfs"]


def test_crc_reference_vectors():
    assert fsk_mod.crc16_ccitt_false(b"123456789") == 0x29B1 == binascii.crc_hqx(b"123456789", 0xFFFF)
    # Mode S DF17 from "The 1090 Megahertz Riddle" (KLM1023, ICAO 4840D6): parity 0x576098.
    msg = bytes.fromhex("8D4840D6202CC371C32CE0576098")
    assert adsb_mod.crc24(msg[:11]) == 0x576098
    assert adsb_mod.me_identification("KLM1023 ") == int.from_bytes(msg[4:11], "big")


# ---- RDS -------------------------------------------------------------------------------------

RDS_OFFSETS = {"A": 0x0FC, "B": 0x198, "C": 0x168, "C'": 0x350, "D": 0x1B4}
#: EN 50067 Annex B parity-check matrix; row k applies to bit 25-k of the 26-bit block (MSB first).
RDS_H = (0x200, 0x100, 0x080, 0x040, 0x020, 0x010, 0x008, 0x004, 0x002, 0x001, 0x2DC, 0x16E, 0x0B7,
         0x287, 0x39F, 0x313, 0x355, 0x376, 0x1BB, 0x201, 0x3DC, 0x1EE, 0x0F7, 0x2A7, 0x38F, 0x31B)
#: Published syndromes of a correctly received block carrying each offset word.
RDS_SYNDROMES = {"A": 0x3D8, "B": 0x3D4, "C": 0x25C, "C'": 0x3CC, "D": 0x258}


def rds_syndrome(word26: int) -> int:
    s = 0
    for k, row in enumerate(RDS_H):
        if word26 & (1 << (25 - k)):
            s ^= row
    return s


def test_rds_checkwords_match_published_syndromes():
    from hkpy.synth import rds as rds_mod

    for name, word in RDS_OFFSETS.items():
        assert rds_syndrome(word) == RDS_SYNDROMES[name], name
        assert rds_mod.OFFSET_WORDS[name] == word
        for info in (0x0000, 0xC0DE, 0x1234, 0xFFFF):
            assert rds_syndrome(rds_mod.block(info, name)) == RDS_SYNDROMES[name], (name, hex(info))


def decode_rds(x: np.ndarray, fs: float) -> dict:
    """Reference RDS receiver: FM discriminator -> 57 kHz downconversion -> BPSK phase from the
    squared signal -> biphase symbol timing search -> differential decode -> block sync on offset
    words -> PI and PS."""
    mpx = np.angle(x[1:] * np.conj(x[:-1])) * fs / (2 * math.pi)
    t = np.arange(len(mpx)) / fs
    pilot_dev = 2 * abs(np.mean(mpx * np.exp(-2j * math.pi * 19000 * t)))
    z = np.convolve(mpx * np.exp(-2j * math.pi * 57000 * t), signal.firwin(801, 2400, fs=fs), mode="same")
    r = np.real(z * np.exp(-1j * np.angle(np.sum(z**2)) / 2))
    sps = fs / 1187.5
    csum = np.concatenate([[0.0], np.cumsum(r)])

    def integrate(a, b):
        return csum[np.clip(np.round(b).astype(int), 0, len(r))] - csum[np.clip(np.round(a).astype(int), 0, len(r))]

    best = None
    n_bits = int(len(r) / sps) - 2
    for off in np.arange(0, sps, sps / 32):
        s0 = off + np.arange(n_bits) * sps
        diff = integrate(s0, s0 + sps / 2) - integrate(s0 + sps / 2, s0 + sps)
        metric = np.sum(np.abs(diff))
        if best is None or metric > best[0]:
            best = (metric, diff)
    d = (best[1] > 0).astype(np.uint8)
    b = d[1:] ^ d[:-1]
    words = [int("".join(map(str, b[i : i + 26])), 2) for i in range(len(b) - 25)]
    pis, ps = set(), {}
    for p in range(len(words) - 78):
        checks = [rds_syndrome(words[p + 26 * k]) for k in range(4)]
        if checks[0] == RDS_SYNDROMES["A"] and checks[1] == RDS_SYNDROMES["B"] and \
                checks[2] in (RDS_SYNDROMES["C"], RDS_SYNDROMES["C'"]) and checks[3] == RDS_SYNDROMES["D"]:
            pis.add(words[p] >> 10)
            b2, b4 = words[p + 26] >> 10, words[p + 78] >> 10
            if (b2 >> 11) == 0:  # group type 0A/0B
                ps[b2 & 3] = chr(b4 >> 8) + chr(b4 & 0xFF)
    return {"pilot_deviation_hz": pilot_dev, "pi": pis,
            "ps": "".join(ps[k] for k in range(4)) if len(ps) == 4 else None}


@pytest.mark.parametrize("extra", [{}, {"offset_hz": 40e3, "lo_ppm": 3.0, "iq_gain_db": 0.3}])
def test_fm_rds_pi_and_ps_recoverable(tmp_path, extra):
    _, meta, x = load(gen(tmp_path, "fm_broadcast_rds", **extra))
    fs = meta["global"]["core:sample_rate"]
    fc = meta["captures"][0]["core:frequency"]
    [(_, t)] = truths(meta, kind="wfm-broadcast")
    x = x * np.exp(-2j * math.pi * (t["center_hz"] - fc) * np.arange(len(x)) / fs)
    x = np.convolve(x, signal.firwin(129, 140e3, fs=fs), mode="same")
    got = decode_rds(x, fs)
    assert got["pi"] == {t["rds"]["pi"]}
    assert got["ps"] == t["rds"]["ps"]
    assert abs(got["pilot_deviation_hz"] - t["pilot"]["deviation_hz"]) < 0.05 * t["pilot"]["deviation_hz"]
    assert t["identity"] == {"type": "rds_pi", "value": "C0DE"}


# ---- ADS-B -----------------------------------------------------------------------------------


def crc24_remainder(bits: np.ndarray) -> int:
    reg = int("".join(map(str, bits)), 2)
    g = 0x1FFF409
    for bit in range(len(bits) - 1, 23, -1):
        if reg & (1 << bit):
            reg ^= g << (bit - 24)
    return reg


def nl(lat):
    if abs(lat) >= 87:
        return 2 if abs(lat) == 87 else 1
    return int(math.floor(2 * math.pi / math.acos(1 - (1 - math.cos(math.pi / 30)) / math.cos(math.radians(lat)) ** 2)))


def cpr_global(even, odd):
    """Globally unambiguous airborne CPR decode, using the odd message as the most recent."""
    (ley, lex), (loy, lox) = even, odd
    j = math.floor((59 * ley - 60 * loy) / 2**17 + 0.5)
    rle = 6.0 * ((j % 60) + ley / 2**17)
    rlo = 360 / 59 * ((j % 59) + loy / 2**17)
    rle, rlo = (v - 360 if v >= 270 else v for v in (rle, rlo))
    assert nl(rle) == nl(rlo)
    n = nl(rlo)
    ni = max(n - 1, 1)
    m = math.floor((lex * (n - 1) - lox * n) / 2**17 + 0.5)
    lon = 360 / ni * ((m % ni) + lox / 2**17)
    return rlo, lon - 360 if lon >= 180 else lon


def test_adsb_ppm_demodulates_with_valid_crc_and_cpr(tmp_path):
    _, meta, x = load(gen(tmp_path, "adsb_squitter"))
    fs = meta["global"]["core:sample_rate"]
    mag = np.abs(x)
    grid = np.arange(len(mag))
    positions: dict[str, dict] = {}
    msgs = truths(meta, kind="adsb-df17")
    assert len(msgs) == 16
    for ann, t in msgs:
        s = ann["core:sample_start"]

        def m(us, s=s):
            return np.interp(s + np.asarray(us) * 1e-6 * fs, grid, mag)

        assert m(0.25) > m(2.25) and m(3.75) > m(2.75)  # preamble pulses
        k = np.arange(112)
        bits = (m(8 + k + 0.25) > m(8 + k + 0.75)).astype(np.uint8)
        assert np.packbits(bits).tobytes().hex() == t["message_hex"]
        assert crc24_remainder(bits) == 0
        assert (bits[:5] @ (1 << np.arange(4, -1, -1))) == 17
        assert np.packbits(bits[8:32]).tobytes().hex() == t["icao"]
        if t["tc"] == 11:
            positions.setdefault(t["icao"], {})[t["metadata"]["cpr_format"]] = t["metadata"]
    for icao, pos in positions.items():
        lat, lon = cpr_global((pos["even"]["cpr_lat"], pos["even"]["cpr_lon"]),
                              (pos["odd"]["cpr_lat"], pos["odd"]["cpr_lon"]))
        assert abs(lat - pos["odd"]["lat"]) < 1e-3 and abs(lon - pos["odd"]["lon"]) < 1e-3, icao


@pytest.mark.skipif(shutil.which("readsb") is None, reason="readsb not installed")
def test_readsb_decodes_adsb_squitters(tmp_path):
    """SIGNAL-001 fallback: the installed readsb decodes every synthetic squitter (default 2.4 Msps)."""
    manifest = generate("adsb_squitter", 1, tmp_path / "adsb")
    man, meta, _ = load(manifest)
    raw = np.fromfile(manifest.parent / man["recordings"][0].replace(".sigmf-meta", ".sigmf-data"), dtype=np.uint8)
    uc8 = tmp_path / "adsb.uc8"
    (raw ^ 0x80).tofile(uc8)  # ci8 -> uc8
    base = ["readsb", "--device-type", "ifile", "--ifile", str(uc8), "--iformat", "UC8"]
    msgs = truths(meta, kind="adsb-df17")

    addr = subprocess.run([*base, "--onlyaddr"], capture_output=True, text=True, timeout=120)
    seen = Counter(line.strip() for line in (addr.stdout + addr.stderr).splitlines()
                   if re.fullmatch(r"[0-9a-f]{6}", line.strip()))
    assert seen == Counter(t["icao"] for _, t in msgs)

    verbose = subprocess.run([*base, "--stats"], capture_output=True, text=True, timeout=120)
    decoded = set(re.findall(r"^\*([0-9a-f]{28});", verbose.stdout + verbose.stderr, re.M))
    if decoded:  # this readsb build prints each decoded frame; older builds print only counters
        assert decoded == {t["message_hex"] for _, t in msgs}


# ---- POCSAG (T-098, M1 tutorial 2) --------------------------------------------------------------

#: Independently re-derived from ITU-R M.584 / the multimon-ng bch.c reference (see py/hkpy/synth/
#: pocsag.py's module docstring); cross-checked below against the standard sync/idle codewords.
POCSAG_BCH_POLY = 0x769
POCSAG_SYNC = 0x7CD215D8
POCSAG_IDLE = 0x7A89C197
POCSAG_NUMERIC_CHARSET = "084 2.6]195-3U7["  # nibble -> char; published POCSAG numeric table


def pocsag_bch_parity(data21: int) -> int:
    reg = (data21 & 0x1FFFFF) << 10
    for i in range(20, -1, -1):
        if reg & (1 << (i + 10)):
            reg ^= POCSAG_BCH_POLY << i
    return reg & 0x3FF


def pocsag_bch_ok(codeword: int) -> bool:
    data = (codeword >> 11) & 0x1FFFFF
    parity_ok = pocsag_bch_parity(data) == (codeword >> 1) & 0x3FF
    return parity_ok and bin(codeword).count("1") % 2 == 0


def pocsag_bch_encode(data21: int) -> int:
    d = data21 & 0x1FFFFF
    codeword = (d << 11) | (pocsag_bch_parity(d) << 1)
    return codeword | (bin(codeword).count("1") % 2)


def test_pocsag_bch_matches_published_sync_and_idle_codewords():
    assert pocsag_bch_ok(POCSAG_SYNC) and pocsag_bch_ok(POCSAG_IDLE)
    assert pocsag_bch_encode(POCSAG_SYNC >> 11) == POCSAG_SYNC
    assert pocsag_bch_encode(POCSAG_IDLE >> 11) == POCSAG_IDLE
    assert pocsag_mod.bch_encode(POCSAG_SYNC >> 11) == POCSAG_SYNC == pocsag_mod.SYNC_CODEWORD


def pocsag_numeric_decode(codewords: list[int]) -> str:
    out = []
    for w in codewords:
        data20 = (w >> 11) & 0xFFFFF
        for shift in (16, 12, 8, 4, 0):
            out.append(POCSAG_NUMERIC_CHARSET[(data20 >> shift) & 0xF])
    return "".join(out)


def pocsag_alpha_decode(codewords: list[int]) -> str:
    bits = np.concatenate([[(((w >> 11) & 0xFFFFF) >> b) & 1 for b in range(19, -1, -1)] for w in codewords])
    chars = []
    for i in range(0, len(bits) - 6, 7):
        v = int("".join(map(str, bits[i : i + 7])), 2)
        r = int(f"{v:07b}"[::-1], 2)  # rev7: this project's packing reverses each 7-bit char
        if r == 0:
            break
        chars.append(chr(r))
    return "".join(chars)


def decode_pocsag_channel(x: np.ndarray, fs: float, t: dict) -> dict:
    """Independent FM-discriminator + BCH decoder: locate the preamble/sync/batches structurally,
    exactly as a real decoder would walk frame/codeword slots, and BCH-check every codeword."""
    off, baud, n_bits = t["offset_hz"], t["symbol_rate_bd"], t["frame"]["n_bits"]
    tt = np.arange(len(x)) / fs
    seg = np.convolve(x * np.exp(-2j * math.pi * off * tt), signal.firwin(401, 1.5 * baud + 3 * t["deviation_hz"], fs=fs), mode="same")
    inst = np.angle(seg[1:] * np.conj(seg[:-1])) * fs / (2 * math.pi)
    sps = fs / baud
    sidx = ((np.arange(n_bits) + 0.5) * sps).astype(int)
    bits = (inst[np.minimum(sidx, len(inst) - 1)] < 0).astype(np.uint8)  # bit1 -> -deviation (see truth "mapping")
    assert np.array_equal(bits[:576], np.array([1 - (i & 1) for i in range(576)], dtype=np.uint8))

    words, pos = [], 576
    n_batches = t["frame"]["n_batches"]
    for _ in range(n_batches):
        sync = int("".join(map(str, bits[pos : pos + 32])), 2)
        assert sync == POCSAG_SYNC
        pos += 32
        for _ in range(16):
            words.append(int("".join(map(str, bits[pos : pos + 32])), 2))
            pos += 32
    assert all(pocsag_bch_ok(w) for w in words)

    addr_slot = t["frame_position"] * 2
    addr_data = (words[addr_slot] >> 11) & 0x1FFFFF
    assert (addr_data >> 20) & 1 == 0  # address flag
    address = ((addr_data >> 2) << 3) | t["frame_position"]
    function = addr_data & 3
    msg_words = []
    for w in words[addr_slot + 1 :]:
        if w == POCSAG_IDLE or (((w >> 11) & 0x1FFFFF) >> 20) & 1 == 0:  # message flag (bit20) clear
            break
        msg_words.append(w)
    text = pocsag_numeric_decode(msg_words) if function == 0 else pocsag_alpha_decode(msg_words)
    return {"address": address, "function": function, "text": text.rstrip(" \x00")}


def test_pocsag_pages_demodulate_and_decode(tmp_path):
    """SIGNAL-062 M1 tutorial 2: BCH(31,21)+parity, 512/1200/2400 Bd, multi-channel (follow_hops)."""
    _, meta, x = load(gen(tmp_path, "pocsag_pagers"))
    fs = meta["global"]["core:sample_rate"]
    fc = meta["captures"][0]["core:frequency"]
    pages = truths(meta, kind="pocsag-page")
    assert len(pages) == 3
    bauds = set()
    for ann, t in pages:
        s, n = ann["core:sample_start"], ann["core:sample_count"]
        got = decode_pocsag_channel(x[s : s + n], fs, t)
        assert got["address"] == t["ric"]
        assert got["function"] == t["function"]
        assert got["text"].strip() == t["message_text"].strip()
        bauds.add(t["symbol_rate_bd"])
    assert bauds == {512.0, 1200.0, 2400.0}
    assert fc == scenario_truth(meta)["channels"][0]["rf_center_hz"] - scenario_truth(meta)["channels"][0]["offset_hz"]


@pytest.mark.skipif(shutil.which("multimon-ng") is None, reason="multimon-ng not installed")
def test_multimon_ng_decodes_pocsag_pages(tmp_path):
    """Oracle check (T-098): multimon-ng decodes every channel's address/function/text exactly."""
    demod_name = {512.0: "POCSAG512", 1200.0: "POCSAG1200", 2400.0: "POCSAG2400"}
    _, meta, x = load(gen(tmp_path, "pocsag_pagers"))
    fs = meta["global"]["core:sample_rate"]
    factor = int(round(fs / 22050))
    assert factor * 22050 == fs
    for ann, t in truths(meta, kind="pocsag-page"):
        s, n = ann["core:sample_start"], ann["core:sample_count"]
        tt = np.arange(n) / fs
        seg = np.convolve(x[s : s + n] * np.exp(-2j * math.pi * t["offset_hz"] * tt),
                          signal.firwin(401, 6000, fs=fs), mode="same")
        inst = np.angle(seg[1:] * np.conj(seg[:-1])) * fs / (2 * math.pi)
        audio = signal.decimate(inst, factor, ftype="fir")
        raw = tmp_path / f"ch_{t['offset_hz']:.0f}.raw"
        np.clip(audio / t["deviation_hz"] * 20000, -32000, 32000).astype("<i2").tofile(raw)
        res = subprocess.run(["multimon-ng", "-t", "raw", "-a", demod_name[t["symbol_rate_bd"]], "-e", str(raw)],
                             capture_output=True, text=True, timeout=30)
        out = res.stdout + res.stderr
        assert f"Address: {t['ric']:>7}" in out
        assert f"Function: {t['function']}" in out
        expect = t["message_text"].strip()
        assert expect in out.replace("\x00", "").replace("<NUL>", "")


# ---- ACARS (T-098, M1 tutorial 3; synthetic, not oracle-validated -- see hkpy.synth.acars) -------


def acars_char_parity_ok(byte: int) -> bool:
    return bin(byte).count("1") % 2 == 1


def decode_acars_frame(x: np.ndarray, fs: float, t: dict, prekey_s: float) -> dict:
    """Independent matched-filter MSK decoder + CRC-16 check, decoupled from acars.py's encoder."""
    baud = t["symbol_rate_bd"]
    n_bits = t["frame"]["n_bits"]
    sps = fs / baud
    start = int(round(prekey_s * fs))
    mark_w = 2 * math.pi * t["mark_hz"] / fs
    space_w = 2 * math.pi * t["space_hz"] / fs
    env = np.abs(x)
    dc = np.mean(env[:start])  # unmodulated prekey carrier level: subtract so tone correlation isn't DC-biased
    bits = np.zeros(n_bits, dtype=np.uint8)
    for k in range(n_bits):
        s0 = start + int(round(k * sps))
        s1 = start + int(round((k + 1) * sps))
        seg = env[s0:s1] - dc
        n = len(seg)
        if n == 0:
            break
        tsamp = np.arange(n)
        e_mark = abs(np.sum(seg * np.exp(-1j * mark_w * tsamp)))
        e_space = abs(np.sum(seg * np.exp(-1j * space_w * tsamp)))
        bits[k] = 1 if e_mark > e_space else 0
    data_bits = bits[32:]  # skip the alternating clock-sync preamble
    tx = np.packbits(data_bits).tobytes()
    assert tx[0] == 0x16 and tx[1] == 0x16  # SYN SYN
    body_with_parity = tx[2:-2]
    crc_hi, crc_lo = tx[-2], tx[-1]
    for b in body_with_parity:
        assert acars_char_parity_ok(b)
    body = bytes(b & 0x7F for b in body_with_parity)
    crc = acars_mod.crc16_acars(body)
    assert crc == (crc_hi << 8) | crc_lo
    assert body[0] == 0x01 and body[-1] == 0x03  # SOH .. ETX
    stx = body.index(0x02)
    return {
        "mode": chr(body[1]), "reg": body[2:9].decode("ascii"), "ack": chr(body[9]),
        "label": body[10:12].decode("ascii"), "block_id": chr(body[12]),
        "text": body[stx + 1 : -1].decode("ascii"), "crc": crc,
    }


def test_acars_message_demodulates_with_valid_crc(tmp_path):
    """SIGNAL-062 M1 tutorial 3: AM + MSK 2400 Bd, SYN/SOH..ETX framing, CRC-16 (synthetic)."""
    manifest = gen(tmp_path, "acars_message")
    man = json.loads(manifest.read_text())
    prekey_s = man["params"]["prekey_s"]
    _, meta, x = load(manifest)
    fs = meta["global"]["core:sample_rate"]
    [(ann, t)] = truths(meta, kind="acars-message")
    s, n = ann["core:sample_start"], ann["core:sample_count"]
    got = decode_acars_frame(x[s : s + n], fs, t, prekey_s)
    assert got["mode"] == t["fields"]["mode"]
    assert got["reg"] == t["fields"]["reg"]
    assert got["label"] == t["fields"]["label"]
    assert got["block_id"] == t["fields"]["block_id"]
    assert got["text"] == t["fields"]["text"] == t["text_expected"]
    assert f"{got['crc']:04x}" == t["frame"]["crc_hex"]
    assert t["identity"] == {"type": "acars_reg", "value": t["fields"]["reg"].strip()}
