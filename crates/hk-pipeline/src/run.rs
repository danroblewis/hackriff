//! Starting, observing, re-plumbing and finishing a run.
//!
//! # Segments and re-plumbing (T-050)
//!
//! A run is one Survey executed as one or more **segments**. A segment is the capture thread,
//! its ring, the always-on readers, the control thread and its chains, all built for one sample
//! rate and one content class (`Shared`). Most runs have a single segment. A live run under the
//! control API ([`PipelineConfig::live_window_class`]) starts a new segment when a retune or rate
//! change needs a different class or rate ([`PipelineController::retune`]):
//!
//! 1. The request is recorded and the old segment's capture thread is stopped between two blocks.
//!    Everything already in its ring belongs to the old window and is processed to the end under
//!    the old class: readers drain, chains detach and finish, recordings end, the detector flushes.
//! 2. The capture thread drops its source wrapper, which hands the **still-open device** back
//!    ([`Lent`]); the segment's inventory and repository connection are recovered.
//! 3. The new rate and centre are sent to the device.
//! 4. A new segment starts with the new window's class ([`crate::class::window_class`]), rate,
//!    ring, detection resolution and STFTs, and the same Survey, database, history store,
//!    counters, display settings and stream ids (the spectrum stream is offered again under its
//!    id with a header for the new window).
//! 5. Its [`WindowGuard`] drops every block until the device delivers the requested window, and
//!    afterwards any block whose window has another class. No block of one class ever reaches a
//!    ring, reader or chain gated for another.
//!
//! So class changes are atomic at a block boundary without any gating code reading a mutable
//! class: every chain, recorder, plugin and stream sees exactly one class for its whole life.
//! A retune that keeps the class and rate is applied in place (no re-plumb); the spectrum header
//! follows it row by row ([`crate::spectrum`]).
//!
//! Counters are shared across segments; per-reader absolute counters (`lost_samples`, `frames`)
//! restart with each segment's reader. History is sealed only when the run ends, never between
//! segments.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use hk_core::{
    BlockHeader, Discontinuity, Pacing, ProvenanceHandle, ReplayOptions, RingConfig, RingHandle,
    SigmfReplaySource, Source, SourceCapabilities, SourceControl, SourceError, ring_buffer,
};
use hk_dsp::radiometry::PowerCalibrations;
use hk_model::sigmf::SigmfMeta;
use hk_model::{
    CalibrationState, CalibrationStateId, ContentClass, InventoryQuery, ProvenanceId, Repository,
    Survey, SurveyId, SurveyState, SurveySummary, Timestamp,
};
use hk_store::{FloorProduct, FloorProductConfig};
use num_complex::{Complex, Complex32};
use serde::Serialize;
use serde_json::Value;

use crate::chains::spec::ChainSpec;
use crate::class::{class_name, source_class, window_class};
use crate::config::{DisplayPatch, DisplaySettings, PipelineConfig, detection_resolution};
use crate::control::{SchedState, SwitchableControl};
use crate::events::{Candidate, ControlEvent};
use crate::gate::FlowGate;
use crate::inventory::Inventory;
use crate::recorder::{ManualRecorder, RecordingStatus};
use crate::spectrum::DisplayControl;
use crate::stats::{Counters, get, inc};

/// `(device_id, calibration)` pins: the newest non-superseded version per device.
type CalibrationPins = Arc<Vec<(String, CalibrationStateId)>>;

/// Stores the loaded calibration versions the run pins on its provenance (T-072). A Provenance row
/// references its `calibration_state_ref` (foreign key), so without the version in the repository
/// every detection, track and recording interning a calibrated provenance failed to store
/// (SPACE-050: 6 detections, 0 stored). Versions already stored are kept; a version is stored after
/// the one it supersedes when both are loaded.
fn store_calibrations(repo: &mut Repository, states: &[CalibrationState]) -> anyhow::Result<()> {
    let mut pending: Vec<&CalibrationState> = states
        .iter()
        .filter(|s| repo.calibration_state(s.id).is_err())
        .collect();
    while !pending.is_empty() {
        let before = pending.len();
        let mut i = 0;
        while i < pending.len() {
            let s = pending[i];
            let ready = s
                .supersedes
                .is_none_or(|p| pending.iter().all(|q| q.id != p));
            if ready {
                repo.insert_calibration_state(s)
                    .with_context(|| format!("storing calibration state {}", s.id))?;
                pending.swap_remove(i);
            } else {
                i += 1;
            }
        }
        anyhow::ensure!(
            pending.len() < before,
            "calibration states supersede each other in a cycle"
        );
    }
    Ok(())
}

fn calibration_pins(states: &[CalibrationState]) -> CalibrationPins {
    let superseded: Vec<CalibrationStateId> = states.iter().filter_map(|s| s.supersedes).collect();
    let mut newest: Vec<(String, CalibrationStateId, Timestamp)> = Vec::new();
    for s in states.iter().filter(|s| !superseded.contains(&s.id)) {
        match newest.iter_mut().find(|(d, _, _)| *d == s.device_id) {
            Some(e) if e.2 < s.measured_at => (e.1, e.2) = (s.id, s.measured_at),
            Some(_) => {}
            None => newest.push((s.device_id.clone(), s.id, s.measured_at)),
        }
    }
    Arc::new(newest.into_iter().map(|(d, id, _)| (d, id)).collect())
}

/// Pins the loaded calibration on blocks of its device (T-037a; see [`crate::config`]).
/// Provenance handles are re-minted only when the source's handle changes.
struct Calibrated {
    inner: Box<dyn Source>,
    pins: CalibrationPins,
    cache: Vec<(ProvenanceId, ProvenanceHandle)>,
}

impl Calibrated {
    fn wrap(inner: Box<dyn Source>, pins: &CalibrationPins) -> Box<dyn Source> {
        if pins.is_empty() {
            return inner;
        }
        Box::new(Self {
            inner,
            pins: Arc::clone(pins),
            cache: Vec::new(),
        })
    }

    fn stamp(&mut self, mut h: BlockHeader) -> BlockHeader {
        let p = h.provenance.get();
        if p.calibration_state_ref.is_some() {
            return h;
        }
        let Some(&(_, cal)) = self.pins.iter().find(|(d, _)| *d == p.device_id) else {
            return h;
        };
        let id = h.provenance.id();
        if let Some((_, pinned)) = self.cache.iter().find(|(i, _)| *i == id) {
            h.provenance = pinned.clone();
            return h;
        }
        let mut record = p.clone();
        record.calibration_state_ref = Some(cal);
        let pinned = ProvenanceHandle::new(record);
        if self.cache.len() >= 256 {
            self.cache.clear();
        }
        self.cache.push((id, pinned.clone()));
        h.provenance = pinned;
        h
    }
}

impl Source for Calibrated {
    fn capabilities(&self) -> &SourceCapabilities {
        self.inner.capabilities()
    }

    fn control(&self) -> Arc<dyn SourceControl> {
        self.inner.control()
    }

    fn pausable(&self) -> bool {
        self.inner.pausable()
    }

    fn read_block(
        &mut self,
        samples: &mut Vec<Complex32>,
    ) -> Result<Option<BlockHeader>, SourceError> {
        let h = self.inner.read_block(samples)?;
        Ok(h.map(|h| self.stamp(h)))
    }

    fn read_block_ci8(
        &mut self,
        samples: &mut Vec<Complex<i8>>,
    ) -> Result<Option<BlockHeader>, SourceError> {
        let h = self.inner.read_block_ci8(samples)?;
        Ok(h.map(|h| self.stamp(h)))
    }
}

/// Where a segment's capture thread hands its source back (see the module docs).
type SourceSlot = Arc<Mutex<Option<Box<dyn Source>>>>;

/// A source lent to a capture thread: dropping the wrapper returns the source to the slot, open.
struct Lent {
    inner: Option<Box<dyn Source>>,
    slot: SourceSlot,
}

