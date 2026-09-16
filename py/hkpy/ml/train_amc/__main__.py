"""Trains one model per family from an exported dev grid.

    uv run --project py python -m hkpy.ml.train_amc <amc-dev.jsonl> <models-dir>

Writes ``<models-dir>/<family>/{model.json,manifest.json,metrics.json}``. Datasets and weights are
never committed.
"""

from __future__ import annotations

import sys
from pathlib import Path

from . import Dataset, TrainConfig, datasets, export, load_jsonl, train


def main(argv: list[str]) -> int:
    if len(argv) < 3:
        print(__doc__)
        return 2
    rows = load_jsonl(Path(argv[1]))
    out = Path(argv[2])
    sets: dict[str, Dataset] = datasets(rows)
    if not sets:
        print("no family had two classes and both splits", file=sys.stderr)
        return 1
    config = TrainConfig()
    for family, data in sets.items():
        weights = train(data, config)
        metrics = export(data, weights, out / family, config=config)
        print(
            f"{family:<9} {len(data.labels)} classes, "
            f"train {metrics['train_accuracy']:.3f}, holdout {metrics['holdout_accuracy']:.3f}, "
            f"T {metrics['temperature']:.2f}, energy threshold {metrics['energy_threshold_95tpr']:.2f}"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
