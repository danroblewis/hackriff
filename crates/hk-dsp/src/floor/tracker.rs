//! The streaming noise-floor tracker: one [`FloorFrame`] per [`SpectrumFrame`]. See the
//! [module docs](super).

use std::ops::Range;

use hk_core::{Discontinuity, ProvenanceHandle};
use hk_model::SampleTime;

use super::{
    AveragingModel, BlockConfig, BlockFcme, BlockLayout, BlockPercentile, FcmeConfig,
    FloorConfigError, FloorMethod, PercentileConfig, QuantisationFloor, check_positive,
    check_probability, db_ratio, gamma, median_in_place, occupancy_from_count,
};
use crate::stft::{DEFAULT_RESET_ON, SpectrumFrame};
use crate::window::WindowKind;

/// Impulsive-frame gate settings (S4 §5).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ImpulsiveGateConfig {
    /// Flag when the band-median excess rises this far above its running median, dB (0.5).
    pub rise_db: f64,
    /// Running-median history, unflagged frames (64).
    pub history_frames: usize,
    /// Frames of history needed before flagging; also the slow floor's warm-up (8).
    pub min_history: usize,
}

impl Default for ImpulsiveGateConfig {
    fn default() -> Self {
        Self {
            rise_db: 0.5,
            history_frames: 64,
            min_history: 8,
        }
    }
}

/// Floor-rise (level-change) detection settings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FloorRiseConfig {
    /// A block is elevated when its per-frame floor is this far above its slow floor, dB (3).
    pub threshold_db: f64,
    /// Consecutive elevated frames that confirm a level change (5). Impulsive bursts shorter
    /// than this never become events.
    pub confirm_frames: usize,
}

impl Default for FloorRiseConfig {
    fn default() -> Self {
        Self {
            threshold_db: 3.0,
            confirm_frames: 5,
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
    /// Floor-rise events.
    pub rise: FloorRiseConfig,
    /// Reported model uncertainty, dB (±0.5).
    pub uncertainty_db: f32,
    /// ADC quantisation floor (S4 HackRF One).
    pub quantisation: QuantisationFloor,
    /// `quantisation_limited` when the band floor is within this of the quantisation floor, dB (3).
    pub quantisation_margin_db: f64,
    /// Frame discontinuities that start a new segment (default: as the STFT's reset flags).
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
            rise: FloorRiseConfig::default(),
            uncertainty_db: 0.5,
            quantisation: QuantisationFloor::HACKRF_ONE_S4,
            quantisation_margin_db: 3.0,
            reset_on: DEFAULT_RESET_ON,
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
        check_positive("impulsive.rise_db", self.impulsive.rise_db)?;
        check_positive(
            "impulsive.history_frames",
            self.impulsive.history_frames as f64,
        )?;
        if self.impulsive.min_history > self.impulsive.history_frames {
            return Err(FloorConfigError::Fraction {
                name: "impulsive.min_history / history_frames",
                value: self.impulsive.min_history as f64 / self.impulsive.history_frames as f64,
            });
        }
        check_positive("rise.threshold_db", self.rise.threshold_db)?;
        check_positive("rise.confirm_frames", self.rise.confirm_frames as f64)?;
        check_positive("uncertainty_db", f64::from(self.uncertainty_db))
    }
}

/// The state an estimate is keyed by. Any change starts a new segment.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GainKey {
    /// RF centre, Hz.
    pub center_hz: f64,
    /// Sample rate, Hz.
    pub sample_rate_hz: f64,
    /// LNA gain, dB.
    pub lna_db: f64,
    /// VGA gain, dB.
    pub vga_db: f64,
    /// RF amp on.
    pub amp_on: bool,
    /// FFT length.
    pub fft_len: usize,
    /// Segment overlap, samples.
    pub overlap: usize,
    /// Segments per frame `K`.
    pub n_avg: u32,
    /// Window.
    pub window: WindowKind,
}

impl GainKey {
    /// The key of a frame (gains from its provenance, geometry from its spectrum).
    pub fn of(frame: &SpectrumFrame) -> Self {
        let tune = &frame.provenance.tune;
        let r = &frame.spectrum.resolution;
        Self {
            center_hz: frame.spectrum.f_center_hz,
            sample_rate_hz: frame.spectrum.sample_rate_hz,
            lna_db: tune.lna_db,
            vga_db: tune.vga_db,
            amp_on: tune.amp_on,
            fft_len: r.fft_len,
            overlap: r.overlap,
            n_avg: r.n_avg,
            window: r.window,
        }
    }
}

