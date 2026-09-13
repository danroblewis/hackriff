# C32 · coherent-df-tdoa
> Layer G — Specialised · Status: taxonomy frozen 2026-09-13 (resolved in docs/06 §5) · Depends on: C01, C05, C06, C09, C11, C29 · Used by: C31, C27, C30, C39

## Purpose
Gives bearings and position fixes from phase or time differences, which one amplitude-only channel can't. Three modes:
- pseudo-Doppler on one HackRF with a switched array;
- a coherent array accessory (KrakenSDR-class);
- TDoA using public GPS-timed receivers.

It serves interferer and pirate hunting and emitter geolocation in the attack map (workflow step 4).

C32 **owns coherent/phase/interferometry/TDoA** bearings and fixes (docs/06 §5); amplitude bearings (Yagi max, body-null) belong to C31, into whose posterior C32's wedge likelihoods fuse. Multi-site TDoA works only with **public** remote receivers (e.g. KiwiSDR); a network of the user's own receivers is out of scope per CLAUDE.md.

## Interface
- **Inputs:**
  - Target selection: a Detection or emitter id (C09/C27).
  - Array IQ, as either:
    - time-multiplexed single-channel IQ plus switch timing (pseudo-Doppler), or
    - N coherent streams.
  - Remote timestamped IQ, e.g. KiwiSDR via C29.
  - Array geometry and phase calibration.
  - Pose and time (C06).
- **Output `Bearing` (provisional):**
  - Azimuth (deg true) with 1σ and quality.
  - Ambiguity flags (front/back, multipath); timestamp and pose.
- **Output `PositionFix` (provisional):** lat, lon, error ellipse and method (bearing intersection or TDoA).
- **Config:**
  - Mode; element count; switching rate.
  - DoA algorithm (MUSIC/Bartlett/MVDR); calibration interval.
  - TDoA correlation window and reference transmitter.
- **Accuracy** (`docs/04 §9.1 "Techniques"`):
  - pseudo-Doppler: "several degrees";
  - Watson-Watt: ~2–3° RMS;
  - interferometry: ~1–2° with calibration;
  - MUSIC: high resolution.
  - TDoA: 1 ns timing error ≈ 30 cm range difference.

## Methods
- **Pseudo-Doppler:**
  - Commutating the array imposes phase modulation at the commutation rate. The bearing is that tone's phase against the switch reference.
  - Opera Cake **time mode** cycles ports every N samples, "intended for pseudo-Doppler" (`docs/01 §1.5 "Opera Cake antenna switch"`), with up to 8 ports per board.
- **Switched Watson-Watt:** `θ = atan2(V_NS, V_EW)`, listed as "3-channel or switched" (§9.1). Opera Cake feasibility is unverified.
- **Coherent array:**
  - Needs a shared LO and sample clock plus periodic phase calibration (`docs/02 §1.9 "Coherent multi-channel (for direction finding)"`).
  - Interferometry: `Δφ = 2π·d·sinθ/λ`, with `d ≤ λ/2`.
  - MUSIC: `θ̂ = argmax 1/(aᴴ(θ)VₙVₙᴴa(θ))`. Elements must outnumber sources; a UCA avoids front/back ambiguity.
  - Default: consume KrakenSDR's DoA output rather than reimplementing it.
- **TDoA:**
  - Cross-correlate the target across ≥3 time-synchronised receivers, then multilaterate the hyperbolae.
  - Synchronise by GPS or a reference transmitter (DAB/FM at a known site; Panoradio method).
  - On-device path: public KiwiSDR IQ (AWARE-050).
- **Fusion:**
  - Bearings from several points along a walk or drive give least-squares intersections.
  - Export wedge likelihoods to C31 (`docs/04 §9.2 "Portable device: GPS + RSSI mapping"`).

## Platform constraints
- **Single channel:** HackRF One can't be phase-coherent multi-channel. Only time-switched pseudo-Doppler works (`docs/01 §7.3 "HackRF as a front end for Pi 5 / Orin Nano (± FPGA)"`).
- **`hardware_fit` by mode:**
  - **Pseudo-Doppler:** `needs-accessory`. Opera Cake (1 MHz–4 GHz, `docs/02 §5`) plus a circular array. Sample-accurate switch timing in host IQ is unverified.
  - **Interferometry/MUSIC:** `needs-other-sdr` (`docs/02 §2.1 "Master spec table"`):
    - KrakenSDR: 24–1766 MHz, 5 coherent channels at ~2.4 MHz each, 8-bit, USB 2.0, auto-cal;
    - RSPduo: 2 channels at 2 MHz each;
    - B210/bladeRF: 2 channels, phase ambiguity per retune.
  - **Two HackRFs on shared CLKIN/CLKOUT:** timing/sync only "Partial" (`docs/01 §7.3`). Phase coherence isn't established, so it's a spike, not a mode. HackRF Pro trigger in/out may help (`docs/01 §1.7`).
  - **TDoA with the user's own sites:** a sensor network, out of scope per CLAUDE.md. TDoA via **public** receivers is in scope.
