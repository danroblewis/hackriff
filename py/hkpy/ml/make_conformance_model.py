"""Generate the fixed model, fixed tensors and independent reference outputs that
`hk-ml/tests/conformance.rs` compares every ML provider against (T-203, ADR-0016 §6).

Why the reference outputs are computed here, in numpy, rather than by an ONNX runtime: a
conformance suite that generated its expectations with one of the runtimes it is testing would
only prove the runtimes agree with each other. These expectations come from a hand-written
forward pass over the same weights, so a provider that agrees with them agrees with the
arithmetic the graph describes, not with another implementation of it.

The model is deliberately tiny (a few KB) and uses only operators on the ADR-0016 §6 allowlist,
chosen to cover the two things the CoreML execution provider's op coverage was flagged as
*unverified* for: **1-D convolution** and a **dynamic batch dimension**.

    input  [N, 2, 64]   iq-2x64, the canonical IQ layout at a small N
      Conv1d(2 -> 8, k=5, pad=2) -> Relu
      Conv1d(8 -> 8, k=3, pad=1) -> Relu
      GlobalAveragePool -> Flatten
      Gemm(8 -> 4)                      logits, in manifest label order

Run: `python -m hkpy.ml.make_conformance_model <out-dir>` (defaults to the crate's test data).
Regenerating with the same seed is bit-identical, so the checked-in fixture and its manifest
sha256 are stable.
"""

from __future__ import annotations

import hashlib
import json
import sys
from pathlib import Path

import numpy as np

try:
    import onnx
    from onnx import TensorProto, helper, numpy_helper
except ImportError as exc:  # pragma: no cover - tooling-only dependency
    raise SystemExit(
        "onnx is needed to regenerate the conformance model: pip install onnx"
    ) from exc

SEED = 20260915
IN_CHANNELS = 2
WIDTH = 64
HIDDEN = 8
LABELS = ["2fsk", "gfsk", "msk", "4fsk"]
BATCHES = (1, 32)

# The op allowlist this model stays inside (ADR-0016 §6).
ALLOWED_OPS = {
    "Conv",
    "BatchNormalization",
    "Relu",
    "Gelu",
    "GlobalAveragePool",
    "MaxPool",
    "AveragePool",
    "Gemm",
    "MatMul",
    "LayerNormalization",
    "Add",
    "Mul",
    "Reshape",
    "Flatten",
}


def weights(rng: np.random.Generator) -> dict[str, np.ndarray]:
    """Small, well-conditioned weights: logits of O(1), so no provider's answer is dominated
    by floating-point noise and none of them saturates."""
    return {
        "conv1_w": rng.normal(0.0, 0.5, (HIDDEN, IN_CHANNELS, 5)).astype(np.float32),
        "conv1_b": rng.normal(0.0, 0.1, HIDDEN).astype(np.float32),
        "conv2_w": rng.normal(0.0, 0.4, (HIDDEN, HIDDEN, 3)).astype(np.float32),
        "conv2_b": rng.normal(0.0, 0.1, HIDDEN).astype(np.float32),
        "fc_w": rng.normal(0.0, 0.8, (len(LABELS), HIDDEN)).astype(np.float32),
        "fc_b": rng.normal(0.0, 0.1, len(LABELS)).astype(np.float32),
    }


def conv1d(x: np.ndarray, w: np.ndarray, b: np.ndarray, pad: int) -> np.ndarray:
    """NCW 1-D convolution (ONNX `Conv` is a cross-correlation, not a flipped convolution)."""
    n, _, width = x.shape
    out_ch, in_ch, k = w.shape
    xp = np.pad(x, ((0, 0), (0, 0), (pad, pad)))
    out = np.empty((n, out_ch, width + 2 * pad - k + 1), dtype=np.float64)
    for o in range(out_ch):
        acc = np.zeros((n, out.shape[2]), dtype=np.float64)
        for c in range(in_ch):
            for t in range(k):
                acc += w[o, c, t] * xp[:, c, t : t + out.shape[2]]
        out[:, o, :] = acc + b[o]
    return out


def forward(x: np.ndarray, p: dict[str, np.ndarray]) -> np.ndarray:
    """The reference forward pass, in float64, independent of any ONNX runtime."""
    h = conv1d(x.astype(np.float64), p["conv1_w"], p["conv1_b"], pad=2)
    h = np.maximum(h, 0.0)
    h = conv1d(h, p["conv2_w"], p["conv2_b"], pad=1)
    h = np.maximum(h, 0.0)
    pooled = h.mean(axis=2)  # GlobalAveragePool -> Flatten
    return pooled @ p["fc_w"].astype(np.float64).T + p["fc_b"].astype(np.float64)


