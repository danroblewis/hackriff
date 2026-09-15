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

## Fit
- Geometric detection needs no extra hardware and runs on this device's own history.
- Multipath by content needs decode identity, so it follows M3 fingerprinting and MAUTO's decode evidence.
- Direction-finding of the reflector is out of scope on one antenna (see C33/C35).

## Reading list
- docs/04 §2 (front-end artifacts, spur rejection), §7.6-7.7 (fingerprints, clustering)
- ADR-0012 §2.6 (DC twin rule: the same "artifact of the receiver, not the air" idea)
- docs/15 §10 (candidate as pipeline; overlap resolution)
