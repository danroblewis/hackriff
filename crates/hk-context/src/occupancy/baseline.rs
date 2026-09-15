//! Baselines (T-119, ADR-0012 §3): site × hour-of-week × calibration slot statistics, pooling
//! and maturity, frozen reference + adaptive copy, CUSUM change points.
//!
//! # Input
//! The engine folds [`IntervalObservation`]s: one per subject (baseline cell or learned channel)
//! per occupancy interval. [`from_occupancy_stat`] is the thin adapter for T-118's
//! `OccupancyStat` series; history-tile cells can feed the same type.
//!
//! # Per fold, in order
//! 1. **Slot** = `HourOfWeek::of(t, site offset)`. Levels are kept per front-end gain state (at
//!    most 4, then `mixed`); occupancy pools over all gain states.
//! 2. **Novelty** against the **reference** at the finest mature pool (hour-of-week → hour-of-day
//!    → day part → all hours, ≥ 24 h observed; `Maturity::from_pools`). Immature → 0.
//! 3. **CUSUM** (mature only, not provenance-explained, not mostly suspect) of
//!    x = (adaptive − reference)/σ_ref at that pool, for level and occupancy, both directions;
//!    crossing `cusum_h_sigma` latches a [`ChangePoint`] until the user re-freezes.
//! 4. **Adaptive copy** accrues every fold with forgetting (half-life `half_life_days` of the
//!    subject's observed time), levels winsorised at the reference mean ± 3σ.
//! 5. **Reference** accrues only folds that are not provenance-explained, not mostly suspect, with
//!    no open or building change point (every CUSUM < h/2), into slots whose **hour-of-day pool**
//!    holds < 24 h, and that are either not novel or, while that hour-of-day pool is immature,
//!    below the alarm "on" level ([`REFERENCE_LEARN_MAX_NOVELTY`], §7.2) **and** not novel
//!    (every z < `z_min`) against the slot's own immature hour-of-day reference, whose adaptive
//!    copy has not drifted from it by 2·`cusum_k_sigma` σ or more, and which is itself novel
//!    against the coarse pool (an established pattern explains the fold's novelty).
//!    - The second branch avoids a maturity deadlock: novelty is judged at the finest *mature*
//!      pool, which for a sharply patterned channel (one busy hour a day) is the all-hours pool,
//!      where that hour is novel every day and would otherwise never accrue.
//!    - The own-hour checks keep a change (an interferer, novel against the slot's own history)
//!      out. The on level alone, or a per-fold own-hour z alone, lets a moderate interferer drain
//!      into the immature pool fold by fold, dragging the reference along so the CUSUM never
//!      builds. An interferer's own hour looks like the coarse pool (not established), so it
//!      keeps the strict rule; the drift guard covers a change on an already patterned slot.
//!    - Ending at hour-of-day maturity (~24 parked days) bounds slow-leak poisoning; waiting for
//!      each hour-of-week slot to hold 24 h would keep learning for ~24 weeks.
//!
//!    So the reference is the statistics of "normal" up to maturity, and a persistent new
//!    interferer is not learnt into it (baseline poisoning): it shows in the adaptive copy,
//!    crosses the CUSUM, and stays novel against the reference until
//!    [`BaselineEngine::refreeze`] (user) or `auto_refreeze_days` copies adaptive → reference.
//!    Re-freeze copies the *decayed* adaptive statistics (a few hours per hour-of-week slot at a
//!    14-day half-life), so resolution coarsens and slots below hour-of-day maturity reopen
//!    learning.
//!
//! # T-132 additions
//! - **Level classes.** A fold with occupied weight is an *occupied* fold (level = occupied level
//!   above the floor), otherwise *idle*; their levels pool apart ([`LevelClass`]), so a low-FCO
//!   channel's pool is not bimodal. The adapter gives an occupied level only with at least
//!   [`LEVEL_MIN_OCCUPIED`] occupied visits (fewer: occupancy only). An occupied fold with no
//!   occupied level pool yet is compared with the idle pool (an emitter appearing on a quiet
//!   channel is level novelty), but that cross-class level z does not keep it out of the
//!   reference: learning then judges occupancy only.
//! - **Gain states.** Levels are pooled per front-end gain-state key (the caller's key, e.g.
//!   `hk_store::history::GainState::key`): a gain change starts a separate, immature level pool.
//! - **Sequential learning test.** Every mature, clean fold feeds CUSUMs of its winsorised
//!   (±`z_min`) level and occupancy z against the reference — the slot's own hour-of-day mean once
//!   that holds [`SEQ_OWN_MIN_VISITS`] visits (so a daily pattern is not a shift), else the chosen
//!   pool — subject-wide ([`SubjectBaseline::seq`]) and per hour of day
//!   ([`SubjectBaseline::seq_hod`]); the level slack is widened by that mean's standard error
//!   (k + 1/√n), so an unchanged channel's biased pool mean does not drift the test up. Either
//!   building (≥ h/2) stops reference learning; an
//!   hour-of-day crossing h latches that hour. So a weak persistent interferer, below the novelty z
//!   on every fold, stops entering the reference within a few folds instead of draining into it
//!   until hour-of-day maturity.
//! - **Per-slot latch.** A change point (or sequential crossing) latches reference learning off
//!   for its **hour of day** only ([`SubjectBaseline::latched_hours`]); the subject-wide change-point
//!   CUSUM blocks learning only while it builds before latching. Other hours keep learning unless
//!   their own tests build. Re-freeze clears all of it.
//! - **Memory cap** ([`Baselines::with_memory_cap`]): least recently visited engines are saved and
//!   unloaded; a fold that would grow memory past the cap with only the active key loaded is
//!   refused (counted in [`Baselines::refused_folds`]). Slots are stored sparse (T-134,
//!   [`hk_store::baseline::SlotSeries`]): a series grows with the hour-of-week slots it observes,
//!   so a parked 48 h run holds 48 per series, not 168. Reads are the dense values, so pooling,
//!   maturity, CUSUMs and novelty are unchanged.
//! - **Loading outside the lock** ([`Baselines::load_site_outside_lock`]).
//!
//! # Keys
//! A new calibration is a new `BaselineKey` ([`Baselines::observe`]): it starts immature, so a cal
//! step is never novelty. `Mobile` and `Unassigned` sites never create or update a baseline.
//!
//! All times are the sample clock (ADR-0012 §0).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, PoisonError};

use hk_model::attention::baseline::{
    AdaptationPolicy, BaselineKey, BaselineResolution, CalKey, HourOfWeek, MATURITY_MIN_OBSERVED_S,
    Maturity, SiteKey,
};
use hk_model::attention::occupancy::{OccupancyStat, OccupancySubject};
use hk_model::attention::score::NoveltyScore;
use hk_model::time::Timestamp;
use hk_store::baseline::{
    BaselineState, BaselineStore, BaselineStoreError, BaselineSubject, ChangePoint,
    ChangeStatistic, CusumState, DecayedStats, GainSeries, LevelClass, MAX_GAIN_STATES,
    SubjectBaseline,
};

use super::novelty::{
    Evidence, NoveltyConfig, combine, level_z, occupancy_sigma, occupancy_z, shrunk_fco,
};

/// Suspect share above which a fold updates neither the reference nor the CUSUM.
pub const SUSPECT_MAX_FRACTION: f64 = 0.5;
/// Winsorising half-width, reference σ.
pub const WINSOR_SIGMA: f64 = 3.0;
/// Novelty below which a fold may still enter the reference while its hour-of-day pool is
/// immature: the default alarm "on" level (ADR-0012 §7.2).
pub const REFERENCE_LEARN_MAX_NOVELTY: f64 = 0.7;
/// Occupied visits an interval needs before its occupied level is folded (T-132): one occupied
/// visit on a quiet channel is occupancy evidence, not a level.
pub const LEVEL_MIN_OCCUPIED: u64 = 3;
/// Level visits the slot's own hour-of-day reference needs before the sequential test uses its
/// mean (four parked days of 15-min folds). The mean's standard error is σ/√n: at 4 visits it is
/// 0.5σ, equal to the CUSUM slack, so about a third of an unchanged channel's hours drifted up and
/// held their learning off (T-132 review); at 16 it is 0.25σ.
pub const SEQ_OWN_MIN_VISITS: f64 = 16.0;
/// Observed seconds the own hour-of-day reference needs before the sequential occupancy test uses
/// it.
pub const SEQ_OWN_MIN_OBSERVED_S: f64 = 3600.0;

fn cusum_max(c: &CusumState) -> f64 {
    [c.level_pos, c.level_neg, c.occ_pos, c.occ_neg]
        .into_iter()
        .fold(0.0, f64::max)
}

/// Hour-of-day bit of `slot` in [`SubjectBaseline::latched_hours`].
pub fn hour_bit(slot: HourOfWeek) -> u32 {
    1 << (slot.index() % 24)
}

/// Reference observed time of `slot`'s hour-of-day pool (the 7 hour-of-week slots sharing its
/// hour), over all gain states.
fn hour_of_day_observed_s(sub: &SubjectBaseline, slot: HourOfWeek) -> f64 {
    let h = slot.index() % 24;
    sub.gains
        .iter()
        .flat_map(|g| g.reference.iter())
        .filter(|(i, _)| i % 24 == h)
        .map(|(_, r)| r.observed_s)
        .sum()
}

/// Whether `obs`'s novelty against the chosen (coarse) pool is explained by its own slot's
/// established hour-of-day pattern. Separates a slot repeating its own pattern from a change:
/// - established: the own (possibly immature) hour-of-day reference has history and is itself
///   novel (z ≥ `z_min`) against the chosen pool;
/// - the fold: level and occupancy z below `z_min` against the own hour-of-day reference;
/// - no drift: the own hour-of-day adaptive copy within `max_drift_sigma` of that reference, so a
///   change cannot drain into the reference by dragging it along fold by fold. A reference that
///   tracks a change stays about `max_drift_sigma` behind, above the CUSUM slack, so the CUSUM
///   builds and stops learning.
fn consistent_with_own_hour(
    sub: &SubjectBaseline,
    slot: HourOfWeek,
    gain: Option<usize>,
    obs: &IntervalObservation,
    cfg: &NoveltyConfig,
    max_drift_sigma: f64,
    chosen: Option<(usize, PoolStats, PoolStats)>,
) -> bool {
    let Some((_, chosen_level, chosen_occ)) = chosen else {
        return false;
    };
    let (level, occ) = pools(sub, slot, gain, BaselineCopy::Reference);
    let r = res_index(BaselineResolution::HourOfDay);
    if occ[r].observed_s <= 0.0 {
        return false;
    }
    let ev = obs.evidence();
    // Established pattern: the slot's own history is itself novel against the coarse pool, so
    // that pool (not a change) explains the fold's novelty.
    let own = Evidence {
        level_db: level[r].mean_db(),
        fco: occ[r].fco(),
        ..ev
    };
    let established = level_z(&chosen_level, &own, cfg).is_some_and(|z| z >= cfg.z_min)
        || occupancy_z(&chosen_occ, &own).is_some_and(|z| z.abs() >= cfg.z_min);
    if !established {
        return false;
    }
    let fold_ok = level_z(&level[r], &ev, cfg).is_none_or(|z| z < cfg.z_min)
        && occupancy_z(&occ[r], &ev).is_none_or(|z| z.abs() < cfg.z_min);
    if !fold_ok {
        return false;
    }
    let (ad_level, ad_occ) = pools(sub, slot, gain, BaselineCopy::Adaptive);
    let level_drift = match (ad_level[r].mean_db(), level[r].mean_db()) {
        (Some(a), Some(m)) => {
            (a - m).abs() / level[r].std_db().unwrap_or(0.0).max(cfg.sigma_floor_db)
        }
        _ => 0.0,
    };
    let occ_drift = match (ad_occ[r].fco(), obs.weight_s > 0.0 && obs.n_eff >= 1.0) {
        (Some(af), true) => shrunk_fco(&occ[r], obs.weight_s / obs.n_eff).map_or(0.0, |p| {
            (af - p).abs() / occupancy_sigma(&occ[r], p, obs.n_eff)
        }),
        _ => 0.0,
    };
    level_drift < max_drift_sigma && occ_drift < max_drift_sigma
}

/// One interval's measurement of one subject.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IntervalObservation {
    /// Cell or channel.
    pub subject: BaselineSubject,
    /// Interval start (sample clock): selects the slot.
    pub t: Timestamp,
    /// Front-end gain-state key (0 = unknown/single).
    pub gain: u32,
    /// Representative level, dB (`None`: occupancy only).
    pub level_db: Option<f64>,
    /// Max level, dB.
    pub max_db: Option<f64>,
    /// Σ weight·occupied, s (§2.5).
    pub occupied_weight_s: f64,
    /// Σ weight of usable (non-suspect) visits, s.
    pub weight_s: f64,
    /// Time the visits represent, s (maturity, forgetting).
    pub observed_s: f64,
    /// Effective independent samples behind the FCO (§2.4).
    pub n_eff: f64,
    /// Share of suspect revisits.
    pub suspect_fraction: f64,
    /// A provenance step (gain, filter, retune) explains any change (§7.4).
    pub provenance_explained: bool,
}

impl IntervalObservation {
    /// Time-weighted FCO.
    pub fn fco(&self) -> Option<f64> {
        (self.weight_s > 0.0).then(|| (self.occupied_weight_s / self.weight_s).clamp(0.0, 1.0))
    }

    /// The level pool this fold belongs to: occupied when it carries occupied weight (T-132).
    pub fn level_class(&self) -> LevelClass {
        if self.occupied_weight_s > 0.0 {
            LevelClass::Occupied
        } else {
            LevelClass::Idle
        }
    }

