# Spike S1 — Live pipeline reconfiguration

*(Written by the spike agent; saved to disk by the coordinator because the agent harness blocked the subagent from writing the .md file.)*

**Date:** 2026-09-13
**Machine:** Apple M3 Ultra (28 cores, 256 GB), macOS 15.5 (24F74). No HackRF — synthetic source only.
**Toolchains:** owned — rustc 1.93.1 stable (Homebrew). FutureSDR — rustc 1.100.0-nightly (2026-09-12), which it requires (§4).
**Gates:** [ADR-0001](../../docs/adr/0001-pipeline-runtime.md) · hypothesis and criteria: [docs/09 §S1](../../docs/09-risks-and-spikes.md)
**Status:** throwaway spike code. Raw outputs in `results/` and `results/repeats/`.

## 1. Verdict

| Implementation | Result | One-line reason |
|---|---|---|
| **FutureSDR 0.8.0** | **FAIL** | 2 of 11 cycling runs passed. No in-graph add/remove, so each chain must be its own flowgraph; `rt.start()` makes worst-case attach 3.5–13 ms, above one buffer period (3.28 ms). Flowgraph start/stop also starved the always-on source for 7–10 ms, dropping 64–137 k samples in 4 of 11 runs. Needs a nightly compiler. |
| **Owned mini-dataflow** | **PASS** (capture thread at raised priority) | Worst-case attach 0.03–0.81 ms (≤ 25 % of a buffer period). Ring and every reader lost zero blocks in all 12 runs (800+ attach/detach per 100×8 run). With the capture thread at raised priority, 3/3 runs had zero drops. At default priority, 2/4 runs dropped samples — an oversleep of the harness's paced source thread (7.9 and 10.3 ms), before the ring. |

**Recommendation for ADR-0001:** adopt the **owned Rust dataflow** as the hk-core substrate; do **not** make FutureSDR the core dependency (§5).

## 2. Method

Shared code in `common/`; both implementations include the same files via `#[path]`.
- **Source:** HackRF-format int8 interleaved I/Q at **20 Msps** complex. A 2^22-sample looped table holds 4 FM carriers (−6.0, −1.5, +2.5, +7.0 MHz; deviation 5/5/75/12.5 kHz) plus Gaussian noise; frequencies snapped to the loop grid so the loop is phase-continuous. Paced to wall clock in **65 536-sample blocks — one buffer period = 3.277 ms**, the pass threshold. Every block carries a monotonic `first_sample` counter.
- **HackRF FIFO model** (identical in both): if the source falls more than `FIFO_CAP` = 131 072 samples (one libhackrf transfer, 6.5 ms) behind real time, the excess is **dropped and counted**, like a HackRF overrun. Strict: real libhackrf queues several transfers (not verified).
- **Always-on path:** source → buffer → int8→c32 → FFT 4096 (rustfft) → PSD accumulator, counting every consumed sample. PSD peaks checked against the carriers (−5.996, −1.504, ±2.5 FM sidebands, +6.99/+7.01 MHz) in every run.
- **Chains:** spec A NBFM at −1.5 MHz, decimate 400 → 50 kSps; spec B WBFM at +2.5 MHz, decimate 80 → 250 kSps. Each = xlating decimating FIR (complex band-pass taps, decimate, rotate back) → quadrature FM discriminator → stats sink. Specs alternate each cycle; each chain dwells 50 ms. Demod correctness: measured AC RMS vs deviation/√2 — p50 error 0.2 % (NBFM), 0.05 % (WBFM) in both.
- **Pass (per run), all of:** zero source drops in the measured window; always-on consumed == produced, no counter gaps; every chain received contiguous data (owned: first block equals the ring position bound at attach; FutureSDR: decimator output count == samples sent / decimation); **max** attach latency < 3.277 ms; ≥ 100 attach/detach cycles; no restart/rebuild.
- **Attach latency** = command → chain live and bound to the stream. **First output** = command → sink produced its first demodulated samples.
- **CPU** = process user+sys CPU / wall over the measured window (100 % = one core). **Headroom** = single-thread unpaced stage throughput.
- Each run = one 6–12 s window after 0.5 s warm-up; runs sequential.

