# C05 · calibration
> Layer A — Acquire · Status: draft (taxonomy draft 2026-09-13) · Depends on: C01, C02, C03, C06, C07, C09 · Used by: C01, C02, C04, C08, C09, C13, C27, C31, C33, C34

## Purpose
Makes measurements trustworthy on an 8-bit, unpreselected, crystal-referenced front end:
- ppm calibration;
- a dBFS→dBm table;
- a spur map;
- image and IMD tests that flag detections "suspect";
- per-band clip-free gain tables.

Without it, a city inventory fills with ghosts and narrowband rasters are misread. It underpins workflow steps 3–4 and every science use case.

## Interface
- **Inputs:** IQ + provenance (C01/C03), SweepFrames (C02), GNSS reference (C06), detections (C09), temperature.
- **Outputs (provisional `CalibrationState`, versioned):**
  - ε_ppm with uncertainty, temperature and time.
  - K(f, gain, T) power table in dB.
  - Spur mask per gain state.
  - Per-band gain table.
  - Detection flags: `suspect_imd`, `image_candidate`, `spur_mask_hit`, `clipped`.
- **Control:** full calibration (antenna terminated), opportunistic ppm refresh, per-detection retune/gain-step test, invalidation on antenna/filter/accessory change.
- **Rate:** periodic. Refresh ppm on temperature change (docs/04 §10.1).

## Methods
- **Frequency** (docs/04 §10.1 "Frequency calibration"):
  - Model: δf = ε_ppm·10⁻⁶·f_c.
  - References:
    - LTE PSS/SSS (base stations ±0.05 ppm; tolerates large offsets).
    - FM 19 kHz pilot (±2 Hz relative to carrier).
    - ATSC pilot, NOAA Weather Radio, WWV.
    - ADS-B/AIS symbol rates (sample clock).
    - GNSS/GPSDO.
    - GSM FCCH +67.708 kHz (international fallback).
  - LO and sample clock share a crystal, so one ε usually fixes both. Verify with a timing loop on a known-rate signal.
  - Apply in software, or via `RADIO_CLOCK_CORRECTION` (USB API 1.13, `main` only; unverified in a release) (docs/01 §1.6 "Firmware, `hackrf_sweep`, and host tools").
- **Power** (docs/04 §10.2 "Power calibration: dBFS to dBm"):
  - P_dBFS = 10·log10(mean|x|²/x_FS²); P_dBm = P_dBFS + K(f, G, T).
  - Build K over a frequency × gain grid from a signal generator or a known-ENR noise source, and interpolate. Re-verify gain steps.
  - Sanity check: N_dBm = −174 + NF + 10·log10(RBW).
  - Hann ENBW is 1.5 bins (+1.76 dB); integrate across OBW for wide signals.
- **Spur map:** 50 Ω load across tuning range and gains; mask or subtract (docs/04 §10.3 "Spur identification and removal").
- **Automated artefact tests** (docs/04 §10.3):
  1. Retune by Δ: real signals stay put; LO spurs and images move.
  2. Gain step X dB: real signals change X dB; IM3 changes ~3X dB.
  3. Antenna off: a persisting signal is internal.
  4. Image: a detection at 2f_LO − f of a stronger, envelope-correlated signal.
- **Image level:** IRR ≈ 10·log10(4/(ε²+θ²)), ~40 dB at 1% / 1° (docs/02 §1.5 "DC offset, IQ imbalance, image rejection, harmonic responses").
- **Gain** (docs/04 §10.4 "Dynamic range management"):
  - Target −6 to −10 dBFS peak.
  - Clip fraction >1e-4 → gain down and flag suspect IM.
  - Periodic gain-step tests in dense bands; learned per-band sweep tables.

