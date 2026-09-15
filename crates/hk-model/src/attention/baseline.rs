//! Baselines (ADR-0012 §3): site keying for a moving device, hour-of-week slots, mergeable slot
//! statistics, calibration keys and the maturity rule.

use serde::{Deserialize, Serialize};

use super::{ValidationError, ensure, ensure_in};
use crate::ids::{CalibrationStateId, SiteId};
use crate::time::Timestamp;

/// Which baseline a measurement belongs to (§3.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind", content = "id")]
pub enum SiteKey {
    /// A discrete site: accrues baselines and may raise novelty alarms.
    Site(SiteId),
    /// Moving (speed or position spread over the radius): observations and occupancy are kept,
    /// baselines do not accrue and baseline alarms are suppressed.
    Mobile,
    /// No site known (no fix and none configured): like `Mobile`.
    Unassigned,
}

impl SiteKey {
    /// Measurements under this key update baselines.
    pub fn accrues_baseline(&self) -> bool {
        matches!(self, SiteKey::Site(_))
    }
}

/// How a site was established.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SiteSource {
    /// Configured (`--site` / config file).
    Config,
    /// Chosen or confirmed by the user through the API.
    User,
    /// Clustered from GNSS fixes (C06).
    Gnss,
}

/// A discrete site (ADR-0012 §3.5). Aggregate; the name is user metadata.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SiteRecord {
    /// Id.
    pub id: SiteId,
    /// User name, e.g. `home`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Centroid latitude, degrees (absent for a configured site without coordinates).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lat_deg: Option<f64>,
    /// Centroid longitude, degrees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lon_deg: Option<f64>,
    /// Membership radius, m.
    pub radius_m: f64,
    /// Fixed UTC offset used for hour-of-week slots, minutes (no DST; see open questions).
    pub utc_offset_min: i16,
    /// Source.
    pub source: SiteSource,
    /// First observation.
    pub first_seen: Timestamp,
    /// Last observation.
    pub last_seen: Timestamp,
    /// Observed seconds at the site (all tiers).
    pub observed_s: f64,
}

impl SiteRecord {
    /// Checks coordinates (both or neither, valid ranges), radius, offset and times.
    pub fn validate(&self) -> Result<(), ValidationError> {
        ensure(
            self.lat_deg.is_some() == self.lon_deg.is_some(),
            "lat_deg",
            "lat and lon go together",
        )?;
        if let (Some(lat), Some(lon)) = (self.lat_deg, self.lon_deg) {
            ensure_in(lat, -90.0, 90.0, "lat_deg")?;
            ensure_in(lon, -180.0, 180.0, "lon_deg")?;
        }
        ensure_in(self.radius_m, 10.0, 50_000.0, "radius_m")?;
        ensure(
            (-840..=840).contains(&self.utc_offset_min),
            "utc_offset_min",
            "must be ±14 h",
        )?;
        ensure(
            self.last_seen >= self.first_seen,
            "last_seen",
            "before first_seen",
        )?;
        ensure_in(self.observed_s, 0.0, f64::MAX, "observed_s")?;
        if let Some(n) = &self.name {
            ensure(
                !n.trim().is_empty() && n.len() <= 64,
                "name",
                "1–64 bytes, not blank",
            )?;
        }
        Ok(())
    }
}

/// The site assignment in force (T-136, §3.5), persisted so a restart on the same database keeps
/// a pinned site, and a fixed site within its no-fix hold, instead of starting `Unassigned`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SiteAssignment {
    /// The discrete site.
    pub site: SiteId,
    /// How it was set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set_by: Option<SiteSource>,
    /// Pinned by config or the user (fixes do not move it).
    pub pinned: bool,
    /// Last in-site time (sample clock): a pin's time, else the last in-site fix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_in_site: Option<Timestamp>,
}

