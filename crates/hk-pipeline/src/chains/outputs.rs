//! Output recorders (T-061, workflow steps 5-7): record a selection's, an emitter's or a band's
//! demodulated outputs and IQ to files, with start and stop. Nothing here demodulates: every
//! output is a **local consumer of an existing stream** (T-060 burst taps for bits and symbols,
//! T-043 Listen for audio) or a ring reader (IQ), so recorded files carry exactly what streaming
//! clients receive (same estimation, framing, refinement and gating).
//!
//! # Files (`<data_dir>/outputs/<session id>/`)
//! | Kind | Data file | Format | Extra |
//! |---|---|---|---|
//! | `bits` | `bits.ru8` | hard bits, one byte (0/1) per bit, bursts concatenated | `bits.bursts.jsonl`: one line per burst (`offset`, `len`, `t_ns`, `sample_index`, the burst status record: sync/payload offsets, bit order, CRC, symbol rate, centre) |
//! | `symbols` | `symbols.rf32_le` | soft symbols, one `f32` LE per symbol (positive = 1) | `symbols.bursts.jsonl` (element offsets) |
//! | `audio` | `audio.wav` | 16-bit PCM WAV, 48 kHz, mono; stream drop markers become silence | – |
//! | `iq` | `iq.sigmf-data` + `iq.sigmf-meta` | SigMF `ci8` | band annotation |
//!
//! Every data file has a `<kind>.json` sidecar ([`hk_store::outputs::OutputSidecar`]): capture
//! settings, band, target, estimated parameters, refined tuning with provenance
//! ([`hk_model::Repository::refined_tuning`]), framing, timestamps, counts, linked rows, software
//! version. Stored files become rows: bits and symbols a stored `Bitstream`, audio and IQ a
//! `Recording` (trigger manual, retention pinned). When the target is a selection, each row is
//! added to its links.
//!
//! # IQ slice: the full tuned window with a band annotation (not channelised)
//! The IQ file is the tuned window at the device rate (`ci8`, as the manual recorder writes), with
//! one SigMF annotation marking the requested band (`core:freq_lower_edge/upper_edge`). Chosen
//! over channelising to the band because it is lossless and bit-exact (no filter or decimator
//! choices baked into the file), re-opens through the same SigMF replay/mock-device path as any
//! capture (so a recording can be re-analysed blind, neighbours and interferers included), and
//! costs no DSP on the capture path. The cost is disk (2 bytes per sample), bounded by the
//! per-recording and global limits below.
//!
//! # Limits (drop, never block)
//! - **Per recording:** `max_s` (wall clock) and `max_bytes` (all files of the session together).
//! - **Global quota:** [`OutputLimits::quota_bytes`] over `outputs/` (on-disk usage plus the
//!   unspent budgets of active sessions). A start with no room left is refused (`quota`, 507); a
//!   session's byte budget is capped at the room left.
//! - Streams are consumed through the publishers' per-consumer queues, which drop with markers
//!   when this writer is slow; a record past the byte budget is dropped too and ends that
//!   output. Both are counted (`dropped_records`). The IQ reader never holds capture on a live
//!   source (ring overruns are counted as `lost_samples`).

use std::any::Any;
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use hk_core::ReadOutcome;
use hk_model::sigmf::{Annotation, Capture, Datatype, SigmfMeta};
use hk_model::{
    Bitstream, BitstreamId, BitstreamPayload, BitstreamTransport, ContentClass, EmitterId, Framing,
    Provenance, Recording, RecordingId, RecordingKind, RecordingTrigger, Repository,
    RetentionClass, SelectionId, SelectionLink, SelectionLinkKind, TimeRange, Timestamp,
};
use hk_store::outputs::{
    Band, CaptureSettings, OutputRows, OutputSidecar, OutputStats, OutputTimes, SIDECAR_SCHEMA,
    SIDECAR_VERSION, Software, WAV_MAX_DATA_BYTES, WavWriter, dir_usage,
};
use hk_stream::record::parse_status_record;
use hk_stream::{OpenRequest, OpenedStream, Record, StreamHeader, StreamOpener, StreamReader};
use num_complex::Complex;
use serde::Serialize;
use serde_json::{Value, json};

use super::listen::{ListenManager, SegmentFn};
use super::record::iso8601;
use super::taps::BurstTapOpener;
use crate::class::window_class;
use crate::run::Shared;

/// Directory under the data directory holding output sessions.
pub const OUTPUTS_DIR: &str = "outputs";
/// Finished sessions kept in memory for listing (older ones are listed from disk).
const KEEP_FINISHED: usize = 64;
/// Per-consumer queue for a recording consumer, bytes.
const CONSUMER_QUEUE_BYTES: usize = 8 << 20;

/// What to record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputKind {
    /// Hard bits with burst framing.
    Bits,
    /// Soft symbols with burst framing.
    Symbols,
    /// Demodulated audio (WAV).
    Audio,
    /// IQ of the tuned window with a band annotation (SigMF).
    Iq,
}

impl OutputKind {
    /// Every kind.
    pub const ALL: [Self; 4] = [Self::Bits, Self::Symbols, Self::Audio, Self::Iq];

    /// Parses `bits`, `symbols`, `audio`, `iq`.
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.name() == s)
    }

    /// The kind's name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Bits => "bits",
            Self::Symbols => "symbols",
            Self::Audio => "audio",
            Self::Iq => "iq",
        }
    }

    fn data_file(self) -> &'static str {
        match self {
            Self::Bits => "bits.ru8",
            Self::Symbols => "symbols.rf32_le",
            Self::Audio => "audio.wav",
            Self::Iq => "iq.sigmf-data",
        }
    }

    fn datatype(self) -> &'static str {
        match self {
            Self::Bits => "ru8",
            Self::Symbols => "rf32_le",
            Self::Audio => "wav-s16le-48k-mono",
            Self::Iq => "ci8",
        }
    }
}

/// Whose outputs to record.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum OutputTarget {
    /// A persisted selection's band; its links receive the rows.
    Selection(SelectionId),
    /// An inventory emitter.
    Emitter(EmitterId),
    /// A band, Hz.
    Band {
        /// Lower edge.
        f_lo_hz: f64,
        /// Upper edge.
        f_hi_hz: f64,
    },
}

/// A start request.
#[derive(Clone, Debug)]
pub struct OutputRequest {
    /// Target.
    pub target: OutputTarget,
    /// Kinds (at least one, no repeats).
    pub kinds: Vec<OutputKind>,
    /// Longest recording, s (default [`OutputLimits::default_s`]).
    pub max_s: Option<f64>,
    /// Largest recording (all files together), bytes (default [`OutputLimits::max_bytes`]).
    pub max_bytes: Option<u64>,
}

/// Limits.
#[derive(Clone, Debug, PartialEq)]
pub struct OutputLimits {
    /// Default `max_s`.
    pub default_s: f64,
    /// Largest accepted `max_s`.
    pub max_s: f64,
    /// Default and largest accepted `max_bytes` per recording.
    pub max_bytes: u64,
    /// Global quota over `outputs/`, bytes.
    pub quota_bytes: u64,
    /// Most sessions recording at once.
    pub max_active: usize,
}

impl Default for OutputLimits {
    fn default() -> Self {
        Self {
            default_s: 60.0,
            max_s: 3600.0,
            max_bytes: 1 << 30,
            quota_bytes: 8 << 30,
            max_active: 4,
        }
    }
}

