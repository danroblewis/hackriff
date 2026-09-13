# C08 · noise-floor
> Layer B — Sense · Status: taxonomy frozen 2026-09-13 (resolved in docs/06 §5) · Depends on: C07 (C05 for dBm and spur mask; C09/C12 for idle-time) · Used by: C09, C13, C19, C30, C33, C39

## Purpose
Robust noise-floor estimates per bin, per channel and over time, with uncertainty. Every threshold depends on it: CFAR guard, squelch, spectral SNR, occupancy. It is also a science product (noise vs frequency and time: space weather, man-made noise, RFI trends). Per docs/06 §5, C08 OWNS the noise-floor-vs-time measurement (e.g. SPACE-050, AWARE-031); when the deliverable is a calibrated, long-integration radiometric product, `radiometry` (C33) leads and C08 supplies the underlying estimate. It serves workflow steps 2–3 and feeds the attack map (C30).

## Interface
- **In:** `SpectrumFrame`s from C07 (linear PSD, `n_avg`, RBW, provenance, DC/edge masks); optional sweep rows from C02; spur mask and dBFS→dBm table from C05; per-channel idle intervals from C09/C12.
- **Out (provisional `NoiseFloorEstimate`):** per-frame scalar (FCME/percentile), per-bin slowly varying floor (minimum statistics), per-channel idle-time floor. Each has `value` (dBFS/Hz, dBm/Hz once calibrated), `uncertainty_db`, `method`, `window_s`, `occupied_fraction`, and provenance (gain state, clip flag, cal version).
- **Out (science series):** noise-floor time series per band, tagged with time/position (C06) for C26/C30/C33.
- **Config:** method chain, FCME T_CME (from target P_FA), min-stat window (minutes for RF), guard margin (default 3–5 dB), update rate.

## Methods
Recommended chain (`docs/04 §3.4 "CFAR detection across frequency (and time)"`, last paragraph): FCME or percentile for the global frame floor, then minimum statistics per bin, then OS-CFAR local detection in C09.
- **Percentile/median across frequency:** robust if <~50% of bins are occupied. For χ²₂ bins, median = σ²·ln2, so divide by ln2 (+1.59 dB). `docs/04 §3.2 "Noise-floor estimation"`.
- **FCME** (default global): sort bins, start with the smallest ~10%, and iteratively add bins below T_CME·mean(set). T_CME comes from the exponential-distribution P_FA. It tolerates up to ~85% occupancy. `docs/04 §3.2`.
- **ITU 80% method:** average the lowest 20% of samples; for SM.2256-comparable reports, weak in busy bands.
- **Minimum statistics** (Martin 2001): recursively smooth each bin in time, track its minimum over a sliding window (minutes for RF), apply bias compensation. It handles drift, and a channel only needs to be idle *sometime* in the window.
- **Per-channel idle-time** (SM.2256 preferred): average power only while the channel is not detected. It captures locally raised noise (reciprocal mixing near a strong carrier).
- **Guard margin:** final threshold ≥3–5 dB above measured noise to avoid "phantom occupancy". `docs/04 §3.2`, `docs/04 §3.9 "Occupancy statistics methodology (ITU-R SM.1880 / SM.2256)"`.
- **SNR wall:** noise uncertainty ρ bounds energy detection at SNR_wall = (ρ²−1)/ρ, so 1 dB uncertainty gives ≈ −3.3 dB. Report the uncertainty so C09 can compute its limit. `docs/04 §3.3 "Energy detection and Neyman–Pearson thresholds"`.
- **Sanity check (calibrated):** N_dBm = −174 + NF + 10·log10(RBW); NF 6 dB at 12.5 kHz gives −127 dBm. `docs/04 §10.2 "Power calibration: dBFS to dBm"`.

## Platform constraints
- The floor depends on gain state (LNA/VGA/amp). Every estimate must be keyed by gain setting, and gain steps invalidate min-stat windows. `docs/04 §10.4 "Dynamic range management"`.
- 8-bit ADC, ~6 effective bits: at low gain the floor can be quantization noise rather than RF noise. `docs/01 §1.3 "Noise figure, dynamic range, and overload"`.
- No GSG-published HackRF One NF, so absolute dBm needs C05. The DC spike and filter skirts bias bins; apply C07 masks. `docs/01 §1.2 "Specifications"`.
- Below ~200 MHz the external man-made noise usually exceeds receiver noise. `docs/02 §1.3 "Noise figure and sensitivity"`.

