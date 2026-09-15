# ADR-0007 — Compute placement and per-platform compute providers

**Status:** PROVISIONAL. The Mac provider numbers are measured (T-041, 2026-09-13). The Jetson placement stays gated on spike S2 (throughput/power) and T-026 (CUDA).
**Touches:** C07, C11, C15, C16, C38; the sample path; [ADR-0001](0001-pipeline-runtime.md), [ADR-0009](0009-hardware-platform.md), [ADR-0010](0010-language-and-licence-ledger.md); T-008, T-026, T-041, T-046

## Context

The Jetson Orin Nano Super has 6× Cortex-A78AE, a 1024-core Ampere GPU (67 TOPS, unified memory), USB 3.2, and power modes 7/15/25 W plus MAXN ([docs/02 §3.3](../02-sdr-landscape.md); verified 2026: JetPack 6.2, 67 TOPS, MAXN). The HackRF is USB 2.0, 8-bit, 20 Msps. A 4096-point FFT is about 10⁵–10⁶ FLOP; at 20 Msps with no overlap that is about 4.9k FFT/s, a fraction of one CPU core or a sliver of the GPU ([docs/02 §3.2](../02-sdr-landscape.md)). The channelizer (PFB) is the dominant fixed DSP cost.

The Jetson is not bought yet. The development machine is an Apple M3 Ultra: 28 CPU cores, a 60-core GPU with Metal 3, and 256 GB of unified memory. hk-dsp already put the compute behind `FftBackend` and `PfbBackend` seams (T-004, T-008), with CUDA stubs behind the `gpu` feature. T-041 adds real providers so GPU paths can be built and verified now, and leaves a clean slot for CUDA.

## Decision (provisional)

### Placement by workload

| Work | Placement | Why |
|---|---|---|
| USB ingest, ring buffer, timing | CPU (one core, real-time thread) | 40 MB/s; latency-sensitive; unified memory means no copy to hand off to the GPU |
| Wideband FFT / STFT rows, persistence, spectral kurtosis | **Selected provider** (below): STFT rows on the GPU (asynchronous readback) where a conformant GPU provider exists, else Accelerate (Mac), else the CPU reference; averaging, SK, holds and persistence stay on the CPU | Measured on the M3: GPU asynchronous ≈ 18–20× real time at N = 1024–16384, against Accelerate 12–14× and CPU 8–12×. The multi-threaded CPU does not help STFT rows (serial accumulation). The Jetson is re-measured in S2 |
| Polyphase channelizer | **GPU** when a conformant GPU provider is present, else CPU multi-threaded | The dominant fixed cost; the GPU scales with channel count ([docs/02 §3.2](../02-sdr-landscape.md)) |
| Noise floor, CFAR, burst tracking, occupancy | CPU/NEON | Low cost, branchy, control-plane-adjacent |
| Parameter estimation, blind symbol estimation | CPU/NEON | Per event and cheap; SSCA cyclostationary analysis on demand only |
| Analog/digital demod (own) | CPU/NEON | A few % of a core per channel |
| ML classification, spectrogram object detection | **GPU** (TensorRT INT8/FP16 on Jetson) | Per event, batched; 10–15× over PyTorch on Orin ([docs/04 §5.5](../04-radio-engineering-and-signals-analysis.md)) |
| Decoders (plugins) | CPU (subprocesses) | Isolation ([ADR-0003](0003-process-plugin-model.md)); each a few % of a core |

- **No usable FPGA.** The HackRF One has only a CPLD. The HackRF Pro adds a tiny iCE40UP5K (5,280 LUTs): enough for DC block, fs/4 shift or CIC decimation, but **not** a channelizer or FFT engine ([docs/01 §1.7](../01-hackrf-and-portapack.md)). So all heavy DSP runs on CPU and GPU. An FPGA front end (RFSoC or an M.2 SDR) is a future tier, not this device.
- **Power modes as operating modes:** a **low-power survey mode** (fewer GPU kernels, longer dwells, reduced UI frame rate, ~7–15 W) and a **full mode** (GPU channelizer + ML + decode, 25 W/MAXN). The scheduler ([ADR-0005](0005-survey-dwell-scheduler.md)) picks the mode from workload and battery. With runtime provider selection, the low-power mode can move the PFB back to `cpu-mt`/`cpu` without a rebuild.

### Provider model

Every heavy kernel sits behind a seam. Each platform has a set of **providers**, chosen at runtime.