## Platform constraints
- HackRF One: plain crystal, no TCXO; "±20 ppm" unverified (docs/01 §1.2 "Specifications"). 20 ppm would be 48 kHz at 2.4 GHz (derived). Uncompensated crystals are 10–50 ppm and drift with temperature (docs/04 §10.1).
- Pro: 0.5 ppm TCXO. CLKIN takes a 10 MHz GPSDO, ~1e-12 long-term (docs/02 §1.8 "Clock accuracy (TCXO/OCXO/GPSDO)").
- 8-bit: ~50 dB ideal, "closer to 6 bits" in practice; no published NF (docs/01 §1.3 "Noise figure, dynamic range, and overload").
- No preselector; 3×/5×LO harmonic responses. A filter bank switched by Opera Cake is the likely add-on (docs/02 §1.6 "Preselection filters: why they matter for wideband surveys").
- Jetson heat in a sealed handheld drives crystal drift (docs/02 §7.3 "Bottlenecks and design recommendations").

## Prior art and reuse
- **LTE-Cell-Scanner:** PSS-based ppm, robust to large offsets. Licence: check.
- **kalibrate-rtl:** GSM FCCH. Licence: check.
- **Trunk Recorder `autoTune`:** frequency-error correction in practice (docs/03 §3.5). Licence: check.
- **SDRangel `noisefigure` and `radioastronomy` plugins:** Y-factor and calibration workflows (docs/03 §2.2).

## Pitfalls
- **Stale tables:** antenna, cable, LNA, filter or port changes invalidate power tables. Key tables on accessory config.
- **Temperature drift after calibration:** stamp temperature and re-run ppm.
- **Spur map contamination:** IMD depends on environment. Take the spur map only with a terminated input.
- **Disruptive tests:** gain-step tests disturb live dwells, so C04 must schedule them.
- **Clipping:** clipped blocks invalidate power readings and hide IMD.
- **Clone variability:** per-unit calibration, never shared tables (docs/01 §7.2 "Fundamentally limiting (don't inherit)").
- **FM pilot:** the pilot is relative to the station carrier. Cross-check against a second reference.
- **Legal:** cellular signals are used for sync timing only, never content (docs/04 §1.3 "Legal considerations (US; not legal advice)").

## Testing
- **Synthetic tones:** known ε (e.g. 15 ppm); assert ε̂ within ±0.1 ppm (estimate target). Also a synthetic FM stereo MPX for the pilot path.
- **Synthetic imbalance:** 1% / 1° IQ imbalance → image flag at the mirror, ~−40 dB.
- **Synthetic nonlinearity:** two tones through a cubic nonlinearity. A 3 dB input step makes IM3 move ~9 dB and flags it.
- **Synthetic clipping:** clip fraction >1e-4 → gain-down action plus flag.
- **Fixtures (HackRF One):**
  - Terminated sweeps at several gains.
  - A strong FM stereo station.
  - An LTE downlink (PSS only).
  - WWV 10 MHz.
  - A downtown 0–1 GHz sweep at two gains.
- **Live:** signal-generator power table; temperature soak; GPSDO CLKIN comparison.

## Example use cases
Provisional until docs/06 §3 mapping.
- SIGNAL-048 — Cellular broadcast metadata & calibration
- AWARE-051 — Oscillator-offset fingerprint for low-cost sensors
- RESEARCH-054 — Noise-figure by Y-factor
- SPACE-009 — Sun-noise antenna calibration
- SPACE-050 — Natural radio noise floor survey
- RESEARCH-050 — SDR as spectrum analyzer / power survey
- RESEARCH-057 — EMC pre-compliance scanning
- AWARE-054 — Amateur-band intruder logging

## Open questions
- docs/06 §2.1 draws C05 as a peer of C02/C03, but its artefact tests need C07/C09 detections and C04 scheduling, which makes a C05 ↔ C09 loop. Split into calibration state (Layer A) and artefact tests (Layer B)?
- A one-person setup likely has no signal generator. Is a known-ENR noise source or Sun/sky (C33) the default power reference?
- Firmware clock correction vs host resampling: ADR.
- Is a switched filter bank/FM notch in the base BOM? That changes which tests run by default.

## Reading list
1. docs/04 §10.3 "Spur identification and removal"
2. docs/04 §10.1 "Frequency calibration"
3. docs/04 §10.4 "Dynamic range management"
4. docs/04 §10.2 "Power calibration: dBFS to dBm"
5. docs/02 §1.7 "Overload and intermodulation in urban RF"
6. docs/02 §1.5 "DC offset, IQ imbalance, image rejection, harmonic responses"
