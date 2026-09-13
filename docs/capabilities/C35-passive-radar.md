# C35 · passive-radar
> Layer G — Specialised · Status: draft (taxonomy draft 2026-09-13) · Depends on: C01, C05, C06, C09, C17, C22, C29 · Used by: C30, C39

## Purpose
Passive coherent location. It cross-correlates a **reference** channel (direct illuminator) with a **surveillance** channel (echoes) into range–Doppler maps. Illuminators are broadcast signals (FM, DAB, DVB-T, cellular). The result is aircraft, drone, meteor or plasma detections with ADS-B truth overlay: receive-only RF-sensing science (docs/05 §2). A single HackRF One cannot do it, so this capability waits for a multi-channel front end.

## Interface
- **Inputs:**
  - ≥2 time-aligned, phase-coherent IQ streams (reference toward illuminator, surveillance toward target area), with timestamps and coherence-cal state.
  - Illuminator record (C17): frequency, site, bandwidth.
  - Receiver pose (C06).
  - ADS-B truth: C22 readsb or an online feed (C29).
- **Output `RangeDopplerMap` (provisional):** bistatic range (m) × Doppler (Hz) power (dB); CPI; timestamp; illuminator id; cancellation depth (dB).
- **Output `RadarDetection` / `RadarTrack` (provisional):** range, Doppler, SNR; track id; associated ICAO and residuals.
- **Config:** CPI; max range and Doppler; cancellation method and taps; window; CFAR parameters.
- **Resolution** (standard relations, not in docs):
  - bistatic range ≈ c/B;
  - Doppler ≈ 1/CPI.
  - Wider illuminators give finer range: DVB-T is used for "finer range resolution" than FM (PROP-053).

## Methods
Standard passive coherent location practice. Docs 01–05 don't describe the algorithms, so each step needs a spike.
1. **Channel alignment:** remove inter-channel delay, phase and frequency offsets using the direct-path peak or the array noise-source cal (KrakenSDR model, `docs/02 §1.9 "Coherent multi-channel (for direction finding)"`).
2. **Direct-path and clutter cancellation:** least-squares projection over range/Doppler taps (ECA-style). Alternative: adaptive LMS.
3. **Cross-ambiguity function:** `χ(τ, f_D) = Σ s[n]·r*[n−τ]·e^{−j2π f_D n/fs}`, as batched FFTs on the GPU (cuFFT/CuPy, per docs/06 C07).
4. **Detection:** 2-D CFAR (reuse C09; `docs/04 §3.4 "CFAR detection across frequency (and time)"`).
5. **Tracking:** α-β/Kalman in range–Doppler, then association with ADS-B positions projected to bistatic coordinates.
6. **Illuminators:**
   - FM (PROP-051) first.
   - OFDM DVB-T/DAB (PROP-053/054) allow reference reconstruction by demod/remod (standard; not in docs).
   - LTE/5G SSB (PROP-055).
- **Single-channel fallback (belongs to C34):** echo spectrograms on a known illuminator give Doppler without range (SPACE-051).

## Platform constraints
- **Always `needs-other-sdr` on a single HackRF:** one channel, half-duplex, no phase-coherent multi-channel (`docs/01 §7.3 "HackRF as a front end for Pi 5 / Orin Nano (± FPGA)"`; docs/06 C35).
  - Opera Cake switching between antennas isn't simultaneous, so correlation breaks. Whether any useful variant exists is unverified.
- **Coherent options** (`docs/02 §2.1 "Master spec table"`):
  - **KrakenSDR:** 5 channels, 24–1766 MHz, ~2.4 MHz each, 8-bit, USB 2.0, built-in cal. Channel bandwidth caps illuminator bandwidth and so range resolution.
  - **RSPduo:** 2 channels at 2 MHz.
  - **AD9361 2×2** (bladeRF/B210): wider, but the phase ambiguity must be calibrated after each retune.
- **Two HackRFs on shared CLKIN/CLKOUT:**
  - Frequency-locked, but relative phase and start time are not established ("Partial" sync, `docs/01 §7.3`). Pro trigger in/out may help (`docs/01 §1.7`).
  - Two 20 Msps streams need 2 × 40 MB/s, beyond one USB 2.0 bus (derived from `docs/01 §1.4 "USB 2.0 throughput ceiling"`). Use lower rates or separate host controllers (check on the Jetson).
