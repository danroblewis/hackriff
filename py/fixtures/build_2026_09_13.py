"""Regenerate the 2026-09-13 HackRF fixture set from the external store.

    uv run --project py python py/fixtures/build_2026_09_13.py [--store DIR] [--only NAME ...]
        [--scratch DIR] [--no-hash-store]

Inputs (read-only): ``<store>/2026-09-13/`` (``$HACKRIFF_FIXTURE_STORE``, default fixtures/store)
and the S4 emitter tables copied to ``fixtures/hackrf/2026-09-13/labels/``.
Outputs: ``fixtures/hackrf/2026-09-13/*.sigmf-{meta,data}``, ``fixtures/sweeps/2026-09-13/``,
``fixtures/manifest.json``; truth JSON and decode logs go to ``--scratch``.

Deterministic given the store bytes (no randomness). Legal: receive-only captures; truth is
PHY metadata only. The 902-928 MHz traffic is unidentified third-party traffic: no payload bits
are decoded into or stored with the fixture. RDS PI/PS are public broadcast identity.
"""

from __future__ import annotations

import argparse
import csv
import math
import shutil
import sys
import tempfile
from pathlib import Path
from typing import Any

import numpy as np

import annotate
import fsk_ref
import fxlib
import rds_ref
import trim
from fxlib import CLIP_COUNT_KEY, sigmf

DATE = "2026-09-13"
OUT = fxlib.FIXTURES / "hackrf" / DATE
SWEEPS_OUT = fxlib.FIXTURES / "sweeps" / DATE
LABELS = OUT / "labels"
LICENSE = "project-owned capture"
BUILDER = "py/fixtures/build_2026_09_13.py"
#: HackRF sample clock/LO error from the 19 kHz pilot of fm_100p8M_2p4M_l32g30a1 (spike S5,
#: re-measured by rds_ref on the full capture). Frequencies read in the capture's clock are low by
#: this much, so an emitter at f_rf appears at f_rf + CLOCK_PPM * 1e-6 * f_tuner.
CLOCK_PPM_NOMINAL = -6.8
LEGAL = ("Receive-only capture. Truth is PHY/metadata only; no third-party payload content is "
         "decoded or stored (CLAUDE.md legal guardrails).")
KNOWN_FM_MHZ = (93.3, 94.9, 96.5, 98.9, 101.3, 104.5, 106.9)
COMB_SPACING_HZ = 209478.60159821565  # spikes/s4-detection-overload/results/metrics.json combs


def fixture_specs() -> list[dict[str, Any]]:
    return [
        dict(name="fm_100p8M_2p4M_l32g30a1_t1p5_5s", source="fm_100p8M_2p4M_l32g30a1",
             start_s=1.5, duration_s=5.0, use_cases=["SIGNAL-062"], builder=truth_fm,
             summary="FM 101.3 MHz (+500 kHz) with 19 kHz pilot and RDS; 100.000 MHz reference-harmonic spur"),
        # 42.3 s: most bursts >= 20 dB (S5 box SNR) with both rates present; many weak ones too.
        dict(name="ism_915M_10M_l24g30a1_t42p3_1p2s", source="ism_915M_10M_l24g30a1",
             start_s=42.3, duration_s=1.2, use_cases=["AWARE-036"], builder=truth_915,
             summary="unidentified FHSS 2-FSK bursts, 100/150 kbit/s, 200 kHz raster (AWARE-036 recorded companion)"),
        dict(name="urban_98M_20M_l32g30a1_t1p0_0p6s", source="urban_98M_20M_l32g30a1",
             start_s=1.0, duration_s=0.6, use_cases=["AWARE-042", "SPACE-050"], builder=truth_urban,
             spike="S4", summary="FM band at high gain: ADC clipping (S4 overload case), 209.48 kHz comb"),
        dict(name="urban_98M_20M_l24g20a0_t1p0_0p6s", source="urban_98M_20M_l24g20a0",
             start_s=1.0, duration_s=0.6, use_cases=["AWARE-042", "SPACE-050"], builder=truth_urban,
             spike="S4", summary="FM band at mid gain, same time offset as the clipped window (S4 gain-step pair)"),
        dict(name="ism_433p62M_2M_l24g30a1_t162p0_6s", source="ism_433p62M_2M_l24g30a1",
             start_s=162.0, duration_s=6.0, use_cases=["AWARE-042", "SPACE-050"], builder=truth_433,
             summary="noise-only negative control at 433.62 MHz: measured floor, no emissions"),
    ]


EXTERNAL_USE_CASES = {
    "fm_100p8M_2p4M_l32g30a1": ["SIGNAL-062"],
    "fm_100p8M_2p4M_l16g20a0": ["SIGNAL-062"],
    "ism_915M_10M_l24g30a1": ["AWARE-036"],
    "ism_433p62M_2M_l24g30a1": ["AWARE-036", "AWARE-042", "SPACE-050"],
    "urban_98M_20M_l8g10a0": ["AWARE-042", "SPACE-050"],
    "urban_98M_20M_l24g20a0": ["AWARE-042", "SPACE-050"],
    "urban_98M_20M_l32g30a1": ["AWARE-042", "SPACE-050"],
    "urban_99M_20M_l24g20a0": ["AWARE-042", "SPACE-050"],
    "urban_915M_20M_l8g10a0": ["AWARE-042"],
    "urban_915M_20M_l32g30a1": ["AWARE-042"],
    "adsb_1090M_2p4M_l32g40a1": ["SIGNAL-001"],
    "adsb_1090M_2p4M_l40g40a1": ["SIGNAL-001"],
}
SWEEP_USE_CASES = ["SPACE-050", "AWARE-042"]