## 3. Results

### 3.1 Owned mini-dataflow

| Run | Cycles | Dropped input samples | Ring blocks lost | Attach ms p50 / p99 / **max** | First output ms p50 / max | Detach ms max | CPU % | Result |
|---|---|---|---|---|---|---|---|---|
| 200×1 (run_all) | 200 | **26 787** (1 event: source overslept 7.9 ms) | 0 | 0.019 / 0.040 / **0.045** | 2.36 / 6.41 | 1.51 | 10.1 | FAIL |
| 200×1 r1 | 200 | 0 | 0 | 0.060 / 0.181 / **0.759** | — | — | — | PASS |
| 200×1 r2 | 200 | 0 | 0 | 0.045 / 0.164 / **0.451** | — | — | — | PASS |
| 200×1 r3 | 200 | **73 977** (1 event: source overslept 10.3 ms) | 0 | 0.055 / 0.206 / **0.813** | — | — | 26.3 | FAIL |
| 200×1 `--qos` r1 | 200 | 0 | 0 | 0.042 / 0.156 / **0.347** | — | — | — | PASS |
| 200×1 `--qos` r2 | 200 | 0 | 0 | 0.020 / 0.042 / **0.091** | — | — | 10.1 | PASS |
| 200×1 `--qos` r3 | 200 | 0 | 0 | 0.021 / 0.056 / **0.088** | — | — | — | PASS |
| 100×8 parallel (run_all) | **800** | 0 | 0 | 0.008 / 0.021 / **0.026** | 2.57 / 4.51 | 0.33 | 55.9 | PASS |
| 100×8 r1 / r2 | 800 / 800 | 0 / 0 | 0 / 0 | max **0.035** / **0.169** | — | — | 111.5 (r2) | PASS / PASS |
| 100×1, 32-block pre-roll | 100 | 0 | 0 | 0.020 / 0.035 / **0.047** | **0.195 / 0.365** | 0.28 | 17.3 | PASS |

- "—" = in `results/repeats/*.txt`, not repeated here.
- CPU breakdown (200×1): source thread 0.4 %, FFT path 4.6 %, one chain 5.6 % while attached.
- First output waits for the next block boundary (≤ ~one period + processing). With pre-roll a chain starts from ring history (C03 pre-trigger) and outputs in 0.2 ms.
- Allocation: the ring allocates 257 blocks at startup, then zero; evicted blocks are recycled.

**Headroom** (`--bench`, one thread, unpaced, M3 Ultra):

| Stage | Throughput | vs 20 Msps |
|---|---|---|
| Ring publish (block memcpy) | 35 496 Msps | 1 775× |
| Always-on: int8→c32 + FFT4096 + PSD | 528.5 Msps | **26.4×** |
| Chain NBFM (conv + DDC/400 + FM) | 460.8 Msps | **23.0×** |
| Chain WBFM (conv + DDC/80 + FM) | 493.0 Msps | **24.6×** |

### 3.2 FutureSDR 0.8.0

