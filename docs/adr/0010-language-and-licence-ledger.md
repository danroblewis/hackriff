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

## Rust crate and tooling ledger (appended as dependencies are adopted)

Licences come from registry metadata: crates.io API and `cargo metadata` for crates, PyPI JSON
and installed package metadata for Python. Versions are the ones locked when each dependency was
adopted (`Cargo.lock`, `py/uv.lock`). Every new dependency gets a row here before use. Rows stay
append-only (`merge=union` in `.gitattributes`).

| Dependency | Version | Licence | Used by | Placement |
|---|---|---|---|---|
| serde (+ serde_derive) | 1.0.229 | MIT OR Apache-2.0 | hk-model | In-core, permissive |
| serde_json | 1.0.151 | MIT OR Apache-2.0 | hk-model | In-core, permissive |
| thiserror | 2.0.20 | MIT OR Apache-2.0 | hk-model | In-core, permissive |
| uuid (features `v7`, `serde`) | 1.26.1 | Apache-2.0 OR MIT | hk-model | In-core, permissive |
| anyhow | 1.0.104 | MIT OR Apache-2.0 | hk-cli | In-core (binaries), permissive |
| clap (feature `derive`) | 4.6.6 | MIT OR Apache-2.0 | hk-cli | In-core (binaries), permissive |
| *Rust transitive deps of the above* | per `Cargo.lock` | MIT OR Apache-2.0; MIT (strsim, slab, zmij); Unlicense OR MIT (memchr); (MIT OR Apache-2.0) AND Unicode-3.0 (unicode-ident); MIT OR Apache-2.0 OR LGPL-2.1-or-later (r-efi, UEFI-only and not built; taken under MIT/Apache) | — | All permissive; checked with `cargo metadata` at T-001 |
| numpy | 2.5.3 | BSD-3-Clause AND 0BSD AND MIT AND Zlib AND CC0-1.0 | py/ (hkpy, fixture generation) | Python tooling only |
| scipy | 1.18.1 | BSD-3-Clause (PyPI classifier "BSD License"; bundled components carry their own permissive licences) | py/ | Python tooling only |
| pytest | 9.1.1 | MIT | py/ (dev group) | Test tooling only |
| *pytest transitive: iniconfig 2.3.0, pluggy 1.6.0, packaging 26.3, pygments 2.21.0* | per `py/uv.lock` | MIT; MIT; Apache-2.0 OR BSD-2-Clause; BSD-2-Clause | py/ (dev) | Test tooling only |
| hatchling | 1.32.0 | MIT | py/ build backend | Build tooling only |
| GitHub Actions: actions/checkout v5, dtolnay/rust-toolchain, Swatinem/rust-cache v2, astral-sh/setup-uv v6 | — | MIT; MIT OR Apache-2.0; LGPL-3.0 (verify); MIT (verify all) | CI | CI infrastructure only; not linked or distributed |
| rusqlite (feature `bundled`) | 0.40.2 | MIT | hk-model (SQLite repository, ADR-0006) | In-core, permissive |
| libsqlite3-sys (bundled SQLite 3.53.2 amalgamation) | 0.38.2 | MIT (crate); SQLite itself is public domain (the "blessing" header in `sqlite3.c`/`sqlite3.h`) | hk-model via rusqlite | In-core, statically linked, permissive |
| *rusqlite transitive deps (T-002)* | per `Cargo.lock` | Runtime: bitflags 2.13.2, hashlink 0.12.2, hashbrown 0.17.1, smallvec 1.16.1 (MIT OR Apache-2.0); fallible-iterator 0.3.0, fallible-streaming-iterator 0.1.9 (MIT/Apache-2.0); foldhash 0.2.0 (Zlib). Build-only: cc 1.4.6, find-msvc-tools 0.1.12, shlex 2.0.1, pkg-config 0.3.34 (MIT OR Apache-2.0); vcpkg 0.2.15 (MIT/Apache-2.0). wasm32-only, not built for macOS/aarch64: sqlite-wasm-rs 0.5.5, rsqlite-vfs 0.1.1 (MIT), hashbrown 0.16.1 (MIT OR Apache-2.0) | — | All permissive; checked with crates.io API and `cargo metadata` at T-002 |
| num-complex | 0.4.6 | MIT OR Apache-2.0 | hk-core (`Complex32` samples) | In-core, permissive |
| *num-complex transitive: num-traits 0.2.19; autocfg 1.5.1 (build)* | per `Cargo.lock` | MIT OR Apache-2.0; Apache-2.0 OR MIT | hk-core | In-core, permissive; checked with `cargo metadata` at T-003 |
| libc | 0.2.189 | MIT OR Apache-2.0 | hk-core (`rt`: capture-thread priority, macOS QoS / Linux SCHED_FIFO) | In-core, permissive |
| thiserror (added to hk-core) | 2.0.20 | MIT OR Apache-2.0 | hk-core | In-core, permissive (row above covers hk-model) |
| libhackrf (NOT linked; T-003 stub only) | — | `host/libhackrf/src/hackrf.c` header is BSD-3-Clause; `hackrf-tools` (`hackrf_transfer.c`) and firmware are GPL-2.0-or-later; repo `COPYING` is GPLv2 (checked on GitHub `master`, 2026-09-13) | hk-core `HackRfSource` (future) | Not a dependency yet. Treated as GPL until this row is confirmed; reach the device through an isolated `hackrf_transfer` process until then |
| uv (executable) | 0.8.3 (dev Mac) | Apache-2.0 OR MIT (Homebrew formula metadata) | tests/e2e (`hk-e2e` runs `uv run … python -m hkpy.synth` to generate scenarios) | Build/test tooling only; invoked as a subprocess, not linked or distributed |
| readsb (executable, optional) | 3.16.16 | GPL-3.0-or-later (Homebrew formula metadata; github.com/wiedehopf/readsb) | py/tests/test_synth.py reference decoder for the `adsb_squitter` synthetic (skipped when absent) | Test tooling only; subprocess, not linked or distributed. The product use stays the subprocess plugin row above |
| rustfft | 6.4.1 | MIT OR Apache-2.0 | hk-dsp (`fft::CpuFft`: CPU FFT for Welch/STFT/SK) | In-core, permissive |
| *rustfft transitive: strength_reduce 0.2.4, transpose 0.2.3, primal-check 0.3.4, num-integer 0.1.47 (num-complex/num-traits rows above)* | per `Cargo.lock` | MIT OR Apache-2.0 (all four) | hk-dsp | In-core, permissive; checked with `cargo metadata` at T-004 |
| num-complex (added to hk-dsp); serde_json (hk-dsp dev-dependency, tests/bench only) | 0.4.6; 1.0.151 | MIT OR Apache-2.0 | hk-dsp | In-core, permissive (rows above cover hk-core / hk-model) |
| libhackrf (verified 2026-09-13, supersedes the "GPL (verify)" row above) | 0.9.2 (fw 2026.01.3) | **BSD-3-Clause** for `host/libhackrf/src/hackrf.c` + `hackrf.h`; hackrf tools (`hackrf_transfer`, `hackrf_sweep`), CMake files and repo COPYING are GPL-2.0; libusb dependency LGPL-2.1 | future HackRF source in hk-core | In-core linking of libhackrf looks permissible (dynamic libusb); tools used only as external processes. Verified upstream by the T-003 ring reviewer. |
| sha2 | 0.11.0 | MIT OR Apache-2.0 | hk-model (`ContentHash`: provenance dedup, external-event payload pinning) | In-core, permissive |
| *sha2 transitive deps (T-002 review)* | per `Cargo.lock` | digest 0.11.3, block-buffer 0.12.1, crypto-common 0.2.2, hybrid-array 0.4.15, cpufeatures 0.3.1, typenum 1.20.1 (MIT OR Apache-2.0); const-oid 0.10.2 (Apache-2.0 OR MIT); cfg-if and libc already listed | — | All permissive; checked with crates.io API and `cargo metadata` at the T-002 review |
| thiserror (added to hk-store); num-complex, serde_json (hk-store dev-dependencies, tests only) | 2.0.20; 0.4.6; 1.0.151 | MIT OR Apache-2.0 | hk-store (T-017 spectrum-history pyramid). No new external crate: the tile format, CRC-32 and varints are hand-written; Parquet/arrow and zstd were considered and not adopted | In-core, permissive (rows above cover the crates) |
