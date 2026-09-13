# C36 · gnss-observables
> Layer G — Specialised · Status: draft (taxonomy draft 2026-09-13) · Depends on: C01, C03, C04, C05, C08, C22, C25 · Used by: C06, C12, C27, C30

## Purpose
Runs a software GNSS receiver (GNSS-SDR class) on raw L-band IQ. It produces per-satellite C/N0, nav data, pseudorange/phase and integrity flags. This serves science (TEC, scintillation, GNSS-IR) and the attack map: local jamming and spoofing **detection**, joined with gpsjam-style context. Workflow steps: 2 (scheduled L-band dwells), 3 (C/N0 history), 4 (known constellations). Spoofing *generation* is out of scope.

## Interface
- **Inputs:** L1 dwell IQ from C03 (1575.42 MHz, `docs/04 §1.2`) at ≥8 Msps (HackRF minimum recommended, `docs/01 §1.2`), decimated in-chain. C01 provenance (gain, clip count, bias-tee, clock source). Optional L5 dwell. Time/position seed from C06.
- **Outputs (provisional objects):**
  - `GnssObservableEpoch`: per SV, C/N0, Doppler, pseudorange, carrier phase, lock time. 1 Hz logging; 10–50 Hz for scintillation (estimate).
  - `GnssNavMessage`: subframes, ephemerides, SBAS, OSNMA tags.
  - `GnssIntegrityEvent`: {jamming-suspect, spoofing-suspect, OSNMA-fail, clock-jump}, with evidence and confidence.
  - `PvtSolution`: to C06 when the SDR is the GNSS source.
- **Control:** constellation/band set, dwell request to C04, bias-tee antenna profile, integrity thresholds, raw-IF snippet recording via C25.

## Methods
- **Recommended:** wrap GNSS-SDR as a C22 plugin over IPC; don't reimplement tracking loops.
- **Acquisition:** FFT parallel code-phase search over the 1 ms C/A period. GNSS is below the noise floor, so energy detection fails and C09 won't "see" L1 (`docs/04 §4.9`).
- **Jamming** (AWARE-002):
  - A uniform C/N0 drop across all SVs plus a rise in the C08 L1 floor means jamming; a single-SV drop means blockage.
  - HackRF has no AGC readout, so use in-band power versus the calibrated floor as the "AGC" proxy.
  - Swept-tone jammers (AWARE-005) are spectral: C07/C09.
- **Spoofing tell-tales** (AWARE-003):
  - Equal power across SVs.
  - Impossible PVT jumps.
  - Clock versus C06/NTP inconsistency.
  - Broadcast ephemeris versus cached almanac (galmon style).
  - Galileo OSNMA failure (SIGNAL-031).
- **Science:** S4 and phase scintillation from high-rate observables (PROP-033); slant TEC needs dual-frequency phase (SPACE-023); SNR versus elevation for GNSS-IR (PROP-039).

## Platform constraints
- **Antenna:** an active antenna needs the bias-tee, max 50 mA at 3.0–3.3 V (`docs/01 §1.2`). Check the antenna's voltage range (unverified).
- **Clock:**
  - HackRF One has **no TCXO** (ppm unverified). This widens the Doppler search and degrades phase products.
  - Use 10 MHz CLKIN or a GPSDO (`docs/02 §1.8`). HackRF Pro has a 0.5 ppm TCXO (`docs/01 §1.7`).
- **Front end:**
  - One ≤20 MHz half-duplex window: L1 and L5 (~400 MHz apart) can't be simultaneous, so true dual-frequency TEC is `needs-other-sdr`.
  - No preselector, with Inmarsat 1525–1559 and Iridium 1616–1626.5 MHz adjacent (`docs/04 §1.2`).
- **Attention:** continuous tracking monopolises the only radio (C04).
- **Compute:** docs/06 says "medium–high continuous CPU". No benchmark in docs; measure per power mode.