| Run | Cycles | Dropped input samples | Attach ms p50 / p99 / **max** | ↳ `rt.start` max | First output ms p50 / max | Detach ms max | CPU % | Result |
|---|---|---|---|---|---|---|---|---|
| 200×1 (run_all) | 200 | 0 | 1.31 / 2.02 / **3.09** | 3.02 | 2.85 / 5.50 | 1.33 | 14.9 | PASS (barely) |
| 200×1, 64 KiB chain buffers (run_all) | 200 | 0 | 0.62 / 0.89 / **2.36** | 2.29 | 2.78 / 6.13 | 0.25 | 16.8 | PASS |
| 200×1 r1 | 200 | 0 | 2.82 / 4.14 / **4.90** | 4.77 | 4.90 / 7.92 | 2.06 | 31.9 | FAIL |
| 200×1 r2 | 200 | **67 240** (4 events) | 3.80 / 5.53 / **8.58** | 6.28 | 5.59 / 11.1 | 4.71 | 40.3 | FAIL |
| 100×8 (run_all) | 800 | **136 849** (3 events) | 1.08 / 4.38 / **13.41** | 13.38 | 2.99 / 16.0 | 7.16 | 57.7 | FAIL |
| 100×8 r1 / r2 / r3 | 800 each | 0 / 0 / 0 | max **7.89 / 8.55 / 6.04** | 5.83 (r3) | 3.55 / 8.19 (r3) | 1.95 (r3) | 83.5 (r3) | FAIL ×3 |
| 100×8, 64 KiB r1 | 800 | **64 509** (3 events) | 1.09 / 3.57 / **10.07** | 9.23 | 3.00 / 18.3 | 8.84 | 145.2 | FAIL |
| 100×8, 64 KiB r2 / r3 | 800 each | 0 / 0 | max **3.83 / 3.65** | 3.54 (r3) | 2.94 / 5.92 (r3) | 2.21 (r3) | 149.9 (r3) | FAIL ×2 |

- **Attach breakdown (typical):** build flowgraph 0.02–0.07 ms; `rt.start(fg)` 0.6–3.7 ms p50 (dominant); `Tap.subscribe` message call 0.01–0.04 ms. Smaller chain edge buffers (64 KiB instead of 2 MiB double-mapped) halve `rt.start` but don't bound its tail.
- **Drop diagnosis:** every drop event logged 2 free output blocks at the previous `work()`, so **not backpressure**. The source block wasn't scheduled for 7–10 ms during bursts of flowgraph start/termination on the shared smol executor (28 workers). Always-on consumed == produced in every run, so FutureSDR's buffers themselves lost nothing.
- **Headroom (unpaced):** always-on graph (source int8→c32 + Tap copy + Fft + PSD) 330 Msps = **16.5×** real time on 1.32 cores. Single chain flowgraph (`--bench`: ChannelSource → XlatingFir → Apply(FM) → sink) 850–859 Msps = **42.5×**. futuredsp's FIR is ~1.8× faster than the spike's FIR in `common/dsp.rs` — a kernel-quality difference, not a runtime one.
- The unpaced run shows "FAIL" only because it deliberately does 0 cycles. run_all files predate two diagnostics (drop-log free-space field, owned `--qos` flag); the repeat files include both.

## 4. Findings about FutureSDR 0.8.0

1. **Nightly compiler required.** `futuresdr` 0.8.0 declares `rust-version = 1.95`, but `src/lib.rs` uses `#![feature(return_type_notation, associated_type_defaults, specialization, where_clause_attrs)]`; its own `rust-toolchain.toml` pins `channel = "nightly"`; it fails on stable 1.98 (`E0554`). `specialization` is an incomplete feature — building on it ties the product core to nightly indefinitely. (Also: `rustup` `+toolchain` / `rust-toolchain.toml` shims didn't take effect with Homebrew's cargo on this Mac, so the build sets `RUSTC` explicitly, §7.)
2. **No in-graph mutation.** In 0.8.0 a running flowgraph cannot add/remove blocks or edges. `RunningFlowgraph`/`FlowgraphHandle` expose only `post`/`call` (message ports), `describe`, `stop`, `wait`. `LocalDomainState::add_block/remove_block` exist but are `pub(crate)` and build-time only. The runtime *can* start any number of flowgraphs at runtime on one `Runtime`/`RuntimeHandle` — the mechanism used here: a custom `Tap` block with `subscribe`/`unsubscribe` message handlers (subscriber = `Pmt::Any(mpsc::Sender)`) feeding a per-chain flowgraph `ChannelSource → XlatingFir → Apply → sink`. That works without restarting capture or rebuilding, but it's a fan-out we wrote around the framework, not a feature of it. Every attach pays `rt.start` (block tasks, inboxes, buffer mmaps); chain boundaries are async channels of copied `Box<[Complex32]>` chunks, so no shared zero-copy history and no pre-trigger. The alternative — a pool of pre-started chains retuned via message ports — is parameter change, not topology change, and `XlatingFir` has no retune handler (custom blocks needed).
3. **Maturity/churn.** 0.0.41 (2026-05-18) → 0.6.0, 0.7.0 (both 2026-07-29) → 0.8.0 (2026-08-07): three breaking releases in 10 days. README: *"we do not recommend to add it as a dependency in a separate project but to clone the repository and implement the application as binary, example, or sub-crate."* Licence Apache-2.0 (verified on crates.io).
4. **Genuinely good:** WASM, wgpu (Vulkan/Metal) buffers, Burn ML buffers, ctrl-port REST API, FFT/PFB/xlating-FIR block library, fast futuredsp kernels. **No CUDA/cuFFT path** — GPU support is via wgpu; a cuFFT stage on the Jetson needs a custom buffer/block either way.

