# ADR-0012 — Attention + memory contracts: observation log, occupancy, baselines, interestingness, bandit, reports, alarms

**Status:** PROVISIONAL (T-113, core interface, reviewed before merge)
**Touches:** C04 attention scheduler, C12 occupancy baseline, C26 spectrum history, C27 inventory, C30 correlation; ScanPlan/Survey, SpectrumTile, Anomaly/Explanation ([docs/07 §2.1–2.5, §2.18–2.19](../07-data-model.md)); [ADR-0005](0005-survey-dwell-scheduler.md) (this ADR is its "revisit" layer), [ADR-0006](0006-storage.md) (storage homes), [ADR-0004](0004-stream-output-contract.md) (streams)
**Code:** `crates/hk-model/src/attention/` (all shared types, formulas and validation), `crates/hk-model/src/context.rs` (`AnomalyKind::{LevelAboveBaseline, ChangePoint}`), `crates/hk-model/src/ids.rs` (`SiteId`), `crates/hk-core/src/scheduler/step.rs` (`Purpose::reason`/`tier`); stubs listed in §11. Planned routes: [`docs/api.md` "Attention and memory (planned)"](../api.md).

## Context

M2 (docs/11) turns *peruse / automate / review history* into the product: a bandit revisit scheduler (C04 full), occupancy baselines and novelty alarms (C12), richer history queries and survey reports (C26). Eight follow-up tasks (T-115, T-118…T-124) plus three already running (T-114 simulator, T-116 history maturity, T-117 synthetic scenes) code against the contracts fixed here.

Constraints carried in:
- **One half-duplex HackRF.** Sweep and dwell are exclusive; revisits are irregular, so every statistic must know when each frequency was *actually* observed (C12 card, "Platform constraints").
- **Blind-first.** Channels are learned from detections; band plans only suggest. Unknown signals are the priority. Tests use hidden truth through the mock SDR.
- **An 8-bit front end without a preselector.** IMD, spurs and gain steps look like novelty. Suspect flags and provenance come first.
- **Portable.** Baselines must be keyed by site, and a moving device must not poison them.
- **Low power, low wear.** Aggregate early, append in batches.
- **Thin client.** All scoring, occupancy and report logic is in the backend; the UI renders `docs/api.md`.
- **ADR-0005 invariants of the v1 scheduler stay:** gap-free discovery coverage, TX gated at the type level, deterministic output for a given plan, config, clock and call sequence, and an allocation-free `next_step`.

## Decision summary

| Question | Decision |
|---|---|
| Where the shared types live | `hk_model::attention` (not re-exported at the crate root). Scheduler (hk-core), engines (hk-context), stores (hk-store) and API (hk-api) all already link hk-model; nothing else avoids a cycle. |
| Observation log | `DwellRecord` per non-sweep step; `SweepRecord` aggregating hop visits over ≤ 1 pass or 60 s against a once-written `SweepGeometry`. Records carry a `Reason` code and `Tier`, the settled `observed` interval and the analysed `ObservedWindow` (usable extent minus DC notch). |
| Coverage vs history tiles | The log is authoritative for *revisits* (counts, gaps, reasons, POI); tile coverage (T-116) is authoritative for the *displayed* mask. Both use the same usable-span rule; T-115/T-124 assert they agree. Tiles are not redefined. |
| Occupancy | SM.1880 FCO/FBO/SRO. `fco` from **activity-independent** revisits (sweep and scheduled plan tiers) only, time-weighted, suspect revisits excluded (with an upper bound counting them). Dynamic 80 % or pre-set threshold, guard ≥ 3 dB (default 5), RBW < OBW correction clamped at floor + 3 dB. Wilson interval on effective samples corrected for revisit correlation. |
| Channel plan | **Learned from detections**, as cell ranges on the history level-0 grid (`ChannelKey`); band rasters only as `RasterHint`. |
| Baseline key | site × calibration × grid, 168 hour-of-week slots of **mergeable** `SlotStats`; level statistics also split per gain state inside a slot. |
| Maturity | A pool is mature at ≥ 24 h observed. Novelty uses the finest mature pool of hour-of-week → hour-of-day → day part → all hours; none mature = immature (no novelty, no alarms). |
| Poisoning | Frozen reference + adaptive copy (14-day half-life) + CUSUM change points; re-freeze on user request (optional auto after N days). |
| Site keying | **Discrete sites** (250 m radius clusters, user-nameable), plus `mobile` and `unassigned` states that accrue no baseline. Not geohash. |
| Interestingness | S = w1·clip(SNR/20) + w2·novelty + w3·H + w4·1[decoder] + w5·periodicity − w6·boring; weights versioned in SQLite and tunable via the API; **C12 computes, C04 consumes** through `InterestingnessProvider` snapshots. |
| Novelty normalisation | New-emitter novelty is a Poisson tail on the site's first-sighting rate × observed seconds; level/occupancy novelty is a z-score on effective samples. |
| Bandit | Arms = packed candidate windows (`ArmKey`); reward = bounded yield per dwell-second; discounted cost-normalised UCB; 15 % exploration floor, 25 % sweep floor, 30 min staleness bound; suspect arms get one verification dwell. |
| Preemption | interactive > pinned leases > scheduled plans > bandit > background sweep (`Tier`). |
| POI | Exact union-of-windows fraction from the log (`poi_fraction`), which reduces to min(1, (τ+T_d)/T_R); P≥1 = 1 − (1 − P_POI)^(r·T_obs). Always disclosed. |
| Report | `SurveyReport` with mandatory `coverage` (observed fraction, gaps, never-observed ranges, POI rows, "unobserved is not quiet") and an explicit `change_vs_baseline.status`. JSON/CSV/PNG exports, rendered by the backend. |
| Alarms | Stored as `Anomaly` rows (kinds `level-above-baseline`, `new-emitter`, `busier-than-baseline`, `change-point`) with a parseable `baseline_ref` dedupe key and an `AlarmDetail`; 2-on/3-off hysteresis, 1 h reopen cooldown; suppressed when mobile/unassigned/immature; provenance explained first. |
| Storage | Series in **hk-store files** (observation log, occupancy series, baselines); **SQLite** for sites, weights and alarms (joins with Explanations). Append in batches (≤ 1/min), atomic rewrites at slot close. |

## 0. Time base (applies to every section)

