//! Sample-rate-change trust test (clock harmonics, T-032). The scheduler's `RateChange` dwell
//! repeats a POI capture at the same centre and gains with another sample rate. A line locked to
//! the sample clock moves when the rate changes, either as `n × fs` (absolute) or as a fixed
//! fraction of `fs` from the LO (e.g. `fs/4`); a real emitter stays at its absolute frequency.
//!
//! Labels per emitter inside the common usable span: `stays` (same absolute frequency at the other
//! rate); `scales_with_rate` (found at the other rate with its LO offset scaled by the rate ratio);
//! `clock_harmonic` (gone at the other rate and within tolerance of `n × fs` of its own capture:
//! the harmonic of the other rate lands elsewhere); otherwise `not_reproduced` (→ `marginal`). An
//! absolute `n × fs` line cannot be followed to `n × fs'` because that is usually far outside the
//! span, hence the "gone and on a harmonic" rule. A real carrier can sit on a clock harmonic and
//! be bursty, so the flags are candidates only.

use hk_model::DetectionFlags;
use hk_model::detection::SpurReason;

use super::{CaptureResult, CaptureSide};

/// Rate-change settings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RateChangeConfig {
    /// Coverage and harmonic tolerance, Hz (10 kHz, as the retune test).
    pub frequency_tolerance_hz: f64,
}

impl Default for RateChangeConfig {
    fn default() -> Self {
        Self {
            frequency_tolerance_hz: 10e3,
        }
    }
}

/// Rate-change verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RateChangeLabel {
    /// Same absolute frequency at both rates: not locked to the sample clock.
    Stays,
    /// Found at the other rate with its offset from the centre scaled by the rate ratio.
    ScalesWithRate,
    /// Gone at the other rate and on a harmonic of its own capture's rate.
    ClockHarmonic,
    /// Gone at the other rate and not clock-locked (bursty, marginal).
    NotReproduced,
}

impl RateChangeLabel {
    /// Applies the verdict: clock-locked → `spur_candidate` (`clock-harmonic`); not reproduced →
    /// `marginal`.
    pub fn apply(self, flags: &mut DetectionFlags) {
        match self {
            RateChangeLabel::Stays => {}
            RateChangeLabel::ScalesWithRate | RateChangeLabel::ClockHarmonic => {
                flags.spur_candidate = true;
                if flags.spur_reason.is_none() {
                    flags.spur_reason = Some(SpurReason::ClockHarmonic);
                }
            }
            RateChangeLabel::NotReproduced => flags.marginal = true,
        }
    }
}

/// Why the comparison was not run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RateChangeSkip {
    /// The captures are not at the same centre (within one bin).
    CentreMismatch,
    /// The rates are not both positive and different.
    SameRate,
}

/// One classified emitter.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RateChangeRow {
    /// Capture (A = base, B = changed).
    pub capture: CaptureSide,
    /// Index into that capture's emitters.
    pub emitter: usize,
    /// Centre, Hz.
    pub f_center_hz: f64,
    /// Verdict.
    pub label: RateChangeLabel,
}

/// Rate-change result.
#[derive(Clone, Debug, PartialEq)]
pub struct RateChangeResult {
    /// Base rate, Hz.
    pub base_rate_hz: f64,
    /// Changed rate, Hz.
    pub rate_hz: f64,
    /// Common usable span, Hz.
    pub span_lo_hz: f64,
    /// See `span_lo_hz`.
    pub span_hi_hz: f64,
    /// Set when the test was not run (rows empty).
    pub skipped: Option<RateChangeSkip>,
    /// Rows for emitters of both captures inside the common span.
    pub rows: Vec<RateChangeRow>,
}