## 5. Recommendation for ADR-0001

**Use the owned Rust dataflow for hk-core** (source → ring → FFT/detect → channelizer + attachable chains). Don't take FutureSDR as the core dependency; keep it reachable as a plugin process (ADR-0003) or a source of ideas/kernels.

- **Meets the hard requirement by construction, with margin.** A chain is a cursor on shared history plus a data-described node list, so attach is O(1): worst case 0.03–0.8 ms, zero loss, pre-trigger for free. FutureSDR needs a flowgraph start per chain, its tail exceeds a buffer period at 800 cycles, and flowgraph churn on the shared executor perturbed the always-on source — the one thing that must never be disturbed.
- **Stable Rust, tiny dependency surface.** Owned: 8 crates, all MIT OR Apache-2.0. FutureSDR: nightly + `specialization`, 144 crates, and the project advises vendoring.
- **Less code than it looks.** Ring 233 lines (safe Rust, unit-tested) + dataflow/harness 570 lines, vs 580 lines of FutureSDR glue for the same behaviour.
- **What owned gives up:** a ready block library and WASM portability of the DSP core. The web UI (ADR-0002) is a client of the core, so core WASM isn't needed. Kernels can come from liquid-dsp (MIT, per ADR-0001); futuredsp (Apache-2.0) is worth evaluating (stable-build status unverified).
- **Conditions to carry into hk-core and S2:**
  - The capture thread **must run at raised priority** (macOS QoS user-interactive; `SCHED_FIFO`/`nice` on the Jetson). At default QoS 2/4 runs lost 27–74 k samples to oversleep; the same will hit the real libusb thread.
  - Headroom was measured on an M3 Ultra (26× always-on, 23× per chain, one core). The Orin Nano CPU is many times slower (not measured) — **S2 must re-measure on the Jetson**; the GPU FFT path (cuFFT on a reader thread) exists for exactly that.
  - Steady state: one `Condvar::notify_all` per block (305/s) and one `Arc` clone per reader per block — fine at this block size; revisit if blocks shrink.

## 6. Owned dataflow design (seed for hk-core)

- **`Ring<T>`** (`owned/src/ring.rs`): single-writer, multi-reader array of `Mutex<Option<Arc<Block<T>>>>` slots plus an atomic `head` sequence. The array *is* the RAM history ring (C03). Each `Block` carries `seq`, `first_sample`, `n_samples`, `dropped_before` (discontinuity provenance). The writer fills a recycled block, swaps it into slot `seq % cap`, bumps `head`, notifies — never waits on readers.
- **Readers are just cursors** (a `u64` seq). `ring.reader()` attaches live; `reader_with_history(k)` attaches with k blocks of pre-trigger. A reader behind by more than `cap` gets `Lagged(n)` (exact loss count), never a torn block. Readers hold `Arc`s, so the writer can't overwrite data being processed.
- **Chains** are a runtime-assembled `Vec<Box<dyn Node>>` (here DDC → FM → sink) built from a data spec (`ChainSpec`), each on its own thread with its own cursor. Attach = create cursor + spawn; detach = stop flag + wake + join. Nothing registers with the writer; nothing is rebuilt or restarted. In hk-core, thread-per-chain can become a worker pool and node specs can come from the API or plugin manifests.

