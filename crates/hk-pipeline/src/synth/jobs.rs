//! **Region-analyze jobs** (MAUTO M-8, T-859; ADR-0015 §5.1–§5.3, ADR-0021 §4, §7A.4, §11.1).
//!
//! `POST /api/analyze` starts a job; this module is the job manager behind it: admission, the one
//! running slot and its queue, acquisition, the search seam, cancellation, the retained trace, the
//! `hackriff.analyze/1` stream, and the fifty-job memory. `hk-api` reaches it through
//! `hk_api::AnalyzeControl` (implemented in `hk-cli`), which speaks JSON; nothing in `hk-api`
//! names a type from here.
//!
//! # A job's life
//!
//! 1. **Admission** ([`AnalyzeJobs::start`]): [`hk_synth::admit`] — one running job, a queue of
//!    ≤ 4 (`503 busy` beyond), the power policy (`422 power`). The **source** is resolved here,
//!    synchronously, so the errors ADR-0015 §5.1 lists on `POST` are answered on `POST`: `503
//!    unavailable` (no IQ ring), `409 outside_window` (a live band outside the tuned window),
//!    `410 evicted` (an explicit window older than the ring's oldest sample), `422 no_iq` (an
//!    explicit window the ring holds nothing of).
//! 2. **Acquiring**: a `ring` source reads its window; a `live` source waits on the **capture
//!    clock** (the ring's own live edge, never a wall clock) for `live_s` of new IQ and then reads
//!    that. Either way the read goes through M-6's [`super::acquire::acquire`] — the ring read
//!    API and a [`crate::region::ReadLedger`] — as a two-part set: the first 60 % is the search
//!    window, the rest the hold-out (ADR-0015 §3.1 step 1). A window longer than
//!    [`MAX_WINDOW_NS`] keeps its newest part and says so in `warnings`. **Coverage is what was
//!    read** (ADR-0015 §14.5): `window` is filled from the ledger, never from the request.
//! 3. **Searching**: the [`SearchBackend`] runs `hk_synth::engine::search` over the acquired IQ.
//!    **Stage evaluation over IQ is MAUTO M-2** (`Block::evidence` + `run_window`), which has not
//!    landed, so a server built without a backend ends every job here as `failed` with
//!    `error.code: "no_evaluator"` — *after* a real acquisition, so the job still says exactly
//!    what it would have searched. That is `not-searched`, never `unknown` (ADR-0021 §7A.4).
//! 4. **Finished**: `done`, `cancelled` or `failed`. The last [`MAX_FINISHED`] finished jobs are
//!    kept in memory; an older one is *forgotten*, and asking for it answers `410 gone`, which is a
//!    different fact from `404 not_found` (an id this server never issued) — *we forgot* is not
//!    *it never ran* (ADR-0021 §4.2).
//!
//! # Cancel
//!
//! `DELETE` on a queued job removes it; on a running job it sets the [`Control`]'s cancel flag,
//! which the engine polls between work units, and the job's state becomes `cancelled`
//! **immediately and finally** — a search that happened to finish in the same instant cannot turn
//! an acknowledged cancel back into `done`. The partial results the worker still returns are
//! kept (ADR-0015 §5.1). `DELETE` on a finished job forgets it.
//!
//! # The stream
//!
//! `hackriff.analyze/1` records are idempotent snapshots and **drop, never block** (ADR-0015
//! §5.2): each subscriber has a bounded queue and a full queue loses the record, not the job's
//! time. `progress` (≤ 1/s), `stage` (the deepest stage reached rose), `best` (the ranked results,
//! when the top 3 change), `trace` (per stage, at most once per stage: counts, tried/not-tried and
//! the ≤ 8 best nodes — ADR-0021 §4.3), `done` (the final job). The `GET` is always authoritative.
//!
//! # Content
//!
//! Everything a job serves is metadata — evidence, parameters, verdicts, recipes, counts — except
//! a result's `frames_preview`, which is decoded content. It is served only when the acquired
//! IQ's content class permits content (ADR-0015 §5.3 "Class"); otherwise it is emptied before the
//! job is ever visible, so neither `GET` nor the stream can carry it.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use hk_model::{ContentClass, EmitterId, FreqRange, RecordingId, TimeRange, Timestamp};
use hk_stream::{
    MessageRecord, OpenRefusal, OpenRequest, OpenedStream, Publisher, PublisherConfig,
    SessionEndSlot, StreamHeader, StreamKind, StreamOpener,
};
use hk_synth::admission::{AutoProfile, Origin, Refusal, Slots, admit};
use hk_synth::engine::{Observer, Progress, SearchOutcome, Used};
use hk_synth::search::{StopReason, SynthBudget};
use hk_synth::trace::{OutcomeKind, Resolution, TraceNode};
use hk_synth::{Control, PipelineResult, Stage, Trace};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::acquire::{
    AcquireError, Acquisition, Burst, BurstSet, MAX_BURST_IQ_NS, Membership, RingRead, acquire,
};
use super::attach::{AttachInput, Attached, attach};
use crate::inventory::SynthesizedConfirm;
use crate::iqbuffer::{ClipError, IqBufferService};

pub use hk_synth::admission::PowerPolicy;
pub use hk_synth::search::{JobState, Profile};

/// Parses a wire name (`"quick"`, `"pruned_floor"`, `"S3"`, `"searching"`) into any of the
/// serde-named enums a request or query carries; `None` when it names nothing.
pub fn parse_name<T: serde::de::DeserializeOwned>(name: &str) -> Option<T> {
    serde_json::from_value(Value::String(name.to_owned())).ok()
}

/// Finished jobs kept in memory (ADR-0015 §5.1: "the last 50 finished are kept").
pub const MAX_FINISHED: usize = 50;
/// Longest `live_s` (ADR-0015 §5.1: `live_s (≤ 30)`).
pub const MAX_LIVE_S: f64 = 30.0;
/// `live_s` when the request names none.
pub const DEFAULT_LIVE_S: f64 = 2.0;
/// Longest `max_wall_s` accepted. It can only lower the profile's wall backstop.
pub const MAX_WALL_S: f64 = 600.0;
/// Most IQ a continuous window reads: the ADR-0015 §6 2 s cap, applied to every window, because
/// the ring read is whole-segment ci8 and a 20 Msps window is 80 MB a second.
pub const MAX_WINDOW_NS: i64 = MAX_BURST_IQ_NS;
/// The search window's share of a continuous window; the rest is hold-out (ADR-0015 §3.1).
pub const SEARCH_FRACTION: f64 = 0.6;
/// `progress` records at most this often (ADR-0015 §5.2).
pub const PROGRESS_INTERVAL: Duration = Duration::from_secs(1);
/// Records a slow stream subscriber may have queued before new ones are dropped.
pub const SUBSCRIBER_QUEUE: usize = 64;
/// Nodes per stage a `trace` stream record carries (ADR-0021 §4.3).
pub const TRACE_RECORD_NODES: usize = 8;
/// Largest `limit` on the trace fetch (ADR-0021 §4.2).
pub const MAX_TRACE_LIMIT: usize = 512;
/// A live collection whose ring edge has not moved for this long ends `source_ended`.
pub const LIVE_STALL: Duration = Duration::from_secs(10);
/// The stream's message schema.
pub const MESSAGE_SCHEMA: &str = "hackriff.analyze/1";

// ---------------------------------------------------------------------------------------------
// The request
// ---------------------------------------------------------------------------------------------

/// Where the IQ comes from (ADR-0015 §5.3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceChoice {
    /// The ring if the window is retained, else live.
    #[default]
    Auto,
    /// The IQ ring's retained window.
    Ring,
    /// `live_s` of new IQ from the live edge.
    Live,
}

/// Which templates may seed the search (ADR-0015 §5.1 `templates`). Carried to the backend,
/// which seeds from the library (M-5) under it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct TemplateFilter {
    /// Only these template ids, when given.
    pub only: Option<Vec<String>>,
    /// Never these.
    pub exclude: Vec<String>,
    /// No templates at all: open skeletons only.
    pub off: bool,
}