    fn evidence(&self) -> Evidence {
        Evidence {
            level_db: self.level_db,
            fco: self.fco(),
            n_eff: self.n_eff,
            weight_s: self.weight_s,
            observed_s: self.observed_s,
        }
    }

    fn usable(&self) -> bool {
        self.observed_s.is_finite()
            && self.observed_s >= 0.0
            && self.weight_s.is_finite()
            && self.weight_s >= 0.0
            && self.occupied_weight_s.is_finite()
            && self.level_db.is_none_or(f64::is_finite)
    }
}

/// T-118 adapter: an `OccupancyStat` row as a fold, with its site and calibration keys. `None` for
/// band subjects and rows without usable revisits.
///
/// **Level (T-128)** is the channel's level **above its local noise floor**
/// ([`channel_level_excess`]): the median occupied-visit level (`level_occupied_p50_db`) when at
/// least [`LEVEL_MIN_OCCUPIED`] visits were occupied, the median idle level (`level_idle_db`) when
/// none was, and no level (occupancy only) in between (T-132), minus `floor_db`; `max_db` is
/// the 90th-percentile occupied level minus the floor. So an emitter appearing raises the level by
/// its SNR (level novelty), while a noise-floor rise moves floor and levels together and leaves
/// the excess unchanged (no level novelty, no level change point duplicating the noise-floor
/// anomalies). The row's `threshold_db` is never used as a level (it follows the floor). A row
/// without a floor, or whose floor may be signal (`floor_suspect`), folds occupancy only.
///
/// The represented time is `min(interval, n_revisits_all × revisit_mean_s)` and the weight its
/// non-suspect share. `gain` is the interval's front-end gain-state key (T-132: e.g.
/// `hk_context::occupancy::engine::dominant_gain_key` of its visits; 0 = unknown/single).
pub fn from_occupancy_stat(
    stat: &OccupancyStat,
    gain: u32,
) -> Option<(SiteKey, CalKey, IntervalObservation)> {
    let OccupancySubject::Channel { key } = stat.subject else {
        return None;
    };
    if stat.n_revisits == 0 {
        return None;
    }
    let interval_s =
        (stat.interval.end.as_unix_nanos() - stat.interval.start.as_unix_nanos()) as f64 / 1e9;
    let represented = stat
        .revisit_mean_s
        .map_or(interval_s, |m| m * stat.n_revisits_all as f64)
        .min(interval_s);
    let usable = stat.n_revisits.saturating_sub(stat.n_suspect);
    let weight = represented * usable as f64 / stat.n_revisits as f64;
    let fco = stat.fco.unwrap_or(0.0);
    let (level_db, max_db) = channel_level_excess(stat).unzip();
    let obs = IntervalObservation {
        subject: BaselineSubject::Channel { key },
        t: stat.interval.start,
        gain,
        level_db,
        max_db: max_db.flatten(),
        occupied_weight_s: if stat.fco.is_some() {
            fco * weight
        } else {
            0.0
        },
        weight_s: if stat.fco.is_some() { weight } else { 0.0 },
        observed_s: represented,
        n_eff: stat.confidence.map_or(usable as f64, |c| c.n_eff),
        suspect_fraction: stat.n_suspect as f64 / stat.n_revisits as f64,
        provenance_explained: false,
    };
    let cal = stat
        .calibration
        .map_or(CalKey::Uncalibrated, CalKey::Calibrated);
    Some((stat.site, cal, obs))
}

/// A channel row's representative level and max level above its local floor, dB
/// (see [`from_occupancy_stat`]). `None` without a trustworthy floor or any level.
pub fn channel_level_excess(stat: &OccupancyStat) -> Option<(f64, Option<f64>)> {
    let floor = stat.floor_db.filter(|f| f.is_finite())?;
    if stat.floor_suspect == Some(true) {
        return None;
    }
    let occupied = stat
        .level_occupied_p50_db
        .filter(|l| l.is_finite() && stat.n_occupied >= LEVEL_MIN_OCCUPIED);
    let level = match occupied {
        Some(l) => l,
        None if stat.n_occupied == 0 => stat.level_idle_db.filter(|l| l.is_finite())?,
        None => return None,
    };
    let max = stat
        .level_occupied_p90_db
        .filter(|l| l.is_finite() && occupied.is_some())
        .map_or(level, |p90| p90.max(level));
    Some((level - floor, Some(max - floor)))
}

/// Pooled moments over a set of slots (real-valued counts, so both copies pool alike), with the
/// weighted spread of slot FCOs for coarse pools.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PoolStats {
    /// Σ visits (level).
    pub n: f64,
    /// Observed seconds.
    pub observed_s: f64,
    /// Σ level.
    pub sum_db: f64,
    /// Σ level².
    pub sum_sq_db: f64,
    /// Σ weight·occupied.
    pub occupied_weight_s: f64,
    /// Σ weight.
    pub weight_s: f64,
    /// Max level.
    pub max_db: f64,
    /// Σ weight·fco_slot².
    fco_sq_w: f64,
}

impl Default for PoolStats {
    fn default() -> Self {
        Self {
            n: 0.0,
            observed_s: 0.0,
            sum_db: 0.0,
            sum_sq_db: 0.0,
            occupied_weight_s: 0.0,
            weight_s: 0.0,
            max_db: f64::NEG_INFINITY,
            fco_sq_w: 0.0,
        }
    }
}

impl PoolStats {
    /// Adds one slot's moments.
    #[allow(clippy::too_many_arguments)]
    pub fn add(
        &mut self,
        n: f64,
        observed_s: f64,
        sum_db: f64,
        sum_sq_db: f64,
        occupied_weight_s: f64,
        weight_s: f64,
        max_db: f64,
    ) {
        self.n += n;
        self.observed_s += observed_s;
        self.sum_db += sum_db;
        self.sum_sq_db += sum_sq_db;
        self.occupied_weight_s += occupied_weight_s;
        self.weight_s += weight_s;
        self.max_db = self.max_db.max(max_db);
        if weight_s > 0.0 {
            let f = occupied_weight_s / weight_s;
            self.fco_sq_w += weight_s * f * f;
        }
    }

    fn add_decayed(&mut self, d: &DecayedStats) {
        self.add(
            d.n,
            d.observed_s,
            d.sum_db,
            d.sum_sq_db,
            d.occupied_weight_s,
            d.weight_s,
            d.max_db,
        );
    }

    /// These level moments with `o`'s occupancy moments (a level pool of one gain state combined
    /// with the all-gain-states occupancy pool, for display).
    pub fn with_occupancy_of(mut self, o: &PoolStats) -> Self {
        self.observed_s = o.observed_s;
        self.occupied_weight_s = o.occupied_weight_s;
        self.weight_s = o.weight_s;
        self.fco_sq_w = o.fco_sq_w;
        self
    }

    /// Mean level.
    pub fn mean_db(&self) -> Option<f64> {
        (self.n > 1e-9).then(|| self.sum_db / self.n)
    }

    /// Sample σ of the level.
    pub fn std_db(&self) -> Option<f64> {
        if self.n < 2.0 {
            return None;
        }
        let m = self.sum_db / self.n;
        Some(
            ((self.sum_sq_db - self.n * m * m) / (self.n - 1.0))
                .max(0.0)
                .sqrt(),
        )
    }

    /// Time-weighted FCO.
    pub fn fco(&self) -> Option<f64> {
        (self.weight_s > 0.0).then(|| (self.occupied_weight_s / self.weight_s).clamp(0.0, 1.0))
    }

    /// Weighted variance of slot FCOs around the pool FCO (0 for a single slot).
    pub fn between_var(&self) -> f64 {
        match self.fco() {
            Some(f) => (self.fco_sq_w / self.weight_s - f * f).max(0.0),
            None => 0.0,
        }
    }
}

/// Whether slot `i` belongs to `slot`'s pool at `res`.
pub fn in_pool(i: usize, slot: HourOfWeek, res: BaselineResolution) -> bool {
    let s = slot.index();
    match res {
        BaselineResolution::HourOfWeek => i == s,
        BaselineResolution::HourOfDay => i % 24 == s % 24,
        BaselineResolution::DayPart => (i % 24) / 6 == (s % 24) / 6,
        BaselineResolution::AllHours => true,
    }
}

/// Index of `res` in the pool arrays [`pools`] returns (finest first).
pub fn res_index(res: BaselineResolution) -> usize {
    BaselineResolution::FINEST_FIRST
        .iter()
        .position(|r| *r == res)
        .expect("listed")
}

/// Which copy to pool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BaselineCopy {
    /// Frozen reference.
    Reference,
    /// Adaptive copy.
    Adaptive,
}

/// Level (one gain state) and occupancy (all gain states) pools of `sub` for `slot`, at all four
/// resolutions (finest first).
pub fn pools(
    sub: &SubjectBaseline,
    slot: HourOfWeek,
    gain: Option<usize>,
    copy: BaselineCopy,
) -> ([PoolStats; 4], [PoolStats; 4]) {
    let mut level = [PoolStats::default(); 4];
    let mut occ = [PoolStats::default(); 4];
    for (gi, g) in sub.gains.iter().enumerate() {
        // Stored slots only, in slot order (T-134): an untouched slot is empty and was skipped.
        let slots: Box<dyn Iterator<Item = (usize, DecayedStats)>> = match copy {
            BaselineCopy::Reference => {
                Box::new(g.reference.iter().map(|(i, s)| (i, DecayedStats::from(s))))
            }
            BaselineCopy::Adaptive => Box::new(g.adaptive.iter().map(|(i, d)| (i, *d))),
        };
        for (i, d) in slots {
            if d.is_empty() && d.weight_s <= 0.0 && d.observed_s <= 0.0 {
                continue;
            }
            for res in BaselineResolution::FINEST_FIRST {
                if !in_pool(i, slot, res) {
                    continue;
                }
                let r = res_index(res);
                // Occupancy only (no level moments) so each slot's FCO enters the spread once.
                occ[r].add(
                    0.0,
                    d.observed_s,
                    0.0,
                    0.0,
                    d.occupied_weight_s,
                    d.weight_s,
                    d.max_db,
                );
                if Some(gi) == gain {
                    level[r].add_decayed(&d);
                }
            }
        }
    }
    (level, occ)
}

/// Maturity of `sub`'s reference for `slot`.
pub fn maturity(sub: &SubjectBaseline, slot: HourOfWeek) -> Maturity {
    let (_, occ) = pools(sub, slot, None, BaselineCopy::Reference);
    Maturity::from_pools(occ.map(|p| p.observed_s))
}

/// Baseline settings.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BaselineConfig {
    /// Adaptation (half-life, CUSUM, auto re-freeze).
    pub policy: AdaptationPolicy,
    /// Novelty mapping.
    pub novelty: NoveltyConfig,
}

/// What one fold produced.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FoldOutcome {
    /// Novelty against the reference (before this fold was added).
    pub novelty: NoveltyScore,
    /// A change point latched by this fold.
    pub change_point: Option<ChangePoint>,
    /// The fold updated a baseline (false for mobile/unassigned sites and unusable folds).
    pub accrued: bool,
    /// The fold also entered the reference.
    pub accrued_reference: bool,
}

impl FoldOutcome {
    fn none(obs: &IntervalObservation, maturity: Maturity) -> Self {
        Self {
            novelty: combine(
                maturity,
                None,
                None,
                None,
                obs.observed_s,
                obs.provenance_explained,
                &NoveltyConfig::default(),
            ),
            change_point: None,
            accrued: false,
            accrued_reference: false,
        }
    }
}

/// Baseline of one key.
#[derive(Clone, Debug)]
pub struct BaselineEngine {
    /// Persisted state.
    pub state: BaselineState,
    utc_offset_min: i16,
    cfg: BaselineConfig,
    dirty: bool,
    /// Approximate heap bytes of `state` (maintained per fold).
    bytes: usize,
}

fn secs(a: Timestamp, b: Timestamp) -> f64 {
    (b.as_unix_nanos() - a.as_unix_nanos()) as f64 / 1e9
}

fn winsorise(level: f64, pool: &PoolStats, cfg: &NoveltyConfig) -> f64 {
    match (pool.mean_db(), pool.std_db()) {
        (Some(m), Some(s)) => {
            let h = WINSOR_SIGMA * s.max(cfg.sigma_floor_db);
            level.clamp(m - h, m + h)
        }
        _ => level,
    }
}

impl BaselineEngine {
    /// Wraps (possibly loaded) `state`.
    pub fn new(state: BaselineState, utc_offset_min: i16, cfg: BaselineConfig) -> Self {
        let bytes = state.approx_bytes();
        Self {
            state,
            utc_offset_min,
            cfg,
            dirty: false,
            bytes,
        }
    }

    /// Approximate heap bytes of the state ([`BaselineState::approx_bytes`], kept per fold).
    pub fn approx_bytes(&self) -> usize {
        self.bytes
    }

    /// Upper bound of the bytes folding `obs` would add: a new subject, series, stored slot
    /// (slots are sparse, T-134) or hour-of-day accumulators.
    pub fn growth_of(&self, obs: &IntervalObservation) -> usize {
        let series = std::mem::size_of::<GainSeries>();
        let slot = self.slot(obs.t).index();
        let heap = GainSeries::new(0).growth_of(slot);
        let hod = 24 * std::mem::size_of::<CusumState>();
        let class = obs.level_class();
        match self.state.subjects.get(&obs.subject) {
            // A new subject has no reference, so it is immature: no hour-of-day accumulators yet.
            None => SubjectBaseline::new(obs.t).approx_bytes() + 4 * series + heap,
            Some(sub) => {
                let hod = if sub.seq_hod.is_empty() { hod } else { 0 };
                let full = sub.gains.iter().filter(|g| g.class == class).count() >= MAX_GAIN_STATES;
                if let Some(gi) = gain_index(sub, obs.gain, class) {
                    hod + sub.gains[gi].growth_of(slot)
                } else if full {
                    hod
                } else {
                    let cap = sub.gains.capacity();
                    let grow = if sub.gains.len() == cap {
                        cap.max(4)
                    } else {
                        0
                    };
                    hod + heap + grow * series
                }
            }
        }
    }