## 7. Reproduce

```sh
cd spikes/s1-live-reconfig
rustup toolchain install nightly --profile minimal     # FutureSDR only
./run_all.sh        # builds both (release), runs benches + main scenarios -> results/
./run_repeats.sh    # repeatability runs -> results/repeats/
```

```sh
# owned (stable)
cargo build --release --manifest-path owned/Cargo.toml
owned/target/release/s1-owned --bench
owned/target/release/s1-owned --cycles 200 --dwell-ms 50 [--qos]
owned/target/release/s1-owned --cycles 100 --parallel 8 --dwell-ms 50
owned/target/release/s1-owned --cycles 100 --dwell-ms 50 --preroll 32
(cd owned && cargo test --release)          # ring unit test

# FutureSDR (nightly; RUSTC set explicitly because Homebrew cargo ignores rustup overrides)
N=$HOME/.rustup/toolchains/nightly-aarch64-apple-darwin/bin
RUSTC=$N/rustc $N/cargo build --release --manifest-path fsdr/Cargo.toml
fsdr/target/release/s1-fsdr --bench
fsdr/target/release/s1-fsdr --cycles 200 --dwell-ms 50 [--chain-buf-kib 64]
fsdr/target/release/s1-fsdr --cycles 100 --parallel 8 --dwell-ms 50 [--chain-buf-kib 64]
fsdr/target/release/s1-fsdr --unpaced --cycles 0 --run-ms 3000
```

## 8. Dependency licences

Normal (non-dev) dependency closure from `cargo metadata`, all targets. Nothing GPL; every crate allows permissive use.

**Owned (8 crates):** all `MIT OR Apache-2.0` — rustfft, num-complex, num-integer, num-traits, primal-check, strength_reduce, transpose, libc.

**FutureSDR (144 crates, `default-features = false`)** plus anyhow, libc, num-complex:

| Licence | Crates |
|---|---|
| Apache-2.0 | futuresdr, futuresdr-macros, futuresdr-types, futuredsp, vmcircbuffer |
| MIT OR Apache-2.0 (and equivalents) | 111 crates incl. rustfft, async-executor/io/task (smol), futures, serde, config, toml, wasm-bindgen, web-sys, core_affinity |
| MIT | kanal, slab, tracing, tracing-subscriber, async-tungstenite, bytes, spin, winnow, … (19) |
| Unlicense OR MIT | memchr, aho-corasick |
| Zlib OR Apache-2.0 OR MIT | bytemuck |
| Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | rustix, linux-raw-sys, wasi |
| MIT OR Apache-2.0 OR LGPL-2.1-or-later | r-efi (UEFI only; choose MIT) |
| MPL-2.0 | option-ext (via `dirs`) — file-level copyleft, fine unmodified |
| (MIT OR Apache-2.0) AND Unicode-3.0 | unicode-ident (build-time proc-macro) |

## 9. Caveats

- Synthetic source paced by macOS sleeps; real libusb delivery jitter and HackRF overrun behaviour are not modelled beyond the FIFO rule, and the FIFO size is a strict assumption.
- Runs are 6–12 s each (12 owned, 12 FutureSDR incl. benches/unpaced), not hours. S2 covers long-duration zero-drop on the Jetson.
- Substrate-level comparison. Tap + flowgraph-per-chain is the cleanest mechanism 0.8.0 offers, but a FutureSDR expert might find a lower-latency pattern (pre-started pooled chains with custom retune handlers) — that trades topology change for parameter change and doesn't address the nightly/churn issues.
- Formats differ slightly: the owned ring stores int8 (the C03 product form) and converts per reader; FutureSDR's graph carries Complex32 after the source because its blocks require it.
