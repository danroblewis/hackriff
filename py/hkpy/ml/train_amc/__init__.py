"""Per-family within-family class models for the C15 DL stage (T-204, ADR-0016 §4.6).

What this trains, and what it deliberately does not
---------------------------------------------------

One small MLP **per family**, over that family's classes only. A model here never chooses a
family: the classical cascade does that, and the ADR rejected a family-choosing model because of
the sim-to-real gap. The model sees ``hk_classify::dl::dl_input`` vectors exported by
``cargo run -p hk-classify --bin dl-train-export`` -- the *same* Rust code that computes the vector
at inference time, so there is no mirrored feature extractor to drift.

Open set without softmax
------------------------

The exported manifest carries an energy calibration, never a max-probability threshold:

* ``E = -T * logsumexp(z / T)`` (Liu et al., NeurIPS 2020),
* ``T`` is fitted on the dev holdout by minimising negative log-likelihood,
* the threshold is the 95th percentile of in-distribution holdout energy, i.e. the 95 % TPR
  operating point the ADR names.

Rust maps energy to 0..1 with a logistic about that threshold, so 0.5 *is* the operating point.

Splits
------

Only dev seeds are ever read (the exporter enforces that with ``SeedGuard``). Inside dev, the
weights are fitted on ``train`` and every calibration number comes from ``holdout``; ``ood`` rows
(out-of-taxonomy generators) are never trained or calibrated on -- they exist so the evaluation can
measure the open set honestly.

No enable evidence is written. ``ModelManifest.allows(Active)`` is therefore false, so a model
trained here can only run in shadow until ADR-0016 §4.6's evidence exists.
"""

from __future__ import annotations

import hashlib
import json
from dataclasses import dataclass, field
from pathlib import Path

import numpy as np

#: Weights-file format understood by ``hk_ml::mlp``.
MLP_FORMAT = "hk-mlp@1"

#: ``hk-ml`` manifest schema (``hk_ml::ML_SCHEMA``).
ML_SCHEMA = 1

#: Hidden widths of the per-family network.
HIDDEN = (48, 24)


@dataclass
class TrainConfig:
    """Optimiser settings. Deterministic: same data and seed give the same weights."""

    hidden: tuple[int, ...] = HIDDEN
    epochs: int = 400
    batch_size: int = 256
    learning_rate: float = 3e-3
    weight_decay: float = 1e-4
    seed: int = 20260915


@dataclass
class Dataset:
    """One family's rows, already split."""

    family: str
    labels: list[str]
    x_train: np.ndarray
    y_train: np.ndarray
    x_holdout: np.ndarray
    y_holdout: np.ndarray
    holdout_offsets: np.ndarray
    x_ood: np.ndarray = field(default_factory=lambda: np.zeros((0, 0), dtype=np.float64))


def load_jsonl(path: Path) -> list[dict]:
    """Reads the exporter's JSONL rows."""
    with path.open() as f:
        return [json.loads(line) for line in f if line.strip()]


def datasets(rows: list[dict]) -> dict[str, Dataset]:
    """Groups exported rows into one :class:`Dataset` per family with at least two classes."""
    out: dict[str, Dataset] = {}
    families = sorted({r["family"] for r in rows})
    for family in families:
        mine = [r for r in rows if r["family"] == family]
        labels = sorted({r["class"] for r in mine if r["split"] != "ood"})
        if len(labels) < 2:
            # A single-class family has nothing for a classifier to decide.
            continue
        index = {label: i for i, label in enumerate(labels)}

        def split(name: str, rows_=mine) -> list[dict]:
            return [r for r in rows_ if r["split"] == name]

        train, holdout, ood = split("train"), split("holdout"), split("ood")
        if not train or not holdout:
            continue
        out[family] = Dataset(
            family=family,
            labels=labels,
            x_train=np.array([r["x"] for r in train], dtype=np.float64),
            y_train=np.array([index[r["class"]] for r in train], dtype=np.int64),
            x_holdout=np.array([r["x"] for r in holdout], dtype=np.float64),
            y_holdout=np.array([index[r["class"]] for r in holdout], dtype=np.int64),
            holdout_offsets=np.array([r["offset_db"] for r in holdout], dtype=np.float64),
            x_ood=np.array([r["x"] for r in ood], dtype=np.float64)
            if ood
            else np.zeros((0, len(train[0]["x"])), dtype=np.float64),
        )
    return out