def build_graph(p: dict[str, np.ndarray]) -> onnx.ModelProto:
    """The ONNX graph, with a symbolic batch dimension ("N") so a provider that cannot do
    dynamic batching fails the suite rather than silently being tested at one size."""
    initializers = [numpy_helper.from_array(v, name=k) for k, v in p.items()]
    nodes = [
        helper.make_node(
            "Conv", ["input", "conv1_w", "conv1_b"], ["c1"],
            kernel_shape=[5], pads=[2, 2], strides=[1],
        ),
        helper.make_node("Relu", ["c1"], ["r1"]),
        helper.make_node(
            "Conv", ["r1", "conv2_w", "conv2_b"], ["c2"],
            kernel_shape=[3], pads=[1, 1], strides=[1],
        ),
        helper.make_node("Relu", ["c2"], ["r2"]),
        helper.make_node("GlobalAveragePool", ["r2"], ["pooled"]),
        helper.make_node("Flatten", ["pooled"], ["flat"], axis=1),
        helper.make_node("Gemm", ["flat", "fc_w", "fc_b"], ["logits"], transB=1),
    ]
    used = {n.op_type for n in nodes}
    assert used <= ALLOWED_OPS, f"off-allowlist operators: {sorted(used - ALLOWED_OPS)}"
    graph = helper.make_graph(
        nodes,
        "hk-conformance",
        [helper.make_tensor_value_info("input", TensorProto.FLOAT, ["N", IN_CHANNELS, WIDTH])],
        [helper.make_tensor_value_info("logits", TensorProto.FLOAT, ["N", len(LABELS)])],
        initializer=initializers,
    )
    model = helper.make_model(
        graph,
        producer_name="hkpy.ml.make_conformance_model",
        opset_imports=[helper.make_opsetid("", 13)],
    )
    model.ir_version = 9  # ORT 1.x / tract 0.22 both read IR 9.
    onnx.checker.check_model(model)
    return model


def inputs(rng: np.random.Generator) -> dict[int, np.ndarray]:
    """Fixed input tensors, shaped like normalised IQ (unit-power complex samples split into
    I and Q rows), so the numbers exercise the same dynamic range a real snippet would."""
    out = {}
    for n in BATCHES:
        x = rng.normal(0.0, 1.0, (n, IN_CHANNELS, WIDTH)).astype(np.float32)
        rms = np.sqrt((x**2).sum(axis=1, keepdims=True).mean(axis=2, keepdims=True))
        out[n] = (x / rms).astype(np.float32)
    return out


def main(argv: list[str]) -> int:
    out_dir = Path(
        argv[1]
        if len(argv) > 1
        else Path(__file__).resolve().parents[3] / "crates/hk-ml/tests/data/conformance"
    )
    out_dir.mkdir(parents=True, exist_ok=True)

    rng = np.random.default_rng(SEED)
    p = weights(rng)
    model = build_graph(p)
    blob = model.SerializeToString()
    model_path = out_dir / "model.onnx"
    model_path.write_bytes(blob)
    sha256 = hashlib.sha256(blob).hexdigest()

    xs = inputs(np.random.default_rng(SEED + 1))
    # Batch 1 is the first row of the batch-32 tensor, so batch invariance is a property the
    # fixture can express: the same input, alone and in a batch, must give the same logits.
    xs[1] = xs[32][:1].copy()
    cases = [
        {
            "batch": n,
            "input": xs[n].reshape(-1).tolist(),
            "logits": forward(xs[n], p).reshape(-1).tolist(),
        }
        for n in sorted(BATCHES)
    ]

    manifest = {
        "schema": 1,
        "model": {"id": "hk-conformance", "version": "1.0.0", "sha8": sha256[:8]},
        "sha256": sha256,
        "task": "family-class",
        "consumer": "hk-ml/conformance",
        "taxonomy": "hk-mod@1",
        "family": "fsk",
        "labels": LABELS,
        "open_set": {
            "method": "energy",
            "temperature": 1.0,
            "threshold": -2.0,
            "calibrated_on": "none@conformance-fixture",
        },
        "precision": "fp32",
        "metrics_ref": None,
        # No enable evidence, for ever: this model classifies nothing real, and ADR-0016 §4.6
        # must refuse to make it active.
        "enable_evidence": None,
    }
    (out_dir / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    (out_dir / "cases.json").write_text(
        json.dumps(
            {
                "generator": "hkpy.ml.make_conformance_model",
                "seed": SEED,
                "shape": [IN_CHANNELS, WIDTH],
                "labels": LABELS,
                "reference": "numpy float64 forward pass, independent of every ONNX runtime",
                "cases": cases,
            },
            indent=2,
        )
        + "\n"
    )
    print(f"{model_path} ({len(blob)} bytes, sha256 {sha256[:16]}…)")
    for c in cases:
        print(f"  batch {c['batch']:2d}: {len(c['input'])} inputs, {len(c['logits'])} logits")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
