//! The model host: load/unload, batching, modes and shadow recording (ADR-0016 §6).
//!
//! ```text
//!   consumer ── InferenceRequest(Subject, Tensor, deadline) ──▶ bounded queue
//!                                                                    │
//!                                       one worker thread per loaded model
//!                                       flush at batch 32 · 20 ms · earliest deadline
//!                                                                    │
//!                                   provider.infer(batch) ──▶ Calibrator ──▶ Prediction
//!                                                                    │
//!                        mode = shadow ─▶ ShadowSink (records, decides nothing)
//!                        mode = active ─▶ returned to the consumer
//! ```
//!
//! # The four properties this module exists to guarantee
//!
//! - **Never on a real-time thread.** Inference runs on a worker thread owned by the host, one
//!   per loaded model (ADR-0016 §6; ADR-0007 places per-event work on the CPU, off the ring and
//!   DSP threads). A consumer that wants to block waits on a channel; the ring never does.
//! - **Bounded, and honest when it is full.** The queue has a fixed capacity. When it is full the
//!   request is **dropped and counted** ([`HostStats::dropped_queue_full`]) and the consumer falls
//!   back to the classical stage — which is what it must do whenever a provider is unavailable
//!   anyway. Back-pressure onto the caller would be worse: it would turn an ML stage that is
//!   meant to be optional into something that can stall a pipeline.
//! - **Shadow cannot decide.** [`ModelHost::observe`] is the shadow path and returns a
//!   [`Prediction`] whose [`Prediction::decides`] is false; [`ModelHost::decide`] is the only way
//!   to get a prediction a consumer may act on, and it **refuses** any mode but
//!   [`MlMode::Active`] — which [`ModelHost::set_mode`] grants only with the ADR-0016 §4.6 enable
//!   evidence *and* a conformant provider (ADR-0007). There is no third path.
//! - **Provenance is measured, not declared.** A model is loaded only after its bytes hash to the
//!   manifest's sha256 ([`crate::registry::verify`]), so the `id@version#sha8` on every
//!   [`Prediction`] names the bytes that actually ran, and `provider`/`precision` name the runtime
//!   that ran them.
//!
//! # Batching
//!
//! Defaults are ADR-0016 §6's, which are the C38 card's *estimates*: flush at
//! [`DEFAULT_MAX_BATCH`] items, after [`DEFAULT_MAX_DELAY`], or at the earliest deadline in the
//! batch, whichever comes first. Batching is what makes per-event inference affordable: the cost
//! scales with detections per second, not with bandwidth (C38 card), and a GPU provider is only
//! worth its per-call overhead if it is handed more than one item at a time.
//!
//! [`HostConfig::max_in_flight`] bounds how many batches run at once **across all models**, so a
//! model load or a burst of detections cannot starve the real-time cuFFT/PFB work sharing the
//! device (C38 card, "GPU contention").

use std::collections::HashMap;
use std::fmt;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use hk_model::Timestamp;
use serde::{Deserialize, Serialize};

use crate::gate::{Subject, SubjectKind};
use crate::predict::Calibrator;
use crate::{
    LoadedModel, MlError, MlMode, MlProvider, MlProviderKind, ModelManifest, ModelRef, Precision,
    Prediction, Tensor, TensorBatch,
};

/// Flush at this many items (ADR-0016 §6).
pub const DEFAULT_MAX_BATCH: u16 = 32;

/// Flush after this long (ADR-0016 §6).
pub const DEFAULT_MAX_DELAY: Duration = Duration::from_millis(20);

/// Requests queued per model before new ones are dropped and counted.
pub const DEFAULT_QUEUE_CAPACITY: usize = 64;

/// Batches running at once across all models.
pub const DEFAULT_MAX_IN_FLIGHT: usize = 1;

/// Who a prediction is for. The mode is set per `(model, consumer)`, so two consumers of the same
/// model can be in different modes — one shadowing while the other is off.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ConsumerId(String);

impl ConsumerId {
    /// A consumer id, e.g. `hk-classify/dl`.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The string form.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for ConsumerId {
    fn from(s: &str) -> Self {
        Self::new(s)
    }
}

impl fmt::Display for ConsumerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Host limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostConfig {
    /// Flush at this many items.
    pub max_batch: u16,
    /// Flush after this long.
    pub max_delay: Duration,
    /// Queued requests per model before new ones are dropped.
    pub queue_capacity: usize,
    /// Batches in flight at once, across models.
    pub max_in_flight: usize,
    /// Low-power mode: **active and shadow are both off** (ADR-0016 §6). Idle models must cost
    /// nothing (C38 card, "Power").
    pub low_power: bool,
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            max_batch: DEFAULT_MAX_BATCH,
            max_delay: DEFAULT_MAX_DELAY,
            queue_capacity: DEFAULT_QUEUE_CAPACITY,
            max_in_flight: DEFAULT_MAX_IN_FLIGHT,
            low_power: false,
        }
    }
}

impl HostConfig {
    /// Checks the limits are usable (a zero batch or capacity would stall every request).
    pub fn validate(&self) -> Result<(), MlError> {
        if self.max_batch == 0 || self.queue_capacity == 0 || self.max_in_flight == 0 {
            return Err(MlError::Invalid(
                "max_batch, queue_capacity and max_in_flight must all be ≥ 1".into(),
            ));
        }
        if self.max_batch > DEFAULT_MAX_BATCH {
            return Err(MlError::Invalid(format!(
                "max_batch {} exceeds the ADR-0016 §6 default of {DEFAULT_MAX_BATCH}",
                self.max_batch
            )));
        }
        if self.max_delay > DEFAULT_MAX_DELAY {
            return Err(MlError::Invalid(format!(
                "max_delay {:?} exceeds the ADR-0016 §6 default of {DEFAULT_MAX_DELAY:?}",
                self.max_delay
            )));
        }
        Ok(())
    }
}

/// One request. `input` is the **per-item** tensor (no batch dimension): the host adds the batch
/// dimension when it assembles a batch, so a consumer cannot accidentally decide the batching.
#[derive(Clone, Debug)]
pub struct InferenceRequest {
    /// What this is about — obtainable only from [`crate::gate::admit`].
    pub subject: Subject,
    /// Who is asking.
    pub consumer: ConsumerId,
    /// The model's input for this one item.
    pub input: Tensor,
    /// When the answer stops being useful. A request still queued at its deadline is dropped and
    /// counted, and the consumer falls back to the classical stage.
    pub deadline: Instant,
}

impl InferenceRequest {
    /// A request due `budget` from now.
    pub fn new(
        subject: Subject,
        consumer: impl Into<ConsumerId>,
        input: Tensor,
        budget: Duration,
    ) -> Self {
        Self {
            subject,
            consumer: consumer.into(),
            input,
            deadline: Instant::now() + budget,
        }
    }
}

