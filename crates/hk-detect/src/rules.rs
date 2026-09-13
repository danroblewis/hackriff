//! In-capture flag rules (S4 rules 1, 2, 3, 5 and the edge zone) as pure functions over a box's
//! frequency extent. Rule 4 (comb) is in [`crate::comb`]; rules 6–7 in [`crate::trust`].

use hk_model::detection::SpurReason;
use hk_model::{FreqRange, SpurMask, SpurRule};

use crate::config::{DcRule, EdgeRule, RefHarmonicRule, Rules};

/// Bin geometry of a segment.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Geometry {
    /// Tuned centre, Hz (bin `bins/2`).
    pub center_hz: f64,
    /// Sample rate, Hz.
    pub sample_rate_hz: f64,
    /// Bin spacing, Hz.
    pub bin_width_hz: f64,
    /// Bins.
    pub bins: usize,
    /// Usable half-span around the centre, Hz; beyond it is the edge zone.
    pub usable_half_hz: f64,
}

impl Geometry {
    /// Geometry for a DC-centred spectrum; `bandwidth_hz` is the baseband filter (0 if unknown).
    pub fn new(
        center_hz: f64,
        sample_rate_hz: f64,
        bins: usize,
        bandwidth_hz: f64,
        edge: &EdgeRule,
    ) -> Self {
        let df = sample_rate_hz / bins as f64;
        let by_filter = if bandwidth_hz > 0.0 && bandwidth_hz.is_finite() {
            bandwidth_hz / 2.0 * edge.bandwidth_factor
        } else {
            f64::INFINITY
        };
        let by_bins = sample_rate_hz / 2.0 - edge.guard_bins as f64 * df;
        Self {
            center_hz,
            sample_rate_hz,
            bin_width_hz: df,
            bins,
            usable_half_hz: by_filter.min(by_bins).max(df),
        }
    }

    /// Frequency of (fractional) bin `b` relative to the centre, Hz.
    #[inline]
    pub fn offset_hz(&self, b: f64) -> f64 {
        (b - (self.bins / 2) as f64) * self.bin_width_hz
    }

    /// Absolute frequency of (fractional) bin `b`, Hz.
    #[inline]
    pub fn bin_hz(&self, b: f64) -> f64 {
        self.center_hz + self.offset_hz(b)
    }

    /// Lower edge of bin `lo` and upper edge of bin `hi − 1`, Hz.
    pub fn extent_hz(&self, lo: usize, hi: usize) -> (f64, f64) {
        let half = self.bin_width_hz / 2.0;
        (
            self.bin_hz(lo as f64) - half,
            self.bin_hz(hi as f64 - 1.0) + half,
        )
    }

    /// First bin whose centre is at or above `f_hz` (numpy `searchsorted`), clamped to `bins`.
    pub fn bin_at_or_above(&self, f_hz: f64) -> usize {
        let b = ((f_hz - self.center_hz) / self.bin_width_hz + (self.bins / 2) as f64).ceil();
        b.clamp(0.0, self.bins as f64) as usize
    }
}

/// Rule 1: the harmonic `n × step` a narrow detection sits on, if any.
pub fn ref_harmonic(
    f_center_hz: f64,
    width_hz: f64,
    bin_width_hz: f64,
    rule: &RefHarmonicRule,
) -> Option<f64> {
    if bin_width_hz >= rule.max_bin_width_hz || width_hz > rule.max_width_hz {
        return None;
    }
    let n = (f_center_hz / rule.step_hz).round();
    if n < 1.0 {
        return None;
    }
    let h = n * rule.step_hz;
    let tol = rule.min_tolerance_hz.max(rule.ppm * 1e-6 * h);
    ((f_center_hz - h).abs() <= tol).then_some(h)
}

/// Rule 2: a narrow extent within `tolerance` of the tuned centre.
pub fn dc_hit(f_lo_hz: f64, f_hi_hz: f64, width_hz: f64, center_hz: f64, rule: &DcRule) -> bool {
    width_hz <= rule.max_width_hz
        && f_lo_hz - rule.tolerance_hz <= center_hz
        && center_hz <= f_hi_hz + rule.tolerance_hz
}

/// The extent reaches into the edge zone.
pub fn edge_hit(f_lo_hz: f64, f_hi_hz: f64, geometry: &Geometry) -> bool {
    f_lo_hz < geometry.center_hz - geometry.usable_half_hz
        || f_hi_hz > geometry.center_hz + geometry.usable_half_hz
}

/// Rule 3: a measured spur of `mask` overlaps the extent (and, when the spur is gain-specific, the
/// total gain is within 1 dB of the gain it was measured at).
pub fn spur_map_hit(f_lo_hz: f64, f_hi_hz: f64, total_gain_db: f64, mask: &SpurMask) -> bool {
    let extent = FreqRange::new(f_lo_hz, f_hi_hz);
    mask.rules.iter().any(|r| match r {
        SpurRule::Spur { freq, gain_db, .. } => {
            freq.overlaps(&extent) && gain_db.is_none_or(|g| (g - total_gain_db).abs() <= 1.0)
        }
        _ => false,
    })
}

