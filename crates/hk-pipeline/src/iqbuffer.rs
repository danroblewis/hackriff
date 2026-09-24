//! Rolling IQ capture buffer of a run (T-157, ADR-0013 §4 API gap 1): always-on raw IQ with
//! tuning/gain provenance segments, a status and clip export into the recordings model.
//! Storage (a pre-allocated on-disk ring that survives restarts, T-178 / ADR-0014), segments and
//! eviction are [`hk_store::iqbuffer`]; this module feeds it from each
//! segment's ring and turns an exported clip into a SigMF recording plus a `Recording` row.
//!
//! - **Real-time safety.** The feeder is one more ring reader on its own thread (`hk-iqbuffer`),
//!   the only thread that writes the buffer's files. The ring never waits for it: on a live source
//!   a slow disk laps the reader, and the lost samples are counted (`dropped_samples`, a gap before
//!   the next segment). Only a lossless replay that opts in (`HK_IQ_BUFFER=1`) registers a flow-gate
//!   cursor, like every other reader of an unpaced replay.
//! - **Default.** On for every run that is not a lossless (unpaced) replay, which is a recording
//!   already: a 2 min retention window on the sample clock, the disk quota `min(retention ×
//!   the device's highest rate × 2 bytes, --iq-buffer-max)`, and never below the free-space floor
//!   ([`hk_store::iqbuffer::IqBufferConfig`]; `hk serve --iq-retention/--iq-buffer-max`,
//!   `HK_IQ_RETENTION`, `HK_IQ_BUFFER_MAX`, `HK_IQ_BUFFER`).
//! - **Clip guard.** A clip is sized before anything is written: over `max_clip_bytes` (256 MiB
//!   by default) it is refused, and so is one the recordings filesystem cannot hold above the
//!   free-space floor.
//! - **Content rule.** A block whose tuned window's class forbids content is never stored
//!   (`gated_samples`), exactly as the manual recorder refuses it (only when `HK_CONTENT_GATING=1`).
//! - **Clips.** `[t0, t1)` on the sample clock, optionally only the segments whose tuned window
//!   overlaps a band (the data stays the window as captured; the band is a SigMF annotation). One
//!   SigMF capture per piece, each with its provenance, centre, time and `core:global_index`, so a
//!   retune or a gap is a capture boundary, never spliced silently. Written to
//!   `recordings/<id>.sigmf-{data,meta}` with a `Recording` row (kind `iq-snippet`, trigger
//!   `manual`, retention `pinned`). A range spanning a sample-rate change is refused (one rate per
//!   SigMF file).

use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use hk_core::{ReadOutcome, RingReader};
use hk_model::sigmf::{Annotation, Capture, Datatype, SigmfMeta};
use hk_model::{
    ContentClass, ProvenanceId, Recording, RecordingId, RecordingKind, RecordingTrigger,
    RetentionClass, TimeRange, Timestamp,
};
use hk_store::iqbuffer::{
    Allocation, IqBuffer, IqBufferConfig, IqBufferStatus, IqBufferWriter, OsHooks, SegmentStart,
    is_allocation_refused, is_ring_incompatible, is_ring_locked,
};
pub use hk_store::iqbuffer::{ClipError, ClipRange, ReadChunk};

/// How long a segment's feeder waits for the ring to open before it reads (and, while the ring
/// is still allocating, discards) captured blocks.
const ALLOCATION_WAIT: Duration = Duration::from_secs(2);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}
use num_complex::Complex;
use serde::Serialize;

use crate::chains::record::iso8601;
use crate::class::window_class;
use crate::config::PipelineConfig;
use crate::gate::GateCursor;
use crate::recorder::{RECORDING_LABEL_MAX, RECORDING_MAX_BYTES};
use crate::run::Shared;

/// The IQ ring directory of a run's **further** front end (T-510):
/// `<data_dir>/iqbuffer-devices/<device id, path-safe>-<its history source key>`. The key — the
/// same hash the history tiles record that front end under — keeps two ids that sanitise alike
/// apart, so two radios can never share a ring directory by accident.
pub fn device_dir(data_dir: &std::path::Path, device_id: &str) -> PathBuf {
    let safe: String = device_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let key = hk_store::history::source_key(device_id);
    data_dir
        .join("iqbuffer-devices")
        .join(format!("{safe}-{key:016x}"))
}

/// A clip export request (frequencies Hz).
#[derive(Clone, Debug, PartialEq)]
pub struct ClipRequest {
    /// The samples: sample-clock ns or stream indices (exact).
    pub range: ClipRange,
    /// Only segments whose tuned window overlaps `(f_lo, f_hi)`.
    pub band: Option<(f64, f64)>,
    /// User label.
    pub label: Option<String>,
    /// Only segments of this run (T-178; `None`: an index range selects in the current run, a
    /// time range in any single run).
    pub run: Option<u64>,
}

