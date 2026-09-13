# 12 — Implementation plan for slice 1

*Architecture planning, Phase 7. Drafted 2026-09-13. Status: **PROVISIONAL.** This turns the [docs/11](11-roadmap.md) slice into something a coordinator session can execute. Task **state** lives in [`tasks.yaml`](tasks.yaml) (the single source of truth); this file is the narrative around it. A fresh coordinator session resumes from `tasks.yaml` + [planning-log.md](planning-log.md) + git.*

## 1. Spike execution order

From [docs/09 §3](09-risks-and-spikes.md). Run the Mac + HackRF spikes first; they need no Jetson and de-risk the most.

| Order | Spike | Runs on | Unblocks (ADR stays PROVISIONAL until it passes) |
|---|---|---|---|
| 1 | **S4** detection in urban overload | Mac + HackRF now | ADR-0005; C05/C09 thresholds. If it fails, prioritise the preselector/notch accessory before M0 detection is trusted. |
| 1 | **S5** blind estimation on 8-bit captures | Mac + HackRF now | C14/C15 confidence. If weak, blind estimation becomes assistive and decoders/priors lead (affects T-011/T-013). |
| 2 | **S1** live reconfiguration | Mac (file source) | ADR-0001 (runtime substrate: FutureSDR vs owned). Blocks the T-001 scaffold's runtime choice. |
| 3 | **S3** web waterfall frame rate | Mac, then Jetson | ADR-0002 (else native-shell fallback for the on-device waterfall). Blocks T-022 UI approach. |
| 4 | **S2** throughput + power | **Jetson** (on arrival) | ADR-0001/0007 on real hardware. |
| 5 | **S6** battery + thermal | **Jetson** + enclosure mock | ADR-0009 battery/enclosure. |
| 6 | **S7** two-HackRF coherence | Mac + 2× HackRF (optional) | C32/C35 fit; only if DF/passive-radar wanted on the base device. |

**Which ADRs are provisional and what changes if a spike fails:** ADR-0001 → owned-dataflow fallback (S1) or reconsider decimation/architecture (S2); ADR-0002 → native on-device waterfall shell (S3); ADR-0005/detection → hardware preselector moves earlier (S4); C14/C15 → blind estimation demoted to assistive (S5); ADR-0009 → cooling/battery redesign (S6). None of these invalidate the data model (docs/07) or the plugin/stream contracts.

## 2. Repo scaffold

A Cargo workspace for the Rust core, a Python tooling env, and a web UI, in one repo (the whole device state and code is one thing).

```
hackriff/
  Cargo.toml                # workspace
  crates/
    hk-model/     # data-model types + SQLite repository (docs/07)  [core interface]
    hk-core/      # source-abstraction, RAM ring buffer, sweep/dwell, scheduler host, SigMF replay source
    hk-dsp/       # spectral estimation, noise floor, channelizer; liquid-dsp (FFI) + cuFFT; CPU fallback
    hk-detect/    # CFAR detection, burst records, burst tracking
    hk-estimate/  # parameter + blind symbol estimation
    hk-demod/     # analog auto-mode + digital demod (own), RDS
    hk-store/     # spectrum-history pyramid, SigMF recording, retention policy
    hk-context/   # feed cache, priors lookup, event correlation
    hk-api/       # control plane + stream-output contract (ADR-0004)  [core interface]
    hk-plugins/   # plugin host, manifest, IPC (ADR-0003)  [core interface]
    hk-cli/       # binaries: hackriffd (daemon), hk (control CLI)
  plugins/        # decoder plugin manifests + thin wrappers (readsb, rtl_433, ...)
  ui/             # TypeScript + WASM web client (WebGL2 waterfall, inventory, region-over-time)
  py/             # synthetic IQ generator, SigMF fixture tooling, research notebooks (orchestration/research only)
  spikes/         # throwaway spike code S1..S7 (not product code)
  fixtures/       # small committed SigMF (Git LFS); large captures in an external store fetched by script
  tests/          # end-to-end IQ-replay scenarios (the 7 slice acceptance tests)
  docs/           # planning docs (existing)
```

