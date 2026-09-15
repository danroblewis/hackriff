# hackriff — Research

Background research for an exploration-first signals-analysis device and software stack: something that could replace a HackRF PortaPack. The goal is to find interesting signals automatically instead of making you know the frequency and pick the mode first.

_Research completed 2026-09-13. Items the agents could not confirm are marked **(verify)** or "unverified" in each document._

## Documents

| # | Document | Words | Scope |
|---|----------|------:|-------|
| 01 | [HackRF & PortaPack](01-hackrf-and-portapack.md) | ~7.3k | HackRF One and Pro hardware, PortaPack models, Mayhem firmware, why the chip limits it, user pain points, how active the projects really are |
| 02 | [SDR Landscape](02-sdr-landscape.md) | ~8.7k | SDR architectures and specs, a ~50-row hardware table, Pi 5 / Jetson / RK3588 / RFSoC data-rate math, FPGA roles, front-ends, candidate device tiers |
| 03 | [SDR Software](03-sdr-software.md) | ~9.0k | Frameworks (GNU Radio 3.10/4.0, Rust, liquid-dsp), receiver apps, survey and analysis tools, trunking, automatic modulation classification in practice, UX critique, gap table |
| 04 | [Radio Engineering & Signals Analysis](04-radio-engineering-and-signals-analysis.md) | ~13.8k | Spectrum use, what makes a signal interesting, detection (CFAR, spectral kurtosis), parameter estimation, AMC, auto-demod and squelch, protocol ID, trunking, DF, calibration, professional monitoring workflow. Includes the legal notes on decrypting your own traffic. |
| 05 | [Use-Cases & Explorations](05-use-cases-and-explorations.md) | ~12.3k | A 391-item, one-line-each catalogue across five themes: space weather and natural radio, propagation and RF sensing, spectrum awareness and anomalies (the radio "attack map"), the long tail of receivable signals, and unknown signals / security / lab / ML. **These are the feature goals and future test suite.** Each item has a permanent ID. |
| 06 | [Capability map](06-capability-map.md) | — | **Planning (Phase 1).** 39 engineering capabilities in seven layers, mapping rules for `use-cases.yaml`, and (after mapping) coverage analysis: the shared core, native-fit counts, and costly capabilities to defer. |
| 07 | [Data model](07-data-model.md) | — | **Planning (Phase 2).** Domain objects everything hangs off: ScanPlan, Survey, Sweep/SpectrumFrame, SpectrumTile, Provenance, Detection, Track, Emitter (inventory), Recording (SigMF), Demodulation, Decode, Bitstream, ExternalEvent, Anomaly, Explanation; storage tiers, the region-over-time query, worked examples. |
| 08 | [Architecture](08-architecture.md) + [ADRs](adr/) | — | **Planning (Phase 3).** System shape (headless Rust core + web thin clients + isolated plugins) and 11 ADRs: pipeline runtime & live reconfiguration, UI web-vs-native, process/plugin model, stream-output contract, scheduler, storage, compute placement, offline-first context, hardware sketch, language/licence ledger, decoder workbench contracts (M1: blocks, recipes, field maps, inspector stream). |
| 09 | [Risks & spikes](09-risks-and-spikes.md) | — | **Planning (Phase 4).** Ranked design-sinking risks and spikes S1–S7 (live reconfiguration, throughput+power, detection in urban overload, blind estimation on 8-bit, web waterfall, battery/thermal, two-HackRF coherence), each with hypothesis, setup, pass/fail, effort, and the ADR it unblocks. |
| 10 | [Test strategy](10-test-strategy.md) | — | **Planning (Phase 5).** Six test tiers (unit→field), the `test_tier` rubric filled into the YAML, SigMF fixtures and sources with licence checks, e2e through a mock SDR device (same tests run on the HackRF as HIL), blind hidden-truth fixtures with top-k explanation asserts and anti-patterns, use-case-ID→test mapping, and CI-without-hardware vs field-only. |
| 11 | [Roadmap & first slice](11-roadmap.md) | — | **Planning (Phase 6).** The first vertical slice M0 (7 use case IDs across science/attack-map/unknown/known-decoder) with acceptance tests; M0b "Live device + exploration UI" (T-042..T-053) before decoder breadth; milestones M1–M7 ordered by shared-core coverage. |
| 12 | [Implementation plan](12-implementation-plan.md) + [tasks.yaml](tasks.yaml) | — | **Planning (Phase 7).** Spike order, repo scaffold, dev env + CI, fixture capture plan, the 25-task M0 breakdown (state in `tasks.yaml`), and the hardware shopping list with timing. |
| — | [Tutorial 1: RDS from blocks](tutorials/01-rds.md) | — | **Engineering (T-094, M1).** The decoder workbench's reference tutorial: the RDS recipe stage by stage with each stage's status readout, run/hot-edit/save through the API, inspector frames, and the blind mock-SDR acceptance against the `hk_demod::rds` oracle. |
| — | [Tutorial 2: POCSAG from blocks](tutorials/02-pocsag.md) | — | **Engineering (T-095/T-109, M1.)** The POCSAG recipe (`fsk_demod`→`clock_recovery`→`sync_search`→`bch`→`assemble`→`follow_hops`→`fields`) decodes a 4-channel synthetic pager net blind through the mock SDR: channels found in the inventory, every page matches truth, simulcast deduplicated, multimon-ng oracle 4/4. It also records the two T-109 fixes: the tracker no longer links co-keyed channels into a false hop set, and repeating channel tracks reach the inventory live. |
| — | [SigMF extension](sigmf-extension.md) | — | **Engineering (T-001).** The `hackriff` SigMF namespace: `hackriff:provenance` on global/captures and `hackriff:truth` on annotations, used by recordings and test fixtures. |
| — | [Stream-output contract](stream-contract.md) | — | **Engineering (T-016, T-014).** Versioned wire contract for external consumers and the plugin data plane: length-prefixed framing, JSON stream header, NDJSON/binary records, drop markers, egress `content_class` gating matrix, drop-not-block backpressure, WebSocket mapping, plugin manifest and IPC. |
| — | [API reference](api.md) | — | **Engineering (T-050, T-051, T-052, T-060, T-061, T-079).** Every `/api` and `/ws` route the control API serves (`hk-api`): auth, request/response JSON, error codes, audit. The web UI is a thin client over this document (ADR-0002); `docs/stream-contract.md` covers stream framing. |
| — | [use-cases.yaml](use-cases.yaml) | — | Machine-readable copy of 05 and the source of truth for use-case IDs (`SPACE-`, `PROP-`, `AWARE-`, `SIGNAL-`, `RESEARCH-`). Architecture planning fills in `capabilities`, `hardware_fit` and `test_tier`. |