- **Clock:**
  - HackRF One has a plain crystal, no TCXO; ±20 ppm is unverified (`docs/01 §1.2 "Specifications"`).
  - A GPSDO/1PPS is "needed for coherent DF/TDOA" (`docs/04 §10.1 "Frequency calibration"`; `docs/02 §1.8`).
  - CLKIN is selected only at RX start.
- **Compute:** medium (docs/06). Narrowband 5-channel MUSIC is light on the Jetson (estimate).

## Prior art and reuse
- **KrakenSDR + krakensdr_doa** (AWARE-049): noise-source auto-cal, Pi-hosted DSP; the array DoA template. Licence: check.
- **Panoradio TDoA** (`DC9ST/tdoa-evaluation-rtlsdr`): reference-transmitter sync. Licence: check.
- **hcab14/TDoA** (AWARE-050): KiwiSDR TDoA. Licence: check.
- **PySDR DOA chapter:** algorithm reference (`docs/04 §9.1`).
- **IQEngine:** TDOA/DOA is an envisioned plugin category (`docs/03 §3.4`).

## Pitfalls
- **Multipath** biases bearings; pseudo-Doppler is "susceptible to multipath". Report quality and average across positions.
- **Coherence loss** after retune or temperature change. Recalibrate after each tune.
- **Array ambiguities:** `d > λ/2` gives phase ambiguity; linear arrays have front/back ambiguity; more sources than elements breaks MUSIC.
- **Pseudo-Doppler switching:** transients and a commutation rate too close to the signal bandwidth. Blank glitches; parameters need a spike.
- **Heading:** compass error maps directly to bearing error (C06).
- **TDoA:**
  - collinear receiver geometry;
  - per-receiver HF skywave modes;
  - timestamp latency on public receivers.
- **Short bursts** give few snapshots. Trigger captures from C09.

## Testing
- **Synthetic:**
  - UCA snapshots at known bearings across SNR and phase-cal error. Assert MUSIC error within the §9.1 ranges after calibration.
  - Commutated single-channel IQ → pseudo-Doppler recovery.
  - TDoA with 1 ns jitter → check fit error against the 30 cm/ns rule.
- **Fixtures:**
  - KrakenSDR multi-file SigMF of an FM transmitter at a known site.
  - Public KiwiSDR captures of a known HF broadcaster. Check licences of external sets.
- **Live:** Opera Cake time-mode timing spike; array calibration; drive-by triangulation. Needs the accessory, GNSS/compass and a known transmitter.

## Example use cases
Regenerated from `use-cases.yaml` (all `needs-other-sdr`/`needs-accessory`):
- SPACE-045 — (coherent-df primary)
- PROP-028 — (coherent-df primary)
- AWARE-049 — Coherent 5-channel direction finding
- AWARE-050 — HF TDoA via public GPS-timed receivers
- PROP-010 — Long-path vs short-path detection
- SIGNAL-076 — VHF collar / radio-tracking

## Open questions
- **External arrays:** does C01 model KrakenSDR as a multi-channel source (a `CoherentGroup` shared with C35), or does C32 ingest bearings from KrakenSDR's software over IPC? Licence/process-boundary question.
- **Spike:** Opera Cake time-mode switch-timing observability and achievable pseudo-Doppler accuracy on HackRF One.
- **Spike:** two HackRFs on shared CLKIN — enough phase stability for 2-element interferometry or C35? (Same Phase 4 spike as C35, docs/06 §5.)
- **Scope (resolved, docs/06 §5):** TDoA uses *public* remote receivers only; own multi-site TDoA is out of scope.
- **Boundary with C31 (resolved, docs/06 §5):** amplitude bearings (Yagi max, body-null) are C31; coherent/phase/TDoA are C32.

## Reading list
1. `docs/04 §9.1 "Techniques"`
2. `docs/02 §1.9 "Coherent multi-channel (for direction finding)"`
3. `docs/01 §1.5 "Opera Cake antenna switch"`
4. `docs/01 §7.3 "HackRF as a front end for Pi 5 / Orin Nano (± FPGA)"`
5. `docs/02 §1.8 "Clock accuracy (TCXO/OCXO/GPSDO)"`
6. `docs/05 §3 "Emitter Identification, DF & Geolocation"`