impl Lent {
    fn wrap(inner: Box<dyn Source>, slot: &SourceSlot) -> Box<dyn Source> {
        Box::new(Self {
            inner: Some(inner),
            slot: Arc::clone(slot),
        })
    }

    fn get(&self) -> &dyn Source {
        self.inner.as_deref().expect("lent source present")
    }

    fn get_mut(&mut self) -> &mut Box<dyn Source> {
        self.inner.as_mut().expect("lent source present")
    }
}

impl Drop for Lent {
    fn drop(&mut self) {
        if let Some(s) = self.inner.take() {
            *self.slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(s);
        }
    }
}

impl Source for Lent {
    fn capabilities(&self) -> &SourceCapabilities {
        self.get().capabilities()
    }

    fn control(&self) -> Arc<dyn SourceControl> {
        self.get().control()
    }

    fn pausable(&self) -> bool {
        self.get().pausable()
    }

    fn read_block(
        &mut self,
        samples: &mut Vec<Complex32>,
    ) -> Result<Option<BlockHeader>, SourceError> {
        self.get_mut().read_block(samples)
    }

    fn read_block_ci8(
        &mut self,
        samples: &mut Vec<Complex<i8>>,
    ) -> Result<Option<BlockHeader>, SourceError> {
        self.get_mut().read_block_ci8(samples)
    }
}

/// Keeps a segment to its class (legal guardrail; see the module docs): drops blocks until the
/// requested window arrives, then any block whose window has another class. A dropped block is
/// returned empty (the capture thread skips empty blocks); the next admitted block carries `GAP`.
struct WindowGuard {
    inner: Box<dyn Source>,
    class: ContentClass,
    expect: Option<(f64, f64)>,
    dropped: bool,
    stats: Arc<ControlStats>,
}

impl WindowGuard {
    fn wrap(
        inner: Box<dyn Source>,
        class: ContentClass,
        expect: Option<(f64, f64)>,
        stats: &Arc<ControlStats>,
    ) -> Box<dyn Source> {
        Box::new(Self {
            inner,
            class,
            expect,
            dropped: false,
            stats: Arc::clone(stats),
        })
    }

    fn admit(&mut self, h: &mut BlockHeader) -> bool {
        let (center, rate) = (
            h.provenance.tune.center_hz,
            h.provenance.tune.sample_rate_hz,
        );
        if let Some((c, r)) = self.expect {
            if (center - c).abs() > 1.0 || (rate - r).abs() > 1.0 {
                return self.reject();
            }
            self.expect = None;
        }
        if window_class(center, rate) != self.class {
            return self.reject();
        }
        if std::mem::take(&mut self.dropped) {
            h.discontinuity = Discontinuity::from_bits_truncate(
                h.discontinuity.bits() | Discontinuity::GAP.bits(),
            );
        }
        true
    }

    fn reject(&mut self) -> bool {
        self.dropped = true;
        inc(&self.stats.blocks_dropped_window);
        false
    }
}

impl Source for WindowGuard {
    fn capabilities(&self) -> &SourceCapabilities {
        self.inner.capabilities()
    }

    fn control(&self) -> Arc<dyn SourceControl> {
        self.inner.control()
    }

    fn pausable(&self) -> bool {
        self.inner.pausable()
    }

    fn read_block(
        &mut self,
        samples: &mut Vec<Complex32>,
    ) -> Result<Option<BlockHeader>, SourceError> {
        let h = self.inner.read_block(samples)?;
        Ok(h.map(|mut h| {
            if !samples.is_empty() && !self.admit(&mut h) {
                samples.clear();
            }
            h
        }))
    }

    fn read_block_ci8(
        &mut self,
        samples: &mut Vec<Complex<i8>>,
    ) -> Result<Option<BlockHeader>, SourceError> {
        let h = self.inner.read_block_ci8(samples)?;
        Ok(h.map(|mut h| {
            if !samples.is_empty() && !self.admit(&mut h) {
                samples.clear();
            }
            h
        }))
    }
}

/// Reopens the source for `--loop`.
pub type SourceFactory = Box<dyn FnMut() -> anyhow::Result<Box<dyn Source>> + Send>;

/// What the pipeline needs to know about a source before its first block.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SourceInfo {
    /// Sample rate, Hz.
    pub sample_rate_hz: f64,
    /// Centre, Hz.
    pub center_hz: f64,
    /// Time of the first sample.
    pub start_time: Timestamp,
}

/// An opened SigMF replay.
pub struct Replay {
    /// The source.
    pub source: SigmfReplaySource,
    /// Rate, centre, start.
    pub info: SourceInfo,
    /// Source class ([`crate::class::source_class`]).
    pub class: ContentClass,
    /// Metadata.
    pub meta: SigmfMeta,
}

/// Replay block length: ~5 ms, 1024..65536 samples.
pub fn replay_block_len(fs: f64) -> usize {
    ((fs / 200.0) as usize).clamp(1024, 65_536)
}

/// Opens a `.sigmf-meta` recording for the pipeline, played back as recorded.
///
/// `virtual_tuning` is refused (T-057): a virtual tune moved the provenance centre while the IQ
/// stayed at the recorded centre, so every signal landed at the wrong absolute frequency. Anything
/// that retunes a recording (the scheduler) uses [`open_mock_replay`], whose retunes really shift
/// and filter the IQ.
pub fn open_replay(path: &Path, pacing: Pacing, virtual_tuning: bool) -> anyhow::Result<Replay> {
    if virtual_tuning {
        anyhow::bail!(
            "virtual tuning of a replay misplaces signals (T-057); open a scheduler-driven \
             replay with open_mock_replay (the mock SDR device) instead"
        );
    }
    let meta = SigmfMeta::read(path).with_context(|| format!("reading {}", path.display()))?;
    let fs = meta
        .global
        .sample_rate
        .context("the recording has no core:sample_rate")?;
    let source = SigmfReplaySource::open(
        path,
        ReplayOptions {
            block_len: replay_block_len(fs),
            pacing,
        },
    )
    .with_context(|| format!("opening {}", path.display()))?;
    let center_hz = meta
        .captures
        .first()
        .and_then(|c| c.frequency)
        .or_else(|| meta.global.provenance.as_ref().map(|p| p.tune.center_hz))
        .unwrap_or(0.0);
    let start_time = meta
        .captures
        .first()
        .and_then(|c| c.datetime.as_deref())
        .and_then(hk_core::source::sigmf_replay::parse_sigmf_datetime)
        .unwrap_or(Timestamp::UNIX_EPOCH);
    Ok(Replay {
        class: source_class(&meta),
        info: SourceInfo {
            sample_rate_hz: fs,
            center_hz,
            start_time,
        },
        meta,
        source,
    })
}

/// A recording served by the mock SDR device (T-049).
pub struct DeviceReplay {
    /// The mock device's stream.
    pub source: hk_core::MockSdrSource,
    /// Rate, centre, start (the recording's).
    pub info: SourceInfo,
    /// Source class ([`crate::class::source_class`]: band-derived from the recorded window as for
    /// a live radio there, or the stricter class the recording declares).
    pub class: ContentClass,
    /// Metadata.
    pub meta: SigmfMeta,
    /// Device identity (`mock:<recorded device>`).
    pub device: hk_core::DeviceInfo,
}

