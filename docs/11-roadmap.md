# 11 — First vertical slice and roadmap

*Architecture planning, Phase 6. Drafted 2026-09-13. Status: **PROVISIONAL.** The slice is chosen to exercise the whole chain end to end while staying mostly CI-provable offline; milestones after it follow the shared-core build order from [docs/06 §4.2](06-capability-map.md).*

## 1. The first vertical slice (M0)

**Goal:** prove the full spine — survey → detect → estimate → demod → bits → external consumer → inventory/history → explanation — on the base HackRF One + Jetson, with as much as possible provable in CI via IQ replay. Every stage is thin but real, so later milestones widen rather than rebuild.

**Chosen use-case IDs** (7), covering the required science / attack-map / unknown-signal / known-decoder mix:

| ID | Role in the slice | Chain stages exercised | Capabilities | test_tier |
|---|---|---|---|---|
| **SIGNAL-001 ADS-B** | **Known decoder** — proves the plugin path and inventory/stream-output | detect → decoder plugin (readsb) → inventory → stream-output | C22, C27, C24 | offline-recorded |
| **AWARE-036 unknown ISM burst** | **Unknown signal** — proves blind exploration | detect → track → estimate → blind demod → bits → framing → inventory | C09, C10, C13, C14, C20, C21, C18, C27 | offline-synth |
| **SIGNAL-062 RDS / FM auto-demod** | Proves "never pick FM/AM" analog path + auto-labels | detect → auto-mode analog demod → RDS decode → inventory label | C19, C13, C27 | offline-recorded |
| **AWARE-006 GNSS jamming correlation** | **Attack map** — proves anomaly → context → explanation | noise-floor → anomaly → correlate with cached feed → explanation | C08, C12, C29, C30 | offline-synth |
| **SPACE-050 noise-floor survey** | **Science** — proves survey → history → radiometry | sweep → noise-floor → history → calibrated floor-vs-time | C02, C08, C26, C33 | offline-synth (native range); field later |
| **AWARE-053 allocation lookup** | Proves priors and the "unexpected here" badge | detect → priors lookup → inventory status | C17, C27 | offline-synth |
| **AWARE-042 occupancy over time** | Proves the region-over-time query (the core product question) | history → occupancy stats → region/time view | C12, C26 | offline-synth |

Together these touch the substrate (C01/C03/C05/C07), the scheduler (C04, first simple version), and every layer through Explain. Six of seven are CI-provable offline; SPACE-050's science claim is confirmed in the field later, but its pipeline is tested with synthetic IQ now.

**Slice dependencies on spikes:** the runtime the slice is built on comes from **S1** (ADR-0001); AWARE-036 and the detection stages depend on **S4** (detection in overload) and **S5** (blind estimation) landing acceptably; the UI view for AWARE-042/SPACE-050 depends on **S3** (web waterfall). S1/S3/S4/S5 all run on the user's Mac + HackRF now ([docs/09 §3](09-risks-and-spikes.md)).

### 1.1 Slice acceptance tests

Each asserts on the data-model objects the use case must produce ([docs/07](07-data-model.md)); IDs trace back to the use case.

- **SIGNAL-001:** replay a recorded 1090 MHz SigMF fixture → ≥N CRC-valid Decode messages, ≥1 aircraft Emitter with a hex identity, `last_seen` updated, and the messages emitted on the stream-output socket with correct framing. (T3)
- **AWARE-036:** synthesise an FSK sensor burst train (with impairments) → all bursts detected, one Track, one Emitter `known_status: unknown`, symbol rate within ±1%, deviation within tolerance, framing recovered, and a seeded CRC validates to flip status toward identified. (T4)
- **SIGNAL-062:** replay an FM-with-RDS fixture → auto-mode selects WFM (no manual mode), 19 kHz pilot detected, RDS PI/PS decoded and written as an Emitter label. (T3)
- **AWARE-006:** synthetic L1 noise-floor rise + a frozen gpsjam ExternalEvent at the same time/region → one Anomaly (`noise-floor-rise`) and a top-ranked Explanation (`time-coincidence`/`geometry`); assert **no** Explanation when the cache has no matching event. (T4)
- **SPACE-050:** synthetic capture with a known injected floor across the native range → recovered calibrated noise floor within ±1 dB, folded into SpectrumTiles, queryable as floor-vs-time. (T4)
- **AWARE-053:** a detection at a known frequency → priors return the expected allocation/service and set `known` vs `unexpected-here` correctly for an off-allocation emitter. (T4)
- **AWARE-042:** replay a multi-hour scenario → per-channel occupancy, burst-length distribution, and hour-of-window profile match expected; the region-over-time query returns them. (T3/T4)

