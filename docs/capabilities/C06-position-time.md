# C06 · position-time
> Layer A — Acquire · Status: taxonomy frozen 2026-09-13 (resolved in docs/06 §5) · Depends on: — (GNSS receiver, optional IMU/compass, optional GPSDO; C36 if the SDR is the GNSS source) · Used by: C01, C03, C05, C17, C25, C26, C30, C31, C32, C34

## Purpose
Provides position, altitude, heading and trustworthy UTC so every record is geotagged and time-aligned. It can also help discipline the HackRF clock — but the HackRF One has **no hardware 1PPS input**, so the only options are a 10 MHz GPSDO into CLKIN or software ppm/time correction from GNSS, never PPS discipline of the HackRF itself (docs/06 §5). It enables RSSI mapping, satellite geometry, licence lookup by location, and joining local anomalies to external feeds (the "attack map"). It serves workflow steps 3–4 and must work offline.

## Interface
- **Outputs (provisional):**
  - `PositionFix`: UTC, lat/lon/alt, fix type, accuracy, satellites, speed/course.
  - `Heading`: value, source (IMU/compass/course), accuracy.
  - `TimeState`: host-clock offset vs GNSS, PPS lock, holdover age, uncertainty.
  - `ClockRef`: GPSDO lock, for C01 provenance.
- **Queries:** `position_at(t)` and `heading_at(t)` with interpolation and a staleness flag. Used by C03/C25/C26 for metadata and by C31 per burst.
- **Rates (estimates; hardware not chosen):** fixes 1–10 Hz, PPS 1 Hz, IMU tens of Hz.
- **Integrity flags:** no-fix, stale, jump-detected, time-inconsistent.

