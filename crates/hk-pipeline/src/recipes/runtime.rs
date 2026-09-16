//! Recipe pipeline runtime (ADR-0011 §1.4, §2.3–§2.5; T-088): a recipe runs as a **chain** on a
//! live or mock channel, beside the M0 chain shapes, without stopping capture.
//!
//! # A pipeline
//! One ring reader on its own thread (ADR-0001 S1), a channel DDC (the input stage, over ring
//! chunks that carry provenance) and an instantiated block [`Graph`] at a recipe revision
//! `(version, edit_rev)`. It is admitted as a `recipe` chain in the run's on-demand chain budget
//! (`503 busy` beyond it; running chains untouched), gated by the target's content class
//! (`clamp(source class, recipe ceiling)`), and serves:
//! - one always-on frames stream per `inspector` output (`inspector/<pipeline>/<output>`): one
//!   record per frame plus one `status` record per ~250 ms tick and one `edit` record per applied
//!   edit ([`crate::recipes::taps`]);
//! - one always-on stage stream per declared `stage` output (`stage/<pipeline>/<output>`);
//! - one Decode-row writer per `messages` output, republishing the stored rows on
//!   `decodes/<pipeline>/<output>` ([`crate::recipes::messages`], T-111);
//! - on-demand stage taps on any node port and the live inspector opener
//!   ([`crate::recipes::openers`]).
//!
//! # Hot edit (§2.3)
//! [`RecipeRuntime::edit`] validates the draft, stages the new graph **on the calling thread**
//! ([`graph::stage`]: build, init, port negotiation, output publishers), hands it to the pipeline
//! thread and waits. The pipeline thread swaps it in at its next chunk boundary
//! ([`swap::apply`]: O(nodes), no allocation), publishes an `edit` record and replies with the
//! sample index the new revision starts at. The reader's cursor is untouched: no ring sample is
//! lost and capture never pauses. Retired instances are dropped on the calling thread.
//!
//! # Targets and matching
//! A pipeline attaches to an inventory emitter, a persisted selection or a band by id
//! ([`Target`]). Recipe `match` hints never tune anything: the user picks the target.
//!
//! # Backpressure
//! Drop, never block: on a live source a pipeline more than [`MAX_BACKLOG_S`] behind the writer
//! skips to the live edge (counted, next chunk `DISCONTINUITY`); ring overruns are counted as
//! lost. In lossless replay the chain's gate cursor holds capture instead.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock, Weak};
use std::thread;
use std::time::{Duration, Instant};

use hk_blocks::{ChunkFlags, ChunkMeta, Input, PortInfo, PortSlice, Registry, TapMask};
use hk_core::{Discontinuity, ReadChunk};
use hk_dsp::{Ddc, DdcSpec, InputInfo};
use hk_model::{ContentClass, EmitterId, SelectionId, Timestamp};
use hk_recipe::{ChannelsSpec, Endpoint, OutputKind, PortType, RECIPE_SCHEMA, Recipe, RecipeError};
use hk_stream::inspector::InspectorRecordType;
use hk_stream::{OpenRefusal, PublisherHandle, StreamHeader};
use serde_json::{Map, Value, json};

use crate::chains::budget::{ChainKind, Slot};
use crate::chains::listen::{ListenConfig, SegmentFn};
use crate::chains::{ChainReader, Next};
use crate::class::{classify_emitter, is_restricted, restricted_band};
use crate::config::ListenSettings;
use crate::recipes::graph::{self, Graph, OutputBinding, Shape, Src, StageError, Staged};
use crate::recipes::hops;
use crate::recipes::messages::MessagesSink;
use crate::recipes::store::{RecipeStore, StoreError};
use crate::recipes::swap::{self, SwapReport};
use crate::recipes::taps::{
    FrameCtx, FrameSink, InspectorSink, OutputSink, StageTap, StreamCtx, TapPublisher,
    frames_header, inspector_policy, output_config, stage_header,
};
use crate::run::Shared;
use crate::stats::{ChainStatGuard, Counters, add, inc};

/// Status record cadence (ADR-0011 §1.3).
pub const STATUS_INTERVAL: Duration = Duration::from_millis(250);
/// A live pipeline further behind the writer than this skips to the live edge.
pub const MAX_BACKLOG_S: f64 = 2.0;
/// How long a hot edit waits for the pipeline thread's chunk boundary.
pub const EDIT_TIMEOUT: Duration = Duration::from_secs(10);
/// Samples a chain reader returns per read at most (`ChainReader::buf`).
const READER_BUF: usize = 1 << 16;
/// Narrowest channel a pipeline down-converts, Hz.
const MIN_BANDWIDTH_HZ: f64 = 1_000.0;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Takes the value out of `m` and releases the lock before returning it, so the caller drops it
/// unlocked (`drop(lock(m).take())` would drop it while the guard is still held: a pending edit's
/// drop joins sink writers).
pub(crate) fn take_unlocked<T>(m: &Mutex<Option<T>>) -> Option<T> {
    let mut guard = lock(m);
    let value = guard.take();
    drop(guard);
    value
}

#[cfg(test)]
mod take_unlocked_tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    type Slot = Arc<Mutex<Option<Probe>>>;

    /// Records whether its mutex was free when it was dropped (as a pending edit's sink writers
    /// are joined on drop).
    struct Probe(Slot, Arc<AtomicBool>);

    impl Drop for Probe {
        fn drop(&mut self) {
            self.1.store(self.0.try_lock().is_ok(), Ordering::SeqCst);
        }
    }

    /// T-112: a never-applied pending edit is dropped after `ctl.pending` is released.
    #[test]
    fn a_taken_pending_value_is_dropped_with_the_lock_released() {
        let free = Arc::new(AtomicBool::new(false));
        let pending: Slot = Arc::new(Mutex::new(None));
        *lock(&pending) = Some(Probe(Arc::clone(&pending), Arc::clone(&free)));
        drop(take_unlocked(&pending));
        assert!(
            free.load(Ordering::SeqCst),
            "dropped while the lock was held"
        );
        assert!(lock(&pending).is_none());

        // The pattern it replaces drops the value under the guard.
        *lock(&pending) = Some(Probe(Arc::clone(&pending), Arc::clone(&free)));
        drop(lock(&pending).take());
        assert!(!free.load(Ordering::SeqCst));
    }
}

/// A refused or failed runtime request: HTTP-style status, stable code, message without values,
/// and the recipe errors/warnings with paths when validation refused it.
#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeError {
    /// Status: 400, 403, 404, 409, 410, 422, 500, 503, 504.
    pub status: u16,
    /// Stable code (`invalid`, `busy`, `not_found`, ...).
    pub code: &'static str,
    /// Message.
    pub message: String,
    /// Validation errors.
    pub errors: Vec<RecipeError>,
    /// Validation warnings.
    pub warnings: Vec<RecipeError>,
}

impl RuntimeError {
    /// An error without paths.
    pub fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            errors: Vec::new(),
            warnings: Vec::new(),
        }
    }

    pub(crate) fn invalid(errors: Vec<RecipeError>) -> Self {
        Self {
            errors,
            ..Self::new(400, "invalid", "the recipe is not valid")
        }
    }

    /// `{errors, warnings}` when there are any, else `null`.
    pub fn detail(&self) -> Value {
        if self.errors.is_empty() && self.warnings.is_empty() {
            Value::Null
        } else {
            json!({
                "errors": graph::errors_json(&self.errors),
                "warnings": graph::errors_json(&self.warnings),
            })
        }
    }
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}: {}", self.status, self.code, self.message)?;
        for e in &self.errors {
            write!(f, "; {e}")?;
        }
        Ok(())
    }
}

impl std::error::Error for RuntimeError {}

impl From<StoreError> for RuntimeError {
    fn from(e: StoreError) -> Self {
        Self::new(e.status, e.code, e.message)
    }
}

impl From<StageError> for RuntimeError {
    fn from(e: StageError) -> Self {
        Self {
            errors: e.errors,
            warnings: e.warnings,
            ..Self::new(
                e.status,
                e.code,
                if e.status == 400 {
                    "the recipe is not valid"
                } else {
                    "the recipe cannot run here"
                },
            )
        }
    }
}

impl From<OpenRefusal> for RuntimeError {
    fn from(r: OpenRefusal) -> Self {
        let code = match (r.status, r.code.as_str()) {
            (503, "busy") => "busy",
            (403, _) => "restricted_class",
            (404, _) => "not_found",
            (409, _) => "conflict",
            (410, _) => "source_ended",
            (503, _) => "unavailable",
            _ => "failed",
        };
        Self::new(r.status, code, r.reason)
    }
}

