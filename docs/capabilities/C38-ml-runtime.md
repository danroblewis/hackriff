# C38 · ml-runtime
> Layer G — Specialised · Status: taxonomy frozen 2026-09-13 (resolved in docs/06 §5) · Depends on: C07, C09, C13, C14, C25, C28 · Used by: C15, C12, C30, C18, C09 (optional dense-band detector)

## Purpose
GPU inference infrastructure on the Jetson:
- TensorRT/ONNX model hosting;
- batched per-event inference;
- spectrogram object detection for dense bands;
- unsupervised waterfall anomaly models;
- the fine-tuning loop from labelled local captures;
- model versions in provenance.

It is the "ML after normalisation, with explicit unknown" stage, not a replacement for classical detection. It serves workflow steps 4–5 and the anomaly side of the attack map.

## Interface
- **Inputs (provisional):**
  - `InferenceRequest{model_id, tensor, event_id, deadline}`, where the tensor is one of:
    - normalised IQ snippet: CFO-corrected, resampled to canonical samples/symbol, 1024 or 4096 samples (`docs/04 §5.4`);
    - spectrogram tile: e.g. 512×512 (`docs/03 §4.4`);
    - PSD/waterfall window for anomaly scoring.
  - Registry entry: ONNX source, TensorRT engine, precision, input spec, classes, open-set calibration, training-data manifest.
- **Outputs:** `Prediction{class_probs, unknown_score, embedding?, model_id@version, precision, latency_ms}` attached to Detection/Emission provenance.
- **Control:** load/unload, batch size and max batching delay, power-mode awareness, per-consumer enable, shadow mode (run without acting).
- **Rates** (docs; estimates, not benchmarked):
  - Small CNN: sub-ms on Jetson GPU.
  - EfficientNet-B0/XCiT-Nano: a few ms at FP16/INT8.
  - YOLO-class 512×512: tens of fps, enough for 1–10 Hz scene updates (`docs/04 §5.5`).
  - Throughput: hundreds to thousands of small-CNN inferences/s on Orin Nano Super (`docs/02 §3.2`).

## Methods
- **Runtime:**
  - TensorRT FP16/INT8, ~10–15× faster than PyTorch on AGX Orin (`docs/04 §5.5`).
  - ONNX as interchange.
  - Alternatives: Holoscan zero-copy pipelines (`docs/03 §1.7`); FutureSDR Burn hooks if Rust wins (`docs/03 §1.5`).
