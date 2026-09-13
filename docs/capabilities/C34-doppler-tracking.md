# C34 · doppler-tracking
> Layer G — Specialised · Status: taxonomy frozen 2026-09-13 (resolved in docs/06 §5) · Depends on: C04, C05, C06, C09, C11, C29 · Used by: C22, C27, C30, C33, C39

## Purpose
Measures and exploits frequency shifts of known carriers:
- TLE-driven pre-correction for satellite passes;
- Doppler-curve fitting to identify satellites or refine orbits;
- Hz-resolution HF carrier Doppler for ionospheric science (Grape, TIDs, flare SFDs);
- single-channel echo detection (meteors, aircraft) on known illuminators.

It serves space-weather science and satellite-pass attribution in the attack map (C30). It also gives satellite decoders Doppler-corrected streams (workflow steps 5–6).

C34 **consumes** the satellite passes that C29 computes from cached TLEs; TLE propagation is a C29 feed-side computation, not C34's (docs/06 §5). C34 does the carrier tracking, curve fitting and pre-correction retune on top of those pass windows.

## Interface
- **Inputs:**
  - Channel stream (C11 DDC around a nominal carrier).
  - TLEs, pass predictions and DSN schedules (C29).
  - Site position and GNSS time (C06).
  - Reference-carrier and ppm state (C05).
- **Output `DopplerTrack` (provisional):**
  - UTC; measured and predicted frequency; residual (Hz); SNR; lock.
  - Velocity `v = −c·Δf/f0` (standard relation).
- **Output `DopplerFit` (provisional):** candidate object ids with residual RMS; `f0`; TCA; max range-rate; ppm term.
- **Output `EchoEvent` (provisional):** time, duration, Doppler (Hz), drift (Hz/s), SNR, illuminator id.
- **Config:**
  - Nominal `f0`; resolution (Grape records at **1 Hz resolution**, SPACE-016); update rate.
  - Tracker (FFT-peak or PLL); pre-correction on/off; drift-rate range; illuminator list.
- **Resolution and accuracy:**
  - `RBW = ENBW·fs/N` (`docs/04 §3.1 "PSD estimation"`), so Hz-level work needs heavy decimation.
  - Absolute accuracy is bounded by the reference clock.

## Methods
- **Carrier tracker:** decimate (C11), then long-FFT peak with parabolic interpolation per update. Robust at low SNR. A PLL is the alternative for strong continuous carriers.
- **Clock separation** (`docs/04 §10.1 "Frequency calibration"`):
  - Remove `δf = ε_ppm·10⁻⁶·f_c` using a GPSDO or a simultaneously measured in-window reference (FM 19 kHz pilot ±2 Hz, WWV, LTE PSS).
  - Differencing against a reference cancels common drift (spike).
- **Satellite pre-correction:**
  - Use pass predictions from C29 (which propagates the cached TLEs, SGP4-class; library not named in docs) — C34 does not re-propagate TLEs itself.
  - Retune the DDC continuously and load presets at AOS, following the SDRangel Satellite Tracker model (`docs/03 §2.2`).
  - Hand corrected streams to C22.
- **Curve identification** (SIGNAL-035): fit measured `f(t)` against each catalogue object's range-rate curve, jointly solving `f0` and ppm; rank by residual; TCA at the maximum slope.
- **HF ionospheric Doppler** (SPACE-016, PROP-017): continuous WWV/CHU or many AM carriers via C11; log to C26; SFD-like anomalies (SPACE-013) go to C30.
- **Echo detection** (SPACE-051, SPACE-053): narrowband spectrogram around the illuminator (e.g. GRAVES 143.050 MHz) with the direct signal masked, 2-D CFAR (C09), then Doppler and drift per echo.
- **Drift search** (SPACE-073): de-Doppler drift-rate search in the turbo_seti/hyperseti style.

## Platform constraints
- **The clock is the binding constraint:**
  - HackRF One has a plain crystal, no TCXO; ±20 ppm is unverified (`docs/01 §1.2 "Specifications"`).
  - 1 ppm is 10 Hz at 10 MHz and 437 Hz at 437 MHz (derived from `docs/04 §10.1`).
  - Grape-style 1 Hz work needs a **GPSDO on CLKIN** (10 MHz, 3.3 V, selected only at RX start) or reference differencing, so `needs-accessory` (GPSDO).
  - Example GPSDO: LBE-1420, 1 Hz–1.1 GHz output, ~1e-12 (`docs/02 §1.8 "Clock accuracy (TCXO/OCXO/GPSDO)"`). Level compatibility is unverified.
  - HackRF Pro has a 0.5 ppm TCXO (`docs/01 §1.7`). Better, but not Hz-level at HF over hours (estimate).