/// Opens a `.sigmf-meta` recording behind the mock SDR device, tuned to the recording, with the
/// recording's clock. Its retunes shift, filter and resample the IQ (noise and a coverage flag
/// outside the recorded band), so a scheduler driving it sees truthful frequencies (T-057).
/// `Pacing::Unpaced` is pausable (lossless); real time is not.
pub fn open_mock_replay(
    path: &Path,
    pacing: Pacing,
    end: hk_core::MockEnd,
) -> anyhow::Result<DeviceReplay> {
    let fs = SigmfMeta::read(path)
        .with_context(|| format!("reading {}", path.display()))?
        .global
        .sample_rate
        .context("the recording has no core:sample_rate")?;
    let driver = hk_core::MockSdrDriver::new(
        path,
        hk_core::MockOptions {
            block_len: replay_block_len(fs),
            pacing,
            end,
            ..hk_core::MockOptions::default()
        },
    )
    .with_context(|| format!("opening the mock device over {}", path.display()))?;
    let source = driver.open_mock(&driver.default_request())?;
    let rec = source.recording();
    let info = SourceInfo {
        sample_rate_hz: rec.sample_rate_hz,
        center_hz: rec.center_hz,
        start_time: source.start_time(),
    };
    let meta = rec.meta.clone();
    let device = source
        .control()
        .device_info()
        .context("the mock device reports its identity")?;
    Ok(DeviceReplay {
        class: source_class(&meta),
        info,
        meta,
        device,
        source,
    })
}

/// State shared by one segment's threads.
pub(crate) struct Shared {
    pub cfg: PipelineConfig,
    pub counters: Arc<Counters>,
    pub ring: RingHandle<Complex<i8>>,
    pub gate: Arc<FlowGate>,
    pub repo: Mutex<Repository>,
    pub db_path: PathBuf,
    pub stop: Arc<AtomicBool>,
    pub survey_id: SurveyId,
    pub fs: f64,
    pub fft_len: usize,
    pub averages: usize,
    pub inventory: Mutex<Box<dyn Inventory>>,
    pub specs: Vec<ChainSpec>,
    /// Display settings (shared by every segment of the run).
    pub display: Arc<DisplayControl>,
    /// The run continues in a new segment after this one: history is not sealed at its end.
    pub continues: AtomicBool,
    /// Burst taps of the run (T-060).
    pub bursts: Arc<crate::chains::taps::BurstHub>,
    /// Which analog chain owns each emission (T-071 dedupe).
    pub claims: crate::chains::EmissionClaims,
    /// Decodes written per track (T-127: a bandit dwell's `valid_decodes`).
    pub track_decodes: Arc<crate::chains::TrackDecodes>,
    /// The run's compute providers (T-056): one registry shared by every segment.
    pub compute: hk_dsp::compute::Compute,
}

