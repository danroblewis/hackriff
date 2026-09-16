# C12 · occupancy-baseline
> Layer B — Sense · Status: taxonomy frozen 2026-09-13 (resolved in docs/06 §5) · Depends on: C09, C10, C08, C26 (C02 sweep rows; C06 position/time; C38 optional anomaly models) · Used by: C04, C27, C30, C39
> **M2 contracts:** [ADR-0012](../adr/0012-attention-memory-contracts.md) §2 (OccupancyStat; channels learned from detections), §3 (baselines; discrete sites), §4 (novelty, score S, `InterestingnessProvider`), §7 (alarms); types in `hk_model::attention`.

## Purpose
Computes ITU-style occupancy statistics (FCO/FBO/SRO) per channel and band, learns hour-of-week baselines, and scores novelty and anomaly against them. **C12 computes the "interestingness"/novelty score and ranking** (it holds the baselines and novelty); **C04 consumes it** and does not recompute it (docs/06 §5). It is the engine behind "what changed?" (workflow step 3, review history), behind scheduler priorities (step 2), and the local-anomaly input to the attack map (C30). Baselines need history (C26→C12) and, for a portable device, must be keyed per location (C06→C12) (docs/06 §2.1). Alarms only mean something against a learned local baseline.

## Interface
- **In:** Detections (C09), Tracks (C10), noise-floor thresholds (C08), sweep/dwell history (C26), revisit log (when each channel was actually observed; from C04/C02/C03), position/time (C06).
- **Out (provisional `OccupancyStat`):** per channel/band and interval: `FCO`, `FBO`, `SRO`, `n_revisits`, `revisit_max_s`, `threshold_db` + method, `confidence_interval`, `rbw_hz`.
- **Out (provisional `Baseline`):** per channel × hour-of-week (168 slots) × site: mean/percentiles of dB, FCO distribution, sample count.
- **Out (provisional `NoveltyScore` / `Candidate`):** z-score/MAD score, new-emitter flag, "busier than usual", ranked list with score components.
- **Config:** channel plan (raster per band), threshold mode (pre-set/dynamic), guard ≥3–5 dB, baseline min duration (≥24 h), score weights w₁…w₆ (user-tunable), site radius.
- **Cost:** very low; storage-bound.

## Methods
- **FCO** = T_O/T = N_O/N; a revisit is occupied if *any* sample in the channel exceeds threshold. **FBO** = fraction of all (f, t) samples above threshold. **SRO** = FCO averaged over channels. `docs/04 §3.9 "Occupancy statistics methodology (ITU-R SM.1880 / SM.2256)"`.
- **Resolution:** RBW ≤ narrowest channel spacing. If RBW < OBW, lower the threshold by 10·log10(OBW/RBW). `docs/04 §3.9`.
- **Thresholds:** pre-set (sensitivity + required S/N) or dynamic (idle noise, 80% method), always ≥3–5 dB above noise. `docs/04 §3.9`, `docs/04 §3.2 "Noise-floor estimation"`.
- **Timing:** complete capture needs revisit ≤ ½ the minimum on/off time; otherwise the result is statistical, with confidence per SM.2256 Annex 1. Duration ≥24 h for unknown patterns. `docs/04 §3.9`.
- **Baselines/novelty:** per-bin and time-of-day mean and dB percentiles; z-score or robust MAD score; isolation forest over emission features; 24×7 occupancy histograms. `docs/04 §2 "What makes a frequency "interesting": a feature taxonomy"` features 7, 13.
- **Interestingness:** S = w₁·clip(SNR_dB/20) + w₂·novelty + w₃·H(p̂_class) + w₄·𝟙[decoder] + w₅·periodicity − w₆·boring-prior. Weights are user-tunable. `docs/04 §2`.
- **Scheduler coupling:** candidates feed UCB bandit revisits; POI ≈ min(1, (τ+T_d)/T_R). `docs/04 §3.8 "Sweep-based survey vs. real-time IBW"`.
- **Learned option:** autoencoder/anomaly model on waterfalls via C38. `docs/04 §5.5 "Compute cost on embedded platforms"`.