BLOCKED = [
    dict(name="adsb_1090M_fixture", use_cases=["SIGNAL-001"],
         reason=("0 CRC-valid Mode S messages in adsb_1090M_2p4M_l32g40a1 and _l40g40a1 (readsb): the "
                 "attached antenna cannot hear 1090 MHz. Needs a 1090 MHz antenna and a recapture. "
                 "SIGNAL-001 uses the synthetic adsb_squitter scenario meanwhile."),
         source=["store/2026-09-13/adsb_1090M_2p4M_l32g40a1.sigmf-data",
                 "store/2026-09-13/adsb_1090M_2p4M_l40g40a1.sigmf-data"]),
    dict(name="terminated_input_spur_map", use_cases=["AWARE-042", "SPACE-050"],
         reason=("Needs the user to swap in a 50 ohm terminator and capture per gain state across the "
                 "tuning range (C05 spur map; S4 recommendation 3). Physical RF change: interactive only."),
         source=[]),
]


# ---- helpers ---------------------------------------------------------------------------------


def db(x: float) -> float:
    return float(10 * math.log10(max(x, 1e-30)))


def scenario_truth(spec, meta, src_meta, src_sha, n, fs, extra) -> dict[str, Any]:
    cap = meta["captures"][0]
    prov = meta["global"][sigmf.PROVENANCE_KEY]
    clips = int(sum(c.get(CLIP_COUNT_KEY, 0) for c in meta["captures"]))
    t = dict(
        role="scenario", kind="capture", recording=spec["name"], use_cases=spec["use_cases"],
        generator=BUILDER, source={
            "capture": spec["source"], "path": f"store/{DATE}/{spec['source']}.sigmf-data",
            "sha256": src_sha, "sample_start": cap["core:global_index"], "sample_count": n,
            "start_s": cap["core:global_index"] / fs, "datetime": src_meta["captures"][0]["core:datetime"],
        },
        datatype=meta["global"]["core:datatype"], sample_rate_hz=fs, n_samples=n, duration_s=n / fs,
        dbfs_reference="full scale = 127 codes per component; dBFS = 10 log10(mean |x|^2)",
        calibration="uncalibrated (no dBm): antenna unknown, as attached by the user",
        clip_count=clips, clip_fraction=clips / n, overload=prov["overload"],
        overload_rule=f"clip_fraction > {fxlib.OVERLOAD_CLIP_FRACTION:g} (component at -128 or 127)",
        quantisation_limited=prov["quantisation_limited"],
        quantisation_noise_dbfs_per_hz=fxlib.quantisation_floor_dbfs_per_hz(fs),
        clock={"source": "internal", "ppm_measured": CLOCK_PPM_NOMINAL,
               "method": "19 kHz FM pilot, fm_100p8M_2p4M_l32g30a1 (spike S5)",
               "lo_offset_at_tuner_hz": CLOCK_PPM_NOMINAL * 1e-6 * cap["core:frequency"]},
        legal=LEGAL,
    )
    t.update(extra)
    return t


def whole(n: int) -> dict[str, int]:
    return {"sample_start": 0, "sample_count": n}


