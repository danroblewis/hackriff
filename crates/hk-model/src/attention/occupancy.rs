//! Occupancy statistics (ADR-0012 §2; ITU-R SM.1880-2, Report SM.2256-1; docs/04 §3.9).
//!
//! - **FCO** (frequency channel occupancy) = occupied revisits / revisits; a revisit is occupied
//!   when *any* sample in the channel exceeds the threshold. Time-weighted per §2.5.
//! - **FBO** (frequency band occupancy) = fraction of all (cell, revisit) samples above threshold.
//! - **SRO** (spectrum resource occupancy) = FCO averaged over the band's channels.
//!
//! Channels are **learned from detections** (blind-first, §2.7): a [`ChannelKey`] is a cell range
//! on the history level-0 grid. A band raster from C17 is only ever a [`RasterHint`] suggestion.

use serde::{Deserialize, Serialize};

use super::baseline::SiteKey;
use super::{ValidationError, ensure, ensure_in, ensure_opt_in, ensure_schema};
use crate::frames::PowerUnit;
use crate::ids::CalibrationStateId;
use crate::region::{FreqRange, TimeRange};
use crate::time::Timestamp;

/// Smallest guard above the noise floor any applied threshold may have, dB (SM.2256: ≥ 3–5 dB).
pub const MIN_GUARD_DB: f64 = 3.0;

/// How the occupancy threshold is set.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "method", deny_unknown_fields)]
pub enum ThresholdMethod {
    /// Pre-set: receiver sensitivity plus the service's required S/N, in the stats unit (dB per
    /// RBW bin for the full-OBW emission; the RBW correction applies).
    PreSet {
        /// Threshold level, dB.
        level_db: f64,
    },
    /// Dynamic: noise measured from idle samples ("80 % method", SM.1753/SM.2256: the highest
    /// `idle_fraction` of samples is discarded and the rest linearly averaged), then `guard_db`
    /// above it.
    Dynamic {
        /// Share of the highest samples **discarded** as possibly occupied, (0, 1); 0.8 by
        /// default (the lowest 20 % make the floor).
        idle_fraction: f64,
    },
    /// Interim stand-in, not an SM.1880 method: the spectrum-history pyramid's tile occupancy
    /// (fraction of frame time above the tile floor + `margin_db`), read per grid row. Rows built
    /// this way carry no activity-independent `fco` (ADR-0012 §2.5).
    HistoryTile {
        /// The pyramid's occupancy margin above its floor, dB.
        margin_db: f64,
    },
}

/// Where the floor under an occupancy threshold came from (T-118).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FloorSource {
    /// The history's bias-corrected cell floor (T-116), local to the subject's frequency.
    History,
    /// The SM.1753/SM.2256 80 % method over level samples, local to the subject's frequency.
    EightyPercent,
    /// No floor measured: a pre-set threshold assumed a guard above noise.
    Assumed,
}

/// Threshold configuration of an occupancy computation.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThresholdSpec {
    /// Method.
    pub method: ThresholdMethod,
    /// Guard above the floor for the dynamic method, dB, in `[MIN_GUARD_DB, 20]`.
    pub guard_db: f64,
    /// Apply the RBW < OBW correction.
    pub rbw_correction: bool,
}

impl Default for ThresholdSpec {
    /// Dynamic 80 % method, 5 dB guard, RBW correction on.
    fn default() -> Self {
        Self {
            method: ThresholdMethod::Dynamic { idle_fraction: 0.8 },
            guard_db: 5.0,
            rbw_correction: true,
        }
    }
}

impl ThresholdSpec {
    /// Checks guard range and method parameters.
    pub fn validate(&self) -> Result<(), ValidationError> {
        ensure_in(self.guard_db, MIN_GUARD_DB, 20.0, "threshold.guard_db")?;
        match self.method {
            ThresholdMethod::PreSet { level_db } => {
                ensure_in(level_db, -300.0, 100.0, "threshold.level_db")
            }
            ThresholdMethod::Dynamic { idle_fraction } => ensure(
                idle_fraction.is_finite() && idle_fraction > 0.0 && idle_fraction < 1.0,
                "threshold.idle_fraction",
                "must be in (0, 1)",
            ),
            ThresholdMethod::HistoryTile { margin_db } => {
                ensure_in(margin_db, 0.0, 60.0, "threshold.margin_db")
            }
        }
    }

