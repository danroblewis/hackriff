//! The streaming noise-floor tracker: one [`FloorFrame`] per [`SpectrumFrame`], plus
//! [`FloorEvent`] episodes. See the [module docs](super) for the state machine.

use std::ops::Range;

use hk_core::{Discontinuity, ProvenanceHandle};
use hk_model::SampleTime;

use super::blocks::{fill_invalid, sliding_min};
use super::{
    AveragingModel, BlockConfig, BlockFcme, BlockLayout, BlockPercentile, FcmeConfig,
    FloorConfigError, FloorMethod, PercentileConfig, QuantisationFloor, check_positive,
    check_probability, db_ratio, gamma, median_in_place, occupancy_from_count,
};
use crate::spectrum::Spectrum;
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

/// Impulsive-frame gate settings (S4 §5).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ImpulsiveGateConfig {
    /// Flag a frame when its band floor rises this far above the running median, dB (0.5).
    pub rise_db: f64,
    /// Running-median history, unflagged frames (64).
    pub history_frames: usize,
    /// Frames of history before flagging; also the warm-up that seeds the slow floor (8).
    pub min_history: usize,
    /// A flagged run lasting this long is a level change, not a burst: the gate releases and
    /// re-seeds (0.1 s).
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

/// Floor-change episode settings (AWARE-006).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FloorChangeConfig {
    /// A block is elevated (or depressed) when its per-frame floor is this far from its slow
    /// floor, dB (3).
    pub threshold_db: f64,
    /// An episode ends when its region is back within this of the baseline, dB (1.5).
    pub end_threshold_db: f64,
    /// Continuous elevation needed to confirm a rise or fall, seconds (1.0).
    pub confirm_s: f64,
    /// Continuous return needed to end an episode, seconds (1.0).
    pub end_s: f64,
    /// After an episode ends or a fall is adopted, its blocks start no new run for this long,
    /// seconds (2.0).
    pub holdoff_s: f64,
    /// Noise-like when the region's mean spectral kurtosis is within this of 1 (0.15).
    pub sk_tolerance: f64,
    /// Structured when the frame-to-frame standard deviation of the excess exceeds this, dB (1.5).
    pub max_excess_std_db: f64,
    /// Also emit events for [`FloorChangeClass::Structured`] episodes (false).
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
            sk_tolerance: 0.15,
            max_excess_std_db: 1.5,
            emit_structured: false,
        }
    }
}

/// The wide-signal detection reference ([`FloorKind::Wide`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WideReferenceConfig {
    /// Sliding minimum over `±half_width_blocks` block floors (16: ±1024 bins with hop 64).
    pub half_width_blocks: usize,
    /// Use the per-frame floor where it is within this of the sliding minimum, dB (1.5).
    pub switch_db: f64,
}

impl Default for WideReferenceConfig {
    fn default() -> Self {
        Self {
            half_width_blocks: 16,
            switch_db: 1.5,
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
    /// Impulsive-frame gate.
    pub impulsive: ImpulsiveGateConfig,
    /// Floor-change episodes.
    pub change: FloorChangeConfig,
    /// Wide-signal reference.
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
        check_positive("change.sk_tolerance", c.sk_tolerance)?;
        check_positive("change.max_excess_std_db", c.max_excess_std_db)?;
        if !(c.holdoff_s >= 0.0 && c.holdoff_s.is_finite()) {
            return Err(FloorConfigError::NonPositive {
                name: "change.holdoff_s",
                value: c.holdoff_s,
            });
        }
        if !(self.wide.switch_db >= 0.0 && self.tune_tolerance_bins >= 0.0) {
            return Err(FloorConfigError::NonPositive {
                name: "wide.switch_db / tune_tolerance_bins",
                value: self.wide.switch_db.min(self.tune_tolerance_bins),
            });
        }
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
    /// Per-frame FCME floor. Reads a flat signal wider than ~a block as floor.
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

/// What a [`FloorEvent`] reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FloorEventKind {
    /// A confirmed floor rise: opens an episode (T-020: open an Anomaly `noise-floor-rise`).
    Rise,
    /// The episode's region returned to its baseline, or the segment reset: closes the episode.
    End,
    /// A confirmed floor drop below the slow floor outside any episode (adopted as the new
    /// floor; informational).
    Fall,
}

/// How a floor change was judged.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FloorChangeClass {
    /// Steady (excess std within limit) and Gaussian within the frame (mean SK within tolerance
    /// of 1): a noise-floor change.
    NoiseLike,
    /// Bursty or modulated within frames (SK away from 1) or unsteady across frames: a wide
    /// structured emission, not a floor rise. Not emitted unless configured.
    Structured,
    /// Steady, but the spectrum carried no SK to check.
    Unverified,
}

/// Why an episode ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EndReason {
    /// The region stayed within `end_threshold_db` of the baseline for `end_s`.
    Returned,
    /// The segment reset (gain/tune/rate change, gap, resolution change).
    Reset,
}