/// Parses a recipe document. Serde messages are reduced so no value is echoed.
pub fn parse_recipe(doc: Value) -> Result<Recipe, RuntimeError> {
    serde_json::from_value::<Recipe>(doc).map_err(|e| {
        let m = e.to_string();
        let message = if m.starts_with("unknown field") || m.starts_with("missing field") {
            m.split(", expected").next().unwrap_or(&m).to_owned()
        } else {
            "a value has the wrong type, or the document is not a recipe".to_owned()
        };
        RuntimeError::invalid(vec![RecipeError {
            path: String::new(),
            message,
        }])
    })
}

/// What a pipeline attaches to (a hint never tunes: the user names it).
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    /// An inventory emitter (its measured centre and bandwidth).
    Emitter(EmitterId),
    /// A persisted selection's extent.
    Selection(SelectionId),
    /// A band.
    Band {
        /// Low edge, Hz.
        f_lo: f64,
        /// High edge, Hz.
        f_hi: f64,
    },
    /// A recorded capture (T-092).
    Capture(String),
}

impl Target {
    /// Parses `{emitter_id}`, `{selection_id}`, `{band: {f_lo, f_hi}}` or `{capture_id}`.
    pub fn from_json(v: &Value) -> Result<Self, RuntimeError> {
        let bad = || {
            RuntimeError::new(
                400,
                "invalid",
                "target must be one of {emitter_id}, {selection_id}, {band: {f_lo, f_hi}}, \
                 {capture_id}",
            )
        };
        let Value::Object(m) = v else {
            return Err(bad());
        };
        if m.len() != 1 {
            return Err(bad());
        }
        let (k, v) = m.iter().next().ok_or_else(bad)?;
        match (k.as_str(), v) {
            ("emitter_id", Value::String(s)) => s.parse().map(Target::Emitter).map_err(|_| bad()),
            ("selection_id", Value::String(s)) => {
                s.parse().map(Target::Selection).map_err(|_| bad())
            }
            ("capture_id", Value::String(s)) => Ok(Target::Capture(s.clone())),
            ("band", Value::Object(b)) if b.len() == 2 => {
                let f = |k: &str| b.get(k).and_then(Value::as_f64).filter(|f| f.is_finite());
                match (f("f_lo"), f("f_hi")) {
                    (Some(f_lo), Some(f_hi)) if f_hi > f_lo => Ok(Target::Band { f_lo, f_hi }),
                    _ => Err(bad()),
                }
            }
            _ => Err(bad()),
        }
    }

    /// As the API serves it.
    pub fn to_json(&self) -> Value {
        match self {
            Target::Emitter(id) => json!({"emitter_id": id.to_string()}),
            Target::Selection(id) => json!({"selection_id": id.to_string()}),
            Target::Band { f_lo, f_hi } => json!({"band": {"f_lo": f_lo, "f_hi": f_hi}}),
            Target::Capture(id) => json!({"capture_id": id}),
        }
    }
}

/// A pipeline's counters.
#[derive(Debug, Default)]
pub struct PipelineStats {
    /// Ring samples read.
    pub samples: AtomicU64,
    /// Ring chunks processed.
    pub chunks: AtomicU64,
    /// Frames published on inspector outputs.
    pub frames: AtomicU64,
    /// Chunks whose first sample did not follow the previous chunk (ring loss).
    pub gaps: AtomicU64,
    /// Chunks flagged `DISCONTINUITY` (start, loss, retune, skip, or flagged by the source) and,
    /// of those, the ones the source itself flagged on a ring block (a device gap, a recording
    /// splice) while the pipeline read contiguously. The two are packed into one atomic (low 32
    /// bits the total, high 32 bits the source-flagged subset) and counted in a single add, so a
    /// reader can never catch one counted without the other (T-228).
    discontinuity_pair: AtomicU64,
    /// Samples skipped to the live edge.
    pub skipped_samples: AtomicU64,
    /// Hot edits applied.
    pub edits: AtomicU64,
    /// Status records published (ticks).
    pub status_ticks: AtomicU64,
    /// Decode rows `messages` outputs stored (T-111).
    pub decodes: AtomicU64,
    /// Frames `messages` outputs dropped because their writer's queue was full (T-111).
    pub decodes_dropped: AtomicU64,
}

impl PipelineStats {
    /// `(discontinuities, of which the source flagged)`, from one load: the pair is always
    /// consistent because both are counted in the same atomic add (T-228).
    pub fn discontinuity_counts(&self) -> (u64, u64) {
        let v = self.discontinuity_pair.load(Ordering::Relaxed);
        (v & u64::from(u32::MAX), v >> 32)
    }

    /// Counts one emitted discontinuity, and whether the source flagged it, together.
    fn count_discontinuity(&self, from_source: bool) {
        let step = if from_source { (1u64 << 32) | 1 } else { 1 };
        self.discontinuity_pair.fetch_add(step, Ordering::Relaxed);
    }

    fn to_json(&self) -> Value {
        let g = |a: &AtomicU64| a.load(Ordering::Relaxed);
        let (discontinuities, source_discontinuities) = self.discontinuity_counts();
        json!({
            "samples": g(&self.samples),
            "chunks": g(&self.chunks),
            "frames": g(&self.frames),
            "gaps": g(&self.gaps),
            "discontinuities": discontinuities,
            "source_discontinuities": source_discontinuities,
            "skipped_samples": g(&self.skipped_samples),
            "edits": g(&self.edits),
            "status_ticks": g(&self.status_ticks),
            "decodes": g(&self.decodes),
            "decodes_dropped": g(&self.decodes_dropped),
        })
    }
}

/// One served stream of a pipeline.
#[derive(Clone)]
pub struct StreamEntry {
    /// Output id.
    pub output_id: String,
    /// `inspector` or `stage`.
    pub kind: &'static str,
    /// Stream id.
    pub stream_id: String,
    /// Header.
    pub header: StreamHeader,
    /// Handle.
    pub handle: PublisherHandle,
}

/// The control thread's view of the running revision.
#[derive(Clone)]
pub(crate) struct ControlState {
    pub recipe: Arc<Recipe>,
    pub shape: Shape,
    pub outputs: Vec<OutputBinding>,
    pub input: PortInfo,
}

// Unboxed on purpose: a staged output stream is moved into place at the swap, and a box would
// have to be freed on the pipeline thread there. One value per output per edit.
#[allow(clippy::large_enum_variant)]
enum SinkSlot {
    Keep(usize),
    New(Option<OutputSink>),
}

/// The output-stream side of an edit, applied with the graph swap.
pub(crate) struct SinkEdit {
    plan: Vec<SinkSlot>,
    old: Vec<Option<OutputSink>>,
    new: Vec<OutputSink>,
}

/// A staged edit handed to the pipeline thread.
pub(crate) struct PipelineEdit {
    staged: Staged,
    sinks: SinkEdit,
    /// New channel DDC, tune it was planned at, bandwidth and input port (an `input` edit).
    channel: Option<(Ddc, (f64, f64), f64)>,
    /// Follow-hops: the upstream sub-recipe staged per channel.
    lanes: Option<hops::LaneEdit>,
    input: PortInfo,
    new_rev: u32,
    reply: SyncSender<EditDone>,
}

/// The pipeline thread's answer: the edit (holding what was retired) and what happened.
pub(crate) struct EditDone {
    edit: PipelineEdit,
    report: Result<SwapReport, &'static str>,
    applied_at_sample: u64,
}

/// Shared state of one pipeline.
pub(crate) struct PipelineCtl {
    pub id: String,
    pub recipe_id: String,
    pub target: Target,
    pub streams_ctx: StreamCtx,
    pub started: Timestamp,
    pub shared: Weak<Shared>,
    pub stop: AtomicBool,
    pub running: AtomicBool,
    pub end: Mutex<Option<String>>,
    pub edit_rev: AtomicU32,
    pub recipe_version: AtomicU32,
    pub edit_lock: Mutex<()>,
    pub control: Mutex<ControlState>,
    pub pending: Mutex<Option<PipelineEdit>>,
    pub edit_pending: AtomicBool,
    pub new_taps: Mutex<Vec<StageTap>>,
    pub taps_dirty: AtomicBool,
    /// Closed or orphaned on-demand taps the pipeline thread handed back, dropped by the
    /// control side ([`PipelineCtl::drop_retired_taps`]) so publisher teardown stays off it.
    pub retired_taps: Mutex<Vec<StageTap>>,
    pub status: Mutex<Value>,
    pub streams: Mutex<Vec<StreamEntry>>,
    pub warnings: Mutex<Vec<RecipeError>>,
    pub stats: Arc<PipelineStats>,
    /// Follow-hops pipelines: channel set and per-channel sub-recipe (T-093).
    pub hops: Option<hops::HopsCtl>,
}

