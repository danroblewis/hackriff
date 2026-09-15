# hackriff

An exploration-first signals-analysis tool for software defined radio. It's meant to replace the HackRF PortaPack: a portable device plus software that **finds** interesting signals instead of making you already know the frequency and pick FM/AM/WFM by hand. It serves science (space weather, radio noise, propagation), spectrum awareness, and decoding unknown signals, as well as hobby listening.

## Status

**Phase: M0 complete locally (2026-09-13); building M0b "Live device + exploration UI".** Product code: Cargo workspace, data model + SQLite, ring/source/replay, live HackRF source (feature `hackrf`, receive-only), generic device contract, spectral estimation, noise floor, channelizer/DDC, compute providers (CPU, Accelerate, wgpu GPU; conformance suite), detection/tracking/trust, blind estimation, WFM/RDS + FSK demod, stream contract + plugin host (readsb), spectrum history, band-plan priors with ranked explanations, context feeds + anomaly correlation, composed pipeline (`hk replay/run/serve`, `hackriffd`), web UI (waterfall/spectrum, hover, multi-region selections, inspect), synthetic generator + e2e harness, M0 acceptance suite (blind). All M0 tasks are done except T-026 (GPU PFB on Jetson, blocked). The M0 acceptance suite passes locally; CI is unverified because nothing has been pushed. M0b (docs/11 §1.2) adds the mock SDR device with e2e through the device interface, authenticated control API + SDR control panel, Listen, persisted selections, and HIL runs. Live task state is `docs/tasks.yaml`; decisions, review findings and user blockers are in the `docs/planning-log.md` build log (B0.x). Research is in `docs/01`–`05`; planning in `docs/06`–`12` + `docs/adr/` (ADRs PROVISIONAL). Needs the user: Jetson purchase (S2/S6, T-026), FM notch + S4b re-run, 1090 MHz antenna, 50 Ω terminator capture, open questions in the planning log.

Planning docs: `06` capability map, `07` data model, `08` architecture + `adr/` (10 ADRs), `09` risks & spikes, `10` test strategy, `11` roadmap/slice, `12` implementation plan. Read the relevant capability cards (`docs/capabilities/`) and the ADRs a task names before building; don't re-read `01`–`05` end to end.

`tools/` holds the user's quick HackRF experiments, not product code:
- `sweep_plot.py` plots `hackrf_sweep` CSV output and lists peaks.
- `fm_rx.py` is a numpy/scipy broadcast-FM receiver fed by `hackrf_transfer`.

| Doc | Use it for |
|---|---|
| `docs/README.md` | Index and the main findings across documents |
| `docs/01-hackrf-and-portapack.md` | What HackRF/Mayhem can and can't do; what to keep from them |
| `docs/02-sdr-landscape.md` | Front-end physics, hardware comparison, compute throughput math, hardware tiers |
| `docs/03-sdr-software.md` | Existing tools to reuse or learn from (SDRangel, SigDigger, URH, rtl_433, Trunk Recorder, SatDump, IQEngine, Maia SDR, GNU Radio 4, FutureSDR, TorchSig); gap table |
| `docs/04-radio-engineering-and-signals-analysis.md` | Algorithms: noise floor, CFAR, spectral kurtosis, parameter estimation, AMC, auto-demod/squelch, protocol ID, trunking, DF, calibration. §12 is a prioritized implementation list. |
| `docs/05-use-cases-and-explorations.md` | **The feature goals.** 391 use cases with permanent IDs (`SPACE-`, `PROP-`, `AWARE-`, `SIGNAL-`, `RESEARCH-`). These become the test suite. |
| `docs/use-cases.yaml` | Machine-readable copy of docs/05 and the source of truth for IDs. Planning adds `capabilities`, `hardware_fit` and `test_tier`. |

## Product vision (from the user)

