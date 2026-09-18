//! Pipeline settings: resolution choice, ring size, batching, chains and classification rules.
//!
//! A ScanPlan can override any [`PipelineSettings`] field under `extra.pipeline` (unknown keys
//! are errors). `extra.scheduler` stays the scheduler's own (`SchedulerConfig::from_plan`).
//!
//! # Detection resolution (T-006 re-probe follow-up)
//!
//! [`detection_resolution`]: bins of about 5 kHz (`fft_len = next_pow2(fs / 5 kHz)` clamped to
//! 512..4096) and frames of about 2.5 ms (`K = round(fs · 2.5 ms / fft_len)` clamped to 4..10).
//! That is the S4 geometry T-006 validated (20 Msps → 4096 × 10, 4.88 kHz, 2.05 ms: all 7 known
//! FM stations on the urban fixture; 2.4 Msps → 512 × 10), where the re-probe's 65536 bins × 2
//! found only 2/7: the floor tracker's 256-bin blocks (~1.25 MHz at 5 kHz) and the detector's
//! Gamma(n) thresholds both want moderate bins and n ≥ 4. Narrow sources keep 512 bins
//! (500 kS/s → 977 Hz bins, K 4; 200 kS/s → 390 Hz, K 4).
//!
//! # STFT overlap and n_eff (T-006 review note)
//!
//! Detection uses **Hann, 0 % overlap**, so the K averaged segments are independent and the
//! floor tracker's `n_avg_effective` is exactly K: the detector's `Gamma(n)` thresholds then hold
//! their design Pfa. With 50 % overlap the moment-matched `Gamma(n_eff)` runs 1.58× the design
//! Pfa at 1e-6 (T-006 review). The cost of no overlap is ~1.8 dB (Hann ENBW) sensitivity loss to
//! bursts shorter than one segment, which the S4 profiles already assume. History and spectrum
//! readers keep their own STFTs (50 % overlap is harmless there: no thresholds).
//!
//! # Calibration (T-037a)
//!
//! [`PipelineConfig::calibrations`] holds T-021 `CalibrationState` versions (JSON as
//! hk-model serialises them; [`load_calibrations`] reads a file with one state or an array, or a
//! directory of `*.json`). The run loads them into its `FloorProduct` and pins, on every block
//! whose provenance names no calibration, the newest non-superseded version for that block's
//! `device_id`. Floors are then calibrated (dBm/Hz) in the pipeline, not after the run.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;
use hk_dsp::window::WindowKind;
use hk_model::{
    CalibrationState, ContentClass, FreqRange, PlanRegion, ScanPlan, ScanPlanId, ScanPolicy,
    Schedule, Timestamp,
};
use hk_stream::{PublisherHandle, StreamHeader};
use serde::{Deserialize, Serialize};

use crate::chains::spec::{ChainSpec, FmRegion, builtin_chains_for};
use crate::class::ClassRule;

/// Target detection bin width, Hz.
pub const TARGET_BIN_HZ: f64 = 5_000.0;
/// Target detection frame period, s.
pub const TARGET_FRAME_S: f64 = 2.5e-3;

/// `(fft_len, averages)` for detection at `fs` (see the module docs).
pub fn detection_resolution(fs: f64, settings: &PipelineSettings) -> (usize, usize) {
    let fft = settings.fft_len.unwrap_or_else(|| {
        ((fs / TARGET_BIN_HZ).ceil().max(1.0) as usize)
            .next_power_of_two()
            .clamp(512, 4096)
    });
    let k = settings
        .averages
        .unwrap_or_else(|| ((fs * TARGET_FRAME_S / fft as f64).round() as usize).clamp(4, 10));
    (fft, k)
}