impl PipelineCtl {
    /// Drops the taps the pipeline thread retired (outside the lock).
    pub(crate) fn drop_retired_taps(&self) {
        let retired = std::mem::take(&mut *lock(&self.retired_taps));
        drop(retired);
    }
}

/// Runs, edits, lists and stores recipes on a running pipeline run
/// (`PipelineHandle::recipe_runtime`).
pub struct RecipeRuntime {
    counters: Arc<Counters>,
    segment: SegmentFn,
    listen: Arc<Mutex<ListenSettings>>,
    registry: RwLock<Arc<Registry>>,
    store: RecipeStore,
    pipelines: Mutex<BTreeMap<String, Arc<PipelineCtl>>>,
    next_id: AtomicU64,
    /// How long an edit waits for a chunk boundary, ms ([`EDIT_TIMEOUT`] by default).
    edit_timeout_ms: AtomicU64,
    /// Always-on decoded-stream capture (T-092, [`crate::recipes::capture`]).
    pub(crate) captures: std::sync::OnceLock<hk_store::decoded::DecodedCaptures>,
    /// Capture replays streaming now (T-092; capped at `capture::MAX_CAPTURE_REPLAYS`).
    pub(crate) capture_replays: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

pub(crate) fn in_window(center: f64, rate: f64, lo: f64, hi: f64) -> bool {
    rate.is_finite() && rate > 0.0 && lo >= center - 0.49 * rate && hi <= center + 0.49 * rate
}

/// The tune `(centre, rate)` to plan a channel at `center` for: the tune the capture thread last
/// published, or — before it has published one (no block captured yet) — a provisional window
/// centred on `center` at the run's rate. The pipeline thread re-plans for the tune its chunks
/// carry (`Runner::chunk`, `Hops::apply_channels`) and refuses a channel outside it then.
/// T-175: an optimised build serves a start and a channel change before the first block.
pub(crate) fn planning_tune(shared: &crate::run::Shared, center: f64) -> (f64, f64) {
    let tune = shared.counters.tune();
    if tune.1.is_finite() && tune.1 > 0.0 {
        tune
    } else {
        (center, shared.fs)
    }
}

pub(crate) struct ChannelPlan {
    pub(crate) ddc: Ddc,
    pub(crate) tune: (f64, f64),
    pub(crate) input: PortInfo,
    pub(crate) bandwidth_hz: f64,
}

/// The channel DDC and recipe input port for `recipe` on a channel at `center` (Hz) of
/// `target_bw` (Hz) under the window `tune` = (centre, rate).
pub(crate) fn channel_plan(
    recipe: &Recipe,
    center: f64,
    target_bw: f64,
    tune: (f64, f64),
) -> Result<ChannelPlan, RuntimeError> {
    let rate = recipe.input.sample_rate_hz;
    if rate.is_some_and(|r| !(r.is_finite() && r > 0.0)) {
        return Err(RuntimeError::new(
            400,
            "invalid",
            "input.sample_rate_hz must be a positive number",
        ));
    }
    let mut bw = recipe
        .input
        .bandwidth_hz
        .unwrap_or(target_bw)
        .max(MIN_BANDWIDTH_HZ);
    if let Some(r) = rate {
        bw = bw.min(r);
    }
    if !bw.is_finite() {
        return Err(RuntimeError::new(
            400,
            "invalid",
            "the channel has no bandwidth",
        ));
    }
    // The window check comes first: a channel outside the window is a 409, not a DDC failure.
    if !in_window(tune.0, tune.1, center - 0.5 * bw, center + 0.5 * bw) {
        return Err(RuntimeError::new(
            409,
            "outside_window",
            "the target channel is not inside the tuned window",
        ));
    }
    let mut spec = DdcSpec::new(center - tune.0, bw);
    spec.output_rate_hz = rate;
    let ddc = Ddc::new(spec, tune.1).map_err(|_| {
        RuntimeError::new(
            422,
            "unrealisable",
            "the channel cannot be down-converted to the recipe's input rate at the tuned rate",
        )
    })?;
    let out = ddc.output_rate_hz();
    let max_items = (READER_BUF as f64 * out / tune.1).ceil() as usize + 1024;
    Ok(ChannelPlan {
        ddc,
        tune,
        input: PortInfo {
            ty: PortType::Iq,
            rate_hz: out,
            max_items,
            hold_items: 0,
        },
        bandwidth_hz: bw,
    })
}

/// The effective class of a pipeline over `[lo, hi]`: the source class for the extent (a
/// restricted band always wins; a user rule may classify it), clamped by the recipe's ceiling.
fn pipeline_class(shared: &Shared, recipe: &Recipe, lo: f64, hi: f64) -> ContentClass {
    let src = shared.cfg.source_class;
    let source = if let Some(b) = restricted_band(lo, hi) {
        hk_stream::gate::clamp(src, b.class)
    } else if is_restricted(src) {
        src
    } else {
        classify_emitter(&shared.cfg.settings.classify, src, lo, hi).map_or(src, |(c, _)| c)
    };
    hk_stream::gate::clamp(source, recipe.output_policy.content_class)
}

/// Builds the stream of one recipe output. It is not offered in the run's stream registry yet:
/// [`offer_streams`] does that once the graph it serves is running, so a failed start or edit
/// never replaces a running output's registry entry.
fn build_sink(
    shared: &Arc<Shared>,
    stats: &Arc<PipelineStats>,
    ctx: &StreamCtx,
    recipe: &Recipe,
    b: &OutputBinding,
) -> Result<(OutputSink, Option<StreamEntry>), RuntimeError> {
    let fail = |_| RuntimeError::new(500, "failed", "creating an output stream");
    let (sink, kind, header, handle) = match b.spec.kind {
        OutputKind::Messages => {
            let (s, stream) = MessagesSink::spawn(shared, ctx, recipe, &b.spec, Arc::clone(stats))
                .map_err(|m| RuntimeError::new(500, "failed", m))?;
            let Some((header, handle)) = stream else {
                return Ok((OutputSink::Messages(s), None));
            };
            (OutputSink::Messages(s), "messages", header, handle)
        }
        OutputKind::Inspector => {
            let header = frames_header(
                ctx,
                recipe,
                format!("inspector/{}/{}", ctx.pipeline_id, b.spec.id),
                &b.spec.id,
            );
            let policy = (!ctx.class.permits_content()).then(|| inspector_policy(recipe));
            let s = InspectorSink::new(header.clone(), output_config(), policy).map_err(fail)?;
            let h = s.handle();
            (OutputSink::Frames(s), "inspector", header, h)
        }
        OutputKind::Stage => {
            let header = stage_header(
                ctx,
                recipe,
                format!("stage/{}/{}", ctx.pipeline_id, b.spec.id),
                &b.spec.id,
                b.ty,
                b.rate_hz,
            );
            let max = match b.src {
                Src::Node { .. } => (b.rate_hz.max(1.0) as usize).max(READER_BUF),
                Src::Input => READER_BUF,
            };
            let p =
                TapPublisher::new(header.clone(), output_config(), recipe, max).map_err(fail)?;
            let (node, port) = match &b.spec.from.split_once('.') {
                Some((n, p)) => ((*n).to_owned(), (*p).to_owned()),
                None => (b.spec.from.clone(), String::new()),
            };
            let tap = StageTap::new(node, port, p, None);
            let h = tap.handle();
            (OutputSink::Stage(tap), "stage", header, h)
        }
    };
    Ok((
        sink,
        Some(StreamEntry {
            output_id: b.spec.id.clone(),
            kind,
            stream_id: header.stream_id.clone(),
            header,
            handle,
        }),
    ))
}

/// Offers `entries` in the run's stream registry (registering an id again replaces its entry).
fn offer_streams<'a>(shared: &Shared, entries: impl IntoIterator<Item = &'a StreamEntry>) {
    if let Some(sink) = &shared.cfg.stream_sink {
        for e in entries {
            sink(&e.header, e.handle.clone());
        }
    }
}

/// Stops offering the streams `ids` in the run's stream registry.
fn withdraw_streams<'a>(shared: &Shared, ids: impl IntoIterator<Item = &'a str>) {
    if let Some(unsink) = &shared.cfg.stream_unsink {
        for id in ids {
            unsink(id);
        }
    }
}

