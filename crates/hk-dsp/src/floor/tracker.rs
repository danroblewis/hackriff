//! The streaming noise-floor tracker: one [`FloorFrame`] per [`SpectrumFrame`], plus
//! [`FloorEvent`]s. See the [module docs](super) for the floors and the event model.

use std::ops::Range;

use hk_core::{Discontinuity, ProvenanceHandle};
use hk_model::SampleTime;

use super::blocks::fill_invalid;
use super::episodes::{Engine, EventCtx, Frame, Stamp, Timing};
use super::wide::{ResponseShape, normalised_blocks, wide_blocks};
use super::{
    AveragingModel, BlockConfig, BlockFcme, BlockLayout, BlockPercentile, FcmeConfig,
    FloorConfigError, FloorMethod, PercentileConfig, QuantisationFloor, check_positive,
    check_probability, db_ratio, gamma, median_in_place, occupancy_from_count,
};
use crate::stft::SpectrumFrame;
use crate::window::WindowKind;

/// Frame discontinuities that start a new segment by default: stream start, rate change and
/// gaps. Retunes and gain changes are detected by comparing [`GainKey`]s with a tolerance, so a
/// sub-bin frequency correction (which the STFT flags as `RETUNE`) does not reset the series.
pub const FLOOR_RESET_ON: Discontinuity = Discontinuity::from_bits_truncate(
    Discontinuity::STREAM_START.bits()
        | Discontinuity::RATE_CHANGE.bits()
        | Discontinuity::GAP.bits(),
);

/// Band-wide impulsive-frame gate settings (S4 §5). The gate only flags frames; it never
/// re-seeds the slow floor (that is [`SlowFloorConfig`]'s per-block gate).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ImpulsiveGateConfig {
    /// Flag a frame when its band floor rises this far above the running median, dB (0.5).
    pub rise_db: f64,
    /// Running-median history, unflagged frames (64).
    pub history_frames: usize,
    /// Frames of history before flagging; also the warm-up that seeds the slow floor (8).
    pub min_history: usize,
    /// A flagged run lasting this long is a level change: the gate releases and re-seeds its
    /// history from the run (0.1 s).
    pub max_duration_s: f64,
}

impl Default for ImpulsiveGateConfig {
    fn default() -> Self {
        Self {
            rise_db: 0.5,
            history_frames: 64,
            min_history: 8,
            max_duration_s: 0.1,
        }
    }
}

/// Per-block slow-floor gate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SlowFloorConfig {
    /// A block whose per-frame floor is more than this from its slow floor does not update it,
    /// dB (0.5). The excursion continues while it stays beyond half of this.
    pub settle_db: f64,
    /// A sub-threshold excursion lasting this long is adopted: the slow floor is re-seeded at the
    /// mean of its within-threshold frames, seconds (0.5). Rises and falls that look like
    /// confirming floor changes are never adopted.
    pub settle_s: f64,
}

impl Default for SlowFloorConfig {
    fn default() -> Self {
        Self {
            settle_db: 0.5,
            settle_s: 0.5,
        }
    }
}

/// Floor-change episode settings (AWARE-006).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FloorChangeConfig {
    /// A block starts a rise (fall) when its per-frame floor is this far above (below) its
    /// reference, dB (3). A run confirms when its recent excess (EMA, τ = `confirm_s`/3) is
    /// still beyond this.
    pub threshold_db: f64,
    /// Hysteresis: a rise run continues, and a member block counts as returned, relative to this
    /// excess over the baseline, dB (1.5).
    pub end_threshold_db: f64,
    /// Run length that confirms a rise or fall, seconds (1.0).
    pub confirm_s: f64,
    /// Continuous return that takes a block out of its episode, seconds (1.0).
    pub end_s: f64,
    /// After a block returns or falls, it cannot seed a new confirmation for this long (it can
    /// still join a neighbour's), seconds (2.0).
    pub holdoff_s: f64,
    /// Long reference time constant, seconds (120): catches rises slower than the slow floor
    /// follows (a 10 s ramp).
    pub long_time_constant_s: f64,
    /// A fall confirms only when at least this fraction of its last `confirm_s` of frames are
    /// below −`threshold_db` (0.5): a dropout of a few frames never confirms.
    pub fall_min_hit_fraction: f64,
    /// A falling block with fewer hit frames than this fraction is interrupted; a fall is
    /// `interrupted` when most of its blocks are (0.9).
    pub fall_hit_fraction: f64,
    /// Edge blocks of a fall with a hit fraction below this times the group median are dropped
    /// (0.5).
    pub edge_hit_fraction: f64,
    /// An episode open this long since confirmation ends with [`EndReason::Rebaselined`] and its
    /// level becomes the floor (`Some(600 s)`; `None` never).
    pub rebaseline_s: Option<f64>,
    /// Noise-like when the region's mean spectral kurtosis is within this of 1 (0.15).
    pub sk_tolerance: f64,
    /// Structured when the excess noise (std of first differences / √2) exceeds this, dB (1.5).
    pub max_excess_std_db: f64,
    /// Emit events for [`FloorChangeClass::Structured`] episodes (true; consumers filter by
    /// class).
    pub emit_structured: bool,
}

impl Default for FloorChangeConfig {
    fn default() -> Self {
        Self {
            threshold_db: 3.0,
            end_threshold_db: 1.5,
            confirm_s: 1.0,
            end_s: 1.0,
            holdoff_s: 2.0,
            long_time_constant_s: 120.0,
            fall_min_hit_fraction: 0.5,
            fall_hit_fraction: 0.9,
            edge_hit_fraction: 0.5,
            rebaseline_s: Some(600.0),
            sk_tolerance: 0.15,
            max_excess_std_db: 1.5,
            emit_structured: true,
        }
    }
}

/// The wide-signal detection reference and the learned response shape (see
/// [`FloorKind::Wide`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WideReferenceConfig {
    /// Window of the lower envelope, `±` blocks (16: ±1024 bins with hop 64).
    pub half_width_blocks: usize,
    /// A block is step-like when it exceeds the slope-limited envelope by more than this, dB
    /// (3.5).
    pub step_db: f64,
    /// Slope allowed in the envelope, dB per block hop (0.15).
    pub slope_db_per_block: f64,
    /// Quantile of the block floors below which the floor is never read as a signal edge, and
    /// the shape's reference level (0.4).
    pub floor_quantile: f64,
    /// Learn the per-bin response shape (true).
    pub learn_shape: bool,
    /// Shape IIR time constant, seconds (1.0; a cumulative mean at segment start).
    pub shape_time_constant_s: f64,
    /// Features narrower than this are not part of the shape, bins (65).
    pub shape_min_width_bins: usize,
    /// Shape values within this of the reference level are 1, dB (0.5).
    pub shape_deadband_db: f64,
    /// Fast release/attack: a bin whose mean deviation from its learned level over a window of
    /// `shape_snap_s` exceeds this (and six standard errors) jumps to the new level, dB (1.0).
    pub shape_snap_db: f64,
    /// The fast release/attack window, seconds (0.4).
    pub shape_snap_s: f64,
}