The core workflow, in order:
1. **Peruse.** Browse live airwaves casually, or open reports of past scan surveys.
2. **Automate.** Run configurable, thorough scans across frequency regions on a schedule.
3. **Review history.** Choose a region and see what activity was seen there over time.
4. **Recommend explanations for what was detected.** Signals are always found by **blind detection** from RF data first, then characterized. Only then does the known-signal database (band plans, licences, catalogues) offer **ranked, reasoned suggestions**, e.g. "looks like FM; FM broadcast allocation nearby; 150 kHz off raster".
   - **Never a source of truth.** The database is never the starting point, never pre-populates the inventory, and never overrides what was measured.
   - **Mismatches are interesting.** An emission that doesn't match its expected frequency (e.g. a shifted FM station) is flagged, not snapped to the database.
   - **Unknown signals are the priority** to surface and catalogue.
   - **Tests:** fixtures carry a hidden ground-truth list of interesting emissions. A test runs blind detection, checks each one is found, and checks that a sensible explanation is among the top recommendations. Never look a frequency up in the database and tune there.
5. **Record and stream demodulations.** Parameters (modulation, bandwidth, squelch, AGC) are estimated from the signal, never picked manually.
6. **Decode to bitstreams.** Blind symbol and bit recovery, including for unknown signals.
7. **Stream bits to other programs**, which turn them into something useful. Decoders are pluggable consumers, not hard-coded apps.

Further goals:
- **A radio "attack map".** Like internet port-scan and botnet maps: explain *why* your spectrum changed by linking local anomalies to external events such as space weather, GNSS jamming, satellite passes, balloon launches and lightning. See docs/05 §3.
- **Science is first-class.** Solar flares, noise-floor studies and propagation, down to what a radio engineer knows but rarely watches.
- **General, not a decoder catalogue.** ADS-B, pagers and radiosondes are the well-known cases, not the point.
- **Tune from the processed output.** Closed-loop refinement: demodulate and decode, measure output quality (e.g. discriminator offset, pilot/RDS lock, audio SNR, CRC-valid rate), and adjust centre, bandwidth and other parameters from it. Rough selections and detections get refined automatically, not set by hand.

## Product constraints (decided with the user, 2026-09-13)

- **Form factor: portable handheld.** A PortaPack replacement: PortaPack-sized is ideal but unlikely; cyberdeck-sized is acceptable. Smaller than a laptop, not benchtop. A bigger battery and more weight are acceptable.
- **Scope: one self-contained device.** Networked sensor meshes, cellular-modem linking between devices, and fixed home-mounted sensors are **out of scope**.
  - The attack map therefore works from **this device's own survey history** plus **external context feeds** (space weather, GNSS-jamming maps, lightning, TLEs, SondeHub…) whenever connectivity exists.
  - Design offline-first: cache reference data (band plans, licence extracts, TLEs), and sync or export when online.
  - Don't design anything that rules out sharing later.
- **Hardware: HackRF One** as the RF front end (1 MHz–6 GHz, 20 Msps, 8-bit, half-duplex, USB 2.0, no preselector). **NVIDIA Jetson** as compute, preferred over a Pi 5 because it costs little more and adds a GPU.
  - Current module is likely the Jetson Orin Nano (Super) class; the original Jetson Nano is a legacy product. Confirm the current module, price, power modes and JetPack support.
  - Keep the front end abstracted so HackRF Pro or other SDRs can be added later.
  - Low-power operating modes matter.
- **Language: no preference; choose what has robust support.** Signal processing must be fast, so **Python is for orchestration and research only**, never the real-time path.
  - GNU Radio 3.10, GNU Radio 4 and FutureSDR (Rust) are all candidates, and nobody is attached to any of them.
  - **Hard requirement:** change and extend signal-processing pipelines **without stopping capture or rebuilding**. The user's past GNU Radio experience involved lots of recompiling. Find out whether that's inherent or avoidable.
- **UI: open question.** Web UI versus native (SDL/ImGui-style). The user considers this one of the most important decisions.
- **Licence: undecided, and not a development concern for now.** Don't let licences gate work.
- **Team: one developer**, with one likely friend-user. Favour low operational complexity, and don't over-build plugin infrastructure for hypothetical contributors.

## Key findings that constrain the design