/// Why a clip was not exported.
#[derive(Debug)]
pub enum ClipFailure {
    /// The run has no buffer.
    Unavailable(String),
    /// A malformed request.
    Invalid(String),
    /// Nothing buffered in the range.
    NotFound(String),
    /// The range spans a sample-rate change.
    Conflict(String),
    /// Larger than the clip cap.
    TooLarge(String),
    /// The recordings filesystem cannot hold it above the free-space floor.
    NoSpace(String),
    /// Storage failed.
    Failed(String),
}

impl std::fmt::Display for ClipFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(m)
            | Self::Invalid(m)
            | Self::NotFound(m)
            | Self::Conflict(m)
            | Self::TooLarge(m)
            | Self::NoSpace(m)
            | Self::Failed(m) => f.write_str(m),
        }
    }
}

/// One SigMF capture of an exported clip.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ClipCaptureInfo {
    /// First sample in the clip file.
    pub sample_start: u64,
    /// Samples.
    pub samples: u64,
    /// Stream index of the first sample.
    pub global_index: u64,
    /// Time of the first sample, Unix s.
    pub t0: f64,
    /// `t0` exactly, Unix ns.
    pub t0_ns: i64,
    /// Buffer segment id.
    pub segment: u64,
    /// Run of the segment (its stream indices are that run's).
    pub run: u64,
    /// Tuned centre, Hz.
    pub center_hz: f64,
    /// Sample rate, Hz.
    pub sample_rate_hz: f64,
    /// Baseband filter, Hz.
    pub bandwidth_hz: f64,
    /// LNA gain, dB.
    pub lna_db: f64,
    /// VGA gain, dB.
    pub vga_db: f64,
    /// RF amplifier on.
    pub amp_on: bool,
    /// Device id.
    pub device_id: String,
}

/// An exported clip: the stored recording.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ClipExported {
    /// `Recording` row id.
    pub id: RecordingId,
    /// User label.
    pub label: Option<String>,
    /// `recordings/<id>.sigmf-meta` (relative to the data directory).
    pub meta_uri: String,
    /// `recordings/<id>.sigmf-data`.
    pub data_uri: String,
    /// Absolute meta path.
    pub meta_path: String,
    /// Absolute data path.
    pub data_path: String,
    /// First sample, Unix s.
    pub t0: f64,
    /// End of the last sample, Unix s.
    pub t1: f64,
    /// `t0` exactly, Unix ns.
    pub t0_ns: i64,
    /// `t1` exactly, Unix ns.
    pub t1_ns: i64,
    /// Samples.
    pub samples: u64,
    /// Data bytes (ci8).
    pub bytes: u64,
    /// Sample rate, Hz.
    pub sample_rate_hz: f64,
    /// Centre of the first capture, Hz.
    pub center_hz: f64,
    /// Requested band `[f_lo, f_hi]`, Hz.
    pub band: Option<[f64; 2]>,
    /// Content class of the recording.
    pub content_class: ContentClass,
    /// One entry per SigMF capture.
    pub captures: Vec<ClipCaptureInfo>,
}

/// The SigMF metadata of an exported ring window: one capture per contiguous piece, each with
/// its sample-clock `core:datetime`, `core:global_index` and provenance — what both a clip file
/// and playback's in-memory window ([`IqBufferService::read_window`]) carry.
fn clip_meta(
    clip: &hk_store::iqbuffer::Clip,
    hw: Option<String>,
    label: Option<&str>,
    band: Option<(f64, f64)>,
) -> SigmfMeta {
    let first = &clip.pieces[0];
    let mut meta = SigmfMeta::new(Datatype::Ci8);
    meta.global.sample_rate = Some(clip.sample_rate_hz);
    meta.global.description = Some(match label {
        Some(l) => format!("hk-pipeline IQ capture buffer clip ({l})"),
        None => "hk-pipeline IQ capture buffer clip".into(),
    });
    meta.global.recorder = Some("hk-pipeline:iqbuffer".into());
    meta.global.hw = hw;
    meta.global.provenance = Some(first.provenance.clone());
    meta.captures = clip
        .pieces
        .iter()
        .map(|p| {
            let mut extra = serde_json::Map::new();
            extra.insert("core:global_index".into(), p.global_index.into());
            extra.insert("hackriff:buffer_segment".into(), p.segment.into());
            extra.insert("hackriff:buffer_run".into(), p.run.into());
            Capture {
                sample_start: p.sample_start,
                frequency: Some(p.provenance.tune.center_hz),
                datetime: Some(iso8601(Timestamp::from_unix_nanos(p.t_ns))),
                provenance: Some(p.provenance.clone()),
                clip_count: None,
                extra,
            }
        })
        .collect();
    if let Some((lo, hi)) = band {
        meta.annotations.push(Annotation {
            sample_start: 0,
            sample_count: Some(clip.samples),
            freq_lower_edge: Some(lo),
            freq_upper_edge: Some(hi),
            label: Some("requested band".into()),
            comment: Some(
                "clip exported for this band: segments whose tuned window overlaps it, as \
                     captured (not channelised)"
                    .into(),
            ),
            truth: None,
            extra: serde_json::Map::new(),
        });
    }
    meta
}