/// Site assignment settings (§3.5).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SiteConfig {
    /// Membership radius of a new site, m (default 250).
    pub radius_m: f64,
    /// Above this ground speed the device is mobile, m/s (default 1.0).
    pub mobile_speed_m_s: f64,
    /// Window over which position spread is judged, s (default 300).
    pub mobile_window_s: f64,
    /// Without a fix, keep the last site for this long after the last in-site fix, s
    /// (default 600; 0 = go `unassigned` immediately).
    pub no_fix_hold_s: f64,
}

impl Default for SiteConfig {
    fn default() -> Self {
        Self {
            radius_m: 250.0,
            mobile_speed_m_s: 1.0,
            mobile_window_s: 300.0,
            no_fix_hold_s: 600.0,
        }
    }
}

impl SiteConfig {
    /// Checks ranges.
    pub fn validate(&self) -> Result<(), ValidationError> {
        ensure_in(self.radius_m, 10.0, 50_000.0, "site.radius_m")?;
        ensure_in(self.mobile_speed_m_s, 0.1, 100.0, "site.mobile_speed_m_s")?;
        ensure_in(self.mobile_window_s, 10.0, 86_400.0, "site.mobile_window_s")?;
        ensure_in(self.no_fix_hold_s, 0.0, 86_400.0, "site.no_fix_hold_s")
    }
}

/// Hour-of-week slot, 0–167: 0 is Monday 00:00–01:00 in the site's offset time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub struct HourOfWeek(u8);

impl HourOfWeek {
    /// Slots per week.
    pub const SLOTS: usize = 168;

    /// The slot of `t` at a fixed UTC offset.
    pub fn of(t: Timestamp, utc_offset_min: i16) -> Self {
        const NS_PER_HOUR: i64 = 3_600_000_000_000;
        let local_ns = t.as_unix_nanos() + i64::from(utc_offset_min) * 60_000_000_000;
        let hours = local_ns.div_euclid(NS_PER_HOUR);
        // 1970-01-01 was a Thursday: Monday-based day index is (days + 3) mod 7.
        let day = (hours.div_euclid(24) + 3).rem_euclid(7);
        let hour = hours.rem_euclid(24);
        Self((day * 24 + hour) as u8)
    }

    /// Slot index.
    pub fn index(self) -> usize {
        usize::from(self.0)
    }

    /// Hour of day, 0–23.
    pub fn hour_of_day(self) -> u8 {
        self.0 % 24
    }

    /// Day part 0–3 (00–06, 06–12, 12–18, 18–24).
    pub fn day_part(self) -> u8 {
        self.hour_of_day() / 6
    }
}

impl TryFrom<u8> for HourOfWeek {
    type Error = &'static str;
    fn try_from(v: u8) -> Result<Self, Self::Error> {
        if usize::from(v) < Self::SLOTS {
            Ok(Self(v))
        } else {
            Err("hour-of-week must be 0–167")
        }
    }
}

impl From<HourOfWeek> for u8 {
    fn from(h: HourOfWeek) -> u8 {
        h.0
    }
}

/// Calibration part of a baseline key: levels are only comparable under one calibration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind", content = "id")]
pub enum CalKey {
    /// dBFS, no calibration state.
    Uncalibrated,
    /// A CalibrationState version.
    Calibrated(CalibrationStateId),
}

/// Key of one baseline: site × calibration × grid (§3.1). The 168 slots and the per-slot,
/// per-gain-state level statistics live inside it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BaselineKey {
    /// Site.
    pub site: SiteId,
    /// Calibration.
    pub cal: CalKey,
    /// History pyramid scheme whose level-0 grid the cells are multiples of.
    pub scheme: u16,
    /// Baseline cell = `cell_factor` × level-0 cell (default 16: 100 kHz on scheme 1).
    pub cell_factor: u16,
}

/// Pools a slot can fall back to, finest first (§3.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BaselineResolution {
    /// The 168 hour-of-week slots.
    HourOfWeek,
    /// 24 hour-of-day slots, weekdays pooled.
    HourOfDay,
    /// 4 six-hour day parts.
    DayPart,
    /// All hours pooled.
    AllHours,
}

