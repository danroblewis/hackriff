# C07 · spectral-estimation
> Layer B — Sense · Status: taxonomy frozen 2026-09-13 (resolved in docs/06 §5) · Depends on: C01, C03 (C05 for dBm/spur context) · Used by: C08, C09, C26, C33, C38, C39

## Purpose
Turns the live dwell IQ window into power spectra that everything downstream shares: Welch PSD rows, multi-resolution STFT, a DPX-style persistence histogram and per-bin spectral-kurtosis (SK) accumulators. It is substrate for workflow step 1 (peruse: waterfall/persistence), and the input to automatic detection (steps 2–3) and science radiometry. Averaging hides bursts; persistence and SK surface them. The decimated narrowband **"zoom" stream (the ~25 Hz-bin / ~800k-point case) is owned by C11** (channelizer); C07 FFTs the decimated stream C11 delivers and does not itself decimate to 800k-point FFTs (docs/06 §5).

## Interface
- **In:** timestamped IQ blocks + provenance (gain, clip count, filter, centre, rate) from C03; typically 20 Msps, 8-bit, ~15–18 MHz usable.
- **Out (provisional `SpectrumFrame`):** `t_start/t_end`, `f_centre`, `bin_hz`, `rbw_hz`, `window`, `n_avg`, `psd[]` (float32, dBFS/Hz or dBm/Hz once C05 calibrated), `max_hold[]`, `sk[]` + `M`, provenance copy, `edge_mask`/`dc_mask`.
- **Out (provisional `PersistenceImage`):** 2-D histogram H[f, P_dB] with decay β.
- **Config:** FFT size per resolution tier, overlap (default 50% Hann), averages K, SK block M, persistence β and dB grid, output row rate per consumer (detector vs display vs history).
- **Rates (derived):** 4096-pt FFT at 20 Msps = 4.88 kHz bins, 205 µs frames, ~4.9k FFT/s (~9.8k at 50% overlap); raw rows ~78 MB/s, so downsample to ~30 lines/s (~0.5 MB/s) for display/history.

## Methods
- **Welch PSD** (default): Hann, 50% overlap, variance ∝ 1/K; RBW = ENBW·fs/N, Hann ENBW = 1.5 bins (+1.76 dB density correction). `docs/04 §3.1 "PSD estimation"`, `docs/04 §10.2 "Power calibration: dBFS to dBm"`.
- **Two resolutions in parallel:** Δf·Δt ≈ 1. Pick Δt below the shortest burst of interest (1 ms burst → ≥1 kHz bins). One tier is burst-oriented (~1 kHz) and one carrier-oriented (~25 Hz). `docs/04 §3.5 "Time–frequency analysis"`.
- **Spectral kurtosis:** keep S1 = ΣP and S2 = ΣP² per bin over M spectra; SK = (M+1)/(M−1)·(M·S2/S1² − 1). ≈1 means noise, <1 means CW/constant-envelope, >1 means pulsed/intermittent. The cost is one extra accumulator per bin. `docs/04 §3.1 "PSD estimation"`.
- **Persistence:** H ← βH + hits for every FFT frame (not the averaged ones); this gives near-100% POI for bursts longer than a frame. `docs/04 §3.5 "Time–frequency analysis"`.
- **On demand only:** multitaper (K ≈ 2NW−1 tapers, K× cost) for zoomed weak-next-to-strong analysis; reassignment (~3× STFT) for chirps/LoRa. `docs/04 §3.1`, `§3.5`.
- **Processing gain:** 10·log10((fs/2)/RBW); 20 Msps with 10 kHz bins gives +30 dB. `docs/02 §1.2 "ADC bit depth, SNR, ENOB, SFDR"`.

## Platform constraints
- HackRF One: 8-bit (~50 dB ideal, "closer to 6 bits" in practice), DC spike at centre, no preselector, 2–20 Msps (<8 Msps not recommended). `docs/01 §1.2 "Specifications"`, `docs/01 §1.3 "Noise figure, dynamic range, and overload"`.
- USB 2.0 caps at ~35–40 MB/s, so 20 Msps is the ceiling and host-side drops are possible. `docs/01 §1.4 "USB 2.0 throughput ceiling"`.
- Compute (estimates, not benchmarked): 4096-FFT at ~4.9k/s is one CPU core or a small GPU fraction; Orin Nano Super has 6× A78AE, Ampere GPU, unified memory, 7/15/25 W modes. `docs/02 §3.2 "DSP compute: order-of-magnitude feasibility"`, `docs/02 §3.3 "Platform comparison"`.