impl Shared {
    /// The shared repository connection.
    pub fn repo(&self) -> MutexGuard<'_, Repository> {
        self.repo.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Control-plane counters of a run (T-050).
#[derive(Debug, Default)]
pub struct ControlStats {
    /// Segments started (1 without a re-plumb).
    pub segments: AtomicU64,
    /// Re-plumbs completed.
    pub replumbs: AtomicU64,
    /// Retunes applied in place (same class and rate).
    pub retunes_in_place: AtomicU64,
    /// Blocks the window guard dropped (not yet the requested window, or another class).
    pub blocks_dropped_window: AtomicU64,
    /// Manual recordings started.
    pub recordings_started: AtomicU64,
    /// Manual recordings refused by the content class.
    pub recordings_refused_class: AtomicU64,
}

/// Why a control request was not applied.
#[derive(Debug)]
pub enum ControlFailure {
    /// A value is malformed or out of range.
    Invalid(String),
    /// Device settings need a live, window-classed source (a recording cannot be retuned).
    NotLive(String),
    /// Refused by policy (the legal guardrail: a class that forbids content).
    Refused(String),
    /// Conflicts with the run's state (a re-plumb in progress, a recording already running).
    Conflict(String),
    /// The re-plumb did not finish in time (it continues in the background).
    Timeout(String),
    /// The device rejected the command.
    Source(SourceError),
    /// The run has finished or is stopping.
    Finished(String),
    /// Anything else.
    Failed(String),
}

impl fmt::Display for ControlFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(s)
            | Self::NotLive(s)
            | Self::Refused(s)
            | Self::Conflict(s)
            | Self::Timeout(s)
            | Self::Finished(s)
            | Self::Failed(s) => f.write_str(s),
            Self::Source(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ControlFailure {}

/// A retune's result.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct RetuneOutcome {
    /// The run's content class from the retune on.
    pub content_class: ContentClass,
    /// The run was re-plumbed (a new segment); `false` for an in-place tune.
    pub replumbed: bool,
    /// Segment number now running (0 for the first).
    pub segment: u64,
}

/// The control plane's view of a run.
#[derive(Clone, Debug, Serialize)]
pub struct ControlStatus {
    /// Device settings can be changed (a live, window-classed source).
    pub live: bool,
    /// Content class of the running segment.
    pub content_class: ContentClass,
    /// Requested centre, Hz.
    pub center_hz: f64,
    /// Requested sample rate, Hz.
    pub sample_rate_hz: f64,
    /// Segment number (0 for the first).
    pub segment: u64,
    /// A re-plumb is in progress.
    pub replumbing: bool,
    /// The run has finished.
    pub finished: bool,
    /// Display settings.
    pub display: DisplaySettings,
    /// The current or last manual recording.
    pub recording: RecordingStatus,
    /// Control counters.
    pub stats: Value,
}

/// How long [`PipelineController::retune`] waits for a re-plumb.
pub const REPLUMB_TIMEOUT: Duration = Duration::from_secs(30);

struct Replumb {
    from: (f64, f64),
    to: (f64, f64),
    class: ContentClass,
}

/// Parts of a run that outlive segments.
struct Common {
    data_dir: PathBuf,
    db_path: PathBuf,
    survey_id: SurveyId,
    counters: Arc<Counters>,
    product: Arc<Mutex<FloorProduct>>,
    display: Arc<DisplayControl>,
    switch: Arc<SwitchableControl>,
    slot: SourceSlot,
    pins: CalibrationPins,
    stats: Arc<ControlStats>,
    user_stop: AtomicBool,
    recorder: Mutex<Option<ManualRecorder>>,
    last_recording: Mutex<RecordingStatus>,
    /// Listen limits in force (T-066; changeable at runtime).
    listen: Arc<Mutex<crate::config::ListenSettings>>,
    /// Burst taps (T-060), closed when the run ends.
    bursts: Arc<crate::chains::taps::BurstHub>,
    /// Compute providers (T-056): built once per run, so no segment changes provider.
    compute: hk_dsp::compute::Compute,
    /// T-115: the observation log (`None` when it could not be opened).
    observations: Option<crate::observe::ObservationLog>,
    /// T-127: the scheduler as the API sees it (snapshot + lease commands), shared by segments.
    scheduler: Arc<crate::control::SchedulerHub>,
}

impl Common {
    /// Ends the manual recording (if any) and keeps its final status.
    fn finish_recorder(&self) {
        let rec = self
            .recorder
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(r) = rec {
            *self
                .last_recording
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = r.finish();
        }
    }
}

struct SupState {
    /// The running segment (`None` while re-plumbing).
    shared: Option<Arc<Shared>>,
    tx: Option<Sender<ControlEvent>>,
    window: (f64, f64),
    class: ContentClass,
    live: bool,
    segment: u64,
    request: Option<Replumb>,
    result: Option<Result<RetuneOutcome, ControlFailure>>,
    finished: bool,
    /// Summary inputs of the last segment.
    resolution: (f64, usize, usize),
}

struct Supervisor {
    common: Common,
    state: Mutex<SupState>,
    cv: Condvar,
}

impl Supervisor {
    fn lock(&self) -> MutexGuard<'_, SupState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Starts pipeline runs.
pub struct Pipeline;

type Worker = (&'static str, JoinHandle<anyhow::Result<()>>);

struct Started {
    shared: Arc<Shared>,
    tx: Sender<ControlEvent>,
    workers: Vec<Worker>,
}

impl Pipeline {
    /// Opens the stores, opens the Survey, and starts the threads (capture last).
    pub fn start(
        mut cfg: PipelineConfig,
        source: Box<dyn Source>,
        info: SourceInfo,
        reopen: Option<crate::run::SourceFactory>,
        inventory: Box<dyn Inventory>,
    ) -> anyhow::Result<PipelineHandle> {
        if cfg.lossless && !source.pausable() {
            anyhow::bail!(
                "lossless mode needs a source that can pause (a recording), and {} cannot: a \
                 held-back capture thread would drop live samples; set PipelineConfig::lossless \
                 = false",
                source.capabilities().driver
            );
        }
        std::fs::create_dir_all(&cfg.data_dir)
            .with_context(|| format!("creating {}", cfg.data_dir.display()))?;
        let db_path = cfg.data_dir.join("hackriff.db");
        let mut repo =
            Repository::open(&db_path).with_context(|| format!("opening {}", db_path.display()))?;
        if repo.scan_plan(cfg.plan.id, cfg.plan.version).is_err() {
            repo.insert_scan_plan(&cfg.plan)?;
        }
        let survey = Survey {
            id: SurveyId::new(),
            plan_id: cfg.plan.id,
            plan_version: cfg.plan.version,
            device_id: cfg.device_id.clone(),
            state: SurveyState::Open,
            t_start: info.start_time,
            t_end: None,
            summary: None,
        };
        repo.insert_survey(&survey)?;
        store_calibrations(&mut repo, &cfg.calibrations)?;
        let product = FloorProduct::open(
            cfg.data_dir.join("history"),
            FloorProductConfig::default(),
            PowerCalibrations::from_states(&cfg.calibrations, None),
        )
        .map_err(|e| anyhow::anyhow!("opening history: {e}"))?;
        let live = cfg.live_window_class && source.capabilities().controllable;
        let counters = Arc::new(Counters::default());
        // T-056: one compute registry for the whole run (every segment shares it, so the
        // provider cannot change mid-run); the HK_COMPUTE* environment wins over the settings.
        let (compute, compute_options) =
            crate::compute::for_run(&cfg.settings.compute, &counters.compute)?;
        cfg.settings.compute = compute_options;
        let common = Common {
            data_dir: cfg.data_dir.clone(),
            db_path,
            survey_id: survey.id,
            counters,
            compute,
            product: Arc::new(Mutex::new(product)),
            display: Arc::new(DisplayControl::new(DisplaySettings::from_settings(
                &cfg.settings,
            ))),
            // The scheduler holds a switchable handle, repointed when `--loop` reopens the source.
            switch: Arc::new(SwitchableControl::new(source.control())),
            slot: Arc::new(Mutex::new(None)),
            pins: calibration_pins(&cfg.calibrations),
            stats: Arc::new(ControlStats::default()),
            user_stop: AtomicBool::new(false),
            recorder: Mutex::new(None),
            last_recording: Mutex::new(RecordingStatus::default()),
            listen: Arc::new(Mutex::new(cfg.settings.listen.clone())),
            bursts: Arc::default(),
            // T-115: never fails the run; a log that cannot open is reported and skipped.
            scheduler: Arc::new(crate::control::SchedulerHub::default()),
            observations: crate::observe::ObservationLog::open(
                &cfg.data_dir,
                cfg.stream_sink.as_ref(),
            )
            .map_err(|e| eprintln!("observation log disabled: {e:#}"))
            .ok(),
        };
        // T-071: the on-demand chain budget is reported from the start of the run.
        crate::chains::listen::publish_limits(
            &common.counters,
            &crate::chains::listen::ListenConfig::from_settings(&cfg.settings.listen),
        );
        let class = cfg.source_class;
        let window = (info.center_hz, info.sample_rate_hz);
        let Started {
            shared,
            tx,
            workers,
        } = start_segment(&common, cfg, repo, info, source, reopen, inventory, None)?;
        let resolution = (shared.fs, shared.fft_len, shared.averages);
        let sup = Arc::new(Supervisor {
            common,
            state: Mutex::new(SupState {
                shared: Some(shared),
                tx: Some(tx),
                window,
                class,
                live,
                segment: 0,
                request: None,
                result: None,
                finished: false,
                resolution,
            }),
            cv: Condvar::new(),
        });
        let s = Arc::clone(&sup);
        let thread = thread::Builder::new()
            .name("hk-supervisor".into())
            .spawn(move || {
                let finished = supervise(&s, workers);
                // The run is over: burst taps finish their streams.
                s.common.bursts.close();
                finished
            })?;
        Ok(PipelineHandle {
            sup,
            thread: Some(thread),
            started: Instant::now(),
            recipes: std::sync::OnceLock::new(),
        })
    }
}

/// Starts one segment's threads (see the module docs). The source is lent first, so it returns
/// to the slot if anything below fails.
#[allow(clippy::too_many_arguments)]
fn start_segment(
    common: &Common,
    cfg: PipelineConfig,
    repo: Repository,
    info: SourceInfo,
    source: Box<dyn Source>,
    reopen: Option<SourceFactory>,
    inventory: Box<dyn Inventory>,
    expect: Option<(f64, f64)>,
) -> anyhow::Result<Started> {
    let mut source = Lent::wrap(source, &common.slot);
    if cfg.live_window_class {
        source = WindowGuard::wrap(source, cfg.source_class, expect, &common.stats);
    }
    let source = Calibrated::wrap(source, &common.pins);
    let fs = info.sample_rate_hz;
    let (fft_len, averages) = detection_resolution(fs, &cfg.settings);
    let min_block = replay_block_len(fs).min(1024);
    let ring_cfg = RingConfig::for_duration(fs, cfg.settings.ring_s.max(0.5), min_block);
    let (writer, ring) = ring_buffer::<Complex<i8>>(ring_cfg);
    let gate = Arc::new(FlowGate::new(cfg.lossless, ring.sample_capacity()));
    let stop = Arc::new(AtomicBool::new(false));
    let reopen: Option<SourceFactory> = reopen.map(|mut open| {
        let (switch, pins, slot) = (
            Arc::clone(&common.switch),
            Arc::clone(&common.pins),
            Arc::clone(&common.slot),
        );
        Box::new(move || -> anyhow::Result<Box<dyn Source>> {
            let s = open()?;
            switch.replace(s.control());
            Ok(Calibrated::wrap(Lent::wrap(s, &slot), &pins))
        }) as SourceFactory
    });
    let mut sched = if cfg.drive_scheduler {
        Some(SchedState::new(
            &cfg.plan,
            Arc::clone(&common.switch),
            fs,
            info.start_time,
            Arc::clone(&common.counters),
            cfg.settings.verify_pois,
        )?)
    } else {
        None
    };
    // T-127: the scheduler publishes to the API hub and serves its lease commands.
    if let Some(s) = sched.as_mut() {
        s.attach_hub(Arc::clone(&common.scheduler));
    }
    // T-115: the scheduler's observer. A source that cannot retune observes its own window.
    if let (Some(s), Some(log)) = (sched.as_mut(), &common.observations) {
        let fixed = (!common.switch.capabilities().controllable)
            .then_some((info.center_hz, info.sample_rate_hz));
        s.observer = Some(log.observer(
            s.plan(),
            fft_len,
            fixed,
            Some(common.survey_id),
            Arc::clone(&common.counters),
        ));
    }
    // T-115: a live run without the scheduler (interactive `hk serve`) logs its tuning as
    // interactive dwell records, polled on the control thread.
    let interactive = match (&sched, &common.observations) {
        (None, Some(log)) if cfg.live_window_class && common.switch.capabilities().controllable => {
            Some(log.interactive(
                fft_len,
                Some(common.survey_id),
                Arc::clone(&common.counters),
            ))
        }
        _ => None,
    };
    let specs = cfg.settings.chain_specs();
    let shared = Arc::new(Shared {
        counters: Arc::clone(&common.counters),
        ring,
        gate,
        repo: Mutex::new(repo),
        db_path: common.db_path.clone(),
        stop,
        survey_id: common.survey_id,
        fs,
        fft_len,
        averages,
        inventory: Mutex::new(inventory),
        specs,
        display: Arc::clone(&common.display),
        continues: AtomicBool::new(false),
        bursts: Arc::clone(&common.bursts),
        claims: crate::chains::EmissionClaims::default(),
        track_decodes: Arc::default(),
        compute: common.compute.clone(),
        cfg,
    });
    inc(&common.stats.segments);

    let (tx, rx) = mpsc::channel::<ControlEvent>();
    let mut workers: Vec<Worker> = Vec::new();
    let spawn = |name: &'static str,
                 f: Box<dyn FnOnce() -> anyhow::Result<()> + Send>|
     -> anyhow::Result<Worker> {
        Ok((name, thread::Builder::new().name(name.into()).spawn(f)?))
    };
    {
        let (s, t) = (Arc::clone(&shared), tx.clone());
        workers.push(spawn(
            "hk-detect",
            Box::new(move || crate::detect::run(s, t)),
        )?);
    }
    {
        let (s, p) = (Arc::clone(&shared), Arc::clone(&common.product));
        workers.push(spawn(
            "hk-history",
            Box::new(move || crate::history::run(s, p)),
        )?);
    }
    {
        let s = Arc::clone(&shared);
        workers.push(spawn(
            "hk-spectrum",
            Box::new(move || crate::spectrum::run(s)),
        )?);
    }
    {
        let s = Arc::clone(&shared);
        workers.push(spawn(
            "hk-control",
            Box::new(move || crate::control::run(s, rx, sched, interactive)),
        )?);
    }
    let s = Arc::clone(&shared);
    let capture = hk_core::rt::spawn_capture_thread("hk-capture", move |_priority| {
        let (writer, s) = (writer, s);
        let r = crate::capture::run(source, reopen, writer, Arc::clone(&s));
        if r.is_err() {
            s.stop.store(true, Ordering::SeqCst);
        }
        r
    })?;
    workers.insert(0, ("hk-capture", capture));
    Ok(Started {
        shared,
        tx,
        workers,
    })
}

/// What the supervisor leaves for [`PipelineHandle::wait`].
struct Finished {
    errors: Vec<String>,
}

fn join_workers(workers: &mut Vec<Worker>, errors: &mut Vec<String>) {
    for (name, join) in workers.drain(..) {
        match join.join() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => errors.push(format!("{name}: {e:#}")),
            Err(_) => errors.push(format!("{name}: panicked")),
        }
    }
}

/// Runs segments until one ends without a re-plumb request (see the module docs).
fn supervise(sup: &Supervisor, mut workers: Vec<Worker>) -> Finished {
    let mut errors = Vec::new();
    loop {
        join_workers(&mut workers, &mut errors);
        let mut st = sup.lock();
        let Some(req) = st.request.take() else {
            st.finished = true;
            sup.cv.notify_all();
            return Finished { errors };
        };
        if sup.common.user_stop.load(Ordering::SeqCst) {
            st.result = Some(Err(ControlFailure::Finished("the run is stopping".into())));
            st.finished = true;
            sup.cv.notify_all();
            return Finished { errors };
        }
        let old = st.shared.take().expect("a running segment");
        st.tx = None;
        drop(st);
        match replumb(&sup.common, old, &req) {
            Ok((started, outcome, window, class)) => {
                let mut st = sup.lock();
                if sup.common.user_stop.load(Ordering::SeqCst) {
                    started.shared.stop.store(true, Ordering::SeqCst);
                }
                st.resolution = (
                    started.shared.fs,
                    started.shared.fft_len,
                    started.shared.averages,
                );
                st.shared = Some(started.shared);
                st.tx = Some(started.tx);
                st.window = window;
                st.class = class;
                st.segment += 1;
                st.result = Some(outcome.map(|mut o| {
                    o.segment = st.segment;
                    o
                }));
                inc(&sup.common.stats.replumbs);
                sup.cv.notify_all();
                workers = started.workers;
            }
            Err(e) => {
                errors.push(format!("re-plumb: {e}"));
                let mut st = sup.lock();
                st.result = Some(Err(ControlFailure::Failed(format!(
                    "the re-plumb failed and the run ended: {e}"
                ))));
                st.finished = true;
                sup.cv.notify_all();
                return Finished { errors };
            }
        }
    }
}

fn unwrap_shared(mut arc: Arc<Shared>) -> Result<Shared, String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match Arc::try_unwrap(arc) {
            Ok(s) => return Ok(s),
            Err(a) if Instant::now() < deadline => {
                arc = a;
                thread::sleep(Duration::from_millis(2));
            }
            Err(_) => return Err("a thread of the previous segment still holds its state".into()),
        }
    }
}

