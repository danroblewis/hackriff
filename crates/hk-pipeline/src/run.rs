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
use hk_store::{FloorProduct, FloorProductConfig, Pyramid};
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

#[path = "devices.rs"]
mod devices;
pub use devices::{ExtraSource, MAX_START_SKEW, RunDevice};

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

    /// **T-541 audit: unreachable, and left as an `expect` for that reason.** `inner` is `Some`
    /// from [`Lent::wrap`] and is taken only in `Drop`, after which no method of this type can be
    /// called. Rewriting it as a handled error would invent a failure mode the type does not
    /// have, and a `Source` method has nowhere honest to report one to.
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

/// **How long a re-plumb may wait for the window it asked for before it stops waiting** (T-497).
///
/// The settle gap after a retune is honest and expected — the front end is moving, and blocks
/// still in the pipe describe the tuning that has ended, so [`WindowGuard`] drops them. What was
/// missing is a **bound**: `expect` was cleared only by a block that matched to within 1 Hz, so a
/// device that never reports that window had **every block dropped for ever**. Capture went silent
/// and stayed silent while `hk serve` answered normally, `run.finished` stayed false and
/// `replumbing` stayed false — the user's report, three times: *"the live capture dies on retune
/// and never recovers"*.
///
/// That state is reachable without anything exotic. [`crate::run`] does not fabricate it;
/// `hk_core`'s HackRF driver applies a posted control **field by field and returns on the first
/// `SourceError`** (`apply_change`: rate, then baseband filter, then gains, then centre), so a
/// failed baseband-filter write leaves the new *rate* in the block provenance and the centre never
/// applied — exactly one component of `expect` off, for ever. [`replumb`]'s own failure path is the
/// second door: it reverts a failed `apply_window` with `let _ =`, and then starts a segment
/// expecting the window the revert may not have restored.
///
/// So the wait is bounded and the bound is generous: whole seconds, orders of magnitude longer than
/// any real in-flight block (a HackRF block is milliseconds), so a device that *is* going to arrive
/// always arrives first and nothing about a normal retune changes. Past it the guard stops
/// demanding the requested window and admits on the **class** check alone.
///
/// **The class check is the legal guardrail and does not move.** Giving up on `expect` is giving up
/// on "is this the window we asked for", never on "may this content reach a ring gated for another
/// class". A device sitting on a restricted-class window still produces no blocks, which is correct
/// and is the documented behaviour.
pub const WINDOW_SETTLE_TIMEOUT: Duration = Duration::from_secs(5);

/// **The window the run last commanded the front end to, and how many commands have been issued**
/// (T-525).
///
/// [`WindowGuard`] takes its `expect` at segment start, but the control plane can command the front
/// end again a millisecond later — the next step of a sweep, or the user pressing Retune — and the
/// device's control mailbox **coalesces**: a change posted behind one the capture thread has not
/// taken yet *replaces* it. The window the segment is waiting for is then a window nobody is
/// commanding any more, and it can never arrive. Measured on the live HackRF under a 1 s-dwell
/// 1 MHz–6 GHz sweep with the canvas driven at its finest level: `blocks_dropped_window` 1912 and
/// `window_settle_timeouts` 9 in two minutes, with `spectrum/live` answering **410 Gone** for the
/// whole of each 5 s wait (T-497's bound is what ends it) — which is what the demo watcher reads as
/// "the server is unhealthy" and restarts it for.
///
/// So the expectation is **shared and versioned** rather than a snapshot: every place that commands
/// the front end's window records it here, and the guard, seeing a newer generation than the one it
/// started on, waits for *that* window instead. A superseded wait is abandoned, not served out.
#[derive(Debug)]
pub struct CommandedWindow {
    center_bits: AtomicU64,
    rate_bits: AtomicU64,
    generation: AtomicU64,
}

impl CommandedWindow {
    pub(crate) fn new(window: (f64, f64)) -> Self {
        Self {
            center_bits: AtomicU64::new(window.0.to_bits()),
            rate_bits: AtomicU64::new(window.1.to_bits()),
            generation: AtomicU64::new(0),
        }
    }

    /// Records a window the control plane has just commanded, bumping the generation.
    fn set(&self, window: (f64, f64)) {
        self.center_bits.store(window.0.to_bits(), Ordering::SeqCst);
        self.rate_bits.store(window.1.to_bits(), Ordering::SeqCst);
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// The commanded window and the generation it was read at. The generation is read on both
    /// sides of the two loads, so the pair returned is never half of one command and half of
    /// another — a torn pair would make the guard wait for a window that was never commanded at
    /// all, which is the very fault this type exists to remove.
    pub(crate) fn get(&self) -> ((f64, f64), u64) {
        loop {
            let before = self.generation.load(Ordering::SeqCst);
            let center = f64::from_bits(self.center_bits.load(Ordering::SeqCst));
            let rate = f64::from_bits(self.rate_bits.load(Ordering::SeqCst));
            if self.generation.load(Ordering::SeqCst) == before {
                return ((center, rate), before);
            }
        }
    }
}

/// Keeps a segment to its class (legal guardrail; see the module docs): drops blocks until the
/// requested window arrives, then any block whose window has another class. A dropped block is
/// returned empty (the capture thread skips empty blocks); the next admitted block carries `GAP`.
///
/// The wait for the requested window is bounded by [`WINDOW_SETTLE_TIMEOUT`] and abandoned when a
/// later command supersedes it ([`CommandedWindow`]); the class check is not bounded by anything,
/// because it is the legal guardrail.
struct WindowGuard {
    inner: Box<dyn Source>,
    class: ContentClass,
    expect: Option<(f64, f64)>,
    /// The [`CommandedWindow`] generation `expect` came from (T-525).
    expect_generation: u64,
    /// When the segment started waiting for `expect`. `None` when there is nothing to wait for.
    waiting_since: Option<Instant>,
    commanded: Arc<CommandedWindow>,
    dropped: bool,
    stats: Arc<ControlStats>,
}

impl WindowGuard {
    fn wrap(
        inner: Box<dyn Source>,
        class: ContentClass,
        expect: Option<(f64, f64)>,
        commanded: &Arc<CommandedWindow>,
        stats: &Arc<ControlStats>,
    ) -> Box<dyn Source> {
        Box::new(Self {
            inner,
            class,
            expect,
            expect_generation: commanded.get().1,
            waiting_since: expect.map(|_| Instant::now()),
            commanded: Arc::clone(commanded),
            dropped: false,
            stats: Arc::clone(stats),
        })
    }

