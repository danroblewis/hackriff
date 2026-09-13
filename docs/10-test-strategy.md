# 10 — Test strategy

*Architecture planning, Phase 5. Drafted 2026-09-13. Status: **PROVISIONAL.** The use cases in [docs/05](05-use-cases-and-explorations.md) / [`use-cases.yaml`](use-cases.yaml) are the acceptance targets; this document says how an ID becomes tests and defines the `test_tier` rubric now filled into the YAML.*

The guiding rule from CLAUDE.md and [docs/04 §11.2](04-radio-engineering-and-signals-analysis.md): **build end-to-end tests that replay IQ through the full pipeline and assert on detections, estimated parameters, decoded bits, and inventory entries**, with unit tests under the DSP blocks. Because measurements are immutable and interpretations are versioned ([docs/07](07-data-model.md)), the same fixture re-run after an algorithm change is a regression test for free.

## 1. Test tiers

| Tier | What it covers | Runs in CI? | Example |
|---|---|---|---|
| **T1 DSP unit** | One block/kernel against known input/output | Yes | FFT/PSD correctness; OS-CFAR false-alarm rate on synthetic noise; M2M4 SNR estimate |
| **T2 component** | One capability end to end in isolation | Yes | noise-floor tracker on a drifting-floor fixture; channelizer channel isolation; SQLite inventory upsert/query |
| **T3 end-to-end IQ replay** | A SigMF recording replayed through the whole pipeline, asserting on Detection / Demodulation / Decode / Emitter rows | Yes | replay an ADS-B capture → assert CRC-valid message count and one aircraft Emitter |
| **T4 synthetic scenario** | Generated IQ with known ground truth (emitters, noise, interference, overload) exercising detection→classify→estimate→inventory | Yes | TorchSig/own generator: 3 emitters + FM blocker at −10 dBFS → assert all found, blocker flagged, false-alarm under bound |
| **T5 hardware-in-the-loop (HIL)** | HackRF in the loop: TX→RX loopback, live capture correctness, timing/throughput | No (nightly on a bench rig / manual) | TX a known waveform, receive and decode it; sustained-rate drop test (spike S2) |
| **T6 field** | Real over-the-air conditions that cannot be captured-and-replayed meaningfully | No (opportunistic, logged) | a real Sporadic-E opening; walking a DF fix; a solar flare SID event |

T1–T4 are the **CI backbone** and need no hardware. T3/T4 are where use-case acceptance lives. T5/T6 confirm what only real hardware or real propagation can.

## 2. `test_tier` rubric (filled into use-cases.yaml)

Each use case gets one **primary** `test_tier` — the highest-fidelity tier at which its core claim can be asserted **cheaply and repeatably**. Most acceptance is T3/T4 (offline IQ replay); the value says whether a use case is CI-testable offline or needs hardware/field/data.

| Value | Meaning | Assigned when |
|---|---|---|
| `offline-synth` | Validated with **generated** IQ + known truth (T4, plus T1/T2 underneath) | The signal can be synthesised faithfully enough (most detection, parameter-estimation, classification, modem, and framing use cases) |
| `offline-recorded` | Validated by **replaying a recorded/public SigMF** fixture (T3) | A real signal we can capture once (or fetch from a public archive) and replay — most decoder use cases (ADS-B, AIS, rtl_433 devices, pagers, sats) |
| `hil` | Needs the **HackRF in the loop** (T5) | TX/own-link use cases, and live-behaviour/throughput/timing claims not capturable as a static file |
| `field` | Needs **real OTA conditions** (T6) | Propagation, space-weather, DF/localization, passive radar, moving-emitter, and "explain a real event" use cases |
| `data-only` | Validated against **cached feed/archive data**, no IQ | `data-only` fit_flag use cases (spot-archive mining, dataset/toolkit items, external-feed correlation with frozen caches) |

Rules when several could apply, most-CI-friendly first: prefer `offline-synth` if we can generate the signal; else `offline-recorded` if we can capture/fetch it; else `hil` (own TX/live) ; else `field`; `data-only` only when no local IQ is involved. Attack-map/event-correlation use cases are `offline-synth` at the pipeline level (frozen cache + synthetic anomaly, per [docs/07 §5.2](07-data-model.md)) even though the real event is `field` — the test asserts the correlation logic, not the weather.

## 3. Fixtures