- **8-bit dynamic range** (~48 dB theoretical, "closer to 6 bits", `docs/01 §1.3`) limits the direct-path/echo ratio. The surveillance antenna must be directional with a null toward the illuminator: `needs-accessory`.
- **ADS-B truth** needs a second simultaneous 1090 MHz receiver or connectivity (C29).
- **Clock:** shared LO/clock across channels is mandatory; GPSDO for multi-unit work (`docs/02 §1.8`).
- **Compute:** high (docs/06). No Jetson benchmark in the docs; spike.

## Prior art and reuse
- **blah2** (PROP-052): real-time KrakenSDR passive radar, web range–Doppler display, ADS-B overlay. Architecture template. Licence and maintenance: check.
- **krakensdr_pr** (PROP-052): KrakenSDR passive radar. Licence: check.
- **passiveRadar, Max-Manning** (PROP-051): FM PCL reference. Licence: check.
- **jmfriedt/passive_radar** (PROP-053): DVB-T with synchronised RTL-SDRs, showing how to sync non-coherent dongles. Licence: check.
- **tar1090** (PROP-052): ADS-B truth display. Licence: check.

## Pitfalls
- **Direct-path leakage and clutter:** incomplete cancellation hides targets near zero Doppler.
- **Illuminator content:** FM programme content (silence, bass-heavy audio) changes the ambiguity function; OFDM pilots and cyclic prefix create ambiguity spikes (standard knowledge, not in docs).
- **Coherence drift** between channels over long CPIs. Recalibrate periodically.
- **Target motion:** range migration and Doppler spread cap the CPI.
- **Bistatic geometry:** a detection is an ellipse, not a position. Needs illuminator location (C17), ideally several illuminators.
- **Broadcast overload:** strong local FM saturates 8-bit front ends. Use filters.
- **Privacy:** indoor Wi-Fi radar (PROP-060) tracks people. Policy TBD.

## Testing
- **Synthetic:**
  - Noise-like reference (FM-like and OFDM-like). Surveillance = attenuated direct path + clutter + targets at known delay/Doppler 40–60 dB below direct (estimate) + noise.
  - Assert cancellation depth ≥ target (TBD), targets in the correct bins, and CFAR false-alarm rate within config.
  - Add an 8-bit quantisation variant.
- **Fixtures:**
  - KrakenSDR multi-channel SigMF of FM illuminators near an airport, plus a time-matched readsb JSON truth log.
  - A single HackRF **cannot** capture these. Check licences of public recordings.
- **Live:** real-time map rate on the Jetson; hours-long coherence stability. Needs a coherent SDR, directional antennas and an airport nearby.

## Example use cases
Provisional until docs/06 §3 mapping:
- PROP-051 — FM passive radar for aircraft
- PROP-052 — Real-time passive radar with KrakenSDR/blah2
- PROP-053 — DVB-T passive radar with synced RTL-SDRs
- PROP-054 — DAB drone detection
- PROP-055 — LTE and 5G NR passive radar
- PROP-059 — Passive radar space surveillance
- PROP-022 — Passive radar of E-region irregularities
- PROP-058 — GNSS-based passive radar

## Open questions
- **Roadmap:** defer C35 until a multi-channel front end is chosen, or wrap blah2 as an external process (licence boundary)?
- **Spike:** can two frequency-locked HackRFs (shared CLKIN, Pro trigger) reach FM-radar phase stability? Same question as for C32 interferometry.
- **ADR:** should C01 expose a `CoherentGroup` (shared timestamps, cal state), shared with C32?
- **Policy:** Wi-Fi/people-sensing (PROP-060) isn't covered by CLAUDE.md guardrails.
- **docs/06 gap:** C35 inputs should list illuminator location (C17) and ADS-B truth (C22/C29).

## Reading list
1. `docs/05 §2 "Passive radar"`
2. `docs/02 §1.9 "Coherent multi-channel (for direction finding)"`
3. `docs/01 §7.3 "HackRF as a front end for Pi 5 / Orin Nano (± FPGA)"`
4. `docs/02 §2.1 "Master spec table"` (KrakenSDR, RSPduo, B210)
5. `docs/04 §3.4 "CFAR detection across frequency (and time)"`
6. `docs/01 §1.4 "USB 2.0 throughput ceiling"`
