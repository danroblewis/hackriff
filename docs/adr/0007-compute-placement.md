# ADR-0007 — Compute placement

**Status:** PROVISIONAL (gated on spike S2 throughput/power)
**Touches:** C07, C11, C15, C16, C38; the sample path; [ADR-0001](0001-pipeline-runtime.md), [ADR-0009](0009-hardware-platform.md)

## Context

The Jetson Orin Nano Super has 6× Cortex-A78AE, a 1024-core Ampere GPU (67 TOPS, unified memory), USB 3.2, and power modes 7/15/25 W plus MAXN ([docs/02 §3.3](../02-sdr-landscape.md); verified 2026: JetPack 6.2, 67 TOPS, MAXN). The HackRF is USB 2.0, 8-bit, 20 Msps. Estimates: a 4096-pt FFT ~10⁵–10⁶ FLOP; at 20 Msps non-overlapping that is ~4.9k FFT/s — a fraction of one CPU core or a sliver of the GPU ([docs/02 §3.2](../02-sdr-landscape.md)). The channelizer (PFB) is the dominant fixed DSP cost.

## Decision (provisional)

| Work | Placement | Why |
|---|---|---|
| USB ingest, ring buffer, timing | CPU (one core, real-time thread) | 40 MB/s; latency-sensitive; unified memory means no copy to hand off to GPU |
| Wideband FFT, persistence/DPX, spectral kurtosis | **GPU** (cuFFT/CuPy) | Cheap on GPU, frees CPU; unified memory avoids PCIe copies |
| Polyphase channelizer | **GPU** | The dominant fixed cost; comfortable on Orin at 20–56 MHz ([docs/02 §3.2](../02-sdr-landscape.md)) |
| Noise floor, CFAR, burst tracking, occupancy | CPU/NEON | Low cost, branchy, control-plane-adjacent |
| Parameter estimation, blind symbol estimation | CPU/NEON | Per-event, cheap; SSCA cyclostationary on demand only |
| Analog/digital demod (own) | CPU/NEON | A few % of a core per channel |
| ML classification, spectrogram object detection | **GPU** (TensorRT INT8/FP16) | Per-event, batched; 10–15× over PyTorch on Orin ([docs/04 §5.5](../04-radio-engineering-and-signals-analysis.md)) |
| Decoders (plugins) | CPU (subprocesses) | Isolation ([ADR-0003](0003-process-plugin-model.md)); each a few % of a core |

- **No usable FPGA.** The HackRF One has only a CPLD; the HackRF Pro adds a tiny iCE40UP5K (5,280 LUTs) that can do DC block / fs4 / CIC decimation but **not** a channelizer or FFT engine ([docs/01 §1.7](../01-hackrf-and-portapack.md)). So all heavy DSP is CPU+GPU. An FPGA front end (RFSoC/M.2 SDR) is a future tier, not this device.
- **Power modes as operating modes:** a **low-power survey mode** (fewer GPU kernels, longer dwells, reduced UI frame rate, ~7–15 W) and a **full mode** (GPU channelizer + ML + decode, 25 W/MAXN). The scheduler ([ADR-0005](0005-survey-dwell-scheduler.md)) picks the mode from workload and battery.

## Consequences

- Unified CPU/GPU memory is the reason the GPU is worth using at these modest rates: no copy tax. Confirmed by spike S2.
- Thermal in a sealed handheld at MAXN is a real risk (spike S6); the low-power mode is the mitigation and the default when idle.
- Keeping detection on CPU and only spawning GPU/ML work per surviving detection matches "classification cost scales with detections, not bandwidth" ([docs/04 §4.4](../04-radio-engineering-and-signals-analysis.md)).