impl BaselineResolution {
    /// Finest first.
    pub const FINEST_FIRST: [BaselineResolution; 4] = [
        BaselineResolution::HourOfWeek,
        BaselineResolution::HourOfDay,
        BaselineResolution::DayPart,
        BaselineResolution::AllHours,
    ];
}

/// Minimum observed time in a pool for it to be mature, s (SM.1880: ≥ 24 h for unknown patterns).
pub const MATURITY_MIN_OBSERVED_S: f64 = 86_400.0;

/// Whether novelty against a baseline means anything yet (§3.2).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "state", deny_unknown_fields)]
pub enum Maturity {
    /// No pool holds 24 h: novelty and baseline alarms are suppressed ("baseline immature").
    Immature {
        /// Observed seconds in the coarsest pool.
        observed_s: f64,
    },
    /// The finest pool holding ≥ 24 h.
    Mature {
        /// That pool.
        resolution: BaselineResolution,
    },
}

impl Maturity {
    /// Picks the finest mature pool from observed seconds per pool, finest first
    /// ([`BaselineResolution::FINEST_FIRST`] order): the slot's hour-of-week, its hour-of-day, its
    /// day part and all hours.
    pub fn from_pools(observed_s: [f64; 4]) -> Self {
        BaselineResolution::FINEST_FIRST
            .iter()
            .zip(observed_s)
            .find(|(_, s)| *s >= MATURITY_MIN_OBSERVED_S)
            .map_or(
                Maturity::Immature {
                    observed_s: observed_s[3],
                },
                |(r, _)| Maturity::Mature { resolution: *r },
            )
    }

    /// Mature at any resolution.
    pub fn is_mature(&self) -> bool {
        matches!(self, Maturity::Mature { .. })
    }
}

/// Mergeable statistics of one baseline cell (or channel) in one slot (§3.3). Pools are sums of
/// slots, so every field is additive. dB moments are of winsorised values (the engine clips at
/// the reference mean ± 3σ before adding) so one burst does not swamp the spread.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlotStats {
    /// Visits folded.
    pub n_visits: u64,
    /// Observed seconds.
    pub observed_s: f64,
    /// Σ level, dB.
    pub sum_db: f64,
    /// Σ level², dB².
    pub sum_sq_db: f64,
    /// Σ weight·occupied (occupied ∈ {0, 1}), s.
    pub occupied_weight_s: f64,
    /// Σ weight, s.
    pub weight_s: f64,
    /// Max level seen, dB (not winsorised). `-inf` when empty.
    pub max_db: f64,
    /// T-146 sampling moment of the slot FCO: Σ(wᵢ²/n_eff,ᵢ)/Σwᵢ over the folded intervals, s.
    /// Under an occupancy probability p the slot FCO's binomial sampling variance is
    /// p(1−p)·`fco_var_s`/`weight_s` ([`Self::fco_var_after`]). 0 for slots written before T-146.
    #[serde(default)]
    pub fco_var_s: f64,
}

impl SlotStats {
    /// An empty slot.
    pub const EMPTY: SlotStats = SlotStats {
        n_visits: 0,
        observed_s: 0.0,
        sum_db: 0.0,
        sum_sq_db: 0.0,
        occupied_weight_s: 0.0,
        weight_s: 0.0,
        max_db: f64::NEG_INFINITY,
        fco_var_s: 0.0,
    };

    /// T-146: the [`Self::fco_var_s`] moment after folding an interval of weight `weight_s` and
    /// `n_eff` effective samples (floored at 1, as the novelty σ) into a slot holding `fco_var_s`
    /// over `prior_weight_s`. The moment is Σw²/n_eff divided by Σw, so multiplying every weight
    /// by a forgetting factor multiplies it by the same factor (it scales like the other additive
    /// moments) while the sampling variance p(1−p)·moment/Σw stays exact.
    pub fn fco_var_after(fco_var_s: f64, prior_weight_s: f64, weight_s: f64, n_eff: f64) -> f64 {
        if weight_s.is_nan() || weight_s <= 0.0 || !n_eff.is_finite() {
            return fco_var_s;
        }
        let prior = prior_weight_s.max(0.0);
        (fco_var_s * prior + weight_s * weight_s / n_eff.max(1.0)) / (prior + weight_s)
    }