/// Where the ring's background open stands.
enum Phase {
    /// Opening, recovering and allocating (`hk-iqbuffer-alloc`); nothing is buffered yet.
    Allocating,
    /// Open: the buffer's index (the writer is in [`Alloc::writer`]).
    Ready(IqBuffer),
    /// No buffer for this run.
    Disabled {
        reason: String,
        allocation: Option<Allocation>,
    },
}

/// State shared by the service, its feeder and the allocation thread (which never holds the
/// service itself, so dropping the service can cancel and join it).
struct Alloc {
    phase: Mutex<Phase>,
    changed: Condvar,
    writer: Mutex<Option<IqBufferWriter>>,
    /// `f64` bits of the allocated fraction.
    progress: AtomicU64,
    cancel: AtomicBool,
    /// Samples the feeder saw before the ring finished opening (T-217): never buffered, because
    /// there was no writer yet. Persists once the ring opens, so the status keeps explaining a gap
    /// at the start of a large ring's history.
    skipped: AtomicU64,
}

impl Alloc {
    fn settle(&self, phase: Phase) {
        *lock(&self.phase) = phase;
        self.changed.notify_all();
    }
}

/// The run's buffer: status, clip export and the per-segment feeder.
///
/// The ring opens **in the background** (T-178 fix round): recovery and allocating a large quota
/// (144 GB for `--iq-retention 1h`) never hold up the run's start or the API. Until it is open
/// the status reports `allocation: "allocating"` with `allocation_progress`, `enabled` is false,
/// clips answer unavailable, and captured samples are **not buffered** (the feeder keeps reading
/// the ring so nothing waits on it); buffering starts with the first block after the ring opens.
pub struct IqBufferService {
    alloc: Arc<Alloc>,
    allocator: Mutex<Option<JoinHandle<()>>>,
    active: bool,
    cfg: IqBufferConfig,
    dir: PathBuf,
    data_dir: PathBuf,
    db_path: PathBuf,
    device_hw: Option<String>,
    exports: Mutex<()>,
}

impl IqBufferService {
    /// Opens the buffer for a run under `cfg` (never fails the run: a buffer that cannot open is
    /// reported as disabled with its reason). Returns at once; the ring opens on a background
    /// thread.
    pub(crate) fn open(cfg: &PipelineConfig, db_path: PathBuf) -> Self {
        let dir = cfg.data_dir.join(hk_store::iqbuffer::DIR_NAME);
        Self::open_in(cfg, db_path, dir)
    }

    /// [`Self::open`] with the ring in `dir` (T-510: **one ring per front end**, because a ring's
    /// segments are one stream's sample indices and its directory lock admits one writer — and
    /// because the coverage map reads those segments per device, so a shared ring would let one
    /// radio's coverage answer for another's). The run's first front end keeps
    /// `<data_dir>/iqbuffer`, so a single-device run's on-disk layout is unchanged; each further
    /// one gets [`device_dir`].
    pub(crate) fn open_in(cfg: &PipelineConfig, db_path: PathBuf, dir: PathBuf) -> Self {
        let bc = cfg.iq_buffer;
        let active = bc.active(cfg.lossless);
        let alloc = Arc::new(Alloc {
            phase: Mutex::new(Phase::Allocating),
            changed: Condvar::new(),
            writer: Mutex::new(None),
            progress: AtomicU64::new(0f64.to_bits()),
            cancel: AtomicBool::new(false),
            skipped: AtomicU64::new(0),
        });
        let mut allocator = None;
        if active {
            let hooks = cfg
                .iq_buffer_hooks
                .clone()
                .unwrap_or_else(|| Arc::new(OsHooks));
            let (a, d) = (Arc::clone(&alloc), dir.clone());
            let spawned = std::thread::Builder::new()
                .name("hk-iqbuffer-alloc".into())
                .spawn(move || open_ring(&a, &d, bc, hooks));
            match spawned {
                Ok(h) => allocator = Some(h),
                Err(e) => alloc.settle(Phase::Disabled {
                    reason: format!("the buffer could not open: spawning its thread: {e}"),
                    allocation: None,
                }),
            }
        } else {
            let why = if bc.enabled == Some(false)
                || bc.max_bytes == Some(0)
                || bc.retention_s <= 0.0
            {
                "disabled by configuration (--iq-retention off, HK_IQ_RETENTION, HK_IQ_BUFFER=0 or a \
                 zero --iq-buffer-max)"
            } else {
                "off for a lossless replay (the recording is the history); HK_IQ_BUFFER=1 forces it"
            };
            alloc.settle(Phase::Disabled {
                reason: why.to_owned(),
                allocation: None,
            });
        }
        Self {
            alloc,
            allocator: Mutex::new(allocator),
            active,
            cfg: bc,
            dir,
            data_dir: cfg.data_dir.clone(),
            db_path,
            device_hw: cfg.device_hw.clone(),
            exports: Mutex::new(()),
        }
    }