**Definition of done for M0:** all seven acceptance tests pass in CI (offline), the seven use cases' `test_tier`-appropriate assertions hold, and a user can, on the device, run a survey, see detections and an inventory populate, watch the waterfall in a browser, get one attack-map explanation, and stream ADS-B to an external program.

## 2. Milestones after the slice

Ordered by shared-core coverage ([docs/06 §4.2](06-capability-map.md)): widen the highest-reach, cheapest, mostly-native capabilities first; defer expensive/accessory-gated ones.

| M | Theme | Adds | Why here | Gate |
|---|---|---|---|---|
| **M0** | Vertical slice | the spine above | prove the chain | S1/S3/S4/S5 |
| **M1** | Decoder breadth | AIS, ACARS/VDL2, POCSAG/FLEX (metadata), rtl_433 device long tail, radiosondes, weather sats (SatDump), redsea/nrsc5 | biggest native coverage lever; the plugin path (C22) already exists, so this is mostly manifests + fixtures; `offline-recorded` so CI-provable | — |
| **M2** | Attention + memory maturity | bandit revisit scheduler (C04 full), occupancy baselines + novelty alarms (C12), richer history queries/reports (C26) | turns "peruse/automate/review history" into the real product; all native, low compute | S4 |
| **M3** | Classification | cascaded classical classifier + open-set (C15), fingerprint clustering (C18), ml-runtime (C38) on GPU, on-device fine-tuning from labelled captures | after normalization exists (C13/C14 from M0); ML as a stage, classical first (CLAUDE.md) | S5 |
| **M4** | Trunking | control-channel discovery + P25/DMR/SmartNet following + encryption flags (C23) | most-requested scanner capability; channelizer (C11) exists from M0; **pending user accept of SIGNAL-080..086** | vocoder-IP decision |
| **M5** | Accessory-gated expansions | GNSS observables cluster (C36 + active antenna); HF/VLF science front ends; radiometry science (C33 + dish); Ku (LNB) | high reach but each gated on an accessory (docs/06 §4.3); sequenced by which accessory the user adds | hardware |
| **M6** | Localization | RSSI walk-mapping (C31) first; coherent DF / passive radar (C32/C35) only with a second/coherent SDR | RSSI is native; the rest are `needs-other-sdr` | S7 |
| **M7** | Device hardening + TX | on-device screen/enclosure UI, low-power tuning, TX experiments (C37, opt-in, legal-gated) | polish and the opt-in transmit path last | S6 |

## 3. What this ordering optimises

- **Provable early:** M0 + M1 are almost entirely `offline-synth`/`offline-recorded`, so the whole chain and most decoders are in CI before any field or enclosure work.
- **Reuse, not rebuild:** M1 rides M0's plugin path; M4 rides M0's channelizer; M3 rides M0's parameter estimation. Each milestone widens an existing seam.
- **Value order matches the vision:** peruse/automate/review-history (M0–M2) come before the decoder catalogue breadth's long tail and well before TX — "general, not a decoder catalogue" (CLAUDE.md).
- **Cost/accessory last:** GPU-heavy ML (M3), accessory clusters (M5), coherent hardware (M6) and TX (M7) come after the native core is solid.

## 4. Open items feeding the plan

- SIGNAL-080..086 trunking acceptance (blocks M4 scope) — user decision, see [planning-log.md](planning-log.md).
- Spike results S1/S3/S4/S5 confirm or adjust M0's build assumptions.
- The executable task breakdown for M0 is [docs/12](12-implementation-plan.md).
