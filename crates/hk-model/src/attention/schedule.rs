//! Scheduler bandit contract and POI accounting (ADR-0012 §5; docs/04 §3.8).
//!
//! The bandit implementation belongs to T-120 (`hk-core::scheduler::bandit`); this module pins
//! what it is judged by: arm identity, the bounded per-dwell-second reward, the UCB index, dwell
//! sizing, and the probability-of-intercept numbers every report discloses.

use serde::{Deserialize, Serialize};

use super::{ValidationError, ensure, ensure_in};
use crate::region::TimeRange;

/// Identity of a bandit arm: a candidate window (§5.1). Stable across re-packing, so an arm's
/// history survives new candidate snapshots.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArmKey {
    /// RF path of the centre.
    pub rf_path: u8,
    /// Centre quantised to `arm_quantum_hz`: `round(center / quantum)`.
    pub center_q: i64,
    /// Sample rate, Hz (integral).
    pub rate_hz: u32,
}

/// Reward shaping (§5.2). Raw reward per dwell-second
/// r = (new_detection·n_new + novelty·Σnovelty + valid_decode·n_decodes + burst·n_bursts) / dwell_s,
/// squashed to u = r / (r + half_saturation_per_s) ∈ [0, 1).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RewardWeights {
    /// Per new (first-seen) non-suspect detection.
    pub new_detection: f64,
    /// Per unit of summed candidate novelty observed.
    pub novelty: f64,
    /// Per CRC/validity-passing decode.
    pub valid_decode: f64,
    /// Per confirmed non-suspect burst.
    pub burst: f64,
    /// Raw reward per second at which u = 0.5.
    pub half_saturation_per_s: f64,
}

impl Default for RewardWeights {
    fn default() -> Self {
        Self {
            new_detection: 1.0,
            novelty: 1.0,
            valid_decode: 0.5,
            burst: 0.1,
            half_saturation_per_s: 0.1,
        }
    }
}

/// What a finished dwell yielded, reported to the scheduler once detection, C12 and decoders have
/// processed it (§5.2). Suspect detections earn nothing and are counted as waste.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DwellOutcome {
    /// Step `seq` of the dwell.
    pub seq: u64,
    /// Arm dwelt on.
    pub arm: ArmKey,
    /// Observed seconds.
    pub dwell_s: f64,
    /// New non-suspect detections.
    pub new_detections: u32,
    /// Confirmed non-suspect bursts.
    pub bursts: u32,
    /// Σ novelty of candidates observed.
    pub novelty_sum: f64,
    /// Valid decodes.
    pub valid_decodes: u32,
    /// Suspect detections (IMD/spur/image/clipped).
    pub suspect_detections: u32,
}

impl DwellOutcome {
    /// The bounded reward u ∈ [0, 1).
    pub fn reward(&self, w: &RewardWeights) -> f64 {
        if self.dwell_s.is_nan() || self.dwell_s <= 0.0 {
            return 0.0;
        }
        let raw = (w.new_detection * f64::from(self.new_detections)
            + w.novelty * self.novelty_sum.max(0.0)
            + w.valid_decode * f64::from(self.valid_decodes)
            + w.burst * f64::from(self.bursts))
            / self.dwell_s;
        if raw <= 0.0 {
            0.0
        } else {
            raw / (raw + w.half_saturation_per_s)
        }
    }
}

/// Bandit configuration (§5.3). Stored with the ScanPlan's scheduler settings.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BanditConfig {
    /// UCB exploration constant c (rewards are in [0, 1)).
    pub ucb_c: f64,
    /// Discount half-life of arm statistics, s of radio time (non-stationary spectrum).
    pub discount_half_life_s: f64,
    /// Share of bandit time forced to the stalest feasible arm (exploration floor).
    pub exploration_floor: f64,
    /// Minimum share of radio time for the background sweep over `sweep_floor_window_s`, unless
    /// interactive intent holds the radio.
    pub sweep_floor: f64,
    /// Window of the sweep floor, s.
    pub sweep_floor_window_s: f64,
    /// Every feasible arm is revisited at least this often, s (starvation bound).
    pub max_arm_staleness_s: f64,
    /// Shortest dwell, s.
    pub min_dwell_s: f64,
    /// Longest dwell, s.
    pub max_dwell_s: f64,
    /// Dwell covers this many expected burst intervals.
    pub dwell_periods: f64,
    /// Prior weight of the candidate score as pseudo dwell-seconds.
    pub prior_pseudo_dwell_s: f64,
    /// Arm centre quantum, Hz.
    pub arm_quantum_hz: f64,
    /// Most arms tracked (preallocated).
    pub max_arms: u16,
    /// A suspect arm that failed verification is skipped for this long, s.
    pub suspect_ban_s: f64,
    /// Reward shaping.
    pub reward: RewardWeights,
}