/// Moves the taps `gone` selects to [`PipelineCtl::retired_taps`], so the control side drops
/// them rather than the pipeline thread.
fn retire_taps(ctl: &PipelineCtl, taps: &mut Vec<StageTap>, gone: impl Fn(&StageTap) -> bool) {
    if !taps.iter().any(&gone) {
        return;
    }
    let mut retired = lock(&ctl.retired_taps);
    let mut k = 0;
    while k < taps.len() {
        if gone(&taps[k]) {
            retired.push(taps.swap_remove(k));
        } else {
            k += 1;
        }
    }
}

impl RecipeRuntime {
    pub(crate) fn new(
        counters: Arc<Counters>,
        segment: SegmentFn,
        listen: Arc<Mutex<ListenSettings>>,
        store: RecipeStore,
    ) -> Self {
        Self {
            counters,
            segment,
            listen,
            registry: RwLock::new(Arc::new(Registry::builtin())),
            store,
            pipelines: Mutex::new(BTreeMap::new()),
            next_id: AtomicU64::new(1),
            edit_timeout_ms: AtomicU64::new(EDIT_TIMEOUT.as_millis() as u64),
            captures: std::sync::OnceLock::new(),
            capture_replays: std::sync::Arc::default(),
        }
    }

    /// Sets how long later edits wait for the pipeline thread's chunk boundary.
    pub fn set_edit_timeout(&self, timeout: Duration) {
        self.edit_timeout_ms
            .store(timeout.as_millis() as u64, Ordering::Relaxed);
    }

    /// The block catalogue recipes validate and build against.
    pub fn registry(&self) -> Arc<Registry> {
        Arc::clone(&self.registry.read().unwrap_or_else(PoisonError::into_inner))
    }

    /// Replaces the block catalogue (later starts and edits use it; running graphs keep their
    /// instances).
    pub fn set_registry(&self, registry: Registry) {
        *self
            .registry
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Arc::new(registry);
    }

    /// The recipe store.
    pub fn store(&self) -> &RecipeStore {
        &self.store
    }

    /// `GET /api/blocks`: every block descriptor.
    pub fn blocks_json(&self) -> Value {
        let r = self.registry();
        let blocks: Vec<Value> = r
            .descriptors()
            .map(|d| serde_json::to_value(d).unwrap_or(Value::Null))
            .collect();
        json!({ "blocks": blocks })
    }

    /// `POST /api/recipes/validate`: `{valid, errors, warnings, edges}`.
    pub fn validate_json(&self, doc: Value) -> Value {
        let recipe = match parse_recipe(doc) {
            Ok(r) => r,
            Err(e) => {
                return json!({"valid": false, "errors": graph::errors_json(&e.errors),
                              "warnings": [], "edges": []});
            }
        };
        match recipe.validate(&*self.registry()) {
            Ok(res) => json!({
                "valid": true,
                "errors": [],
                "warnings": graph::errors_json(&res.warnings),
                "edges": res.edges.iter().map(|e| json!({
                    "node": e.node,
                    "port": e.port,
                    "from": match &e.from {
                        Endpoint::Input => "input".to_owned(),
                        Endpoint::Node { node, port } => format!("{node}.{port}"),
                    },
                    "type": e.ty,
                })).collect::<Vec<_>>(),
            }),
            Err(errors) => json!({"valid": false, "errors": graph::errors_json(&errors),
                                  "warnings": [], "edges": []}),
        }
    }

    /// `GET /api/recipes`.
    pub fn recipes_json(&self) -> Value {
        json!({ "recipes": self.store.list().iter().map(|s| s.to_json()).collect::<Vec<_>>() })
    }

    /// `GET /api/recipes/{id}[/versions/{n}]`: the document.
    pub fn recipe_json(&self, id: &str, version: Option<u32>) -> Result<Value, RuntimeError> {
        let r = self.store.get(id, version)?;
        Ok(serde_json::to_value(r).unwrap_or(Value::Null))
    }

    /// `POST /api/recipes`: validates against the catalogue and saves as `latest + 1`.
    pub fn save_json(&self, doc: Value) -> Result<Value, RuntimeError> {
        let recipe = parse_recipe(doc)?;
        let warnings = recipe
            .validate(&*self.registry())
            .map_err(RuntimeError::invalid)?
            .warnings;
        let saved = self.store.save(recipe)?;
        Ok(json!({
            "id": saved.id,
            "version": saved.version,
            "warnings": graph::errors_json(&warnings),
            "recipe": saved,
        }))
    }

    /// `DELETE /api/recipes/{id}`: every user version.
    pub fn delete_recipe_json(&self, id: &str) -> Result<Value, RuntimeError> {
        let versions = self.store.delete(id)?;
        Ok(json!({ "id": id, "deleted_versions": versions }))
    }

    /// `POST /api/pipelines`: `{recipe_id, version?}` (a saved recipe) or `{recipe}` (a draft
    /// document), and `target`.
    pub fn start_json(&self, body: Value) -> Result<Value, RuntimeError> {
        let Value::Object(m) = body else {
            return Err(RuntimeError::new(
                400,
                "invalid",
                "the body must be an object",
            ));
        };
        if let Some(k) = m
            .keys()
            .find(|k| !["recipe_id", "version", "recipe", "target"].contains(&k.as_str()))
        {
            return Err(RuntimeError::new(
                400,
                "invalid",
                format!("unknown field {k}"),
            ));
        }
        let recipe = match (m.get("recipe_id"), m.get("recipe")) {
            (Some(Value::String(id)), None) => {
                let version = match m.get("version") {
                    None | Some(Value::Null) => None,
                    Some(v) => Some(
                        v.as_u64()
                            .and_then(|v| u32::try_from(v).ok())
                            .filter(|v| *v > 0)
                            .ok_or_else(|| {
                                RuntimeError::new(
                                    400,
                                    "invalid",
                                    "version must be a positive integer",
                                )
                            })?,
                    ),
                };
                self.store.get(id, version)?
            }
            (None, Some(doc)) if !m.contains_key("version") => parse_recipe(doc.clone())?,
            _ => {
                return Err(RuntimeError::new(
                    400,
                    "invalid",
                    "give recipe_id (and optionally version) for a saved recipe, or recipe for a draft",
                ));
            }
        };
        let target = Target::from_json(
            m.get("target")
                .ok_or_else(|| RuntimeError::new(400, "invalid", "target is required"))?,
        )?;
        let id = self.start(recipe, target)?;
        self.pipeline_json(&id)
    }

