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
from hkpy.synth import lora as lora_mod
from hkpy.synth import pocsag as pocsag_mod
from hkpy.synth.__main__ import main as cli_main

#: Small parameter sets so the suite stays fast.
SMALL: dict[str, dict] = {
    "tone": {},
    "fsk_burst_train": {"duration_s": 0.3},
    "noise_floor_rise": {"duration_s": 0.1, "t0_s": 0.05},
    "injected_floor": {"segment_duration_s": 0.02},
    "occupancy_multi_hour": {"hours": 1.0, "windows": 1, "window_duration_s": 0.1},
    "occupancy_markov_scene": {"span_hours": 2.0, "novelty_start_hour": 1.0, "n_iq_windows": 1,
                               "window_duration_s": 0.02, "revisit_mean_gap_s": 120.0},
    "fm_broadcast_rds": {},
    "adsb_squitter": {"duration_s": 0.1, "messages_per_aircraft": 4},
    "pocsag_pagers": {},
    "acars_message": {"prekey_s": 0.02, "text": "TEST"},
    "trunk_control_channel": {"duration_s": 0.2},
    "trunk_tsbk_control_channel": {"duration_s": 0.2},
    "trunk_encrypted_control_channel": {"duration_s": 0.2},
    "trunk_p25p2_control_channel": {"duration_s": 0.2},
    "trunk_dmr_control_channel": {"duration_s": 0.2},
    "trunk_nxdn_control_channel": {"duration_s": 0.2},
    "lora_ism_burst": {"duration_s": 0.15, "sf": 7, "first_packet_s": 0.02,
                       "packet_period_s": 0.06, "fsk_period_s": 0.05},
    "retune_diversity": {"dwell_s": 0.05},
    "mismatched_hypothesis": {"duration_s": 0.3},
    "multipath_echo": {"duration_s": 0.6},
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


@pytest.mark.parametrize("width", [8, 16, 24, 32])
def test_fsk_check_width_is_parameterised_and_round_trips(tmp_path, width):
    """T-622: docs/22 A7 needs CRC-8/16/24/32 on the generic generator; ADR-0022 SS4.3 lowered
    the confirm-gate width floor to 8."""
    _, meta, _ = load(gen(tmp_path, "fsk_burst_train", check_width=width))
    st = scenario_truth(meta)
    bursts = truths(meta, kind="fsk-burst")
    assert len(bursts) >= 2
    assert st["emitter"]["crc"]["width"] == width
    hex_width = width // 4
    for _, t in bursts:
        assert t["crc"]["width"] == width
        payload = bytes.fromhex(t["frame"]["payload_hex"])
        params = t["crc"]
        # Independent reference: the bit-serial RevEng engine, over the params truth states.
        crc = fsk_mod.crc_generic(payload, width, int(params["poly"], 16), int(params["init"], 16),
                                   params["refin"], params["refout"], int(params["xorout"], 16))
        assert f"{crc:0{hex_width}x}" == t["frame"]["crc_hex"] == params["value"][2:]
        assert len(t["frame"]["crc_hex"]) == hex_width
        assert t["frame"]["layout"][-1] == {"field": "check", "bits": width,
                                            "covers": "sensor_id..flags (6 bytes)"}
        assert params["start_bit"] == t["frame"]["preamble_bits"] + 16  # sync is always 2 bytes


def test_fsk_off_catalogue_polynomial_is_flagged_and_not_recognised(tmp_path):
    """T-622 / docs/22 A7: a polynomial not in the RevEng catalogue, so a structured signal
    exists whose check cannot be looked up."""
    _, meta, _ = load(gen(tmp_path, "fsk_burst_train", check_width=16, check_poly_hex="0x8F45"))
    _, t = truths(meta, kind="fsk-burst")[0]
    assert t["crc"]["poly"] == "0x8f45"
    assert t["crc"]["in_reveng_catalogue"] is False
    assert t["crc"]["catalogue_name"] is None
    assert fsk_mod.crc_catalogue_name(16, 0x8F45, 0xFFFF, False, False, 0) is None
    # The default (no override) still lands exactly on the historical CRC-16/CCITT-FALSE, so
    # existing fixtures and their byte-identical determinism are unaffected by this feature.
    _, meta_default, _ = load(gen(tmp_path, "fsk_burst_train"))
    _, td = truths(meta_default, kind="fsk-burst")[0]
    assert td["crc"]["catalogue_name"] == "CRC-16/CCITT-FALSE" and td["crc"]["in_reveng_catalogue"]


def test_fsk_constant_payload_beacon_repeats_the_same_frame(tmp_path):
    """T-622 / docs/22 P7, ADR-0022 SS4.2: a beacon must NOT confirm on repeat count alone -
    `differences` (chance-corrected) stays 1 for a beacon how ever many bursts are sent, unlike
    `distinct_valid`, which counts every valid frame. This fixture is the case that exercises it:
    every burst carries the identical payload and therefore the identical CRC."""
    _, meta, _ = load(gen(tmp_path, "fsk_burst_train", constant_payload=True))
    st = scenario_truth(meta)
    bursts = truths(meta, kind="fsk-burst")
    assert len(bursts) >= 3 and st["emitter"]["constant_payload"] is True
    payloads = {t["frame"]["payload_hex"] for _, t in bursts}
    crcs = {t["frame"]["crc_hex"] for _, t in bursts}
    assert len(payloads) == 1 and len(crcs) == 1
    # A non-beacon run (the default) varies payload/CRC frame to frame.
    _, meta_varying, _ = load(gen(tmp_path, "fsk_burst_train"))
    varying_bursts = truths(meta_varying, kind="fsk-burst")
    assert len(varying_bursts) >= 3
    assert len({t["frame"]["payload_hex"] for _, t in varying_bursts}) > 1


def st_floor(meta):
    [(_, fl)] = truths(meta, role="floor")
    return fl["expected_floor_dbfs"]


def test_tsbk_fields_encode_the_published_worked_example():
    """The generator's TSBK packing, checked against a third party's numbers rather than ours.

    A published example resolves identifier 3, channel 1554 on a system whose base-frequency field
    is 0x09157562 to 771.718750 MHz. Encoding that band plan here must reproduce the field values
    the example shows, and the channel arithmetic must reproduce its frequency -- which is what
    makes "base is in units of 5 Hz" and "spacing is in units of 125 Hz" claims someone else can
    check, instead of a convention this repo agreed with itself.
    """
    from hkpy.synth import trunking as tk

    base_hz, spacing_hz = 762_006_250.0, 6_250.0
    args = tk.iden_up_args(3, base_hz, spacing_hz)
    v = int.from_bytes(args, "big")
    assert (v >> 60) & 0xF == 3, "identifier is the top 4 bits"
    assert (v >> 32) & 0x3FF == 50, "spacing field: 6250 Hz / 125 Hz"
    assert v & 0xFFFF_FFFF == 0x0915_7562, "base field: 762006250 Hz / 5 Hz"
    assert base_hz + spacing_hz * 1554 == 771_718_750.0

    chan16 = tk.channel_number(3, 1554)
    assert chan16 >> 12 == 3 and chan16 & 0xFFF == 1554

    block = tk.tsbk(tk.TSBK_OP_IDEN_UP, args)
    assert len(block) == tk.TSBK_BYTES
    assert block[0] & 0x3F == tk.TSBK_OP_IDEN_UP and block[1] == 0
    assert block[2:10] == args
    assert fsk_mod.crc16_ccitt_false(block[:10]) == int.from_bytes(block[10:], "big")


def test_tsbk_scene_grant_channel_derives_from_the_target_frequency(tmp_path):
    """The scene's truth is a frequency chosen first; the channel number follows from it."""
    manifest = gen(tmp_path, "trunk_tsbk_control_channel")
    _, meta, _ = load(manifest)
    t = scenario_truth(meta)["trunking"]["tsbk"]
    assert t["base_hz"] + t["spacing_hz"] * t["grant_channel"] == t["grant_target_hz"]
    assert t["grant_channel_16bit"] == (t["iden"] << 12) | t["grant_channel"]
    assert t["unannounced_channel_16bit"] >> 12 == t["unannounced_iden"]
    # The trap frequency is a real, plausible frequency -- that is what makes it a trap.
    assert t["wrong_frequency_if_misresolved_hz"] != t["grant_target_hz"]
    assert 851e6 < t["wrong_frequency_if_misresolved_hz"] < 869e6
    # Both grants and both announcements actually occur.
    assert t["counts"]["iden-up"] >= 2 and t["counts"]["grant"] >= 1
    assert t["counts"]["grant-unannounced"] >= 1

    # T-269: the followed channel is derived from its frequency the same way, and the scene stages
    # BOTH span cases -- one grant inside the window the radio holds, one outside it.
    assert t["base_hz"] + t["spacing_hz"] * t["follow_channel"] == t["follow_target_hz"]
    assert t["follow_channel_16bit"] == (t["iden"] << 12) | t["follow_channel"]
    assert t["counts"]["grant-follow"] >= 1
    usable_half = 0.4 * t["sample_rate_hz"]
    assert abs(t["follow_offset_hz"]) < usable_half, "the followed grant must be inside the window"
    assert abs(t["grant_target_offset_hz"]) > usable_half, "the other grant must be outside it"
    # Keyings are bounded events with silence between them, not a carrier. (How many there are
    # depends on duration_s, so the shape is asserted, not the count.)
    keyings = t["follow_keyings_s"]
    assert keyings, "the followed channel carries no traffic to follow"
    tol_s = 1.0 / t["sample_rate_hz"]
    for a, b in keyings[:-1]:
        assert abs((b - a) - t["follow_on_s"]) <= tol_s
    gaps = [keyings[i + 1][0] - keyings[i][1] for i in range(len(keyings) - 1)]
    assert all(g >= t["follow_off_s"] - tol_s for g in gaps)


def test_encrypted_trunk_scene_stages_an_encrypted_grant_and_a_late_entry_channel(tmp_path):
    """T-270: two more granted channels, differing only in what their announcement could say.

    Both sit inside the window the radio holds and both carry traffic, so both are followed; what
    separates them is that one grant carries the service-options encryption bit and the other
    channel is announced *only* by a grant update -- no header, so nothing ever stated its state.
    """
    manifest = gen(tmp_path, "trunk_encrypted_control_channel")
    _, meta, _ = load(manifest)
    t = scenario_truth(meta)["trunking"]["tsbk"]
    e = t["encryption"]

    # Each channel number is derived from a frequency chosen first, like every other target here.
    for key in ("encrypted", "late_entry"):
        assert t["base_hz"] + t["spacing_hz"] * e[f"{key}_channel"] == e[f"{key}_target_hz"]
        assert e[f"{key}_channel_16bit"] == (t["iden"] << 12) | e[f"{key}_channel"]
        # Inside the window, and carrying real keyings to follow.
        assert abs(e[f"{key}_offset_hz"]) < 0.4 * t["sample_rate_hz"]
        assert e[f"{key}_keyings_s"], f"the {key} channel carries no traffic"

    # The encrypted grant carries the verified bit, and only that bit: the generator must not
    # depend on fields the decoder is not entitled to read.
    assert e["encrypted_service_options"] == 0x40
    assert t["counts"]["grant-encrypted"] >= 1
    assert t["counts"]["grant-late-entry"] >= 1

    # The late-entry channel must never be announced by a plain grant anywhere in the stream --
    # that is what makes it late entry rather than just another grant.
    assert "grant-late-entry" in t["counts"]
    late16 = e["late_entry_channel_16bit"]
    assert late16 not in (t["grant_channel_16bit"], t["follow_channel_16bit"])

    # All four granted frequencies are distinct, so no assertion can be satisfied by the wrong one.
    assert len({e["encrypted_target_hz"], e["late_entry_target_hz"],
                t["follow_target_hz"], t["grant_target_hz"]}) == 4


def test_tsbk_trunk_scene_is_unchanged_by_the_encryption_branch(tmp_path):
    """T-268/T-269's fixture must be untouched: the encryption branch is off by default and
    consumes no randomness and emits no extra message when off."""
    def iq_bytes(manifest):
        man = json.loads(manifest.read_text())
        return sigmf.data_path(manifest.parent / man["recordings"][0]).read_bytes()

    a = gen(tmp_path / "a", "trunk_tsbk_control_channel", seed=7)
    b = gen(tmp_path / "b", "trunk_tsbk_control_channel", seed=7)
    assert iq_bytes(a) == iq_bytes(b)
    _, meta, _ = load(a)
    assert "encryption" not in scenario_truth(meta)["trunking"]["tsbk"]


def test_trunk_scene_without_tsbk_is_unchanged_by_the_tsbk_branch(tmp_path):
    """T-267's fixture must be byte-identical: the new branch consumes no randomness when off."""
    def iq_bytes(manifest):
        man = json.loads(manifest.read_text())
        return sigmf.data_path(manifest.parent / man["recordings"][0]).read_bytes()

    a = gen(tmp_path / "a", "trunk_control_channel", seed=5)
    b = gen(tmp_path / "b", "trunk_control_channel", seed=5)
    assert iq_bytes(a) == iq_bytes(b)
    _, meta, _ = load(a)
    assert "tsbk" not in scenario_truth(meta)["trunking"]


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


def test_fm_rds_radiotext_groups_carry_the_text(tmp_path):
    text = "HACKRIFF TUTORIAL 1 - RDS BUILT FROM BLOCKS"
    _, meta, _ = load(gen(tmp_path, "fm_broadcast_rds", duration_s=1.5, radiotext=text))
    [(_, t)] = truths(meta, kind="wfm-broadcast")
    assert t["rds"]["group_types"] == ["0A", "2A"] and t["rds"]["radiotext"] == text
    blocks = [int(b, 16) for b in t["rds"]["blocks_hex"]]
    segments = {}
    for g in range(0, len(blocks) - 3, 4):
        assert [rds_syndrome(blocks[g + k]) for k in range(4)] == [RDS_SYNDROMES[o] for o in "ABCD"]
        b2 = blocks[g + 1] >> 10
        if b2 >> 12 == 2:
            c, d = blocks[g + 2] >> 10, blocks[g + 3] >> 10
            segments[b2 & 15] = bytes([c >> 8, c & 0xFF, d >> 8, d & 0xFF])
    assert b"".join(segments[k] for k in sorted(segments)).split(b"\r")[0].decode() == text


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


#: The M1 tutorial 2 pager net (tests/e2e/tests/acceptance/tutorial_pocsag.rs, T-109): 25 kHz
#: raster, staggered key-ups with channels 1 and 3 simulcast, all 1200 Bd.
POCSAG_TUTORIAL_NET = {
    "center_hz": 152.36e6, "sample_rate": 132300.0,
    "channel_offsets_hz": [-50e3, -25e3, 25e3, 50e3], "bauds_bd": [1200.0] * 4,
    "start_offsets_s": [0.0, 0.21, 0.37, 0.21],
    "rics": [1234560, 1876544, 654320, 1876544], "functions": [0, 3, 3, 3],
    "messages": ["911234", "STANDBY AT GATE 12", "HACKRIFF PAGE TEST", "STANDBY AT GATE 12"],
}


@pytest.mark.skipif(shutil.which("multimon-ng") is None, reason="multimon-ng not installed")
def test_multimon_ng_decodes_the_tutorial_pager_net_per_channel(tmp_path):
    """Oracle (T-109): multimon-ng on the tutorial's own IQ, whole recording, one channel at a time."""
    _, meta, x = load(gen(tmp_path, "pocsag_pagers", seed=95, **POCSAG_TUTORIAL_NET))
    fs = meta["global"]["core:sample_rate"]
    factor = int(round(fs / 22050))
    tt = np.arange(len(x)) / fs
    pages = truths(meta, kind="pocsag-page")
    assert len(pages) == 4
    starts = sorted({ann["core:sample_start"] for ann, _ in pages})
    assert len(starts) == 3, "independent key-ups, simulcast pair together"
    for _, t in pages:
        seg = np.convolve(x * np.exp(-2j * math.pi * t["offset_hz"] * tt),
                          signal.firwin(401, 6000, fs=fs), mode="same")
        inst = np.angle(seg[1:] * np.conj(seg[:-1])) * fs / (2 * math.pi)
        audio = signal.decimate(inst, factor, ftype="fir")
        raw = tmp_path / f"net_{t['offset_hz']:.0f}.raw"
        np.clip(audio / t["deviation_hz"] * 20000, -32000, 32000).astype("<i2").tofile(raw)
        res = subprocess.run(["multimon-ng", "-t", "raw", "-a", "POCSAG1200", "-e", str(raw)],
                             capture_output=True, text=True, timeout=30)
        out = (res.stdout + res.stderr).replace("\x00", "").replace("<NUL>", "")
        lines = [ln for ln in out.splitlines() if ln.startswith("POCSAG1200:")]
        assert len(lines) == 1, f"one page on channel {t['offset_hz']}: {lines}"
        assert f"Address: {t['ric']:>7}" in lines[0]
        assert f"Function: {t['function']}" in lines[0]
        assert t["message_text"].strip() in lines[0]


# ---- ACARS (T-098/T-108, M1 tutorial 3; conventions from acarsdec -- see hkpy.synth.acars) --------


def acars_char_parity_ok(byte: int) -> bool:
    return bin(byte).count("1") % 2 == 1


def kermit_residue(data: bytes) -> int:
    """CRC-16/KERMIT via the reflected table update acarsdec uses (syndrom.h ``update_crc``)."""
    table = []
    for i in range(256):
        c = i
        for _ in range(8):
            c = (c >> 1) ^ 0x8408 if c & 1 else c >> 1
        table.append(c)
    assert table[1] == 0x1189  # acarsdec syndrom.h crc_ccitt_table[1]
    crc = 0
    for b in data:
        crc = (crc >> 8) ^ table[(crc ^ b) & 0xFF]
    return crc


def acarsdec_chips(audio: np.ndarray, fs: float, t: dict, first_bit_sample: int) -> np.ndarray:
    """Coherent MSK chip decisions the way acarsdec's ``demodMSK`` makes them (VCO at 1800 Hz,
    half-sine matched filter over two bits, decisions Re, Im, -Re, -Im), with known timing and the
    carrier phase estimated instead of PLL-tracked. Decoupled from acars.py's tone mapping."""
    baud = t["symbol_rate_bd"]
    sps = fs / baud
    n_bits = t["frame"]["n_bits"]
    tt = np.arange(len(audio)) / fs
    bb = audio * np.exp(-2j * np.pi * 1800.0 * tt)
    spec = np.fft.fft(bb)
    f = np.fft.fftfreq(len(bb), 1 / fs)
    spec[np.abs(f) > 1500.0] = 0
    bb = np.fft.ifft(spec)
    z = np.zeros(n_bits, dtype=complex)
    for k in range(n_bits):
        c = first_bit_sample + (k + 1) * sps  # chip k decided at the end of bit k's tone
        i0, i1 = int(math.ceil(c - sps)), int(math.floor(c + sps))
        idx = np.arange(max(i0, 0), min(i1, len(bb) - 1) + 1)
        h = np.cos(np.pi * (idx - c) / (2 * sps))
        z[k] = np.sum(h * bb[idx]) * np.exp(-1j * k * np.pi / 2)
    phi = np.angle(np.sum(z**2)) / 2
    return (np.real(z * np.exp(-1j * phi)) > 0).astype(np.uint8)


def decode_acars_frame(x: np.ndarray, fs: float, t: dict, prekey_s: float) -> dict:
    audio = np.abs(x)
    start = int(round(prekey_s * fs))
    audio = audio - np.mean(audio[:start])  # unmodulated pre-key carrier level
    chips = acarsdec_chips(audio, fs, t, start)
    sync = np.unpackbits(np.array([0x16, 0x16, 0x01], dtype=np.uint8), bitorder="little")
    for bits in (chips, 1 - chips):  # acarsdec accepts ~SYN too
        hits = [i for i in range(len(bits) - 24) if np.array_equal(bits[i : i + 24], sync)]
        if hits:
            break
    assert hits, "SYN SYN SOH not found in either polarity"
    body = bits[hits[0] + 24 :]
    chars = np.packbits(body[: len(body) // 8 * 8], bitorder="little").tobytes()
    end = chars.index(0x83)  # ETX with its parity bit, acarsdec's constant
    txt, bcs = chars[: end + 1], chars[end + 1 : end + 3]
    for b in txt:
        assert acars_char_parity_ok(b)
    assert kermit_residue(txt + bcs) == 0  # acarsdec's CRC check
    v = bytes(b & 0x7F for b in txt)
    stx = v.index(0x02)
    return {
        "mode": chr(v[0]), "reg": v[1:8].decode("ascii"), "ack": v[8],
        "label": v[9:11].decode("ascii"), "block_id": chr(v[11]),
        "text": v[stx + 1 : -1].decode("ascii"), "crc": bcs[0] | bcs[1] << 8,
        "suffix": chars[end + 3],
    }


def test_acars_message_demodulates_with_valid_crc(tmp_path):
    """SIGNAL-062 M1 tutorial 3: AM + MSK 2400 Bd, ARINC 618 framing, CRC-16/KERMIT, decoded the
    way acarsdec decodes (coherent chips, LSB-first characters, residue over parity-bearing chars)."""
    manifest = gen(tmp_path, "acars_message")
    man = json.loads(manifest.read_text())
    prekey_s = man["params"]["prekey_s"]
    _, meta, x = load(manifest)
    fs = meta["global"]["core:sample_rate"]
    tr = truths(meta, kind="acars-message")
    assert tr
    for ann, t in tr:
        s, n = ann["core:sample_start"], ann["core:sample_count"]
        got = decode_acars_frame(x[s : s + n], fs, t, prekey_s)
        assert got["mode"] == t["fields"]["mode"]
        assert got["reg"] == t["fields"]["reg"]
        assert got["label"] == t["fields"]["label"]
        assert got["block_id"] == t["fields"]["block_id"]
        assert got["text"] == t["fields"]["text"] == t["text_expected"]
        assert f"{got['crc']:04x}" == t["frame"]["crc_hex"]
        assert got["suffix"] == 0x7F  # DEL
        assert t["identity"] == {"type": "acars_reg", "value": t["fields"]["reg"].strip()}


def test_acars_crc_is_kermit_over_parity_bearing_chars():
    """The ACARS block check is CRC-16/KERMIT (check value 0x2189 for "123456789"), computed over
    the transmitted characters with their parity bits, as acarsdec checks it."""
    assert acars_mod.crc16_kermit(b"123456789") == 0x2189
    fr = acars_mod.build_frame("2", ".N12345", "H1", "1", "HELLO")
    assert all(acars_char_parity_ok(b) for b in fr.chars)
    assert kermit_residue(fr.chars + bytes([fr.crc & 0xFF, fr.crc >> 8])) == 0


# ---- LoRa CSS (T-255) --------------------------------------------------------------------------
#
# The generator claims a chirp with no stable frequency. That claim is checked here against the
# waveform's own arithmetic and an independently written demodulator BEFORE any detection result
# is trusted: T-267 shipped a fixture whose pulse shape no receiver could read, and a fixture that
# cannot be demodulated tests nothing.

#: Two (sf, cr, scene overrides) pairs: the fast low-SF end and the slow high-SF, heavily coded end.
LORA_CASES = [
    (7, 1, {"duration_s": 0.15, "first_packet_s": 0.02, "packet_period_s": 0.06,
            "fsk_period_s": 0.05}),
    (9, 4, {}),
]


def test_lora_hamming_is_a_real_code_not_just_a_defined_one():
    """The module claims (7,4) Hamming at 4/7 and SECDED at 4/8. Measure the minimum distance."""
    for cr, claimed in ((1, 2), (2, 2), (3, 3), (4, 4)):
        words = [lora_mod.hamming_encode(n, cr) for n in range(16)]
        assert len(set(words)) == 16, f"4/{4 + cr}: encoding is not injective"
        d = min(bin(a ^ b).count("1") for i, a in enumerate(words) for b in words[i + 1 :])
        assert d == claimed, f"4/{4 + cr}: minimum distance {d}, CODING_SPEC claims {claimed}"
        assert lora_mod.CODING_SPEC["hamming_parity"]["distance"][f"4/{4 + cr}"] == claimed


def test_lora_crc_matches_the_stdlib_xmodem_reference():
    assert lora_mod.crc16_xmodem(b"123456789") == 0x31C3 == binascii.crc_hqx(b"123456789", 0x0000)
    assert lora_mod.CRC16_SPEC["check_123456789"] == "0x31C3"


def test_lora_gray_round_trips_over_every_symbol_value():
    for sf in (7, 9, 12):
        n = lora_mod.n_chips(sf)
        assert sorted(lora_mod.gray(v) for v in range(n)) == list(range(n))
        assert all(lora_mod.ungray(lora_mod.gray(v), sf) == v for v in range(n))


def test_lora_chirp_sweeps_the_whole_channel_once_per_symbol():
    """The waveform itself: slope BW^2/2^SF, span BW, one fold per symbol, and a falling SFD.

    Checked on the instantaneous frequency recovered from the samples by phase differencing, which
    knows nothing about how the modulator built them.
    """
    sf, bw, fs = 9, 125e3, 500e3
    sps = int(round(lora_mod.symbol_duration_s(sf, bw) * fs))
    assert sps == 2048
    slope = lora_mod.chirp_rate_hz_per_s(sf, bw)
    x = lora_mod.modulate(np.zeros(0, dtype=np.int64), sf, bw, fs, preamble_symbols=4)
    inst = np.angle(x[1:] * np.conj(x[:-1])) * fs / (2 * math.pi)

    guard = 32  # drop the samples either side of a fold, where the frequency is discontinuous
    seg = inst[sps + guard : 2 * sps - guard]
    fit = np.polyfit(np.arange(len(seg)) / fs, seg, 1)[0]
    assert abs(fit / slope - 1) < 0.01, f"slope {fit:.3e} Hz/s, expected {slope:.3e}"
    assert abs(seg.max() - bw / 2) < 0.02 * bw and abs(seg.min() + bw / 2) < 0.02 * bw

    # Exactly one fold per symbol boundary, evenly spaced, and none inside a symbol: symbol 0
    # starts at -BW/2 and does not wrap (the sync-word symbols, which do, are excluded here).
    folds = np.flatnonzero(np.diff(inst[: 4 * sps]) < -0.5 * bw)
    assert len(folds) == 4, f"4 preamble symbols should fold 4 times, folded {len(folds)}"
    assert np.allclose(np.diff(folds), sps, atol=2)

    # The SFD is a genuine DOWN-chirp: 4 preamble + 2 sync symbols, then 2.25 falling ones.
    f = lora_mod.packet_frequency(np.zeros(0, dtype=np.int64), sf, bw, fs, preamble_symbols=4)
    sfd = f[6 * sps + guard : 7 * sps - guard]
    down = np.polyfit(np.arange(len(sfd)) / fs, sfd, 1)[0]
    assert down < -0.9 * slope, f"SFD slope {down:.3e} Hz/s is not a down-chirp"


@pytest.mark.parametrize("sf,cr,extra", LORA_CASES)
def test_lora_packets_demodulate_to_the_truth_symbols_and_payload(tmp_path, sf, cr, extra):
    """Dechirp-and-FFT, the standard LoRa demodulator, recovers every hidden parameter's effect."""
    manifest = generate("lora_ism_burst", 255, tmp_path / f"lora-{sf}-{cr}",
                        {**extra, "sf": sf, "coding_rate": cr})
    _, meta, x = load(manifest)
    fs = meta["global"]["core:sample_rate"]
    fc = meta["captures"][0]["core:frequency"]
    st = scenario_truth(meta)
    bw = st["lora"]["bandwidth_hz"]
    sps = int(round(st["lora"]["symbol_duration_s"] * fs))
    packets = truths(meta, kind="lora-packet")
    assert len(packets) == len(st["lora"]["packets"]) >= 2
    assert st["lora"]["coding_rate"] == f"4/{4 + cr}" and st["lora"]["spreading_factor"] == sf
    assert 902e6 < st["lora"]["rf_center_hz"] < 928e6, "the scene must sit in 902-928 MHz US ISM"

    for ann, t in packets:
        s0, n = ann["core:sample_start"], ann["core:sample_count"]
        seg = x[s0 : s0 + n] * np.exp(
            -2j * math.pi * (t["center_hz"] - fc) * np.arange(s0, s0 + n) / fs)
        # The preamble is 2^SF-value-0 up-chirps: an independent check on the base chirp itself.
        pre = lora_mod.demodulate_symbols(seg, sf, bw, fs, int(t["preamble_symbols"]))
        assert np.all(pre == 0), f"preamble demodulated to {pre.tolist()}, expected all zeros"
        lead = int(round((t["preamble_symbols"] + 2 + t["sfd_symbols"]) * sps))
        want = np.array(t["payload_symbols"], dtype=np.int64)
        got = lora_mod.demodulate_symbols(seg, sf, bw, fs, len(want), start=lead)
        assert np.array_equal(got, want), f"{np.sum(got != want)}/{len(want)} symbols wrong"
        payload, crc, valid = lora_mod.decode(got, sf, cr, t["frame"]["payload_bytes"])
        assert payload.hex() == t["frame"]["payload_hex"]
        assert f"{crc:04x}" == t["frame"]["crc_hex"] and valid
        assert t["identity"] == {"type": "lora_payload", "value": t["frame"]["payload_hex"]}
        assert t["frame"]["n_symbols"] == len(want) == lora_mod.symbol_count(
            t["frame"]["payload_bytes"], sf, cr)


def _occupied_span_hz(seg, fs, nfft, limit_hz):
    """Median per-frame occupied width inside ``+/- limit_hz`` of baseband: the *contiguous* run of
    bins around that frame's peak staying within 10 dB of it.

    Both restrictions matter. Band-limiting keeps another channel's emitter (or its alias, which is
    what the first draft of this measured) out of the answer; taking the contiguous run around the
    peak rather than the outermost hot bins means a second lobe cannot widen it either.
    """
    spans = []
    win = np.hanning(nfft)
    keep = np.abs(np.fft.fftshift(np.fft.fftfreq(nfft, 1.0 / fs))) <= limit_hz
    for i in range(0, len(seg) - nfft, nfft):
        p = np.abs(np.fft.fftshift(np.fft.fft(seg[i : i + nfft] * win))) ** 2
        p = p[keep]
        hot = p >= p.max() / 10.0
        lo = hi = int(np.argmax(p))
        while lo > 0 and hot[lo - 1]:
            lo -= 1
        while hi < len(p) - 1 and hot[hi + 1]:
            hi += 1
        spans.append((hi - lo + 1) * fs / nfft)
    return float(np.median(spans))


def test_lora_box_is_a_bounding_box_far_wider_than_the_emission(tmp_path):
    """ADR-0017 §1.3 as a number rather than a caveat.

    One ``(f_lo, f_hi)`` per detection cannot describe a swept carrier, so the annotation box is
    the sweep's hull. How much that costs is ``symbol_duration / analysis_frame`` — stated in the
    truth as an identity, and measured here from the samples. Threshold fixed a priori: the chirp
    sweeps a quarter of the box per frame at these parameters, so half the box allows a factor of
    two for window leakage and for the minority of frames that straddle a fold.
    """
    manifest = generate("lora_ism_burst", 255, tmp_path / "lora-box", {"n_packets": 1})
    _, meta, x = load(manifest)
    fs = meta["global"]["core:sample_rate"]
    fc = meta["captures"][0]["core:frequency"]
    [(ann, t)] = truths(meta, kind="lora-packet")
    bw = t["bandwidth_hz"]
    sweep = t["sweep"]
    assert ann["core:freq_upper_edge"] - ann["core:freq_lower_edge"] == pytest.approx(bw)
    assert sweep["stable_frequency"] is False and sweep["box_is_bounding_box"] is True
    assert sweep["box_to_instantaneous_ratio"] == pytest.approx(
        t["symbol_duration_s"] / sweep["instantaneous_frame_s"])
    assert sweep["box_to_instantaneous_ratio"] >= 4.0

    nfft = int(round(sweep["instantaneous_frame_s"] * fs))
    s0, n = ann["core:sample_start"], ann["core:sample_count"]
    seg = x[s0 : s0 + n] * np.exp(
        -2j * math.pi * (t["center_hz"] - fc) * np.arange(s0, s0 + n) / fs)
    occupied = _occupied_span_hz(seg, fs, nfft, bw / 2)
    assert occupied < 0.5 * bw, f"occupied {occupied:.0f} Hz of a {bw:.0f} Hz box"
    assert occupied == pytest.approx(sweep["instantaneous_bandwidth_hz"], rel=0.6)

    # Control: the steady carrier in the same recording occupies a few bins, and its box is a line.
    [(cw_ann, cw)] = truths(meta, kind="cw")
    cw_seg = x[:n] * np.exp(-2j * math.pi * (cw["center_hz"] - fc) * np.arange(n) / fs)
    assert _occupied_span_hz(cw_seg, fs, nfft, bw / 2) < 0.05 * bw
    assert cw_ann["core:freq_lower_edge"] == cw_ann["core:freq_upper_edge"]


def test_lora_sweep_polyline_is_the_frequency_the_box_cannot_draw(tmp_path):
    """The truth carries the polyline ADR-0017 §1.3 says the model cannot render. Check it is real.

    Per-frame spectral centroid tracks a linear chirp's mid-frame frequency, so it can be compared
    with the polyline directly. Frames straddling a fold are excluded — identified from the
    polyline, not from a result. Tolerance fixed a priori at 0.1 x BW: the centroid of the band a
    chirp sweeps within one frame is unbiased to within a few kHz, and 0.1 x BW is still an order
    of magnitude tighter than the bounding box the polyline is being contrasted with.
    """
    manifest = generate("lora_ism_burst", 255, tmp_path / "lora-poly", {"n_packets": 1})
    _, meta, x = load(manifest)
    fs = meta["global"]["core:sample_rate"]
    fc = meta["captures"][0]["core:frequency"]
    [(ann, t)] = truths(meta, kind="lora-packet")
    bw, slope = t["bandwidth_hz"], t["sweep"]["chirp_rate_hz_per_s"]
    poly = np.array(t["sweep_polyline"], dtype=float)
    assert len(poly) > 50
    assert np.all(poly[:, 1] >= t["sweep"]["f_low_hz"] - 1.0)
    assert np.all(poly[:, 1] <= t["sweep"]["f_high_hz"] + 1.0)

    s0, n = ann["core:sample_start"], ann["core:sample_count"]
    seg = x[s0 : s0 + n] * np.exp(
        -2j * math.pi * (t["center_hz"] - fc) * np.arange(s0, s0 + n) / fs)
    # Frames on the polyline's own grid, so each frame has a bracketing polyline point either side.
    nfft = int(round(t["polyline_step_s"] * fs))
    frame_s = nfft / fs
    assert slope * frame_s < bw, "a frame must not sweep the whole channel, or nothing is trackable"
    bins = np.fft.fftshift(np.fft.fftfreq(nfft, 1.0 / fs))
    inband = np.abs(bins) <= bw / 2  # the channel the box already gives; noise outside it is not ours
    win = np.hanning(nfft)
    errors = []
    for i in range(0, n - nfft, nfft):
        t0 = (s0 + i) / fs
        j = int(np.searchsorted(poly[:, 0], t0))
        if j < 1 or j + 1 >= len(poly) or np.any(np.diff(poly[j - 1 : j + 2, 1]) < 0):
            continue  # a fold lands in or beside this frame; the mid-frame frequency is undefined
        p = np.abs(np.fft.fftshift(np.fft.fft(seg[i : i + nfft] * win))) ** 2
        centroid = float(np.sum(bins[inband] * p[inband]) / np.sum(p[inband])) + t["center_hz"]
        errors.append(abs(centroid - float(np.interp(t0 + frame_s / 2, poly[:, 0], poly[:, 1]))))
    assert len(errors) > 15, f"only {len(errors)} fold-free frames to compare"
    assert np.median(errors) < 0.1 * bw, f"median polyline error {np.median(errors):.0f} Hz"


def test_lora_scene_separates_stable_from_swept_and_persistent_from_ephemeral(tmp_path):
    """The three species of the scene, as the truth describes them (invariant 1's two axes)."""
    manifest = generate("lora_ism_burst", 255, tmp_path / "lora-contrast",
                        SMALL["lora_ism_burst"])
    _, meta, _ = load(manifest)
    st = scenario_truth(meta)
    n = st["n_samples"]

    [(cw_ann, cw)] = truths(meta, kind="cw")
    assert cw_ann["core:sample_count"] == n and cw["stable_frequency"] and cw["persistent"]

    fsks = truths(meta, kind="fsk-burst")
    assert len(fsks) == st["contrast"]["fsk"]["n_bursts"] >= 2
    for ann, t in fsks:
        assert 0 < ann["core:sample_count"] < n / 4
        assert t["stable_frequency"] is True and t["persistent"] is False

    loras = truths(meta, kind="lora-packet")
    assert len(loras) >= 2
    for ann, t in loras:
        assert 0 < ann["core:sample_count"] < n / 2
        assert t["sweep"]["stable_frequency"] is False
        assert t["spreading_factor"] == SMALL["lora_ism_burst"]["sf"]

    # The three occupy three disjoint channels, so nothing here is a blend of two species.
    species: dict[str, tuple[float, float]] = {}
    for ann, t in truths(meta, role="emission"):
        lo, hi = species.get(t["kind"], (math.inf, -math.inf))
        species[t["kind"]] = (min(lo, ann["core:freq_lower_edge"]),
                              max(hi, ann["core:freq_upper_edge"]))
    assert set(species) == {"cw", "fsk-burst", "lora-packet"}
    boxes = sorted(species.values())
    assert all(boxes[i][1] < boxes[i + 1][0] for i in range(2)), species


def test_dmr_csbk_block_layout_and_masked_crc():
    """T-271: a CSBK's fields sum to 96 bits, and its CRC carries the DMR mask.

    The mask is the verified half. An unmasked CRC-CCITT is a perfectly valid CRC and the wrong
    one for DMR, so it must not check out -- that is what the assertion below is for.
    """
    from hkpy.synth import trunking as tk

    payload = tk.dmr_grant_payload(5, 1, 2468, 1357, flags=0b101)
    assert len(payload) == 8
    block = tk.csbk(tk.CSBKO_BTV_GRANT, payload)
    assert len(block) == tk.CSBK_BYTES
    assert block[0] & 0x3F == tk.CSBKO_BTV_GRANT and block[1] == 0
    assert block[2:10] == payload
    stored = int.from_bytes(block[10:], "big")
    assert stored == tk.crc16_ccitt_zero(block[:10]) ^ tk.CSBK_CRC_MASK
    assert stored != tk.crc16_ccitt_zero(block[:10]), "the mask was not applied"

    # The grant field split, read back by hand: LPCN(12) TS(1) flags(3) target(24) source(24).
    v = int.from_bytes(payload, "big")
    assert v >> 52 == 5
    assert (v >> 51) & 1 == 1
    assert (v >> 48) & 7 == 0b101
    assert (v >> 24) & 0xFF_FFFF == 2468
    assert v & 0xFF_FFFF == 1357


def test_dmr_syncs_are_outer_symbols_only_and_exact_complements():
    """The sync constants verified by arithmetic rather than by recitation (T-271).

    Two published hex words must reproduce two documented properties of DMR's sync patterns: every
    symbol is an outer one, and the base-station data and voice syncs are exact dibit complements.
    A transcription error survives neither.
    """
    from hkpy.synth import trunking as tk

    level = {0b01: 3, 0b00: 1, 0b10: -1, 0b11: -3}
    data = tk.dmr_sync_dibits()
    voice = tk.dmr_sync_dibits(voice=True)
    assert len(data) == 24 and len(voice) == 24
    for d in list(data) + list(voice):
        assert abs(level[int(d)]) == 3, "a DMR sync never uses an inner symbol"
    for a, b in zip(data, voice):
        assert level[int(a)] == -level[int(b)], "the two BS syncs are not complements"


def test_the_nxdn_constants_reproduce_the_published_symbol_sequences():
    """T-345: two published tables, in different notations, have to agree under the dibit map.

    The specification prints the frame sync word BOTH as hex and as symbols, and prints the
    Preamble as hex while printing the Post field only as symbols. Both pairs have to close, which
    is what verifies the constants and the dibit map together -- a transcription error survives
    neither.
    """
    from hkpy.synth import trunking as tk

    level = {0b01: 3, 0b00: 1, 0b10: -1, 0b11: -3}
    fsw = tk.nxdn_sync_dibits()
    assert len(fsw) == 10, "the frame sync word is 10 symbols (20 bits)"
    assert [level[int(d)] for d in fsw] == [-3, 1, -3, 3, -3, -3, 3, 3, -1, 3]
    post = tk.nxdn_post_dibits()
    assert len(post) == 12
    assert [level[int(d)] for d in post] == [3, 3, 3, -3, 3, -3, 3, 3, -3, -3, -3, 3]

    # The CAC coding chain, as arithmetic: 152 + 3 = 155, + 16 = 171, + 4 = 175, x2 = 350,
    # punctured 12-of-14 = 300, interleaved 25 x 12 = 300.
    assert tk.NXDN_L3_BITS + 3 == 155
    assert 155 + 16 + 4 == 175
    assert 175 * 2 * 12 // 14 == tk.NXDN_CAC_BITS
    assert tk.NXDN_INTERLEAVE_DEPTH * tk.NXDN_INTERLEAVE_WIDTH == tk.NXDN_CAC_BITS
    # And the frame: 20 + 16 + 300 + 24 + 24 = 384 bits = 192 symbols.
    assert 20 + 16 + tk.NXDN_CAC_BITS + 24 + 24 == tk.NXDN_FRAME_DIBITS * 2
    assert len(fsw) + tk.NXDN_SCRAMBLED_DIBITS == tk.NXDN_FRAME_DIBITS

    # The scrambler is its own inverse and only ever flips a symbol's SIGN, so an outer symbol stays
    # outer -- which is why the decoder's LICH check does not depend on the shift direction.
    import numpy as np
    src = np.arange(tk.NXDN_SCRAMBLED_DIBITS, dtype=np.uint8) % 4
    once = tk.nxdn_scramble(src)
    assert np.array_equal(tk.nxdn_scramble(once), src)
    assert not np.array_equal(once, src)
    assert np.array_equal(src & 1, once & 1), "the scrambler altered a dibit's low bit"

    # A LICH is eight OUTER symbols, and a frame is 192.
    lich = tk.nxdn_lich_dibits(0b000_0001)
    assert len(lich) == 8
    assert all(int(d) in (0b01, 0b11) for d in lich)
    frame = tk.nxdn_frame_dibits(tk.nxdn_message(tk.NXDN_MSG_SITE_INFO, bytes(17)),
                                 np.random.default_rng(0))
    assert len(frame) == tk.NXDN_FRAME_DIBITS


def test_nxdn_scene_baits_the_trap_it_asks_the_decoder_to_refuse(tmp_path):
    """T-345: the assigned channel number is derived from a frequency that carries real traffic.

    An NXDN Type-C assignment names a 10-bit channel NUMBER and the air interface defines no
    mapping from one to hertz -- the map is configured in the radio. This scene only proves that
    refusal if refusing *costs* something, which is why the frequency an assumed 12.5 kHz band plan
    would produce is (a) inside the window the radio holds and (b) carrying keyings.
    """
    from hkpy.synth import trunking as tk

    manifest = gen(tmp_path, "trunk_nxdn_control_channel")
    _, meta, _ = load(manifest)
    t = scenario_truth(meta)["trunking"]
    d = t["nxdn"]

    # The channel number follows from the trap frequency, never the other way round.
    assert (d["assumed_base_hz"] + d["assumed_spacing_hz"] * d["grant_channel"]
            == d["wrong_frequency_if_channel_assumed_hz"])
    assert 1 <= d["grant_channel"] <= tk.NXDN_CHANNEL_MAX
    # Inside the window, so a guessing decoder could have followed it.
    assert abs(d["trap_offset_hz"]) < 0.4 * d["sample_rate_hz"]
    # And carrying traffic, so it would have been rewarded with a convincing call record.
    keyings = d["trap_keyings_s"]
    assert keyings, "the trap channel carries no traffic, so refusing costs nothing"
    tol_s = 1.0 / d["sample_rate_hz"]
    for a, b in keyings[:-1]:
        assert abs((b - a) - d["trap_on_s"]) <= tol_s

    # The SECOND assignment baits the same mistake again, and the frequency it points at carries one
    # of the scene's bursty NBFM neighbours -- so even the second guess would find something.
    assert d["second_channel"] != d["grant_channel"]
    second_hz = d["wrong_frequency_for_second_channel_hz"]
    assert round((second_hz - d["assumed_base_hz"]) / t["raster_hz"]) in t["nbfm_channels"]
    assert t["n_nbfm_bursts"] > 0

    # Both kinds of assignment are exercised, plus messages the decoder does not read.
    assert d["counts"]["vcall-assgn"] >= 1 and d["counts"]["vcall-assgn-dup"] >= 1
    assert d["counts"]["vcall-assgn-individual"] >= 1 and d["counts"]["dcall-assgn"] >= 1
    assert d["counts"]["site-info"] >= 1, "nothing corroborates the system as Type-C"
    # Asserted on the cycle rather than on the counts: this fixture is short enough that the last
    # message of the cycle may not land in it, and that is a fact about the duration, not the scene.
    assert "other" in d["cycle"], "the stream is only messages the decoder understands"

    # The decoy is still there and still unconfirmable -- now under three framings, not two.
    assert t["continuous_decoy"]["expected_confirmed"] is False
    assert t["control_channel"]["sync_hex"] == tk.NXDN_FSW_HEX


def test_dmr_scene_baits_the_trap_it_asks_the_decoder_to_refuse(tmp_path):
    """T-271: the granted LPCN is derived from a frequency that carries real traffic.

    A DMR Tier III grant names a logical channel and nothing else, and no on-air channel-parameter
    announcement could be corroborated -- so the number resolves to nothing. This scene only proves
    that refusal if refusing *costs* something, which is why the frequency an assumed 12.5 kHz band
    plan would produce is (a) inside the window the radio holds and (b) carrying keyings.
    """
    from hkpy.synth import trunking as tk

    manifest = gen(tmp_path, "trunk_dmr_control_channel")
    _, meta, _ = load(manifest)
    t = scenario_truth(meta)["trunking"]
    d = t["dmr"]

    # The LPCN follows from the trap frequency, never the other way round.
    assert (d["assumed_base_hz"] + d["assumed_spacing_hz"] * d["grant_lpcn"]
            == d["wrong_frequency_if_lpcn_assumed_hz"])
    # Inside the window, so a guessing decoder could have followed it.
    assert abs(d["trap_offset_hz"]) < 0.4 * d["sample_rate_hz"]
    # And carrying traffic, so it would have been rewarded with a convincing call record.
    keyings = d["trap_keyings_s"]
    assert keyings, "the trap channel carries no traffic, so refusing costs nothing"
    tol_s = 1.0 / d["sample_rate_hz"]
    for a, b in keyings[:-1]:
        assert abs((b - a) - d["trap_on_s"]) <= tol_s

    # Both timeslots are exercised and the two grants name different logical channels, so a
    # decoder cannot satisfy an assertion with the wrong one.
    assert d["second_lpcn"] != d["grant_lpcn"]
    assert {d["grant_timeslot"], d["second_timeslot"]} == {0, 1}
    assert d["counts"]["btv-grant"] >= 1 and d["counts"]["p-grant"] >= 1
    assert d["counts"]["c-aloha"] >= 1, "nothing corroborates the system as trunked"
    assert d["counts"]["other"] >= 1, "the stream is only messages the decoder understands"

    # The decoy is still there and still unconfirmable -- now under two framings, not one.
    assert t["continuous_decoy"]["expected_confirmed"] is False
    assert t["control_channel"]["sync_hex"] == tk.DMR_BS_DATA_SYNC_HEX


# ---- retune_diversity (T-586) -----------------------------------------------------------------


def test_retune_diversity_emitters_stay_put_and_artefacts_move_with_the_lo(tmp_path):
    """The fixture's own physics, checked in the IQ rather than only in the annotations.

    A real emitter must sit at the same ABSOLUTE frequency in every capture, which means a
    different baseband offset in each; an LO-relative artefact must sit at the same BASEBAND
    offset in every capture, which means a different absolute frequency in each. If the generator
    ever stopped doing that, the acceptance suite built on it would be measuring nothing.
    """
    manifest = gen(tmp_path, "retune_diversity")
    _, meta, x = load(manifest, 0)
    fs = meta["global"]["core:sample_rate"]
    st = scenario_truth(meta)["retune_diversity"]
    centres = [c["core:frequency"] for c in meta["captures"]]
    assert centres == st["centers_hz"] and len(centres) >= 2

    for cap in meta["captures"]:
        start = cap["core:sample_start"]
        seg = x[start:start + int(round(SMALL["retune_diversity"]["dwell_s"] * fs))]
        centre = cap["core:frequency"]
        floor = median_floor_dbfs(seg, fs)
        for f_abs in st["emitters_hz"]:
            # Fixed absolute frequency: the baseband offset changes with the centre.
            power = tone_power_dbfs(seg, fs, f_abs - centre)
            assert power > floor + 6, f"emitter {f_abs} missing at centre {centre}: {power} dBFS"
        for offset in st["lo_relative_offsets_hz"]:
            # Fixed LO offset: the absolute frequency changes with the centre.
            power = tone_power_dbfs(seg, fs, offset)
            assert power > floor + 6, f"artefact at offset {offset} missing at centre {centre}"
        # T-599: an IQ image, fixed in the invariant f - 2*f_LO, so its baseband offset (and
        # absolute frequency) changes with the centre at TWICE the LO's own step.
        image_offset = 2.0 * centre - st["image_source_hz"] - centre
        power = tone_power_dbfs(seg, fs, image_offset)
        assert power > floor + 6, f"IQ image missing at centre {centre}"

    # The annotations say the same thing.
    for _, t in truths(meta, role="emission"):
        assert t["center_hz"] in st["emitters_hz"]
    for _, t in truths(meta, role="artefact"):
        assert t["kind"] in ("dc-offset", "lo-spur", "iq-image")
        if t["kind"] == "iq-image":
            # Fixed in the invariant f - 2*f_LO, not in a fixed LO offset.
            centre = t["center_hz"] - t["offset_hz"]
            assert centre in centres
            assert t["center_hz"] - 2.0 * centre == pytest.approx(-st["image_source_hz"])
        else:
            assert t["offset_hz"] in st["lo_relative_offsets_hz"]
            assert t["center_hz"] - t["offset_hz"] in centres


def test_retune_diversity_refuses_a_layout_whose_lines_would_merge(tmp_path):
    """The generator's a-priori separation guard, so a merged pair can never look like a moving
    signal."""
    with pytest.raises(ValueError, match="minimum separation"):
        gen(tmp_path, "retune_diversity", lo_spur_offset_hz=20e3)