/// Overridable settings (`ScanPlan.extra.pipeline`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PipelineSettings {
    /// Detection FFT length override.
    pub fft_len: Option<usize>,
    /// Detection averages override.
    pub averages: Option<usize>,
    /// Ring history, s (4): the pre-trigger reach of chains and recordings.
    pub ring_s: f64,
    /// History frames per second into the floor product / pyramid (10).
    pub history_rows_per_s: f64,
    /// Spectrum stream rows per second (25; ≤ 50 under a gated class).
    pub spectrum_rows_per_s: f64,
    /// Spectrum stream FFT length (1024).
    pub spectrum_fft_len: usize,
    /// Detections per repository batch (256).
    pub detection_batch: usize,
    /// Stream time between repository flushes of detections and tracks, s (0.5).
    pub flush_interval_s: f64,
    /// Bands that use the S4 short-burst detection profile, `[lo, hi]` Hz (902–928 MHz ISM).
    pub short_burst_bands_hz: Vec<[f64; 2]>,
    /// Chain registry; `None` uses [`builtin_chains`].
    pub chains: Option<Vec<ChainSpec>>,
    /// User emitter classification rules.
    pub classify: Vec<ClassRule>,
    /// Site `[lat, lon]` for the correlator.
    pub site: Option<[f64; 2]>,
    /// Offer confirmed tracks to the scheduler with a verification group (hackriffd).
    pub verify_pois: bool,
    /// Observation-log age horizon, **days** (T-406, `docs/16` §5.4); `None` keeps
    /// [`hk_store::observation::DEFAULT_MAX_AGE_NS`] (180 days).
    ///
    /// This is the horizon over which the coverage map can still say *"this front end was not
    /// tuned here, then"* rather than *"we no longer know whether we looked"* — so it is the
    /// setting an installation with a small disk lowers, knowingly trading that distinction for
    /// space. It is never raised to cover a gap in the data: a longer horizon retains records, it
    /// does not invent them.
    pub observation_retention_days: Option<f64>,
    /// Observation-log byte quota, **MiB** (T-406); `None` keeps
    /// [`hk_store::observation::DEFAULT_MAX_BYTES`] (2 GiB). On a continuously *sweeping*
    /// installation this is the bound that actually binds, not the age.
    pub observation_max_mb: Option<f64>,
    /// T-439: open the **view-scheme** pyramid (`docs/16` §6.2/§8.2) alongside scheme 1 and write
    /// the live edge into it (`true`).
    ///
    /// Scheme 1 is a welded ladder, so only its diagonal exists: `(fine frequency, coarse time)`
    /// is not a node it has, and the unified surface's whole premise is that the two axes move
    /// independently. The view scheme is the de-welded lattice that makes those addresses real,
    /// and the live chain is what fills its finest node — *"live" is the finest-level growing edge
    /// where hardware is currently tuned*, not a separate mode. Turning this off leaves
    /// `/api/tiles?scheme=view` folding out of scheme 1's ladder exactly as it did before T-439.
    pub view_history: bool,
    /// T-439: the **frequency** cell of the view lattice's node (0, 0), Hz (6250, scheme 1's own
    /// level-0 cell).
    ///
    /// The time cell is fixed at 1 s and is **not** configurable: T-437's finding F1 is that
    /// `docs/16` §6.2's 128 s floor puts the entire 120 s IQ retention inside one time cell, which
    /// re-welds the axis the lattice exists to de-weld. The frequency floor is the knob, because
    /// it is the one that costs memory.
    ///
    /// **Measured** (`tests/live_edge_tiles.rs`, at this default): a peak of **2.28 MB of resident
    /// accumulator per MHz of tuned span**, against a bound of **3.65 MB/MHz** — one tile row per
    /// *time* level — so a 20 MHz live edge is **~46 MB measured, 73 MB worst case**, and the
    /// finest tile stays open for **64 s** against `docs/16` §6.2's 9.1 h. The cost is linear in
    /// the span and inversely linear in this setting, so a 25 kHz floor divides all of it by four.
    ///
    /// Turning the lattice **off** is not a way to save memory — it is the switch for the *write*
    /// cost, which is the one that showed up: a de-welded ×2 lattice writes ~4× a welded ladder's
    /// tiles per second of capture, measured at ~1.5 s of M0-acceptance wall per node. See
    /// [`crate::history::VIEW_F_CELLS_PER_BLOCK`].
    /// **T-484 made this an override rather than the floor itself.** `None` — the default — derives
    /// node (0, 0) from the display plan ([`crate::history::view_geometry`]): `f_cell = fs /
    /// spectrum_fft_len`, so the canvas's finest tier is the display stream's own bin. Setting it
    /// pins the frequency cell and the fold stops being 1:1, which is honest but coarser: a
    /// 6250 Hz cell over 2343.75 Hz bins is the 2.67-bins-per-cell collapse T-483 measured.
    ///
    /// The **time** cell is derived the same way (the display row period) and has no override:
    /// T-437's finding F1 plus T-483's measurement — a 1 s cell against 25 rows/s kept 3 % of the
    /// station's level variation — leave no value worth pinning.
    pub view_f_cell_hz: Option<f64>,
    /// On-demand listening limits (T-066).
    pub listen: ListenSettings,
    /// Compute providers (T-056, ADR-0007): `{"provider": "auto|cpu|cpu-mt|accelerate|gpu",
    /// "stft": …, "pfb": …, "threads": n, "gpu_in_flight": 2}`. `Pipeline::start` applies the
    /// `HK_COMPUTE*` environment over it (the environment wins) and builds one
    /// `hk_dsp::compute::Compute` for the whole run; see [`crate::compute`].
    pub compute: hk_dsp::compute::ComputeOptions,
}

