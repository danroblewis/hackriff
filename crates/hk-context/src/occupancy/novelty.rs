//! Novelty against a baseline (T-119, ADR-0012 §4.4): level and occupancy z-scores normalised by
//! observation time, new-emitter novelty as a Poisson tail, combined and forced to zero when the
//! baseline is immature or provenance explains the change.
//!
//! - **`level_z`** = (level − pool mean)/max(σ_pool, 1 dB), upward only (`level-above-baseline`;
//!   a drop in level is a front-end or propagation question, not a new signal).
//! - **`occupancy_z`** = (FCO_obs − p)/√(max(p(1−p), q(1−q))/n_eff + v_between), two-sided, with
//!   q the observed FCO shrunk by half a sample (a conservative variance: a few looks cannot
//!   exploit the normal approximation at tiny p). `p` is the pool's
//!   time-weighted FCO shrunk by half an effective sample towards ½ (so an empty channel's p is
//!   small but not 0, and one occupied look is not infinitely novel); `n_eff` is the interval's
//!   effective samples (§2.4), so a short look cannot produce a large z; `v_between` is the
//!   spread of the pool's slot FCOs, so a coarse pool (e.g. all hours) does not call an
//!   hour-of-week pattern novel.
//! - **`new_emitter`** = [`new_emitter_novelty`] of first sightings in a window against the site's
//!   baseline first-sighting rate ([`FirstSightingRate`]).
//! - **Combined** = max of `novelty_from_z(z, 3, 10)` over the z-scores and `new_emitter`.

use std::collections::VecDeque;

use hk_model::attention::baseline::{MATURITY_MIN_OBSERVED_S, Maturity};
use hk_model::attention::score::{NoveltyScore, new_emitter_novelty, novelty_from_z};
use hk_model::time::Timestamp;

use super::baseline::PoolStats;

/// Novelty mapping settings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NoveltyConfig {
    /// z at which novelty starts (3).
    pub z_min: f64,
    /// z at which novelty saturates at 1 (10).
    pub z_sat: f64,
    /// Floor on the level σ, dB (1).
    pub sigma_floor_db: f64,
}

impl Default for NoveltyConfig {
    fn default() -> Self {
        Self {
            z_min: 3.0,
            z_sat: 10.0,
            sigma_floor_db: 1.0,
        }
    }
}

/// What one interval measured on a subject, as novelty needs it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Evidence {
    /// Representative level, dB.
    pub level_db: Option<f64>,
    /// Time-weighted FCO.
    pub fco: Option<f64>,
    /// Effective independent samples behind `fco` (§2.4).
    pub n_eff: f64,
    /// Σ weight behind `fco`, s.
    pub weight_s: f64,
    /// Represented seconds.
    pub observed_s: f64,
}

/// Level z against a level pool (upward), `None` without a mean.
pub fn level_z(pool: &PoolStats, ev: &Evidence, cfg: &NoveltyConfig) -> Option<f64> {
    let level = ev.level_db?;
    let mean = pool.mean_db()?;
    let sigma = pool.std_db().unwrap_or(0.0).max(cfg.sigma_floor_db);
    Some((level - mean) / sigma)
}

/// The pool's shrunk FCO for an interval whose one effective sample represents `delta_s`.
pub fn shrunk_fco(pool: &PoolStats, delta_s: f64) -> Option<f64> {
    let f = pool.fco()?;
    let d = delta_s.max(1e-9);
    Some((f * pool.weight_s + 0.5 * d) / (pool.weight_s + d))
}

/// σ of an interval FCO with `n_eff` samples around the pool (binomial + between-slot spread).
pub fn occupancy_sigma(pool: &PoolStats, p: f64, n_eff: f64) -> f64 {
    (p * (1.0 - p) / n_eff.max(1.0) + pool.between_var()).sqrt()
}