## Platform constraints
- **Sweep revisit:** 0–6 GHz in ~0.75 s with sub-ms dwell per step. Sweep occupancy captures persistent emitters; bursty FCO needs dwell data or statistical treatment. `docs/04 §3.8`, `docs/01 §1.6 "Firmware, `hackrf_sweep`, and host tools"`.
- **One radio, half-duplex:** revisit intervals are irregular (the scheduler), so track n_revisits and gaps explicitly.
- **8-bit, no preselector:** IMD and spurs inflate occupancy in cities. Exclude or weight flagged detections. `docs/02 §1.7 "Overload and intermodulation in urban RF"`.
- **2.4 GHz and 902–928 MHz ISM are exactly where this capability, not C16/C20 decode, carries the use case.** Wi-Fi's 20 MHz OFDM channels sit at the HackRF's 20 Msps ceiling and classic Bluetooth's 1600 hops/s outrun the IBW, so channel-busy/duty-cycle/burst-length statistics (AWARE-042) are the honest target at those bands, not full protocol decode — BLE advertising is the one 2.4 GHz decode exception (C10). **Capability statement, unverified until measured** — `docs/02 §2.3`.
- **Handheld:** the device moves, so baselines must be keyed by site; the ITU ≥24 h assumption fits a parked device, not a walk.
- **Storage:** a 4096-bin float32 spectrogram at 30 lines/s is ~0.5 MB/s before compression; aggregate to occupancy counts early. `docs/02 §3.1 "Throughput math"`.

## Prior art and reuse
- **Spectre (HB9TF):** Go, long-term sweep history in SQLite/MySQL, time/frequency filtering; active. Licence: check. `docs/03 §3.1 "Wideband sweep / survey"`.
- **NTIA SCOS Sensor / scos-actions:** task scheduling + SigMF metadata model. Licence: check. `docs/03 §3.1`.
- **CRFS RFeye / R&S ARGUS:** occupancy, alarm masks ("level above mask", "not in licence DB"); UX references. `docs/04 §11.1 "Professional practice"`.
- **Reference surveys:** McHenry Chicago 2005 (17.4% average occupancy 30–3000 MHz), NTIA TR-13-496, TR-14-498, TR-20-548: sanity baselines. `docs/04 §3.9`.

## Pitfalls
- **Phantom occupancy** from thresholds too close to noise (<3 dB guard).
- **Revisit bias:** FCO from a scheduler that dwells where activity is overstates occupancy; weight by observation time, not detections.
- **Cold start:** novelty is meaningless before ≥24 h per slot; show "baseline immature".
- **Baseline poisoning:** a persistent new interferer becomes "normal". Keep a frozen reference, or use slow adaptation plus change-point flags.
- **Gain-table / calibration changes** shift levels and look like anomalies; version the baselines by cal state.
- **Bias-tee / active-antenna changes** shift the floor the same way (T-333) and are worse than they look: pooling the two states does not mainly cause false alarms, it **masks** a real rise. Key the baselines by bias-tee state, and treat `unknown` as its own cohort rather than as `off`.
- **Propagation effects** (Es, tropo, HF diurnal) look like novelty; C30 explains, C12 must not suppress. A **moving device** looks like global change.
- **Cellular/public-safety baselines** are metadata only (docs/04 §1.3).

## Testing
- **Synthetic:** channels with a known on/off Markov process (FCO 1%, 10%, 50%, 100%), observed under irregular revisit schedules. Assert FCO error inside the SM.2256 Annex 1 confidence interval. Inject a new emitter at hour 30 of a 48 h synthetic run; assert the novelty score crosses threshold within N revisits, with false-alarm rate measured on unchanged channels (targets: proposed, not in docs).
- **Threshold-resolution:** RBW < OBW correction; the result must match the reference occupancy.
- **SigMF / history fixtures (HackRF One):** 24–48 h sweep logs of 30–1000 MHz at one site (hour-of-week pattern), 902–928 MHz meter band (busy), 400–406 MHz radiosonde launches at 00Z/12Z (known periodic novelty), FM band (always occupied, "boring prior").
- **Live only:** multi-day parked runs, site switching, and cal-version transitions.