    /// Folds one observation (steps 2–5 of the module docs), keeping the byte count.
    pub fn observe(&mut self, obs: &IntervalObservation) -> FoldOutcome {
        let size = |e: &Self| {
            e.state
                .subjects
                .get(&obs.subject)
                .map_or(0, SubjectBaseline::approx_bytes)
        };
        let before = size(self);
        let out = self.fold(obs);
        self.bytes = (self.bytes + size(self)).saturating_sub(before);
        out
    }

    /// Changed since the last save.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Marks saved.
    pub fn mark_saved(&mut self) {
        self.dirty = false;
    }

    /// Slot of `t` at this site.
    pub fn slot(&self, t: Timestamp) -> HourOfWeek {
        HourOfWeek::of(t, self.utc_offset_min)
    }

    /// Novelty of `obs` against the reference without folding it.
    pub fn novelty_of(&self, obs: &IntervalObservation) -> NoveltyScore {
        let slot = self.slot(obs.t);
        match self.state.subjects.get(&obs.subject) {
            Some(sub) => {
                self.evaluate(sub, slot, gain_index(sub, obs.gain, obs.level_class()), obs)
                    .0
            }
            None => FoldOutcome::none(obs, Maturity::Immature { observed_s: 0.0 }).novelty,
        }
    }

    /// Returns the novelty, when mature the chosen resolution with its reference pools (the level
    /// pool of the fold's own class), and the novelty that gates reference learning (without a
    /// cross-class level z, T-132).
    fn evaluate(
        &self,
        sub: &SubjectBaseline,
        slot: HourOfWeek,
        gain: Option<usize>,
        obs: &IntervalObservation,
    ) -> (NoveltyScore, Option<(usize, PoolStats, PoolStats)>, f64) {
        let (level, occ) = pools(sub, slot, gain, BaselineCopy::Reference);
        let m = Maturity::from_pools(occ.map(|p| p.observed_s));
        let cfg = &self.cfg.novelty;
        let Maturity::Mature { resolution } = m else {
            return (FoldOutcome::none(obs, m).novelty, None, 0.0);
        };
        let r = res_index(resolution);
        let ev = obs.evidence();
        // An occupied fold without an occupied level pool yet: compare with the idle pool.
        let fallback = (level[r].n < 2.0 && obs.level_class() == LevelClass::Occupied)
            .then(|| gain_index(sub, obs.gain, LevelClass::Idle))
            .flatten()
            .map(|ig| pools(sub, slot, Some(ig), BaselineCopy::Reference).0[r])
            .filter(|p| p.n >= 2.0);
        let lz = level_z(fallback.as_ref().unwrap_or(&level[r]), &ev, cfg);
        let oz = occupancy_z(&occ[r], &ev);
        let combined = |lz| {
            combine(
                m,
                lz,
                oz,
                None,
                obs.observed_s,
                obs.provenance_explained,
                cfg,
            )
        };
        let n = combined(lz);
        let learn = if fallback.is_some() {
            combined(None).novelty
        } else {
            n.novelty
        };
        (n, Some((r, level[r], occ[r])), learn)
    }

    fn fold(&mut self, obs: &IntervalObservation) -> FoldOutcome {
        if !obs.usable() {
            let m = self
                .state
                .subjects
                .get(&obs.subject)
                .map_or(Maturity::Immature { observed_s: 0.0 }, |s| {
                    maturity(s, self.slot(obs.t))
                });
            return FoldOutcome::none(obs, m);
        }
        let slot = self.slot(obs.t);
        let policy = self.cfg.policy;
        let ncfg = self.cfg.novelty;
        let sub = self
            .state
            .subjects
            .entry(obs.subject)
            .or_insert_with(|| SubjectBaseline::new(obs.t));
        let class = obs.level_class();
        let gain = match gain_index(sub, obs.gain, class) {
            Some(g) => Some(g),
            None if sub.gains.iter().filter(|g| g.class == class).count() < MAX_GAIN_STATES => {
                sub.gains.push(GainSeries::with_class(obs.gain, class));
                Some(sub.gains.len() - 1)
            }
            None => {
                sub.mixed = true;
                None
            }
        };
        // Auto re-freeze an unanswered change point.
        if let (Some(days), Some(cp)) = (policy.auto_refreeze_days, sub.change_point)
            && secs(cp.t, obs.t) >= days * 86_400.0
        {
            refreeze_subject(sub, obs.t);
        }
        let sub_ref: &SubjectBaseline = sub;
        let engine = Self {
            state: BaselineState::new(self.state.key, obs.t),
            utc_offset_min: self.utc_offset_min,
            cfg: self.cfg,
            dirty: false,
            bytes: 0,
        };
        let (novelty, chosen, learn_novelty) = engine.evaluate(sub_ref, slot, gain, obs);
        let sub = self
            .state
            .subjects
            .get_mut(&obs.subject)
            .expect("inserted above");
        let clean = !obs.provenance_explained && obs.suspect_fraction < SUSPECT_MAX_FRACTION;

        // CUSUM of adaptive vs reference at the chosen pool.
        let mut raised = None;
        if let (Some((r, ref_level, ref_occ)), true) = (chosen, clean) {
            let (ad_level, ad_occ) = pools(sub, slot, gain, BaselineCopy::Adaptive);
            let k = policy.cusum_k_sigma;
            let c = &mut sub.cusum;
            let mut crossed: Option<(ChangeStatistic, i8, f64)> = None;
            if let (Some(am), Some(rm)) = (ad_level[r].mean_db(), ref_level.mean_db()) {
                let x = (am - rm) / ref_level.std_db().unwrap_or(0.0).max(ncfg.sigma_floor_db);
                c.level_pos = (c.level_pos + x - k).max(0.0);
                c.level_neg = (c.level_neg - x - k).max(0.0);
                for (v, d) in [(c.level_pos, 1), (c.level_neg, -1)] {
                    if v >= policy.cusum_h_sigma && crossed.is_none() {
                        crossed = Some((ChangeStatistic::Level, d, v));
                    }
                }
            }
            if let (Some(af), true) = (ad_occ[r].fco(), obs.weight_s > 0.0 && obs.n_eff >= 1.0)
                && let Some(p) = shrunk_fco(&ref_occ, obs.weight_s / obs.n_eff)
            {
                let x = (af - p) / occupancy_sigma(&ref_occ, p, obs.n_eff);
                c.occ_pos = (c.occ_pos + x - k).max(0.0);
                c.occ_neg = (c.occ_neg - x - k).max(0.0);
                for (v, d) in [(c.occ_pos, 1), (c.occ_neg, -1)] {
                    if v >= policy.cusum_h_sigma && crossed.is_none() {
                        crossed = Some((ChangeStatistic::Occupancy, d, v));
                    }
                }
            }
            if sub.change_point.is_none()
                && let Some((statistic, direction, cusum)) = crossed
            {
                let cp = ChangePoint {
                    t: obs.t,
                    statistic,
                    direction,
                    cusum,
                };
                sub.change_point = Some(cp);
                // Per-slot latch: the crossing's hour of day stops learning.
                sub.latched_hours |= hour_bit(slot);
                raised = Some(cp);
            }
        }

        // T-132 sequential learning test (module docs).
        let hod = slot.index() % 24;
        if let (Some((_, ref_level, ref_occ)), true) = (chosen, clean) {
            let (own_level, own_occ) = pools(sub, slot, gain, BaselineCopy::Reference);
            let hr = res_index(BaselineResolution::HourOfDay);
            let zc = ncfg.z_min;
            // The pool mean is itself uncertain (standard error σ/√n): the level slack is widened
            // by it, so an unchanged channel's biased own-hour mean does not drift the test up.
            let level_pool = if own_level[hr].n >= SEQ_OWN_MIN_VISITS {
                own_level[hr]
            } else {
                ref_level
            };
            let k_level = policy.cusum_k_sigma + 1.0 / level_pool.n.max(1.0).sqrt();
            let lz = obs.level_db.and_then(|l| {
                let mean = level_pool.mean_db()?;
                let sigma = ref_level.std_db().unwrap_or(0.0).max(ncfg.sigma_floor_db);
                Some(((l - mean) / sigma).clamp(-zc, zc))
            });
            let occ_pool = if own_occ[hr].observed_s >= SEQ_OWN_MIN_OBSERVED_S {
                &own_occ[hr]
            } else {
                &ref_occ
            };
            // Unshrunk pool FCO as the mean (shrinkage towards ½ would bias a quiet channel's
            // folds low against a small own-hour pool); the shrunk FCO only sizes σ.
            let oz = match (obs.fco(), occ_pool.fco()) {
                (Some(f), Some(p)) if obs.n_eff >= 1.0 && obs.weight_s > 0.0 => {
                    shrunk_fco(occ_pool, obs.weight_s / obs.n_eff).map(|ps| {
                        ((f - p) / occupancy_sigma(occ_pool, ps, obs.n_eff)).clamp(-zc, zc)
                    })
                }
                _ => None,
            };
            if sub.seq_hod.len() != 24 {
                sub.seq_hod = vec![CusumState::default(); 24];
            }
            let k = policy.cusum_k_sigma;
            for c in [&mut sub.seq, &mut sub.seq_hod[hod]] {
                if let Some(z) = lz {
                    c.level_pos = (c.level_pos + z - k_level).max(0.0);
                    c.level_neg = (c.level_neg - z - k_level).max(0.0);
                }
                if let Some(z) = oz {
                    c.occ_pos = (c.occ_pos + z - k).max(0.0);
                    c.occ_neg = (c.occ_neg - z - k).max(0.0);
                }
            }
            // Latch only with a novel fold: a sustained sub-novelty shift is gated while it
            // builds, but ordinary fat-tailed noise must not switch an hour off for good.
            if cusum_max(&sub.seq_hod[hod]) >= policy.cusum_h_sigma && novelty.novelty > 0.0 {
                sub.latched_hours |= hour_bit(slot);
            }
        }

        let Some(gi) = gain else {
            // Beyond the kept gain states: nothing more is folded.
            return FoldOutcome {
                novelty,
                change_point: raised,
                accrued: false,
                accrued_reference: false,
            };
        };
        let ref_level_pool = chosen.map(|c| c.1);
        let level = obs.level_db.map(|l| match &ref_level_pool {
            Some(p) => winsorise(l, p, &ncfg),
            None => l,
        });
        let max_db = obs.max_db.or(obs.level_db).unwrap_or(f64::NEG_INFINITY);
        let slot_i = slot.index();

        // Adaptive copy with forgetting over the subject's observed time.
        let factor = 0.5_f64.powf(obs.observed_s / (policy.half_life_days * 86_400.0));
        for g in &mut sub.gains {
            for d in &mut g.adaptive {
                d.scale(factor);
            }
        }
        let a = &mut sub.gains[gi].adaptive[slot_i];
        match level {
            Some(l) => a.add(
                l,
                max_db,
                obs.occupied_weight_s,
                obs.weight_s,
                obs.observed_s,
            ),
            None => {
                a.observed_s += obs.observed_s;
                a.occupied_weight_s += obs.occupied_weight_s;
                a.weight_s += obs.weight_s;
            }
        }

        // Reference: normal folds only, until the slot's hour-of-day pool is mature.
        let hod_immature = hour_of_day_observed_s(sub, slot) < MATURITY_MIN_OBSERVED_S;
        // Not novel, or (while the hour-of-day pool is immature, which learning requires anyway)
        // below the on level and consistent with the slot's own hour-of-day history.
        let normal = learn_novelty == 0.0
            || (learn_novelty < REFERENCE_LEARN_MAX_NOVELTY
                && hod_immature
                && consistent_with_own_hour(
                    sub,
                    slot,
                    gain,
                    obs,
                    &ncfg,
                    2.0 * policy.cusum_k_sigma,
                    chosen,
                ));
        // The change-point CUSUM blocks every slot only while it builds towards a first crossing;
        // once latched, the crossing's hour stays off and the sequential tests gate the others.
        let h2 = 0.5 * policy.cusum_h_sigma;
        let building = sub.change_point.is_none() && cusum_max(&sub.cusum) >= h2;
        let seq_building =
            cusum_max(&sub.seq) >= h2 || sub.seq_hod.get(hod).is_some_and(|c| cusum_max(c) >= h2);
        let latched = sub.latched_hours & hour_bit(slot) != 0;
        let accrue_ref = clean && normal && !latched && !building && !seq_building && hod_immature;
        if accrue_ref {
            let s = &mut sub.gains[gi].reference[slot_i];
            match level {
                Some(l) => s.add(
                    l,
                    max_db,
                    false,
                    obs.weight_s - obs.occupied_weight_s,
                    obs.observed_s,
                ),
                None => {
                    s.observed_s += obs.observed_s;
                    s.weight_s += obs.weight_s - obs.occupied_weight_s;
                }
            }
            // `add` takes a boolean occupancy; fold the time-weighted part directly.
            s.occupied_weight_s += obs.occupied_weight_s;
            s.weight_s += obs.occupied_weight_s;
        }
        if sub.mature_at.is_none() && maturity(sub, slot).is_mature() {
            sub.mature_at = Some(obs.t);
        }
        sub.last_seen = sub.last_seen.max(obs.t);
        self.state.last_visit = self.state.last_visit.max(obs.t);
        self.dirty = true;
        FoldOutcome {
            novelty,
            change_point: raised,
            accrued: true,
            accrued_reference: accrue_ref,
        }
    }