- **Sweep to find *where*, dwell to find *what*.** A HackRF sweeps 0–6 GHz in under a second but misses short bursts. A scheduler has to balance survey against real-time dwell windows of up to about 20 MHz.
- **Classical DSP first, ML later.** The big wins are noise-floor estimation, CFAR plus spectral kurtosis, calibration and spur rejection, blind symbol-rate/frequency-offset/bandwidth estimation, and licence and band-plan priors. ML classifiers lose a lot of accuracy over the air and can't reliably say "unknown". Use ML as a stage after normalization, with an explicit unknown/open-set output. The Jetson GPU makes that stage feasible.
- **The front end limits what automation can trust.** An 8-bit HackRF without preselection mostly shows intermodulation in cities. Detections need provenance: gain state, overload flags, spur masks. A switched filter bank is a likely hardware add-on.
- **No open-source tool covers the whole chain** (survey → detect → classify → estimate → decode → inventory → record). Reuse proven decoders and ideas rather than rewriting them.
- **Many use cases fall outside 1 MHz–6 GHz** (VLF/ELF, Ku-band) or need TX. Model receiver capabilities explicitly and mark what the base device can't do.

## Test strategy direction

Items in `docs/05` and `docs/use-cases.yaml` are acceptance targets:
- Map each ID to the capabilities it exercises. Tag whether it can be tested **offline with recorded or synthetic IQ** or needs live hardware.
- Prefer **SigMF** recordings plus annotations as fixtures. Candidate sources: your own captures, IQEngine/SigMF archives, sigidwiki samples, and synthetic data (TorchSig / generated).
- Build end-to-end tests that replay IQ through the full pipeline and assert on detections, estimated parameters, decoded bits and inventory entries. Keep unit tests for the DSP blocks.
- **End-to-end tests drive the system through the SDR device interface.** They never feed files straight into the pipeline.
  - **A mock SDR device** implements the same interface as the real HackRF source: tune, sample rate, gains, bias-tee, sweep, start/stop, timestamps, overruns. It replays recorded SigMF IQ behind that interface, honouring retunes and gain/rate changes realistically.
  - **The same tests can run against the real HackRF** as hardware-in-the-loop tests.
- **Keep the device interface generic** so other SDRs (e.g. SoapySDR) can be added later. That's not required yet, but don't bake HackRF specifics into the core.

## Working conventions

- Docs are numbered markdown files in `docs/` with inline source links and a Sources section. Mark anything unverified. Keep `docs/README.md` indexed. Architecture decisions go in `docs/adr/NNNN-title.md`.
- Use-case IDs are permanent. Never renumber; append new ones. Keep docs/05 and `use-cases.yaml` in sync.
- **Per-capability context:** `docs/capabilities/Cnn-slug.md` cards (index: `docs/capabilities/README.md`) condense docs 01–05 for each capability in docs/06. Agents read the relevant cards and their reading lists, not the full research docs. Keep cards in sync when the taxonomy or ADRs change.
- **Model and effort choice:** follow `prompts/model-selection.md` when starting sessions, briefing subagents, or writing task entries.
  - Fable: architecture, core contracts, novel DSP, hard debugging.
  - Opus: coordinator, core real-time implementation, reviews.
  - Sonnet: well-specified tasks with tests.
  - Haiku: mechanical work.

  Changes to core interfaces or the real-time path never go to Sonnet or Haiku alone.
- Don't recommend SDR#/GQRX-style tune-and-listen tools as answers; the user wants exploration and analysis tooling.
- The user runs the `md` doc viewer and cloudflared tunnel themselves. Don't start, restart or kill those processes.
- Ask before committing.

## Engineering

Repo layout and build come from `docs/12` and the ADRs; the workspace below exists (T-001 landed). Crates fill in as M0 tasks merge.