impl Default for WideReferenceConfig {
    fn default() -> Self {
        Self {
            half_width_blocks: 16,
            step_db: 3.5,
            slope_db_per_block: 0.15,
            floor_quantile: 0.4,
            learn_shape: true,
            shape_time_constant_s: 1.0,
            shape_min_width_bins: 65,
            shape_deadband_db: 0.5,
            shape_snap_db: 1.0,
            shape_snap_s: 0.4,
        }
    }
}

/// [`NoiseFloorTracker`] settings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FloorConfig {
    /// Block geometry (256 / 64).
    pub blocks: BlockConfig,
    /// FCME settings.
    pub fcme: FcmeConfig,
    /// Percentile cross-check (`Some(p20)` by default; `None` skips it).
    pub percentile: Option<PercentileConfig>,
    /// Gamma shape model.
    pub averaging: AveragingModel,
    /// Noise exceedance of the occupancy threshold (1e-2).
    pub occupancy_pfa: f64,
    /// Slow floor IIR time constant, seconds (1.0).
    pub slow_time_constant_s: f64,
    /// Per-block slow-floor gate.
    pub slow: SlowFloorConfig,
    /// Band-wide impulsive-frame gate.
    pub impulsive: ImpulsiveGateConfig,
    /// Floor-change episodes.
    pub change: FloorChangeConfig,
    /// Wide-signal reference and response shape.
    pub wide: WideReferenceConfig,
    /// Reported model uncertainty, dB (±0.5).
    pub uncertainty_db: f32,
    /// Low-gain receiver floor for `quantisation_limited` (S4 HackRF One).
    pub quantisation: QuantisationFloor,
    /// `quantisation_limited` when the band floor is within this of that floor, dB (3).
    pub quantisation_margin_db: f64,
    /// Centre-frequency changes up to this many bins keep the segment (0.25).
    pub tune_tolerance_bins: f64,
    /// Frame discontinuities that start a new segment ([`FLOOR_RESET_ON`]).
    pub reset_on: Discontinuity,
}

impl Default for FloorConfig {
    fn default() -> Self {
        Self {
            blocks: BlockConfig::default(),
            fcme: FcmeConfig::default(),
            percentile: Some(PercentileConfig::default()),
            averaging: AveragingModel::Effective,
            occupancy_pfa: 1e-2,
            slow_time_constant_s: 1.0,
            slow: SlowFloorConfig::default(),
            impulsive: ImpulsiveGateConfig::default(),
            change: FloorChangeConfig::default(),
            wide: WideReferenceConfig::default(),
            uncertainty_db: 0.5,
            quantisation: QuantisationFloor::HACKRF_ONE_S4,
            quantisation_margin_db: 3.0,
            tune_tolerance_bins: 0.25,
            reset_on: FLOOR_RESET_ON,
        }
    }
}

fn check_fraction_closed(name: &'static str, value: f64) -> Result<(), FloorConfigError> {
    if value > 0.0 && value <= 1.0 {
        Ok(())
    } else {
        Err(FloorConfigError::Fraction { name, value })
    }
}

fn check_non_negative(name: &'static str, value: f64) -> Result<(), FloorConfigError> {
    if value >= 0.0 && value.is_finite() {
        Ok(())
    } else {
        Err(FloorConfigError::NonPositive { name, value })
    }
}

impl FloorConfig {
    /// Checks the settings.
    pub fn validate(&self) -> Result<(), FloorConfigError> {
        self.blocks.validate()?;
        self.fcme.validate()?;
        if let Some(p) = &self.percentile {
            p.validate()?;
        }
        if let AveragingModel::Fixed(n) = self.averaging {
            check_positive("averaging", n)?;
        }
        check_probability("occupancy_pfa", self.occupancy_pfa)?;
        check_positive("slow_time_constant_s", self.slow_time_constant_s)?;
        check_positive("slow.settle_db", self.slow.settle_db)?;
        check_positive("slow.settle_s", self.slow.settle_s)?;
        let g = &self.impulsive;
        check_positive("impulsive.rise_db", g.rise_db)?;
        check_positive("impulsive.max_duration_s", g.max_duration_s)?;
        if g.min_history == 0 || g.min_history > g.history_frames {
            return Err(FloorConfigError::NonPositive {
                name: "impulsive: need 1 <= min_history <= history_frames",
                value: g.min_history as f64,
            });
        }
        let c = &self.change;
        check_positive("change.threshold_db", c.threshold_db)?;
        check_positive("change.end_threshold_db", c.end_threshold_db)?;
        if c.end_threshold_db > c.threshold_db {
            return Err(FloorConfigError::NonPositive {
                name: "change: need end_threshold_db <= threshold_db",
                value: c.end_threshold_db,
            });
        }
        check_positive("change.confirm_s", c.confirm_s)?;
        check_positive("change.end_s", c.end_s)?;
        check_non_negative("change.holdoff_s", c.holdoff_s)?;
        check_positive("change.long_time_constant_s", c.long_time_constant_s)?;
        check_fraction_closed("change.fall_hit_fraction", c.fall_hit_fraction)?;
        check_fraction_closed("change.fall_min_hit_fraction", c.fall_min_hit_fraction)?;
        check_fraction_closed("change.edge_hit_fraction", c.edge_hit_fraction)?;
        if let Some(r) = c.rebaseline_s {
            check_positive("change.rebaseline_s", r)?;
        }
        check_positive("change.sk_tolerance", c.sk_tolerance)?;
        check_positive("change.max_excess_std_db", c.max_excess_std_db)?;
        let w = &self.wide;
        check_positive("wide.step_db", w.step_db)?;
        check_non_negative("wide.slope_db_per_block", w.slope_db_per_block)?;
        check_probability("wide.floor_quantile", w.floor_quantile)?;
        check_positive("wide.shape_time_constant_s", w.shape_time_constant_s)?;
        check_non_negative("wide.shape_deadband_db", w.shape_deadband_db)?;
        check_positive("wide.shape_snap_db", w.shape_snap_db)?;
        check_positive("wide.shape_snap_s", w.shape_snap_s)?;
        check_non_negative("tune_tolerance_bins", self.tune_tolerance_bins)?;
        check_positive("uncertainty_db", f64::from(self.uncertainty_db))
    }
}

