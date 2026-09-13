# C33 · radiometry
> Layer G — Specialised · Status: draft (taxonomy draft 2026-09-13) · Depends on: C01, C02, C03, C05, C06, C07, C08 · Used by: C26, C30, C39

## Purpose
Turns the receiver into a science instrument that measures **noise power**, not signals. It provides:
- calibrated total-power and spectrometer time series with long integration;
- drift scans;
- Y-factor and Sun/sky calibration;
- sidereal tagging and spectral-kurtosis RFI flagging;
- dynamic spectra.

It serves riometer, H I, solar and noise-survey science, and supplies local evidence for C30's space-weather explanations.

## Interface
- **Inputs:**
  - IQ window (C03), or SweepFrames (C02) for wideband dynamic spectra.
  - Calibration (C05): `K(f, G, T)` dBFS→dBm table, noise-source ENR, spur mask.
  - UTC, LST and pointing (C06).
- **Output `RadiometryRecord` (provisional):**
  - UTC/LST; integration τ.
  - Band power (dBFS; dBm or K when calibrated); per-channel spectrum.
  - SK flag mask and flagged fraction.
  - Provenance: gain, temperature, clip count, cal epoch.
- **Products:** drift scans, riometer absorption, H I velocity spectra, dynamic spectra, NF/Tsys.
- **Config:** mode (`total-power | spectrometer | dynamic-spectrum | y-factor`), centre/bandwidth, bins, τ, SK M, calibration cycle.
- **Resolution:** `RBW = ENBW·fs/N` (`docs/04 §3.1 "PSD estimation"`); sensitivity `ΔT/Tsys ≈ 1/√(B·τ)` (standard, not in docs). No accuracy target in docs.

## Methods
- **Integration:** Welch PSD from C07, with per-bin `S1 = ΣP` and `S2 = ΣP²` accumulators, decimated to the output cadence.
- **RFI flagging** (`docs/04 §3.1`): `SK = (M+1)/(M−1)·(M·S2/S1² − 1)`, ≈1 for Gaussian noise, <1 for CW, >1 for pulsed. Excise flagged bins and blocks before averaging, and report the flagged fraction.
- **Power calibration** (`docs/04 §10.2 "Power calibration: dBFS to dBm"`):
  - `P_dBm = P_dBFS + K(f, G, T)`, with ENBW corrections.
  - Sanity-check against the noise floor `−174 + NF + 10·log10(RBW)`.
- **Y-factor** (RESEARCH-054): hot/cold ratio `Y` with a known-ENR source, then `F = ENR/(Y−1)` (linear; standard, not in docs). The same cycling tracks gain drift.
- **Astronomical calibration:**
  - quiet Sun with daily F10.7 flux (SPACE-009);
  - sidereal drift fitted to a global sky model with pygdsm (SPACE-066).
- **Riometer** (SPACE-031): build a quiet-day curve vs LST over many days. Absorption = QDC − measured.
- **H I** (SPACE-057): spectrometer at 1420.406 MHz with baseline subtraction, following Virgo/PICTOR.
  - Velocity axis via Doppler. The Earth-orbit term is ±30 km/s over a year (SPACE-060). Share kinematics with C34.
- **Dynamic spectra:** e-CALLISTO-style sweep over 45–870 MHz via C02 (SPACE-003), with a per-band gain table and spur masking.
- **Offset tuning:** exclude the DC-spike bins and ≥10% at each edge (`docs/04 §10.3 "Spur identification and removal"`).

## Platform constraints
- **Uncalibrated amplitude:** HackRF One can't do calibrated amplitude measurements out of the box (`docs/01 §7.3 "HackRF as a front end for Pi 5 / Orin Nano (± FPGA)"`). There is no published NF (`docs/01 §1.3`). Absolute results need C05 plus a noise source or sky calibration.
- **Gain drift:** `K` depends on temperature, the main long-integration error. Log temperature and cycle calibration.
- **8-bit, no preselector:** RFI or IMD raises total power and looks like sky signal (`docs/02 §1.7 "Overload and intermodulation in urban RF"`). A band filter is effectively required: `needs-accessory` (filter or Opera Cake filter bank).
- **Sample rate:** below 8 Msps is not recommended (`docs/01 §1.2 "Specifications"`). Capture at ≥8 Msps and decimate on host.
  - HackRF Pro 16-bit mode (ENOB 9–11, ≥16× decimation) suits narrowband H I (`docs/01 §1.7`).
