"""Write ``hackriff:truth`` annotations into a ``.sigmf-meta`` from a truth JSON file.

    uv run --project py python py/fixtures/annotate.py FIXTURE.sigmf-meta TRUTH.json [--append]

Truth JSON: ``{"annotations": [item, ...]}``. Each item has ``truth`` (the ``hackriff:truth``
object, conventions in py/README.md) and a box: ``sample_start``/``sample_count`` or, failing
that, ``truth.t_start_s``/``truth.duration_s``; optional ``freq_lower_edge``/``freq_upper_edge``,
``label``, ``comment``. Missing ``t_start_s``/``duration_s`` are filled from the sample box, so the
two always agree. Without ``--append`` existing ``hackriff:truth`` annotations are replaced (other
annotations are kept).
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path
from typing import Any

import fxlib
from fxlib import sigmf

ROLES = frozenset({"scenario", "emission", "artefact", "floor", "event"})


class TruthError(ValueError):
    pass


def apply(meta: dict[str, Any], items: list[dict[str, Any]], n_samples: int,
          append: bool = False) -> dict[str, Any]:
    fs = float(meta["global"]["core:sample_rate"])
    if not append:
        meta["annotations"] = [a for a in meta.get("annotations", []) if sigmf.TRUTH_KEY not in a]
    for i, item in enumerate(items):
        truth = dict(item["truth"])
        if truth.get("role") not in ROLES:
            raise TruthError(f"item {i}: role {truth.get('role')!r} not in {sorted(ROLES)}")
        if not truth.get("kind"):
            raise TruthError(f"item {i}: missing kind")
        if "sample_start" in item:
            start = int(item["sample_start"])
            count = int(item["sample_count"])
        else:
            start = int(round(float(truth["t_start_s"]) * fs))
            count = int(round(float(truth["duration_s"]) * fs))
        start = max(0, start)
        count = min(count, n_samples - start)
        if count <= 0:
            raise TruthError(f"item {i}: box outside the recording")
        truth["t_start_s"] = start / fs
        truth["duration_s"] = count / fs
        lo, hi = item.get("freq_lower_edge"), item.get("freq_upper_edge")
        if lo is not None and hi is not None and lo > hi:
            raise TruthError(f"item {i}: freq_lower_edge > freq_upper_edge")
        sigmf.add_annotation(meta, start, sample_count=count, freq_lower_edge=lo,
                             freq_upper_edge=hi, label=item.get("label"),
                             comment=item.get("comment"), truth=truth)
    return meta


def annotate(meta_path: Path, truth: dict[str, Any] | Path, append: bool = False) -> dict[str, Any]:
    meta_path = Path(meta_path)
    if not isinstance(truth, dict):
        truth = fxlib.load_json(Path(truth))
    meta = sigmf.read_meta(meta_path)
    n = fxlib.n_samples(sigmf.data_path(meta_path), meta["global"]["core:datatype"])
    apply(meta, truth["annotations"], n, append=append)
    sigmf.write_meta(meta, meta_path)
    return meta


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("meta")
    ap.add_argument("truth")
    ap.add_argument("--append", action="store_true")
    args = ap.parse_args(argv)
    meta = annotate(Path(args.meta), Path(args.truth), append=args.append)
    print(f"{args.meta}: {sum(sigmf.TRUTH_KEY in a for a in meta['annotations'])} truth annotations")
    return 0


if __name__ == "__main__":
    sys.exit(main())