/// On-demand listening limits (T-066; `ScanPlan.extra.pipeline.listen`, `--listen-*` flags of
/// `hk serve` and `hackriffd`). Admission is by count **and** by an estimated CPU budget.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ListenSettings {
    /// Most listen chains at once (8).
    pub max_listeners: usize,
    /// Share of the available CPU cores all listen chains may use together (0.5).
    pub cpu_fraction: f64,
    /// Estimated cost of a WFM chain, cores per Msps of tuned sample rate (0.05). A rough,
    /// conservative estimate (DDC at the full tuned rate dominates); tune it per device.
    pub wfm_cores_per_msps: f64,
    /// Estimated cost of an NBFM, AM, SSB or CW chain, cores per Msps (0.04).
    pub narrow_cores_per_msps: f64,
    /// A chain with no consumer for this long ends, s (10).
    pub idle_timeout_s: f64,
    /// A chain that published no audio (squelch closed) for this long ends, s (600; 0 never).
    pub squelch_timeout_s: f64,
    /// Most on-demand chains at once, Listen chains and burst taps together (16; T-071). One
    /// per-run budget: a per-kind limit larger than this raises it.
    pub max_chains: usize,
    /// Most burst taps (bits/symbols streams) at once (8; T-060's cap).
    pub max_taps: usize,
    /// Estimated cost of one burst tap, cores (0.01): taps read no samples; they cost the
    /// publishing of bursts FSK chains already demodulate.
    pub tap_cores: f64,
}

impl Default for ListenSettings {
    fn default() -> Self {
        Self {
            max_listeners: 8,
            cpu_fraction: 0.5,
            wfm_cores_per_msps: 0.05,
            narrow_cores_per_msps: 0.04,
            idle_timeout_s: 10.0,
            squelch_timeout_s: 600.0,
            max_chains: 16,
            max_taps: 8,
            tap_cores: 0.01,
        }
    }
}

impl Default for PipelineSettings {
    fn default() -> Self {
        Self {
            fft_len: None,
            averages: None,
            ring_s: 4.0,
            history_rows_per_s: 10.0,
            spectrum_rows_per_s: 25.0,
            spectrum_fft_len: 1024,
            detection_batch: 256,
            flush_interval_s: 0.5,
            short_burst_bands_hz: vec![[902e6, 928e6]],
            chains: None,
            classify: Vec::new(),
            site: None,
            verify_pois: true,
            observation_retention_days: None,
            observation_max_mb: None,
            view_history: true,
            view_f_cell_hz: None,
            listen: ListenSettings::default(),
            compute: hk_dsp::compute::ComputeOptions::default(),
        }
    }
}

impl PipelineSettings {
    /// Defaults overridden by `plan.extra.pipeline`.
    pub fn from_plan(plan: &ScanPlan) -> anyhow::Result<Self> {
        let s = match plan.extra.get("pipeline") {
            None | Some(serde_json::Value::Null) => Self::default(),
            Some(v) => serde_json::from_value(v.clone()).context("ScanPlan.extra.pipeline")?,
        };
        for c in s.chain_specs() {
            c.validate()
                .map_err(|e| anyhow::anyhow!("chain {}: {e}", c.id))?;
        }
        Ok(s)
    }