## Prior art and reuse
- **GNSS-SDR:** full receiver (GPS, Galileo, SBAS). Licence: check.
- **galmon:** nav archiving and forensics (SIGNAL-032). Licence: check.
- **OSNMAlib:** OSNMA verification. Licence: check.
- **RTKLIB:** TEC (SPACE-023 ref). Licence: check.
- **gnssrefl:** GNSS-IR (PROP-039 ref). Licence: check.
- **Maintenance:** status of all five is not assessed in the docs. Doc 03 has no GNSS section.
- **Mayhem "GPS Sim"** (`docs/01 §3.3`): **do not inherit.** GNSS transmission is illegal (`docs/04 §1.3`).

## Pitfalls
- A handheld indoors or near the body looks like jamming. Require an all-SV drop plus a floor rise.
- A bias-tee left on into a passive or DC-shorted port. Put the bias-tee state in provenance and the UI.
- Cellular or Inmarsat IMD raises the L1 floor and creates false jamming flags. Cross-check C05 gain-step tests.
- DC spike at band centre (`docs/01 §1.3`): tune L1 off-centre.
- Clock drift without a time aid produces false "spoofing" clock jumps.
- Time-sliced dwells break carrier-phase continuity needed for scintillation and TEC.
- OSNMA needs accurate time and a cached public key offline (C29).

## Testing
- **Fixtures:** own L1 SigMF captures (≥8 Msps, 60–300 s) with a surveyed antenna point and a reference u-blox log. Public GNSS-SDR samples if licence allows (existence unverified).
- **Assertions:** ≥4 SVs acquired; C/N0 within ±3 dB of the reference (estimate); PVT error versus survey point; nav CRC pass rate; OSNMA status on a known-good capture.
- **Synthetic jamming:** add noise and a chirp to recorded IQ at stepped J/S. The flag must fire; single-SV attenuation must not.
- **Spoofing logic:** test on mocked observables (equal power, jumps, clock steps). **Build no GNSS RF generator.** Any third-party simulator output stays file-only, never routed to C37.
- **Live hardware:** antenna/bias-tee compatibility, TTFF, CPU and thermals per power mode.

## Example use cases
Provisional until docs/06 §3 mapping:
- AWARE-002 — Local GNSS C/N0 and AGC watchdog
- AWARE-003 — Spoofing tell-tales detector
- SIGNAL-030 — GPS L1 C/A + SBAS/WAAS raw processing
- SIGNAL-031 — Galileo OSNMA authentication
- SIGNAL-032 — GNSS constellation forensics
- PROP-033 — GNSS scintillation with SDR
- SPACE-007 — Solar radio bursts vs GNSS
- SPACE-023 — GNSS total electron content
- PROP-039 — GNSS-IR snow depth
- PROP-080 — Antenna patterns from satellite passes

## Open questions
- **C06 source:** should it use a dedicated GNSS module so C36 runs only on scheduled dwells? This is an attention-budget ADR.
- **"Front-end AGC" in docs/06 C36:** HackRF has none. Reword as "in-band power / noise-floor proxy"?
- **Dual-frequency products:** mark `needs-other-sdr` in §3?
- **AWARE-005:** confirm it maps to C09/C10 first, with C36 secondary.
- **GNSS-IR:** docs/06 C36 omits it (SNR versus elevation, no nav solution).
- **GNSS-SDR:** process boundary, licence isolation, and CPU fit alongside C07/C11.

## Reading list
1. `docs/05 §3 "GNSS Jamming, Spoofing & Navigation Integrity"` — target behaviours and refs.
2. `docs/04 §4.9 "DSSS detection"` — correlation, not energy.
3. `docs/01 §1.2 "Specifications"` — bias-tee, clock, sample-rate limits.
4. `docs/05 §2 "Satellites & GNSS: ionosphere and troposphere"` — science products.
5. `docs/02 §1.8 "Clock accuracy (TCXO/OCXO/GPSDO)"`.
6. `docs/04 §1.3 "Legal considerations (US; not legal advice)"`.