- **Layout:** Cargo workspace in `crates/` (`hk-model` data model + SQLite; `hk-core` source/ring/scheduler; `hk-dsp` spectral/noise/channelizer; `hk-detect`; `hk-estimate`; `hk-demod`; `hk-store` history+SigMF; `hk-context` feeds+priors+correlation; `hk-api` control API (re-exports `hk-stream`); `hk-stream` stream-output contract (framing, gating, publisher); `hk-plugins` plugin host; `hk-cli`). `plugins/` decoder manifests+wrappers; `ui/` TS+WASM web client; `py/` synthetic-gen/fixtures/research (orchestration/research only, never the real-time path); `spikes/` throwaway; `fixtures/` SigMF (Git LFS / external store); `tests/` e2e IQ-replay.
- **Languages/licence rule:** Rust core, C-via-FFI liquid-dsp (MIT), CUDA (Jetson-only, behind the `gpu` cargo feature), TS+WASM UI, Python tooling. **GPLv3 code (VOLK, GNU Radio, most decoders) stays behind the plugin process boundary** (ADR-0010). The dependency licence ledger is optional bookkeeping, not a gate.
- **Build/test/run:** `just build`, `just test` (T1 unit + T2 component + T3 replay + T4 synthetic; no hardware; `gpu` off), `just replay <fixture.sigmf-meta>` (run a SigMF fixture through the pipeline), `just deploy-jetson` (rsync + on-device build). CI runs `just test` + Python tooling tests with no hardware and must stay green. HIL (T5) runs on a bench rig nightly/manually; field (T6) is logged, never gates CI.
  - **Testing (T-077):** `just test` runs the Rust suite through cargo-nextest in parallel; heavy/timing-sensitive tests (tests/e2e, hk-pipeline listen/retune/lossless/refine/stream, hk-api, hk-cli, hk-core ring stress/concurrency, hk-demod::refine_wfm_real) are pinned to a serial `heavy-serial` group by `.config/nextest.toml` so they don't race each other under load, everything else runs fully parallel. `just test-seq` is the fully sequential `cargo test` fallback. `just acceptance` (the M0 slice suite) stays its own step, matching the CI `test`/`acceptance` job split — the coordinator runs `just test` + `just acceptance` once per merge as the full check. **Agents working a single task run targeted tests, not the full suite:** `just test-crate <crate>` (`cargo nextest run -p <crate>`) or `just test-one <name>` (`cargo nextest run -E 'test(<name>) or binary(<name>)'`, matching by test-function or test-file name).
- **Adding a use case:** append an ID in `docs/05` and `use-cases.yaml` (never renumber), set `capabilities`/`hardware_fit`/`accessory`/`fit_flags`/`fit_note`/`test_tier` per `docs/06 §3` and `docs/10 §2`. **Adding a fixture:** capture/annotate as SigMF, put small ones in `fixtures/` (LFS) or the external store, reference it from the acceptance test by use-case ID.

## Coordination

Development runs from one long-lived coordinator session (Opus) that delegates to subagents/workflows. Files, not the conversation, carry state.

- **Task state lives in `docs/tasks.yaml`.** Update `status` (todo/in-progress/blocked/done) + commit/PR links there as work proceeds; a fresh session resumes from it + `docs/planning-log.md` + git. It is the single source of truth.
- **Briefing a subagent:** give it its task entry, the capability cards it touches (`docs/capabilities/`), and the ADRs + data-model sections the task names — not the full research docs. The **use-case IDs in the task are its definition of done**; the agent asserts on the data-model objects (`docs/07`) and reports back a short summary with results written to files.
- **Model/effort** per `prompts/model-selection.md`. Core-interface tasks (schema, plugin/stream contracts, detection thresholds, scheduler — marked `core_interface` in `tasks.yaml`) and anything touching the real-time path go to Fable/Opus and are reviewed before merge; never Sonnet/Haiku alone.
- **Parallel work uses git worktrees**, one per `parallel_group`; tasks sharing a crate serialise on it or split file ownership (see `tasks.yaml` notes). A cheaper model's output touching core interfaces is reviewed by Opus before merge. Changing an ACCEPTED ADR goes to Fable + the user.
- **A real HackRF One is attached to the dev Mac** and verified with `hackrf_info`: firmware 2026.01.3, board revision older than r6, on its own USB bus. Development may use it for receive-side work: spikes S4/S5/S1, fixture capture (T-025), HIL tests. Rules:
  - **One agent at a time.** Only one process can open the device, so the coordinator hands out access explicitly, and any agent using it runs without parallel hardware users.
  - **Check it's free first** (`hackrf_info`), and release it when done.
  - **Receive only.** Never transmit (C37 stays gated).
  - **Record every capture's settings** (frequency, rate, LNA/VGA/amp, antenna) in its SigMF metadata.
- **Stays interactive with the user (not background agents):** anything needing the Jetson, physical changes to the RF setup (antennas, filters, moving the device), trade-off/decision calls, and the open questions in `docs/planning-log.md`. **Ask before committing.**