/// One shadow observation as the host sees it: the prediction, its provenance, and the subject it
/// was about. A consumer that also has a classical decision to compare against puts it in
/// [`ShadowEntry::extra`] (`hk_classify::dl::ShadowRecord` serialises straight into it), so there
/// is one shadow record format rather than two.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ShadowEntry {
    /// When the prediction was made.
    pub t: Timestamp,
    /// `id@version#sha8`.
    pub model: String,
    /// Runtime that ran it.
    pub provider: MlProviderKind,
    /// Precision it ran at.
    pub precision: Precision,
    /// Who it was for.
    pub consumer: String,
    /// What it was about.
    pub subject_kind: SubjectKind,
    /// The CFAR detection it was about.
    pub detection: String,
    /// The family the classical cascade named (the model refines within it).
    pub family: String,
    /// Measured in-band SNR, dB, for per-SNR-bin aggregates (ADR-0016 §7).
    pub snr_db: Option<f64>,
    /// The model's highest-probability label. **A class call, never an unknown detector.**
    pub label: String,
    /// Its temperature-calibrated probability.
    pub p: f64,
    /// Energy `E = −T·logsumexp(z/T)`.
    pub energy: f64,
    /// Calibrated open-set score, 0–1.
    pub unknown_score: f64,
    /// Inference latency of the batch it ran in, ms.
    pub latency_ms: f32,
    /// How many items that batch held.
    pub batch_size: u16,
    /// The mode it ran in. `shadow`, or nothing would be recorded here.
    pub mode: String,
    /// The consumer's own comparison against its classical decision, if it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra: Option<serde_json::Value>,
}

/// Where shadow records go (ADR-0016 §6: hk-store `ml/shadow/`, hourly CRC-line NDJSON).
///
/// The sink is a trait so the host does not depend on the store, and so a test can hold the
/// records in memory. A failing sink never fails inference: shadow records are diagnostics.
pub trait ShadowSink: Send + Sync {
    /// Appends one record.
    fn record(&self, entry: &ShadowEntry) -> Result<(), MlError>;
}

/// An in-memory sink, for tests and for a run with no store attached.
#[derive(Debug, Default)]
pub struct MemoryShadowSink(Mutex<Vec<ShadowEntry>>);

impl MemoryShadowSink {
    /// An empty sink.
    pub fn new() -> Self {
        Self::default()
    }

    /// Everything recorded so far.
    pub fn entries(&self) -> Vec<ShadowEntry> {
        self.0.lock().expect("shadow sink mutex").clone()
    }
}

impl ShadowSink for MemoryShadowSink {
    fn record(&self, entry: &ShadowEntry) -> Result<(), MlError> {
        self.0
            .lock()
            .expect("shadow sink mutex")
            .push(entry.clone());
        Ok(())
    }
}

/// Host counters. Every rejection has a counter: a stage that silently does nothing is worse than
/// one that reports it is dropping work.
///
/// **Counters are published before the answer they describe** (T-283): once
/// [`ModelHost::observe`] or [`ModelHost::decide`] has returned, [`ModelHost::stats`] already
/// accounts for that request. Without that ordering a consumer could read its own outcome as not
/// yet counted, and every read of the counters would be a race with the worker thread.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct HostStats {
    /// Requests accepted into a queue.
    pub submitted: u64,
    /// Predictions returned.
    pub inferred: u64,
    /// Requests refused because the queue was full.
    pub dropped_queue_full: u64,
    /// Requests dropped because their deadline had passed before they ran.
    pub deadline_missed: u64,
    /// Batches run.
    pub batches: u64,
    /// Items in those batches.
    pub batched_items: u64,
    /// The largest batch actually formed.
    pub largest_batch: u16,
    /// Batches whose inference returned an error.
    pub errors: u64,
    /// Shadow records handed to the sink.
    pub shadow_records: u64,
    /// Sum of batch latencies, ms.
    pub latency_ms_sum: f64,
    /// The slowest batch, ms.
    pub latency_ms_max: f32,
}

impl HostStats {
    /// Mean items per batch, or 0 with no batches.
    pub fn mean_batch(&self) -> f64 {
        if self.batches == 0 {
            0.0
        } else {
            self.batched_items as f64 / self.batches as f64
        }
    }

    /// Mean batch latency, ms.
    pub fn mean_latency_ms(&self) -> f64 {
        if self.batches == 0 {
            0.0
        } else {
            self.latency_ms_sum / self.batches as f64
        }
    }
}

/// A `(model, consumer)` mode and how it got there.
#[derive(Clone, Debug, PartialEq)]
pub struct ModeEntry {
    /// The mode in force.
    pub mode: MlMode,
    /// Whether ADR-0016 §4.6's requirements were overridden to set it. A forced `active` is
    /// audited rather than refused, so an operator can do it deliberately and it is never
    /// invisible.
    pub forced: bool,
}

/// Bounds concurrent inference across all models in one host.
#[derive(Debug)]
struct InFlight {
    max: usize,
    state: Mutex<usize>,
    cv: Condvar,
}

impl InFlight {
    fn new(max: usize) -> Self {
        Self {
            max,
            state: Mutex::new(0),
            cv: Condvar::new(),
        }
    }

    fn acquire(&self) -> Permit<'_> {
        let mut n = self.state.lock().expect("in-flight mutex");
        while *n >= self.max {
            n = self.cv.wait(n).expect("in-flight mutex");
        }
        *n += 1;
        Permit(self)
    }
}

struct Permit<'a>(&'a InFlight);

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        let mut n = self.0.state.lock().expect("in-flight mutex");
        *n = n.saturating_sub(1);
        self.0.cv.notify_one();
    }
}

/// One queued request.
struct Job {
    input: Tensor,
    mode: MlMode,
    deadline: Instant,
    reply: SyncSender<Result<Prediction, MlError>>,
}

/// One loaded model: its manifest, its worker and the channel to it.
struct Loaded {
    manifest: ModelManifest,
    provider: MlProviderKind,
    conformant: bool,
    tx: Option<SyncSender<Job>>,
    worker: Option<JoinHandle<()>>,
}

impl Loaded {
    /// Drops the queue and waits for the worker to finish the batch it is running.
    fn stop(&mut self) {
        self.tx = None;
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

/// The model host.
pub struct ModelHost {
    config: HostConfig,
    provider: Arc<dyn MlProvider>,
    loaded: Mutex<HashMap<String, Loaded>>,
    modes: Mutex<HashMap<(String, ConsumerId), ModeEntry>>,
    stats: Arc<Mutex<HostStats>>,
    sink: Option<Arc<dyn ShadowSink>>,
    in_flight: Arc<InFlight>,
}

impl fmt::Debug for ModelHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModelHost")
            .field("provider", &self.provider.kind())
            .field("config", &self.config)
            .field("loaded", &self.loaded.lock().map(|l| l.len()).unwrap_or(0))
            .finish()
    }
}

