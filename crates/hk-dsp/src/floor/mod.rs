//! Noise-floor estimation (C08, T-005): per-frame and slow floors, per bin and per channel,
//! with uncertainty, occupancy, gain keying, quantisation and impulsive-frame state, and the
//! floor-referenced threshold helper T-006 builds on. Design input: spike S4 (§3.1, §3.2, §5).
//!
//! # Which floor to use
//!
//! | Output | Estimator | Use for | Never for |
//! |---|---|---|---|
//! | [`FloorFrame::floor`] per-frame floor | Block FCME per [`SpectrumFrame`](crate::SpectrumFrame) | **The detection reference** (C09 floor branch, OS guard). It absorbs impulsive frames, so it keeps the per-cell tail near design where a static floor gave 61–4400× (S4 §3.2) | — |
//! | [`FloorFrame::slow_floor`] slow floor | IIR (time constant [`FloorConfig::slow_time_constant_s`], default 1 s) of the block floors, skipping impulsive frames | Science series (SPACE-050, C33), integrated (≥ 1 s) detection, floor-rise events (AWARE-006) | Per-frame CFAR in impulsive environments |
//! | [`FloorFrame::percentile`] | Block p20, Gamma-corrected | A cross-check; valid only at occupancy < 40 % | Dense bands (p50 never) |
//! | [`MinStatistics`] | Per-bin minimum statistics | Drift tracking; idle floors of intermittent channels | **Any CFAR reference**: continuous carriers read as floor (+4 to +14 dB in S4) |
//!
//! Both tracker floors are per bin (blocks of 256 bins, hop 64, interpolated linearly in dB
//! between block centres), linear FS²/Hz like [`Spectrum::psd`](crate::Spectrum); per-channel
//! values come from [`FloorFrame::channel_floor`]. Band scalars are block medians.
//!
//! # Formulas and constants
//!
//! Bins averaged over `K` segments are `Gamma(n)` with `n` = [`effective_averages`] (Welch
//! overlap correction; `n = K` without overlap). [`gamma`] holds the special functions.
//! - FCME ([`fcme`]): `T_CME = Q⁻¹(n, 1e-3)/n` (Gamma(10): 3.55 dB), start from the 10 %
//!   smallest bins, iterate, divide by `P(n+1, nT)/P(n, nT)`.
//! - Percentile ([`percentile`]): `q_0.2 / (P⁻¹(n, 0.2)/n)`.
//! - Occupancy: fraction of bins above `Q⁻¹(n, 1e-2)/n · min(floor, band floor)`, less the 1 %
//!   noise exceedance.
//!
//! **Known limit (inherent to block estimation):** a block almost entirely covered by one wide
//! flat signal (≳ 200 bins of a 256-bin block) reads that signal as its floor. The band median
//! stays unbiased to 80 % occupancy, but on S4-style synthetic spectra (signals 5–200 bins) the
//! top 1 % of per-bin floor errors are +4 to +20 dB at 23–61 % occupancy. Wide-signal interiors
//! are the OS-CFAR/integrated detector's job (T-006); a narrower block trades this for noisier
//! estimates.
//! - Uncertainty: [`FloorFrame::uncertainty_db`] = ±0.5 dB model uncertainty by default (S4:
//!   estimators agree within 0.3 dB on real captures) → SNR wall ≈ −6.4 dB
//!   ([`snr_wall_db`]); the purely statistical part is reported separately.
//! - Quantisation ([`quantisation`]): `quantisation_limited` when the band floor is within 3 dB of
//!   the ADC floor (default S4's HackRF One: −120.3 dBFS/Hz at 20 Msps, scaled with `fs`). Maps to
//!   `hk_model::Provenance::quantisation_limited`.
//! - Impulsive gate: a frame is `impulsive` when the band median of `block floor / slow floor`
//!   rises more than 0.5 dB above its running median (last 64 unflagged frames). Flagged frames
//!   do not update the slow floor; T-006 merges boxes inside them into one `impulsive` event.
//! - Floor rise: when a run of blocks sits more than 3 dB above the slow floor for 5 consecutive
//!   frames, the slow floor of those blocks is re-seeded at the run's median and a
//!   [`FloorRiseEvent`] is emitted with the run's first frame time and the step (T-020 maps it to
//!   an Anomaly `noise-floor-rise`, docs/07 §2.18). Falls re-seed silently.
//! - Thresholds ([`threshold`]): floor branch `P > T·floor`, `T = Q⁻¹(n, pfa)/n` (Gamma(10): 1e-3
//!   → 3.55 dB, 1e-6 → 5.15 dB), guard margin default 3 dB.
//!
//! # Gain state
//!
//! Every estimate is keyed by a [`GainKey`] (tune, rate, LNA/VGA/amp, FFT length, overlap, K,
//! window). A key change or a frame discontinuity in [`FloorConfig::reset_on`] starts a new
//! segment ([`FloorFrame::segment`]): the slow floor is re-seeded, histories and runs cleared.
//!
//! # Real-time path
//!
//! [`NoiseFloorTracker::update`] allocates nothing once the first frame of a given resolution has
//! been seen (tested under a counting allocator), including segment resets.