- **Language/toolchain per component** ([ADR-0010](adr/0010-language-and-licence-ledger.md)): Rust for every `crates/*`; C via FFI for liquid-dsp; CUDA (built on-device) for `hk-dsp` GPU kernels; TypeScript+WASM for `ui/`; Python (via **uv**) for `py/` and fixture/synthetic tooling only.
- **Build system:** Cargo workspace + a `justfile` (or Makefile) for common commands (`just test`, `just replay <fixture>`, `just deploy-jetson`). Python deps in `py/pyproject.toml` under uv.
- **macOS dev → Jetson deploy:** develop and run T1–T4 tests on macOS with a **CPU FFT fallback** in `hk-dsp` (no CUDA on Mac). Deploy to the Jetson by **remote build on the device** first (rsync the tree, `cargo build` on the Jetson where CUDA/JetPack live), moving to cross-compilation (`aarch64-unknown-linux-gnu`) once the build stabilises. GPU kernels only compile on the Jetson; a Cargo feature flag (`gpu`) gates them so the Mac builds the CPU path.
- **Fixtures:** small SigMF committed via **Git LFS** under a size cap; large HIL/field captures in an external store (object storage or a directory synced out of band) fetched by `py/fixtures/fetch.py` with checksums. Synthetic scenarios are code (a seed + params), not files.

## 3. Dev environment and CI

- **HackRF on macOS:** `brew install hackrf`; verify with `hackrf_info`; capture with `hackrf_transfer` (the user already has `tools/` scripts for sweep and FM). Confirm the r10/clone caveats from [docs/01](01-hackrf-and-portapack.md).
- **Jetson / JetPack:** flash **JetPack 6.2** (verified 2026); confirm CUDA/cuFFT, set power mode, install the Rust toolchain and build deps on-device. `tegrastats` for the S2/S6 measurements.
- **CI (no hardware):** GitHub Actions (or a local runner) runs `cargo test` (T1 unit, T2 component, T3 replay of committed/LFS fixtures, T4 synthetic scenarios) and the Python tooling tests, plus the frozen-cache correlation tests. Green CI = the whole slice chain proven offline. The `gpu` feature is off in CI (CPU FFT fallback).
- **HIL (T5):** a self-hosted runner or bench script with the HackRF, run nightly/manually (TX loopback, sustained-rate drop test). Results are dated reports, not CI gates.
- **Field (T6):** logged manually as dated reports with the SigMF capture attached; never gate CI.

## 4. Fixture capture plan (slice 1)

Capture with the HackRF One; annotate as SigMF; commit small ones via LFS. Synthetic generation covers the rest.

| Fixture | For | Freq / rate | Duration | Gain | Annotations |
|---|---|---|---|---|---|
| ADS-B | SIGNAL-001 | 1090 MHz, 8–20 Msps | 60 s | LNA on, amp on, antenna | expected aircraft hex ids, message count |
| FM + RDS | SIGNAL-062 | strong local station, 2.4 Msps | 30 s | moderate | station PI/PS, 19 kHz pilot present |
| Unknown ISM sensor | AWARE-036 (recorded companion) | 433.92 / 315 MHz, 2–4 Msps | 2–5 min (to catch bursts) | LNA on | burst times, known device truth if any |
| Wide sweep survey | SPACE-050 / AWARE-042 | 1 MHz–1 GHz sweep | minutes | per-band table | quiet-band reference for floor |
| Terminated-input | C05 spur map | across tuning range | short | stepped gains | internal spur frequencies |

- **Synthetic (primary for CI):** AWARE-036 FSK burst train, AWARE-006 L1 noise-floor rise, SPACE-050 injected floor, AWARE-042 multi-hour occupancy — all generated by `py/` with exact truth and front-end impairments (TorchSig + own generator).
- **Public sets (licence-checked before commit, [ADR-0010](adr/0010-language-and-licence-ledger.md)):** an ADS-B and an FM/RDS SigMF from IQEngine/sigidwiki as cross-checks; a gpsjam extract for the AWARE-006 cache. RadioML is prototyping-only, not acceptance.

## 5. Task list