fn apply_window(
    control: &dyn SourceControl,
    to: (f64, f64),
    from: (f64, f64),
) -> Result<(), SourceError> {
    if to.1 != from.1 {
        control.set_sample_rate(to.1)?;
    }
    if to.0 != from.0 {
        control.tune(to.0)?;
    }
    Ok(())
}

type Replumbed = (
    Started,
    Result<RetuneOutcome, ControlFailure>,
    (f64, f64),
    ContentClass,
);

/// Steps 2–5 of the module docs, after the old segment's threads have all ended.
fn replumb(common: &Common, old: Arc<Shared>, req: &Replumb) -> Result<Replumbed, String> {
    common.finish_recorder();
    let Shared {
        mut cfg,
        repo,
        inventory,
        ..
    } = unwrap_shared(old)?;
    let repo = repo.into_inner().unwrap_or_else(PoisonError::into_inner);
    let inventory = inventory
        .into_inner()
        .unwrap_or_else(PoisonError::into_inner);
    let source = common
        .slot
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
        .ok_or("the capture thread did not hand the source back")?;
    let old_class = cfg.source_class;
    let (window, class, outcome) = match apply_window(common.switch.as_ref(), req.to, req.from) {
        Ok(()) => (
            req.to,
            req.class,
            Ok(RetuneOutcome {
                content_class: req.class,
                replumbed: true,
                segment: 0,
            }),
        ),
        Err(e) => {
            // Back to the window the old segment had (its class still holds there).
            let _ = apply_window(common.switch.as_ref(), req.from, req.to);
            (req.from, old_class, Err(ControlFailure::Source(e)))
        }
    };
    cfg.source_class = class;
    let info = SourceInfo {
        center_hz: window.0,
        sample_rate_hz: window.1,
        start_time: Timestamp::from_unix_nanos(
            common.counters.stream_time_ns.load(Ordering::Relaxed),
        ),
    };
    let started = start_segment(
        common,
        cfg,
        repo,
        info,
        source,
        None,
        inventory,
        Some(window),
    )
    .map_err(|e| format!("starting the new segment: {e:#}"))?;
    Ok((started, outcome, window, class))
}

/// Stops a running pipeline ([`PipelineHandle::stopper`]).
#[derive(Clone)]
pub struct Stopper {
    sup: Arc<Supervisor>,
}

impl fmt::Debug for Stopper {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Stopper")
            .field("stopped", &self.is_stopped())
            .finish()
    }
}

impl Stopper {
    /// Stops capture, like [`PipelineHandle::stop`].
    pub fn stop(&self) {
        self.sup.common.user_stop.store(true, Ordering::SeqCst);
        let st = self.sup.lock();
        if let Some(s) = &st.shared {
            s.stop.store(true, Ordering::SeqCst);
        }
        self.sup.cv.notify_all();
    }

    /// `stop` has been called (by anyone), or the run has finished.
    pub fn is_stopped(&self) -> bool {
        self.sup.common.user_stop.load(Ordering::SeqCst) || self.sup.lock().finished
    }
}

/// The control plane of a running pipeline (T-050): device window changes with legal
/// re-classification, display settings, pause/resume and manual recording. Cheap to clone; every
/// method is safe from any thread.
#[derive(Clone)]
pub struct PipelineController {
    sup: Arc<Supervisor>,
}

impl fmt::Debug for PipelineController {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PipelineController").finish_non_exhaustive()
    }
}

