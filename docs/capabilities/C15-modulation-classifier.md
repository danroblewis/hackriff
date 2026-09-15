# C15 · modulation-classifier
> Layer C — Characterize · Status: taxonomy frozen 2026-09-13 (resolved in docs/06 §5) · Depends on: C13, C14, C16, C17, C38 · Used by: C19, C20, C22, C27, C12, C04, C25

## Purpose
Assigns each emission a modulation family and class, with calibrated probabilities that always include `unknown`. It routes emissions to demodulators and decoders (workflow steps 5–7) and separates known from unknown (step 4). C17 (known-signal-priors) *supplies* the priors; C15 *fuses* them with its likelihoods — C17 does not classify (docs/06 §5). "Unknown" is first-class: it triggers recording and review (docs/04 §5.4 #4). Classify at modulation level; C18 fingerprints and C22 decoders resolve protocols (docs/04 §5.4 #6).

## Interface
- **Inputs** (provisional names):
  - `NormalizedSnippet`: CFO-corrected and resampled to a canonical sps after C13/C14 (docs/04 §5.4 #3). Keep the raw snippet too.
  - `ParameterSet`, `SymbolParameters`, and the C16 result.
  - `PriorSet` from C17.
  - Provenance: clip count, spur-mask hit, image candidate.
- **Output: `Classification`.**
  - `family ∈ {analog, OOK/ASK, FSK, PSK/QAM, OFDM, CSS, DSSS, pulsed, noise-like}`, then `class`.
  - Probabilities including `unknown`, stored both as likelihood-only and as posterior (docs/04 §11.2).
  - Open-set score, entropy (docs/04 §2 #16), deciding stage.
  - Model version (C38) and reason codes.
- **Unknown handling:**
  - Below ~0 dB SNR, emit "digital, unknown order" (docs/04 §5.4 #2).
  - Never assign probability 1.
  - A CRC-valid decode overrides the classification and becomes a label (docs/04 §5.5).
- **Config:** taxonomy version, per-family thresholds, SNR gate, per-family DL enable, open-set threshold.
- **Decided in [ADR-0016](../adr/0016-classification-contracts.md) §1–§4 (PROVISIONAL):**
  - **Taxonomy.** `hk-mod@1` is coarse → family → class. `unknown` is the global open-set outcome, not a leaf. The family set above keeps its names as `ook-ask`, `psk-qam` and `noise-like`.
  - **Classification.** Posterior plus likelihood-only distributions (both include `unknown`), with an optional within-family `class`, `open_set_score`, `entropy_norm`, deciding `stage`, provenance (features@version, rules/model@version, SNR vs gate) and flags. It is stored additively on `emitter_classification`.
  - **Current family.** Picked by arbitration rank: user > decoder > lock-verified > classifier > track shape.
  - **Fusion.** Priors never scale `unknown`, and λ₀ ≥ 0.1. The evidence-dominance rule applies: likelihood ratio ≥ 10 keeps the likelihood top. Flags are `prior-tiebreak` and `prior-mismatch`.
  - **Classical cascade.** Class-conditional densities give `p(x|c)`, and χ² gives the open-set score. The verifier runs post-sync only and can only re-rank.
  - **DL.** Within-family class only, with an energy-score open set. It is enabled per family only on the a-priori dev margins (+5 points at every bin ≥ gate).
  - **Gates.** `thresholds@1`, from S5: FSK/OOK 20 dB, PSK 15 dB; the others are unverified.

## Methods
Cascade per docs/04 §5.5: features → per-family DL → open-set score → prior fusion → decoders arbitrate.

- **Feature tree** (docs/04 §5.2):
  - **Azzouz–Nandi** γ_max, σ_ap, σ_dp, P, σ_aa, σ_af, μ₄₂. High-90s% above ~10–15 dB in the original simulations (synthetic). Share the analog branch with C19 (docs/04 §6.1).
  - **Cumulants** C̃₄₀/C̃₄₂: BPSK −2/−2, QPSK |1|/−1, 8PSK 0/−1, 16QAM −0.68, 64QAM −0.62. Insensitive to AWGN.
  - **Cyclic patterns** (C14): robust to CFO and sample-rate shifts.
- **Verifier:** likelihood tests (ALRT/GLRT) only within a small post-sync candidate set (docs/04 §5.1).
- **DL stage:** small CNN or XCiT-Nano per family, INT8/FP16 TensorRT via C38. Train with TorchSig impairments, then fine-tune on our own captures and decoder labels (docs/04 §5.4 #1).
- **Open set:** not softmax. OpenMax/Weibull, centre loss plus distance, or energy-based OOD (docs/04 §5.4 #4).
- **Fusion:** `P(c|x,f,ℓ) ∝ p(x|c)·P(c|f,ℓ)`, λ₀ > 0 (docs/04 §1.1); priors never veto evidence (docs/04 §12 #8).
- **Reported accuracy with conditions** (docs/04 §5.3; docs/03 §4.2):

| Source | Conditions | Result |
|---|---|---|
| O'Shea 2016 | Synthetic, 11 classes, −20…+20 dB | ~87.4% |
| O'Shea 2018 | Synthetic, 24 classes, high SNR | ResNet ~95% vs ~61% boosted trees |
| O'Shea 2018 | Over the air | 95.6% trained OTA; ~87% synthetic→OTA (−7 points); clock/LO offsets cut it to 59–80% |
| Sig53 | Impaired synthetic, 53 classes | XCiT-Tiny12 71.16% |
| *Sensors* 2026 | OTA, 4 OFDM-subcarrier classes, cooperative link | 93.4%; 16-QAM ~81% |
| RTL-ML | Service-level, one location | 96.9% vs 87.5%; the sources disagree |

## Platform constraints
- **Compute** (docs/04 §5.5):
  - Features: μs–ms per snippet on ARM.
  - Small CNN: sub-ms on the Jetson GPU; EfficientNet-B0/XCiT-Nano a few ms at FP16/INT8.
  - TensorRT 10–15× over PyTorch, measured on AGX Orin, not Nano.
  - Orin Nano Super: hundreds to thousands of small-CNN inferences/s (estimate, docs/02 §3.2).
  - Batch per event.
- **Power modes.** 7/15/25 W (docs/02 §3.3). In low power, run the feature tree only and report the stage.
- **Front-end artefacts.** Images (−25…−40 dB) carry the parent's modulation, and IMD ghosts look real (docs/04 §10.3). Consume C05 flags; train with quantisation, clipping, IMD and spurs.
- **Train off-device.** TorchSig recommends a ≥16 GB GPU (docs/03 §4.1).

## Prior art and reuse
- **TorchSig:** MIT, active, v2.2.0 (2026-08). Impairment model; LoRa/BLE/802.11a families; mixes in real data.
- **torchsig-models:** licence: check.
- **RadioML:** CC BY-NC-SA 4.0 with errata. Don't ship RML-trained models (docs/04 §5.3).
- **Panoradio HF dataset:** licence: check.
- **RTL-ML:** service-level Random Forest. Licence: check.
- **gr-inspector:** AMC block. Stale; reference only.
- **IQEngine:** plugin → annotation contract.
- **Commercial targets:** OmniSIG (known and unknown signals; shown on Jetson Thor), R&S AMMOS.

## Pitfalls
- **Synthetic-to-real gap.** Raw-IQ CNNs collapse under clock/LO offsets; normalise first.
- **SNR averaging** hides failure; report accuracy against SNR.
- **Mislabelled datasets.** RML "AM-SSB" examples contain only noise; 2018.01A has label-mapping mismatches.
- **Softmax is overconfident** on unknowns.
- **Several signals per snippet.** Dense 2.4 GHz scenes need spectrogram detection.
- **Known confusions:** 8PSK/QPSK, WBFM/AM-DSB, high-order QAM.
- **Strong priors hide** pirates and faulty transmitters.
- **Label bias.** Decoder labels only teach classes that have decoders (inference, not from the docs).

## Testing
- **Synthetic:**
  - TorchSig classes with HackRF-like impairments, SNR −10…+30 dB.
  - Hold out whole classes (CSS, OFDM) as unknowns.
  - Metrics: accuracy vs SNR, confusion matrices, open-set AUROC, calibration (ECE), latency per power mode.
  - No docs targets; don't adopt RML numbers as targets.
- **OTA fixtures labelled by decoders:**
  - FM broadcast with RDS, aviation AM, NOAA WX, AIS, APRS, P25/DMR.
  - POCSAG: labels only.
  - ADS-B, rtl_433 OOK, Meteor LRPT, LTE, LoRa, BLE, NEXRAD.
  - Split by site and day.
- **Unknown set:** excluded classes; sigidwiki samples (licence: check).
- **Live:** false-class rate in urban overload, with and without a filter.

## Example use cases
Regenerated from `use-cases.yaml`:
- AWARE-036 — Unknown burst reverse-engineering triage
- AWARE-041 — PSD-based technology classifier
- SIGNAL-069 — STANAG 4285 modems
- RESEARCH-004 — (classifier primary)
- RESEARCH-069 — RadioML AMC baseline (and its critiques)
- RESEARCH-073 — Open-set / unknown-signal detection
- AWARE-005 — "Personal privacy device" hunter
- AWARE-054 — Amateur-band intruder logging
- SIGNAL-047 — Key-fob capture and analysis
- RESEARCH-007 — Catalog unknowns against Sig ID Wiki

## Open questions
- **Missing edges (resolved, docs/06 §2.1/§5).** C38→C15 (DL stage) and C16→C15 (OFDM/DSSS features) are now in §2.1.
- **Fusion owner (resolved, docs/06 §5).** C17 supplies priors; C15 fuses them. C17 does not classify.
- **Unknown semantics.** Family-level vs global unknown; taxonomy versioning (doc 07).
- **Shared code.** The analog branch is duplicated with C19. Spectrogram object detection: C09, C15 or C38?
- **Fine-tuning governance.** Dataset licences, label provenance, rollback.

## Reading list
1. docs/04 §5.4 "Known issues"
2. docs/04 §5.2 "Classical features"
3. docs/04 §5.5 "Compute cost on embedded platforms"
4. docs/04 §5.3 "Deep learning approaches and reported numbers"
5. docs/03 §4.2 "How well AMC works on real OTA data"
6. docs/03 §4.1 "Datasets and toolkits"