    /// The chain registry in force.
    pub fn chain_specs(&self) -> Vec<ChainSpec> {
        self.chains
            .clone()
            .unwrap_or_else(|| builtin_chains_for(FmRegion::from_site(self.site)))
    }
}

/// Where new streams are offered (e.g. the hk-api bridge registry).
pub type StreamSink = Arc<dyn Fn(&StreamHeader, PublisherHandle) + Send + Sync>;

/// Where streams stop being offered, by `stream_id` (the counterpart of [`StreamSink`]).
pub type StreamUnsink = Arc<dyn Fn(&str) + Send + Sync>;

/// A run's configuration.
#[derive(Clone)]
pub struct PipelineConfig {
    /// Device data directory: `hackriff.db`, `history/`, `recordings/`, `feeds/`.
    pub data_dir: PathBuf,
    /// The plan the Survey runs under.
    pub plan: ScanPlan,
    /// Settings (normally [`PipelineSettings::from_plan`]).
    pub settings: PipelineSettings,
    /// Lossless backpressure: the capture thread waits for slow readers instead of letting the
    /// ring lap them. Off by default (live-source semantics); opt in for unpaced replay. Only a
    /// source that can pause ([`hk_core::Source::pausable`]) may run lossless: `Pipeline::start`
    /// refuses any other.
    pub lossless: bool,
    /// Drive the attention scheduler (hackriffd).
    pub drive_scheduler: bool,
    /// Device id for the Survey.
    pub device_id: String,
    /// Content class of the source (see [`crate::class::source_class`]).
    pub source_class: ContentClass,
    /// Offline feed cache for the correlator; `None` disables correlation.
    pub feeds_dir: Option<PathBuf>,
    /// Directories searched for plugin executables named without a path.
    pub plugin_dirs: Vec<PathBuf>,
    /// Base for relative plugin manifest paths.
    pub manifest_root: PathBuf,
    /// Stream registration hook.
    pub stream_sink: Option<StreamSink>,
    /// Stream withdrawal hook (e.g. a stopped recipe pipeline's streams).
    pub stream_unsink: Option<StreamUnsink>,
    /// Spectrum stream id.
    pub spectrum_stream_id: String,
    /// Calibration versions for in-pipeline calibrated floors (see the module docs).
    pub calibrations: Vec<CalibrationState>,
    /// Capture hardware description for SigMF `core:hw` (e.g. HackRF serial and firmware).
    pub device_hw: Option<String>,
    /// The source class is the **tuned window's** band class (a live radio under the control
    /// API, T-050). The run then (a) drops any block whose window has another class before it
    /// reaches the ring, and (b) re-plumbs itself at a block boundary when a retune or rate change
    /// moves it into another class ([`crate::PipelineController::retune`]). Off for recordings
    /// and scheduler-driven runs (their class covers every window they visit).
    pub live_window_class: bool,
    /// Rolling IQ capture buffer (T-157, [`crate::iqbuffer`]). [`Self::new`] leaves it **off**
    /// (T-178: the ring is allocated on disk up front, so a library or test run never reserves the
    /// 4.8 GB default without asking); the `hk`/`hackriffd` composition turns it on with
    /// [`hk_store::iqbuffer::IqBufferConfig::from_env`] (on for every run that is not a lossless
    /// replay, 2 min retention; `HK_IQ_RETENTION`, `HK_IQ_BUFFER_MAX`, `HK_IQ_BUFFER`) and
    /// `--iq-retention/--iq-buffer-max` on top. Tests that buffer set an explicit small quota.
    pub iq_buffer: hk_store::iqbuffer::IqBufferConfig,
    /// Filesystem probes of the IQ capture buffer (`None`: the real filesystem); tests inject a
    /// full disk or a slow writer.
    pub iq_buffer_hooks: Option<Arc<dyn hk_store::iqbuffer::IqBufferHooks>>,
}