impl PipelineController {
    /// The control plane's view of the run.
    pub fn status(&self) -> ControlStatus {
        let st = self.sup.lock();
        let c = &self.sup.common;
        let stats = &c.stats;
        ControlStatus {
            live: st.live,
            content_class: st.class,
            center_hz: st.window.0,
            sample_rate_hz: st.window.1,
            segment: st.segment,
            replumbing: st.request.is_some() || (st.shared.is_none() && !st.finished),
            finished: st.finished,
            display: c.display.get(),
            recording: self.recording(),
            stats: serde_json::json!({
                "segments": get(&stats.segments),
                "replumbs": get(&stats.replumbs),
                "retunes_in_place": get(&stats.retunes_in_place),
                "blocks_dropped_window": get(&stats.blocks_dropped_window),
                "recordings_started": get(&stats.recordings_started),
                "recordings_refused_class": get(&stats.recordings_refused_class),
            }),
        }
    }

    /// Moves a live run to `(center_hz, sample_rate_hz)`. A window with the run's class and rate
    /// is tuned in place; any other re-plumbs the run with the window's class at a block boundary
    /// and returns once the new segment runs (see the module docs), or [`ControlFailure::Timeout`]
    /// after [`REPLUMB_TIMEOUT`].
    pub fn retune(
        &self,
        center_hz: f64,
        sample_rate_hz: f64,
    ) -> Result<RetuneOutcome, ControlFailure> {
        let caps = self.sup.common.switch.capabilities();
        let mut st = self.sup.lock();
        if st.finished || self.sup.common.user_stop.load(Ordering::SeqCst) {
            return Err(ControlFailure::Finished("the run has finished".into()));
        }
        if !st.live {
            return Err(ControlFailure::NotLive(
                "device settings apply to a live source; this run's window is fixed (a recording \
                 or a scheduler-driven run)"
                    .into(),
            ));
        }
        if !(center_hz.is_finite() && caps.supports_frequency(center_hz)) {
            return Err(ControlFailure::Invalid(format!(
                "centre frequency {center_hz} Hz is outside the device's ranges"
            )));
        }
        if !(sample_rate_hz.is_finite() && caps.sample_rates.supports(sample_rate_hz)) {
            return Err(ControlFailure::Invalid(format!(
                "sample rate {sample_rate_hz} Hz is not supported by the device"
            )));
        }
        let Some(shared) = st.shared.clone().filter(|_| st.request.is_none()) else {
            return Err(ControlFailure::Conflict("a re-plumb is in progress".into()));
        };
        let class = window_class(center_hz, sample_rate_hz);
        if class == st.class && sample_rate_hz == st.window.1 {
            self.sup
                .common
                .switch
                .tune(center_hz)
                .map_err(ControlFailure::Source)?;
            st.window.0 = center_hz;
            inc(&self.sup.common.stats.retunes_in_place);
            return Ok(RetuneOutcome {
                content_class: class,
                replumbed: false,
                segment: st.segment,
            });
        }
        st.request = Some(Replumb {
            from: st.window,
            to: (center_hz, sample_rate_hz),
            class,
        });
        st.result = None;
        shared.continues.store(true, Ordering::SeqCst);
        shared.stop.store(true, Ordering::SeqCst);
        drop(shared);
        let (mut st, timeout) = self
            .sup
            .cv
            .wait_timeout_while(st, REPLUMB_TIMEOUT, |s| s.result.is_none())
            .unwrap_or_else(PoisonError::into_inner);
        match st.result.take() {
            Some(r) => r,
            None if timeout.timed_out() => Err(ControlFailure::Timeout(format!(
                "the re-plumb did not finish within {} s; it continues",
                REPLUMB_TIMEOUT.as_secs()
            ))),
            None => Err(ControlFailure::Failed("no re-plumb result".into())),
        }
    }

    /// The display settings in force.
    pub fn display(&self) -> DisplaySettings {
        self.sup.common.display.get()
    }

    /// Changes display settings (all or nothing); the spectrum reader applies them at its next
    /// chunk. Works for recordings and live runs alike.
    pub fn set_display(&self, patch: &DisplayPatch) -> Result<DisplaySettings, ControlFailure> {
        self.sup
            .common
            .display
            .patch(patch)
            .map_err(ControlFailure::Invalid)
    }

    /// Pauses or resumes spectrum publishing (capture, detection and history continue).
    pub fn set_paused(&self, paused: bool) -> DisplaySettings {
        self.sup.common.display.set_paused(paused)
    }

    /// Starts recording the tuned window's IQ (see [`crate::recorder`]): refused under a class
    /// that forbids content, or while another manual recording runs.
    pub fn start_recording(
        &self,
        label: Option<&str>,
        max_s: Option<f64>,
    ) -> Result<RecordingStatus, ControlFailure> {
        let c = &self.sup.common;
        let shared = {
            let st = self.sup.lock();
            if st.finished || c.user_stop.load(Ordering::SeqCst) {
                return Err(ControlFailure::Finished("the run has finished".into()));
            }
            st.shared
                .clone()
                .filter(|_| st.request.is_none())
                .ok_or_else(|| ControlFailure::Conflict("a re-plumb is in progress".into()))?
        };
        let mut rec = c.recorder.lock().unwrap_or_else(PoisonError::into_inner);
        if rec.as_ref().is_some_and(|r| !r.is_finished()) {
            return Err(ControlFailure::Conflict(
                "a manual recording is already running".into(),
            ));
        }
        if let Some(done) = rec.take() {
            *c.last_recording
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = done.finish();
        }
        if !shared.cfg.source_class.permits_content() {
            inc(&c.stats.recordings_refused_class);
        }
        let r = ManualRecorder::start(shared, label, max_s)?;
        inc(&c.stats.recordings_started);
        let status = r.status();
        *rec = Some(r);
        Ok(status)
    }

    /// Stops the manual recording and returns its final state (the Recording row is stored).
    pub fn stop_recording(&self) -> Result<RecordingStatus, ControlFailure> {
        let c = &self.sup.common;
        let r = c
            .recorder
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .ok_or_else(|| ControlFailure::Conflict("no manual recording is running".into()))?;
        let status = r.finish();
        *c.last_recording
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = status.clone();
        Ok(status)
    }

    /// The current manual recording, else the last one.
    pub fn recording(&self) -> RecordingStatus {
        let c = &self.sup.common;
        match c
            .recorder
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            Some(r) => r.status(),
            None => c
                .last_recording
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone(),
        }
    }
}

/// A running pipeline.
pub struct PipelineHandle {
    sup: Arc<Supervisor>,
    thread: Option<JoinHandle<Finished>>,
    started: Instant,
    /// The run's recipe runtime (T-088), created on first use.
    recipes: std::sync::OnceLock<Arc<crate::recipes::runtime::RecipeRuntime>>,
}

/// Detection resolution of the run.
#[derive(Clone, Copy, Debug, Serialize, PartialEq)]
pub struct ResolutionSummary {
    /// FFT length.
    pub fft_len: usize,
    /// Averages `K`.
    pub averages: usize,
    /// Segment overlap, samples (0).
    pub overlap: usize,
    /// Effective averages of the thresholds (= K with no overlap).
    pub n_eff: f64,
    /// Bin width, Hz.
    pub bin_hz: f64,
    /// Frame period, s.
    pub frame_s: f64,
}

/// What a run did.
#[derive(Clone, Debug, Serialize)]
pub struct RunSummary {
    /// Data directory.
    pub data_dir: PathBuf,
    /// Survey.
    pub survey_id: SurveyId,
    /// Wall-clock run time, s.
    pub elapsed_s: f64,
    /// Source class (of the last segment).
    pub source_class: String,
    /// Detection resolution (of the last segment).
    pub resolution: ResolutionSummary,
    /// Detection rows in the database.
    pub detections_stored: u64,
    /// Live emitters in the inventory (`query_inventory`, default states candidate + confirmed)
    /// **after the run stopped**: every open track closed at stop and entered the inventory then.
    /// During a live run `/api/inventory` holds only closed tracks, formed hop sets and chain
    /// (demod/decoder) entries, so a station tracked continuously appears mid-run only through
    /// its chain entry (T-084: a live run's inventory listed 1 row while this counted 6).
    pub emitters: u64,
    /// Samples the always-on readers lost to ring overruns.
    pub always_on_lost_samples: u64,
    /// Every counter.
    pub counters: Value,
    /// Thread errors.
    pub errors: Vec<String>,
}