    /// Copies adaptive → reference for subjects matching `filter`, clears their change points and
    /// CUSUMs. Returns how many were re-frozen.
    ///
    /// The copy is of the *decayed* adaptive statistics: at a 14-day half-life each hour-of-week
    /// slot holds only a few hours (~3 h) of effective observed time, so the re-frozen reference
    /// is mature at a coarser resolution than before, and slots whose hour-of-day pool falls below
    /// 24 h reopen reference learning.
    pub fn refreeze(
        &mut self,
        t: Timestamp,
        mut filter: impl FnMut(&BaselineSubject) -> bool,
    ) -> usize {
        let mut n = 0;
        for (subject, sub) in &mut self.state.subjects {
            if filter(subject) {
                refreeze_subject(sub, t);
                n += 1;
            }
        }
        if n > 0 {
            self.dirty = true;
        }
        n
    }
}

fn gain_index(sub: &SubjectBaseline, gain: u32, class: LevelClass) -> Option<usize> {
    sub.gains
        .iter()
        .position(|g| g.gain == gain && g.class == class)
}

fn refreeze_subject(sub: &mut SubjectBaseline, t: Timestamp) {
    for g in &mut sub.gains {
        // Every slot: untouched adaptive slots re-freeze to untouched (empty) reference slots.
        g.reference = g.adaptive.map(to_slot_stats);
    }
    sub.cusum = Default::default();
    sub.change_point = None;
    sub.seq = Default::default();
    sub.seq_hod.iter_mut().for_each(|c| *c = Default::default());
    sub.latched_hours = 0;
    sub.refrozen_at = Some(t);
}

/// A decayed slot as integer-count `SlotStats`, moments rescaled so the mean is unchanged.
fn to_slot_stats(d: &DecayedStats) -> hk_model::attention::baseline::SlotStats {
    use hk_model::attention::baseline::SlotStats;
    let n = d.n.round();
    let k = if d.n > 0.0 { n / d.n } else { 0.0 };
    if n < 1.0 {
        return SlotStats {
            observed_s: d.observed_s,
            occupied_weight_s: d.occupied_weight_s,
            weight_s: d.weight_s,
            ..SlotStats::EMPTY
        };
    }
    SlotStats {
        n_visits: n as u64,
        observed_s: d.observed_s,
        sum_db: d.sum_db * k,
        sum_sq_db: d.sum_sq_db * k,
        occupied_weight_s: d.occupied_weight_s,
        weight_s: d.weight_s,
        max_db: d.max_db,
    }
}

/// All baselines of a run: one engine per key, loaded from and saved to the store at slot close.
#[derive(Debug)]
pub struct Baselines {
    cfg: BaselineConfig,
    scheme: u16,
    cell_factor: u16,
    store: Option<BaselineStore>,
    engines: BTreeMap<BaselineKey, BaselineEngine>,
    last_hour: Option<i64>,
    /// Site of the latest fold: protected from quota eviction.
    current: Option<hk_model::ids::SiteId>,
    /// Memory cap, bytes (T-132; `None` = unbounded).
    memory_cap: Option<usize>,
    refused_folds: u64,
    unloaded_engines: u64,
    gain_overflow_folds: u64,
    /// Bumped whenever engines leave memory (cap unload, quota eviction): a read planned under an
    /// older generation may hold a stale disk copy and is re-planned, not inserted.
    generation: u64,
}

/// What [`Baselines::load_plan`] hands to the unlocked read: the store and the keys already in
/// memory.
#[derive(Clone, Debug)]
pub struct LoadPlan {
    /// Store to read.
    pub store: BaselineStore,
    /// Keys already loaded (not read again).
    pub loaded: BTreeSet<BaselineKey>,
    /// [`Baselines`]' unload generation at plan time.
    pub generation: u64,
}

/// Attempts [`Baselines::load_site_outside_lock`] makes before reading under the lock.
const LOAD_OUTSIDE_LOCK_ATTEMPTS: usize = 3;

impl Baselines {
    /// A set over `store` (None = memory only) on grid `scheme` × `cell_factor`.
    pub fn new(
        cfg: BaselineConfig,
        scheme: u16,
        cell_factor: u16,
        store: Option<BaselineStore>,
    ) -> Self {
        Self {
            cfg,
            scheme,
            cell_factor,
            store,
            engines: BTreeMap::new(),
            last_hour: None,
            current: None,
            memory_cap: None,
            refused_folds: 0,
            unloaded_engines: 0,
            gain_overflow_folds: 0,
            generation: 0,
        }
    }

    /// Replaces the memory cap (`None` = unbounded); applies from the next fold or load.
    pub fn set_memory_cap(&mut self, bytes: Option<usize>) {
        self.memory_cap = bytes;
    }

    /// Bounds the loaded engines to about `bytes` ([`BaselineState::approx_bytes`]): least
    /// recently visited engines are saved and unloaded (reloaded on their next fold), and a fold
    /// that would grow past the cap with nothing else to unload is refused. Without a store,
    /// nothing is unloaded (it would be lost) and only growth is refused.
    pub fn with_memory_cap(mut self, bytes: usize) -> Self {
        self.memory_cap = Some(bytes);
        self
    }

    /// Approximate heap bytes of the loaded engines.
    pub fn memory_bytes(&self) -> usize {
        self.engines
            .values()
            .map(BaselineEngine::approx_bytes)
            .sum()
    }

    /// Folds refused by the memory cap.
    pub fn refused_folds(&self) -> u64 {
        self.refused_folds
    }

    /// Engines unloaded by the memory cap.
    pub fn unloaded_engines(&self) -> u64 {
        self.unloaded_engines
    }

    /// Saves and unloads least recently visited engines other than `keep` until within `cap`.
    fn unload_to(
        &mut self,
        cap: usize,
        keep: Option<BaselineKey>,
    ) -> Result<(), BaselineStoreError> {
        let Some(store) = &self.store else {
            return Ok(());
        };
        while self.memory_bytes() > cap {
            let Some(victim) = self
                .engines
                .iter()
                .filter(|(k, _)| Some(**k) != keep)
                .min_by_key(|(_, e)| e.state.last_visit)
                .map(|(k, _)| *k)
            else {
                break;
            };
            let e = self.engines.remove(&victim).expect("listed");
            if e.is_dirty()
                && let Err(err) = store.save(&e.state)
            {
                self.engines.insert(victim, e);
                return Err(err);
            }
            self.unloaded_engines += 1;
            self.generation += 1;
        }
        Ok(())
    }

    /// The key a fold under `site`/`cal` goes to; `None` for sites that do not accrue.
    pub fn key(&self, site: SiteKey, cal: CalKey) -> Option<BaselineKey> {
        match site {
            SiteKey::Site(id) => Some(BaselineKey {
                site: id,
                cal,
                scheme: self.scheme,
                cell_factor: self.cell_factor,
            }),
            SiteKey::Mobile | SiteKey::Unassigned => None,
        }
    }

    fn engine(
        &mut self,
        key: BaselineKey,
        utc_offset_min: i16,
        t: Timestamp,
    ) -> Result<&mut BaselineEngine, BaselineStoreError> {
        if !self.engines.contains_key(&key) {
            let state = match &self.store {
                Some(s) => s.load(&key)?,
                None => None,
            }
            .unwrap_or_else(|| BaselineState::new(key, t));
            self.engines
                .insert(key, BaselineEngine::new(state, utc_offset_min, self.cfg));
        }
        Ok(self.engines.get_mut(&key).expect("inserted"))
    }

    /// Folds `obs` under `site` and `cal`. Mobile/unassigned: nothing accrues and novelty is 0
    /// (immature). A store read error is returned (the fold is not applied).
    pub fn observe(
        &mut self,
        site: SiteKey,
        utc_offset_min: i16,
        cal: CalKey,
        obs: &IntervalObservation,
    ) -> Result<FoldOutcome, BaselineStoreError> {
        let Some(key) = self.key(site, cal) else {
            return Ok(FoldOutcome::none(
                obs,
                Maturity::Immature { observed_s: 0.0 },
            ));
        };
        self.current = Some(key.site);
        self.engine(key, utc_offset_min, obs.t)?;
        if let Some(cap) = self.memory_cap {
            let growth = self.engines[&key].growth_of(obs);
            self.unload_to(cap.saturating_sub(growth), Some(key))?;
            if growth > 0 && self.memory_bytes() + growth > cap {
                // Learning is refused, novelty is not: score against what is loaded, so an
                // emitter appearing under memory pressure still raises novelty.
                self.refused_folds += 1;
                return Ok(FoldOutcome {
                    novelty: self.engines[&key].novelty_of(obs),
                    change_point: None,
                    accrued: false,
                    accrued_reference: false,
                });
            }
        }
        let e = self.engines.get_mut(&key).expect("loaded");
        let out = e.observe(obs);
        if obs.usable()
            && e.state
                .subjects
                .get(&obs.subject)
                .is_some_and(|s| gain_index(s, obs.gain, obs.level_class()).is_none())
        {
            self.gain_overflow_folds += 1;
        }
        Ok(out)
    }

    /// Folds under a gain state beyond the subject's [`MAX_GAIN_STATES`] kept slots (scored, not
    /// learned).
    pub fn gain_overflow_folds(&self) -> u64 {
        self.gain_overflow_folds
    }

    /// Engines loaded or created.
    pub fn engines(&self) -> impl Iterator<Item = &BaselineEngine> {
        self.engines.values()
    }

    /// Mutable engines (re-freeze).
    pub fn engines_mut(&mut self) -> impl Iterator<Item = &mut BaselineEngine> {
        self.engines.values_mut()
    }

    /// Phase 1 of [`Self::load_site_outside_lock`] (under the lock, no I/O): the store and the keys
    /// in memory. `None` without a store.
    pub fn load_plan(&self) -> Option<LoadPlan> {
        Some(LoadPlan {
            store: self.store.clone()?,
            loaded: self.engines.keys().copied().collect(),
            generation: self.generation,
        })
    }

    /// Phase 2 (no lock): reads `site`'s stored keys not in `plan.loaded` with `read`.
    pub fn read_site(
        plan: &LoadPlan,
        site: hk_model::ids::SiteId,
        mut read: impl FnMut(
            &BaselineStore,
            &BaselineKey,
        ) -> Result<Option<BaselineState>, BaselineStoreError>,
    ) -> Result<Vec<BaselineState>, BaselineStoreError> {
        let mut out = Vec::new();
        for (key, _, _) in plan.store.entries()? {
            if key.site == site
                && !plan.loaded.contains(&key)
                && let Some(state) = read(&plan.store, &key)?
            {
                out.push(state);
            }
        }
        Ok(out)
    }

    /// Phase 3 (under the lock, no I/O): inserts the read states whose keys are still absent (a
    /// fold may have loaded or created one meanwhile; memory is newer) and applies the memory cap.
    /// Returns `false` and inserts nothing when engines left memory since `plan` (a cap unload or
    /// quota eviction may have saved a newer file, or deleted the site, after the read): the
    /// caller re-plans.
    pub fn insert_loaded(
        &mut self,
        plan: &LoadPlan,
        states: Vec<BaselineState>,
        utc_offset_min: i16,
    ) -> Result<bool, BaselineStoreError> {
        if plan.generation != self.generation {
            return Ok(false);
        }
        for state in states {
            self.engines
                .entry(state.key)
                .or_insert_with(|| BaselineEngine::new(state, utc_offset_min, self.cfg));
        }
        match self.memory_cap {
            Some(cap) => self.unload_to(cap, None).map(|()| true),
            None => Ok(true),
        }
    }

    /// Loads `site`'s stored keys with the disk reads **outside** `baselines`' lock (T-132): the
    /// lock is taken briefly to plan and to insert, so concurrent folds are not blocked by I/O.
    pub fn load_site_outside_lock(
        baselines: &Mutex<Baselines>,
        site: hk_model::ids::SiteId,
        utc_offset_min: i16,
    ) -> Result<(), BaselineStoreError> {
        Self::load_site_outside_lock_with(baselines, site, utc_offset_min, BaselineStore::load)
    }

    /// [`Self::load_site_outside_lock`] with a custom reader (tests).
    pub fn load_site_outside_lock_with(
        baselines: &Mutex<Baselines>,
        site: hk_model::ids::SiteId,
        utc_offset_min: i16,
        mut read: impl FnMut(
            &BaselineStore,
            &BaselineKey,
        ) -> Result<Option<BaselineState>, BaselineStoreError>,
    ) -> Result<(), BaselineStoreError> {
        let lock = || baselines.lock().unwrap_or_else(PoisonError::into_inner);
        for _ in 0..LOAD_OUTSIDE_LOCK_ATTEMPTS {
            let Some(plan) = lock().load_plan() else {
                return Ok(());
            };
            let states = Self::read_site(&plan, site, &mut read)?;
            if lock().insert_loaded(&plan, states, utc_offset_min)? {
                return Ok(());
            }
        }
        // Engines kept leaving memory during the reads: read under the lock (never stale).
        lock().load_site(site, utc_offset_min)
    }

    /// The memory cap in force (`None` = unbounded).
    pub fn memory_cap(&self) -> Option<usize> {
        self.memory_cap
    }

    /// Loads every stored key of `site` not yet in memory (API listing). Reads the disk under the
    /// caller's lock; prefer [`Self::load_site_outside_lock`].
    pub fn load_site(
        &mut self,
        site: hk_model::ids::SiteId,
        utc_offset_min: i16,
    ) -> Result<(), BaselineStoreError> {
        let Some(store) = &self.store else {
            return Ok(());
        };
        let keys: Vec<_> = store
            .entries()?
            .into_iter()
            .filter(|e| e.0.site == site)
            .map(|e| (e.0, e.1))
            .collect();
        for (key, t) in keys {
            self.engine(key, utc_offset_min, t)?;
        }
        Ok(())
    }