/// The receiver state an estimate is keyed by: tune, rate, bandwidth, gains, antenna port and
/// the spectral resolution. A change beyond tolerance starts a new segment.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GainKey {
    /// RF centre, Hz.
    pub center_hz: f64,
    /// Sample rate, Hz.
    pub sample_rate_hz: f64,
    /// Baseband filter bandwidth, Hz.
    pub bandwidth_hz: f64,
    /// LNA gain, dB.
    pub lna_db: f64,
    /// VGA gain, dB.
    pub vga_db: f64,
    /// RF amp on.
    pub amp_on: bool,
    /// FNV-1a hash of the antenna/filter port name (`None` when unset).
    pub antenna_port_hash: Option<u64>,
    /// FFT length.
    pub fft_len: usize,
    /// Segment overlap, samples.
    pub overlap: usize,
    /// Segments per frame `K`.
    pub n_avg: u32,
    /// Window.
    pub window: WindowKind,
}

fn fnv1a(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

impl GainKey {
    /// The key of a frame (receiver state from its provenance, geometry from its spectrum).
    pub fn of(frame: &SpectrumFrame) -> Self {
        let p = &frame.provenance;
        let r = &frame.spectrum.resolution;
        Self {
            center_hz: frame.spectrum.f_center_hz,
            sample_rate_hz: frame.spectrum.sample_rate_hz,
            bandwidth_hz: p.tune.bandwidth_hz,
            lna_db: p.tune.lna_db,
            vga_db: p.tune.vga_db,
            amp_on: p.tune.amp_on,
            antenna_port_hash: p.antenna_port.as_deref().map(fnv1a),
            fft_len: r.fft_len,
            overlap: r.overlap,
            n_avg: r.n_avg,
            window: r.window,
        }
    }

    /// Same state within tolerance: centre within `tune_tolerance_hz`, rate and bandwidth within
    /// 1 ppm (bandwidth ≥ 1 Hz), gains within 0.01 dB; everything else exactly.
    pub fn matches(&self, other: &GainKey, tune_tolerance_hz: f64) -> bool {
        let near = |a: f64, b: f64, tol: f64| (a - b).abs() <= tol;
        near(self.center_hz, other.center_hz, tune_tolerance_hz)
            && near(
                self.sample_rate_hz,
                other.sample_rate_hz,
                1e-6 * self.sample_rate_hz.abs(),
            )
            && near(
                self.bandwidth_hz,
                other.bandwidth_hz,
                (1e-6 * self.bandwidth_hz.abs()).max(1.0),
            )
            && near(self.lna_db, other.lna_db, 0.01)
            && near(self.vga_db, other.vga_db, 0.01)
            && self.amp_on == other.amp_on
            && self.antenna_port_hash == other.antenna_port_hash
            && self.fft_len == other.fft_len
            && self.overlap == other.overlap
            && self.n_avg == other.n_avg
            && self.window == other.window
    }
}

/// Which tracker floor to read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FloorKind {
    /// Per-frame floor (block FCME, shaped). Reads a flat signal wider than ~a block as floor.
    Frame,
    /// Wide-signal reference: the detection reference for the floor branch and the OS guard.
    Wide,
    /// Slow floor: science series and integrated detection.
    Slow,
}

/// The percentile cross-check for one frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PercentileCheck {
    /// Median of the valid block percentile floors, linear FS²/Hz (NaN when none).
    pub band_floor: f32,
    /// `band_floor` relative to the FCME band floor, dB.
    pub delta_db: f32,
    /// Mean block occupancy against the percentile floor.
    pub occupancy: f32,
    /// Both this occupancy and the FCME occupancy are below `max_occupancy`.
    pub valid: bool,
}

/// A floor over a channel's bins.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChannelFloor {
    /// First bin.
    pub start_bin: usize,
    /// One past the last bin.
    pub end_bin: usize,
    /// Mean per-bin floor, linear FS²/Hz.
    pub floor: f32,
    /// `floor` in dBFS/Hz.
    pub dbfs_per_hz: f32,
    /// Floor power integrated over the channel's bins, dBFS.
    pub dbfs: f32,
    /// Model uncertainty, dB.
    pub uncertainty_db: f32,
    /// Within the quantisation margin of the receiver's low-gain floor.
    pub quantisation_limited: bool,
}

/// What a [`FloorEvent`] reports. See the [event model](super#floor-change-events).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FloorEventKind {
    /// A confirmed floor rise opens an episode (T-020: open an Anomaly `noise-floor-rise`).
    Rise,
    /// Blocks joined an open episode (a wider or overlapping rise); `change_bins` is the added
    /// extent and `onset_*` its onset.
    Extend,
    /// Blocks of an open episode returned while others are still elevated; `change_bins` is the
    /// returned extent.
    Update,
    /// The episode closed: see [`EndReason`].
    End,
    /// A confirmed drop below the floor outside any episode (standalone; its own id). The slow
    /// floor adopts it. A bare Fall may close a floor rise the tracker lost track of.
    Fall,
    /// The receiver state changed (gain, tune, rate, resolution): the episode's state can no
    /// longer be judged and it is closed. The physical rise may continue.
    Unknown,
}

/// How a floor change was judged.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FloorChangeClass {
    /// Steady (excess noise within limit) and Gaussian within the frame (mean SK within tolerance
    /// of 1): a noise-floor change.
    NoiseLike,
    /// Bursty or modulated within frames (SK away from 1) or noisy across frames: a wide
    /// structured emission rather than a floor rise.
    Structured,
    /// Steady, but the spectrum carried no SK to check.
    Unverified,
}

/// Why an episode ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EndReason {
    /// Every member block stayed within `end_threshold_db` of its baseline (after background
    /// drift) for `end_s`.
    Returned,
    /// Across a comparable reset (a gap or forced reset with the same receiver state), the new
    /// segment's warm-up level was back at the baseline. `onset_*` is the resetting frame.
    Reset,
    /// A rise bridged it to an older episode, which continues (`merged_into`).
    Merged,
    /// Open for `rebaseline_s`: its level is now the floor.
    Rebaselined,
}