impl Default for BanditConfig {
    fn default() -> Self {
        Self {
            ucb_c: 0.5,
            discount_half_life_s: 6.0 * 3600.0,
            exploration_floor: 0.15,
            sweep_floor: 0.25,
            sweep_floor_window_s: 600.0,
            max_arm_staleness_s: 1800.0,
            min_dwell_s: 2.0,
            max_dwell_s: 120.0,
            dwell_periods: 3.0,
            prior_pseudo_dwell_s: 5.0,
            arm_quantum_hz: 1e6,
            max_arms: 256,
            suspect_ban_s: 3600.0,
            reward: RewardWeights::default(),
        }
    }
}

impl BanditConfig {
    /// Checks ranges and orderings.
    pub fn validate(&self) -> Result<(), ValidationError> {
        ensure_in(self.ucb_c, 0.0, 10.0, "bandit.ucb_c")?;
        ensure_in(
            self.discount_half_life_s,
            60.0,
            30.0 * 86_400.0,
            "bandit.discount_half_life_s",
        )?;
        ensure_in(self.exploration_floor, 0.0, 0.9, "bandit.exploration_floor")?;
        ensure_in(self.sweep_floor, 0.0, 0.9, "bandit.sweep_floor")?;
        ensure_in(
            self.sweep_floor_window_s,
            10.0,
            86_400.0,
            "bandit.sweep_floor_window_s",
        )?;
        ensure_in(
            self.max_arm_staleness_s,
            1.0,
            7.0 * 86_400.0,
            "bandit.max_arm_staleness_s",
        )?;
        ensure_in(self.min_dwell_s, 0.01, 3600.0, "bandit.min_dwell_s")?;
        ensure_in(
            self.max_dwell_s,
            self.min_dwell_s,
            3600.0,
            "bandit.max_dwell_s",
        )?;
        ensure_in(self.dwell_periods, 1.0, 100.0, "bandit.dwell_periods")?;
        ensure_in(
            self.prior_pseudo_dwell_s,
            0.0,
            3600.0,
            "bandit.prior_pseudo_dwell_s",
        )?;
        ensure_in(self.arm_quantum_hz, 1.0, 1e9, "bandit.arm_quantum_hz")?;
        ensure(
            (1..=4096).contains(&self.max_arms),
            "bandit.max_arms",
            "must be 1–4096",
        )?;
        ensure_in(
            self.suspect_ban_s,
            0.0,
            7.0 * 86_400.0,
            "bandit.suspect_ban_s",
        )?;
        ensure_in(
            self.reward.half_saturation_per_s,
            1e-6,
            1e6,
            "bandit.reward.half_saturation_per_s",
        )?;
        for (v, f) in [
            (self.reward.new_detection, "bandit.reward.new_detection"),
            (self.reward.novelty, "bandit.reward.novelty"),
            (self.reward.valid_decode, "bandit.reward.valid_decode"),
            (self.reward.burst, "bandit.reward.burst"),
        ] {
            ensure_in(v, 0.0, 100.0, f)?;
        }
        ensure(
            self.exploration_floor + self.sweep_floor < 1.0,
            "bandit.sweep_floor",
            "floors must leave room for exploitation",
        )
    }

    /// Dwell length for a candidate with expected burst interval `expected_interval_s`:
    /// `dwell_periods` intervals, clamped to `[min_dwell_s, max_dwell_s]`; `min_dwell_s` × 4
    /// (clamped) when unknown.
    pub fn dwell_s(&self, expected_interval_s: Option<f64>) -> f64 {
        let want = expected_interval_s
            .filter(|i| i.is_finite() && *i > 0.0)
            .map_or(self.min_dwell_s * 4.0, |i| i * self.dwell_periods);
        want.clamp(self.min_dwell_s, self.max_dwell_s)
    }
}

