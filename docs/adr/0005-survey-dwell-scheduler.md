# ADR-0005 — Survey/dwell scheduler

**Status:** PROVISIONAL (core policy; first version simple, gated on spike S4 false-alarm/POI behaviour)
**Touches:** C04 (reads C02, C10, C12, C22, C23, C29, C37); ScanPlan/Survey ([docs/07](../07-data-model.md))

## Context

One HackRF: half-duplex, one ≤20 MHz window, ~8 GHz/s sweep. Sweeping finds *where*; dwelling finds *what*; a 5 ms burst is caught <1% of the time per sweep ([docs/04 §3.8](../04-radio-engineering-and-signals-analysis.md)). The scheduler decides where the single window points at every instant. It is the device's "attention" and, per Phase 0, the single hardest design problem. It cannot be deferred, but its first version can be simple.

## Decision (provisional)

- **Two interleaved activities:** a **discovery sweep** of the ScanPlan's full span (continuously updating per-bin max-hold/mean/SK and occupancy), and **dwells** on candidate regions sized to expected burst intervals.
  - **Off-DC view per cell (T-173).** A detection within the DC rule of its tuning's LO stays suspect unless another tuning sees it clean, so the sweep guarantees one by construction. Odd passes tune every discovery hop `dc_dither_hz` (75 kHz) from its even-pass centre: towards the middle of its band when the hop's slice leaves that much room either side, else upwards (the other way at a range or RF-path limit). 75 kHz exceeds twice the 15 kHz tolerance plus a 45 kHz extent (the 40 kHz widest DC-flagged width plus bin margin), so every covered cell has a clean view inside the usable span within two passes. Hop count, slices, even-pass centres and pass length are unchanged, so revisit time is unchanged. A band with no room loses at most its lowest 75 kHz beyond the usable span on odd passes; those cells are far from every LO on even passes. Plans whose usable span is below 4 × the dither are not dithered and warn `DcDitherDisabled`. Each pass parity has its own `SweepGeometry` (ADR-0012 §1.3).
- **Candidate score** from the docs/04 §2 features — occupancy, novelty (C12), bursty SK, known-signal priors (C17), decoder demand, and external pass/launch windows (C29). `occupancy-baseline` (C12) computes the interestingness score; the scheduler consumes it (docs/06 §5).
- **Revisit as a multi-armed bandit** (UCB on interestingness) balancing exploit (dwell known-active regions) vs explore (revisit quiet/novel regions), with POI-aware dwell lengths from ITU SM.1880 revisit rules.
- **Priorities** layer: explicit user intent (a pinned "watch this", a ScanPlan region priority) preempts; then decoder demand (a trunk control channel being followed holds its window); then novelty/interestingness.
- **TX arbitration:** because the radio is half-duplex, any C37 transmit request is scheduled as an exclusive slot the scheduler owns (docs/06 §5 edge), never overlapping receive.
- **First version:** a fixed sweep/dwell alternation with a static priority list and POI-sized dwells. The bandit and the richer scoring are added once detection and occupancy exist. This keeps slice 1 buildable.

## Consequences

- The scheduler is a control-plane policy object reading the stores, not on the sample path — it can be tuned and even swapped without touching capture.
- Its effectiveness is measurable (probability of intercept for known burst patterns) and becomes a spike (S4) and a test scenario.
- Multiple HackRFs (a survey radio + a dwell radio) are a later option the policy can grow into; the interface assumes one window now.

## Alternatives rejected

- **Pure sweep** (rtl_power style): misses bursts. **Pure dwell/park**: misses everything outside the window. The interleave is the known-good pattern ([docs/04 §3.8](../04-radio-engineering-and-signals-analysis.md)).