    /// Saves dirty engines when the hour slot of `t` differs from the last save's (≤ 24
    /// writes/day per active key), or always when `force` (checkpoint), then enforces the store
    /// quota (evicting least recently visited sites, never the current one) and drops the evicted
    /// sites' engines so they are not written back. Returns files written.
    pub fn flush(&mut self, t: Timestamp, force: bool) -> Result<usize, BaselineStoreError> {
        let hour = t.as_unix_nanos().div_euclid(3_600_000_000_000);
        if !force && self.last_hour.is_none_or(|h| h == hour) {
            self.last_hour.get_or_insert(hour);
            return Ok(0);
        }
        self.last_hour = Some(hour);
        let Some(store) = &self.store else {
            return Ok(0);
        };
        let mut n = 0;
        for e in self.engines.values_mut().filter(|e| e.is_dirty()) {
            store.save(&e.state)?;
            e.mark_saved();
            n += 1;
        }
        let evicted = store.enforce_quota(self.current)?;
        if !evicted.is_empty() {
            self.engines.retain(|k, _| !evicted.contains(&k.site));
            self.generation += 1;
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use hk_model::attention::baseline::SlotStats;
    use hk_model::attention::occupancy::{ChannelKey, effective_samples};
    use hk_model::ids::{CalibrationStateId, SiteId};

    use super::*;

    const H: f64 = 3600.0;
    /// Monday 00:00 UTC (1970-01-05) + 52 weeks: slot 0 at t = 0 h.
    const T0_NS: i64 = (4 + 364) * 86_400 * 1_000_000_000;

    fn at(hours: f64) -> Timestamp {
        Timestamp::from_unix_nanos(T0_NS + (hours * 3.6e12) as i64)
    }

    /// Deterministic splitmix64.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> f64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            ((z ^ (z >> 31)) >> 11) as f64 / (1u64 << 53) as f64
        }
        fn normal(&mut self) -> f64 {
            let (u, v) = (self.next().max(1e-12), self.next());
            (-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * v).cos()
        }
    }

    fn channel(i: i64) -> BaselineSubject {
        BaselineSubject::Channel {
            key: ChannelKey {
                scheme: 1,
                lo_cell: 16_000 + 8 * i,
                hi_cell: 16_004 + 8 * i,
            },
        }
    }

    /// A two-state Markov channel observed by `n` revisits every `interval_s`.
    struct MarkovChannel {
        on: bool,
        tau_on_s: f64,
        tau_off_s: f64,
    }

    impl MarkovChannel {
        fn with_fco(p: f64, mean_cycle_s: f64) -> Self {
            let p = p.clamp(1e-4, 1.0 - 1e-4);
            Self {
                on: false,
                tau_on_s: mean_cycle_s * p,
                tau_off_s: mean_cycle_s * (1.0 - p),
            }
        }

        fn visit(&mut self, dt: f64, rng: &mut Rng) -> bool {
            let tau = if self.on {
                self.tau_on_s
            } else {
                self.tau_off_s
            };
            if rng.next() < 1.0 - (-dt / tau).exp() {
                self.on = !self.on;
            }
            self.on
        }

        fn corr_s(&self) -> f64 {
            self.tau_on_s * self.tau_off_s / (self.tau_on_s + self.tau_off_s)
        }
    }

    /// One 15-min interval: `n` revisits of a channel whose target FCO is `p` and floor+signal
    /// level follows occupancy. Returns the fold.
    fn interval(
        subject: BaselineSubject,
        hours: f64,
        p: f64,
        snr_db: f64,
        rng: &mut Rng,
    ) -> IntervalObservation {
        let (n, dt) = (12_u64, 75.0);
        let mut ch = MarkovChannel::with_fco(p, 600.0);
        ch.on = rng.next() < p;
        let occupied = (0..n).filter(|_| ch.visit(dt, rng)).count() as f64;
        let (n_eff, _) = effective_samples(n, Some(dt), Some(ch.corr_s()));
        let fco = occupied / n as f64;
        let level = -100.0 + fco * snr_db + 0.5 * rng.normal();
        IntervalObservation {
            subject,
            t: at(hours),
            gain: 0,
            level_db: Some(level),
            max_db: Some(level + 3.0),
            occupied_weight_s: fco * 900.0,
            weight_s: 900.0,
            observed_s: 900.0,
            n_eff,
            suspect_fraction: 0.0,
            provenance_explained: false,
        }
    }

    /// Hour-of-week pattern of channel `c` (hidden generator parameters).
    fn pattern(c: i64, hours: f64) -> f64 {
        let hod = hours.rem_euclid(24.0);
        let base: f64 = [0.02, 0.1, 0.3, 0.5, 0.8][(c % 5) as usize];
        let day = if (8.0..20.0).contains(&hod) { 1.6 } else { 0.5 };
        (base * day).min(0.97)
    }

    fn site_key() -> BaselineKey {
        BaselineKey {
            site: SiteId::new(),
            cal: CalKey::Uncalibrated,
            scheme: 1,
            cell_factor: 16,
        }
    }

    fn engine() -> BaselineEngine {
        BaselineEngine::new(
            BaselineState::new(site_key(), at(0.0)),
            0,
            BaselineConfig::default(),
        )
    }

    /// Blind 48 h scene: 20 channels with hour-of-week patterns; the generator (hidden from the
    /// engine) switches a new emitter on one channel at hour 30. Asserts detection latency and a
    /// bounded false-alarm rate on unchanged channels, and prints both.
    #[test]
    fn baseline_48h_scene_flags_the_hour_30_emitter_with_bounded_false_alarms() {
        // Hidden truth: only the generator and the assertions read these.
        const INJECT_H: f64 = 30.0;
        const NOVEL: i64 = 13;
        const THRESHOLD: f64 = 0.25;
        let mut rng = Rng(0x7119);
        let mut e = engine();
        let mut first_hit: Option<f64> = None;
        let (mut fa, mut trials) = (0usize, 0usize);
        let mut max_quiet_novelty: f64 = 0.0;
        let mut cps_unchanged = 0;
        for q in 0..(48 * 4) {
            let hours = f64::from(q) / 4.0;
            for c in 0..20 {
                let novel_on = c == NOVEL && hours >= INJECT_H;
                let (p, snr) = if c == NOVEL {
                    if novel_on { (0.4, 15.0) } else { (0.0, 0.0) }
                } else {
                    (pattern(c, hours), 10.0)
                };
                let out = e.observe(&interval(channel(c), hours, p, snr, &mut rng));
                out.novelty.validate().unwrap();
                if hours < 24.0 {
                    assert_eq!(out.novelty.novelty, 0.0, "immature before 24 h");
                    continue;
                }
                if c == NOVEL {
                    if novel_on && first_hit.is_none() && out.novelty.novelty >= THRESHOLD {
                        first_hit = Some(hours);
                    }
                    if !novel_on {
                        max_quiet_novelty = max_quiet_novelty.max(out.novelty.novelty);
                    }
                } else {
                    trials += 1;
                    fa += usize::from(out.novelty.novelty >= THRESHOLD);
                    cps_unchanged += usize::from(out.change_point.is_some());
                }
            }
        }
        let latency = ((first_hit.expect("the emitter is flagged") - INJECT_H) * 4.0) as u32 + 1;
        let fa_rate = fa as f64 / trials as f64;
        println!(
            "T-119 48h scene: detection latency {latency} interval(s) (15 min), false alarms \
             {fa}/{trials} = {:.4} at novelty ≥ {THRESHOLD}, change points on unchanged channels \
             {cps_unchanged}",
            fa_rate
        );
        assert!(latency <= 4, "latency {latency} intervals");
        assert!(fa_rate <= 0.01, "false-alarm rate {fa_rate}");
        assert_eq!(max_quiet_novelty, 0.0);
        // The change point runs on (adaptive − reference) with a 14-day adaptive copy, so within
        // 18 h of onset it need not have crossed; novelty already flagged the emitter.
        assert_eq!(cps_unchanged, 0);
    }

    #[test]
    fn baseline_persistent_interferer_is_a_change_point_not_absorbed() {
        let mut rng = Rng(7);
        let mut e = engine();
        let ch = channel(1);
        let mut raised = None;
        let mut last_day: Vec<f64> = Vec::new();
        let (mut early_novel, mut early) = (0, 0);
        // 3 clean days, then 21 days with an interferer (FCO 0.2 → 0.6, +6 dB).
        for q in 0..(24 * 4 * 24) {
            let hours = f64::from(q) / 4.0;
            let bad = hours >= 72.0;
            let (p, snr) = if bad { (0.6, 16.0) } else { (0.2, 10.0) };
            let out = e.observe(&interval(ch, hours, p, snr, &mut rng));
            if (72.0..78.0).contains(&hours) {
                early += 1;
                early_novel += usize::from(out.novelty.novelty > 0.0);
            }
            if let Some(cp) = out.change_point {
                assert!(bad, "no change point before the interferer");
                raised.get_or_insert((hours, cp));
            }
            if hours >= 23.0 * 24.0 {
                last_day.push(out.novelty.novelty);
            }
        }
        let (h, cp) = raised.expect("change point raised");
        // The CUSUM runs on (adaptive - reference), and the adaptive copy forgets over 14 days, so
        // the change point is deliberately slow; per-interval novelty flags the onset at once.
        println!(
            "T-119 interferer: change point {:.2} h after onset; novel in {early_novel}/{early} \
             intervals of the first 6 h",
            h - 72.0
        );
        assert!(h - 72.0 <= 72.0, "raised {:.2} h after onset", h - 72.0);
        assert!(
            early_novel * 2 >= early,
            "onset novel in {early_novel}/{early} intervals"
        );
        assert_eq!(cp.direction, 1);
        let sub = &e.state.subjects[&ch];
        let slot = e.slot(at(0.0));
        let (_, ad) = pools(sub, slot, Some(0), BaselineCopy::Adaptive);
        let (_, rf) = pools(sub, slot, Some(0), BaselineCopy::Reference);
        assert!(ad[3].fco().unwrap() > 0.5, "the adaptive copy absorbed it");
        assert!(rf[3].fco().unwrap() < 0.3, "the reference did not");
        let flagged = last_day.iter().filter(|n| **n > 0.0).count();
        println!(
            "T-119 interferer: last day novel in {flagged}/{} intervals",
            last_day.len()
        );
        assert!(
            flagged * 2 >= last_day.len(),
            "still flagged after 21 days: {flagged}/{}",
            last_day.len()
        );
        // The user accepts the new normal.
        assert_eq!(e.refreeze(at(600.0), |s| *s == ch), 1);
        let out = e.observe(&interval(ch, 600.25, 0.6, 16.0, &mut rng));
        assert_eq!(out.novelty.novelty, 0.0);
        assert!(e.state.subjects[&ch].change_point.is_none());
    }

    #[test]
    fn baseline_cal_step_starts_a_new_immature_key_and_is_not_novelty() {
        let mut rng = Rng(3);
        let site = SiteKey::Site(SiteId::new());
        let mut b = Baselines::new(BaselineConfig::default(), 1, 16, None);
        let ch = channel(0);
        for q in 0..(30 * 4) {
            let o = interval(ch, f64::from(q) / 4.0, 0.1, 10.0, &mut rng);
            b.observe(site, 0, CalKey::Uncalibrated, &o).unwrap();
        }
        // New calibration: levels shift by +20 dB (dBFS → dBm-ish offset) and occupancy looks
        // different under the new threshold.
        let cal = CalKey::Calibrated(CalibrationStateId::new());
        for q in 0..8 {
            let mut o = interval(ch, 30.0 + f64::from(q) / 4.0, 0.5, 10.0, &mut rng);
            o.level_db = o.level_db.map(|l| l + 20.0);
            let out = b.observe(site, 0, cal, &o).unwrap();
            assert_eq!(out.novelty.novelty, 0.0);
            assert!(!out.novelty.maturity.is_mature());
            assert!(out.change_point.is_none());
        }
        assert_eq!(b.engines().count(), 2, "one key per calibration");
        // The same step under the old key would have been novel.
        let mut o = interval(ch, 32.0, 0.5, 10.0, &mut rng);
        o.level_db = o.level_db.map(|l| l + 20.0);
        let old = b.key(site, CalKey::Uncalibrated).unwrap();
        let e = b.engines().find(|e| e.state.key == old).unwrap();
        assert!(e.novelty_of(&o).novelty > 0.9);
    }