    /// Folds one visit: its (winsorised) level, raw max, occupancy and time weight.
    pub fn add(
        &mut self,
        level_db: f64,
        max_db: f64,
        occupied: bool,
        weight_s: f64,
        observed_s: f64,
    ) {
        if self.n_visits == 0 {
            self.max_db = f64::NEG_INFINITY;
        }
        self.n_visits += 1;
        self.observed_s += observed_s;
        self.sum_db += level_db;
        self.sum_sq_db += level_db * level_db;
        self.weight_s += weight_s;
        if occupied {
            self.occupied_weight_s += weight_s;
        }
        self.max_db = self.max_db.max(max_db);
    }

    /// Adds another slot (pooling).
    pub fn merge(&mut self, o: &SlotStats) {
        if o.n_visits == 0 {
            return;
        }
        if self.n_visits == 0 {
            *self = *o;
            return;
        }
        self.n_visits += o.n_visits;
        self.observed_s += o.observed_s;
        self.sum_db += o.sum_db;
        self.sum_sq_db += o.sum_sq_db;
        self.occupied_weight_s += o.occupied_weight_s;
        let w = self.weight_s + o.weight_s;
        if w > 0.0 {
            self.fco_var_s = (self.fco_var_s * self.weight_s + o.fco_var_s * o.weight_s) / w;
        }
        self.weight_s = w;
        self.max_db = self.max_db.max(o.max_db);
    }

    /// Mean level, dB.
    pub fn mean_db(&self) -> Option<f64> {
        (self.n_visits > 0).then(|| self.sum_db / self.n_visits as f64)
    }

    /// Sample standard deviation of the level, dB.
    pub fn std_db(&self) -> Option<f64> {
        if self.n_visits < 2 {
            return None;
        }
        let n = self.n_visits as f64;
        let mean = self.sum_db / n;
        Some(
            ((self.sum_sq_db - n * mean * mean) / (n - 1.0))
                .max(0.0)
                .sqrt(),
        )
    }

    /// Time-weighted occupancy fraction.
    pub fn fco(&self) -> Option<f64> {
        (self.weight_s > 0.0).then(|| self.occupied_weight_s / self.weight_s)
    }
}

/// Baseline adaptation (§3.4): a frozen reference plus a slowly adapting copy, and change points
/// when they diverge.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdaptationPolicy {
    /// Half-life of the adaptive copy, in observed days (default 14).
    pub half_life_days: f64,
    /// CUSUM slack, in reference σ (default 0.5).
    pub cusum_k_sigma: f64,
    /// CUSUM decision threshold, in reference σ (default 8).
    pub cusum_h_sigma: f64,
    /// Re-freeze the reference automatically this many days after an unanswered change point;
    /// `None` (default) = only on user request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_refreeze_days: Option<f64>,
}

impl Default for AdaptationPolicy {
    fn default() -> Self {
        Self {
            half_life_days: 14.0,
            cusum_k_sigma: 0.5,
            cusum_h_sigma: 8.0,
            auto_refreeze_days: None,
        }
    }
}

