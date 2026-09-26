//! [`FloorProduct`]: ingest (calibrate, fold, log state) and the floor-vs-time query.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use hk_dsp::SpectrumFrame;
use hk_dsp::floor::FloorFrame;
use hk_dsp::radiometry::{
    FloorFlags, PowerCalibrations, cell_value_sd_db, cell_value_shape, db_above_kt0,
    exact_percentile_probability, frame_flags, frame_period_ns, gain_setting_of,
    noise_temperature_k, percentile_bias_db, same_gain,
};
use hk_dsp::window::WindowKind;
use hk_model::{CalibrationStateId, FreqRange, GainSetting, PowerUnit, TimeRange, Timestamp};

use super::runs::{FloorRun, RunLog};
use crate::history::{
    CellStats, FrameInput, IngestOutcome, ProvenanceSummary, Pyramid, PyramidConfig, RegionQuery,
    Resolution, StoreError,
};

const META: &str = "product.txt";

/// Flags describing frame state, carried from runs into steps.
const STATE_FLAGS: FloorFlags = FloorFlags::from_bits_truncate(
    FloorFlags::QUANTISATION_LIMITED.bits()
        | FloorFlags::IMPULSIVE_PAUSED.bits()
        | FloorFlags::GATE_RELEASED.bits()
        | FloorFlags::SEGMENT_START.bits()
        | FloorFlags::NOT_READY.bits()
        | FloorFlags::INVALID.bits()
        | FloorFlags::EPISODE.bits()
        | FloorFlags::CAL_EDGE_HELD.bits(),
);

/// Standard error of a median relative to the sample standard deviation (normal data): `√(π/2)`.
const MEDIAN_SE_FACTOR: f64 = 1.253_314;

/// MAD → standard deviation for normal data.
const MAD_TO_SD: f64 = 1.482_6;

/// [`FloorProduct`] settings.
#[derive(Clone, Debug, PartialEq)]
pub struct FloorProductConfig {
    /// Pyramid geometry for both pyramids (their `unit` is set per pyramid).
    pub pyramid: PyramidConfig,
    /// `σ_model` of the percentile estimator, dB (0.5, as T-005's floor model uncertainty).
    pub model_uncertainty_db: f64,
    /// Longest run in the state log (60 s, the pyramid checkpoint interval).
    pub max_run: Duration,
    /// Frames further apart than this many periods break a run (1.5).
    pub gap_factor: f64,
    /// Frames whose cell shape differs from the product's by more than this fraction are
    /// rejected (0.05), unless `mixed_shapes`.
    pub shape_tolerance: f64,
    /// T-139: fold frames of another cell shape (e.g. a scheduler's short-step history rows with
    /// fewer averages) instead of rejecting them (false). Tiles record whether their shape is
    /// uniform, and [`FloorProduct::floor_vs_time`] decides per tile from that persisted record: a
    /// uniform tile's cells use their own shape's bias, a mixed-shape tile's cells the bias of the
    /// Gamma mixture of its values (T-141; no floor for a mixed tile of format < 4).
    pub mixed_shapes: bool,
}

/// Resolutions whose cell shape is cached before the cache is cleared.
const SHAPE_CACHE_MAX: usize = 16;

/// A uniform tile's stored `floor_db` and `p_low_db` are each rounded to 0.01 dB, so its own bias
/// is known to about this much; within it the product shape's exact bias is used.
const STORED_BIAS_ROUNDING_DB: f64 = 0.015;

impl Default for FloorProductConfig {
    fn default() -> Self {
        Self {
            pyramid: PyramidConfig::default(),
            model_uncertainty_db: 0.5,
            max_run: Duration::from_secs(60),
            gap_factor: 1.5,
            shape_tolerance: 0.05,
            mixed_shapes: false,
        }
    }
}

/// Counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FloorProductStats {
    /// Frames folded into the calibrated (dBm) pyramid.
    pub calibrated_frames: u64,
    /// Frames folded into the uncalibrated (dBFS) pyramid.
    pub uncalibrated_frames: u64,
    /// Frames whose tile had already sealed.
    pub late_frames: u64,
    /// Frames rejected (mismatched floor frame, geometry, or malformed).
    pub rejected_frames: u64,
    /// T-139: frames folded with a cell shape other than the product's (`mixed_shapes`).
    pub mixed_shape_frames: u64,
}