impl Drop for ModelHost {
    fn drop(&mut self) {
        if let Ok(mut loaded) = self.loaded.lock() {
            for (_, mut entry) in loaded.drain() {
                entry.stop();
            }
        }
    }
}

/// `id@version`, the key a mode and a loaded model are held under.
pub fn model_key(model: &ModelRef) -> String {
    format!("{}@{}", model.id, model.version)
}

impl ModelHost {
    /// A host over `provider`.
    pub fn new(config: HostConfig, provider: Arc<dyn MlProvider>) -> Result<Self, MlError> {
        config.validate()?;
        Ok(Self {
            in_flight: Arc::new(InFlight::new(config.max_in_flight)),
            config,
            provider,
            loaded: Mutex::new(HashMap::new()),
            modes: Mutex::new(HashMap::new()),
            stats: Arc::new(Mutex::new(HostStats::default())),
            sink: None,
        })
    }

    /// Attaches a shadow sink.
    pub fn with_shadow_sink(mut self, sink: Arc<dyn ShadowSink>) -> Self {
        self.sink = Some(sink);
        self
    }

    /// The limits in force.
    pub fn config(&self) -> &HostConfig {
        &self.config
    }

    /// The provider backing it.
    pub fn provider(&self) -> MlProviderKind {
        self.provider.kind()
    }

    /// Loads `bytes` under `manifest` and starts its worker.
    ///
    /// The bytes are verified against the manifest's sha256 first, so the `#sha8` in the
    /// [`ModelRef`] this returns — and in every [`Prediction`] the model produces — names the
    /// bytes that ran.
    pub fn load(&self, manifest: &ModelManifest, bytes: &[u8]) -> Result<ModelRef, MlError> {
        crate::registry::verify(manifest, bytes)?;
        let key = model_key(&manifest.model);
        let mut loaded = self.loaded.lock().expect("host mutex");
        if loaded.contains_key(&key) {
            return Err(MlError::Invalid(format!("{key} is already loaded")));
        }
        let model = self.provider.load(manifest, bytes)?;
        let calibrator =
            Calibrator::from_manifest(manifest, self.provider.kind(), manifest.precision)?;
        let (tx, rx) = sync_channel::<Job>(self.config.queue_capacity);
        let config = self.config;
        let stats = Arc::clone(&self.stats);
        let in_flight = Arc::clone(&self.in_flight);
        let name = format!("hk-ml:{key}");
        let worker = thread::Builder::new()
            .name(name)
            .spawn(move || worker_loop(rx, model, calibrator, config, stats, in_flight))
            .map_err(|e| MlError::Invalid(format!("cannot start a worker thread: {e}")))?;
        loaded.insert(
            key,
            Loaded {
                manifest: manifest.clone(),
                provider: self.provider.kind(),
                conformant: self.provider.conformant(),
                tx: Some(tx),
                worker: Some(worker),
            },
        );
        Ok(manifest.model.clone())
    }

    /// Loads `id@version` out of a registry.
    pub fn load_from_registry(
        &self,
        registry: &crate::registry::ModelRegistry,
        id: &str,
        version: &str,
    ) -> Result<ModelRef, MlError> {
        let stored = registry.read(id, version)?;
        self.load(&stored.manifest, &stored.bytes)
    }

    /// Unloads a model: the queue is closed, the worker finishes the batch it is running and
    /// stops, and every mode set for the model is forgotten.
    ///
    /// Rolling back to an earlier version is this plus a [`ModelHost::set_mode`] on the version
    /// that stays loaded — never a rebuild (ADR-0001).
    pub fn unload(&self, model: &ModelRef) -> Result<(), MlError> {
        let key = model_key(model);
        let mut entry = {
            let mut loaded = self.loaded.lock().expect("host mutex");
            loaded
                .remove(&key)
                .ok_or_else(|| MlError::Invalid(format!("{key} is not loaded")))?
        };
        entry.stop();
        self.modes
            .lock()
            .expect("host mutex")
            .retain(|(k, _), _| k != &key);
        Ok(())
    }

    /// What is loaded, with the manifest each was loaded under.
    pub fn loaded(&self) -> Vec<ModelManifest> {
        let loaded = self.loaded.lock().expect("host mutex");
        let mut out: Vec<_> = loaded.values().map(|l| l.manifest.clone()).collect();
        out.sort_by(|a, b| model_key(&a.model).cmp(&model_key(&b.model)));
        out
    }

    /// The mode in force for `(model, consumer)`; [`MlMode::Off`] if none was set.
    pub fn mode(&self, model: &ModelRef, consumer: &ConsumerId) -> MlMode {
        self.modes
            .lock()
            .expect("host mutex")
            .get(&(model_key(model), consumer.clone()))
            .map_or(MlMode::Off, |m| m.mode)
    }

    /// Every mode set, for the `/api/ml/models` view.
    pub fn modes(&self) -> Vec<(String, ConsumerId, ModeEntry)> {
        let modes = self.modes.lock().expect("host mutex");
        let mut out: Vec<_> = modes
            .iter()
            .map(|((k, c), e)| (k.clone(), c.clone(), e.clone()))
            .collect();
        out.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
        out
    }

    /// Sets the mode for `(model, consumer)`.
    ///
    /// [`MlMode::Active`] needs **both** of ADR-0016's conditions, and says which is missing:
    /// the §4.6 enable evidence on the manifest, and a provider that has passed the conformance
    /// suite (ADR-0007). `force` overrides them and is recorded on the entry
    /// ([`ModeEntry::forced`]) so it is auditable; nothing else can reach `active`.
    pub fn set_mode(
        &self,
        model: &ModelRef,
        consumer: &ConsumerId,
        mode: MlMode,
        force: bool,
    ) -> Result<(), MlError> {
        let key = model_key(model);
        let loaded = self.loaded.lock().expect("host mutex");
        let entry = loaded
            .get(&key)
            .ok_or_else(|| MlError::Invalid(format!("{key} is not loaded")))?;
        if mode == MlMode::Active && !force {
            if !entry.manifest.allows(MlMode::Active) {
                return Err(MlError::Invalid(format!(
                    "{key}: active needs the ADR-0016 §4.6 enable evidence on the manifest"
                )));
            }
            if !entry.conformant {
                return Err(MlError::Invalid(format!(
                    "{key}: provider {} has not passed the conformance suite, so it never \
                     decides anything (ADR-0007)",
                    entry.provider
                )));
            }
        }
        drop(loaded);
        self.modes.lock().expect("host mutex").insert(
            (key, consumer.clone()),
            ModeEntry {
                mode,
                forced: force && mode == MlMode::Active,
            },
        );
        Ok(())
    }