    fn admit(&mut self, h: &mut BlockHeader) -> bool {
        let (center, rate) = (
            h.provenance.tune.center_hz,
            h.provenance.tune.sample_rate_hz,
        );
        // T-525: a later command supersedes the window this segment started waiting for. Keeping
        // the old one would be waiting for a window the run is no longer asking for, which can
        // only end at [`WINDOW_SETTLE_TIMEOUT`] — five seconds of no capture, no spectrum rows and
        // a `spectrum/live` that answers "stream finished" to everything that asks.
        if self.expect.is_some() {
            let (window, generation) = self.commanded.get();
            if generation != self.expect_generation {
                self.expect = Some(window);
                self.expect_generation = generation;
                self.waiting_since = Some(Instant::now());
                inc(&self.stats.window_commands_superseded);
            }
        }
        if let Some((c, r)) = self.expect {
            if (center - c).abs() > 1.0 || (rate - r).abs() > 1.0 {
                // T-497: the settle gap is bounded. Past the bound the front end is where it is,
                // and a view of the window it is actually on beats silence for ever over the window
                // it was asked for. The give-up is counted, not swallowed.
                let waited = self
                    .waiting_since
                    .is_some_and(|t| t.elapsed() >= WINDOW_SETTLE_TIMEOUT);
                if !waited {
                    return self.reject();
                }
                inc(&self.stats.window_settle_timeouts);
                // T-525: counted AND said out loud. A settle timeout means the front end is not
                // where the control plane sent it and capture was silent for the whole bound; a
                // counter alone leaves the operator reading "the server went unhealthy" with
                // nothing to attach it to. One line per give-up, and give-ups are rare by
                // construction — every one of them is a defect.
                eprintln!(
                    "retune settle timeout after {:.1} s: asked for {} Hz / {} Hz, the front end \
                     is on {center} Hz / {rate} Hz; capture resumes on the window it is actually \
                     on (this step's window was not captured)",
                    WINDOW_SETTLE_TIMEOUT.as_secs_f64(),
                    c,
                    r
                );
            }
            self.expect = None;
            self.waiting_since = None;
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
    /// **The run's inventory, not the segment's** (T-941): every segment holds a clone of the one
    /// `Arc` [`Common::inventory`] owns. See there for why it is the run's.
    pub inventory: Arc<Mutex<Box<dyn Inventory>>>,
    pub specs: Vec<ChainSpec>,
    /// Display settings (shared by every segment of the run).
    pub display: Arc<DisplayControl>,
    /// T-974: the window the run last commanded the front end to (the run's own, shared by every
    /// segment; a further front end with no control plane holds its fixed window). What a channel
    /// is planned against before the capture thread has published a block's tune
    /// ([`crate::recipes::runtime::planning_tune`]).
    pub commanded: Arc<CommandedWindow>,
    /// The run continues in a new segment after this one: history is not sealed at its end.
    pub continues: AtomicBool,
    /// T-510: this segment's history reader takes the end-of-run seal itself. True for a
    /// single-device run (behaviour unchanged); false for **every** front end of a multi-source
    /// run, whose one seal [`devices::finish`] takes after all of them have drained — the shared
    /// pyramids have one forward-only watermark, so a first-past-the-post seal would make every
    /// later frame of the other front ends late.
    pub seal_at_end: bool,
    /// T-541: how long a consumer arriving **after** this segment's publishers end is told the
    /// stream is between windows rather than gone
    /// ([`hk_stream::Publisher::finish_between_windows_for`]), in milliseconds.
    ///
    /// A re-plumb keeps [`hk_stream::BETWEEN_WINDOWS_GRACE`] (the default here): the handover is
    /// the producer's own and measured at ~0.17 s since T-525. A segment that ended because the
    /// **device** failed sets [`RECOVERY_SUCCESSOR_GRACE`] instead, because its successor cannot
    /// arrive until [`recover`] has worked through [`recovery_backoff`] — and until it does, or
    /// gives up, "the stream is gone" is not true.
    pub successor_grace_ms: AtomicU64,
    /// Burst taps of the run (T-060).
    pub bursts: Arc<crate::chains::taps::BurstHub>,
    /// Which analog chain owns each emission (T-071 dedupe).
    pub claims: crate::chains::EmissionClaims,
    /// Decodes written per track (T-127: a bandit dwell's `valid_decodes`).
    pub track_decodes: Arc<crate::chains::TrackDecodes>,
    /// The run's compute providers (T-056): one registry shared by every segment.
    pub compute: hk_dsp::compute::Compute,
    /// T-174: the §2.6 DC-twin rule on the history grid (the occupancy engine's cells).
    pub dc_twin: hk_context::occupancy::channels::DcTwinRule,
    /// T-399: the receiver-line survey in force (`crate::survey`), measured once per capture state
    /// by the `hk-survey` reader and applied to every classification of a window captured under
    /// that state.
    pub receiver: Arc<crate::survey::ReceiverSurvey>,
    /// T-484: the hand-off to the view lattice's writer thread. The **spectrum** reader fills it
    /// (the finest node is that reader's own rows); the **history** reader owns the writer thread
    /// that drains it and the T-446 decision about sealing. `None` when the view lattice is off or
    /// could not open.
    pub view_queue: Option<Arc<crate::history::ViewQueue>>,
    /// T-844: the run's C38 shadow stage, observed at the classifier's call site.
    pub ml: Option<Arc<crate::ml::MlStage>>,
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
    /// Re-plumbs whose requested window never arrived, so the guard stopped waiting for it
    /// ([`WINDOW_SETTLE_TIMEOUT`], T-497). Non-zero means the front end is **not** on the window
    /// the control plane asked for, and capture resumed on the one it is actually on rather than
    /// staying silent for ever. It is a defect signal, not a normal outcome.
    pub window_settle_timeouts: AtomicU64,
    /// Segments whose requested window was **superseded** by a later command before it arrived
    /// (T-525). The guard then waits for the newer window instead of serving out
    /// [`WINDOW_SETTLE_TIMEOUT`] on one that can never come. Ordinary under a sweep whose steps
    /// are closer together than the device's control path is long; a defect only if it is
    /// accompanied by `window_settle_timeouts`.
    pub window_commands_superseded: AtomicU64,
    /// Live segments that ended on a **device error** rather than a stop or a re-plumb (T-508):
    /// the capture thread's read failed. Each one is followed by a recovery attempt, never by the
    /// run quietly ending.
    pub capture_failures: AtomicU64,
    /// Re-plumbs that failed after the old segment stopped (T-508): its state could not be
    /// recovered cleanly, the device was not handed back, or the new segment did not start. Each
    /// is followed by a recovery attempt.
    pub replumb_failures: AtomicU64,
    /// Segments restarted after a capture or re-plumb failure (T-508).
    pub capture_recoveries: AtomicU64,
    /// T-941: threads of an earlier segment **left behind** because they had not stopped within
    /// [`REPLUMB_JOIN_BOUND`] of their segment ending (`join_workers`).
    ///
    /// Served rather than kept internal, for the reason `window_settle_timeouts` is: a thread that
    /// does not observe its segment's stop is a defect, and it is one the user could otherwise
    /// only learn about from a line on stderr. Non-zero means the re-plumb went on without one —
    /// **not** that anything was detached: since T-941 the run's inventory is the run's, so an
    /// abandoned straggler can delay the next segment's inventory writes and never silence them.
    pub workers_abandoned: AtomicU64,
    /// Segments whose state was **salvaged** because a thread of the old segment still held it
    /// past [`unwrap_shared`]'s bound (T-508): a fresh database connection, and the inventory taken
    /// from under the straggler. It used to end the run.
    pub segments_salvaged: AtomicU64,
    /// T-541: pipeline threads that ended by **panicking** rather than returning. Always a defect
    /// — nothing in the pipeline is supposed to unwind — but a *reported* one: the thread's death
    /// ends its segment, so the supervisor sees it and either recovers or ends the run with the
    /// panic as the cause, instead of leaving a run that reads `running` with a reader missing.
    pub worker_panics: AtomicU64,
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
    /// Whether the front end is delivering samples (T-508): `running`, `recovering` or `ended`.
    pub capture: CaptureState,
    /// Why capture is recovering or has ended, `None` while it runs. Carried until capture is
    /// delivering again, so a client that polls between two failures still reads the cause.
    pub capture_note: Option<String>,
    /// Display settings.
    pub display: DisplaySettings,
    /// The current or last manual recording.
    pub recording: RecordingStatus,
    /// Control counters.
    pub stats: Value,
}

/// Whether a run's front end is delivering samples (T-508).
///
/// `finished` alone could not say this: a run that died on a device error and a run whose
/// recording reached its end both read `finished: true`, and a run that was *restarting* capture
/// read exactly like one that was running. The user's report — a live edge frozen with nothing on
/// screen saying so — was the product failing to distinguish these, so the control plane states
/// them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureState {
    /// A segment is running and has delivered samples since it started (or is the first).
    Running,
    /// Capture failed and a new segment is being started, or has started and not yet delivered
    /// a sample. The live edge is not advancing, and that is the true state, not a stall.
    Recovering,
    /// The run has ended. Nothing more will arrive; `capture_note` says why when it was not a
    /// requested stop or the end of a recording.
    Ended,
}

/// How many times in a row capture may be restarted without delivering a sample before the run
/// is ended for real (T-508). Each attempt waits [`recovery_backoff`] first.
pub const MAX_RECOVERY_ATTEMPTS: u32 = 5;

/// The wait before recovery attempt `n` (1-based): 0.25 s doubling to a 4 s cap, about 8 s in all
/// over [`MAX_RECOVERY_ATTEMPTS`] — long enough for a USB hiccup to clear, short enough that a
/// front end that is really gone is reported as gone within seconds.
pub fn recovery_backoff(attempt: u32) -> Duration {
    Duration::from_millis(250u64 << attempt.saturating_sub(1).min(4))
}

/// T-541 — how long a would-be consumer arriving during a **capture recovery** is told the stream
/// is between windows rather than finished ([`Shared::successor_grace_ms`]).
///
/// The whole recovery budget with room for the restarts themselves: [`recovery_backoff`] sums to
/// 7.75 s over [`MAX_RECOVERY_ATTEMPTS`], and each attempt then re-sends the window and starts a
/// segment. Past this the run really has given up and `410` is the honest answer again — which is
/// the point of putting a *bound* on it rather than holding the claim open for ever: a server that
/// says "try again" about a run that has ended is exactly as dishonest as one that says "gone"
/// about a run that is recovering.
pub const RECOVERY_SUCCESSOR_GRACE: Duration = Duration::from_secs(20);

/// How long [`PipelineController::retune`] waits for a re-plumb.
pub const REPLUMB_TIMEOUT: Duration = Duration::from_secs(30);

struct Replumb {
    from: (f64, f64),
    to: (f64, f64),
    class: ContentClass,
}

/// Parts of a run that outlive segments.
struct Common {
    /// T-884: `ConfirmPolicy.synthesized` as the run's inventory holds it, read **once** at start
    /// and kept as a value. MAUTO's attach step runs on its own repository connection outside the
    /// inventory (`hk_cli::pipeline`), so it has to be handed the configured rule; reading it here
    /// rather than through `Shared.inventory` keeps an API read off a mutex a capture worker may
    /// hold. A re-plumb carries the inventory across unchanged, so the value stays true.
    synthesized_confirm: crate::inventory::SynthesizedConfirm,
    /// **The run's inventory** (T-941). One inventory for the whole run: every segment's
    /// [`Shared`] holds a clone of this `Arc`, so a re-plumb hands it to the next segment *by
    /// construction* — there is nothing to move, and so nothing that can fail to be moved.
    ///
    /// It used to be a segment's own ([`Parts`]), moved out of the old [`Shared`] at every
    /// re-plumb. That move needed the old state to be unwrappable and the inventory mutex to be
    /// free, and when a straggler held either, [`take_parts`] **took the inventory from under it
    /// and left the run a [`crate::inventory::NullInventory`]** — permanently. T-941's live
    /// report is what that costs: after one class-boundary retune whose `hk-detect` and
    /// `hk-control` had not stopped within [`REPLUMB_JOIN_BOUND`], detection went on writing rows
    /// (`detections_written` 12 487 → 15 619 in 20 s) while `tracks_opened` never moved again and
    /// `/api/inventory` answered `total 0` — "Nothing on the air" everywhere, until a restart.
    ///
    /// A straggler still inside an `Inventory` call can now only **delay** the next segment's
    /// first inventory write, by the length of that one call, and the run recovers by itself when
    /// it returns. The inventory is also where it belongs: its track→emitter bindings and
    /// re-measurement keys are memory of the *run*, which is why they were carried across a
    /// re-plumb in the first place.
    inventory: Arc<Mutex<Box<dyn Inventory>>>,
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
    /// T-525: the window the control plane last commanded the front end to. Written by every
    /// path that moves it (a re-plumb's `apply_window`, an in-place retune, a capture recovery)
    /// and read by every segment's [`WindowGuard`], so a segment never waits out the bound for a
    /// window a later command has already replaced.
    commanded: Arc<CommandedWindow>,
    user_stop: AtomicBool,
    recorder: Mutex<Option<ManualRecorder>>,
    last_recording: Mutex<RecordingStatus>,
    /// Listen limits in force (T-066; changeable at runtime).
    listen: Arc<Mutex<crate::config::ListenSettings>>,
    /// Burst taps (T-060), closed when the run ends.
    bursts: Arc<crate::chains::taps::BurstHub>,
    /// Compute providers (T-056): built once per run, so no segment changes provider.
    compute: hk_dsp::compute::Compute,
    /// Occupancy engine and series (T-118), closed when the run ends.
    occupancy: Arc<crate::occupancy::OccupancyService>,
    /// T-904: the detection-retention thread, stopped when the run ends (`None`: not configured).
    retention: Option<Arc<crate::retention::RetentionService>>,
    /// T-128: the run's C12 attention service (baselines, candidates), fed by the occupancy
    /// thread and the scheduler; `None` when it could not open.
    attention: Option<Arc<crate::attention::AttentionService>>,
    /// T-131: the run's novelty alarm service (fed by the occupancy thread, served by
    /// `/api/anomalies`); `None` when it could not open.
    alarms: Option<Arc<crate::alarms::AlarmService>>,
    /// T-115: the observation log (`None` when it could not be opened).
    observations: Option<crate::observe::ObservationLog>,
    /// T-844: the C38 shadow stage (`crate::ml`) — the model registry under the data directory,
    /// the modes an operator set, and the durable shadow log. `None` only when its store could not
    /// open, which is reported and never fails the run.
    ml: Option<Arc<crate::ml::MlStage>>,
    /// T-127: the scheduler as the API sees it (snapshot + lease commands), shared by segments.
    scheduler: Arc<crate::control::SchedulerHub>,
    /// T-157: the rolling IQ capture buffer, fed by every segment's `hk-iqbuffer` reader.
    iq_buffer: Arc<crate::iqbuffer::IqBufferService>,
    /// T-399: the receiver-line survey in force, measured once per capture state by every
    /// segment's `hk-survey` reader and read by [`crate::classify::classify_box`]. It lives for
    /// the run, not the segment: a re-plumb landing back on the same device, tune and gain is the
    /// same receiver, and re-measuring it would pay twice for an unchanged answer.
    receiver: Arc<crate::survey::ReceiverSurvey>,
    /// T-439: the **view-scheme** pyramid (`docs/16` §6.2/§8.2), opened beside the floor product's
    /// scheme-1 pair and written by every segment's history reader. It is the surface the unified
    /// canvas addresses with independent `(level_f, level_t)`, and its finest node is the *live
    /// edge* — there is no second write path, only this one, which is the whole point of §8.
    /// `None` when `PipelineSettings::view_history` is off or the store could not open (never a
    /// reason to fail a run: the tile route then folds out of scheme 1 as it did before T-439).
    view: Option<Arc<Mutex<Pyramid>>>,
    /// T-322: the run's C36 L1 dwell service. Like the survey it lives for the run, not the
    /// segment: the constellation overhead and the quiet in-band reference are properties of the
    /// site and the receiver, not of a re-plumb.
    gnss: Arc<crate::gnss::GnssDwell>,
    /// T-517: the run's detection FFT override (`PipelineSettings::fft_len`), kept so
    /// [`PipelineHandle::detection_bin_hz`] answers for any rate a re-plumb could move to.
    detection_fft_len: Option<usize>,
    /// T-508 test seam: segment starts still to fail, at the last step (after every reader has
    /// spawned, before the capture thread), so the cleanup path is the one exercised.
    fail_segment_starts: std::sync::atomic::AtomicU32,
    /// T-541: the first pipeline thread panic of the run, as [`guarded`] recorded it.
    ///
    /// The **first**, not the latest: a panic in one reader usually takes the segment down and
    /// several threads report the fall-out, and the first one is the cause. Cleared when the
    /// supervisor has consumed it as a segment's cause, so a later segment's panic is its own.
    worker_panic: Arc<Mutex<Option<String>>>,
    /// T-510: the run's **further front ends** ([`devices`]), each its own source, ring, capture
    /// thread, detector and history reader writing the shared, already-per-device stores. Empty
    /// for a single-device run, which is then byte-for-byte what it was.
    aux: Vec<devices::AuxDevice>,
    /// T-510: the run has further front ends, so no reader seals the shared history at its own
    /// end; [`devices::finish`] does, once, after every front end has drained.
    defer_seal: bool,
    /// T-510: that one seal has been taken.
    sealed: AtomicBool,
    /// T-541 test seam: `(thread name, how many more of its starts panic)`
    /// ([`PipelineHandle::panic_worker`]). There is no other way to get an unwinding pipeline
    /// thread on demand, and a guard over a panic that no test can produce is not a guard.
    panic_worker: Mutex<Option<(String, u32)>>,
    /// T-534: the run's reopen policy (`--loop`), held for the **run**, not the segment. Every
    /// segment's capture thread reopens through it — the first, one a re-plumb starts and one a
    /// capture recovery starts — so a looping replay keeps wrapping after a retune. It used to be
    /// handed to the first segment alone, and every later segment started with `None`: a
    /// `--loop` run then simply ended at the recording's end after the first retune.
    ///
    /// A mutex because it is `FnMut`; only one capture thread runs at a time (a re-plumb or a
    /// recovery starts the next segment after the last one's threads have ended), so it is never
    /// contended.
    reopen: Option<Arc<Mutex<SourceFactory>>>,
}

impl Common {
    /// T-541 test seam: whether the worker `name` starting now must panic, consuming one of the
    /// armed starts. Decided here rather than inside the thread, so "which starts panic" is a
    /// fact about the spawn order and not about the scheduler.
    fn panic_seam(&self, name: &str) -> bool {
        let mut g = self
            .panic_worker
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match g.as_mut() {
            Some((n, left)) if n == name => {
                if *left != u32::MAX {
                    *left -= 1;
                    if *left == 0 {
                        *g = None;
                    }
                }
                true
            }
            _ => false,
        }
    }

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
    /// T-508: a recovery is under way (set when capture fails, cleared by the next segment that
    /// delivers a sample — read lazily by [`PipelineController::status`]).
    recovering: bool,
    /// T-508: why capture is recovering or why the run ended.
    capture_note: Option<String>,
    /// T-508: `counters.source.samples` when the running segment started, so "has it delivered?"
    /// is a comparison rather than a flag some thread must remember to set.
    samples_at_start: u64,
    /// T-508: the window (and its class) of the last segment that delivered samples — where a
    /// recovery goes back to when retrying the requested window has not worked.
    last_good: ((f64, f64), ContentClass),
    /// T-508: recovery attempts since a segment last delivered samples.
    attempts: u32,
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

/// What a panic payload says, as far as it can be recovered (T-541).
///
/// `panic!("…")` gives a `String` and `panic!("literal")` a `&'static str`; anything else — a
/// `panic_any`, a foreign payload — has no text, and "an unprintable payload" is still better
/// than nothing, because the *fact* of the panic is what the run has to state.
fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
        .unwrap_or_else(|| "an unprintable payload".to_owned())
}

/// **T-541 — a pipeline thread that panics must be observed, not silently absent.**
///
/// Wraps a worker body so an unwinding panic becomes three things instead of a dead thread:
///
/// 1. an `Err` naming the thread and the payload, which [`join_workers`] already collects;
/// 2. a recorded **cause** on the run ([`Common::worker_panic`]), which [`supervise`] turns into
///    `capture_note` — so `/api/status` says what happened rather than `capture: running`;
/// 3. a **stop of the segment**, so the loss is noticed at the next supervisor wake-up.
///
/// (3) is the part that matters. Without it a panicking reader is joined only when the segment
/// ends for some *other* reason, which on a live run may be never: the capture thread keeps
/// filling the ring, `/api/status` keeps saying `running`, and the surface the dead reader fed
/// — the spectrum rows, the history pyramid — simply stops advancing. That is the user's "frozen
/// edge that looks live", produced by a thread nobody was waiting on. Ending the segment costs a
/// restart (a live run recovers; a replay ends with the panic as its stated cause) and buys the
/// invariant that no pipeline thread can die unnoticed.
fn guarded(
    name: &'static str,
    shared: Arc<Shared>,
    stats: Arc<ControlStats>,
    slot: Arc<Mutex<Option<String>>>,
    seam: bool,
    f: Box<dyn FnOnce() -> anyhow::Result<()> + Send>,
) -> impl FnOnce() -> anyhow::Result<()> + Send {
    move || {
        // The seam panics *inside* the guard, not before it: a test seam that escaped the thing
        // it exists to exercise would prove the opposite of what it claims.
        let body = move || {
            assert!(!seam, "injected worker panic (T-541 test seam)");
            f()
        };
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
            Ok(r) => r,
            Err(p) => {
                let why = format!("the {name} thread panicked: {}", panic_text(&*p));
                eprintln!("hk-pipeline: {why}");
                inc(&stats.worker_panics);
                let mut first = slot.lock().unwrap_or_else(PoisonError::into_inner);
                if first.is_none() {
                    *first = Some(why.clone());
                }
                drop(first);
                // The segment is now short a reader: end it rather than run on without one.
                shared.stop.store(true, Ordering::SeqCst);
                Err(anyhow::anyhow!(why))
            }
        }
    }
}

struct Started {
    shared: Arc<Shared>,
    tx: Sender<ControlEvent>,
    workers: Vec<Worker>,
}

impl Pipeline {
    /// Opens the stores, opens the Survey, and starts the threads (capture last).
    pub fn start(
        cfg: PipelineConfig,
        source: Box<dyn Source>,
        info: SourceInfo,
        reopen: Option<crate::run::SourceFactory>,
        inventory: Box<dyn Inventory>,
    ) -> anyhow::Result<PipelineHandle> {
        Self::start_multi(cfg, source, info, reopen, inventory, Vec::new())
    }