impl RunSummary {
    /// A counter by JSON pointer, e.g. `/chains/attached`.
    pub fn counter(&self, pointer: &str) -> u64 {
        self.counters
            .pointer(pointer)
            .and_then(Value::as_u64)
            .unwrap_or(0)
    }

    /// A human-readable summary.
    pub fn to_text(&self) -> String {
        let c = |p: &str| self.counter(p);
        let r = &self.resolution;
        let mut s = String::new();
        let mut line = |l: String| {
            s.push_str(&l);
            s.push('\n');
        };
        line(format!("data dir:    {}", self.data_dir.display()));
        line(format!(
            "survey:      {} ({:.2} s)",
            self.survey_id, self.elapsed_s
        ));
        line(format!("class:       {}", self.source_class));
        line(format!(
            "resolution:  {} bins x {} averages, overlap {}, n_eff {:.0}, {:.1} Hz bins, {:.2} ms frames",
            r.fft_len,
            r.averages,
            r.overlap,
            r.n_eff,
            r.bin_hz,
            r.frame_s * 1e3
        ));
        let text = |p: &str| {
            self.counters
                .pointer(p)
                .and_then(Value::as_str)
                .unwrap_or("-")
                .to_owned()
        };
        line(format!(
            "compute:     requested {}; stft detect {} / history {} / spectrum {}; {} provider \
             changes",
            text("/compute/options/provider"),
            text("/compute/stft/detect/provider"),
            text("/compute/stft/history/provider"),
            text("/compute/stft/spectrum/provider"),
            c("/compute/provider_changes"),
        ));
        line(format!(
            "source:      {} samples in {} blocks, {} loops, {} gate waits, {} ring errors",
            c("/source/samples"),
            c("/source/blocks"),
            c("/source/loops"),
            c("/source/gate_waits"),
            c("/source/ring_errors")
        ));
        for name in ["detect", "history", "spectrum"] {
            line(format!(
                "reader {name:<9} {} samples, {} frames, lost {} ({} overruns), gaps {}",
                c(&format!("/readers/{name}/samples")),
                c(&format!("/readers/{name}/frames")),
                c(&format!("/readers/{name}/lost_samples")),
                c(&format!("/readers/{name}/overruns")),
                c(&format!("/readers/{name}/gap_samples")),
            ));
        }
        line(format!(
            "detections:  {} ({} stored), {} confirmations, {} dense frames",
            c("/detect/detections"),
            self.detections_stored,
            c("/detect/confirmations"),
            c("/detect/dense_frames")
        ));
        line(format!(
            "tracks:      {} opened, {} closed, {} confirmed, {} rows, {} links",
            c("/detect/tracks_opened"),
            c("/detect/tracks_closed"),
            c("/detect/tracks_confirmed"),
            c("/detect/track_rows"),
            c("/detect/track_links")
        ));
        line(format!(
            "anomalies:   {} opened, {} closed, {} explanations",
            c("/detect/anomalies_opened"),
            c("/detect/anomalies_closed"),
            c("/detect/explanations")
        ));
        line(format!(
            "chains:      {} attached, {} detached, {} refused (class), {} unmatched, {} errors",
            c("/chains/attached"),
            c("/chains/detached"),
            c("/chains/refused_class"),
            c("/chains/unmatched"),
            c("/chains/errors") + c("/chains/attach_errors")
        ));
        line(format!(
            "decodes:     {} demodulations, {} decodes ({} content withheld), {} CRC-valid; plugins {} decodes, {} records dropped, {} restarts",
            c("/chains/demodulations"),
            c("/chains/decodes"),
            c("/chains/content_withheld"),
            c("/chains/crc_valid"),
            c("/chains/plugin_decodes"),
            c("/chains/plugin_dropped"),
            c("/chains/plugin_restarts")
        ));
        line(format!(
            "rejected:    {} outside window, {} duplicate channel, {} mode rejected, {} FSK boxes missed",
            c("/chains/outside_window"),
            c("/chains/duplicate_channel"),
            c("/chains/mode_rejected"),
            c("/chains/fsk_boxes_missed")
        ));
        line(format!(
            "emitters:    {} in inventory at stop (open tracks closed), {} labels; recordings {}",
            self.emitters,
            c("/chains/labels"),
            c("/chains/recordings")
        ));
        line(format!(
            "history:     {} frames, {} tiles written; spectrum {} rows ({} gated)",
            c("/history/frames_ingested"),
            c("/history/tiles_written"),
            c("/spectrum/rows"),
            c("/spectrum/rows_gated")
        ));
        line(format!(
            "scheduler:   {} steps, {} POIs, {} verifications",
            c("/scheduler/steps"),
            c("/scheduler/pois_offered"),
            c("/scheduler/verifications")
        ));
        line(format!(
            "drops:       always-on readers lost {} samples",
            self.always_on_lost_samples
        ));
        for e in &self.errors {
            line(format!("error:       {e}"));
        }
        s
    }
}

impl PipelineHandle {
    /// Live counters (shared by every segment of the run).
    pub fn counters(&self) -> Arc<Counters> {
        Arc::clone(&self.sup.common.counters)
    }

    /// The floor product (history), shared with `/api/history` and `/api/floor`.
    pub fn floor_product(&self) -> Arc<Mutex<FloorProduct>> {
        Arc::clone(&self.sup.common.product)
    }

    /// The observation log (T-115), shared with `/api/observations`; `None` when it could not be
    /// opened.
    pub fn observation_store(&self) -> Option<hk_store::observation::ObservationStore> {
        self.sup
            .common
            .observations
            .as_ref()
            .map(crate::observe::ObservationLog::store)
    }

    /// The scheduler hub (T-127) for `/api/scheduler*`: empty when the run has no scheduler.
    pub fn scheduler_hub(&self) -> Arc<crate::control::SchedulerHub> {
        Arc::clone(&self.sup.common.scheduler)
    }

    /// The data directory (`hackriff.db`, `history/`, `recordings/`).
    pub fn data_dir(&self) -> &Path {
        &self.sup.common.data_dir
    }

    /// The run's Survey (the same across re-plumbs).
    pub fn survey_id(&self) -> SurveyId {
        self.sup.common.survey_id
    }

    /// Stops capture; the readers and chains then drain and finish.
    pub fn stop(&self) {
        self.stopper().stop();
    }

    /// A handle that stops this run from another thread (a watchdog, a signal handler) while
    /// [`Self::wait`] owns the handle.
    pub fn stopper(&self) -> Stopper {
        Stopper {
            sup: Arc::clone(&self.sup),
        }
    }

    /// The control plane (T-050).
    pub fn controller(&self) -> PipelineController {
        PipelineController {
            sup: Arc::clone(&self.sup),
        }
    }

    fn send(&self, ev: ControlEvent) {
        if let Some(tx) = &self.sup.lock().tx {
            let _ = tx.send(ev);
        }
    }

    /// Attaches `spec` for `candidate` at runtime (no restart).
    pub fn attach_chain(&self, spec: ChainSpec, candidate: Candidate) {
        self.send(ControlEvent::Manual {
            spec: Box::new(spec),
            candidate,
        });
    }

    /// Detaches every chain attached with [`Self::attach_chain`].
    pub fn detach_manual_chains(&self) {
        self.send(ControlEvent::DetachManual);
    }

    /// On-demand listening (T-043): an opener attaching audio chains at runtime. It admits under
    /// the run's listen limits in force at each request ([`Self::set_listen_settings`]).
    pub fn listen_service(&self) -> Arc<crate::chains::listen::ListenManager> {
        let sup = Arc::clone(&self.sup);
        Arc::new(crate::chains::listen::ListenManager::new(
            Arc::clone(&self.sup.common.counters),
            Arc::new(move || sup.lock().shared.clone()),
            Arc::clone(&self.sup.common.listen),
        ))
    }

