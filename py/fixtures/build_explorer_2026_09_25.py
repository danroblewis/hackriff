"""Build the 2026-09-25 explorer-agent FM/RDS fixture pair's truth annotations and manifest rows.

    uv run --project py python py/fixtures/build_explorer_2026_09_25.py

Inputs (read-only): ``fixtures/hackrf/explorer-2026-09-25/*.sigmf-{meta,data}`` (already placed,
clipped live via ``POST /api/iqbuffer/clip`` on the explorer agent's staging build, 2026-09-25
~04:00-04:11 PDT, SF) and the explorer's own hidden-truth claim at
``~/.hackriff-ops/explorer/captures/20260925/*.truth.json``. Cross-checks every station against
the independent oracle ``py/fixtures/rds_ref.py`` and writes what the oracle found alongside the
explorer's claim — it does not silently "fix" either one.

Outputs: rewrites ``hackriff:truth`` annotations into the two ``.sigmf-meta`` files (via
``annotate.annotate``) and appends/refreshes their rows in ``fixtures/manifest.json``.

Legal: receive-only capture; RDS PI/PS/PTY are public broadcast station identity, not third-party
payload content (CLAUDE.md legal guardrails).
"""

from __future__ import annotations

import json
import sys
from pathlib import Path
from typing import Any

import annotate
import fxlib
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
    return 0


if __name__ == "__main__":
    sys.exit(main())