/// A validated, resolved job request. `hk-api` resolves the target (selection, emitter or band)
/// to a band and an optional window before this exists.
#[derive(Clone, Debug, PartialEq)]
pub struct JobRequest {
    /// The target as the caller gave it, echoed on the job.
    pub target: Value,
    /// The emitter, for an emitter target.
    pub emitter_id: Option<EmitterId>,
    /// The band to analyse.
    pub band: FreqRange,
    /// The time window, on the capture clock.
    pub window: Option<TimeRange>,
    /// Whether the caller named the window. An emitter's default window is not explicit, so an
    /// `auto` source falls back to live when the ring no longer holds it (ADR-0015 §5.3); an
    /// explicit past window never silently becomes "whatever is on the air now".
    pub window_explicit: bool,
    /// Budget profile.
    pub profile: Profile,
    /// Lowers the profile's wall backstop.
    pub max_wall_s: Option<f64>,
    /// The source.
    pub source: SourceChoice,
    /// Live collection length, s.
    pub live_s: Option<f64>,
    /// Template filter.
    pub templates: TemplateFilter,
    /// Attach results to an emitter when done (M-9 performs it).
    pub attach: bool,
}

/// A refusal, as an HTTP status, a stable code and a message that echoes no request value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobFailure {
    /// HTTP status.
    pub status: u16,
    /// Machine code.
    pub code: &'static str,
    /// Human text.
    pub message: String,
}

impl JobFailure {
    fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The seams
// ---------------------------------------------------------------------------------------------

/// What the run's IQ ring holds right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RingState {
    /// No ring on this run (disabled or never allocated).
    Absent,
    /// A ring with nothing in it yet.
    Empty,
    /// The buffered span, on the capture clock.
    Buffered(TimeRange),
}

/// The run a job reads from.
pub trait JobEnv: Send + Sync {
    /// The ring's state.
    fn ring_state(&self) -> RingState;
    /// The ring reader; `None` when [`Self::ring_state`] is `Absent`.
    fn ring(&self) -> Option<&dyn RingRead>;
    /// The tuned window live collection can reach, when known.
    fn tuned(&self) -> Option<FreqRange>;
    /// Pins what a job is about to search as an `analyze` clip (ADR-0015 §6), returning its id.
    /// The default pins nothing.
    fn pin(
        &self,
        acquisition: &Acquisition,
        band: (f64, f64),
        label: &str,
    ) -> Result<Option<RecordingId>, String> {
        let _ = (acquisition, band, label);
        Ok(None)
    }
}

/// What a backend is given.
pub struct SearchInput<'a> {
    /// The request.
    pub request: &'a JobRequest,
    /// What was read.
    pub acquisition: &'a Acquisition,
    /// The admitted budget (profile, power, `max_wall_s`).
    pub budget: SynthBudget,
    /// When the job started, RFC 3339-free: Unix seconds as text (coverage `t`).
    pub started_at: String,
}

/// Runs the search over an acquisition. MAUTO M-2's `run_window` evaluator implements it over
/// `hk_synth::engine::search`; until then a server has none (module docs).
pub trait SearchBackend: Send + Sync {
    /// Searches, polling `control` between work units and reporting through `observer`.
    fn search(
        &self,
        input: &SearchInput<'_>,
        control: &Control,
        observer: &mut dyn Observer,
    ) -> Result<SearchOutcome, String>;
}

/// Attaches a finished job's results to the inventory (MAUTO M-9, [`super::attach`]). A server
/// without one keeps results in memory only and says `not-attached`.
pub trait Attacher: Send + Sync {
    /// Attaches `input`; `Ok(None)` when there was nothing to attach.
    fn attach(&self, input: &AttachInput<'_>) -> Result<Option<Attached>, String>;
}

/// The run's attacher: a connection to the run's repository per attach (jobs are rare; a held
/// connection would be a second writer for the life of the run), ordinary decode ingestion, and
/// `ConfirmPolicy.synthesized`.
pub struct RepoAttacher {
    db: std::path::PathBuf,
    policy: SynthesizedConfirm,
}

impl RepoAttacher {
    /// Over the repository at `db`, under `policy`.
    pub fn new(db: impl Into<std::path::PathBuf>, policy: SynthesizedConfirm) -> Self {
        Self {
            db: db.into(),
            policy,
        }
    }
}

impl Attacher for RepoAttacher {
    fn attach(&self, input: &AttachInput<'_>) -> Result<Option<Attached>, String> {
        let repo = hk_model::Repository::open(&self.db).map_err(|e| e.to_string())?;
        let mut ingest = hk_plugins::Ingest::new(repo);
        attach(&mut ingest, &self.policy, input).map_err(|e| e.to_string())
    }
}

// ---------------------------------------------------------------------------------------------
// The job object (ADR-0015 §5.2 + ADR-0021 §4.1, §11.1)
// ---------------------------------------------------------------------------------------------

/// Why a job failed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct JobError {
    /// `no_iq`, `evicted`, `source_ended`, `no_evaluator`, `failed`.
    pub code: &'static str,
    /// Human text.
    pub message: String,
}

/// What was read (filled from the read ledger, never from the request).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct JobWindow {
    /// `ring` or `live`.
    pub source: &'static str,
    /// First sample read, Unix s.
    pub t_lo: Option<f64>,
    /// End of the last sample read, Unix s.
    pub t_hi: Option<f64>,
    /// Ring segments read.
    pub segments: usize,
    /// Samples read.
    pub samples: u64,
    /// Discontinuities inside the read.
    pub gaps: usize,
    /// Holes the ledger skipped, ns.
    pub skipped_ns: i64,
    /// Parts read (search + hold-out).
    pub bursts: usize,
    /// Parts the ring no longer held.
    pub bursts_missing: usize,
    /// The pinned `analyze` clip, when one was stored.
    pub clip_id: Option<String>,
}

/// The channel searched.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Channel {
    /// Centre, Hz.
    pub center_hz: f64,
    /// Width, Hz.
    pub bandwidth_hz: f64,
    /// The IQ's sample rate, Hz.
    pub sample_rate_hz: Option<f64>,
}

/// One stage's row in [`TraceSummary::by_stage`].
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct StageSummary {
    /// Stage.
    pub stage: Stage,
    /// Nodes evaluated.
    pub tried: u64,
    /// Nodes not evaluated.
    pub not_tried: u64,
    /// Best cumulative bits among the tried.
    pub best_bits: Option<f32>,
}

/// ADR-0021 §4.1: what polling and the inventory read of the trace — a few hundred bytes.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TraceSummary {
    /// Nodes that left the frontier: recorded plus elided.
    pub decisions: u64,
    /// Nodes retained.
    pub nodes_recorded: u64,
    /// Nodes dropped by the bound (counted, never silently).
    pub nodes_elided: u64,
    /// Whether anything was dropped.
    pub truncated: bool,
    /// Per outcome.
    pub by_outcome: BTreeMap<OutcomeKind, u64>,
    /// Per stage.
    pub by_stage: Vec<StageSummary>,
    /// Count-bounded, so a re-run makes the same decisions (ADR-0021 §5).
    pub replayable: bool,
}

