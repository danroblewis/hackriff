# 11 — First vertical slice and roadmap

*Architecture planning, Phase 6. Drafted 2026-09-13; revised 2026-09-13 (user feedback: M0b inserted). Status: **PROVISIONAL.** The slice is chosen to exercise the whole chain end to end while staying mostly CI-provable offline. M0b follows it and puts the chain on the live radio behind a real exploration UI. Milestones after that follow the shared-core build order from [docs/06 §4.2](06-capability-map.md).*

## 1. The first vertical slice (M0) and its live follow-through (M0b)

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

Each asserts on the data-model objects the use case must produce ([docs/07](07-data-model.md)); IDs trace back to the use case. From M0b on, every one of these runs **through the mock SDR device, blind against a hidden truth list**, with top-k explanation asserts ([docs/10 §1.1, §3.1](10-test-strategy.md), T-047). No test looks a frequency up and tunes there.

- **SIGNAL-001:** replay a recorded 1090 MHz SigMF fixture → ≥N CRC-valid Decode messages, ≥1 aircraft Emitter with a hex identity, `last_seen` updated, and the messages emitted on the stream-output socket with correct framing. (T3)
- **AWARE-036:** synthesise an FSK sensor burst train (with impairments) → all bursts detected, one Track, one Emitter `known_status: unknown`, symbol rate within ±1%, deviation within tolerance, framing recovered, and a seeded CRC validates to flip status toward identified. (T4)
- **SIGNAL-062:** replay an FM-with-RDS fixture → station detected at its true frequency, auto-mode selects WFM (no manual mode), 19 kHz pilot detected, RDS PI/PS decoded and written as an Emitter label, and "FM broadcast" among the top-k explanations; the +150 kHz shifted variant is still detected and flagged off-raster. (T3 + T4 variant)
- **AWARE-006:** synthetic L1 noise-floor rise + a frozen gpsjam ExternalEvent at the same time/region → one Anomaly (`noise-floor-rise`) and a top-ranked Explanation (`time-coincidence`/`geometry`); assert **no** Explanation when the cache has no matching event. (T4)
- **SPACE-050:** synthetic capture with a known injected floor across the native range → recovered calibrated noise floor within ±1 dB, folded into SpectrumTiles, queryable as floor-vs-time. (T4)
- **AWARE-053:** a detection at a truth-list frequency (the test never reads the database) → priors recommend the expected allocation/service among the top-k, and set `known` vs `unexpected-here` correctly for an off-allocation emitter. (T4)
- **AWARE-042:** replay a multi-hour scenario → per-channel occupancy, burst-length distribution, and hour-of-window profile match expected; the region-over-time query returns them. (T3/T4)

**Definition of done for M0:** all seven acceptance tests pass in CI (offline), and the seven use cases' `test_tier`-appropriate assertions hold. The on-device half of the original goal needs a live radio and a real UI, which M0's 40 tasks don't cover, so it moves to M0b. That half is: a user can, on the device, run a survey, see detections and an inventory populate, watch the waterfall in a browser, get one attack-map explanation, and stream ADS-B to an external program.

### 1.2 M0b — Live device + exploration UI

**Goal:** turn the proven spine into something a user explores on the live HackRF. Tests reach the system only through a generic SDR device interface, and the UI gives the standard SDR control set, oriented to exploration rather than tune-and-listen. The user asked for M0b on 2026-09-13; it comes **right after M0 and before decoder breadth (M1)**.