/// Which tracker floor to read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FloorKind {
    /// Per-frame FCME floor: the detection reference.
    Frame,
    /// Slow floor: science series and integrated detection.
    Slow,
}

/// The percentile cross-check for one frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PercentileCheck {
    /// Median of the block percentile floors, linear FS²/Hz.
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
    /// Within the quantisation margin of the ADC floor.
    pub quantisation_limited: bool,
}

/// A confirmed rise of the slow floor (AWARE-006). T-020 maps it to an Anomaly `noise-floor-rise`.
#[derive(Clone, Debug, PartialEq)]
pub struct FloorRiseEvent {
    /// Frame counter of the first elevated frame.
    pub start_seq: u64,
    /// Time of the first elevated frame (the rise happened within that frame).
    pub t: SampleTime,
    /// Frame that confirmed the change.
    pub confirmed_seq: u64,
    /// Its time.
    pub confirmed_t: SampleTime,
    /// Affected bins.
    pub bins: Range<usize>,
    /// Lower edge, Hz.
    pub f_lo_hz: f64,
    /// Upper edge, Hz.
    pub f_hi_hz: f64,
    /// Rise, dB (median over the affected blocks).
    pub step_db: f32,
    /// Slow floor before, dBFS/Hz (median over the affected blocks).
    pub floor_before_dbfs_per_hz: f32,
    /// Floor after (`before + step`), dBFS/Hz.
    pub floor_after_dbfs_per_hz: f32,
    /// Model uncertainty of each level, dB.
    pub uncertainty_db: f32,
    /// The floor before was quantisation-limited (the step is then understated).
    pub quantisation_limited_before: bool,
    /// Gain state.
    pub gain: GainKey,
    /// Segment.
    pub segment: u64,
    /// Provenance of the confirming frame.
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
    /// Gain state.
    pub gain: GainKey,
    /// Segment counter (increments on every reset).
    pub segment: u64,
    /// Frames in this segment including this one.
    pub frames_in_segment: u64,
    /// This frame started a segment.
    pub reset: bool,
    /// Estimator behind `floor`/`slow_floor`.
    pub method: FloorMethod,
    /// Gamma shape used.
    pub n_avg_effective: f64,
    /// RF centre, Hz.
    pub f_center_hz: f64,
    /// Bin spacing, Hz.
    pub bin_width_hz: f64,
    /// Per-frame per-bin floor, linear FS²/Hz: the detection reference.
    pub floor: Vec<f32>,
    /// Median of the per-frame block floors, linear FS²/Hz.
    pub band_floor: f32,
    /// Per-frame block floors (see [`NoiseFloorTracker::layout`]), linear FS²/Hz.
    pub block_floor: Vec<f32>,
    /// Slow per-bin floor, linear FS²/Hz.
    pub slow_floor: Vec<f32>,
    /// Median of the slow block floors, linear FS²/Hz.
    pub slow_band_floor: f32,
    /// Slow block floors, linear FS²/Hz.
    pub block_slow: Vec<f32>,
    /// The slow floor has warmed up in this segment.
    pub slow_ready: bool,
    /// Occupied fraction: bins above `Q⁻¹(n, occupancy_pfa)/n · min(floor, band_floor)`, less the
    /// expected noise exceedance.
    pub occupancy: f32,
    /// Percentile cross-check, when configured.
    pub percentile: Option<PercentileCheck>,
    /// Model uncertainty, dB.
    pub uncertainty_db: f32,
    /// Statistical standard error of a block floor, dB (`4.34/√(n·clean bins)`).
    pub statistical_uncertainty_db: f32,
    /// ADC quantisation floor at this sample rate, dBFS/Hz.
    pub quantisation_floor_dbfs_per_hz: Option<f32>,
    /// Margin for `quantisation_limited`, dB.
    pub quantisation_margin_db: f32,
    /// Band floor within the margin of the quantisation floor.
    pub quantisation_limited: bool,
    /// Broadband impulsive frame: did not update the slow floor.
    pub impulsive: bool,
    /// Band-median excess over its running median, dB (the gate statistic).
    pub impulsive_excess_db: f32,
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
            floor: vec![0.0; bins],
            band_floor: 0.0,
            block_floor: vec![0.0; blocks],
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
            impulsive_excess_db: 0.0,
        }
    }

    /// The per-bin trace of `kind`.
    pub fn trace(&self, kind: FloorKind) -> &[f32] {
        match kind {
            FloorKind::Frame => &self.floor,
            FloorKind::Slow => &self.slow_floor,
        }
    }

    /// The band floor of `kind`, dBFS/Hz.
    pub fn band_floor_dbfs_per_hz(&self, kind: FloorKind) -> f32 {
        let v = match kind {
            FloorKind::Frame => self.band_floor,
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
    /// Frames flagged impulsive.
    pub impulsive_frames: u64,
    /// Floor-rise events emitted.
    pub rise_events: u64,
    /// Confirmed floor falls (re-seeded without an event).
    pub level_falls: u64,
}

type ResolutionKey = (usize, usize, u32, WindowKind);

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
    alpha: f64,
    quantisation_db: Option<f64>,
    out: Option<FloorFrame>,
    excess: Vec<f32>,
    run_dir: Vec<i8>,
    run_len: Vec<u32>,
    run_start: Vec<(u64, SampleTime)>,
    run_hist: Vec<f32>,
    scratch: Vec<f32>,
    scratch2: Vec<f32>,
    small: Vec<f32>,
    band_hist: Vec<f32>,
    band_hist_len: usize,
    band_hist_pos: usize,
    hist_scratch: Vec<f32>,
    slow_updates: u64,
    stats: FloorStats,
}