## Prior art and reuse
- **RTLSDR-Airband:** noise-tracking squelch opening ~10 dB above the estimate; its wiki shows the failure case (continuous carriers). Licence: check. `docs/03 §3.2 "Real-time scanning inside receivers"`.
- **Trunk Recorder:** detector with automatic noise-floor threshold. Licence: check.
- **SDRangel `channelpower`, `noisefigure`, `radioastronomy`:** measurement references. Licence: check. `docs/03 §2.2 "SDRangel — the most engineering-grade open-source receiver"`.
- **ITU-R SM.1753 / SM.2256-1:** normative method definitions.

## Pitfalls
- **Occupied bands** (FM broadcast, cellular, 2.4 GHz): percentile and 80% methods overestimate the floor; FCME or idle-time is needed.
- **Reciprocal mixing:** a −20 dBm blocker with −110 dBc/Hz phase noise raises the local floor ~40 dB above thermal. A band-wide estimate misses this, so use per-channel floors. `docs/02 §1.4 "LO phase noise and reciprocal mixing"`.
- **IMD raises the floor** as gain rises; a gain-step test distinguishes it. `docs/04 §10.3 "Spur identification and removal"`.
- **Non-stationary noise:** diurnal man-made noise and switching-supply combs. A min-stat window that is too long lags; too short, and it tracks signals.
- **Continuous carriers never idle** (control channels, 100% FCO): fall back to adjacent-bin estimates.
- **Spur bins** bias percentiles; apply the C05 spur mask first. **Temperature drift** of gain mimics science signals; log it (C01).

## Testing
- **Synthetic:** AWGN at known σ²; occupy 0–90% of bins with random-width signals at 3–30 dB SNR. Assert percentile error <0.5 dB up to 40% occupancy and FCME error <1 dB up to 80% occupancy (proposed targets, consistent with the doc 04 §3.2 tolerance claims).
- **Drift:** ramp the floor by 3 dB over 10 min with intermittent bursts. Min-stat should track it with bounded lag; plot bias vs window length.
- **Guard:** with thresholds at floor + 3 dB on pure noise, measured occupancy should be ≈0 ("phantom occupancy" check).
- **SigMF fixtures:** 50 Ω terminated input per gain setting; FM band 88–108 MHz (dense); 144–148 or 440 MHz segment with sparse traffic (idle-time).
- **Live only:** −174 dBm/Hz sanity against a known-ENR noise source (C05); long diurnal runs.

## Example use cases
Regenerated from `use-cases.yaml`:
- SPACE-050 — Natural radio noise floor survey
- AWARE-031 — Long-term noise-floor trend logger
- SPACE-031 — Riometer
- SPACE-012 — Shortwave fadeout detector
- AWARE-027 — ISM car-key jammer detection
- AWARE-064 — Satellite pass vs. noise-floor attribution
- AWARE-032 — (noise-floor anomaly use case)

## Open questions
- **Circular dependency:** idle-time estimation needs detections (C09/C12), while C09 needs the floor. Define the bootstrap order (FCME first, idle-time refinement later). docs/06 §2.1 doesn't show it.
- **Overlap with C33 radiometry:** who owns the calibrated noise-vs-time science series? docs/06 says C08 "is also the science-grade measurement".
- **C05 dependency is missing from §2.1:** dBm output and spur masks require it.
- Key estimates by position (C06) for a moving handheld? Default min-stat window per band (spike).

## Reading list
1. `docs/04 §3.2 "Noise-floor estimation"`
2. `docs/04 §3.3 "Energy detection and Neyman–Pearson thresholds"`
3. `docs/04 §3.9 "Occupancy statistics methodology (ITU-R SM.1880 / SM.2256)"`
4. `docs/04 §10.2 "Power calibration: dBFS to dBm"`
5. `docs/02 §1.4 "LO phase noise and reciprocal mixing"`
6. `docs/04 §6.2 "Automatic squelch"`
