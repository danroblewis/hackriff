# C28 · annotation-labeling
> Layer E — Remember · Status: taxonomy frozen 2026-09-13 (resolved in docs/06 §5) · Depends on: C09, C22, C25, C27, C39 (C30 explanations can attach to annotations — edge C30→C28, docs/06 §2.1) · Used by: C15, C18, C27, C38

## Purpose
Captures ground truth: user labels and corrections on detections, emitters and recordings, plus "confirmed by decoder" labels from CRC-valid frames. It exports them as labelled SigMF datasets. This closes the fine-tuning loop that makes classifiers work on this device's own front end (docs/04 §5.4 #1) and supports separating known from unknown (workflow step 4). It also produces the annotated fixtures the test strategy asks for.

## Interface
- **Inputs:**
  - User actions from C39: draw a box, accept/reject a suggestion, rename, tag.
  - Decoder results from C22: protocol, identity, CRC status.
  - Classifier suggestions from C15.
- **`Annotation`** (provisional):
  - id and target {recording sample range + frequency edges | detection id | emitter id}.
  - Label path: family → modulation → protocol → identity (docs/04 §5.4 #6). `unknown` is a first-class label (docs/04 §5.4 #4).
  - Source {user-typed, user-accepted, decoder}, confidence, author, created, supersedes.
  - Evidence (CRC passes, frame count) and content class.
- **SigMF mirror:** recording `annotations` carry start sample, sample count, frequency edges, label, comment and generator. Field names per the SigMF core spec (verify); see docs/03 §1.6.
- **Queries:** labels per class; review queue of unlabelled unknowns; user-vs-classifier disagreements; per-model confusion.
- **`DatasetExport`:** a SigMF collection/archive plus a manifest (classes, splits, source hardware, provenance, licence). Optionally a TorchSig-compatible layout (TorchSig v2 uses HDF5, docs/03 §4.1).
- **Sizes:** annotations are negligible (estimate <1 kB each); exports are dominated by IQ (see C25 rates).

## Methods
- **Append-only with supersession:** corrections keep an audit trail, and "labels as of T" makes training runs reproducible.
- **Decoder ground truth:** CRC-valid decodes label the snippet automatically (docs/06 C22; docs/03 §7 "Implications for This Project", item 4: trial decoding as the final arbiter). Require several valid frames, or a strong CRC, before trusting.
- **Active labelling:** queue unknowns and high-entropy classifications ("class uncertainty", docs/04 §2 feature 16), ordered by interestingness.
- **Taxonomy:** modulation-level classes, refined to protocol by fingerprints and decoders (docs/04 §5.4 #6). Seed names from sigidwiki/Artemis (docs/03 §3.7) and TorchSig (docs/03 §4.1). Version the taxonomy.
- **Dataset hygiene:**
  - Normalise snippets after C13/C14 estimation: resample to canonical samples per symbol, coarse CFO correction (docs/04 §5.4 #3).
  - Split by session/site/emitter, never by snippet.
  - Keep provenance for domain-gap analysis.
- **Unknown-protocol loop:** labels on burst clusters feed field inference (docs/04 §7.7).

## Platform constraints
- **Small screen:** labelling on the handheld needs one-tap accept, reject and "unknown". Heavy editing may happen on a remote client (C39 UI ADR).
- **Training happens elsewhere:** C38 runs fine-tuning; C28 prepares data. TorchSig recommends a ≥16 GB GPU and ≥1 TB storage for training/generation (docs/03 §4.1), versus the Orin Nano Super's 8 GB (docs/02 §3.3). Expect off-device training or small fine-tunes.
- **Exports** go to removable media or sync when online, and never block capture.
- **Labels outlive IQ:** when C25 evicts the samples, keep the annotation and mark the source as gone.

## Prior art and reuse
- **IQEngine:** annotation editing on spectrograms; plugins return SigMF annotations; recording-centric; maintained at lower velocity (docs/03 §3.4). Licence: check.
- **DeepSig OmniSIG Studio:** trains custom models from labelled recordings; commercial (docs/03 §3.9).
- **TorchSig:** class taxonomy, synthetic data, HDF5 datasets (docs/03 §4.1). Licence: check.
- **RTL-ML:** simple classifier trained on your own labelled recordings (docs/03 §5.2).
- **inspectrum, URH:** manual burst marking and bit views. URH is archived (2026) (docs/03 §6).
- **Artemis / sigidwiki:** label vocabulary and "looks like" references (docs/03 §3.7).

## Pitfalls
- **Leakage:** the same emitter or session in both train and test inflates accuracy.
- **Confirmation bias:** users accept classifier suggestions. Record typed vs accepted.
- **Decoder false positives:** short checksums pass on noise. Trust scales with CRC strength.
- **Coordinate errors** when annotations move between full-rate and decimated recordings.
- **Class imbalance:** FM broadcast and ISM sensors dominate exports.
- **Taxonomy drift:** renamed classes orphan old labels.
- **Legal:** sharing datasets that contain third-party content can be "divulging" (47 USC 605). Cellular and paging contents are off-limits, and encrypted signals are metadata only (docs/04 §1.3). Exports default to permitted content classes and must record the licence of each sample set (CLAUDE.md test strategy).

## Testing
- **Labels on a fixture:** a SigMF fixture with known bursts, scripted user labels, and a mock decoder emitting CRC-valid frames. Assert Annotation rows; SigMF boxes within ±1 sample, including after decimation; decoder labels tagged source=decoder.
- **Supersession:** correct a label twice; assert the history and the "as-of" query.
- **Export:** no emitter appears in two splits; every file passes sigmf-python validation; restricted content classes are excluded.
- **Round-trip:** importing the export into a fresh store yields identical labels.
- **Review queue:** ordering matches a scripted entropy/interestingness set.
- **Needs hardware:** on-device labelling UX; real decoder label quality.

## Example use cases
Regenerated from `use-cases.yaml` (primary, then notable secondary):
- RESEARCH-070 — Build labeled RF datasets from your own captures
- RESEARCH-071 — (annotation-labeling primary)
- RESEARCH-072 — (annotation-labeling primary)
- AWARE-022 — ML drone RF classifier
- AWARE-030 — Switching-supply / LED / inverter RFI signatures
- AWARE-040 — (annotation-labeling secondary)
- AWARE-047 — (annotation-labeling secondary)
- AWARE-048 — (annotation-labeling secondary)
- RESEARCH-067 — (annotation-labeling secondary)
- RESEARCH-076 — Browser-based IQ exploration

## Open questions
- **Taxonomy source and versioning:** TorchSig classes, sigidwiki names, or our own?
- **Boundary with C18:** are user-edited signatures annotations, or signatures?
- **Tags and bookmarks:** does C28 own free-form tags on emitters and frequency regions? docs/06 doesn't say.
- **Decoder trust:** thresholds per protocol.
- **Legal enforcement:** restricted-content gating is now owned by C24 as the enforcement point (docs/06 §5); C28 exports must honor the same content-class flag. The export-licence field owner awaits the Phase 3 legal-guardrail ADR.
- **Export format:** depends on the outcome of the C38 on-device fine-tuning spike.

## Reading list
1. docs/04 §5.4 "Known issues"
2. docs/03 §3.4 "Protocol reverse engineering and signal inspection" (IQEngine)
3. docs/03 §1.6 "Metadata: SigMF"
4. docs/03 §7 "Implications for This Project (short)" (classification strategy)
5. docs/03 §4.1 "Datasets and toolkits"
6. docs/04 §1.3 "Legal considerations (US; not legal advice)"
