"""Build the 2026-09-25 explorer-agent FM/RDS/FLEX/P25/LMR/DMR fixtures' truth annotations and
manifest rows.

    uv run --project py python py/fixtures/build_explorer_2026_09_25.py

Inputs (read-only): ``fixtures/hackrf/explorer-2026-09-25/*.sigmf-{meta,data}`` (already placed,
clipped live via ``POST /api/iqbuffer/clip`` on the explorer agent's staging build, 2026-09-25
~04:00-04:11 PDT, SF; the P25 capture ~06:19 PDT the same session) and the explorer's own
hidden-truth claim at ``~/.hackriff-ops/explorer/captures/20260925/*.truth.json``. Cross-checks
every station/channel against an independent oracle (``rds_ref.py``, ``flex_ref.py``,
``p25_ref.py``, ``ctcss_ref.py``, ``dmr_ref.py``) and writes what the oracle found alongside the
explorer's claim — it does not silently "fix" either one. The window-3 LMR/DMR captures (T-985,
~06:45-07:03 PDT) were taken through ``POST /api/outputs/record/start`` instead of the clip
endpoint, so unlike the earlier three their source ``.sigmf-meta`` carries no
``hackriff:clip_count`` yet either (same gap ``build_p25`` already handles).

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
import ctcss_ref
import dmr_ref
import flex_ref
import fxlib
import p25_ref
import rds_ref
import trim
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

#: The LMR/GMRS NBFM+CTCSS capture (T-985): 15 s at 2.4 Msps, 72 MB -- like FLEX, over the 25 MB
#: committed cap even before considering that the 8.1 s burst (the whole truth) alone would be
#: ~39 MB, so this goes to the external store whole, untrimmed.
CTCSS_NAME = "lmr-461p125-nbfm-ctcss"
CTCSS_USE_CASE = "SIGNAL-090"

#: The conventional DMR capture (T-985): 10 s at 2.4 Msps, 48 MB -- over the cap, but its whole
#: burst (measured: all 41 independently-found frame syncs) sits inside the file's final ~1.25 s
#: (duty 12%), so a trimmed tail keeps 100% of the real signal content under the cap.
DMR_NAME = "dmr-464p6125-bs"
DMR_USE_CASE = "SIGNAL-091"
DMR_TRIM_START_S = 5.0
DMR_TRIM_DURATION_S = 5.0  # 5 s * 2.4 Msps * 2 bytes/sample = 24,000,000 bytes, under the cap


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


#: kinds whose CTCSS claim (if any) is worth an independent oracle pass; a continuous, unmodulated
#: carrier carries no sub-audible signalling to check.
_CTCSS_RELEVANT_KINDS = ("nbfm-voice-ctcss", "nbfm-burst")


def build_ctcss() -> dict[str, Any]:
    """Builds the LMR/GMRS NBFM+CTCSS fixture (T-985) into the **external store** (gitignored):
    at 72 MB the raw 15 s/2.4 Msps capture is over fixtures/README.md's 25 MB committed cap, and
    even a trim keeping just its 8.1 s burst (~39 MB) would still be -- so unlike the DMR capture
    below this goes to the store whole, untrimmed, same discipline as ``build_flex``.

    Cross-checks every emission the explorer's truth names a CTCSS tone for against the
    independent oracle ``ctcss_ref.py``, run over the fixture's own claimed burst window (or the
    whole file where the explorer gave none), never over the explorer's own decode."""
    src_meta = EXPLORER_HOME / f"{CTCSS_NAME}.sigmf-meta"
    src_data = EXPLORER_HOME / f"{CTCSS_NAME}.sigmf-data"
    explorer_truth = json.loads((EXPLORER_HOME / f"{CTCSS_NAME}.truth.json").read_text())

    meta = sigmf.read_meta(src_meta)
    meta["global"]["core:license"] = LICENSE
    meta["global"]["core:description"] = f"hk-pipeline IQ capture buffer clip ({CTCSS_USE_CASE}, {CTCSS_NAME})"
    fs = float(meta["global"]["core:sample_rate"])
    fc = float(meta["captures"][0]["core:frequency"])
    n = fxlib.n_samples(src_data, meta["global"]["core:datatype"])
    prov = meta["global"][sigmf.PROVENANCE_KEY]

    # this capture's placed-by-hand source (recorded via /api/outputs/record/start, not the clip
    # endpoint the FM/RDS pair used) carries no hackriff:clip_count yet, same gap build_p25 fixed.
    if CLIP_COUNT_KEY not in meta["captures"][0]:
        meta["captures"][0][CLIP_COUNT_KEY] = fxlib.count_clipped(
            src_data, meta["global"]["core:datatype"], 0, n)
    clip_count = int(sum(c.get(CLIP_COUNT_KEY, 0) for c in meta["captures"]))
    clip_fraction = clip_count / n

    items: list[dict[str, Any]] = []
    disagreements: list[str] = []
    summary_bits: list[str] = []

    for em in explorer_truth["emissions"]:
        f_center = float(em["f_center_hz"])
        offset_hz = f_center - fc
        t0 = float(em.get("t_start_s", 0.0))
        dur = float(em.get("t_dur_s", n / fs - t0))
        label = f"{f_center / 1e6:.4f} MHz"
        claimed_hz = em.get("ctcss_hz")
        ctcss_truth: dict[str, Any] | None = None

        if em["kind"] in _CTCSS_RELEVANT_KINDS:
            oracle = ctcss_ref.decode_ci8(str(src_data), fs, offset_hz, start_s=t0, duration_s=dur)
            oracle_hz = oracle["ctcss"]["table_hz"] if oracle["ctcss"] else None
            agrees = (claimed_hz is None and oracle_hz is None) or (
                claimed_hz is not None and oracle_hz is not None and abs(claimed_hz - oracle_hz) < 0.05
            )
            if not agrees:
                disagreements.append(
                    f"{label}: explorer claims CTCSS {claimed_hz!r} Hz, oracle measured "
                    f"{oracle['tone_hz_measured']} Hz (SNR {oracle['tone_snr_db']} dB) -> table "
                    f"match {oracle_hz!r} Hz, over [{t0:.2f}s, {t0 + dur:.2f}s)"
                )
            ctcss_truth = {
                "tone_hz_measured": oracle["tone_hz_measured"],
                "tone_snr_db": oracle["tone_snr_db"],
                "min_snr_db": oracle["min_snr_db"],
                "table_match": oracle["ctcss"],
                "table": oracle["table"],
                "window": {"t_start_s": t0, "duration_s": dur},
                "decoder": oracle["decoder"],
                "oracle_agrees_with_explorer_ctcss": agrees,
                "explorer_claim": {
                    "ctcss_hz": claimed_hz,
                    "decoded": em.get("decoded"),
                    "source": "app blind detection + the explorer's own numpy oracle "
                              "tools/nbfm3.py (longest contiguous burst, NBFM discriminator, "
                              "Welch 1 Hz search), 2026-09-25 ~06:37-06:46 PDT "
                              "(journal-20260925.md)",
                },
            }
            match_txt = f"{oracle_hz:g} Hz" if oracle_hz else "no table match"
            summary_bits.append(
                f"{label}: oracle {oracle['tone_hz_measured']:.2f} Hz ({match_txt}, SNR "
                f"{oracle['tone_snr_db']:.1f} dB) vs explorer claim {claimed_hz!r} Hz"
            )

        items.append(dict(
            **whole(n),
            freq_lower_edge=f_center - float(em["bandwidth_hz"]) / 2,
            freq_upper_edge=f_center + float(em["bandwidth_hz"]) / 2,
            label=label,
            comment=em.get("decoded"),
            truth=dict(
                role="emission", kind=em["kind"], modulation="NBFM",
                center_hz=f_center, offset_hz=offset_hz, bandwidth_hz=float(em["bandwidth_hz"]),
                channel_hz=f_center, decoded=False,
                decode_note="Not decoded: blind detection + burst timing/CTCSS identification "
                            "only; no voice/audio content is stored (CLAUDE.md legal guardrails).",
                ctcss=ctcss_truth,
            ),
        ))

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
        role="scenario", kind="capture", recording=CTCSS_NAME,
        use_cases=[CTCSS_USE_CASE],
        generator=BUILDER,
        source={
            "capture": "explorer agent live clip",
            "method": "POST /api/outputs/record/start on the explorer agent's staging build",
            "captured": meta["captures"][0]["core:datetime"],
            "device": "hackrf:0000000000000000d2b861dc263bc293",
        },
        datatype=meta["global"]["core:datatype"], sample_rate_hz=fs, n_samples=n, duration_s=n / fs,
        dbfs_reference="full scale = 127 codes per component; dBFS = 10 log10(mean |x|^2)",
        calibration="uncalibrated (no dBm): antenna unknown, as attached by the user",
        clip_count=clip_count, clip_fraction=clip_fraction, overload=prov["overload"],
        quantisation_limited=prov["quantisation_limited"],
        quantisation_noise_dbfs_per_hz=fxlib.quantisation_floor_dbfs_per_hz(fs),
        capture_settings={"lna_db": 32.0, "vga_db": 30.0, "amp": True, "bias_tee": "off",
                          "antenna": "unknown"},
        identification_source=explorer_truth["source"],
        not_signals=explorer_truth.get("not_signals"),
        annotation_completeness=(
            "partial: the emissions named in the explorer's truth file, plus the whole-file "
            "overload artefact"
        ),
        legal=LEGAL,
    )
    truth = {"annotations": [dict(**whole(n), label="capture", truth=scenario), *items]}
    annotate.apply(meta, truth["annotations"], n)

    dst_dir = fxlib.store_dir() / f"explorer-{DATE}"
    dst_dir.mkdir(parents=True, exist_ok=True)
    dst_meta = dst_dir / f"{CTCSS_NAME}.sigmf-meta"
    dst_data = dst_dir / f"{CTCSS_NAME}.sigmf-data"
    sigmf.write_meta(meta, dst_meta)
    dst_data.write_bytes(src_data.read_bytes())

    return {
        "name": CTCSS_NAME,
        "clip_count": clip_count,
        "clip_fraction": clip_fraction,
        "truth_summary": "; ".join(summary_bits) + f"; overload clip_fraction {clip_fraction:.3f}",
        "disagreements": disagreements,
        "n": n,
        "size_bytes": dst_data.stat().st_size,
        "sha256_data": fxlib.sha256_file(dst_data),
        "sha256_meta": fxlib.sha256_file(dst_meta),
    }