/// Smallest display FFT size.
pub const DISPLAY_FFT_MIN: usize = 64;
/// Largest display FFT size.
pub const DISPLAY_FFT_MAX: usize = 65_536;
/// Largest display averaging (rows in the exponential average).
pub const DISPLAY_AVERAGING_MAX: u32 = 100;
/// Fastest requested spectrum row rate, rows/s (a gated class is still capped at 50).
pub const DISPLAY_ROWS_MAX: f64 = 200.0;
/// Slowest requested spectrum row rate, rows/s.
pub const DISPLAY_ROWS_MIN: f64 = 0.5;

/// Live display settings of the spectrum stream (T-050): applied by the spectrum reader at its
/// next row without a restart; they never touch detection, history or the device.
///
/// **There is no `paused` here, and there must not be one (T-347).** These settings belong to the
/// *run*, and every connected client shares them — which is exactly right for the geometry of the
/// published rows (one STFT, one FFT size, one row rate) and exactly wrong for whether a viewer is
/// looking. "Pause holds a fixed time range for inspection" (CLAUDE.md) is *per viewer*: a
/// run-wide boolean cannot represent N viewers, so the one that used to live here froze every
/// browser's waterfall when any one of them pressed Pause. The view's time window is client state
/// and is held there; nothing about it reaches this struct, the reader, or the device.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct DisplaySettings {
    /// Bins per spectrum row (a power of two, [`DISPLAY_FFT_MIN`]..=[`DISPLAY_FFT_MAX`]).
    pub fft_size: usize,
    /// Rows in the exponential moving average of the published PSD (1 = off).
    pub averaging: u32,
    /// Requested rows per second (the waterfall speed); a class that forbids content caps the
    /// published rate at 50 rows/s.
    pub rows_per_s: f64,
    /// Analysis window for the published PSD (T-067; default Hann).
    pub window: WindowKind,
}

/// A partial display update.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DisplayPatch {
    /// New FFT size.
    pub fft_size: Option<usize>,
    /// New averaging.
    pub averaging: Option<u32>,
    /// New row rate.
    pub rows_per_s: Option<f64>,
    /// New analysis window (T-067).
    pub window: Option<WindowKind>,
}

impl DisplaySettings {
    /// The settings from a run's [`PipelineSettings`] (no averaging, Hann window).
    pub fn from_settings(s: &PipelineSettings) -> Self {
        Self {
            fft_size: s.spectrum_fft_len,
            averaging: 1,
            rows_per_s: s.spectrum_rows_per_s,
            window: WindowKind::default(),
        }
    }

    /// `self` with `patch` applied, or why the patch is invalid (nothing is applied then).
    pub fn patched(&self, patch: &DisplayPatch) -> Result<Self, String> {
        let mut next = *self;
        if let Some(n) = patch.fft_size {
            if !(n.is_power_of_two() && (DISPLAY_FFT_MIN..=DISPLAY_FFT_MAX).contains(&n)) {
                return Err(format!(
                    "fft_size {n} must be a power of two in {DISPLAY_FFT_MIN}..={DISPLAY_FFT_MAX}"
                ));
            }
            next.fft_size = n;
        }
        if let Some(a) = patch.averaging {
            if !(1..=DISPLAY_AVERAGING_MAX).contains(&a) {
                return Err(format!(
                    "averaging {a} must be in 1..={DISPLAY_AVERAGING_MAX} (1 = off)"
                ));
            }
            next.averaging = a;
        }
        if let Some(r) = patch.rows_per_s {
            if !(r.is_finite() && (DISPLAY_ROWS_MIN..=DISPLAY_ROWS_MAX).contains(&r)) {
                return Err(format!(
                    "rows_per_s {r} must be in {DISPLAY_ROWS_MIN}..={DISPLAY_ROWS_MAX}"
                ));
            }
            next.rows_per_s = r;
        }
        if let Some(w) = patch.window {
            next.window = w;
        }
        Ok(next)
    }
}

