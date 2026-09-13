# C31 · rssi-localization
> Layer G — Specialised · Status: taxonomy frozen 2026-09-13 (resolved in docs/06 §5) · Depends on: C01, C05, C06, C09, C10, C18 · Used by: C27, C30, C39 (C32 bearings fuse into its posterior)

## Purpose
Answers "where is it?" with the single HackRF. It logs burst power for one target while the user walks, fits an emitter location, and gives "hot/cold" homing guidance. It is the only localization method that needs no second SDR. It serves RFI, pirate and interferer hunts and link-budget mapping. It also gives inventory entries a location for review and the attack map (workflow steps 3–4).

C31 **owns amplitude bearings** — directional-antenna (Yagi/LPDA) rotate-to-peak and body-null sweeps (docs/06 §5). Coherent/phase/interferometry/TDoA bearings belong to C32; they fuse into C31's posterior as wedge likelihoods but are not C31's to produce.

## Interface
- **Inputs:**
  - Detections/Tracks (C09/C10) gated to one target by a fingerprint or emitter id (C18).
  - PositionFix stream (C06): t, lat, lon, alt, heading.
  - Block provenance (C01): gain, clip count, attenuator.
  - Optional bearing wedges: Yagi/body-null, or C32.
- **Output `LocationEstimate` (provisional):**
  - Posterior (grid or particles) and MAP position.
  - 50%/95% region radius (m).
  - Fitted `P0` (dBm, or dBFS if uncalibrated) and exponent `n`.
  - Measurement count and a geometry-quality score.
- **Output `HomingCue` (provisional):** smoothed RSSI trend, suggested direction, "add attenuation" prompt.
- **Config:**
  - Priors `n` ∈ 2–4 and shadowing σ ∈ 4–10 dB.
  - Estimator (particle filter or NLS); Huber δ; grid resolution (m).
- **Accuracy:** coarse, "tens to hundreds of m", and it needs many measurements (`docs/04 §9.1 "Techniques"`). The docs give no tighter target.

## Methods
- **Measurement** (`docs/04 §9.2 "Portable device: GPS + RSSI mapping"`):
  - Log `(t, lat, lon, heading, RSSI_burst)`.
  - Use **burst peak** power, then the median over each burst to reject fades.
  - Hold gain fixed; if it must change, correct via the C05 gain table.
- **Model:** `P_r(d) = P0 − 10·n·log10(d/d0) + X_σ`.
- **Fit:** minimise `Σ(RSSI_i − P0 + 10·n·log10‖x_i − p‖)²` with Huber loss.
  - Default: particle filter (incremental, multimodal, renders as a heat map).
  - Alternative: batch robust NLS after the walk.
- **Geometry:** prompt a loop walk so measurements come from diverse bearings.
- **Near the source:** switch in attenuation so RSSI keeps a gradient (`docs/04 §10.4 "Dynamic range management"`).
- **Bearing fusion:** each bearing becomes a wedge likelihood. Sources: Yagi/LPDA rotate-to-peak (~10–30°, §9.1), body-null sweeps, or C32.
- **Multi-session accumulation** for fixed emitters such as ISM meters (§9.2 step 5).

## Platform constraints
- **Hardware:** works on HackRF One + GNSS (C06). `hardware_fit`:
  - `native` if C06's GNSS counts as base hardware, else `needs-accessory`.
  - Directional antenna (PCB LPDA, `docs/02 §5 "Antennas and RF front-end accessories"`) and step attenuator: `needs-accessory`.
- **Dynamic range:** 8-bit ADC, ~48 dB theoretical, "closer to 6 bits" in practice (`docs/01 §1.3 "Noise figure, dynamic range, and overload"`). A far-to-near walk exceeds that, so gain or attenuation steps must be recorded in provenance.
- **Clipping and overload:**
  - Above 1e-4 full-scale samples per block, the measurement is suspect (`docs/04 §10.4`).
  - Max RX input is −5 dBm, a damage risk (`docs/01 §1.2 "Specifications"`), so use a limiter near transmitters.