/// A floor-change event. A `Rise` and its `End` share `episode`.
#[derive(Clone, Debug, PartialEq)]
pub struct FloorEvent {
    /// Rise, End or Fall.
    pub kind: FloorEventKind,
    /// Episode id (unique per tracker).
    pub episode: u64,
    /// Discriminator verdict.
    pub class: FloorChangeClass,
    /// For `End`.
    pub end_reason: Option<EndReason>,
    /// Frame counter of the first frame of this change (Rise/Fall: first elevated or depressed
    /// frame; End: first returned frame, or the resetting frame).
    pub onset_seq: u64,
    /// Time of that frame (the change happened within it).
    pub onset_t: SampleTime,
    /// Frame that confirmed the change.
    pub confirmed_seq: u64,
    /// Its time (`onset_t + confirm_s` for rises and falls).
    pub confirmed_t: SampleTime,
    /// Onset of the episode's rise (equal to `onset_t` for Rise and Fall).
    pub episode_onset_t: SampleTime,
    /// Rise/Fall: onset to confirmation. End: rise onset to return onset (the episode length).
    pub duration_s: f64,
    /// Affected bins.
    pub bins: Range<usize>,
    /// Lower edge, Hz.
    pub f_lo_hz: f64,
    /// Upper edge, Hz.
    pub f_hi_hz: f64,
    /// Fraction of the span covered.
    pub band_fraction: f32,
    /// Slow floor before the change (median over the region's blocks), dBFS/Hz.
    pub baseline_dbfs_per_hz: f32,
    /// Segment the baseline belongs to.
    pub baseline_segment: u64,
    /// Mean level over the confirmation run (median over blocks), dBFS/Hz.
    pub level_dbfs_per_hz: f32,
    /// `level − baseline`, dB.
    pub step_db: f32,
    /// Largest region excess over the baseline seen during the episode, dB.
    pub peak_step_db: f32,
    /// Statistical uncertainty of `step_db` (run standard error ⊕ slow-floor noise), dB. The
    /// model uncertainty largely cancels in a same-gain difference.
    pub step_uncertainty_db: f32,
    /// Model uncertainty of each absolute level, dB.
    pub uncertainty_db: f32,
    /// Region mean spectral kurtosis over the run (`None` without SK).
    pub sk: Option<f32>,
    /// Frame-to-frame standard deviation of the block excess over the run, dB.
    pub excess_std_db: f32,
    /// The baseline was quantisation-limited (a rise is then overstated relative to RF).
    pub quantisation_limited_before: bool,
    /// Receiver state.
    pub gain: GainKey,
    /// Segment of the episode.
    pub segment: u64,
    /// Provenance of the frame that produced this event.
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
    /// Per-frame per-bin FCME floor, linear FS²/Hz.
    pub floor: Vec<f32>,
    /// Wide-signal reference, linear FS²/Hz: the detection reference (see [`FloorKind::Wide`]).
    pub wide_floor: Vec<f32>,
    /// Median of the per-frame block floors, linear FS²/Hz.
    pub band_floor: f32,
    /// Per-frame block floors (see [`NoiseFloorTracker::layout`]); invalid blocks hold the
    /// nearest valid block's value.
    pub block_floor: Vec<f32>,
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
    /// Broadband impulsive frame: does not update the slow floor.
    pub impulsive: bool,
    /// This frame ended a sustained flagged run: the gate released and re-seeded.
    pub gate_released: bool,
    /// Band floor over its running median, dB (the gate statistic).
    pub impulsive_excess_db: f32,
    /// Active floor-change episodes (all classes).
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
            band_floor: 0.0,
            block_floor: vec![0.0; blocks],
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
    /// `Rise` events emitted.
    pub rise_events: u64,
    /// Episodes ended (all classes, returned or reset).
    pub episode_ends: u64,
    /// `Fall` events emitted.
    pub level_falls: u64,
    /// Interrupted falls adopted silently (the slow floor had followed an intermittent signal).
    pub floor_corrections: u64,
    /// Episodes judged structured.
    pub structured_episodes: u64,
}

type ResolutionKey = (usize, usize, u32, WindowKind);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BlockState {
    Idle,
    Holdoff(u64),
    Up,
    Down,
    Episode(usize),
}