/// Reads T-021 `CalibrationState` JSON: a file holding one state or an array of states, or a
/// directory of such `*.json` files (read in name order).
pub fn load_calibrations(path: &Path) -> anyhow::Result<Vec<CalibrationState>> {
    let files: Vec<PathBuf> = if path.is_dir() {
        let mut v: Vec<PathBuf> = std::fs::read_dir(path)
            .with_context(|| format!("reading {}", path.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect();
        v.sort();
        v
    } else {
        vec![path.to_path_buf()]
    };
    let mut out = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file)
            .with_context(|| format!("reading {}", file.display()))?;
        let value: serde_json::Value =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", file.display()))?;
        let items = match value {
            serde_json::Value::Array(items) => items,
            one => vec![one],
        };
        for item in items {
            out.push(
                serde_json::from_value(item)
                    .with_context(|| format!("{}: not a CalibrationState", file.display()))?,
            );
        }
    }
    if out.is_empty() {
        anyhow::bail!("no CalibrationState in {}", path.display());
    }
    Ok(out)
}

impl PipelineConfig {
    /// A configuration with defaults for `data_dir` and `plan`.
    pub fn new(data_dir: impl Into<PathBuf>, plan: ScanPlan) -> anyhow::Result<Self> {
        let settings = PipelineSettings::from_plan(&plan)?;
        let exe_dirs: Vec<PathBuf> = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(PathBuf::from))
            .map(|d| {
                let mut v = vec![d.clone()];
                if let Some(parent) = d.parent() {
                    v.push(parent.to_path_buf());
                }
                v
            })
            .unwrap_or_default();
        Ok(Self {
            data_dir: data_dir.into(),
            plan,
            settings,
            lossless: false,
            drive_scheduler: false,
            device_id: "sigmf-replay".into(),
            source_class: ContentClass::FAIL_CLOSED,
            feeds_dir: None,
            plugin_dirs: exe_dirs,
            manifest_root: default_manifest_root(),
            stream_sink: None,
            stream_unsink: None,
            spectrum_stream_id: "spectrum/live".into(),
            calibrations: Vec::new(),
            device_hw: None,
            live_window_class: false,
            iq_buffer: hk_store::iqbuffer::IqBufferConfig {
                enabled: Some(false),
                ..hk_store::iqbuffer::IqBufferConfig::default()
            },
            iq_buffer_hooks: None,
        })
    }
}

/// The repository root when run from the source tree (plugin manifests live in `plugins/`).
pub fn default_manifest_root() -> PathBuf {
    let from_build = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    if from_build.join("plugins").is_dir() {
        from_build
    } else {
        PathBuf::from(".")
    }
}