impl TraceSummary {
    /// The summary of `trace`; `replayable` when nothing depended on wall/CPU time.
    pub fn of(trace: &Trace, replayable: bool) -> Self {
        let mut by_outcome: BTreeMap<OutcomeKind, u64> = BTreeMap::new();
        let mut stages: BTreeMap<Stage, StageSummary> = BTreeMap::new();
        let row = |stages: &mut BTreeMap<Stage, StageSummary>, stage| {
            *stages.entry(stage).or_insert(StageSummary {
                stage,
                tried: 0,
                not_tried: 0,
                best_bits: None,
            })
        };
        fn best(a: Option<f32>, b: Option<f32>) -> Option<f32> {
            match (a, b) {
                (Some(x), Some(y)) => Some(x.max(y)),
                (x, None) => x,
                (None, y) => y,
            }
        }
        for n in &trace.nodes {
            *by_outcome.entry(n.outcome.kind()).or_default() += 1;
            let mut r = row(&mut stages, n.stage);
            if n.tried {
                r.tried += 1;
                r.best_bits = best(r.best_bits, n.evidence_bits);
            } else {
                r.not_tried += 1;
            }
            stages.insert(n.stage, r);
        }
        for e in &trace.elided {
            *by_outcome.entry(e.outcome).or_default() += e.count;
            let mut r = row(&mut stages, e.stage);
            if e.outcome.tried() {
                r.tried += e.count;
                if e.count > 0 && e.bits_max.is_finite() {
                    r.best_bits = best(r.best_bits, Some(e.bits_max));
                }
            } else {
                r.not_tried += e.count;
            }
            stages.insert(e.stage, r);
        }
        let recorded = trace.nodes.len() as u64;
        Self {
            decisions: recorded + trace.nodes_elided,
            nodes_recorded: recorded,
            nodes_elided: trace.nodes_elided,
            truncated: trace.truncated,
            by_outcome,
            by_stage: stages.into_values().collect(),
            replayable,
        }
    }
}

/// The served job (`AnalyzeJob`, ADR-0015 §5.2 with ADR-0021 §11.1's fields).
#[derive(Clone, Debug, Serialize)]
pub struct AnalyzeJob {
    /// `a<n>`.
    pub id: String,
    /// Where it is.
    pub href: String,
    /// State.
    pub state: JobState,
    /// Why the search stopped; `null` until it has, and when it failed before searching.
    pub end_reason: Option<StopReason>,
    /// Why it failed, when it did.
    pub error: Option<JobError>,
    /// The target as given.
    pub target: Value,
    /// Profile.
    pub profile: Profile,
    /// Requested source.
    pub source: SourceChoice,
    /// Live collection length, when the source is live.
    pub live_s: Option<f64>,
    /// Attach when done (M-9).
    pub attach: bool,
    /// Template filter.
    pub templates: TemplateFilter,
    /// What was read.
    pub window: Option<JobWindow>,
    /// The channel.
    pub channel: Option<Channel>,
    /// The admitted budget.
    pub budget: SynthBudget,
    /// What was used.
    pub used: Used,
    /// Live counters.
    pub progress: Progress,
    /// Ranked results (≤ 10).
    pub results: Vec<PipelineResult>,
    /// ADR-0021 §4.1.
    pub trace_summary: Option<TraceSummary>,
    /// ADR-0021 §7A.2: present whenever the job finished without a solved result.
    pub resolution: Option<Resolution>,
    /// The emitter the job analyses (an emitter target) or attached to (M-9).
    pub emitter_id: Option<String>,
    /// Decodes stored from the rank-1 hold-out run, `{stored, valid}` (M-9); `null` until the job
    /// has attached.
    pub decodes: Option<Value>,
    /// Confirm-by-decode outcome `{rule, outcome, evidence_bits, reason}` (M-9); `null` until the
    /// job has finished. `outcome` is `confirmed | already | insufficient | not-attached`.
    pub confirm: Option<Value>,
    /// The acquired IQ's content class; gates `frames_preview`.
    pub content_class: Option<ContentClass>,
    /// Unix s.
    pub created: f64,
    /// Unix s.
    pub started: Option<f64>,
    /// Unix s.
    pub ended: Option<f64>,
    /// Human notes on what the job did differently from what was asked.
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------------------------------------
// The manager
// ---------------------------------------------------------------------------------------------

struct Job {
    snap: AnalyzeJob,
    request: Arc<JobRequest>,
    control: Arc<Control>,
    /// The source decided at admission; taken by the worker. Never served.
    plan: Option<Plan>,
    trace: Option<Trace>,
    subscribers: Vec<SyncSender<Value>>,
    last_progress: Option<Instant>,
    /// Records dropped to full subscriber queues.
    dropped: u64,
}

#[derive(Default)]
struct Jobs {
    next: u64,
    jobs: BTreeMap<u64, Job>,
    queue: VecDeque<u64>,
    running: Option<u64>,
    finished: VecDeque<u64>,
    shutdown: bool,
}

struct Inner {
    jobs: Mutex<Jobs>,
    wake: Condvar,
    env: Arc<dyn JobEnv>,
    backend: Option<Arc<dyn SearchBackend>>,
    attacher: Option<Arc<dyn Attacher>>,
    power: PowerPolicy,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, Jobs> {
        self.jobs.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The region-analyze job manager (module docs).
pub struct AnalyzeJobs {
    inner: Arc<Inner>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

fn secs(t: Timestamp) -> f64 {
    t.as_unix_nanos() as f64 / 1e9
}

fn now_s() -> f64 {
    secs(Timestamp::now())
}

/// Parses `a<n>`; `None` for anything else.
pub fn parse_id(id: &str) -> Option<u64> {
    let n = id.strip_prefix('a')?;
    if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) || n.starts_with('0') {
        return None;
    }
    n.parse().ok()
}

fn not_found() -> JobFailure {
    JobFailure::new(404, "not_found", "no such analyze job")
}

fn gone() -> JobFailure {
    JobFailure::new(
        410,
        "gone",
        "that analyze job finished and has been forgotten (only the last 50 are kept)",
    )
}

/// The source a job will read, decided at admission.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Plan {
    Ring(TimeRange),
    Live(f64),
}

fn plan(req: &JobRequest, ring: RingState, tuned: Option<FreqRange>) -> Result<Plan, JobFailure> {
    let no_ring = || {
        JobFailure::new(
            503,
            "unavailable",
            "this server has no IQ ring to analyse from",
        )
    };
    let from_ring = |w: TimeRange| -> Result<Plan, JobFailure> {
        match ring {
            RingState::Absent => Err(no_ring()),
            RingState::Empty => Err(JobFailure::new(
                422,
                "no_iq",
                "the IQ ring holds no samples yet",
            )),
            RingState::Buffered(span) => {
                if w.end <= span.start {
                    Err(JobFailure::new(
                        410,
                        "evicted",
                        "the window is older than the IQ ring's oldest sample",
                    ))
                } else if w.start >= span.end || !w.overlaps(&span) {
                    Err(JobFailure::new(
                        422,
                        "no_iq",
                        "the IQ ring holds no samples in the window",
                    ))
                } else {
                    Ok(Plan::Ring(TimeRange::new(
                        w.start.max(span.start),
                        w.end.min(span.end),
                    )))
                }
            }
        }
    };
    let live = || -> Result<Plan, JobFailure> {
        if ring == RingState::Absent {
            // Live collection reads the ring behind the live edge, as T-070's probe does.
            return Err(no_ring());
        }
        let inside = tuned.is_some_and(|t| t.lo_hz <= req.band.lo_hz && req.band.hi_hz <= t.hi_hz);
        if !inside {
            return Err(JobFailure::new(
                409,
                "outside_window",
                "the band is not inside the tuned window, so live IQ cannot contain it",
            ));
        }
        Ok(Plan::Live(req.live_s.unwrap_or(DEFAULT_LIVE_S)))
    };
    match (req.source, req.window) {
        (SourceChoice::Ring, None) => Err(JobFailure::new(
            400,
            "invalid",
            "source \"ring\" needs a time window",
        )),
        (SourceChoice::Ring, Some(w)) => from_ring(w),
        (SourceChoice::Live, _) => live(),
        (SourceChoice::Auto, None) => live(),
        (SourceChoice::Auto, Some(w)) => match from_ring(w) {
            Ok(p) => Ok(p),
            Err(e) if req.window_explicit || e.status == 503 => Err(e),
            Err(_) => live(),
        },
    }
}

/// Splits a continuous window into its search and hold-out parts (ADR-0015 §3.1 step 1), keeping
/// the newest [`MAX_WINDOW_NS`]. Returns the set and whether it was trimmed.
fn continuous_set(window: TimeRange, membership: Membership) -> (BurstSet, bool) {
    let mut w = window;
    let trimmed = w.duration_ns() > MAX_WINDOW_NS;
    if trimmed {
        w.start = Timestamp::from_unix_nanos(w.end.as_unix_nanos() - MAX_WINDOW_NS);
    }
    let split = Timestamp::from_unix_nanos(
        w.start.as_unix_nanos() + (w.duration_ns() as f64 * SEARCH_FRACTION).round() as i64,
    );
    let part = |index, time: TimeRange, holdout| Burst {
        index,
        time,
        read: time,
        membership,
        holdout,
        trimmed,
    };
    (
        BurstSet {
            bursts: vec![
                part(0, TimeRange::new(w.start, split), false),
                part(1, TimeRange::new(split, w.end), true),
            ],
            capped: 0,
            joined: 0,
        },
        trimmed,
    )
}

fn clamp_class(a: ContentClass, b: ContentClass) -> ContentClass {
    hk_stream::gate::clamp(a, b)
}

impl AnalyzeJobs {
    /// A manager over `env`, searching with `backend` (none until MAUTO M-2), under `power`.
    pub fn new(
        env: Arc<dyn JobEnv>,
        backend: Option<Arc<dyn SearchBackend>>,
        power: PowerPolicy,
    ) -> Self {
        Self::with_attacher(env, backend, power, None)
    }