- **Format: SigMF** everywhere ([docs/03 §1.6](03-sdr-software.md)) — `.sigmf-data` + `.sigmf-meta` with ground-truth **annotations** (time/frequency boxes, labels, expected decodes). A fixture is self-describing and doubles as a Recording object ([docs/07 §2.12](07-data-model.md)).
- **Sources, with licences checked before use** (tracked in [ADR-0010](adr/0010-language-and-licence-ledger.md)):
  - **Own captures** — the primary source; captured with the HackRF One per the [docs/12](12-implementation-plan.md) fixture plan, annotated, committed as fixtures. Licence: ours.
  - **Public SigMF / IQEngine archives, sigidwiki samples** — for signals we can't easily generate; **check each set's licence** (many are CC-variants) before committing.
  - **Synthetic (TorchSig / own generator, MIT)** — for T4 scenarios with exact ground truth and impairments (IQ imbalance, spurs, overload, fading) matching real front-end effects ([docs/03 §4.1](03-sdr-software.md)).
  - **RadioML** — usable for algorithm prototyping only, **not** as product acceptance (documented dataset errata, non-commercial licence, [docs/04 §5.3](04-radio-engineering-and-signals-analysis.md)).
- **Size & storage:** IQ fixtures are large; keep them out of the main Git history. **Git LFS** for fixtures under a size cap, or an external fixture store fetched by a script with checksums. CI pulls only the small fixtures it needs; large HIL/field captures live in the external store. Decided concretely in [docs/12](12-implementation-plan.md).
- **Synthetic-first for CI:** generated fixtures are tiny to store (a generator seed + params), so most T4 scenarios are code, not files.

## 4. From a use-case ID to tests

A use-case ID becomes one or more test cases that assert on the **data-model objects** it should produce. Worked pattern (matching the [docs/07 §5](07-data-model.md) examples):

- **SIGNAL-001 ADS-B** (`offline-recorded`): replay a recorded 1090 MHz SigMF fixture → assert (T3) N CRC-valid Decode messages, ≥1 aircraft Emitter with a hex identity, and inventory `last_seen` updated. Underneath: T1 preamble correlation, T2 the readsb plugin wrapper.
- **AWARE-036 unknown ISM burst** (`offline-synth`): generate an FSK sensor burst train (T4) → assert Detections, one unknown Emitter with `known_status: unknown`, estimated symbol rate ±1%, recovered framing, and (if seeded with a CRC) a valid Decode flipping status toward identified.
- **AWARE-006 GNSS jamming attack-map** (`offline-synth`): synthetic L1 noise-floor rise + a frozen gpsjam ExternalEvent → assert an Anomaly and a top-ranked Explanation of type time-coincidence/geometry; assert no Explanation when the cache lacks a matching event.
- **SPACE-050 noise-floor survey** (`field` primary, `offline-synth` for the pipeline): T4 asserts the calibrated floor is recovered within ±1 dB on a synthetic capture; the science claim itself is confirmed in the field (T6).

Each acceptance test names its use-case ID(s) so coverage is traceable both ways; the Phase 7 tasks carry the IDs they satisfy.

## 5. CI without hardware, and what only the field can verify

- **CI (no hardware):** T1 unit, T2 component, T3 replay of committed/LFS fixtures, T4 synthetic scenarios, and the frozen-cache correlation tests. This covers the great majority of use cases (all `offline-synth`, `offline-recorded`, `data-only`). CI asserts detections, estimated parameters, decoded bits/messages, and inventory/explanation rows — the full pipeline, deterministically.
- **Nightly/bench HIL (T5):** HackRF loopback and sustained-rate tests on a bench rig; triggered manually or on a self-hosted runner with the radio; results recorded as dated reports (not gating CI).
- **Field (T6):** propagation, space-weather, DF, passive-radar, and "explain a real event" claims. These are validated opportunistically, logged as dated field reports with the SigMF capture attached, and their *pipeline* portion is still covered offline. They never gate CI.
- **Provenance in tests:** every replayed fixture carries provenance so tests can assert that overload/spur flags propagate (the trust requirement, [docs/07 §2.6](07-data-model.md)).

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

- The first vertical slice ([docs/11](11-roadmap.md)) deliberately picks use cases that are `offline-synth`/`offline-recorded` (six of seven) so the whole chain is provable in CI before any field work.
- Regenerate this table when `test_tier` values change: `python3 -c "import yaml,collections; print(collections.Counter(u['test_tier'] for u in yaml.safe_load(open('docs/use-cases.yaml'))['use_cases']))"`.