/// A single-region plan covering `[center ± usable/2]` (for `hk replay` without `--plan`).
pub fn replay_plan(center_hz: f64, sample_rate_hz: f64, t: Timestamp) -> ScanPlan {
    let usable = sample_rate_hz * 0.9;
    ScanPlan {
        id: ScanPlanId::new(),
        version: 1,
        name: "replay".into(),
        created_at: t,
        regions: vec![PlanRegion {
            freq: FreqRange::centered(center_hz, usable),
            priority: 1.0,
            revisit_ns: None,
        }],
        policy: ScanPolicy::SweepThenDwell,
        gain_table: Vec::new(),
        schedule: Schedule::Continuous,
        extra: serde_json::json!({}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolution_follows_the_s4_geometry() {
        let s = PipelineSettings::default();
        assert_eq!(detection_resolution(20e6, &s), (4096, 10));
        assert_eq!(detection_resolution(2.4e6, &s), (512, 10));
        assert_eq!(detection_resolution(10e6, &s), (2048, 10));
        assert_eq!(detection_resolution(500e3, &s), (512, 4));
        assert_eq!(detection_resolution(200e3, &s), (512, 4));
    }

    #[test]
    fn display_patches_are_validated_all_or_nothing() {
        let d = DisplaySettings::from_settings(&PipelineSettings::default());
        assert_eq!((d.fft_size, d.averaging), (1024, 1));
        assert_eq!(d.window, WindowKind::Hann, "default window");
        let ok = d
            .patched(&DisplayPatch {
                fft_size: Some(4096),
                averaging: Some(8),
                rows_per_s: Some(10.0),
                window: Some(WindowKind::FlatTop),
            })
            .unwrap();
        assert_eq!((ok.fft_size, ok.averaging, ok.rows_per_s), (4096, 8, 10.0));
        assert_eq!(ok.window, WindowKind::FlatTop);
        assert_eq!(
            d.patched(&DisplayPatch::default()).unwrap().window,
            WindowKind::Hann,
            "an omitted window leaves it unchanged"
        );
        for bad in [
            DisplayPatch {
                fft_size: Some(1000),
                ..DisplayPatch::default()
            },
            DisplayPatch {
                fft_size: Some(1 << 20),
                ..DisplayPatch::default()
            },
            DisplayPatch {
                averaging: Some(0),
                ..DisplayPatch::default()
            },
            DisplayPatch {
                fft_size: Some(2048),
                rows_per_s: Some(f64::NAN),
                ..DisplayPatch::default()
            },
            DisplayPatch {
                rows_per_s: Some(1e6),
                ..DisplayPatch::default()
            },
        ] {
            assert!(d.patched(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn plan_extra_carries_compute_options() {
        use hk_dsp::compute::Preference;
        let mut plan = replay_plan(100e6, 2.4e6, Timestamp::UNIX_EPOCH);
        assert_eq!(
            PipelineSettings::from_plan(&plan).unwrap().compute.provider,
            Preference::Auto
        );
        plan.extra = serde_json::json!({
            "pipeline": { "compute": { "provider": "cpu", "pfb": "gpu", "gpu_in_flight": 1 } }
        });
        let s = PipelineSettings::from_plan(&plan).unwrap();
        assert_eq!(s.compute.provider, Preference::Cpu);
        assert_eq!(s.compute.pfb, Some(Preference::Gpu));
        assert_eq!(s.compute.gpu_in_flight, 1);
        for bad in [
            serde_json::json!({ "pipeline": { "compute": { "provider": "fpga" } } }),
            serde_json::json!({ "pipeline": { "compute": { "bogus": 1 } } }),
        ] {
            plan.extra = bad;
            assert!(PipelineSettings::from_plan(&plan).is_err());
        }
    }

    #[test]
    fn plan_extra_overrides_and_rejects_unknown_keys() {
        let mut plan = replay_plan(100e6, 2.4e6, Timestamp::UNIX_EPOCH);
        plan.extra = serde_json::json!({ "pipeline": { "fft_len": 1024, "ring_s": 2.0 } });
        let s = PipelineSettings::from_plan(&plan).unwrap();
        assert_eq!((s.fft_len, s.ring_s), (Some(1024), 2.0));
        plan.extra = serde_json::json!({ "pipeline": { "bogus": 1 } });
        assert!(PipelineSettings::from_plan(&plan).is_err());
    }

    #[test]
    fn calibrations_load_from_a_state_an_array_or_a_directory() {
        use hk_dsp::radiometry::{SyntheticCalSegment, synthetic_calibration_state};
        use hk_model::GainSetting;
        let dir = std::env::temp_dir().join(format!(
            "hk-pipeline-cal-{}-{}",
            std::process::id(),
            Timestamp::now().as_unix_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let seg = SyntheticCalSegment {
            band: FreqRange::centered(100e6, 2e6),
            gain: GainSetting {
                lna_db: 32.0,
                vga_db: 30.0,
                amp_on: true,
            },
            k_db: -70.0,
        };
        let a = synthetic_calibration_state("hackrf:a", &[seg], 0.2, Timestamp::UNIX_EPOCH);
        let b = synthetic_calibration_state("hackrf:b", &[seg], 0.2, Timestamp::UNIX_EPOCH);
        std::fs::write(dir.join("a.json"), serde_json::to_vec(&a).unwrap()).unwrap();
        std::fs::write(dir.join("b.json"), serde_json::to_vec(&vec![&b]).unwrap()).unwrap();
        std::fs::write(dir.join("notes.txt"), "ignored").unwrap();
        assert_eq!(
            load_calibrations(&dir.join("a.json")).unwrap(),
            vec![a.clone()]
        );
        assert_eq!(load_calibrations(&dir).unwrap(), vec![a, b]);
        std::fs::write(dir.join("c.json"), "{\"not\": \"a state\"}").unwrap();
        assert!(load_calibrations(&dir).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}