    /// [`Self::start`] with **further front ends** (T-510, milestone MSDR): each `extra` source
    /// gets its own ring, capture thread, detector, history reader, IQ capture ring and coverage
    /// observer, writing the run's shared history, view pyramid, repository and observation log
    /// under **its own** `device_id`. [`devices`] has the composition and what a further front end
    /// does not get yet. `source` stays the primary: segments, re-plumbs, chains, the scheduler
    /// and the control plane are its. With `extra` empty this *is* [`Self::start`].
    ///
    /// One self-contained unit with N front ends, never a networked mesh; and N front ends widen
    /// the coverage available to display, they never add a view window.
    ///
    /// Refused before anything opens when a further front end states no identity, shares one with
    /// another front end, cannot pause under a lossless run, or starts more than
    /// [`MAX_START_SKEW`] from the primary (the shared history has one watermark).
    pub fn start_multi(
        mut cfg: PipelineConfig,
        source: Box<dyn Source>,
        info: SourceInfo,
        reopen: Option<crate::run::SourceFactory>,
        inventory: Box<dyn Inventory>,
        extra: Vec<ExtraSource>,
    ) -> anyhow::Result<PipelineHandle> {
        // Every refusal happens here, before a file is created or a device is started.
        let extra_devices = if extra.is_empty() {
            Vec::new()
        } else {
            let primary = source.control().device_info().map(|d| d.device_id);
            devices::validate(&cfg, primary.as_deref(), &info, &extra)?
        };
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
        // T-913: a run that crashed left its survey open, and retention ages an open survey from
        // its own newest row — so its last hour is never aged out until something closes it. One
        // process writes a store, so any survey still open here belongs to a process that is gone.
        match repo.abort_orphaned_surveys() {
            Ok(0) => {}
            Ok(n) => eprintln!("hk-pipeline: aborted {n} survey(s) left open by an earlier run"),
            Err(e) => {
                eprintln!("hk-pipeline: cannot abort surveys left open by an earlier run: {e}")
            }
        }
        repo.insert_survey(&survey)?;
        store_calibrations(&mut repo, &cfg.calibrations)?;
        let product = FloorProduct::open(
            cfg.data_dir.join("history"),
            // T-139: scheduler short-step rows (fewer averages) fold instead of being rejected.
            FloorProductConfig {
                mixed_shapes: true,
                ..FloorProductConfig::default()
            },
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
        let product = Arc::new(Mutex::new(product));
        // The run's receive chain, named once: T-303 keys the baselines by it, and T-314 reads
        // the history through it, so the key and the measurement under it must be the same front
        // end. It therefore comes from the **source's own identity**, which is what stamps the
        // provenance `device_id` of every block and so the history origin of every frame
        // (T-304) — not from `PipelineConfig::device_id`, which is a default (`"sigmf-replay"`)
        // in every path that does not set it and names no device that produced anything. A
        // source that states no identity gives `ChainKey::Unknown`: the baselines pool and the
        // history read is unrestricted, which is honest about what is known, where a key built
        // from the config default would claim a front end and measure another.
        // T-378: the observation log's records name the same front end, so the long-horizon
        // coverage map can answer "did THIS radio look here" and not merely "did anything". One
        // string, read once, hashed into the chain below — a second spelling of "which device"
        // would be a new drift surface.
        let device_id = source.control().device_info().map(|d| d.device_id);
        let chain = device_id.as_deref().map_or(
            hk_model::attention::baseline::ChainKey::Unknown,
            hk_model::attention::baseline::ChainKey::of_device,
        );
        // T-118: the occupancy engine reads history and detections off the real-time path.
        let occupancy = crate::occupancy::OccupancyService::open(
            cfg.data_dir.join("occupancy"),
            Arc::clone(&product),
            db_path.clone(),
            Arc::clone(&counters),
            crate::occupancy::OccupancyConfig::default(),
            chain,
        );
        // T-128: one attention service for the run; never fails the run.
        // T-303: baselines are keyed by the front end the run measures with, so a second device at
        // the same site accrues its own noise floor instead of averaging into this one's.
        let attention = crate::attention::AttentionService::open_for_run(
            &cfg.data_dir,
            &db_path,
            chain,
            Arc::clone(&counters),
        )
        .map_err(|e| eprintln!("attention service disabled: {e:#}"))
        .ok()
        .map(Arc::new);
        occupancy.set_attention(attention.clone());
        // T-131: the alarm service sits next to the attention loop that feeds it, so `hk run` and
        // `hk serve` both raise alarms; never fails the run. Dismissals before any snapshot are
        // stamped with stream time (the wall clock before any frame).
        let alarms = {
            let clock_counters = Arc::clone(&counters);
            let clock: Arc<dyn Fn() -> Timestamp + Send + Sync> = Arc::new(move || {
                let ns = clock_counters.stream_time_ns.load(Ordering::Relaxed);
                if ns > 0 {
                    Timestamp::from_unix_nanos(ns)
                } else {
                    Timestamp::now()
                }
            });
            Repository::open(&db_path)
                .map_err(anyhow::Error::from)
                .and_then(|repo| {
                    crate::alarms::AlarmService::open(
                        Arc::new(Mutex::new(repo)),
                        cfg.stream_sink.as_ref(),
                        clock,
                    )
                })
                .map_err(|e| eprintln!("novelty alarms disabled: {e:#}"))
                .ok()
                .map(Arc::new)
        };
        occupancy.set_alarms(alarms.clone(), cfg.feeds_dir.as_deref());
        let iq_buffer = Arc::new(crate::iqbuffer::IqBufferService::open(
            &cfg,
            db_path.clone(),
        ));
        // T-439: the de-welded view lattice, in its own scheme root beside `calibrated/` and
        // `uncalibrated/`. Its own `Mutex`, deliberately: `/api/history` and `/api/floor` hold the
        // floor product for a whole query, and the growing edge must not be behind that lock.
        // T-484: node (0, 0) is the display STFT's own bin and row, so the canvas's finest tier is
        // the rows the spectrum stream publishes rather than a coarser second STFT's view of the
        // same samples. It is fixed for the life of the pyramid — the store's geometry is checked
        // bit-exactly on reopen — so it is taken from the run's opening rate and configured display
        // plan. A later rate change or display patch still folds honestly (the regrid handles the
        // bin width, the time assignment the row period); it just stops being 1:1.
        let (view_f_cell_hz, view_t_cell) =
            crate::history::view_geometry(info.sample_rate_hz, &cfg.settings, cfg.source_class);
        let view = cfg
            .settings
            .view_history
            .then(|| {
                let dir = cfg.data_dir.join("history").join("view");
                Pyramid::open(
                    &dir,
                    crate::history::view_config(view_f_cell_hz, view_t_cell),
                )
                // T-901: a seal only indexes and queues; the view writer does the file work with
                // the lock released, so a live row push never waits out a seal.
                .and_then(|mut p| p.set_deferred_writes(true).map(|()| p))
                .map(|p| Arc::new(Mutex::new(p)))
                .map_err(|e| eprintln!("view-scheme history disabled: {e}"))
                .ok()
            })
            .flatten();
        // T-322: the C36 L1 dwell service, configured from the plan's `extra.gnss`.
        let gnss = Arc::new(crate::gnss::GnssDwell::new(crate::gnss::gnss_config(
            &cfg.plan,
        )?));
        // T-904: detection retention runs on its own thread and connection, off the real-time
        // path. Started after the last fallible step above, so a failed start leaks no thread.
        let retention = cfg.retention.map(|s| {
            crate::retention::RetentionService::start(db_path.clone(), s, Arc::clone(&counters))
        });
        let common = Common {
            synthesized_confirm: inventory.synthesized_confirm(),
            inventory: Arc::new(Mutex::new(inventory)),
            view,
            iq_buffer,
            receiver: Arc::default(),
            gnss,
            data_dir: cfg.data_dir.clone(),
            db_path,
            survey_id: survey.id,
            counters,
            compute,
            occupancy,
            retention,
            attention,
            alarms,
            product,
            display: Arc::new(DisplayControl::new(DisplaySettings::from_settings(
                &cfg.settings,
            ))),
            // The scheduler holds a switchable handle, repointed when `--loop` reopens the source.
            switch: Arc::new(SwitchableControl::new(source.control())),
            slot: Arc::new(Mutex::new(None)),
            pins: calibration_pins(&cfg.calibrations),
            stats: Arc::new(ControlStats::default()),
            // T-525: the run opens on the window it was opened with; nothing has been commanded.
            commanded: Arc::new(CommandedWindow::new((info.center_hz, info.sample_rate_hz))),
            user_stop: AtomicBool::new(false),
            recorder: Mutex::new(None),
            last_recording: Mutex::new(RecordingStatus::default()),
            listen: Arc::new(Mutex::new(cfg.settings.listen.clone())),
            detection_fft_len: cfg.settings.fft_len,
            bursts: Arc::default(),
            // T-115: never fails the run; a log that cannot open is reported and skipped.
            scheduler: Arc::new(crate::control::SchedulerHub::default()),
            observations: crate::observe::ObservationLog::open(
                &cfg.data_dir,
                device_id,
                cfg.stream_sink.as_ref(),
                (
                    cfg.settings.observation_retention_days,
                    cfg.settings.observation_max_mb,
                ),
            )
            .map_err(|e| eprintln!("observation log disabled: {e:#}"))
            .ok(),
            ml: crate::ml::MlStage::open(&cfg.data_dir)
                .map(Arc::new)
                .map_err(|e| eprintln!("ML shadow stage disabled: {e:#}"))
                .ok(),
            fail_segment_starts: std::sync::atomic::AtomicU32::new(0),
            reopen: reopen.map(|f| Arc::new(Mutex::new(f))),
            worker_panic: Arc::default(),
            panic_worker: Mutex::new(None),
            aux: Vec::new(),
            defer_seal: !extra.is_empty(),
            sealed: AtomicBool::new(false),
        };
        // T-118: visits and tiers from the T-115 log when it opened.
        if let Some(log) = &common.observations {
            common.occupancy.set_observation_store(log.store());
        }
        common.occupancy.start();
        // T-071: the on-demand chain budget is reported from the start of the run.
        crate::chains::listen::publish_limits(
            &common.counters,
            &crate::chains::listen::ListenConfig::from_settings(&cfg.settings.listen),
        );
        let class = cfg.source_class;
        let window = (info.center_hz, info.sample_rate_hz);
        // T-510: every further front end is built from the run's configuration as it stands here.
        let template = (!extra.is_empty()).then(|| cfg.clone());
        let parts = Parts { cfg, repo };
        let Started {
            shared,
            tx,
            mut workers,
        } = start_segment(&common, parts, info, source, None).map_err(|f| f.error)?;
        // T-510: the further front ends start **after** the primary, so a primary that cannot
        // start leaves nothing to tear down. One of them that cannot start ends the run it was
        // part of, rather than leaving a run that is quietly one radio short of what was asked
        // for.
        let mut common = common;
        if let Some(template) = template {
            for (e, dev) in extra.into_iter().zip(extra_devices) {
                match devices::start(&common, &template, e, dev) {
                    Ok(d) => common.aux.push(d),
                    Err(e) => {
                        shared.stop.store(true, Ordering::SeqCst);
                        drop(tx);
                        let mut errors = Vec::new();
                        for (name, h) in workers.drain(..) {
                            if let Ok(Err(err)) = h.join() {
                                errors.push(format!("{name}: {err:#}"));
                            }
                        }
                        devices::finish(&common, &mut errors);
                        common.occupancy.finish();
                        if let Some(r) = &common.retention {
                            r.finish();
                        }
                        if !errors.is_empty() {
                            eprintln!("hk-pipeline: while ending the run: {}", errors.join("; "));
                        }
                        return Err(e);
                    }
                }
            }
        }
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
                recovering: false,
                capture_note: None,
                samples_at_start: 0,
                last_good: (window, class),
                attempts: 0,
            }),
            cv: Condvar::new(),
        });
        let s = Arc::clone(&sup);
        let thread = thread::Builder::new()
            .name("hk-supervisor".into())
            .spawn(move || {
                // **T-541: the supervisor is the one thread whose death nothing else observes.**
                // Every other thread is joined by it; it is joined only by
                // [`PipelineHandle::wait`], which a server calls at shutdown and never before. So
                // an unwinding panic here left `hk serve` answering every route, `finished: false`
                // and `capture: running` for ever, with the live edge frozen — *indistinguishable
                // from a dead process*, which is the exact shape of the defect this ticket exists
                // to rule out. Catching it does not make the panic acceptable; it makes the run
                // **say** it, which is the difference between a degraded server and a lying one.
                let finished = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    supervise(&s, workers)
                })) {
                    Ok(f) => f,
                    Err(p) => {
                        let why = format!("the pipeline supervisor panicked: {}", panic_text(&*p));
                        eprintln!("hk-pipeline: {why}");
                        inc(&s.common.stats.worker_panics);
                        let mut st = s.lock();
                        if let Some(shared) = st.shared.take() {
                            shared.stop.store(true, Ordering::SeqCst);
                        }
                        st.tx = None;
                        st.finished = true;
                        st.recovering = false;
                        st.capture_note = Some(why.clone());
                        // A control caller blocked on a re-plumb would otherwise wait out
                        // `REPLUMB_TIMEOUT` for an answer that is never coming.
                        if st.result.is_none() && st.request.take().is_some() {
                            st.result = Some(Err(ControlFailure::Failed(why.clone())));
                        }
                        drop(st);
                        s.cv.notify_all();
                        Finished {
                            errors: vec![format!("hk-supervisor: {why}")],
                        }
                    }
                };
                // The run is over: burst taps finish their streams.
                s.common.bursts.close();
                finished
            })?;
        Ok(PipelineHandle {
            sup,
            thread: Some(thread),
            started: Instant::now(),
            recipes: Arc::new(std::sync::OnceLock::new()),
            vlf: Arc::default(),
        })
    }
}