impl NoiseFloorTracker {
    /// A tracker; buffers are sized on the first frame.
    pub fn new(config: FloorConfig) -> Result<Self, FloorConfigError> {
        config.validate()?;
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
            alpha: 1.0,
            quantisation_db: None,
            out: None,
            excess: Vec::new(),
            run_dir: Vec::new(),
            run_len: Vec::new(),
            run_start: Vec::new(),
            run_hist: Vec::new(),
            scratch: Vec::new(),
            scratch2: Vec::new(),
            small: vec![0.0; config.rise.confirm_frames],
            band_hist: vec![0.0; config.impulsive.history_frames],
            band_hist_len: 0,
            band_hist_pos: 0,
            hist_scratch: vec![0.0; config.impulsive.history_frames],
            slow_updates: 0,
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
                out.slow_floor.resize(bins, 0.0);
                out.block_floor.resize(nb, 0.0);
                out.block_slow.resize(nb, 0.0);
            }
            None => self.out = Some(FloorFrame::new(bins, nb, frame.provenance.clone(), key)),
        }
        let c = self.config.rise.confirm_frames;
        self.excess.resize(nb, 0.0);
        self.run_dir.resize(nb, 0);
        self.run_len.resize(nb, 0);
        self.run_start.resize(nb, (0, frame.t));
        self.run_hist.resize(nb * c, 0.0);
        self.scratch.resize(nb, 0.0);
        self.scratch2.resize(nb, 0.0);
        self.layout = Some(layout);
        self.force_reset = true;
    }

    /// Folds in one spectrum frame; calls `on_rise` for each confirmed floor rise and returns the
    /// frame's estimate.
    pub fn update(
        &mut self,
        frame: &SpectrumFrame,
        mut on_rise: impl FnMut(&FloorRiseEvent),
    ) -> &FloorFrame {
        let spectrum = &frame.spectrum;
        let bins = spectrum.bins();
        assert!(bins > 0, "empty spectrum");
        let r = &spectrum.resolution;
        let res_key = (r.fft_len, r.overlap, r.n_avg, r.window);
        if self.resolution != Some(res_key)
            || self.layout.as_ref().map(BlockLayout::bins) != Some(bins)
        {
            self.configure(frame);
        }
        let key = GainKey::of(frame);
        let reset = self.force_reset
            || self.key != Some(key)
            || frame.discontinuity.bits() & self.config.reset_on.bits() != 0;
        if reset {
            let fs = spectrum.sample_rate_hz;
            let period = f64::from(r.n_avg) * r.hop() as f64 / fs;
            self.alpha = -(-period / self.config.slow_time_constant_s).exp_m1();
            self.quantisation_db = self.config.quantisation.dbfs_per_hz(fs);
        }

        let layout = self.layout.as_ref().expect("configured");
        let out = self.out.as_mut().expect("configured");
        let nb = layout.count();
        let psd = &spectrum.psd;
        let cfg = &self.config;

        // 1. Per-frame block FCME → per-bin floor.
        let mut clean_total = 0usize;
        for (b, f) in out.block_floor.iter_mut().enumerate() {
            let est = self.fcme.estimate_block(&psd[layout.range(b)]);
            *f = est.floor as f32;
            clean_total += est.clean;
        }
        layout.interpolate(&out.block_floor, &mut out.floor);
        self.scratch.copy_from_slice(&out.block_floor);
        let band_floor = median_in_place(&mut self.scratch);
        // Occupancy against min(per-bin floor, band floor): a block filled by a wide signal
        // reads that signal as its local floor, which would hide its bins.
        let occupancy = {
            let m = self.occupancy_multiplier as f32;
            let above = psd
                .iter()
                .zip(&out.floor)
                .filter(|&(&p, &f)| p > m * f.min(band_floor))
                .count();
            occupancy_from_count(above, bins, cfg.occupancy_pfa) as f32
        };

        // 2. Percentile cross-check.
        out.percentile = match &mut self.percentile {
            Some(p) => {
                let mut occ = 0.0;
                for (b, s) in self.scratch.iter_mut().enumerate() {
                    let est = p.estimate_block(&psd[layout.range(b)]);
                    *s = est.floor as f32;
                    occ += est.occupancy;
                }
                let occ = (occ / nb as f64) as f32;
                let band = median_in_place(&mut self.scratch);
                let max = p.config().max_occupancy as f32;
                Some(PercentileCheck {
                    band_floor: band,
                    delta_db: db_ratio(band, band_floor),
                    occupancy: occ,
                    valid: occ.max(occupancy) < max,
                })
            }
            None => None,
        };

        // 3. Segment reset.
        if reset {
            out.segment = self.next_segment;
            self.next_segment += 1;
            out.frames_in_segment = 0;
            out.block_slow.copy_from_slice(&out.block_floor);
            self.run_dir.fill(0);
            self.run_len.fill(0);
            self.band_hist_len = 0;
            self.band_hist_pos = 0;
            self.slow_updates = 1;
            self.key = Some(key);
            self.force_reset = false;
            self.stats.resets += 1;
        }

        // 4. Impulsive-frame gate on the band median of block floor / slow floor.
        for ((e, &f), &s) in self
            .excess
            .iter_mut()
            .zip(&out.block_floor)
            .zip(&out.block_slow)
        {
            *e = db_ratio(f, s);
        }
        self.scratch.copy_from_slice(&self.excess);
        let e_band = median_in_place(&mut self.scratch);
        let ready = self.band_hist_len >= cfg.impulsive.min_history;
        let gate_excess = if ready {
            let h = &mut self.hist_scratch[..self.band_hist_len];
            h.copy_from_slice(&self.band_hist[..self.band_hist_len]);
            e_band - median_in_place(h)
        } else {
            0.0
        };
        let impulsive = !reset && ready && f64::from(gate_excess) > cfg.impulsive.rise_db;
        if !impulsive {
            let cap = self.band_hist.len();
            self.band_hist[self.band_hist_pos] = e_band;
            self.band_hist_pos = (self.band_hist_pos + 1) % cap;
            self.band_hist_len = (self.band_hist_len + 1).min(cap);
        }

        // 5. Slow floor update and level-change runs.
        let c = cfg.rise.confirm_frames;
        let thr = cfg.rise.threshold_db as f32;
        let alpha = self.alpha.max(1.0 / (self.slow_updates as f64 + 1.0)) as f32;
        let mut any_confirmed = false;
        if !reset {
            for b in 0..nb {
                let e = self.excess[b];
                let dir: i8 = if e > thr {
                    1
                } else if e < -thr {
                    -1
                } else {
                    0
                };
                if dir == 0 {
                    self.run_dir[b] = 0;
                    self.run_len[b] = 0;
                    if !impulsive {
                        let s = &mut out.block_slow[b];
                        *s += alpha * (out.block_floor[b] - *s);
                    }
                } else {
                    if self.run_dir[b] != dir {
                        self.run_dir[b] = dir;
                        self.run_len[b] = 0;
                        self.run_start[b] = (frame.seq, frame.t);
                    }
                    self.run_hist[b * c + self.run_len[b] as usize % c] = out.block_floor[b];
                    self.run_len[b] += 1;
                    any_confirmed |= self.run_len[b] as usize >= c;
                }
            }
            if !impulsive {
                self.slow_updates += 1;
            }
        }

        // 6. Confirmed level changes: re-seed contiguous elevated blocks, emit rises.
        if any_confirmed {
            let mut b = 0;
            while b < nb {
                let dir = self.run_dir[b];
                if dir == 0 {
                    b += 1;
                    continue;
                }
                let start = b;
                let mut confirmed = false;
                while b < nb && self.run_dir[b] == dir {
                    confirmed |= self.run_len[b] as usize >= c;
                    b += 1;
                }
                if !confirmed {
                    continue;
                }
                let end = b;
                let mut first = self.run_start[start];
                for j in start..end {
                    let n = (self.run_len[j] as usize).min(c);
                    let h = &mut self.small[..n];
                    h.copy_from_slice(&self.run_hist[j * c..j * c + n]);
                    let level = median_in_place(h);
                    let before = out.block_slow[j];
                    self.scratch[j - start] = db_ratio(level, before);
                    self.scratch2[j - start] = before;
                    out.block_slow[j] = level;
                    self.run_dir[j] = 0;
                    self.run_len[j] = 0;
                    if self.run_start[j].0 < first.0 {
                        first = self.run_start[j];
                    }
                }
                if dir > 0 {
                    let k = end - start;
                    let step_db = median_in_place(&mut self.scratch[..k]);
                    let before_db =
                        10.0 * median_in_place(&mut self.scratch2[..k]).max(1e-37).log10();
                    let half_hop = layout.hop_bins() as f64 / 2.0;
                    let lo = if start == 0 {
                        0
                    } else {
                        (layout.centre(start) - half_hop).round() as usize
                    };
                    let hi = if end == nb {
                        bins
                    } else {
                        ((layout.centre(end - 1) + half_hop).round() as usize).min(bins)
                    };
                    let bw = spectrum.bin_width_hz();
                    let event = FloorRiseEvent {
                        start_seq: first.0,
                        t: first.1,
                        confirmed_seq: frame.seq,
                        confirmed_t: frame.t,
                        bins: lo..hi,
                        f_lo_hz: spectrum.bin_frequency_hz(lo) - bw / 2.0,
                        f_hi_hz: spectrum.bin_frequency_hz(hi - 1) + bw / 2.0,
                        step_db,
                        floor_before_dbfs_per_hz: before_db,
                        floor_after_dbfs_per_hz: before_db + step_db,
                        uncertainty_db: cfg.uncertainty_db,
                        quantisation_limited_before: self
                            .quantisation_db
                            .is_some_and(|q| f64::from(before_db) < q + cfg.quantisation_margin_db),
                        gain: key,
                        segment: out.segment,
                        provenance: frame.provenance.clone(),
                    };
                    on_rise(&event);
                    self.stats.rise_events += 1;
                } else {
                    self.stats.level_falls += 1;
                }
            }
            self.band_hist_len = 0;
            self.band_hist_pos = 0;
        }

        // 7. Slow per-bin floor and frame metadata.
        layout.interpolate(&out.block_slow, &mut out.slow_floor);
        self.scratch.copy_from_slice(&out.block_slow);
        out.slow_band_floor = median_in_place(&mut self.scratch);
        out.seq = frame.seq;
        out.t = frame.t;
        if out.provenance != frame.provenance {
            out.provenance = frame.provenance.clone();
        }
        out.gain = key;
        out.reset = reset;
        out.frames_in_segment += 1;
        out.n_avg_effective = self.n_eff;
        out.f_center_hz = spectrum.f_center_hz;
        out.bin_width_hz = spectrum.bin_width_hz();
        out.band_floor = band_floor;
        out.occupancy = occupancy;
        out.slow_ready = out.frames_in_segment >= cfg.impulsive.min_history as u64;
        out.uncertainty_db = cfg.uncertainty_db;
        let mean_clean = clean_total as f64 / nb as f64;
        out.statistical_uncertainty_db =
            (10.0 / std::f64::consts::LN_10 / (self.n_eff * mean_clean).sqrt()) as f32;
        out.quantisation_floor_dbfs_per_hz = self.quantisation_db.map(|q| q as f32);
        out.quantisation_margin_db = cfg.quantisation_margin_db as f32;
        out.quantisation_limited = self.quantisation_db.is_some_and(|q| {
            f64::from(10.0 * band_floor.max(1e-37).log10()) < q + cfg.quantisation_margin_db
        });
        out.impulsive = impulsive;
        out.impulsive_excess_db = gate_excess;
        self.stats.frames += 1;
        if impulsive {
            self.stats.impulsive_frames += 1;
        }
        out
    }
}
