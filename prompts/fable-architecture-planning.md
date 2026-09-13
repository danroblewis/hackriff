# Brief: hackriff architecture planning (Fable, high effort)

You are the architect for hackriff. `CLAUDE.md` is already loaded. It holds the vision, the product constraints the user decided, the legal guardrails and the doc map. Treat those constraints as settled unless you find a strong reason to challenge one; if you do, raise it explicitly instead of quietly working around it.

This is a **planning** engagement. Don't write product code. Small throwaway spike scripts are fine only after the user approves a specific spike (see Phase 4); put them in `spikes/`.

## How to work

- **Run the phases in order. At each ⏸ checkpoint, stop,** summarize in about 15 lines what you produced and the decisions you need, and wait for the user. The user is a single developer who wants to be involved in trade-offs, not handed a finished tome.
- **Read first.** Before Phase 1, read `CLAUDE.md`, all of `docs/01`–`05`, and `docs/use-cases.yaml`. Don't redo research the docs already cover.
  - When a decision depends on facts the docs don't settle (current Jetson modules, GNU Radio 4 runtime reconfiguration, FutureSDR maturity), verify against primary sources on the web and cite them.
  - Mark anything unverified.
- **Use subagents for mechanical breadth.** Mapping 391 use cases is a good example. Keep the judgment calls yourself: the taxonomy, the data model, the ADRs.
- **Put every conclusion in files.** Update `docs/README.md` as documents are added. Update the Status section of `CLAUDE.md` when phases finish and decisions are made.
- **Commits:** ask before committing. Offer one commit per phase.

## Phase 0: Orientation (short)

Tell the user in a few lines what you understand the product to be, and what you think the three hardest problems are. Ask any question that blocks Phase 1. Don't ask questions that CLAUDE.md already answers.

## Phase 1: Capability map → `docs/06-capability-map.md`

1. **Draft a capability taxonomy** of roughly 25–40 capabilities. They should be real engineering building blocks, for example:
   - wideband sweep survey
   - noise-floor estimation
   - burst detection
   - blind symbol-rate estimation
   - FSK demod to bits
   - protocol framing/CRC discovery
   - inventory and history queries
   - external-feed correlation
   - SigMF recording
   - bitstream output to external consumers
   - direction finding
   - TX link experiments
   - …

   Give each one a definition, inputs and outputs, and a rough compute cost.
2. **⏸ Checkpoint:** present the taxonomy for approval before mass mapping.
3. **Map every use case** in `docs/use-cases.yaml` to capabilities. Fill `capabilities`, plus `hardware_fit`, one of:
   - `native` (HackRF One + Jetson as-is)
   - `needs-accessory` (upconverter, LNB, filter, antenna, GPSDO…; name it)
   - `needs-tx`
   - `needs-other-sdr`
   - `out-of-scope`

   Batch this across subagents with the approved taxonomy, then reconcile the results yourself.
4. **Coverage analysis:**
   - Which capabilities unlock the most use cases (the **shared core**)?
   - How many use cases are `native`?
   - Which capabilities are costly but serve few use cases (candidates to defer)?
5. **⏸ Checkpoint.**

## Phase 2: Core data model → `docs/07-data-model.md`

Define the domain objects everything else hangs off, including:
- Survey/ScanPlan
- Sweep/SpectrumFrame
- Detection
- Emitter/Signal (inventory entry, known vs unknown)
- Recording (SigMF)
- Demodulation
- Bitstream/Decode
- Annotation/Label
- ExternalEvent
- Correlation/Explanation (the attack map)
- Provenance: gain, overload, calibration, spur masks

For each object, cover:
- identity and lifecycle
- relationships
- retention and storage size, on a device with limited disk
- how it is queried for "what has this region looked like over time"
- how tests assert on it

Show two or three worked examples of real use cases flowing through the model: one science, one attack-map, one unknown signal.

**⏸ Checkpoint.**

## Phase 3: Architecture → `docs/08-architecture.md` + `docs/adr/`