/// A floor-change event. All events of one episode share `episode`.
#[derive(Clone, Debug, PartialEq)]
pub struct FloorEvent {
    /// What happened.
    pub kind: FloorEventKind,
    /// Episode id (unique per tracker; a Fall has its own).
    pub episode: u64,
    /// Discriminator verdict of the confirmation group that opened the episode. For an `Extend`
    /// of a non-structured episode: `Structured` when the added blocks joined as a structured
    /// group (their own run, e.g. a merged structured episode), so consumers filtering by class
    /// never cover them; a split-off run of only such blocks is a `Structured` episode.
    pub class: FloorChangeClass,
    /// For `End`.
    pub end_reason: Option<EndReason>,
    /// For `End` with [`EndReason::Merged`]: the episode that continues.
    pub merged_into: Option<u64>,
    /// For a `Rise` that splits an open episode whose members became disconnected: that
    /// episode's id. The new episode continues part of it (same onset and class).
    pub split_from: Option<u64>,
    /// For `Fall`: most of its blocks were back near the old floor on some frames (an
    /// intermittent signal the slow floor had followed), so this corrects the floor rather than
    /// reporting a drop of it.
    pub interrupted: bool,
    /// Frame counter of the first frame of this change: Rise/Fall/Extend: the first elevated or
    /// depressed frame of the (added) blocks; Update/End: the frame from which the leaving blocks stayed back (a comparable
    /// reset's frame for End(Reset)); Unknown/End(Merged/Rebaselined): the frame that produced it.
    pub onset_seq: u64,
    /// Time of that frame.
    pub onset_t: SampleTime,
    /// Frame that produced the event.
    pub confirmed_seq: u64,
    /// Its time.
    pub confirmed_t: SampleTime,
    /// Onset of the episode's rise (equal to `onset_t` for Rise and Fall).
    pub episode_onset_t: SampleTime,
    /// Rise/Extend/Fall: onset to confirmation. End: episode onset to return onset. Update and
    /// Unknown: episode onset to this frame.
    pub duration_s: f64,
    /// The episode's extent after this event (hull of its member blocks; at close for End and
    /// Unknown).
    pub bins: Range<usize>,
    /// The bins this event is about: Rise/Fall/End/Unknown: `bins`; Extend: the added blocks;
    /// Update: the returned blocks.
    pub change_bins: Range<usize>,
    /// Lower edge of `change_bins`, Hz.
    pub change_f_lo_hz: f64,
    /// Upper edge of `change_bins`, Hz.
    pub change_f_hi_hz: f64,
    /// Lower edge of `bins`, Hz.
    pub f_lo_hz: f64,
    /// Upper edge of `bins`, Hz.
    pub f_hi_hz: f64,
    /// Fraction of the span covered by member blocks (≤ `bins.len()` / span).
    pub band_fraction: f32,
    /// Floor before the change (median of the members' baselines), dBFS/Hz.
    pub baseline_dbfs_per_hz: f32,
    /// Segment the baseline belongs to.
    pub baseline_segment: u64,
    /// Rise/Fall: mean level over the confirmation run (median over blocks); later events: the
    /// members' recent level; dBFS/Hz.
    pub level_dbfs_per_hz: f32,
    /// `level − baseline`, dB.
    pub step_db: f32,
    /// Largest median member excess over the baseline seen during the episode, dB.
    pub peak_step_db: f32,
    /// Statistical uncertainty of the Rise's `step_db` (run standard error ⊕ slow-floor noise),
    /// dB. The model uncertainty largely cancels in a same-gain difference.
    pub step_uncertainty_db: f32,
    /// Model uncertainty of each absolute level, dB.
    pub uncertainty_db: f32,
    /// Mean spectral kurtosis over the Rise's confirmation run (`None` without SK).
    pub sk: Option<f32>,
    /// Excess noise over the Rise's confirmation run (std of first differences / √2), dB.
    pub excess_std_db: f32,
    /// The baseline was quantisation-limited (a rise is then overstated relative to RF).
    pub quantisation_limited_before: bool,
    /// Receiver state when the episode opened.
    pub gain: GainKey,
    /// Segment of the frame that produced the event.
    pub segment: u64,
    /// Provenance of that frame.
    pub provenance: ProvenanceHandle,
}

/// One frame's floor estimate. Maps onto the provisional C08 `NoiseFloorEstimate`: `value`,
/// `uncertainty_db`, `method`, `occupied_fraction`, provenance, and the Provenance
/// `quantisation_limited` bit.
#[derive(Clone, Debug, PartialEq)]
pub struct FloorFrame {
    /// The spectrum frame's `seq`.
    pub seq: u64,
    /// The spectrum frame's time.
    pub t: SampleTime,
    /// The spectrum frame's provenance.
    pub provenance: ProvenanceHandle,
    /// Receiver state of the segment.
    pub gain: GainKey,
    /// Segment counter (increments on every reset).
    pub segment: u64,
    /// Frames in this segment including this one.
    pub frames_in_segment: u64,
    /// This frame started a segment.
    pub reset: bool,
    /// Estimator behind the floors.
    pub method: FloorMethod,
    /// Gamma shape used.
    pub n_avg_effective: f64,
    /// RF centre, Hz.
    pub f_center_hz: f64,
    /// Bin spacing, Hz.
    pub bin_width_hz: f64,
    /// At least one block was valid. When false, `floor` repeats the slow floor.
    pub valid: bool,
    /// Per-frame per-bin floor ([`FloorKind::Frame`]), linear FS²/Hz.
    pub floor: Vec<f32>,
    /// Wide-signal reference, linear FS²/Hz: the detection reference (see [`FloorKind::Wide`]).
    pub wide_floor: Vec<f32>,
    /// Learned per-bin response shape `S` (≤ 1; 1 where the floor has no downward feature).
    pub shape: Vec<f32>,
    /// Median of the per-frame block floors, linear FS²/Hz.
    pub band_floor: f32,
    /// Per-frame block floors (raw block FCME, see [`NoiseFloorTracker::layout`]); invalid
    /// blocks hold the nearest valid block's value.
    pub block_floor: Vec<f32>,
    /// Shape-normalised block floors, linear FS²/Hz: the blocks `floor` and `wide_floor` are
    /// interpolated from before the shape is multiplied back (FCME on `psd / S` where the shape
    /// varies by more than 1 dB over a block, the raw floor over the block's mean shape
    /// elsewhere; equal to `block_floor` while no shape applies, before the segment's 32nd
    /// frame). Holds the last valid frame's values when `valid` is false.
    pub norm_block_floor: Vec<f32>,
    /// Per-block FCME validity.
    pub block_valid: Vec<bool>,
    /// Per-block FCME iterations (the cap means not converged).
    pub block_iterations: Vec<u8>,
    /// Valid blocks.
    pub valid_blocks: u32,
    /// Valid blocks that hit the iteration cap.
    pub unconverged_blocks: u32,
    /// Slow per-bin floor, linear FS²/Hz.
    pub slow_floor: Vec<f32>,
    /// Median of the slow block floors, linear FS²/Hz.
    pub slow_band_floor: f32,
    /// Slow block floors, linear FS²/Hz.
    pub block_slow: Vec<f32>,
    /// The slow floor has warmed up in this segment.
    pub slow_ready: bool,
    /// Occupied fraction: bins above `Q⁻¹(n, occupancy_pfa)/n · wide_floor`, less the expected
    /// noise exceedance.
    pub occupancy: f32,
    /// Percentile cross-check, when configured.
    pub percentile: Option<PercentileCheck>,
    /// Model uncertainty, dB.
    pub uncertainty_db: f32,
    /// Statistical standard error of a block floor, dB (`4.34/√(n·clean bins)`).
    pub statistical_uncertainty_db: f32,
    /// Receiver low-gain floor at this sample rate, dBFS/Hz.
    pub quantisation_floor_dbfs_per_hz: Option<f32>,
    /// Margin for `quantisation_limited`, dB.
    pub quantisation_margin_db: f32,
    /// Band floor within the margin of the quantisation floor.
    pub quantisation_limited: bool,
    /// Broadband impulsive frame (band-wide gate).
    pub impulsive: bool,
    /// This frame ended a sustained flagged run: the gate released and re-seeded its history.
    pub gate_released: bool,
    /// Band floor over its running median, dB (the gate statistic).
    pub impulsive_excess_db: f32,
    /// Open floor-change episodes (all classes, including ones suspended across a reset).
    pub active_episodes: u32,
}

