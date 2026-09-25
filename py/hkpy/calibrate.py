"""The `hackriff.calibration/1` generator (ADR-0015 §13.2/§13.3, T-660 (b)). Tooling only, never
the real-time path.

Given empirical null-sample draws for one `(block, metric, null, n, fill bucket)` cell, computes
the **expressible levels** a quantile table can honestly publish and the ones it must refuse. The
contract this exists to keep, restated from ADR-0015 §13.2:

    A level is expressible when the realised significance of its threshold is within
    0.25 bits of the claim. `levels` is a list of the admissible answers, not a curve:
    no interpolation, no extrapolation.

So :func:`calibrate_cell` **never rounds an inexpressible level into `levels`** — it goes into
`unexpressible` instead, with the realised significance the data actually supports and why it was
refused (an atom in the null distribution, the `log2(windows)` sample ceiling, or the per-metric
6-bit calibrated claim cap, ADR-0015 §13.2 item 1). `admissible_bits` is the max of what
`levels` holds, which is already `min(max expressible level, claim cap, sample ceiling)` by
construction.

:func:`write_calibration_file` assembles one or more cells into the `hackriff.calibration/1`
document `crates/hk-synth/src/calibration.rs`'s loader reads, including §13.1's `groups` /
`correlation` blocks when the caller has them.

The Rust mirror of every constant here is `hk_synth::calibration`; `py/tests/test_calibrate.py`
keeps the two in step.

**Generating the shipped tables (T-853 = MAUTO M-2).** :func:`document_from_draws` turns one
block's null draws — printed by the Rust ``calibration_draws`` example, which runs the block's
canonical noise chain (``hk_synth::nullchain``) through the real blocks — into a
``hackriff.calibration/1`` document: one cell per (metric, ``noise`` null, support), §13.1's
``correlation`` and ``groups`` for any stage the block publishes more than one metric at (groups
whose members correlate at ``|rho| >= 0.3`` are merged, never split), and the conditioning
bounds passed explicitly. ``python -m hkpy.calibrate generate`` runs the example and writes
``synth/calibration/<block>.json``; it passes :data:`TIGHT_NOMINAL_BOUNDS` because ADR-0015 §16.4
requires it of the first real generation while the T-619 fill amendment is pending. This module
does statistics only; every raw value comes from the Rust blocks.
"""

from __future__ import annotations

import argparse
import json
import math
import subprocess
import sys
from collections.abc import Mapping, Sequence
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

import numpy as np

#: Schema name the loader accepts. A `raw`-only (pre-§13.2) table is not loadable.
CALIBRATION_SCHEMA = "hackriff.calibration/1"

#: A level is expressible when its realised significance is within this many bits of the claim
#: (§13.2). Stated in the file (`levels_expressibility_tolerance_bits`), never assumed by a reader.
EXPRESSIBILITY_TOLERANCE_BITS = 0.25

#: Per-metric ceiling on any calibrated metric's claim (§13.2 item 1). Above 6 bits the
#: quantisation discount is unmeasured, not small.
CALIBRATED_CLAIM_CAP_BITS = 6.0

#: The §13.3 sampling budget: N per (block, metric, null, n, fill bucket) cell. Sufficient to
#: *touch* 12 bits once and to put ~64 exceedances in the tail at the 6-bit claim cap.
WINDOWS_PER_CELL_FLOOR = 4096

#: The ask ladder a cell is calibrated against by default: every S0-S3 floor/cap boundary
#: (ADR-0015 §1.3) plus one point above the claim cap, so a generator run demonstrates the refusal
#: rather than only ever landing inside it.
DEFAULT_ASK_BITS: tuple[float, ...] = (1.0, 2.0, 3.0, 4.0, 6.0, 8.0, 10.0, 12.0)

#: §13.3's `nominal` bucket as ADR-0015 states it today. **Not the tighter bucket T-619 measured**
#: (docs/21 §10.10: σ >= 1.0 LSB and clip <= 0.10 holds every path measured so far to <= 1.2 bits
#: at a 6-bit claim, against 5-6 bits over-claimed here on AM/OOK at high clip). ADR-0015 §16.4
#: proposes accepting that amendment before this generator emits any real table; until the ADR
#: text changes, callers who want the tighter bucket pass `nominal_bounds=TIGHT_NOMINAL_BOUNDS`
#: rather than this module silently choosing for them.
ADR_NOMINAL_BOUNDS = {"sigma_lsb_min": 0.5, "clip_fraction_max": 0.30}