    /// Counters.
    pub fn stats(&self) -> HostStats {
        *self.stats.lock().expect("stats mutex")
    }

    /// **The shadow path.** Runs the model if `(model, consumer)` is in shadow or active mode and
    /// records what it said.
    ///
    /// The prediction comes back so a consumer can compare it with its own classical decision and
    /// put that comparison in the record — but it carries [`MlMode::Shadow`], so
    /// [`Prediction::decides`] is false and nothing downstream may act on it. `Ok(None)` means
    /// nothing ran: mode off, low power, or the request was dropped (counted in [`Self::stats`]).
    pub fn observe(
        &self,
        model: &ModelRef,
        request: &InferenceRequest,
        extra: Option<serde_json::Value>,
    ) -> Result<Option<Prediction>, MlError> {
        let mode = self.mode(model, &request.consumer);
        if mode == MlMode::Off || self.config.low_power {
            return Ok(None);
        }
        let prediction = match self.run(model, request, MlMode::Shadow)? {
            Some(p) => p,
            None => return Ok(None),
        };
        if let (Some(sink), Some((label, p))) = (self.sink.as_ref(), prediction.top()) {
            let entry = ShadowEntry {
                t: prediction.t,
                model: prediction.model.to_string(),
                provider: prediction.provider,
                precision: prediction.precision,
                consumer: request.consumer.to_string(),
                subject_kind: request.subject.kind(),
                detection: request.subject.detection().to_string(),
                family: request.subject.family().to_owned(),
                snr_db: request.subject.snr_db(),
                label: label.to_owned(),
                p: f64::from(p),
                energy: f64::from(prediction.energy),
                unknown_score: f64::from(prediction.unknown_score),
                latency_ms: prediction.latency_ms,
                batch_size: prediction.batch_size,
                mode: prediction.mode.as_str().to_owned(),
                extra,
            };
            // A sink that cannot write must not fail inference, but it is counted.
            if sink.record(&entry).is_ok() {
                self.stats.lock().expect("stats mutex").shadow_records += 1;
            }
        }
        Ok(Some(prediction))
    }

    /// **The decision path.** Refuses unless `(model, consumer)` is [`MlMode::Active`], which
    /// [`ModelHost::set_mode`] grants only with enable evidence and a conformant provider.
    ///
    /// A shadow prediction can never be obtained here, and an active one can never be obtained
    /// without those two pieces of evidence: that is the whole of "shadow never decides".
    pub fn decide(
        &self,
        model: &ModelRef,
        request: &InferenceRequest,
    ) -> Result<Prediction, MlError> {
        if self.config.low_power {
            return Err(MlError::Invalid(
                "low-power mode: active and shadow are both off (ADR-0016 §6)".into(),
            ));
        }
        let mode = self.mode(model, &request.consumer);
        if mode != MlMode::Active {
            return Err(MlError::Invalid(format!(
                "{} is in {} mode for {}: only an active model decides anything",
                model_key(model),
                mode.as_str(),
                request.consumer
            )));
        }
        self.run(model, request, MlMode::Active)?
            .ok_or(MlError::DeadlineMissed)
    }

    /// Queues one request and waits for its answer. `None` means it was dropped (queue full or
    /// deadline missed), both of which are counted.
    fn run(
        &self,
        model: &ModelRef,
        request: &InferenceRequest,
        mode: MlMode,
    ) -> Result<Option<Prediction>, MlError> {
        let key = model_key(model);
        let (reply, answer) = sync_channel::<Result<Prediction, MlError>>(1);
        {
            let loaded = self.loaded.lock().expect("host mutex");
            let entry = loaded
                .get(&key)
                .ok_or_else(|| MlError::Invalid(format!("{key} is not loaded")))?;
            if entry.manifest.family.as_deref() != Some(request.subject.family()) {
                return Err(MlError::Invalid(format!(
                    "{key} is scoped to family {:?}, not {:?}: a model never chooses the family \
                     (ADR-0016 §4.6)",
                    entry.manifest.family,
                    request.subject.family()
                )));
            }
            let tx = entry
                .tx
                .as_ref()
                .ok_or_else(|| MlError::Invalid(format!("{key} is unloading")))?;
            let job = Job {
                input: request.input.clone(),
                mode,
                deadline: request.deadline,
                reply,
            };
            match tx.try_send(job) {
                Ok(()) => self.stats.lock().expect("stats mutex").submitted += 1,
                Err(TrySendError::Full(_)) => {
                    self.stats.lock().expect("stats mutex").dropped_queue_full += 1;
                    return Ok(None);
                }
                Err(TrySendError::Disconnected(_)) => {
                    return Err(MlError::Invalid(format!("{key}'s worker has stopped")));
                }
            }
        }
        match answer.recv() {
            Ok(Ok(p)) => Ok(Some(p)),
            Ok(Err(MlError::DeadlineMissed)) => Ok(None),
            Ok(Err(e)) => Err(e),
            // The worker died (a panicking provider): report it rather than hanging.
            Err(_) => Err(MlError::Invalid(format!(
                "{key}'s worker stopped answering"
            ))),
        }
    }
}

