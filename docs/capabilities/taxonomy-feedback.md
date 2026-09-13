# Feedback on the draft taxonomy (docs/06) from writing the capability cards

> **Resolved (2026-09-13).** Every point below is now decided in [docs/06 §5](../06-capability-map.md#5-resolved-taxonomy-questions-from-capability-card-review) (with edges folded into §2.1), and the capability cards have been reconciled to those decisions. This file is kept as the record of what was raised; §5 is the source of truth for the resolutions.

*2026-09-13. Written by the card-writing agents while condensing docs 01–05 into the 39 cards, and collected here for the architect's Phase 1 checkpoint. Each point is also recorded in the Open questions section of the relevant card.*

## 1. Missing or wrong edges in §2.1 (dependency sketch)

- **Acquire/Sense:**
  - C03 → C11 (the channelizer runs on the dwell window)
  - C05 → C08 and C05 → C09 (spur mask and calibration feed thresholds)
  - C26 → C12 (baselines need history)
  - C06 → C12 (the device moves, so baselines must be per location)
- **Scheduler inputs:** C02, C10, C22, C23 and C29 → C04. C04's definition also omits pass/launch events from C29 and TX arbitration for C37.
- **Loop:** C05's spur and intermodulation tests need C09 detections, while C09 depends on C05's spur mask. Decide the bootstrap order (e.g. factory spur map first, runtime refinement later).
- **Layer D is not a chain.** The linear C19 → C24 line is misleading:
  - C22 doesn't need C21.
  - C23 needs C11 and C12 (control-channel hunting uses occupancy/continuous duty).
  - C24 takes input from all of Layer D.
- **Layer E has no arrows.** C27 ↔ C30 and C27 ↔ C17 are loops: the inventory feeds correlation and priors, and correlation and priors write back to the inventory.
- **Other missing edges:** C29 → C04, C30 → C28, C06 → C37 (time for TX scheduling), and most C06 consumers.
- **ML consumers:** C38 is also used by C09 (learned detectors) and C18 (clustering/embeddings).

## 2. Responsibilities with no owner

- **Zoom/decimated narrowband view.** C07 promises 25 Hz bins, which at 20 Msps would need an ~800k-point FFT. Something must produce a decimated zoom stream: C07 or C11?
- **Own-key decryption** of the user's own traffic, allowed by CLAUDE.md, has no capability.
- **Restricted-content gating:** a policy that keeps cellular or common-carrier paging content out of recordings, streams and exports. Candidate owners: C22, C24, C25.
- **Conventional (non-trunked) digital voice** (P25/DMR/NXDN without a control channel): C20 + C22, or its own capability?
- **OFDM demodulation.** C16 estimates OFDM parameters but nothing demodulates.
- **Storage quota and retention policy** across C25/C26/C27 on a small disk.
- **Anomaly contract:** the shared shape of an "anomaly" that C08, C12 and C27 emit and C30 consumes.
- **Attack-map view, dashboards and maps.** C39 bundles four products (live view, history browser, signal table, inspector), and nothing owns map/geo views or dashboards.
- **Satellite-pass computation** (TLE propagation): C29, C34 or C30?
- **GNSS reflectometry** use cases (C36 covers observables but not reflectometry).
- **GNSS receiver module** for C06 was never researched (part choice, PPS, antenna sharing).

## 3. Overlaps to resolve

- Clustering emissions: C10 or C18.
- Interestingness score: C12 or C04.
- Noise-floor-vs-time science data (SPACE-050, AWARE-031): C08 or C33.
- RDS: C19 or C22.
- FEC decoding: C21 (inference) vs C22 (known decoders). Probably inference vs execution; say so.
- C24 mixes data streams with the control/API surface; consider splitting.
- Amplitude bearings (Yagi, body null): C31 or C32.
- FMLIST and SatNOGS DB appear in both C17 (reference data) and C29 (feeds).
- VLF sudden ionospheric disturbance monitoring (SPACE-001): C33 or C34.

## 4. Factual corrections

- **C06 "1PPS discipline" isn't possible on HackRF One**: it has no PPS input. Options are a 10 MHz GPSDO into CLKIN, or software ppm/time correction from GNSS.
- **C03 "sample-accurate timestamps"** needs a stated method (host arrival time plus sample counting? GNSS time tagging?), with the error budget noted.
- **C36 "front-end AGC"**: the HackRF has no GNSS-style AGC to report. Dual-frequency TEC can't fit in one 20 MHz window on a single HackRF. Docs 03 has no GNSS-SDR/galmon section, so their licences are unchecked.
- **C35:** inputs should include the illuminator location (C17) and ADS-B truth (C22/C29). Whether two HackRFs sharing a 10 MHz clock stay phase-coherent enough is unverified. It's a spike candidate, and the answer affects `needs-other-sdr`.

## 5. Scope and hardware_fit

- **C32:** multi-site TDoA with the user's *own* receivers breaks the one-device scope. Only public remote receivers (e.g. KiwiSDR TDoA) fit.
- **C37:** use cases that involve jamming, e.g. RESEARCH-027 "RollJam-style keyfob capture-and-replay" (jam + record + replay), should be `out-of-scope`, even on your own vehicle. Capture-and-analysis variants without jamming can stay.
- **Trunking coverage gap:** docs/04 calls trunking the most-requested scanner capability, but docs/05 has only about 2 trunking use cases, so C23 lists 3 examples. Consider adding trunking use cases with new IDs.

## 6. Characterize layer (C13–C18)

- **Missing edges:** C38 → C15, C16 → C15, C17 → C19 / C04 / C27 / C30 (priors inform demod choice, scheduling, inventory status and explanations).
- **Duplicate responsibilities:**
  - Bayesian fusion of features and priors appears in both C15 and C17. Decide whether C17 only *supplies* priors and C15 *fuses* them.
  - Clustering appears in C10, C18 and C21 (plus §3 above).
- **No owner:** LoRa/CSS chirp parameter estimation (slope, SF, bandwidth) and radar pulse parameters (PRI, pulse width, chirp).
- **Mapping observations** (against the 391-item mapping now in `use-cases.yaml`):
  - C14 is never the *primary* capability for any use case, though it gates every unknown-digital-signal case.
  - C16 is primary for cellular items that are fenced to metadata only (e.g. RESEARCH-022).
  - C13 is stretched onto bench-measurement and side-channel items.

## 7. Card examples vs the mapping

The card-writing agents ran mostly before, and partly during, the mapping. Comparing each card's example use cases with `capabilities` in `use-cases.yaml`:
- **Full or near-full agreement:** C13–C19, C21, C22, C25–C27, C29–C31, C33–C38.
- **Low agreement, expected:** C01, C03, C07, C39. These are substrate capabilities that the mapping rules (docs/06 §3) deliberately leave off.
- **Low agreement, worth a look:** C04 (2 of 8), C28 (4 of 8), C11 (3 of 6), C20 (5 of 9), C24 (6 of 9), C32 (4 of 6). Either the card's examples or the mapping should change. Once the taxonomy is final, regenerating the example lists from the YAML is the easy fix.
