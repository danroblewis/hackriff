//! Occupancy thresholds (T-118, ADR-0012 §2.2–§2.3): pre-set and dynamic (80 % method) floors,
//! guard, RBW < OBW correction.
//!
//! **Floor sources, in order.**
//! 1. The history's bias-corrected `floor_db` (T-116 `CellStats::floor_db`), the median over the
//!    subject's observed cells. This is the C08/C26 floor the ADR allows for `dynamic`.
//! 2. The **80 % method** (Rec. ITU-R SM.1753 as restated in Report SM.2256-1 §"Calculated
//!    threshold", verified against the Report text): of all level samples, the highest 80 % are
//!    discarded and the remaining 20 % are **linearly averaged**; the result is the noise level.
//!    `ThresholdMethod::Dynamic { idle_fraction }` is that discarded share (0.8). The Report notes
//!    the method is only valid over a band or several equal-bandwidth channels (a busy single
//!    channel raises its own floor), so the engine pools the band's cells for it.
//!
//! The threshold is floor + guard (dynamic) or the pre-set level, lowered by 10·log10(OBW/RBW)
//! when RBW < OBW and clamped at floor + 3 dB (`ThresholdSpec::applied_db`, hk-model).

pub use hk_model::attention::occupancy::FloorSource;
use hk_model::attention::occupancy::{MIN_GUARD_DB, ThresholdMethod, ThresholdSpec};

/// A threshold ready to compare levels against.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AppliedThreshold {
    /// Noise floor, dB/Hz.
    pub floor_db: f64,
    /// Threshold, dB/Hz.
    pub threshold_db: f64,
    /// The RBW correction hit the floor + 3 dB clamp.
    pub guard_clamped: bool,
    /// Floor source.
    pub source: FloorSource,
}

/// The 80 % method: discards the highest `discard_fraction` of `levels_db` and returns the linear
/// (power) average of the rest, in dB. At least one sample is kept. `None` when no finite sample.
pub fn eighty_percent_floor_db(levels_db: &[f64], discard_fraction: f64) -> Option<f64> {
    let mut v: Vec<f64> = levels_db
        .iter()
        .copied()
        .filter(|x| x.is_finite())
        .collect();
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    let keep_frac = (1.0 - discard_fraction).clamp(0.0, 1.0);
    let keep = ((v.len() as f64 * keep_frac).round() as usize).clamp(1, v.len());
    let mean_lin = v[..keep].iter().map(|x| 10f64.powf(x / 10.0)).sum::<f64>() / keep as f64;
    Some(10.0 * mean_lin.log10())
}

/// Median of the finite values (`None` when there are none).
pub fn median_finite(values: impl IntoIterator<Item = f64>) -> Option<f64> {
    let mut v: Vec<f64> = values.into_iter().filter(|x| x.is_finite()).collect();
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    let m = v.len() / 2;
    Some(if v.len() % 2 == 1 {
        v[m]
    } else {
        0.5 * (v[m - 1] + v[m])
    })
}

