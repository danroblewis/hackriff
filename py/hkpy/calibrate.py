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
"""

from __future__ import annotations

import json
import math
import subprocess
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