/// Discounted, cost-normalised UCB index (§5.3):
/// `mean + c·sqrt(ln(max(total_dwell_s, e)) / arm_dwell_s)`; an arm with no dwell-seconds is
/// infinite (tried first).
pub fn ucb_index(mean_reward: f64, arm_dwell_s: f64, total_dwell_s: f64, c: f64) -> f64 {
    if arm_dwell_s.is_nan() || arm_dwell_s <= 0.0 {
        return f64::INFINITY;
    }
    mean_reward + c * (total_dwell_s.max(std::f64::consts::E).ln() / arm_dwell_s).sqrt()
}

/// Revisit interval required for complete capture of an emitter with minimum on/off time
/// `min_on_off_s` (SM.1880: ≤ half of it).
pub fn required_revisit_s(min_on_off_s: f64) -> f64 {
    min_on_off_s / 2.0
}

/// The docs/04 §3.8 single-burst probability of intercept for periodic revisits:
/// `min(1, (τ + T_d) / T_R)`.
pub fn nominal_poi(tau_s: f64, dwell_s: f64, revisit_s: f64) -> f64 {
    if revisit_s.is_nan() || revisit_s <= 0.0 {
        return 1.0;
    }
    ((tau_s + dwell_s) / revisit_s).clamp(0.0, 1.0)
}

/// POI from the observation log for irregular revisits (§5.5): the fraction of burst start times
/// in `span` at which a burst of duration `tau_ns` overlaps at least one observed interval, i.e.
/// |(∪ᵢ [sᵢ − τ, eᵢ]) ∩ span| / |span|. Reduces to [`nominal_poi`] for periodic visits. Visits need
/// not be sorted.
pub fn poi_fraction(visits: &[TimeRange], span: TimeRange, tau_ns: i64) -> f64 {
    let (s0, s1) = (
        i128::from(span.start.as_unix_nanos()),
        i128::from(span.end.as_unix_nanos()),
    );
    if s1 <= s0 {
        return 0.0;
    }
    let tau = i128::from(tau_ns.max(0));
    let mut iv: Vec<(i128, i128)> = visits
        .iter()
        .map(|v| {
            let lo = (i128::from(v.start.as_unix_nanos()) - tau).max(s0);
            let hi = i128::from(v.end.as_unix_nanos()).min(s1);
            (lo, hi)
        })
        .filter(|(lo, hi)| hi > lo)
        .collect();
    iv.sort_unstable();
    let mut covered = 0i128;
    let mut cur: Option<(i128, i128)> = None;
    for (lo, hi) in iv {
        match cur {
            Some((clo, chi)) if lo <= chi => cur = Some((clo, chi.max(hi))),
            Some((clo, chi)) => {
                covered += chi - clo;
                cur = Some((lo, hi));
            }
            None => cur = Some((lo, hi)),
        }
    }
    if let Some((clo, chi)) = cur {
        covered += chi - clo;
    }
    covered as f64 / (s1 - s0) as f64
}

/// Probability of at least one intercept of bursts at rate `rate_hz` over `t_obs_s`:
/// `1 − (1 − P_POI)^(r·T_obs)`.
pub fn p_at_least_one(p_poi: f64, rate_hz: f64, t_obs_s: f64) -> f64 {
    let n = (rate_hz * t_obs_s).max(0.0);
    (1.0 - (1.0 - p_poi.clamp(0.0, 1.0)).powf(n)).clamp(0.0, 1.0)
}

/// One disclosed POI row (§6): for bursts of duration `tau_s` in a region.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoiEntry {
    /// Burst duration, s.
    pub tau_s: f64,
    /// [`poi_fraction`].
    pub p_poi: f64,
    /// Burst rate assumed, Hz, when [`PoiEntry::p_at_least_one`] is given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_hz: Option<f64>,
    /// [`p_at_least_one`] over the report span.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p_at_least_one: Option<f64>,
}