impl AdaptationPolicy {
    /// Checks ranges.
    pub fn validate(&self) -> Result<(), ValidationError> {
        ensure_in(self.half_life_days, 1.0, 365.0, "adaptation.half_life_days")?;
        ensure_in(self.cusum_k_sigma, 0.0, 5.0, "adaptation.cusum_k_sigma")?;
        ensure_in(self.cusum_h_sigma, 1.0, 50.0, "adaptation.cusum_h_sigma")?;
        if let Some(d) = self.auto_refreeze_days {
            ensure_in(d, 1.0, 365.0, "adaptation.auto_refreeze_days")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(day: i64, hour: i64) -> Timestamp {
        Timestamp::from_unix_nanos((day * 24 + hour) * 3_600_000_000_000)
    }

    #[test]
    fn hour_of_week_is_monday_based() {
        // 1970-01-01 (day 0) was a Thursday → Thursday 00:00 = 3·24.
        assert_eq!(HourOfWeek::of(at(0, 0), 0).index(), 72);
        // 1970-01-05 was a Monday.
        assert_eq!(HourOfWeek::of(at(4, 0), 0).index(), 0);
        assert_eq!(HourOfWeek::of(at(3, 23), 0).index(), 167, "Sunday 23:00");
        // UTC+2: Sunday 23:00 UTC is Monday 01:00 local.
        assert_eq!(HourOfWeek::of(at(3, 23), 120).index(), 1);
        // Before the epoch.
        assert_eq!(
            HourOfWeek::of(at(-1, 0), 0).index(),
            48,
            "1969-12-31 was a Wednesday"
        );
        let h = HourOfWeek::of(at(4, 13), 0);
        assert_eq!((h.hour_of_day(), h.day_part()), (13, 2));
        assert!(serde_json::from_str::<HourOfWeek>("168").is_err());
        assert_eq!(serde_json::from_str::<HourOfWeek>("5").unwrap().index(), 5);
    }

    #[test]
    fn maturity_uses_the_finest_pool_with_24_hours() {
        let day = MATURITY_MIN_OBSERVED_S;
        assert_eq!(
            Maturity::from_pools([3600.0, 7200.0, 20_000.0, day - 1.0]),
            Maturity::Immature {
                observed_s: day - 1.0
            }
        );
        assert_eq!(
            Maturity::from_pools([3600.0, 7200.0, 20_000.0, day]),
            Maturity::Mature {
                resolution: BaselineResolution::AllHours
            }
        );
        assert_eq!(
            Maturity::from_pools([3600.0, day, day, 10.0 * day]),
            Maturity::Mature {
                resolution: BaselineResolution::HourOfDay
            }
        );
        assert!(!Maturity::from_pools([0.0; 4]).is_mature());
    }

    #[test]
    fn slot_stats_merge_equals_sequential_adds() {
        let visits = [
            (-100.0, -99.0, false, 1.0),
            (-90.0, -80.0, true, 2.0),
            (-95.0, -94.0, false, 1.5),
        ];
        let mut all = SlotStats::EMPTY;
        let (mut a, mut b) = (SlotStats::EMPTY, SlotStats::EMPTY);
        for (i, (l, m, o, w)) in visits.iter().enumerate() {
            all.add(*l, *m, *o, *w, *w);
            if i == 0 { &mut a } else { &mut b }.add(*l, *m, *o, *w, *w);
        }
        a.merge(&b);
        assert_eq!(a, all);
        assert!((all.mean_db().unwrap() - (-95.0)).abs() < 1e-12);
        assert!((all.std_db().unwrap() - 5.0).abs() < 1e-12);
        assert!((all.fco().unwrap() - 2.0 / 4.5).abs() < 1e-12);
        assert_eq!(all.max_db, -80.0);
        let mut e = SlotStats::default();
        e.merge(&SlotStats::EMPTY);
        assert_eq!(e.mean_db(), None);
        assert_eq!(SlotStats::EMPTY.std_db(), None);
    }

    #[test]
    fn site_keys_and_records() {
        assert!(SiteKey::Site(SiteId::new()).accrues_baseline());
        assert!(!SiteKey::Mobile.accrues_baseline());
        assert_eq!(
            serde_json::to_value(SiteKey::Unassigned).unwrap()["kind"],
            "unassigned"
        );
        let t = Timestamp::from_unix_nanos(0);
        let mut s = SiteRecord {
            id: SiteId::new(),
            name: Some("home".into()),
            lat_deg: Some(51.5),
            lon_deg: None,
            radius_m: 250.0,
            utc_offset_min: 0,
            source: SiteSource::User,
            first_seen: t,
            last_seen: t,
            observed_s: 0.0,
        };
        assert_eq!(s.validate().unwrap_err().field, "lat_deg");
        s.lon_deg = Some(-0.1);
        s.validate().unwrap();
        SiteConfig::default().validate().unwrap();
        AdaptationPolicy::default().validate().unwrap();
    }
}