def _standardise(x: np.ndarray) -> tuple[np.ndarray, np.ndarray]:
    mean = x.mean(axis=0)
    scale = x.std(axis=0)
    # A constant feature has no scale to divide by; 1.0 leaves it as a (centred) constant rather
    # than exploding it. The Rust loader refuses a zero scale, so this also keeps the file valid.
    scale = np.where(scale > 1e-8, scale, 1.0)
    return mean, scale


def _init_layers(
    widths: list[int], rng: np.random.Generator
) -> list[tuple[np.ndarray, np.ndarray]]:
    layers = []
    for a, b in zip(widths[:-1], widths[1:]):
        # He initialisation for the ReLU stack.
        w = rng.normal(0.0, np.sqrt(2.0 / a), size=(b, a))
        layers.append((w, np.zeros(b)))
    return layers


def _forward(
    layers: list[tuple[np.ndarray, np.ndarray]], x: np.ndarray
) -> tuple[np.ndarray, list[np.ndarray]]:
    """Returns logits and the per-layer activations (inputs of each layer, for the backward pass)."""
    acts = [x]
    v = x
    for i, (w, b) in enumerate(layers):
        v = v @ w.T + b
        if i + 1 < len(layers):
            v = np.maximum(v, 0.0)
        acts.append(v)
    return v, acts


def _softmax(z: np.ndarray, temperature: float = 1.0) -> np.ndarray:
    s = z / temperature
    s = s - s.max(axis=-1, keepdims=True)
    e = np.exp(s)
    return e / e.sum(axis=-1, keepdims=True)


def energy(logits: np.ndarray, temperature: float) -> np.ndarray:
    """``E = -T * logsumexp(z / T)``: the open-set statistic, never a softmax maximum."""
    s = logits / temperature
    m = s.max(axis=-1)
    return -temperature * (m + np.log(np.exp(s - m[:, None]).sum(axis=-1)))


def train(data: Dataset, config: TrainConfig | None = None) -> dict:
    """Fits one family's model; returns the ``hk-mlp@1`` weights dictionary."""
    config = config or TrainConfig()
    rng = np.random.default_rng(config.seed)
    mean, scale = _standardise(data.x_train)
    x = (data.x_train - mean) / scale
    y = data.y_train
    k = len(data.labels)
    widths = [x.shape[1], *config.hidden, k]
    layers = _init_layers(widths, rng)

    # Adam, plain and explicit: this is a few thousand parameters, not a framework's job.
    m = [(np.zeros_like(w), np.zeros_like(b)) for w, b in layers]
    v = [(np.zeros_like(w), np.zeros_like(b)) for w, b in layers]
    beta1, beta2, eps = 0.9, 0.999, 1e-8
    step = 0
    n = x.shape[0]
    for _ in range(config.epochs):
        order = rng.permutation(n)
        for start in range(0, n, config.batch_size):
            idx = order[start : start + config.batch_size]
            xb, yb = x[idx], y[idx]
            logits, acts = _forward(layers, xb)
            probs = _softmax(logits)
            grad = probs.copy()
            grad[np.arange(len(yb)), yb] -= 1.0
            grad /= len(yb)

            step += 1
            for i in range(len(layers) - 1, -1, -1):
                w, b = layers[i]
                a = acts[i]
                gw = grad.T @ a + config.weight_decay * w
                gb = grad.sum(axis=0)
                if i > 0:
                    grad = (grad @ w) * (acts[i] > 0.0)
                mw, mb = m[i]
                vw, vb = v[i]
                mw = beta1 * mw + (1 - beta1) * gw
                mb = beta1 * mb + (1 - beta1) * gb
                vw = beta2 * vw + (1 - beta2) * gw**2
                vb = beta2 * vb + (1 - beta2) * gb**2
                m[i], v[i] = (mw, mb), (vw, vb)
                lr = config.learning_rate * np.sqrt(1 - beta2**step) / (1 - beta1**step)
                layers[i] = (w - lr * mw / (np.sqrt(vw) + eps), b - lr * mb / (np.sqrt(vb) + eps))

    return {
        "format": MLP_FORMAT,
        "input_dim": int(x.shape[1]),
        "mean": [float(m_) for m_ in mean],
        "scale": [float(s_) for s_ in scale],
        "layers": [
            {
                "in_dim": int(w.shape[1]),
                "out_dim": int(w.shape[0]),
                "weight": [float(t) for t in w.reshape(-1)],
                "bias": [float(t) for t in b],
                "activation": "relu" if i + 1 < len(layers) else "identity",
            }
            for i, (w, b) in enumerate(layers)
        ],
        "trained_on": (
            f"hk-classify dl-train-export dev grid: family {data.family}, "
            f"{x.shape[0]} train rows, {len(data.labels)} classes, seed {config.seed}"
        ),
    }