impl PoiEntry {
    /// Checks ranges and that rate and P≥1 go together.
    pub fn validate(&self) -> Result<(), ValidationError> {
        ensure_in(self.tau_s, 0.0, 1e7, "poi.tau_s")?;
        ensure_in(self.p_poi, 0.0, 1.0, "poi.p_poi")?;
        ensure(
            self.rate_hz.is_some() == self.p_at_least_one.is_some(),
            "poi.rate_hz",
            "rate and p_at_least_one go together",
        )?;
        if let Some(p) = self.p_at_least_one {
            ensure_in(p, 0.0, 1.0, "poi.p_at_least_one")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::Timestamp;

    fn tr(a_s: f64, b_s: f64) -> TimeRange {
        TimeRange::new(
            Timestamp::from_unix_nanos((a_s * 1e9) as i64),
            Timestamp::from_unix_nanos((b_s * 1e9) as i64),
        )
    }

    #[test]
    fn poi_matches_the_periodic_formula() {
        // Dwell 0.4 s every 750 s (docs/04 worked example scale), 5 ms bursts, 100 periods.
        let (td, trv, tau) = (0.4, 750.0, 0.005);
        let visits: Vec<TimeRange> = (0..100)
            .map(|i| tr(i as f64 * trv + 1.0, i as f64 * trv + 1.0 + td))
            .collect();
        let p = poi_fraction(&visits, tr(0.0, 100.0 * trv), (tau * 1e9) as i64);
        assert!((p - nominal_poi(tau, td, trv)).abs() < 1e-6, "{p}");
        // Continuous observation: 1.
        assert_eq!(poi_fraction(&[tr(0.0, 10.0)], tr(0.0, 10.0), 1), 1.0);
        // Overlapping and unsorted visits merge.
        let p = poi_fraction(
            &[tr(5.0, 7.0), tr(0.0, 2.0), tr(1.0, 3.0)],
            tr(0.0, 10.0),
            0,
        );
        assert!((p - 0.5).abs() < 1e-12);
        assert_eq!(poi_fraction(&[], tr(0.0, 10.0), 0), 0.0);
        assert_eq!(nominal_poi(1.0, 1.0, 1.0), 1.0);
    }

    #[test]
    fn repeated_bursts() {
        assert!(
            (p_at_least_one(0.007, 1.0 / 60.0, 3600.0) - (1.0 - 0.993f64.powf(60.0))).abs() < 1e-12
        );
        assert_eq!(p_at_least_one(0.0, 1.0, 100.0), 0.0);
        assert_eq!(p_at_least_one(1.0, 1.0, 100.0), 1.0);
    }

    #[test]
    fn reward_is_bounded_per_dwell_second() {
        let w = RewardWeights::default();
        let mut o = DwellOutcome {
            seq: 1,
            arm: ArmKey {
                rf_path: 0,
                center_q: 433,
                rate_hz: 2_000_000,
            },
            dwell_s: 10.0,
            new_detections: 1,
            bursts: 0,
            novelty_sum: 0.0,
            valid_decodes: 0,
            suspect_detections: 5,
        };
        assert!(
            (o.reward(&w) - 0.5).abs() < 1e-12,
            "0.1/s raw = half saturation"
        );
        o.dwell_s = 100.0;
        assert!(
            o.reward(&w) < 0.5,
            "same yield over longer dwell earns less per second"
        );
        o.new_detections = 1_000_000;
        assert!(o.reward(&w) < 1.0);
        o.dwell_s = 0.0;
        assert_eq!(o.reward(&w), 0.0);
    }

    #[test]
    fn ucb_and_dwell_sizing() {
        assert_eq!(ucb_index(0.2, 0.0, 100.0, 0.5), f64::INFINITY);
        assert!(ucb_index(0.2, 10.0, 1000.0, 0.5) > ucb_index(0.2, 100.0, 1000.0, 0.5));
        let cfg = BanditConfig::default();
        cfg.validate().unwrap();
        assert_eq!(cfg.dwell_s(Some(30.0)), 90.0);
        assert_eq!(cfg.dwell_s(Some(300.0)), 120.0);
        assert_eq!(cfg.dwell_s(None), 8.0);
        assert_eq!(required_revisit_s(10.0), 5.0);
        let bad = BanditConfig {
            exploration_floor: 0.5,
            sweep_floor: 0.5,
            ..cfg
        };
        assert!(bad.validate().is_err());
        let mut v = serde_json::to_value(cfg).unwrap();
        v["epsilon"] = serde_json::json!(0.1);
        assert!(serde_json::from_value::<BanditConfig>(v).is_err());
    }
}
