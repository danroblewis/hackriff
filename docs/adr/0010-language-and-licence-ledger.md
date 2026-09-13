# ADR-0010 — Language, toolchain, and dependency licence ledger

**Status:** PROVISIONAL (the project's own licence stays undecided per CLAUDE.md; this ADR keeps options open and tracks what constrains them)
**Touches:** all components; [ADR-0001](0001-pipeline-runtime.md), [ADR-0003](0003-process-plugin-model.md)

## Context

CLAUDE.md: no language preference, choose robust support; the real-time path must be fast so **Python is orchestration/research only**; the project licence is **undecided**, and GPL components (GNU Radio) constrain later choices, so track every dependency's licence and use process boundaries to isolate GPL.

## Decision (provisional)

**Languages by layer:**

| Layer | Language | Rationale |
|---|---|---|
| Real-time core (sample path, control plane, API server) | **Rust** | No GC pauses, memory safety for ring-buffer/timing code, strong FFI to liquid-dsp/CUDA, first-class WASM for the UI, and the FutureSDR option ([ADR-0001](0001-pipeline-runtime.md)). |
| DSP kernels | **liquid-dsp (C, MIT)** via FFI + **cuFFT/CUDA** on GPU | Licence-clean, embedded-friendly ([docs/03 §1.4](../03-sdr-software.md)). |
| Orchestration, research, test tooling, fixture/synthetic generation | **Python** | Numpy/scipy/CuPy/TorchSig/SigMF ecosystem; never on the sample path (CLAUDE.md). |
| UI | **TypeScript + WASM (WebGL2)** | [ADR-0002](0002-ui-web-vs-native.md). |
| Plugins | Native binaries wrapped as subprocesses; own demods in Rust | [ADR-0003](0003-process-plugin-model.md). |

**Build/toolchain:** a **Cargo workspace** for the Rust core; a Python env via **uv** (or poetry) for tooling; the UI built with a standard TS bundler. Develop on macOS; **cross-compile or remote-build for the Jetson (aarch64, JetPack 6.2)** — decided concretely in [docs/12](../12-implementation-plan.md). CUDA code builds on-device or via the JetPack cross toolchain.

**Dependency licence ledger** (the point of this ADR — GPL isolation is a design input, not an afterthought). Licences marked **(verify)** are confirmed before the dependency is adopted:

| Dependency | Role | Licence | Placement / constraint |
|---|---|---|---|
| liquid-dsp | Core DSP kernels | MIT | **In-core** — licence-clean |
| VOLK | SIMD kernels | **GPLv3** | **Plugin/subprocess only**, never in a non-GPL core |
| GNU Radio 3.10 / GR4-ported blocks | Optional decode chains | **GPLv3** (GR4 core MIT) | **Subprocess plugin only** ([ADR-0003](0003-process-plugin-model.md)) |
| FutureSDR | Candidate runtime | Apache-2.0 (verify) | In-core if adopted — permissive |
| SoapySDR | Device abstraction (later SDRs) | Boost (verify) | In-core — permissive |
| libhackrf / hackrf tools | HackRF driver | GPL (verify GPLv2 scope of libhackrf) | If libhackrf linking is GPL, reach it via a thin isolated process or confirm the linking exception before in-core use |
| cuFFT / CUDA / TensorRT | GPU FFT/inference | NVIDIA proprietary EULA | In-core (binary redistribution terms apply; check for shipped images) |
| CuPy | GPU array (tooling) | MIT | Python tooling |
| SigMF / sigmf-python | Recording+metadata | spec CC-BY-SA; code Apache/LGPL (verify) | Format everywhere; code in tooling |
| rtl_433 | ISM decoder | GPLv2+ | Subprocess plugin |
| readsb / dump1090 / dump978 | ADS-B/UAT | GPL/BSD mix (verify per tool) | Subprocess plugin |
| multimon-ng | POCSAG/FLEX/etc | GPLv2 | Subprocess plugin |
| AIS-catcher | AIS | (verify GPL/MIT) | Subprocess plugin |
| dsd-fme | Digital voice | GPLv2 | Subprocess plugin; **vocoder (AMBE/IMBE) IP is separate** — licensing reviewed before trunking voice ships ([docs/04 §8.4](../04-radio-engineering-and-signals-analysis.md)) |
| SatDump | Satellite pipelines | GPLv3 | Subprocess plugin |
| gr-satellites | Sat telemetry | GPLv3 | Subprocess plugin |
| GNSS-SDR / galmon | GNSS observables | GPLv3 (verify) | Subprocess plugin (licences flagged unchecked in docs/06 §5) |
| TorchSig | Synthetic data/models | MIT | Python tooling / GPU plugin |

## Consequences

- **The core can stay under a permissive or a copyleft licence — the choice is not forced by dependencies** because every GPL component is behind the [ADR-0003](0003-process-plugin-model.md) process boundary and the in-core DSP is MIT/permissive. This directly preserves the "licence undecided" position in CLAUDE.md.
- The `(verify)` rows are checked at adoption and the ledger is kept current in this ADR; a dependency whose licence would bind the core is either isolated or replaced.
- Vocoder IP (DVSI AMBE/IMBE) and per-feed data ToS (RadioReference, some map/feed sources) are tracked as separate, non-code licence constraints.
- Python staying off the sample path is enforced by the language split, not convention.