/// Why a request failed.
#[derive(Clone, Debug, PartialEq)]
pub enum OutputError {
    /// Bad request (400).
    Invalid(String),
    /// Unknown selection, emitter or session (404).
    NotFound(String),
    /// The global quota has no room (507).
    Quota(String),
    /// Too many active sessions (503).
    Busy(String),
    /// No running segment (re-plumb in progress or run ended) (503).
    Unavailable(String),
    /// Every requested output was refused by its stream opener.
    Refused {
        /// HTTP-style status from the opener.
        status: u16,
        /// Machine token from the opener.
        code: String,
        /// Reason.
        reason: String,
    },
    /// Anything else (500).
    Failed(String),
}

impl OutputError {
    /// HTTP status.
    pub fn http_status(&self) -> u16 {
        match self {
            Self::Invalid(_) => 400,
            Self::NotFound(_) => 404,
            Self::Quota(_) => 507,
            Self::Busy(_) | Self::Unavailable(_) => 503,
            Self::Refused { status, .. } => *status,
            Self::Failed(_) => 500,
        }
    }

    /// Machine token.
    pub fn code(&self) -> &str {
        match self {
            Self::Invalid(_) => "invalid",
            Self::NotFound(_) => "not_found",
            Self::Quota(_) => "quota",
            Self::Busy(_) => "busy",
            Self::Unavailable(_) => "unavailable",
            Self::Refused { code, .. } => code,
            Self::Failed(_) => "failed",
        }
    }
}

impl std::fmt::Display for OutputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(m)
            | Self::NotFound(m)
            | Self::Quota(m)
            | Self::Busy(m)
            | Self::Unavailable(m)
            | Self::Failed(m) => f.write_str(m),
            Self::Refused { reason, .. } => f.write_str(reason),
        }
    }
}

impl std::error::Error for OutputError {}

/// One file of a session.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct OutputFileStatus {
    /// Kind.
    pub kind: String,
    /// Data file name.
    pub file: String,
    /// Sidecar file name.
    pub sidecar: String,
    /// Other files of this output (framing index, SigMF meta).
    pub extra_files: Vec<String>,
    /// `recording`, `done`, `empty` (nothing arrived), `refused`, `error`.
    pub state: String,
    /// Data bytes written.
    pub bytes: u64,
    /// Records written (bursts, audio frames, IQ chunks).
    pub records: u64,
    /// Records dropped (queue drops and byte-limit drops).
    pub dropped_records: u64,
    /// Why it was refused or ended early.
    pub message: Option<String>,
    /// Recording row.
    pub recording_id: Option<String>,
    /// Bitstream row.
    pub bitstream_id: Option<String>,
}

/// A session's state.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct OutputStatus {
    /// Session id (directory name under `outputs/`).
    pub id: String,
    /// Still recording.
    pub active: bool,
    /// Target selection.
    pub selection_id: Option<String>,
    /// Target emitter.
    pub emitter_id: Option<String>,
    /// Band lower edge, Hz.
    pub f_lo_hz: f64,
    /// Band upper edge, Hz.
    pub f_hi_hz: f64,
    /// Requested kinds.
    pub kinds: Vec<String>,
    /// Limit, s.
    pub max_s: f64,
    /// Byte budget (after the quota cap).
    pub max_bytes: u64,
    /// Bytes written by every file.
    pub bytes: u64,
    /// Started (host clock), ISO-8601.
    pub started_at: String,
    /// Seconds since start (frozen when ended).
    pub elapsed_s: f64,
    /// Files.
    pub files: Vec<OutputFileStatus>,
    /// Why the session ended (`None` while active).
    pub ended: Option<String>,
    /// Links added to the selection.
    pub links_saved: usize,
}

/// Shared by a session's writers: stop flag, deadline, byte budget, end reason.
struct Budget {
    stop: AtomicBool,
    max_bytes: u64,
    used: AtomicU64,
    reason: Mutex<Option<String>>,
}