def dc_item(data: Path, fc: float, n: int, fs: float) -> dict[str, Any]:
    step = max(1, n // 2_000_000)
    x = fxlib.read_ci8(data, 0, n)[::step]
    i_off, q_off = float(np.mean(x.real)), float(np.mean(x.imag))
    return dict(**whole(n), freq_lower_edge=fc - 5e3, freq_upper_edge=fc + 5e3, label="DC",
                truth=dict(role="artefact", kind="dc-offset", reason="dc", center_hz=fc, offset_hz=0.0,
                           i_offset=i_off / 127, q_offset=q_off / 127, i_offset_codes=i_off,
                           q_offset_codes=q_off, power_dbfs=db((i_off**2 + q_off**2) / 127**2)))


def window_psd(data: Path, n: int, nfft: int) -> np.ndarray:
    """Mean periodogram (fftshifted), full-scale power per bin, over the whole span."""
    return fxlib.floor_psd(data, 0, n, nfft=nfft, max_frames=n // nfft)


def band_measure(psd: np.ndarray, fs: float, f_lo: float, f_hi: float, guard_hz: float = 500e3):
    nfft = len(psd)
    f = (np.arange(nfft) - nfft // 2) * fs / nfft
    inb = (f >= f_lo) & (f <= f_hi)
    ref = (np.abs(f - 0.5 * (f_lo + f_hi)) <= guard_hz) & ~inb
    if not inb.any():
        k = int(np.argmin(np.abs(f - 0.5 * (f_lo + f_hi))))
        inb[k] = True
    floor = float(np.median(psd[ref])) if ref.any() else float(np.median(psd))
    peak = float(psd[inb].max())
    # floor_psd bins hold density * fs, so total power = sum / nfft (Parseval, any window).
    return dict(peak_snr_db=db(peak / floor), power_dbfs=db(float(psd[inb].sum()) / nfft),
                peak_offset_hz=float(f[inb][np.argmax(psd[inb])]))


# ---- FM + RDS ---------------------------------------------------------------------------------


def truth_fm(spec, meta_path, data, meta, n, fs, fc, ctx) -> list[dict[str, Any]]:
    src_data = ctx["store"] / f"{spec['source']}.sigmf-data"
    full = rds_ref.decode_ci8(str(src_data), fs, 500e3)
    win = rds_ref.decode_ci8(str(data), fs, 500e3)
    ctx["logs"][f"{spec['name']}.rds_full.json"] = full
    ctx["logs"][f"{spec['name']}.rds_window.json"] = win
    ppm = full["clock_ppm_from_pilot"]
    lo_off = ppm * 1e-6 * fc
    center = fc + 500e3 + win["carrier_offset_hz"]
    psd = window_psd(data, n, 8192)
    f = (np.arange(8192) - 4096) * fs / 8192
    noise = float(np.median(psd))
    ch = np.abs(f - (center - fc)) <= 100e3
    s = float(psd[ch].sum() - noise * ch.sum())
    snr = db(s / (noise * ch.sum()))
    rds = {
        "pi_hex": full["pi_hex"], "pi": full["pi"], "ps": full["ps"], "ps_dynamic": full["ps_dynamic"],
        "ps_frames": full["ps_frames"], "ps_frames_window": win["ps_frames"],
        "ps_frame_log_window": [{"t_s": e["t_s"], "ps": e["ps"]} for e in win["ps_frame_log"]],
        "pty": full["pty"], "tp": full["tp"], "group_types": full["group_types"],
        "group_types_window": win["group_types"], "bitrate_bd": 1187.5,
        "bitrate_bd_in_sample_clock": full["bitrate_bd_in_sample_clock"], "subcarrier_hz": 57000.0,
        "decode_full_capture": {k: full[k] for k in ("groups_decoded", "groups_on_lattice", "blocks_on_lattice",
                                                     "blocks_ok", "block_error_rate")} | {"duration_s": 30.0},
        "decode_window": {k: win[k] for k in ("groups_decoded", "groups_on_lattice", "blocks_on_lattice",
                                              "blocks_ok", "block_error_rate", "first_valid_group_s")},
        "block_error_rate_definition": ("fraction of 26-bit blocks with a wrong offset-word syndrome on the "
                                        "group lattice between the first and last CRC-valid group, re-anchored "
                                        "on each valid group"),
        "decoder": "py/fixtures/rds_ref.py (promoted T-023 reference decoder)",
        "note": "PS scrolls (dynamic PS, song/artist text): assert PI; PS frames are a set, not one string",
    }
    station = dict(
        freq_lower_edge=center - 100e3, freq_upper_edge=center + 100e3, label="FM 101.3 MHz",
        **whole(n),
        truth=dict(role="emission", kind="wfm-broadcast", modulation="WFM", center_hz=center,
                   rf_center_hz=center - lo_off, channel_hz=101.3e6, offset_hz=center - fc,
                   lo_offset_hz=lo_off, bandwidth_hz=200e3, snr_db=snr,
                   snr_definition="(P in +-100 kHz - N0*B)/(N0*B), N0 = median PSD bin of the window",
                   carrier_offset_from_channel_hz=center - 101.3e6, stereo=True,
                   pilot={"present": True, "frequency_hz": win["pilot_hz"],
                          "deviation_hz": win["pilot_deviation_hz"]},
                   rds=rds, identity={"type": "rds_pi", "value": full["pi_hex"]}))
    k = np.abs(f - (100e6 - fc)) <= 3e3
    spur_off = float(f[k][np.argmax(psd[k])])
    kk = np.abs(f - spur_off) <= 4 * fs / 8192
    spur_power = db(float(np.clip(psd[kk] - noise, 0, None).sum()) / 8192)
    spur = dict(
        **whole(n), freq_lower_edge=100e6 - 5e3, freq_upper_edge=100e6 + 5e3, label="spur 100.000 MHz",
        truth=dict(role="artefact", kind="spur", reason="ref_harmonic", center_hz=fc + spur_off,
                   offset_hz=spur_off, harmonic_n=10, spur_step_hz=10e6, tuner_center_hz=fc,
                   power_dbfs=spur_power, peak_over_floor_db=db(float(psd[k].max()) / noise),
                   note="internal 10 MHz reference harmonic; stays at absolute frequency on retune (S4)"))
    ctx["summary"] = (f"PI {full['pi_hex']}; PS frames {list(full['ps_frames'])}; "
                      f"BLER {full['block_error_rate']:.3f} ({full['groups_decoded']} groups/30 s), "
                      f"window {win['groups_decoded']} groups BLER {win['block_error_rate']:.3f}; pilot "
                      f"{win['pilot_hz']:.3f} Hz; clock {ppm:+.2f} ppm")
    ctx["scenario_extra"] = {"clock_ppm_this_capture": ppm,
                             "annotation_completeness": "partial: the 101.3 MHz station, the 100.000 MHz spur and DC only"}
    return [station, spur, dc_item(data, fc, n, fs)]


# ---- 915 MHz FHSS FSK --------------------------------------------------------------------------


def truth_915(spec, meta_path, data, meta, n, fs, fc, ctx) -> list[dict[str, Any]]:
    bursts = fsk_ref.detect_bursts(str(data), fs, nfft=1024, avg=4, thr_db=12.0, min_frames=3)
    items, logs = [], []
    lo_off = CLOCK_PPM_NOMINAL * 1e-6 * fc
    n_truth = strong = weak = 0
    for i, b in enumerate(sorted(bursts, key=lambda b: (b["t0"], b["f_lo"]))):
        bt = fsk_ref.burst_truth(str(data), fs, fc, b)
        s0 = b["frame_start"] * b["hop"]
        cnt = (b["frame_stop"] - b["frame_start"]) * b["hop"]
        snr = bt["snr"] or {}
        t = dict(role="emission", burst_index=i, detector_peak_db=b["peak_db"],
                 snr_db=snr.get("snr_db"), bandwidth_hz=snr.get("obw99_hz"),
                 snr_definition="S5 C13: noise-subtracted Welch OBW99, N0 = mean density of the 2 ms pre-burst pad")
        hit = bt["hits"][0] if bt["truth_rate"] else None
        if hit:
            n_truth += 1
            center = fc + bt["box_center_offset_hz"] + hit["if_mid_hz"]
            rate = hit["rate"]
            dev = hit["dev_ref_hz"]
            t.update(kind="fsk-burst", modulation="2-FSK", levels=2, symbol_rate_bd=rate,
                     deviation_hz=dev, mod_index=None if dev is None else 2 * dev / rate,
                     deviation_definition=("reference: median |IF - centre| at symbol centres inside runs of "
                                           ">= 3 equal bits on the fixed-rate demodulator (self-consistent, S5)"),
                     center_hz=center, rf_center_hz=center - lo_off, lo_offset_hz=lo_off,
                     offset_hz=center - fc,
                     raster_channel_hz=round((center - lo_off) / 200e3) * 200e3,
                     raster_error_hz=(center - lo_off) - round((center - lo_off) / 200e3) * 200e3,
                     preamble_bits=hit["preamble_bits"], n_symbols_observed=hit["n_bits"],
                     sync={"present": True, "pattern_bits": fsk_ref.SYNC, "pattern_hex": "0C5F",
                           "bit_index": hit["sync_bit"], "bit_errors": hit["sync_bit_errors"],
                           "polarity_inverted": bool(hit["polarity"])},
                     truth_method=("fixed standard-rate trial demodulation: >= 24 preamble bits then sync at "
                                   "exactly one rate in {50,100,150,200,300} kbit/s (S5 fsk_truth)"),
                     truth_confidence="low" if rate == 300e3 else "high",
                     identity={"type": "unidentified", "value": "FHSS 2-FSK, sync 0C5F"})
            if (snr.get("snr_db") or -99) >= 20:
                strong += 1
            elif (snr.get("snr_db") or 99) < 12:
                weak += 1
        else:
            t.update(kind="burst", center_hz=fc + bt["box_center_offset_hz"],
                     offset_hz=bt["box_center_offset_hz"], sync={"present": False},
                     truth_level="detection-only (no fixed-rate sync found; rate unknown)")
        edge = b["f_lo"] <= -fs / 2 + fs / 1024 or b["f_hi"] >= fs / 2 - fs / 1024
        if edge:
            t["at_nyquist_edge"] = True
        t = {k: v for k, v in t.items() if v is not None}
        items.append(dict(sample_start=s0, sample_count=cnt, freq_lower_edge=fc + b["f_lo"],
                          freq_upper_edge=fc + b["f_hi"], label=t["kind"], truth=t))
        logs.append(dict(burst_index=i, t0=b["t0"], f_lo=b["f_lo"], f_hi=b["f_hi"], peak_db=b["peak_db"],
                         snr=snr, truth_rate=bt["truth_rate"], ambiguous=bt["ambiguous"],
                         hits=[{k: v for k, v in h.items() if k != "sync_sample_in_snippet"} for h in bt["hits"]]))
    # A transmission straddling +-fs/2 is detected at both edges with the same time box (outside
    # the 7 MHz baseband filter, aliased). Pair them so tests don't count it twice.
    n_alias = 0
    edges = [it for it in items if it["truth"].get("at_nyquist_edge")]
    for a in edges:
        for b in edges:
            if a is not b and a["sample_start"] == b["sample_start"] and \
                    (a["truth"]["offset_hz"] < 0) != (b["truth"]["offset_hz"] < 0):
                a["truth"]["alias_of_burst_index"] = b["truth"]["burst_index"]
                a["truth"]["note"] = ("box touches +-fs/2: the same transmission appears at the opposite edge "
                                      "(outside the 7 MHz baseband filter); count once")
                n_alias += 1
    ctx["logs"][f"{spec['name']}.fsk_truth.json"] = logs
    rates = sorted({it["truth"]["symbol_rate_bd"] for it in items if it["truth"]["kind"] == "fsk-burst"})
    ctx["summary"] = (f"{len(items)} bursts detected, {n_truth} with sync truth "
                      f"({', '.join(f'{sum(1 for it in items if it['truth'].get('symbol_rate_bd') == r)} @ {r / 1e3:.0f} k' for r in rates)}); "
                      f"{strong} at >= 20 dB, {weak} below 12 dB; {n_alias // 2} +-fs/2 edge alias pairs")
    ctx["scenario_extra"] = {
        "annotation_completeness": "bursts >= 12 dB over the per-bin median (STFT 1024x4); DC artefact; continuous lines not annotated",
        "payload": "not decoded or stored (unidentified third-party traffic)",
        "detector": {"nfft": 1024, "avg": 4, "thr_db": 12.0, "min_frames": 3, "dc_guard_bins": 2}}
    return items + [dc_item(data, fc, n, fs)]


# ---- urban FM band (S4) ------------------------------------------------------------------------


def truth_urban(spec, meta_path, data, meta, n, fs, fc, ctx) -> list[dict[str, Any]]:
    rows = list(csv.DictReader(open(LABELS / f"emitters_{spec['source']}.csv")))
    psd = window_psd(data, n, 2048)
    items = []
    missing = []
    clipped = sum(c.get(CLIP_COUNT_KEY, 0) for c in meta["captures"])
    for r in rows:
        f0, flo, fhi = (float(r[k]) * 1e6 for k in ("f_center_mhz", "f_lo_mhz", "f_hi_mhz"))
        flags = [x for x in r["flags"].split(";") if x]
        cat = r["category"]
        m = band_measure(psd, fs, flo - fc, fhi - fc)
        s4 = {"category": cat or None, "flags": flags, "peak_snr_db_4s": float(r["peak_snr_db"]),
              "level_dbfs_4s": float(r["level_dbfs"]), "bw_khz": float(r["bw_khz"]), "duty": float(r["duty"]),
              "retune": r["retune"] or None, "gain_tests": r["gain_tests"] or None}
        common = dict(center_hz=f0, offset_hz=f0 - fc, bandwidth_hz=fhi - flo,
                      window_peak_snr_db=m["peak_snr_db"], window_power_dbfs=m["power_dbfs"], s4=s4,
                      truth_source="spike S4 emitter table (full 4 s capture), re-measured on this window")
        known = next((k for k in KNOWN_FM_MHZ if abs(f0 - k * 1e6) <= 60e3), None)
        if abs(f0 - fc) >= 0.495 * fs:
            it = dict(label="band edge", truth=dict(
                role="artefact", kind="band-edge", reason="fft_edge_bin",
                note="S4 row at +-fs/2 (FFT edge bin, outside the 15 MHz baseband filter)", **common))
        elif "dc" in flags:
            it = dc_item(data, fc, n, fs)
            it["truth"].update(s4=s4, window_peak_snr_db=m["peak_snr_db"])
        elif "spur_candidate" in flags:
            it = dict(label="spur 100.000 MHz", truth=dict(
                role="artefact", kind="spur", reason="ref_harmonic", harmonic_n=int(round(f0 / 10e6)),
                spur_step_hz=10e6, tuner_center_hz=fc, **common))
        elif known is not None:
            it = dict(label=f"FM {known} MHz", truth=dict(
                role="emission", kind="wfm-broadcast", modulation="WFM", known=True,
                rf_center_hz=known * 1e6, channel_hz=known * 1e6, snr_db=m["peak_snr_db"],
                identity={"type": "fm-channel", "value": f"{known} MHz"}, **common))
        elif "comb_candidate" in flags:
            it = dict(label="comb line", truth=dict(
                role="artefact", kind="comb", reason="comb", comb_spacing_hz=COMB_SPACING_HZ, comb_members=14,
                note="narrowband comb flagged by S4 at high gain only; local RFI or internal, unresolved", **common))
        elif cat == "raster_station":
            ch = round(f0 / 100e3) * 100e3
            it = dict(label="FM raster emitter", truth=dict(
                role="emission", kind="wfm-broadcast", modulation="WFM", known=False, channel_hz=ch,
                snr_db=m["peak_snr_db"], note="on the FM raster, not one of the 7 strong reference stations",
                **common))
        else:
            it = dict(label=cat or "unverified", truth=dict(
                role="emission", kind="unverified-line" if cat != "station_part" else "station-part",
                verified=False, note="persistent emitter S4 could not attribute (antenna-off capture needed)",
                **common))
        if "sample_start" not in it:
            it.update(whole(n), freq_lower_edge=flo, freq_upper_edge=fhi)
        items.append(it)
        if m["peak_snr_db"] < 3.0 and "dc" not in flags and it["truth"]["kind"] != "band-edge":
            missing.append(f"{f0 / 1e6:.4f} ({cat or ','.join(flags)}: {m['peak_snr_db']:.1f} dB)")
    if clipped:
        items.append(dict(**whole(n), label="overload", truth=dict(
            role="artefact", kind="overload", clipped_samples=int(clipped), clip_fraction=clipped / n,
            note="ADC clipping in every frame (aggregate FM-band power); no discrete IMD ghosts found by S4")))
    known_found = sum(1 for it in items if it["truth"].get("known"))
    ctx["summary"] = (f"{known_found}/7 known FM + {sum(1 for it in items if it['truth'].get('known') is False)} raster; "
                      f"{sum(1 for it in items if it['truth']['kind'] == 'comb')} comb lines; spur + DC artefacts; "
                      f"clip {clipped} ({clipped / n:.2%})")
    ctx["suspicious"] += [f"{spec['name']}: S4 emitter weak/absent in window: {m}" for m in missing]
    ctx["scenario_extra"] = {"annotation_completeness": "all S4 emitters of the source capture (edge ones included)",
                             "spike": "S4", "s4_labels": f"hackrf/{DATE}/labels/emitters_{spec['source']}.csv"}
    return items


# ---- 433 MHz noise-only negative control ------------------------------------------------------


def truth_433(spec, meta_path, data, meta, n, fs, fc, ctx) -> list[dict[str, Any]]:
    nfft = 1024
    f = (np.arange(nfft) - nfft // 2) * fs / nfft
    band = (np.abs(f) <= 0.7e6) & (np.abs(np.arange(nfft) - nfft // 2) > 3)
    bw = float(band.sum() * fs / nfft)
    blocks = []
    block = int(0.5 * fs) // nfft * nfft
    for a in range(0, n - block + 1, block):
        p = fxlib.floor_psd(data, a, block, nfft=nfft, max_frames=block // nfft)
        blocks.append(float(np.median(p[band])))
    psd = window_psd(data, n, nfft)
    med = float(np.median(psd[band]))
    floor_per_hz = db(med / fs)
    block_db = [db(b / fs) for b in blocks]
    ripple = 10 * np.log10(psd[band] / med)
    excess = float(ripple.max())
    q = fxlib.quantisation_floor_dbfs_per_hz(fs)
    floor = dict(
        **whole(n), freq_lower_edge=fc - 0.7e6, freq_upper_edge=fc + 0.7e6, label="noise floor",
        truth=dict(role="floor", kind="noise-floor", floor_dbfs_per_hz=floor_per_hz,
                   floor_dbfs=floor_per_hz + db(bw), bandwidth_hz=bw,
                   uncertainty_db=float(max(np.std(block_db), 0.05)), block_floor_dbfs_per_hz=block_db,
                   block_s=block / fs, ripple_db_p10=float(np.percentile(ripple, 10)),
                   ripple_db_p90=float(np.percentile(ripple, 90)), max_bin_excess_db=excess,
                   quantisation_noise_dbfs_per_hz=q, expected_floor_dbfs_per_hz=floor_per_hz,
                   method=("median over |f| <= 0.7 MHz (DC +-3 bins excluded) of the mean Hann periodogram, "
                           "1024 bins, all frames; uncertainty = 1 sigma of 0.5 s block floors"),
                   calibration="uncalibrated: no floor_dbm (antenna and gain chain not calibrated)"))
    lines = narrowband_lines(data, n, fs, fc, band_hz=0.7e6, nfft=16384, thr_db=6.0)
    items = [floor, dc_item(data, fc, n, fs)]
    for ln in lines:
        near_fs = abs(ln["center_hz"] / fs - round(ln["center_hz"] / fs)) * fs
        suspected = "sample_clock_harmonic" if near_fs < 50.0 else "unknown"
        t = dict(role="artefact", kind="narrowband-line", reason=suspected, verified=False,
                 center_hz=ln["center_hz"], offset_hz=ln["center_hz"] - fc, excess_db=ln["excess_db"],
                 power_dbfs=ln["power_dbfs"], resolution_bw_hz=fs / 16384,
                 note=("persistent narrowband line in the noise-only control; origin unverified (internal spur "
                       "or weak unmodulated carrier). A 50 ohm terminated-input capture would settle it."))
        if suspected == "sample_clock_harmonic":
            t["harmonic_n"] = int(round(ln["center_hz"] / fs))
            t["note"] += (f" Sits within {near_fs:.0f} Hz of {t['harmonic_n']} x fs; an external carrier on "
                          f"{round(ln['center_hz'] / 1e6, 3)} MHz would read {CLOCK_PPM_NOMINAL * 1e-6 * fc:+.0f} Hz off.")
        items.append(dict(**whole(n), freq_lower_edge=ln["center_hz"] - ln["width_hz"] / 2,
                          freq_upper_edge=ln["center_hz"] + ln["width_hz"] / 2, label="narrowband line", truth=t))
    strongest = max(lines, key=lambda ln: ln["excess_db"]) if lines else None
    ctx["summary"] = (f"floor {floor_per_hz:.2f} dBFS/Hz +- {floor['truth']['uncertainty_db']:.2f} dB over "
                      f"{bw / 1e6:.2f} MHz; no device emissions; {len(lines)} narrowband lines >= 6 dB "
                      f"(122 Hz RBW)" + (f", strongest {strongest['center_hz'] / 1e6:.6f} MHz "
                                         f"+{strongest['excess_db']:.1f} dB" if strongest else ""))
    ctx["scenario_extra"] = {
        "emissions": [], "negative_control": True,
        "annotation_completeness": ("no device emissions (rtl_433 decoded nothing in 180 s; S5 detector: 0 events "
                                    "at 8 dB in this window); persistent narrowband lines >= 6 dB over the median "
                                    "at 122 Hz RBW annotated as unverified artefacts"),
    }
    return items


def narrowband_lines(data: Path, n: int, fs: float, fc: float, band_hz: float, nfft: int,
                     thr_db: float) -> list[dict[str, Any]]:
    """Persistent lines: mean periodogram over the whole window, bins >= thr_db over the in-band
    median, adjacent bins merged, reported at the peak bin."""
    psd = fxlib.floor_psd(data, 0, n, nfft=nfft, max_frames=n // nfft)
    f = (np.arange(nfft) - nfft // 2) * fs / nfft
    band = (np.abs(f) <= band_hz) & (np.abs(np.arange(nfft) - nfft // 2) > 3)
    med = float(np.median(psd[band]))
    hot = band & (psd >= med * 10 ** (thr_db / 10))
    out, k = [], 0
    while k < nfft:
        if not hot[k]:
            k += 1
            continue
        j = k
        while j + 1 < nfft and hot[j + 1]:
            j += 1
        seg = slice(max(0, k - 1), min(nfft, j + 2))
        p = int(np.argmax(psd[seg])) + seg.start
        out.append(dict(center_hz=float(fc + f[p]), width_hz=float((j - k + 3) * fs / nfft),
                        excess_db=db(float(psd[p]) / med),
                        power_dbfs=db(float(np.clip(psd[seg] - med, 0, None).sum()) / nfft)))
        k = j + 1
    return out


# ---- driver ----------------------------------------------------------------------------------


def build_fixture(spec, ctx) -> dict[str, Any]:
    store = ctx["store"]
    src_meta_path = store / f"{spec['source']}.sigmf-meta"
    src_meta = fxlib.load_json(src_meta_path)
    dst_meta = OUT / f"{spec['name']}.sigmf-meta"
    g = src_meta["global"]
    antenna = g.get("hackriff:antenna", "unknown")
    t = g["hackriff:provenance"]["tune"]
    fs = float(g["core:sample_rate"])
    desc = (f"hackriff fixture {spec['name']}: {spec['summary']}. Serves {', '.join(spec['use_cases'])}"
            f"{' (spike ' + spec['spike'] + ')' if spec.get('spike') else ''}. Window {spec['start_s']} s + "
            f"{spec['duration_s']} s of store capture {spec['source']} (HackRF One, {t['center_hz'] / 1e6:g} MHz, "
            f"{fs / 1e6:g} Msps, LNA {t['lna_db']} / VGA {t['vga_db']} / amp {'on' if t['amp_on'] else 'off'}, "
            f"antenna {antenna}). Receive-only; truth is PHY metadata only.")
    trim.trim(src_meta_path, dst_meta, start_s=spec["start_s"], duration_s=spec["duration_s"], description=desc)
    meta = sigmf.read_meta(dst_meta)
    meta["global"]["core:license"] = LICENSE
    meta["global"]["core:recorder"] = f"{g.get('core:recorder', 'hackrf_transfer')}; trimmed by {BUILDER}"
    sigmf.write_meta(meta, dst_meta)
    data = sigmf.data_path(dst_meta)
    n = fxlib.n_samples(data, meta["global"]["core:datatype"])
    fc = float(meta["captures"][0]["core:frequency"])
    ctx["summary"], ctx["scenario_extra"] = "", {}
    items = spec["builder"](spec, dst_meta, data, meta, n, fs, fc, ctx)
    scen = scenario_truth(spec, meta, src_meta, ctx["source_sha"][spec["source"]], n, fs, ctx["scenario_extra"])
    scen["truth_summary"] = ctx["summary"]
    annotate.annotate(dst_meta, {"annotations": [dict(**whole(n), label="capture", truth=scen)] + items})
    print(f"  {spec['name']}: {ctx['summary']}")
    return dict(meta=dst_meta, data=data, summary=ctx["summary"], n=n)


def copy_sweeps(store: Path) -> list[Path]:
    SWEEPS_OUT.mkdir(parents=True, exist_ok=True)
    out = []
    for csv_path in sorted(store.glob("sweep_*.csv")):
        for p in (csv_path, csv_path.with_suffix(".json")):
            dst = SWEEPS_OUT / p.name
            shutil.copyfile(p, dst)
            out.append(dst)
    return out


def entry(name, status, path: Path | str, **kw) -> dict[str, Any]:
    e = {"name": name, "status": status}
    if isinstance(path, Path):
        e["path"] = fxlib.rel_to_fixtures(path)
        e["size_bytes"] = path.stat().st_size
        e["sha256"] = fxlib.sha256_file(path)
    else:
        e["path"] = path
    e.update(kw)
    return e


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--store", help="store root (default $HACKRIFF_FIXTURE_STORE or fixtures/store)")
    ap.add_argument("--only", nargs="*", help="fixture names to rebuild (manifest is still rewritten)")
    ap.add_argument("--scratch", help="directory for truth JSON and decode logs (default: a temp dir)")
    args = ap.parse_args(argv)
    store = fxlib.store_dir(args.store) / DATE
    if not store.is_dir():
        print(f"store not found: {store}", file=sys.stderr)
        return 2
    scratch = Path(args.scratch or tempfile.mkdtemp(prefix="hackriff-fixtures-"))
    scratch.mkdir(parents=True, exist_ok=True)
    store_manifest = {e["name"]: e for e in fxlib.load_json(store / "manifest.json")["entries"]}
    OUT.mkdir(parents=True, exist_ok=True)

    print("hashing store originals ...")
    ext_entries, source_sha, suspicious = [], {}, []
    for meta_path in sorted(store.glob("*.sigmf-meta")):
        name = meta_path.name[: -len(".sigmf-meta")]
        data_path = meta_path.with_suffix(".sigmf-data")
        src_meta = fxlib.load_json(meta_path)
        sm = store_manifest.get(name, {})
        e = entry(name, "external", data_path)
        e["path"] = f"store/{DATE}/{data_path.name}"
        source_sha[name] = e["sha256"]
        if sm.get("sha256") and sm["sha256"] != e["sha256"]:
            suspicious.append(f"{name}: sha256 differs from store manifest")
        clip_full = fxlib.count_clipped(data_path, "ci8", 0, fxlib.n_samples(data_path, "ci8"))
        if sm.get("clip_samples") is not None and sm["clip_samples"] != clip_full:
            suspicious.append(f"{name}: clip count {clip_full} (rails -128/127) != store manifest {sm['clip_samples']}")
        e.update(kind="sigmf-data", source=f"hackrf_transfer capture {DATE} (coordinator, receive-only)",
                 purpose=src_meta["global"].get("core:description"), use_cases=EXTERNAL_USE_CASES.get(name, []),
                 license=LICENSE, clip_count=clip_full, datetime=src_meta["captures"][0].get("core:datetime"))
        me = entry(name, "external", meta_path, kind="sigmf-meta", license=LICENSE,
                   use_cases=EXTERNAL_USE_CASES.get(name, []))
        me["path"] = f"store/{DATE}/{meta_path.name}"
        ext_entries += [e, me]
    for p in sorted(store.glob("*.csv")):
        if not p.name.startswith("sweep_"):
            e = entry(p.stem, "external", p, kind="sweep-csv", license=LICENSE, use_cases=SWEEP_USE_CASES,
                      source=f"hackrf_sweep {DATE} (coordinator, receive-only); no JSON sidecar")
            e["path"] = f"store/{DATE}/{p.name}"
            ext_entries.append(e)

    ctx = dict(store=store, logs={}, suspicious=suspicious, source_sha=source_sha)
    results = {}
    for spec in fixture_specs():
        meta_path = OUT / f"{spec['name']}.sigmf-meta"
        if args.only and spec["name"] not in args.only:
            if meta_path.exists():
                results[spec["name"]] = dict(meta=meta_path, data=meta_path.with_suffix(".sigmf-data"),
                                             summary=_scenario(meta_path).get("truth_summary", ""))
            continue
        print(f"building {spec['name']} ...")
        results[spec["name"]] = build_fixture(spec, ctx)
    for name, obj in ctx["logs"].items():
        fxlib.dump_json(obj, scratch / name)

    committed = []
    for spec in fixture_specs():
        r = results.get(spec["name"])
        if not r:
            continue
        if r["data"].stat().st_size > fxlib.MAX_COMMITTED_BYTES:
            raise SystemExit(f"{r['data']} exceeds {fxlib.MAX_COMMITTED_BYTES} bytes")
        common = dict(source=f"store/{DATE}/{spec['source']}.sigmf-data", use_cases=spec["use_cases"],
                      license=LICENSE, truth_summary=r["summary"])
        committed.append(entry(spec["name"], "committed", r["data"], kind="sigmf-data", lfs=True,
                               source_sha256=source_sha[spec["source"]],
                               source_start_s=spec["start_s"], **common))
        committed.append(entry(spec["name"], "committed", r["meta"], kind="sigmf-meta", **common))
    for p in sorted(LABELS.glob("*.csv")):
        committed.append(entry(p.stem, "committed", p, kind="labels",
                               source="spikes/s4-detection-overload/results (spike S4)", license=LICENSE))
    for p in copy_sweeps(store):
        kind = "sweep-csv" if p.suffix == ".csv" else "sweep-sidecar"
        committed.append(entry(p.stem, "committed", p, kind=kind, source=f"store/{DATE}/{p.name}",
                               use_cases=SWEEP_USE_CASES, license=LICENSE))
    blocked = [dict(name=b["name"], status="blocked", reason=b["reason"], use_cases=b["use_cases"],
                    source=b["source"]) for b in BLOCKED]
    manifest = {"version": fxlib.MANIFEST_VERSION, "store": "fixtures/store",
                "entries": committed + ext_entries + blocked}
    fxlib.dump_json(manifest, fxlib.MANIFEST)
    print(f"manifest: {len(committed)} committed, {len(ext_entries)} external, {len(blocked)} blocked")
    print(f"logs: {scratch}")
    for s in suspicious:
        print("SUSPICIOUS:", s)
    return 0


def _scenario(meta_path: Path) -> dict[str, Any]:
    meta = fxlib.load_json(meta_path)
    for a in meta["annotations"]:
        t = a.get(sigmf.TRUTH_KEY) or {}
        if t.get("role") == "scenario":
            return t
    return {}


if __name__ == "__main__":
    sys.exit(main())
