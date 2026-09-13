# C01 · source-abstraction
> Layer A — Acquire · Status: taxonomy frozen 2026-09-13 (resolved in docs/06 §5) · Depends on: — (root; reads C05 correction state, C06 time) · Used by: C02, C03, C04, C05, C37 (and every offline test via file replay)

## Purpose
One driver layer that controls every sample source and delivers timestamped sample blocks. Each block carries a **provenance** record. Sources: HackRF One (later Pro), SoapySDR devices, a soundcard for VLF/ELF, and SigMF file replay. It is the substrate under workflow steps 1–2. Provenance lets later stages say "suspect IMD" instead of logging ghosts, and replay is the basis of offline tests.

## Interface
- **Control (provisional `SourceCommand`):**
  - Tune (Hz); sample rate (2–20 Msps HackRF); baseband filter.
  - LNA 0–40 dB/8 dB steps; VGA 0–62 dB/2 dB steps; amp on/off.
  - Bias-tee (≤50 mA, 3.0–3.3 V); clock source (internal/10 MHz CLKIN); Opera Cake port/mode.
  - Start/stop, sweep-mode handoff to C02; TX gated and off by default (C37).
- **Output (provisional `SampleBlock`):** int8 interleaved I/Q (40 MB/s at 20 Msps), monotonic sample index, host timestamp of the first sample, centre, rate.
- **Provenance (provisional):** gain state, ADC clip count, port/filter, amp/bias-tee, clock source/lock, applied ppm, temperature if available, discontinuity flag, hardware rev/serial, accessory LO offsets (upconverter/LNB).
- **Capabilities descriptor:** range, max rate, bit depth, duplex and features, so the planner can mark what the base device can't do.
- **Replay source:** SigMF, paced or unpaced.

## Methods
- **Drivers:** libhackrf directly (API-documented since 2024.02.1; One/Pro/rad1o). SoapySDR as the generic path for later devices (docs/03 §1.3 "Hardware abstraction").
- **Buffering:** zero-copy block views. Format conversion and memcpy dominate cost at high rates (docs/02 §3.2 "DSP compute: order-of-magnitude feasibility").
- **Clip counting:** count full-scale samples per block. A fraction >1e-4 sets "suspect IM" (docs/04 §10.4 "Dynamic range management").
- **Discontinuity detection:** compare expected with received counts per transfer and flag, never splice. How libhackrf reports drops is **unverified**; spike.
- **Timestamps:** sample index/fs + GNSS-disciplined host time at stream start (C06). The docs give no HackRF One hardware sample timestamps (unverified).
- **Opera Cake `frequency` mode:** filter/antenna switching follows retunes, including in firmware sweep (docs/01 §1.5 "Opera Cake antenna switch").
- **Rate:** run ≥8 Msps and decimate on the host (docs/01 §1.2 "Specifications").

## Platform constraints
- 1 MHz–6 GHz, 2–20 Msps, 8-bit, half-duplex; RX damage risk above −5 dBm (docs/01 §1.2 "Specifications").
- USB 2.0 payload ~35–40 MB/s, so 20 Msps is at the limit and weak hosts drop samples (docs/01 §1.4 "USB 2.0 throughput ceiling").
- No TCXO on One ("±20 ppm" unverified). CLKIN is switched only at RX/TX start (docs/01 §1.2).
- DC spike at centre on One (docs/01 §1.3 "Noise figure, dynamic range, and overload").
- Pro: 16-bit at ≥16× decimation, 4-bit at 40 Msps, TCXO, trigger in/out (docs/01 §1.7 "HackRF Pro (codename "Praline")").
- Jetson Orin Nano Super 8 GB: unified memory; USB 3.2 (docs/02 §3.3 "Platform comparison").

## Prior art and reuse
- **libhackrf / `hackrf_transfer`:** primary driver and raw capture. Active (v2026.01.3); licence: check.
- **SoapySDR:** vendor-neutral API. Slow but alive; licence: check.
- **SDRangel device sets:** a source hosting channel plugins at offsets (docs/03 §2.2).
- **Maia SDR:** Rust REST control plane over the radio (docs/03 §2.4 "Web-based and embedded receivers"). Licence: check.
- **Mayhem C8/C16 + `.TXT` sidecar:** keep the simplicity, use SigMF natively (docs/03 §1.6 "Metadata: SigMF").
- **`tools/fm_rx.py`:** reads `hackrf_transfer -r -` as int8 pairs and uses a 250 kHz offset tune against DC. Its 2.4 Msps is below the recommended 8 Msps.

## Pitfalls
- Silent drops at 20 Msps break timing estimates and pre-trigger alignment.
- A gain change mid-block without provenance invalidates power comparisons.
- Clone "R10C" boards vary (docs/01 §1.1 "Signal chain and major components").
- Temperature readout and `RADIO_CLOCK_CORRECTION` (USB API 1.13) are on `main`, not in a tagged release (docs/01 §1.6). Treat them as optional.
- Unmodelled upconverter/LNB offsets corrupt every absolute frequency.
- TX must be impossible without explicit enablement (docs/04 §1.3 "Legal considerations (US; not legal advice)").

## Testing
- **Replay:** the SigMF source is first-class. Assert blocks, indices and metadata round-trip exactly.
- **Synthetic int8:** tone + noise with injected full-scale runs; assert clip counts and the >1e-4 flag. Break the sample counter; assert the discontinuity flag.
- **Fixtures:** 20 Msps `hackrf_transfer` captures of FM broadcast (88–108 MHz) and ADS-B (1090 MHz), converted to SigMF with provenance.
- **Live:**
  - Sustained 20 Msps soak on the Jetson (target zero discontinuities; duration TBD).
  - Gain/amp/bias-tee/Opera Cake effects.
  - Clock-source switching.

## Example use cases
C01 is substrate; docs/06 §3 lists it only where a use case is specifically about the source layer. Per `use-cases.yaml`:
- SPACE-041 — (source/replay use case)
- PROP-012 — (source use case)
- AWARE-028 — Wi-Fi + BLE + drone combined sweep
- AWARE-056 — Woodpecker history replay
- SIGNAL-022 — (source use case)
- SIGNAL-059 — (source use case)
- SIGNAL-079 — Hearing-aid induction loops

## Open questions
- In-process real-time source, or a capture process with shared memory? Tied to "change pipelines without stopping capture" (CLAUDE.md).
- Are non-SDR sensors (Wi-Fi/BLE scanners, AWARE-028) C01 sources or C29 feeds? docs/06 is silent.
- Who owns the real-time clip-avoidance gain loop: C01, C03 or C05? docs/06 puts the gain table in C05 and the clip count in C01.
- Accessory/antenna configuration isn't a named object in docs/06. Suggest adding it to provenance.
- Spike: libhackrf drop reporting and host-timestamp latency.

## Reading list
1. docs/01 §1.2 "Specifications"
2. docs/01 §1.4 "USB 2.0 throughput ceiling"
3. docs/04 §10.4 "Dynamic range management"
4. docs/01 §1.5 "Opera Cake antenna switch"
5. docs/03 §1.3 "Hardware abstraction"
6. docs/01 §7.3 "HackRF as a front end for Pi 5 / Orin Nano (± FPGA)"