impl FloorFrame {
    fn new(bins: usize, blocks: usize, provenance: ProvenanceHandle, gain: GainKey) -> Self {
        Self {
            seq: 0,
            t: SampleTime {
                sample_index: 0,
                host_time: hk_model::Timestamp::UNIX_EPOCH,
            },
            provenance,
            gain,
            segment: 0,
            frames_in_segment: 0,
            reset: true,
            method: FloorMethod::BlockFcme,
            n_avg_effective: 1.0,
            f_center_hz: 0.0,
            bin_width_hz: 0.0,
            valid: false,
            floor: vec![0.0; bins],
            wide_floor: vec![0.0; bins],
            shape: vec![1.0; bins],
            band_floor: 0.0,
            block_floor: vec![0.0; blocks],
            norm_block_floor: vec![0.0; blocks],
            block_valid: vec![false; blocks],
            block_iterations: vec![0; blocks],
            valid_blocks: 0,
            unconverged_blocks: 0,
            slow_floor: vec![0.0; bins],
            slow_band_floor: 0.0,
            block_slow: vec![0.0; blocks],
            slow_ready: false,
            occupancy: 0.0,
            percentile: None,
            uncertainty_db: 0.5,
            statistical_uncertainty_db: 0.0,
            quantisation_floor_dbfs_per_hz: None,
            quantisation_margin_db: 3.0,
            quantisation_limited: false,
            impulsive: false,
            gate_released: false,
            impulsive_excess_db: 0.0,
            active_episodes: 0,
        }
    }

    /// The per-bin trace of `kind`.
    pub fn trace(&self, kind: FloorKind) -> &[f32] {
        match kind {
            FloorKind::Frame => &self.floor,
            FloorKind::Wide => &self.wide_floor,
            FloorKind::Slow => &self.slow_floor,
        }
    }

    /// The band floor of `kind` (Frame and Wide share the per-frame band floor), dBFS/Hz.
    pub fn band_floor_dbfs_per_hz(&self, kind: FloorKind) -> f32 {
        let v = match kind {
            FloorKind::Frame | FloorKind::Wide => self.band_floor,
            FloorKind::Slow => self.slow_band_floor,
        };
        10.0 * v.max(1e-37).log10()
    }

    /// Writes a trace in dBFS/Hz.
    pub fn write_db(&self, kind: FloorKind, out: &mut [f32]) {
        let t = self.trace(kind);
        assert_eq!(t.len(), out.len(), "trace/output length mismatch");
        for (o, &v) in out.iter_mut().zip(t) {
            *o = 10.0 * v.max(1e-37).log10();
        }
    }

    /// The floor over `bins` (mean of the per-bin floor).
    pub fn channel_floor(&self, bins: Range<usize>, kind: FloorKind) -> ChannelFloor {
        let t = &self.trace(kind)[bins.clone()];
        assert!(!t.is_empty(), "empty channel");
        let mean = t.iter().map(|&v| f64::from(v)).sum::<f64>() / t.len() as f64;
        let dbfs_per_hz = (10.0 * mean.max(1e-300).log10()) as f32;
        let dbfs = dbfs_per_hz + (10.0 * (self.bin_width_hz * t.len() as f64).log10()) as f32;
        ChannelFloor {
            start_bin: bins.start,
            end_bin: bins.end,
            floor: mean as f32,
            dbfs_per_hz,
            dbfs,
            uncertainty_db: self.uncertainty_db,
            quantisation_limited: self
                .quantisation_floor_dbfs_per_hz
                .is_some_and(|q| dbfs_per_hz < q + self.quantisation_margin_db),
        }
    }

    /// [`channel_floor`](Self::channel_floor) for each range into `out` (allocation-free).
    pub fn channel_floors(
        &self,
        channels: &[Range<usize>],
        kind: FloorKind,
        out: &mut [ChannelFloor],
    ) {
        assert_eq!(channels.len(), out.len(), "one output per channel");
        for (o, c) in out.iter_mut().zip(channels) {
            *o = self.channel_floor(c.clone(), kind);
        }
    }
}

/// Tracker counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FloorStats {
    /// Frames processed.
    pub frames: u64,
    /// Segments started.
    pub resets: u64,
    /// Frames with no valid block.
    pub invalid_frames: u64,
    /// Frames flagged impulsive.
    pub impulsive_frames: u64,
    /// Sustained flagged runs released as level changes.
    pub gate_releases: u64,
    /// Sub-threshold excursions adopted into the slow floor (per block).
    pub adoptions: u64,
    /// `Rise` events emitted.
    pub rise_events: u64,
    /// `Extend` events emitted.
    pub extend_events: u64,
    /// `Update` events emitted.
    pub update_events: u64,
    /// Episodes closed for any reason, emitted or not (End, Unknown).
    pub episode_ends: u64,
    /// Episodes merged into another.
    pub merges: u64,
    /// Episodes split off a disconnected episode.
    pub splits: u64,
    /// Episodes ended by `rebaseline_s`.
    pub rebaselines: u64,
    /// `Unknown` events emitted.
    pub unknown_events: u64,
    /// `Fall` events emitted.
    pub level_falls: u64,
    /// Falls flagged `interrupted`.
    pub floor_corrections: u64,
    /// Episodes whose opening group was judged structured.
    pub structured_episodes: u64,
}