/// The parts of a segment that outlive it: handed from each segment to the next at a re-plumb,
/// and — since T-508 — **never lost on a failure**, so a failed re-plumb can start another
/// segment instead of ending the run.
///
/// T-941: the **inventory is not here any more**. It is the run's ([`Common::inventory`]), so
/// there is nothing for a re-plumb to hand over and nothing a straggler can hold it away from.
/// What is left is the configuration and the segment's repository *connection* — a connection is
/// the one thing that is genuinely better per segment: a straggler that never lets go of one
/// blocks only its own segment, and [`take_parts`] opens a fresh one for the next.
struct Parts {
    cfg: PipelineConfig,
    repo: Repository,
}

/// A segment that did not start, with its parts when they could be kept (T-508).
struct SegmentFailure {
    error: anyhow::Error,
    parts: Option<Box<Parts>>,
}

/// Starts one segment's threads (see the module docs). The source is lent first, so it returns
/// to the slot if anything below fails; since T-508 the [`Parts`] come back too, so the caller
/// can try again rather than end the run.
fn start_segment(
    common: &Common,
    parts: Parts,
    info: SourceInfo,
    source: Box<dyn Source>,
    expect: Option<(f64, f64)>,
) -> Result<Started, SegmentFailure> {
    let Parts { cfg, repo } = parts;
    let mut source = Lent::wrap(source, &common.slot);
    if cfg.live_window_class {
        source = WindowGuard::wrap(
            source,
            cfg.source_class,
            expect,
            &common.commanded,
            &common.stats,
        );
    }
    let source = Calibrated::wrap(source, &common.pins);
    let fs = info.sample_rate_hz;
    let (fft_len, averages) = detection_resolution(fs, &cfg.settings);
    let min_block = replay_block_len(fs).min(1024);
    let ring_cfg = RingConfig::for_duration(fs, cfg.settings.ring_s.max(0.5), min_block);
    let (writer, ring) = ring_buffer::<Complex<i8>>(ring_cfg);
    let gate = Arc::new(FlowGate::new(cfg.lossless, ring.sample_capacity()));
    let stop = Arc::new(AtomicBool::new(false));
    // T-534: every segment reopens through the run's one factory, whoever started it.
    let live = cfg.live_window_class && common.switch.capabilities().controllable;
    let reopen: Option<SourceFactory> = common.reopen.as_ref().map(|open| {
        let (open, switch, pins, slot, commanded) = (
            Arc::clone(open),
            Arc::clone(&common.switch),
            Arc::clone(&common.pins),
            Arc::clone(&common.slot),
            Arc::clone(&common.commanded),
        );
        Box::new(move || -> anyhow::Result<Box<dyn Source>> {
            let s = (open.lock().unwrap_or_else(PoisonError::into_inner))()?;
            let control = s.control();
            // T-534: a reopened device starts where *it* defaults to, not where the run was
            // moved. On a live run the commanded window is the truth (a re-plumb or an in-place
            // retune set it), so it is re-sent before the first read; otherwise the segment's
            // [`WindowGuard`] would drop every block of the new pass waiting for a window the
            // device was never told about. A fixed-window run has nothing to re-send.
            if live {
                let (window, _) = commanded.get();
                control
                    .set_sample_rate(window.1)
                    .and_then(|()| control.tune(window.0))
                    .map_err(|e| {
                        anyhow::anyhow!(
                            "re-sending {} Hz / {} Hz to the reopened source failed: {e}",
                            window.0,
                            window.1
                        )
                    })?;
            }
            switch.replace(control);
            Ok(Calibrated::wrap(Lent::wrap(s, &slot), &pins))
        }) as SourceFactory
    });
    let mut sched = if cfg.drive_scheduler {
        match SchedState::new(
            &cfg.plan,
            Arc::clone(&common.switch),
            fs,
            info.start_time,
            Arc::clone(&common.counters),
            cfg.settings.verify_pois,
            common.attention.clone(),
        ) {
            Ok(s) => Some(s),
            Err(error) => {
                return Err(SegmentFailure {
                    error,
                    parts: Some(Box::new(Parts { cfg, repo })),
                });
            }
        }
    } else {
        None
    };
    // T-127: the scheduler publishes to the API hub and serves its lease commands.
    if let Some(s) = sched.as_mut() {
        s.attach_hub(Arc::clone(&common.scheduler));
        // T-322: C04 is asked for the recurring L1 dwell here, once the scheduler exists.
        s.attach_gnss(Arc::clone(&common.gnss), info.start_time);
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
        dc_twin: dc_twin_rule(common),
        receiver: Arc::clone(&common.receiver),
        ml: common.ml.clone(),
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
        inventory: Arc::clone(&common.inventory),
        specs,
        display: Arc::clone(&common.display),
        commanded: Arc::clone(&common.commanded),
        continues: AtomicBool::new(false),
        successor_grace_ms: AtomicU64::new(hk_stream::BETWEEN_WINDOWS_GRACE.as_millis() as u64),
        seal_at_end: !common.defer_seal,
        bursts: Arc::clone(&common.bursts),
        claims: crate::chains::EmissionClaims::default(),
        track_decodes: Arc::default(),
        compute: common.compute.clone(),
        view_queue: common.view.is_some().then(|| {
            Arc::new(crate::history::ViewQueue::new(
                crate::history::VIEW_QUEUE_FRAMES,
            ))
        }),
        cfg,
    });
    inc(&common.stats.segments);

    let (tx, rx) = mpsc::channel::<ControlEvent>();
    let mut workers: Vec<Worker> = Vec::new();
    // Everything below can fail after `shared` exists and readers are running. Run it as one
    // fallible step so a failure can stop and join what did start and hand the parts back
    // (T-508) — the source is already back in the slot by then, because whatever held it has been
    // dropped with this closure.
    let spawned = (|| -> anyhow::Result<()> {
        // T-915: the spectrum reader's claim on the view queue, taken before ANY reader starts —
        // the history reader (spawned first) must not be able to end the view writer ahead of the
        // rows the spectrum reader has yet to push. If this closure fails before that reader is
        // spawned, the claim drops with it and the writer is not left waiting.
        let mut view_producer = shared.view_queue.as_ref().map(|q| q.producer());
        // T-541: every reader goes through `guarded`, so none of them can die unnoticed.
        let spawn = |name: &'static str,
                     f: Box<dyn FnOnce() -> anyhow::Result<()> + Send>|
         -> anyhow::Result<Worker> {
            let body = guarded(
                name,
                Arc::clone(&shared),
                Arc::clone(&common.stats),
                Arc::clone(&common.worker_panic),
                common.panic_seam(name),
                f,
            );
            Ok((name, thread::Builder::new().name(name.into()).spawn(body)?))
        };
        {
            let (s, t) = (Arc::clone(&shared), tx.clone());
            workers.push(spawn(
                "hk-detect",
                Box::new(move || crate::detect::run(s, t)),
            )?);
        }
        {
            let (s, p, a, v) = (
                Arc::clone(&shared),
                Arc::clone(&common.product),
                common.attention.clone(),
                common.view.clone(),
            );
            workers.push(spawn(
                "hk-history",
                Box::new(move || crate::history::run(s, p, a, v)),
            )?);
        }
        if common.iq_buffer.active() {
            // T-157: positioned before the capture thread starts, so the first block is buffered
            // (T-178: once the ring has opened in the background; until then blocks are read and
            // not buffered).
            let (s, b) = (Arc::clone(&shared), Arc::clone(&common.iq_buffer));
            let reader = shared.ring.reader_at(0);
            let cursor = shared.gate.register(0);
            workers.push(spawn(
                "hk-iqbuffer",
                Box::new(move || b.feed(s, reader, cursor)),
            )?);
        }
        {
            let (s, a, vp) = (
                Arc::clone(&shared),
                common.attention.clone(),
                view_producer.take(),
            );
            workers.push(spawn(
                "hk-spectrum",
                Box::new(move || crate::spectrum::run(s, a, vp)),
            )?);
        }
        {
            // T-399: the receiver-line survey. One window per capture state, on its own thread, so
            // nothing on the detection path waits for the second of capture it needs.
            let (s, r) = (Arc::clone(&shared), Arc::clone(&common.receiver));
            workers.push(spawn(
                "hk-survey",
                Box::new(move || crate::survey::run(s, r)),
            )?);
        }
        {
            // T-322: the C36 L1 dwell reader. It holds nothing until C04 grants a scheduled L1 step,
            // so on every run whose plan does not cover L1 it reads the ring and drops it.
            let (s, g) = (Arc::clone(&shared), Arc::clone(&common.gnss));
            workers.push(spawn("hk-gnss", Box::new(move || crate::gnss::run(s, g)))?);
        }
        {
            let s = Arc::clone(&shared);
            let attention = common.attention.clone();
            workers.push(spawn(
                "hk-control",
                Box::new(move || crate::control::run(s, rx, sched, interactive, attention)),
            )?);
        }
        // T-508 test seam: fail here, with every reader running, so the cleanup below is exercised.
        if common
            .fail_segment_starts
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            anyhow::bail!("injected segment-start failure (T-508 test seam)");
        }
        // T-541: the capture thread gets the same guard as the readers. `join_workers` already
        // treated a panicked `hk-capture` as a capture failure, which is right, but it could only
        // report the word "panicked"; through `guarded` the payload reaches `capture_note`, so
        // `/api/status` names what went wrong on the one thread the run cannot do without.
        let (s, stats, slot, seam) = (
            Arc::clone(&shared),
            Arc::clone(&common.stats),
            Arc::clone(&common.worker_panic),
            common.panic_seam("hk-capture"),
        );
        let capture = hk_core::rt::spawn_capture_thread("hk-capture", move |_priority| {
            let inner: Box<dyn FnOnce() -> anyhow::Result<()> + Send> = {
                let s = Arc::clone(&s);
                Box::new(move || crate::capture::run(source, reopen, writer, s))
            };
            let r = guarded("hk-capture", Arc::clone(&s), stats, slot, seam, inner)();
            if r.is_err() {
                s.stop.store(true, Ordering::SeqCst);
            }
            r
        })?;
        workers.insert(0, ("hk-capture", capture));
        Ok(())
    })();
    if let Err(error) = spawned {
        shared.stop.store(true, Ordering::SeqCst);
        drop(tx);
        let mut ignored = Vec::new();
        join_workers_bounded(&mut workers, &mut ignored, SEGMENT_JOIN_BOUND);
        let parts = take_parts(common, shared)
            .map_err(|e| eprintln!("{e}"))
            .ok()
            .map(Box::new);
        return Err(SegmentFailure { error, parts });
    }
    Ok(Started {
        shared,
        tx,
        workers,
    })
}