- **External noise below ~200 MHz** often exceeds receiver noise (`docs/02 §1.3 "Noise figure and sensitivity"`). Good for a sky-noise-limited riometer; an LNA adds nothing there.
- **At 1420 MHz** receiver noise dominates. H I needs LNA + filter + horn/dish: `needs-accessory`.
  - Bias-tee: max 50 mA at 3.0–3.3 V (`docs/01 §1.2`).
- **Other accessories:** Sun-outage at Ku (SPACE-010) needs an LNB; Y-factor needs a calibrated noise source.
- **One window:** long integrations block survey. C04 must schedule science dwells.
- **Compute and storage:** compute low–medium; power series are tiny versus IQ (C26).

## Prior art and reuse
- **SDRangel:** `radioastronomy` plugin, Star Tracker drift scans, SID feature. Very active; licence: check (`docs/03 §2.2`).
- **Virgo / PICTOR** (0xCoto): H I spectrometer. Licence: check.
- **gr-radio_astro** (SPACE-059): GNU Radio blocks. Licence: check.
- **pygdsm** (SPACE-066): sky model. Licence: check.

## Pitfalls
- **Gain/temperature drift** masquerading as astrophysical change.
- **Residual RFI:** weak or narrow CW RFI escaping SK; FM/cellular IMD in wide bands.
- **Receiver artefacts:** DC spike, IQ image and LO spurs in narrow lines. Use the spur map.
- **Sidereal time:** a solar-time QDC smears.
- **Antenna and weather:** pattern, ground and rain change effective temperature.
- **Clipping** in solar bursts. Flag blocks with >1e-4 clipped samples (`docs/04 §10.4`).

## Testing
- **Synthetic:**
  - Gaussian noise plus a Gaussian-beam transit (Sun drift scan) plus injected CW/pulsed RFI. Assert SK flags the RFI and the transit amplitude is recovered within an estimated tolerance; SK≈1 on pure noise.
  - Hot/cold blocks → NF recovery.
  - Sidereal curve plus a dip → absorption detection.
- **Fixtures:**
  - Power-series captures: Sun drift scan (Yagi/dish); small-horn H I spectrum; a 20–50 MHz sky-noise day.
  - Terminated-input stability runs over temperature.
  - Short SigMF IQ snippets containing RFI for SK tests.
- **Live/field:** multi-day riometer QDC; H I chain; Y-factor. Needs accessories and long unattended runs.

## Example use cases
Provisional until docs/06 §3 mapping:
- SPACE-031 — Riometer
- SPACE-057 — Hydrogen line with an SDR
- SPACE-068 — Drift-scan radio transits
- SPACE-009 — Sun-noise antenna calibration
- SPACE-003 — e-CALLISTO solar burst spectrograms
- SPACE-066 — Galactic synchrotron sky as calibrator
- SPACE-050 — Natural radio noise floor survey
- RESEARCH-054 — Noise-figure by Y-factor
- SPACE-010 — Sun-outage radiometry
- SPACE-012 — Shortwave fadeout detector

## Open questions
- **Overlap with C08:** docs/06 gives C08 the science-grade "noise floor vs time" (SPACE-050, AWARE-031), while C33 claims "noise-survey use cases". Pick the primary for mapping.
- **VLF SID** (SPACE-001, soundcard amplitude/phase of carriers): C33, C34 or both?
- **Noise source:** a supported accessory switched via Opera Cake? Needs an ADR.
- **H I baseline method** (frequency vs position switching) isn't in the docs. Spike.
- **Canonical unit:** K or dBm in `RadiometryRecord`? This waits on docs/07.

## Reading list
1. `docs/04 §3.1 "PSD estimation"`
2. `docs/04 §10.2 "Power calibration: dBFS to dBm"`
3. `docs/03 §2.2 "SDRangel — the most engineering-grade open-source receiver"`
4. `docs/05 §1 "Radio Astronomy"` and `"Aurora & High-Latitude Physics"`
5. `docs/01 §7.3 "HackRF as a front end for Pi 5 / Orin Nano (± FPGA)"`
6. `docs/04 §10.3 "Spur identification and removal"`