/// What [`FloorProduct::ingest`] did.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FloorIngest {
    /// Folded or late.
    pub outcome: IngestOutcome,
    /// Went to the calibrated pyramid.
    pub calibrated: bool,
    /// Frame flags recorded.
    pub flags: FloorFlags,
    /// Cell shape `n_c` of the frame geometry.
    pub shape: f64,
}

/// One time step of the floor-vs-time product. See the [module docs](super).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FloorStep {
    /// Step start.
    pub t: Timestamp,
    /// Step length, ns.
    pub duration_ns: i64,
    /// `Dbm` (calibrated), `Dbfs` (uncalibrated) or `None` (gap).
    pub unit: Option<PowerUnit>,
    /// The floor: median over the region's cells of the bias-corrected low percentile, per Hz.
    pub value_db_per_hz: Option<f64>,
    /// Median of the uncorrected low percentiles (what T-017 reads).
    pub raw_p_low_db_per_hz: Option<f64>,
    /// Median correction subtracted, dB (negative: a low percentile reads below the mean).
    pub bias_db: Option<f64>,
    /// Median of the cells' power means (a cross-check; biased high by any signal).
    pub mean_db_per_hz: Option<f64>,
    /// Observed cells used.
    pub cells: usize,
    /// Largest cell coverage.
    pub coverage: f32,
    /// Finest level among the cells (`u8::MAX` for a gap).
    pub level: u8,
    /// `σ_model`, dB.
    pub model_uncertainty_db: f64,
    /// `σ_cal`, dB (0 uncalibrated; NaN if the state log lost this span).
    pub calibration_uncertainty_db: f64,
    /// `σ_hist = step/√12` when any cell came from a rolled-up histogram, else 0, dB.
    pub histogram_uncertainty_db: f64,
    /// `σ_stat`, dB: `√(π/2)·1.4826·MAD/√cells` (one cell: `4.343/√n_c`).
    pub statistical_uncertainty_db: f64,
    /// `√(σ_model² + σ_cal² + σ_hist² + σ_stat²)`, dB (NaN for a gap).
    pub uncertainty_db: f64,
    /// Flags.
    pub flags: FloorFlags,
    /// Distinct gain settings seen in the step.
    pub gain_states: usize,
}

impl FloorStep {
    /// Nothing observed.
    pub fn is_gap(&self) -> bool {
        self.value_db_per_hz.is_none()
    }

    /// The floor in dBm/Hz, only when calibrated.
    pub fn dbm_per_hz(&self) -> Option<f64> {
        (self.unit == Some(PowerUnit::Dbm))
            .then_some(self.value_db_per_hz)
            .flatten()
    }

    /// Noise temperature, K (calibrated only).
    pub fn noise_temperature_k(&self) -> Option<f64> {
        self.dbm_per_hz().map(noise_temperature_k)
    }

    /// dB above `kT₀` (calibrated only).
    pub fn db_above_kt0(&self) -> Option<f64> {
        self.dbm_per_hz().map(db_above_kt0)
    }
}

/// A floor-vs-time result.
#[derive(Clone, Debug, PartialEq)]
pub struct FloorVsTime {
    /// Region.
    pub region: FreqRange,
    /// Pyramid level of the grid.
    pub level: u8,
    /// Step length, ns.
    pub t_cell_ns: i64,
    /// Cell shape `n_c` used for the correction.
    pub shape: Option<f64>,
    /// Steps in time order.
    pub steps: Vec<FloorStep>,
    /// Tile digest of the calibrated pyramid (gain states, calibration id).
    pub calibrated_provenance: ProvenanceSummary,
    /// Tile digest of the uncalibrated pyramid.
    pub uncalibrated_provenance: ProvenanceSummary,
}

type FactorKey = (CalibrationStateId, u64, u64, usize, u64, u64, bool);
type ShapeKey = (usize, usize, u32, WindowKind, u64);