/// Classifies emitters of `base` (at `base_rate_hz`) and `changed` (at `rate_hz`), same centre.
pub fn rate_change(
    base: &CaptureResult,
    changed: &CaptureResult,
    base_rate_hz: f64,
    rate_hz: f64,
    cfg: &RateChangeConfig,
) -> RateChangeResult {
    let (ga, gb) = (&base.spectrum.geometry, &changed.spectrum.geometry);
    let span_lo_hz =
        (base.center_hz - ga.usable_half_hz).max(changed.center_hz - gb.usable_half_hz);
    let span_hi_hz =
        (base.center_hz + ga.usable_half_hz).min(changed.center_hz + gb.usable_half_hz);
    let mut result = RateChangeResult {
        base_rate_hz,
        rate_hz,
        span_lo_hz,
        span_hi_hz,
        skipped: None,
        rows: Vec::new(),
    };
    let bin = ga.bin_width_hz.min(gb.bin_width_hz);
    if (base.center_hz - changed.center_hz).abs() > bin {
        result.skipped = Some(RateChangeSkip::CentreMismatch);
        return result;
    }
    if !(base_rate_hz > 0.0 && rate_hz > 0.0 && (base_rate_hz - rate_hz).abs() > bin) {
        result.skipped = Some(RateChangeSkip::SameRate);
        return result;
    }
    let tol = cfg.frequency_tolerance_hz;
    let centre = base.center_hz;
    for (x, y, x_rate, y_rate, side) in [
        (base, changed, base_rate_hz, rate_hz, CaptureSide::A),
        (changed, base, rate_hz, base_rate_hz, CaptureSide::B),
    ] {
        let covered = |f: f64| {
            y.emitters
                .iter()
                .any(|c| c.f_lo_hz - tol <= f && f <= c.f_hi_hz + tol)
        };
        for (i, e) in x.emitters.iter().enumerate() {
            let f = e.f_center_hz;
            if !(span_lo_hz <= f && f <= span_hi_hz) {
                continue;
            }
            let n = (f / x_rate).round();
            let label = if covered(f) {
                RateChangeLabel::Stays
            } else if covered(centre + (f - centre) * y_rate / x_rate) {
                RateChangeLabel::ScalesWithRate
            } else if n >= 1.0 && (f - n * x_rate).abs() <= tol {
                RateChangeLabel::ClockHarmonic
            } else {
                RateChangeLabel::NotReproduced
            };
            result.rows.push(RateChangeRow {
                capture: side,
                emitter: i,
                f_center_hz: f,
                label,
            });
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EdgeRule;
    use crate::integrated::IntegratedSnapshot;
    use crate::rules::Geometry;
    use crate::trust::{CaptureEmitter, GainState};

    const BINS: usize = 4096;

    fn capture(center_hz: f64, rate_hz: f64, emitters: &[(f64, f64)]) -> CaptureResult {
        let geometry = Geometry::new(center_hz, rate_hz, BINS, 0.0, &EdgeRule::default());
        CaptureResult {
            center_hz,
            gain: GainState {
                lna_db: 24.0,
                vga_db: 20.0,
                amp_on: false,
            },
            quantisation_limited: false,
            clipped: false,
            spectrum: IntegratedSnapshot {
                geometry,
                span_s: 0.5,
                mean_psd: vec![1e-12; BINS],
                mean_floor: vec![1e-12; BINS],
                block_psd: vec![vec![1e-12; BINS]; 4],
            },
            emitters: emitters
                .iter()
                .map(|&(f, w)| CaptureEmitter {
                    f_lo_hz: f - w / 2.0,
                    f_hi_hz: f + w / 2.0,
                    f_center_hz: f,
                    bandwidth_hz: w,
                    peak_excess_dbfs: -60.0,
                    spur: false,
                    dc: false,
                    image: false,
                    edge: false,
                })
                .collect(),
        }
    }

    fn label(r: &RateChangeResult, side: CaptureSide, f: f64) -> RateChangeLabel {
        r.rows
            .iter()
            .find(|row| row.capture == side && (row.f_center_hz - f).abs() < 1.0)
            .unwrap_or_else(|| panic!("no row at {f} in {side:?}: {r:?}"))
            .label
    }

    #[test]
    fn clock_locked_lines_move_with_the_rate_and_real_emitters_stay() {
        // Centre 913 MHz. At 10 Msps: 910 MHz = 91 × fs, an fs/4 line at +2.5 MHz, a real 100 kHz
        // emitter at 914.3 MHz and a burst seen only once. At 8 Msps: 912 MHz = 114 × fs, the
        // fs/4 line at +2.0 MHz, the real emitter.
        let c = 913e6;
        let base = capture(
            c,
            10e6,
            &[
                (910.0e6, 5e3),
                (914.3e6, 100e3),
                (915.5e6, 5e3),
                (911.3e6, 50e3),
            ],
        );
        let changed = capture(c, 8e6, &[(912.0e6, 5e3), (914.3e6, 100e3), (915.0e6, 5e3)]);
        let r = rate_change(&base, &changed, 10e6, 8e6, &RateChangeConfig::default());
        assert_eq!(r.skipped, None);
        use CaptureSide::{A, B};
        use RateChangeLabel::*;
        assert_eq!(label(&r, A, 914.3e6), Stays);
        assert_eq!(label(&r, B, 914.3e6), Stays);
        assert_eq!(label(&r, A, 910.0e6), ClockHarmonic);
        assert_eq!(label(&r, B, 912.0e6), ClockHarmonic);
        assert_eq!(label(&r, A, 915.5e6), ScalesWithRate);
        assert_eq!(label(&r, B, 915.0e6), ScalesWithRate);
        assert_eq!(label(&r, A, 911.3e6), NotReproduced);
        assert_eq!(r.rows.len(), 7);

        let mut flags = DetectionFlags::default();
        ClockHarmonic.apply(&mut flags);
        assert!(flags.spur_candidate && flags.spur_reason == Some(SpurReason::ClockHarmonic));
        let mut flags = DetectionFlags::default();
        Stays.apply(&mut flags);
        assert_eq!(flags, DetectionFlags::default());
    }

    #[test]
    fn mismatched_captures_are_not_compared() {
        let a = capture(913e6, 10e6, &[(914e6, 100e3)]);
        let moved = capture(914e6, 8e6, &[(914e6, 100e3)]);
        let cfg = RateChangeConfig::default();
        let r = rate_change(&a, &moved, 10e6, 8e6, &cfg);
        assert_eq!(
            (r.skipped, r.rows.len()),
            (Some(RateChangeSkip::CentreMismatch), 0)
        );
        let r = rate_change(&a, &a, 10e6, 10e6, &cfg);
        assert_eq!(r.skipped, Some(RateChangeSkip::SameRate));
    }
}