impl Budget {
    /// Reserves `n` bytes; `false` when the budget has no room.
    fn take(&self, n: u64) -> bool {
        self.used
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |u| {
                (u + n <= self.max_bytes).then_some(u + n)
            })
            .is_ok()
    }

    fn end(&self, reason: &str) {
        let mut r = self.reason.lock().unwrap_or_else(PoisonError::into_inner);
        r.get_or_insert_with(|| reason.to_owned());
        self.stop.store(true, Ordering::SeqCst);
    }

    fn reason(&self) -> Option<String> {
        self.reason
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

type Guard = Arc<Mutex<Option<Box<dyn Any + Send>>>>;

/// Drops a stream session guard on its own thread (the producer may drain toward the caller).
fn release(guard: &Guard) {
    if let Some(g) = guard.lock().unwrap_or_else(PoisonError::into_inner).take() {
        let _ = thread::Builder::new()
            .name("hk-output-release".into())
            .spawn(move || drop(g));
    }
}

struct Session {
    id: String,
    started: Instant,
    budget: Arc<Budget>,
    status: Arc<Mutex<OutputStatus>>,
    guards: Vec<Guard>,
    watchdog: Mutex<Option<JoinHandle<()>>>,
}

impl Session {
    fn snapshot(&self) -> OutputStatus {
        let mut s = self
            .status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if s.active {
            s.elapsed_s = self.started.elapsed().as_secs_f64();
        }
        s.bytes = self.budget.used.load(Ordering::Relaxed);
        s
    }

    fn active(&self) -> bool {
        self.status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .active
    }
}

/// Everything a writer needs to finalise its file.
#[derive(Clone)]
struct Ctx {
    session: String,
    dir: PathBuf,
    rel_dir: String,
    db_path: PathBuf,
    kind: OutputKind,
    index: usize,
    band: Band,
    target: Value,
    selection: Option<SelectionId>,
    emitter: Option<EmitterId>,
    capture: CaptureSettings,
    provenance: Option<Provenance>,
    started_at: String,
    budget: Arc<Budget>,
    status: Arc<Mutex<OutputStatus>>,
}

impl Ctx {
    fn update(&self, f: impl FnOnce(&mut OutputFileStatus)) {
        let mut s = self.status.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(file) = s.files.get_mut(self.index) {
            f(file);
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    fn uri(&self, name: &str) -> String {
        format!("{}/{name}", self.rel_dir)
    }

    fn ended(&self, own: Option<&str>) -> String {
        own.map(str::to_owned)
            .or_else(|| self.budget.reason())
            .unwrap_or_else(|| "the stream ended (re-plumb or end of run)".into())
    }

    fn sidecar(&self, content_class: ContentClass, component: String) -> OutputSidecar {
        OutputSidecar {
            schema: SIDECAR_SCHEMA.into(),
            version: SIDECAR_VERSION.into(),
            session_id: self.session.clone(),
            kind: self.kind.name().into(),
            data_file: self.kind.data_file().into(),
            datatype: self.kind.datatype().into(),
            content_class,
            capture: self.capture.clone(),
            band: self.band,
            target: self.target.clone(),
            time: OutputTimes {
                started_at: self.started_at.clone(),
                finalised_at: iso8601(Timestamp::now()),
                ..OutputTimes::default()
            },
            rows: OutputRows {
                selection_id: self.selection.map(|s| s.to_string()),
                ..OutputRows::default()
            },
            software: Software {
                name: "hackriff".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                component,
            },
            ..OutputSidecar::default()
        }
    }

    /// The refined tuning of `emitter` (the target's, else the stream's), with its provenance.
    fn refined(&self, repo: &Repository, stream_emitter: Option<EmitterId>) -> Option<Value> {
        let e = self.emitter.or(stream_emitter)?;
        let r = repo.refined_tuning(e).ok().flatten()?;
        serde_json::to_value(r).ok()
    }
}

fn times(sidecar: &mut OutputSidecar, t0: Option<Timestamp>, t1: Option<Timestamp>) {
    sidecar.time.start_ns = t0.map(Timestamp::as_unix_nanos);
    sidecar.time.end_ns = t1.map(Timestamp::as_unix_nanos);
    sidecar.time.start = t0.map(iso8601);
    sidecar.time.end = t1.map(iso8601);
}

/// Records outputs of a running pipeline (`PipelineHandle::output_recorders`).
pub struct OutputRecorders {
    segment: SegmentFn,
    bits: Arc<BurstTapOpener>,
    symbols: Arc<BurstTapOpener>,
    listen: Arc<ListenManager>,
    data_dir: PathBuf,
    limits: Mutex<OutputLimits>,
    sessions: Mutex<Vec<Arc<Session>>>,
    refused_quota: AtomicU64,
}

impl OutputRecorders {
    pub(crate) fn new(
        segment: SegmentFn,
        bits: Arc<BurstTapOpener>,
        symbols: Arc<BurstTapOpener>,
        listen: Arc<ListenManager>,
        data_dir: PathBuf,
    ) -> Self {
        Self {
            segment,
            bits,
            symbols,
            listen,
            data_dir,
            limits: Mutex::new(OutputLimits::default()),
            sessions: Mutex::new(Vec::new()),
            refused_quota: AtomicU64::new(0),
        }
    }

    /// The limits in force.
    pub fn limits(&self) -> OutputLimits {
        self.limits
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Replaces the limits (later starts use them).
    pub fn set_limits(&self, limits: OutputLimits) {
        *self.limits.lock().unwrap_or_else(PoisonError::into_inner) = limits;
    }

    /// Starts refused for lack of quota.
    pub fn refused_quota(&self) -> u64 {
        self.refused_quota.load(Ordering::Relaxed)
    }

    /// `<data_dir>/outputs`.
    pub fn outputs_dir(&self) -> PathBuf {
        self.data_dir.join(OUTPUTS_DIR)
    }

    fn sessions(&self) -> std::sync::MutexGuard<'_, Vec<Arc<Session>>> {
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Starts recording. Outputs a stream opener refuses are reported per file (`refused`); when
    /// every output is refused the start fails with the first refusal.
    pub fn start(&self, req: &OutputRequest) -> Result<OutputStatus, OutputError> {
        let limits = self.limits();
        let mut kinds = req.kinds.clone();
        kinds.sort();
        kinds.dedup();
        if kinds.is_empty() || kinds.len() != req.kinds.len() {
            return Err(OutputError::Invalid(
                "kinds must name at least one of bits, symbols, audio, iq, each once".into(),
            ));
        }
        let max_s = req.max_s.unwrap_or(limits.default_s);
        if !(max_s.is_finite() && max_s > 0.0 && max_s <= limits.max_s) {
            return Err(OutputError::Invalid(format!(
                "max_s {max_s} must be in (0, {}]",
                limits.max_s
            )));
        }
        let max_bytes = req.max_bytes.unwrap_or(limits.max_bytes);
        if max_bytes == 0 || max_bytes > limits.max_bytes {
            return Err(OutputError::Invalid(format!(
                "max_bytes {max_bytes} must be in [1, {}]",
                limits.max_bytes
            )));
        }
        let shared = (self.segment)().ok_or_else(|| {
            OutputError::Unavailable("the run is changing window or has ended; try again".into())
        })?;

        let (band, selection, emitter, target) = resolve(&shared, req.target)?;
        let mut sessions = self.sessions();
        let active: Vec<&Arc<Session>> = sessions.iter().filter(|s| s.active()).collect();
        if active.len() >= limits.max_active {
            return Err(OutputError::Busy(format!(
                "{} output recordings already running",
                limits.max_active
            )));
        }
        let reserved: u64 = active
            .iter()
            .map(|s| {
                s.budget
                    .max_bytes
                    .saturating_sub(s.budget.used.load(Ordering::Relaxed))
            })
            .sum();
        let used = dir_usage(&self.outputs_dir()) + reserved;
        let room = limits.quota_bytes.saturating_sub(used);
        if room < 64 * 1024 {
            self.refused_quota.fetch_add(1, Ordering::Relaxed);
            return Err(OutputError::Quota(format!(
                "output quota full: {used} of {} bytes used in {} (finished files plus active \
                 recordings); delete old outputs or raise the quota",
                limits.quota_bytes,
                self.outputs_dir().display()
            )));
        }
        let budget_bytes = max_bytes.min(room);

        let id = RecordingId::new().to_string();
        let rel_dir = format!("{OUTPUTS_DIR}/{id}");
        let dir = self.data_dir.join(&rel_dir);
        fs::create_dir_all(&dir)
            .map_err(|e| OutputError::Failed(format!("creating {}: {e}", dir.display())))?;
        let provenance = snapshot_provenance(&shared);
        let capture = capture_settings(&shared, provenance.as_ref());
        let budget = Arc::new(Budget {
            stop: AtomicBool::new(false),
            max_bytes: budget_bytes,
            used: AtomicU64::new(0),
            reason: Mutex::new(None),
        });
        let started_at = iso8601(Timestamp::now());
        let status = Arc::new(Mutex::new(OutputStatus {
            id: id.clone(),
            active: true,
            selection_id: selection.map(|s| s.to_string()),
            emitter_id: emitter.map(|e| e.to_string()),
            f_lo_hz: band.f_lo_hz,
            f_hi_hz: band.f_hi_hz,
            kinds: kinds.iter().map(|k| k.name().to_owned()).collect(),
            max_s,
            max_bytes: budget_bytes,
            started_at: started_at.clone(),
            files: kinds
                .iter()
                .map(|k| OutputFileStatus {
                    kind: k.name().into(),
                    file: k.data_file().into(),
                    sidecar: format!("{}.json", k.name()),
                    extra_files: match k {
                        OutputKind::Bits | OutputKind::Symbols => {
                            vec![format!("{}.bursts.jsonl", k.name())]
                        }
                        OutputKind::Iq => vec!["iq.sigmf-meta".into()],
                        OutputKind::Audio => vec![],
                    },
                    state: "recording".into(),
                    ..OutputFileStatus::default()
                })
                .collect(),
            ..OutputStatus::default()
        }));
        let params: Vec<(String, String)> = match emitter {
            Some(e) if selection.is_none() => vec![("emitter".into(), e.to_string())],
            _ => vec![
                ("f_lo".into(), format!("{}", band.f_lo_hz)),
                ("f_hi".into(), format!("{}", band.f_hi_hz)),
            ],
        };
        let request = OpenRequest {
            params,
            peer: format!("output:{id}"),
        };
        let mut guards = Vec::new();
        let mut writers = Vec::new();
        let mut refusals = Vec::new();
        for (index, kind) in kinds.iter().copied().enumerate() {
            let ctx = Ctx {
                session: id.clone(),
                dir: dir.clone(),
                rel_dir: rel_dir.clone(),
                db_path: shared.db_path.clone(),
                kind,
                index,
                band,
                target: target.clone(),
                selection,
                emitter,
                capture: capture.clone(),
                provenance: provenance.clone(),
                started_at: started_at.clone(),
                budget: Arc::clone(&budget),
                status: Arc::clone(&status),
            };
            let spawned = match kind {
                OutputKind::Iq => spawn_iq(ctx.clone(), Arc::clone(&shared)).map(|j| (j, None)),
                _ => {
                    let opener: &dyn StreamOpener = match kind {
                        OutputKind::Bits => self.bits.as_ref(),
                        OutputKind::Symbols => self.symbols.as_ref(),
                        _ => self.listen.as_ref(),
                    };
                    spawn_stream(ctx.clone(), opener, &request).map(|(j, g)| (j, Some(g)))
                }
            };
            match spawned {
                Ok((join, guard)) => {
                    writers.push(join);
                    guards.extend(guard);
                }
                Err((status_code, code, reason)) => {
                    ctx.update(|f| {
                        f.state = "refused".into();
                        f.message = Some(format!("{code}: {reason}"));
                    });
                    refusals.push(OutputError::Refused {
                        status: status_code,
                        code,
                        reason,
                    });
                }
            }
        }
        drop(shared);
        if writers.is_empty() {
            let _ = fs::remove_dir_all(&dir);
            return Err(refusals
                .into_iter()
                .next()
                .unwrap_or_else(|| OutputError::Failed("nothing to record".into())));
        }
        let session = Arc::new(Session {
            id,
            started: Instant::now(),
            budget: Arc::clone(&budget),
            status: Arc::clone(&status),
            guards,
            watchdog: Mutex::new(None),
        });
        let watch = Arc::clone(&session);
        let db_path = self.data_dir.join("hackriff.db");
        let deadline = Instant::now() + Duration::from_secs_f64(max_s);
        let join = thread::Builder::new()
            .name("hk-output-watch".into())
            .spawn(move || watchdog(&watch, writers, deadline, &db_path, selection))
            .map_err(|e| OutputError::Failed(format!("starting the output watchdog: {e}")))?;
        *session
            .watchdog
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(join);
        let snap = session.snapshot();
        sessions.push(session);
        let excess = sessions.len().saturating_sub(KEEP_FINISHED);
        if excess > 0 {
            let mut dropped = 0;
            sessions.retain(|s| {
                if dropped < excess && !s.active() {
                    dropped += 1;
                    false
                } else {
                    true
                }
            });
        }
        Ok(snap)
    }

    /// Stops a session, waits for its files to be finalised, and returns its final state.
    pub fn stop(&self, id: &str) -> Result<OutputStatus, OutputError> {
        let session = self
            .sessions()
            .iter()
            .find(|s| s.id == id)
            .cloned()
            .ok_or_else(|| OutputError::NotFound(format!("no output recording {id}")))?;
        session.budget.end("stopped");
        let join = session
            .watchdog
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(j) = join {
            let _ = j.join();
        } else {
            let t0 = Instant::now();
            while session.active() && t0.elapsed() < Duration::from_secs(60) {
                thread::sleep(Duration::from_millis(20));
            }
        }
        Ok(session.snapshot())
    }

    /// Stops every active session.
    pub fn stop_all(&self) {
        let ids: Vec<String> = self
            .sessions()
            .iter()
            .filter(|s| s.active())
            .map(|s| s.id.clone())
            .collect();
        for id in ids {
            let _ = self.stop(&id);
        }
    }

    /// One session (in memory, else read from its sidecars on disk).
    pub fn status(&self, id: &str) -> Option<OutputStatus> {
        if let Some(s) = self.sessions().iter().find(|s| s.id == id) {
            return Some(s.snapshot());
        }
        safe_name(id)
            .then(|| from_disk(&self.outputs_dir().join(id), id))
            .flatten()
    }

    /// Sessions, newest first: those in memory, then older ones found on disk.
    pub fn list(&self) -> Vec<OutputStatus> {
        let mut out: Vec<OutputStatus> =
            self.sessions().iter().rev().map(|s| s.snapshot()).collect();
        let known: Vec<String> = out.iter().map(|s| s.id.clone()).collect();
        if let Ok(entries) = fs::read_dir(self.outputs_dir()) {
            let mut disk: Vec<OutputStatus> = entries
                .flatten()
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    (safe_name(&name) && !known.contains(&name))
                        .then(|| from_disk(&e.path(), &name))
                        .flatten()
                })
                .collect();
            disk.sort_by(|a, b| b.started_at.cmp(&a.started_at));
            out.extend(disk);
        }
        out
    }

    /// The path of a session's file for download: a plain name of a regular file in the
    /// session directory.
    pub fn file(&self, id: &str, name: &str) -> Result<PathBuf, OutputError> {
        let nf = || OutputError::NotFound(format!("no file {name} in output recording {id}"));
        if !safe_name(id) || !safe_name(name) {
            return Err(nf());
        }
        let p = self.outputs_dir().join(id).join(name);
        match fs::symlink_metadata(&p) {
            Ok(m) if m.is_file() => Ok(p),
            _ => Err(nf()),
        }
    }
}

impl Drop for OutputRecorders {
    fn drop(&mut self) {
        for s in self.sessions().iter() {
            s.budget.end("the recorder service was dropped");
        }
    }
}

/// A plain file or directory name: `[A-Za-z0-9._-]`, not starting with a dot.
fn safe_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && !s.starts_with('.')
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn from_disk(dir: &Path, id: &str) -> Option<OutputStatus> {
    let mut files = Vec::new();
    let mut status = OutputStatus {
        id: id.to_owned(),
        ended: Some("finished (listed from disk)".into()),
        ..OutputStatus::default()
    };
    for kind in OutputKind::ALL {
        let Ok(sc) = OutputSidecar::read(&dir.join(format!("{}.json", kind.name()))) else {
            continue;
        };
        status.f_lo_hz = sc.band.f_lo_hz;
        status.f_hi_hz = sc.band.f_hi_hz;
        status.selection_id = sc.rows.selection_id.clone().or(status.selection_id);
        status.started_at = sc.time.started_at.clone();
        status.kinds.push(kind.name().into());
        status.bytes += sc.stats.bytes;
        files.push(OutputFileStatus {
            kind: kind.name().into(),
            file: sc.data_file.clone(),
            sidecar: format!("{}.json", kind.name()),
            extra_files: match kind {
                OutputKind::Bits | OutputKind::Symbols => {
                    vec![format!("{}.bursts.jsonl", kind.name())]
                }
                OutputKind::Iq => vec!["iq.sigmf-meta".into()],
                OutputKind::Audio => vec![],
            },
            state: "done".into(),
            bytes: sc.stats.bytes,
            records: sc.stats.records,
            dropped_records: sc.stats.dropped_records,
            message: Some(sc.ended.clone()),
            recording_id: sc.rows.recording_id.clone(),
            bitstream_id: sc.rows.bitstream_id.clone(),
        });
    }
    (!files.is_empty()).then(|| {
        status.files = files;
        status
    })
}

fn resolve(
    shared: &Shared,
    target: OutputTarget,
) -> Result<(Band, Option<SelectionId>, Option<EmitterId>, Value), OutputError> {
    let repo = shared.repo();
    let (lo, hi, selection, emitter) = match target {
        OutputTarget::Selection(id) => {
            let s = repo
                .selection(id)
                .map_err(|_| OutputError::NotFound(format!("no selection {id}")))?;
            (s.f_lo_hz, s.f_hi_hz, Some(id), None)
        }
        OutputTarget::Emitter(id) => {
            let e = repo
                .emitter(id)
                .map_err(|_| OutputError::NotFound(format!("no emitter {id}")))?;
            let h = 0.5 * e.bandwidth_hz.max(0.0);
            (e.f_center_hz - h, e.f_center_hz + h, None, Some(id))
        }
        OutputTarget::Band { f_lo_hz, f_hi_hz } => (f_lo_hz, f_hi_hz, None, None),
    };
    if !(lo.is_finite() && hi.is_finite() && lo < hi && lo > 0.0) {
        return Err(OutputError::Invalid(format!(
            "the band {lo}..{hi} Hz must have 0 < f_lo < f_hi"
        )));
    }
    let target = match target {
        OutputTarget::Selection(id) => json!({ "selection_id": id.to_string() }),
        OutputTarget::Emitter(id) => json!({ "emitter_id": id.to_string() }),
        OutputTarget::Band { .. } => json!({ "band": { "f_lo_hz": lo, "f_hi_hz": hi } }),
    };
    Ok((
        Band {
            f_lo_hz: lo,
            f_hi_hz: hi,
        },
        selection,
        emitter,
        target,
    ))
}

/// The provenance of the newest samples (for capture settings and rows of stream outputs).
fn snapshot_provenance(shared: &Shared) -> Option<Provenance> {
    let start = shared.ring.next_sample()?;
    let mut reader = shared.ring.reader_at(start.saturating_sub(1));
    let mut buf = vec![Complex::<i8>::default(); 256];
    for _ in 0..20 {
        match reader.read_timeout(&mut buf, Duration::from_millis(50)) {
            ReadOutcome::Data(c) => return Some(c.provenance.get().clone()),
            ReadOutcome::Closed => return None,
            _ => {}
        }
    }
    None
}

fn capture_settings(shared: &Shared, prov: Option<&Provenance>) -> CaptureSettings {
    let (center, rate) = shared.counters.tune();
    CaptureSettings {
        device_id: prov.map_or_else(|| shared.cfg.device_id.clone(), |p| p.device_id.clone()),
        hw: shared.cfg.device_hw.clone(),
        center_hz: prov.map_or(center, |p| p.tune.center_hz),
        sample_rate_hz: prov.map_or(rate, |p| p.tune.sample_rate_hz),
        lna_db: prov.map_or(0.0, |p| p.tune.lna_db),
        vga_db: prov.map_or(0.0, |p| p.tune.vga_db),
        amp_on: prov.is_some_and(|p| p.tune.amp_on),
        baseband_filter_hz: prov.map_or(0.0, |p| p.tune.bandwidth_hz),
        antenna_port: prov.and_then(|p| p.antenna_port.clone()),
        source_class: shared.cfg.source_class,
    }
}

fn watchdog(
    session: &Session,
    writers: Vec<JoinHandle<()>>,
    deadline: Instant,
    db_path: &Path,
    selection: Option<SelectionId>,
) {
    loop {
        if session.budget.stop.load(Ordering::SeqCst) {
            break;
        }
        if Instant::now() >= deadline {
            session.budget.end("max_s reached");
            break;
        }
        if writers.iter().all(JoinHandle::is_finished) {
            session.budget.end("every output ended");
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    for g in &session.guards {
        release(g);
    }
    for w in writers {
        let _ = w.join();
    }
    let mut links = 0;
    if let Some(sel) = selection {
        let files = session
            .status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .files
            .clone();
        if let Ok(mut repo) = Repository::open(db_path) {
            for f in files {
                let (kind, target) = match (&f.recording_id, &f.bitstream_id) {
                    (Some(r), _) => (SelectionLinkKind::Recording, r.clone()),
                    (None, Some(b)) => (SelectionLinkKind::Bitstream, b.clone()),
                    _ => continue,
                };
                let link = SelectionLink {
                    kind,
                    target,
                    t: Timestamp::now(),
                    note: Some(format!("output {} ({})", f.kind, f.file)),
                };
                if repo.add_selection_link(sel, link).is_ok() {
                    links += 1;
                }
            }
        }
    }
    let mut s = session
        .status
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    s.active = false;
    s.elapsed_s = session.started.elapsed().as_secs_f64();
    s.links_saved = links;
    s.ended = session.budget.reason();
}

type Refusal = (u16, String, String);

fn spawn_stream(
    ctx: Ctx,
    opener: &dyn StreamOpener,
    request: &OpenRequest,
) -> Result<(JoinHandle<()>, Guard), Refusal> {
    let opened: OpenedStream = opener
        .open(request)
        .map_err(|r| (r.status, r.code.clone(), r.reason.clone()))?;
    let failed = |e: String| (500u16, "failed".to_owned(), e);
    let (w, r) = UnixStream::pair().map_err(|e| failed(e.to_string()))?;
    let closer = w.try_clone().map_err(|e| failed(e.to_string()))?;
    opened
        .handle
        .subscribe_with_queue(
            format!("output:{}:{}", ctx.session, ctx.kind.name()),
            w,
            Box::new(move |_| {
                let _ = closer.shutdown(Shutdown::Both);
            }),
            CONSUMER_QUEUE_BYTES,
        )
        .map_err(|e| failed(e.to_string()))?;
    let header = opened.header.clone();
    let guard: Guard = Arc::new(Mutex::new(Some(opened.session)));
    let g = Arc::clone(&guard);
    let join = thread::Builder::new()
        .name(format!("hk-output-{}", ctx.kind.name()))
        .spawn(move || {
            let result = match ctx.kind {
                OutputKind::Audio => write_audio(&ctx, &header, r, &g),
                _ => write_bursts(&ctx, &header, r, &g),
            };
            release(&g);
            if let Err(e) = result {
                ctx.update(|f| {
                    f.state = "error".into();
                    f.message = Some(format!("{e:#}"));
                });
            }
        })
        .map_err(|e| failed(e.to_string()))?;
    Ok((join, guard))
}

/// A status record carried as an unknown-type frame or a type-3 binary record.
fn status_of(rec: &Record) -> Option<Value> {
    match rec {
        Record::Unknown(frame) => parse_status_record(frame).map(|(_, v)| v),
        Record::Binary(b) if b.header.record_type == 3 => serde_json::from_slice(&b.payload).ok(),
        _ => None,
    }
}

#[derive(Default)]
struct FramingSummary {
    bursts: u64,
    framed: u64,
    inverted: u64,
    crc_valid: u64,
    crc_invalid: u64,
    bit_order: Option<String>,
    sync_bits: Option<u64>,
    sync_word_hex: Option<String>,
    rate_sum: f64,
    center_sum: f64,
    bw_sum: f64,
    snr: Vec<f64>,
    emitters: BTreeMap<String, u64>,
}

impl FramingSummary {
    fn add(&mut self, st: &Value, payload: &[u8], kind: OutputKind) {
        self.bursts += 1;
        self.framed += u64::from(st["framed"] == true);
        self.inverted += u64::from(st["inverted"] == true);
        match st["crc"].as_str() {
            Some("valid") => self.crc_valid += 1,
            Some("invalid") => self.crc_invalid += 1,
            _ => {}
        }
        if let Some(o) = st["bit_order"].as_str() {
            self.bit_order = Some(o.to_owned());
        }
        if let Some(n) = st["sync_bits"].as_u64() {
            self.sync_bits = Some(n);
        }
        self.rate_sum += st["symbol_rate_bd"].as_f64().unwrap_or(0.0);
        self.center_sum += st["f_center_hz"].as_f64().unwrap_or(0.0);
        self.bw_sum += st["bandwidth_hz"].as_f64().unwrap_or(0.0);
        if let Some(s) = st["snr_db"].as_f64() {
            self.snr.push(s);
        }
        if let Some(e) = st["emitter_id"].as_str() {
            *self.emitters.entry(e.to_owned()).or_default() += 1;
        }
        if self.sync_word_hex.is_none()
            && st["crc"] == "valid"
            && let (Some(at), Some(n)) = (st["sync_bit"].as_u64(), st["sync_bits"].as_u64())
        {
            let bits: Vec<u8> = match kind {
                OutputKind::Symbols => payload
                    .chunks_exact(4)
                    .map(|c| u8::from(f32::from_le_bytes([c[0], c[1], c[2], c[3]]) > 0.0))
                    .collect(),
                _ => payload.to_vec(),
            };
            let (at, n) = (at as usize, n as usize);
            if let Some(sync) = bits.get(at..at + n).filter(|_| n % 8 == 0 && n > 0) {
                self.sync_word_hex = Some(
                    sync.chunks_exact(8)
                        .map(|c| format!("{:02x}", c.iter().fold(0u8, |a, &b| (a << 1) | (b & 1))))
                        .collect(),
                );
            }
        }
    }

    fn mean(&self, sum: f64) -> Option<f64> {
        (self.bursts > 0).then(|| sum / self.bursts as f64)
    }

    fn to_framing(&self) -> Value {
        json!({
            "bursts": self.bursts,
            "framed": self.framed,
            "inverted": self.inverted,
            "sync_word_hex": self.sync_word_hex,
            "sync_bits": self.sync_bits,
            "bit_order": self.bit_order,
            "crc": { "valid": self.crc_valid, "invalid": self.crc_invalid },
            "bits_per_symbol": 1,
            "index": "one JSON line per burst in <kind>.bursts.jsonl: offset and len in elements, \
                      t_ns, sample_index, and the burst status record",
        })
    }

    fn to_estimated(&self) -> Value {
        let snr =
            (!self.snr.is_empty()).then(|| self.snr.iter().sum::<f64>() / self.snr.len() as f64);
        json!({
            "modulation": "fsk",
            "symbol_rate_bd": self.mean(self.rate_sum),
            "f_center_hz": self.mean(self.center_sum),
            "bandwidth_hz": self.mean(self.bw_sum),
            "snr_db": snr,
        })
    }
}

fn write_bursts(
    ctx: &Ctx,
    header: &StreamHeader,
    sock: UnixStream,
    guard: &Guard,
) -> anyhow::Result<()> {
    let kind = ctx.kind;
    let elem = if kind == OutputKind::Symbols { 4 } else { 1 };
    let mut data = BufWriter::new(File::create(ctx.path(kind.data_file()))?);
    let index_name = format!("{}.bursts.jsonl", kind.name());
    let mut index = BufWriter::new(File::create(ctx.path(&index_name))?);
    let mut reader = StreamReader::new(sock);
    reader.read_header()?;
    let mut pending: Option<Value> = None;
    let (mut offset, mut bytes, mut records, mut dropped) = (0u64, 0u64, 0u64, 0u64);
    let mut own_end: Option<&str> = None;
    let mut summary = FramingSummary::default();
    let (mut t0, mut t1) = (None, None);
    while let Ok(Some(rec)) = reader.next_record() {
        if let Some(st) = status_of(&rec) {
            pending = Some(st);
            continue;
        }
        match rec {
            Record::Binary(b) => {
                let status = pending.take().unwrap_or(Value::Null);
                let n = b.payload.len() as u64;
                if own_end.is_some() || !ctx.budget.take(n) {
                    dropped += 1;
                    if own_end.is_none() {
                        own_end = Some("max_bytes reached");
                        release(guard);
                    }
                } else {
                    data.write_all(&b.payload)?;
                    let line = json!({
                        "burst": records,
                        "offset": offset,
                        "len": n / elem,
                        "t_ns": b.header.t.as_unix_nanos(),
                        "sample_index": b.header.sample_index,
                        "status": status,
                    });
                    serde_json::to_writer(&mut index, &line)?;
                    index.write_all(b"\n")?;
                    summary.add(&status, &b.payload, kind);
                    t0.get_or_insert(b.header.t);
                    t1 = Some(b.header.t);
                    offset += n / elem;
                    bytes += n;
                    records += 1;
                }
            }
            Record::Dropped(d) => dropped += d.count,
            _ => {}
        }
        ctx.update(|f| {
            f.bytes = bytes;
            f.records = records;
            f.dropped_records = dropped;
        });
    }
    data.flush()?;
    index.flush()?;
    drop((data, index));

    let stream_emitter = header.emitter_id.or_else(|| {
        summary
            .emitters
            .iter()
            .max_by_key(|(_, n)| **n)
            .and_then(|(e, _)| e.parse().ok())
    });
    let class = header.content_class;
    let mut sc = ctx.sidecar(class, "hk-pipeline:outputs (hk-pipeline:burst-tap)".into());
    sc.estimated = Some(summary.to_estimated());
    sc.framing = Some(summary.to_framing());
    sc.stats = OutputStats {
        bytes,
        records,
        dropped_records: dropped,
        lost_samples: 0,
    };
    times(&mut sc, t0, t1);
    sc.ended = ctx.ended(own_end);
    let mut state = "done";
    let mut message = own_end.map(str::to_owned);
    let mut row = None;
    let mut repo = Repository::open(&ctx.db_path).ok();
    if let Some(repo) = repo.as_mut() {
        sc.refined_tuning = ctx.refined(repo, stream_emitter);
    }
    if records == 0 {
        state = "empty";
        message = Some("no bursts arrived".into());
    } else if !class.permits_content() {
        message = Some("the stream class does not permit stored content: no Bitstream row".into());
    } else if let Some(repo) = repo.as_mut() {
        let provenance_ref = ctx
            .provenance
            .as_ref()
            .and_then(|p| repo.intern_provenance(p).ok());
        let emitter_ref = ctx
            .emitter
            .or(stream_emitter)
            .filter(|e| repo.emitter(*e).is_ok());
        let t0 = t0.unwrap_or_else(Timestamp::now);
        let b = Bitstream {
            id: BitstreamId::new(),
            emitter_ref,
            demodulation_ref: None,
            framing: Framing {
                payload: if kind == OutputKind::Symbols {
                    BitstreamPayload::SoftSymbols
                } else {
                    BitstreamPayload::HardBits
                },
                bits_per_symbol: Some(1),
                symbol_rate_hz: summary.mean(summary.rate_sum),
                schema_id: None,
                sync_word_hex: summary.sync_word_hex.clone(),
            },
            transport: BitstreamTransport::Stored {
                uri: ctx.uri(kind.data_file()),
            },
            time: TimeRange::new(t0, t1.unwrap_or(t0)),
            provenance_ref,
            content_class: class,
        };
        match repo.insert_bitstream(&b) {
            Ok(()) => row = Some(b.id.to_string()),
            Err(e) => message = Some(format!("Bitstream row not stored: {e}")),
        }
    }
    sc.rows.bitstream_id = row.clone();
    sc.write(&ctx.path(&format!("{}.json", kind.name())))?;
    ctx.update(|f| {
        f.state = state.into();
        f.message = message;
        f.bitstream_id = row;
    });
    Ok(())
}

fn write_audio(
    ctx: &Ctx,
    header: &StreamHeader,
    sock: UnixStream,
    guard: &Guard,
) -> anyhow::Result<()> {
    let rate = header.sample_rate_hz.unwrap_or(48_000.0).round() as u32;
    let frame = header
        .audio
        .as_ref()
        .map_or(960, |a| a.frame_samples as usize);
    let mut wav = WavWriter::create(&ctx.path(OutputKind::Audio.data_file()), rate, 1)?;
    let mut reader = StreamReader::new(sock);
    reader.read_header()?;
    let (mut records, mut dropped) = (0u64, 0u64);
    let mut own_end: Option<&str> = None;
    let mut last_status: Option<Value> = None;
    let (mut t0, mut t1) = (None, None);
    let take =
        |n: u64, wav: &WavWriter| wav.data_bytes() + n <= WAV_MAX_DATA_BYTES && ctx.budget.take(n);
    while let Ok(Some(rec)) = reader.next_record() {
        if let Some(st) = status_of(&rec) {
            last_status = Some(st);
            continue;
        }
        match rec {
            Record::Binary(b) if b.header.record_type == 1 => {
                let n = b.payload.len() as u64;
                if own_end.is_some() || !take(n, &wav) {
                    dropped += 1;
                    if own_end.is_none() {
                        own_end = Some("max_bytes reached");
                        release(guard);
                    }
                } else {
                    wav.write_pcm_le(&b.payload)?;
                    t0.get_or_insert(b.header.t);
                    t1 = Some(b.header.t);
                    records += 1;
                }
            }
            Record::Dropped(d) => {
                dropped += d.count;
                // Keep the timeline: dropped frames become silence while the budget allows.
                let n = (d.count as usize * frame * 2) as u64;
                if own_end.is_none() && take(n, &wav) {
                    wav.write_silence(d.count as usize * frame)?;
                }
            }
            _ => {}
        }
        ctx.update(|f| {
            f.bytes = wav.data_bytes();
            f.records = records;
            f.dropped_records = dropped;
        });
    }
    let bytes = wav.finalize()?;
    let t1 = t1
        .map(|t: Timestamp| t.saturating_add_nanos((frame as f64 * 1e9 / f64::from(rate)) as i64));

    let class = header.content_class;
    let audio = header.audio.as_ref();
    let mut sc = ctx.sidecar(
        class,
        format!(
            "hk-pipeline:outputs ({})",
            audio.map_or("listen", |a| a.demod.as_str())
        ),
    );
    sc.estimated = Some(json!({
        "center_hz": header.center_hz,
        "bandwidth_hz": header.bandwidth_hz,
        "audio": audio,
        "last_status": last_status,
    }));
    sc.stats = OutputStats {
        bytes,
        records,
        dropped_records: dropped,
        lost_samples: 0,
    };
    times(&mut sc, t0, t1);
    sc.ended = ctx.ended(own_end);
    let (mut state, mut message, mut row) = ("done", own_end.map(str::to_owned), None);
    let mut repo = Repository::open(&ctx.db_path).ok();
    if let Some(repo) = repo.as_mut() {
        sc.refined_tuning = ctx.refined(repo, header.emitter_id);
    }
    if records == 0 {
        state = "empty";
        message = Some("no audio frames arrived".into());
    } else if let (Some(repo), Some(prov)) = (repo.as_mut(), ctx.provenance.as_ref()) {
        let t0 = t0.unwrap_or_else(Timestamp::now);
        let duration_s = bytes as f64 / (2.0 * f64::from(rate));
        let rec = repo
            .intern_provenance(prov)
            .map(|provenance_ref| Recording {
                id: RecordingId::new(),
                meta_uri: ctx.uri("audio.json"),
                data_uri: ctx.uri(OutputKind::Audio.data_file()),
                kind: RecordingKind::Audio,
                time: TimeRange::new(t0, t1.unwrap_or(t0)),
                f_center_hz: header
                    .center_hz
                    .unwrap_or(0.5 * (ctx.band.f_lo_hz + ctx.band.f_hi_hz)),
                sample_rate_hz: f64::from(rate),
                trigger: RecordingTrigger::Manual,
                pre_trigger_s: 0.0,
                post_trigger_s: duration_s,
                size_bytes: bytes + 44,
                retention_class: RetentionClass::Pinned,
                content_class: class,
                provenance_ref,
            });
        match rec.and_then(|r| repo.insert_recording(&r).map(|()| r.id)) {
            Ok(id) => row = Some(id.to_string()),
            Err(e) => message = Some(format!("Recording row not stored: {e}")),
        }
    }
    sc.rows.recording_id = row.clone();
    sc.write(&ctx.path("audio.json"))?;
    ctx.update(|f| {
        f.state = state.into();
        f.message = message;
        f.recording_id = row;
    });
    Ok(())
}

fn spawn_iq(ctx: Ctx, shared: Arc<Shared>) -> Result<JoinHandle<()>, Refusal> {
    if !shared.cfg.source_class.permits_content() {
        return Err((
            403,
            "refused".into(),
            format!(
                "IQ recording refused: content class {} forbids storing IQ of this window",
                crate::class::class_name(shared.cfg.source_class)
            ),
        ));
    }
    thread::Builder::new()
        .name("hk-output-iq".into())
        .spawn(move || {
            if let Err(e) = write_iq(&ctx, &shared) {
                inc_errors(&shared);
                ctx.update(|f| {
                    f.state = "error".into();
                    f.message = Some(format!("{e:#}"));
                });
            }
        })
        .map_err(|e| (500, "failed".into(), e.to_string()))
}

fn inc_errors(shared: &Shared) {
    crate::stats::inc(&shared.counters.chains.errors);
}

fn write_iq(ctx: &Ctx, shared: &Arc<Shared>) -> anyhow::Result<()> {
    let start = shared.ring.next_sample().unwrap_or(0);
    let cursor = shared.gate.register(start);
    let mut reader = shared.ring.reader_at(start);
    let mut out = BufWriter::new(File::create(ctx.path(OutputKind::Iq.data_file()))?);
    let mut buf = vec![Complex::<i8>::default(); 1 << 16];
    let mut bytes = Vec::new();
    let mut captures: Vec<Capture> = Vec::new();
    let mut first: Option<(Timestamp, hk_core::ProvenanceHandle)> = None;
    let mut last_prov = None;
    let mut next_index: Option<u64> = None;
    let (mut delivered, mut lost, mut chunks, mut t_end) = (0u64, 0u64, 0u64, None);
    let own_end: Option<&str> = loop {
        if ctx.budget.stop.load(Ordering::SeqCst) {
            break None;
        }
        match reader.read_timeout(&mut buf, Duration::from_millis(50)) {
            ReadOutcome::Data(c) => {
                let t = &c.provenance.tune;
                if !window_class(t.center_hz, t.sample_rate_hz).permits_content() {
                    break Some("the tuned window's content class forbids storing IQ");
                }
                let pid = c.provenance.id();
                if let Some(expected) = next_index {
                    lost += c.first_sample().saturating_sub(expected);
                }
                if last_prov != Some(pid) || next_index != Some(c.first_sample()) {
                    last_prov = Some(pid);
                    captures.push(Capture {
                        sample_start: delivered,
                        frequency: Some(t.center_hz),
                        datetime: Some(iso8601(c.time.host_time)),
                        provenance: Some(c.provenance.get().clone()),
                        clip_count: None,
                        extra: serde_json::Map::new(),
                    });
                }
                first.get_or_insert((c.time.host_time, c.provenance.clone()));
                let fs = t.sample_rate_hz;
                next_index = Some(c.end_sample());
                cursor.set(c.end_sample());
                if !ctx.budget.take(2 * c.len as u64) {
                    ctx.update(|f| f.dropped_records += 1);
                    break Some("max_bytes reached");
                }
                bytes.clear();
                for s in &buf[..c.len] {
                    bytes.push(s.re as u8);
                    bytes.push(s.im as u8);
                }
                out.write_all(&bytes)?;
                delivered += c.len as u64;
                chunks += 1;
                t_end = Some(
                    c.time
                        .host_time
                        .saturating_add_nanos((c.len as f64 * 1e9 / fs) as i64),
                );
                ctx.update(|f| {
                    f.bytes = 2 * delivered;
                    f.records = chunks;
                });
            }
            ReadOutcome::Overrun { lost_samples, .. } => {
                lost += lost_samples;
                next_index = None;
            }
            ReadOutcome::Empty => {}
            ReadOutcome::Closed => break Some("the run segment ended (stop or re-plumb)"),
        }
    };
    out.flush()?;
    drop(out);
    drop(cursor);

    let fs = first
        .as_ref()
        .map_or(ctx.capture.sample_rate_hz, |(_, p)| p.tune.sample_rate_hz);
    let meta_path = ctx.path("iq.sigmf-meta");
    let mut meta = SigmfMeta::new(Datatype::Ci8);
    meta.global.sample_rate = Some(fs);
    meta.global.description = Some(format!(
        "hk-pipeline output recording {}: tuned window with the requested band annotated",
        ctx.session
    ));
    meta.global.recorder = Some("hk-pipeline:outputs".into());
    meta.global.hw = shared.cfg.device_hw.clone();
    meta.global.provenance = first.as_ref().map(|(_, p)| p.get().clone());
    meta.captures = captures;
    meta.annotations = vec![Annotation {
        sample_start: 0,
        sample_count: Some(delivered),
        freq_lower_edge: Some(ctx.band.f_lo_hz),
        freq_upper_edge: Some(ctx.band.f_hi_hz),
        label: Some("requested band".into()),
        comment: Some(ctx.target.to_string()),
        truth: None,
        extra: serde_json::Map::new(),
    }];
    meta.write(&meta_path)
        .map_err(|e| anyhow::anyhow!("writing {}: {e}", meta_path.display()))?;

    let class = shared.cfg.source_class;
    let mut capture = ctx.capture.clone();
    if let Some((_, p)) = &first {
        capture.center_hz = p.tune.center_hz;
        capture.sample_rate_hz = p.tune.sample_rate_hz;
        capture.lna_db = p.tune.lna_db;
        capture.vga_db = p.tune.vga_db;
        capture.amp_on = p.tune.amp_on;
        capture.baseband_filter_hz = p.tune.bandwidth_hz;
    }
    let mut sc = ctx.sidecar(class, "hk-pipeline:outputs (ring reader)".into());
    sc.capture = capture;
    sc.stats = OutputStats {
        bytes: 2 * delivered,
        records: chunks,
        dropped_records: 0,
        lost_samples: lost,
    };
    times(&mut sc, first.as_ref().map(|(t, _)| *t), t_end);
    sc.ended = ctx.ended(own_end);
    let (mut state, mut message, mut row) = ("done", own_end.map(str::to_owned), None);
    let mut repo = Repository::open(&ctx.db_path).ok();
    if let Some(repo) = repo.as_mut() {
        sc.refined_tuning = ctx.refined(repo, None);
    }
    match (&first, repo.as_mut()) {
        (None, _) => {
            state = "empty";
            message = Some("no samples arrived".into());
        }
        (Some((t0, prov)), Some(repo)) => {
            let duration_s = delivered as f64 / fs;
            let rec = repo
                .intern_provenance(prov.get())
                .map(|provenance_ref| Recording {
                    id: RecordingId::new(),
                    meta_uri: ctx.uri("iq.sigmf-meta"),
                    data_uri: ctx.uri(OutputKind::Iq.data_file()),
                    kind: RecordingKind::IqSnippet,
                    time: TimeRange::new(*t0, t0.saturating_add_nanos((duration_s * 1e9) as i64)),
                    f_center_hz: prov.tune.center_hz,
                    sample_rate_hz: fs,
                    trigger: RecordingTrigger::Manual,
                    pre_trigger_s: 0.0,
                    post_trigger_s: duration_s,
                    size_bytes: 2 * delivered,
                    retention_class: RetentionClass::Pinned,
                    content_class: class,
                    provenance_ref,
                });
            match rec.and_then(|r| repo.insert_recording(&r).map(|()| r.id)) {
                Ok(id) => row = Some(id.to_string()),
                Err(e) => message = Some(format!("Recording row not stored: {e}")),
            }
        }
        (Some(_), None) => message = Some("the database could not be opened".into()),
    }
    sc.rows.recording_id = row.clone();
    sc.write(&ctx.path("iq.json"))?;
    ctx.update(|f| {
        f.state = state.into();
        f.message = message;
        f.recording_id = row;
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_kinds() {
        assert!(safe_name("0190a-b_c.json"));
        assert!(!safe_name("../x"));
        assert!(!safe_name(".hidden"));
        assert!(!safe_name("a/b"));
        assert_eq!(OutputKind::parse("iq"), Some(OutputKind::Iq));
        assert_eq!(OutputKind::parse("IQ"), None);
    }

    #[test]
    fn budget_refuses_past_its_limit() {
        let b = Budget {
            stop: AtomicBool::new(false),
            max_bytes: 10,
            used: AtomicU64::new(0),
            reason: Mutex::new(None),
        };
        assert!(b.take(6));
        assert!(!b.take(6));
        assert!(b.take(4));
        b.end("stopped");
        b.end("later");
        assert_eq!(b.reason().as_deref(), Some("stopped"));
    }

    #[test]
    fn sync_word_is_packed_from_the_located_bits() {
        let mut s = FramingSummary::default();
        let sync = [0, 0, 1, 0, 1, 1, 0, 1, 1, 1, 0, 1, 0, 1, 0, 0];
        let mut bits = vec![1, 0, 1, 0];
        bits.extend_from_slice(&sync);
        s.add(
            &json!({ "crc": "valid", "sync_bit": 4, "sync_bits": 16, "framed": true }),
            &bits,
            OutputKind::Bits,
        );
        assert_eq!(s.sync_word_hex.as_deref(), Some("2dd4"));
        assert_eq!(s.crc_valid, 1);
    }
}