| Contents | Task(s) |
|---|---|
| Generic SDR device interface: named gain stages, optional bias-tee/sweep, timestamps, overruns; no HackRF specifics in the core, SoapySDR-ready | T-048 |
| Mock SDR device replaying SigMF behind that interface (retune in/out of coverage, rate resampling, gain scaling with 8-bit clip, injectable overruns) | T-049 |
| E2E through the device interface + blind ground-truth harness (hidden truth lists, top-k explanation asserts, perturbed variants such as the off-raster FM station); existing acceptance tests audited | T-047 |
| Recommended explanations: ranked top-k with an off-raster flag, which the harness asserts against | T-039 (M0, feeds T-047) |
| Live HackRF serving: `hk serve`/`hackriffd` run the HackRF source by default through the full pipeline to inventory; no demo seeds | T-042 |
| Authenticated control API with legal/TX gating: no TX endpoints, content gating kept on retune, audit log (M0's API was GET-only) | T-050 |
| SDR control panel: center entry/step/shift, drag-pan, scroll-zoom, span/rate, named gains, bias-tee, FFT size, averaging, waterfall speed, colour scale auto/manual, peak hold, markers/bookmarks, pause/resume, record. Follows [SDR++](https://github.com/AlexandreRouma/SDRPlusPlus), [SDRangel](https://github.com/f4exb/sdrangel) and [SigDigger](https://github.com/BatchDrake/SigDigger) conventions but exploration-first (control-set survey is part of the task; unverified until then) | T-051 |
| Hover readout (through the tunnel) and click-to-inspect | T-044 |
| Multi-region selections as first-class, persisted objects (docs/07 addition), sent to demod/record/inspect | T-052 |
| Listen: click a signal → auto analog demod → audio to the browser over [Web Audio](https://www.w3.org/TR/webaudio/); restricted classes never stream audio | T-043 |
| Frequency-axis correctness (tone at a known frequency renders there, within one bin) | T-045 |
| HIL acceptance run: the same suite against the real HackRF, receive-only | T-053 |

**Use cases exercised:** the M0 seven again, now through the device interface. SIGNAL-062 (FM, live and blind) and AWARE-042 (region over time, live selections) carry most of the UI work; SPACE-050 covers axis and floor correctness on the live source.

**Definition of done for M0b:**
- The M0 acceptance suite passes **in CI through the mock device**, blind against hidden truth lists with top-k explanation asserts, including the perturbed variants.
- No test feeds files directly, uses lookup-and-tune, or relies on demo seeds ([docs/10 §3.2](10-test-strategy.md)).
- **The same suite runs against the real HackRF as T5 HIL** (T-053), with results logged as a dated report (never gating CI).
- A user on the live HackRF can:
  - drive the control panel through the authenticated API;
  - pan and zoom on a correct frequency axis;
  - make and act on several region selections;
  - click a signal to listen;
  - see inventory entries with recommended explanations populate from real air, starting from an empty inventory.

## 2. Milestones after the slice

Ordered by shared-core coverage ([docs/06 §4.2](06-capability-map.md)): widen the highest-reach, cheapest, mostly-native capabilities first; defer expensive/accessory-gated ones. M0b is the one exception to that ordering. It widens nothing, but without it the user can't use the device or trust the tests, so it precedes decoder breadth.

| M | Theme | Adds | Why here | Gate |
|---|---|---|---|---|
| **M0** | Vertical slice | the spine above | prove the chain | S1/S3/S4/S5 |
| **M0b** | Live device + exploration UI | generic device interface, mock SDR + blind e2e, live HackRF serving, authenticated control API, SDR control panel, selections, Listen, top-k explanations, axis fix, HIL run (§1.2) | the user can't explore without it, and tests through a device interface are HIL-ready before more features pile onto file-fed tests | M0; HackRF for T-042/T-053 |
| **M1** | Decoder breadth | AIS, ACARS/VDL2, POCSAG/FLEX (metadata), rtl_433 device long tail, radiosondes, weather sats (SatDump), redsea/nrsc5 | biggest native coverage lever; the plugin path (C22) already exists, so this is mostly manifests + fixtures (each with a truth list, run through the mock device); `offline-recorded` so CI-provable | M0b |
| **M2** | Attention + memory maturity | bandit revisit scheduler (C04 full), occupancy baselines + novelty alarms (C12), richer history queries/reports (C26) | turns "peruse/automate/review history" into the real product; all native, low compute | S4 |
| **M3** | Classification | cascaded classical classifier + open-set (C15), fingerprint clustering (C18), ml-runtime (C38) on GPU, on-device fine-tuning from labelled captures | after normalization exists (C13/C14 from M0); ML as a stage, classical first (CLAUDE.md) | S5 |
| **M4** | Trunking | control-channel discovery + P25/DMR/SmartNet following + encryption flags (C23) | most-requested scanner capability; channelizer (C11) exists from M0; **pending user accept of SIGNAL-080..086** | vocoder-IP decision |
| **M5** | Accessory-gated expansions | GNSS observables cluster (C36 + active antenna); HF/VLF science front ends; radiometry science (C33 + dish); Ku (LNB) | high reach but each gated on an accessory (docs/06 §4.3); sequenced by which accessory the user adds | hardware |
| **M6** | Localization | RSSI walk-mapping (C31) first; coherent DF / passive radar (C32/C35) only with a second/coherent SDR (the generic device interface from M0b is the seam for it) | RSSI is native; the rest are `needs-other-sdr` | S7 |
| **M7** | Device hardening + TX | on-device screen/enclosure UI, low-power tuning, TX experiments (C37, opt-in, legal-gated behind M0b's authenticated control API) | polish and the opt-in transmit path last | S6 |

## 3. What this ordering optimises

- **Provable early:** M0, M0b and M1 are almost entirely `offline-synth`/`offline-recorded`, so the whole chain and most decoders are in CI before any field or enclosure work. From M0b those tests are blind and drive the mock device, so each also runs against the real HackRF unchanged.
- **Reuse, not rebuild:** M1 rides M0's plugin path and M0b's blind harness; M4 rides M0's channelizer; M3 rides M0's parameter estimation; M6 and other SDRs ride M0b's device interface. Each milestone widens an existing seam.
- **Value order matches the vision:** peruse/automate/review-history (M0, M0b live exploration, M2) come before the decoder catalogue breadth's long tail and well before TX — "general, not a decoder catalogue" (CLAUDE.md).
- **Cost/accessory last:** GPU-heavy ML (M3), accessory clusters (M5), coherent hardware (M6) and TX (M7) come after the native core is solid.

## 4. Open items feeding the plan

- SIGNAL-080..086 trunking acceptance (blocks M4 scope) — user decision, see [planning-log.md](planning-log.md).
- Spike results S1/S3/S4/S5 confirm or adjust M0's build assumptions.
- M0b adds a Selection object to [docs/07](07-data-model.md) (T-052) and needs the HackRF for T-042 and the T-053 HIL run (one HackRF user at a time, receive-only).
- The executable task breakdown for M0 is [docs/12](12-implementation-plan.md); M0 and M0b task state lives in [tasks.yaml](tasks.yaml).

## Sources

- [SDR++](https://github.com/AlexandreRouma/SDRPlusPlus), [SDRangel](https://github.com/f4exb/sdrangel), [SigDigger](https://github.com/BatchDrake/SigDigger): reference control-panel conventions for T-051. The specific control sets are unverified until T-051's survey note.
- [W3C Web Audio API](https://www.w3.org/TR/webaudio/): browser audio output for Listen (T-043).
- [SoapySDR](https://github.com/pothosware/SoapySDR): target for later non-HackRF devices behind the generic interface (T-048).
- Internal: [docs/06 §4.2](06-capability-map.md), [docs/10](10-test-strategy.md), [ADR-0005](adr/0005-survey-dwell-scheduler.md), [tasks.yaml](tasks.yaml) T-039, T-042..T-045, T-047..T-053.