    /// The threshold applied to one RBW bin, dB, and whether the minimum guard clamped it.
    ///
    /// Base = `level_db` (pre-set) or `floor_db + guard_db` (dynamic). With the correction on and
    /// `rbw_hz < obw_hz`, base is lowered by `10·log10(obw/rbw)`; the result is never below
    /// `floor_db + MIN_GUARD_DB` (phantom occupancy guard).
    pub fn applied_db(&self, floor_db: f64, obw_hz: f64, rbw_hz: f64) -> (f64, bool) {
        let base = match self.method {
            ThresholdMethod::PreSet { level_db } => level_db,
            ThresholdMethod::Dynamic { .. } => floor_db + self.guard_db,
            ThresholdMethod::HistoryTile { margin_db } => floor_db + margin_db,
        };
        let corrected = if self.rbw_correction {
            base - rbw_correction_db(obw_hz, rbw_hz)
        } else {
            base
        };
        let min = floor_db + MIN_GUARD_DB;
        if corrected < min {
            (min, true)
        } else {
            (corrected, false)
        }
    }
}

/// The SM.1880 resolution correction: `10·log10(OBW/RBW)` dB when `RBW < OBW`, else 0.
pub fn rbw_correction_db(obw_hz: f64, rbw_hz: f64) -> f64 {
    if rbw_hz > 0.0 && obw_hz > rbw_hz {
        10.0 * (obw_hz / rbw_hz).log10()
    } else {
        0.0
    }
}

/// Confidence level of an interval.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConfidenceLevel {
    /// 90 %.
    P90,
    /// 95 % (default).
    P95,
    /// 99 %.
    P99,
}

impl ConfidenceLevel {
    /// Two-sided standard-normal quantile.
    pub fn z(self) -> f64 {
        match self {
            ConfidenceLevel::P90 => 1.644_853_626_951_472_2,
            ConfidenceLevel::P95 => 1.959_963_984_540_054,
            ConfidenceLevel::P99 => 2.575_829_303_548_900_4,
        }
    }
}

/// A confidence interval on a fraction (ADR-0012 §2.4).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfidenceInterval {
    /// Lower bound, 0–1.
    pub lo: f64,
    /// Upper bound, 0–1.
    pub hi: f64,
    /// Level.
    pub level: ConfidenceLevel,
    /// Effective independent samples used.
    pub n_eff: f64,
    /// No correlation time was known, so samples were assumed independent (the interval may be
    /// too narrow when revisits are faster than the on/off process).
    pub independence_assumed: bool,
}

/// Effective independent sample count for `n` revisits at mean interval `revisit_s` of a
/// two-state on/off process with correlation time `corr_time_s` (= T_on·T_off/(T_on+T_off)).
///
/// With lag-one correlation ρ = exp(−revisit/τ_c), the variance of the mean inflates by
/// (1+ρ)/(1−ρ), so `n_eff = n·(1−ρ)/(1+ρ)`, at least 1 when `n ≥ 1`. Unknown τ_c → `n`, flagged.
pub fn effective_samples(n: u64, revisit_s: Option<f64>, corr_time_s: Option<f64>) -> (f64, bool) {
    let n = n as f64;
    match (revisit_s, corr_time_s) {
        (Some(r), Some(tc)) if r.is_finite() && tc.is_finite() && r >= 0.0 && tc > 0.0 => {
            let rho = (-r / tc).exp();
            ((n * (1.0 - rho) / (1.0 + rho)).max(n.min(1.0)), false)
        }
        _ => (n, true),
    }
}

/// Wilson score interval for a fraction `p_hat` from `n_eff` effective samples (SM.2256 Annex 1
/// treats the estimate as approximately normal; Wilson is that approximation made well-behaved at
/// 0 and 1). `None` when `n_eff < 1`.
pub fn fraction_interval(
    p_hat: f64,
    n_eff: f64,
    level: ConfidenceLevel,
    independence_assumed: bool,
) -> Option<ConfidenceInterval> {
    if n_eff.is_nan() || n_eff < 1.0 || !(0.0..=1.0).contains(&p_hat) {
        return None;
    }
    let z = level.z();
    let z2 = z * z;
    let denom = 1.0 + z2 / n_eff;
    let centre = (p_hat + z2 / (2.0 * n_eff)) / denom;
    let half = z * (p_hat * (1.0 - p_hat) / n_eff + z2 / (4.0 * n_eff * n_eff)).sqrt() / denom;
    // The Wilson bound is exactly 0 at p̂ = 0 and 1 at p̂ = 1; pin them against float rounding
    // (T-147: an always-on channel's interval read hi 0.9999999999999999, excluding FCO 1).
    Some(ConfidenceInterval {
        lo: if p_hat == 0.0 {
            0.0
        } else {
            (centre - half).max(0.0)
        },
        hi: if p_hat == 1.0 {
            1.0
        } else {
            (centre + half).min(1.0)
        },
        level,
        n_eff,
        independence_assumed,
    })
}