## Example use cases
Regenerated from `use-cases.yaml`:
- PROP-023 — (occupancy baseline)
- AWARE-042 — Duty-cycle and occupancy statistics
- AWARE-002 — Local GNSS C/N0 watchdog
- AWARE-011 — (occupancy/anomaly)
- AWARE-012 — (occupancy/anomaly)
- AWARE-019 — (occupancy/anomaly)
- AWARE-026 — (occupancy/anomaly)
- AWARE-027 — (occupancy/anomaly)
- AWARE-044 — "Why did my spectrum change?" event feed
- AWARE-060 — (anomaly baseline)

## Open questions
- **Site keying (resolved, docs/06 §2.1):** C06→C12 — baselines for a portable device are keyed per location.
- **Front-end keying (resolved, T-303):** the baseline key carries the **receive chain** as well as the site and calibration. A noise floor is a property of one chain (antenna, cable, LNA, mixer), so two front ends at one site have genuinely different floors; `CalKey::Uncalibrated` is one value for all of them, so without the chain their floors averaged and novelty fired on the mixture. Keyed on the **device**, not the antenna port: a key is a total equality key and the on-disk path, so an `Option` port would split one device's history the day a switch is fitted. A port change is sequential and already explained as a self-inflicted provenance step; two devices are concurrent, and only the key separates them. **Still open (T-307-adjacent):** the occupancy close reads history unfiltered (`Pyramid::query` = `OriginFilter::ANY`), so several front ends folding into one pyramid are already averaged in the level-0 cells before occupancy runs. The key stops baselines pooling; a per-source occupancy read is what makes it end-to-end.
- **Bias-tee keying (resolved, T-333):** the key also carries the antenna-port **bias-tee state**, which completes the receive chain — the DC powers an external LNA, so the floor moves the instant it arrives (the same fact T-331 keyed the floor tracker on). The comparison across differing states is **declined**, not disclosed-and-pooled, because a disclosed-but-pooled comparison still **masks**: a real 12 dB rise scores 1.000 against its own cohort and 0.000 against the pooled one (T-303's measurement, reproduced in T-333 for a 25 dB bias-tee step). `Unknown` is a third cohort, never `Off`: it pools with other unknowns and with nothing else. Measured, not assumed: *every* stored baseline and *every* fold today is `Unknown`, so refusing unknowns would refuse everything. **Carrier laid (T-332):** `FrameInput::bias_tee` → `FrontEndState::bias_tee` → `ProvenanceSummary::bias_tee`/`bias_tee_mixed` (tile format 5; v1–v4 read `unknown`), and a switch is now a `bias-tee` provenance step in the report. Unlike the chain this is per-measurement, not per-run — a bias tee is switched during a run. **Still open (T-359):** `VisitSample`/`OccupancyStat` do not carry it, so the pipeline still folds under `unknown`; the hop is `grid.provenance.bias_tee` at the point `VisitSample` already reads `dominant_gain_key()`.
- **History input (resolved, docs/06 §2.1):** C26→C12 — baselines need spectrum history.
- **Interestingness score location (resolved, docs/06 §5):** C12 computes the full score (using classifier entropy from C15 and decoder demand from C22); C04 consumes it.
- Channel plans per band: from C17 band plans or learned from detections?
- Baseline adaptation policy (frozen vs rolling) and retention on limited disk.

## Reading list
1. `docs/04 §3.9 "Occupancy statistics methodology (ITU-R SM.1880 / SM.2256)"`
2. `docs/04 §2 "What makes a frequency "interesting": a feature taxonomy"`
3. `docs/04 §3.8 "Sweep-based survey vs. real-time IBW"`
4. `docs/04 §11.2 "Mapping to an exploration device"`
5. `docs/03 §3.1 "Wideband sweep / survey"`
6. `docs/04 §3.2 "Noise-floor estimation"`
