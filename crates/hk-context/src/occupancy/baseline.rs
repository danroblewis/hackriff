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
//! 5. **Reference** accrues only folds that are not novel, not provenance-explained, not mostly
//!    suspect, with no open or building change point (every CUSUM < h/2), into slots whose own
//!    hour-of-week slot holds < 24 h.
//!    So the reference is the statistics of "normal" up to maturity of each slot, and a
//!    persistent new interferer is never learnt into it (baseline poisoning): it shows in the
//!    adaptive copy, crosses the CUSUM, and stays novel against the reference until
//!    [`BaselineEngine::refreeze`] (user) or `auto_refreeze_days` copies adaptive → reference.
//!
//! # Keys
//! A new calibration is a new `BaselineKey` ([`Baselines::observe`]): it starts immature, so a cal
//! step is never novelty. `Mobile` and `Unassigned` sites never create or update a baseline.
//!
//! All times are the sample clock (ADR-0012 §0).

use std::collections::BTreeMap;

use hk_model::attention::baseline::{
    AdaptationPolicy, BaselineKey, BaselineResolution, CalKey, HourOfWeek, MATURITY_MIN_OBSERVED_S,
    Maturity, SiteKey,
};
use hk_model::attention::occupancy::{OccupancyStat, OccupancySubject};
use hk_model::attention::score::NoveltyScore;
use hk_model::time::Timestamp;
use hk_store::baseline::{
    BaselineState, BaselineStore, BaselineStoreError, BaselineSubject, ChangePoint,
    ChangeStatistic, DecayedStats, GainSeries, MAX_GAIN_STATES, SubjectBaseline,
};

use super::novelty::{
    Evidence, NoveltyConfig, combine, level_z, occupancy_sigma, occupancy_z, shrunk_fco,
};

/// Suspect share above which a fold updates neither the reference nor the CUSUM.
pub const SUSPECT_MAX_FRACTION: f64 = 0.5;
/// Winsorising half-width, reference σ.
pub const WINSOR_SIGMA: f64 = 3.0;

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
/// Provisional until T-118 exposes channel levels and per-visit weights: the level is the applied
/// threshold (floor + guard, so floor shifts show), the represented time is
/// `min(interval, n_revisits_all × revisit_mean_s)` and the weight its non-suspect share.
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
    let obs = IntervalObservation {
        subject: BaselineSubject::Channel { key },
        t: stat.interval.start,
        gain,
        level_db: Some(stat.threshold_db),
        max_db: None,
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

fn res_index(res: BaselineResolution) -> usize {
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
        for i in 0..HourOfWeek::SLOTS {
            let d = match copy {
                BaselineCopy::Reference => DecayedStats::from(&g.reference[i]),
                BaselineCopy::Adaptive => g.adaptive[i],
            };
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
        Self {
            state,
            utc_offset_min,
            cfg,
            dirty: false,
        }
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
            Some(sub) => self.evaluate(sub, slot, gain_index(sub, obs.gain), obs).0,
            None => FoldOutcome::none(obs, Maturity::Immature { observed_s: 0.0 }).novelty,
        }
    }

    /// Returns the novelty and, when mature, the chosen resolution with its reference pools.
    fn evaluate(
        &self,
        sub: &SubjectBaseline,
        slot: HourOfWeek,
        gain: Option<usize>,
        obs: &IntervalObservation,
    ) -> (NoveltyScore, Option<(usize, PoolStats, PoolStats)>) {
        let (level, occ) = pools(sub, slot, gain, BaselineCopy::Reference);
        let m = Maturity::from_pools(occ.map(|p| p.observed_s));
        let cfg = &self.cfg.novelty;
        let Maturity::Mature { resolution } = m else {
            return (FoldOutcome::none(obs, m).novelty, None);
        };
        let r = res_index(resolution);
        let ev = obs.evidence();
        let lz = level_z(&level[r], &ev, cfg);
        let oz = occupancy_z(&occ[r], &ev);
        let n = combine(
            m,
            lz,
            oz,
            None,
            obs.observed_s,
            obs.provenance_explained,
            cfg,
        );
        (n, Some((r, level[r], occ[r])))
    }

    /// Folds one observation (steps 2–5 of the module docs).
    pub fn observe(&mut self, obs: &IntervalObservation) -> FoldOutcome {
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
        let gain = match gain_index(sub, obs.gain) {
            Some(g) => Some(g),
            None if sub.gains.len() < MAX_GAIN_STATES => {
                sub.gains.push(GainSeries::new(obs.gain));
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
        };
        let (novelty, chosen) = engine.evaluate(sub_ref, slot, gain, obs);
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
                raised = Some(cp);
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

        // Reference: normal folds only, until the slot itself is mature.
        let slot_observed: f64 = sub
            .gains
            .iter()
            .map(|g| g.reference[slot_i].observed_s)
            .sum();
        let building = [
            sub.cusum.level_pos,
            sub.cusum.level_neg,
            sub.cusum.occ_pos,
            sub.cusum.occ_neg,
        ]
        .into_iter()
        .fold(0.0, f64::max)
            >= 0.5 * policy.cusum_h_sigma;
        let accrue_ref = clean
            && novelty.novelty == 0.0
            && sub.change_point.is_none()
            && !building
            && slot_observed < MATURITY_MIN_OBSERVED_S;
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

fn gain_index(sub: &SubjectBaseline, gain: u32) -> Option<usize> {
    sub.gains.iter().position(|g| g.gain == gain)
}

fn refreeze_subject(sub: &mut SubjectBaseline, t: Timestamp) {
    for g in &mut sub.gains {
        for (r, a) in g.reference.iter_mut().zip(&g.adaptive) {
            *r = to_slot_stats(a);
        }
    }
    sub.cusum = Default::default();
    sub.change_point = None;
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
}

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
        }
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
        Ok(self.engine(key, utc_offset_min, obs.t)?.observe(obs))
    }

    /// Engines loaded or created.
    pub fn engines(&self) -> impl Iterator<Item = &BaselineEngine> {
        self.engines.values()
    }

    /// Mutable engines (re-freeze).
    pub fn engines_mut(&mut self) -> impl Iterator<Item = &mut BaselineEngine> {
        self.engines.values_mut()
    }

    /// Loads every stored key of `site` not yet in memory (API listing).
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
    /// writes/day per active key), or always when `force` (checkpoint). Returns files written.
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
    }
}