def build_dmr() -> dict[str, Any]:
    """Builds the conventional-DMR fixture (T-985): the raw 10 s/2.4 Msps capture (48 MB) is over
    the committed cap, but its whole burst measures inside the file's final ~1.25 s (12% duty),
    so a 5 s tail trim (``DMR_TRIM_START_S``/``DMR_TRIM_DURATION_S``) keeps 100% of the real
    signal content at 24 MB, under the cap -- unlike the CTCSS capture above, this one *is*
    committed (Git LFS), with its untrimmed original kept in the external store per
    fixtures/README.md's ``source``/``source_start_s`` convention (same as
    ``py/fixtures/build_2026_09_13.py``'s trimmed committed fixtures).

    Cross-checks the DMR sync count against the independent oracle ``dmr_ref.py``, run over the
    *trimmed* clip's own samples, never the explorer's ``tools/dmrsync.py`` claim."""
    src_meta = EXPLORER_HOME / f"{DMR_NAME}.sigmf-meta"
    src_data = EXPLORER_HOME / f"{DMR_NAME}.sigmf-data"
    explorer_truth = json.loads((EXPLORER_HOME / f"{DMR_NAME}.truth.json").read_text())

    # in-memory only: the explorer's own capture under ~/.hackriff-ops is never written back to
    # (it is evidence, not a build artefact -- unlike build_p25's meta_path, which reads a copy
    # already placed inside the repo).
    src_full = sigmf.read_meta(src_meta)
    fs = float(src_full["global"]["core:sample_rate"])
    n_full = fxlib.n_samples(src_data, src_full["global"]["core:datatype"])
    if CLIP_COUNT_KEY not in src_full["captures"][0]:
        src_full["captures"][0][CLIP_COUNT_KEY] = fxlib.count_clipped(
            src_data, src_full["global"]["core:datatype"], 0, n_full)

    # the untrimmed original, kept in the external store so the full 10 s stays available.
    store_dir = fxlib.store_dir() / f"explorer-{DATE}"
    store_dir.mkdir(parents=True, exist_ok=True)
    store_meta = store_dir / f"{DMR_NAME}-full.sigmf-meta"
    store_data = store_dir / f"{DMR_NAME}-full.sigmf-data"
    sigmf.write_meta(src_full, store_meta)
    store_data.write_bytes(src_data.read_bytes())
    source_sha256 = fxlib.sha256_file(store_data)

    meta_path = OUT / f"{DMR_NAME}.sigmf-meta"
    data_path = OUT / f"{DMR_NAME}.sigmf-data"
    trim.trim(src_meta, meta_path, start_s=DMR_TRIM_START_S, duration_s=DMR_TRIM_DURATION_S,
             description=f"hk-pipeline IQ capture buffer clip ({DMR_USE_CASE}, {DMR_NAME}), "
                         f"trimmed to [{DMR_TRIM_START_S}s, "
                         f"{DMR_TRIM_START_S + DMR_TRIM_DURATION_S}s) of the original 10 s "
                         f"capture -- the whole burst measures inside this window "
                         f"(source: store/explorer-{DATE}/{DMR_NAME}-full.sigmf-{{meta,data}})")

    meta = sigmf.read_meta(meta_path)
    meta["global"]["core:license"] = LICENSE
    fc = float(meta["captures"][0]["core:frequency"])
    n = fxlib.n_samples(data_path, meta["global"]["core:datatype"])
    prov = meta["global"][sigmf.PROVENANCE_KEY]
    clip_count = int(sum(c.get(CLIP_COUNT_KEY, 0) for c in meta["captures"]))
    clip_fraction = clip_count / n

    items: list[dict[str, Any]] = []
    disagreements: list[str] = []
    summary_bits: list[str] = []

    for em in explorer_truth["emissions"]:
        f_center = float(em["f_center_hz"])
        offset_hz = f_center - fc
        label = f"{f_center / 1e6:.4f} MHz"
        dmr_truth: dict[str, Any] | None = None
        ctcss_truth: dict[str, Any] | None = None

        if em["kind"] == "dmr-4fsk-bursts":
            oracle = dmr_ref.decode_ci8(str(data_path), fs, offset_hz, duration_s=n / fs)
            claim_match = re.search(r"found (\d+)x", em["decoded"])
            claimed_syncs = int(claim_match.group(1)) if claim_match else None
            agrees = claimed_syncs is not None and claimed_syncs == oracle["n_syncs"]
            if not agrees:
                disagreements.append(
                    f"{label}: explorer claims {claimed_syncs} sync(s) (whole 10 s file), oracle "
                    f"found {oracle['n_syncs']} in this {DMR_TRIM_DURATION_S:g} s trimmed clip "
                    f"(Hamming <= {oracle['max_hamming']}/{oracle['sync_bits']} bits, >= "
                    f"{dmr_ref.MIN_PHASE_CORROBORATION}/{oracle['n_timing_phases']} timing-phase "
                    f"corroboration)"
                )
            dmr_truth = {
                "sync_patterns": oracle["sync_patterns"],
                "sync_rate_bd": oracle["sync_rate_bd"],
                "n_syncs": oracle["n_syncs"],
                "syncs_by_pattern": oracle["syncs_by_pattern"],
                "syncs": [{"t_s": round(s["sample"] / (fs / max(1, int(fs // 48_000.0))), 4),
                          "hamming": s["hamming"], "order": s["order"], "n_phases": s["n_phases"]}
                         for s in oracle["syncs"]],
                "levels": oracle["levels"],
                "decoder": oracle["decoder"],
                "oracle_agrees_with_explorer_sync_count": agrees,
                "explorer_claim": {
                    "kind": em["kind"],
                    "decoded": em["decoded"],
                    "claimed_syncs_in_full_10s_file": claimed_syncs,
                    "source": "app blind detection + the explorer's own numpy oracle "
                              "tools/dmrsync.py (4800 Bd 4FSK slicer against the ETSI SYNC "
                              "table), 2026-09-25 ~07:02 PDT (journal-20260925.md)",
                },
            }
            summary_bits.append(
                f"{label}: oracle {oracle['n_syncs']} sync(s) {oracle['syncs_by_pattern']} in "
                f"this {DMR_TRIM_DURATION_S:g} s clip vs explorer's {claimed_syncs} in the full "
                f"10 s file"
            )
        elif em["kind"] == "nbfm-burst":
            oracle = ctcss_ref.decode_ci8(str(data_path), fs, offset_hz, duration_s=n / fs)
            claimed_hz = em.get("ctcss_hz")
            oracle_hz = oracle["ctcss"]["table_hz"] if oracle["ctcss"] else None
            # explorer's own note says this tone was "not re-verified in this file" -- no claim
            # to check this trimmed clip against, so a null oracle result is not a disagreement.
            ctcss_truth = {
                "tone_hz_measured": oracle["tone_hz_measured"],
                "tone_snr_db": oracle["tone_snr_db"],
                "table_match": oracle["ctcss"],
                "table": oracle["table"],
                "decoder": oracle["decoder"],
                "note": "explorer's own claim is for an earlier, wider observation window, not "
                       "re-verified in this specific file/clip; a null or differing oracle result "
                       "here is not a disagreement with that claim.",
                "explorer_claim": {"ctcss_hz": claimed_hz, "decoded": em.get("decoded")},
            }
            summary_bits.append(
                f"{label}: oracle {oracle['tone_hz_measured']:.2f} Hz "
                f"({'match ' + f'{oracle_hz:g} Hz' if oracle_hz else 'no table match'}) "
                f"(not re-verifying explorer's earlier claim)"
            )

        items.append(dict(
            **whole(n),
            freq_lower_edge=f_center - float(em["bandwidth_hz"]) / 2,
            freq_upper_edge=f_center + float(em["bandwidth_hz"]) / 2,
            label=label,
            comment=em.get("decoded"),
            truth=dict(
                role="emission", kind=em["kind"],
                modulation="4FSK" if em["kind"] == "dmr-4fsk-bursts" else "NBFM",
                center_hz=f_center, offset_hz=offset_hz, bandwidth_hz=float(em["bandwidth_hz"]),
                channel_hz=f_center, decoded=False,
                decode_note="Not decoded: blind detection + sync/tone identification only; no "
                            "DMR payload (data/voice) content is stored (CLAUDE.md legal "
                            "guardrails).",
                dmr=dmr_truth, ctcss=ctcss_truth,
            ),
        ))

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
        role="scenario", kind="capture", recording=DMR_NAME,
        use_cases=[DMR_USE_CASE],
        generator=BUILDER,
        source={
            "capture": "explorer agent live clip",
            "method": "POST /api/outputs/record/start on the explorer agent's staging build",
            "captured": meta["captures"][0]["core:datetime"],
            "device": "hackrf:0000000000000000d2b861dc263bc293",
            "trim": f"[{DMR_TRIM_START_S}s, {DMR_TRIM_START_S + DMR_TRIM_DURATION_S}s) of the "
                   f"original 10 s capture, kept whole at store/explorer-{DATE}/"
                   f"{DMR_NAME}-full.sigmf-{{meta,data}}",
        },
        datatype=meta["global"]["core:datatype"], sample_rate_hz=fs, n_samples=n, duration_s=n / fs,
        dbfs_reference="full scale = 127 codes per component; dBFS = 10 log10(mean |x|^2)",
        calibration="uncalibrated (no dBm): antenna unknown, as attached by the user",
        clip_count=clip_count, clip_fraction=clip_fraction, overload=prov["overload"],
        quantisation_limited=prov["quantisation_limited"],
        quantisation_noise_dbfs_per_hz=fxlib.quantisation_floor_dbfs_per_hz(fs),
        capture_settings={"lna_db": 32.0, "vga_db": 30.0, "amp": True, "bias_tee": "off",
                          "antenna": "unknown"},
        identification_source=explorer_truth["source"],
        not_signals=explorer_truth.get("not_signals"),
        annotation_completeness=(
            "partial: the emissions named in the explorer's truth file, plus the whole-clip "
            "overload artefact; this is a 5 s tail trim of a 10 s original, not the whole capture"
        ),
        legal=LEGAL,
    )
    truth = {"annotations": [dict(**whole(n), label="capture", truth=scenario), *items]}
    annotate.apply(meta, truth["annotations"], n)
    sigmf.write_meta(meta, meta_path)

    return {
        "name": DMR_NAME,
        "clip_count": clip_count,
        "clip_fraction": clip_fraction,
        "truth_summary": "; ".join(summary_bits) + f"; overload clip_fraction {clip_fraction:.3f}",
        "disagreements": disagreements,
        "n": n,
        "size_bytes": data_path.stat().st_size,
        "sha256_data": fxlib.sha256_file(data_path),
        "sha256_meta": fxlib.sha256_file(meta_path),
        "source_sha256": source_sha256,
        "source_start_s": DMR_TRIM_START_S,
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


def update_manifest_dmr(row: dict[str, Any]) -> None:
    """Like ``update_manifest_p25`` for the trimmed committed clip, plus one external-store entry
    (per file) for the untrimmed 10 s original ``source``/``source_start_s`` points at."""
    manifest = json.loads(fxlib.MANIFEST.read_text())
    entries = manifest["entries"]
    name = row["name"]
    rel_store = f"store/explorer-{DATE}/{name}-full"
    for kind, path, size, sha in (
        ("sigmf-data", f"hackrf/explorer-{DATE}/{name}.sigmf-data", row["size_bytes"], row["sha256_data"]),
        ("sigmf-meta", f"hackrf/explorer-{DATE}/{name}.sigmf-meta",
         (fxlib.FIXTURES / f"hackrf/explorer-{DATE}/{name}.sigmf-meta").stat().st_size,
         row["sha256_meta"]),
    ):
        entries[:] = [e for e in entries if not (e.get("name") == name and e.get("kind") == kind)]
        entry = {
            "name": name, "status": "committed", "path": path, "size_bytes": size, "sha256": sha,
            "kind": kind, "use_cases": [DMR_USE_CASE], "license": LICENSE,
            "truth_summary": row["truth_summary"],
            "purpose": "explorer-agent captured live conventional DMR (Tier II) channel, blind "
                      "truth cross-checked against the independent dmr_ref.py frame-sync oracle "
                      "(T-985); a 5 s tail trim of the original 10 s capture",
            "datetime": "2026-09-25",
            "clip_count": row["clip_count"],
        }
        if kind == "sigmf-data":
            entry["lfs"] = True
            entry["source"] = f"{rel_store}.sigmf-data"
            entry["source_sha256"] = row["source_sha256"]
            entry["source_start_s"] = row["source_start_s"]
        entries.append(entry)

    store_meta_path = fxlib.store_dir() / f"explorer-{DATE}" / f"{name}-full.sigmf-meta"
    store_data_path = fxlib.store_dir() / f"explorer-{DATE}" / f"{name}-full.sigmf-data"
    for kind, path, size in (
        ("sigmf-data", f"{rel_store}.sigmf-data", store_data_path.stat().st_size),
        ("sigmf-meta", f"{rel_store}.sigmf-meta", store_meta_path.stat().st_size),
    ):
        ext_name = f"{name}-full"
        entries[:] = [e for e in entries if not (e.get("name") == ext_name and e.get("kind") == kind)]
        entries.append({
            "name": ext_name, "status": "external", "path": path, "size_bytes": size,
            "sha256": fxlib.sha256_file(store_meta_path if kind == "sigmf-meta" else store_data_path),
            "kind": kind, "use_cases": [DMR_USE_CASE], "license": LICENSE,
            "purpose": f"untrimmed 10 s original the committed {name} clip was trimmed from "
                      f"(fixtures/README.md's source/source_start_s convention)",
            "datetime": "2026-09-25",
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

    ctcss_row = build_ctcss()
    update_manifest_external(
        ctcss_row, CTCSS_NAME, CTCSS_USE_CASE,
        purpose="explorer-agent captured live NBFM land-mobile/GMRS-adjacent voice bursts, blind "
                "truth cross-checked against the independent ctcss_ref.py CTCSS tone oracle "
                "(T-985)",
    )
    print(f"{ctcss_row['name']}: {ctcss_row['truth_summary']}")
    for d in ctcss_row["disagreements"]:
        print(f"  DISAGREEMENT: {d}")

    dmr_row = build_dmr()
    update_manifest_dmr(dmr_row)
    print(f"{dmr_row['name']}: {dmr_row['truth_summary']}")
    for d in dmr_row["disagreements"]:
        print(f"  DISAGREEMENT: {d}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