- **No preselector:** IMD from strong neighbours can mimic the target.
- **Calibration:** HackRF can't make calibrated absolute amplitude measurements (`docs/01 §7.3`). Localization needs only within-session relative consistency; `P0` in dBm needs C05.
- **One window:** homing must pin the dwell, which competes with survey (C04).
- **Compute:** low. A 10³–10⁴-particle filter is trivial on the Jetson (estimate).

## Prior art and reuse
- **SDRangel `heatmap` plugin:** GPS-tagged power mapping with CSV/image export. Very active; licence: check (`docs/03 §2.2`).
- **rtl_power + heatmap.py:** survey heat maps, not localization. Stable/old (`docs/03 §3.1 "Wideband sweep / survey"`).
- **KrakenSDR DoA software:** bearing source via C32. Licence: check.
- **TTN Mapper** (PROP-078 ref): crowd RSSI-mapping UX.

## Pitfalls
- **Multipath fading and antenna effects:**
  - Large swings over a wavelength; the path-loss model fails indoors.
  - A handheld whip's pattern plus body shadowing varies RSSI with heading. Log heading; optionally blank the body-shadow sector.
- **Wrong or merged target:** co-channel emitters or identical device models merge into one fit. Gate strictly on fingerprint and burst timing.
- **Model violations:**
  - `n` varies between open and urban terrain; a single global `n` biases range.
  - Moving or duty-cycled emitters break the static model. Watch residual trends.
- **Hidden gain changes (AGC)** corrupt the gradient. Force manual gain.
- **Poor geometry:** straight-line walks give a mirror ambiguity. Show the geometry score.
- **Privacy:** locating other people's personal devices is sensitive. Output location metadata only, never content.

## Testing
- **Synthetic (offline):**
  - Emitter plus walk path with GNSS noise; RSSI from the log-distance model with `n` ∈ {2, 3, 4} and σ ∈ {4, 8, 10} dB; inject clipping and gain steps.
  - Assert the truth lies inside the 95% region in ≥90% of Monte-Carlo runs (target is an estimate). Error must shrink with loop diversity, and clipped blocks must be excluded.
- **Fixtures:**
  - Captures of a user-owned 433/915 MHz sensor or key fob during a recorded walk, with a GNSS track as sidecar or annotations. SigMF geolocation field support is unverified.
  - An FM broadcast site at a known location as a coarse sanity target.
- **Live/field:** homing UX, attenuator switching, body-null bearings. Needs GNSS, a directional antenna and outdoor space.

## Example use cases
Regenerated from `use-cases.yaml`:
- PROP-068 — (rssi-localization primary)
- PROP-069 — (rssi-localization primary)
- PROP-078 — LoRa terrain link-budget mapping
- SIGNAL-079 — (rssi-localization primary)
- PROP-081 — Directional emitter hunt
- PROP-082 — Path-loss / link-budget survey
- AWARE-008 — RSSI-based interferer hunt
- AWARE-009 — ADS-B RSSI-vs-distance plausibility check
- AWARE-052 — Pirate / unlicensed broadcaster hunting
- SIGNAL-076 — VHF collar / radio-tracking

## Open questions
- **Data model:** locations as fields on C27 emitters, or separate `LocationEstimate` history records? This waits on docs/07.
- **Policy** on localizing third-party personal devices: CLAUDE.md is silent. Needs a Phase 3 ADR.
- **Boundary with C32 (resolved, docs/06 §5):** amplitude bearings (Yagi max, body-null) are C31; coherent/phase/interferometry/TDoA bearings are C32.
- **`hardware_fit`:** is C06's GNSS base hardware (`native`) or an accessory? docs/06 §3 doesn't say.

## Reading list
1. `docs/04 §9.2 "Portable device: GPS + RSSI mapping"`
2. `docs/04 §9.1 "Techniques"`
3. `docs/04 §10.4 "Dynamic range management"`
4. `docs/03 §2.2 "SDRangel — the most engineering-grade open-source receiver"` (`heatmap`)
5. `docs/01 §1.3 "Noise figure, dynamic range, and overload"`
6. `docs/02 §5 "Antennas and RF front-end accessories"`
