"""ADC fill: the measurement, the two buckets, and the ladder sweep (T-625).

**Fill, not gain.** T-547 (docs/21 §4) applied 51 dB of gain with the ADC *skipped* and reproduced
the float calibration table to 0.02 bits on every metric, with a bit-identical demodulator success
rate — every metric in the set is a ratio or a power-normalised statistic, so gain on float IQ is
exactly a no-op. What moves the evidence tails is the ADC, and within the ADC it is **under-fill**
(σ = 0.21 LSB costs 1.0–1.6 bits and swings demod success 19 % → 53 %), not clipping (28.4 %
clipped costs ≤ 0.34 bits). So the corpus stratifies on σ in LSB, docs/22 §2 axis A3, and a
dashboard keyed on gain state is keyed on a no-op.

**Six levels, two buckets.** :data:`FILL_LADDER` is the six-level sweep docs/22 §2/§4.4 asks for.
:func:`fill_bucket` is the *conditioning key* ADR-0015 §13.3 fixes, and it has only two buckets
with a table between them, because σ = 0.5 LSB is the only boundary docs/21 §3's sweep supports:
the rows from 0.57 to 77 LSB sit within 0.34 bits of each other, and 0.21 LSB is 1.64 bits away.
The ladder is therefore a *measurement sweep* over six rungs that classify into two buckets; it is
not six buckets, and nothing here should grow one without a measurement behind it.

Mirror of ``hk_model::FillBucket`` (crates/hk-model/src/provenance.rs). The constants are asserted
equal across the two in ``py/tests/test_fill_ladder.py``.
"""

from __future__ import annotations

import argparse
import json
import math
import sys
from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import Any

import numpy as np

#: σ below which no calibration table exists, in ADC LSB (ADR-0015 §13.3).
UNDER_FILL_SIGMA_LSB = 0.5
#: Clip fraction above which no calibration table exists — a boundary of the *measurement*.
OVER_CLIP_FRACTION = 0.30

#: docs/22 §2 axis A3: the six fill levels, σ in LSB. 0.21 is the under-filled end; 2.0 is where
#: four of docs/21 §6's five real captures sit; 77 is where the measurement stopped (28.4 % clipped).
FILL_LADDER: tuple[float, ...] = (0.21, 0.6, 2.0, 23.0, 43.0, 77.0)

#: Codes per unit full scale used by :mod:`hkpy.synth.scene` when quantising to ``ci8``.
CI8_SCALE = 127.0


def fill_bucket(sigma_lsb: float | None, clip_fraction: float | None) -> str:
    """The ADC-fill bucket, ADR-0015 §13.3. Fail-closed at every unknown.

    ``None`` (or non-finite) ``sigma_lsb`` is ``"under_filled"``, **never** ``"nominal"``: an
    unknown fill is not a good fill, the same rule that makes ``BiasTee::Unknown`` not ``Off``.
    A ``None`` clip fraction is "no clipping measured", which cannot promote a window — it only
    fails to demote one.
    """
    filled = sigma_lsb is not None and math.isfinite(sigma_lsb) and sigma_lsb >= UNDER_FILL_SIGMA_LSB
    if not filled:
        return "under_filled"
    if clip_fraction is not None and (
        not math.isfinite(clip_fraction) or clip_fraction > OVER_CLIP_FRACTION
    ):
        return "over_clipped"
    return "nominal"


def has_calibration_table(bucket: str) -> bool:
    """Whether a calibration table exists for ``bucket``; otherwise calibrated metrics score 0 bits."""
    return bucket == "nominal"


def measure_fill(samples: np.ndarray) -> tuple[float | None, float]:
    """``(sigma_lsb, clip_fraction)`` of an ``int8`` I/Q array, shaped ``(n, 2)`` or flat.

    σ is the pooled per-component deviation ``sqrt((var(re) + var(im)) / 2)``, each component about
    its own mean so a DC offset does not read as fill. The integer samples *are* the LSB units, so
    nothing here needs a scale constant — the reason to measure before the normalisation rather
    than after it. Clipping is counted at the rails, which is what a reader of the recording can
    observe; :meth:`hkpy.synth.scene.Scene.quantise` separately knows which samples *saturated*.

    Returns ``(None, 0.0)`` for an empty array — not measured, which classifies ``under_filled``.
    """
    arr = np.asarray(samples)
    if arr.size == 0:
        return None, 0.0
    iq = arr.reshape(-1, 2).astype(np.float64)
    sigma = float(math.sqrt(0.5 * (iq[:, 0].var() + iq[:, 1].var())))
    rails = np.asarray(arr).reshape(-1, 2)
    clipped = ((rails == 127) | (rails == -128)).any(axis=-1)
    return sigma, float(clipped.mean())


def sigma_lsb_of_noise_dbfs(noise_dbfs: float, scale: float = CI8_SCALE) -> float:
    """Per-component σ in LSB of complex white noise at ``noise_dbfs`` (total, both components)."""
    return math.sqrt(10.0 ** (noise_dbfs / 10.0) / 2.0) * scale