- **Gating:** infer only on CFAR-surviving detections, batched. Cost scales with detections/s, not bandwidth (`docs/02 §3.2`). Defaults (estimate): batching delay ≤20 ms, batch ≤32.
- **Cascade:** C13/C14 → normalise → C15 feature tree → per-family DL → open-set score → C17 priors → decoder arbitration (`docs/04 §5.5`).
- **Open set:** OpenMax/Weibull, centre-distance, or energy OOD. Never raw softmax (`docs/04 §5.4`).
- **Starter models:**
  - TorchSig-trained XCiT-Tiny/EfficientNet-B0 (XCiT-Tiny12 71.16% on impaired Sig53, `docs/04 §5.3`).
  - YOLO/DETR spectrogram detector for 2.4/5.8 GHz (`docs/04 §12`, #20).
  - Waterfall autoencoder (AWARE-040).
  - CPU baseline: RTL-ML-style Random Forest (`docs/03 §4.2`).
- **Fine-tuning:** CRC-valid decoder labels and user labels (C28), plus TorchSig impairment augmentation (`docs/03 §4.1`).

## Platform constraints
- **Orin Nano Super 8 GB:** 1024 CUDA cores, 67 TOPS, **unified** memory, 7/15/25 W/MAXN modes (`docs/02 §3.3`). The 8 GB is shared with OS, cuFFT/PFB (C07/C11), the C03 ring buffer (30 s = 1.2 GB) and UI.
- **Training:** TorchSig recommends a ≥16 GB GPU (`docs/03 §4.1`). Full training on-device is infeasible.
- **Power:** compute is 7–25 W of a ~15–38 W tier-B budget (`docs/02 §7.2`). Idle models must cost ~nothing.
- **Thermals:** throttling in a sealed handheld is an open risk (`docs/02 §7.3`).
- **JetPack lock-in** (`docs/02 §7.1`): engines are built per TensorRT/JetPack version (general TensorRT behaviour, not in docs). Keep ONNX as the source of truth.

## Prior art and reuse
- **TorchSig v2.2.0:** data, impairments, 61+ classes. MIT; active (`docs/03 §4.1`).
- **torchsig-models:** XCiT checkpoints. Licence: check; tiny, active.
- **RadioML:** CC BY-NC-SA 4.0, known errata. Don't ship RML-trained models (`docs/04 §5.3`).
- **TensorRT, Holoscan SDK:** active. Licence: check.
- **IQEngine plugin → SigMF annotation contract:** shape for offline runs (`docs/03 §3.4`).
- **DeepSig OmniSIG Studio:** reference UX for train-from-recordings plus unknown output (`docs/03 §3.9`).
- **ORACLE/WiSig:** fingerprinting datasets. Licence: check.

## Pitfalls
- **Sim-to-real:** ~7-point drop synthetic → OTA; clock/LO offsets drop ResNet to 59–80% (`docs/04 §5.3`). HackRF 8-bit IMD and spurs are absent from training data.
- **Overconfident softmax on unknowns:** "unknown" must trigger recording.
- **Un-normalised inputs:** arbitrary samples/symbol or CFO kill generalisation.
- **Multiple signals in a narrowband snippet.**
- **Below ~0 dB SNR:** fall back to "digital, unknown order".
- **GPU contention:** a model load spike starves real-time cuFFT/PFB and causes sample drops.
- **Stale INT8 calibration** after retraining; engine and version drift breaks provenance.
- **Label leakage** from random frame splits.

## Testing
- **Held-out real captures:** SigMF split **by session, site and date**, never by frame. Report accuracy versus SNR, confusion matrix and macro-F1.
- **Open-set:** hold out whole classes; report AUROC and false-known rate.
- **Ground truth:** CRC-valid decoder outputs (ADS-B, AIS, POCSAG, rtl_433) as a self-growing real test set.
- **Parity:** TensorRT FP16/INT8 versus ONNX/PyTorch on fixed tensors (top-1 agreement ≥99%, estimate).
- **Latency:** p50/p99 per model × batch × power mode with C07/C11 running. Assert zero C03 sample drops.
- **Anomaly:** inject TorchSig emitters into recorded waterfalls; measure detection delay and false alarms per hour.
- **Regression:** golden fixtures pinned to `model_id@version`.
- **Live hardware:** throttling, power, on-device fine-tune timing.

## Example use cases
Regenerated from `use-cases.yaml`:
- AWARE-040 — Unsupervised spectrum anomaly detector
- AWARE-041 — PSD-based technology classifier
- AWARE-022 — ML drone RF classifier
- AWARE-047 — RF fingerprinting of same-model transmitters
- RESEARCH-073 — Open-set / unknown-signal detection
- RESEARCH-067 — (labelled-dataset / fine-tuning use case)
- RESEARCH-068 — TorchSig + Sig53
- RESEARCH-069 — RadioML AMC baseline (and its critiques)
- RESEARCH-062 — RF fingerprinting / physical-layer auth (ORACLE)

## Open questions
- **Where training runs:** off-device workstation versus head-only fine-tune on the Jetson (spike: memory and time).
- **Runtime choice:** TensorRT direct, Holoscan or ONNX Runtime-TRT; tied to the DSP framework ADR.
- **Provenance fields:** registry and model-version fields belong in docs/07.
- **docs/06 "Used by":** add C18 (fingerprinting) and C09 (spectrogram detection, `docs/04 §12` #20)?
- **GPU scheduling:** shared scheduler with C07/C11, or process priorities?
- **Weights licensing:** policy for bundled pretrained weights and datasets.

## Reading list
1. `docs/04 §5.4 "Known issues"` — sim-to-real, normalisation, open-set.
2. `docs/04 §5.5 "Compute cost on embedded platforms"` — latency, TensorRT, cascade.
3. `docs/03 §4.2 "How well AMC works on real OTA data"`.
4. `docs/03 §4.1 "Datasets and toolkits"` — TorchSig, RadioML licences.
5. `docs/02 §3.3 "Platform comparison"` — Orin Nano Super.
6. `docs/03 §4.4 "Compute requirements (engineering estimates)"`.