def apply(weights: dict, x: np.ndarray) -> np.ndarray:
    """Runs the exported file the way ``hk_ml::mlp`` does: standardise, then the layers."""
    mean = np.array(weights["mean"])
    scale = np.array(weights["scale"])
    v = (x - mean) / scale
    for layer in weights["layers"]:
        w = np.array(layer["weight"]).reshape(layer["out_dim"], layer["in_dim"])
        v = v @ w.T + np.array(layer["bias"])
        if layer["activation"] == "relu":
            v = np.maximum(v, 0.0)
    return v


def fit_temperature(logits: np.ndarray, y: np.ndarray) -> float:
    """Temperature minimising holdout NLL, over a fixed grid (no gradient, no test-set peeking)."""
    best_t, best_nll = 1.0, np.inf
    for t in np.geomspace(0.25, 8.0, 40):
        p = _softmax(logits, float(t))
        nll = -np.log(np.clip(p[np.arange(len(y)), y], 1e-12, None)).mean()
        if nll < best_nll:
            best_t, best_nll = float(t), float(nll)
    return best_t


def calibrate_open_set(logits: np.ndarray, temperature: float) -> float:
    """Energy threshold at 95 % TPR on in-distribution holdout data (ADR-0016 §4.6)."""
    return float(np.percentile(energy(logits, temperature), 95.0))


def export(
    data: Dataset,
    weights: dict,
    out_dir: Path,
    *,
    config: TrainConfig | None = None,
) -> dict:
    """Writes ``model.json``, ``manifest.json`` and ``metrics.json`` for one family."""
    config = config or TrainConfig()
    out_dir.mkdir(parents=True, exist_ok=True)
    blob = json.dumps(weights, separators=(",", ":")).encode()
    sha256 = hashlib.sha256(blob).hexdigest()
    (out_dir / "model.json").write_bytes(blob)

    holdout_logits = apply(weights, data.x_holdout)
    temperature = fit_temperature(holdout_logits, data.y_holdout)
    threshold = calibrate_open_set(holdout_logits, temperature)
    holdout_acc = float((holdout_logits.argmax(axis=1) == data.y_holdout).mean())

    manifest = {
        "schema": ML_SCHEMA,
        "model": {
            "id": f"amc-{data.family}",
            "version": "0.1.0",
            "sha8": sha256[:8],
        },
        "sha256": sha256,
        "task": "family-class",
        "consumer": "hk-classify/dl",
        "taxonomy": "hk-mod@1",
        "family": data.family,
        "labels": data.labels,
        "open_set": {
            "method": "energy",
            "temperature": temperature,
            "threshold": threshold,
            "calibrated_on": "dev@amc-grid-1 holdout",
        },
        "precision": "fp32",
        "metrics_ref": "metrics.json",
        # ADR-0016 §4.6: without this a model can never be set active. T-204 measured the enable
        # rule and does not claim it; the evidence file is written only if a family passes.
        "enable_evidence": None,
    }
    (out_dir / "manifest.json").write_text(json.dumps(manifest, indent=1))

    per_offset = {}
    for offset in sorted(set(data.holdout_offsets.tolist())):
        sel = data.holdout_offsets == offset
        per_offset[f"gate+{offset:.0f}dB"] = float(
            (holdout_logits[sel].argmax(axis=1) == data.y_holdout[sel]).mean()
        )
    metrics = {
        "family": data.family,
        "labels": data.labels,
        "n_train": int(data.x_train.shape[0]),
        "n_holdout": int(data.x_holdout.shape[0]),
        "n_ood": int(data.x_ood.shape[0]),
        "train_accuracy": float(
            (apply(weights, data.x_train).argmax(axis=1) == data.y_train).mean()
        ),
        "holdout_accuracy": holdout_acc,
        "holdout_accuracy_per_offset": per_offset,
        "temperature": temperature,
        "energy_threshold_95tpr": threshold,
        "config": {
            "hidden": list(config.hidden),
            "epochs": config.epochs,
            "seed": config.seed,
        },
    }
    (out_dir / "metrics.json").write_text(json.dumps(metrics, indent=1))
    return metrics