/// Whether every transmission could have been captured (SM.1880 timing rule).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TimingRegime {
    /// Max revisit ≤ ½ the minimum on/off time: complete capture.
    Complete,
    /// Otherwise: a statistical estimate, read with its confidence interval.
    Statistical,
    /// On/off times unknown.
    Unknown,
}

impl TimingRegime {
    /// Classifies a max revisit interval against the minimum on/off time.
    pub fn classify(revisit_max_s: Option<f64>, min_on_off_s: Option<f64>) -> Self {
        match (revisit_max_s, min_on_off_s) {
            (Some(r), Some(m)) if r <= m / 2.0 => TimingRegime::Complete,
            (Some(_), Some(_)) => TimingRegime::Statistical,
            _ => TimingRegime::Unknown,
        }
    }
}

/// A learned channel: a cell range `[lo_cell, hi_cell)` on the history level-0 frequency grid of
/// pyramid scheme `scheme` (ADR-0012 §2.7). Deterministic and registry-free: the same learned
/// extent always has the same key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelKey {
    /// History pyramid scheme whose level-0 grid is used.
    pub scheme: u16,
    /// First cell index.
    pub lo_cell: i64,
    /// One past the last cell index (`hi_cell > lo_cell`).
    pub hi_cell: i64,
}

impl ChannelKey {
    /// Snaps `freq` outward to whole cells of width `f_cell_hz`.
    pub fn snap(scheme: u16, f_cell_hz: f64, freq: FreqRange) -> Option<Self> {
        if f_cell_hz.is_nan() || f_cell_hz <= 0.0 || freq.hi_hz.is_nan() || freq.hi_hz <= freq.lo_hz
        {
            return None;
        }
        let lo_cell = (freq.lo_hz / f_cell_hz).floor() as i64;
        let hi_cell = ((freq.hi_hz / f_cell_hz).ceil() as i64).max(lo_cell + 1);
        Some(Self {
            scheme,
            lo_cell,
            hi_cell,
        })
    }

    /// The channel's extent, Hz.
    pub fn freq(&self, f_cell_hz: f64) -> FreqRange {
        FreqRange::new(
            self.lo_cell as f64 * f_cell_hz,
            self.hi_cell as f64 * f_cell_hz,
        )
    }
}

/// Where a channel came from. There is no "band plan" source: plans only suggest (§2.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChannelSource {
    /// Learned from blind detections/tracks/emitters.
    Learned,
    /// Drawn by the user (a Selection).
    User,
}

/// A raster the learned channel happens to match: a suggestion from C17, never the definition.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RasterHint {
    /// Channel spacing, Hz.
    pub spacing_hz: f64,
    /// Learned centre minus the nearest raster centre, Hz (non-zero offsets are interesting).
    pub offset_hz: f64,
    /// Which prior suggested it, e.g. `band-table@2`.
    pub source: String,
}

/// One entry of the learned channel plan.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Channel {
    /// Key.
    pub key: ChannelKey,
    /// Source.
    pub source: ChannelSource,
    /// Channel plan version that introduced this extent (merges/splits bump the version).
    pub plan_version: u32,
    /// When first learned.
    pub first_learned: Timestamp,
    /// Tracks/emitters that support it.
    pub evidence: u64,
    /// Occupied bandwidth used for the RBW correction, Hz.
    pub obw_hz: f64,
    /// Matching raster suggestion, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raster_hint: Option<RasterHint>,
}

/// The learning evidence behind one published channel, persisted with the plan so a restart keeps
/// its publication and host state (T-129, ADR-0012 §2.7). Additive: a plan without it restores
/// its channels as confident.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelEvidence {
    /// The channel it belongs to.
    pub key: ChannelKey,
    /// Median centre of the sample window, Hz.
    pub center_hz: f64,
    /// Median SNR of the window's non-fragment detections, dB (none when all were fragments).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snr_db: Option<f64>,
    /// Non-fragment detections.
    pub clean: u64,
    /// Separated intervals with a detection at the stable centre.
    pub intervals: u32,
    /// The newest of those intervals (interval indices).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_intervals: Vec<i64>,
    /// Cumulative detected duration at the stable centre, ns.
    pub detected_ns: i64,
}

