# C04 · attention-scheduler
> Layer A — Acquire · Status: taxonomy frozen 2026-09-13 (resolved in docs/06 §5) · Depends on: C01, C02, C03, C10, C12, C22, C23, C29 · Used by: C02, C03, C23, C34, C37, C39

## Purpose
Decides where the single half-duplex radio points. It alternates discovery sweeps (C02) with dwells (C03), weighted by user intent, novelty, expected burst timing and decoder demand. The **interestingness score it prioritises on is computed by C12** (occupancy-baseline, which holds the baselines and novelty); C04 only *consumes* it and does not recompute it (docs/06 §5). docs/06 calls it the single hardest design problem for a one-radio device. It turns workflow step 2 (automate) into efficient use of a 20 MHz window across 6 GHz.

## Interface
- **Inputs:**
  - `ScanPlan` (provisional): spans, schedule, priority, dwell policy, required revisit.
  - User "watch this" pins (C39).
  - Occupancy/novelty/interestingness (C12).
  - Expected next-burst times and periodicity (C10).
  - Decoder leases (C22/C23).
  - Time-anchored events such as satellite passes and 00Z/12Z radiosonde launches (C29).
  - Radio state (C01).
- **Outputs:**
  - `TuneSchedule` commands: sweep segment | dwell(centre, rate, duration, gain) | TX slot (C37, gated).
  - A `DwellRecord`/`SweepRecord` log with reason codes ("novelty 0.8", "user pin", "beacon due"), so reports can state coverage and POI.
- **Rates:** decisions at dwell boundaries, seconds to minutes. Negligible compute.

## Methods
- **Loop** (docs/04 §3.8 "Sweep-based survey vs. real-time IBW"):
  1. Continuous discovery sweep with per-bin max-hold, mean and SK.
  2. Candidate selection by interestingness.
  3. Dwell the top-k, for durations proportional to expected burst intervals.
  4. Record qualifying bursts; classify and decode on the recording.
