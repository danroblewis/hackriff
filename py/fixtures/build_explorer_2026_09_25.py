"""Build the 2026-09-25 explorer-agent FM/RDS/FLEX/P25 fixtures' truth annotations and manifest
rows.

    uv run --project py python py/fixtures/build_explorer_2026_09_25.py

Inputs (read-only): ``fixtures/hackrf/explorer-2026-09-25/*.sigmf-{meta,data}`` (already placed,
clipped live via ``POST /api/iqbuffer/clip`` on the explorer agent's staging build, 2026-09-25
~04:00-04:11 PDT, SF; the P25 capture ~06:19 PDT the same session) and the explorer's own
hidden-truth claim at ``~/.hackriff-ops/explorer/captures/20260925/*.truth.json``. Cross-checks
every station/channel against an independent oracle (``rds_ref.py``, ``flex_ref.py``,
``p25_ref.py``) and writes what the oracle found alongside the explorer's claim — it does not
silently "fix" either one.

Outputs: rewrites ``hackriff:truth`` annotations into the ``.sigmf-meta`` files (via
``annotate.annotate``) and appends/refreshes their rows in ``fixtures/manifest.json``.

Legal: receive-only capture; RDS PI/PS/PTY are public broadcast station identity, not third-party
payload content (CLAUDE.md legal guardrails).
"""

from __future__ import annotations

import json
import re
import sys
from pathlib import Path
from typing import Any

import annotate
import flex_ref
import fxlib
import p25_ref
import rds_ref
from fxlib import CLIP_COUNT_KEY, sigmf

DATE = "2026-09-25"
OUT = fxlib.FIXTURES / "hackrf" / f"explorer-{DATE}"
LICENSE = "project-owned capture"
BUILDER = "py/fixtures/build_explorer_2026_09_25.py"
LEGAL = ("Receive-only capture. Truth is PHY/metadata only; RDS PI/PS/PTY are public broadcast "
         "station identity (CLAUDE.md legal guardrails).")
EXPLORER_HOME = Path.home() / ".hackriff-ops" / "explorer" / "captures" / DATE.replace("-", "")

#: name -> (station center offsets from capture centre to decode, each (f_center_hz, label))
STATIONS = {
    "fm-101p3-pi1694": [(101_300_000.0, "FM 101.3 MHz")],
    "fm-98p9-piA4FF": [(98_900_000.0, "FM 98.9 MHz"), (98_100_000.0, "FM 98.1 MHz")],
}

#: The FLEX paging capture (T-949): 12 s at 2.4 Msps, 57.6 MB -- over fixtures/README.md's 25 MB
#: committed cap, so unlike the FM/RDS pair above it goes into the external store, not Git LFS.
FLEX_NAME = "flex-pagers-930p8"
FLEX_USE_CASE = "SIGNAL-088"
#: "... x<N> in this clip ..." in the explorer's own prose claim (e.g. "at 1600 bps x3 in this
#: clip", "0xA6C6AAAA x1 in this clip (x3 in a 30 s window)"); parsed rather than trusted, so a
#: mismatch against the independent oracle below is reported, not silently fixed.
FLEX_CLAIM_RE = re.compile(r"x(\d+) in this clip")

#: The P25 C4FM capture (T-975): 5 s at 2.4 Msps, 24 MB -- under the committed cap, unlike FLEX.
P25_NAME = "p25-852p86-2p4M"
P25_USE_CASE = "SIGNAL-085"
P25_ALSO_USE_CASES = ["SIGNAL-080", "SIGNAL-087"]
#: "... N frame sync(s) ..." / "4x in this 5 s clip" style count in the explorer's evidence prose;
#: parsed rather than trusted (same discipline as ``FLEX_CLAIM_RE``).
P25_CLAIM_RE = re.compile(r"(\d+)x in this \d+ s clip")


def db(x: float) -> float:
    import math

    return float(10 * math.log10(max(x, 1e-30)))


def whole(n: int) -> dict[str, int]:
    return {"sample_start": 0, "sample_count": n}