#: The T-619-measured bucket (docs/21 §10.10's "the cheap answer is to tighten globally").
TIGHT_NOMINAL_BOUNDS = {"sigma_lsb_min": 1.0, "clip_fraction_max": 0.10}


def _git_sha() -> str:
    try:
        out = subprocess.run(
            ["git", "rev-parse", "--short=12", "HEAD"],
            capture_output=True,
            text=True,
            check=True,
            cwd=Path(__file__).resolve().parent,
        )
        return out.stdout.strip()
    except Exception:
        return "unknown"


def _order_threshold(
    sorted_asc: np.ndarray, bits: float, windows: int
) -> tuple[float, float]:
    """`(threshold, realised_bits)` for a claim of `bits`, by the empirical order statistic.

    `threshold` is the value whose upper tail holds closest to `2**-bits` of the windows, taken
    from the **large side** (the direction convention: evidence is large, ADR-0015 §13.1).
    `realised_bits` is what the data actually delivers at that threshold — never the asked-for
    value — computed from the *count* of windows at or above it, so an atom (many windows sharing
    one value) is visible as a realised significance below the claim, not silently granted.
    """
    target_count = max(1, round(windows * (2.0**-bits)))
    target_count = min(target_count, windows)
    idx = windows - target_count  # 0-based index into sorted_asc of the threshold
    threshold = float(sorted_asc[idx])
    realised_count = int(np.sum(sorted_asc >= threshold))
    realised_p = realised_count / windows
    realised_bits = -math.log2(realised_p) if realised_p > 0 else math.log2(windows)
    return threshold, realised_bits


def calibrate_cell(
    null_samples: Sequence[float] | np.ndarray,
    *,
    ask_bits: Sequence[float] = DEFAULT_ASK_BITS,
    tolerance_bits: float = EXPRESSIBILITY_TOLERANCE_BITS,
    claim_cap_bits: float = CALIBRATED_CLAIM_CAP_BITS,
) -> dict[str, Any]:
    """One §13.2 table cell's `levels` / `unexpressible` / `admissible_bits`, from raw null draws.

    `null_samples` are the metric's raw values under the null, **already in the evidence
    direction** (larger = more significant — ADR-0015 §13.1's direction convention; a caller whose
    raw statistic is "evidence when small", e.g. EVM, negates or inverts before calling this).

    Never rounds: a level within `tolerance_bits` of what the data realises is published in
    `levels`; every other asked level — an atom, a level above `log2(windows)`, or one above
    `claim_cap_bits` — is refused into `unexpressible` with the realised significance the
    generator actually measured, never the claimed one.
    """
    arr = np.sort(np.asarray(null_samples, dtype=np.float64))
    windows = int(arr.size)
    if windows < 2:
        raise ValueError("need at least 2 null windows to calibrate a cell")
    sample_ceiling_bits = math.log2(windows)
    distinct_values = int(np.unique(arr).size)

    levels: list[dict[str, float]] = []
    unexpressible: list[dict[str, Any]] = []
    for bits in sorted(set(ask_bits)):
        threshold, realised_bits = _order_threshold(arr, bits, windows)
        if bits > claim_cap_bits:
            unexpressible.append(
                {
                    "bits": float(bits),
                    "realised_bits": float(min(realised_bits, claim_cap_bits)),
                    "reason": "above_calibrated_claim_cap",
                }
            )
            continue
        if bits > sample_ceiling_bits:
            unexpressible.append(
                {
                    "bits": float(bits),
                    "realised_bits": float(sample_ceiling_bits),
                    "reason": "above_sample_ceiling",
                }
            )
            continue
        if abs(realised_bits - bits) <= tolerance_bits:
            levels.append(
                {
                    "bits": float(bits),
                    "threshold": threshold,
                    "realised_bits": float(realised_bits),
                }
            )
        else:
            # An atom: the support cannot express this level within tolerance, in either
            # direction. Refuse it rather than publish a rounded or interpolated answer.
            unexpressible.append(
                {
                    "bits": float(bits),
                    "realised_bits": float(realised_bits),
                    "reason": "atom",
                }
            )

    admissible_bits = max((level["bits"] for level in levels), default=0.0)
    return {
        "windows": windows,
        "sample_ceiling_bits": sample_ceiling_bits,
        "distinct_values": distinct_values,
        "levels": levels,
        "unexpressible": unexpressible,
        "admissible_bits": admissible_bits,
    }