- **Score** (docs/04 §2 "What makes a frequency "interesting": a feature taxonomy):
  - S = w1·clip(SNR/20) + w2·novelty + w3·H(p_class) + w4·1[decoder] + w5·periodicity − w6·boring_prior
  - This score is **computed by C12** and consumed here (docs/06 §5); C04 does not recompute it. User-tunable weights live with C12.
- **Revisit:** multi-armed bandit, UCB on interestingness (docs/04 §3.8). Arms are candidate windows; reward is detections, novelty and decodes per dwell-second.
- **POI accounting** per region: P_POI ≈ min(1, (τ + T_d)/T_R) and P_≥1 = 1 − (1 − P_POI)^(r·T_obs).
- **Coverage rules:** maximum revisit ≤ ½ the minimum on/off time for complete capture, otherwise statistical reporting. Baselines ≥24 h when patterns are unknown (docs/04 §3.9 "Occupancy statistics methodology (ITU-R SM.1880 / SM.2256)").
- **Window packing:** centre dwells to cover the most candidates while avoiding DC (SDRangel Frequency Scanner, docs/03 §2.2).
- **Preemption** (estimate): interactive user > pinned leases (trunking CC, pass) > scheduled plans > bandit exploration > background sweep. Precedent: OpenWebRX users preempt background schedules (docs/03 §2.4 "Web-based and embedded receivers").
- **Pass-driven dwells:** SatDump-style auto scheduler (docs/03 §3.6 "Satellites").

## Platform constraints
- One radio, half-duplex: sweep, dwell and TX are mutually exclusive (docs/01 §1.2 "Specifications").
- Sweep mode is fixed at 20 Msps/15 MHz, ~0.75 s per 0–6 GHz. Sweep↔stream switch latency is unmeasured (docs/01 §1.6 "Firmware, `hackrf_sweep`, and host tools").
- Dwell ≤20 MHz, usable ~15–18 MHz (docs/01 §7.3 "HackRF as a front end for Pi 5 / Orin Nano (± FPGA)").
- Classification cost scales with detections/s (docs/02 §3.2 "DSP compute: order-of-magnitude feasibility"). Gating dwells can hold lower Jetson power modes (docs/02 §7.2 "Power budget sketches"). Coverage vs battery is a scheduler knob.
- A second receiver would split survey and dwell, at a USB, power and size cost (docs/01 §7.3).

## Prior art and reuse
- **ITU-R SM.1880 / SM.2256:** revisit and duration rules (docs/04 §3.9).
- **SatDump:** auto scheduler + pipeline registry (docs/03 §3.6). Licence: check.
- **NTIA SCOS Sensor:** scheduled "actions" with SigMF metadata (docs/03 §3.1 "Wideband sweep / survey"). Licence: check.
- **OpenWebRX+:** Static/Daylight background schedulers (docs/03 §2.4). Licence: check.
- **Mayhem Scanner/Recon:** copy confirm-before-stop. Avoid the 10–20 frequencies/s list model (docs/01 §3.4 "Spectrum and "exploration" apps and their limits").
- **CRFS RFeye:** occupancy/detection as scheduled tasks (docs/02 §6 "Handheld and portable precedents: what they teach about finding signals").

## Pitfalls
- **Lock-in:** the bandit parks on busy broadcast/cellular bands. Keep a boring prior and an exploration floor.
- **Starvation:** pinned leases starve discovery. Reports must disclose coverage gaps, never imply "nothing there".
- **Artefact chasing:** IMD ghosts and spurs look novel. Honour C05 suspect flags before spending dwell budget.
- **Observation bias:** more dwell yields more "new" detections. Normalise novelty by observation time.
- **Long periods:** periodic emitters such as TPMS (minutes), weather sensors (30–60 s) and BLE (20 ms–10 s) need dwells longer than the period (docs/04 §2, feature 5).
- **Thrashing:** frequent sweep↔stream switches waste settling time and trigger first-sweep-low artefacts.
- **Legal:** TX only through C37 gating; receive-only by default (docs/04 §1.3 "Legal considerations (US; not legal advice)").

## Testing
- **Offline discrete-event simulator:**
  - Emitter population: carriers, Poisson bursts (τ 5–100 ms), periodic beacons (30 s, 60 s, minutes), hoppers, tagged IMD ghosts.
  - Radio model with ~0.75 s sweeps and 20 MHz dwells.
- **Metrics:** emitters discovered vs time; bursts captured per class; time-to-first-detection of a new emitter; T_R; dwell-seconds wasted on suspect emitters.
- **Baselines:** pure sweep and round-robin dwell. The bandit should beat round-robin on bursts captured per hour (target TBD).
- **Rule checks:** POI report matches the formula within simulation error; preemption order honoured; 24 h baseline flag correct.
- **Replay:** recorded SweepFrames plus SigMF dwell captures.
- **Live:** mode-switch latency; discovery rate in a city vs a quiet site.

## Example use cases
Regenerated from `use-cases.yaml`:
- SIGNAL-033 — SatNOGS ground station
- SIGNAL-072 — NCDXF/IARU beacon chain
- AWARE-027 — (scheduler-driven survey)
- AWARE-034 — Wi-Fi DFS radar event logging
- AWARE-045 — CBRS/shared-band incumbent activity sensing
- AWARE-057 — (scheduled dwell/track)
- SIGNAL-008 — (scan-plan automation)
- SIGNAL-018 — (scan-plan automation)
- SIGNAL-027 — (scheduled revisit)
- SIGNAL-028 — (scheduled revisit)

## Open questions
- **Scheduler inputs (resolved, docs/06 §5 / §2.1):** C04 takes C02, C10, C22, C23, C29 (incl. satellite-pass/launch events) and C37 (TX arbitration); the interestingness score comes from C12. The earlier "docs/06 omits these" note is superseded.
- Bandit reward and weights: spike with the simulator before an ADR.
- Is a second receiver (survey + dwell) compatible with "one self-contained device"? It removes most conflicts.
- Plan format (cron-like vs SCOS-like actions), and how reports express POI and coverage to users.

## Reading list
1. docs/04 §3.8 "Sweep-based survey vs. real-time IBW"
2. docs/04 §2 "What makes a frequency "interesting": a feature taxonomy"
3. docs/04 §3.9 "Occupancy statistics methodology (ITU-R SM.1880 / SM.2256)"
4. docs/03 §3.6 "Satellites"
5. docs/03 §2.4 "Web-based and embedded receivers"
6. docs/01 §7.3 "HackRF as a front end for Pi 5 / Orin Nano (± FPGA)"