def build_one(name: str) -> dict[str, Any]:
    meta_path = OUT / f"{name}.sigmf-meta"
    data_path = OUT / f"{name}.sigmf-data"
    explorer_truth = json.loads((EXPLORER_HOME / f"{name}.truth.json").read_text())

    meta = sigmf.read_meta(meta_path)
    use_case = explorer_truth["use_case"]
    meta["global"]["core:license"] = LICENSE
    meta["global"]["core:description"] = f"hk-pipeline IQ capture buffer clip ({use_case}, {name})"
    fs = float(meta["global"]["core:sample_rate"])
    fc = float(meta["captures"][0]["core:frequency"])
    n = fxlib.n_samples(data_path, meta["global"]["core:datatype"])
    prov = meta["global"][sigmf.PROVENANCE_KEY]

    clip_count = int(sum(c.get(CLIP_COUNT_KEY, 0) for c in meta["captures"]))
    clip_fraction = clip_count / n

    items: list[dict[str, Any]] = []
    oracle_log: dict[str, Any] = {}
    disagreements: list[str] = []

    for f_center, label in STATIONS[name]:
        offset_hz = f_center - fc
        explorer_em = next(
            e for e in explorer_truth["emissions"] if abs(e["f_center_hz"] - f_center) < 1.0
        )
        oracle = rds_ref.decode_ci8(str(data_path), fs, offset_hz, duration_s=n / fs)
        oracle_log[label] = oracle

        explorer_pi = explorer_em.get("pi")
        oracle_pi = oracle["pi_hex"]
        agrees = (explorer_pi is None and oracle_pi is None) or (
            explorer_pi is not None and oracle_pi is not None
            and explorer_pi.upper() == oracle_pi.upper()
        )
        if not agrees:
            disagreements.append(
                f"{label}: explorer claims PI {explorer_pi!r}, oracle decoded PI {oracle_pi!r} "
                f"({oracle['groups_decoded']} groups, BLER {oracle['block_error_rate']})"
            )

        rds = {
            "pi_hex": oracle["pi_hex"],
            "pi_votes": oracle["pi_votes"],
            "ps": oracle["ps"],
            "ps_frames": oracle["ps_frames"],
            "pty": oracle["pty"],
            "tp": oracle["tp"],
            "group_types": oracle["group_types"],
            "groups_decoded": oracle["groups_decoded"],
            "groups_on_lattice": oracle["groups_on_lattice"],
            "blocks_on_lattice": oracle["blocks_on_lattice"],
            "blocks_ok": oracle["blocks_ok"],
            "block_error_rate": oracle["block_error_rate"],
            "bitrate_bd": 1187.5,
            "bitrate_bd_in_sample_clock": oracle["bitrate_bd_in_sample_clock"],
            "subcarrier_hz": 57000.0,
            "decoder": "py/fixtures/rds_ref.py (independent oracle, run over this fixture's own window)",
            "oracle_agrees_with_explorer_pi": agrees,
            "explorer_claim": {
                "pi": explorer_pi,
                "decoded": explorer_em.get("decoded"),
                "source": (
                    "app blind detection + recipe:rds on the explorer agent's live staging build "
                    "(/api/pipelines, /api/inventory/{id}/decode), 2026-09-25 04:00-04:14 PDT, SF"
                ),
            },
        }
        truth_obj = dict(
            role="emission",
            kind=explorer_em["kind"],
            modulation="WFM",
            center_hz=f_center,
            offset_hz=offset_hz,
            bandwidth_hz=explorer_em["bandwidth_hz"],
            channel_hz=f_center,
            pilot={"present": True, "frequency_hz": oracle["pilot_hz"],
                   "deviation_hz": oracle["pilot_deviation_hz"]},
            clock_ppm_from_pilot=oracle["clock_ppm_from_pilot"],
            rds=rds,
        )
        if oracle["pi_hex"]:
            truth_obj["identity"] = {"type": "rds_pi", "value": oracle["pi_hex"]}
        items.append(dict(
            **whole(n),
            freq_lower_edge=f_center - 100e3,
            freq_upper_edge=f_center + 100e3,
            label=label,
            comment=explorer_em.get("decoded"),
            truth=truth_obj,
        ))

    items.append(dict(
        **whole(n),
        label="overload",
        truth=dict(
            role="artefact", kind="overload", clipped_samples=clip_count,
            clip_fraction=clip_fraction,
            overload_rule=f"clip_fraction > {fxlib.OVERLOAD_CLIP_FRACTION:g} (component at -128 or 127)",
            note=("LNA 32 / VGA 30 / amp on in San Francisco's strong-signal FM environment on an "
                  "8-bit HackRF One with no preselector; the explorer agent's one gain-reduction "
                  "retry (LNA 24 / VGA 24 / amp off) made RDS worse, so this is a front-end/location "
                  "limit, not a bug (journal-20260925.md)"),
        ),
    ))

    scenario = dict(
        role="scenario", kind="capture", recording=name,
        use_cases=[explorer_truth["use_case"]],
        generator=BUILDER,
        source={
            "capture": "explorer agent live clip",
            "method": "POST /api/iqbuffer/clip on the explorer agent's staging build",
            "captured": explorer_truth["capture"]["captured"],
            "device": explorer_truth["capture"]["device"],
        },
        datatype=meta["global"]["core:datatype"], sample_rate_hz=fs, n_samples=n, duration_s=n / fs,
        dbfs_reference="full scale = 127 codes per component; dBFS = 10 log10(mean |x|^2)",
        calibration="uncalibrated (no dBm): antenna unknown, as attached by the user",
        clip_count=clip_count, clip_fraction=clip_fraction, overload=prov["overload"],
        quantisation_limited=prov["quantisation_limited"],
        quantisation_noise_dbfs_per_hz=fxlib.quantisation_floor_dbfs_per_hz(fs),
        capture_settings={
            "lna_db": explorer_truth["capture"]["lna_db"],
            "vga_db": explorer_truth["capture"]["vga_db"],
            "amp": explorer_truth["capture"]["amp"],
            "bias_tee": "unknown (untouched by the explorer agent; not confirmed off)",
            "antenna": explorer_truth["capture"]["antenna"],
        },
        identification_source=explorer_truth["source"],
        annotation_completeness=(
            "partial: the station(s) named in the explorer's truth file, plus the whole-file "
            "overload artefact"
        ),
        legal=LEGAL,
    )
    truth = {"annotations": [dict(**whole(n), label="capture", truth=scenario), *items]}
    annotate.apply(meta, truth["annotations"], n)
    sigmf.write_meta(meta, meta_path)

    summary_bits = []
    for f_center, label in STATIONS[name]:
        o = oracle_log[label]
        pi_txt = o["pi_hex"] or "none decoded"
        bler_txt = "n/a" if o["block_error_rate"] is None else f"{o['block_error_rate']:.3f}"
        summary_bits.append(
            f"{label.split()[1]}: oracle PI {pi_txt} ({o['groups_decoded']} groups, BLER {bler_txt})"
        )
    return {
        "name": name,
        "clip_count": clip_count,
        "clip_fraction": clip_fraction,
        "truth_summary": "; ".join(summary_bits) + f"; overload clip_fraction {clip_fraction:.3f}",
        "disagreements": disagreements,
        "n": n,
        "size_bytes": data_path.stat().st_size,
        "sha256_data": fxlib.sha256_file(data_path),
        "sha256_meta": fxlib.sha256_file(meta_path),
    }


