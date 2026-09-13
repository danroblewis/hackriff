//! Noise-floor estimation (C08, T-005): per-frame and slow floors, per bin and per channel,
//! with uncertainty, occupancy, gain keying, quantisation and impulsive-frame state, the
//! floor-referenced threshold helper T-006 builds on, and floor-change events for T-020. Design
//! input: spike S4 (§3.1, §3.2, §5); re-review fixes against 854614c.
//!
//! # Which floor to use
//!
//! | Output | Estimator | Use for | Never for |
//! |---|---|---|---|
//! | [`FloorFrame::wide_floor`] ([`FloorKind::Wide`]) | The per-frame floor, except inside **step-like** elevated regions (more than 3.5 dB over a slope-limited lower envelope of the block floors, ±16 blocks, slope 0.15 dB per block), where it reads the surrounding floor | **The detection reference** (T-006 floor branch and OS guard). Per frame, so it absorbs impulsive frames (a static floor gave 61–4400× the design tail, S4 §3.2); keeps wide-signal interiors (≥ 95 % of a 2048-bin +10 dB signal); floor-branch Pfa ≤ 1.5× design on tilts to 12 dB, baseband roll-off, notches and the real urban capture (`tests/floor_wide_reference.rs`) | Interior coverage falls with signal width (from ≈ 2000 bins at 4096) and is 0 % for signals wider than ≈ 55 % of the span or with soft (ramped) edges; dips wider than 40 % of the span read as a floor step; a band-pass accessory needs a calibrated response (≈ 900× design otherwise, permanently); a feature in its first ≈ 50 frames, before the shape learns it |
//! | [`FloorFrame::floor`] ([`FloorKind::Frame`]) | Block FCME per [`SpectrumFrame`](crate::SpectrumFrame), re-estimated on `psd / S` over blocks where the learned response shape `S` has features, interpolated and multiplied back by `S` | Local floor for per-channel SNR and occupancy of narrow signals | **Wide-signal interiors**: a flat signal wider than ≈ 200 of a 256-bin block reads as floor (+10.4/+20.0 dB inside 1024/2048-bin signals at +10/+20 dB; the floor branch then fires on 9–18 % of interior bins) |
//! | [`FloorFrame::slow_floor`] ([`FloorKind::Slow`]) | IIR (τ = [`FloorConfig::slow_time_constant_s`], 1 s) of the raw block floors behind a per-block gate (below), re-seeded only by adoptions and floor-change events | Science series (SPACE-050, C33, T-021), integrated (≥ 1 s) detection, floor-change episodes (AWARE-006) | Per-frame CFAR |
//! | [`FloorFrame::percentile`] | Block p20, Gamma-corrected | A cross-check; valid only at occupancy < 40 % | Dense bands (p50 never) |
//! | [`MinStatistics`] | Per-bin minimum statistics | Drift tracking; idle floors of intermittent channels | **Any CFAR reference**: continuous carriers read as floor (+4 to +14 dB in S4) |
//!
//! Tracker floors are per bin (blocks of 256 bins, hop 64, interpolated linearly in dB between
//! block centres), linear FS²/Hz like [`Spectrum::psd`](crate::Spectrum); per-channel values
//! come from [`FloorFrame::channel_floor`]; band scalars are block medians. **Edge bins:** the
//! outer ≈ 128 bins at each end hold the edge block's value ([`BlockLayout::held_bins`]); across
//! a roll-off the learned shape corrects them, but before it has learned they overestimate the
//! floor: treat them as `edge`.
//!
//! # Formulas and constants
//!
//! Bins averaged over `K` segments are `Gamma(n)` with `n` = [`effective_averages`] (Welch
//! overlap correction; `n = K` without overlap). [`gamma`] holds the special functions.
//! - FCME ([`fcme`]): `T_CME = Q⁻¹(n, 1e-3)/n` (Gamma(10): 3.5521 dB), start from the 10 %
//!   smallest usable bins, at most 20 iterations, divide by `P(n+1, nT)/P(n, nT)` (0.998578).
//!   Non-finite and zero bins are excised; blocks with < 50 % usable bins are invalid and take
//!   the nearest valid block's value ([`FloorFrame::block_valid`]).
//! - Response shape `S` ([`FloorFrame::shape`], per bin, ≤ 1): the floor's static downward
//!   features (baseband roll-off, notches, the low side of a tilt). Each bin tracks the 25 %
//!   quantile of `ln psd` over time (Robbins–Monro, τ = `wide.shape_time_constant_s`, 1 s), so a
//!   signal present in fewer than 75 % of frames barely moves it. The tracked level is opened
//!   (erosion then dilation over 65 bins, so bumps narrower than that vanish), referenced to the
//!   frame's FCME band floor (robust to 80 % occupancy), and mapped to 1 within a soft deadband
//!   of 0.5 dB plus four standard errors of the tracker (`5.9/√(frames·n)` dB). Signals only add
//!   power, so wide signals never enter `S`. It applies from a segment's 32nd frame, is kept
//!   across comparable resets and is forgotten when the receiver state changes. **Fast
//!   release/attack:** each bin's mean deviation from its tracked level over 0.4 s windows
//!   (`wide.shape_snap_s`) that exceeds 1 dB and six standard errors is applied at once, so an
//!   accessory change (a −20 dB notch removed or inserted) is learned within two windows
//!   (floor-branch Pfa back within 1.5× design 1 s after the change). Signals present in most of a
//!   window lift a learned dip the same way; it re-forms after them.
//!
//! **Known limits of the wide reference** (T-005 re-review #3, T-006):
//! - Interior coverage of wide signals degrades beyond ≈ 2000 bins (4096 bins: ≥ 95 % of a
//!   2048-bin signal; 16k FFT: 70 % of a 3072-bin and 33.5 % of a 6144-bin signal).
//! - 0 % for signals wider than ≈ 55 % of the span, or with soft (ramped) edges.
//! - A band-pass accessory or external LNA passband needs a calibrated response: uncalibrated,
//!   the floor-branch Pfa is ≈ 900× design, permanently.
//! - A signal present through most of a 0.4 s snap window lifts a learned dip until it ends.
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
//! # Slow floor and gates (per segment)
//!
//! 1. **Warm-up** (8 frames): the slow floor is the per-block median of the frames so far, so a
//!    start-up transient cannot seed it; the gate history fills.
//! 2. **Band-wide impulsive gate** (a T-006 input): a frame is `impulsive` when its band floor
//!    (dB) exceeds the running median of the last 64 unflagged band floors by more than 0.5 dB.
//!    Impulsive frames update no floor and no run statistic; T-006 merges boxes inside them into
//!    one `impulsive` event. A flagged run lasting 0.1 s is a level change: the gate releases
//!    (`gate_released`) and re-seeds **its history only**.
//! 3. **Per-block slow-floor gate** ([`SlowFloorConfig`]): a block whose per-frame floor is more
//!    than 0.5 dB from its slow floor does not update it (the excursion continues while beyond
//!    0.25 dB). An excursion lasting 0.5 s is **adopted**: the slow floor is re-seeded at the
//!    mean of its frames within ±3 dB (`FloorStats::adoptions`), unless the block has a rise or
//!    fall run whose recent excess is beyond 3 dB. So band-wide or partial-band bursts shorter
//!    than 0.5 s never move the slow floor (+1 to +3 dB, 150–300 ms, 5–20 % duty: mean error
//!    ≤ 0.01 dB), and sub-threshold steps are adopted after 0.5 s.
//! 4. **Long reference** (per block): an IIR of the slow floor with τ = 120 s (a cumulative mean
//!    early in a segment), frozen during excursions. It catches changes too slow for the 1 s slow
//!    floor to report (a 10 s ramp).
//!
//! # Floor-change events
//!
//! [`FloorEvent`]s describe **episodes**: one physical floor rise is one episode, from `Rise` to
//! a closing `End` or `Unknown`, with `Extend` and `Update` in between; a `Fall` is a standalone
//! event with its own id. Events are emitted in frame order; within a frame: at a reset the
//! `Unknown`s; then `Update`/`End` from returns, merges (`End` with [`EndReason::Merged`] before
//! the survivor's `Extend`), `Rise`/`Extend`, `Fall`, and rebaseline `End`s. Settings are
//! [`FloorChangeConfig`] (defaults in brackets).
//!
//! **Per-block classifier.** A block outside any episode starts a **rise run** when its
//! per-frame floor exceeds `min(slow, long)` by `threshold_db` [3 dB]; that reference is its
//! baseline. The run continues while the excess over the baseline stays ≥ `end_threshold_db`
//! [1.5 dB], and is *confirmable* once it has lasted `confirm_s` [1 s] with its recent excess
//! (EMA, τ = `confirm_s`/3, non-impulsive frames) still ≥ 3 dB. A **fall run** starts 3 dB below
//! `max(slow, long)`, tolerates frames back near the floor for up to `confirm_s` since its last
//! hit (a frame below −3 dB), and is confirmable after `confirm_s` when at least
//! `fall_min_hit_fraction` [0.5] of its last `confirm_s` of frames are hits, so a dropout of a
//! few low frames never confirms. The slow floor is frozen while a run is pending. A block in **hold-off**
//! (`holdoff_s` [2 s] after it returned or fell) still runs but cannot seed a confirmation.
//!
//! **Aggregator** (each frame, in order):
//! 1. *Returns.* A member block has returned when its excess over its baseline, less the
//!    background drift since it joined (the running median slow-floor change of idle blocks),
//!    stays below 1.5 dB for `end_s` [1 s]. A completed member waits while another member of the
//!    same episode is part-way through its return, so blocks crossing a few frames apart leave
//!    together. Leaving re-seeds the block's slow floor at its return level and starts its
//!    hold-off; a block that is still elevated never leaves and is never re-seeded. If no member
//!    remains: `End` with [`EndReason::Returned`] (`onset` = the frame from which all the leaving
//!    blocks stayed back). Otherwise, when the remaining members are no longer contiguous, the
//!    largest run keeps the id and every other run becomes a new episode announced by a `Rise`
//!    with `split_from` (same class and onset); then `Update` (`change_bins` = the returned blocks,
//!    `bins` = the real remaining extent). Every open episode is therefore one contiguous run.
//! 2. *Rises.* A maximal run of adjacent blocks, each pending a rise or a member of an open
//!    episode, that contains a confirmable block not in hold-off forms a group; all its pending
//!    blocks join, so blocks in hold-off or with shorter runs join a neighbour's confirmation.
//!    No episode touched: a new episode, `Rise` (`onset` = the group's earliest onset, `level` =
//!    mean over the run, class of the group). One touched: `Extend` (`onset` = the added blocks'
//!    onset, `change_bins` = the added blocks). Several touched: the survivor is the noise-like
//!    one before an unverified one before a structured one, then an emitted one, then the oldest;
//!    every other ends with `End` ([`EndReason::Merged`], `merged_into`). The survivor emits one
//!    `Extend` per contiguous run of added blocks (new and absorbed), so a merge reports bridge +
//!    absorbed extent and a region widening on both sides reports only the two added strips. In a
//!    non-structured episode, blocks that joined as a structured group are their own runs,
//!    reported as `Extend` with class `Structured` (so a noise-like anomaly never covers them);
//!    a split keeps the id on a run with non-structured blocks and a run of only structured blocks
//!    splits off as a `Structured` episode. A joining block's slow floor is re-seeded at its
//!    recent level only when its group is `NoiseLike`; otherwise (`Structured`, or `Unverified`
//!    without SK) it stays at the baseline: the floor under a wide signal.
//! 3. *Falls.* A maximal run of adjacent blocks pending a fall, with a confirmable seed, is one
//!    `Fall`. Edge blocks whose hit fraction is below `edge_hit_fraction` [0.5] of the group
//!    median do not vote; the fall is `interrupted` when more than half of the voting blocks
//!    have a hit fraction below `fall_hit_fraction` [0.9] (an intermittent signal the slow floor
//!    had followed: a correction rather than a drop of the floor). An interrupted fall narrower
//!    than a block width (a block straddling a sharp floor edge) is dropped without an event.
//!    Every block of the run adopts the `hit_fraction/2` quantile of all its window frames (the
//!    median of a clean fall, the middle of the low frames of an interrupted one) and starts its
//!    hold-off.
//! 4. *Extent* (T-038). An episode's `bins` stay the hull of its member blocks (the invariants
//!    below are on `bins`), but a block floor only rises once nearly all its 256 bins are
//!    elevated, so the hull under-reports a partial-band emission by up to a block per side (an
//!    800 kHz jammer read 375 kHz). `f_lo_hz`/`f_hi_hz` (and a change edge on the extent's edge)
//!    are therefore refined per bin at every summary: from each hull edge, walk outward (at most
//!    one block width, never past the midpoint of the gap to another episode's member block)
//!    while a 15-bin median of the recent PSD (EMA, 0.1 s) is at or above the geometric
//!    mid-point of the edge block's baseline and level, or inward (at most half a hop) while it is
//!    below. Partial-band noise jammers 600 kHz–1.2 MHz at several positions, including
//!    block-straddling edges, land within 20 % of the true bandwidth
//!    (`tests/aware_006_structured_vs_noise.rs`; typically within 2 kHz).
//! 5. *Rebaseline.* An episode open for `rebaseline_s` [600 s] ends with
//!    [`EndReason::Rebaselined`]; its level becomes the floor (a later drop is a `Fall`).
//!
//! **Discriminator.** In order:
//! 1. Excess noise = standard deviation of the frame-to-frame first differences of the excess / √2
//!    (a level trend such as an onset ramp adds nothing), median over the group's blocks: > 1.5 dB
//!    → `Structured`.
//! 2. **Bin-fluctuation correlation** (T-038): per frame and block pending a rise,
//!    `d_i = (P_i − P'_i)/(P_i + P'_i)` against the previous frame (static shape, edges, CP ripple
//!    and pilots cancel; scale-free per bin), and `Σ d_i d_{i+lag} / (lags · Σ d_i²)` over lags
//!    3–8 (Hann; Blackman-Harris 7–12, flat-top 9–14: beyond the window's own bin correlation),
//!    averaged over the run's frames (never the onset or impulsive frames) and the group's
//!    blocks. Periodogram bins of stationary Gaussian noise that far apart are independent, so a
//!    noise jammer reads 0 (measured −0.008…+0.002); FM-by-noise reads positive (a wandering
//!    carrier lights neighbouring bins together, +0.005…+0.4); OFDM with constant per-symbol
//!    energy reads negative (each subcarrier's windowed energy is conserved across its lobe,
//!    −0.02…−0.08 for 16–96 subcarriers at 6–15 dB, with or without CP). Below
//!    `−max(max_bin_anticorrelation (0.01), 4σ)` → `Structured`, where σ is the statistic's
//!    standard error under noise (so a run of a few frames, e.g. a ramp mostly flagged impulsive,
//!    is not evidence).
//! 3. Mean spectral kurtosis in `[1 − 0.15, 1 + 0.35]` → `NoiseLike` (the upper side is wider:
//!    FM-by-noise reads 1.04–1.31, OFDM below 1; long-symbol OFDM, 256 of 512 subcarriers, reads
//!    0.65–0.82), outside → `Structured`, no SK → `Unverified`.
//!
//! All classes are emitted by default (`emit_structured`); consumers filter by class.
//! `tests/aware_006_structured_vs_noise.rs` replays synthetic 8-bit IQ (1024-bin Hann × 10): 18/18
//! OFDM runs `Structured`, 12/12 noise-jammer runs (broadband, 800 kHz partial-band, FM-by-noise
//! with 100/250 kHz modulating noise; 6–15 dB) `NoiseLike`. Known limits:
//! - A wideband single-carrier PSK/QAM whose symbol is much shorter than the FFT window, or OFDM
//!   whose per-subcarrier energy varies a lot (dense QAM), is Gaussian enough at this resolution
//!   to read `NoiseLike`; cyclostationary estimates from IQ (C12) must separate those.
//! - The anti-correlation shrinks as the FFT window grows relative to the OFDM symbol (≈ 1/(window
//!   /symbol) per lag): OFDM with a 32-sample symbol at 1024 bins is near the threshold, and at
//!   16k bins most OFDM relies on SK. Tuned on Hann (the default).
//! - Slow FM-by-noise (modulating bandwidth ≲ 2 % of the deviation at this resolution) is a
//!   wandering carrier: SK ≥ 1.35 or excess noise makes it `Structured` (1 MHz/20 kHz reads
//!   `Structured` from 10 dB), so it opens no floor-rise Anomaly.
//!
//! A continuous, steady, Gaussian-like wideband emission that reads `NoiseLike` has its blocks'
//! slow floor follow it. A steady wide signal that is not `NoiseLike` (SK away from 1, or no SK)
//! holds the slow floor at the baseline (a +6 dB one moves it ≤ 0.5 dB,
//! `t029_steady_wide_signal_does_not_lift_the_slow_floor`). A **sub-threshold** (< 3 dB) steady
//! wide signal is still adopted after `settle_s` regardless of SK (T-021: read
//! `active_episodes`/SK before trusting the slow floor there). The FM fixture
//! (`fm_100p8M_2p4M…`) gives one `interrupted` `Fall` about 1 s in (4096 bins: 101.27–101.46 MHz,
//! −4.6 dB; 16k bins: −11 dB; none at 1024). This is not a warm-up bug: the capture's first
//! ≈ 70–270 ms read 4–11 dB above the later level there, the warm-up median seeds from that
//! transient, and the Fall corrects it.
//!
//! **Resets.** A *comparable* reset (a gap, stream start or [`NoiseFloorTracker::reset`] with the
//! same [`GainKey`]) suspends open episodes and carries every block's reference (its baseline if
//! a member, its slow floor otherwise) to the end of the new segment's warm-up. There, members
//! still ≥ 1.5 dB above their carried baseline stay (the episode continues with the same id, with
//! an `Update` if some left); an episode with no member left ends with `End`
//! ([`EndReason::Reset`], `onset` = the resetting frame). Other blocks more than 3 dB from their
//! carried reference restart from it, and changes inside the warm-up start runs backdated to
//! their first beyond-threshold warm-up frame (a rise three frames after a gap is a `Rise` with
//! that onset). An *incomparable* reset (gain, tune beyond 0.25 bin, rate, bandwidth, antenna port
//! or resolution) closes each open episode with `Unknown`: the new segment seeds at whatever it
//! sees, so the physical rise may continue unseen. **Consumers must treat a later bare `Fall`
//! over an `Unknown` episode's region as a possible close of it.**
//!
//! **Invariants** (`tests/floor_episodes.rs`: named repros and randomised sequences): no
//! `Extend`/`Update`/`End`/`Unknown` without an open `Rise`, nothing after a close, one `Rise`
//! per episode; every `Rise` closes once the scene is quiet for `rebaseline_s` + `end_s`;
//! `Extend` adds inside the new extent and `Update` removes from the old; open episodes never
//! touch; no block's slow floor jumps by the threshold without an event covering it.
//!
//! **T-020 mapping** (`hk_context::anomaly`): `Rise` opens an Anomaly `noise-floor-rise`;
//! `Extend` grows the episode's primary anomaly; `Update` clips its anomalies to the current
//! extent; a `Rise` with `split_from` moves the parent's anomalies inside it; `End(Merged)`
//! re-parents the episode's open anomalies (and Explanations) to `merged_into`; other `End`s
//! close them; `Unknown` closes them as indeterminate.
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
//! been seen (tested under a counting allocator), including resets, gate releases, shape updates
//! and every event kind. Cost at 4096 bins (mean, p99.9, max): `benches/floor_throughput.rs`.

pub mod averaging;
pub mod blocks;
mod episodes;
pub mod fcme;
pub mod gamma;
pub mod minstat;
pub mod percentile;
pub mod quantisation;
pub mod threshold;
pub mod tracker;
mod wide;

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
    NoiseFloorTracker, PercentileCheck, SlowFloorConfig, WideReferenceConfig,
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