#[derive(Clone, Copy, Debug)]
struct RunAcc {
    onset_seq: u64,
    onset_t: SampleTime,
    frames: u64,
    used: u64,
    sum_floor: f64,
    sum_ex: f64,
    sum_ex2: f64,
    sum_sk: f64,
    sk_n: u64,
    /// Frame counter of the last frame beyond the threshold in the run's direction.
    last_hit: u64,
    /// A fall run survived frames back within the threshold.
    interrupted: bool,
}

impl RunAcc {
    fn start(seq: u64, t: SampleTime, frames: u64, counter: u64) -> Self {
        Self {
            onset_seq: seq,
            onset_t: t,
            frames,
            used: 0,
            sum_floor: 0.0,
            sum_ex: 0.0,
            sum_ex2: 0.0,
            sum_sk: 0.0,
            sk_n: 0,
            last_hit: counter,
            interrupted: false,
        }
    }
}

#[derive(Clone, Debug)]
struct Episode {
    active: bool,
    id: u64,
    class: FloorChangeClass,
    emitted: bool,
    b0: usize,
    b1: usize,
    bins: Range<usize>,
    f_lo_hz: f64,
    f_hi_hz: f64,
    band_fraction: f32,
    fs: f64,
    onset_t: SampleTime,
    baseline_db: f32,
    level_db: f32,
    step_db: f32,
    peak_step_db: f32,
    step_unc_db: f32,
    sk: Option<f32>,
    excess_std_db: f32,
    q_limited_before: bool,
    segment: u64,
    gain: GainKey,
    end_frames: u64,
    end_onset: (u64, SampleTime),
}

impl Episode {
    fn inactive(t: SampleTime, gain: GainKey) -> Self {
        Self {
            active: false,
            id: 0,
            class: FloorChangeClass::Unverified,
            emitted: false,
            b0: 0,
            b1: 0,
            bins: 0..0,
            f_lo_hz: 0.0,
            f_hi_hz: 0.0,
            band_fraction: 0.0,
            fs: 1.0,
            onset_t: t,
            baseline_db: 0.0,
            level_db: 0.0,
            step_db: 0.0,
            peak_step_db: 0.0,
            step_unc_db: 0.0,
            sk: None,
            excess_std_db: 0.0,
            q_limited_before: false,
            segment: 0,
            gain,
            end_frames: 0,
            end_onset: (0, t),
        }
    }

    fn event(
        &self,
        kind: FloorEventKind,
        end_reason: Option<EndReason>,
        onset: (u64, SampleTime),
        confirmed: (u64, SampleTime),
        uncertainty_db: f32,
        provenance: &ProvenanceHandle,
    ) -> FloorEvent {
        let secs = |a: SampleTime, b: SampleTime| {
            b.sample_index.saturating_sub(a.sample_index) as f64 / self.fs
        };
        let duration_s = match kind {
            FloorEventKind::End => secs(self.onset_t, onset.1),
            _ => secs(onset.1, confirmed.1),
        };
        FloorEvent {
            kind,
            episode: self.id,
            class: self.class,
            end_reason,
            onset_seq: onset.0,
            onset_t: onset.1,
            confirmed_seq: confirmed.0,
            confirmed_t: confirmed.1,
            episode_onset_t: self.onset_t,
            duration_s,
            bins: self.bins.clone(),
            f_lo_hz: self.f_lo_hz,
            f_hi_hz: self.f_hi_hz,
            band_fraction: self.band_fraction,
            baseline_dbfs_per_hz: self.baseline_db,
            baseline_segment: self.segment,
            level_dbfs_per_hz: self.level_db,
            step_db: self.step_db,
            peak_step_db: self.peak_step_db,
            step_uncertainty_db: self.step_unc_db,
            uncertainty_db,
            sk: self.sk,
            excess_std_db: self.excess_std_db,
            quantisation_limited_before: self.q_limited_before,
            gain: self.gain,
            segment: self.segment,
            provenance: provenance.clone(),
        }
    }
}

struct GroupStats {
    onset: (u64, SampleTime),
    baseline_db: f32,
    level_db: f32,
    step_db: f32,
    excess_std_db: f32,
    step_stat_db: f32,
    sk: Option<f32>,
}

fn db(x: f32) -> f32 {
    10.0 * x.max(1e-37).log10()
}