    /// The listen limits in force (T-066).
    pub fn listen_settings(&self) -> crate::config::ListenSettings {
        self.sup
            .common
            .listen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Replaces the listen limits (T-066): later requests are admitted under them; running
    /// chains keep going.
    pub fn set_listen_settings(&self, settings: crate::config::ListenSettings) {
        crate::chains::listen::publish_limits(
            &self.sup.common.counters,
            &crate::chains::listen::ListenConfig::from_settings(&settings),
        );
        *self
            .sup
            .common
            .listen
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = settings;
    }

    /// Burst bits taps (T-060): an opener streaming the hard bits of demodulated bursts.
    pub fn bits_service(&self) -> Arc<crate::chains::taps::BurstTapOpener> {
        self.tap_service(crate::chains::taps::TapKind::Bits)
    }

    /// Burst symbols taps (T-060): an opener streaming the soft symbols of demodulated bursts.
    pub fn symbols_service(&self) -> Arc<crate::chains::taps::BurstTapOpener> {
        self.tap_service(crate::chains::taps::TapKind::Symbols)
    }

    fn tap_service(
        &self,
        kind: crate::chains::taps::TapKind,
    ) -> Arc<crate::chains::taps::BurstTapOpener> {
        let sup = Arc::clone(&self.sup);
        Arc::new(crate::chains::taps::BurstTapOpener::new(
            kind,
            Arc::clone(&self.sup.common.bursts),
            Arc::clone(&self.sup.common.counters),
            Arc::new(move || sup.lock().shared.clone()),
            Arc::clone(&self.sup.common.listen),
        ))
    }

    /// Output recorders (T-061): record a selection's, emitter's or band's bits, symbols, audio
    /// and IQ to files through the run's burst taps, Listen and ring. Create one per front end
    /// and share it (sessions live in the instance; the quota is over `outputs/` on disk).
    pub fn output_recorders(&self) -> Arc<crate::chains::outputs::OutputRecorders> {
        let sup = Arc::clone(&self.sup);
        Arc::new(crate::chains::outputs::OutputRecorders::new(
            Arc::new(move || sup.lock().shared.clone()),
            self.bits_service(),
            self.symbols_service(),
            self.listen_service(),
            self.sup.common.data_dir.clone(),
        ))
    }

    /// The decoder-workbench recipe runtime (T-088, ADR-0011): runs, hot-edits and stores
    /// recipes as chains on this run, and serves their `stage` and `inspector` streams. One
    /// instance per run (later calls return the same). Built-in recipes come from
    /// `$HK_RECIPES_DIR` or the repository's `recipes/`; user versions live in
    /// `<data dir>/recipes/`.
    pub fn recipe_runtime(&self) -> Arc<crate::recipes::runtime::RecipeRuntime> {
        Arc::clone(self.recipes.get_or_init(|| {
            let sup = Arc::clone(&self.sup);
            let builtin = std::env::var_os("HK_RECIPES_DIR")
                .map(PathBuf::from)
                .or_else(|| {
                    Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../recipes"))
                        .filter(|p| p.is_dir())
                });
            let rt = Arc::new(crate::recipes::runtime::RecipeRuntime::new(
                Arc::clone(&self.sup.common.counters),
                Arc::new(move || sup.lock().shared.clone()),
                Arc::clone(&self.sup.common.listen),
                crate::recipes::store::RecipeStore::new(
                    builtin,
                    self.sup.common.data_dir.join("recipes"),
                ),
            ));
            // T-092: always-on decoded-stream capture into `<data dir>/captures/`.
            rt.attach_default_capture_store(&self.sup.common.data_dir);
            rt
        }))
    }

    /// The run's decoded-stream capture store (T-092): every pipeline's inspector output is
    /// recorded there, quota-managed. `None` when the store could not be opened.
    pub fn decoded_captures(&self) -> Option<hk_store::decoded::DecodedCaptures> {
        self.recipe_runtime().capture_store().cloned()
    }

    /// The newest ring sample index.
    pub fn ring_position(&self) -> u64 {
        self.sup
            .lock()
            .shared
            .as_ref()
            .and_then(|s| s.ring.next_sample())
            .unwrap_or(0)
    }

    /// Waits for every segment, closes the Survey and summarises.
    pub fn wait(mut self) -> anyhow::Result<RunSummary> {
        let thread = self.thread.take().expect("the supervisor thread");
        let Finished { mut errors } = thread
            .join()
            .map_err(|_| anyhow::anyhow!("the pipeline supervisor panicked"))?;
        let common = &self.sup.common;
        common.finish_recorder();
        let (class, (fs, fft_len, averages)) = {
            let st = self.sup.lock();
            (st.class, st.resolution)
        };
        let counters = &common.counters;
        let lost = counters.always_on_lost();
        let t_end = Timestamp::from_unix_nanos(counters.stream_time_ns.load(Ordering::Relaxed));
        let summary = SurveySummary {
            sweep_frames: 0,
            spectrum_frames: get(&counters.detect_reader.frames),
            detections: get(&counters.detect.detections),
            recordings: get(&counters.chains.recordings),
            dropped_samples: lost + get(&counters.source.source_dropped),
        };
        let mut detections_stored = 0;
        let mut emitters = 0u64;
        match Repository::open(&common.db_path) {
            Ok(mut repo) => {
                if let Err(e) =
                    repo.finish_survey(common.survey_id, SurveyState::Closed, t_end, &summary)
                {
                    errors.push(format!("closing the survey: {e}"));
                }
                detections_stored = repo.detection_count().unwrap_or(0);
                let mut q = InventoryQuery {
                    limit: 100,
                    ..InventoryQuery::default()
                };
                while let Ok(page) = repo.query_inventory(&q) {
                    emitters += page.entries.len() as u64;
                    match page.next_offset {
                        Some(o) => q.offset = o,
                        None => break,
                    }
                }
                let _ = repo.checkpoint();
            }
            Err(e) => errors.push(format!("opening the database to close the survey: {e}")),
        }
        let resolution = ResolutionSummary {
            fft_len,
            averages,
            overlap: 0,
            n_eff: averages as f64,
            bin_hz: fs / fft_len as f64,
            frame_s: (fft_len * averages) as f64 / fs,
        };
        Ok(RunSummary {
            data_dir: common.data_dir.clone(),
            survey_id: common.survey_id,
            elapsed_s: self.started.elapsed().as_secs_f64(),
            source_class: class_name(class),
            resolution,
            detections_stored,
            emitters,
            always_on_lost_samples: lost,
            counters: counters.to_json(),
            errors,
        })
    }
}

/// Runs a replay once through the pipeline (the `hk replay` path).
pub fn replay_once(
    path: &Path,
    mut cfg: PipelineConfig,
    pacing: Pacing,
    inventory: Box<dyn Inventory>,
) -> anyhow::Result<RunSummary> {
    cfg.lossless = matches!(pacing, Pacing::Unpaced);
    if cfg.drive_scheduler {
        // The scheduler retunes: serve the recording through the mock device (T-057).
        let replay = open_mock_replay(path, pacing, hk_core::MockEnd::Stop)?;
        cfg.source_class = replay.class;
        cfg.device_id = replay.device.device_id.clone();
        cfg.device_hw = Some(replay.device.hw.clone());
        let handle = Pipeline::start(cfg, Box::new(replay.source), replay.info, None, inventory)?;
        return handle.wait();
    }
    let replay = open_replay(path, pacing, false)?;
    cfg.source_class = replay.class;
    if let Some(hw) = &replay.meta.global.hw {
        cfg.device_id = format!("sigmf:{hw}");
    }
    cfg.device_hw = replay.meta.global.hw.clone();
    let handle = Pipeline::start(cfg, Box::new(replay.source), replay.info, None, inventory)?;
    handle.wait()
}