    /// Whether the run is configured to buffer IQ (the ring may still be allocating, or have
    /// failed to open: [`Self::enabled`]).
    pub fn active(&self) -> bool {
        self.active
    }

    /// Whether the ring is open and buffering.
    pub fn enabled(&self) -> bool {
        self.buffer().is_some()
    }

    /// Waits up to `timeout` for the background open to finish (open or disabled); whether it
    /// finished.
    pub fn wait_allocated(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut phase = lock(&self.alloc.phase);
        while matches!(*phase, Phase::Allocating) {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            phase = self
                .alloc
                .changed
                .wait_timeout(phase, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        true
    }

    fn buffer(&self) -> Option<IqBuffer> {
        match &*lock(&self.alloc.phase) {
            Phase::Ready(b) => Some(b.clone()),
            _ => None,
        }
    }

    /// The status: segments overlapping `[t0_s, t1_s)` (all when `None`), the newest `limit`.
    pub fn status(&self, t0_s: Option<f64>, t1_s: Option<f64>, limit: usize) -> IqBufferStatus {
        let ns = |s: f64| (s * 1e9).round() as i64;
        let skipped = self.alloc.skipped.load(Ordering::Relaxed);
        let (reason, allocation) = match &*lock(&self.alloc.phase) {
            Phase::Ready(b) => {
                let b = b.clone();
                let mut s = b.status(t0_s.map(ns), t1_s.map(ns), limit);
                s.allocation_skipped_samples = skipped;
                return s;
            }
            Phase::Allocating => (
                "allocating the IQ capture ring in the background: capture is not buffered until \
                 it is allocated"
                    .to_owned(),
                Some(Allocation::Allocating),
            ),
            Phase::Disabled { reason, allocation } => (reason.clone(), *allocation),
        };
        let mut s = IqBufferStatus::disabled(&self.cfg, reason);
        s.allocation = allocation;
        s.allocation_skipped_samples = skipped;
        if allocation == Some(Allocation::Allocating) {
            s.allocation_progress =
                Some(f64::from_bits(self.alloc.progress.load(Ordering::Relaxed)));
        }
        if allocation.is_some() {
            s.dir = Some(self.dir.display().to_string());
        }
        s
    }

    /// Exports `request` as a SigMF recording and stores its `Recording` row.
    pub fn export_clip(&self, request: &ClipRequest) -> Result<ClipExported, ClipFailure> {
        let buffer = self.buffer().ok_or_else(|| {
            ClipFailure::Unavailable(format!(
                "this run has no IQ capture buffer: {}",
                self.status(None, None, 0).reason.unwrap_or_default()
            ))
        })?;
        let ClipRequest {
            range,
            band,
            label,
            run,
        } = request;
        match *range {
            ClipRange::Time { t0_ns, t1_ns } if !(0 <= t0_ns && t0_ns < t1_ns) => {
                return Err(ClipFailure::Invalid(
                    "the range needs 0 ≤ t0 < t1 on the sample clock".into(),
                ));
            }
            ClipRange::Index { start, end } if start >= end => {
                return Err(ClipFailure::Invalid(
                    "the range needs at least one sample".into(),
                ));
            }
            _ => {}
        }
        if let Some((lo, hi)) = band {
            if !(lo.is_finite() && hi.is_finite() && lo < hi) {
                return Err(ClipFailure::Invalid("band needs f_lo < f_hi (Hz)".into()));
            }
        }
        let label = match label.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(l)
                if l.chars().count() > RECORDING_LABEL_MAX || l.chars().any(char::is_control) =>
            {
                return Err(ClipFailure::Invalid(format!(
                    "label must be at most {RECORDING_LABEL_MAX} printable characters"
                )));
            }
            Some(l) => Some(l.to_owned()),
        };
        let _one = self.exports.lock().unwrap_or_else(PoisonError::into_inner);
        let failure = |e: ClipError| match e {
            ClipError::Empty | ClipError::Evicted => ClipFailure::NotFound(e.to_string()),
            ClipError::MixedRates { .. } | ClipError::MixedRuns { .. } => {
                ClipFailure::Conflict(e.to_string())
            }
            ClipError::TooLarge { .. } => ClipFailure::TooLarge(e.to_string()),
            ClipError::Io(_) => ClipFailure::Failed(format!("exporting the clip: {e}")),
        };
        // Sized before anything is written.
        let plan = buffer.plan_clip_run(*range, *band, *run).map_err(failure)?;
        let cap = self.cfg.max_clip_bytes.min(RECORDING_MAX_BYTES);
        if plan.bytes() > cap {
            return Err(ClipFailure::TooLarge(format!(
                "the clip would be {} bytes; a clip holds at most {cap} bytes \
                 (HK_IQ_BUFFER_CLIP_MAX): export a shorter range",
                plan.bytes()
            )));
        }
        let dir = self.data_dir.join("recordings");
        std::fs::create_dir_all(&dir).map_err(|e| ClipFailure::Failed(e.to_string()))?;
        if let Ok(space) = buffer.space_at(&dir) {
            let floor = self.cfg.free_floor(space.total);
            if space.free < plan.bytes().saturating_add(floor) {
                return Err(ClipFailure::NoSpace(format!(
                    "not enough free space for the clip: it needs {} bytes above the {floor}-byte \
                     free-space floor and {} bytes are free",
                    plan.bytes(),
                    space.free
                )));
            }
        }
        let id = RecordingId::new();
        let stem = id.to_string();
        let data_path = dir.join(format!("{stem}.sigmf-data"));
        let meta_path = dir.join(format!("{stem}.sigmf-meta"));
        let cleanup = || {
            let _ = std::fs::remove_file(&data_path);
            let _ = std::fs::remove_file(&meta_path);
        };
        let clip = File::create(&data_path)
            .map_err(ClipError::Io)
            .and_then(|f| plan.write(&mut BufWriter::new(f)));
        let clip = match clip {
            Ok(c) => c,
            Err(e) => {
                cleanup();
                return Err(failure(e));
            }
        };
        let r = self.store(
            id,
            label.as_deref(),
            *band,
            &clip,
            RecordingTrigger::Manual,
            (&meta_path, &data_path),
        );
        if r.is_err() {
            cleanup();
        }
        r
    }

    /// **The ring read API** (ADR-0015 §5.3, T-857): the buffered samples of `range` — segments
    /// whose tuned window overlaps `band` when given — read **into memory** as one [`ReadChunk`]
    /// per segment piece, each carrying its provenance, stream index and sample-clock times.
    ///
    /// Additive beside [`Self::export_clip`] (same plan, same eviction check), but it writes no
    /// file and stores no `Recording` row: the analysis engine reads windows, and pins the ones it
    /// searches with [`Self::pin_chunks`]. A chunk never spans a segment boundary, so the reader
    /// turns each boundary into a `DISCONTINUITY`. At most the configured clip cap
    /// (`HK_IQ_BUFFER_CLIP_MAX`) is read at once. A run without an open buffer reads
    /// [`ClipError::Empty`], as does a malformed range.
    pub fn read(
        &self,
        range: ClipRange,
        band: Option<(f64, f64)>,
    ) -> Result<Vec<ReadChunk>, ClipError> {
        let Some(buffer) = self.buffer() else {
            return Err(ClipError::Empty);
        };
        match range {
            ClipRange::Time { t0_ns, t1_ns } if !(0 <= t0_ns && t0_ns < t1_ns) => {
                return Err(ClipError::Empty);
            }
            ClipRange::Index { start, end } if start >= end => return Err(ClipError::Empty),
            _ => {}
        }
        buffer.read(range, band, self.cfg.max_clip_bytes)
    }

    /// **Pin on analyze** (ADR-0015 §6, T-857): stores `chunks` — IQ an analysis job already read
    /// with [`Self::read`] — as one SigMF recording with a `Recording` row (kind `iq-snippet`,
    /// trigger `analyze`, retention `pinned`), one SigMF capture per chunk.
    ///
    /// The file is written **from the chunks in memory**, not re-read from the ring, so the pinned
    /// clip holds exactly the samples the search reads and ring eviction after the read cannot
    /// make the two differ. Disjoint windows (a burst set) are one file whose captures carry each
    /// window's own `core:global_index` and time, so the gaps between bursts stay visible. The
    /// same guards as a clip export apply: the clip cap, the free-space floor, and one sample rate
    /// and one run per file.
    pub fn pin_chunks(
        &self,
        chunks: &[ReadChunk],
        band: Option<(f64, f64)>,
        label: Option<&str>,
    ) -> Result<ClipExported, ClipFailure> {
        let clip = hk_store::iqbuffer::Clip::from_chunks(chunks).map_err(|e| match e {
            ClipError::Empty => ClipFailure::NotFound("nothing was read to pin".into()),
            ClipError::MixedRates { .. } | ClipError::MixedRuns { .. } => {
                ClipFailure::Conflict(e.to_string())
            }
            _ => ClipFailure::Invalid(e.to_string()),
        })?;
        let bytes = clip.samples * hk_store::iqbuffer::BYTES_PER_SAMPLE;
        let cap = self.cfg.max_clip_bytes.min(RECORDING_MAX_BYTES);
        if bytes > cap {
            return Err(ClipFailure::TooLarge(format!(
                "the pinned clip would be {bytes} bytes; a clip holds at most {cap} bytes \
                 (HK_IQ_BUFFER_CLIP_MAX)"
            )));
        }
        let _one = self.exports.lock().unwrap_or_else(PoisonError::into_inner);
        let dir = self.data_dir.join("recordings");
        std::fs::create_dir_all(&dir).map_err(|e| ClipFailure::Failed(e.to_string()))?;
        let space = match self.buffer() {
            Some(b) => b.space_at(&dir),
            None => hk_store::iqbuffer::fs_space(&dir),
        };
        if let Ok(space) = space {
            let floor = self.cfg.free_floor(space.total);
            if space.free < bytes.saturating_add(floor) {
                return Err(ClipFailure::NoSpace(format!(
                    "not enough free space to pin the analysis clip: it needs {bytes} bytes above \
                     the {floor}-byte free-space floor and {} bytes are free",
                    space.free
                )));
            }
        }
        let id = RecordingId::new();
        let stem = id.to_string();
        let data_path = dir.join(format!("{stem}.sigmf-data"));
        let meta_path = dir.join(format!("{stem}.sigmf-meta"));
        let cleanup = || {
            let _ = std::fs::remove_file(&data_path);
            let _ = std::fs::remove_file(&meta_path);
        };
        let written = File::create(&data_path).and_then(|f| {
            let mut w = BufWriter::new(f);
            for c in chunks {
                std::io::Write::write_all(&mut w, &c.data)?;
            }
            std::io::Write::flush(&mut w)
        });
        if let Err(e) = written {
            cleanup();
            return Err(ClipFailure::Failed(format!("writing the pinned clip: {e}")));
        }
        let r = self.store(
            id,
            label,
            band,
            &clip,
            RecordingTrigger::Analyze,
            (&meta_path, &data_path),
        );
        if r.is_err() {
            cleanup();
        }
        r
    }

    /// Raw IQ of `[t0_ns, t1_ns)` on the sample clock — segments whose tuned window overlaps
    /// `band` when given — **in memory**, as SigMF metadata plus ci8 data, for playback (T-463)
    /// to replay through the device interface ([`hk_core::SigmfReplaySource::from_reader`]).
    ///
    /// The same ring-window extract as [`Self::export_clip`] (same plan, same eviction check,
    /// same metadata), but it writes no file and stores no `Recording` row: a playhead walking
    /// forward reads many windows, and none of them is a recording the user made. At most
    /// `max_bytes` of data. Returns the window's content class (of its first piece) beside it.
    pub fn read_window(
        &self,
        t0_ns: i64,
        t1_ns: i64,
        band: Option<(f64, f64)>,
        max_bytes: u64,
    ) -> Result<(SigmfMeta, Vec<u8>, ContentClass), ClipError> {
        let Some(buffer) = self.buffer() else {
            return Err(ClipError::Empty);
        };
        if !(0 <= t0_ns && t0_ns < t1_ns) {
            return Err(ClipError::Empty);
        }
        let plan = buffer.plan_clip_run(ClipRange::Time { t0_ns, t1_ns }, band, None)?;
        if plan.bytes() > max_bytes {
            return Err(ClipError::TooLarge {
                bytes: plan.bytes(),
            });
        }
        let mut data = Vec::with_capacity(plan.bytes() as usize);
        let clip = plan.write(&mut data)?;
        let class = clip.pieces[0].content_class;
        Ok((
            clip_meta(&clip, self.device_hw.clone(), Some("playback window"), band),
            data,
            class,
        ))
    }

    fn store(
        &self,
        id: RecordingId,
        label: Option<&str>,
        band: Option<(f64, f64)>,
        clip: &hk_store::iqbuffer::Clip,
        trigger: RecordingTrigger,
        (meta_path, data_path): (&std::path::Path, &std::path::Path),
    ) -> Result<ClipExported, ClipFailure> {
        let failed = |e: String| ClipFailure::Failed(e);
        let first = &clip.pieces[0];
        let meta = clip_meta(clip, self.device_hw.clone(), label, band);
        meta.write(meta_path)
            .map_err(|e| failed(format!("writing the clip metadata: {e}")))?;
        let duration_s = clip.samples as f64 / clip.sample_rate_hz;
        let content_class = first.content_class;
        let mut repo = hk_model::Repository::open(&self.db_path)
            .map_err(|e| failed(format!("opening the recordings database: {e}")))?;
        let provenance_ref: ProvenanceId = repo
            .intern_provenance(&first.provenance)
            .map_err(|e| failed(format!("storing the clip provenance: {e}")))?;
        let stem = id.to_string();
        let (meta_uri, data_uri) = (
            format!("recordings/{stem}.sigmf-meta"),
            format!("recordings/{stem}.sigmf-data"),
        );
        let t0 = Timestamp::from_unix_nanos(clip.t0_ns);
        repo.insert_recording(&Recording {
            id,
            meta_uri: meta_uri.clone(),
            data_uri: data_uri.clone(),
            kind: RecordingKind::IqSnippet,
            time: TimeRange::new(t0, Timestamp::from_unix_nanos(clip.t1_ns)),
            f_center_hz: first.provenance.tune.center_hz,
            sample_rate_hz: clip.sample_rate_hz,
            trigger,
            pre_trigger_s: 0.0,
            post_trigger_s: duration_s,
            size_bytes: 2 * clip.samples,
            retention_class: RetentionClass::Pinned,
            content_class,
            provenance_ref,
        })
        .map_err(|e| failed(format!("storing the Recording row: {e}")))?;
        let s = |ns: i64| ns as f64 / 1e9;
        Ok(ClipExported {
            id,
            label: label.map(str::to_owned),
            meta_uri,
            data_uri,
            meta_path: meta_path.display().to_string(),
            data_path: data_path.display().to_string(),
            t0: s(clip.t0_ns),
            t1: s(clip.t1_ns),
            t0_ns: clip.t0_ns,
            t1_ns: clip.t1_ns,
            samples: clip.samples,
            bytes: 2 * clip.samples,
            sample_rate_hz: clip.sample_rate_hz,
            center_hz: first.provenance.tune.center_hz,
            band: band.map(|(lo, hi)| [lo, hi]),
            content_class,
            captures: clip
                .pieces
                .iter()
                .map(|p| ClipCaptureInfo {
                    sample_start: p.sample_start,
                    samples: p.samples,
                    global_index: p.global_index,
                    t0: s(p.t_ns),
                    t0_ns: p.t_ns,
                    segment: p.segment,
                    run: p.run,
                    center_hz: p.provenance.tune.center_hz,
                    sample_rate_hz: p.provenance.tune.sample_rate_hz,
                    bandwidth_hz: p.provenance.tune.bandwidth_hz,
                    lna_db: p.provenance.tune.lna_db,
                    vga_db: p.provenance.tune.vga_db,
                    amp_on: p.provenance.tune.amp_on,
                    device_id: p.provenance.device_id.clone(),
                })
                .collect(),
        })
    }

    /// Feeds one run segment's ring into the buffer until the ring closes (`hk-iqbuffer`). The
    /// reader and cursor are created before the capture thread starts, so the segment's first
    /// block is buffered.
    pub(crate) fn feed(
        &self,
        shared: Arc<Shared>,
        mut reader: RingReader<Complex<i8>>,
        cursor: GateCursor,
    ) -> anyhow::Result<()> {
        // A ring that opens quickly (every quota up to a few GiB opens in milliseconds) buffers
        // from the segment's first block: wait for it, briefly, before reading. Capture never
        // waits for this reader; a larger quota is not buffered until it is allocated.
        let deadline = Instant::now() + ALLOCATION_WAIT;
        while Instant::now() < deadline
            && !shared.stop.load(Ordering::Relaxed)
            && !self.wait_allocated(Duration::from_millis(50))
        {}
        let mut buf = vec![Complex::<i8>::default(); 1 << 16];
        let mut bytes = Vec::with_capacity(2 << 16);
        let mut last_prov: Option<ProvenanceId> = None;
        let mut next_index: Option<u64> = None;
        let mut dropped_before = 0u64;
        let mut reported = false;
        loop {
            match reader.read_timeout(&mut buf, Duration::from_millis(50)) {
                ReadOutcome::Data(c) => {
                    // The ring may still be allocating: nothing is buffered until it is open.
                    let mut guard = lock(&self.alloc.writer);
                    let Some(w) = guard.as_mut() else {
                        next_index = None;
                        cursor.set(c.end_sample());
                        self.alloc
                            .skipped
                            .fetch_add(c.len as u64, Ordering::Relaxed);
                        continue;
                    };
                    let t = &c.provenance.tune;
                    let class = window_class(t.center_hz, t.sample_rate_hz);
                    if !class.permits_content() {
                        w.count_gated(c.len as u64);
                        w.end_segment();
                        next_index = None;
                        cursor.set(c.end_sample());
                        continue;
                    }
                    let pid = c.provenance.id();
                    let flagged = c.block_start && !c.discontinuity.is_empty();
                    if !w.is_open()
                        || last_prov != Some(pid)
                        || next_index != Some(c.first_sample())
                        || flagged
                    {
                        w.begin_segment(SegmentStart {
                            global_index: c.first_sample(),
                            t_ns: c.time.host_time.as_unix_nanos(),
                            provenance: c.provenance.get().clone(),
                            content_class: class,
                            dropped_before: std::mem::take(&mut dropped_before),
                        });
                        last_prov = Some(pid);
                    }
                    bytes.clear();
                    for s in &buf[..c.len] {
                        bytes.push(s.re as u8);
                        bytes.push(s.im as u8);
                    }
                    match w.append(&bytes) {
                        Ok(()) => next_index = Some(c.end_sample()),
                        Err(e) => {
                            next_index = None;
                            if !std::mem::replace(&mut reported, true) {
                                eprintln!(
                                    "IQ capture buffer: write failed (dropping, counted): {e}"
                                );
                            }
                        }
                    }
                    cursor.set(c.end_sample());
                }
                ReadOutcome::Overrun {
                    lost_samples,
                    resume_at,
                    ..
                } => {
                    if let Some(w) = lock(&self.alloc.writer).as_mut() {
                        w.count_dropped(lost_samples);
                        dropped_before += lost_samples;
                    } else {
                        self.alloc
                            .skipped
                            .fetch_add(lost_samples, Ordering::Relaxed);
                    }
                    next_index = None;
                    cursor.set(resume_at);
                }
                ReadOutcome::Empty => {}
                // Everything written has been read (the segment ended: stop or re-plumb).
                ReadOutcome::Closed => break,
            }
        }
        if let Some(w) = lock(&self.alloc.writer).as_mut() {
            w.end_segment();
            // The segment's end (stop or re-plumb) makes everything stored durable for a restart.
            if let Err(e) = w.checkpoint() {
                eprintln!("IQ capture buffer: checkpoint failed: {e}");
            }
        }
        Ok(())
    }
}

impl Drop for IqBufferService {
    /// Cancels a background open still allocating and waits for it (at most one allocation step),
    /// so the ring's lock is released before a later run opens the same directory.
    fn drop(&mut self) {
        self.alloc.cancel.store(true, Ordering::Relaxed);
        if let Some(h) = lock(&self.allocator).take() {
            let _ = h.join();
        }
    }
}

/// The background open of the ring (`hk-iqbuffer-alloc`).
fn open_ring(
    alloc: &Alloc,
    dir: &std::path::Path,
    cfg: IqBufferConfig,
    hooks: Arc<dyn hk_store::iqbuffer::IqBufferHooks>,
) {
    let started = Instant::now();
    let progress = |f: f64| alloc.progress.store(f.to_bits(), Ordering::Relaxed);
    let phase = match IqBuffer::open_with_progress(dir, cfg, hooks, &progress, &alloc.cancel) {
        Ok((b, w)) => {
            let s = b.status(None, None, 0);
            eprintln!(
                "IQ capture ring {}: {} slots of {} bytes ({:?}, preallocated {}), run {}, {} \
                 segments recovered, opened in {:.3} s",
                dir.display(),
                s.slot_count,
                s.chunk_bytes,
                s.allocation,
                s.preallocated,
                b.run(),
                s.recovered_segments,
                started.elapsed().as_secs_f64()
            );
            *lock(&alloc.writer) = Some(w);
            Phase::Ready(b)
        }
        Err(e) => {
            let allocation = if is_allocation_refused(&e) {
                Some(Allocation::Refused)
            } else if is_ring_locked(&e) {
                Some(Allocation::Locked)
            } else if is_ring_incompatible(&e) {
                Some(Allocation::Incompatible)
            } else {
                None
            };
            let reason = if allocation.is_some() {
                e.to_string()
            } else {
                format!("the buffer could not open: {e}")
            };
            eprintln!("IQ capture buffer disabled ({}): {reason}", dir.display());
            Phase::Disabled { reason, allocation }
        }
    };
    alloc.settle(phase);
}