def build_flex() -> dict[str, Any]:
    """Builds the FLEX paging fixture into the **external store** (fixtures/store/, gitignored):
    at 57.6 MB the raw 12 s/2.4 Msps capture is over fixtures/README.md's 25 MB committed cap, so
    unlike the FM/RDS pair it cannot go through Git LFS. Same annotate.apply/manifest discipline
    as a committed fixture, just a different ``status``/``path`` (fixtures/README.md's
    ``manifest.json`` table)."""
    src_meta = EXPLORER_HOME / f"{FLEX_NAME}.sigmf-meta"
    src_data = EXPLORER_HOME / f"{FLEX_NAME}.sigmf-data"
    explorer_truth = json.loads((EXPLORER_HOME / f"{FLEX_NAME}.truth.json").read_text())

    meta = sigmf.read_meta(src_meta)
    meta["global"]["core:license"] = LICENSE
    meta["global"]["core:description"] = (
        f"hk-pipeline IQ capture buffer clip ({FLEX_USE_CASE}, {FLEX_NAME})"
    )
    fs = float(meta["global"]["core:sample_rate"])
    fc = float(meta["captures"][0]["core:frequency"])
    n = fxlib.n_samples(src_data, meta["global"]["core:datatype"])
    prov = meta["global"][sigmf.PROVENANCE_KEY]

    clip_count = int(sum(c.get(CLIP_COUNT_KEY, 0) for c in meta["captures"]))
    clip_fraction = clip_count / n

    items: list[dict[str, Any]] = []
    oracle_log: dict[str, Any] = {}
    disagreements: list[str] = []
    summary_bits: list[str] = []

    for em in explorer_truth["emissions"]:
        f_center = float(em["f_center_hz"])
        offset_hz = f_center - fc
        oracle = flex_ref.decode_ci8(str(src_data), fs, offset_hz, duration_s=n / fs)
        oracle_log[f_center] = oracle

        claim_match = FLEX_CLAIM_RE.search(em["decoded"])
        claimed_syncs = int(claim_match.group(1)) if claim_match else None
        agrees = claimed_syncs is not None and claimed_syncs == oracle["n_syncs"]
        if not agrees:
            disagreements.append(
                f"{f_center / 1e6:.4f} MHz: explorer claims {claimed_syncs} sync(s) in this clip, "
                f"oracle found {oracle['n_syncs']} (Hamming <= {oracle['max_hamming']}/32)"
            )

        level_claims = re.findall(r"(\d)(?:-level|FSK)", em["kind"] + " " + em["decoded"])
        claimed_levels = int(level_claims[-1]) if level_claims else None
        oracle_levels = {rate: v["n_levels"] for rate, v in oracle["levels"].items()}
        if claimed_levels is not None and claimed_levels not in oracle_levels.values():
            disagreements.append(
                f"{f_center / 1e6:.4f} MHz: explorer claims {claimed_levels}-level FSK, oracle "
                f"level-count re-slice found {oracle_levels} clean levels per candidate rate"
            )

        flex_truth = {
            "sync_hex": oracle["sync_hex"],
            "sync_rate_bd": oracle["sync_rate_bd"],
            "n_syncs": oracle["n_syncs"],
            "syncs": oracle["syncs"],
            "levels": oracle["levels"],
            "decoder": oracle["decoder"],
            "oracle_agrees_with_explorer_claimed_sync_count": agrees,
            "explorer_claim": {
                "kind": em["kind"],
                "decoded": em["decoded"],
                "claimed_syncs_in_clip": claimed_syncs,
                "source": "app blind detection (family: unknown, confidence 0.999) + the "
                          "explorer's own numpy oracle tools/oracle_pager.py, 2026-09-25 ~04:27-"
                          "04:33 PDT (journal-20260925.md)",
            },
        }
        items.append(dict(
            **whole(n),
            freq_lower_edge=f_center - float(em["bandwidth_hz"]) / 2,
            freq_upper_edge=f_center + float(em["bandwidth_hz"]) / 2,
            label=f"paging {f_center / 1e6:.4f} MHz",
            comment=em["kind"],
            truth=dict(
                role="emission", kind="flex-pager", modulation="FSK",
                center_hz=f_center, offset_hz=offset_hz, bandwidth_hz=float(em["bandwidth_hz"]),
                channel_hz=f_center, decoded=False,
                decode_note="Not decoded: no FLEX bitstream decoder in the app yet "
                            "(journal-20260925.md); confirmed only by sync-word oracles.",
                paging=flex_truth,
            ),
        ))
        levels_txt = "/".join(f"{r}:{v['n_levels']}L" for r, v in oracle["levels"].items())
        summary_bits.append(f"{f_center / 1e6:.4f}: oracle {oracle['n_syncs']} sync(s), "
                            f"levels {levels_txt}")

    items.append(dict(
        **whole(n),
        label="overload",
        truth=dict(
            role="artefact", kind="overload", clipped_samples=clip_count,
            clip_fraction=clip_fraction,
            overload_rule=f"clip_fraction > {fxlib.OVERLOAD_CLIP_FRACTION:g} (component at -128 or 127)",
            note="LNA 32 / VGA 30 / amp on; provenance reports overload=false for this capture.",
        ),
    ))

    scenario = dict(
        role="scenario", kind="capture", recording=FLEX_NAME,
        use_cases=[FLEX_USE_CASE],
        generator=BUILDER,
        source={
            "capture": "explorer agent live clip",
            "method": "POST /api/iqbuffer/clip on the explorer agent's staging build",
            "captured": explorer_truth["capture"]["captured"],
            "device": explorer_truth["capture"]["device"],
        },
        datatype=meta["global"]["core:datatype"], sample_rate_hz=fs, n_samples=n, duration_s=n / fs,
        dbfs_reference="full scale = 127 codes per component; dBFS = 10 log10(mean |x|^2)",
        calibration="uncalibrated (no dBm): antenna unknown, as attached by the user",
        clip_count=clip_count, clip_fraction=clip_fraction, overload=prov["overload"],
        quantisation_limited=prov["quantisation_limited"],
        quantisation_noise_dbfs_per_hz=fxlib.quantisation_floor_dbfs_per_hz(fs),
        capture_settings={
            "lna_db": explorer_truth["capture"]["lna_db"],
            "vga_db": explorer_truth["capture"]["vga_db"],
            "amp": explorer_truth["capture"]["amp"],
            "bias_tee": explorer_truth["capture"]["bias_tee"],
            "antenna": explorer_truth["capture"]["antenna"],
        },
        identification_source=explorer_truth["source"],
        not_in_clip_but_seen=explorer_truth.get("not_in_clip_but_seen"),
        annotation_completeness=(
            "partial: the three FLEX channels named in the explorer's truth file, plus the "
            "whole-file overload artefact; a fourth channel (931.7331 MHz) was seen active in a "
            "wider 30 s window but is idle in this 12 s clip"
        ),
        legal=LEGAL,
    )
    truth = {"annotations": [dict(**whole(n), label="capture", truth=scenario), *items]}
    annotate.apply(meta, truth["annotations"], n)

    dst_dir = fxlib.store_dir() / f"explorer-{DATE}"
    dst_dir.mkdir(parents=True, exist_ok=True)
    dst_meta = dst_dir / f"{FLEX_NAME}.sigmf-meta"
    dst_data = dst_dir / f"{FLEX_NAME}.sigmf-data"
    sigmf.write_meta(meta, dst_meta)
    dst_data.write_bytes(src_data.read_bytes())

    return {
        "name": FLEX_NAME,
        "clip_count": clip_count,
        "clip_fraction": clip_fraction,
        "truth_summary": "; ".join(summary_bits) + f"; overload clip_fraction {clip_fraction:.3f}",
        "disagreements": disagreements,
        "n": n,
        "size_bytes": dst_data.stat().st_size,
        "sha256_data": fxlib.sha256_file(dst_data),
        "sha256_meta": fxlib.sha256_file(dst_meta),
    }