/// What an [`OccupancyStat`] is about.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum OccupancySubject {
    /// One learned channel (FCO).
    Channel {
        /// Key.
        key: ChannelKey,
    },
    /// A band (FBO over cells, SRO over its channels).
    Band {
        /// Extent.
        freq: FreqRange,
    },
}

/// Occupancy of one channel or band over one interval (ADR-0012 §2).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OccupancyStat {
    /// [`super::ATTENTION_SCHEMA_VERSION`].
    pub schema: u32,
    /// Site key.
    pub site: SiteKey,
    /// Channel or band.
    pub subject: OccupancySubject,
    /// Interval (default 15 min; rolled up to 1 h).
    pub interval: TimeRange,
    /// FCO from activity-independent revisits, suspect revisits excluded, time-weighted. `None`
    /// with no usable revisit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fco: Option<f64>,
    /// FCO from all revisits, stratified per interval so activity-driven dwells do not dominate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fco_all_visits: Option<f64>,
    /// Upper bound counting suspect revisits as occupied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fco_suspect_upper: Option<f64>,
    /// FBO (band subjects; for a channel, the fraction of its cell samples).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fbo: Option<f64>,
    /// SRO (band subjects only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sro: Option<f64>,
    /// Activity-independent revisits considered.
    pub n_revisits: u64,
    /// Of those, occupied and not suspect.
    pub n_occupied: u64,
    /// Of those, suspect (IMD/spur/image/clipped/overload): excluded from `fco`.
    pub n_suspect: u64,
    /// All revisits, any tier.
    pub n_revisits_all: u64,
    /// Observed seconds (all tiers).
    pub observed_s: f64,
    /// Longest revisit gap, s.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revisit_max_s: Option<f64>,
    /// Mean revisit interval, s.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revisit_mean_s: Option<f64>,
    /// Complete vs statistical capture.
    pub timing: TimingRegime,
    /// Threshold configuration.
    pub threshold: ThresholdSpec,
    /// Representative applied threshold (median over revisits), dB.
    pub threshold_db: f64,
    /// The minimum guard clamped the threshold on some revisits.
    pub guard_clamped: bool,
    /// RBW of the samples, Hz.
    pub rbw_hz: f64,
    /// Occupied bandwidth used for the correction, Hz.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub obw_hz: Option<f64>,
    /// Unit of `threshold_db`.
    pub unit: PowerUnit,
    /// Calibration in force.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calibration: Option<CalibrationStateId>,
    /// Interval on `fco`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<ConfidenceInterval>,
    /// Too few activity-independent revisits: `fco` fell back to `fco_all_visits`.
    pub revisit_biased: bool,
    /// The window `fco`, its counts and `confidence` were computed over (§2.5 "widen, never
    /// substitute"; additive, T-118). Equals `interval` unless that held fewer than 30
    /// activity-independent visits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fco_window: Option<TimeRange>,
    /// Representative noise floor under the threshold (median over the window's visits), in
    /// `unit` (additive, T-118).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub floor_db: Option<f64>,
    /// Where `floor_db` came from (most common over the window's visits).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub floor_source: Option<FloorSource>,
    /// The local floor may be signal rather than noise (dense band: most of the neighbourhood is
    /// occupied), so `fco` may be underestimated there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub floor_suspect: Option<bool>,
    /// Median visit level (highest cell) of the `fco` visits that were occupied, in `unit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level_occupied_p50_db: Option<f64>,
    /// 90th percentile of the same levels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level_occupied_p90_db: Option<f64>,
    /// Median visit level of the `fco` visits that were idle, in `unit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level_idle_db: Option<f64>,
}

