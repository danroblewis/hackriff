# 08 — Architecture

*Architecture planning, Phase 3. Drafted 2026-09-13. Status: **PROVISIONAL.** Several decisions are gated on Phase 4 spikes; each ADR states what would change it. The two decisions the brief flags as most important — the pipeline runtime and the UI — are [ADR-0001](adr/0001-pipeline-runtime.md) and [ADR-0002](adr/0002-ui-web-vs-native.md).*

This document is the system overview; the reasoning behind each decision is in its ADR under [`adr/`](adr/).

## 1. Shape of the system

hackriff is a **headless real-time core** with an **API-first control plane**, **plugin decoders** at arm's length, and **thin UI clients** (a phone/laptop browser and an on-device kiosk browser). This shape falls directly out of the research: it is the Maia SDR reference (FPGA/GPU FFT → small server → WASM/WebGL UI on a phone, [docs/03 §2.4](03-sdr-software.md)) crossed with SDRangel's headless-plus-REST model and Trunk Recorder's capture-everything pipeline.

```
   HackRF One ──USB2──►  ┌───────────────────────── real-time core (Rust) ─────────────────────────┐
   (+ Opera Cake,        │  source-abstraction ─ RAM ring buffer (pre-trigger)                       │
    filter bank,         │        │                                                                  │
    accessories)         │        ├─ sweep-survey ─┐                                                 │
                         │        └─ dwell window ─┴─ spectral-estimation ─ noise-floor ─ detection  │
   GNSS ────────────────►│                                    │                    │                │
                         │        attention-scheduler ◄───── occupancy/novelty ◄────┘                │
                         │        channelizer ─► DDCs ─► (own demods, in-process)                     │
                         └──────────────┬──────────────────────────────┬──────────────────────────────┘
                          control plane │ (API)          data-plane IPC │ (framed streams)
                                        ▼                                ▼
                    ┌────── stores ──────┐              ┌──── plugin processes (isolated) ────┐
                    │ SQLite state       │              │ rtl_433, readsb/dump1090, dump978,   │
                    │ spectrum pyramid   │              │ multimon-ng, AIS-catcher, dsd-fme,   │
                    │ SigMF files        │              │ SatDump CLI, gr-satellites, GNSS-SDR │
                    │ ExternalEvent cache│              │  (subprocess = crash + GPL isolation)│
                    └─────────┬──────────┘              └──────────────────┬───────────────────┘
                              │                                            │ messages/annotations
                              ▼                    ▼                       ▼
                   context feeds (offline-first) ─ event-correlation ─► inventory / explanations
                              │
                     ┌────────┴─────────  thin clients  ─────────┐
                     │  phone/laptop browser (WASM + WebGL2)      │
                     │  on-device kiosk browser (same app)        │
                     │  external programs (bitstream/API consumers)│
                     └────────────────────────────────────────────┘
```

Two planes:

- **The data plane / sample path** is the latency- and throughput-critical part: USB ingest → ring buffer → FFT/detection/channelizer → demod. It stays in one Rust process on CPU+GPU, never crossing a language or process boundary at sample rate. This is where [ADR-0001](adr/0001-pipeline-runtime.md) (runtime) and [ADR-0007](adr/0007-compute-placement.md) (compute placement) apply.
- **The control plane** is everything else: scheduling policy, the stores, the API, plugins, context feeds, correlation, UI. It runs at human/event rates and can be Python-orchestrated or plugin-isolated without hurting the sample path.

## 2. Why this shape

- **Live reconfiguration without stopping capture** (the hard requirement) is achieved by keeping the always-on core (capture → survey → detect → channelize) separate from the demod/decode chains, which are **data-driven nodes and plugins added or removed at runtime**, never compiled into the core. See [ADR-0001](adr/0001-pipeline-runtime.md) §"where the recompiling comes from".
- **Trust on an 8-bit front end** comes from provenance on every measurement and calibration/spur handling in the core, per the data model ([docs/07 §2.6–2.8](07-data-model.md)).
- **GPL isolation.** Existing decoders and any GPLv3 DSP (VOLK, GNU Radio) sit behind a process boundary, so they cannot force the core's licence. The licence-clean DSP kernel for the core is liquid-dsp (MIT). See [ADR-0003](adr/0003-process-plugin-model.md), [ADR-0010](adr/0010-language-and-licence-ledger.md).
- **One developer.** A web UI over an API-first core is one codebase for on-device and remote, and reuses the huge browser ecosystem instead of a hand-built native GUI. See [ADR-0002](adr/0002-ui-web-vs-native.md).

## 3. Mapping to capabilities and data model

| Architecture element | Capabilities (docs/06) | Data-model objects (docs/07) |
|---|---|---|
| Real-time core sample path | C01, C02, C03, C05, C07–C11 | Sweep/SpectrumFrame, Provenance, Detection, Track |
| Scheduler | C04 | ScanPlan, Survey |
| Characterize/demod (in-process) | C13, C14, C19, C20 | Demodulation, Bitstream |
| Plugin processes | C15 (DL), C22, C23, C36 | Decode/Message |
| Stores | C25, C26, C27, C28 | Recording, SpectrumTile, Emitter, Annotation |
| Context + correlation | C17, C29, C30 | ExternalEvent, Anomaly, Explanation |
| API + stream output | C24 | Bitstream, all objects (read) |
| Thin clients | C39 | (reads everything) |
| GPU inference | C38, C15, C16 | (feeds Emitter classification) |

## 4. ADR index

| ADR | Decision | Status |
|---|---|---|
| [0001](adr/0001-pipeline-runtime.md) | Pipeline runtime & live reconfiguration: Rust core, dynamic graph, plugins; GR4/FutureSDR evaluated | **Provisional** — gated on spikes S1 (live reconfig) & S2 (throughput) |
| [0002](adr/0002-ui-web-vs-native.md) | UI: headless core + web (WASM/WebGL2) thin clients | **Provisional** — user's call; gated on spike S3 (waterfall frame rate) |
| [0003](adr/0003-process-plugin-model.md) | Process & plugin model: subprocess/IPC for existing tools, in-process for own demods | **Provisional** |
| [0004](adr/0004-stream-output-contract.md) | Bitstream/stream output contract: framed messages over UDS/TCP (+ optional ZeroMQ), SigMF headers, backpressure | **Provisional** |
| [0005](adr/0005-survey-dwell-scheduler.md) | Survey/dwell scheduler: POI-aware bandit revisit on one half-duplex window | **Provisional** — core policy, gated on spike S4 |
| [0006](adr/0006-storage.md) | Storage: SQLite + tiled spectrum pyramid + SigMF files + RAM ring buffer | **Provisional** |
| [0007](adr/0007-compute-placement.md) | Compute placement: CPU/NEON control+decode, GPU FFT/channelizer/ML, no usable FPGA on HackRF | **Provisional** |
| [0008](adr/0008-offline-first-context.md) | Offline-first external context: cached reference data + opportunistic sync | **Provisional** |
| [0009](adr/0009-hardware-platform.md) | Hardware sketch: Jetson Orin Nano Super + HackRF One + staged RF accessories | **Provisional** — sets constraints, not a final BOM |
| [0010](adr/0010-language-and-licence-ledger.md) | Language/toolchain (Rust core, Python orchestration, TS/WASM UI) + dependency licence ledger | **Provisional** — project licence still undecided |

See [`adr/README.md`](adr/README.md) for the ADR format and lifecycle.