def calibration_document(
    *,
    block: str,
    bucket: str,
    tables: Sequence[Mapping[str, Any]],
    nominal_bounds: Mapping[str, float] = ADR_NOMINAL_BOUNDS,
    correlation: Mapping[str, Any] | None = None,
    groups: Mapping[str, Any] | None = None,
    generated_utc: str | None = None,
) -> dict[str, Any]:
    """Assembles a `hackriff.calibration/1` document (§13.2's schema) from calibrated cells.

    `tables` entries carry `metric`, `null`, `n` plus whatever :func:`calibrate_cell` returned for
    that cell. `correlation` / `groups` are §13.1's mandatory-when-more-than-one-metric blocks;
    omitted when a block publishes one metric per stage.
    """
    doc: dict[str, Any] = {
        "schema": CALIBRATION_SCHEMA,
        "block": block,
        "generated_utc": generated_utc
        or datetime.now(UTC).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "generator": f"py/hkpy/calibrate.py@{_git_sha()}",
        "conditioning": {
            "key": "adc_fill_sigma_lsb",
            "bucket": bucket,
            "sigma_lsb_range": [nominal_bounds["sigma_lsb_min"], 77.0],
            "clip_fraction_max": nominal_bounds["clip_fraction_max"],
        },
        "tables": list(tables),
    }
    if correlation is not None:
        doc["correlation"] = dict(correlation)
    if groups is not None:
        doc["groups"] = dict(groups)
    return doc


def write_calibration_file(path: str | Path, document: Mapping[str, Any]) -> Path:
    """Writes a `calibration_document()` result to `path` (`synth/calibration/<block>.json`)."""
    out = Path(path)
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(document, indent=2) + "\n")
    return out


#: Two metrics a block declared in different groups stay separate only below this |rho| in
#: the shipped matrix (ADR-0015 §13.1). At or above it the generator merges their groups.
GROUP_SPLIT_MAX_RHO = 0.3

#: The null every shipped table is drawn under. The mismatched-parameter nulls (§13.3's other
#: two) need per-block signal corpora and are not generated yet; a cell for them is `NoTable`.
NOISE_NULL = "noise"


def support_slack(n: int) -> int:
    """How far above a cell's support `n` its windows may run: `n // 50 + 16`. Mirror of
    `hk_synth::calibration::support_matches`; a window outside `[n, n + slack]` is not scored
    against the cell, and a generated cell whose draws were is refused."""
    return n // 50 + 16


def _ranks(x: np.ndarray) -> np.ndarray:
    """Average ranks (ties share the mean rank), as Spearman needs."""
    order = np.argsort(x, kind="mergesort")
    ranks = np.empty(len(x), dtype=np.float64)
    ranks[order] = np.arange(len(x), dtype=np.float64)
    _, inv, counts = np.unique(x, return_inverse=True, return_counts=True)
    sums = np.zeros(len(counts))
    np.add.at(sums, inv, ranks)
    return sums[inv] / counts[inv]


def spearman_matrix(columns: Sequence[np.ndarray]) -> list[list[float]]:
    """Spearman's rho between every pair of equally long columns. A constant column has no rank
    information; its correlations are reported as 1.0 (fail closed: it cannot justify a split)."""
    r = [_ranks(np.asarray(c, dtype=np.float64)) for c in columns]
    k = len(r)
    out = [[1.0] * k for _ in range(k)]
    for i in range(k):
        for j in range(i + 1, k):
            a, b = r[i] - r[i].mean(), r[j] - r[j].mean()
            den = math.sqrt(float(np.dot(a, a)) * float(np.dot(b, b)))
            rho = float(np.dot(a, b)) / den if den > 0 else 1.0
            out[i][j] = out[j][i] = round(rho, 4)
    return out


def _evidence_direction(raw: Sequence[float], direction: str) -> np.ndarray:
    arr = np.asarray(raw, dtype=np.float64)
    return -arr if direction == "smaller" else arr