## Methods
- **Hardware (docs 01–05 don't research a GNSS module, so everything here is an estimate):**
  - Separate low-power GNSS module over UART/USB, PPS to a Jetson GPIO.
  - Host clock disciplined by a standard NTP/PPS daemon (choice unverified; spike).
  - Magnetometer/IMU heading when stationary; GNSS course when moving.
- **HackRF clock options** (docs/01 §1.2 "Specifications"; docs/02 §1.8 "Clock accuracy (TCXO/OCXO/GPSDO)"):
  1. GPSDO 10 MHz into CLKIN: ~1e-12 long-term plus 1PPS (e.g. Leo Bodnar LBE-1420).
  2. Software correction with ε_ppm from C05.
  3. Pro: 0.5 ppm TCXO plus trigger in/out. Marking PPS edges in-stream via trigger-in is an unverified idea (docs/01 §1.7 "HackRF Pro (codename "Praline")").
- **Sample time:** index → UTC through the PPS-disciplined host clock at stream start. USB latency residual is unquantified; spike.
- **RSSI mapping needs:** per burst (t, lat, lon, heading, peak RSSI), and prompts to walk loops for geometry (docs/04 §9.2 "Portable device: GPS + RSSI mapping").
- **TDoA:** 1 ns error ≈ 30 cm range difference, so GPS-disciplined time is mandatory (docs/04 §9.1 "Techniques").
- **Integrity:** reject impossible jumps and GNSS-time vs holdover disagreement, following spoofing tell-tales (docs/05 §3 "Spectrum Situational Awareness, Interference & Anomalies", AWARE-003).
- **Fallback:** the HackRF as GNSS receiver via GNSS-SDR (C36), with an active antenna on bias-tee. It occupies the only radio, so snapshot fixes only.

## Platform constraints
- HackRF One: CLKIN expects 10 MHz 3.3 V, switched only at RX/TX start. No PPS input, no TCXO (docs/01 §1.2 "Specifications").
- Bias-tee max 50 mA at 3.0–3.3 V, enough for an active GNSS antenna on the SDR path (docs/01 §1.2).
- Power (docs/02 §7.2 "Power budget sketches"): Tier A front end incl. GPSDO ~0.5–1 W; Tier B GPSDO ~1–2 W. Make the GPSDO switchable on a 99 Wh pack.
- Handheld use indoors or in vehicles gives intermittent fixes. Design for holdover and "last known position" with age.
- Jetson RTC/holdover behaviour: not covered in docs (unverified).

## Prior art and reuse
- **Mayhem Capture `.TXT`:** optional GPS latitude/longitude/satinuse, the minimal precedent (docs/01 §3.5 "Recording and replay formats").
- **SDRangel `heatmap`:** GPS-tagged power mapping, CSV/image export (docs/03 §2.2). Licence: check.
- **R&S FPH K16 and long spectrogram logs with GPS/heading:** copy-list items 3 and 6 (docs/02 §6 "Handheld and portable precedents: what they teach about finding signals").
- **GNSS-SDR:** fallback path and C36. Licence: check.
- **CRFS RFeye:** GPS multi-unit synchronisation (docs/02 §6).

## Pitfalls
- **GNSS is a victim too:** it gets jammed or spoofed during exactly the events the device should explain (AWARE-001…007). Degrade to holdover and report the loss itself as an anomaly to C30.
- **Self-interference:** Jetson, USB and DC-DC noise near L1 may degrade an in-enclosure GNSS antenna (unverified; measure).
- **Silent staleness:** interpolating across long no-fix gaps yields false RSSI maps. Enforce a staleness limit.
- **Time-scale errors:** GPS–UTC leap-second offset; host clock steps mid-capture. Step only between captures; slew otherwise.
- **Heading:** magnetometers are corrupted by the device's own currents and nearby metal. Record accuracy.
- **Privacy:** position logs are personal data. Exports should support coarsening or stripping.
- **Pro-only features:** PPS-trigger alignment would exist only on the Pro. Keep it optional in C01's capability descriptor.

## Testing
- **Replay:** NMEA logs from walks and drives. Also a synthetic track with injected jumps, dropouts and clock steps.
- **Assertions:**
  - Bounded interpolation error.
  - Stale flag after timeout.
  - Jump detector fires on a 1 km step.
  - Monotonic timestamps across a simulated clock step.
- **SigMF round trip:** C25 captures carry C06 position/time.
- **SDR fixture:** HackRF L1 1575.42 MHz capture (active antenna, bias-tee) through GNSS-SDR. Assert a position/time solution.
- **Live:**
  - PPS jitter vs host clock (target TBD).
  - Time-to-first-fix.
  - GPSDO CLKIN lock and resulting ppm (with C05).
  - Heading accuracy on a walked loop.

## Example use cases
Regenerated from `use-cases.yaml` (primary, then notable secondary):
- SPACE-044 — (position-time primary)
- PROP-076 — (position-time primary)
- PROP-078 — LoRa terrain link-budget mapping
- PROP-077 — Time-station propagation delay
- AWARE-015 — City-wide wardriving cell anomaly map
- AWARE-009 — ADS-B RSSI-vs-distance plausibility check
- AWARE-003 — Spoofing tell-tales detector
- SIGNAL-079 — (RSSI/position use case)

## Open questions
- The "1PPS discipline of the HackRF clock" wording is corrected in docs/06 §5: no PPS input on HackRF One; discipline is via a 10 MHz GPSDO into CLKIN or software ppm correction.
- GPSDO in base BOM or accessory? This affects `hardware_fit` for PROP-077 and AWARE-050. (A Phase 3 hardware ADR.)
- docs/06 §2.1 now shows C06 → C12, C30, C31, C34, C37; C01/C03 (timestamps), C05 (reference), C17 (location priors) and C25/C26 (geotags) also depend on it.
- GNSS module, IMU and clock daemon are unresearched in docs 01–05; short spike.
- Which SigMF fields carry position/time: decide with docs/07.

## Reading list
1. docs/02 §1.8 "Clock accuracy (TCXO/OCXO/GPSDO)"
2. docs/04 §9.2 "Portable device: GPS + RSSI mapping"
3. docs/01 §1.2 "Specifications"
4. docs/04 §9.1 "Techniques"
5. docs/02 §7.2 "Power budget sketches"
6. docs/05 §3 "Spectrum Situational Awareness, Interference & Anomalies"