    /// [`Self::new`], attaching finished jobs through `attacher` (MAUTO M-9).
    pub fn with_attacher(
        env: Arc<dyn JobEnv>,
        backend: Option<Arc<dyn SearchBackend>>,
        power: PowerPolicy,
        attacher: Option<Arc<dyn Attacher>>,
    ) -> Self {
        let inner = Arc::new(Inner {
            jobs: Mutex::new(Jobs {
                next: 1,
                ..Jobs::default()
            }),
            wake: Condvar::new(),
            env,
            backend,
            attacher,
            power,
        });
        let worker = {
            let inner = Arc::clone(&inner);
            std::thread::Builder::new()
                .name("hk-analyze".into())
                .spawn(move || worker(&inner))
                .ok()
        };
        Self {
            inner,
            worker: Mutex::new(worker),
        }
    }

    /// Admits a job (module docs, step 1). `Ok` is the job as queued or started.
    pub fn start(&self, req: JobRequest) -> Result<AnalyzeJob, JobFailure> {
        if let Some(w) = req.max_wall_s
            && !(w > 0.0 && w <= MAX_WALL_S)
        {
            return Err(JobFailure::new(
                400,
                "invalid",
                format!("max_wall_s must be in (0, {MAX_WALL_S}] s"),
            ));
        }
        if let Some(l) = req.live_s
            && !(l > 0.0 && l <= MAX_LIVE_S)
        {
            return Err(JobFailure::new(
                400,
                "invalid",
                format!("live_s must be in (0, {MAX_LIVE_S}] s"),
            ));
        }
        // NaN edges fail this too.
        if req.band.lo_hz.partial_cmp(&req.band.hi_hz) != Some(std::cmp::Ordering::Less) {
            return Err(JobFailure::new(400, "invalid", "the band is empty"));
        }
        let plan = plan(&req, self.inner.env.ring_state(), self.inner.env.tuned())?;
        let mut g = self.inner.lock();
        if g.shutdown {
            return Err(JobFailure::new(503, "unavailable", "the run is stopping"));
        }
        let slots = Slots {
            running: g.running.map(|_| Origin::User),
            queued: g.queue.len(),
            auto_in_flight: false,
        };
        let admitted = admit(
            req.profile,
            Origin::User,
            self.inner.power,
            AutoProfile::None,
            slots,
        )
        .map_err(|r| match r {
            Refusal::Power => JobFailure::new(
                422,
                "power",
                "the power policy does not allow that profile right now",
            ),
            _ => JobFailure::new(
                503,
                "busy",
                "an analysis is running and the queue is full; try again later",
            ),
        })?;
        let mut budget = admitted.budget;
        if let Some(w) = req.max_wall_s {
            budget.wall_s = budget.wall_s.min(w);
        }
        let n = g.next;
        g.next += 1;
        let id = format!("a{n}");
        let (source_label, live_s) = match plan {
            Plan::Ring(_) => (req.source, None),
            Plan::Live(s) => (req.source, Some(s)),
        };
        let snap = AnalyzeJob {
            href: format!("/api/analyze/{id}"),
            id,
            state: JobState::Queued,
            end_reason: None,
            error: None,
            target: req.target.clone(),
            profile: req.profile,
            source: source_label,
            live_s,
            attach: req.attach,
            templates: req.templates.clone(),
            window: None,
            channel: None,
            budget,
            used: Used::default(),
            progress: Progress {
                state: Some(JobState::Queued),
                ..Progress::default()
            },
            results: Vec::new(),
            trace_summary: None,
            resolution: None,
            emitter_id: req.emitter_id.map(|e| e.to_string()),
            decodes: None,
            confirm: None,
            content_class: None,
            created: now_s(),
            started: None,
            ended: None,
            warnings: Vec::new(),
        };
        let out = snap.clone();
        g.jobs.insert(
            n,
            Job {
                snap,
                request: Arc::new(req),
                control: Arc::new(Control::with_power(self.inner.power)),
                plan: Some(plan),
                trace: None,
                subscribers: Vec::new(),
                last_progress: None,
                dropped: 0,
            },
        );
        g.queue.push_back(n);
        drop(g);
        self.inner.wake.notify_all();
        Ok(out)
    }

    /// Jobs, newest first, optionally only those in `state`.
    pub fn list(&self, state: Option<JobState>) -> Vec<AnalyzeJob> {
        let g = self.inner.lock();
        g.jobs
            .values()
            .rev()
            .filter(|j| state.is_none_or(|s| j.snap.state == s))
            .map(|j| j.snap.clone())
            .collect()
    }

    fn resolve<'a>(g: &'a Jobs, id: &str) -> Result<(u64, &'a Job), JobFailure> {
        let n = parse_id(id).ok_or_else(not_found)?;
        match g.jobs.get(&n) {
            Some(j) => Ok((n, j)),
            None if n < g.next => Err(gone()),
            None => Err(not_found()),
        }
    }

    /// One job.
    pub fn get(&self, id: &str) -> Result<AnalyzeJob, JobFailure> {
        let g = self.inner.lock();
        Self::resolve(&g, id).map(|(_, j)| j.snap.clone())
    }

    /// Cancels a queued or running job, or forgets a finished one (module docs). Returns the job
    /// and whether it was forgotten.
    pub fn cancel(&self, id: &str) -> Result<(AnalyzeJob, bool), JobFailure> {
        let mut g = self.inner.lock();
        let (n, _) = Self::resolve(&g, id)?;
        let queued = g.queue.contains(&n);
        let job = g.jobs.get_mut(&n).expect("resolved");
        if job.snap.state.is_terminal() {
            let snap = job.snap.clone();
            g.jobs.remove(&n);
            g.finished.retain(|x| *x != n);
            return Ok((snap, true));
        }
        job.control.cancel();
        job.snap.state = JobState::Cancelled;
        job.snap.progress.state = Some(JobState::Cancelled);
        job.snap.ended = Some(now_s());
        job.snap.resolution = Some(Resolution::not_searched(Some(
            job.snap.ended.map(|t| t.to_string()).unwrap_or_default(),
        )));
        let snap = job.snap.clone();
        if queued {
            // Never started: finished now, and nothing else will announce it.
            g.queue.retain(|x| *x != n);
            emit(g.jobs.get_mut(&n).expect("resolved"), "done", &snap);
            close_subscribers(g.jobs.get_mut(&n).expect("resolved"));
            finish(&mut g, n);
        }
        Ok((snap, false))
    }

