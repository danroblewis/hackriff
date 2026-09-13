# C30 · event-correlation
> Layer F — Explain · Status: taxonomy frozen 2026-09-13 (resolved in docs/06 §5) · Depends on: C06, C08, C12, C17, C26, C27, C29 (optional C05, C34, C38) · Used by: C17, C27, C28, C39

## Purpose
Answers "why did my spectrum change?" by joining local anomalies with external events and the device's own history by time, frequency and geometry, producing ranked Explanations with confidence and evidence. It is the radio "attack map" (docs/05 §3; AWARE-044), serving science (flares, propagation) as much as awareness (jamming, satellites). Time coincidence first, geometry second (docs/06).

C30 sits in the **Layer E loops** (docs/06 §5, §2.1): it and C27 (signal-inventory) and C17 (known-signal-priors) read and write each other — the inventory and priors feed correlation, and correlation writes explanation/status back to them. C30 also feeds C28 (a user confirms or corrects explanations) and uses C29's satellite-pass windows (computed by C29 from cached TLEs, not by C30).

## Interface
- **Input `LocalAnomaly`** (provisional), from C08, C12, C26 and C27:
  - type {noise-floor step, occupancy change, novelty, new emitter, decode-rate loss, distant station appears};
  - t_start/t_end, frequency range, magnitude, site position;
  - provenance snapshot (gain, clipping, filter, cal id).
- **Other inputs:** ExternalEvents with coverage/freshness (C29); position and time (C06).
- **Output `Explanation`:**
  - anomaly id;
  - ranked hypotheses [{rule id, event ids, score 0–1, lag, geometry values, evidence links to tiles, recordings and feed payloads}];
  - explicit `self-inflicted` and `unexplained` hypotheses;
  - feed coverage at evaluation time, version, and a `provisional` flag when feeds are stale.
- **Queries:** explanations by region/time; anomalies left unexplained; events that explained ≥1 anomaly (the attack-map view in C39).
- **Config:** rule weights, lag windows, geometry thresholds, display threshold.

## Methods
1. **Self-explanations first:** gain or filter change, ADC clipping, scheduler retune, spur-mask or calibration update, temperature (provenance from C01/C05; docs/04 §10.4). On an 8-bit front end with no preselector these are common.
2. **Candidate generation by interval join:** anomaly window ± a rule-specific lag against event windows. Lags and thresholds below are estimates to tune.
   - **Flare** (GOES X-ray, R-scale) → HF noise or beacon drop (SPACE-012).
   - **Proton event** (S-scale) → multi-day HF blackout on polar paths (SPACE-032).
   - **Lightning within a radius** → VLF–HF crashes at near-zero lag (AWARE-032).
   - **Radiosonde launch or 00Z/12Z window** → 400–406 MHz bursts (AWARE-062).
   - **Satellite pass** (AOS–LOS, elevation above threshold, downlink in range) → noise bump or new emitter (AWARE-064).
   - **Es/tropo forecast or spot surge** → distant FM/TV/ADS-B stations appear (AWARE-060, PROP-023, SIGNAL-068).
   - **GNSS-jamming cell containing the site** → L-band noise rise (AWARE-001, AWARE-006).
   - **Global band collapse in WSPR/PSKReporter** → "not your station" (AWARE-059).
3. **Frequency and geometry filters:** band compatibility per rule. Geometry from C06: elevation, storm distance, launch-site distance, sunlit path.
4. **Scoring:** rule prior × temporal overlap × geometry × magnitude consistency, corrected by a **base rate**: how often the rule fires against time-shifted copies of the device's own history (C26/C27). Carry confidence and provenance everywhere (docs/04 §11.2).
5. **Baseline first:** anomalies only mean something against a learned local baseline of ≥24 h (docs/04 §11.2; SM.1880 in §3.9).
6. **Re-evaluate** when C29 backfills. Version explanations; don't overwrite them.