All contract time comes from the **device/sample clock** (the `Timestamp` carried by captured blocks, as the pipeline scheduler's `SyntheticClock` already does), never from wall time. This covers: observation record times and hourly segment naming; retention ages; occupancy intervals and rollups; hour-of-week slots, 24 h maturity and the 14-day / 6 h half-lives; `no_fix_hold_s` and `mobile_window_s`; bandit discounting, the 10-min sweep-floor window, 10 s re-scoring and the 30-min maximum revisit; alarm hysteresis intervals, the 1 h re-raise cooldown and the 7-day dismissal expiry. Only I/O flush cadence (e.g. "flush at most once a minute") may use wall time. Rationale: time-compressed replay (T-125) and blind acceptance (T-124) run 48 h scenes in minutes; wall-clock cooldowns or retention would never expire or would key on replay time. Implementations must not call `SystemTime::now` on these paths.

## 1. Observation / revisit log (C04 → C12 input)

### 1.1 Dwell records

`attention::observation::DwellRecord`, one per non-sweep step (POI/bandit dwells, verification steps, region dwells, leases, interactive intent):

- **Identity:** `survey_id`, `seq` (scheduler step `seq`), `plan_version`, `site`.
- **Why:** `reason: Reason` and `tier: Tier`; `tier` must equal `reason.tier()`.
- **Where:** `window: ObservedWindow` (`center_hz`, `sample_rate_hz`, `usable` analysed extent, optional `dc_excluded` notch, `rbw_hz`) and `rf_path`.
- **When:** `planned` and `observed` intervals. `observed` starts after retune settle and is cut when a higher tier preempts (`preempted`).
- **Trust:** `dropped_samples`, `overload`, `provenance_ref`.

`observed`, not `planned`, feeds every statistic: a step cut by the user, or one whose samples were dropped, did not observe its planned time.

### 1.2 Reason codes

`Reason` is a `Copy` tagged enum (`code` field), so the scheduler carries it on every step without allocating. `Reason::text()` gives the human form: "background sweep", "scheduled region", "revisit due", "poi dwell", "verify gain step", "novelty 0.80", "explore", "beacon due", "user pin", "decoder lease", "pass window", "interactive". `Purpose::reason()` in `hk-core::scheduler::step` maps today's purposes: `Sweep` → `background-sweep`, `RegionDwell` → `region-dwell`, `Dwell` → `poi-dwell`, trust tests → `verification`, `UserIntent` → `interactive`. T-120 adds bandit and lease purposes, and their mapping, in the same file.

### 1.3 Sweep records

Discovery hops run at ~20 steps/s (`sweep_step_ns` 50 ms), so per-hop rows would be ~1.7 M/day. Instead:
- **`SweepGeometry`** (id = hash of the canonical hop windows, `plan_version`, `hops: Vec<ObservedWindow>`) is written once per geometry change.
- **`SweepRecord`** covers at most one pass or 60 s, whichever ends first. It holds `geometry`, `span`, and `visits: Vec<HopVisit { hop, start_ms, observed_ms }>` in time order, plus `preempted_hops`, `dropped_samples` and `overload_hops`.

At 400 hops per pass that is ~12 B per visit before compression, about 7 MB/day worst case.

### 1.4 Totals, coverage and alignment with history

`ObservationTotals` (the T-115 query result) for a frequency range × span:
- `n_visits` and `n_visits_activity_independent`;
- observed seconds per tier (`TierSeconds`);
- `max_gap_s` (span edges count);
- `mean_revisit_s`.

A range counts as observed at an instant only when it lies **entirely** inside a covered extent (usable minus DC notch). That is the SM.1880 "any sample in the channel" rule's precondition: a channel half in the window was not observed.

**Alignment rule.** `ObservedWindow::usable` uses the same usable-span rule the history ingest applies to frames, so the level-0 tile coverage (T-116's mask) and the log describe the same cells. Tiles keep their own coverage fraction (display, "not observed ≠ quiet"). The log adds what tiles cannot: revisit counts, gaps shorter than a cell, reasons and tiers. T-115's e2e asserts log coverage equals the tuned windows exactly; T-124 asserts log and tile coverage agree to within one level-0 cell.

*Note (T-115 review):* the usable span currently follows the history level-0 fold extent (the analysed frame's first-bin lower edge to last-bin upper edge, clipped to the sampled band) less the ±15 kHz DC notch, so log and tiles align cell for cell. A shared roll-off trim (the scheduler's `usable_fraction`) applied to both history and the log is a follow-up; until then both include the anti-alias roll-off edges.

### 1.5 Persistence and retention

The log lives in hk-store `observation/` (T-115):
- **Layout:** hourly segments `<data>/observations/YYYY/MM/DD/HH.log`. Each line is `<crc32-hex8> <json ObservationRecord>`.
- **Writes:** a writer thread buffers and flushes at most once a minute or at 256 KiB, and fsyncs when the hour seals.
- **Crash recovery:** a torn tail line fails its CRC and is dropped on open. At most one flush interval is lost.
- **Retention:** defaults 30 days and 512 MiB, whole hours deleted oldest first.
- **Never blocks the pipeline:** records reach the writer on a bounded queue, and when it is full they are dropped and counted.

## 2. OccupancyStat (C12)

### 2.1 Definitions (SM.1880-2 / SM.2256-1; docs/04 §3.9)

- **FCO** = T_O/T = N_O/N. A revisit of a channel is occupied if **any** sample in the channel exceeds the threshold.
- **FBO** = fraction of all (cell, revisit) samples above threshold, on the history level-0 cell grid.
- **SRO** = FCO averaged over the band's learned channels.

`OccupancyStat` rows are per channel (`OccupancySubject::Channel`) or band (`Band`), per interval (15 min, rolled up to 1 h). Fields:
- `fco`, `fco_all_visits`, `fco_suspect_upper`, `fbo` and `sro` (bands only);
- `n_revisits`, `n_occupied`, `n_suspect` and `n_revisits_all`;
- `observed_s`, `revisit_max_s` and `revisit_mean_s`;
- `timing` (`complete` when max revisit ≤ ½ the minimum on/off time, else `statistical` or `unknown`);
- `threshold` (spec), `threshold_db` (median applied), `guard_clamped`, `rbw_hz`, `obw_hz`, `unit` and `calibration`;
- `confidence` and `revisit_biased`.

### 2.2 Threshold method and guard

`ThresholdSpec { method, guard_db, rbw_correction }`:
- **`dynamic`** (default): the floor is estimated from the lowest `idle_fraction` (0.8, the "80 % method") of the channel's per-revisit levels over the interval, then `guard_db` (default 5, allowed 3–20) is added.
- **`pre-set`:** `level_db` is receiver sensitivity plus the service's required S/N.

The floor used by `dynamic` may also be the C08 tracker floor when present. **Verified by T-118 against the Report SM.2256-1 text ("Calculated threshold", citing SM.1753):** the 80 % method discards the highest 80 % of the samples and **linearly averages the remaining lowest 20 %**; the threshold is then 3–5 dB above that noise level. The Report recommends recomputing it per scan and notes it only works over a band or several equal-bandwidth channels (a busy channel raises its own floor). `idle_fraction` is therefore the *discarded* share. T-118 prefers the history's bias-corrected `floor_db` (T-116, the C08/C26 floor) and falls back to the 80 % method pooled over the band's cells.

### 2.3 RBW < OBW correction

When `rbw_hz < obw_hz`, the per-bin threshold is lowered by 10·log10(OBW/RBW), since a signal spread over OBW puts less power per RBW bin. `obw_hz` is the learned channel's occupied bandwidth. **The result is clamped at floor + 3 dB** (`MIN_GUARD_DB`) and flagged `guard_clamped`: the correction must not recreate phantom occupancy (`ThresholdSpec::applied_db`, tested).

### 2.4 Confidence interval (SM.2256 Annex 1)

The Annex treats the FCO estimate as approximately normal, with error shrinking as sample count grows and revisit times stabilise. This contract fixes the concrete form:
- **Effective samples:** `n_eff = n·(1−ρ)/(1+ρ)` with ρ = exp(−T̄_R/τ_c) and τ_c = T_on·T_off/(T_on+T_off) from C10 timing (a two-state on/off process sampled every T̄_R). Unknown τ_c gives `n_eff = n`, flagged `independence_assumed`.
- **Interval:** Wilson score at 90/95/99 % (`fraction_interval`), which is the normal approximation made well-behaved at FCO 0 and 1.

T-118's blind test asserts FCO error falls inside this interval on T-117 Markov scenes. If it doesn't, T-118 changes `effective_samples` and amends this section.

**T-118 amendment (ρ measured, formula unchanged).** A blind engine rarely has C10 on/off timing, so T-118 measures ρ as the lag-1 autocorrelation of the activity-independent visit states and passes τ_c = −T̄_R/ln ρ to `effective_samples`; ρ ≤ 0 gives `n_eff = n` (measured), a constant sequence leaves `independence_assumed` set.

**What SM.2256-1 Annex 1 says (verified by T-118 from the Report text).** The Annex does not define an interval formula; it gives sample-size rules for an absolute error ΔSO at confidence P_SOC:
- **Pulsed signals** (A18/A19): J_min = SO(1−SO)(x_p/ΔSO)², the binomial normal approximation; Wilson is its well-behaved form (Table A2 reproduced in a unit test).
- **Lengthy signals** (A12/A16): J_min = x_p/(2ΔSO)·√(V_avr(1.06+δT²)), driven by the number of state changes V rather than by SO (Table A1 reproduced with the A16 constant 194.2; the A12 layout is inferred from A16 and Table A1 because the PDF text extraction garbles it).
- **Unstable revisit times** (A5.1.2, δT > 10 %): accumulate T_AI += T_Rj and T_O += T_Rj (both ends occupied) or T_Rj/2 (a change), SOCR = T_O/T_AI, which is exactly §2.5's half-gap weighting.

**`fco_window` (additive, T-118).** §2.5 rule 4 records the window used in `OccupancyStat::fco_window`.

### 2.5 Observation-time weighting against revisit bias

The bandit dwells where activity is, so counting its visits overstates FCO (C12 pitfall). Rules:
1. **`fco` uses activity-independent visits only** (`Tier::activity_independent`: background sweep, scheduled plan). Their timing does not depend on what was measured, so they carry no selection bias.
2. **Each visit is weighted by the time it represents:** half the gap to the previous visit plus half to the next, capped at `2 × nominal T_R`. A longer gap is unobserved, not interpolated.
3. **`fco_all_visits` stratifies:** within each 1-minute stratum, the time fraction occupied from all visits; strata then weighted by their observed duration. Reported for information.
4. **Too few visits: widen, never substitute.** With fewer than 30 activity-independent visits in an interval, `fco` for that interval is computed over the smallest enclosing rollup window (15 min → 1 h → 6 h → 24 h → whole span) that reaches 30, and the window used is recorded with the stat. If no window reaches 30, `fco` stays activity-independent with its (wide) Wilson interval. `fco_all_visits` is never substituted for `fco`; `revisit_biased` is set only when a caller explicitly asks for the all-visits estimate. Rationale: at the 25 % sweep floor a 20 s pass revisits a channel about every 80 s (~11 visits per 15 min), so a per-interval fallback would always return the biased value exactly when the bandit dominates; over 48 h there are ~2 000 activity-independent visits, so hourly and longer windows are unbiased and usable.

### 2.6 Suspect and IMD detections

A threshold crossing coincides with a suspect detection (flagged `clipped`, `suspect_imd`, `spur_candidate`, confirmed image, or `compressed`) when it lies in the visit window ± one time cell and inside the detection's extent widened by one level-0 cell. A revisit is **suspect** when it lies under `overload`, or when every above-threshold cell coincides with a suspect detection. One clean crossing makes the visit occupied and not suspect (T-129). A suspect revisit:
- It is excluded from `fco`, as unobserved rather than unoccupied, and counted in `n_suspect`.
- `fco_suspect_upper` counts it as occupied, so the pair brackets the truth.
- Suspect crossings never create or widen a learned channel.

### 2.7 Channel plan source: learned (decided)

Channels come from **blind detections**:
- The union of confirmed tracks' and inventory emitters' occupied extents, snapped outward to the history level-0 grid: `ChannelKey { scheme, lo_cell, hi_cell }`.
- The key is deterministic and needs no registry.
- The plan is versioned. Merges and splits bump `plan_version`; series keyed by an old extent stay readable.

Channels are learned from non-suspect detections (T-129):
- A cluster is **published** when it is **confident** (median SNR ≥ 7 dB over a bounded window of its newest detections) or **persistent** (it recurs across ≥ 3 intervals at a stable centre, spread ≤ max(1 level-0 cell, 0.1 × OBW), with cumulative detected duration ≥ D, default 0.5 s).
- A detection inside a published host ≥ 4× wider and ≥ 6 dB stronger, overlapping it in time, is an **in-band fragment** unless its own cluster is persistent. Fragments never seed, join or narrow a channel, and never remove a published one.
- Overlapping neighbours split at the midpoint of their centres.
- Non-recurring flicker makes no channel; FBO and cell baselines still cover it.
- The persisted plan carries each channel's learning evidence (median centre and SNR, interval and duration evidence), so a restart keeps a host's width, `first_learned` and fragment hosting.

A band raster from C17 is only a `RasterHint { spacing_hz, offset_hz, source }` attached to a learned channel, and a non-zero `offset_hz` is itself interesting. Where nothing was ever detected there are no channels, but FBO and cell baselines still cover the band, so a first emitter there is caught as `level-above-baseline` / `new-emitter`.

Rejected: band-raster channels. They pre-populate structure from a database, hide off-raster emitters, and have no answer for unallocated or unknown services.

### 2.8 Intervals and rollup

Stats close every 15 min (config). Hourly rows are rebuilt from 15-min rows: counts and weights sum, and the interval is recomputed from summed `n_eff`.

### 2.9 Storage

hk-store `occupancy/` (T-118):
- **Series:** daily segments of `OccupancyStat` lines (same CRC-line format as §1.5), appended once per interval close.
- **Channel plan:** `channels.json` (versioned), rewritten atomically on change.
- **Retention:** 15-min rows 90 days, 1-h rows 2 years, quota 256 MiB.

## 3. Baseline (C12)

### 3.1 Key and slots

`BaselineKey { site: SiteId, cal: CalKey, scheme, cell_factor }`:
- **Cells:** `cell_factor` × level-0 cells. Default 16, which is 100 kHz on scheme 1.
- **Slots:** each key holds 168 `HourOfWeek` slots (0 = Monday 00:00 in the site's fixed UTC offset).
- **Per cell and per learned channel, per slot:** mergeable `SlotStats`, with level statistics further split by gain state (at most 4 per slot; beyond that the cell reports `mixed`).
- **Calibration:** dBFS baselines are keyed `uncalibrated`. A new CalibrationState starts a new key (C12 pitfall: calibration changes look like anomalies).

Only observed cells are stored. Size estimate: a 30–1000 MHz plan is ~9 700 cells × 168 × ~56 B ≈ 90 MB per site/cal for one copy, doubled with the frozen reference.

### 3.2 Maturity (decided; see open question 1)

A **pool** is mature once it holds ≥ `MATURITY_MIN_OBSERVED_S` = 24 h of observation (SM.1880's ≥ 24 h for unknown patterns). For a slot, novelty uses the finest mature pool in order:
1. its hour-of-week slot;
2. its hour-of-day, weekdays pooled;
3. its six-hour day part;
4. all hours.

When none is mature: `Maturity::Immature`. Novelty is 0, baseline alarms are suppressed, and the UI shows "baseline immature".

Pools are sums of slots because `SlotStats` is additive. A literal "24 h in each hour-of-week slot" would need 24 weeks parked. The fallback keeps the rule (< 24 h in the pool used = immature) and gets a parked device mature after one day, with the resolution reported (`Maturity::Mature { resolution }`, `NoveltyScore::maturity`).

### 3.3 Statistics kept

`SlotStats`:
- `n_visits`, `observed_s`;
- Σ level and Σ level² (dB), with levels **winsorised** at the reference mean ± 3σ before adding, so a burst doesn't swamp the spread;
- Σ weight·occupied and Σ weight (time-weighted FCO, §2.5);
- `max_db` (raw).

These give mean, standard deviation, FCO and max; merging equals sequential adding (tested). Percentiles are deliberately not kept per slot: 168 histograms per cell cost ~8× the storage. Low percentiles for the floor come from history tiles (C26), which already keep p10/p90.

### 3.4 Frozen reference, slow adaptation, change points

- **Frozen reference:** the pool statistics at the moment of maturity, never updated automatically. Novelty z-scores are computed against it.
- **Adaptive copy:** the same statistics with exponential forgetting (`half_life_days` 14, in observed days).
- **Change point:** a per-cell/channel CUSUM of (adaptive − reference)/σ_ref with slack `cusum_k_sigma` 0.5 and threshold `cusum_h_sigma` 8. Crossing raises a `change-point` alarm (§7).
- **Re-freeze:** only by `POST /api/baselines/refreeze` (user), or after `auto_refreeze_days` if configured (default off).
  - It copies the **decayed** adaptive statistics into the reference. With a 14-day half-life, a parked device's adaptive copy holds only a few hours (~3 h) of effective observed time per hour-of-week slot, so no slot is mature on its own. Resolution coarsens to the finest pool that is still ≥ 24 h, and slots below their hour-of-day maturity reopen reference learning.
- **Reference learning (T-119):** a fold enters the reference only if it is clean, not provenance-explained, no change point is open or building (every CUSUM < h/2), and its slot's **hour-of-day pool is immature** (< 24 h). Learning therefore stops after ~24 parked days, which bounds slow-leak poisoning.
  - The fold must also be not novel (novelty 0), **or** its novelty must be below the alarm "on" level (0.7, §7.2) while that hour-of-day pool is immature **and** it must not be novel (all z < 3) against the slot's own immature hour-of-day reference, whose adaptive copy must be within 2·`cusum_k_sigma` σ of it (no drift), and which must itself be novel against the coarse pool (an established pattern, not a change, explains the fold's novelty). Novelty is judged at the finest mature pool, which may be coarser than the slot's pattern. Without this exception, a sharply patterned channel (e.g. one busy hour a day) would stay novel against the all-hours pool and never accrue. Without the own-hour checks, a moderate new interferer would drain into the immature pool fold by fold, dragging the reference along, instead of raising a change point.

A persistent new interferer thus becomes "normal" in the adaptive copy but stays flagged against the reference until the user accepts it (C12 pitfall: baseline poisoning).

### 3.5 Site keying for a moving device (decided: discrete sites)

`SiteKey = Site(SiteId) | Mobile | Unassigned`. A `SiteRecord` holds:
- `name`, centroid `lat_deg`/`lon_deg`, `radius_m` (default 250);
- `utc_offset_min`;
- `source` (config / user / GNSS);
- `first_seen`, `last_seen`, `observed_s`.

Assignment (T-119):
- **Config/user site:** used as given. In M2 there is no GNSS (C06), so the site comes from `--site` or `PUT /api/sites/current`.
- **With fixes:**
  - Ground speed > `mobile_speed_m_s` (1 m/s), or position spread over `mobile_window_s` (300 s) larger than the radius, gives `Mobile`.
  - Otherwise the device joins the nearest site whose radius contains it, or creates one.
- **Without a fix:** the last site is kept for `no_fix_hold_s` (600 s), then `Unassigned`.
- **`Mobile` and `Unassigned`:** observations, occupancy and inventory continue, but baselines don't accrue and baseline alarms are suppressed.

Why not geohash/H3 buckets:
- **Cell edges:** a device parked near a cell edge splits one RF environment into two immature baselines.
- **Scale:** a fixed cell size has nothing to do with the RF environment's scale.
- **Maturity:** buckets multiply keys and delay maturity.
- **Names and privacy:** users think in places ("home", "hilltop"). Site ids don't reveal location unless the centroid is shared, which suits "don't rule out sharing later".

H3 (already a dependency) may index sites for nearest-site lookup; it is not the key.

### 3.6 Storage and retention

hk-store `baseline/` (T-119):
- **Layout:** one file per `BaselineKey` (`<data>/baselines/<site>/<cal>/<scheme>-<factor>.bin`) holding the reference and adaptive slot stats, sparse by cell.
- **Writes:** temp → fsync → rename when an hour slot closes (≤ 24 writes/day per active key).
- **Quota:** 1 GiB. Eviction takes the least recently visited site first, and never a site visited within 180 days unless over quota.

Sites and weights are rows in SQLite (§9).

## 4. NoveltyScore, Candidate and interestingness S (C12 computes, C04 consumes)

### 4.1 Components and formula

`ScoreComponents` are measured, each already in its term's range:
- `snr_db`: from the detection/track;
- `novelty` 0–1 (§4.4);
- `class_entropy`: H/ln K from C15, where `None` = never classified, scored **1** (unknown is maximally uncertain and unknown signals are the priority);
- `decoder_available`: a recipe match on measured parameters (`/api/recipes/match`), never a frequency lookup;
- `periodicity`: C10 period confidence;
- `boring_prior` 0–1 (§4.3).

S = w1·clip(SNR_dB/20, 0, 1) + w2·novelty + w3·H + w4·1[decoder] + w5·periodicity − w6·boring (`score::interestingness`). `score_norm` = S / (w1+…+w5), clipped to [0, 1]; it is the bandit prior and the "novelty 0.8" reason value.

### 4.2 Weights: user-tunable, versioned

`ScoreWeights { version, snr, novelty, class_entropy, decoder, periodicity, boring }`:
- **Defaults:** 1, 2, 1, 0.5, 0.5, 1 (novelty-led). Each weight must be in [0, 10], and at least one positive term weight is needed.
- **Storage:** SQLite `attention_weights` rows (T-119 migration). `PUT /api/attention/weights` inserts version n+1 (audited), and every `CandidateSet` records the weights it used.
- **Changes:** take effect at the next scoring pass, never retroactively.

### 4.3 Boring prior

Computed from measurement first:
- persistently always-on in the baseline (FCO > 0.95 in every mature pool);
- stable (novelty < 0.1 over ≥ 10 revisits);
- well characterised (class entropy < 0.2, or valid decodes);
- a user "boring" tag on the emitter or a Selection.

A C17 allocation suggestion (e.g. broadcast FM) may add at most 0.3 of the prior, and **only after** the emitter was blindly detected and characterised (docs/04 §2: "after first discovery"). The prior never suppresses an off-raster or mismatched emitter: a mismatch zeroes the C17 part.

### 4.4 Novelty and normalisation by observation time

`NoveltyScore`:
- **`level_z`:** (observed − reference mean)/max(σ, 1 dB) on the mature pool.
- **`occupancy_z`:** (FCO_obs − FCO_ref)/sqrt(p(1−p)/n_eff), using §2.4's `n_eff`, so a short look at a channel cannot produce a large z.
- **`new_emitter`:** the Poisson tail of k first sightings in the observed seconds at the site's baseline first-sighting rate, mapped as clip(−log10 p / 6, 0, 1) (`new_emitter_novelty`). Expected counts scale with observed time, so dwelling longer does not manufacture novelty (C04 pitfall: observation bias).
- **Combined:** `novelty` = max over available components, with z mapped by `novelty_from_z(z, 3, 10)`.
- **Forced zero** when immature or `provenance_explained` (§7.4). Both rules are validated.

### 4.5 Candidates

`Candidate`:
- subject (`emitter` / `track` / `cells`), `freq`, `score`, `score_norm`, `components`, `novelty`;
- `suspect_fraction`, `needs_verification`;
- `expected_interval_s`, `min_on_off_s`, `next_burst_eta` (from C10).

`CandidateSet { schema, version, t, site, weights, candidates }` is sorted by `score` descending and validated as a whole: each `score` must equal the formula under the recorded weights.

C12 re-scores at most every 10 s (60 s in low-power mode) and publishes a set only when it changed.

### 4.6 The trait

```rust
pub trait InterestingnessProvider: Send + Sync {
    fn version(&self) -> u64;              // cheap, lock-free; called at every decision boundary
    fn snapshot(&self) -> Arc<CandidateSet>; // only when version moved
}
```

`SharedInterestingness` (publish/subscribe, `AtomicU64` version + `Mutex<Arc<_>>`) is both the real handoff the C12 thread uses and the stub T-120 codes against before T-119 exists: tests and the T-114 simulator publish hand-built or detection-count sets. `EmptyInterestingness` makes the bandit explore uniformly. Neither ever blocks on scoring.

## 5. Scheduler bandit contract (C04 full, T-120)

### 5.1 Arms and window packing

An arm is a candidate **window**: `ArmKey { rf_path, center_q = round(center / arm_quantum_hz), rate_hz }`, with a 1 MHz quantum so arm history survives re-packing. Packing at each new snapshot (off `next_step`, allocation allowed there, arm table preallocated to `max_arms` 256):
1. Take candidates by `score` desc, skipping any that is banned or `needs_verification` (verification is scheduled separately).
2. For the top unpacked candidate, place the centre so it sits **off DC** by ≥ its half-bandwidth + DC guard (10 kHz) and inside the usable span, like today's quarter-span POI offset.
3. Slide the centre within the allowed range to maximise the Σ `score_norm` of other candidates fully inside the usable span and clear of DC (SDRangel Frequency Scanner precedent).
4. Mark them packed; repeat.
5. Add one exploration arm per discovery hop not already covered, with prior 0.

**Where packing runs (amended by T-127).** Packing allocates, so it never runs inside `next_step`. The owner calls `Scheduler::refresh_bandit()` at its decision boundaries: the pipeline control loop before each step, the T-114 simulator before each decision. `next_step` keeps using the last packed table until then. `refresh_bandit` is a lock-free version compare when nothing new was published.

### 5.2 Reward per dwell-second

`DwellOutcome { seq, arm, dwell_s, new_detections, bursts, novelty_sum, valid_decodes, suspect_detections }` reaches the scheduler once detection, C12 and decoders have processed the dwell. Raw r = (1·new + 1·Σnovelty + 0.5·decodes + 0.1·bursts)/dwell_s, squashed to u = r/(r + 0.1/s) ∈ [0, 1) (`DwellOutcome::reward`). Suspect detections earn nothing and accrue "dwell-seconds wasted on suspect" (the T-114 metric).

### 5.3 UCB, floors, suspects, dwell length

- **Index:** discounted, cost-normalised UCB, `ū + c·sqrt(ln(max(Σ dwell_s, e)) / arm_dwell_s)` with c = 0.5 (`ucb_index`).
  - Arm statistics decay with a 6 h half-life of radio time (non-stationary spectrum).
  - `ū` starts from the arm's best `score_norm` with 5 pseudo dwell-seconds.
  - Ties go to the lower `ArmKey`. No RNG.
- **Exploration floor:** 15 % of bandit time goes to the stalest feasible arm.
- **Starvation bound:** every feasible **candidate** arm is revisited within `max_arm_staleness_s` (30 min).
  - **Hop exploration arms are exempt (amended by T-127).** Exploration arms (prior 0, one per uncovered discovery hop) are served by the exploration floor and UCB, not the bound: tiling 0–6 GHz with 8 s exploration dwells cannot meet 30 minutes. The background sweep's pass still covers those hops gap-free (§5.7).
- **Sweep floor:** the background sweep keeps ≥ 25 % of radio time over any 10-min window unless interactive intent holds the radio, so discovery never starves. When leases make the floor unmeetable, that is recorded and disclosed (§5.5), not hidden.
- **Suspects (C05):** a `needs_verification` candidate gets exactly one verification group, using the existing S4 gain-step/retune machinery.
  - Pass: C12 clears the flag.
  - Fail: the arm is banned for `suspect_ban_s` (1 h).
  - Arms whose candidates are mostly suspect are scaled by (1 − suspect_fraction).
- **Dwell length:** `dwell_periods` (3) × expected interval, clamped to 2–120 s (`BanditConfig::dwell_s`), so periodic emitters are dwelt longer than their period (C04 pitfall: long periods).
- **Complete capture:** a candidate with `min_on_off_s` gets a required revisit of ½ of it. When that is infeasible, its occupancy is `statistical`.

### 5.4 Preemption order

`Tier`, highest first: **interactive > pinned leases > scheduled plans > bandit > background sweep**.
- **Interactive:** today's `UserIntent`.
- **Pinned leases:** user "watch this" pins, decoder/trunking leases, pass/launch windows.
- **Scheduled plans:** dwell-only regions, plan revisit targets, cron plans.
- **Bandit:** exploit, explore, beacon-due and, until T-120, WRR POI dwells.
- **Background sweep:** the discovery pass.

A running verification group is atomic within its tier: only interactive intent and leases cut it, and the cut group restarts under a new id, as today. A cut step is rolled back and revisited, as in v1.

### 5.5 POI accounting and coverage-gap disclosure

For any region and burst duration τ:
- **P_POI** = |(∪ᵢ [sᵢ − τ, eᵢ]) ∩ span| / |span| over the log's observed intervals (`poi_fraction`). It is exact for irregular revisits and equals min(1, (τ + T_d)/T_R) for periodic ones (tested against the docs/04 worked example scale).
- **P≥1** = 1 − (1 − P_POI)^(r·T_obs) for bursts at rate r (`p_at_least_one`).

`GET /api/scheduler` and every report disclose POI for τ ∈ {5 ms, 100 ms, 1 s, 10 s}, plus coverage gaps longer than 2 × the region's nominal revisit. Reports never state absence without these numbers.

### 5.6 Stub provider before T-119

T-120 uses `SharedInterestingness`. The simulator (T-114) and hk-core tests publish sets built from blind detections, e.g. `score = clip(SNR/20)` with `novelty` from "first seen in the last hour". T-120 may put such a detection-count provider in its own files. T-119 later replaces the publisher, not the trait.

### 5.7 Compatibility with v1 invariants

- **Gap-free discovery coverage:** pass geometry is unchanged. Bandit dwells occupy the slots `dwells_per_cycle` occupies today, and a pass interrupted by any tier resumes at its position.
- **TX gated:** no tier or reason is a TX slot; `request_tx_slot` still returns `TxGated`.
- **Determinism:** output depends on plan, config, capabilities, clock readings and the call sequence, where "call sequence" now includes each `snapshot()` taken (keyed by `version`) and each `record_outcome`. Floating-point ties break on `ArmKey`.
- **Allocation:** `next_step` stays allocation-free, including after a new provider version is published. Packing happens in `refresh_bandit` (§5.1), off `next_step`, into preallocated tables.

### 5.8 Low-power modes

Under a low-power profile:
- the bandit share shrinks: `dwells_per_cycle` down and the sweep floor up, since sweeping needs no demod/classify;
- C12 scores every 60 s;
- stores flush every 5 min.

Coverage and POI disclosure make the battery trade-off visible.

## 6. Survey report schema (C26/C39, T-121)

### 6.1 `report(region, span)`

`SurveyReport`:
- `schema`, `generated_at`, `region`, `span`, `site`;
- `occupancy { bands, channels (FCO desc, capped), truncated }`;
- `top_emitters`: from the inventory (C27): id, extent, first/last seen, sightings in span, lifecycle, channel FCO, top suggestion label (a suggestion, never truth), `new_in_span`;
- `change_vs_baseline { status, baseline, resolution, changes }`, where `status` is `available`, `immature`, `no-baseline` or `unavailable` (before T-119). Changes are listed only when available;
- `coverage` (mandatory, §6.2);
- `provenance_steps`;
- `anomalies` (ids overlapping the box);
- `warnings`.

### 6.2 Coverage (always disclosed)

`CoverageDisclosure`:
- `observed_fraction`, `observed_s`;
- `gaps` (longest first, capped, `gaps_truncated`);
- `never_observed` ranges;
- `poi` rows (at least one);
- a `statement` that must say unobserved is not quiet whenever coverage < 1.

Validation refuses a report without them, and serde refuses a document missing `coverage`.

### 6.3 Provenance steps

`ProvenanceStep { t, kind, freq?, detail }`, time-ordered. Kinds: gain, calibration, spur mask, antenna port, overload, source restart, sample drop, site change. They come from history `ProvenanceSummary` and the observation log. They are what C30 rules out first (§7.4).

### 6.4 Exports

`format=json` (the document), `csv` (channel occupancy rows, then coverage gaps, with a header comment carrying the coverage statement), `png` (occupancy heatmap over the history grid, backend-rendered with the coverage mask hatched). hackrf_sweep CSV import/export of raw history remains T-116's.

## 7. Novelty alarm schema (C12 → C30, T-122)

### 7.1 Kinds

| `AlarmKind` | Stored `AnomalyKind` | Trigger | Unit |
|---|---|---|---|
| `level-above-baseline` | `level-above-baseline` (new) | `level_z` novelty on merged adjacent cells | dB |
| `new-emitter` | `new-emitter` | a new inventory emitter with `new_emitter` novelty ≥ on | count |
| `busier-than-usual` | `busier-than-baseline` | `occupancy_z` novelty on a channel/band | fraction |
| `change-point` | `change-point` (new) | CUSUM (§3.4) | dB or fraction |

Each alarm is an `Anomaly` row (existing tables, append-only, status history, Explanations):
- `subject`: emitter or region;
- `score` = novelty;
- `detector_version` `hk-context.c12-alarm@1`;
- `baseline_ref` = `AlarmKey::baseline_ref`: `c12-alarm:v1;kind=…;site=…;cal=…;res=…;slot=…;subject=channel:<scheme>:<lo>..<hi>|cells:…|emitter:<uuid>`. It is parseable, so `resume` rebuilds state from the repository, like floor episodes.

The evidence is an `AlarmDetail` (observed, baseline mean/spread, z, novelty, intervals above, observed seconds, stages applied), stored by T-122 in an `anomaly_detail` table (migration 0003).

### 7.2 Hysteresis and dedupe

`HysteresisConfig` defaults (validated `0 < off < on ≤ 1`): on 0.7, off 0.4, raise after 2 consecutive scored intervals ≥ on, clear after 3 < off, cooldown 1 h.

`HysteresisState::step` returns `quiet | raise | reopen | hold | clear`:
- **Dedupe key:** `AlarmKey { kind, site, subject }`, with adjacent cells merged (gap ≤ 2 baseline cells) before keying.
- **Open key:** extended (supersession, as floor anomalies do), never duplicated.
- **Re-raise within cooldown:** re-opens the same anomaly (appends `open`) rather than creating a row.

### 7.3 Suppression

In order (`alarm::suppression`):
1. `mobile-site`;
2. `unassigned-site`;
3. `provenance-explained` (§7.4);
4. `immature-baseline`;
5. then `dismissed` (the user dismissed the key; expires after 7 days).

Suppressions are counted per kind in `/api/status` and the report, never silently dropped. Immature suppression still lets the candidate's `novelty` stay 0 and the inventory record the new emitter as a `candidate`.

### 7.4 Linkage into anomaly correlation (explain the device first)

Before raising, T-122 runs `ExplanationStage::ORDER`:
1. **Provenance.** Look for gain/cal/spur-mask/antenna/overload/restart/drop/site steps in [t − lookback, t] from the log and tile provenance. If one coincides and the change is consistent with it (e.g. a broadband shift ≈ the gain delta across cells under that gain state), the change is *not* a novelty alarm. It is written as an `Anomaly` of the same kind with a top `Explanation { cause: self-inflicted, … }`, so AWARE-044's "why did my spectrum change?" answers "you changed the gain". Novelty is forced to 0 and the baseline's per-gain-state split keeps the new state from polluting the old one.
2. **Propagation.** Space weather, Es and tropo via the existing feed cache.
3. **External event.** gpsjam, passes, launches, lightning, via `hk_context::correlate`. `Correlator::rank` today returns nothing for kinds other than `noise-floor-rise`; T-122 extends it.
4. **Own history.** A known periodic emitter due, a weekly pattern.
5. **Unexplained.**

Propagation effects are explained, **never suppressed** (C12 pitfall). `AlarmDetail::stages_applied` must start with `provenance` (validated).

## 8. Planned API routes and streams

Named in [`docs/api.md` "Attention and memory (planned, M2; ADR-0012)"](../api.md) with owner per route. Nothing is served yet and no row is in `ROUTES`. When a task lands it moves its rows into a normal section with shapes and adds contract tests in `crates/hk-cli/tests/api_contract.rs` (CLAUDE.md route rule).

Streams (ADR-0004 `messages` kind, metadata only, never content):
- **`observations`** (T-115): `dwell` and `sweep-summary` records for the coverage panel.
- **`anomalies`** (T-122): `anomaly` records on raise/reopen/hold-extend/clear with top explanations.

## 9. Storage home and write pattern

| Data | Home | Format | Write pattern | Retention default |
|---|---|---|---|---|
| Observation log | hk-store `observation/` | hourly CRC-line NDJSON segments | writer thread; flush ≤ 1/min or 256 KiB; fsync at hour seal; drop-and-count when the queue is full | 30 days, 512 MiB |
| Occupancy series | hk-store `occupancy/` | daily CRC-line segments of `OccupancyStat` | one append batch per interval close (15 min); hourly rollup | 15-min 90 days; 1-h 2 years; 256 MiB |
| Learned channel plan | hk-store `occupancy/channels.json` | versioned JSON | atomic rewrite on version change | kept |
| Baselines | hk-store `baseline/` | one binary file per `BaselineKey` (reference + adaptive) | temp → fsync → rename at hour-slot close | 1 GiB, least-recently-visited site first |
| Candidates, bandit arm state | memory | `Arc<CandidateSet>`, arm table | none; arms cold-start, priors from the next snapshot | — |
| Sites, weights | hk-model SQLite (migration 0002, T-119) | rows | on change | kept (user metadata) |
| Alarms | hk-model SQLite `anomaly`/`anomaly_status`/`explanation` + `anomaly_detail` (migration 0003, T-122) | rows | per transition, batched with correlation writes | ADR-0006 retention object |

Why files for the series: they are rebuildable aggregates with steady append rates, and the same pattern already serves history tiles, radiometry and decoded captures. Keeping them out of SQLite avoids WAL churn and write-lock contention with detection/inventory writes (T-112 batching), and limits flash wear to ≤ 1 append per minute per store. SQLite holds what must join with Explanations or be edited by the user.

Low-power mode stretches every flush to 5 min. Shutdown and low battery flush all writers (the `checkpoint` path).

## 10. Type skeletons

In `crates/hk-model/src/attention/`, all serde, wire structs `deny_unknown_fields`, with `validate()` and unit tests:

| File | Types and functions |
|---|---|
| `mod.rs` | `ATTENTION_SCHEMA_VERSION`, `ValidationError` |
| `observation.rs` | `Tier`, `LeaseKind`, `TrustTestKind`, `Reason` (+ `tier`, `text`), `ObservedWindow` (+ `covered`), `DwellRecord`, `HopVisit`, `SweepGeometry`, `SweepRecord`, `ObservationRecord`, `TierSeconds`, `ObservationTotals` |
| `occupancy.rs` | `MIN_GUARD_DB`, `ThresholdMethod`, `ThresholdSpec` (+ `applied_db`), `rbw_correction_db`, `ConfidenceLevel`, `ConfidenceInterval`, `effective_samples`, `fraction_interval`, `TimingRegime`, `ChannelKey` (+ `snap`), `ChannelSource`, `RasterHint`, `Channel`, `OccupancySubject`, `OccupancyStat` |
| `baseline.rs` | `SiteKey`, `SiteSource`, `SiteRecord`, `SiteConfig`, `HourOfWeek`, `CalKey`, `BaselineKey`, `BaselineResolution`, `MATURITY_MIN_OBSERVED_S`, `Maturity` (+ `from_pools`), `SlotStats` (+ `add`, `merge`, moments), `AdaptationPolicy` |
| `score.rs` | `ScoreWeights`, `ScoreComponents`, `interestingness`, `normalised`, `novelty_from_z`, `new_emitter_novelty`, `NoveltyScore`, `CandidateSubject`, `Candidate`, `CandidateSet`, `InterestingnessProvider`, `EmptyInterestingness`, `SharedInterestingness` |
| `schedule.rs` | `ArmKey`, `RewardWeights`, `DwellOutcome` (+ `reward`), `BanditConfig` (+ `dwell_s`), `ucb_index`, `required_revisit_s`, `nominal_poi`, `poi_fraction`, `p_at_least_one`, `PoiEntry` |
| `report.rs` | `SurveyReport` and sections, `ComparisonStatus`, `ChangeEntry`, `CoverageGap`, `CoverageDisclosure`, `ProvenanceStepKind`, `ProvenanceStep`, `ExportFormat` |
| `alarm.rs` | `AlarmKind` (+ `anomaly_kind`), `AlarmSubject`, `AlarmKey` (+ `baseline_ref`/`parse_baseline_ref`), `AlarmRef`, `HysteresisConfig`, `HysteresisState`, `AlarmTransition`, `Suppression`, `suppression`, `ExplanationStage`, `AlarmUnit`, `AlarmDetail` |

Elsewhere: `SiteId` (ids), `AnomalyKind::{LevelAboveBaseline, ChangePoint}` (no exhaustive matches existed), `Purpose::reason`/`tier` (hk-core).

**Evolution rule:** additive changes (optional fields with serde defaults, new enum variants) are allowed in the owning task's PR and flagged for review. Renames, removals or semantic changes amend this ADR first. Bump `ATTENTION_SCHEMA_VERSION` on any change that makes stored records unreadable.

## 11. Crate / file ownership map

T-113 **pre-added** the shared declarations: `pub mod` lines, empty stub modules, the hk-api dispatch chain and marker comments, and the `hk-store` dependency of hk-context. Tasks fill in only their own files.

| Task | Owns (writes) | Codes against | Notes |
|---|---|---|---|
| **T-114** simulator (running) | `crates/hk-sim/**` (or its feature module) | `attention::schedule` (POI, reward), `SharedInterestingness` | Report JSON uses the §5 metric names. |
| **T-115** observation log | `hk-core/src/scheduler/observe.rs`; `hk-store/src/observation/**`; `hk-pipeline/src/observe.rs` + **the single observer call site** in `hk-pipeline/src/control.rs` (`SchedState::new`/`tick`); `hk-api/src/observations.rs`; `observations` stream | `attention::observation`, `Purpose::reason` | Merges before T-120 touches `control.rs`; T-120 rebases over the one call. |
| **T-116** history maturity (running) | `hk-store/src/history/**`, history routes | §1.4 alignment rule | Coverage mask uses the same usable-span rule as `ObservedWindow::usable`. Additive site/source filters for T-121 go in `history/query.rs`. |
| **T-117** synthetic scenes (running) | `py/hkpy/synth/…`, `py/tests` | §2 names | Truth file (`schedule.json`, merged 4a34de5) fields: `channels[].target_fco`, `stats.per_channel[].fco_realized` and `hour_of_week`, `novelty.start_hour`/`start_s`, `observation_schedule.times_s`, `sampled_fco.by_channel` (`n_revisits`, `n_occupied`, `fco`, Wilson CI). |
| **T-118** occupancy engine | `hk-context/src/occupancy/{engine,channels,threshold}.rs`; `hk-store/src/occupancy/**`; `hk-pipeline/src/occupancy.rs`; `hk-api/src/occupancy.rs` | `attention::{occupancy, observation}` | May amend `effective_samples` (§2.4) with evidence. |
| **T-119** baselines + novelty + score | `hk-context/src/occupancy/{baseline,novelty,score,site}.rs`; `hk-store/src/baseline/**`; `hk-model/src/repo/sites.rs` + `migrations/0002_attention.sql` (`site`, `attention_weights`); `hk-pipeline/src/attention.rs`; `hk-api/src/attention.rs` | `attention::{baseline, score}` | Publishes through `SharedInterestingness`. |
| **T-120** bandit scheduler | `hk-core/src/scheduler/bandit/**`, `scheduler/{core,step,config,plan}.rs` and the scheduler module docs; `hk-pipeline/src/control.rs` (except T-115's call site); `hk-sim` policy registration; `hk-api/src/schedule.rs` | `attention::{schedule, score, observation}` | Adds `Purpose` variants + their `reason()` arms. |
| **T-121** reports | `hk-context/src/report/**`; `hk-pipeline/src/reports.rs`; `hk-api/src/reports.rs` | `attention::report` | Moved out of `hk-store/src/history/report` (T-116 owns `history/**`; the report needs C12 comparisons in hk-context). |
| **T-122** alarms | `hk-context/src/occupancy/alarm.rs`; the non-floor-kind gate in `hk-context/src/correlate/mod.rs` (additive); `hk-model/src/repo/alarms.rs` + `migrations/0003_anomaly_detail.sql`; `hk-pipeline/src/alarms.rs`; `hk-api/src/anomalies.rs`; `anomalies` stream | `attention::alarm` | — |
| **T-123** UI hooks | `ui/src/survey/**`, `ui/src/alarms/**`, `ui/src/coverage/**` | `docs/api.md` only | Thin client: no scoring or POI arithmetic client-side. |
| **T-124** acceptance | `tests/e2e/tests/acceptance/m2_*.rs`; a `just` step if > 60 s | everything | Blind, through the mock SDR. |

**Pre-added (final; don't edit):**
- `pub mod` lines:
  - `hk-core/src/scheduler/mod.rs` (`bandit`, `observe`);
  - `hk-store/src/lib.rs` (`baseline`, `observation`, `occupancy`);
  - `hk-context/src/lib.rs` (`occupancy`, `report`) and `occupancy/mod.rs` (all eight files);
  - `hk-api/src/lib.rs` (`anomalies`, `attention`, `observations`, `occupancy`, `reports`, `schedule`);
  - `hk-pipeline/src/lib.rs` (`alarms`, `attention`, `observe`, `occupancy`, `reports`).
- API dispatch: `hk-api/src/http.rs` chains the six route stubs (each returns `None`).
- Cargo: `hk-store` in hk-context. Existing edges already cover every other need: hk-pipeline → hk-context/hk-store/hk-core; hk-api → hk-model/hk-store/hk-core. hk-api reaches hk-context results through traits implemented in hk-pipeline, the `RecipeControl` pattern.

**Shared append-only (textual conflicts only; merge in dependency order T-115 → T-118 → T-119 → T-122):**
- `hk-api/src/http.rs` `ROUTES`: rows under the task's marker comment. `ApiState`: one field per task, appended.
- `hk-cli/tests/api_contract.rs`: tests under the task's marker comment.
- `docs/api.md`: the task's own subsection; move its rows out of "Attention and memory (planned)".
- `hk-cli` `serve`/pipeline construction of `ApiState` and stores: one line per task.
- `hk-model/src/repo/mod.rs` `MIGRATIONS` and `mod` lines: T-119 (0002), then T-122 (0003).
- `hk-pipeline/src/stats.rs` counters: one struct per task, appended.

## Options considered

- **Types in a new `hk-attention` crate.** Rejected: a sixth crate for ~2 k lines of data types with no dependencies beyond hk-model. hk-model already hosts every docs/07 object and the M1 precedent (`hk-recipe`) existed only to avoid linking DSP.
- **Occupancy and baselines in SQLite.** Rejected for the series (§9: write volume, WAL churn, wear, rebuildable), kept for sites/weights/alarms.
- **Per-hop sweep observation rows.** Rejected (~1.7 M rows/day); geometry + visit offsets instead.
- **Reusing tile coverage as the revisit log.** Rejected: tiles cannot give revisit counts, sub-cell gaps or reasons, and T-116 owns their internals. Aligned instead (§1.4).
- **Band-raster channels.** Rejected (§2.7).
- **Geohash/H3 site buckets.** Rejected (§3.5).
- **Per-slot percentile histograms.** Rejected (~8× storage). Tiles keep percentiles, slots keep mergeable moments.
- **Thompson sampling.** Rejected for v1: it needs an RNG in the scheduler, which weakens determinism, and UCB is what ADR-0005 and docs/04 name. It can be tried in the T-114 simulator behind the same arm/reward contract.
- **Suppressing provenance-explained changes entirely.** Rejected: "why did my spectrum change?" must answer "you changed the gain" (AWARE-044).

## Consequences

- Every M2 statistic is traceable to where and when the radio looked, and why. Reports can state coverage and POI rather than imply absence.
- C04 and C12 meet at one trait and one snapshot type. The scheduler stays deterministic and allocation-free, and scoring can run at its own cadence and power mode.
- Blind-first holds end to end: learned channels, measured components, database suggestions capped and post-discovery.
- M2 fans out over disjoint files (§11). The contracts cost ~2 k lines in hk-model that downstream tasks must keep in step (additive-only rule, §10).
- Two `AnomalyKind` variants are added. Consumers comparing kinds are unaffected; any future exhaustive match must handle them.

## 12. Open questions (for the user)

1. **Maturity pooling.** OK to fall back from hour-of-week (needs 24 h *in that slot*, ~24 weeks) to hour-of-day, day part, then all hours (mature after one parked day), with the resolution shown? Or insist on literal hour-of-week maturity?
2. **Local time for slots.** A fixed UTC offset per site (no DST) is simple, but shifts human patterns by an hour for half the year. Accept for M2, or bring in a time-zone database?
3. **Retention defaults.** Observation log 30 d / 512 MiB; occupancy 90 d at 15 min, 2 y hourly; baselines 1 GiB. Right for the device's disk?
4. **Walk surveys.** While moving, occupancy and inventory continue but novelty alarms are off. Is that the behaviour you want, and is 250 m the right default site radius?
5. **Scheduler floors and weights.** 25 % sweep floor, 15 % exploration floor, and novelty-led score weights (1, 2, 1, 0.5, 0.5, 1) as defaults, to be revisited with T-114 simulator numbers?

## Sources

- ITU-R Rec. SM.1880-2, *Spectrum occupancy measurement and evaluation* — https://www.itu.int/rec/R-REC-SM.1880-2-201709-I/en (via docs/04 §3.9; not re-read for this ADR).
- ITU-R Report SM.2256-1, *Spectrum occupancy measurements and evaluation* — https://www.itu.int/dms_pub/itu-r/opb/rep/R-REP-SM.2256-1-2016-PDF-E.pdf (via docs/04 §3.9; the Annex 1 interval form in §2.4 is **unverified** against the text).
- docs/04 §2 (feature taxonomy, score S), §3.8 (sweep vs IBW, POI), §3.9 (occupancy methodology).
- P. Auer, N. Cesa-Bianchi, P. Fischer, "Finite-time analysis of the multiarmed bandit problem", *Machine Learning* 47 (2002) — UCB1.
- A. Garivier, E. Moulines, "On upper-confidence bound policies for switching bandit problems", ALT 2011 — discounted UCB for non-stationary rewards (**unverified** citation details).
- E. B. Wilson, "Probable inference, the law of succession, and statistical inference", *JASA* 22 (1927) — score interval.
- E. S. Page, "Continuous inspection schemes", *Biometrika* 41 (1954) — CUSUM.
- Capability cards C04, C12, C26; ADR-0005, ADR-0006, ADR-0011 (format and parallel-work map precedent).