## Findings across the four documents

1. **The HackRF ecosystem isn't dead, but the handheld can't grow.** 2026 is the busiest HackRF firmware year since at least 2022, HackRF Pro shipped in January 2026 (it adds an FPGA and TCXO), and Mayhem still ships regular releases. But the PortaPack runs all its DSP on an LPC4320 microcontroller with about 200 kB of RAM. Scanning, classifying and trunking are out of reach, and the maintainers have closed every DMR/P25 request as impossible. → [01](01-hackrf-and-portapack.md)
2. **No open-source tool covers the whole exploration chain.** The chain is survey → detect → classify → estimate parameters → decode → inventory → record. The pieces are spread across SDRangel, SigDigger/Suscan, URH (archived 2026), rtl_433, Trunk Recorder, OpenWebRX+, SatDump, IQEngine and Maia SDR. Every mainstream GUI is built around tuning first. Only commercial suites (DeepSig OmniSIG, CRFS, R&S, Aaronia) put ML detection into a live receiver. → [03](03-sdr-software.md)
3. **Sweep to find *where*, dwell to find *what*.** A HackRF sweeps 0–6 GHz in under a second, yet catches a 5 ms burst less than 1% of the time per sweep. Short bursts need a real-time window. The scheduler that trades these off is a core design problem. → [02](02-sdr-landscape.md), [04](04-radio-engineering-and-signals-analysis.md)
4. **The big wins are mostly classical DSP, not deep learning.** Priorities:
   - noise-floor estimation
   - CFAR plus spectral kurtosis for detection
   - calibration and spur rejection
   - blind estimation of symbol rate, frequency offset and bandwidth
   - licence and band-plan data as priors

   Deep-learning classifiers reach about 95% on synthetic data but drop to the 59–87% range over the air, and softmax confidence doesn't flag unknown signals. The better design uses ML as a stage after normalization, with an "unknown" output. → [04](04-radio-engineering-and-signals-analysis.md)
5. **Filtering and linearity decide what automation can trust.** In urban RF, 8-bit samples without a preselector mostly show your own intermodulation. A filter bank that switches in step with the sweep is the best-value hardware upgrade. → [02](02-sdr-landscape.md)
6. **The hardware architecture has commercial precedent.** Epiq's Matchstiq and Deepwave's AIR-T both pair an FPGA and RF chip with a Jetson Orin. The report sketches three tiers:

   | Tier | Hardware | Cost |
   |---|---|---|
   | A | HackRF Pro + Pi 5 + filter bank | ~$700–1.1k |
   | B | Orin + PCIe SDR + GPSDO | ~$1.5–4k |
   | C | RFSoC + Orin | ~$5–20k |

   The Pi 5 has an unresolved USB 3 throughput problem with some SDRs. → [02](02-sdr-landscape.md)

## Next

Architecture planning is under way (brief: `prompts/fable-architecture-planning.md`). Planning documents are numbered from 06 onward; decisions go in `adr/`.