/// Summary of a confirmed run group (`a`, `b` are scratch of at least `run.len()`).
fn group_stats(
    run: &[RunAcc],
    slow: &[f32],
    floor: &[f32],
    a: &mut [f32],
    b: &mut [f32],
) -> GroupStats {
    let k = run.len();
    let mut onset = (run[0].onset_seq, run[0].onset_t);
    let (mut var, mut used, mut sk_s, mut sk_n) = (0.0f64, 0.0f64, 0.0f64, 0u64);
    for j in 0..k {
        let r = &run[j];
        a[j] = if r.used > 0 {
            (r.sum_floor / r.used as f64) as f32
        } else {
            floor[j]
        };
        b[j] = slow[j];
        if r.used > 0 {
            let n = r.used as f64;
            let m = r.sum_ex / n;
            var += (r.sum_ex2 / n - m * m).max(0.0);
            used += n;
        }
        sk_s += r.sum_sk;
        sk_n += r.sk_n;
        if r.onset_seq < onset.0 {
            onset = (r.onset_seq, r.onset_t);
        }
    }
    let level = median_in_place(&mut a[..k]);
    let base = median_in_place(&mut b[..k]);
    let excess_std = (var / k as f64).sqrt();
    GroupStats {
        onset,
        baseline_db: db(base),
        level_db: db(level),
        step_db: db_ratio(level, base),
        excess_std_db: excess_std as f32,
        step_stat_db: (excess_std / (used / k as f64).max(1.0).sqrt()) as f32,
        sk: (sk_n > 0).then(|| (sk_s / sk_n as f64) as f32),
    }
}

fn classify(sk: Option<f32>, excess_std_db: f32, cfg: &FloorChangeConfig) -> FloorChangeClass {
    if f64::from(excess_std_db) > cfg.max_excess_std_db {
        return FloorChangeClass::Structured;
    }
    match sk {
        None => FloorChangeClass::Unverified,
        Some(s) if (f64::from(s) - 1.0).abs() <= cfg.sk_tolerance => FloorChangeClass::NoiseLike,
        Some(_) => FloorChangeClass::Structured,
    }
}

/// Mean SK over a block's bins, when the spectrum carries SK.
fn block_sk(sk: &[f32], range: Range<usize>) -> Option<f64> {
    if sk.len() < range.end {
        return None;
    }
    let (mut s, mut n) = (0.0f64, 0u32);
    for &v in &sk[range] {
        if v.is_finite() {
            s += f64::from(v);
            n += 1;
        }
    }
    (n > 0).then(|| s / f64::from(n))
}

/// Bins covered by blocks `b0..b1`: from half a hop below the first centre to half a hop above
/// the last, extended to the span edges for the outermost blocks.
fn region_bins(layout: &BlockLayout, b0: usize, b1: usize, bins: usize) -> Range<usize> {
    let half_hop = layout.hop_bins() as f64 / 2.0;
    let lo = if b0 == 0 {
        0
    } else {
        (layout.centre(b0) - half_hop).round() as usize
    };
    let hi = if b1 == layout.count() {
        bins
    } else {
        ((layout.centre(b1 - 1) + half_hop).round() as usize).min(bins)
    };
    lo..hi.max(lo + 1)
}

fn edges(spectrum: &Spectrum, bins: &Range<usize>) -> (f64, f64) {
    let bw = spectrum.bin_width_hz();
    (
        spectrum.bin_frequency_hz(bins.start) - bw / 2.0,
        spectrum.bin_frequency_hz(bins.end - 1) + bw / 2.0,
    )
}

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
    next_episode: u64,
    alpha: f64,
    confirm_frames: u64,
    end_frames: u64,
    holdoff_frames: u64,
    gate_max_frames: u64,
    quantisation_db: Option<f64>,
    out: Option<FloorFrame>,
    state: Vec<BlockState>,
    run: Vec<RunAcc>,
    baseline: Vec<f32>,
    flag_sum: Vec<f64>,
    warm: Vec<f32>,
    warm_count: usize,
    hist: Vec<f32>,
    hist_len: usize,
    hist_pos: usize,
    hist_scratch: Vec<f32>,
    flag_vals: Vec<f32>,
    flag_len: usize,
    flag_pos: usize,
    flag_run: u64,
    episodes: Vec<Episode>,
    scratch: Vec<f32>,
    scratch2: Vec<f32>,
    slow_updates: u64,
    frame_counter: u64,
    stats: FloorStats,
}

