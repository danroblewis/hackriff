//! Noise-floor estimation (C08, T-005): per-frame and slow floors, per bin and per channel,
//! with uncertainty, occupancy, gain keying, quantisation and impulsive-frame state, and the
//! floor-referenced threshold helper T-006 builds on. Design input: spike S4 (§3.1, §3.2, §5).
//!
//! # Which floor to use
//!
//! | Output | Estimator | Use for | Never for |
//! |---|---|---|---|
//! | [`FloorFrame::wide_floor`] ([`FloorKind::Wide`]) | Per-frame FCME floor where it is within 1.5 dB of the sliding minimum of block floors over ±16 blocks; that minimum elsewhere | **The detection reference** (T-006 floor branch and OS guard). Per frame, so it absorbs impulsive frames (a static floor gave 61–4400× the design tail, S4 §3.2); unbiased on noise; keeps wide-signal interiors | Flat signals wider than ≈ 2270 bins at the defaults (raise `half_width_blocks`) |
//! | [`FloorFrame::floor`] ([`FloorKind::Frame`]) | Block FCME per [`SpectrumFrame`](crate::SpectrumFrame) | Local floor for per-channel SNR and occupancy of narrow signals | **Wide-signal interiors**: a flat signal wider than ≈ 200 of a 256-bin block reads as floor (+10.4/+20.0 dB inside 1024/2048-bin signals at +10/+20 dB; the floor branch then fires on 9–18 % of interior bins and the guard passes 11–21 %) |
//! | [`FloorFrame::slow_floor`] ([`FloorKind::Slow`]) | IIR (τ = [`FloorConfig::slow_time_constant_s`], 1 s) of block floors, skipping impulsive frames; re-seeded by gate releases and episodes | Science series (SPACE-050, C33), integrated (≥ 1 s) detection, floor-change episodes (AWARE-006) | Per-frame CFAR |
//! | [`FloorFrame::percentile`] | Block p20, Gamma-corrected | A cross-check; valid only at occupancy < 40 % | Dense bands (p50 never) |
//! | [`MinStatistics`] | Per-bin minimum statistics | Drift tracking; idle floors of intermittent channels | **Any CFAR reference**: continuous carriers read as floor (+4 to +14 dB in S4) |
//!
//! Tracker floors are per bin (blocks of 256 bins, hop 64, interpolated linearly in dB between
//! block centres), linear FS²/Hz like [`Spectrum::psd`](crate::Spectrum); per-channel values
//! come from [`FloorFrame::channel_floor`]; band scalars are block medians. **Edge bins:** the
//! outer ≈ 128 bins at each end hold the edge block's value
//! ([`BlockLayout::held_bins`]), which overestimates the floor across the baseband roll-off;
//! treat them as `edge`.
//!
//! # Formulas and constants
//!
//! Bins averaged over `K` segments are `Gamma(n)` with `n` = [`effective_averages`] (Welch
//! overlap correction; `n = K` without overlap). [`gamma`] holds the special functions.
//! - FCME ([`fcme`]): `T_CME = Q⁻¹(n, 1e-3)/n` (Gamma(10): 3.5521 dB), start from the 10 %
//!   smallest usable bins, at most 20 iterations, divide by `P(n+1, nT)/P(n, nT)` (0.998578).
//!   Non-finite and zero bins are excised; blocks with < 50 % usable bins are invalid and take
//!   the nearest valid block's value ([`FloorFrame::block_valid`]).
//! - Percentile ([`percentile`]): `q_0.2 / (P⁻¹(n, 0.2)/n)`.
//! - Occupancy: fraction of bins above `Q⁻¹(n, 1e-2)/n · wide_floor`, less the 1 % noise
//!   exceedance.
//! - Uncertainty: ±0.5 dB model uncertainty by default (S4: estimators agree within 0.3 dB on
//!   real captures) → SNR wall −6.4 dB ([`snr_wall_db`]; S4's "−9 dB" is ±0.25 dB). The
//!   statistical part is reported separately.
//! - Quantisation ([`quantisation`]): `quantisation_limited` when the band floor is within 3 dB of
//!   the receiver's low-gain floor (default: S4's HackRF One, −120.3 dBFS/Hz at 20 Msps; the ci8
//!   term scales with `fs`, the measured excess is held constant and is unverified at other
//!   rates). Maps to `hk_model::Provenance::quantisation_limited`.
//! - Thresholds ([`threshold`]): floor branch `P > T·wide_floor` with separate on/off `T`
//!   (Gamma(10): 1e-6 → 5.1469 dB, 1e-3 → 3.5521 dB); OS-branch guard default 3 dB, applied only
//!   to the OS branch.
//!
//! # State machine (per segment)
//!
//! 1. **Warm-up** (8 frames): the slow floor is the per-block median of the frames so far, so a
//!    start-up transient cannot seed it; the gate history fills with band floors.
//! 2. **Impulsive gate:** a frame is `impulsive` when its band floor (dB) exceeds the running
//!    median of the last 64 unflagged band floors by more than 0.5 dB. Impulsive frames update
//!    neither the slow floor nor the gate history; T-006 merges boxes inside them into one
//!    `impulsive` event. A flagged run lasting 0.1 s is a **level change**: the gate releases
//!    (`gate_released`), its history is re-seeded from the run, and blocks whose run mean is
//!    within the 3 dB change threshold adopt it as their slow floor. So any sustained step,
//!    including 0.5–3 dB steps that never make an episode, ends within 0.1 s.
//! 3. **Blocks:** within ±3 dB of the slow floor, the slow floor follows by IIR (non-impulsive
//!    frames). Beyond it, a block starts an up or down run; a run must persist for `confirm_s`
//!    (1 s) of consecutive frames to confirm, so wide bursty signals (LTE, WiFi, DVB bursts) that
//!    drop out within a second never confirm.
//! 4. **Rise → episode:** contiguous confirmed up-run blocks form (or extend an adjacent)
//!    episode. The discriminator is the run's frame-to-frame excess std (> 1.5 dB → structured)
//!    and the region's mean spectral kurtosis (within 0.15 of 1 → noise-like, else structured;
//!    no SK → unverified). Noise-like and unverified episodes re-seed the slow floor at the run
//!    level and emit [`FloorEvent`] `Rise` with the onset time (the first elevated frame), the
//!    confirmation time, the baseline (slow floor before, segment), level, step ± statistical
//!    uncertainty, SK, excess std, band fraction and receiver state. Structured episodes keep the
//!    slow floor at the baseline and are not emitted by default. Known limit: a continuous,
//!    steady, Gaussian-like wideband emission (e.g. an OFDM carrier) is indistinguishable from a
//!    noise rise by these tests.
//! 5. **End:** when the episode region's median `floor/baseline` stays below 1.5 dB for `end_s`
//!    (1 s), the slow floor is re-seeded at the returned level, `End` (reason `Returned`) is
//!    emitted with the episode duration, and the blocks enter a 2 s **hold-off** (no new runs; a
//!    rise inside the hold-off is reported with onset at its expiry). A segment reset ends every
//!    episode with reason `Reset`. T-020 opens an Anomaly `noise-floor-rise` on `Rise` and closes
//!    it on `End`.
//! 6. **Fall:** a down run outside an episode confirms after `confirm_s` and re-seeds the slow
//!    floor at the run's level. Because signals only add power, a down run survives frames back
//!    near the slow floor for up to `confirm_s` (e.g. a wide intermittent signal that was on
//!    during warm-up, which the IIR would otherwise keep following); such an interrupted fall is
//!    adopted silently (`FloorStats::floor_corrections`). A continuous fall emits `Fall`.
//!
//! # Gain state
//!
//! Every estimate is keyed by a [`GainKey`]: centre (tolerance 0.25 bin, so sub-bin frequency
//! corrections keep the series), rate, baseband bandwidth, LNA/VGA/amp, antenna port, FFT
//! length, overlap, K, window. A key change beyond tolerance, or a frame discontinuity in
//! [`FloorConfig::reset_on`] (stream start, rate change, gap), starts a new segment
//! ([`FloorFrame::segment`]).
//!
//! # Real-time path
//!
//! [`NoiseFloorTracker::update`] allocates nothing once the first frame of a given resolution has
//! been seen (tested under a counting allocator), including resets, gate releases and episodes.
//! Cost at 4096 bins: see `benches/floor_throughput.rs`.

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
pub use blocks::{BlockConfig, BlockLayout, fill_invalid, sliding_min};
pub use fcme::{BlockFcme, FcmeBlock, FcmeConfig, fcme_floor};
pub use minstat::{MinStatConfig, MinStatistics, monte_carlo_bias};
pub use percentile::{BlockPercentile, PercentileBlock, PercentileConfig};
pub use quantisation::{QuantisationFloor, QuantisationNoise, ci8_quantisation_noise};
pub use threshold::{DEFAULT_GUARD_DB, FloorThreshold, snr_wall_db};
pub use tracker::{
    ChannelFloor, EndReason, FLOOR_RESET_ON, FloorChangeClass, FloorChangeConfig, FloorConfig,
    FloorEvent, FloorEventKind, FloorFrame, FloorKind, FloorStats, GainKey, ImpulsiveGateConfig,
    NoiseFloorTracker, PercentileCheck, WideReferenceConfig,
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