Write one ADR per major decision with context, options, trade-offs, a recommendation and consequences. At minimum:
1. **Pipeline runtime and live reconfiguration.** Compare GNU Radio 3.10, GNU Radio 4, FutureSDR, custom Rust or C++ on libraries such as liquid-dsp/VOLK, and hybrids.
   - The hard requirement is changing and adding processing chains **without stopping capture or rebuilding**. Explain exactly where GNU Radio's recompiling comes from (C++ out-of-tree blocks, GRC code generation, flowgraph restarts) and whether each candidate avoids it.
   - Also cover CUDA/GPU use on the Jetson.
2. **UI: web versus native (SDL/ImGui-style)**, the user's most important open question. Consider:
   - the on-device display (size, touch, physical controls)
   - remote viewing from a phone or laptop
   - GPU waterfall performance at 20 Msps
   - battery cost
   - development speed for one person
   - hybrids: headless core with thin clients, local kiosk browser, WASM
3. **Process and plugin model.** How demodulators, estimators and decoders plug in. How existing tools are reused (rtl_433, dump1090/readsb, SatDump, multimon-ng…): in-process, subprocess or IPC. Isolation for crashes and GPL licences.
4. **Bitstream and stream output contract to external programs**: transport (sockets, ZeroMQ, pipes, files), framing, metadata, backpressure.
5. **Survey/dwell scheduler** on a single half-duplex 20 Msps HackRF: probability of intercept, revisit, and priorities from user intent and novelty.
6. **Storage.** Spectrum history compression, inventory database, IQ ring buffer with pre-trigger recording, SigMF archive, disk budget.
7. **Compute placement** across CPU/NEON, the Jetson GPU, and possibly FPGA later; power modes and thermal limits.
8. **Offline-first external context.** Cached reference data (band plans, FCC ULS extracts, TLEs, sigidwiki) and opportunistic feed sync for the attack map.
9. **Hardware platform sketch.** Jetson module choice, display and input, battery and power budget, enclosure size class, and RF accessories (filter bank, LNA/bias-tee, upconverter). This is a sketch to set constraints, not a finished hardware design.
10. **Language/toolchain and dependency-licence ledger.**

**⏸ Checkpoint after ADRs 1–2** (runtime and UI), before building the rest on them. **⏸ Checkpoint again** at the end of the phase.

## Phase 4: Risks and spikes → `docs/09-risks-and-spikes.md`

Rank the assumptions that could sink the design. Examples:
- sustained 20 Msps USB ingest plus a GPU FFT on the Jetson within the power budget
- blind symbol-rate and modulation estimation on real 8-bit HackRF captures
- false-alarm rates in urban overload
- live pipeline reconfiguration in the chosen runtime
- waterfall UI frame rate
- battery life

For each risk, define a spike: hypothesis, setup, pass/fail criterion, estimated effort. **⏸ Checkpoint:** the user picks which spikes to run.

## Phase 5: Test strategy → `docs/10-test-strategy.md`

- **Test tiers:**
  - DSP unit tests
  - component tests
  - end-to-end IQ replay through the full pipeline
  - synthetic scenario generation (known emitters, noise, interference, overload)
  - hardware-in-the-loop with the HackRF
  - field tests
- **Fixtures:**
  - SigMF plus annotations as ground truth
  - candidate public sample sources, with licences checked
  - how to capture your own fixtures
  - fixture size and storage (Git LFS or external)
- **Use-case to test:** fill `test_tier` in `use-cases.yaml`, and describe how a use case ID becomes one or more test cases with assertions on the data model from Phase 2.
- **CI without hardware**, and what can only be verified in the field.

## Phase 6: First vertical slice and roadmap → `docs/11-roadmap.md`

- Pick about 5–10 use case IDs that together exercise the whole chain: survey → detect → estimate → demod → bits → external consumer → inventory/history → explanation. Include at least:
  - one science use case
  - one attack-map use case
  - one unknown-signal use case
  - one known decoder
- Define its acceptance tests.
- Lay out milestones after the slice, ordered by shared-core coverage from Phase 1.

**⏸ Final checkpoint:** a summary of all decisions, open questions, and the recommended next implementation step.