impl NoiseFloorTracker {
    /// A tracker; buffers are sized on the first frame.
    pub fn new(config: FloorConfig) -> Result<Self, FloorConfigError> {
        config.validate()?;
        let h = config.impulsive.history_frames;
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
            next_episode: 0,
            alpha: 1.0,
            confirm_frames: 1,
            end_frames: 1,
            holdoff_frames: 0,
            gate_max_frames: 1,
            quantisation_db: None,
            out: None,
            state: Vec::new(),
            run: Vec::new(),
            baseline: Vec::new(),
            flag_sum: Vec::new(),
            warm: Vec::new(),
            warm_count: 0,
            hist: vec![0.0; h],
            hist_len: 0,
            hist_pos: 0,
            hist_scratch: vec![0.0; h],
            flag_vals: vec![0.0; h],
            flag_len: 0,
            flag_pos: 0,
            flag_run: 0,
            episodes: Vec::new(),
            scratch: Vec::new(),
            scratch2: Vec::new(),
            slow_updates: 0,
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

    /// Forces the next frame to start a new segment.
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
                out.slow_floor.resize(bins, 0.0);
                out.block_floor.resize(nb, 0.0);
                out.block_valid.resize(nb, false);
                out.block_iterations.resize(nb, 0);
                out.block_slow.resize(nb, 0.0);
            }
            None => self.out = Some(FloorFrame::new(bins, nb, frame.provenance.clone(), key)),
        }
        let w = self.config.impulsive.min_history;
        self.state.resize(nb, BlockState::Idle);
        self.run.resize(nb, RunAcc::start(0, frame.t, 0, 0));
        self.baseline.resize(nb, 0.0);
        self.flag_sum.resize(nb, 0.0);
        self.warm.resize(nb * w, 0.0);
        self.episodes.clear();
        self.episodes.resize(nb, Episode::inactive(frame.t, key));
        self.scratch.resize(nb, 0.0);
        self.scratch2.resize(nb.max(w), 0.0);
        self.layout = Some(layout);
        self.force_reset = true;
    }

    fn close_episodes(&mut self, frame: &SpectrumFrame, on_event: &mut impl FnMut(&FloorEvent)) {
        let unc = self.config.uncertainty_db;
        for ep in self.episodes.iter_mut().filter(|e| e.active) {
            ep.active = false;
            self.stats.episode_ends += 1;
            if ep.emitted {
                let now = (frame.seq, frame.t);
                let ev = ep.event(
                    FloorEventKind::End,
                    Some(EndReason::Reset),
                    now,
                    now,
                    unc,
                    &frame.provenance,
                );
                on_event(&ev);
            }
        }
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
        if self.resolution != Some(res_key)
            || self.layout.as_ref().map(BlockLayout::bins) != Some(bins)
        {
            self.close_episodes(frame, &mut on_event);
            self.configure(frame);
        }
        let key = GainKey::of(frame);
        let fs = spectrum.sample_rate_hz;
        let tol_hz = self.config.tune_tolerance_bins * spectrum.bin_width_hz();
        let reset = self.force_reset
            || !self.key.is_some_and(|k| k.matches(&key, tol_hz))
            || frame.discontinuity.bits() & self.config.reset_on.bits() != 0;
        if reset {
            self.close_episodes(frame, &mut on_event);
            let period = f64::from(r.n_avg) * r.hop() as f64 / fs;
            let frames = |s: f64| (s / period - 1e-9).ceil().max(1.0) as u64;
            let c = &self.config;
            self.alpha = -(-period / c.slow_time_constant_s).exp_m1();
            self.confirm_frames = frames(c.change.confirm_s);
            self.end_frames = frames(c.change.end_s);
            self.holdoff_frames = (c.change.holdoff_s / period - 1e-9).ceil().max(0.0) as u64;
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
            next_episode,
            alpha,
            confirm_frames,
            end_frames,
            holdoff_frames,
            gate_max_frames,
            quantisation_db,
            out,
            state,
            run,
            baseline,
            flag_sum,
            warm,
            warm_count,
            hist,
            hist_len,
            hist_pos,
            hist_scratch,
            flag_vals,
            flag_len,
            flag_pos,
            flag_run,
            episodes,
            scratch,
            scratch2,
            slow_updates,
            frame_counter,
            stats,
            ..
        } = self;
        let layout = layout.as_ref().expect("configured");
        let out = out.as_mut().expect("configured");
        let nb = layout.count();
        let psd = &spectrum.psd;

        // 1. Per-frame block FCME (invalid blocks filled from neighbours) → per-bin floor.
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
        layout.interpolate(&out.block_floor, &mut out.floor);
        scratch.copy_from_slice(&out.block_floor);
        let band_floor = median_in_place(scratch);
        let stat_block_db = if valid_blocks > 0 {
            (10.0
                / std::f64::consts::LN_10
                / (*n_eff * clean_total as f64 / valid_blocks as f64).sqrt()) as f32
        } else {
            0.0
        };

        // 2. Wide-signal reference: the per-frame floor where it is near the local minimum of
        //    block floors, the minimum elsewhere.
        sliding_min(&out.block_floor, cfg.wide.half_width_blocks, scratch);
        layout.interpolate(scratch, &mut out.wide_floor);
        let switch = 10f32.powf(cfg.wide.switch_db as f32 / 10.0);
        for (w, &f) in out.wide_floor.iter_mut().zip(&out.floor) {
            if f <= switch * *w {
                *w = f;
            }
        }
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
            flag_sum.fill(0.0);
            state.fill(BlockState::Idle);
            *slow_updates = 0;
            *seg_key = Some(key);
            *force_reset = false;
            stats.resets += 1;
        }
        out.frames_in_segment += 1;

        let w = cfg.impulsive.min_history;
        let band_db = db(band_floor);
        let thr = cfg.change.threshold_db as f32;
        let (mut impulsive, mut gate_excess, mut released) = (false, 0.0f32, false);
        if !frame_valid {
            stats.invalid_frames += 1;
        } else if *warm_count < w {
            // 5a. Warm-up: the slow floor is the per-block median of the frames so far, so one
            //     transient frame at start-up cannot seed it; the gate history fills.
            let k = *warm_count;
            warm[k * nb..(k + 1) * nb].copy_from_slice(&out.block_floor);
            for b in 0..nb {
                for j in 0..=k {
                    scratch2[j] = warm[j * nb + b];
                }
                out.block_slow[b] = median_in_place(&mut scratch2[..=k]);
            }
            push_ring(hist, hist_len, hist_pos, band_db);
            *warm_count += 1;
            *slow_updates = *warm_count as u64;
        } else {
            // 5b. Impulsive gate on the band floor against its running median.
            if *hist_len >= w {
                let h = &mut hist_scratch[..*hist_len];
                h.copy_from_slice(&hist[..*hist_len]);
                gate_excess = band_db - median_in_place(h);
            }
            impulsive = *hist_len >= w && f64::from(gate_excess) > cfg.impulsive.rise_db;
            if impulsive {
                *flag_run += 1;
                for (s, &f) in flag_sum.iter_mut().zip(&out.block_floor) {
                    *s += f64::from(f);
                }
                push_ring(flag_vals, flag_len, flag_pos, band_db);
                if *flag_run >= *gate_max_frames {
                    // Sustained: a level change. Release the gate, re-seed its history and the
                    // slow floor of blocks within the change threshold (larger changes go
                    // through the episode machinery).
                    let n = *flag_len;
                    hist[..n].copy_from_slice(&flag_vals[..n]);
                    *hist_len = n;
                    *hist_pos = n % hist.len();
                    for b in 0..nb {
                        let mean = (flag_sum[b] / *flag_run as f64) as f32;
                        let adoptable = matches!(
                            state[b],
                            BlockState::Idle | BlockState::Holdoff(_) | BlockState::Episode(_)
                        );
                        if adoptable && db_ratio(mean, out.block_slow[b]).abs() <= thr {
                            out.block_slow[b] = mean;
                        }
                    }
                    *flag_run = 0;
                    flag_sum.fill(0.0);
                    *flag_len = 0;
                    *flag_pos = 0;
                    impulsive = false;
                    released = true;
                    stats.gate_releases += 1;
                }
            } else {
                if *flag_run > 0 {
                    *flag_run = 0;
                    flag_sum.fill(0.0);
                    *flag_len = 0;
                    *flag_pos = 0;
                }
                push_ring(hist, hist_len, hist_pos, band_db);
            }

            // 5c. Per block: slow-floor IIR within the threshold; runs beyond it.
            let a = alpha.max(1.0 / (*slow_updates as f64 + 1.0)) as f32;
            let mut any_confirm = false;
            for b in 0..nb {
                let f = out.block_floor[b];
                let e = db_ratio(f, out.block_slow[b]);
                let mut st = state[b];
                if let BlockState::Holdoff(until) = st {
                    if *frame_counter >= until {
                        st = BlockState::Idle;
                    }
                }
                if matches!(st, BlockState::Up | BlockState::Down) {
                    let up = st == BlockState::Up;
                    if (up && e > thr) || (!up && e < -thr) {
                        let acc = &mut run[b];
                        acc.frames += 1;
                        acc.last_hit = *frame_counter;
                        if !impulsive {
                            acc.used += 1;
                            acc.sum_floor += f64::from(f);
                            acc.sum_ex += f64::from(e);
                            acc.sum_ex2 += f64::from(e) * f64::from(e);
                            if let Some(s) = block_sk(&spectrum.sk, layout.range(b)) {
                                acc.sum_sk += s;
                                acc.sk_n += 1;
                            }
                        }
                        any_confirm |= acc.frames >= *confirm_frames && acc.used > 0;
                        continue;
                    }
                    // Signals only add power: a fall survives frames back near the slow floor
                    // (an intermittent signal the slow floor was seeded on, or is following)
                    // for up to `confirm_s`, without updating the slow floor. A rise must be
                    // continuous, so bursty signals never confirm.
                    if !up && *frame_counter - run[b].last_hit <= *confirm_frames {
                        run[b].frames += 1;
                        run[b].interrupted = true;
                        continue;
                    }
                    st = BlockState::Idle;
                }
                if e.abs() <= thr {
                    if !impulsive {
                        let s = &mut out.block_slow[b];
                        *s += a * (f - *s);
                    }
                } else if st == BlockState::Idle {
                    st = if e > 0.0 {
                        BlockState::Up
                    } else {
                        BlockState::Down
                    };
                    run[b] = RunAcc::start(frame.seq, frame.t, 1, *frame_counter);
                }
                state[b] = st;
            }
            if !impulsive {
                *slow_updates += 1;
            }

            // 5d. Confirmed runs: contiguous groups of same-direction blocks.
            if any_confirm {
                let mut b = 0;
                while b < nb {
                    let st = state[b];
                    if !matches!(st, BlockState::Up | BlockState::Down) {
                        b += 1;
                        continue;
                    }
                    let start = b;
                    let mut confirmed = false;
                    while b < nb && state[b] == st {
                        confirmed |= run[b].frames >= *confirm_frames && run[b].used > 0;
                        b += 1;
                    }
                    if !confirmed {
                        continue;
                    }
                    let end = b;
                    let g = group_stats(
                        &run[start..end],
                        &out.block_slow[start..end],
                        &out.block_floor[start..end],
                        scratch,
                        scratch2,
                    );
                    let class = classify(g.sk, g.excess_std_db, &cfg.change);
                    let step_unc_db = f64::from(g.step_stat_db)
                        .hypot(f64::from(stat_block_db) * (*alpha / (2.0 - *alpha)).sqrt())
                        as f32;
                    let q_before = quantisation_db
                        .is_some_and(|q| f64::from(g.baseline_db) < q + cfg.quantisation_margin_db);
                    let level = |acc: &RunAcc, f: f32| {
                        if acc.used > 0 {
                            (acc.sum_floor / acc.used as f64) as f32
                        } else {
                            f
                        }
                    };
                    let bins_r = region_bins(layout, start, end, bins);
                    let (f_lo_hz, f_hi_hz) = edges(spectrum, &bins_r);
                    if st == BlockState::Up {
                        for j in start..end {
                            baseline[j] = out.block_slow[j];
                            if class != FloorChangeClass::Structured {
                                out.block_slow[j] = level(&run[j], out.block_floor[j]);
                            }
                        }
                        let adjacent = |j: usize| match state[j] {
                            BlockState::Episode(i) if episodes[i].class == class => Some(i),
                            _ => None,
                        };
                        let merge = start
                            .checked_sub(1)
                            .and_then(adjacent)
                            .or_else(|| (end < nb).then(|| adjacent(end)).flatten());
                        let idx = if let Some(i) = merge {
                            let ep = &mut episodes[i];
                            ep.b0 = ep.b0.min(start);
                            ep.b1 = ep.b1.max(end);
                            ep.bins = region_bins(layout, ep.b0, ep.b1, bins);
                            (ep.f_lo_hz, ep.f_hi_hz) = edges(spectrum, &ep.bins);
                            ep.band_fraction = ep.bins.len() as f32 / bins as f32;
                            i
                        } else {
                            let i = episodes
                                .iter()
                                .position(|e| !e.active)
                                .expect("one slot per block");
                            let emitted =
                                class != FloorChangeClass::Structured || cfg.change.emit_structured;
                            let ep = &mut episodes[i];
                            *ep = Episode {
                                active: true,
                                id: *next_episode,
                                class,
                                emitted,
                                b0: start,
                                b1: end,
                                band_fraction: bins_r.len() as f32 / bins as f32,
                                bins: bins_r,
                                f_lo_hz,
                                f_hi_hz,
                                fs,
                                onset_t: g.onset.1,
                                baseline_db: g.baseline_db,
                                level_db: g.level_db,
                                step_db: g.step_db,
                                peak_step_db: g.step_db,
                                step_unc_db,
                                sk: g.sk,
                                excess_std_db: g.excess_std_db,
                                q_limited_before: q_before,
                                segment: out.segment,
                                gain: seg_key.unwrap_or(key),
                                end_frames: 0,
                                end_onset: (frame.seq, frame.t),
                            };
                            *next_episode += 1;
                            if class == FloorChangeClass::Structured {
                                stats.structured_episodes += 1;
                            }
                            if emitted {
                                let ev = ep.event(
                                    FloorEventKind::Rise,
                                    None,
                                    g.onset,
                                    (frame.seq, frame.t),
                                    cfg.uncertainty_db,
                                    &frame.provenance,
                                );
                                on_event(&ev);
                                stats.rise_events += 1;
                            }
                            i
                        };
                        for s in &mut state[start..end] {
                            *s = BlockState::Episode(idx);
                        }
                    } else {
                        let mut interrupted = false;
                        for j in start..end {
                            out.block_slow[j] = level(&run[j], out.block_floor[j]);
                            state[j] = BlockState::Holdoff(*frame_counter + *holdoff_frames);
                            interrupted |= run[j].interrupted;
                        }
                        if interrupted {
                            // The slow floor was above the floor seen between signal bursts:
                            // a correction, not a change of the floor.
                            stats.floor_corrections += 1;
                            continue;
                        }
                        let mut ep = Episode::inactive(g.onset.1, seg_key.unwrap_or(key));
                        ep.id = *next_episode;
                        ep.class = class;
                        ep.band_fraction = bins_r.len() as f32 / bins as f32;
                        ep.bins = bins_r;
                        (ep.f_lo_hz, ep.f_hi_hz) = (f_lo_hz, f_hi_hz);
                        ep.fs = fs;
                        ep.baseline_db = g.baseline_db;
                        ep.level_db = g.level_db;
                        ep.step_db = g.step_db;
                        ep.peak_step_db = g.step_db;
                        ep.step_unc_db = step_unc_db;
                        ep.sk = g.sk;
                        ep.excess_std_db = g.excess_std_db;
                        ep.q_limited_before = q_before;
                        ep.segment = out.segment;
                        *next_episode += 1;
                        let ev = ep.event(
                            FloorEventKind::Fall,
                            None,
                            g.onset,
                            (frame.seq, frame.t),
                            cfg.uncertainty_db,
                            &frame.provenance,
                        );
                        on_event(&ev);
                        stats.level_falls += 1;
                    }
                }
            }

            // 5e. Episode ends: the region median of floor/baseline back within the end threshold.
            let end_thr = cfg.change.end_threshold_db as f32;
            for ep in episodes.iter_mut().filter(|e| e.active) {
                let (b0, b1) = (ep.b0, ep.b1);
                for (k, j) in (b0..b1).enumerate() {
                    scratch[k] = db_ratio(out.block_floor[j], baseline[j]);
                }
                let ex = median_in_place(&mut scratch[..b1 - b0]);
                if !impulsive {
                    ep.peak_step_db = ep.peak_step_db.max(ex);
                }
                if ex >= end_thr {
                    ep.end_frames = 0;
                    continue;
                }
                if ep.end_frames == 0 {
                    ep.end_onset = (frame.seq, frame.t);
                    for acc in &mut run[b0..b1] {
                        *acc = RunAcc::start(frame.seq, frame.t, 0, *frame_counter);
                    }
                }
                ep.end_frames += 1;
                if !impulsive {
                    for (acc, &f) in run[b0..b1].iter_mut().zip(&out.block_floor[b0..b1]) {
                        acc.used += 1;
                        acc.sum_floor += f64::from(f);
                    }
                }
                if ep.end_frames >= *end_frames && run[b0].used > 0 {
                    for j in b0..b1 {
                        out.block_slow[j] = (run[j].sum_floor / run[j].used as f64) as f32;
                        state[j] = BlockState::Holdoff(*frame_counter + *holdoff_frames);
                    }
                    ep.active = false;
                    stats.episode_ends += 1;
                    if ep.emitted {
                        let ev = ep.event(
                            FloorEventKind::End,
                            Some(EndReason::Returned),
                            ep.end_onset,
                            (frame.seq, frame.t),
                            cfg.uncertainty_db,
                            &frame.provenance,
                        );
                        on_event(&ev);
                    }
                }
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
        out.gain = seg_key.unwrap_or(key);
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
        out.active_episodes = episodes.iter().filter(|e| e.active).count() as u32;
        *frame_counter += 1;
        stats.frames += 1;
        if impulsive {
            stats.impulsive_frames += 1;
        }
        out
    }
}
