# C13 · param-estimation
> Layer C — Characterize · Status: taxonomy frozen 2026-09-13 (resolved in docs/06 §5) · Depends on: C11, C08, C05, C09 · Used by: C14, C15, C16, C18, C19, C20, C27

## Purpose
Measures one detected emission from a channelized snippet: occupied bandwidth, carrier offset, SNR, spectral shape, and whether it looks analog or digital. These normalise the input for every classifier and demodulator, and supply fingerprint and inventory fields. Serves workflow step 5 (parameters estimated, never picked), step 4 (bandwidth and raster offset against emission designators) and step 6.

## Interface
- **Inputs** (provisional names):
  - `ChannelSnippet`: IQ at a few × the bandwidth (docs/04 §4), with sample rate, RF centre and timestamps.
  - Burst gate from the `Detection`.
  - `NoiseFloor` N₀ ± uncertainty (C08).
  - `CalibrationState`: ppm, spur mask, image candidates (C05).
  - Provenance: gain, clip count.
- **Output: `ParameterSet`.** Each field carries value, uncertainty and method id.
  - `obw99_hz`, `xdb_bw_hz`, `fc_hz` (ppm-corrected).
  - `cfo_hz`, `raster_offset_hz`, `snr_db`.
  - Flatness, symmetry P, carrier-line strength.
  - `analog_digital ∈ {analog, digital, unknown}`, `duration_s`.
- **Unknown handling:**
  - Any field may be `null` with a reason: `low_snr`, `too_short`, `clipped`, `multi_signal`, `spur_mask_hit`, `image_candidate`.
  - No defaults substituted. Pass C05 suspect flags through.
- **Config:** β = 99%; x-dB values −3/−6/−26; per-band raster table (C17); minimum SNR per estimator.

## Methods
- **Bandwidth** (docs/04 §4.1): OBW₉₉ from the noise-subtracted, burst-gated Welch PSD, between the 0.5% and 99.5% cumulative-power points. −26 dB for regulatory checks, −3/−6 dB for filters.
- **CFO** (docs/04 §4.2):
  - Default: spectral centroid.
  - Carrier signals: Jacobsen peak interpolation, then Kay/Fitz.
  - M-PSK: power-of-M, `f_off = (1/M)·argmax|F{x^M}|`. Unambiguous only within ±fs/(2M); M = 4 for QAM; needs a family hint.
  - FSK: mean instantaneous frequency. OFDM: C16.
  - Raster offset only after C05 ppm correction.
- **SNR** (docs/04 §4.3):
  - Default: spectral `(P_band − N₀B)/(N₀B)`, modulation-agnostic.
  - M2M4 `Ŝ = √(2M₂² − M₄)`: no timing needed, but biased for QAM.
  - EVM-based SNR from C20 supersedes both once locked.