- **HF is a poor fit for HackRF:** 8-bit, no preselector (`docs/02 §2.2 "Which hardware suits which exploration mode"`), so use a filter or attenuator: `needs-accessory`.
  - MW AM carriers (PROP-017, 530–1700 kHz) are partly below 1 MHz: upconverter or other front end.
- **Ku-band** Starlink tones (PROP-074) need an LNB, whose LO drift adds error.
- **One ≤20 MHz window:** passes and long HF logs monopolise it, so C04 reserves windows from predictions. Multi-carrier tracking is limited to that window.
- **Satellite antennas:** directional or QFH (SIGNAL-024 hint "RTL-SDR + QFH").
- **Compute:** low.

## Prior art and reuse
- **SigDigger/Suscan:** "Doppler analysis with TLE-based correction" inside the inspector model. Single maintainer; last tagged release 2022, `master` active; licence: check (`docs/03 §3.4 "Protocol reverse engineering and signal inspection"`).
- **SDRangel:** Satellite Tracker (Doppler correction, AOS presets) and `freqtracker`. Very active; licence: check (`docs/03 §2.2`).
- **SatDump:** pass scheduler with rotator control; pipeline registry (`docs/03 §3.6 "Satellites"`). Licence: check.
- **HamSCI Grape/PSWS, ka9q-radio** (SPACE-016/017): upload formats. Licence: check.
- **turbo_seti, hyperseti, setigen** (SPACE-073): drift search, synthetic signals. Licence: check.

## Pitfalls
- **Clock drift** reads as ionospheric Doppler. Log clock source and reference residual.
- **Stale TLEs** (C29 cache age) bias pre-correction, worst after launches.
- **Bad site position or time** biases fits.
- **HF multipath:** O/X modes and multi-hop split carriers; single-peak tracking hides that. Keep spectra.
- **Model error:** low-elevation refraction; LNB/transponder LO offsets.
- **Echo false alarms:** direct-signal sidelobes, aircraft/meteor confusion, sporadic-E.

## Testing
- **Synthetic:**
  - Carrier following a Doppler curve from a fixed TLE and site, with ppm offset, drift and noise. Assert the true object ranks first and `f0`/ppm are recovered within an estimated tolerance.
  - Sub-Hz sinusoidal Doppler on 10 MHz: detected with a GPSDO-grade clock model, lost without one.
  - Synthetic meteor pings.
  - setigen injections for drift search.
- **Fixtures:**
  - WWV 10 MHz carrier over hours, with and without GPSDO.
  - NOAA/Meteor 137 MHz or ISS pass as a decimated SigMF snippet.
  - FM or GRAVES echo recordings.
  - FM pilot reference.
- **Live:** real-time pre-correction during passes; long HF logs (GPSDO, HF antenna).

## Example use cases
Regenerated from `use-cases.yaml`:
- SPACE-013 — Sudden frequency deviation
- SPACE-016 — Grape HF Doppler on WWV/CHU
- SPACE-017 — HamSCI PSWS Doppler
- SPACE-018 — Traveling ionospheric disturbance tracking
- SPACE-034 — (doppler-tracking primary)
- SPACE-051 — GRAVES meteor echoes
- SPACE-052 — Meteor echo Doppler profiles
- PROP-013 — (doppler-tracking primary)
- PROP-017 — AM broadcast Doppler for TIDs
- PROP-077 — Time-station propagation delay

## Open questions
- **Boundary with C35:** single-channel echo detection stays in C34, range–Doppler in C35. Confirm meteor counting (SPACE-053) maps here.
- **TLE propagation (resolved owner, docs/06 §5):** C29 propagates cached TLEs and computes passes; C34 consumes them. The SGP4-class library and its licence (not in docs) sit with C29 / the Phase 3 licence ledger.
- **Pre-correction path:** retune the DDC (C11) or the HackRF LO (C01)? LO retunes restart streams. ADR.
- **Kinematics:** share with C33 H I correction?
- **GPSDO:** base kit or optional? It decides `hardware_fit` for every HF Doppler use case.

## Reading list
1. `docs/04 §10.1 "Frequency calibration"`
2. `docs/02 §1.8 "Clock accuracy (TCXO/OCXO/GPSDO)"`
3. `docs/03 §3.4 "Protocol reverse engineering and signal inspection"` (SigDigger)
4. `docs/03 §2.2 "SDRangel — the most engineering-grade open-source receiver"` (Satellite Tracker)
5. `docs/05 §1 "Ionosphere & HF Propagation Science"` and `"Meteors"`
6. `docs/01 §1.2 "Specifications"` (clock, CLKIN)
