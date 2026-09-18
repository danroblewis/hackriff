# C40 · signal-relationships

## Purpose
Assert that one detection is not an independent emission but **related to another**: a receiver artifact of it (image, harmonic, intermodulation) or the same emission arriving by another path (multipath). Duplicates that look like separate signals are attributed to their source instead of cluttering the inventory, and genuine multipath becomes a measurement (path difference) rather than a second station.

## Interface
- **Inputs:** confirmed/candidate emitters with centre, bandwidth and level; the tuning history (LO per observation, ADR-0012 §2.6 already tracks DC/LO); decoded identity (RDS PI, ADS-B ICAO, frame content) from M1/M3; raw IQ or audio for correlation.
- **Output: `Relationship { kind: image | harmonic | intermod | multipath | unrelated, source_emitter, target_emitter, evidence, params }`** where `params` carries the arithmetic (n, a, b, LO used) or the measured cross-correlation lag and path difference.
- **Unknown handling:** a coincidence that fits an arithmetic rule but lacks corroboration stays a ranked suggestion, never an automatic deletion. Relationships are reversible when evidence changes.

## Methods
- **Geometric (deterministic, M2-era).** For each strong confirmed emitter, predict image `2*LO - f`, harmonics `n*f`, and intermod `a*f1 +/- b*f2` for small a, b. A candidate landing on a predicted frequency, with a level consistent with the artifact mechanism and appearing only while the source is present, is flagged as that artifact. Front-end state (gain, overload) raises the prior. This is the same "same-source duplicate" mechanism as overlap dedup (T-219).
- **Content correlation (real multipath, M3/MAUTO-era).** Two detections that decode to the same content are the same emission: identical RDS PI/PS, identical ADS-B ICAO, or high audio/bit cross-correlation. The cross-correlation lag gives the path-difference in samples, hence metres.
- **Never truth:** a relationship is evidence, ranked with everything else, and is disclosed with its reasoning (the exploration-first rule).
- **A family against an unseen common cause (T-374, from T-317's method).** The two methods above both reason about **one emitter against one other thing** — a source row, or the device. Neither can say *"these emitters are harmonics of one fundamental nobody can see"*, which is the shape of a real local artefact: T-317 identified the FM capture's unexplained 100.465339 MHz emission as harmonic 43 of a free-running ~2.3364 MHz oscillator that sits outside every band ever tuned, so there is no source row to point at and the claim binds three emitters at once. `hk_model::harmonic` fits `f = n·f₀ + b` over a **set** of measured centres and judges it (docs/07 §2.32, `HarmonicFamily`):
  - **The residual** about the fitted line, at a tolerance taken from the measurement (the tighter of 5 ppm of frequency and 10 % of the narrowest member's width) and never from the prediction — the same rule that keeps `ARTIFACT_CENTER_BW_FRACTION` from widening with the order.
  - **The intercept pins the indices.** A uniform shift `n → n+1` leaves the slope and *every residual bit-identical* and moves `b` by exactly one `f₀`, so the residual can never choose a labelling; only the physics — a harmonic family passes through the **origin** — can. Recorded as `|b|/se(b)` **and** `|b|/f₀`, because the first is self-scaling and a sloppy fit would otherwise acquit itself. `se(b)/f₀` is the non-vacuity number: past ½ the labelling is a coin flip and no family is claimed.
  - **Width ∝ n**, the independent corroboration, because a fundamental's frequency noise multiplies with the harmonic number and the widths are a column the fit never touched. T-317 measured 138/120/137 Hz at n = 43/44/45. How much it *discriminates* depends on the index leverage, and the model records that rather than claiming more than it has.
  - **Line-shape correlation** where profiles exist (T-317 measured 0.967–0.988) — able only to reject, never to create a family.
  - **Device-local** like every other artifact claim: one `ReceiveChain` per family (T-302/T-259).
  - **It must be able to say no.** A fit over enough emitters always finds some `f₀`, so the control is unrelated real emitters and randomly drawn populations that must not be declared families — and it is the control, not intuition, that set the thresholds (an earlier draft declared 55.5 % of unrelated FM-band draws to be families).
- **Boundary with `hk_detect::comb`:** that fits `n × f₀` over spectral **lines inside one detection**; this fits it over **separate emitters in different bands**. Members whose measured bands overlap are refused here.

## Fit
- Geometric detection needs no extra hardware and runs on this device's own history.
- Harmonic-family detection needs only the inventory's measured centres and widths, so it also runs offline over recorded surveys.
- Multipath by content needs decode identity, so it follows M3 fingerprinting and MAUTO's decode evidence.
- Direction-finding of the reflector is out of scope on one antenna (see C33/C35).

## Reading list
- docs/04 §2 (front-end artifacts, spur rejection), §7.6-7.7 (fingerprints, clustering)
- ADR-0012 §2.6 (DC twin rule: the same "artifact of the receiver, not the air" idea)
- docs/15 §10 (candidate as pipeline; overlap resolution)
- docs/07 §2.32 (`HarmonicFamily`), `crates/hk-model/src/harmonic.rs`, migration `0014_harmonic_family.sql`
- `docs/planning-log.md` B0.654 (T-317's identification, and the figures the positive control reproduces)