type ResolutionKey = (usize, usize, u32, WindowKind);

fn push_ring(buf: &mut [f32], len: &mut usize, pos: &mut usize, v: f32) {
    let cap = buf.len();
    buf[*pos] = v;
    *pos = (*pos + 1) % cap;
    *len = (*len + 1).min(cap);
}

/// The streaming floor tracker. See the [module docs](super).
pub struct NoiseFloorTracker {
    config: FloorConfig,
    fcme: BlockFcme,
    percentile: Option<BlockPercentile>,
    layout: Option<BlockLayout>,
    resolution: Option<ResolutionKey>,
    n_eff: f64,
    occupancy_multiplier: f64,
    key: Option<GainKey>,
    force_reset: bool,
    next_segment: u64,
    timing: Timing,
    gate_max_frames: u64,
    shape_alpha: f64,
    quantisation_db: Option<f64>,
    out: Option<FloorFrame>,
    shape: ResponseShape,
    engine: Engine,
    warm: Vec<f32>,
    warm_stamps: Vec<Stamp>,
    warm_count: usize,
    hist: Vec<f32>,
    hist_len: usize,
    hist_pos: usize,
    hist_scratch: Vec<f32>,
    flag_vals: Vec<f32>,
    flag_len: usize,
    flag_pos: usize,
    flag_run: u64,
    scratch: Vec<f32>,
    scratch2: Vec<f32>,
    norm_block: Vec<f32>,
    wide_block: Vec<f32>,
    scaled: Vec<f32>,
    updates: u64,
    frame_counter: u64,
    stats: FloorStats,
}

impl NoiseFloorTracker {
    /// A tracker; buffers are sized on the first frame.
    pub fn new(config: FloorConfig) -> Result<Self, FloorConfigError> {
        config.validate()?;
        let h = config.impulsive.history_frames;
        let w = config.impulsive.min_history;
        let t0 = SampleTime {
            sample_index: 0,
            host_time: hk_model::Timestamp::UNIX_EPOCH,
        };
        Ok(Self {
            fcme: BlockFcme::new(config.fcme, 1.0)?,
            percentile: config
                .percentile
                .map(|p| BlockPercentile::new(p, 1.0))
                .transpose()?,
            config,
            layout: None,
            resolution: None,
            n_eff: 1.0,
            occupancy_multiplier: 1.0,
            key: None,
            force_reset: true,
            next_segment: 0,
            timing: Timing::default(),
            gate_max_frames: 1,
            shape_alpha: 1.0,
            quantisation_db: None,
            out: None,
            shape: ResponseShape::default(),
            engine: Engine::default(),
            warm: Vec::new(),
            warm_stamps: vec![(0, t0); w],
            warm_count: 0,
            hist: vec![0.0; h],
            hist_len: 0,
            hist_pos: 0,
            hist_scratch: vec![0.0; h],
            flag_vals: vec![0.0; h],
            flag_len: 0,
            flag_pos: 0,
            flag_run: 0,
            scratch: Vec::new(),
            scratch2: Vec::new(),
            norm_block: Vec::new(),
            wide_block: Vec::new(),
            scaled: Vec::new(),
            updates: 0,
            frame_counter: 0,
            stats: FloorStats::default(),
        })
    }

    /// Settings.
    pub fn config(&self) -> &FloorConfig {
        &self.config
    }

    /// Counters.
    pub fn stats(&self) -> FloorStats {
        self.stats
    }

    /// Block geometry in use (after the first frame).
    pub fn layout(&self) -> Option<&BlockLayout> {
        self.layout.as_ref()
    }

    /// The latest estimate.
    pub fn last(&self) -> Option<&FloorFrame> {
        self.out.as_ref()
    }

    /// Forces the next frame to start a new segment. The receiver state is unchanged, so open
    /// episodes are carried across it (a comparable reset).
    pub fn reset(&mut self) {
        self.force_reset = true;
    }

    fn configure(&mut self, frame: &SpectrumFrame) {
        let spectrum = &frame.spectrum;
        let bins = spectrum.bins();
        let layout = BlockLayout::new(bins, self.config.blocks).expect("validated config");
        let nb = layout.count();
        let r = &spectrum.resolution;
        self.resolution = Some((r.fft_len, r.overlap, r.n_avg, r.window));
        self.n_eff = self.config.averaging.n_avg(r).max(1e-3);
        self.fcme.set_n_avg(self.n_eff);
        if let Some(p) = &mut self.percentile {
            p.set_n_avg(self.n_eff);
        }
        self.occupancy_multiplier = gamma::mean_threshold(self.n_eff, self.config.occupancy_pfa);
        let key = GainKey::of(frame);
        match &mut self.out {
            Some(out) => {
                out.floor.resize(bins, 0.0);
                out.wide_floor.resize(bins, 0.0);
                out.shape.resize(bins, 1.0);
                out.slow_floor.resize(bins, 0.0);
                out.block_floor.resize(nb, 0.0);
                out.norm_block_floor.resize(nb, 0.0);
                out.block_valid.resize(nb, false);
                out.block_iterations.resize(nb, 0);
                out.block_slow.resize(nb, 0.0);
            }
            None => self.out = Some(FloorFrame::new(bins, nb, frame.provenance.clone(), key)),
        }
        let w = self.config.impulsive.min_history;
        self.warm.resize(nb * w, 0.0);
        self.engine.resize(nb, frame.t, key);
        self.shape.resize(bins, nb);
        self.scratch.resize(nb, 0.0);
        self.scratch2.resize(nb.max(w), 0.0);
        self.norm_block.resize(nb, 0.0);
        self.wide_block.resize(nb, 0.0);
        self.scaled.resize(bins, 0.0);
        self.layout = Some(layout);
        self.force_reset = true;
    }