## Prior art and reuse
- **cuFFT / CuPy `cupyx.scipy.signal`** (cuSignal folded in; cuSignal archived): GPU FFT on Jetson. Licence: check. `docs/03 §1.7 "GPU / ML stacks"`.
- **VOLK** (active, v3.3.0), **liquid-dsp** (C, no deps, spectral periodogram, active v1.8.2), **FFTW** (stable, version unverified): CPU path. Licences: check. `docs/03 §1.4 "DSP libraries"`.
- **Maia SDR**: FPGA FFT + WASM/WebGL2 waterfall; architecture reference. Licence: check.
- **gr-fosphor** (OpenCL, last push 2024-06), **Tektronix DPX**: persistence references. `docs/03 §5.2 "Best UX ideas that already exist (steal these)"`.

## Pitfalls
- **DC spike / LO leakage** at 0 Hz shows up as a permanent "signal". Mask it, or offset-tune and DDC back. `docs/04 §10.3 "Spur identification and removal"`.
- **Band edges** alias (discard ≥10% per side); **IQ images** at −25…−40 dB (mark mirror bins).
- **Clipping/IMD:** one strong in-window emitter sets the dynamic range. Carry the clip count (>1e-4 full-scale samples per block means suspect) into every frame. `docs/04 §10.4 "Dynamic range management"`.
- **Scalloping/leakage:** tone level varies with bin offset; correct coherent gain, integrate across OBW for wide signals.
- **Averaging hides bursts**: never feed only K-averaged rows to detection. Average and compute SK in linear power, not dB.

## Testing
- **Synthetic:** complex AWGN at known σ², then check PSD mean within ±0.2 dB of σ²/fs after ENBW correction (proposed target). Add tones at bin-centre and half-bin offsets and check scalloping matches the Hann theory. Add 1 ms OOK bursts at 10% duty: persistence must show them, the K=100 average must not dominate, and SK > 1 at the burst bins.
- **SK:** noise ≈ 1, CW < 1, pulsed > 1 (`docs/04 §3.1`); record SK variance vs M as a baseline.
- **Throughput:** 20 Msps with no dropped blocks on Orin at 15 W; log FFT/s, GPU%, per-frame latency.
- **SigMF fixtures (HackRF One):** FM band (carriers, overload); 433.92 MHz ISM (bursts); ADS-B 1090 MHz (µs bursts); 50 Ω terminated input (spur/DC).
- **Live only:** thermal/power-mode throughput, USB drops, dBm accuracy vs a signal generator.

## Example use cases
C07 is substrate; docs/06 §3 lists it only where a use case is specifically about spectra. Regenerated from `use-cases.yaml`:
- SPACE-073 — SETI narrowband drift search
- RESEARCH-050 — SDR as spectrum analyzer / power survey

## Open questions
- **Zoom-stream ownership (resolved, docs/06 §5):** the decimated narrowband "zoom" stream (25 Hz-bin / ~800k-point case) is owned by C11; C07 FFTs the stream C11 delivers. The full-rate STFT still needs ~16–32k points for 1 kHz bins.
- **Sweep rows:** doc 04 §3.8 has the discovery sweep update max-hold, mean and SK per bin. Is that C02 or C07? docs/06 leaves it ambiguous.
- CPU (VOLK/liquid) vs GPU (cuFFT) default; does the pipeline-framework ADR decide it?
- Persistence on-device vs in the UI client (C39 UI ADR); who decimates rows for C26.

## Reading list
1. `docs/04 §3.1 "PSD estimation"`
2. `docs/04 §3.5 "Time–frequency analysis"`
3. `docs/04 §10.3 "Spur identification and removal"`
4. `docs/02 §3.2 "DSP compute: order-of-magnitude feasibility"`
5. `docs/01 §7.3 "HackRF as a front end for Pi 5 / Orin Nano (± FPGA)"`
6. `docs/03 §1.4 "DSP libraries"`