    /// Starts `recipe` on `target`; returns the pipeline id.
    pub fn start(&self, recipe: Recipe, target: Target) -> Result<String, RuntimeError> {
        let registry = self.registry();
        if recipe.schema != RECIPE_SCHEMA {
            return Err(RuntimeError::new(400, "invalid", "not a recipe document"));
        }
        recipe.validate(&*registry).map_err(RuntimeError::invalid)?;
        if recipe.input.port != PortType::Iq {
            return Err(RuntimeError::new(
                422,
                "unsupported_input",
                "only iq recipes run on a live channel; bits/soft/frames recipes run over recorded \
                 decoded streams (T-092)",
            ));
        }
        let shared = (self.segment)().ok_or_else(|| {
            RuntimeError::new(503, "unavailable", "the run is changing window; try again")
        })?;
        if shared.ring.is_closed() || shared.stop.load(Ordering::SeqCst) {
            return Err(if shared.continues.load(Ordering::SeqCst) {
                RuntimeError::new(
                    503,
                    "unavailable",
                    "the run is moving to a new window; try again",
                )
            } else {
                RuntimeError::new(410, "source_ended", "the source has ended")
            });
        }
        let (lo, hi, emitter) = self.resolve(&shared, &target)?;
        let center = 0.5 * (lo + hi);
        let tune = planning_tune(&shared, center);
        // Follow-hops (T-093): per-channel lanes run everything upstream of the merge node; the
        // main graph is the merge node and downstream of it.
        let hop = match recipe.input.channels {
            ChannelsSpec::Single => None,
            ChannelsSpec::FollowHops { .. } => Some(hops::prepare(
                &shared,
                &recipe,
                &registry,
                &target,
                (lo, hi),
                tune,
            )?),
        };
        let plan = match &hop {
            None => channel_plan(&recipe, center, hi - lo, tune)?,
            Some(h) => channel_plan(&h.up, h.lanes[0].center_hz, h.lanes[0].bandwidth_hz, tune)?,
        };
        let (clo, chi) = match &hop {
            None => (
                center - 0.5 * plan.bandwidth_hz,
                center + 0.5 * plan.bandwidth_hz,
            ),
            Some(h) => h.extent,
        };
        let (center, bandwidth_hz) = match &hop {
            None => (center, plan.bandwidth_hz),
            Some(_) => (0.5 * (clo + chi), chi - clo),
        };
        if !in_window(tune.0, tune.1, clo, chi) {
            return Err(RuntimeError::new(
                409,
                "outside_window",
                "the target channel is not inside the tuned window",
            ));
        }
        let class = pipeline_class(&shared, &recipe, clo.min(lo), chi.max(hi));
        let cfg = ListenConfig::from_settings(&lock(&self.listen));
        let slot = Slot::claim(
            &self.counters,
            &cfg.limits(),
            ChainKind::Recipe,
            cfg.chain_mcores(tune.1, Some(false)),
            tune.1,
        )?;
        let extra_slots = match &hop {
            Some(h) => hops::claim_extra(&self.counters, &cfg, h.lanes.len() - 1, tune.1)?,
            None => Vec::new(),
        };
        let id = format!("p{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let recipe = Arc::new(recipe);
        let graph_input = hop.as_ref().map_or(plan.input, |h| h.merge_port);
        let graph_recipe = hop
            .as_ref()
            .map_or_else(|| Arc::clone(&recipe), |h| Arc::clone(&h.down));
        let mut staged = graph::stage(None, graph_recipe, &registry, graph_input)?;
        let shape = staged.shape.clone();
        let warnings = staged.warnings.clone();
        let mut g = Graph::empty(graph_input);
        swap::apply(&mut g, &mut staged).map_err(|m| RuntimeError::new(500, "failed", m))?;
        drop(staged);
        let streams_ctx = StreamCtx {
            pipeline_id: id.clone(),
            class,
            center_hz: center,
            bandwidth_hz,
            emitter_id: emitter,
            channels: hop.as_ref().map_or_else(Vec::new, |h| h.channel_infos()),
        };
        let stats = Arc::new(PipelineStats::default());
        let mut sinks = Vec::with_capacity(g.outputs.len());
        let mut streams = Vec::new();
        for b in &g.outputs {
            let (s, e) = build_sink(&shared, &stats, &streams_ctx, &recipe, b)?;
            sinks.push(s);
            streams.extend(e);
        }
        let stat = self.counters.chain_stats.register("recipe");
        stat.set_channel(center, bandwidth_hz);
        if let Some(e) = streams.first() {
            stat.set_stream(&e.stream_id, e.handle.clone());
        }
        let start = shared.ring.next_sample().unwrap_or(0);
        let cursor = shared.gate.register(start);
        let reader = ChainReader::new(Arc::clone(&shared), start, cursor).with_stat(stat.stat());
        let (hops_ctl, hops_rt) = match hop {
            Some(h) => {
                let (c, r) = h.finish(extra_slots, shared.fs);
                (Some(c), Some(r))
            }
            None => (None, None),
        };
        let ctl = Arc::new(PipelineCtl {
            id: id.clone(),
            recipe_id: recipe.id.clone(),
            target,
            streams_ctx,
            started: Timestamp::now(),
            shared: Arc::downgrade(&shared),
            stop: AtomicBool::new(false),
            running: AtomicBool::new(true),
            end: Mutex::new(None),
            edit_rev: AtomicU32::new(0),
            recipe_version: AtomicU32::new(recipe.version),
            edit_lock: Mutex::new(()),
            control: Mutex::new(ControlState {
                recipe: Arc::clone(&recipe),
                shape,
                outputs: g.outputs.clone(),
                input: graph_input,
            }),
            pending: Mutex::new(None),
            edit_pending: AtomicBool::new(false),
            new_taps: Mutex::new(Vec::new()),
            taps_dirty: AtomicBool::new(false),
            retired_taps: Mutex::new(Vec::new()),
            status: Mutex::new(Value::Object(Map::new())),
            streams: Mutex::new(streams),
            warnings: Mutex::new(warnings),
            stats,
            hops: hops_ctl,
        });
        let mut runner = Runner {
            ctl: Arc::clone(&ctl),
            decoder: format!("recipe:{}@{}", recipe.id, recipe.version),
            recipe_version: recipe.version,
            shared: Arc::clone(&shared),
            reader,
            ddc: plan.ddc,
            graph: g,
            sinks,
            taps: Vec::new(),
            slot,
            stat,
            tune: plan.tune,
            center_hz: center,
            bandwidth_hz,
            next_sample: None,
            anchor: (start, Timestamp::now()),
            edit_rev: 0,
            disc: true,
            disc_source: false,
            hops: hops_rt,
        };
        runner.retap();
        // Held until the streams are offered, so an edit can't offer its streams first.
        let _serial = lock(&ctl.edit_lock);
        lock(&self.pipelines).insert(id.clone(), Arc::clone(&ctl));
        // T-092: recorded from the first frame (a failed spawn leaves no frames, so no capture).
        crate::recipes::capture::tee_streams(self, lock(&ctl.streams).iter());
        if let Err(e) = thread::Builder::new()
            .name("hk-recipe".into())
            .spawn(move || runner.run())
        {
            lock(&self.pipelines).remove(&id);
            return Err(RuntimeError::new(500, "failed", format!("spawn: {e}")));
        }
        offer_streams(&shared, lock(&ctl.streams).iter());
        Ok(id)
    }

    fn resolve(
        &self,
        shared: &Shared,
        target: &Target,
    ) -> Result<(f64, f64, Option<EmitterId>), RuntimeError> {
        match target {
            Target::Band { f_lo, f_hi } => Ok((*f_lo, *f_hi, None)),
            Target::Emitter(id) => {
                let e = shared
                    .repo()
                    .emitter(*id)
                    .map_err(|_| RuntimeError::new(404, "not_found", "no such emitter"))?;
                let h = 0.5 * e.bandwidth_hz.max(0.0);
                Ok((e.f_center_hz - h, e.f_center_hz + h, Some(*id)))
            }
            Target::Selection(id) => {
                let s = shared
                    .repo()
                    .selection(*id)
                    .map_err(|_| RuntimeError::new(404, "not_found", "no such selection"))?;
                Ok((s.f_lo_hz, s.f_hi_hz, None))
            }
            Target::Capture(_) => Err(RuntimeError::new(
                422,
                "unsupported_input",
                "running a recipe over a recorded capture lands with T-092",
            )),
        }
    }

    pub(crate) fn pipeline(&self, id: &str) -> Option<Arc<PipelineCtl>> {
        lock(&self.pipelines).get(id).cloned()
    }

    fn found(&self, id: &str) -> Result<Arc<PipelineCtl>, RuntimeError> {
        self.pipeline(id)
            .ok_or_else(|| RuntimeError::new(404, "not_found", "no such pipeline"))
    }

    fn value_of(ctl: &PipelineCtl) -> Value {
        ctl.drop_retired_taps();
        let cs = lock(&ctl.control).clone();
        let running = ctl.running.load(Ordering::SeqCst);
        json!({
            "id": ctl.id,
            "recipe_id": ctl.recipe_id,
            "recipe_version": ctl.recipe_version.load(Ordering::Relaxed),
            "edit_rev": ctl.edit_rev.load(Ordering::Relaxed),
            "state": if running { "running" } else { "ended" },
            "end_reason": *lock(&ctl.end),
            "target": ctl.target.to_json(),
            "channel": {
                "center_hz": ctl.streams_ctx.center_hz,
                "bandwidth_hz": ctl.streams_ctx.bandwidth_hz,
                "sample_rate_hz": cs.input.rate_hz,
            },
            "content_class": ctl.streams_ctx.class,
            "emitter_id": ctl.streams_ctx.emitter_id.map(|e| e.to_string()),
            "started": ctl.started.as_unix_nanos() as f64 / 1e9,
            "nodes": cs.shape.nodes.iter().map(|n| json!({"id": n.id, "block": n.block,
                "outputs": n.out_names})).collect::<Vec<_>>(),
            "outputs": lock(&ctl.streams).iter().map(|s| json!({
                "id": s.output_id, "kind": s.kind, "stream_id": s.stream_id
            })).collect::<Vec<_>>(),
            "status": lock(&ctl.status).clone(),
            "stats": ctl.stats.to_json(),
            "warnings": graph::errors_json(&lock(&ctl.warnings)),
            "follow_hops": ctl.hops.as_ref().map(hops::HopsCtl::json),
        })
    }

    /// `GET /api/pipelines`.
    pub fn pipelines_json(&self) -> Value {
        let all: Vec<Arc<PipelineCtl>> = lock(&self.pipelines).values().cloned().collect();
        json!({ "pipelines": all.iter().map(|c| Self::value_of(c)).collect::<Vec<_>>() })
    }

    /// `GET /api/pipelines/{id}`.
    pub fn pipeline_json(&self, id: &str) -> Result<Value, RuntimeError> {
        let ctl = self.found(id)?;
        Ok(Self::value_of(&ctl))
    }

    /// Pipeline counters by id (tests, `/api/pipelines/{id}` `stats`).
    pub fn stats_json(&self, id: &str) -> Option<Value> {
        self.pipeline(id).map(|c| c.stats.to_json())
    }

    /// `PUT /api/pipelines/{id}/recipe`: hot edit (ADR-0011 §2.3).
    pub fn edit_json(&self, id: &str, doc: Value) -> Result<Value, RuntimeError> {
        let draft = parse_recipe(doc)?;
        self.edit(id, draft)
    }

    /// Hot-edits pipeline `id` to `draft`: `{edit_rev, applied_at_sample, plan, swap, warnings}`.
    pub fn edit(&self, id: &str, draft: Recipe) -> Result<Value, RuntimeError> {
        let ctl = self.found(id)?;
        let ended = || RuntimeError::new(409, "ended", "the pipeline has ended");
        if !ctl.running.load(Ordering::SeqCst) {
            return Err(ended());
        }
        let _serial = lock(&ctl.edit_lock);
        let cs = lock(&ctl.control).clone();
        if draft.id != cs.recipe.id {
            return Err(RuntimeError {
                errors: vec![RecipeError {
                    path: "id".into(),
                    message: "an edit keeps the recipe id".into(),
                }],
                ..RuntimeError::invalid(Vec::new())
            });
        }
        let follow = ctl.hops.is_some();
        if draft.input.port != PortType::Iq
            || (!follow && !matches!(draft.input.channels, ChannelsSpec::Single))
        {
            return Err(RuntimeError::new(
                422,
                "unsupported_input",
                "a running pipeline keeps a single iq input",
            ));
        }
        if follow && draft.input != cs.recipe.input {
            return Err(RuntimeError::new(
                422,
                "unsupported_input",
                "a follow-hops pipeline keeps its input; change its channels with set_channels",
            ));
        }
        let registry = self.registry();
        let shared = ctl.shared.upgrade().ok_or_else(ended)?;
        let (channel, input) = if draft.input != cs.recipe.input {
            // An input edit re-plumbs the channel (every node rebuilds); capture continues.
            let tune = shared.counters.tune();
            let c = ctl.streams_ctx.center_hz;
            let plan = channel_plan(&draft, c, ctl.streams_ctx.bandwidth_hz, tune)?;
            let (lo, hi) = (c - 0.5 * plan.bandwidth_hz, c + 0.5 * plan.bandwidth_hz);
            if !in_window(tune.0, tune.1, lo, hi) {
                return Err(RuntimeError::new(
                    409,
                    "outside_window",
                    "the edited channel is not inside the tuned window",
                ));
            }
            (Some((plan.ddc, plan.tune, plan.bandwidth_hz)), plan.input)
        } else {
            (None, cs.input)
        };
        let recipe = Arc::new(draft);
        let hops_edit = match &ctl.hops {
            Some(h) => Some(h.stage_edit(&recipe, &registry)?),
            None => None,
        };
        let (base, next) = match &hops_edit {
            Some(h) => (Arc::clone(&h.old_down), Arc::clone(&h.new_down)),
            None => (Arc::clone(&cs.recipe), Arc::clone(&recipe)),
        };
        let staged = graph::stage(Some((&base, &cs.shape)), next, &registry, input)?;
        let (lanes, hops_commit) = match hops_edit {
            Some(h) => (Some(h.lanes), Some(h.commit)),
            None => (None, None),
        };
        // Output streams: unchanged outputs keep their publishers (consumers and seq); changed or
        // new ones get new streams; removed ones finish.
        let mut plan = Vec::with_capacity(staged.outputs.len());
        let mut entries = Vec::with_capacity(staged.outputs.len());
        for b in &staged.outputs {
            // The same declared output of the same type keeps its stream; a sample-rate port
            // whose rate changed gets a new stream (its header declares the rate), a frames
            // stream does not (its header has none).
            let same = |o: &OutputBinding| {
                o.spec == b.spec
                    && o.ty == b.ty
                    && (b.ty == PortType::Frames || o.rate_hz == b.rate_hz)
            };
            match cs.outputs.iter().position(same) {
                Some(j) => {
                    plan.push(SinkSlot::Keep(j));
                    entries.push(None);
                }
                None => {
                    let (s, e) = build_sink(&shared, &ctl.stats, &ctl.streams_ctx, &recipe, b)?;
                    plan.push(SinkSlot::New(Some(s)));
                    entries.push(e);
                }
            }
        }
        let n_out = staged.outputs.len();
        let new_shape = staged.shape.clone();
        let new_outputs = staged.outputs.clone();
        let edit_plan = staged.plan.clone();
        let warnings = staged.warnings.clone();
        let new_rev = ctl.edit_rev.load(Ordering::SeqCst) + 1;
        let (tx, rx) = mpsc::sync_channel(1);
        *lock(&ctl.pending) = Some(PipelineEdit {
            staged,
            sinks: SinkEdit {
                plan,
                old: Vec::with_capacity(cs.outputs.len()),
                new: Vec::with_capacity(n_out),
            },
            channel,
            lanes,
            input,
            new_rev,
            reply: tx,
        });
        ctl.edit_pending.store(true, Ordering::SeqCst);
        let EditDone {
            edit: retired,
            report,
            applied_at_sample,
        } = wait_edit(
            &ctl,
            &rx,
            Duration::from_millis(self.edit_timeout_ms.load(Ordering::Relaxed)),
        )?;
        // Retired instances and streams are dropped here, off the pipeline thread.
        drop(retired);
        ctl.drop_retired_taps();
        let report = report.map_err(|m| RuntimeError::new(409, "conflict", m))?;
        // Stream list: kept outputs keep their entries; new ones are added. Only now that the new
        // graph runs are new streams offered (replacing a changed output's entry of the same id)
        // and removed ones withdrawn; a failed edit left every running entry registered.
        {
            let mut streams = lock(&ctl.streams);
            let old: Vec<StreamEntry> = std::mem::take(&mut *streams);
            for (k, b) in new_outputs.iter().enumerate() {
                match &entries[k] {
                    Some(e) => streams.push(e.clone()),
                    None => {
                        if let Some(e) = old.iter().find(|e| e.output_id == b.spec.id) {
                            streams.push(e.clone());
                        }
                    }
                }
            }
            offer_streams(&shared, entries.iter().flatten());
            crate::recipes::capture::tee_streams(self, entries.iter().flatten()); // T-092
            withdraw_streams(
                &shared,
                old.iter()
                    .filter(|o| !streams.iter().any(|s| s.stream_id == o.stream_id))
                    .map(|o| o.stream_id.as_str()),
            );
        }
        if let (Some(h), Some(c)) = (&ctl.hops, hops_commit) {
            h.commit(c);
        }
        *lock(&ctl.control) = ControlState {
            recipe: Arc::clone(&recipe),
            shape: new_shape,
            outputs: new_outputs,
            input,
        };
        *lock(&ctl.warnings) = warnings.clone();
        Ok(json!({
            "id": ctl.id,
            "edit_rev": new_rev,
            "applied_at_sample": applied_at_sample,
            "plan": edit_plan.as_ref().map(graph::plan_json),
            "swap": {"rebuilt": report.rebuilt, "reset": report.reset,
                     "updated": report.updated, "kept": report.kept},
            "warnings": graph::errors_json(&warnings),
        }))
    }

    /// Changes the channel set of follow-hops pipeline `id` (T-093, [`hops`]): a running channel
    /// within a quarter channel bandwidth of a requested one keeps its instance and state, new
    /// ones start without stopping the others, missing ones stop.
    pub fn set_channels(&self, id: &str, channels_hz: &[f64]) -> Result<Value, RuntimeError> {
        let ctl = self.found(id)?;
        let cfg = ListenConfig::from_settings(&lock(&self.listen));
        hops::set_channels(
            &ctl,
            &self.registry(),
            &self.counters,
            &cfg,
            channels_hz,
            Duration::from_millis(self.edit_timeout_ms.load(Ordering::Relaxed)),
        )
    }

    /// Re-resolves follow-hops pipeline `id`'s channel source (its list, hop-set emitter or the
    /// detections in its band) and applies the change, e.g. a hop set that gained a channel.
    pub fn refresh_channels(&self, id: &str) -> Result<Value, RuntimeError> {
        let ctl = self.found(id)?;
        let shared = ctl
            .shared
            .upgrade()
            .ok_or_else(|| RuntimeError::new(409, "ended", "the pipeline has ended"))?;
        let recipe = Arc::clone(&lock(&ctl.control).recipe);
        let (lo, hi, _) = self.resolve(&shared, &ctl.target)?;
        let set = hops::resolve_channels(&shared, &recipe, &ctl.target, (lo, hi))?;
        self.set_channels(id, &set.channels_hz)
    }

    /// `POST /api/pipelines/{id}/save`: the running revision as the recipe's next version.
    pub fn save_pipeline_json(&self, id: &str) -> Result<Value, RuntimeError> {
        let ctl = self.found(id)?;
        let _serial = lock(&ctl.edit_lock);
        let recipe = (*lock(&ctl.control).recipe).clone();
        let saved = self.store.save(recipe)?;
        ctl.recipe_version.store(saved.version, Ordering::SeqCst);
        {
            let mut cs = lock(&ctl.control);
            let mut r = (*cs.recipe).clone();
            r.version = saved.version;
            cs.recipe = Arc::new(r);
        }
        Ok(json!({"id": saved.id, "version": saved.version, "pipeline_id": ctl.id}))
    }

    /// `DELETE /api/pipelines/{id}`: stops it (its streams finish and are no longer offered) and
    /// forgets it.
    pub fn stop_json(&self, id: &str) -> Result<Value, RuntimeError> {
        let ctl = self.found(id)?;
        ctl.stop.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_secs(5);
        while ctl.running.load(Ordering::SeqCst) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        let v = Self::value_of(&ctl);
        lock(&self.pipelines).remove(id);
        if let Some(shared) = ctl.shared.upgrade() {
            let _serial = lock(&ctl.edit_lock);
            withdraw_streams(
                &shared,
                lock(&ctl.streams).iter().map(|s| s.stream_id.as_str()),
            );
        }
        Ok(json!({ "stopped": v }))
    }

    /// Stops every pipeline (e.g. when the run ends).
    pub fn stop_all(&self) {
        let ids: Vec<String> = lock(&self.pipelines).keys().cloned().collect();
        for id in ids {
            let _ = self.stop_json(&id);
        }
    }
}

fn wait_edit(
    ctl: &PipelineCtl,
    rx: &Receiver<EditDone>,
    timeout: Duration,
) -> Result<EditDone, RuntimeError> {
    let deadline = Instant::now() + timeout;
    loop {
        match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(d) => return Ok(d),
            Err(RecvTimeoutError::Timeout) => {
                let running = ctl.running.load(Ordering::SeqCst);
                if (!running || Instant::now() > deadline) && take_unlocked(&ctl.pending).is_some()
                {
                    // Not taken by the pipeline thread: withdrawn, nothing changed.
                    return Err(if running {
                        RuntimeError::new(
                            504,
                            "timeout",
                            "the pipeline did not reach a chunk boundary in time; nothing changed",
                        )
                    } else {
                        RuntimeError::new(409, "ended", "the pipeline has ended")
                    });
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(RuntimeError::new(409, "ended", "the pipeline has ended"));
            }
        }
    }
}

struct Runner {
    ctl: Arc<PipelineCtl>,
    shared: Arc<Shared>,
    reader: ChainReader,
    ddc: Ddc,
    graph: Graph,
    sinks: Vec<OutputSink>,
    taps: Vec<StageTap>,
    slot: Slot,
    stat: ChainStatGuard,
    tune: (f64, f64),
    center_hz: f64,
    bandwidth_hz: f64,
    next_sample: Option<u64>,
    /// `(ring sample index, host time)` of the latest chunk: the frame time map.
    anchor: (u64, Timestamp),
    edit_rev: u32,
    recipe_version: u32,
    decoder: String,
    disc: bool,
    /// Whether the pending discontinuity is one the source flagged: counted with it (T-228).
    disc_source: bool,
    /// Follow-hops: the per-channel lanes feeding `graph` (T-093).
    hops: Option<hops::Hops>,
}

impl Runner {
    fn run(mut self) {
        crate::chains::set_thread_stat(Some(self.stat.stat()));
        let mut last_status = Instant::now();
        let end = loop {
            if self.ctl.stop.load(Ordering::SeqCst) {
                break "stopped".to_owned();
            }
            self.boundary();
            match self.reader.next() {
                Next::Data(chunk) => {
                    if let Err(reason) = self.chunk(&chunk) {
                        break reason;
                    }
                }
                Next::Lost => self.disc = true,
                Next::Idle => {}
                Next::Closed => {
                    self.flush_hops();
                    break if self.shared.continues.load(Ordering::SeqCst) {
                        "segment-ended".to_owned()
                    } else {
                        "source-ended".to_owned()
                    };
                }
            }
            if last_status.elapsed() >= STATUS_INTERVAL {
                last_status = Instant::now();
                self.status_tick();
            }
        };
        self.finish(end);
    }