    /// Folds in one spectrum frame; calls `on_event` for each floor-change event and returns the
    /// frame's estimate.
    pub fn update(
        &mut self,
        frame: &SpectrumFrame,
        mut on_event: impl FnMut(&FloorEvent),
    ) -> &FloorFrame {
        let spectrum = &frame.spectrum;
        let bins = spectrum.bins();
        assert!(bins > 0, "empty spectrum");
        let r = &spectrum.resolution;
        let res_key = (r.fft_len, r.overlap, r.n_avg, r.window);
        let fs = spectrum.sample_rate_hz;
        let now = (frame.seq, frame.t);
        let reconfigure = self.resolution != Some(res_key)
            || self.layout.as_ref().map(BlockLayout::bins) != Some(bins);
        if reconfigure {
            if let Some(out) = &self.out {
                // A new block layout: nothing carries over.
                let ctx = EventCtx {
                    now,
                    provenance: &frame.provenance,
                    segment: out.segment,
                    fs,
                    uncertainty_db: self.config.uncertainty_db,
                };
                self.engine
                    .on_reset(false, &ctx, &[], &mut self.stats, &mut on_event);
            }
            self.configure(frame);
        }
        let key = GainKey::of(frame);
        let tol_hz = self.config.tune_tolerance_bins * spectrum.bin_width_hz();
        let key_changed = !self.key.is_some_and(|k| k.matches(&key, tol_hz));
        let reset = self.force_reset
            || key_changed
            || frame.discontinuity.bits() & self.config.reset_on.bits() != 0;
        if reset {
            let out = self.out.as_ref().expect("configured");
            let comparable = !reconfigure && !key_changed;
            let ctx = EventCtx {
                now,
                provenance: &frame.provenance,
                segment: out.segment,
                fs,
                uncertainty_db: self.config.uncertainty_db,
            };
            self.engine.on_reset(
                comparable,
                &ctx,
                &out.block_slow,
                &mut self.stats,
                &mut on_event,
            );
            if !comparable {
                self.shape.reset();
            }
            let period = f64::from(r.n_avg) * r.hop() as f64 / fs;
            let frames = |s: f64| (s / period - 1e-9).ceil().max(1.0) as u64;
            let decay = |tau: f64| -(-period / tau).exp_m1();
            let c = &self.config;
            let confirm = frames(c.change.confirm_s);
            self.timing = Timing {
                confirm,
                end: frames(c.change.end_s),
                holdoff: (c.change.holdoff_s / period - 1e-9).ceil().max(0.0) as u64,
                settle: frames(c.slow.settle_s),
                alpha: decay(c.slow_time_constant_s),
                alpha_long: decay(c.change.long_time_constant_s),
                ema: (3.0 / confirm as f64).min(1.0) as f32,
                recent: decay(0.1) as f32,
                rebaseline: c.change.rebaseline_s.map(frames),
                fall_hits: (c.change.fall_min_hit_fraction * confirm as f64 - 1e-9)
                    .ceil()
                    .max(1.0) as u64,
            };
            self.engine.set_window(confirm as usize);
            self.shape_alpha = decay(c.wide.shape_time_constant_s);
            self.gate_max_frames = frames(c.impulsive.max_duration_s);
            self.quantisation_db = c.quantisation.dbfs_per_hz(fs);
        }

        let Self {
            config: cfg,
            fcme,
            percentile,
            layout,
            n_eff,
            occupancy_multiplier,
            key: seg_key,
            force_reset,
            next_segment,
            timing,
            gate_max_frames,
            shape_alpha,
            quantisation_db,
            out,
            shape,
            engine,
            warm,
            warm_stamps,
            warm_count,
            hist,
            hist_len,
            hist_pos,
            hist_scratch,
            flag_vals,
            flag_len,
            flag_pos,
            flag_run,
            scratch,
            scratch2,
            norm_block,
            wide_block,
            scaled,
            updates,
            frame_counter,
            stats,
            ..
        } = self;
        let layout = layout.as_ref().expect("configured");
        let out = out.as_mut().expect("configured");
        let nb = layout.count();
        let psd = &spectrum.psd;

        // 1. Per-frame block FCME (invalid blocks filled from neighbours).
        let (mut clean_total, mut valid_blocks, mut unconverged) = (0usize, 0usize, 0u32);
        for b in 0..nb {
            let est = fcme.estimate_block(&psd[layout.range(b)]);
            out.block_valid[b] = est.valid;
            out.block_iterations[b] = est.iterations.min(usize::from(u8::MAX)) as u8;
            out.block_floor[b] = est.floor as f32;
            if est.valid {
                valid_blocks += 1;
                clean_total += est.clean;
                unconverged += u32::from(!est.converged);
            }
        }
        let frame_valid = valid_blocks > 0;
        if !frame_valid {
            if !reset && *warm_count > 0 {
                out.block_floor.copy_from_slice(&out.block_slow);
            } else {
                out.block_floor.fill(f32::MIN_POSITIVE);
            }
        } else if valid_blocks < nb {
            fill_invalid(&mut out.block_floor, &out.block_valid);
        }
        scratch.copy_from_slice(&out.block_floor);
        let band_floor = median_in_place(scratch);
        let stat_block_db = if valid_blocks > 0 {
            (10.0
                / std::f64::consts::LN_10
                / (*n_eff * clean_total as f64 / valid_blocks as f64).sqrt()) as f32
        } else {
            0.0
        };

        // 2. Shaped per-frame floor and the wide-signal reference.
        if frame_valid {
            if cfg.wide.learn_shape {
                shape.update(psd, *shape_alpha, *n_eff, band_floor, &cfg.wide, layout);
            }
            normalised_blocks(
                shape,
                psd,
                layout,
                fcme,
                &out.block_floor,
                scaled,
                norm_block,
            );
            layout.interpolate(norm_block, &mut out.floor);
            out.norm_block_floor.copy_from_slice(norm_block);
            wide_blocks(
                norm_block,
                &cfg.wide,
                scratch,
                &mut scratch2[..nb],
                wide_block,
            );
            layout.interpolate(wide_block, &mut out.wide_floor);
            if shape.any_active() {
                for ((f, w), &s) in out
                    .floor
                    .iter_mut()
                    .zip(out.wide_floor.iter_mut())
                    .zip(shape.shape())
                {
                    *f *= s;
                    *w *= s;
                }
            }
        } else {
            layout.interpolate(&out.block_floor, &mut out.floor);
            out.wide_floor.copy_from_slice(&out.floor);
        }
        out.shape.copy_from_slice(shape.shape());
        let occupancy = {
            let m = *occupancy_multiplier as f32;
            let above = psd
                .iter()
                .zip(&out.wide_floor)
                .filter(|&(&p, &f)| p > m * f)
                .count();
            occupancy_from_count(above, bins, cfg.occupancy_pfa) as f32
        };

        // 3. Percentile cross-check.
        out.percentile = percentile.as_mut().map(|p| {
            let (mut occ, mut k) = (0.0, 0);
            for b in 0..nb {
                let est = p.estimate_block(&psd[layout.range(b)]);
                if est.floor.is_finite() {
                    scratch[k] = est.floor as f32;
                    k += 1;
                }
                occ += est.occupancy;
            }
            let occ = (occ / nb as f64) as f32;
            let band = if k > 0 {
                median_in_place(&mut scratch[..k])
            } else {
                f32::NAN
            };
            PercentileCheck {
                band_floor: band,
                delta_db: db_ratio(band, band_floor),
                occupancy: occ,
                valid: k > 0 && occ.max(occupancy) < p.config().max_occupancy as f32,
            }
        });

        // 4. Segment reset.
        if reset {
            out.segment = *next_segment;
            *next_segment += 1;
            out.frames_in_segment = 0;
            *warm_count = 0;
            *hist_len = 0;
            *hist_pos = 0;
            *flag_run = 0;
            *flag_len = 0;
            *flag_pos = 0;
            *updates = 0;
            *seg_key = Some(key);
            *force_reset = false;
            stats.resets += 1;
        }
        out.frames_in_segment += 1;

        let w = cfg.impulsive.min_history;
        let band_db = 10.0 * band_floor.max(1e-37).log10();
        let (mut impulsive, mut gate_excess, mut released) = (false, 0.0f32, false);
        let gain = seg_key.unwrap_or(key);
        if !frame_valid {
            stats.invalid_frames += 1;
        } else if *warm_count < w {
            // 5a. Warm-up: the slow floor is the per-block median of the frames so far, so one
            //     transient frame at start-up cannot seed it; the gate history fills.
            let k = *warm_count;
            warm[k * nb..(k + 1) * nb].copy_from_slice(&out.block_floor);
            warm_stamps[k] = now;
            for b in 0..nb {
                for j in 0..=k {
                    scratch2[j] = warm[j * nb + b];
                }
                out.block_slow[b] = median_in_place(&mut scratch2[..=k]);
            }
            push_ring(hist, hist_len, hist_pos, band_db);
            *warm_count += 1;
            *updates = *warm_count as u64;
            if *warm_count == w {
                let fr = Frame {
                    ev: EventCtx {
                        now,
                        provenance: &frame.provenance,
                        segment: out.segment,
                        fs,
                        uncertainty_db: cfg.uncertainty_db,
                    },
                    counter: *frame_counter,
                    impulsive: false,
                    floor: &out.block_floor,
                    spectrum,
                    layout,
                    gain,
                    stat_block_db,
                    quantisation_db: *quantisation_db,
                    quantisation_margin_db: cfg.quantisation_margin_db,
                    updates: *updates,
                };
                engine.start_segment(
                    &fr,
                    &mut out.block_slow,
                    warm,
                    warm_stamps,
                    &cfg.change,
                    timing,
                    stats,
                    &mut on_event,
                );
            }
        } else {
            // 5b. Band-wide impulsive gate on the band floor against its running median.
            if *hist_len >= w {
                let h = &mut hist_scratch[..*hist_len];
                h.copy_from_slice(&hist[..*hist_len]);
                gate_excess = band_db - median_in_place(h);
            }
            impulsive = *hist_len >= w && f64::from(gate_excess) > cfg.impulsive.rise_db;
            if impulsive {
                *flag_run += 1;
                push_ring(flag_vals, flag_len, flag_pos, band_db);
                if *flag_run >= *gate_max_frames {
                    // Sustained: a level change. Release and re-seed the gate history only; the
                    // slow floor follows through its own per-block gate.
                    let n = *flag_len;
                    hist[..n].copy_from_slice(&flag_vals[..n]);
                    *hist_len = n;
                    *hist_pos = n % hist.len();
                    *flag_run = 0;
                    *flag_len = 0;
                    *flag_pos = 0;
                    impulsive = false;
                    released = true;
                    stats.gate_releases += 1;
                }
            } else {
                if *flag_run > 0 {
                    *flag_run = 0;
                    *flag_len = 0;
                    *flag_pos = 0;
                }
                push_ring(hist, hist_len, hist_pos, band_db);
            }

            // 5c. Per-block classifier and episode aggregator.
            let fr = Frame {
                ev: EventCtx {
                    now,
                    provenance: &frame.provenance,
                    segment: out.segment,
                    fs,
                    uncertainty_db: cfg.uncertainty_db,
                },
                counter: *frame_counter,
                impulsive,
                floor: &out.block_floor,
                spectrum,
                layout,
                gain,
                stat_block_db,
                quantisation_db: *quantisation_db,
                quantisation_margin_db: cfg.quantisation_margin_db,
                updates: *updates,
            };
            engine.step(
                &fr,
                &mut out.block_slow,
                &cfg.change,
                &cfg.slow,
                timing,
                stats,
                &mut on_event,
            );
            if !impulsive {
                *updates += 1;
            }
        }

        // 6. Slow per-bin floor and frame metadata.
        layout.interpolate(&out.block_slow, &mut out.slow_floor);
        scratch.copy_from_slice(&out.block_slow);
        out.slow_band_floor = median_in_place(scratch);
        out.seq = frame.seq;
        out.t = frame.t;
        if out.provenance != frame.provenance {
            out.provenance = frame.provenance.clone();
        }
        out.gain = gain;
        out.reset = reset;
        out.n_avg_effective = *n_eff;
        out.f_center_hz = spectrum.f_center_hz;
        out.bin_width_hz = spectrum.bin_width_hz();
        out.valid = frame_valid;
        out.valid_blocks = valid_blocks as u32;
        out.unconverged_blocks = unconverged;
        out.band_floor = band_floor;
        out.occupancy = occupancy;
        out.slow_ready = *warm_count >= w;
        out.uncertainty_db = cfg.uncertainty_db;
        out.statistical_uncertainty_db = stat_block_db;
        out.quantisation_floor_dbfs_per_hz = quantisation_db.map(|q| q as f32);
        out.quantisation_margin_db = cfg.quantisation_margin_db as f32;
        out.quantisation_limited =
            quantisation_db.is_some_and(|q| f64::from(band_db) < q + cfg.quantisation_margin_db);
        out.impulsive = impulsive;
        out.gate_released = released;
        out.impulsive_excess_db = gate_excess;
        out.active_episodes = engine.active_count();
        *frame_counter += 1;
        stats.frames += 1;
        if impulsive {
            stats.impulsive_frames += 1;
        }
        out
    }
}
