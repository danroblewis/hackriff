# Capability cards

One short card per capability in [docs/06 §2](../06-capability-map.md). Each card condenses what an implementation agent needs from docs 01–05 (~50k words) into one or two pages: purpose, interface, methods, platform constraints, prior art, pitfalls, testing, example use cases, open questions, and a **reading list** of exact doc sections.

**Status: taxonomy approved and frozen (39 capabilities); cards reconciled to docs/06 §5 (2026-09-13).** The cards follow the final taxonomy in docs/06. The card-writing feedback in [taxonomy-feedback.md](taxonomy-feedback.md) — missing edges, unowned responsibilities, overlaps, factual corrections — has been **resolved in [docs/06 §5](../06-capability-map.md#5-resolved-taxonomy-questions-from-capability-card-review)**, and the cards' ownership notes, dependency edges, example use cases (regenerated from `use-cases.yaml`) and Open questions now reflect it. Items still marked **provisional** in §5 await `docs/07-data-model.md` or a Phase 3 ADR; cards name those domain objects provisionally until doc 07 exists, and each card's Open questions point at the deciding §5 item or ADR. Where a card ever disagrees with the mapping, the YAML still wins.

## How agents should use them

1. `CLAUDE.md`, loaded automatically.
2. The task entry: task state file from Phase 7, or the coordinator's brief.
3. **The cards for the capabilities the task touches.** Usually one to three.
4. The ADRs and data-model sections the task names.
5. The card's reading list, and only the pointers you actually need. Don't read docs 01–05 end to end.

To list use cases for a capability once mapping exists, query `use-cases.yaml` instead of reading it whole.

## Cards

| ID | Capability | Layer |
|---|---|---|
| C01 | [source-abstraction](C01-source-abstraction.md) | A — Acquire |
| C02 | [sweep-survey](C02-sweep-survey.md) | A — Acquire |
| C03 | [dwell-capture](C03-dwell-capture.md) | A — Acquire |
| C04 | [attention-scheduler](C04-attention-scheduler.md) | A — Acquire |
| C05 | [calibration](C05-calibration.md) | A — Acquire |
| C06 | [position-time](C06-position-time.md) | A — Acquire |
| C07 | [spectral-estimation](C07-spectral-estimation.md) | B — Sense |
| C08 | [noise-floor](C08-noise-floor.md) | B — Sense |
| C09 | [cfar-detection](C09-cfar-detection.md) | B — Sense |
| C10 | [burst-tracking](C10-burst-tracking.md) | B — Sense |
| C11 | [channelizer](C11-channelizer.md) | B — Sense |
| C12 | [occupancy-baseline](C12-occupancy-baseline.md) | B — Sense |
| C13 | [param-estimation](C13-param-estimation.md) | C — Characterize |
| C14 | [blind-symbol-estimation](C14-blind-symbol-estimation.md) | C — Characterize |
| C15 | [modulation-classifier](C15-modulation-classifier.md) | C — Characterize |
| C16 | [ofdm-dsss-analysis](C16-ofdm-dsss-analysis.md) | C — Characterize |
| C17 | [known-signal-priors](C17-known-signal-priors.md) | C — Characterize |
| C18 | [fingerprint-signatures](C18-fingerprint-signatures.md) | C — Characterize |
| C19 | [analog-demod](C19-analog-demod.md) | D — Demodulate & decode |
| C20 | [digital-demod](C20-digital-demod.md) | D — Demodulate & decode |
| C21 | [bit-framing-inference](C21-bit-framing-inference.md) | D — Demodulate & decode |
| C22 | [decoder-plugins](C22-decoder-plugins.md) | D — Demodulate & decode |
| C23 | [trunking-follow](C23-trunking-follow.md) | D — Demodulate & decode |
| C24 | [stream-output](C24-stream-output.md) | D — Demodulate & decode |
| C25 | [sigmf-recording](C25-sigmf-recording.md) | E — Remember |
| C26 | [spectrum-history](C26-spectrum-history.md) | E — Remember |
| C27 | [signal-inventory](C27-signal-inventory.md) | E — Remember |
| C28 | [annotation-labeling](C28-annotation-labeling.md) | E — Remember |
| C29 | [context-feeds](C29-context-feeds.md) | F — Explain |
| C30 | [event-correlation](C30-event-correlation.md) | F — Explain |
| C31 | [rssi-localization](C31-rssi-localization.md) | G — Specialised |
| C32 | [coherent-df-tdoa](C32-coherent-df-tdoa.md) | G — Specialised |
| C33 | [radiometry](C33-radiometry.md) | G — Specialised |
| C34 | [doppler-tracking](C34-doppler-tracking.md) | G — Specialised |
| C35 | [passive-radar](C35-passive-radar.md) | G — Specialised |
| C36 | [gnss-observables](C36-gnss-observables.md) | G — Specialised |
| C37 | [tx-experiments](C37-tx-experiments.md) | G — Specialised |
| C38 | [ml-runtime](C38-ml-runtime.md) | G — Specialised |
| C39 | [live-view-inspector](C39-live-view-inspector.md) | G — Specialised |
| C40 | [signal-relationships](C40-signal-relationships.md) | Image/harmonic/intermod attribution and content-correlated multipath; ranked relationships, never automatic deletion |

## Card template

Every card uses these headings in this order:

```
# Cnn · slug
> Layer · Status · Depends on · Used by
## Purpose
## Interface
## Methods
## Platform constraints
## Prior art and reuse
## Pitfalls
## Testing
## Example use cases
## Open questions
## Reading list
```