The ordered, dependency-aware task breakdown is in [`tasks.yaml`](tasks.yaml). Each task carries: `id`, `title`, `status`, `deps`, `use_cases` + `acceptance` (the docs/11 §1.1 tests it serves), `area`/`files` (so parallel worktrees don't collide), `needs` (`none`/`hardware`/`user`), `model`+`effort` ([model-selection](../prompts/model-selection.md)), `parallel_group`, and `dod` (definition of done). Summary:

- **Foundation (sequential):** T-001 scaffold+CI → T-002 data-model+repository, T-003 source+replay, T-023 synthetic generator+replay harness (these three parallel after T-001).
- **Sense chain:** T-004 spectral estimation → T-005 noise floor → T-006 CFAR detection → T-007 burst tracking; T-008 channelizer parallel.
- **Characterize/demod:** T-010 param estimation, T-011 blind symbol estimation (gated S5), T-012 analog+RDS, T-013 digital demod + framing (gated S5).
- **Route/output/memory:** T-014 plugin host (core contract), T-015 readsb plugin, T-016 stream-output (core contract), T-017 spectrum history+query, T-018 inventory+clustering.
- **Explain/priors:** T-019 priors band-plan lookup, T-020 feed cache + gpsjam + correlation, T-021 radiometry product.
- **UI + acceptance:** T-022 web UI (gated S3), T-024 wire the 7 acceptance tests, T-009 sweep+simple scheduler.
- **Hardware/user:** T-025 fixture capture (needs HackRF + user).

Tasks touching **core interfaces** (T-002 schema, T-014 plugin contract, T-016 stream contract, T-006 detection thresholds, T-009 scheduler) are **Fable/Opus only** and reviewed before merge ([model-selection](../prompts/model-selection.md)); T-015/T-019/T-022/T-024 are Sonnet-suitable against those contracts. Parallel groups are marked in `tasks.yaml`.

## 6. Hardware shopping list (with timing; prices unverified — confirm at purchase)

| Item | Why | Needed by | Rough cost (unverified) |
|---|---|---|---|
| **HackRF One** | RF head (already owned) | now — spikes S4/S5, all fixtures | ~$300–350 |
| **Jetson Orin Nano Super dev kit, 8 GB** | compute; GPU FFT/channelizer/ML | before spike **S2** (throughput on real hardware), ~after the Mac spikes | **$249** (verified list; stock thin) |
| **NVMe SSD 512 GB–1 TB (M.2)** | IQ pool + spectrum pyramid + state | with the Jetson | ~$50–120 |
| **FM band-stop + a switched sub-octave preselector/notch bank** | the highest-value front-end fix for trustworthy detection in a city; escalated if **S4** fails | M0 detection hardening / early M1 | notch ~$30; filter bank ~$100–300 (or DIY + Opera Cake) |
| **Opera Cake antenna switch** | drives the filter bank / per-band antennas in sync with sweep | with the filter bank | ~$40–100 |
| **Wideband antenna set** (discone + a Yagi/LPDA) | survey omni + directional for RSSI DF | fixtures / M0 | ~$40–120 |
| **LNA + bias-tee (switchable)** | sensitivity for weak-signal dwell; ADS-B/sat | M1 (decoder breadth) | ~$20–50 |
| **GPSDO (10 MHz)** | disciplined time/frequency; marginal-HF science, DF | M2/M5 (not slice 1) | ~$100–200 |
| **HF upconverter / active loop** | HF/VLF science coverage | M5 | ~$50–150 |
| **5–7″ touch IPS + encoder/buttons** | on-device UI (phone-as-display until then) | M7 | ~$50–120 |
| **Battery pack (~99–158 Wh) + enclosure/cooling** | portable operation; thermal spike S6 | M7 / after S6 | ~$100–300 |
| **Ku LNB + small dish** | Ku-band science/decoders | M5 (optional) | ~$30–80 |

Order the **Jetson dev kit + NVMe** as soon as the Mac spikes (S4/S5/S1/S3) are under way, so S2/S6 aren't blocked. The **preselector/notch** is the next priority if S4 shows urban false alarms. Everything else is milestone-timed, not slice-1-blocking.

## 7. CLAUDE.md additions

Phase 7 adds two concise sections to CLAUDE.md — **Engineering** (repo layout, build/test/run commands incl. IQ-replay, conventions, licence-ledger rule, how to add a use case/fixture) and **Coordination** (how the coordinator briefs subagents, which files/ADRs to read, use-case IDs as definition of done, worktree isolation + merge/review rules, where task state lives, what stays interactive with the user, ask-before-committing) — plus a refreshed Status. Those are applied to CLAUDE.md directly in this phase.