- **Shape** (docs/04 §2 #9, #12): flatness, symmetry `P = (P_L−P_U)/(P_L+P_U)` (≈ ±1 for SSB), carrier line, subcarrier comb.
- **Analog vs digital** (docs/04 §4.10):
  - Cyclic line at a symbol rate (light C14 call).
  - Discrete vs continuous instantaneous-frequency histogram.
  - PSD stationarity; envelope kurtosis over 100 ms windows.

## Platform constraints
- **8-bit ADC.** ~50 dB ideal, "closer to 6 bits" in practice. A strong in-window signal sets the gain; weak skirts vanish, biasing −26 dB and 99% widths (docs/01 §1.3).
- **Ghosts with plausible parameters.** IQ image at −25…−40 dB, IM3 products, DC spike (docs/04 §10.3). Honour C05 flags; a clip fraction above 1e-4 means suspect (docs/04 §10.4).
- **Frequency error.** 1 ppm at 1 GHz = 1 kHz; uncompensated crystals drift 10–50 ppm (docs/04 §10.1). Raster offsets are meaningless without C05.
- **Usable width.** ~15–18 MHz of the 20 MHz window (docs/01 §7.3).
- **Compute.** μs–ms per snippet on one ARM core (docs/04 §5.5); CPU-only.

## Prior art and reuse
- **GNU Radio `mpsk_snr_est_m2m4`:** GPL; reimplement or isolate behind a process boundary.
- **SigDigger/Suscan:** gradient-descent SNR, inspector pattern. Licence: check. Single maintainer.
- **URH:** automatic noise level and threshold. GPLv3, archived 2026-03.
- **rtl_433 `-A`:** OOK/FSK levels. Licence: check.
- **SDRangel `channelpower`** (docs/03 §2.2). Licence: check.
- **ITU-R SM.328/SM.443:** bandwidth definitions (docs/04 §4.1).

## Pitfalls
- Without noise subtraction, OBW inflates to the span; idle time dilutes the PSD.
- Two emissions in one snippet corrupt every field (docs/04 §5.4 #5).
- The centroid is biased on SSB, unbalanced FSK and asymmetric OFDM. The SSB carrier is C19's job (docs/04 §6.1).
- Power-of-M with the wrong M gives confident garbage; M2M4 is biased by QAM and multipath.
- Short bursts leave too few PSD bins.
- Off-raster can be Doppler or a fault, not only unlicensed.
- HD Radio sidebands inflate WFM bandwidth (docs/04 §6.4).

## Testing
- **Synthetic:**
  - Signals: AM, NBFM, WFM, SSB, BPSK/QPSK/16QAM (α 0.2/0.35/0.5), 2/4-FSK, OOK, CP-OFDM.
  - Known OBW, sub-bin CFO sweep, SNR −5…+30 dB.
  - Impairments: 8-bit quantisation, DC offset, IQ imbalance (1%/1° ≈ 40 dB image, docs/02 §1.5), clipping, via TorchSig transforms (MIT).
- **Metrics:**
  - OBW relative error, CFO error, SNR bias/σ against true SNR.
  - Analog/digital confusion by SNR.
  - No targets in the docs; set them in the test strategy.
- **SigMF fixtures:**
  - FM broadcast: 19 kHz ±2 Hz pilot gives CFO truth.
  - ATSC 1.0 pilot, 309.44 kHz above the channel edge.
  - Aviation AM on the 25 kHz raster; NOAA Weather Radio NBFM.
  - AIS GMSK; P25.
  - LTE downlink (eNB ±0.05 ppm).
  - 315/433.92 MHz OOK sensors.
- **Live only:** gain-step/IMD behaviour, ppm drift with temperature.

## Example use cases
*Regenerated from `use-cases.yaml`.*
- AWARE-036 — Unknown burst reverse-engineering triage
- AWARE-051 — Oscillator-offset fingerprint for low-cost sensors
- AWARE-055 — Over-the-horizon radar signature catalogue
- AWARE-030 — Switching-supply / LED / inverter RFI signatures
- SIGNAL-012 — ILS localizer/glideslope DDM
- SIGNAL-043 — PTC 220 MHz
- SIGNAL-077 — Argos & ICARUS animal tags
- RESEARCH-005 — Blind signal detection with gr-inspector

## Open questions
- **Loop missing from §2.1.** Power-of-M and the analog/digital test need a family hint, so C13 ⇄ C14/C15 iterate; §2.1 shows a one-way chain.
- **Duplicated analog tests.** C15's feature tree and C19 (docs/04 §6.1) compute the same features; propose one shared extractor.
- **Unassigned radar parameters.** Pulse width, PRI and FMCW sweep (AWARE-034, AWARE-055, PROP-071) belong to no capability.
- **Mapping stretches C13.** It is primary for bench and side-channel items (RESEARCH-053 scikit-rf, RESEARCH-055 phase noise, RESEARCH-047/049) that aren't per-emission characterization. Consider a lab-measurement capability.
- **Ownership and format.** Raster tables: C17 or C05? Uncertainty as σ or interval: docs/07.
- **Wide emissions.** Emissions wider than the usable window.

## Reading list
1. docs/04 §4.1 "Bandwidth", §4.2 "Carrier frequency offset (CFO)", §4.3 "SNR estimation"
2. docs/04 §4.10 "Analog vs. digital discrimination (quick tests)"
3. docs/04 §10.3 "Spur identification and removal" and §10.4 "Dynamic range management"
4. docs/04 §10.1 "Frequency calibration"
5. docs/01 §1.3 "Noise figure, dynamic range, and overload"
6. docs/04 §2 "What makes a frequency "interesting": a feature taxonomy"
