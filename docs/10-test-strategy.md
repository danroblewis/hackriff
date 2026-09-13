# 10 — Test strategy

*Architecture planning, Phase 5. Drafted 2026-09-13; revised 2026-09-13 (user feedback: e2e through the SDR device interface, blind ground truth). Status: **PROVISIONAL.** The use cases in [docs/05](05-use-cases-and-explorations.md) / [`use-cases.yaml`](use-cases.yaml) are the acceptance targets; this document says how an ID becomes tests and defines the `test_tier` rubric now filled into the YAML.*

The guiding rule from CLAUDE.md and [docs/04 §11.2](04-radio-engineering-and-signals-analysis.md): **build end-to-end tests that replay IQ through the full pipeline and assert on detections, estimated parameters, decoded bits, and inventory entries**, with unit tests under the DSP blocks. Two user rules (2026-09-13) sharpen it: **the IQ enters through the SDR device interface, never as a file handed to the pipeline** (§1.1), and **the system under test never sees the ground truth** (§3.1). Because measurements are immutable and interpretations are versioned ([docs/07](07-data-model.md)), the same fixture re-run after an algorithm change is a regression test for free.

## 1. Test tiers

| Tier | What it covers | Runs in CI? | Example |
|---|---|---|---|
| **T1 DSP unit** | One block/kernel against known input/output | Yes | FFT/PSD correctness; OS-CFAR false-alarm rate on synthetic noise; M2M4 SNR estimate |
| **T2 component** | One capability end to end in isolation | Yes | noise-floor tracker on a drifting-floor fixture; channelizer channel isolation; SQLite inventory upsert/query; device-interface conformance of the mock and HackRF sources |
| **T3 end-to-end IQ replay** | A SigMF recording served by the **mock SDR device** (§1.1) and driven through the whole system via the device/control interface, detecting blind and asserting on Detection / Demodulation / Decode / Emitter / Explanation rows against a hidden truth list (§3.1) | Yes | mock device loaded with an ADS-B capture → system surveys and finds 1090 MHz itself → assert CRC-valid message count, one aircraft Emitter, "ADS-B" in the top-k explanations |
| **T4 synthetic scenario** | Generated IQ with known ground truth (emitters, noise, interference, overload), served by the same mock device, exercising detection→classify→estimate→inventory | Yes | TorchSig/own generator: 3 emitters + FM blocker at −10 dBFS → assert all found, blocker flagged, false-alarm under bound |
| **T5 hardware-in-the-loop (HIL)** | **The same T3/T4 acceptance suite** with the device set to the real HackRF against live air (receive-only), plus live capture correctness and timing/throughput | No (nightly on a bench rig / manual) | FM broadcast band blind survey through the real HackRF (T-053); sustained-rate drop test (spike S2); TX→RX loopback only once C37 is un-gated |
| **T6 field** | Real over-the-air conditions that cannot be captured-and-replayed meaningfully | No (opportunistic, logged) | a real Sporadic-E opening; walking a DF fix; a solar flare SID event |

T1–T4 are the **CI backbone** and need no hardware. T3/T4 are where use-case acceptance lives. T5/T6 confirm what only real hardware or real propagation can. T1/T2 may feed in-memory buffers or files straight into a block; **from T3 up, IQ only enters through the device interface.**

### 1.1 The device interface is the test seam

