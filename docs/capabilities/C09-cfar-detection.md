# C09 · cfar-detection
> Layer B — Sense · Status: draft (taxonomy draft 2026-09-13) · Depends on: C07, C08, C05 (spur mask), C01 (provenance); C02 for sweep rows · Used by: C10, C12, C13, C25, C31, C39

## Purpose
Turns spectra into emissions. OS-CFAR across frequency plus 2-D CFAR on spectrograms, hysteresis and minimum-duration filters, and connected components produce time×frequency boxes emitted as **Detection** (burst) records with SNR, extent, SK and trust flags. It is the step that makes the device *find* signals instead of making you tune (workflow steps 1–2), and it triggers recording (C25) and characterization.

## Interface
- **In:** `SpectrumFrame`s (dwell) or `SweepFrame`s (sweep) with linear PSD, SK, provenance; `NoiseFloorEstimate` + uncertainty (C08); spur mask and image-candidate rules (C05).
- **Out (provisional `Detection`):** `id`, `t_start`, `t_end`, `f_centre`, `bw_hz` (box extent, refined from the burst-averaged PSD), `peak_snr_db`, `mean_snr_db`, `sk`, `source` (sweep|dwell), `rbw_hz`, `iq_ref` (ring-buffer span with ±20% padding), and `flags`: `clipping`, `spur_mask_hit`, `image_candidate`, `edge`, `dc`, `suspect_imd`. `docs/04 §3.6 "Burst detection and segmentation"`.
- **Config:** P_FA target, reference/guard cells, OS rank k, th_on/th_off (dB), min on-time, gap merge g_min, 2-D window sizes, min box area, per-band profiles.

## Methods
- **OS-CFAR** (default across frequency): the k-th order statistic of N reference cells, k ≈ 3N/4, robust to adjacent channels and clutter edges. `docs/04 §3.4 "CFAR detection across frequency (and time)"`.
- **CA-CFAR** baseline: α = N(P_FA^(−1/N) − 1) for square-law/exponential noise. Example (derived): N=32, P_FA=1e-6 gives α ≈ 17.3 (12.4 dB). It masks near other signals. GO/SO-CFAR handle edges.
- **Averaged bins:** with n_avg > 1 the bin statistic is χ²_{2n}, so thresholds must use n_avg, not the exponential formula. Energy-detector threshold γ ≈ σ²(1 + Q⁻¹(P_FA)/√N). `docs/04 §3.3 "Energy detection and Neyman–Pearson thresholds"`.
- **Pipeline:** global floor (C08) → per-bin min-stat → OS-CFAR → **floor guard ≥3–5 dB** → hysteresis (open th_on, close th_off) → min on-time, gap merge → **2-D CFAR** on the spectrogram → connected components → boxes. `docs/04 §3.4`, `§3.6`.
- **SK co-detector:** SK > 1 flags bursty content at low energy; SK < 1 flags CW. Record SK per detection. `docs/04 §3.1 "PSD estimation"`.
- **Refinement:** f_c/BW from the burst-gated PSD (not idle time). `docs/04 §4.1 "Bandwidth"`.
- **Trust tests** (flags, via C05): retune test, gain-step (IM3 moves ~3× the gain step), antenna-off, image at 2f_LO − f. `docs/04 §10.3 "Spur identification and removal"`.
- **Alternatives, later:** YOLO/DETR spectrogram detectors for dense 2.4/5.8 GHz scenes (C38); feature detectors (cyclostationary, preamble) escape the SNR wall (C14/C16). `docs/04 §3.6`, `docs/03 §4.2 "How well AMC works on real OTA data"`.

## Platform constraints
- **Dwell vs sweep POI:** in sweep mode a 5 ms 902–928 MHz burst has P_POI ≈ 0.7% per occurrence; parked, ~100%. Sweep detections are for persistent emitters. `docs/04 §3.8 "Sweep-based survey vs. real-time IBW"`.
- Frame time bounds the shortest detectable burst: 4096-pt FFT at 20 Msps = 205 µs (derived).
- 8-bit, no preselector: in cities many detections are IMD. SFDR, not noise, sets the false-alarm floor. `docs/02 §1.2 "ADC bit depth, SNR, ENOB, SFDR"`, `docs/02 §1.7 "Overload and intermodulation in urban RF"`.
- Clipping: >1e-4 full-scale samples/block means reduce gain and mark detections suspect. `docs/04 §10.4 "Dynamic range management"`. CFAR is cheap on CPU at 20 MHz (`docs/02 §3.2`).