def document_from_draws(
    draws: Mapping[str, Any],
    *,
    nominal_bounds: Mapping[str, float],
    ask_bits: Sequence[float] = DEFAULT_ASK_BITS,
    generated_utc: str | None = None,
) -> dict[str, Any]:
    """One block's `calibration_draws` output -> a `hackriff.calibration/1` document.

    `nominal_bounds` has no default on purpose: the caller states which fill bucket the tables
    are conditioned on (ADR-0015 §16.4).
    """
    tables: list[dict[str, Any]] = []
    stage_metrics: dict[str, dict[str, str]] = {}
    for cell in draws["cells"]:
        n = int(cell["n"])
        lo, hi = cell.get("min_n", n), cell.get("max_n", n)
        if not (n <= lo and hi - n <= support_slack(n)):
            # A cell keyed on n whose windows had another support is not that cell (§13.2).
            raise ValueError(
                f"{draws['block']}: draws at n in [{lo}, {hi}] for a cell at n = {n}"
            )
        for metric, m in sorted(cell["metrics"].items()):
            samples = _evidence_direction(m["raw"], m["direction"])
            cal = calibrate_cell(samples, ask_bits=ask_bits)
            tables.append(
                {"metric": metric, "null": NOISE_NULL, "n": int(cell["n"]), **cal}
            )
            stage_metrics.setdefault(m["stage"], {})[metric] = m["group"]

    correlation = None
    groups: dict[str, dict[str, list[str]]] = {}
    multi = {st: ms for st, ms in stage_metrics.items() if len(ms) > 1}
    if len(multi) > 1:
        raise ValueError(
            "more than one stage with several metrics: one correlation block only"
        )
    for stage, metric_groups in multi.items():
        names = sorted(metric_groups)
        # The largest support's windows: the tables' own corpus, at the file's own n.
        cell = max(draws["cells"], key=lambda c: c["n"])
        cols = [
            _evidence_direction(
                cell["metrics"][n]["raw"], cell["metrics"][n]["direction"]
            )
            for n in names
        ]
        if len({len(c) for c in cols}) != 1:
            raise ValueError(f"{stage}: metrics not present in every window")
        rho = spearman_matrix(cols)
        correlation = {
            "method": "spearman",
            "windows": len(cols[0]),
            "n": int(cell["n"]),
            "corpus": f"{NOISE_NULL}: {draws.get('corpus', '')}",
            "metrics": names,
            "rho": rho,
        }
        # Union-find over the declared groups; `undeclared` is one group (§13.1's default).
        parent = {n: metric_groups[n] for n in names}

        def find(g: str) -> str:
            while parent.get(g, g) != g:
                g = parent[g]
            return g

        for g in set(metric_groups.values()):
            parent.setdefault(g, g)
        for i, a in enumerate(names):
            for j in range(i + 1, len(names)):
                b = names[j]
                ga, gb = find(metric_groups[a]), find(metric_groups[b])
                if ga != gb and abs(rho[i][j]) >= GROUP_SPLIT_MAX_RHO:
                    keep, drop = sorted((ga, gb))
                    parent[drop] = keep
        merged: dict[str, list[str]] = {}
        for n in names:
            merged.setdefault(find(metric_groups[n]), []).append(n)
        groups[stage] = {g: sorted(ms) for g, ms in sorted(merged.items())}

    doc = calibration_document(
        block=draws["block"],
        bucket="nominal",
        tables=tables,
        nominal_bounds=nominal_bounds,
        correlation=correlation,
        groups=groups or None,
        generated_utc=generated_utc,
    )
    doc["null_corpus"] = draws.get("corpus", "")
    doc["null_fill"] = draws.get("fill", {})
    return doc


def _run_draws(
    blocks: Sequence[str], windows: int, threads: int
) -> list[dict[str, Any]]:
    root = Path(__file__).resolve().parents[2]
    cmd = [
        "cargo",
        "run",
        "--quiet",
        "--release",
        "-p",
        "hk-synth",
        "--example",
        "calibration_draws",
        "--",
        "--windows",
        str(windows),
        "--threads",
        str(threads),
        *blocks,
    ]
    out = subprocess.run(cmd, cwd=root, check=True, capture_output=True, text=True)
    return json.loads(out.stdout)


def main(argv: Sequence[str] | None = None) -> int:
    ap = argparse.ArgumentParser(prog="python -m hkpy.calibrate")
    sub = ap.add_subparsers(dest="cmd", required=True)
    gen = sub.add_parser(
        "generate", help="draw nulls through the Rust blocks and write tables"
    )
    gen.add_argument(
        "blocks", nargs="*", help="block names (default: every null chain)"
    )
    gen.add_argument("--windows", type=int, default=WINDOWS_PER_CELL_FLOOR)
    gen.add_argument("--threads", type=int, default=6)
    gen.add_argument(
        "--draws", type=Path, help="read draws JSON instead of running cargo"
    )
    gen.add_argument(
        "--out",
        type=Path,
        default=Path(__file__).resolve().parents[2] / "synth" / "calibration",
    )
    args = ap.parse_args(argv)
    if args.draws:
        all_draws = json.loads(args.draws.read_text())
    else:
        all_draws = _run_draws(args.blocks, args.windows, args.threads)
    for d in all_draws:
        name = d["block"].split("@", 1)[0]
        doc = document_from_draws(d, nominal_bounds=TIGHT_NOMINAL_BOUNDS)
        path = write_calibration_file(args.out / f"{name}.json", doc)
        print(f"{path}: {len(doc['tables'])} cells", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