| Seam | What it computes | Implementations |
|---|---|---|
| `FftBackend` | one forward FFT (and `forward_batch`) | `CpuFft` (rustfft), `AccelerateFft`, `WgpuFft`, `gpu::CudaFft` (stub) |
| `compute::SpectralBackend` (new) | STFT rows: window → FFT → DC-centred \|X\|² for hop-spaced segments of a span, delivered **in submission order**, synchronously or asynchronously | `CpuSpectral`, `CpuMtSpectral`, `AccelerateSpectral`, `WgpuSpectral` |
| `PfbBackend` (+ `flush`, new, default no-op) | the 2× oversampled PFB | `Pfb` (CPU reference, unchanged), `channelizer::batch::BatchPfb` over a `PfbExecutor` |
| `channelizer::batch::PfbExecutor` (new) | one block's frames from a planned job (staged window, per-frame fold slot and raster rotator) | `SerialExecutor`, `MtExecutor`, `WgpuPfbExecutor`; **CUDA plugs in here** (T-026) |

`StftProcessor` now stages samples, submits every complete segment as one batch, and replays an ordered queue of input events (flags, drops, provenance, resets) as rows arrive. `BatchPfb` likewise owns everything that defines the output: stream tracking, resets, the time map and the exact `u64` raster NCO. So frames, timestamps and discontinuity flags come from the same code whichever provider computes the numbers. An asynchronous provider only delays *when* output appears. It then shows up on a later `push`/`process_*` call with its own header, and `flush` drains the rest.

| Provider | Platforms | Cargo feature | FFT | STFT rows | PFB | Conformant |
|---|---|---|---|---|---|---|
| `cpu`: CPU reference | all | always | ✓ | ✓ | ✓ | ✓ (the reference) |
| `cpu-mt`: CPU multi-threaded (rayon) | all | `cpu-mt` (**default on**, so CI runs it) | — | ✓ bit-identical | ✓ bit-identical | ✓ |
| `accelerate`: Apple vDSP | macOS | `accelerate` | ✓ (f·2ⁿ sizes) | ✓ (f·2ⁿ sizes) | — | ✓ |
| `gpu`: wgpu compute | macOS (Metal); Linux/Jetson (Vulkan, **unverified on Orin**) | `gpu-wgpu` | ✓ any size | ✓ any size | ✓ any even M | ✓ (Metal, M3 Ultra) |
| `cuda`: CUDA / cuFFT | Jetson | `gpu` | stub | — | stub | ✗ (refused until T-026 passes the suite) |

### Selection rules

- **API.** `hk_dsp::compute::Compute::new(ComputeOptions)` returns processors plus a `Selection {workload, requested, provider, fallback}`: `.stft(StftConfig)`, `.spectral(&StftConfig)`, `.pfb(PfbConfig)`, `.fft(len)`, and `.status()` for every provider.
- **Options.** `ComputeOptions` is a serde data spec: `{"provider": "auto|cpu|cpu-mt|accelerate|gpu|cuda", "stft": …, "pfb": …, "threads": n, "gpu_in_flight": 2}`. The environment overrides it: `HK_COMPUTE`, `HK_COMPUTE_STFT`, `HK_COMPUTE_PFB`, `HK_COMPUTE_THREADS`, `HK_GPU_IN_FLIGHT`. Selection happens at runtime from config or env; a cargo feature only decides what is compiled in.
- **A provider is used only if** it is compiled in, it is **marked conformant** (`ProviderKind::conformant()`), it is usable now (device present; for the GPU, a numeric self-check against rustfft at first use, on both the power-of-two and Bluestein paths), and it supports the size (vDSP refuses non-f·2ⁿ lengths).
- **Fallback** order: requested → `cpu-mt` → `cpu`. Every refused candidate adds its reason to `Selection::fallback`. An explicit request that falls back is logged to stderr, e.g. `Pfb: requested cuda, using cpu-mt (cuda: …)`. Invalid configs are errors, never fallbacks.
- **`auto`** (M3 Ultra measurements below): STFT rows → `gpu` → `accelerate` → `cpu`; PFB → `gpu` → `cpu-mt` → `cpu`; single FFT → `cpu`. A named provider falls back through the same per-workload chain. `cpu-mt` is available by name for STFT but is not in its chain. S2 re-measures `auto` on the Jetson.

### Parity requirements: the conformance suite (T-046)

`hk_dsp::conformance::{fft_suite, spectral_suite, pfb_suite}` run the same checks against every provider through the seams. Tolerances are defined once (`conformance::TOLERANCES`) as parity with the CPU reference:

| Tolerance | Value |
|---|---|
| complex outputs (FFT bins, PFB samples): max \|got − ref\| / max(\|ref\|, rms ref) | ≤ 1e-4 |
| power (PSD, max/min hold): max per-bin dB error, reference floored at −30 dB below the frame's mean PSD | ≤ 0.01 dB |
| spectral kurtosis | ≤ 1e-3 absolute |
| tone and channel-centre gain / noise level (absolute) | ≤ 0.05 dB / ≤ 0.3 dB |
| adjacent-channel leakage of a channel-centre tone | ≤ −58 dB (60 dB design) |
| PFB phase step deviation across blocks | ≤ 1e-3 rad |
| Parseval | ≤ 1e-5 relative |

| Suite | Checks |
|---|---|
| FFT | `fft_tones_at_known_bins` (bins 0, 1, N/3, N/2 = Nyquist, N−1), `fft_parseval_noise`, `fft_parity_random` (N 256–16384), `fft_odd_and_unsupported_sizes` (N 4…3000: parity, or a clean `Err`), `fft_batch_equivalence`, `fft_determinism` |
| Spectral (via `StftProcessor`) | `spectral_window_and_overlap` (Hann 50/75 %, Blackman-Harris 0 %, flat-top 1000/700), `spectral_tone_and_noise_calibration`, `spectral_edge_bins`, `spectral_stream_semantics` (gap, drop, retune, gain, rate change, ci8: metadata identical), `spectral_batch_vs_single_segment`, `spectral_determinism` |
| PFB | `pfb_parity_reference` (M 64, 256 subset, 100 + raster, 800 + raster subset), `pfb_channel_centre_and_gain`, `pfb_adjacent_channel_rejection`, `pfb_phase_continuity_across_blocks`, `pfb_retune_and_reset`, `pfb_block_chopping_invariance`, `pfb_determinism`, `pfb_invalid_config_refused` |

Test binaries:
- `tests/conformance_cpu.rs` covers `cpu`, `cpu-pfb-batch` and `cpu-mt`, and runs in `just test`.
- `tests/conformance_accelerate.rs` needs `accelerate` on macOS.
- `tests/conformance_gpu.rs` needs `gpu-wgpu` and skips with a logged reason when no adapter exists. It runs the spectral and PFB suites both synchronously (in-flight 0) and asynchronously (in-flight 2).
- `tests/stft_refactor_parity.rs` proves the restructured STFT is bit-identical to the pre-T-041 loop (CPU, CPU-MT and a delayed asynchronous provider).