## Prior art and reuse
- **rtl_433:** in-band burst detection feeding the pulse analyzer; the best detect→characterize→decode model. Licence: check. `docs/03 §3.3 "Automatic device and protocol decoders"`.
- **IQEngine `simple_detector`, `markos_detector`, `fm_signal_detector`:** detectors returning SigMF annotations; copy the output contract. Licence: check. `docs/03 §3.4 "Protocol reverse engineering and signal inspection"`.
- **SigDigger waveform-window burst detection; URH burst detection** (GPLv3, archived). `docs/03 §3.4`.
- **Mayhem Signal Hunter / Search** (energy or mean+threshold triggers): the baseline to beat. `docs/01 §3.4 "Spectrum and "exploration" apps and their limits"`.
- **Tektronix RSA306B** "detects signals ≥100 µs": a stated-POI spec to emulate. `docs/02 §6 "Handheld and portable precedents: what they teach about finding signals"`.

## Pitfalls
- DC spike, band-edge bins, spurs and IQ images produce false emissions without masks and flags.
- **SNR wall:** 1 dB floor uncertainty caps energy detection near −3.3 dB SNR; don't promise below it.
- **CA-CFAR masking** in adjacent-channel rasters (LMR, cellular); **box splitting** of OFDM/FSK with spectral nulls; **box merging** of adjacent channels. Tune the connected-component dilation per band.
- Hysteresis too tight: one PTT becomes many bursts; too loose: TDMA slots merge.
- Measured P_FA drifts when noise isn't exponential (quantization, combs, impulsive man-made noise) or gain changes mid-burst.

## Testing
- **Synthetic recipe:** AWGN + K bursts (OOK/FSK/QPSK, 0.5–100 ms, BW 5–200 kHz) at SNR −5…+30 dB, random times; 2 strong adjacent carriers for masking; a DC spike, a fixed spur, and an image at −35 dBc.
- **Metrics:** Pd vs SNR curves per burst length; measured P_FA on noise-only within 2× of configured (proposed); box IoU vs truth (proposed ≥0.7 at 10 dB SNR); f_c error < 1 bin, BW error < 10% at ≥10 dB (proposed). Spur/image detections must carry flags (100%).
- **SigMF fixtures (HackRF One):** 433.92 MHz remotes and weather sensors; 902–928 MHz meter bursts; ADS-B 1090 MHz (decoder-confirmed truth); 162.55 MHz NOAA Weather Radio (continuous carrier); FM band at high gain (IMD ghosts, flags expected).
- **Live only:** retune/gain-step trust tests and POI against a burst generator.

## Example use cases
Provisional until docs/06 §3 mapping:
- AWARE-036 — Unknown burst reverse-engineering triage
- AWARE-034 — Wi-Fi DFS radar event logging
- AWARE-045 — CBRS/shared-band incumbent activity sensing
- AWARE-005 — "Personal privacy device" hunter
- AWARE-035 — Smart-meter mesh as noise contributor
- SIGNAL-052 — rtl_433 long tail
- SIGNAL-076 — VHF collar / radio-tracking
- SPACE-064 — Jupiter S-burst microstructure
- PROP-071 — Weather radar pulses + NEXRAD cross-reference

## Open questions
- Who cuts and holds IQ snippets (±20% padding): C09, C03 or C25?
- Is the §2.1 edge C05 → C09 explicit? The spur mask is a hard input; the diagram is ambiguous.
- Ownership of learned detectors (C38) and feature-based detection (C14/C16) as alternative Detection sources; one record type for all?
- Separate profiles for sweep rows (few averages, per-slice gain) and dwell spectrograms? Per-band defaults need a spike.

## Reading list
1. `docs/04 §3.4 "CFAR detection across frequency (and time)"`
2. `docs/04 §3.6 "Burst detection and segmentation"`
3. `docs/04 §3.3 "Energy detection and Neyman–Pearson thresholds"`
4. `docs/04 §10.3 "Spur identification and removal"`
5. `docs/04 §3.8 "Sweep-based survey vs. real-time IBW"`
6. `docs/02 §1.7 "Overload and intermodulation in urban RF"`