    fn finish(mut self, reason: String) {
        self.status_tick();
        self.slot.release();
        if crate::debug_enabled() {
            eprintln!(
                "hk-pipeline: recipe pipeline {} ended: {reason}",
                self.ctl.id
            );
        }
        *lock(&self.ctl.end) = Some(reason);
        self.ctl.running.store(false, Ordering::SeqCst);
        // An edit that never reached a boundary: dropping it disconnects its waiter. Dropping it
        // joins the idle writers of its new message sinks, so it is taken out under the lock and
        // dropped after the lock is released (T-112).
        drop(take_unlocked(&self.ctl.pending));
        drop(std::mem::take(&mut *lock(&self.ctl.new_taps)));
        crate::chains::set_thread_stat(None);
    }

    /// Frame time of source sample `source_index`.
    fn time_of(anchor: (u64, Timestamp), fs: f64, source_index: f64) -> Timestamp {
        anchor
            .1
            .saturating_add_nanos(((source_index - anchor.0 as f64) / fs * 1e9).round() as i64)
    }

    /// Chunk-boundary work: new and closed taps, a staged edit.
    fn boundary(&mut self) {
        if self.ctl.taps_dirty.swap(false, Ordering::SeqCst) {
            let mut new = lock(&self.ctl.new_taps);
            self.taps.append(&mut new);
            drop(new);
            self.retap();
        }
        if self.taps.iter().any(StageTap::is_closed) {
            retire_taps(&self.ctl, &mut self.taps, StageTap::is_closed);
            self.retap();
        }
        if self.ctl.edit_pending.swap(false, Ordering::SeqCst) {
            let edit = lock(&self.ctl.pending).take();
            if let Some(e) = edit {
                self.apply_edit(e);
            }
        }
        if let Some(h) = self.hops.as_mut()
            && let Some(hc) = self.ctl.hops.as_ref()
            && hc.edit_pending.swap(false, Ordering::SeqCst)
        {
            let edit = lock(&hc.pending).take();
            if let Some(mut e) = edit {
                // At the tune the lanes follow now (T-107: a retune may have landed since the
                // new lanes were built).
                let applied = h.apply_channels(&mut e, self.tune).map(|()| {
                    self.next_sample
                        .unwrap_or_else(|| self.shared.ring.next_sample().unwrap_or(0))
                });
                e.done(applied);
            }
        }
    }