    /// The retained trace (ADR-0021 §4.2), filtered.
    pub fn trace(&self, id: &str, q: &TraceQuery) -> Result<Value, JobFailure> {
        let limit = q.limit.unwrap_or(MAX_TRACE_LIMIT);
        if limit == 0 || limit > MAX_TRACE_LIMIT {
            return Err(JobFailure::new(
                400,
                "invalid",
                format!("limit must be in 1..={MAX_TRACE_LIMIT}"),
            ));
        }
        let g = self.inner.lock();
        let (_, job) = Self::resolve(&g, id)?;
        let bounds = job.snap.profile.trace_bounds();
        let keep = |n: &TraceNode| {
            q.stage.is_none_or(|s| n.stage == s)
                && q.outcome.is_none_or(|o| n.outcome.kind() == o)
                && q.tried.is_none_or(|t| n.tried == t)
                && q.family
                    .as_deref()
                    .is_none_or(|f| n.hypothesis.family.as_deref() == Some(f))
        };
        let (nodes, elided, truncated): (Vec<&TraceNode>, Vec<Value>, bool) = match &job.trace {
            Some(t) => (
                t.nodes.iter().filter(|n| keep(n)).take(limit).collect(),
                t.elided
                    .iter()
                    .filter(|e| {
                        q.stage.is_none_or(|s| e.stage == s)
                            && q.outcome.is_none_or(|o| e.outcome == o)
                            && q.tried.is_none_or(|t| e.outcome.tried() == t)
                            && q.family
                                .as_deref()
                                .is_none_or(|f| e.family.as_deref() == Some(f))
                    })
                    .map(|e| serde_json::to_value(e).unwrap_or(Value::Null))
                    .collect(),
                t.truncated,
            ),
            None => (Vec::new(), Vec::new(), false),
        };
        Ok(json!({
            "job_id": job.snap.id,
            "state": job.snap.state,
            "engine": hk_synth::ENGINE,
            // `final: false` while the job runs: the engine hands its trace over when it stops.
            "final": job.trace.is_some(),
            "replay_key": Value::Null,
            "bounds": {
                "max_nodes": bounds.max_trace_nodes,
                "max_bytes": bounds.max_trace_bytes,
                "truncated": truncated,
            },
            "nodes": nodes,
            "elided": elided,
        }))
    }

    /// Subscribes to a job's stream: the current job first, then records as they happen. `None`
    /// receiver end-of-stream follows the `done` record.
    pub fn subscribe(&self, id: &str) -> Result<Receiver<Value>, JobFailure> {
        let mut g = self.inner.lock();
        let (n, _) = Self::resolve(&g, id)?;
        let (tx, rx) = sync_channel(SUBSCRIBER_QUEUE);
        // A cancelled job is terminal at once but still running until the worker hands back.
        let terminal = g.running != Some(n) && !g.queue.contains(&n);
        let job = g.jobs.get_mut(&n).expect("resolved");
        let kind = if terminal { "done" } else { "progress" };
        let _ = tx.try_send(record(kind, &job.snap));
        if !terminal {
            job.subscribers.push(tx);
        }
        Ok(rx)
    }

    /// Records dropped to slow subscribers for a job, for tests and status.
    pub fn dropped(&self, id: &str) -> Result<u64, JobFailure> {
        let g = self.inner.lock();
        Self::resolve(&g, id).map(|(_, j)| j.dropped)
    }
}

impl Drop for AnalyzeJobs {
    fn drop(&mut self) {
        {
            let mut g = self.inner.lock();
            g.shutdown = true;
            for j in g.jobs.values() {
                j.control.cancel();
            }
        }
        self.inner.wake.notify_all();
        if let Some(h) = self
            .worker
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            let _ = h.join();
        }
    }
}

/// The trace fetch's filters (ADR-0021 §4.2).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TraceQuery {
    /// Only this stage.
    pub stage: Option<Stage>,
    /// Only this outcome.
    pub outcome: Option<OutcomeKind>,
    /// Only this `hk-mod@1` family.
    pub family: Option<String>,
    /// Only tried (`true`) or not-tried (`false`) nodes.
    pub tried: Option<bool>,
    /// At most this many nodes (≤ 512).
    pub limit: Option<usize>,
}

fn record(kind: &str, snap: &AnalyzeJob) -> Value {
    json!({ "type": kind, "job_id": snap.id, "job": snap })
}

/// Sends to every subscriber without blocking; a full queue drops the record, a gone subscriber
/// is removed.
fn emit_value(job: &mut Job, v: Value) {
    let mut dropped = 0;
    job.subscribers.retain(|tx| match tx.try_send(v.clone()) {
        Ok(()) => true,
        Err(TrySendError::Full(_)) => {
            dropped += 1;
            true
        }
        Err(TrySendError::Disconnected(_)) => false,
    });
    job.dropped += dropped;
}

fn emit(job: &mut Job, kind: &str, snap: &AnalyzeJob) {
    let v = record(kind, snap);
    emit_value(job, v);
}

fn close_subscribers(job: &mut Job) {
    job.subscribers.clear();
}

/// Moves `n` to the finished list and forgets the oldest beyond [`MAX_FINISHED`].
fn finish(g: &mut Jobs, n: u64) {
    g.finished.push_back(n);
    while g.finished.len() > MAX_FINISHED {
        if let Some(old) = g.finished.pop_front() {
            g.jobs.remove(&old);
        }
    }
}

fn worker(inner: &Arc<Inner>) {
    loop {
        let (n, plan) = {
            let mut g = inner.lock();
            loop {
                if g.shutdown {
                    return;
                }
                if let Some(n) = g.queue.pop_front() {
                    g.running = Some(n);
                    let plan = g.jobs.get_mut(&n).and_then(|j| j.plan.take());
                    break (n, plan);
                }
                g = inner.wake.wait(g).unwrap_or_else(PoisonError::into_inner);
            }
        };
        run(inner, n, plan);
        let mut g = inner.lock();
        g.running = None;
        if let Some(job) = g.jobs.get_mut(&n) {
            let snap = job.snap.clone();
            emit(job, "done", &snap);
            close_subscribers(job);
        }
        finish(&mut g, n);
    }
}

/// Applies `f` to job `n`'s snapshot unless the job was cancelled, whose state is final.
fn update(inner: &Inner, n: u64, f: impl FnOnce(&mut AnalyzeJob)) {
    let mut g = inner.lock();
    if let Some(job) = g.jobs.get_mut(&n) {
        f(&mut job.snap);
    }
}

fn set_state(inner: &Inner, n: u64, state: JobState) {
    let mut g = inner.lock();
    if let Some(job) = g.jobs.get_mut(&n)
        && job.snap.state != JobState::Cancelled
    {
        job.snap.state = state;
        job.snap.progress.state = Some(state);
        let snap = job.snap.clone();
        job.last_progress = Some(Instant::now());
        emit(job, "progress", &snap);
    }
}

fn fail(inner: &Inner, n: u64, code: &'static str, message: String, stop: Option<StopReason>) {
    update(inner, n, |s| {
        let ended = now_s();
        s.ended = Some(ended);
        s.end_reason = stop;
        // A cancel that raced the failure stays a cancel; the error is still recorded.
        s.error = Some(JobError { code, message });
        if s.state != JobState::Cancelled {
            s.state = JobState::Failed;
            s.progress.state = Some(JobState::Failed);
        }
        s.resolution = Some(Resolution::not_searched(Some(ended.to_string())));
    });
}