/// Occupancy z (two-sided sign kept), `None` without FCO on either side.
pub fn occupancy_z(pool: &PoolStats, ev: &Evidence) -> Option<f64> {
    let fco = ev.fco?;
    if ev.n_eff < 1.0 || ev.weight_s <= 0.0 {
        return None;
    }
    let p = shrunk_fco(pool, ev.weight_s / ev.n_eff)?;
    // Conservative variance: the larger of the pool's and the (shrunk) observed binomial
    // variance, so a handful of looks cannot exploit the normal approximation at tiny p.
    let q = (fco * ev.n_eff + 0.5) / (ev.n_eff + 1.0);
    let var = (p * (1.0 - p)).max(q * (1.0 - q)) / ev.n_eff + pool.between_var();
    Some((fco - p) / var.sqrt())
}

/// Combines the components into a [`NoveltyScore`] under the zero rules.
pub fn combine(
    maturity: Maturity,
    level_z: Option<f64>,
    occupancy_z: Option<f64>,
    new_emitter: Option<f64>,
    observed_s: f64,
    provenance_explained: bool,
    cfg: &NoveltyConfig,
) -> NoveltyScore {
    let from_z = |z: Option<f64>| z.map_or(0.0, |z| novelty_from_z(z, cfg.z_min, cfg.z_sat));
    let raw = from_z(level_z)
        .max(from_z(occupancy_z.map(f64::abs)))
        .max(new_emitter.unwrap_or(0.0));
    let novelty = if maturity.is_mature() && !provenance_explained {
        raw
    } else {
        0.0
    };
    NoveltyScore {
        novelty,
        level_z: level_z.filter(|z| z.is_finite()),
        occupancy_z: occupancy_z.filter(|z| z.is_finite()),
        new_emitter: new_emitter.map(|v| v.clamp(0.0, 1.0)),
        observed_s: observed_s.max(0.0),
        maturity,
        provenance_explained,
    }
}

/// The site's baseline rate of first sightings (new emitters per observed second) and a sliding
/// window of recent sightings (§4.4). Mature once `MATURITY_MIN_OBSERVED_S` of observation is
/// behind the rate.
#[derive(Clone, Debug, Default)]
pub struct FirstSightingRate {
    baseline_k: f64,
    baseline_observed_s: f64,
    window: VecDeque<(Timestamp, u64, f64)>,
}

impl FirstSightingRate {
    /// Prior sightings added to the baseline count (so a never-seen rate is not zero).
    pub const PRIOR_COUNT: f64 = 1.0;
    /// Sliding window, s.
    pub const WINDOW_S: f64 = 3600.0;

    /// Folds `k` first sightings over `observed_s` ending at `t`. `accrue` adds them to the
    /// baseline rate (a `Site` key, not novel); the window always gets them.
    pub fn record(&mut self, t: Timestamp, k: u64, observed_s: f64, accrue: bool) {
        if accrue {
            self.baseline_k += k as f64;
            self.baseline_observed_s += observed_s.max(0.0);
        }
        self.window.push_back((t, k, observed_s.max(0.0)));
        let horizon = t.as_unix_nanos() - (Self::WINDOW_S * 1e9) as i64;
        while self
            .window
            .front()
            .is_some_and(|(ts, _, _)| ts.as_unix_nanos() <= horizon)
        {
            self.window.pop_front();
        }
    }

    /// Baseline rate, per observed second; `None` while immature.
    pub fn rate(&self) -> Option<f64> {
        (self.baseline_observed_s >= MATURITY_MIN_OBSERVED_S)
            .then(|| (self.baseline_k + Self::PRIOR_COUNT) / self.baseline_observed_s)
    }

    /// Novelty of the window's sightings; `None` while immature.
    pub fn novelty(&self) -> Option<f64> {
        let rate = self.rate()?;
        let (k, s) = self
            .window
            .iter()
            .fold((0, 0.0), |(k, s), (_, dk, ds)| (k + dk, s + ds));
        Some(new_emitter_novelty(k, rate, s))
    }
}