/// Resolves `spec` to a threshold: floor from `history_floor_db` when finite, else the 80 % method
/// over `idle_levels_db` (the band's level samples); RBW correction and clamp per ADR §2.3.
/// `None` when a dynamic threshold has no floor at all.
pub fn resolve(
    spec: &ThresholdSpec,
    history_floor_db: Option<f64>,
    idle_levels_db: &[f64],
    obw_hz: f64,
    rbw_hz: f64,
) -> Option<AppliedThreshold> {
    let discard = match spec.method {
        ThresholdMethod::Dynamic { idle_fraction } => idle_fraction,
        ThresholdMethod::PreSet { .. } | ThresholdMethod::HistoryTile { .. } => 0.8,
    };
    let measured = match history_floor_db.filter(|f| f.is_finite()) {
        Some(f) => Some((f, FloorSource::History)),
        None => eighty_percent_floor_db(idle_levels_db, discard)
            .map(|f| (f, FloorSource::EightyPercent)),
    };
    let (floor_db, source) = match (measured, spec.method) {
        (Some(m), _) => m,
        (None, ThresholdMethod::PreSet { level_db }) => (
            level_db - spec.guard_db.max(MIN_GUARD_DB),
            FloorSource::Assumed,
        ),
        // T-121's report stand-in carries no floor of its own: no threshold without a measured one.
        (None, ThresholdMethod::Dynamic { .. } | ThresholdMethod::HistoryTile { .. }) => return None,
    };
    let (threshold_db, guard_clamped) = spec.applied_db(floor_db, obw_hz, rbw_hz);
    Some(AppliedThreshold {
        floor_db,
        threshold_db,
        guard_clamped,
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn occupancy_eighty_percent_averages_the_lowest_fifth_linearly() {
        // 8 noise samples at -100 dB and 2 at -97 dB (2x power), 40 busy samples far above.
        let mut v = vec![-100.0; 8];
        v.extend([-97.0, -97.0]);
        v.extend(std::iter::repeat_n(-60.0, 40));
        let f = eighty_percent_floor_db(&v, 0.8).unwrap();
        // Lowest 10 of 50: linear mean of 8×1 + 2×~2 = ~1.2 → -99.2 dB.
        let expect = 10.0 * ((8.0 + 2.0 * 10f64.powf(0.3)) / 10.0f64).log10() - 100.0;
        assert!((f - expect).abs() < 1e-9, "{f} vs {expect}");
        assert_eq!(eighty_percent_floor_db(&[], 0.8), None);
        assert_eq!(eighty_percent_floor_db(&[-50.0], 0.8), Some(-50.0));
    }

    #[test]
    fn occupancy_threshold_prefers_the_history_floor_and_applies_the_guard() {
        let spec = ThresholdSpec::default();
        let t = resolve(&spec, Some(-120.0), &[-10.0], 6250.0, 6250.0).unwrap();
        assert_eq!(t.source, FloorSource::History);
        assert!((t.threshold_db - -115.0).abs() < 1e-12 && !t.guard_clamped);
        let t = resolve(&spec, Some(f64::NAN), &[-130.0; 10], 6250.0, 6250.0).unwrap();
        assert_eq!(t.source, FloorSource::EightyPercent);
        assert!((t.threshold_db - -125.0).abs() < 1e-9);
        assert!(resolve(&spec, None, &[], 1.0, 1.0).is_none());
    }

    #[test]
    fn occupancy_rbw_correction_lowers_then_clamps_at_three_db() {
        let spec = ThresholdSpec {
            guard_db: 10.0,
            ..ThresholdSpec::default()
        };
        // OBW/RBW = 2 → 3.01 dB lower: floor + 6.99, not clamped.
        let t = resolve(&spec, Some(-100.0), &[], 12_500.0, 6_250.0).unwrap();
        assert!((t.threshold_db - (-90.0 - 10.0 * 2f64.log10())).abs() < 1e-9);
        assert!(!t.guard_clamped);
        // OBW/RBW = 16 → 12 dB lower would be floor - 2: clamped at floor + 3.
        let t = resolve(&spec, Some(-100.0), &[], 100_000.0, 6_250.0).unwrap();
        assert!((t.threshold_db - -97.0).abs() < 1e-12 && t.guard_clamped);
        // RBW ≥ OBW: no correction.
        let t = resolve(&spec, Some(-100.0), &[], 3_000.0, 6_250.0).unwrap();
        assert!((t.threshold_db - -90.0).abs() < 1e-12);
        // Pre-set with the correction off keeps its level.
        let pre = ThresholdSpec {
            method: ThresholdMethod::PreSet { level_db: -80.0 },
            guard_db: 5.0,
            rbw_correction: false,
        };
        let t = resolve(&pre, Some(-100.0), &[], 100_000.0, 6_250.0).unwrap();
        assert!((t.threshold_db - -80.0).abs() < 1e-12);
    }
}