/// The calibrated floor-vs-time product (see the [module docs](super)).
pub struct FloorProduct {
    cfg: FloorProductConfig,
    dir: PathBuf,
    calibrated: Pyramid,
    uncalibrated: Pyramid,
    cals: PowerCalibrations,
    runs: RunLog,
    shape: Option<f64>,
    shape_cache: HashMap<ShapeKey, f64>,
    factor_key: Option<FactorKey>,
    factor: Vec<f32>,
    lin: Vec<f32>,
    peak: Vec<f32>,
    stats: FloorProductStats,
}

fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> StoreError + '_ {
    move |source| StoreError::Io {
        path: path.to_owned(),
        source,
    }
}

fn read_meta(dir: &Path) -> Result<Option<f64>, StoreError> {
    let path = dir.join(META);
    match fs::read_to_string(&path) {
        Ok(s) => s
            .lines()
            .find_map(|l| l.strip_prefix("shape "))
            .map(|v| {
                v.trim()
                    .parse::<f64>()
                    .ok()
                    .filter(|x| x.is_finite() && *x > 0.0)
                    .ok_or_else(|| StoreError::Config(format!("{}: bad shape", path.display())))
            })
            .transpose(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(StoreError::Io { path, source: e }),
    }
}

fn write_meta(dir: &Path, shape: f64) -> Result<(), StoreError> {
    let path = dir.join(META);
    let tmp = dir.join(format!("{META}.tmp{}", std::process::id()));
    fs::write(&tmp, format!("hackriff-floor-product 1\nshape {shape}\n")).map_err(io_err(&tmp))?;
    fs::rename(&tmp, &path).map_err(io_err(&path))
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(f64::total_cmp);
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        0.5 * (v[n / 2 - 1] + v[n / 2])
    }
}

struct RowStats {
    value: f64,
    raw: f64,
    mean: f64,
    bias: f64,
    stat_sd: f64,
    cells: usize,
    coverage: f32,
    level: u8,
    rolled_up: bool,
}