    /// Follow-hops: flushes the merge when the source ends and publishes what it released.
    fn flush_hops(&mut self) {
        let at = self.next_sample.unwrap_or(self.anchor.0);
        if let Some(h) = self.hops.as_mut()
            && h.flush(at, &mut self.graph).is_ok()
        {
            self.publish_outputs();
        }
    }

    /// Resolves taps to graph positions and sets the nodes' tap masks. A tap whose node or port
    /// no longer exists (removed by an edit) finishes.
    fn retap(&mut self) {
        for n in &mut self.graph.nodes {
            n.taps = TapMask::default();
        }
        for (k, b) in self.graph.outputs.iter().enumerate() {
            if let (Some(OutputSink::Stage(_)), Src::Node { pos, port }) =
                (self.sinks.get(k), b.src)
                && port < 32
            {
                self.graph.nodes[pos].taps.0 |= 1 << port;
            }
        }
        for tap in &mut self.taps {
            tap.at = self
                .graph
                .nodes
                .iter()
                .position(|n| n.id == tap.node)
                .and_then(|p| {
                    self.graph.nodes[p]
                        .out_names
                        .iter()
                        .position(|x| *x == tap.port)
                        .map(|k| (p, k))
                });
            if let Some((p, k)) = tap.at
                && k < 32
            {
                self.graph.nodes[p].taps.0 |= 1 << k;
            }
        }
        retire_taps(&self.ctl, &mut self.taps, |t| t.at.is_none());
    }

