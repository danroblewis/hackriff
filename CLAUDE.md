# hackriff

An exploration-first signals-analysis tool for software defined radio. It's meant to replace the HackRF PortaPack: a portable device plus software that **finds** interesting signals instead of making you already know the frequency and pick FM/AM/WFM by hand. It serves science (space weather, radio noise, propagation), spectrum awareness, and decoding unknown signals, as well as hobby listening.

## Status

**Phase: architecture planning.** There is no product code yet. Research is finished and lives in `docs/`. The next deliverables are a capability map, a data model, architecture decisions, risks and spikes, a test strategy, and a first vertical slice. The planning brief is `prompts/fable-architecture-planning.md`. Read the docs before proposing designs; don't re-research what they already cover.

`tools/` holds the user's quick HackRF experiments, not product code:
- `sweep_plot.py` plots `hackrf_sweep` CSV output and lists peaks.
- `fm_rx.py` is a numpy/scipy broadcast-FM receiver fed by `hackrf_transfer`.

| Doc | Use it for |
|---|---|
| `docs/README.md` | Index and the main findings across documents |
| `docs/01-hackrf-and-portapack.md` | What HackRF/Mayhem can and can't do; what to keep from them |
| `docs/02-sdr-landscape.md` | Front-end physics, hardware comparison, compute throughput math, hardware tiers |
| `docs/03-sdr-software.md` | Existing tools to reuse or learn from (SDRangel, SigDigger, URH, rtl_433, Trunk Recorder, SatDump, IQEngine, Maia SDR, GNU Radio 4, FutureSDR, TorchSig); gap table |
| `docs/04-radio-engineering-and-signals-analysis.md` | Algorithms: noise floor, CFAR, spectral kurtosis, parameter estimation, AMC, auto-demod/squelch, protocol ID, trunking, DF, calibration. §12 is a prioritized implementation list. §1.3 covers legality. |
| `docs/05-use-cases-and-explorations.md` | **The feature goals.** 391 use cases with permanent IDs (`SPACE-`, `PROP-`, `AWARE-`, `SIGNAL-`, `RESEARCH-`). These become the test suite. |
| `docs/use-cases.yaml` | Machine-readable copy of docs/05 and the source of truth for IDs. Planning adds `capabilities`, `hardware_fit` and `test_tier`. |

## Product vision (from the user)

The core workflow, in order:
1. **Peruse.** Browse live airwaves casually, or open reports of past scan surveys.
2. **Automate.** Run configurable, thorough scans across frequency regions on a schedule.
3. **Review history.** Choose a region and see what activity was seen there over time.
4. **Cross-reference known signals.** Band plans, licences, signal databases. This exists *to separate known from unknown*; it is not the goal.
5. **Record and stream demodulations.** Parameters (modulation, bandwidth, squelch, AGC) are estimated from the signal, never picked manually.
6. **Decode to bitstreams.** Blind symbol and bit recovery, including for unknown signals.
7. **Stream bits to other programs**, which turn them into something useful. Decoders are pluggable consumers, not hard-coded apps.

Further goals:
- **A radio "attack map".** Like internet port-scan and botnet maps: explain *why* your spectrum changed by linking local anomalies to external events such as space weather, GNSS jamming, satellite passes, balloon launches and lightning. See docs/05 §3.
- **Science is first-class.** Solar flares, noise-floor studies and propagation, down to what a radio engineer knows but rarely watches.
- **General, not a decoder catalogue.** ADS-B, pagers and radiosondes are the well-known cases, not the point.

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
- **Licence: undecided.** Don't commit yet. Track the licence of every dependency: GPL components such as GNU Radio constrain later choices, and process boundaries can isolate them.
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
- Prefer **SigMF** recordings plus annotations as fixtures. Candidate sources: your own captures, IQEngine/SigMF archives, sigidwiki samples, and synthetic data (TorchSig / generated). Check the licence of each sample set before committing it.
- Build end-to-end tests that replay IQ through the full pipeline and assert on detections, estimated parameters, decoded bits and inventory entries. Keep unit tests for the DSP blocks.

## Legal guardrails

- Receive-only by default. TX features must be explicit and assume a licence or rule authority, or your own devices.
- Users may record and **decrypt their own traffic** for any research purpose.
- **Never** build features that circumvent the security of other people's traffic. For others' encrypted signals, detect and label metadata only.
- US law also restricts some *unencrypted* content: cellular, common-carrier paging, and divulging content under 47 USC 605. See docs/04 §1.3.
- Security-research features cover finding issues and testing your own devices, not attack tooling.

## Working conventions

- Docs are numbered markdown files in `docs/` with inline source links and a Sources section. Mark anything unverified. Keep `docs/README.md` indexed. Architecture decisions go in `docs/adr/NNNN-title.md`.
- Use-case IDs are permanent. Never renumber; append new ones. Keep docs/05 and `use-cases.yaml` in sync.
- Don't recommend SDR#/GQRX-style tune-and-listen tools as answers; the user wants exploration and analysis tooling.
- The user runs the `md` doc viewer and cloudflared tunnel themselves. Don't start, restart or kill those processes.
- Ask before committing.