def build_p25() -> dict[str, Any]:
    """Builds the P25 C4FM capture (T-975) into the committed fixture directory (24 MB, under the
    25 MB cap, so unlike FLEX this stays in Git LFS rather than the external store). Cross-checks
    the explorer's frame-sync-count claim against the independent oracle ``p25_ref.py`` and
    records the explorer's own re-grading of the channel (control vs conventional/voice) rather
    than asserting either identification as fact -- this capture is blind-detection-only, no
    TSBK/NID decode."""
    name = P25_NAME
    meta_path = OUT / f"{name}.sigmf-meta"
    data_path = OUT / f"{name}.sigmf-data"
    explorer_truth = json.loads((EXPLORER_HOME / f"{name}.truth.json").read_text())

    meta = sigmf.read_meta(meta_path)
    meta["global"]["core:license"] = LICENSE
    meta["global"]["core:description"] = f"hk-pipeline IQ capture buffer clip ({P25_USE_CASE}, {name})"
    fs = float(meta["global"]["core:sample_rate"])
    fc = float(meta["captures"][0]["core:frequency"])
    n = fxlib.n_samples(data_path, meta["global"]["core:datatype"])
    prov = meta["global"][sigmf.PROVENANCE_KEY]

    # this capture's placed-by-hand source (unlike the FM/RDS pair's) carries no
    # ``hackriff:clip_count`` yet -- measure it directly, same as the capture-agent skill would.
    if CLIP_COUNT_KEY not in meta["captures"][0]:
        meta["captures"][0][CLIP_COUNT_KEY] = fxlib.count_clipped(
            data_path, meta["global"]["core:datatype"], 0, n)
    clip_count = int(sum(c.get(CLIP_COUNT_KEY, 0) for c in meta["captures"]))
    clip_fraction = clip_count / n

    em = explorer_truth["emissions"][0]
    f_center = float(em["f_center_hz"])
    offset_hz = f_center - fc
    oracle = p25_ref.decode_ci8(str(data_path), fs, offset_hz, duration_s=n / fs)

    claim_match = P25_CLAIM_RE.search(em["evidence"])
    claimed_syncs = int(claim_match.group(1)) if claim_match else None
    agrees = claimed_syncs is not None and claimed_syncs == oracle["n_syncs"]
    disagreements: list[str] = []
    if not agrees:
        disagreements.append(
            f"{f_center / 1e6:.4f} MHz: explorer claims {claimed_syncs} frame sync(s) in this "
            f"clip, oracle found {oracle['n_syncs']} (Hamming <= {oracle['max_hamming']}/"
            f"{oracle['sync_symbols']} symbols, >= {p25_ref.MIN_PHASE_CORROBORATION}/"
            f"{oracle['n_timing_phases']} timing-phase corroboration)"
        )

    p25_truth = {
        "sync_hex": oracle["sync_hex"],
        "sync_rate_bd": oracle["sync_rate_bd"],
        "n_syncs": oracle["n_syncs"],
        "syncs": [
            {"t_s": round(s["sample"] / (fs / max(1, int(fs // 48_000.0))), 4),
             "hamming": s["hamming"], "order": s["order"], "n_phases": s["n_phases"]}
            for s in oracle["syncs"]
        ],
        "levels": oracle["levels"],
        "decoder": oracle["decoder"],
        "oracle_agrees_with_explorer_sync_count": agrees,
        "explorer_claim": {
            "kind": em["kind"],
            "decoded": em.get("decoded"),
            "claimed_syncs_in_clip": claimed_syncs,
            "evidence": em["evidence"],
            "source": "app blind detection (candidate 852.8586 9.3 kHz SNR 14.7, family unknown "
                      "0.999) + the explorer's own numpy oracle tools/oracle_p25.py, 2026-09-25 "
                      "~06:19 PDT (journal-20260925.md)",
        },
        # The explorer's own re-grading of the channel identity, recorded as an opinion with its
        # reasoning, never as fact: this fixture carries no TSBK/control-channel confirmation.
        "channel_classification": {
            "verdict": "most likely a conventional or voice P25 channel, not an established "
                      "control channel",
            "reasoning": "intermittent frame syncs (a handful over 5 s, not the near-continuous "
                        "cadence of a control channel's repeating TSBK stream) plus a companion "
                        "wideband capture at this same nominal frequency (852.86 MHz, "
                        "p25-cc-852p86, not committed here) that itself did not resolve a "
                        "confirmed control channel (cc chain 7 passes/0 confirmed/0 TSBK)",
        },
    }
    items = [dict(
        sample_start=0, sample_count=n,
        freq_lower_edge=f_center - float(em["bandwidth_hz"]) / 2,
        freq_upper_edge=f_center + float(em["bandwidth_hz"]) / 2,
        label=f"P25 {f_center / 1e6:.4f} MHz",
        comment=em["kind"],
        truth=dict(
            role="emission", kind="p25-c4fm", modulation="C4FM",
            center_hz=f_center, offset_hz=offset_hz, bandwidth_hz=float(em["bandwidth_hz"]),
            channel_hz=f_center, decoded=False,
            decode_note="Not decoded: blind detection + frame-sync identification only; no "
                        "TSBK/NID decode attempted on this capture.",
            p25=p25_truth,
        ),
    ), dict(
        sample_start=0, sample_count=n, label="overload",
        truth=dict(
            role="artefact", kind="overload", clipped_samples=clip_count,
            clip_fraction=clip_fraction,
            overload_rule=f"clip_fraction > {fxlib.OVERLOAD_CLIP_FRACTION:g} (component at -128 or 127)",
            note="LNA 40 / VGA 30 / amp on (raised from 32 after an earlier weaker pass); "
                "provenance reports overload=false for this capture.",
        ),
    )]

    scenario = dict(
        role="scenario", kind="capture", recording=name,
        use_cases=[P25_USE_CASE, *P25_ALSO_USE_CASES],
        generator=BUILDER,
        source={
            "capture": "explorer agent live clip",
            "method": "POST /api/iqbuffer/clip on the explorer agent's staging build",
            "captured": explorer_truth.get("captured", "2026-09-25T13:19:26.000000333Z"),
            "device": "hackrf:0000000000000000d2b861dc263bc293",
        },
        datatype=meta["global"]["core:datatype"], sample_rate_hz=fs, n_samples=n, duration_s=n / fs,
        dbfs_reference="full scale = 127 codes per component; dBFS = 10 log10(mean |x|^2)",
        calibration="uncalibrated (no dBm): antenna unknown, as attached by the user",
        clip_count=clip_count, clip_fraction=clip_fraction, overload=prov["overload"],
        quantisation_limited=prov["quantisation_limited"],
        quantisation_noise_dbfs_per_hz=fxlib.quantisation_floor_dbfs_per_hz(fs),
        capture_settings={
            "lna_db": 40.0, "vga_db": 30.0, "amp": True,
            "bias_tee": "off",
            "antenna": "unknown",
        },
        identification_source=explorer_truth["source"],
        not_signals=explorer_truth.get("not_signals"),
        annotation_completeness=(
            "partial: the P25 channel named in the explorer's truth file, plus the whole-file "
            "overload artefact; the 800 MHz public-safety band around it was chosen from FCC 47 "
            "CFR 90.617, never from a frequency lookup that then tuned there"
        ),
        legal=LEGAL,
    )
    truth = {"annotations": [dict(sample_start=0, sample_count=n, label="capture", truth=scenario),
                             *items]}
    annotate.apply(meta, truth["annotations"], n)
    sigmf.write_meta(meta, meta_path)

    syncs_txt = ", ".join(f"{s['t_s']:.3f}s (H{s['hamming']})" for s in p25_truth["syncs"])
    return {
        "name": name,
        "clip_count": clip_count,
        "clip_fraction": clip_fraction,
        "truth_summary": (
            f"{f_center / 1e6:.4f} MHz: oracle {oracle['n_syncs']} frame sync(s) [{syncs_txt}], "
            f"0 on every neighbouring channel tried; overload clip_fraction {clip_fraction:.3f}"
        ),
        "disagreements": disagreements,
        "n": n,
        "size_bytes": data_path.stat().st_size,
        "sha256_data": fxlib.sha256_file(data_path),
        "sha256_meta": fxlib.sha256_file(meta_path),
    }


def update_manifest_p25(row: dict[str, Any]) -> None:
    manifest = json.loads(fxlib.MANIFEST.read_text())
    entries = manifest["entries"]
    name = row["name"]
    for kind, path, size, sha in (
        ("sigmf-data", f"hackrf/explorer-{DATE}/{name}.sigmf-data", row["size_bytes"], row["sha256_data"]),
        ("sigmf-meta", f"hackrf/explorer-{DATE}/{name}.sigmf-meta",
         (fxlib.FIXTURES / f"hackrf/explorer-{DATE}/{name}.sigmf-meta").stat().st_size,
         row["sha256_meta"]),
    ):
        entries[:] = [e for e in entries if not (e.get("name") == name and e.get("kind") == kind)]
        entry = {
            "name": name, "status": "committed", "path": path, "size_bytes": size, "sha256": sha,
            "kind": kind, "use_cases": [P25_USE_CASE, *P25_ALSO_USE_CASES], "license": LICENSE,
            "truth_summary": row["truth_summary"],
            "purpose": "explorer-agent captured live P25 C4FM channel (800 MHz public-safety), "
                      "blind truth cross-checked against the independent p25_ref.py frame-sync "
                      "oracle (T-975)",
            "datetime": "2026-09-25",
            "clip_count": row["clip_count"],
        }
        if kind == "sigmf-data":
            entry["lfs"] = True
        entries.append(entry)
    fxlib.MANIFEST.write_text(json.dumps(manifest, indent=2, sort_keys=False) + "\n")


def update_manifest_external(row: dict[str, Any], name: str, use_case: str,
                             purpose: str) -> None:
    manifest = json.loads(fxlib.MANIFEST.read_text())
    entries = manifest["entries"]
    rel_dir = f"store/explorer-{DATE}"
    for kind, size, sha in (
        ("sigmf-data", row["size_bytes"], row["sha256_data"]),
        ("sigmf-meta", (fxlib.store_dir() / f"explorer-{DATE}" / f"{name}.sigmf-meta").stat().st_size,
         row["sha256_meta"]),
    ):
        entries[:] = [e for e in entries if not (e.get("name") == name and e.get("kind") == kind)]
        entries.append({
            "name": name, "status": "external", "path": f"{rel_dir}/{name}.{kind}",
            "size_bytes": size, "sha256": sha, "kind": kind, "use_cases": [use_case],
            "license": LICENSE, "truth_summary": row["truth_summary"], "purpose": purpose,
            "datetime": "2026-09-25", "clip_count": row["clip_count"],
        })
    fxlib.MANIFEST.write_text(json.dumps(manifest, indent=2, sort_keys=False) + "\n")


def update_manifest(rows: list[dict[str, Any]]) -> None:
    manifest = json.loads(fxlib.MANIFEST.read_text())
    entries = manifest["entries"]
    for r in rows:
        name = r["name"]
        for kind, path, size, sha in (
            ("sigmf-data", f"hackrf/explorer-{DATE}/{name}.sigmf-data", r["size_bytes"], r["sha256_data"]),
            ("sigmf-meta", f"hackrf/explorer-{DATE}/{name}.sigmf-meta",
             (fxlib.FIXTURES / f"hackrf/explorer-{DATE}/{name}.sigmf-meta").stat().st_size,
             r["sha256_meta"]),
        ):
            entries[:] = [e for e in entries if not (e.get("name") == name and e.get("kind") == kind)]
            entry = {
                "name": name, "status": "committed", "path": path, "size_bytes": size, "sha256": sha,
                "kind": kind, "use_cases": ["SIGNAL-062"], "license": LICENSE,
                "truth_summary": r["truth_summary"],
                "purpose": "explorer-agent captured live FM/RDS stations, blind truth cross-checked "
                           "against the rds_ref.py oracle (T-935)",
                "datetime": "2026-09-25",
                "clip_count": r["clip_count"],
            }
            if kind == "sigmf-data":
                entry["lfs"] = True
            entries.append(entry)
    fxlib.MANIFEST.write_text(json.dumps(manifest, indent=2, sort_keys=False) + "\n")


def main(argv: list[str] | None = None) -> int:
    rows = [build_one(name) for name in STATIONS]
    update_manifest(rows)
    for r in rows:
        print(f"{r['name']}: {r['truth_summary']}")
        for d in r["disagreements"]:
            print(f"  DISAGREEMENT: {d}")

    flex_row = build_flex()
    update_manifest_external(
        flex_row, FLEX_NAME, FLEX_USE_CASE,
        purpose="explorer-agent captured live FLEX paging channels, blind truth cross-checked "
                "against the independent flex_ref.py sync-word oracle (T-949)",
    )
    print(f"{flex_row['name']}: {flex_row['truth_summary']}")
    for d in flex_row["disagreements"]:
        print(f"  DISAGREEMENT: {d}")

    p25_row = build_p25()
    update_manifest_p25(p25_row)
    print(f"{p25_row['name']}: {p25_row['truth_summary']}")
    for d in p25_row["disagreements"]:
        print(f"  DISAGREEMENT: {d}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