#[cfg(test)]
mod tests {
    use hk_model::attention::baseline::BaselineResolution;

    use super::*;

    fn pool(levels: &[f64], fcos: &[(f64, f64)]) -> PoolStats {
        let mut p = PoolStats::default();
        for (i, l) in levels.iter().enumerate() {
            let (f, w) = fcos.get(i).copied().unwrap_or((0.0, 0.0));
            p.add(1.0, w, *l, l * l, f * w, w, *l);
        }
        p
    }

    fn mature() -> Maturity {
        Maturity::Mature {
            resolution: BaselineResolution::AllHours,
        }
    }

    #[test]
    fn novelty_level_z_floors_sigma_and_is_upward() {
        let cfg = NoveltyConfig::default();
        let flat = pool(&[-100.0; 10], &[]);
        let ev = |l| Evidence {
            level_db: Some(l),
            fco: None,
            n_eff: 0.0,
            weight_s: 0.0,
            observed_s: 900.0,
        };
        assert_eq!(
            level_z(&flat, &ev(-95.0), &cfg),
            Some(5.0),
            "σ floored at 1 dB"
        );
        let n = combine(mature(), Some(-20.0), None, None, 900.0, false, &cfg);
        assert_eq!(n.novelty, 0.0, "a level drop is not novelty");
        let n = combine(mature(), Some(6.5), None, None, 900.0, false, &cfg);
        assert_eq!(n.novelty, 0.5);
    }

    #[test]
    fn novelty_occupancy_z_is_normalised_by_observation_time() {
        // A channel empty over 30 h of 15-min intervals.
        let quiet = pool(&[-100.0; 120], &[(0.0, 900.0); 120]);
        let look = |n_eff: f64, fco| Evidence {
            level_db: None,
            fco: Some(fco),
            n_eff,
            weight_s: 900.0,
            observed_s: 900.0,
        };
        let short = occupancy_z(&quiet, &look(1.0, 1.0)).unwrap();
        let long = occupancy_z(&quiet, &look(30.0, 0.6)).unwrap();
        assert!(short < 3.0, "one occupied look is not novel: z={short}");
        assert!(long > 5.0, "a sustained emitter is: z={long}");
        // A patterned channel (half the slots 0.1, half 0.7) under a coarse pool: its daytime FCO
        // is not novel thanks to the between-slot spread.
        let fcos: Vec<_> = (0..120)
            .map(|i| (if i % 2 == 0 { 0.1 } else { 0.7 }, 900.0))
            .collect();
        let patterned = pool(&[-100.0; 120], &fcos);
        assert!(occupancy_z(&patterned, &look(30.0, 0.75)).unwrap() < 3.0);
    }

    #[test]
    fn novelty_zero_rules_and_new_emitter_rate() {
        let cfg = NoveltyConfig::default();
        let imm = Maturity::Immature { observed_s: 10.0 };
        let n = combine(imm, Some(50.0), Some(50.0), Some(1.0), 1.0, false, &cfg);
        assert_eq!(n.novelty, 0.0);
        n.validate().unwrap();
        let n = combine(mature(), Some(50.0), None, None, 1.0, true, &cfg);
        assert_eq!(n.novelty, 0.0);
        n.validate().unwrap();

        let t = |h: f64| Timestamp::from_unix_nanos((h * 3.6e12) as i64);
        let mut r = FirstSightingRate::default();
        // Two new emitters a day while building the baseline.
        for h in 0..24 {
            r.record(t(f64::from(h)), u64::from(h % 12 == 0), 3600.0, true);
            assert!(h == 23 || r.novelty().is_none(), "immature before 24 h");
        }
        assert!(r.novelty().unwrap() < 0.1, "the usual rate is not novel");
        // Ten first sightings in the next hour: novel.
        r.record(t(24.5), 10, 3600.0, false);
        assert!(r.novelty().unwrap() > 0.9, "{:?}", r.novelty());
    }
}
