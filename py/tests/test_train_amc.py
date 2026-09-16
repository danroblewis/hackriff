"""T-204: the per-family DL trainer exports a file the Rust side can actually run.

These tests use a tiny synthetic dataset rather than the real grid: what is being checked is the
*contract* between the trainer and ``hk_ml::mlp`` (shapes, standardisation, energy calibration,
manifest rules), not modulation accuracy. The accuracy question is measured in Rust, on the dev
holdout, by ``dl-eval``.
"""

from __future__ import annotations

import hashlib
import json

import numpy as np
import pytest

from hkpy.ml.train_amc import (
    MLP_FORMAT,
    Dataset,
    TrainConfig,
    apply,
    calibrate_open_set,
    datasets,
    energy,
    export,
    train,
)


def centres(dim: int, classes: int) -> np.ndarray:
    """The class means. Drawn once: train and holdout must be the *same* distribution, or a
    holdout number measures nothing (the first version of this fixture drew fresh centres per
    split, and a perfectly trained model scored 0.61 on unrelated data)."""
    return np.random.default_rng(0).normal(0.0, 3.0, size=(classes, dim))


def blobs(n: int, dim: int, classes: int, seed: int) -> tuple[np.ndarray, np.ndarray]:
    """Separable Gaussian blobs: enough for a small MLP to learn, not trivially linear."""
    rng = np.random.default_rng(seed)
    y = rng.integers(0, classes, size=n)
    x = centres(dim, classes)[y] + rng.normal(0.0, 0.6, size=(n, dim))
    return x, y


def dataset(dim: int = 12, classes: int = 3) -> Dataset:
    x_train, y_train = blobs(600, dim, classes, seed=1)
    x_hold, y_hold = blobs(300, dim, classes, seed=2)
    # Far from every centre: the open-set negatives.
    rng = np.random.default_rng(3)
    x_ood = rng.normal(12.0, 1.0, size=(100, dim))
    return Dataset(
        family="fsk",
        labels=[f"c{i}" for i in range(classes)],
        x_train=x_train,
        y_train=y_train,
        x_holdout=x_hold,
        y_holdout=y_hold,
        holdout_offsets=np.zeros(len(y_hold)),
        x_ood=x_ood,
    )


@pytest.fixture(scope="module")
def trained() -> tuple[Dataset, dict]:
    data = dataset()
    return data, train(data, TrainConfig(epochs=120, seed=7))


def test_training_learns_and_is_deterministic(trained):
    data, weights = trained
    acc = (apply(weights, data.x_holdout).argmax(axis=1) == data.y_holdout).mean()
    assert acc > 0.9, f"holdout accuracy {acc:.3f}"
    again = train(data, TrainConfig(epochs=120, seed=7))
    assert weights == again, "same data and seed must give the same weights"


def test_the_exported_file_matches_what_the_rust_loader_requires(trained):
    data, weights = trained
    assert weights["format"] == MLP_FORMAT
    assert weights["input_dim"] == data.x_train.shape[1]
    assert len(weights["mean"]) == weights["input_dim"]
    assert all(s > 0 for s in weights["scale"]), "a zero scale is refused by hk_ml::mlp"
    width = weights["input_dim"]
    for i, layer in enumerate(weights["layers"]):
        assert layer["in_dim"] == width
        assert len(layer["weight"]) == layer["in_dim"] * layer["out_dim"]
        assert len(layer["bias"]) == layer["out_dim"]
        assert np.isfinite(layer["weight"]).all()
        width = layer["out_dim"]
    assert width == len(data.labels)
    # The output layer is linear: it produces logits. Probabilities (and the open set) are the
    # consumer's business, and the open set is never a softmax.
    assert weights["layers"][-1]["activation"] == "identity"
    assert [layer["activation"] for layer in weights["layers"][:-1]] == ["relu"] * (
        len(weights["layers"]) - 1
    )


def test_the_open_set_is_energy_based_and_calibrated_at_95_percent_tpr(trained):
    data, weights = trained
    logits_id = apply(weights, data.x_holdout)
    logits_ood = apply(weights, data.x_ood)
    threshold = calibrate_open_set(logits_id, temperature=1.0)

    e_id = energy(logits_id, 1.0)
    e_ood = energy(logits_ood, 1.0)
    # The operating point is what it claims: 95 % of in-distribution holdout sits below it.
    assert abs((e_id <= threshold).mean() - 0.95) < 0.02
    # And the statistic separates: OOD energy is higher.
    assert e_ood.mean() > e_id.mean()

    # The failure mode this replaces: a max-softmax threshold is *not* a usable unknown detector
    # here -- the OOD blob is confidently labelled by the softmax.
    probs_ood = np.exp(logits_ood - logits_ood.max(axis=1, keepdims=True))
    probs_ood /= probs_ood.sum(axis=1, keepdims=True)
    assert probs_ood.max(axis=1).mean() > 0.9
    assert (e_ood > threshold).mean() > (probs_ood.max(axis=1) < 0.9).mean()


def test_the_manifest_cannot_authorise_an_active_stage(tmp_path, trained):
    data, weights = trained
    metrics = export(data, weights, tmp_path)
    manifest = json.loads((tmp_path / "manifest.json").read_text())

    assert manifest["task"] == "family-class"
    assert manifest["family"] == "fsk", "a family-class model names the family it is scoped to"
    assert manifest["labels"] == data.labels
    assert manifest["open_set"]["method"] == "energy"
    assert manifest["open_set"]["temperature"] > 0
    # ADR-0016 §4.6: no enable evidence, so hk_ml's ModelManifest::allows(Active) is false.
    assert manifest["enable_evidence"] is None

    # The manifest's hash is the file's identity; hk_ml::mlp refuses a mismatch on load.
    blob = (tmp_path / "model.json").read_bytes()
    assert manifest["sha256"] == hashlib.sha256(blob).hexdigest()
    assert manifest["sha256"].startswith(manifest["model"]["sha8"])
    assert metrics["holdout_accuracy"] > 0.9
    assert json.loads(blob) == weights


def test_families_with_one_class_are_not_trained():
    rows = [
        {"family": "css", "class": "chirp", "split": "train", "x": [0.0], "offset_db": 0.0},
        {"family": "css", "class": "chirp", "split": "holdout", "x": [0.0], "offset_db": 0.0},
        {"family": "fsk", "class": "2fsk", "split": "train", "x": [0.0], "offset_db": 0.0},
        {"family": "fsk", "class": "gfsk", "split": "train", "x": [1.0], "offset_db": 0.0},
        {"family": "fsk", "class": "2fsk", "split": "holdout", "x": [0.0], "offset_db": 0.0},
        {"family": "fsk", "class": "gfsk", "split": "holdout", "x": [1.0], "offset_db": 0.0},
    ]
    sets = datasets(rows)
    assert set(sets) == {"fsk"}, "a one-class family has nothing to decide"
    assert sets["fsk"].labels == ["2fsk", "gfsk"]