def adc_gain_db_for(target_sigma_lsb: float, noise_dbfs: float, scale: float = CI8_SCALE) -> float:
    """The ``adc_gain_db`` impairment that puts a ``noise_dbfs`` floor at ``target_sigma_lsb``.

    Derived, not searched: ``adc_gain_db`` is a voltage-domain gain in front of the converter and σ
    scales with it, so the rung is a closed form. The generated recording is measured afterwards
    and the *measured* σ is what gets recorded — the prediction is never trusted as the datum.
    """
    if target_sigma_lsb <= 0.0:
        raise ValueError("target_sigma_lsb must be positive")
    return 20.0 * math.log10(target_sigma_lsb / sigma_lsb_of_noise_dbfs(noise_dbfs, scale))


def ladder(
    scenario: str,
    seed: int,
    out_dir: str | Path,
    *,
    levels: Sequence[float] = FILL_LADDER,
    params: Mapping[str, Any] | None = None,
    noise_dbfs: float | None = None,
) -> dict[str, Any]:
    """Generates one ``ci8`` recording per fill level and returns the ladder manifest.

    Each rung is the same scene at the same seed with only ``adc_gain_db`` moved, so the rungs
    differ in ADC fill and in nothing else — which is what makes a comparative report across them
    (docs/22 §4.4) attributable. The manifest records, per rung, the target σ, the
    ``adc_gain_db`` used, the **measured** σ and clip fraction as written into the recording's
    provenance, and the resulting bucket.
    """
    from hkpy.synth import generate, resolve_params  # local: avoids a package import cycle
    from hkpy import sigmf

    out = Path(out_dir)
    base = dict(params or {})
    resolved = resolve_params(scenario, base)
    floor_dbfs = float(noise_dbfs if noise_dbfs is not None else resolved["noise_dbfs"])

    rungs: list[dict[str, Any]] = []
    for target in levels:
        gain_db = adc_gain_db_for(target, floor_dbfs)
        rung_dir = out / f"sigma_{target:g}"
        manifest_path = generate(
            scenario, seed, rung_dir, {**base, "adc_gain_db": gain_db}, "ci8"
        )
        manifest = json.loads(Path(manifest_path).read_text())
        name = manifest["recordings"][0]
        meta = sigmf.read_meta(rung_dir / name)
        prov = meta["global"][sigmf.PROVENANCE_KEY]
        data = np.fromfile(sigmf.data_path(rung_dir / name), dtype=np.int8)
        measured, clip_fraction = measure_fill(data)
        rungs.append(
            {
                "target_sigma_lsb": float(target),
                "adc_gain_db": gain_db,
                "measured_sigma_lsb": measured,
                "recorded_sigma_lsb": prov.get("noise_sigma_lsb"),
                "clip_fraction": clip_fraction,
                "bucket": fill_bucket(prov.get("noise_sigma_lsb"), clip_fraction),
                "has_calibration_table": has_calibration_table(
                    fill_bucket(prov.get("noise_sigma_lsb"), clip_fraction)
                ),
                "recording": str(Path(rung_dir / name).relative_to(out)),
            }
        )

    ladder_manifest = {
        "kind": "adc_fill_ladder",
        "ticket": "T-625",
        "spec": "docs/22 §2 axis A3, §4.4; buckets ADR-0015 §13.3",
        "scenario": scenario,
        "seed": seed,
        "noise_dbfs": floor_dbfs,
        "params": {k: v for k, v in base.items()},
        "under_fill_sigma_lsb": UNDER_FILL_SIGMA_LSB,
        "over_clip_fraction": OVER_CLIP_FRACTION,
        "rungs": rungs,
        "bucket_counts": {
            b: sum(1 for r in rungs if r["bucket"] == b)
            for b in ("nominal", "under_filled", "over_clipped")
        },
    }
    out.mkdir(parents=True, exist_ok=True)
    path = out / "fill-ladder.json"
    path.write_text(json.dumps(ladder_manifest, indent=2) + "\n")
    return ladder_manifest


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(
        prog="python -m hkpy.synth.fill",
        description="Generate the ADC-fill ladder (T-625, docs/22 §2 A3 / §4.4).",
    )
    ap.add_argument("scenario")
    ap.add_argument("--seed", type=int, default=625)
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument(
        "--levels",
        default=",".join(f"{v:g}" for v in FILL_LADDER),
        help="comma-separated target sigma values in LSB",
    )
    ap.add_argument("--param", action="append", default=[], metavar="K=V")
    args = ap.parse_args(argv)
    overrides: dict[str, Any] = {}
    for item in args.param:
        key, sep, value = item.partition("=")
        if not sep:
            ap.error(f"--param expects K=V, got {item!r}")
        overrides[key.strip()] = value
    levels = tuple(float(v) for v in args.levels.split(",") if v.strip())
    manifest = ladder(args.scenario, args.seed, args.out, levels=levels, params=overrides)
    json.dump(manifest, sys.stdout, indent=2)
    print()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