**Adding a provider** (CUDA, T-026, or a future SDR's DSP):
1. Implement a seam (`PfbExecutor` for the PFB; `SpectralBackend`; `FftBackend`).
2. Add a conformance test binary that calls the three suites.
3. Pass it on the target hardware.
4. Only then flip `ProviderKind::conformant()`. Selection refuses non-conformant providers.

Changing a tolerance changes this ADR.

**Results, 2026-09-13, M3 Ultra:**
- Every provider passes every check it implements. `cpu`, `cpu-mt` and the serial batch PFB are bitwise equal to the reference.
- Accelerate: FFT parity ≤ 3.8e-6; spectral ≤ 9.2e-4 dB, SK ≤ 1.4e-4. It refuses N = 5…3000 non-f·2ⁿ lengths cleanly.
- GPU (Metal): FFT parity ≤ 1.9e-6, any length through Bluestein ≤ 6.2e-7; spectral ≤ 6.6e-4 dB, SK ≤ 1.2e-4. PFB samples ≤ 1.5e-5. Gain +0.0000 dB, adjacent channels −61.3 dB, phase step error 1.5e-7 rad. Output is identical after retune/reset and bitwise identical across chopping and runs.

### GPU provider choice: wgpu over native Metal

| | wgpu compute (chosen) | Native Metal (objc2-metal / metal-rs) |
|---|---|---|
| Jetson portability | Same WGSL kernels on Vulkan (JetPack ships a Vulkan 1.3 driver; **unverified on Orin**), so one GPU provider for both platforms | Mac only; the Jetson would need a second GPU provider (CUDA) anyway |
| Throughput | Dispatch-batched kernels (one pass per FFT stage over every segment or frame); ~16 passes per batch; wgpu adds validation and tracking per pass | Lower per-pass overhead, and shared-storage buffers with no staging copy. Not prototyped (timebox); the gain is bounded by the per-batch share of overhead in the table below |
| Transfer | Own mappable upload and readback buffers per slot (no per-batch buffer creation), `copy_buffer_to_buffer` in the same command buffer; asynchronous `map_async` readback polled without blocking | Zero-copy on unified memory |
| Latency | One block of in-flight lag at `gpu_in_flight ≥ 1`; synchronous mode available | Same design choices |
| Maintenance | Pure Rust, stable API (breaking releases roughly quarterly); WGSL is readable; errors caught at pipeline creation | objc2 bindings plus MSL; Apple-only code path to keep working |
| Licence | MIT/Apache-2.0 (tree all permissive, ADR-0010) | Zlib/Apache/MIT |

wgpu gives one conformant GPU provider for the Mac today and a Vulkan path for the Jetson, at the cost of some per-pass overhead. CUDA stays the Jetson's performance option if S2 shows the Vulkan path is short of headroom (T-026).

Kernel design: a batched radix-2 FFT with Bluestein for non-power-of-two sizes (the PFB rasters are M = 800/1600 at 20 Msps). The STFT windowing and the PFB polyphase fold run on the GPU. Tables are computed in f64 on the CPU. A slot pool keeps buffers and bind groups; a slot grows only when a larger batch arrives. Readback is an in-flight FIFO bounded by `gpu_in_flight`. wgpu itself allocates about 100 host allocations per block (command recording, map callbacks: `tests/provider_alloc.rs`). Our code creates no buffers and grows no vectors in steady state. The CPU batch planner allocates nothing per block. rayon adds under one allocation per block.

### Apple Accelerate outcome

Implemented behind `accelerate` (macOS; vDSP DFT through `extern "C"`, no crate; ~150 lines) and conformant. It is **the CPU choice for STFT rows on the Mac**: 12–14× real time against 8–12× for rustfft (median; N = 1024 / 4096 / 16384 → 14.4× / 13.6× / 12.3× against 11.6× / 8.9× / 7.9×, ~1.3–1.6×).

It is second in the `auto` STFT chain, after the GPU:
- The asynchronous GPU is faster still (18–20×) and leaves the CPU free.
- vDSP only takes f·2ⁿ lengths (f ∈ {1, 3, 5, 15}), so it cannot run the PFB rasters (M = 800/1600).
- It does not exist on the Jetson.

It provides no PFB.

### Measured numbers (M3 Ultra, 20 Msps ci8, 65 536-sample blocks)

`cargo bench -p hk-dsp --features gpu-wgpu,accelerate --bench compute_providers`, 5 repeats × 1.5 s per row. Real-time factor = input Msps / 20; values are **min / median**. Other agents were building on the machine throughout (1-minute load average 19–35 on 28 cores), so CPU rows are pessimistic, and the multi-threaded CPU in particular had no idle cores to use. Rows marked † are from the second run (load 23–33), taken after the rayon grouping was coarsened.

Before T-041 the CPU path was single-threaded: STFT 4096/50 % at 8.9× and PFB M=800 at 4.5× (one core). Multi-threading was added as `cpu-mt`.

**STFT** (Hann, 50 % overlap, K = 16, SK + max/min hold):

| N | cpu | cpu-mt | accelerate | gpu, in-flight 0 | gpu, in-flight 2 | load |
|---|---|---|---|---|---|---|
| 1024 | 11.4 / 11.6× (0.28 ms/blk) | 6.5 / 6.8× | 14.4 / 14.4× | 7.0 / 7.3× (0.45 ms/blk) | 17.1 / 20.3× (0.16 ms/blk) | 23–27 |
| 4096 | 8.9 / 8.9×; † 9.1 / 9.1× | 10.3 / 10.6×; † 9.3 / 9.4× | 13.4 / 13.6× | 6.5 / 7.0× | 17.1 / 19.9× | 19–33 |
| 16384 | 7.8 / 7.9× | 7.5 / 7.6× | 12.1 / 12.3× | 6.4 / 6.4× | 17.8 / 17.9× | 20–22 |

**PFB** (all channels materialised, 60 dB prototype):

| M (L taps) | cpu (reference) | cpu-mt † | gpu, in-flight 0 † | gpu, in-flight 2 † | load |
|---|---|---|---|---|---|
| 800 (5803): T-008 25 kHz raster | 3.6 / 4.5×; † 4.5 / 4.5× (0.73 ms/blk) | 3.3 / 3.5× | 3.1 / 3.5× (0.94 ms/blk) | **14.2 / 14.4×** (0.23 ms/blk) | 20–31 |
| 1600 (11603): 12.5 kHz | 2.2 / 2.5×; † 3.6 / 4.2× | 2.1 / 4.3× | 2.1 / 2.6× | **8.0 / 13.9×** | 23–35 |
| 512 (3715): power of two | 8.3 / 8.3× | 8.7 / 9.8× | 5.9 / 7.2× | **14.8 / 19.5×** | 25–31 |

**Reading the table:**
- **Asynchronous readback is what makes the GPU pay.** In synchronous mode (in-flight 0), every block waits for its own batch, about 0.45–0.95 ms of dispatch, wait and readback. That is slower than the CPU at these sizes. With 2 batches in flight, the ring-reader thread stages and accumulates while the GPU works: 14–20× across every size, with one to two blocks (3–7 ms at 20 Msps) of added output latency.
- **The GPU PFB is the only provider with real headroom at the 12.5/25 kHz rasters:** 14× at M = 800 and 1600, against 2.5–4.5× on one core.
- **`cpu-mt` is inconclusive on this loaded machine** (≤ 1.1× over one core). The STFT's per-segment accumulation is serial, so `cpu-mt` is kept out of the STFT chain. It stays in the PFB fallback chain, for machines without a GPU, pending a quiet-machine and Jetson (S2) measurement.
- **Per-pass overhead.** wgpu's per-pass cost sets the synchronous floor. Native Metal could lower that floor, but in asynchronous mode the GPU is no longer the bottleneck at 20 Msps, so native Metal was not pursued.

## Consequences

- Unified CPU/GPU memory is the reason the GPU is worth using at these modest rates: no copy tax. On the M3 the wgpu path still pays a staging copy and per-pass overhead, which is why STFT rows stay on the CPU there. Spike S2 confirms or overturns this on the Orin, where the CPU is ~4× weaker and the balance may flip.
- Thermal in a sealed handheld at MAXN is a real risk (spike S6). The low-power mode is the mitigation and the default when idle. Runtime selection lets that mode move work between providers without a rebuild.
- Keeping detection on CPU and only spawning GPU/ML work per surviving detection matches "classification cost scales with detections, not bandwidth" ([docs/04 §4.4](../04-radio-engineering-and-signals-analysis.md)).
- **Pipeline hookup (T-056, `hk_pipeline::compute`).**
  - **Options.** `PipelineSettings::compute: ComputeOptions` comes from `ScanPlan::extra.pipeline.compute` or `--compute auto|cpu|cpu-mt|accelerate|gpu` on `hk replay/run/serve` and `hackriffd`. `HK_COMPUTE*` wins over both.
  - **One registry per run.** `Pipeline::start` applies `.with_env()` and builds one `Compute`, shared by every segment including re-plumbs. The provider cannot change mid-run; `provider_changes` counts any change and should stay 0.
  - **Call sites.** The detection, history and spectrum readers build their STFTs through `compute.stft`. Their selections, fallback reasons and provider availability are reported under `compute` in the run summary and `/api/status` (docs/api.md).
  - **No PFB.** No pipeline stage runs a channelizer (the chains use DDCs), so none is selected yet.
  - **Default build** (no `gpu-wgpu`, no `accelerate`): `auto` resolves to `cpu`, bit-identical to before. A replay with `provider: cpu` and one with `auto` store identical detections (`tests/compute_providers.rs`).
  - **Asynchronous latency.** With `gpu-wgpu`, frames can arrive up to `gpu_in_flight` ring chunks late. Every reader keeps that safe:
    - it flushes at `Closed` (stream end, stop, or segment detach), before its stage finishes;
    - it flushes when a 50 ms read finds nothing new and rows are in flight;
    - the spectrum reader flushes under the old row plan before a display or rate rebuild.

    Downstream stages key on each frame's own sample index and time, and the ring holds seconds of pre-trigger reach, so detection, tracking and history timing are unchanged.
- The CUDA slot is explicit: `ProviderKind::Cuda`, `Preference::Cuda` and the `PfbExecutor` seam exist. T-026 implements an executor, passes `pfb_suite` on the Orin, and flips `conformant()`.
- `cpu-mt` (rayon) is a default dependency of hk-dsp. The GPU and Accelerate code never builds in CI (`gpu-wgpu`, `accelerate` off), and their conformance binaries compile to empty test sets there.