pub mod averaging;
pub mod blocks;
pub mod fcme;
pub mod gamma;
pub mod minstat;
pub mod percentile;
pub mod quantisation;
pub mod threshold;
pub mod tracker;

use std::fmt;

pub use averaging::{AveragingModel, effective_averages};
pub use blocks::{BlockConfig, BlockLayout};
pub use fcme::{BlockFcme, FcmeBlock, FcmeConfig, fcme_floor};
pub use minstat::{MinStatConfig, MinStatistics, monte_carlo_bias};
pub use percentile::{BlockPercentile, PercentileBlock, PercentileConfig};
pub use quantisation::{QuantisationFloor, QuantisationNoise, ci8_quantisation_noise};
pub use threshold::{DEFAULT_GUARD_DB, FloorThreshold, snr_wall_db};
pub use tracker::{
    ChannelFloor, FloorConfig, FloorFrame, FloorKind, FloorRiseConfig, FloorRiseEvent, FloorStats,
    GainKey, ImpulsiveGateConfig, NoiseFloorTracker, PercentileCheck,
};

/// Which estimator produced a floor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FloorMethod {
    /// Block FCME (the tracker's floors).
    BlockFcme,
    /// Block percentile.
    BlockPercentile,
    /// Per-bin minimum statistics.
    MinStatistics,
}

impl FloorMethod {
    /// A short lowercase name.
    pub fn name(self) -> &'static str {
        match self {
            FloorMethod::BlockFcme => "block-fcme",
            FloorMethod::BlockPercentile => "block-percentile",
            FloorMethod::MinStatistics => "min-statistics",
        }
    }
}

/// Invalid floor-estimation settings.
#[derive(Clone, Debug, PartialEq)]
pub enum FloorConfigError {
    /// The spectrum has no bins.
    NoBins,
    /// Blocks must have at least 8 bins.
    BlockTooSmall(usize),
    /// Hop must be in `1..=block`.
    BadHop {
        /// Requested hop.
        hop: usize,
        /// Block size.
        block: usize,
    },
    /// A probability outside (0, 1).
    Probability {
        /// Setting name.
        name: &'static str,
        /// Value given.
        value: f64,
    },
    /// A fraction outside (0, 1).
    Fraction {
        /// Setting name.
        name: &'static str,
        /// Value given.
        value: f64,
    },
    /// A value that must be positive and finite.
    NonPositive {
        /// Setting name.
        name: &'static str,
        /// Value given.
        value: f64,
    },
    /// Minimum statistics needs `2 <= subwindows <= window_frames`.
    Subwindows {
        /// Sub-windows.
        subwindows: usize,
        /// Window, frames.
        window_frames: usize,
    },
}