/// The spur reason a box gets from rules 1–3, in precedence order ref-harmonic, DC, spur map.
/// (Comb, rule 4, needs the integrated line set and is applied by the caller.)
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SpurDecision {
    /// The reason, if any rule hit.
    pub reason: Option<SpurReason>,
    /// The reference harmonic for `ref-harmonic`, Hz.
    pub harmonic_hz: Option<f64>,
}

/// Applies rules 1–3 to an extent `[f_lo, f_hi]` with centroid `f_center` and narrow-width
/// measure `width_hz` (the x-dB bandwidth for boxes, the extent for integrated emitters).
#[allow(clippy::too_many_arguments)]
pub fn spur_decision(
    f_lo_hz: f64,
    f_hi_hz: f64,
    f_center_hz: f64,
    width_hz: f64,
    geometry: &Geometry,
    rules: &Rules,
    mask: Option<&SpurMask>,
    total_gain_db: f64,
) -> SpurDecision {
    if let Some(h) = ref_harmonic(
        f_center_hz,
        width_hz,
        geometry.bin_width_hz,
        &rules.ref_harmonic,
    ) {
        return SpurDecision {
            reason: Some(SpurReason::RefHarmonic),
            harmonic_hz: Some(h),
        };
    }
    if dc_hit(f_lo_hz, f_hi_hz, width_hz, geometry.center_hz, &rules.dc) {
        return SpurDecision {
            reason: Some(SpurReason::Dc),
            harmonic_hz: None,
        };
    }
    if let Some(m) = mask.filter(|m| spur_map_hit(f_lo_hz, f_hi_hz, total_gain_db, m)) {
        return SpurDecision {
            reason: Some(SpurReason::SpurMap { mask: m.id }),
            harmonic_hz: None,
        };
    }
    SpurDecision::default()
}

/// Pearson correlation from running sums; `None` when undefined.
#[derive(Clone, Copy, Debug, Default)]
pub struct Pearson {
    n: f64,
    sx: f64,
    sy: f64,
    sxx: f64,
    syy: f64,
    sxy: f64,
}

impl Pearson {
    /// Adds a pair.
    pub fn add(&mut self, x: f64, y: f64) {
        self.n += 1.0;
        self.sx += x;
        self.sy += y;
        self.sxx += x * x;
        self.syy += y * y;
        self.sxy += x * y;
    }

    /// Pairs added.
    pub fn count(&self) -> usize {
        self.n as usize
    }

    /// The correlation coefficient.
    pub fn value(&self) -> Option<f64> {
        if self.n < 2.0 {
            return None;
        }
        let cov = self.sxy - self.sx * self.sy / self.n;
        let vx = self.sxx - self.sx * self.sx / self.n;
        let vy = self.syy - self.sy * self.sy / self.n;
        let d = (vx * vy).sqrt();
        (d > 0.0 && d.is_finite()).then(|| cov / d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hk_model::{SpurMaskId, Timestamp};

    #[test]
    fn reference_harmonic_rule() {
        let r = RefHarmonicRule::default();
        assert_eq!(ref_harmonic(100.000_4e6, 19.5e3, 4.88e3, &r), Some(100e6));
        // 25 ppm of 900 MHz = 22.5 kHz beats the 10 kHz floor.
        assert_eq!(ref_harmonic(900.02e6, 10e3, 4.88e3, &r), Some(900e6));
        assert_eq!(ref_harmonic(100.3e6, 19.5e3, 4.88e3, &r), None);
        assert_eq!(ref_harmonic(100.0e6, 150e3, 4.88e3, &r), None, "wide");
        assert_eq!(ref_harmonic(100.0e6, 10e3, 455e3, &r), None, "sweep bins");
    }

    #[test]
    fn dc_edge_and_spur_map() {
        let dc = DcRule::default();
        assert!(dc_hit(97.99e6, 98.004e6, 14e3, 98e6, &dc));
        assert!(!dc_hit(98.02e6, 98.03e6, 10e3, 98e6, &dc));
        assert!(!dc_hit(97.9e6, 98.1e6, 200e3, 98e6, &dc));
        let g = Geometry::new(98e6, 20e6, 4096, 15e6, &EdgeRule::default());
        assert!((g.usable_half_hz - 8e6).abs() < 1.0);
        assert!(edge_hit(106.1e6, 106.2e6, &g));
        assert!(!edge_hit(105.8e6, 105.9e6, &g));
        assert_eq!(g.bin_at_or_above(98e6), 2048);
        let mask = SpurMask {
            id: SpurMaskId::new(),
            supersedes: None,
            device_id: "t".into(),
            measured_at: Timestamp::UNIX_EPOCH,
            rules: vec![SpurRule::Spur {
                freq: FreqRange::centered(91.1e6, 2e3),
                level_dbfs: -70.0,
                gain_db: Some(44.0),
            }],
        };
        assert!(spur_map_hit(91.09e6, 91.11e6, 44.5, &mask));
        assert!(!spur_map_hit(91.09e6, 91.11e6, 50.0, &mask));
        let mut p = Pearson::default();
        for i in 0..5 {
            p.add(i as f64, 2.0 * i as f64 + 1.0);
        }
        assert!((p.value().unwrap() - 1.0).abs() < 1e-12);
    }
}