/// T-174: the live DC-twin rule uses the occupancy engine's grid (level-0 cells). One rule for
/// the run: it is a property of the shared history grid, so every front end reads the same one.
fn dc_twin_rule(common: &Common) -> hk_context::occupancy::channels::DcTwinRule {
    let p = common
        .product
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let py = &p.config().pyramid;
    hk_context::occupancy::channels::DcTwinRule {
        f_cell_hz: py.f_cell_hz,
        slack_ns: i64::try_from(py.t_cell.as_nanos())
            .unwrap_or(1_000_000_000)
            .max(1),
        lo_tolerance_hz: hk_detect::DcRule::default().tolerance_hz,
    }
}

/// What the supervisor leaves for [`PipelineHandle::wait`].
struct Finished {
    errors: Vec<String>,
}

/// Joins a segment's threads. Returns the capture thread's error, if it ended on one — the one
/// failure the supervisor treats as "the front end stopped delivering" (T-508).
///
/// **The wait is bounded whenever the control plane is waiting behind it** (T-542). It used to be
/// unbounded in every case, on the reasoning quoted at [`report_stragglers`] — each of these
/// threads owns state the run is about to close, so abandoning one is worse than waiting. That is
/// right for a *stop*, where the process is going away and nobody is left to be kept waiting. It
/// is not right for a **re-plumb**, where the whole run is held hostage to whichever worker is
/// slowest: capture is off, the spectrum stream has no publisher, and `retune` is blocked on a
/// condvar. Measured on the live HackRF under a 1 MHz–6 GHz sweep at dwell 1 s, one such join ran
/// **88 seconds** while ten `hk-chain-fsk-bursts-*` threads kept reading a ring whose segment had
/// ended — from the user's seat, the backend was down.
///
/// The root cause of that particular straggler is fixed where it belongs
/// ([`crate::chains::ChainReader::next`] now reads a stopped segment as closed). This is the
/// guard, and it is a guard the file already had a shape for: past the bound the thread is named,
/// counted and left behind, and [`take_parts`] salvages the segment state it is still holding —
/// the path written for exactly this ("a chain or tap thread that did not end with its segment"),
/// counted as `segments_salvaged`.
fn join_workers(
    sup: &Supervisor,
    workers: &mut Vec<Worker>,
    errors: &mut Vec<String>,
) -> Option<String> {
    let stopping = sup.common.user_stop.load(Ordering::SeqCst);
    if stopping {
        report_stragglers(workers);
    }
    // The bound starts when a re-plumb starts waiting, which is normally *after* this join is
    // already blocked: the supervisor sits here for the whole life of a segment, and `retune` sets
    // the request and the stop flag from another thread. So it is discovered inside the wait, not
    // decided before it.
    let mut deadline: Option<Instant> = None;
    let mut capture = None;
    for (name, join) in workers.drain(..) {
        // **`hk-capture` is never abandoned.** It owns the device: the re-plumb's very next act is
        // to take the source back out of `Common::slot`, which only this thread puts there, so
        // leaving it behind does not get the radio moving sooner — it guarantees the re-plumb
        // fails. It is also the one worker whose stop is already bounded by the driver (a HackRF
        // read returns every 50 ms and errors at `STALL_TIMEOUT`), which is why it was never the
        // straggler in any of the measurements.
        let abandonable = name != "hk-capture";
        let mut abandoned = false;
        while !join.is_finished() {
            if let Some(d) = deadline.filter(|_| abandonable) {
                if Instant::now() >= d {
                    abandoned = true;
                    break;
                }
            } else if !stopping && deadline.is_none() && sup.lock().request.is_some() {
                deadline = Some(Instant::now() + REPLUMB_JOIN_BOUND);
            }
            // 50 ms, not 2: this poll runs for the whole life of a segment (the supervisor waits
            // here while the run is healthy), and it is measured against a bound of 8 s.
            //
            // **Once a re-plumb is waiting, 2 ms** (T-536). `deadline` is armed only when a
            // request is pending, which is exactly the case where the poll interval is not idle
            // bookkeeping but dead air: capture is off, `retune` is on a condvar and every worker
            // here is already stopping. Measured with nothing else wrong, a re-plumb's join was
            // 50–60 ms with the IQ buffer off, almost all of it this sleep rounding up the one
            // worker that had not quite finished.
            let idle = deadline.is_none();
            thread::sleep(if idle {
                Duration::from_millis(50)
            } else {
                Duration::from_millis(2)
            });
        }
        if abandoned {
            // Said out loud, not only counted: "the re-plumb is taking a while" and "a worker of
            // the last segment never stopped" are different facts, and only the second one names
            // what to go and look at.
            eprintln!(
                "{name} did not stop within {REPLUMB_JOIN_BOUND:?} of its segment ending; the \
                 re-plumb goes on without it and its segment state is salvaged"
            );
            errors.push(format!(
                "{name}: still running {REPLUMB_JOIN_BOUND:?} after its segment ended; left behind"
            ));
            // T-941: counted, not only printed and pushed into `errors` — `errors` reaches the
            // user when the run *ends*, and this is a fact about a run that is still going.
            inc(&sup.common.stats.workers_abandoned);
            continue;
        }
        let failed = match join.join() {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(format!("{e:#}")),
            Err(_) => Some("panicked".to_owned()),
        };
        if let Some(e) = failed {
            if name == "hk-capture" {
                capture = Some(e.clone());
            }
            errors.push(format!("{name}: {e}"));
        }
    }
    capture
}

/// **How long the supervisor waits for the last segment's workers before a re-plumb goes on
/// without them** (T-542).
///
/// It has to be shorter than the two bounds a client is judged against, or the guard changes
/// nothing a user can see: `hk_stream::BETWEEN_WINDOWS_GRACE` (30 s, past which a would-be
/// consumer of `spectrum/live` is told the stream is over) and [`REPLUMB_TIMEOUT`] (30 s, past
/// which `retune` gives up on the re-plumb it asked for). It also has to be long enough that an
/// ordinary drain never trips it — the readers observe `stop` within a block, and a chain within
/// one 20 ms ring read.
const REPLUMB_JOIN_BOUND: Duration = Duration::from_secs(8);

/// How long a segment that failed to start may take to wind down what it did start.
const SEGMENT_JOIN_BOUND: Duration = Duration::from_secs(10);

/// How long [`join_workers`] waits before naming the threads it is still waiting for (T-531).
const STRAGGLER_REPORT_AFTER: Duration = Duration::from_secs(3);