    #[test]
    fn baseline_moving_or_unassigned_site_builds_nothing() {
        let dir = std::env::temp_dir().join(format!("hk-t119-mobile-{}", SiteId::new()));
        let store = BaselineStore::open(&dir).unwrap();
        let mut b = Baselines::new(BaselineConfig::default(), 1, 16, Some(store.clone()));
        let mut rng = Rng(9);
        for q in 0..(30 * 4) {
            let o = interval(channel(2), f64::from(q) / 4.0, 0.9, 20.0, &mut rng);
            for site in [SiteKey::Mobile, SiteKey::Unassigned] {
                let out = b.observe(site, 0, CalKey::Uncalibrated, &o).unwrap();
                assert!(!out.accrued && out.novelty.novelty == 0.0);
            }
        }
        assert_eq!(b.flush(at(100.0), true).unwrap(), 0);
        assert_eq!(b.engines().count(), 0);
        assert!(store.entries().unwrap().is_empty());
        // A real site does write at slot close, and reloads.
        let site = SiteId::new();
        let o = interval(channel(2), 100.0, 0.9, 20.0, &mut rng);
        b.observe(SiteKey::Site(site), 0, CalKey::Uncalibrated, &o)
            .unwrap();
        assert_eq!(b.flush(at(100.5), false).unwrap(), 0, "same hour");
        assert_eq!(b.flush(at(101.0), false).unwrap(), 1, "hour slot closed");
        let mut again = Baselines::new(BaselineConfig::default(), 1, 16, Some(store));
        again.load_site(site, 0).unwrap();
        assert_eq!(again.engines().next().unwrap().state.subjects.len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn baseline_maturity_pooling_discloses_the_resolution() {
        let fold = |e: &mut BaselineEngine, hours: f64| {
            e.observe(&IntervalObservation {
                subject: channel(0),
                t: at(hours),
                gain: 0,
                level_db: Some(-100.0),
                max_db: None,
                occupied_weight_s: 0.0,
                weight_s: H,
                observed_s: H,
                n_eff: 40.0,
                suspect_fraction: 0.0,
                provenance_explained: false,
            })
        };
        let res = |e: &BaselineEngine, hours: f64| {
            maturity(&e.state.subjects[&channel(0)], e.slot(at(hours)))
        };
        let mut e = engine();
        for h in 0..23 {
            fold(&mut e, f64::from(h));
        }
        assert_eq!(
            res(&e, 0.0),
            Maturity::Immature {
                observed_s: 23.0 * H
            }
        );
        // One parked day: all hours.
        fold(&mut e, 23.0);
        assert_eq!(
            res(&e, 5.0),
            Maturity::Mature {
                resolution: BaselineResolution::AllHours
            }
        );
        // Four parked days: each six-hour day part holds 24 h.
        for h in 24..96 {
            fold(&mut e, f64::from(h));
        }
        assert_eq!(
            res(&e, 5.0),
            Maturity::Mature {
                resolution: BaselineResolution::DayPart
            }
        );
        // 24 days: each hour of day holds 24 h.
        for h in 96..(24 * 24) {
            fold(&mut e, f64::from(h));
        }
        assert_eq!(
            res(&e, 5.0),
            Maturity::Mature {
                resolution: BaselineResolution::HourOfDay
            }
        );
        // 24 weeks visiting Monday 03:00 only: that hour-of-week slot is mature on its own.
        let mut w = engine();
        for week in 0..24 {
            fold(&mut w, f64::from(week) * 168.0 + 3.0);
        }
        assert_eq!(
            res(&w, 3.0),
            Maturity::Mature {
                resolution: BaselineResolution::HourOfWeek
            }
        );
        // Monday 04:00 shares the 00-06 day part with the 03:00 data.
        assert_eq!(
            res(&w, 4.0),
            Maturity::Mature {
                resolution: BaselineResolution::DayPart
            }
        );
        assert_eq!(
            res(&w, 12.0),
            Maturity::Mature {
                resolution: BaselineResolution::AllHours
            }
        );
        let n = w.novelty_of(&IntervalObservation {
            level_db: Some(-80.0),
            ..IntervalObservation {
                subject: channel(0),
                t: at(24.0 * 168.0 + 3.0),
                gain: 0,
                level_db: None,
                max_db: None,
                occupied_weight_s: 0.0,
                weight_s: H,
                observed_s: H,
                n_eff: 40.0,
                suspect_fraction: 0.0,
                provenance_explained: false,
            }
        });
        assert_eq!(
            n.maturity,
            Maturity::Mature {
                resolution: BaselineResolution::HourOfWeek
            }
        );
        assert_eq!(n.novelty, 1.0);
        // Provenance-explained: validated zero.
        let mut ex = engine();
        for h in 0..30 {
            fold(&mut ex, f64::from(h));
        }
        let mut o = IntervalObservation {
            subject: channel(0),
            t: at(30.0),
            gain: 0,
            level_db: Some(-60.0),
            max_db: None,
            occupied_weight_s: H,
            weight_s: H,
            observed_s: H,
            n_eff: 40.0,
            suspect_fraction: 0.0,
            provenance_explained: true,
        };
        let out = ex.observe(&o);
        assert!(out.novelty.provenance_explained && out.novelty.novelty == 0.0);
        assert!(
            !out.accrued_reference,
            "explained folds never enter the reference"
        );
        o.provenance_explained = false;
        o.gain = 9;
        assert_eq!(
            ex.novelty_of(&o).level_z,
            None,
            "a new gain state has no level history"
        );
        let _ = SlotStats::EMPTY;
    }

    #[test]
    fn baseline_occupancy_stat_adapter_maps_channel_rows() {
        use hk_model::attention::occupancy::{ThresholdSpec, TimingRegime};
        use hk_model::region::TimeRange;
        let key = ChannelKey {
            scheme: 1,
            lo_cell: 10,
            hi_cell: 12,
        };
        let json = serde_json::json!({
            "schema": 1,
            "site": {"kind": "mobile"},
            "subject": {"kind": "channel", "key": key},
            "interval": TimeRange::new(at(0.0), at(0.25)),
            "fco": 0.5,
            "n_revisits": 10, "n_occupied": 4, "n_suspect": 2, "n_revisits_all": 12,
            "observed_s": 1.2, "revisit_mean_s": 75.0,
            "timing": serde_json::to_value(TimingRegime::Statistical).unwrap(),
            "threshold": serde_json::to_value(ThresholdSpec::default()).unwrap(),
            "threshold_db": -95.0, "guard_clamped": false, "rbw_hz": 6250.0,
            "unit": "dbfs", "revisit_biased": false,
        });
        let stat: OccupancyStat = match serde_json::from_value(json) {
            Ok(s) => s,
            // The wire shape is T-118's; this test only pins the mapping, so skip if it moved.
            Err(e) => {
                println!("OccupancyStat shape differs ({e}); adapter mapping unchecked");
                return;
            }
        };
        let (site, cal, o) = from_occupancy_stat(&stat, 3).unwrap();
        assert_eq!((site, cal), (SiteKey::Mobile, CalKey::Uncalibrated));
        assert_eq!(o.subject, BaselineSubject::Channel { key });
        assert_eq!(o.observed_s, 900.0);
        assert!((o.weight_s - 720.0).abs() < 1e-9 && (o.fco().unwrap() - 0.5).abs() < 1e-12);
        assert!((o.suspect_fraction - 0.2).abs() < 1e-12);
        assert_eq!(
            o.level_db, None,
            "occupancy only: the threshold is not a level"
        );
    }

    /// [`occupancy_row`] with T-118's floor and levels: floor `floor_db`, idle visits
    /// `idle_excess` dB above it, occupied visits `occ_excess` dB above it (p90 +2 dB).
    fn leveled_row(
        hours: f64,
        fco: f64,
        floor_db: f64,
        idle_excess: f64,
        occ_excess: f64,
    ) -> OccupancyStat {
        let mut r = occupancy_row(hours, fco, floor_db + 6.0).expect("OccupancyStat shape");
        r.floor_db = Some(floor_db);
        r.floor_suspect = Some(false);
        r.level_idle_db = Some(floor_db + idle_excess);
        if r.n_occupied > 0 {
            r.level_occupied_p50_db = Some(floor_db + occ_excess);
            r.level_occupied_p90_db = Some(floor_db + occ_excess + 2.0);
        }
        r
    }

    /// T-128: an emitter appearing on a quiet channel raises level novelty through the adapter
    /// (its level above the floor jumps by its SNR).
    #[test]
    fn baseline_adapter_emitter_appearing_raises_level_novelty() {
        let mut e = engine();
        let (mut before, mut after_z) = (0.0_f64, f64::NEG_INFINITY);
        // 3 days quiet (idle 2 dB above a −100 dB floor), then an emitter 25 dB above it.
        for q in 0..(24 * 4 * 4) {
            let hours = f64::from(q) / 4.0;
            let row = if hours >= 72.0 {
                leveled_row(hours, 0.8, -100.0, 2.0, 25.0)
            } else {
                leveled_row(hours, 0.0, -100.0, 2.0 + 0.3 * ((q % 5) as f64 - 2.0), 25.0)
            };
            let (_, _, o) = from_occupancy_stat(&row, 0).unwrap();
            assert!(o.level_db.is_some(), "a level from T-118 fields");
            let out = e.observe(&o);
            if hours < 72.0 {
                before = before.max(out.novelty.novelty);
            } else if hours < 73.0 {
                after_z = after_z.max(out.novelty.level_z.unwrap_or(f64::NEG_INFINITY));
            }
        }
        println!("T-128 emitter appears: novelty before {before}, level z after {after_z}");
        assert!(before < 0.1, "quiet channel not novel: {before}");
        assert!(after_z >= 10.0, "level novelty saturates: z={after_z}");
    }

    /// A T-118 channel row at `hours` (15 min) with FCO `fco` and applied threshold `threshold_db`.
    fn occupancy_row(hours: f64, fco: f64, threshold_db: f64) -> Option<OccupancyStat> {
        use hk_model::attention::occupancy::{ThresholdSpec, TimingRegime};
        use hk_model::region::TimeRange;
        let key = ChannelKey {
            scheme: 1,
            lo_cell: 10,
            hi_cell: 12,
        };
        let json = serde_json::json!({
            "schema": 1,
            "site": {"kind": "mobile"},
            "subject": {"kind": "channel", "key": key},
            "interval": TimeRange::new(at(hours), at(hours + 0.25)),
            "fco": fco,
            "n_revisits": 12, "n_occupied": (fco * 12.0).round() as u64, "n_suspect": 0,
            "n_revisits_all": 12, "observed_s": 1.2, "revisit_mean_s": 75.0,
            "timing": serde_json::to_value(TimingRegime::Statistical).unwrap(),
            "threshold": serde_json::to_value(ThresholdSpec::default()).unwrap(),
            "threshold_db": threshold_db, "guard_clamped": false, "rbw_hz": 6250.0,
            "unit": "dbfs", "revisit_biased": false,
        });
        serde_json::from_value(json).ok()
    }

    /// A noise-floor rise moves the applied threshold, not the emitter: through the adapter it
    /// must give neither level novelty nor a level change point.
    #[test]
    fn baseline_adapter_floor_rise_is_not_level_novelty() {
        assert!(
            occupancy_row(0.0, 0.3, -95.0).is_some(),
            "OccupancyStat shape"
        );
        let mut e = engine();
        let (mut level_cps, mut max_after) = (0, 0.0_f64);
        // 3 days at a −101 dB floor, then 3 days with the floor (threshold, idle and occupied
        // levels with it) 10 dB higher. T-128: the adapter now folds real levels (above the floor).
        for q in 0..(24 * 4 * 6) {
            let hours = f64::from(q) / 4.0;
            let floor = if hours >= 72.0 { -91.0 } else { -101.0 };
            let wobble = 0.3 * ((q % 5) as f64 - 2.0);
            let row = leveled_row(hours, 0.3, floor, 2.0 + wobble, 20.0 + wobble);
            let (_, _, o) = from_occupancy_stat(&row, 0).unwrap();
            let out = e.observe(&o);
            assert!(o.level_db.is_some(), "a real level at {hours} h");
            assert!(
                out.novelty.level_z.is_none_or(|z| z < 3.0),
                "no level novelty at {hours} h: {:?}",
                out.novelty.level_z
            );
            level_cps += usize::from(
                out.change_point
                    .is_some_and(|c| c.statistic == ChangeStatistic::Level),
            );
            if hours >= 72.0 {
                max_after = max_after.max(out.novelty.novelty);
            }
        }
        println!(
            "T-119 adapter floor +10 dB: max novelty after {max_after}, level change points \
             {level_cps}"
        );
        assert_eq!(level_cps, 0);
        assert_eq!(max_after, 0.0);
    }

    /// An occupancy-only 15-min fold with `n_eff` 50 and FCO drawn around `p`.
    fn occ_fold(
        subject: BaselineSubject,
        hours: f64,
        p: f64,
        rng: &mut Rng,
    ) -> IntervalObservation {
        let n = 50;
        let fco = (0..n).filter(|_| rng.next() < p).count() as f64 / f64::from(n);
        IntervalObservation {
            subject,
            t: at(hours),
            gain: 0,
            level_db: None,
            max_db: None,
            occupied_weight_s: fco * 900.0,
            weight_s: 900.0,
            observed_s: 900.0,
            n_eff: f64::from(n),
            suspect_fraction: 0.0,
            provenance_explained: false,
        }
    }

    /// A channel quiet all day except one busy hour (FCO 0.8). After day 1 that hour is novel
    /// against the coarse (all-hours) pool; it must still accrue into its own slot while its
    /// hour-of-day pool is immature, so it matures instead of staying novel every day. A genuinely
    /// new emitter in a quiet hour is still novel.
    #[test]
    fn baseline_sharply_patterned_channel_matures_instead_of_deadlocking() {
        const BUSY_HOD: f64 = 14.0;
        let mut rng = Rng(0x119);
        let mut e = engine();
        let ch = channel(0);
        let days = 9;
        let mut busy_max = vec![0.0_f64; days];
        let mut busy_ref = vec![0usize; days];
        for q in 0..(24 * 4 * days) {
            let hours = f64::from(q as u32) / 4.0;
            let hod = hours.rem_euclid(24.0);
            let busy = (BUSY_HOD..BUSY_HOD + 1.0).contains(&hod);
            let out = e.observe(&occ_fold(ch, hours, if busy { 0.8 } else { 0.0 }, &mut rng));
            if busy {
                let d = (hours / 24.0) as usize;
                busy_max[d] = busy_max[d].max(out.novelty.novelty);
                busy_ref[d] += usize::from(out.accrued_reference);
            }
        }
        println!(
            "T-119 busy hour: max novelty per day {busy_max:.3?}, reference folds {busy_ref:?}"
        );
        assert!(
            busy_max[1] > 0.0,
            "day 2 is novel against the all-hours pool"
        );
        assert!(
            busy_ref[1..].iter().all(|n| *n > 0),
            "the busy slot keeps accruing"
        );
        assert!(
            busy_max[6..].iter().all(|n| *n < 0.05),
            "the busy hour matured: {busy_max:?}"
        );
        // A genuinely new emitter in a quiet hour (03:00) on day 10.
        let h = 24.0 * days as f64 + 3.0;
        let out = e.observe(&occ_fold(ch, h, 0.8, &mut rng));
        println!(
            "T-119 new emitter at 03:00: novelty {:.3}",
            out.novelty.novelty
        );
        assert!(
            out.novelty.novelty >= 0.7,
            "new emitter novelty {}",
            out.novelty.novelty
        );
    }

    /// A slow creep (FCO +0.003/day) never looks novel per fold; reference learning must end once
    /// the hour-of-day pool is mature (~24 parked days), not when each hour-of-week slot holds
    /// 24 h (~24 weeks), so the creep stops entering the reference.
    #[test]
    fn baseline_slow_creep_stops_entering_the_reference_after_maturity() {
        let mut e = engine();
        let ch = channel(3);
        let mut last_ref_h = 0.0;
        for q in 0..(24 * 4 * 40) {
            let hours = f64::from(q) / 4.0;
            let p = 0.2 + 0.003 * hours / 24.0;
            let o = IntervalObservation {
                subject: ch,
                t: at(hours),
                gain: 0,
                level_db: None,
                max_db: None,
                occupied_weight_s: p * 900.0,
                weight_s: 900.0,
                observed_s: 900.0,
                n_eff: 50.0,
                suspect_fraction: 0.0,
                provenance_explained: false,
            };
            let out = e.observe(&o);
            if out.accrued_reference {
                last_ref_h = hours;
            }
        }
        let sub = &e.state.subjects[&ch];
        let (_, rf) = pools(sub, e.slot(at(0.0)), Some(0), BaselineCopy::Reference);
        let (_, ad) = pools(sub, e.slot(at(0.0)), Some(0), BaselineCopy::Adaptive);
        let (rf_fco, ad_fco) = (rf[1].fco().unwrap(), ad[1].fco().unwrap());
        println!(
            "T-119 slow creep: last reference fold at day {:.2}, reference FCO {rf_fco:.3}, \
             adaptive {ad_fco:.3}",
            last_ref_h / 24.0
        );
        // T-132: the sequential learning test notices the creep (+0.03 FCO against its own
        // hour's reference by then) a little before hour-of-day maturity.
        assert!(last_ref_h > 20.0 * 24.0, "still learning for weeks");
        assert!(
            last_ref_h < 24.0 * 24.0,
            "learning ended at hour-of-day maturity"
        );
        assert!(rf_fco < 0.25 && ad_fco - rf_fco > 0.03);
    }

    /// Flush enforces the store quota and drops evicted sites' engines so they are not written
    /// back.
    /// The occupied-level series of `sub` under gain `gain`.
    fn occupied_series(sub: &SubjectBaseline, gain: u32) -> usize {
        gain_index(sub, gain, LevelClass::Occupied).expect("occupied series")
    }

    /// T-132 item 5: a front-end gain change starts a separate (immature) level pool instead of
    /// reading as level novelty; the same step under one gain key is novel (control).
    #[test]
    fn baseline_gain_change_starts_a_separate_level_pool_not_novelty() {
        let run = |gain_after: u32| {
            let mut rng = Rng(0x1325);
            let mut e = engine();
            let (mut max_lz, mut subject) = (f64::NEG_INFINITY, None);
            for q in 0..(24 * 4 * 4) {
                let hours = f64::from(q) / 4.0;
                let after = hours >= 72.0;
                let (gain, occ) = if after {
                    (gain_after, 18.0)
                } else {
                    (0x1234, 12.0)
                };
                let row = leveled_row(hours, 0.5, -100.0, 2.0, occ + 0.3 * rng.normal());
                let (_, _, o) = from_occupancy_stat(&row, gain).unwrap();
                subject = Some(o.subject);
                let out = e.observe(&o);
                if (72.0..78.0).contains(&hours) {
                    max_lz = max_lz.max(out.novelty.level_z.unwrap_or(f64::NEG_INFINITY));
                }
            }
            let sub = e.state.subjects[&subject.unwrap()].clone();
            (max_lz, sub)
        };
        let (lz_new_gain, sub) = run(0xBEEF);
        let (lz_same_gain, _) = run(0x1234);
        println!(
            "T-132 gain step (+6 dB excess): max level z after, new gain key {lz_new_gain}, \
             same key {lz_same_gain}"
        );
        assert!(
            lz_new_gain < 3.0,
            "gain change is not level novelty: {lz_new_gain}"
        );
        assert!(
            lz_same_gain >= 3.0,
            "control: same key is novel: {lz_same_gain}"
        );
        let occ = occupied_series(&sub, 0xBEEF);
        assert_ne!(occ, occupied_series(&sub, 0x1234), "separate level pools");
    }

    /// T-132 item 6: on a quiet channel one occupied visit in an interval is occupancy evidence,
    /// not a level (the old adapter read its +20 dB as level novelty); a sustained real level rise
    /// on a busy channel is still level novelty.
    #[test]
    fn baseline_low_fco_single_occupied_visit_is_not_level_novelty_but_a_rise_is() {
        let mut rng = Rng(0x1326);
        let mut e = engine();
        let mut max_lz_single = f64::NEG_INFINITY;
        for q in 0..(24 * 4 * 6) {
            let hours = f64::from(q) / 4.0;
            // Every 8th interval one of 12 visits is occupied, +20 dB above the floor.
            let fco = if q % 8 == 0 { 1.0 / 12.0 } else { 0.0 };
            let row = leveled_row(hours, fco, -100.0, 2.0 + 0.3 * rng.normal(), 20.0);
            let (_, _, o) = from_occupancy_stat(&row, 0).unwrap();
            let out = e.observe(&o);
            if hours >= 24.0 && q % 8 == 0 {
                assert_eq!(o.level_db, None, "one occupied visit folds occupancy only");
                max_lz_single = max_lz_single.max(out.novelty.level_z.unwrap_or(f64::NEG_INFINITY));
            }
        }
        // A busy channel (6 of 12 occupied) at +12 dB for 3 days, then +18 dB.
        let mut e = engine();
        let (mut first_rise, mut max_before) = (None, f64::NEG_INFINITY);
        for q in 0..(24 * 4 * 4) {
            let hours = f64::from(q) / 4.0;
            let occ = if hours >= 72.0 { 18.0 } else { 12.0 } + 0.5 * rng.normal();
            let row = leveled_row(hours, 0.5, -100.0, 2.0, occ);
            let (_, _, o) = from_occupancy_stat(&row, 0).unwrap();
            let out = e.observe(&o);
            let lz = out.novelty.level_z.unwrap_or(f64::NEG_INFINITY);
            if (24.0..72.0).contains(&hours) {
                max_before = max_before.max(lz);
            } else if hours >= 72.0 && lz >= 3.0 && first_rise.is_none() {
                first_rise = Some(hours - 72.0);
            }
        }
        println!(
            "T-132 bimodality: single occupied visit max level z {max_lz_single}; busy channel \
             max z before {max_before:.2}, +6 dB rise flagged after {first_rise:?} h"
        );
        assert!(max_lz_single < 3.0);
        assert!(max_before < 3.0);
        assert_eq!(
            first_rise,
            Some(0.0),
            "sustained level rise is novel at once"
        );
    }

    /// T-132 item 3: a change confined to one hour of day latches that hour only; the other hours
    /// keep learning, including while a subject-wide change point is open.
    #[test]
    fn baseline_change_latches_its_hour_of_day_only() {
        let mut rng = Rng(0x1323);
        let mut e = engine();
        let ch = channel(2);
        let bad_bit = hour_bit(e.slot(at(72.0 + 14.0)));
        let (mut other_accrued, mut bad_accrued) = (0, 0);
        for q in 0..(24 * 4 * 7) {
            let hours = f64::from(q) / 4.0;
            let bad = hours >= 72.0 && (hours % 24.0).floor() == 14.0;
            let (p, snr) = if bad { (0.8, 18.0) } else { (0.2, 10.0) };
            let out = e.observe(&interval(ch, hours, p, snr, &mut rng));
            if hours >= 96.0 {
                if bad {
                    bad_accrued += usize::from(out.accrued_reference);
                } else {
                    other_accrued += usize::from(out.accrued_reference);
                }
            }
        }
        let latched = e.state.subjects[&ch].latched_hours;
        println!(
            "T-132 hour-14 interferer: latched hours {latched:#x} (hour 14 = {bad_bit:#x}); \
             reference folds after day 4: other hours {other_accrued}, hour 14 {bad_accrued}"
        );
        assert_eq!(latched, bad_bit);
        assert_eq!(bad_accrued, 0);
        assert!(
            other_accrued > 50,
            "other hours keep learning: {other_accrued}"
        );
        // An open subject-wide change point (latched at hour 14) no longer stops the others.
        e.state.subjects.get_mut(&ch).unwrap().change_point = Some(ChangePoint {
            t: at(86.0),
            statistic: ChangeStatistic::Occupancy,
            direction: 1,
            cusum: 9.0,
        });
        let accrued = (0..16)
            .filter(|i| {
                let hours = 7.0 * 24.0 + 2.0 + f64::from(*i) / 4.0;
                e.observe(&interval(ch, hours, 0.2, 10.0, &mut rng))
                    .accrued_reference
            })
            .count();
        assert!(accrued > 0, "learning continues outside the latched hour");
    }

    /// T-132 item 4: a weak persistent interferer (+1.5 dB on a channel whose occupied level
    /// wanders by 1 dB, so z ≈ 1.5 < 3 on every fold: never novel) with real channel levels
    /// through the adapter. The sequential learning test stops it entering the reference within
    /// hours, so the frozen reference barely moves while the adaptive copy follows.
    #[test]
    fn baseline_weak_persistent_interferer_is_not_absorbed() {
        const ONSET_H: f64 = 72.0;
        let mut rng = Rng(0x1324);
        let mut e = engine();
        let (mut subject, mut ref_before) = (None, None);
        let (mut stop_h, mut cp_h, mut novel, mut folds, mut absorbed) = (None, None, 0, 0, 0);
        let mean_of = |e: &BaselineEngine, subject, copy| {
            let sub = &e.state.subjects[&subject];
            let gi = occupied_series(sub, 0);
            pools(sub, e.slot(at(0.0)), Some(gi), copy).0[3]
                .mean_db()
                .unwrap()
        };
        for q in 0..(24 * 4 * 24) {
            let hours = f64::from(q) / 4.0;
            let weak = hours >= ONSET_H;
            let occ = 12.0 + if weak { 1.5 } else { 0.0 } + rng.normal();
            let row = leveled_row(hours, 0.5, -100.0, 2.0, occ);
            let (_, _, o) = from_occupancy_stat(&row, 0).unwrap();
            if weak && ref_before.is_none() {
                ref_before = Some(mean_of(&e, o.subject, BaselineCopy::Reference));
            }
            subject = Some(o.subject);
            let out = e.observe(&o);
            if weak {
                folds += 1;
                novel += usize::from(out.novelty.novelty > 0.0);
                absorbed += usize::from(out.accrued_reference);
                let sub = &e.state.subjects[&o.subject];
                if stop_h.is_none() && cusum_max(&sub.seq) >= 4.0 {
                    stop_h = Some(hours - ONSET_H);
                }
                if cp_h.is_none() && out.change_point.is_some() {
                    cp_h = Some(hours - ONSET_H);
                }
            }
        }
        let subject = subject.unwrap();
        let drift = mean_of(&e, subject, BaselineCopy::Reference) - ref_before.unwrap();
        let adaptive = mean_of(&e, subject, BaselineCopy::Adaptive) - ref_before.unwrap();
        println!(
            "T-132 weak interferer +1.5 dB (σ 1 dB): novel folds {novel}/{folds}; learning stopped \
             {stop_h:?} h after onset; {absorbed} folds absorbed in 21 days; reference drift \
             {drift:.3} dB, adaptive copy +{adaptive:.3} dB; change point {cp_h:?} h after onset"
        );
        assert!(stop_h.is_some_and(|h| h <= 6.0), "stopped at {stop_h:?}");
        assert!(drift.abs() < 0.15, "reference drift {drift}");
        assert!(
            adaptive > 3.0 * drift.abs().max(0.05),
            "adaptive follows: {adaptive}"
        );
    }

    /// T-132 review: on an unchanged channel (the weak-interferer generator without the
    /// interferer) the sequential learning test must not hold an hour's learning off: over 30
    /// parked days every hour of day reaches hour-of-day maturity and none is latched. Several
    /// seeds, since a small own-hour pool biases a third of the hours.
    #[test]
    fn baseline_stationary_channel_matures_every_hour_without_latching() {
        let mut worst = (usize::MAX, f64::INFINITY, 0u32);
        for seed in 0..4_u64 {
            let mut rng = Rng(0x1330 + seed);
            let mut e = engine();
            let mut subject = None;
            for q in 0..(24 * 4 * 30) {
                let hours = f64::from(q) / 4.0;
                let row = leveled_row(hours, 0.5, -100.0, 2.0, 12.0 + rng.normal());
                let (_, _, o) = from_occupancy_stat(&row, 0).unwrap();
                subject = Some(o.subject);
                e.observe(&o);
            }
            let sub = &e.state.subjects[&subject.unwrap()];
            let hod_s: Vec<f64> = (0..24)
                .map(|h| hour_of_day_observed_s(sub, e.slot(at(f64::from(h)))))
                .collect();
            let mature = hod_s
                .iter()
                .filter(|s| **s >= MATURITY_MIN_OBSERVED_S)
                .count();
            let min_h = hod_s.iter().copied().fold(f64::INFINITY, f64::min) / H;
            println!(
                "T-132 stationary channel seed {seed}: {mature}/24 hours mature, least-learned \
                 hour {min_h:.2} h, latched hours {:#x}",
                sub.latched_hours
            );
            if mature < worst.0 {
                worst = (mature, min_h, sub.latched_hours);
            }
            worst.2 |= sub.latched_hours;
        }
        assert_eq!(worst.0, 24, "every hour matures: {worst:?}");
        assert_eq!(worst.2, 0, "no hour latched: {worst:?}");
    }

    /// T-132 item 2: a memory cap unloads (saves) least recently visited engines and loses no fold;
    /// with only the active key loaded, growth past the cap is refused.
    #[test]
    fn baseline_memory_cap_unloads_idle_engines_and_bounds_growth() {
        let root = std::env::temp_dir().join(format!("hk-bl-cap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let store = BaselineStore::open(&root).unwrap();
        let mut rng = Rng(0x1322);
        let one = {
            let mut b = Baselines::new(BaselineConfig::default(), 1, 16, None);
            b.observe(
                SiteKey::Site(SiteId::new()),
                0,
                CalKey::Uncalibrated,
                &occ_fold(channel(0), 0.0, 0.2, &mut rng),
            )
            .unwrap();
            b.memory_bytes()
        };
        let cap = 10 * one;
        let mut b = Baselines::new(BaselineConfig::default(), 1, 16, Some(store.clone()))
            .with_memory_cap(cap);
        let sites: Vec<SiteId> = (0..6).map(|_| SiteId::new()).collect();
        let mut max_seen = 0;
        for round in 0..3 {
            for (i, site) in sites.iter().enumerate() {
                for c in 0..3 {
                    let hours = f64::from(round) * 24.0 + i as f64 + c as f64 / 4.0;
                    let o = occ_fold(channel(c), hours, 0.2, &mut rng);
                    b.observe(SiteKey::Site(*site), 0, CalKey::Uncalibrated, &o)
                        .unwrap();
                    max_seen = max_seen.max(b.memory_bytes());
                    assert!(b.memory_bytes() <= cap, "{} > {cap}", b.memory_bytes());
                }
            }
        }
        b.flush(at(100.0), true).unwrap();
        println!(
            "T-132 memory cap: {one} B for one subject with one dense series ({} B per slot pair); \
             cap {cap} B, max loaded {max_seen} B, {} engines unloaded, {} folds refused",
            hk_store::baseline::SLOT_PAIR_BYTES,
            b.unloaded_engines(),
            b.refused_folds()
        );
        assert!(b.unloaded_engines() > 0);
        assert_eq!(b.refused_folds(), 0);
        // Every fold survived unloading: each subject's reference holds 3 × 900 s.
        let mut fresh = Baselines::new(BaselineConfig::default(), 1, 16, Some(store));
        for site in &sites {
            fresh.load_site(*site, 0).unwrap();
        }
        let observed: Vec<f64> = fresh
            .engines()
            .flat_map(|e| e.state.subjects.values())
            .map(|s| {
                s.gains
                    .iter()
                    .flat_map(|g| &g.reference)
                    .map(|r| r.observed_s)
                    .sum()
            })
            .collect();
        assert_eq!(observed.len(), 18);
        assert!(
            observed.iter().all(|o| (*o - 2700.0).abs() < 1e-6),
            "{observed:?}"
        );
        // Memory only, one key: growth past the cap is refused.
        let mut b = Baselines::new(BaselineConfig::default(), 1, 16, None)
            .with_memory_cap(2 * one + one / 2);
        let site = SiteKey::Site(SiteId::new());
        for c in 0..5 {
            b.observe(
                site,
                0,
                CalKey::Uncalibrated,
                &occ_fold(channel(c), 0.0, 0.2, &mut rng),
            )
            .unwrap();
            assert!(b.memory_bytes() <= 2 * one + one / 2);
        }
        assert_eq!(b.refused_folds(), 3);
        let _ = std::fs::remove_dir_all(root);
    }

    /// T-132 item 7: `load_site_outside_lock` reads the disk without holding the lock: a fold on
    /// another site completes while the (blocked) read is in progress.
    #[test]
    fn baseline_load_site_reads_outside_the_lock() {
        use std::sync::{Arc, mpsc};
        use std::time::Duration;
        let root = std::env::temp_dir().join(format!("hk-bl-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let store = BaselineStore::open(&root).unwrap();
        let mut rng = Rng(0x1327);
        let (a, other) = (SiteId::new(), SiteId::new());
        {
            let mut b = Baselines::new(BaselineConfig::default(), 1, 16, Some(store.clone()));
            b.observe(
                SiteKey::Site(a),
                0,
                CalKey::Uncalibrated,
                &occ_fold(channel(0), 0.0, 0.2, &mut rng),
            )
            .unwrap();
            b.flush(at(1.0), true).unwrap();
        }
        let shared = Arc::new(Mutex::new(Baselines::new(
            BaselineConfig::default(),
            1,
            16,
            Some(store),
        )));
        let (started_tx, started_rx) = mpsc::channel();
        let (go_tx, go_rx) = mpsc::channel::<()>();
        let loader = {
            let shared = shared.clone();
            std::thread::spawn(move || {
                Baselines::load_site_outside_lock_with(&shared, a, 0, |st, k| {
                    started_tx.send(()).unwrap();
                    go_rx
                        .recv_timeout(Duration::from_secs(20))
                        .expect("a concurrent fold completed during the slow read");
                    st.load(k)
                })
            })
        };
        started_rx.recv_timeout(Duration::from_secs(20)).unwrap();
        let out = shared
            .lock()
            .unwrap()
            .observe(
                SiteKey::Site(other),
                0,
                CalKey::Uncalibrated,
                &occ_fold(channel(1), 2.0, 0.2, &mut rng),
            )
            .unwrap();
        assert!(out.accrued, "the fold ran while the read was blocked");
        go_tx.send(()).unwrap();
        loader.join().unwrap().unwrap();
        let b = shared.lock().unwrap();
        assert!(
            b.engines().any(|e| e.state.key.site == a),
            "loaded after the read"
        );
        assert!(b.engines().any(|e| e.state.key.site == other));
        drop(b);
        let _ = std::fs::remove_dir_all(root);
    }

    /// T-132 review: a fold the memory cap refuses skips learning only; its novelty is still
    /// scored against what is loaded, so an emitter appearing under memory pressure is not lost.
    #[test]
    fn baseline_refused_fold_still_scores_novelty() {
        let site = SiteKey::Site(SiteId::new());
        let mut b = Baselines::new(BaselineConfig::default(), 1, 16, None);
        for q in 0..(24 * 4 * 3) {
            let hours = f64::from(q) / 4.0;
            let row = leveled_row(hours, 0.0, -100.0, 2.0 + 0.3 * ((q % 5) as f64 - 2.0), 25.0);
            let (_, _, o) = from_occupancy_stat(&row, 0).unwrap();
            b.observe(site, 0, CalKey::Uncalibrated, &o).unwrap();
        }
        let loaded = b.memory_bytes();
        b.set_memory_cap(Some(loaded));
        // An emitter 25 dB above the floor: a new occupied-level series, so the fold would grow.
        let (_, _, o) = from_occupancy_stat(&leveled_row(72.0, 0.8, -100.0, 2.0, 25.0), 0).unwrap();
        let out = b.observe(site, 0, CalKey::Uncalibrated, &o).unwrap();
        let before_learning = b.memory_bytes();
        b.set_memory_cap(None);
        let control = b.observe(site, 0, CalKey::Uncalibrated, &o).unwrap();
        println!(
            "T-132 refused fold: novelty {:.3} (level z {:?}), refused {}, memory {loaded} → \
             {before_learning} B; uncapped control novelty {:.3}",
            out.novelty.novelty,
            out.novelty.level_z,
            b.refused_folds(),
            control.novelty.novelty
        );
        assert_eq!(b.refused_folds(), 1);
        assert!(!out.accrued && !out.accrued_reference, "learning refused");
        assert_eq!(before_learning, loaded, "nothing grew");
        assert!(
            out.novelty.novelty >= 0.7 && out.novelty.level_z.is_some_and(|z| z >= 3.0),
            "novelty kept: {:?}",
            out.novelty
        );
        assert!(control.accrued);
    }

    /// T-132 review: a cap unload between the unlocked read's plan and its insert must not
    /// resurrect the stale disk copy the read returned (it would later be marked dirty and
    /// overwrite the newer file); the load re-plans and reads the newer file.
    #[test]
    fn baseline_unload_between_plan_and_insert_does_not_resurrect_stale_state() {
        let root = std::env::temp_dir().join(format!("hk-bl-gen-{}", SiteId::new()));
        let store = BaselineStore::open(&root).unwrap();
        let mut rng = Rng(0x1328);
        let (a, other) = (SiteKey::Site(SiteId::new()), SiteKey::Site(SiteId::new()));
        let SiteKey::Site(a_id) = a else {
            unreachable!()
        };
        let observed_of = |b: &Baselines| -> Option<f64> {
            b.engines().find(|e| e.state.key.site == a_id).map(|e| {
                e.state
                    .subjects
                    .values()
                    .flat_map(|s| s.gains.iter().flat_map(|g| &g.reference))
                    .map(|r| r.observed_s)
                    .sum()
            })
        };
        let mut fold = |h: f64| occ_fold(channel(0), h, 0.2, &mut rng);
        {
            // On disk: one fold (900 s).
            let mut b = Baselines::new(BaselineConfig::default(), 1, 16, Some(store.clone()));
            b.observe(a, 0, CalKey::Uncalibrated, &fold(0.0)).unwrap();
            b.flush(at(0.5), true).unwrap();
        }
        let (f1, f2) = (fold(1.0), fold(2.0));
        let shared = Mutex::new(Baselines::new(
            BaselineConfig::default(),
            1,
            16,
            Some(store.clone()),
        ));
        let mut reads = 0;
        Baselines::load_site_outside_lock_with(&shared, a_id, 0, |st, k| {
            reads += 1;
            let read = st.load(k);
            if reads == 1 {
                // While this (stale) copy is in flight: a fold loads and grows the key, then the
                // cap unloads it (saving 1800 s).
                let mut b = shared.lock().unwrap();
                b.observe(a, 0, CalKey::Uncalibrated, &f1).unwrap();
                b.set_memory_cap(Some(1));
                let _ = b.observe(other, 0, CalKey::Uncalibrated, &f2).unwrap();
                b.set_memory_cap(None);
                assert_eq!(observed_of(&b), None, "unloaded");
            }
            read
        })
        .unwrap();
        let in_memory = observed_of(&shared.lock().unwrap());
        // A later fold marks it dirty and it is written back.
        {
            let mut b = shared.lock().unwrap();
            b.observe(a, 0, CalKey::Uncalibrated, &fold(3.0)).unwrap();
            b.flush(at(3.5), true).unwrap();
        }
        let mut fresh = Baselines::new(BaselineConfig::default(), 1, 16, Some(store));
        fresh.load_site(a_id, 0).unwrap();
        let on_disk = observed_of(&fresh);
        println!(
            "T-132 generation: {reads} reads; loaded {in_memory:?} s (stale copy 900 s); after one \
             more fold on disk {on_disk:?} s"
        );
        assert!(reads >= 2, "re-planned");
        assert_eq!(
            in_memory,
            Some(1800.0),
            "the newer file, not the stale read"
        );
        assert_eq!(on_disk, Some(2700.0), "no fold lost");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn baseline_flush_enforces_quota_and_drops_evicted_engines() {
        let tmp = |tag: &str| std::env::temp_dir().join(format!("hk-t119-{tag}-{}", SiteId::new()));
        let mut rng = Rng(5);
        let fold = interval(channel(4), 0.0, 0.5, 10.0, &mut rng);
        let at_h = |mut o: IntervalObservation, h: f64| {
            o.t = at(h);
            o
        };
        // One site's file size.
        let d1 = tmp("quota-size");
        let s1 = BaselineStore::open(&d1).unwrap();
        let mut b = Baselines::new(BaselineConfig::default(), 1, 16, Some(s1.clone()));
        b.observe(SiteKey::Site(SiteId::new()), 0, CalKey::Uncalibrated, &fold)
            .unwrap();
        b.flush(at(0.5), true).unwrap();
        let size = s1.usage().unwrap();
        let _ = std::fs::remove_dir_all(d1);

        let d2 = tmp("quota");
        let store = BaselineStore::open(&d2).unwrap().with_quota(size * 3 / 2);
        let mut b = Baselines::new(BaselineConfig::default(), 1, 16, Some(store.clone()));
        let (a, bsite) = (SiteId::new(), SiteId::new());
        b.observe(SiteKey::Site(a), 0, CalKey::Uncalibrated, &fold)
            .unwrap();
        assert_eq!(b.flush(at(0.5), true).unwrap(), 1);
        // A dirty again (same visit time), then B visited later: A is the oldest and evicted.
        b.observe(SiteKey::Site(a), 0, CalKey::Uncalibrated, &fold)
            .unwrap();
        b.observe(
            SiteKey::Site(bsite),
            0,
            CalKey::Uncalibrated,
            &at_h(fold, 1.0),
        )
        .unwrap();
        b.flush(at(1.5), true).unwrap();
        let on_disk: Vec<_> = store.entries().unwrap().iter().map(|e| e.0.site).collect();
        assert_eq!(on_disk, vec![bsite], "A evicted from the store");
        assert!(
            b.engines().all(|e| e.state.key.site != a),
            "A's engine dropped"
        );
        // A later flush does not write A back.
        b.observe(
            SiteKey::Site(bsite),
            0,
            CalKey::Uncalibrated,
            &at_h(fold, 2.0),
        )
        .unwrap();
        b.flush(at(2.5), true).unwrap();
        let on_disk: Vec<_> = store.entries().unwrap().iter().map(|e| e.0.site).collect();
        assert_eq!(on_disk, vec![bsite]);
        let _ = std::fs::remove_dir_all(d2);
    }
}