fn run(inner: &Arc<Inner>, n: u64, plan: Option<Plan>) {
    let Some((request, control)) = ({
        let g = inner.lock();
        g.jobs
            .get(&n)
            .map(|j| (Arc::clone(&j.request), Arc::clone(&j.control)))
    }) else {
        return;
    };
    if control.is_cancelled() {
        return;
    }
    update(inner, n, |s| s.started = Some(now_s()));
    set_state(inner, n, JobState::Acquiring);
    let Some(plan) = plan else {
        return fail(inner, n, "failed", "the job lost its plan".into(), None);
    };
    // ---- the window ----
    let (window, source) = match plan {
        Plan::Ring(w) => (w, "ring"),
        Plan::Live(live_s) => match collect_live(inner, &control, live_s) {
            Ok(w) => (w, "live"),
            Err(LiveEnd::Cancelled) => return,
            Err(LiveEnd::Stalled) => {
                return fail(
                    inner,
                    n,
                    "source_ended",
                    "the capture stopped advancing before the live window was collected".into(),
                    Some(StopReason::SourceEnded),
                );
            }
        },
    };
    let membership = if request.emitter_id.is_some() {
        Membership::Emitter
    } else {
        Membership::Band
    };
    let (set, trimmed) = continuous_set(window, membership);
    if trimmed {
        update(inner, n, |s| {
            s.warnings.push(format!(
                "the window was longer than the {} s acquisition cap; its newest {} s were read",
                MAX_WINDOW_NS / 1_000_000_000,
                MAX_WINDOW_NS / 1_000_000_000
            ))
        });
    }
    let band = (request.band.lo_hz, request.band.hi_hz);
    // Looked up after a live window is collected: a ring still allocating at admission has
    // opened by now, or the collection stalled.
    let Some(ring) = inner.env.ring() else {
        return fail(inner, n, "no_iq", "this server has no IQ ring".into(), None);
    };
    let mut acq = match acquire(ring, &set, Some(band)) {
        Ok(a) => a,
        Err(e) => {
            let (code, stop) = match &e {
                AcquireError::Evicted => ("evicted", Some(StopReason::Evicted)),
                AcquireError::NoBursts
                | AcquireError::NoIq
                | AcquireError::Ring(ClipError::Empty) => ("no_iq", None),
                _ => ("failed", None),
            };
            return fail(inner, n, code, e.to_string(), stop);
        }
    };
    let class = acq
        .chunks
        .iter()
        .map(|c| c.chunk.piece.content_class)
        .reduce(clamp_class);
    let rate = acq
        .chunks
        .first()
        .map(|c| c.chunk.piece.provenance.tune.sample_rate_hz);
    // ADR-0015 §5.5 condition 4: any piece recorded under an overloaded front end. No pieces is
    // "not known", which the confirm gate refuses.
    let overload = (!acq.chunks.is_empty())
        .then(|| acq.chunks.iter().any(|c| c.chunk.piece.provenance.overload));
    let job_window = |acq: &Acquisition, clip: Option<RecordingId>| {
        let w = acq.window();
        JobWindow {
            source,
            t_lo: w.t_lo.map(secs),
            t_hi: w.t_hi.map(secs),
            segments: w.segments,
            samples: w.samples,
            gaps: w.gaps,
            skipped_ns: w.skipped_ns,
            bursts: w.bursts,
            bursts_missing: w.bursts_missing,
            clip_id: clip.map(|c| c.to_string()),
        }
    };
    let first = job_window(&acq, None);
    update(inner, n, |s| {
        s.window = Some(first);
        s.content_class = class;
        s.channel = Some(Channel {
            center_hz: (band.0 + band.1) / 2.0,
            bandwidth_hz: band.1 - band.0,
            sample_rate_hz: rate,
        });
    });
    if control.is_cancelled() {
        return;
    }
    // ---- the search ----
    let Some(backend) = &inner.backend else {
        return fail(
            inner,
            n,
            "no_evaluator",
            "the IQ was acquired, but stage evaluation over IQ (MAUTO M-2) is not built on this \
             server, so nothing was searched"
                .into(),
            None,
        );
    };
    // Pin before the search, so eviction cannot race it (ADR-0015 §6).
    match inner.env.pin(&acq, band, &format!("analyze a{n}")) {
        Ok(Some(clip)) => {
            let w = job_window(&acq, Some(clip));
            update(inner, n, |s| s.window = Some(w));
        }
        Ok(None) => {}
        Err(e) => update(inner, n, |s| {
            s.warnings
                .push(format!("the analysed IQ could not be pinned: {e}"))
        }),
    }
    acq.pinned = None;
    set_state(inner, n, JobState::Searching);
    let budget = {
        let g = inner.lock();
        g.jobs.get(&n).map(|j| j.snap.budget)
    }
    .unwrap_or_else(|| request.profile.budget());
    let started_at = {
        let g = inner.lock();
        g.jobs
            .get(&n)
            .and_then(|j| j.snap.started)
            .map(|t| t.to_string())
            .unwrap_or_default()
    };
    let input = SearchInput {
        request: &request,
        acquisition: &acq,
        budget,
        started_at: started_at.clone(),
    };
    let mut observer = JobObserver {
        inner: Arc::clone(inner),
        n,
    };
    let outcome = backend.search(&input, &control, &mut observer);
    let read = {
        let w = acq.window();
        match (w.t_lo, w.t_hi) {
            (Some(a), Some(b)) if a < b => TimeRange::new(a, b),
            _ => window,
        }
    };
    let clip_id = {
        let g = inner.lock();
        g.jobs
            .get(&n)
            .and_then(|j| j.snap.window.as_ref())
            .and_then(|w| w.clip_id.clone())
    };
    // ADR-0021 §5: what makes this search reproducible. Blocks and the calibration hash are the
    // backend's to add once one exists (MAUTO M-2's evaluator); what the job knows is here.
    let replay_key = json!({
        "engine": hk_synth::ENGINE,
        "templates": &request.templates,
        "profile": request.profile,
        "budget": { "max_evaluations": budget.max_evaluations,
                    "max_proposal_calls": budget.max_proposal_calls },
        "window": { "clip_id": clip_id, "t_lo": secs(read.start), "t_hi": secs(read.end) },
        "seed_ref": Value::Null,
    });
    match outcome {
        Ok(o) => finish_search(
            inner,
            n,
            o,
            Finished {
                class,
                overload,
                read,
                replay_key,
            },
        ),
        Err(e) => fail(inner, n, "failed", e, None),
    }
}

/// What the worker knows about a finished search besides its outcome.
struct Finished {
    class: Option<ContentClass>,
    overload: Option<bool>,
    read: TimeRange,
    replay_key: Value,
}

enum LiveEnd {
    Cancelled,
    Stalled,
}