    fn apply_edit(&mut self, mut e: PipelineEdit) {
        let report = match (self.hops.as_ref(), e.lanes.as_ref()) {
            (Some(h), Some(l)) if !h.lanes_ready(l) => Err("a channel changed during the edit"),
            _ => swap::apply(&mut self.graph, &mut e.staged),
        };
        if report.is_ok() {
            if let (Some(h), Some(l)) = (self.hops.as_mut(), e.lanes.as_mut()) {
                h.apply_lanes(l);
            }
            {
                let SinkEdit { plan, old, new } = &mut e.sinks;
                old.clear();
                for s in self.sinks.drain(..) {
                    old.push(Some(s));
                }
                new.clear();
                for slot in plan.iter_mut() {
                    let s = match slot {
                        SinkSlot::Keep(i) => old.get_mut(*i).and_then(Option::take),
                        SinkSlot::New(s) => s.take(),
                    };
                    new.push(s.unwrap_or(OutputSink::Idle));
                }
                std::mem::swap(&mut self.sinks, new);
            }
            if let Some((ddc, tune, bw)) = e.channel.as_mut() {
                std::mem::swap(&mut self.ddc, ddc);
                self.tune = *tune;
                self.bandwidth_hz = *bw;
                self.disc = true;
            }
            self.graph.input = e.input;
            self.edit_rev = e.new_rev;
            self.ctl.edit_rev.store(e.new_rev, Ordering::SeqCst);
            inc(&self.ctl.stats.edits);
            self.retap();
        }
        let applied_at_sample = self
            .next_sample
            .unwrap_or_else(|| self.shared.ring.next_sample().unwrap_or(0));
        if let Ok(r) = &report {
            let mut m = Map::new();
            m.insert("edit_rev".into(), e.new_rev.into());
            m.insert("recipe_version".into(), self.recipe_version.into());
            m.insert("applied_at_sample".into(), applied_at_sample.into());
            m.insert("rebuilt".into(), r.rebuilt.into());
            m.insert("reset".into(), r.reset.into());
            m.insert(
                "field_maps_changed".into(),
                e.staged
                    .plan
                    .as_ref()
                    .map_or(0, |p| p.field_maps_changed.len())
                    .into(),
            );
            let t = Self::time_of(self.anchor, self.shared.fs, applied_at_sample as f64);
            for s in &mut self.sinks {
                if let OutputSink::Frames(f) = s {
                    let _ = f.record(t, InspectorRecordType::Edit, m.clone());
                }
            }
        }
        let reply = e.reply.clone();
        let _ = reply.try_send(EditDone {
            edit: e,
            report,
            applied_at_sample,
        });
    }

    fn chunk(&mut self, chunk: &ReadChunk) -> Result<(), String> {
        let tune = (
            chunk.provenance.tune.center_hz,
            chunk.provenance.tune.sample_rate_hz,
        );
        if tune != self.tune
            && let Some(h) = self.hops.as_mut()
        {
            h.retune(tune)?;
            self.tune = tune;
            self.disc = true;
        } else if tune != self.tune {
            let (lo, hi) = (
                self.center_hz - 0.5 * self.bandwidth_hz,
                self.center_hz + 0.5 * self.bandwidth_hz,
            );
            if !in_window(tune.0, tune.1, lo, hi) {
                return Err("retune: the channel left the tuned window".into());
            }
            let mut spec = self.ddc.spec().clone();
            spec.center_offset_hz = self.center_hz - tune.0;
            let ddc = Ddc::new(spec, tune.1)
                .map_err(|_| "rate-change: the channel cannot be down-converted".to_owned())?;
            if (ddc.output_rate_hz() - self.graph.input.rate_hz).abs() > 1e-6 {
                return Err(
                    "rate-change: the channel rate changed; start the pipeline again".into(),
                );
            }
            self.ddc = ddc;
            self.tune = tune;
            self.disc = true;
        }
        let st = &self.ctl.stats;
        if !self.shared.gate.enabled() {
            let head = self.shared.ring.next_sample().unwrap_or(0);
            let behind = head.saturating_sub(chunk.end_sample());
            if behind as f64 / self.shared.fs > MAX_BACKLOG_S {
                add(&st.skipped_samples, behind);
                add(&self.stat.lost_samples, behind);
                let cursor = self.shared.gate.register(head);
                self.reader = ChainReader::new(Arc::clone(&self.shared), head, cursor);
                self.next_sample = None;
                self.disc = true;
                return Ok(());
            }
        }
        if self.next_sample.is_some_and(|n| n != chunk.first_sample()) {
            inc(&st.gaps);
            self.disc = true;
        }
        if chunk.block_start
            && (chunk.discontinuity != Discontinuity::NONE || chunk.dropped_before > 0)
            && !self.disc
        {
            self.disc_source = true;
            self.disc = true;
        }
        self.next_sample = Some(chunk.end_sample());
        self.anchor = (chunk.time.sample_index, chunk.time.host_time);
        add(&st.samples, chunk.len as u64);
        inc(&st.chunks);
        let Runner {
            ddc,
            graph,
            reader,
            disc,
            disc_source,
            hops,
            ..
        } = self;
        if let Some(h) = hops.as_mut() {
            let flags = if *disc {
                st.count_discontinuity(std::mem::take(disc_source));
                ChunkFlags::DISCONTINUITY
            } else {
                ChunkFlags::NONE
            };
            *disc = false;
            h.process(chunk, &reader.buf[..chunk.len], flags, graph)?;
            reader.release_to(chunk.end_sample());
            self.publish_outputs();
            return Ok(());
        }
        let block = ddc
            .process(InputInfo::from(chunk), &reader.buf[..chunk.len])
            .map_err(|_| "error: channel down-conversion failed".to_owned())?;
        if !block.samples.is_empty() {
            let flags = if *disc {
                st.count_discontinuity(std::mem::take(disc_source));
                ChunkFlags::DISCONTINUITY
            } else {
                ChunkFlags::NONE
            };
            *disc = false;
            let time = &block.header.time;
            let meta = ChunkMeta {
                index: time.out_index,
                source_index: time.source_index,
                source_per_item: time.source_per_output,
                rate_hz: block.header.sample_rate_hz,
                channel: 0,
                flags,
            };
            let result = graph.process(Input {
                meta,
                data: PortSlice::Iq(block.samples),
            });
            if let Err((pos, e)) = result {
                return Err(format!("error: node {}: {e}", graph.nodes[pos].id));
            }
        }
        reader.release_to(chunk.end_sample());
        self.publish_outputs();
        Ok(())
    }

    fn publish_outputs(&mut self) {
        let version = self.ctl.recipe_version.load(Ordering::Relaxed);
        if version != self.recipe_version {
            self.recipe_version = version;
            self.decoder = format!("recipe:{}@{version}", self.ctl.recipe_id);
        }
        let ctx = FrameCtx {
            decoder: &self.decoder,
            frame_model: &self.ctl.recipe_id,
            emitter_id: self.ctl.streams_ctx.emitter_id,
            channel_hz: self.center_hz,
            channels_hz: self
                .hops
                .as_ref()
                .map_or(&[][..], |h| h.channels_hz.as_slice()),
            recipe_version: self.recipe_version,
            edit_rev: self.edit_rev,
        };
        let (anchor, fs) = (self.anchor, self.shared.fs);
        let t_of = move |s: f64| Self::time_of(anchor, fs, s);
        let mut frames = 0;
        for (k, b) in self.graph.outputs.iter().enumerate() {
            let (Some(out), Some(sink)) = (self.graph.output(b.src), self.sinks.get_mut(k)) else {
                continue;
            };
            let n = sink.publish(out, &ctx, &t_of);
            if b.spec.kind == OutputKind::Inspector {
                frames += n;
            }
        }
        for tap in &mut self.taps {
            if let Some(out) = tap
                .at
                .and_then(|(p, k)| self.graph.nodes.get(p).and_then(|n| n.outputs.get(k)))
            {
                tap.publish(out, &ctx, &t_of);
            }
        }
        if frames > 0 {
            add(&self.ctl.stats.frames, frames);
            add(&self.stat.records, frames);
        }
    }

    fn status_tick(&mut self) {
        let mut m = self.graph.status_metadata();
        if let Some(h) = &self.hops {
            h.status_metadata(&mut m);
        }
        *lock(&self.ctl.status) = Value::Object(m.clone());
        let at = self.next_sample.unwrap_or(self.anchor.0);
        let t = Self::time_of(self.anchor, self.shared.fs, at as f64);
        for s in &mut self.sinks {
            if let OutputSink::Frames(f) = s {
                let _ = f.record(t, InspectorRecordType::Status, m.clone());
            }
        }
        inc(&self.ctl.stats.status_ticks);
    }
}