impl OccupancyStat {
    /// Checks schema, fraction ranges, count consistency and that the interval contains `fco`.
    pub fn validate(&self) -> Result<(), ValidationError> {
        ensure_schema(self.schema)?;
        ensure(
            self.interval.end > self.interval.start,
            "interval",
            "must be positive",
        )?;
        ensure_opt_in(self.fco, 0.0, 1.0, "fco")?;
        ensure_opt_in(self.fco_all_visits, 0.0, 1.0, "fco_all_visits")?;
        ensure_opt_in(self.fco_suspect_upper, 0.0, 1.0, "fco_suspect_upper")?;
        ensure_opt_in(self.fbo, 0.0, 1.0, "fbo")?;
        ensure_opt_in(self.sro, 0.0, 1.0, "sro")?;
        ensure(
            self.n_occupied + self.n_suspect <= self.n_revisits,
            "n_occupied",
            "n_occupied + n_suspect must not exceed n_revisits",
        )?;
        ensure(
            self.n_revisits <= self.n_revisits_all,
            "n_revisits",
            "must not exceed n_revisits_all",
        )?;
        ensure_in(self.observed_s, 0.0, f64::MAX, "observed_s")?;
        ensure_opt_in(self.revisit_max_s, 0.0, f64::MAX, "revisit_max_s")?;
        ensure_opt_in(self.revisit_mean_s, 0.0, f64::MAX, "revisit_mean_s")?;
        ensure_in(self.threshold_db, -300.0, 100.0, "threshold_db")?;
        ensure_in(self.rbw_hz, f64::MIN_POSITIVE, 1e12, "rbw_hz")?;
        self.threshold.validate()?;
        if matches!(self.subject, OccupancySubject::Channel { .. }) {
            ensure(self.sro.is_none(), "sro", "only for band subjects")?;
        }
        if let (Some(ci), Some(f)) = (self.confidence, self.fco) {
            ensure(
                ci.lo <= f + 1e-9 && f <= ci.hi + 1e-9 && ci.lo >= 0.0 && ci.hi <= 1.0,
                "confidence",
                "must contain fco",
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::SiteId;

    #[test]
    fn rbw_correction_follows_sm1880() {
        assert_eq!(
            rbw_correction_db(12_500.0, 25_000.0),
            0.0,
            "RBW ≥ OBW: none"
        );
        assert!((rbw_correction_db(25_000.0, 6_250.0) - 6.0206).abs() < 1e-3);
        let spec = ThresholdSpec::default();
        // Dynamic: floor −100 + 5 guard − 6.02 correction → clamped at floor + 3.
        let (db, clamped) = spec.applied_db(-100.0, 25_000.0, 6_250.0);
        assert!(clamped);
        assert_eq!(db, -97.0);
        let (db, clamped) = spec.applied_db(-100.0, 6_250.0, 6_250.0);
        assert!(!clamped);
        assert_eq!(db, -95.0);
        let pre = ThresholdSpec {
            method: ThresholdMethod::PreSet { level_db: -80.0 },
            ..spec
        };
        let (db, clamped) = pre.applied_db(-100.0, 25_000.0, 6_250.0);
        assert!(!clamped);
        assert!((db - (-86.0206)).abs() < 1e-3);
    }

    #[test]
    fn threshold_guard_is_enforced() {
        ThresholdSpec::default().validate().unwrap();
        let low = ThresholdSpec {
            guard_db: 2.0,
            ..ThresholdSpec::default()
        };
        assert_eq!(low.validate().unwrap_err().field, "threshold.guard_db");
        let bad = ThresholdSpec {
            method: ThresholdMethod::Dynamic { idle_fraction: 1.0 },
            ..ThresholdSpec::default()
        };
        assert!(bad.validate().is_err());
        let json = serde_json::json!({"method": "dynamic", "idle_fraction": 0.8, "guard_db": 5.0, "rbw_correction": true, "x": 1});
        assert!(serde_json::from_value::<ThresholdSpec>(json).is_err());
    }

    #[test]
    fn interval_contains_estimate_and_shrinks_with_samples() {
        let a = fraction_interval(0.1, 100.0, ConfidenceLevel::P95, true).unwrap();
        let b = fraction_interval(0.1, 10_000.0, ConfidenceLevel::P95, true).unwrap();
        assert!(a.lo < 0.1 && 0.1 < a.hi);
        assert!(b.hi - b.lo < (a.hi - a.lo) / 5.0);
        let zero = fraction_interval(0.0, 50.0, ConfidenceLevel::P95, true).unwrap();
        assert_eq!(zero.lo, 0.0);
        assert!(
            zero.hi > 0.0,
            "Wilson keeps a non-zero upper bound at p = 0"
        );
        // T-147: the bound at p̂ = 1 is exactly 1, so the interval holds an always-on truth.
        for n in [13.0, 50.0, 950.0] {
            let one = fraction_interval(1.0, n, ConfidenceLevel::P95, false).unwrap();
            assert_eq!(one.hi, 1.0, "n_eff {n}: {one:?}");
            assert!(one.lo < 1.0);
        }
        assert!(fraction_interval(0.5, 0.5, ConfidenceLevel::P95, true).is_none());
    }

    #[test]
    fn correlated_revisits_reduce_effective_samples() {
        let (n, assumed) = effective_samples(1000, None, None);
        assert_eq!((n, assumed), (1000.0, true));
        let (fast, _) = effective_samples(1000, Some(1.0), Some(100.0));
        let (slow, _) = effective_samples(1000, Some(1000.0), Some(100.0));
        assert!(
            fast < 10.0,
            "revisits much faster than τc are nearly redundant"
        );
        assert!(slow > 999.0);
        assert!(effective_samples(1, Some(0.0), Some(1.0)).0 >= 1.0);
    }

    #[test]
    fn timing_rule() {
        assert_eq!(
            TimingRegime::classify(Some(5.0), Some(10.0)),
            TimingRegime::Complete
        );
        assert_eq!(
            TimingRegime::classify(Some(6.0), Some(10.0)),
            TimingRegime::Statistical
        );
        assert_eq!(
            TimingRegime::classify(Some(6.0), None),
            TimingRegime::Unknown
        );
    }

    #[test]
    fn channel_keys_snap_outward() {
        let k = ChannelKey::snap(1, 6250.0, FreqRange::new(433_910_000.0, 433_930_000.0)).unwrap();
        assert!(k.freq(6250.0).lo_hz <= 433_910_000.0);
        assert!(k.freq(6250.0).hi_hz >= 433_930_000.0);
        assert_eq!(
            k,
            ChannelKey::snap(1, 6250.0, FreqRange::new(433_910_001.0, 433_929_999.0)).unwrap()
        );
        assert!(ChannelKey::snap(1, 0.0, FreqRange::new(1.0, 2.0)).is_none());
    }

    #[test]
    fn stat_validation() {
        let fco = 0.25;
        let ci = fraction_interval(fco, 40.0, ConfidenceLevel::P95, false).unwrap();
        let mut s = OccupancyStat {
            schema: 1,
            site: SiteKey::Site(SiteId::new()),
            subject: OccupancySubject::Channel {
                key: ChannelKey {
                    scheme: 1,
                    lo_cell: 10,
                    hi_cell: 14,
                },
            },
            interval: TimeRange::new(
                Timestamp::from_unix_nanos(0),
                Timestamp::from_unix_nanos(900_000_000_000),
            ),
            fco: Some(fco),
            fco_all_visits: Some(0.3),
            fco_suspect_upper: Some(0.3),
            fbo: Some(0.2),
            sro: None,
            n_revisits: 44,
            n_occupied: 10,
            n_suspect: 4,
            n_revisits_all: 60,
            observed_s: 120.0,
            revisit_max_s: Some(30.0),
            revisit_mean_s: Some(20.0),
            timing: TimingRegime::Statistical,
            threshold: ThresholdSpec::default(),
            threshold_db: -95.0,
            guard_clamped: false,
            rbw_hz: 6250.0,
            obw_hz: Some(25_000.0),
            unit: PowerUnit::Dbfs,
            calibration: None,
            confidence: Some(ci),
            revisit_biased: false,
            fco_window: None,
            floor_db: None,
            floor_source: None,
            floor_suspect: None,
            level_occupied_p50_db: None,
            level_occupied_p90_db: None,
            level_idle_db: None,
        };
        s.validate().unwrap();
        let json = serde_json::to_string(&s).unwrap();
        // Rows written before the level fields existed still read.
        assert!(!json.contains("floor_db") && !json.contains("level_idle_db"));
        assert_eq!(serde_json::from_str::<OccupancyStat>(&json).unwrap(), s);
        let mut lv = s.clone();
        lv.floor_db = Some(-110.0);
        lv.floor_source = Some(FloorSource::History);
        lv.floor_suspect = Some(false);
        lv.level_occupied_p50_db = Some(-80.0);
        lv.level_occupied_p90_db = Some(-75.0);
        lv.level_idle_db = Some(-108.0);
        let json = serde_json::to_string(&lv).unwrap();
        assert!(json.contains(r#""floor_source":"history""#), "{json}");
        assert_eq!(serde_json::from_str::<OccupancyStat>(&json).unwrap(), lv);
        s.n_suspect = 40;
        assert_eq!(s.validate().unwrap_err().field, "n_occupied");
        s.n_suspect = 4;
        s.sro = Some(0.1);
        assert_eq!(s.validate().unwrap_err().field, "sro");
        s.sro = None;
        s.fco = Some(0.9);
        assert_eq!(s.validate().unwrap_err().field, "confidence");
    }
}