impl fmt::Display for FloorConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FloorConfigError::NoBins => f.write_str("spectrum has no bins"),
            FloorConfigError::BlockTooSmall(n) => write!(f, "block of {n} bins is below 8"),
            FloorConfigError::BadHop { hop, block } => {
                write!(f, "hop {hop} must be in 1..={block}")
            }
            FloorConfigError::Probability { name, value } => {
                write!(f, "{name} = {value} is not a probability in (0, 1)")
            }
            FloorConfigError::Fraction { name, value } => {
                write!(f, "{name} = {value} is not in (0, 1)")
            }
            FloorConfigError::NonPositive { name, value } => {
                write!(f, "{name} = {value} must be positive and finite")
            }
            FloorConfigError::Subwindows {
                subwindows,
                window_frames,
            } => write!(
                f,
                "minimum statistics needs 2 <= subwindows ({subwindows}) <= window_frames ({window_frames})"
            ),
        }
    }
}

impl std::error::Error for FloorConfigError {}

fn check_probability(name: &'static str, value: f64) -> Result<(), FloorConfigError> {
    if value > 0.0 && value < 1.0 {
        Ok(())
    } else {
        Err(FloorConfigError::Probability { name, value })
    }
}

fn check_fraction(name: &'static str, value: f64) -> Result<(), FloorConfigError> {
    if value > 0.0 && value < 1.0 {
        Ok(())
    } else {
        Err(FloorConfigError::Fraction { name, value })
    }
}

fn check_positive(name: &'static str, value: f64) -> Result<(), FloorConfigError> {
    if value > 0.0 && value.is_finite() {
        Ok(())
    } else {
        Err(FloorConfigError::NonPositive { name, value })
    }
}

/// Occupied fraction from a count of bins above an occupancy threshold with per-bin noise
/// exceedance `pfa`: `(above/total − pfa)/(1 − pfa)`, clamped to [0, 1].
fn occupancy_from_count(above: usize, total: usize, pfa: f64) -> f64 {
    let frac = above as f64 / total.max(1) as f64;
    ((frac - pfa) / (1.0 - pfa)).clamp(0.0, 1.0)
}

/// Occupied fraction of `psd` against a per-bin `floor`: bins above `multiplier·floor`, corrected
/// for the noise exceedance `pfa` of that multiplier. Allocation-free.
pub fn occupancy_fraction(psd: &[f32], floor: &[f32], multiplier: f64, pfa: f64) -> f64 {
    assert_eq!(psd.len(), floor.len(), "psd/floor length mismatch");
    let m = multiplier as f32;
    let above = psd.iter().zip(floor).filter(|&(&p, &f)| p > m * f).count();
    occupancy_from_count(above, psd.len(), pfa)
}

/// Median of `v` (mean of the two middle values for even lengths); reorders `v`.
fn median_in_place(v: &mut [f32]) -> f32 {
    let n = v.len();
    assert!(n > 0, "median of nothing");
    let (lower, &mut mid, _) = v.select_nth_unstable_by(n / 2, f32::total_cmp);
    if n % 2 == 1 {
        mid
    } else {
        let below = lower.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        0.5 * (below + mid)
    }
}

/// `10·log10(a/b)` with both clamped away from zero.
#[inline]
fn db_ratio(a: f32, b: f32) -> f32 {
    10.0 * (a.max(1e-37) / b.max(1e-37)).log10()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(median_in_place(&mut [3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median_in_place(&mut [4.0, 1.0, 3.0, 2.0]), 2.5);
        assert_eq!(occupancy_from_count(1, 100, 0.01), 0.0);
        assert!((occupancy_fraction(&[1.0, 10.0], &[1.0, 1.0], 2.0, 0.0) - 0.5).abs() < 1e-12);
        assert!(check_probability("x", 1.0).is_err());
        assert!(check_positive("x", f64::NAN).is_err());
        assert_eq!(FloorMethod::BlockFcme.name(), "block-fcme");
        assert!((db_ratio(10.0, 1.0) - 10.0).abs() < 1e-5);
    }
}