/// Names, once, the workers that have not finished after [`STRAGGLER_REPORT_AFTER`].
///
/// Only on a **stop**: a segment ending for a re-plumb legitimately waits on a slow step (opening
/// a multi-gigabyte IQ ring, for one), and that is not the thing this is looking for.
///
/// The join below is unbounded on purpose — every one of these threads owns state the run is about
/// to close, so abandoning one to close the stores underneath it is worse than waiting. But an
/// unbounded wait with nothing to show for it is how a 78-second SIGTERM (T-525) became a mystery:
/// the process sat there and no one could say which thread was not observing the stop. This costs
/// nothing when the drain is prompt (it returns as soon as they are all finished, which is the
/// wait the join would have done anyway) and says the name out loud when it is not.
fn report_stragglers(workers: &[Worker]) {
    let deadline = Instant::now() + STRAGGLER_REPORT_AFTER;
    while Instant::now() < deadline {
        if workers.iter().all(|(_, j)| j.is_finished()) {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
    let left: Vec<&str> = workers
        .iter()
        .filter(|(_, j)| !j.is_finished())
        .map(|(name, _)| *name)
        .collect();
    if !left.is_empty() {
        eprintln!(
            "hk-pipeline: still waiting after {STRAGGLER_REPORT_AFTER:?} for: {}",
            left.join(", ")
        );
    }
}

/// [`join_workers`] with a bound: a thread still running after `bound` is left behind (and named
/// in `errors`) rather than holding up a recovery for ever.
fn join_workers_bounded(workers: &mut Vec<Worker>, errors: &mut Vec<String>, bound: Duration) {
    let deadline = Instant::now() + bound;
    for (name, join) in workers.drain(..) {
        while !join.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        if !join.is_finished() {
            errors.push(format!(
                "{name}: still running after {bound:?}; left behind"
            ));
            continue;
        }
        match join.join() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => errors.push(format!("{name}: {e:#}")),
            Err(_) => errors.push(format!("{name}: panicked")),
        }
    }
}

/// A re-plumb or segment start that failed, with whatever could be kept (T-508).
struct Failure {
    parts: Option<Box<Parts>>,
    why: String,
}

/// Runs segments until the run ends (see the module docs).
///
/// # T-508: a failure restarts capture; only a stop or the end of a recording ends the run
///
/// A segment ends for one of four reasons, and before T-508 three of them ended the run for good
/// while `hk serve` stayed up reporting `finished: true` to a client that showed nothing:
///
/// 1. **A re-plumb was requested** — start the next segment on the new window. A capture failure
///    that arrived in the same wake-up is *counted* (T-529) and then superseded: the re-plumb
///    re-commands the whole window, so recovering first would be work undone.
/// 2. **The re-plumb itself failed** — the old segment's state still held by a straggler past
///    [`unwrap_shared`]'s bound, the device not handed back, or [`start_segment`] failing. The
///    state is now salvaged ([`take_parts`]) and the parts survive a failed start, so this goes to
///    recovery.
/// 3. **The capture thread's read failed on a live run** — how a HackRF reports a retune it could
///    not apply (its driver applies controls on the capture thread and a failed libhackrf call is
///    a read error), and how it reports a USB stall. Recovery.
/// 4. **The source ended, or a stop was requested** — the run ends. A recording reaching its end
///    is a real end; so is the user stopping it.
///
/// Recovery ([`recover`]) re-sends the whole window to the device (never a difference: the point
/// is to put the front end into a *known* state), first the window the failed segment was built
/// for, then the last window that delivered samples, with [`recovery_backoff`] between attempts.
/// A segment that delivers a sample resets the count. After [`MAX_RECOVERY_ATTEMPTS`] in a row
/// that delivered nothing the run ends — **visibly**: [`CaptureState::Ended`] with the last
/// failure in `capture_note`.
fn supervise(sup: &Supervisor, mut workers: Vec<Worker>) -> Finished {
    let c = &sup.common;
    let mut errors = Vec::new();
    loop {
        // T-542: a re-plumb is a caller waiting on a condvar with the radio stopped, so its join is
        // bounded. A stop is not — nothing is waiting behind it and the state is about to close.
        let capture_failed = join_workers(sup, &mut workers, &mut errors);
        // **T-541: a reader that panicked is a segment failure like any other.** [`guarded`] has
        // already stopped the segment and recorded the cause; taking it here is what turns it
        // from a line in `errors` that nobody reads into the run's *stated* reason for
        // recovering or ending. Taken unconditionally, so a panic that arrived alongside a
        // capture failure cannot leak into the next segment's decision — and `or` keeps the
        // capture thread's own error first when both are set, because that is the one the
        // recovery is about.
        let panicked = c
            .worker_panic
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let capture_failed = capture_failed.or(panicked);
        let mut st = sup.lock();
        if get(&c.counters.source.samples) > st.samples_at_start {
            st.last_good = (st.window, st.class);
            st.attempts = 0;
            st.recovering = false;
        }
        let req = st.request.take();
        if c.user_stop.load(Ordering::SeqCst) {
            if req.is_some() {
                st.result = Some(Err(ControlFailure::Finished("the run is stopping".into())));
            }
            return end_run(sup, st, errors, None);
        }
        // A re-plumb that failed is answered once recovery has settled where capture went.
        let mut answer: Option<Replumb> = None;
        let failure = match (req, capture_failed) {
            (Some(req), failed) => {
                // **T-529: a device failure that arrives together with a re-plumb request is still
                // a device failure.** This branch takes the request and goes on, which is right —
                // the re-plumb is about to re-command the whole window anyway, so recovering first
                // would be work undone. What was wrong is that the failure vanished from the
                // *count*: `capture_failures` stayed 0 while a front end had just refused a
                // change, so "the device refused something" and "nothing went wrong" read
                // identically on `/api/status`.
                //
                // It is not a hypothetical. Until T-529 one user retune was two posts, so the
                // second post's request routinely landed on the first segment's first read — which
                // is where a HackRF reports a refused retune — and T-508's one-shot mock fault was
                // swallowed here about half the time. That is what made `canvas-journey` test 5
                // flake. Counting it costs nothing and cannot change control flow; the error text
                // is already in `errors` from `join_workers`.
                if let Some(e) = &failed {
                    inc(&c.stats.capture_failures);
                    eprintln!("the front end failed while a re-plumb was queued: {e}");
                }
                // **T-541 audit.** `shared` is `None` only between the two `take`s in this loop
                // and the `install` that follows each of them, and the loop is the only writer,
                // so at the top of an iteration it is always `Some`. It is left as an `expect`
                // rather than rewritten: the invariant is real and an `if let` here would
                // silently skip a segment's teardown instead of stating that the loop is wrong.
                // What T-541 changes is the *consequence* — this thread now unwinds into
                // `catch_unwind` at its spawn, which ends the run with the panic as its stated
                // cause, rather than leaving `hk serve` reporting `capture: running` for ever.
                let old = st.shared.take().expect("a running segment");
                st.tx = None;
                drop(st);
                match replumb(c, old, &req) {
                    Ok((started, outcome, window, class)) => {
                        inc(&c.stats.replumbs);
                        workers = install(sup, started, window, class, Some(outcome));
                        continue;
                    }
                    Err(f) => {
                        inc(&c.stats.replumb_failures);
                        errors.push(format!("re-plumb: {}", f.why));
                        let mut st = sup.lock();
                        // The re-plumb was going to the requested window; recovery retries it.
                        st.window = req.to;
                        st.class = req.class;
                        answer = Some(req);
                        f
                    }
                }
            }
            (None, Some(e)) if st.live => {
                inc(&c.stats.capture_failures);
                let old = st.shared.take().expect("a running segment");
                st.tx = None;
                drop(st);
                Failure {
                    parts: take_parts(c, old)
                        .map_err(|e| eprintln!("{e}"))
                        .ok()
                        .map(Box::new),
                    why: format!("the front end stopped delivering: {e}"),
                }
            }
            // The source ended (a recording's end is not a failure), or a source that cannot be
            // re-commanded failed: the run ends, and a failure says why.
            (None, e) => {
                let why = e.map(|e| format!("the source failed: {e}"));
                return end_run(sup, st, errors, why);
            }
        };
        let cause = failure.why.clone();
        match recover(sup, failure) {
            Ok(w) => {
                workers = w;
                if let Some(req) = answer {
                    let mut st = sup.lock();
                    st.result = Some(if st.window == req.to {
                        Ok(RetuneOutcome {
                            content_class: st.class,
                            replumbed: true,
                            segment: st.segment,
                        })
                    } else {
                        Err(ControlFailure::Failed(format!(
                            "the re-plumb failed ({cause}); capture restarted on {} Hz / {} Hz",
                            st.window.0, st.window.1
                        )))
                    });
                    sup.cv.notify_all();
                }
            }
            Err(why) => {
                errors.push(format!("capture: {why}"));
                let mut st = sup.lock();
                if answer.is_some() {
                    st.result = Some(Err(ControlFailure::Failed(format!(
                        "the re-plumb failed and capture could not be restarted: {why}"
                    ))));
                }
                return end_run(sup, st, errors, Some(why));
            }
        }
    }
}

/// Marks the run finished; `why` is set when it ended on a failure rather than a stop or the end
/// of its source.
fn end_run(
    sup: &Supervisor,
    mut st: MutexGuard<'_, SupState>,
    errors: Vec<String>,
    why: Option<String>,
) -> Finished {
    let mut errors = errors;
    // T-510: the run's one end. Every further front end stops and drains here, and the shared
    // history's single end-of-run seal is taken once, after all of them have — never by whichever
    // reader finished first. A no-op on a single-device run.
    devices::finish(&sup.common, &mut errors);
    sup.common.occupancy.finish(); // T-118: close the last interval after the readers drain
    if let Some(r) = &sup.common.retention {
        r.finish(); // T-904
    }
    st.finished = true;
    st.recovering = false;
    // A requested stop or a recording's end carries no note, whatever an earlier, recovered
    // failure left behind.
    st.capture_note = why;
    sup.cv.notify_all();
    Finished { errors }
}

/// Installs a started segment as the running one; returns its workers.
fn install(
    sup: &Supervisor,
    started: Started,
    window: (f64, f64),
    class: ContentClass,
    outcome: Option<Result<RetuneOutcome, ControlFailure>>,
) -> Vec<Worker> {
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
    st.samples_at_start = get(&sup.common.counters.source.samples);
    if let Some(outcome) = outcome {
        // A requested re-plumb, not a recovery: nothing is being recovered from.
        st.recovering = false;
        st.result = Some(outcome.map(|mut o| {
            o.segment = st.segment;
            o
        }));
    }
    sup.cv.notify_all();
    started.workers
}

/// Starts a new segment after a failure (T-508; see [`supervise`]). `Err` only when capture cannot
/// be restarted at all: the attempts are used up, the device was not handed back, the segment's
/// state was lost, or a stop was requested meanwhile.
fn recover(sup: &Supervisor, failure: Failure) -> Result<Vec<Worker>, String> {
    let c = &sup.common;
    let Failure { mut parts, mut why } = failure;
    loop {
        let (attempt, window, class) = {
            let mut st = sup.lock();
            st.attempts += 1;
            st.recovering = true;
            st.capture_note = Some(why.clone());
            sup.cv.notify_all();
            if st.attempts > MAX_RECOVERY_ATTEMPTS {
                return Err(format!(
                    "capture could not be restarted: {MAX_RECOVERY_ATTEMPTS} attempts in a row \
                     delivered nothing; the last failure: {why}"
                ));
            }
            // First the window the failed segment was built for (a transient fault clears, and
            // the user's retune still lands); after that, the last window that delivered.
            let (window, class) = if st.attempts == 1 {
                (st.window, st.class)
            } else {
                st.last_good
            };
            (st.attempts, window, class)
        };
        let wake = Instant::now() + recovery_backoff(attempt);
        while Instant::now() < wake {
            if c.user_stop.load(Ordering::SeqCst) {
                return Err("stopped while capture was being restarted".into());
            }
            thread::sleep(Duration::from_millis(20));
        }
        let Some(mut p) = parts.take() else {
            return Err(format!(
                "the segment's state could not be recovered, so capture cannot restart ({why})"
            ));
        };
        let Some(source) = c.slot.lock().unwrap_or_else(PoisonError::into_inner).take() else {
            return Err(format!(
                "the capture thread did not hand the device back, and a device cannot be reopened \
                 from inside a run ({why})"
            ));
        };
        eprintln!(
            "capture recovery {attempt}/{MAX_RECOVERY_ATTEMPTS}: restarting on {} Hz / {} Hz after: \
             {why}",
            window.0, window.1
        );
        // The whole window, not a difference: the device's state is what is unknown here.
        c.commanded.set(window);
        if let Err(e) = c
            .switch
            .set_sample_rate(window.1)
            .and_then(|()| c.switch.tune(window.0))
        {
            *c.slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(source);
            parts = Some(p);
            why = format!(
                "re-sending {} Hz / {} Hz to the device failed: {e}",
                window.0, window.1
            );
            continue;
        }
        p.cfg.source_class = class;
        let info = SourceInfo {
            center_hz: window.0,
            sample_rate_hz: window.1,
            start_time: Timestamp::from_unix_nanos(
                c.counters.stream_time_ns.load(Ordering::Relaxed),
            ),
        };
        match start_segment(c, *p, info, source, Some(window)) {
            Ok(started) => {
                inc(&c.stats.capture_recoveries);
                return Ok(install(sup, started, window, class, None));
            }
            Err(f) => {
                parts = f.parts;
                why = format!("restarting the segment failed: {:#}", f.error);
            }
        }
    }
}

/// How long [`unwrap_shared`] waits for the old segment's last holder to let go.
const UNWRAP_BOUND: Duration = Duration::from_secs(5);

/// The old segment's state, once every thread has let go of it; `Err` hands the `Arc` back when
/// one still holds it after [`UNWRAP_BOUND`].
fn unwrap_shared(mut arc: Arc<Shared>) -> Result<Shared, Arc<Shared>> {
    let deadline = Instant::now() + UNWRAP_BOUND;
    loop {
        match Arc::try_unwrap(arc) {
            Ok(s) => return Ok(s),
            Err(a) if Instant::now() < deadline => {
                arc = a;
                thread::sleep(Duration::from_millis(2));
            }
            Err(a) => return Err(a),
        }
    }
}

/// The [`Parts`] of a segment that has ended: unwrapped when nothing else holds its state, else
/// **salvaged** (T-508). A straggler still holding the old `Shared` — a chain or tap thread that
/// did not end with its segment — used to end the run ("a thread of the previous segment still
/// holds its state"). Now the next segment gets its own database connection, counted as
/// `segments_salvaged`. `Err` only when the database cannot be opened again.
///
/// # T-941: what a straggler can no longer take with it
///
/// This used to salvage the **inventory** too, by taking it out from under the straggler with a
/// one-second `try_lock` and leaving a [`crate::inventory::NullInventory`] behind. When that lock
/// was busy — which is exactly what a straggler *inside* an inventory call is — it printed "the
/// old segment's inventory is locked by its straggler; continuing without inventory" and the run
/// went on with a null inventory **for the rest of its life**: rows kept being written and no
/// track, candidate or emitter ever reached the user again (the report at
/// [`Common::inventory`]).
///
/// The inventory is the run's now, so this function never touches it: the next segment already
/// has it. A straggler holding the inventory lock delays that segment's first inventory write by
/// the length of its own call and nothing else — and the whole "continue without X" shape is
/// gone, because there is no X to continue without.
fn take_parts(c: &Common, old: Arc<Shared>) -> Result<Parts, String> {
    let arc = match unwrap_shared(old) {
        Ok(s) => {
            return Ok(Parts {
                cfg: s.cfg,
                repo: s.repo.into_inner().unwrap_or_else(PoisonError::into_inner),
            });
        }
        Err(arc) => arc,
    };
    inc(&c.stats.segments_salvaged);
    eprintln!(
        "segment state still held {} s after its threads ended ({} holders); salvaging it (the \
         run's inventory is not the segment's and is unaffected)",
        UNWRAP_BOUND.as_secs(),
        Arc::strong_count(&arc) - 1
    );
    let repo = Repository::open(&c.db_path).map_err(|e| {
        format!("a thread of the previous segment still holds its state, and reopening the database failed: {e}")
    })?;
    Ok(Parts {
        cfg: arc.cfg.clone(),
        repo,
    })
}

/// Moves the front end to `to`, and records it as the window the run is commanding (T-525).
///
/// The record is made **before** the calls, not after: the whole point is that a later command can
/// arrive while these are still in the device's mailbox, and the guard's question is only ever
/// "which window did the control plane ask for last". A call that then fails leaves the record
/// pointing at a window the device is not on — which is exactly the case
/// [`WINDOW_SETTLE_TIMEOUT`] bounds, and the caller's revert records `from` over it.
fn apply_window(
    control: &dyn SourceControl,
    commanded: &CommandedWindow,
    to: (f64, f64),
    from: (f64, f64),
) -> Result<(), SourceError> {
    commanded.set(to);
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

/// Steps 2–5 of the module docs, after the old segment's threads have all ended. A failure keeps
/// the [`Parts`] whenever it can, so [`supervise`] can restart capture (T-508).
fn replumb(common: &Common, old: Arc<Shared>, req: &Replumb) -> Result<Replumbed, Failure> {
    common.finish_recorder();
    let mut parts = take_parts(common, old).map_err(|why| Failure { parts: None, why })?;
    let Some(source) = common
        .slot
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
    else {
        return Err(Failure {
            parts: Some(Box::new(parts)),
            why: "the capture thread did not hand the source back".into(),
        });
    };
    let old_class = parts.cfg.source_class;
    let (window, class, outcome) =
        match apply_window(common.switch.as_ref(), &common.commanded, req.to, req.from) {
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
                //
                // **T-497: the revert's own failure is reported, not discarded.** It used to be
                // `let _ =`, and then the segment started `expect`ing `req.from` — a window the revert
                // had just failed to restore. [`WindowGuard`] would then drop every block for ever
                // (it now gives up after [`WINDOW_SETTLE_TIMEOUT`] instead), and the caller was told
                // only about the first failure, so "the retune was refused and the radio is back where
                // it was" and "the retune was refused and nobody knows where the radio is" read
                // identically. They are not the same fact.
                match apply_window(common.switch.as_ref(), &common.commanded, req.from, req.to) {
                    Ok(()) => (req.from, old_class, Err(ControlFailure::Source(e))),
                    Err(back) => (
                        req.from,
                        old_class,
                        Err(ControlFailure::Failed(format!(
                            "the retune failed ({e}) and restoring the previous window failed too \
                         ({back}): the front end may not be on {} Hz / {} Hz",
                            req.from.0, req.from.1
                        ))),
                    ),
                }
            }
        };
    parts.cfg.source_class = class;
    let info = SourceInfo {
        center_hz: window.0,
        sample_rate_hz: window.1,
        start_time: Timestamp::from_unix_nanos(
            common.counters.stream_time_ns.load(Ordering::Relaxed),
        ),
    };
    let started =
        start_segment(common, parts, info, source, Some(window)).map_err(|f| Failure {
            parts: f.parts,
            why: format!("starting the new segment: {:#}", f.error),
        })?;
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
///
/// # One method reaches the device (T-343)
///
/// [`PipelineController::retune`] is a **device action**: it moves the front end, and when the
/// window's content class or sample rate changes it stops the running segment
/// (`shared.stop.store(true)`) and re-plumbs the run, tearing down and restarting the always-on
/// readers. It is the only method here that can do that.
///
/// [`PipelineController::set_display`], [`PipelineController::start_recording`] and
/// [`PipelineController::stop_recording`] are **view and output controls**: capture, the ring and
/// detection are always-on and none of them stops or slows the source (T-339, pinned by
/// `tests/view_pause_keeps_capture.rs`).
///
/// There is **no pause here at all** (T-347): holding the view is the client's own time window, so
/// no control on this type — and no route in front of it — can stop the run's rows for everyone.
///
/// So a caller must reach `retune` only for an explicit user request to move the front end —
/// never as the continuation of a pan, a zoom or a scrub, which change what is shown and nothing
/// else. `hk_api::control`'s `Action::device_action` is the same split at the HTTP boundary, and
/// `hk_api::DeviceGate` serialises device actions so a retune cannot race another holder of the
/// device.
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
        // Recovering until the new segment has delivered a sample: a segment that started and
        // then died on its first read is not "running", whatever `shared` says.
        let delivering = get(&c.counters.source.samples) > st.samples_at_start;
        let capture = if st.finished {
            CaptureState::Ended
        } else if st.recovering && !delivering {
            CaptureState::Recovering
        } else {
            CaptureState::Running
        };
        ControlStatus {
            live: st.live,
            content_class: st.class,
            center_hz: st.window.0,
            sample_rate_hz: st.window.1,
            segment: st.segment,
            replumbing: st.request.is_some() || (st.shared.is_none() && !st.finished),
            finished: st.finished,
            capture,
            capture_note: (capture != CaptureState::Running)
                .then(|| st.capture_note.clone())
                .flatten(),
            display: c.display.get(),
            recording: self.recording(),
            stats: serde_json::json!({
                "segments": get(&stats.segments),
                "replumbs": get(&stats.replumbs),
                "retunes_in_place": get(&stats.retunes_in_place),
                "blocks_dropped_window": get(&stats.blocks_dropped_window),
                // T-497: non-zero means a re-plumb's requested window never arrived and capture
                // resumed on whatever the front end is actually on. Served rather than kept
                // internal, because "the radio is not where you asked it to be" is exactly the kind
                // of thing this product refuses to render as if it were.
                "window_settle_timeouts": get(&stats.window_settle_timeouts),
                // T-525: how often a segment's requested window was replaced by a later command
                // before it arrived. Ordinary under a fast sweep; read it beside
                // `window_settle_timeouts`, which is the defect.
                "window_commands_superseded": get(&stats.window_commands_superseded),
                // T-508: a device read failed on a live run / a re-plumb failed, and how many times
                // capture was restarted after either. Non-zero failures with matching recoveries
                // is a front end that misbehaved and a run that kept going.
                "capture_failures": get(&stats.capture_failures),
                "replumb_failures": get(&stats.replumb_failures),
                "capture_recoveries": get(&stats.capture_recoveries),
                "segments_salvaged": get(&stats.segments_salvaged),
                // T-941: threads of an earlier segment left behind past `REPLUMB_JOIN_BOUND`. A
                // straggler no longer costs the run its inventory, but it is still a thread that
                // did not stop, and this is where a client can see that it happened.
                "workers_abandoned": get(&stats.workers_abandoned),
                // T-541: pipeline threads that ended by panicking. Always a defect, and served
                // rather than kept internal for the same reason as `window_settle_timeouts`:
                // a fault the system handled is still a fault the operator should be able to see.
                "worker_panics": get(&stats.worker_panics),
                "recordings_started": get(&stats.recordings_started),
                "recordings_refused_class": get(&stats.recordings_refused_class),
            }),
        }
    }

    /// Moves a live run to `(center_hz, sample_rate_hz)`. A window with the run's class and rate
    /// is tuned in place; any other re-plumbs the run with the window's class at a block boundary
    /// and returns once the new segment runs (see the module docs), or [`ControlFailure::Timeout`]
    /// after [`REPLUMB_TIMEOUT`].
    ///
    /// **This is a device action** (T-343), and the only method on this controller that can stop
    /// the running segment: a re-plumb tears down and restarts the segment's always-on readers.
    /// Call it for an explicit user request to move the front end, never as the continuation of a
    /// view change — panning, zooming, scrubbing and pausing must not reach it.
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
            return Err(ControlFailure::Conflict(if st.recovering {
                "capture is being restarted after a device failure".into()
            } else {
                "a re-plumb is in progress".into()
            }));
        };
        let class = window_class(center_hz, sample_rate_hz);
        if class == st.class && sample_rate_hz == st.window.1 {
            // T-525: recorded before the call, so a segment still waiting for the window the
            // previous command asked for learns that this one has replaced it. The device's
            // control mailbox coalesces; without this the earlier window is one the front end
            // will never report and the guard waits out the whole settle bound for it.
            self.sup.common.commanded.set((center_hz, sample_rate_hz));
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

/// A segment's state held from outside, as a straggling thread would ([`PipelineHandle::hold_segment`]).
#[doc(hidden)]
pub struct SegmentHold(#[allow(dead_code)] Arc<Shared>);

/// **Test seam (T-941).** The run's inventory held *locked*, from a thread that also holds the
/// running segment's state — a straggler caught inside an `Inventory` call, which is the state the
/// live report was taken in ([`Common::inventory`]). Dropping it releases both.
#[doc(hidden)]
pub struct InventoryHold {
    release: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for InventoryHold {
    fn drop(&mut self) {
        self.release.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// A running pipeline.
pub struct PipelineHandle {
    sup: Arc<Supervisor>,
    thread: Option<JoinHandle<Finished>>,
    started: Instant,
    /// The run's recipe runtime (T-088), created on first use. Shared (rather than owned) so a
    /// service handed out by this handle — the `listen` opener's recipe path, T-869 — can build
    /// it later without holding the handle.
    recipes: Arc<std::sync::OnceLock<Arc<crate::recipes::runtime::RecipeRuntime>>>,
    /// T-891: accessory-fed VLF services attached to this run (none by default); stopped with it.
    vlf: Arc<crate::vlf::VlfServices>,
}

/// The run's recipe runtime, built on first use ([`PipelineHandle::recipe_runtime`]). A free
/// function so a service can hold the cell and the supervisor instead of the handle.
fn recipe_runtime_of(
    sup: &Arc<Supervisor>,
    cell: &std::sync::OnceLock<Arc<crate::recipes::runtime::RecipeRuntime>>,
) -> Arc<crate::recipes::runtime::RecipeRuntime> {
    Arc::clone(cell.get_or_init(|| {
        let seg = Arc::clone(sup);
        let builtin = std::env::var_os("HK_RECIPES_DIR")
            .map(PathBuf::from)
            .or_else(|| {
                Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../recipes"))
                    .filter(|p| p.is_dir())
            });
        let rt = crate::recipes::runtime::RecipeRuntime::new(
            Arc::clone(&sup.common.counters),
            Arc::new(move || seg.lock().shared.clone()),
            Arc::clone(&sup.common.listen),
            crate::recipes::store::RecipeStore::new(builtin, sup.common.data_dir.join("recipes")),
        );
        // T-092: always-on decoded-stream capture into `<data dir>/captures/`.
        rt.attach_default_capture_store(&sup.common.data_dir);
        rt
    }))
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
            "tracks:      {} opened, {} closed, {} confirmed, {} rows, {} links ({} dropped)",
            c("/detect/tracks_opened"),
            c("/detect/tracks_closed"),
            c("/detect/tracks_confirmed"),
            c("/detect/track_rows"),
            c("/detect/track_links"),
            c("/detect/track_links_dropped")
        ));
        line(format!(
            "anomalies:   {} opened, {} closed, {} explanations",
            c("/detect/anomalies_opened"),
            c("/detect/anomalies_closed"),
            c("/detect/explanations")
        ));
        line(format!(
            "chains:      {} attached, {} detached, {} refused (class), {} unmatched, {} errors \
             ({} from storage)",
            c("/chains/attached"),
            c("/chains/detached"),
            c("/chains/refused_class"),
            c("/chains/unmatched"),
            c("/chains/errors") + c("/chains/attach_errors"),
            // T-605: broken out because a storage error is a write that did not happen, not a
            // signal that would not demodulate, and it must be visible without reading the log.
            c("/chains/storage_errors")
        ));
        line(format!(
            "characterise: {} sweep chain(s), {} window(s) examined, {} characterised, {} left \
             alone",
            c("/chains/sweep_attached"),
            c("/chains/sweep_passes"),
            c("/chains/sweep_characterised"),
            c("/chains/sweep_uncharacterised")
        ));
        line(format!(
            "classify:    {} chain(s), {} row(s) written, {} abstained, {} without an entry, {} \
             refused at the cap",
            c("/chains/classify_attached"),
            c("/chains/classifications"),
            c("/chains/classify_abstained"),
            c("/chains/classify_no_emitter"),
            c("/chains/classify_admission_refused")
        ));
        line(format!(
            "trunking:    {} CC confirmed, {} TSBK(s) + {} CSBK(s) + {} CAC(s), {} grant(s) \
             mapped / {} unmapped / {} outside window; {} followed ({} refused, {} silent), {} \
             call(s) ({} closed on silence, {} truncated when the window ended, {} continued \
             across passes)",
            c("/chains/cc_confirmed"),
            c("/chains/cc_tsbks"),
            c("/chains/cc_csbks"),
            c("/chains/cc_cacs"),
            c("/chains/cc_grants_mapped"),
            c("/chains/cc_grants_unmapped"),
            c("/chains/cc_grants_outside_window"),
            c("/chains/cc_follows"),
            c("/chains/cc_follow_refused"),
            c("/chains/cc_follow_silent"),
            c("/chains/cc_calls"),
            c("/chains/cc_calls_closed"),
            // T-308: printed beside the closed count, because "open" and "we stopped looking" are
            // different answers and a person reading the summary is entitled to both.
            c("/chains/cc_calls_truncated"),
            c("/chains/cc_calls_continued"),
        ));
        // T-271. Printed unconditionally, and that is deliberate: a run that found nothing is
        // exactly the run where a person needs to know that some trunked systems cannot be found at
        // all by a control-channel decoder. Naming them is the difference between an answer and a
        // silence that reads as "there is nothing here".
        line(format!(
            "trunk gaps:  {}",
            hk_detect::trunk::unsupported_text()
        ));
        if c("/chains/cc_dmr_grants") > 0 {
            line(format!(
                "             {} DMR grant(s) decoded, none resolved to a frequency: DMR Tier III \
                 announces no channel parameters this build could corroborate",
                c("/chains/cc_dmr_grants")
            ));
        }
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
            "rejected:    {} outside window, {} duplicate channel, {} mode rejected ({} recorded), {} FSK boxes missed",
            c("/chains/outside_window"),
            c("/chains/duplicate_channel"),
            c("/chains/mode_rejected"),
            c("/chains/declined_measurements"),
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

    /// T-439: the view-scheme pyramid, shared with `/api/tiles?scheme=view`. `None` when the run
    /// did not open one.
    pub fn view_history(&self) -> Option<Arc<Mutex<Pyramid>>> {
        self.sup.common.view.clone()
    }

    /// The occupancy engine, series and learned channel plan (T-118).
    pub fn occupancy(&self) -> Arc<crate::occupancy::OccupancyService> {
        Arc::clone(&self.sup.common.occupancy)
    }

    /// The run's attention service (T-128): baselines, candidates, sites; `None` when it could
    /// not open.
    pub fn attention(&self) -> Option<Arc<crate::attention::AttentionService>> {
        self.sup.common.attention.clone()
    }

    /// The run's novelty alarm service (T-131; `/api/anomalies`, the `anomalies` stream); `None`
    /// when it could not open.
    pub fn alarms(&self) -> Option<Arc<crate::alarms::AlarmService>> {
        self.sup.common.alarms.clone()
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

    /// T-844: the run's C38 shadow stage (`/api/ml/*`); `None` when its store could not open.
    pub fn ml(&self) -> Option<Arc<crate::ml::MlStage>> {
        self.sup.common.ml.clone()
    }

    /// The `ConfirmPolicy.synthesized` clause this run's inventory confirms under (T-884).
    ///
    /// The MAUTO attach step (`hk_pipeline::synth::attach`) is built outside the run's inventory
    /// and must be given the configured rule rather than a fresh default — otherwise the
    /// configuration field that gates an irreversible confirm is never read.
    pub fn synthesized_confirm(&self) -> crate::inventory::SynthesizedConfirm {
        self.sup.common.synthesized_confirm.clone()
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
        self.vlf.stop_all();
    }

    /// T-891: the run's accessory-fed VLF services ([`crate::vlf`]); attach one with
    /// [`crate::vlf::VlfServices::attach`]. Empty unless an accessory was given.
    pub fn vlf(&self) -> Arc<crate::vlf::VlfServices> {
        Arc::clone(&self.vlf)
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
        // T-869 (ADR-0015 §12.9 stage 4): the chooser's recipe path, wired lazily — the runtime
        // is built only if a request actually goes down it (`HK_LISTEN_PIPELINE`).
        let (rt_sup, cell) = (Arc::clone(&self.sup), Arc::clone(&self.recipes));
        Arc::new(
            crate::chains::listen::ListenManager::new(
                Arc::clone(&self.sup.common.counters),
                Arc::new(move || sup.lock().shared.clone()),
                Arc::clone(&self.sup.common.listen),
            )
            .with_recipes(Arc::new(move || recipe_runtime_of(&rt_sup, &cell))),
        )
    }

    /// The run's detection FFT override (`PipelineSettings::fft_len`; `None` sizes it per rate).
    /// With [`crate::config::detection_bin_hz`] it states the detection **and history** bin width
    /// at any rate a re-plumb could move to (T-517).
    pub fn detection_fft_len(&self) -> Option<usize> {
        self.sup.common.detection_fft_len
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

    /// On-demand channelised IQ (T-165, ADR-0013 §4.9 gap 8): an opener attaching raw
    /// down-converted (`cf32_le`) chains at runtime for an emitter or an explicit band, no
    /// demodulation. It admits under the same run limits Listen uses ([`Self::set_listen_settings`]).
    pub fn iq_service(&self) -> Arc<crate::chains::iq::IqTapOpener> {
        let sup = Arc::clone(&self.sup);
        Arc::new(crate::chains::iq::IqTapOpener::new(
            Arc::clone(&self.sup.common.counters),
            Arc::new(move || sup.lock().shared.clone()),
            Arc::clone(&self.sup.common.listen),
        ))
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
        recipe_runtime_of(&self.sup, &self.recipes)
    }

    /// The run's decoded-stream capture store (T-092): every pipeline's inspector output is
    /// recorded there, quota-managed. `None` when the store could not be opened.
    pub fn decoded_captures(&self) -> Option<hk_store::decoded::DecodedCaptures> {
        self.recipe_runtime().capture_store().cloned()
    }

    /// **Test seam (T-508).** Holds the running segment's state the way a straggling chain thread
    /// would, so a re-plumb finds it still held past its bound. Drop the hold to release it.
    #[doc(hidden)]
    pub fn hold_segment(&self) -> Option<SegmentHold> {
        self.sup.lock().shared.clone().map(SegmentHold)
    }

    /// **Test seam (T-941).** Holds the **run's inventory locked**, from a thread that also holds
    /// the running segment's state — a straggler caught inside an `Inventory` call, the state
    /// T-941's live report was taken in. Drop the hold to release both.
    ///
    /// Deterministic on purpose: it returns only once the lock is really held, so a test never
    /// depends on winning a race with a writer, and `None` if it could not be taken within
    /// [`UNWRAP_BOUND`] (which would make the test vacuous rather than red).
    #[doc(hidden)]
    pub fn hold_inventory(&self) -> Option<InventoryHold> {
        let shared = self.sup.lock().shared.clone()?;
        let release = Arc::new(AtomicBool::new(false));
        let held = Arc::new(AtomicBool::new(false));
        let (r, h) = (Arc::clone(&release), Arc::clone(&held));
        let thread = thread::Builder::new()
            .name("test-inventory-hold".into())
            .spawn(move || {
                // Holds the segment's `Arc` *and* its inventory lock, as a straggler does.
                let _guard = shared
                    .inventory
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                h.store(true, Ordering::SeqCst);
                while !r.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(2));
                }
            })
            .ok()?;
        let deadline = Instant::now() + UNWRAP_BOUND;
        while !held.load(Ordering::SeqCst) {
            if Instant::now() >= deadline {
                release.store(true, Ordering::SeqCst);
                let _ = thread.join();
                return None;
            }
            thread::sleep(Duration::from_millis(1));
        }
        Some(InventoryHold {
            release,
            thread: Some(thread),
        })
    }

    /// **Test seam (T-508).** The next `n` segment starts fail at their last step (every reader
    /// running, the capture thread not yet), so the failed-start cleanup is what runs.
    #[doc(hidden)]
    pub fn fail_segment_starts(&self, n: u32) {
        self.sup
            .common
            .fail_segment_starts
            .store(n, Ordering::SeqCst);
    }

    /// T-541 test seam: the worker thread whose body **panics** as soon as it starts, by name
    /// (`"hk-capture"`, `"hk-spectrum"`, `"hk-detect"`, `"hk-history"`, …), for its next `starts`
    /// starts (`u32::MAX`: every one, for ever); `None` clears it.
    ///
    /// A count rather than a flag, because the two cases the guard has to get right are
    /// different: **one** panic must be recovered from and leave the run running, while a reader
    /// that panics **every** time must end the run honestly instead of restarting for ever. There
    /// is no way to make a real reader unwind on demand, and a guard over a panic no test can
    /// produce is not a guard.
    pub fn panic_worker(&self, name: Option<(&str, u32)>) {
        *self
            .sup
            .common
            .panic_worker
            .lock()
            .unwrap_or_else(PoisonError::into_inner) =
            name.map(|(n, starts)| (n.to_owned(), starts));
    }

    /// T-541: pipeline threads that ended by panicking (`ControlStats::worker_panics`).
    pub fn worker_panics(&self) -> u64 {
        get(&self.sup.common.stats.worker_panics)
    }

    /// The run's rolling IQ capture buffer (T-157): status and clip export.
    pub fn iq_buffer(&self) -> Arc<crate::iqbuffer::IqBufferService> {
        Arc::clone(&self.sup.common.iq_buffer)
    }

    /// The run's receiver-line survey (T-399): what has been measured, and how often — the cadence
    /// is once per capture state, so `counts().attempts()` never tracks the number of
    /// classifications.
    pub fn receiver_survey(&self) -> Arc<crate::survey::ReceiverSurvey> {
        Arc::clone(&self.sup.common.receiver)
    }

    /// The run's C36 L1 dwell service (T-322): what C04 granted, what acquisition found, and how
    /// much of it reached C30.
    pub fn gnss(&self) -> Arc<crate::gnss::GnssDwell> {
        Arc::clone(&self.sup.common.gnss)
    }

    /// Every front end of the run (T-510): the primary first, then the further ones in the order
    /// [`Pipeline::start_multi`] was given them. A single-device run lists one.
    pub fn devices(&self) -> Vec<RunDevice> {
        let c = &self.sup.common;
        let primary = RunDevice {
            device_id: c.switch.device_info().map(|d| d.device_id),
            primary: true,
            control: Arc::clone(&c.switch) as Arc<dyn SourceControl>,
            counters: Arc::clone(&c.counters),
            iq_buffer: Arc::clone(&c.iq_buffer),
        };
        std::iter::once(primary)
            .chain(c.aux.iter().map(devices::AuxDevice::run_device))
            .collect()
    }

    /// **The run's lossless flow gate** ([`crate::gate::FlowGate`]), while a segment is running.
    ///
    /// The gate is what makes a lossless replay lossless: every reader holds a [`crate::gate::GateCursor`] and
    /// the capture thread refuses to write a block that would lap the slowest of them. That is a
    /// *property of the run*, observable the same way [`Self::ring_position`] is, and a caller
    /// that wants to establish the backpressure path is armed has no other way to ask.
    ///
    /// **Why this exists** (T-920). `view_pause_keeps_capture` guarded against a vacuous claim —
    /// "no view control stalled the source" means nothing if a stalled reader could not have
    /// stalled it either — by asserting the gate happened to hold a block back during its
    /// measurement window (`gate_waits > 0`). But whether the writer runs a *half ring* ahead of
    /// the readers inside any given 5 s of capture is a race between a replay thread and three
    /// FFT threads, not a property of the system: on one Linux box, alone, on current `main`, that
    /// assertion failed 5 runs in 7 while the run itself stayed perfectly lossless. The guard is
    /// now a deterministic demonstration instead — pin a cursor, watch the source stop, release
    /// it, watch it resume — and it needs the gate.
    ///
    /// `None` before the first segment's shared state exists (the same window in which
    /// [`Self::ring_position`] answers 0).
    pub fn flow_gate(&self) -> Option<Arc<FlowGate>> {
        self.sup.lock().shared.as_ref().map(|s| Arc::clone(&s.gate))
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
                    // T-629: `survey <id> not found` has been seen here once, at the end of a run
                    // that inserted this very row at start-up, and has not reproduced since (170
                    // runs to load average 35). "Not found" means the row is absent from the
                    // database *this* connection opened, so the report names the file and what it
                    // held: a wrong or re-created file and a vanished row are different bugs, and
                    // one line of evidence saves the next person a triage from scratch.
                    let held = match repo.survey(common.survey_id) {
                        Ok(s) => format!("state {:?}, t_start {:?}", s.state, s.t_start),
                        Err(e2) => format!("no row ({e2})"),
                    };
                    errors.push(format!(
                        "closing the survey: {e} [survey {} in {}: {held}]",
                        common.survey_id,
                        common.db_path.display()
                    ));
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