impl FloorProduct {
    /// Opens (or creates) a product under `dir`: `calibrated/` (dBm/Hz pyramid),
    /// `uncalibrated/` (dBFS/Hz pyramid), `runs.tsv` (state log) and `product.txt` (cell shape).
    pub fn open(
        dir: impl AsRef<Path>,
        config: FloorProductConfig,
        calibrations: PowerCalibrations,
    ) -> Result<Self, StoreError> {
        let valid = config.model_uncertainty_db >= 0.0
            && config.model_uncertainty_db.is_finite()
            && config.gap_factor > 1.0
            && config.shape_tolerance > 0.0;
        if !valid {
            return Err(StoreError::Config(
                "floor product: model_uncertainty_db >= 0, gap_factor > 1, shape_tolerance > 0"
                    .into(),
            ));
        }
        let dir = dir.as_ref().to_owned();
        fs::create_dir_all(&dir).map_err(io_err(&dir))?;
        let mut cc = config.pyramid.clone();
        cc.unit = PowerUnit::Dbm;
        let mut uc = config.pyramid.clone();
        uc.unit = PowerUnit::Dbfs;
        let calibrated = Pyramid::open(dir.join("calibrated"), cc)?;
        let uncalibrated = Pyramid::open(dir.join("uncalibrated"), uc)?;
        let nanos = |d: Duration| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX);
        let runs = RunLog::open(
            dir.join("runs.tsv"),
            nanos(config.pyramid.t_cell),
            nanos(config.max_run),
            config.gap_factor,
        )?;
        let shape = read_meta(&dir)?;
        Ok(Self {
            cfg: config,
            dir,
            calibrated,
            uncalibrated,
            cals: calibrations,
            runs,
            shape,
            shape_cache: HashMap::new(),
            factor_key: None,
            factor: Vec::new(),
            lin: Vec::new(),
            peak: Vec::new(),
            stats: FloorProductStats::default(),
        })
    }

    /// Settings.
    pub fn config(&self) -> &FloorProductConfig {
        &self.cfg
    }

    /// Counters.
    pub fn stats(&self) -> FloorProductStats {
        self.stats
    }

    /// Unparseable lines skipped when the state log was opened.
    pub fn corrupt_run_lines(&self) -> u64 {
        self.runs.corrupt_lines
    }

    /// The product's cell shape `n_c` (set by the first frame, persisted).
    pub fn shape(&self) -> Option<f64> {
        self.shape
    }

    /// Calibration versions.
    pub fn calibrations(&self) -> &PowerCalibrations {
        &self.cals
    }

    /// Adds calibration versions as C05 measures them.
    pub fn calibrations_mut(&mut self) -> &mut PowerCalibrations {
        &mut self.cals
    }

    /// The dBm/Hz pyramid.
    pub fn calibrated_pyramid(&self) -> &Pyramid {
        &self.calibrated
    }

    /// The dBFS/Hz pyramid (frames with no applicable calibration).
    pub fn uncalibrated_pyramid(&self) -> &Pyramid {
        &self.uncalibrated
    }

    fn shape_for(&mut self, r: &hk_dsp::Resolution) -> Result<f64, StoreError> {
        let key = (
            r.fft_len,
            r.overlap,
            r.n_avg,
            r.window,
            r.bin_width_hz.to_bits(),
        );
        // T-139: scheduler rows alternate between a few `n_avg`s; a small map avoids recomputing.
        if self.shape_cache.len() > SHAPE_CACHE_MAX {
            self.shape_cache.clear();
        }
        let f_cell_hz = self.cfg.pyramid.f_cell_hz;
        let s = *self
            .shape_cache
            .entry(key)
            .or_insert_with(|| cell_value_shape(r, f_cell_hz));
        match self.shape {
            None => {
                write_meta(&self.dir, s)?;
                self.shape = Some(s);
            }
            Some(p) if (s / p - 1.0).abs() > self.cfg.shape_tolerance && self.cfg.mixed_shapes => {
                self.stats.mixed_shape_frames += 1;
            }
            Some(p) if (s / p - 1.0).abs() > self.cfg.shape_tolerance => {
                self.stats.rejected_frames += 1;
                return Err(StoreError::BadFrame(
                    "spectrum geometry changes this product's cell statistics; use a separate product",
                ));
            }
            Some(_) => {}
        }
        Ok(s)
    }

    /// Folds one spectrum frame and its floor frame (from the same [`hk_dsp::NoiseFloorTracker`]
    /// update). With an applicable calibration (the version its provenance pins, covering the
    /// frame's span and gain at its time) the PSD is scaled per bin by `10^(K(f)/10)` into the
    /// dBm/Hz pyramid; otherwise it goes unchanged into the dBFS/Hz pyramid. The frame's flags,
    /// gain and calibration go to the state log.
    pub fn ingest(
        &mut self,
        spectrum: &SpectrumFrame,
        floor: &FloorFrame,
    ) -> Result<FloorIngest, StoreError> {
        self.ingest_from(spectrum, floor, crate::history::FrameOrigin::default())
    }

    /// [`Self::ingest`] with the frame's source and site recorded in the history tiles (T-133).
    pub fn ingest_from(
        &mut self,
        spectrum: &SpectrumFrame,
        floor: &FloorFrame,
        origin: crate::history::FrameOrigin,
    ) -> Result<FloorIngest, StoreError> {
        if spectrum.seq != floor.seq || spectrum.t != floor.t || spectrum.spectrum.bins() == 0 {
            self.stats.rejected_frames += 1;
            return Err(StoreError::BadFrame(
                "floor frame does not belong to this spectrum frame",
            ));
        }
        let s = &spectrum.spectrum;
        let shape = self.shape_for(&s.resolution)?;
        let prov = spectrum.provenance.get();
        let gain = gain_setting_of(prov);
        let t = spectrum.t.host_time;
        let n = s.bins();
        let band = FreqRange::new(s.bin_frequency_hz(0), s.bin_frequency_hz(n - 1));
        let mut flags = frame_flags(floor);
        flags.set(FloorFlags::QUANTISATION_LIMITED, floor.quantisation_limited);
        let base = FrameInput::from_dsp(spectrum).with_origin(origin);
        let (outcome, calibration, sigma) =
            match self.cals.band(prov.calibration_state_ref, &gain, band, t) {
                Ok(bc) => {
                    flags.set(FloorFlags::CAL_EDGE_HELD, bc.edge_held);
                    let key = (
                        bc.calibration,
                        s.f_center_hz.to_bits(),
                        s.bin_width_hz().to_bits(),
                        n,
                        gain.lna_db.to_bits(),
                        gain.vga_db.to_bits(),
                        gain.amp_on,
                    );
                    if self.factor_key != Some(key) {
                        let table = self.cals.get(bc.calibration).expect("band() found it");
                        self.factor.clear();
                        self.factor.extend((0..n).map(|i| {
                            let k = table
                                .at(&gain, s.bin_frequency_hz(i), t)
                                .map_or(bc.k_center_db, |v| v.k_db);
                            10f64.powf(k / 10.0) as f32
                        }));
                        self.factor_key = Some(key);
                    }
                    self.lin.clear();
                    self.lin
                        .extend(s.psd.iter().zip(&self.factor).map(|(&p, &k)| p * k));
                    self.peak.clear();
                    if base.peak.is_some() {
                        self.peak
                            .extend(s.max_hold.iter().zip(&self.factor).map(|(&p, &k)| p * k));
                    }
                    let input = FrameInput {
                        unit: PowerUnit::Dbm,
                        psd: &self.lin,
                        peak: base.peak.map(|_| &self.peak[..]),
                        ..base
                    };
                    // Per-bin K: the band-spread term of `BandCal::uncertainty_db` does not apply.
                    (
                        self.calibrated.ingest(&input)?,
                        Some(bc.calibration),
                        bc.point_uncertainty_db,
                    )
                }
                Err(_) => {
                    flags |= FloorFlags::UNCALIBRATED;
                    (self.uncalibrated.ingest(&base)?, None, 0.0)
                }
            };
        match outcome {
            IngestOutcome::Folded => {
                if calibration.is_some() {
                    self.stats.calibrated_frames += 1;
                } else {
                    self.stats.uncalibrated_frames += 1;
                }
                // Like the pyramid, a frame belongs to the time cell holding its midpoint; its
                // logged span is clipped to that cell.
                let cell_ns = i64::try_from(self.cfg.pyramid.t_cell.as_nanos()).unwrap_or(i64::MAX);
                let t0 = t.as_unix_nanos();
                let cell = t0.saturating_add(base.duration_ns / 2).div_euclid(cell_ns);
                let (c0, c1) = (cell * cell_ns, (cell + 1) * cell_ns);
                let lo = t0.max(c0);
                let hi = t0
                    .saturating_add(frame_period_ns(floor).max(1))
                    .min(c1)
                    .max(lo + 1);
                self.runs.record(FloorRun {
                    t0_ns: lo,
                    t1_ns: hi,
                    f_lo_hz: band.lo_hz,
                    f_hi_hz: band.hi_hz,
                    flags,
                    gain,
                    calibration,
                    cal_uncertainty_db: sigma,
                })?;
            }
            IngestOutcome::Late => self.stats.late_frames += 1,
        }
        Ok(FloorIngest {
            outcome,
            calibrated: calibration.is_some(),
            flags,
            shape,
        })
    }

    /// Seals both pyramids through `t` and writes the state log.
    pub fn seal_through(&mut self, t: Timestamp) -> Result<(), StoreError> {
        self.calibrated.seal_through(t)?;
        self.uncalibrated.seal_through(t)?;
        self.runs.flush()
    }

    /// T-942: forces every open tile of both pyramids shut, leaving the watermark at `t` — the
    /// end-of-run gesture (see [`Pyramid::seal_all`]).
    pub fn seal_all(&mut self, t: Timestamp) -> Result<(), StoreError> {
        self.calibrated.seal_all(t)?;
        self.uncalibrated.seal_all(t)?;
        self.runs.flush()
    }

    /// Checkpoints both pyramids and writes the state log.
    pub fn checkpoint(&mut self) -> Result<(), StoreError> {
        self.calibrated.checkpoint()?;
        self.uncalibrated.checkpoint()?;
        self.runs.flush()
    }

    /// Writes the state log and checkpoints and closes both pyramids.
    pub fn close(self) -> Result<(), StoreError> {
        let Self {
            calibrated,
            uncalibrated,
            mut runs,
            ..
        } = self;
        runs.flush()?;
        calibrated.close()?;
        uncalibrated.close()
    }

    fn row_stats(
        &self,
        cells: &[CellStats],
        q: f64,
        cache: &mut HashMap<(bool, u32), f64>,
    ) -> Option<RowStats> {
        let shape = self.shape?;
        let (mut corr, mut raw, mut mean, mut bias) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let (mut coverage, mut level, mut rolled_up) = (0f32, u8::MAX, false);
        // T-139: the decision is per tile, from the tile's own persisted shape, so it survives a
        // restart. A mixed-shape tile's cells carry the Gamma-mixture bias (T-141); a cell with a
        // finite `p_low_db` and no `floor_db` sits in a mixed tile without per-shape counts
        // (format < 4): it gives no floor, never a wrong one. A cell's own bias is
        // `p_low − floor_db`; when that agrees with the
        // product shape's to the stored 0.01-dB rounding, the product's exact bias is used (so a
        // single-geometry product computes exactly as before T-139).
        for c in cells
            .iter()
            .filter(|c| c.observed() && c.p_low_db.is_finite() && c.floor_db.is_finite())
        {
            let exact = c.level == 0;
            rolled_up |= !exact;
            let own = f64::from(c.p_low_db) - f64::from(c.floor_db);
            let product_bias = *cache
                .entry((exact, if exact { c.frames } else { 0 }))
                .or_insert_with(|| {
                    let p = if exact {
                        exact_percentile_probability(q, c.frames)
                    } else {
                        q / 100.0
                    };
                    percentile_bias_db(shape, p)
                });
            let b = if (own - product_bias).abs() <= STORED_BIAS_ROUNDING_DB {
                product_bias
            } else {
                own
            };
            let p_low = f64::from(c.p_low_db);
            corr.push(p_low - b);
            raw.push(p_low);
            mean.push(f64::from(c.mean_db));
            bias.push(b);
            coverage = coverage.max(c.coverage);
            level = level.min(c.level);
        }
        if corr.is_empty() {
            return None;
        }
        let n = corr.len();
        let value = median(&mut corr);
        let stat_sd = if n >= 2 {
            let mut dev: Vec<f64> = corr.iter().map(|x| (x - value).abs()).collect();
            MEDIAN_SE_FACTOR * MAD_TO_SD * median(&mut dev) / (n as f64).sqrt()
        } else {
            cell_value_sd_db(shape)
        };
        Some(RowStats {
            value,
            raw: median(&mut raw),
            mean: median(&mut mean),
            bias: median(&mut bias),
            stat_sd,
            cells: n,
            coverage,
            level,
            rolled_up,
        })
    }

    /// The floor per time step over `region` × `[t0, t1)` at `resolution` (T-017's level choice).
    ///
    /// Each step is the calibrated (dBm/Hz) value when the calibrated pyramid observed the step,
    /// else the uncalibrated (dBFS/Hz, [`FloorFlags::UNCALIBRATED`]) value, else a gap
    /// ([`FloorFlags::GAP`], no value, never interpolated). See the [module docs](super) for the
    /// estimator, uncertainty and flags.
    pub fn floor_vs_time(
        &self,
        region: FreqRange,
        t0: Timestamp,
        t1: Timestamp,
        resolution: Resolution,
    ) -> Result<FloorVsTime, StoreError> {
        let q = RegionQuery {
            freq: region,
            time: TimeRange::new(t0, t1),
            resolution,
        };
        let hc = self.calibrated.query(&q)?;
        let hu = self.uncalibrated.query(&q)?;
        if (hc.level, hc.nt, hc.nf, hc.t_first_cell) != (hu.level, hu.nt, hu.nf, hu.t_first_cell) {
            return Err(StoreError::BadQuery(
                "calibrated and uncalibrated pyramids disagree on the grid".into(),
            ));
        }
        let q_pct = f64::from(hc.percentiles.0);
        let hist_sd = f64::from(self.cfg.pyramid.histogram.step_db) / 12f64.sqrt();
        let mut cache = HashMap::new();
        let mut steps = Vec::with_capacity(hc.nt);
        for row in 0..hc.nt {
            let ts = hc.time_of(row).as_unix_nanos();
            let te = ts.saturating_add(hc.t_cell_ns);
            let runs = self.runs.overlapping(ts, te, region);
            let cal = self.row_stats(hc.row(row), q_pct, &mut cache);
            let unc = self.row_stats(hu.row(row), q_pct, &mut cache);
            let calibrated = cal.is_some();
            let mut flags = FloorFlags::NONE;
            let (unit, stats) = match (cal, unc) {
                (Some(c), u) => {
                    flags.set(
                        FloorFlags::PARTLY_UNCALIBRATED,
                        u.is_some() || runs.iter().any(|r| r.calibration.is_none()),
                    );
                    (Some(PowerUnit::Dbm), Some(c))
                }
                (None, Some(u)) => {
                    flags |= FloorFlags::UNCALIBRATED;
                    (Some(PowerUnit::Dbfs), Some(u))
                }
                (None, None) => (None, None),
            };
            let Some(st) = stats else {
                steps.push(FloorStep {
                    t: Timestamp::from_unix_nanos(ts),
                    duration_ns: hc.t_cell_ns,
                    unit: None,
                    value_db_per_hz: None,
                    raw_p_low_db_per_hz: None,
                    bias_db: None,
                    mean_db_per_hz: None,
                    cells: 0,
                    coverage: 0.0,
                    level: u8::MAX,
                    model_uncertainty_db: self.cfg.model_uncertainty_db,
                    calibration_uncertainty_db: f64::NAN,
                    histogram_uncertainty_db: f64::NAN,
                    statistical_uncertainty_db: f64::NAN,
                    uncertainty_db: f64::NAN,
                    flags: FloorFlags::GAP,
                    gain_states: 0,
                });
                continue;
            };
            let relevant: Vec<&FloorRun> = runs
                .iter()
                .filter(|r| r.calibration.is_some() == calibrated)
                .collect();
            let mut gains: Vec<GainSetting> = Vec::new();
            let mut cal_ids: Vec<CalibrationStateId> = Vec::new();
            let mut sigma_cal = if calibrated { f64::NAN } else { 0.0 };
            for r in &relevant {
                flags |= FloorFlags::from_bits_truncate(r.flags.bits() & STATE_FLAGS.bits());
                if !gains.iter().any(|g| same_gain(g, &r.gain)) {
                    gains.push(r.gain);
                }
                if let Some(c) = r.calibration {
                    if !cal_ids.contains(&c) {
                        cal_ids.push(c);
                    }
                    sigma_cal = sigma_cal.max(r.cal_uncertainty_db);
                }
            }
            flags.set(FloorFlags::MIXED_GAIN, gains.len() > 1);
            flags.set(FloorFlags::MIXED_CALIBRATION, cal_ids.len() > 1);
            let sigma_hist = if st.rolled_up { hist_sd } else { 0.0 };
            let model = self.cfg.model_uncertainty_db;
            steps.push(FloorStep {
                t: Timestamp::from_unix_nanos(ts),
                duration_ns: hc.t_cell_ns,
                unit,
                value_db_per_hz: Some(st.value),
                raw_p_low_db_per_hz: Some(st.raw),
                bias_db: Some(st.bias),
                mean_db_per_hz: Some(st.mean),
                cells: st.cells,
                coverage: st.coverage,
                level: st.level,
                model_uncertainty_db: model,
                calibration_uncertainty_db: sigma_cal,
                histogram_uncertainty_db: sigma_hist,
                statistical_uncertainty_db: st.stat_sd,
                uncertainty_db: hk_dsp::radiometry::combine_uncertainty(&[
                    model, sigma_cal, sigma_hist, st.stat_sd,
                ]),
                flags,
                gain_states: gains.len(),
            });
        }
        Ok(FloorVsTime {
            region,
            level: hc.level,
            t_cell_ns: hc.t_cell_ns,
            shape: self.shape,
            steps,
            calibrated_provenance: hc.provenance,
            uncalibrated_provenance: hu.provenance,
        })
    }
}