/// One model's worker thread: assemble a batch, run it, answer every request in it.
fn worker_loop(
    rx: Receiver<Job>,
    model: Box<dyn LoadedModel>,
    calibrator: Calibrator,
    config: HostConfig,
    stats: Arc<Mutex<HostStats>>,
    in_flight: Arc<InFlight>,
) {
    let mut carry: Option<Job> = None;
    loop {
        // Block until there is something to do: an idle model costs nothing.
        let first = match carry.take() {
            Some(j) => j,
            None => match rx.recv() {
                Ok(j) => j,
                Err(_) => return, // unloaded
            },
        };
        let opened = Instant::now();
        let shape = first.input.shape.clone();
        let mut batch = vec![first];
        let mut disconnected = false;

        // Fill until the batch is full, the delay expires, or the earliest deadline arrives.
        while batch.len() < usize::from(config.max_batch) {
            let earliest = batch.iter().map(|j| j.deadline).min().unwrap_or(opened);
            let now = Instant::now();
            let by_delay = (opened + config.max_delay).saturating_duration_since(now);
            let by_deadline = earliest.saturating_duration_since(now);
            let wait = by_delay.min(by_deadline);
            if wait.is_zero() {
                break;
            }
            match rx.recv_timeout(wait) {
                Ok(job) => {
                    // A different input shape cannot share a batch; it starts the next one.
                    if job.input.shape == shape {
                        batch.push(job);
                    } else {
                        carry = Some(job);
                        break;
                    }
                }
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }

        // Anything already past its deadline is dropped and counted; the consumer falls back to
        // the classical stage.
        //
        // The count is published *before* the replies go out (T-283). Answering a consumer
        // releases it, and it may read [`ModelHost::stats`] on its very next instruction — so a
        // counter written after the reply can be read one increment behind the answer it
        // describes. Counting first makes "the consumer has its answer" imply "the counters
        // account for it", which is what makes the stats a usable record of what the host did.
        let now = Instant::now();
        let mut late: Vec<Job> = Vec::new();
        let kept: Vec<Job> = batch
            .drain(..)
            .filter_map(|job| {
                if job.deadline <= now {
                    late.push(job);
                    None
                } else {
                    Some(job)
                }
            })
            .collect();
        batch = kept;
        if !late.is_empty() {
            stats.lock().expect("stats mutex").deadline_missed += late.len() as u64;
            for job in late {
                let _ = job.reply.send(Err(MlError::DeadlineMissed));
            }
        }
        if batch.is_empty() {
            if disconnected && carry.is_none() {
                return;
            }
            continue;
        }

        let n = batch.len();
        let mut data = Vec::with_capacity(n * batch[0].input.data.len());
        for job in &batch {
            data.extend_from_slice(&job.input.data);
        }
        let mut batched_shape = Vec::with_capacity(shape.len() + 1);
        batched_shape.push(n);
        batched_shape.extend_from_slice(&shape);
        let Some(input) = Tensor::new(batched_shape, data) else {
            for job in &batch {
                let _ = job.reply.send(Err(MlError::Invalid(
                    "the batch's shape and data disagree".into(),
                )));
            }
            continue;
        };
        let tensors = TensorBatch {
            input,
            batch_size: u16::try_from(n).unwrap_or(u16::MAX),
        };

        let started = Instant::now();
        let outputs = {
            let _permit = in_flight.acquire();
            model.infer(&tensors)
        };
        let latency_ms = started.elapsed().as_secs_f32() * 1e3;

        {
            let mut s = stats.lock().expect("stats mutex");
            s.batches += 1;
            s.batched_items += n as u64;
            s.largest_batch = s.largest_batch.max(tensors.batch_size);
            s.latency_ms_sum += f64::from(latency_ms);
            s.latency_ms_max = s.latency_ms_max.max(latency_ms);
            if outputs.is_err() {
                s.errors += 1;
            }
        }

        let t = Timestamp::now();
        match outputs {
            Ok(raw) if raw.len() == n => {
                // Calibrate the whole batch, publish the count, then release the consumers —
                // same ordering rule as the deadline drops above (T-283).
                let answers: Vec<_> = batch
                    .iter()
                    .zip(&raw)
                    .map(|(job, out)| {
                        calibrator.predict(out, job.mode, latency_ms, tensors.batch_size, t)
                    })
                    .collect();
                let inferred = answers.iter().filter(|p| p.is_ok()).count() as u64;
                stats.lock().expect("stats mutex").inferred += inferred;
                for (job, p) in batch.iter().zip(answers) {
                    let _ = job.reply.send(p);
                }
            }
            Ok(raw) => {
                stats.lock().expect("stats mutex").errors += 1;
                for job in &batch {
                    let _ = job.reply.send(Err(MlError::Invalid(format!(
                        "the model returned {} outputs for a batch of {n}",
                        raw.len()
                    ))));
                }
            }
            Err(e) => {
                for job in &batch {
                    let _ = job.reply.send(Err(e.clone()));
                }
            }
        }

        if disconnected && carry.is_none() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        LoadedModel, ModelRef, ModelTask, OpenSetMethod, OpenSetSpec, Precision, RawOutput,
    };
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A provider that answers instantly (or slowly, on request) and records the batches it was
    /// handed. The host's behaviour is what these tests are about, so the arithmetic is trivial:
    /// the logits are derived from the input so a wrong item↔answer pairing would show up.
    struct Fake {
        conformant: bool,
        delay: Duration,
        batches: Arc<Mutex<Vec<u16>>>,
        calls: Arc<AtomicU64>,
    }

    impl Fake {
        fn new(conformant: bool, delay: Duration) -> Self {
            Self {
                conformant,
                delay,
                batches: Arc::new(Mutex::new(Vec::new())),
                calls: Arc::new(AtomicU64::new(0)),
            }
        }
    }

    struct FakeModel {
        manifest: ModelManifest,
        delay: Duration,
        batches: Arc<Mutex<Vec<u16>>>,
        calls: Arc<AtomicU64>,
    }

    impl MlProvider for Fake {
        fn kind(&self) -> MlProviderKind {
            MlProviderKind::CpuTract
        }

        fn conformant(&self) -> bool {
            self.conformant
        }

        fn load(
            &self,
            manifest: &ModelManifest,
            _bytes: &[u8],
        ) -> Result<Box<dyn LoadedModel>, MlError> {
            Ok(Box::new(FakeModel {
                manifest: manifest.clone(),
                delay: self.delay,
                batches: Arc::clone(&self.batches),
                calls: Arc::clone(&self.calls),
            }))
        }
    }

    impl LoadedModel for FakeModel {
        fn manifest(&self) -> &ModelManifest {
            &self.manifest
        }

        fn infer(&self, batch: &TensorBatch) -> Result<Vec<RawOutput>, MlError> {
            self.batches.lock().expect("batches").push(batch.batch_size);
            self.calls.fetch_add(1, Ordering::Relaxed);
            if !self.delay.is_zero() {
                thread::sleep(self.delay);
            }
            let width = usize::from(batch.batch_size.max(1));
            let per_item = batch.input.data.len() / width;
            Ok((0..width)
                .map(|i| RawOutput {
                    // The first input value, and a fixed second logit: a swapped answer would
                    // land on the wrong request and the assertions would catch it.
                    logits: vec![batch.input.data[i * per_item], 0.0],
                    embedding: None,
                })
                .collect())
        }
    }

    const BYTES: &[u8] = b"fake model bytes";

    fn manifest(evidence: bool) -> ModelManifest {
        let sha = crate::registry::sha256_hex(BYTES);
        ModelManifest {
            schema: crate::ML_SCHEMA,
            model: ModelRef {
                id: "amc-fsk".into(),
                version: "1.0.0".into(),
                sha8: sha[..8].to_owned(),
            },
            sha256: sha,
            task: ModelTask::FamilyClass,
            consumer: "hk-classify/dl".into(),
            taxonomy: None,
            family: Some("fsk".into()),
            labels: vec!["2fsk".into(), "gfsk".into()],
            open_set: OpenSetSpec {
                method: OpenSetMethod::Energy,
                temperature: 1.0,
                threshold: -4.0,
                calibrated_on: "dev@amc-grid-1".into(),
            },
            precision: Precision::Fp32,
            metrics_ref: None,
            enable_evidence: evidence.then(|| "dev/amc-eval@1".to_owned()),
        }
    }

    fn subject(family: &str) -> Subject {
        use hk_model::classify::{
            CLASSIFICATION_SCHEMA, ClassProvenance, Classification, Coarse, HK_MOD_V1, LabelP,
            Stage, SuspectFlags, TaxonomyRef, entropy_norm,
        };
        use hk_model::{
            Detection, DetectionFlags, DetectionId, ProvenanceId, SurveyId, TimeRange, Timestamp,
        };

        let detection = Detection {
            id: DetectionId::new(),
            survey_id: SurveyId::new(),
            time: TimeRange::new(
                Timestamp::from_unix_nanos(1_789_300_820_000_000_000),
                Timestamp::from_unix_nanos(1_789_300_821_000_000_000),
            ),
            f_center_hz: 915e6,
            obw_hz: 40e3,
            xdb_bandwidth_hz: None,
            xdb_level_db: None,
            snr_peak_db: 24.0,
            snr_mean_db: 21.0,
            peak_level_dbfs: -18.0,
            peak_level_dbm: None,
            sk: None,
            clip_count: 0,
            detector_version: "hk-detect/cfar@0.1.0;pfa=1e-6".into(),
            provenance_ref: ProvenanceId::new(),
            flags: DetectionFlags::default(),
        };
        let posterior = vec![
            LabelP {
                label: family.to_owned(),
                p: 0.8,
            },
            LabelP {
                label: "unknown".into(),
                p: 0.2,
            },
        ];
        let classification = Classification {
            schema: CLASSIFICATION_SCHEMA,
            t: Timestamp::from_unix_nanos(1_789_300_820_000_000_000),
            taxonomy: TaxonomyRef::current(),
            input: None,
            coarse: Coarse::Digital,
            entropy_norm: entropy_norm(&posterior, HK_MOD_V1.families.len() + 1),
            likelihood: posterior.clone(),
            posterior,
            prior: None,
            family: family.to_owned(),
            confidence: 0.8,
            class: None,
            open_set_score: 0.2,
            stage: Stage::FeatureTree,
            provenance: ClassProvenance {
                rules: "hk-classify/tree@1".into(),
                // Determinate (T-292): this fixture stands for a row a current writer produced,
                // not the pre-T-290 `FEATURES_VERSION_INDETERMINATE` marker. hk-ml doesn't depend
                // on hk-classify, so it can't name `hk_classify::FEATURES_VERSION` directly.
                features_version: 2,
                features_ref: None,
                ml: None,
                snr_db: Some(24.0),
                snr_gate_db: 20.0,
                gated: false,
                thresholds: "thresholds@1".into(),
                suspect: SuspectFlags::default(),
                power_mode: None,
            },
            flags: Vec::new(),
            reasons: Vec::new(),
        };
        crate::gate::admit(&detection, &classification).expect("a classified CFAR detection")
    }

    fn request(family: &str, value: f32) -> InferenceRequest {
        InferenceRequest::new(
            subject(family),
            "hk-classify/dl",
            Tensor::new(vec![4], vec![value, 0.0, 0.0, 0.0]).unwrap(),
            Duration::from_secs(10),
        )
    }

    fn new_host(provider: Fake) -> (ModelHost, Arc<Mutex<Vec<u16>>>, Arc<MemoryShadowSink>) {
        let batches = Arc::clone(&provider.batches);
        let sink = Arc::new(MemoryShadowSink::new());
        let host = ModelHost::new(HostConfig::default(), Arc::new(provider))
            .unwrap()
            .with_shadow_sink(Arc::clone(&sink) as Arc<dyn ShadowSink>);
        (host, batches, sink)
    }

    #[test]
    fn requests_that_arrive_together_are_batched_up_to_the_adr_limit() {
        // The provider holds each batch for 10 ms, so requests pile up behind the first one and
        // the host has something to batch. 64 requests cannot fit in one batch of 32.
        let (host, batches, _) = new_host(Fake::new(true, Duration::from_millis(10)));
        let m = manifest(false);
        let model = host.load(&m, BYTES).unwrap();
        let consumer = ConsumerId::new("hk-classify/dl");
        host.set_mode(&model, &consumer, MlMode::Shadow, false)
            .unwrap();
        let host = Arc::new(host);

        let threads: Vec<_> = (0..64)
            .map(|i| {
                let host = Arc::clone(&host);
                let model = model.clone();
                thread::spawn(move || {
                    host.observe(&model, &request("fsk", i as f32), None)
                        .unwrap()
                        .map(|p| p.batch_size)
                })
            })
            .collect();
        let sizes: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert_eq!(sizes.iter().filter(|s| s.is_some()).count(), 64);

        let seen = batches.lock().unwrap().clone();
        assert!(
            seen.iter().all(|b| *b <= DEFAULT_MAX_BATCH),
            "no batch may exceed the ADR-0016 §6 limit of {DEFAULT_MAX_BATCH}: {seen:?}"
        );
        assert!(
            seen.iter().any(|b| *b > 1),
            "64 simultaneous requests behind a 10 ms model must batch: {seen:?}"
        );
        assert_eq!(seen.iter().map(|b| u64::from(*b)).sum::<u64>(), 64);
        let stats = host.stats();
        assert_eq!(stats.inferred, 64);
        assert!(stats.largest_batch > 1 && stats.largest_batch <= DEFAULT_MAX_BATCH);
        assert!(stats.mean_batch() >= 1.0);
    }

    #[test]
    fn a_lone_request_is_answered_at_the_delay_rather_than_waiting_for_a_full_batch() {
        let (host, batches, _) = new_host(Fake::new(true, Duration::ZERO));
        let m = manifest(false);
        let model = host.load(&m, BYTES).unwrap();
        host.set_mode(
            &model,
            &ConsumerId::new("hk-classify/dl"),
            MlMode::Shadow,
            false,
        )
        .unwrap();
        let started = Instant::now();
        let p = host
            .observe(&model, &request("fsk", 3.0), None)
            .unwrap()
            .expect("one request is answered on its own");
        let elapsed = started.elapsed();
        assert_eq!(p.batch_size, 1);
        assert_eq!(batches.lock().unwrap().as_slice(), [1]);
        assert!(
            elapsed < Duration::from_secs(2),
            "a lone request waited {elapsed:?}: it must flush at the {DEFAULT_MAX_DELAY:?} delay"
        );
        // The answer is this request's: the fake's first logit is the input's first value.
        assert_eq!(p.logits[0], 3.0);
    }

    /// T-283. The conformance suite reads `deadline_missed` on the instruction after `observe`
    /// returns, and under four concurrent worktrees that read came back one short: the worker had
    /// answered the consumer and had not yet taken the stats mutex. That is an ordering defect in
    /// the host, not a timing budget in the test — nothing here has a wall-clock bound, and the
    /// answer is always correct; only the counter was late.
    ///
    /// So the invariant is stated in the terms the host actually controls: after each answer the
    /// counter for *that* answer already stands. Repeating it many times turns a window of a few
    /// microseconds into a regression that shows up on the first loaded run rather than one run
    /// in ten.
    #[test]
    fn a_counter_is_published_before_the_answer_it_counts() {
        // A 1 ms delay keeps the loop quick: a lone request flushes at the delay, not at its
        // deadline. Nothing in this test asserts on elapsed time.
        let config = HostConfig {
            max_delay: Duration::from_millis(1),
            ..HostConfig::default()
        };
        let host = ModelHost::new(config, Arc::new(Fake::new(true, Duration::ZERO))).unwrap();
        let m = manifest(false);
        let model = host.load(&m, BYTES).unwrap();
        let consumer = ConsumerId::new("hk-classify/dl");
        host.set_mode(&model, &consumer, MlMode::Shadow, false)
            .unwrap();

        const ROUNDS: u64 = 200;
        for i in 1..=ROUNDS {
            let mut late = request("fsk", i as f32);
            late.deadline = Instant::now() - Duration::from_millis(1);
            assert!(host.observe(&model, &late, None).unwrap().is_none());
            let stats = host.stats();
            assert_eq!(
                stats.deadline_missed, i,
                "round {i}: the consumer has its DeadlineMissed but the counter has not caught \
                 up — the drop must be counted before the reply is sent: {stats:?}"
            );
        }
        for i in 1..=ROUNDS {
            assert!(
                host.observe(&model, &request("fsk", i as f32), None)
                    .unwrap()
                    .is_some()
            );
            let stats = host.stats();
            assert_eq!(
                stats.inferred, i,
                "round {i}: the consumer has its prediction but the counter has not caught up — \
                 the inference must be counted before the reply is sent: {stats:?}"
            );
        }
        let stats = host.stats();
        assert_eq!(stats.submitted, 2 * ROUNDS);
        assert_eq!(stats.deadline_missed, ROUNDS);
        assert_eq!(stats.inferred, ROUNDS);
        assert_eq!(stats.dropped_queue_full, 0, "{stats:?}");
        assert_eq!(stats.errors, 0, "{stats:?}");
    }

    #[test]
    fn a_full_queue_drops_and_counts_instead_of_blocking_the_pipeline() {
        let config = HostConfig {
            queue_capacity: 1,
            max_batch: 1,
            ..HostConfig::default()
        };
        // Each batch takes 150 ms, so the queue is full almost immediately.
        let host = Arc::new(
            ModelHost::new(
                config,
                Arc::new(Fake::new(true, Duration::from_millis(150))),
            )
            .unwrap(),
        );
        let m = manifest(false);
        let model = host.load(&m, BYTES).unwrap();
        host.set_mode(
            &model,
            &ConsumerId::new("hk-classify/dl"),
            MlMode::Shadow,
            false,
        )
        .unwrap();

        let threads: Vec<_> = (0..16)
            .map(|i| {
                let host = Arc::clone(&host);
                let model = model.clone();
                thread::spawn(move || host.observe(&model, &request("fsk", i as f32), None))
            })
            .collect();
        let answers: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        let dropped = answers.iter().filter(|a| matches!(a, Ok(None))).count();
        assert!(answers.iter().all(|a| a.is_ok()), "a drop is not an error");
        assert!(
            dropped > 0,
            "16 requests into a queue of 1 behind a 150 ms model must drop some"
        );
        let stats = host.stats();
        assert_eq!(stats.dropped_queue_full as usize, dropped);
        assert_eq!(stats.submitted + stats.dropped_queue_full, 16);
    }

    #[test]
    fn shadow_records_and_never_decides_while_active_needs_evidence_and_conformance() {
        let (host, _, sink) = new_host(Fake::new(true, Duration::ZERO));
        let consumer = ConsumerId::new("hk-classify/dl");

        // No enable evidence: refused, and the message says which requirement is missing.
        let no_evidence = manifest(false);
        let model = host.load(&no_evidence, BYTES).unwrap();
        let err = host
            .set_mode(&model, &consumer, MlMode::Active, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("enable evidence"), "{err}");

        // Shadow is always allowed, records, and decides nothing.
        host.set_mode(&model, &consumer, MlMode::Shadow, false)
            .unwrap();
        let req = request("fsk", 1.0);
        let p = host.observe(&model, &req, None).unwrap().unwrap();
        assert!(!p.decides(), "a shadow prediction never decides");
        assert_eq!(p.mode, MlMode::Shadow);
        assert!(
            host.decide(&model, &req).is_err(),
            "the decision path must refuse a shadow model"
        );
        assert_eq!(sink.entries().len(), 1);
        assert_eq!(sink.entries()[0].mode, "shadow");
        assert_eq!(host.stats().shadow_records, 1);

        // Off: nothing runs, and nothing is recorded.
        host.set_mode(&model, &consumer, MlMode::Off, false)
            .unwrap();
        assert!(host.observe(&model, &req, None).unwrap().is_none());
        assert_eq!(sink.entries().len(), 1);

        // Forcing active is possible, and audited.
        host.set_mode(&model, &consumer, MlMode::Active, true)
            .unwrap();
        assert!(host.modes().iter().any(|(_, _, e)| e.forced));
        let decided = host.decide(&model, &req).unwrap();
        assert!(decided.decides(), "an active prediction may decide");
        assert!(
            sink.entries().len() == 1,
            "the shadow sink records shadow runs only"
        );
    }

    #[test]
    fn an_unconformant_provider_never_reaches_active_however_good_its_manifest() {
        let (host, _, _) = new_host(Fake::new(false, Duration::ZERO));
        let m = manifest(true); // enable evidence present…
        let model = host.load(&m, BYTES).unwrap();
        let consumer = ConsumerId::new("hk-classify/dl");
        let err = host
            .set_mode(&model, &consumer, MlMode::Active, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("conformance suite"), "{err}");
        // …and with a conformant provider the same manifest is allowed.
        let (host2, _, _) = new_host(Fake::new(true, Duration::ZERO));
        let model2 = host2.load(&m, BYTES).unwrap();
        host2
            .set_mode(&model2, &consumer, MlMode::Active, false)
            .unwrap();
    }

    #[test]
    fn low_power_turns_both_modes_off() {
        let config = HostConfig {
            low_power: true,
            ..HostConfig::default()
        };
        let host = ModelHost::new(config, Arc::new(Fake::new(true, Duration::ZERO))).unwrap();
        let m = manifest(true);
        let model = host.load(&m, BYTES).unwrap();
        let consumer = ConsumerId::new("hk-classify/dl");
        host.set_mode(&model, &consumer, MlMode::Active, false)
            .unwrap();
        let req = request("fsk", 1.0);
        assert!(host.observe(&model, &req, None).unwrap().is_none());
        assert!(host.decide(&model, &req).is_err());
        assert_eq!(host.stats().submitted, 0, "an idle model costs nothing");
    }

    #[test]
    fn a_model_is_never_asked_about_a_family_it_is_not_scoped_to() {
        let (host, _, _) = new_host(Fake::new(true, Duration::ZERO));
        let m = manifest(false);
        let model = host.load(&m, BYTES).unwrap();
        host.set_mode(
            &model,
            &ConsumerId::new("hk-classify/dl"),
            MlMode::Shadow,
            false,
        )
        .unwrap();
        // The subject's family came from the classical cascade; this model is scoped to `fsk`.
        let err = host
            .observe(&model, &request("psk-qam", 1.0), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("never chooses the family"), "{err}");
    }

    #[test]
    fn loading_verifies_the_bytes_and_unloading_stops_the_worker_and_its_modes() {
        let (host, _, _) = new_host(Fake::new(true, Duration::ZERO));
        let m = manifest(false);
        // Bytes that are not the manifest's never reach the provider.
        assert!(matches!(
            host.load(&m, b"other bytes"),
            Err(MlError::HashMismatch(_))
        ));
        let model = host.load(&m, BYTES).unwrap();
        assert!(host.load(&m, BYTES).is_err(), "loading twice is refused");
        assert_eq!(host.loaded().len(), 1);
        let consumer = ConsumerId::new("hk-classify/dl");
        host.set_mode(&model, &consumer, MlMode::Shadow, false)
            .unwrap();

        host.unload(&model).unwrap();
        assert!(host.loaded().is_empty());
        assert_eq!(host.mode(&model, &consumer), MlMode::Off);
        assert!(host.modes().is_empty());
        assert!(host.unload(&model).is_err(), "unloading twice is refused");
        // Its modes went with it, so nothing runs, nothing decides, and it cannot be put back
        // into a mode without being loaded again.
        let req = request("fsk", 1.0);
        assert!(host.observe(&model, &req, None).unwrap().is_none());
        assert!(host.decide(&model, &req).is_err());
        assert!(
            host.set_mode(&model, &consumer, MlMode::Shadow, false)
                .is_err()
        );

        // Loading the same id@version again is how a rollback works: no rebuild involved.
        let again = host.load(&m, BYTES).unwrap();
        assert_eq!(again, m.model);
    }

    #[test]
    fn the_configured_limits_stay_inside_the_adr_defaults() {
        assert_eq!(DEFAULT_MAX_BATCH, 32);
        assert_eq!(DEFAULT_MAX_DELAY, Duration::from_millis(20));
        for bad in [
            HostConfig {
                max_batch: 0,
                ..HostConfig::default()
            },
            HostConfig {
                max_batch: 64,
                ..HostConfig::default()
            },
            HostConfig {
                max_delay: Duration::from_millis(50),
                ..HostConfig::default()
            },
            HostConfig {
                queue_capacity: 0,
                ..HostConfig::default()
            },
            HostConfig {
                max_in_flight: 0,
                ..HostConfig::default()
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?} must be refused");
        }
    }

    /// **ADR-0016 §7's ML exit-gate row, proved non-vacuous against a real host (T-366).**
    ///
    /// The gate clause "each `active` family has enable evidence" can only be caught at the gate
    /// if something reaches `active` without it, and exactly one path does: [`ModelHost::set_mode`]
    /// with `force`, which ADR-0016 §4.6 audits rather than refuses. So the forbidden state is
    /// *constructed here through the real API* and handed to
    /// [`crate::exit_gate::MlGateSnapshot::from_host`], which must report it — the T-287/T-297
    /// pattern: build the state the clause forbids and show the assertion catching it.
    ///
    /// The same host, with its evidence and no force, satisfies the row. Neither half is a
    /// hand-built struct: both are read out of a loaded host's own mode table.
    #[test]
    fn a_forced_active_model_without_evidence_fails_the_adr_0016_s7_exit_gate() {
        use crate::exit_gate::{GateViolation, MlAttributedRow, MlGateSnapshot};

        let (host, _, _) = new_host(Fake::new(true, Duration::ZERO));
        let consumer = ConsumerId::new("hk-classify/dl");

        // No evidence on the manifest, so `active` is refused outright...
        let model = host.load(&manifest(false), BYTES).unwrap();
        assert!(
            host.set_mode(&model, &consumer, MlMode::Active, false)
                .is_err(),
            "active without enable evidence must be refused (ADR-0016 §4.6)"
        );
        // ...and the one path that gets there anyway is audited, not blocked.
        host.set_mode(&model, &consumer, MlMode::Active, true)
            .unwrap();

        let snap = MlGateSnapshot::from_host(&host, Vec::new(), 0);
        assert_eq!(
            snap.check(),
            [GateViolation::ActiveWithoutEnableEvidence {
                model: model_key(&model),
                consumer: "hk-classify/dl".into(),
                forced: true,
            }],
            "the exit gate must catch a forced active model: {}",
            snap.summary()
        );

        // A shadow prediction the host really produced, recorded as a classification by a
        // hypothetical consumer, is caught as clause 1 — over this same live host.
        host.set_mode(&model, &consumer, MlMode::Shadow, false)
            .unwrap();
        let p = host
            .observe(&model, &request("fsk", 1.0), None)
            .unwrap()
            .expect("shadow runs the model");
        assert!(!p.decides(), "a shadow prediction never decides");
        let snap = MlGateSnapshot::from_host(
            &host,
            vec![MlAttributedRow {
                subject: "emitter-under-test".into(),
                model: Some(p.model.to_string()),
                stage: "dl".into(),
            }],
            0,
        );
        assert!(
            snap.check()
                .iter()
                .any(|v| matches!(v, GateViolation::ClassificationFromANonActiveModel { .. })),
            "a classification attributed to a shadow model must fail the gate: {:?} / {}",
            snap.check(),
            snap.summary()
        );

        // And with the evidence, unforced, the row is satisfied: the gate is about the evidence,
        // not about ML existing.
        let (host, _, _) = new_host(Fake::new(true, Duration::ZERO));
        let model = host.load(&manifest(true), BYTES).unwrap();
        host.set_mode(&model, &consumer, MlMode::Active, false)
            .unwrap();
        let snap = MlGateSnapshot::from_host(&host, Vec::new(), 0);
        assert_eq!(snap.check(), [], "{}", snap.summary());
        assert!(snap.ml_on().is_some(), "{}", snap.summary());
    }
}