## Platform constraints
- **Offline:** run against cached and offline-computable events, mark results provisional, and re-run after sync.
- **Time quality bounds every join:** GNSS/1PPS discipline (docs/02 §1.8, §7.3 #5). Sweep rows carry one timestamp per ~0.75 s sweep (docs/01 §1.6; docs/04 §3.8).
- **Single device, no sensor mesh** (CLAUDE.md): multi-site coincidence (AWARE-043) is only approximated through public networks (spots, gpsjam).

## Prior art and reuse
- **None directly:** docs 01–05 identify no open-source correlation engine.
- **R&S ARGUS / CRFS alarm masks:** "emission not in the licence DB", "level above mask" (docs/04 §11.1). Commercial.
- **SDRangel SID and Satellite Tracker feature plugins:** flare and pass context inside a receiver (docs/03 §2.2). Licence: check.
- **gpsjam method (AWARE-001); WSPR/HamSCI baselines (AWARE-059).** Terms: check.

## Pitfalls
- **Spurious correlation:** something is always happening (Kp elevated, lightning somewhere, a satellite overhead). Without base-rate correction every anomaly gets "explained".
- **Front-end artifacts blamed on space weather:** evaluate self-explanations before external events.
- **Clock skew or timezone bugs** shift joins by seconds to hours. Coarse feeds (daily gpsjam) must not be joined as precise.
- **Stale inputs:** old TLEs give wrong pass windows; stale caches give confident wrong answers.
- **Missing data:** a coverage gap must lower confidence, not imply "no event".
- **Feedback loop:** an explained emitter marked known in C27 can mask a later genuine anomaly on the same frequency.
- **Over-claiming:** e.g., attributing jamming to actors (AWARE-004). Present evidence, not accusations.

## Testing
- **Flare:** recorded or synthetic HF noise-floor series with a step at a flare time from a recorded SWPC snapshot. The flare hypothesis ranks first with lag in window. With the feed shifted ±6 h, the score falls below the display threshold.
- **Radiosonde:** replay 400–406 MHz SigMF with bursts at 00Z/12Z plus a SondeHub snapshot; the sonde rule wins. Bursts at 03Z stay unexplained.
- **Satellite pass:** TLE fixture plus a synthetic noise bump during a computed pass at a fixed site gives a pass explanation; a bump outside the pass does not.
- **Self-inflicted:** an injected gain step coinciding with a Kp storm ranks `self-inflicted` first.
- **Stale or missing feeds:** a coverage gap marks results `provisional` with lower scores; backfill produces a new version.
- **Clock skew:** injected ±2 s and ±1 h offsets degrade scores measurably.
- **Precision/recall:** a scripted scenario library with ground-truth explanations and decoy events.
- **Needs live data:** real flares (unpredictable), real feed latency.

## Example use cases
Regenerated from `use-cases.yaml`:
- SPACE-002 — (event-correlation primary)
- SPACE-015 — "Space weather now" local dashboard
- SPACE-026 — (event-correlation primary)
- SPACE-032 — Polar cap absorption events
- AWARE-004 — (event-correlation primary)
- AWARE-043 — Multi-site coincidence via public networks
- AWARE-044 — "Why did my spectrum change?" event feed
- AWARE-060 — Sporadic-E / tropo "why am I hearing distant stations"
- AWARE-062 — Radiosonde launch correlation
- AWARE-068 — (event-correlation primary)

## Open questions
- **Anomaly contract (resolved owner, docs/06 §5):** the shared LocalAnomaly record emitted by C08/C12/C27 and consumed by C30 is a **doc 07 domain object** — defined in the data model.
- **Hand rules vs learned correlation:** start with rules plus the base-rate check?
- **Orbit geometry (resolved, docs/06 §5):** C29 computes SGP4 passes from cached TLEs; C30 consumes the pass windows.
- **Attack-map view (resolved, docs/06 §5):** C39 explicitly owns the attack-map dashboard and map/geo views.
- **Missing edges (resolved, docs/06 §2.1):** C30 → C28 (user confirms explanations); C30 → C04 (dwell to confirm) is also carried.

## Reading list
1. docs/05 §3 "Spectrum Situational Awareness, Interference & Anomalies" (AWARE-043/044; "Space Weather, Propagation & Natural Explainers"; "Satellite & Space-Segment Interference")
2. docs/04 §11.2 "Mapping to an exploration device" (design principles)
3. docs/04 §10.4 "Dynamic range management"
4. docs/04 §3.9 "Occupancy statistics methodology (ITU-R SM.1880 / SM.2256)"
5. docs/02 §1.8 "Clock accuracy (TCXO/OCXO/GPSDO)"
6. docs/06 §2 "Capability taxonomy (39 capabilities)" (C08, C12, C27, C29, C30 rows)