E2E and acceptance tests drive the system exactly as a user with real hardware would: through the generic SDR device/control interface (T-048), not by handing a recording to the pipeline. The interface is generic, with no HackRF specifics in the core, so a SoapySDR-backed device can be added later without touching tests ([SoapySDR](https://github.com/pothosware/SoapySDR)). It covers open, capabilities, tune, sample rate, baseband filter, **named gain stages** (LNA/VGA/amp on HackRF), optional bias-tee, optional sweep mode, start/stop RX, sample timestamps and overrun/drop reporting. A trait-level conformance test that every device must pass keeps the mock honest against the real source.

The **mock SDR device** (T-049) implements that interface and replays SigMF IQ (recorded or synthetic) behind it, honouring control changes realistically:

- **Retune inside the recording's coverage** serves that band: the mock shifts, filters and decimates the recorded IQ.
- **Retune outside coverage** serves calibrated noise and raises an out-of-coverage flag in Provenance, so a test can't pass by tuning somewhere the recording doesn't cover.
- **Sample-rate changes** resample.
- **Gain changes** scale the samples and clip at 8 bits, setting overload flags just as the HackRF would.
- **Bias-tee and sweep mode** are modelled, sample timestamps are emitted, pacing is real-time or accelerated, and overruns can be injected.
- **Limits (unverified until HIL):** gain on a recording is digital scaling only. The mock can't model the front end's noise figure or the intermodulation a real LNA produces at high gain, so those claims stay T5/T6.

Because a test only names a device, the same test runs against the real HackRF by switching the device selection (e.g. `HK_DEVICE=hackrf`, T-053). The HIL run is receive-only, one HackRF user at a time, and its results are logged without gating CI.

## 2. `test_tier` rubric (filled into use-cases.yaml)

Each use case gets one **primary** `test_tier` — the highest-fidelity tier at which its core claim can be asserted **cheaply and repeatably**. Most acceptance is T3/T4 (offline IQ replay through the mock device); the value says whether a use case is CI-testable offline or needs hardware/field/data.

| Value | Meaning | Assigned when |
|---|---|---|
| `offline-synth` | Validated with **generated** IQ + hidden truth (T4, plus T1/T2 underneath) | The signal can be synthesised faithfully enough (most detection, parameter-estimation, classification, modem, and framing use cases) |
| `offline-recorded` | Validated by **replaying a recorded/public SigMF** fixture through the mock device (T3) | A real signal we can capture once (or fetch from a public archive) and replay — most decoder use cases (ADS-B, AIS, rtl_433 devices, pagers, sats) |
| `hil` | Needs the **HackRF in the loop** (T5) | TX/own-link use cases, and live-behaviour/throughput/timing claims not capturable as a static file |
| `field` | Needs **real OTA conditions** (T6) | Propagation, space-weather, DF/localization, passive radar, moving-emitter, and "explain a real event" use cases |
| `data-only` | Validated against **cached feed/archive data**, no IQ | `data-only` fit_flag use cases (spot-archive mining, dataset/toolkit items, external-feed correlation with frozen caches) |

Rules when several could apply, most-CI-friendly first: prefer `offline-synth` if we can generate the signal; else `offline-recorded` if we can capture/fetch it; else `hil` (own TX/live) ; else `field`; `data-only` only when no local IQ is involved. Attack-map/event-correlation use cases are `offline-synth` at the pipeline level (frozen cache + synthetic anomaly, per [docs/07 §5.2](07-data-model.md)) even though the real event is `field` — the test asserts the correlation logic, not the weather. `offline-*` tests are HIL-ready by construction (§1.1), so a `hil` run of an offline use case is extra confirmation, not a different test.

## 3. Fixtures

- **Format: SigMF** everywhere ([docs/03 §1.6](03-sdr-software.md), [SigMF spec](https://github.com/sigmf/SigMF)) — `.sigmf-data` + `.sigmf-meta` with ground-truth **annotations** (time/frequency boxes, labels, expected decodes, the `hackriff:truth` field in [sigmf-extension.md](sigmf-extension.md)). A fixture is self-describing and doubles as a Recording object ([docs/07 §2.12](07-data-model.md)). The annotations are for the test harness only; §3.1 says how they are kept from the system.
- **Sources, with licences checked before use** (tracked in [ADR-0010](adr/0010-language-and-licence-ledger.md)):
  - **Own captures** — the primary source; captured with the HackRF One per the [docs/12](12-implementation-plan.md) fixture plan, annotated, committed as fixtures. Licence: ours.
  - **Public SigMF / IQEngine archives, sigidwiki samples** — for signals we can't easily generate; **check each set's licence** (many are CC-variants) before committing.
  - **Synthetic (TorchSig / own generator, MIT)** — for T4 scenarios with exact ground truth and impairments (IQ imbalance, spurs, overload, fading) matching real front-end effects ([docs/03 §4.1](03-sdr-software.md)).
  - **RadioML** — usable for algorithm prototyping only, **not** as product acceptance (documented dataset errata, non-commercial licence, [docs/04 §5.3](04-radio-engineering-and-signals-analysis.md)).
- **Size & storage:** IQ fixtures are large; keep them out of the main Git history. **Git LFS** for fixtures under a size cap, or an external fixture store fetched by a script with checksums. CI pulls only the small fixtures it needs; large HIL/field captures live in the external store. Decided concretely in [docs/12](12-implementation-plan.md).
- **Synthetic-first for CI:** generated fixtures are tiny to store (a generator seed + params), so most T4 scenarios are code, not files.

### 3.1 Ground truth is hidden (blind acceptance)

Acceptance tests prove the system **finds and explains** signals on its own. They don't check that it can look up a frequency it was already told about (T-047).

- **Truth list per fixture.** Every fixture, recorded or synthetic, carries a list of its known-interesting emissions: frequency, bandwidth/extent, time span and label. Recorded fixtures get theirs from annotation; synthetic ones get theirs from the generator.
- **Stripped before replay.** The mock device serves only IQ plus capture metadata (centre, rate, gains, provenance), with annotations and truth removed. The harness asserts that the system never opens the truth file.
- **Blind detection, then two asserts per truth emission:**
  1. It was **detected**, within frequency/extent/time tolerances.
  2. A reasonable **explanation appears among the top-k recommended explanations** (ranked output from T-039). k and the tolerances are set once in the harness; the values are provisional until T-047 lands.

  Emissions the system finds beyond the truth list are allowed, within a false-alarm bound.
- **Perturbed variants.** Each key case gets a variant the database can't answer, so a lookup can't pass it. For example, the `fm_100p8M` FM station shifted +150 kHz in a synthetic variant must still be detected, keep "FM broadcast" in its top-k, and be **flagged off-raster**. Other useful perturbations: off-allocation placement (AWARE-053 `unexpected-here`), level changes into overload, and truncated or overlapping bursts.
- **The known-signal database only recommends.** Band plans, licences and signal databases rank candidate explanations and set `known`/`unexpected-here` ([docs/07 §2.11](07-data-model.md)). They are never truth, never tell the scheduler where to tune in a test, and never pre-populate the inventory.
- **HIL truth.** Live air has no fixture metadata, so the T5 run derives its truth list from an independent survey of an always-occupied band (FM broadcast, T-053). It is logged, not gated.

### 3.2 Anti-patterns (review rejects these)

- **Direct file feeding.** A T3+ test constructs the pipeline from a SigMF reader or sample array and bypasses the device interface. Use the mock device instead.
- **Lookup-and-tune.** A test (or test-only code path) reads a frequency from the known-signal DB, band plan or truth list, then tunes there or asserts only there. Tuning comes from the system's own survey/dwell scheduler ([ADR-0005](adr/0005-survey-dwell-scheduler.md)); the truth list is only read by the asserting harness, after the run.
- **Demo seeds in serving.** `hk serve`/`hackriffd` pre-populate the inventory or explanations from seed data or the DB. Normal serving has no demo seeds: a fresh server with no input has an empty inventory. Seeds live only inside unit tests (T-042).
- **Truth leakage.** The pipeline reads annotations, `hackriff:truth` or truth filenames, or a test passes because the fixture's label reached the system.
- **DB-as-truth.** A test asserts a label is correct only because it matches the database entry for that frequency, with no perturbed variant.

## 4. From a use-case ID to tests

A use-case ID becomes one or more test cases that assert on the **data-model objects** it should produce. Every T3/T4 case loads its fixture into the mock device, lets the system survey and detect blind, and then asserts against the hidden truth list. Worked pattern (matching the [docs/07 §5](07-data-model.md) examples):

- **SIGNAL-001 ADS-B** (`offline-recorded`): mock device loaded with a recorded 1090 MHz SigMF fixture; the system finds the emission itself → assert (T3) the truth emission is detected, N CRC-valid Decode messages, ≥1 aircraft Emitter with a hex identity, "ADS-B" in the top-k explanations, and inventory `last_seen` updated. Underneath: T1 preamble correlation, T2 the readsb plugin wrapper.
- **SIGNAL-062 FM broadcast** (`offline-recorded` + synthetic variant): mock device serving `fm_100p8M` → the station is detected at its true frequency, auto-mode selects WFM, and "FM broadcast" is in the top-k. The +150 kHz shifted variant must also be detected, keep "FM broadcast" in its top-k, and be flagged off-raster. No test tunes to a DB frequency.
- **AWARE-036 unknown ISM burst** (`offline-synth`): generate an FSK sensor burst train (T4), served through the mock device → assert Detections matching the truth bursts, one unknown Emitter with `known_status: unknown`, estimated symbol rate ±1%, recovered framing, and (if seeded with a CRC) a valid Decode flipping status toward identified.
- **AWARE-053 allocation priors** (`offline-synth`): the same emitter placed on- and off-allocation → the priors recommend the allocation in the top-k and set `known` vs `unexpected-here`. The truth list, not the database, says which placement is off-allocation.
- **AWARE-006 GNSS jamming attack-map** (`offline-synth`): synthetic L1 noise-floor rise served through the mock device + a frozen gpsjam ExternalEvent → assert an Anomaly and a top-ranked Explanation of type time-coincidence/geometry; assert no Explanation when the cache lacks a matching event. The frozen feed is context input, not truth.
- **SPACE-050 noise-floor survey** (`field` primary, `offline-synth` for the pipeline): T4 asserts the calibrated floor is recovered within ±1 dB on a synthetic capture served through the mock device, including its out-of-coverage noise and gain/clip behaviour; the science claim itself is confirmed in the field (T6).

Each acceptance test names its use-case ID(s) and its fixture's truth list, so coverage is traceable both ways; the Phase 7 tasks carry the IDs they satisfy. Existing acceptance tests that feed files directly or use lookup-and-tune are audited and rewritten under T-047.

## 5. CI without hardware, and what only the field can verify

- **CI (no hardware):** T1 unit, T2 component (including device-interface conformance), T3 replay of committed/LFS fixtures through the mock device, T4 synthetic scenarios through the mock device, and the frozen-cache correlation tests. This covers the great majority of use cases (all `offline-synth`, `offline-recorded`, `data-only`). CI asserts detections, estimated parameters, decoded bits/messages, and inventory/explanation rows blind against hidden truth — the full pipeline, deterministically.
- **Nightly/bench HIL (T5):** the **same acceptance suite** run with the real HackRF selected (receive-only, T-053), plus sustained-rate tests on a bench rig. It is triggered manually or on a self-hosted runner with the radio, one HackRF user at a time, and its results are recorded as dated reports (not gating CI).
- **Field (T6):** propagation, space-weather, DF, passive-radar, and "explain a real event" claims. These are validated opportunistically, logged as dated field reports with the SigMF capture attached, and their *pipeline* portion is still covered offline. They never gate CI.
- **Provenance in tests:** every replayed fixture carries provenance so tests can assert that overload/spur flags propagate (the trust requirement, [docs/07 §2.6](07-data-model.md)). The mock device's gain-clip and out-of-coverage flags exercise this path deterministically.

## 6. Coverage bookkeeping

`test_tier` is filled for all 398 use cases in `use-cases.yaml`. Distribution:

| test_tier | Count | In CI (no hardware)? |
|---|---:|---|
| `field` | 143 | No — opportunistic, logged; pipeline portion still tested offline |
| `offline-recorded` | 92 | Yes (T3) |
| `offline-synth` | 80 | Yes (T4) |
| `data-only` | 53 | Yes (frozen-cache / archive) |
| `hil` | 30 | No — bench rig |

**225 of 398 (57%) are CI-testable with no hardware** (`offline-synth` + `offline-recorded` + `data-only`); 173 need hardware or the field (`hil` + `field`). The `field` count is high because SPACE/PROP science use cases anchor on their real-world physical claim (the SPACE-050 rule in §4) even when the underlying algorithm is separately synth-testable — for those, the *pipeline* is still covered offline and only the science claim is field-confirmed.

- The first vertical slice ([docs/11](11-roadmap.md)) deliberately picks use cases that are `offline-synth`/`offline-recorded` (six of seven) so the whole chain is provable in CI before any field work. M0b moves those acceptance tests behind the mock device, makes them blind (§1.1, §3.1), and runs them once against the real HackRF.
- Regenerate this table when `test_tier` values change: `python3 -c "import yaml,collections; print(collections.Counter(u['test_tier'] for u in yaml.safe_load(open('docs/use-cases.yaml'))['use_cases']))"`.

## Sources

- [SigMF specification](https://github.com/sigmf/SigMF): fixture format, captures and annotations.
- [SoapySDR](https://github.com/pothosware/SoapySDR): the vendor-neutral device API the generic interface should be able to back later. Its mapping onto our interface is unverified until a SoapySDR device is added.
- Internal: [docs/07](07-data-model.md) (objects asserted on), [ADR-0005](adr/0005-survey-dwell-scheduler.md) (where tuning decisions come from), [sigmf-extension.md](sigmf-extension.md) (`hackriff:truth`), [tasks.yaml](tasks.yaml) T-039, T-042, T-047, T-048, T-049, T-053.