/// Waits on the capture clock for `live_s` of new IQ: the window starts at the ring's live edge
/// when the job starts, and ends `live_s` of **sample time** later.
fn collect_live(inner: &Inner, control: &Control, live_s: f64) -> Result<TimeRange, LiveEnd> {
    let edge = |r: RingState| match r {
        RingState::Buffered(s) => Some(s.end),
        _ => None,
    };
    let mut start = edge(inner.env.ring_state());
    let mut last = start;
    let mut moved = Instant::now();
    loop {
        if control.is_cancelled() {
            return Err(LiveEnd::Cancelled);
        }
        let now = edge(inner.env.ring_state());
        if start.is_none() {
            start = now;
        }
        if now != last {
            last = now;
            moved = Instant::now();
        }
        if let (Some(s), Some(e)) = (start, now) {
            let want = s.as_unix_nanos() + (live_s * 1e9).round() as i64;
            if e.as_unix_nanos() >= want {
                return Ok(TimeRange::new(s, Timestamp::from_unix_nanos(want)));
            }
        }
        if moved.elapsed() > LIVE_STALL {
            return Err(LiveEnd::Stalled);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

struct JobObserver {
    inner: Arc<Inner>,
    n: u64,
}

impl Observer for JobObserver {
    fn progress(&mut self, p: &Progress) {
        let mut g = self.inner.lock();
        let Some(job) = g.jobs.get_mut(&self.n) else {
            return;
        };
        let rose = p.stage_max > job.snap.progress.stage_max;
        let cancelled = job.snap.state == JobState::Cancelled;
        job.snap.progress = *p;
        if cancelled {
            job.snap.progress.state = Some(JobState::Cancelled);
        } else if let Some(s) = p.state {
            job.snap.state = s;
        }
        let snap = job.snap.clone();
        if rose {
            emit_value(
                job,
                json!({ "type": "stage", "job_id": snap.id, "stage": p.stage_max, "job": snap }),
            );
        }
        let due = job
            .last_progress
            .is_none_or(|t| t.elapsed() >= PROGRESS_INTERVAL);
        if due {
            job.last_progress = Some(Instant::now());
            emit(job, "progress", &snap);
        }
    }
}

/// The resolution of a finished search (ADR-0021 §7A.2), **sealed by `hk-synth`**
/// ([`SearchOutcome::seal`], ADR-0021 §9.3). `None` when a result solved. Only a `done` job may
/// say `unknown`; a cancelled or failed one ruled nothing out.
pub fn resolution_of(o: &SearchOutcome, summary: &TraceSummary, ended: &str) -> Option<Resolution> {
    o.seal(serde_json::to_value(summary).ok(), None, ended)
}

fn finish_search(inner: &Inner, n: u64, mut o: SearchOutcome, f: Finished) {
    let class = f.class;
    // Content leaves only when the IQ's class permits it (module docs).
    if !class.is_some_and(ContentClass::permits_content) {
        for r in &mut o.results {
            r.frames_preview.clear();
        }
        for fr in &mut o.holdout_frames {
            fr.content = None;
        }
    }
    let summary = TraceSummary::of(&o.trace, !o.nondeterministic);
    let ended = now_s();
    let summary_value = serde_json::to_value(&summary).ok();
    // Sealed by hk-synth (ADR-0021 §9.3) before anything else reads it.
    let resolution = o.seal(
        summary_value.clone(),
        Some(f.replay_key.clone()),
        &ended.to_string(),
    );
    // ---- attach (M-9): a `done` job, asked to, with an attacher; outside the lock ----
    let (request, cancelled) = {
        let g = inner.lock();
        match g.jobs.get(&n) {
            Some(j) => (
                Some(Arc::clone(&j.request)),
                j.snap.state == JobState::Cancelled,
            ),
            None => (None, true),
        }
    };
    let mut attach_warning = None;
    let attached = match (&inner.attacher, &request) {
        (Some(a), Some(req)) if req.attach && !cancelled && o.state == JobState::Done => {
            // Re-read under the lock at the end of the attach transaction: a cancel that lands
            // while attaching rolls the attach back. (Today the engine's last progress report
            // already marks the job `done`, so a `DELETE` in this window *forgets* the finished
            // job rather than cancelling it — and forgetting a job does not undo its analysis, so
            // a vanished job is not a cancelled one.)
            let is_cancelled = || {
                inner
                    .lock()
                    .jobs
                    .get(&n)
                    .is_some_and(|j| j.snap.state == JobState::Cancelled)
            };
            let input = AttachInput {
                job_id: &format!("a{n}"),
                profile: req.profile,
                target: req.emitter_id,
                band: req.band,
                window: f.read,
                outcome: &o,
                trace_summary: summary_value,
                replay_key: Some(f.replay_key),
                resolution: resolution.as_ref(),
                content_class: class.unwrap_or(ContentClass::FAIL_CLOSED),
                overload: f.overload,
                cancelled: Some(&is_cancelled),
            };
            match a.attach(&input) {
                Ok(x) => x,
                Err(e) => {
                    attach_warning = Some(format!("the results could not be attached: {e}"));
                    None
                }
            }
        }
        _ => None,
    };
    let mut g = inner.lock();
    let Some(job) = g.jobs.get_mut(&n) else {
        return;
    };
    let s = &mut job.snap;
    let cancelled = s.state == JobState::Cancelled;
    s.ended = s.ended.or(Some(ended));
    s.end_reason = o.stop;
    s.used = o.used;
    s.results = std::mem::take(&mut o.results);
    s.trace_summary = Some(summary);
    if let Some(w) = attach_warning {
        s.warnings.push(w);
    }
    if cancelled {
        // An acknowledged cancel is final; the partial results are kept.
        s.progress.state = Some(JobState::Cancelled);
    } else {
        s.state = o.state;
        s.progress.state = Some(o.state);
        s.resolution = resolution;
        if let Some(e) = o.error.take() {
            s.error = Some(JobError {
                code: "failed",
                message: e,
            });
        }
        if s.state == JobState::Done {
            match &attached {
                Some(a) => {
                    if let Some(e) = a.emitter {
                        s.emitter_id = Some(e.to_string());
                    }
                    s.decodes =
                        Some(json!({ "stored": a.decodes_stored, "valid": a.decodes_valid }));
                    s.confirm = serde_json::to_value(&a.confirm).ok();
                }
                None => {
                    s.confirm = Some(json!({
                        "rule": crate::inventory::CONFIRM_SYNTH_RULE,
                        "outcome": "not-attached",
                        "evidence_bits": s.results.first().and_then(|r| r.analytic_holdout_bits),
                        "reason": if s.attach {
                            "the results were not attached to the inventory"
                        } else {
                            "attach was not requested"
                        },
                    }));
                }
            }
        }
    }
    let snap = s.clone();
    job.trace = Some(o.trace);
    if !snap.results.is_empty() {
        emit_value(
            job,
            json!({ "type": "best", "job_id": snap.id, "results": snap.results.iter().take(3).collect::<Vec<_>>() }),
        );
    }
    let per_stage = trace_records(&snap, job.trace.as_ref().expect("just set"));
    for r in per_stage {
        emit_value(job, r);
    }
}

/// ADR-0021 §4.3: one `trace` record per stage — its outcome counts, its tried/not-tried split
/// and its ≤ 8 highest-bits retained nodes.
fn trace_records(snap: &AnalyzeJob, trace: &Trace) -> Vec<Value> {
    let Some(summary) = &snap.trace_summary else {
        return Vec::new();
    };
    summary
        .by_stage
        .iter()
        .map(|row| {
            let mut by_outcome: BTreeMap<OutcomeKind, u64> = BTreeMap::new();
            let mut nodes: Vec<&TraceNode> = Vec::new();
            for nd in trace.nodes.iter().filter(|nd| nd.stage == row.stage) {
                *by_outcome.entry(nd.outcome.kind()).or_default() += 1;
                nodes.push(nd);
            }
            for e in trace.elided.iter().filter(|e| e.stage == row.stage) {
                *by_outcome.entry(e.outcome).or_default() += e.count;
            }
            nodes.sort_by(|a, b| {
                b.evidence_bits
                    .unwrap_or(f32::NEG_INFINITY)
                    .total_cmp(&a.evidence_bits.unwrap_or(f32::NEG_INFINITY))
            });
            nodes.truncate(TRACE_RECORD_NODES);
            json!({
                "type": "trace",
                "job_id": snap.id,
                "stage": row.stage,
                "tried": row.tried,
                "not_tried": row.not_tried,
                "by_outcome": by_outcome,
                "nodes": nodes,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// The run's environment
// ---------------------------------------------------------------------------------------------

/// The tuned window of a live run, when it has one (`None` for a replay or while unknown).
pub type TunedFn = Box<dyn Fn() -> Option<FreqRange> + Send + Sync>;

/// [`JobEnv`] over a run's IQ ring (ADR-0014) and its current tuning.
pub struct RingJobEnv {
    iq: Arc<IqBufferService>,
    tuned: TunedFn,
}

impl RingJobEnv {
    /// Over `iq`, with `tuned` reporting the window live collection can reach.
    pub fn new(iq: Arc<IqBufferService>, tuned: TunedFn) -> Self {
        Self { iq, tuned }
    }
}

fn from_s(s: f64) -> Timestamp {
    Timestamp::from_unix_nanos((s * 1e9).round() as i64)
}

impl JobEnv for RingJobEnv {
    fn ring_state(&self) -> RingState {
        if !self.iq.enabled() {
            // Configured but still allocating (the open runs in the background) is a ring with
            // nothing in it yet, not a run without one.
            return if self.iq.active() {
                RingState::Empty
            } else {
                RingState::Absent
            };
        }
        let s = self.iq.status(None, None, 0);
        match (s.t0, s.t1) {
            (Some(a), Some(b)) if b > a => {
                RingState::Buffered(TimeRange::new(from_s(a), from_s(b)))
            }
            _ => RingState::Empty,
        }
    }

    fn ring(&self) -> Option<&dyn RingRead> {
        self.iq.enabled().then_some(&*self.iq as &dyn RingRead)
    }

    fn tuned(&self) -> Option<FreqRange> {
        (self.tuned)()
    }

    fn pin(
        &self,
        acquisition: &Acquisition,
        band: (f64, f64),
        label: &str,
    ) -> Result<Option<RecordingId>, String> {
        let chunks: Vec<_> = acquisition.chunks.iter().map(|c| c.chunk.clone()).collect();
        self.iq
            .pin_chunks(&chunks, Some(band), Some(label))
            .map(|c| Some(c.id))
            .map_err(|e| e.to_string())
    }
}

// ---------------------------------------------------------------------------------------------
// The stream: `hackriff.analyze/1`
// ---------------------------------------------------------------------------------------------

/// How long a stream waits for its consumer to attach before publishing, so the first snapshot
/// (or an already-finished job's `done`) is not published into an empty room.
const ATTACH_WAIT: Duration = Duration::from_secs(5);

struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// The `analyze` on-demand opener: `/ws/analyze/{id}`, `/ws/open/analyze?id=…` and TCP
/// `open/analyze?id=…`. A `messages` stream, `message_schema` [`MESSAGE_SCHEMA`], every field
/// in `metadata` (module docs, "Content"). It ends after the job's `done` record.
pub struct AnalyzeOpener(pub Arc<AnalyzeJobs>);

impl StreamOpener for AnalyzeOpener {
    fn open(&self, req: &OpenRequest) -> Result<OpenedStream, OpenRefusal> {
        let id = req
            .param("id")
            .ok_or_else(|| OpenRefusal::new(400, "invalid", "id is required"))?;
        let rx = self
            .0
            .subscribe(id)
            .map_err(|f| OpenRefusal::new(f.status, f.code, f.message))?;
        let mut header = StreamHeader::new(
            format!("analyze/{id}"),
            StreamKind::Messages,
            ContentClass::Unrestricted,
            "hk-pipeline/analyze",
        );
        header.message_schema = Some(MESSAGE_SCHEMA.into());
        let mut publisher = Publisher::new(header.clone(), PublisherConfig::default())
            .map_err(|e| OpenRefusal::new(500, "failed", e.to_string()))?;
        let handle = publisher.handle();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let attached = handle.clone();
        std::thread::Builder::new()
            .name("hk-analyze-stream".into())
            .spawn(move || {
                let t0 = Instant::now();
                while attached.open_consumers() == 0 && t0.elapsed() < ATTACH_WAIT {
                    if stopped.load(Ordering::SeqCst) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                while !stopped.load(Ordering::SeqCst) {
                    match rx.recv_timeout(Duration::from_millis(200)) {
                        Ok(v) => {
                            let done = v["type"] == "done";
                            let _ = publisher.publish_message(&MessageRecord {
                                t: Timestamp::now(),
                                emitter_id: None,
                                provenance_ref: None,
                                content_class: ContentClass::Unrestricted,
                                decode_id: None,
                                annotation_id: None,
                                decoder: None,
                                frame_model: None,
                                crc_status: None,
                                identity: None,
                                metadata: v,
                                content: None,
                            });
                            if done {
                                break;
                            }
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
                publisher.finish();
            })
            .map_err(|e| OpenRefusal::new(500, "failed", e.to_string()))?;
        Ok(OpenedStream {
            header,
            handle,
            session: Box::new(StopOnDrop(stop)),
            end: SessionEndSlot::default(),
        })
    }

    fn describe(&self) -> Value {
        json!({
            "kind": "messages",
            "message_schema": MESSAGE_SCHEMA,
            "params": ["id"],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_parse_strictly() {
        assert_eq!(parse_id("a1"), Some(1));
        assert_eq!(parse_id("a42"), Some(42));
        for bad in ["", "a", "a0", "a01", "b1", "a-1", "a1x", "1"] {
            assert_eq!(parse_id(bad), None, "{bad}");
        }
    }

    fn span(a_ms: i64, b_ms: i64) -> TimeRange {
        TimeRange::new(
            Timestamp::from_unix_nanos(a_ms * 1_000_000),
            Timestamp::from_unix_nanos(b_ms * 1_000_000),
        )
    }

    fn req(source: SourceChoice, window: Option<TimeRange>, explicit: bool) -> JobRequest {
        JobRequest {
            target: json!({}),
            emitter_id: None,
            band: FreqRange::new(100.0e6, 100.2e6),
            window,
            window_explicit: explicit,
            profile: Profile::Quick,
            max_wall_s: None,
            source,
            live_s: None,
            templates: TemplateFilter::default(),
            attach: true,
        }
    }

    #[test]
    fn the_source_is_resolved_at_admission_with_the_documented_errors() {
        let ring = RingState::Buffered(span(10_000, 20_000));
        let tuned = Some(FreqRange::new(99e6, 101e6));
        let st = |r: Result<Plan, JobFailure>| r.map_err(|e| (e.status, e.code));
        // A retained window reads the ring, clipped to what it holds.
        assert_eq!(
            st(plan(
                &req(SourceChoice::Auto, Some(span(5_000, 12_000)), true),
                ring,
                tuned
            )),
            Ok(Plan::Ring(span(10_000, 12_000)))
        );
        // Older than the oldest sample: evicted. Past the live edge: no IQ.
        assert_eq!(
            st(plan(
                &req(SourceChoice::Ring, Some(span(1_000, 2_000)), true),
                ring,
                tuned
            )),
            Err((410, "evicted"))
        );
        assert_eq!(
            st(plan(
                &req(SourceChoice::Ring, Some(span(30_000, 31_000)), true),
                ring,
                tuned
            )),
            Err((422, "no_iq"))
        );
        // An explicit past window never falls back to whatever is on the air now…
        assert_eq!(
            st(plan(
                &req(SourceChoice::Auto, Some(span(1_000, 2_000)), true),
                ring,
                tuned
            )),
            Err((410, "evicted"))
        );
        // …an emitter's default window does (ADR-0015 §5.3).
        assert_eq!(
            st(plan(
                &req(SourceChoice::Auto, Some(span(1_000, 2_000)), false),
                ring,
                tuned
            )),
            Ok(Plan::Live(DEFAULT_LIVE_S))
        );
        // Live needs the band inside the tuned window, and a ring to read behind the edge.
        assert_eq!(
            st(plan(
                &req(SourceChoice::Live, None, false),
                ring,
                Some(FreqRange::new(90e6, 100.1e6))
            )),
            Err((409, "outside_window"))
        );
        assert_eq!(
            st(plan(&req(SourceChoice::Live, None, false), ring, None)),
            Err((409, "outside_window"))
        );
        assert_eq!(
            st(plan(
                &req(SourceChoice::Auto, None, false),
                RingState::Absent,
                tuned
            )),
            Err((503, "unavailable"))
        );
        assert_eq!(
            st(plan(
                &req(SourceChoice::Ring, Some(span(10_000, 11_000)), true),
                RingState::Empty,
                tuned
            )),
            Err((422, "no_iq"))
        );
        assert_eq!(
            st(plan(&req(SourceChoice::Ring, None, false), ring, tuned)),
            Err((400, "invalid"))
        );
    }

    #[test]
    fn a_continuous_window_splits_60_40_and_keeps_its_newest_two_seconds() {
        let (set, trimmed) = continuous_set(span(0, 1_000), Membership::Band);
        assert!(!trimmed);
        assert_eq!(set.bursts[0].time, span(0, 600));
        assert_eq!(set.bursts[1].time, span(600, 1_000));
        assert_eq!(
            set.bursts.iter().map(|b| b.holdout).collect::<Vec<_>>(),
            vec![false, true]
        );
        let (set, trimmed) = continuous_set(span(0, 10_000), Membership::Band);
        assert!(trimmed);
        assert_eq!(set.bursts[0].time.start, span(8_000, 8_000).start);
        assert_eq!(set.bursts[1].time.end, span(10_000, 10_000).start);
        assert_eq!(set.read_ns(), MAX_WINDOW_NS);
    }
}
