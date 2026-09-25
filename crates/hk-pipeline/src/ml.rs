//! The C38 shadow stage of a run (ADR-0016 §6, T-844): the model host wired onto the classifier's
//! call site, writing what it saw into hk-store's durable shadow log.
//!
//! ```text
//! classify chain ─▶ classify::classify_box_observed ─▶ Classification (published, unchanged)
//!                          │                                  │
//!                          └─ dl_input ─▶ MlStage::observe ────┴─▶ ModelHost::observe ─▶ StoreShadowSink
//!                                                                                      └▶ <data>/ml/shadow/
//! ```
//!
//! # What turns it on, and what cannot
//!
//! A model runs here only when **all** of these hold, and each is someone's deliberate act rather
//! than a default:
//!
//! 1. it is **installed** in the run's registry, `<data dir>/models/<id>/<version>/` — models are
//!    data an operator produces (`py/hkpy/ml/train_amc`), never shipped in the tree;
//! 2. it is a within-family class model whose consumer is [`DL_CONSUMER`], i.e. it takes
//!    `hk_classify::dl_input` — the only input this call site produces;
//! 3. an operator put its `(model, consumer)` in `shadow` (or `active`) through
//!    `PUT /api/ml/models/{id}/mode`, which the API audits; the mode is persisted in
//!    `<data dir>/ml/modes.json` and restored when the next run opens;
//! 4. the classical cascade named a family the model is scoped to, for a stored CFAR
//!    [`Detection`] ([`hk_ml::gate::admit`] — the gate is a type, not a policy).
//!
//! ADR-0016 §4.6's evidence rule governs `active` and nothing else: `shadow` is how the evidence
//! the rule asks for is *collected* (its OTA clause can only ever be measured on records like the
//! ones this stage writes), so no evidence is required to enter it, exactly as
//! [`hk_ml::ModelManifest::allows`] says. `active` needs the manifest's enable evidence **and** a
//! conformant provider, or an audited `force`; and even `active` changes nothing here —
//! [`MlStage::observe`] calls [`ModelHost::observe`] only, which returns predictions marked
//! `shadow`, and this module has no path that writes a `Classification`. The M3 exit gate
//! (`tests/e2e/tests/acceptance/m3_ml.rs`) asserts that over a run whose modes it reads from here
//! ([`MlStage::gate_snapshot`]).
//!
//! # Cost, and where it runs
//!
//! On the classify chain's own thread (`chains/classify.rs`, T-878), **after** the repository lock
//! is released (the host batches for up to 20 ms, so observing under the lock would stall every
//! other writer). One `dl_input` vector per classification, computed only when a model is in a
//! non-`off` mode for the family the cascade named ([`MlStage::wants`]); an idle stage costs a
//! mutex read. Never on the ring or a DSP thread (ADR-0007).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

/// The mode vocabulary, re-exported so the composition can name it without depending on `hk-ml`.
pub use hk_ml::MlMode;
use hk_ml::exit_gate::{MlAttributedRow, MlGateSnapshot, ModeInForce};
use hk_ml::host::{
    ConsumerId, HostConfig, InferenceRequest, ModelHost, ShadowEntry, ShadowSink, model_key,
};
use hk_ml::registry::ModelRegistry;
use hk_ml::{MlError, MlProvider, ModelManifest, ModelRef, ModelTask, Tensor};
use hk_model::classify::Classification;
use hk_model::{Detection, Timestamp};
use hk_store::ml::{
    ClassicalDecision, SHADOW_SCHEMA, ShadowPrediction, ShadowQuery, ShadowRecord, ShadowStore,
    ShadowSubject, snr_bin_db,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The consumer this call site serves: models that take `hk_classify::dl_input` and refine the
/// class within the family the classical cascade named (the manifest's `consumer`).
pub const DL_CONSUMER: &str = "hk-classify/dl";

/// How long one shadow inference may take before it is dropped and counted. Shadow decides
/// nothing, so a miss costs a record, never a decision.
pub const SHADOW_BUDGET: Duration = Duration::from_millis(250);

/// `<data dir>/models`: the run's model registry.
pub fn registry_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("models")
}

/// `<data dir>/ml/shadow`: the durable shadow log.
pub fn shadow_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("ml").join("shadow")
}

/// `<data dir>/ml/modes.json`: the `(model, consumer)` modes an operator set.
pub fn modes_path(data_dir: &Path) -> PathBuf {
    data_dir.join("ml").join("modes.json")
}

/// The [`ShadowSink`] the production host writes through: hk-store's durable log.
///
/// It needs the classical decision the record is compared against, which the host does not know;
/// [`MlStage::observe`] puts it in [`ShadowEntry::extra`], and an entry without one is refused
/// (a shadow record with nothing to compare to is not the record ADR-0016 §6 describes).
#[derive(Debug)]
pub struct StoreShadowSink(pub Arc<ShadowStore>);

impl StoreShadowSink {
    /// The record `entry` becomes.
    pub fn record_of(entry: &ShadowEntry) -> Result<ShadowRecord, MlError> {
        let classical: ClassicalDecision = entry
            .extra
            .as_ref()
            .and_then(|v| v.get("classical"))
            .cloned()
            .ok_or_else(|| {
                MlError::Invalid("a shadow record carries the classical decision".into())
            })
            .and_then(|v| {
                serde_json::from_value(v)
                    .map_err(|e| MlError::Invalid(format!("classical decision: {e}")))
            })?;
        let t = entry
            .extra
            .as_ref()
            .and_then(|v| v.get("subject_t_ns"))
            .and_then(Value::as_i64)
            .map_or(entry.t, Timestamp::from_unix_nanos);
        Ok(ShadowRecord {
            schema: SHADOW_SCHEMA,
            t,
            model: entry.model.clone(),
            consumer: entry.consumer.clone(),
            subject: ShadowSubject {
                kind: serde_json::to_value(entry.subject_kind)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_else(|| "detection".into()),
                detection: entry.detection.clone(),
            },
            snr_db: entry.snr_db,
            snr_bin_db: snr_bin_db(entry.snr_db),
            prediction: ShadowPrediction {
                label: entry.label.clone(),
                p: entry.p,
                energy: entry.energy,
                unknown_score: entry.unknown_score,
                provider: entry.provider.as_str().to_owned(),
                precision: entry.precision.as_str().to_owned(),
                latency_ms: entry.latency_ms,
                batch_size: entry.batch_size,
                mode: entry.mode.clone(),
            },
            classical,
        })
    }
}

impl ShadowSink for StoreShadowSink {
    fn record(&self, entry: &ShadowEntry) -> Result<(), MlError> {
        let rec = Self::record_of(entry)?;
        self.0
            .append(&rec)
            .map_err(|e| MlError::Invalid(format!("shadow store: {e}")))
    }
}

/// The classical decision a shadow record is compared against.
pub fn classical_decision(c: &Classification) -> ClassicalDecision {
    ClassicalDecision {
        family: c.family.clone(),
        class: c.class.as_ref().map(|cc| cc.label.clone()),
        class_p: c.class.as_ref().map(|cc| cc.p),
        confidence: c.confidence,
        open_set_score: c.open_set_score,
        stage: c.stage.as_str().to_owned(),
    }
}

/// One persisted mode (`<data dir>/ml/modes.json`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedMode {
    /// `id@version`.
    model: String,
    consumer: String,
    mode: MlMode,
    forced: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedModes {
    schema: u16,
    modes: Vec<PersistedMode>,
}

/// A refused or failed ML action, in HTTP terms (the API maps it one to one).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MlStageFailure {
    /// HTTP status.
    pub status: u16,
    /// Machine token: `invalid`, `not_found`, `needs_evidence`, `not_conformant`, `failed`.
    pub code: &'static str,
    /// Reason.
    pub message: String,
}

impl MlStageFailure {
    fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }
}

/// A mode change request (`PUT /api/ml/models/{id}/mode`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModeRequest {
    /// Registry id.
    pub id: String,
    /// Version; may be omitted when exactly one is installed.
    pub version: Option<String>,
    /// Consumer; defaults to (and must equal) the manifest's.
    pub consumer: Option<String>,
    /// The mode wanted.
    pub mode: MlMode,
    /// Override ADR-0016 §4.6's `active` requirements (recorded on the mode, and audited).
    pub force: bool,
}

/// What the producer did, for `/api/ml/models` and the run summary.
#[derive(Debug, Default)]
pub struct ProducerStats {
    /// Classifications offered to the stage with a model in a non-`off` mode for their family.
    pub offered: AtomicU64,
    /// Predictions the host returned (each one a shadow record, when the sink took it).
    pub observed: AtomicU64,
    /// Offers the CFAR gate refused (no family, a DL or track-shape input).
    pub not_admitted: AtomicU64,
    /// Offers with no measurable `dl_input` (a snippet too short or unscaled).
    pub no_input: AtomicU64,
    /// Offers whose stored detection could not be read.
    pub no_detection: AtomicU64,
    /// Inference errors the host reported.
    pub errors: AtomicU64,
}

/// The run's shadow stage: registry, hosts (one per provider a registry file can need), the
/// durable store and the persisted modes.
pub struct MlStage {
    registry: ModelRegistry,
    store: Arc<ShadowStore>,
    modes_path: PathBuf,
    /// `hk-mlp@1` files (`model.json`) run on the MLP reference; ONNX (`model.onnx`) on tract.
    mlp: ModelHost,
    onnx: ModelHost,
    /// Serialises mode changes and their persistence.
    changes: Mutex<()>,
    /// Models a restored mode could not be re-established for, with why.
    restore_errors: Mutex<Vec<(String, String)>>,
    /// What the producer did.
    pub stats: ProducerStats,
}

impl std::fmt::Debug for MlStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MlStage")
            .field("registry", &self.registry.root())
            .field("store", &self.store.root())
            .finish()
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn is_onnx(path: &Path) -> bool {
    path.file_name().is_some_and(|n| n == "model.onnx")
}

impl MlStage {
    /// Opens the stage for `data_dir`: the shadow store (created), the registry (read lazily), and
    /// every persisted non-`off` mode re-established — a model that has since vanished, fails its
    /// hash or no longer has the evidence its `active` mode needed is reported in
    /// `/api/ml/models` and left off, never silently kept.
    pub fn open(data_dir: &Path) -> anyhow::Result<Self> {
        let store = Arc::new(ShadowStore::open(shadow_dir(data_dir))?);
        let sink: Arc<dyn ShadowSink> = Arc::new(StoreShadowSink(Arc::clone(&store)));
        let host = |p: Arc<dyn MlProvider>| -> anyhow::Result<ModelHost> {
            Ok(ModelHost::new(HostConfig::default(), p)
                .map_err(|e| anyhow::anyhow!("{e}"))?
                .with_shadow_sink(Arc::clone(&sink)))
        };
        let stage = Self {
            registry: ModelRegistry::new(registry_dir(data_dir)),
            store,
            modes_path: modes_path(data_dir),
            mlp: host(Arc::new(hk_ml::mlp::MlpProvider))?,
            onnx: host(Arc::new(hk_ml::tract_provider::TractProvider))?,
            changes: Mutex::new(()),
            restore_errors: Mutex::new(Vec::new()),
            stats: ProducerStats::default(),
        };
        stage.restore();
        Ok(stage)
    }

    fn restore(&self) {
        let Ok(text) = std::fs::read_to_string(&self.modes_path) else {
            return;
        };
        let persisted: PersistedModes = match serde_json::from_str(&text) {
            Ok(p) => p,
            Err(e) => {
                lock(&self.restore_errors).push((
                    self.modes_path.display().to_string(),
                    format!("modes file does not parse: {e}"),
                ));
                return;
            }
        };
        for m in persisted.modes {
            if m.mode == MlMode::Off {
                continue;
            }
            let (id, version) = m.model.split_once('@').unwrap_or((&m.model, ""));
            let req = ModeRequest {
                id: id.to_owned(),
                version: (!version.is_empty()).then(|| version.to_owned()),
                consumer: Some(m.consumer.clone()),
                mode: m.mode,
                force: m.forced,
            };
            if let Err(f) = self.apply(&req) {
                lock(&self.restore_errors).push((m.model.clone(), f.message));
            }
        }
    }

    /// The durable shadow store.
    pub fn store(&self) -> &Arc<ShadowStore> {
        &self.store
    }

    /// The registry this stage loads from.
    pub fn registry(&self) -> &ModelRegistry {
        &self.registry
    }

    /// Both hosts, for accounting.
    pub fn hosts(&self) -> [&ModelHost; 2] {
        [&self.mlp, &self.onnx]
    }

    /// Loaded [`DL_CONSUMER`] models scoped to `family`, with the host each runs on and the mode
    /// in force.
    fn models_for(&self, family: &str) -> Vec<(&ModelHost, ModelRef, MlMode)> {
        let consumer = ConsumerId::new(DL_CONSUMER);
        let mut out = Vec::new();
        for host in self.hosts() {
            for m in host.loaded() {
                if m.consumer == DL_CONSUMER
                    && m.task == ModelTask::FamilyClass
                    && m.family.as_deref() == Some(family)
                {
                    let mode = host.mode(&m.model, &consumer);
                    if mode != MlMode::Off {
                        out.push((host, m.model.clone(), mode));
                    }
                }
            }
        }
        out
    }

    /// Whether no model is in a non-`off` mode at all — the chain then waits for nothing and
    /// computes nothing on the stage's behalf.
    pub fn is_idle(&self) -> bool {
        self.hosts()
            .iter()
            .all(|h| h.modes().iter().all(|(_, _, e)| e.mode == MlMode::Off))
    }

    /// Whether any model would run for a classification naming `family` — the check that keeps
    /// an idle stage from computing a `dl_input` nobody reads.
    pub fn wants(&self, family: &str) -> bool {
        !self.models_for(family).is_empty()
    }

    /// **The producer.** Runs every model in a non-`off` mode for the family `classical` named,
    /// over `input` (`hk_classify::dl_input` of the same normalised snippet), and records what it
    /// said through the host's durable sink. Returns how many models ran (each one's record is
    /// appended by the sink; a sink that fails is counted by the host, never fails inference).
    ///
    /// `classical` is borrowed and never returned: nothing here can change the published row.
    pub fn observe(
        &self,
        detection: Option<&Detection>,
        classical: &Classification,
        input: Option<&[f32]>,
    ) -> usize {
        let models = self.models_for(&classical.family);
        if models.is_empty() {
            return 0;
        }
        self.stats.offered.fetch_add(1, Ordering::Relaxed);
        let Some(detection) = detection else {
            self.stats.no_detection.fetch_add(1, Ordering::Relaxed);
            return 0;
        };
        let subject = match hk_ml::gate::admit(detection, classical) {
            Ok(s) => s,
            Err(_) => {
                self.stats.not_admitted.fetch_add(1, Ordering::Relaxed);
                return 0;
            }
        };
        let Some(x) = input.filter(|x| !x.is_empty()) else {
            self.stats.no_input.fetch_add(1, Ordering::Relaxed);
            return 0;
        };
        let Some(tensor) = Tensor::new(vec![x.len()], x.to_vec()) else {
            self.stats.no_input.fetch_add(1, Ordering::Relaxed);
            return 0;
        };
        // The record is keyed on the subject's capture time (the classification's `t`), not on
        // the wall clock the host stamps a prediction with: retention and the time filters of
        // `GET /api/ml/shadow` are on the same axis as every other record of the run.
        let extra = json!({
            "classical": classical_decision(classical),
            "subject_t_ns": classical.t.as_unix_nanos(),
        });
        let mut recorded = 0;
        for (host, model, _mode) in models {
            let req =
                InferenceRequest::new(subject.clone(), DL_CONSUMER, tensor.clone(), SHADOW_BUDGET);
            match host.observe(&model, &req, Some(extra.clone())) {
                Ok(Some(_)) => {
                    self.stats.observed.fetch_add(1, Ordering::Relaxed);
                    recorded += 1;
                }
                Ok(None) => {}
                Err(_) => {
                    self.stats.errors.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        recorded
    }

    fn resolve(&self, id: &str, version: Option<&str>) -> Result<ModelManifest, MlStageFailure> {
        let all = self.registry.list();
        let mut candidates: Vec<&ModelManifest> = all
            .values()
            .filter(|m| m.model.id == id && version.is_none_or(|v| m.model.version == v))
            .collect();
        match candidates.len() {
            0 => Err(MlStageFailure::new(
                404,
                "not_found",
                match version {
                    Some(v) => format!(
                        "{id}@{v} is not installed in {}",
                        self.registry.root().display()
                    ),
                    None => format!(
                        "{id} is not installed in {}",
                        self.registry.root().display()
                    ),
                },
            )),
            1 => Ok(candidates.remove(0).clone()),
            _ => Err(MlStageFailure::new(
                400,
                "invalid",
                format!(
                    "{id} has {} versions installed ({}): name one with \"version\"",
                    candidates.len(),
                    candidates
                        .iter()
                        .map(|m| m.model.version.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            )),
        }
    }

    /// Sets a `(model, consumer)` mode, loading the model out of the registry when it needs to
    /// run and unloading it when nothing uses it any more. Persists the result.
    ///
    /// `active` without `force` needs the manifest's §4.6 enable evidence (409 `needs_evidence`)
    /// and a conformant provider (409 `not_conformant`), checked here so the refusal says which.
    pub fn set_mode(&self, req: &ModeRequest) -> Result<Value, MlStageFailure> {
        let out = self.apply(req)?;
        self.persist()?;
        Ok(out)
    }

    fn apply(&self, req: &ModeRequest) -> Result<Value, MlStageFailure> {
        let _changes = lock(&self.changes);
        let manifest = self.resolve(&req.id, req.version.as_deref())?;
        let consumer = req
            .consumer
            .clone()
            .unwrap_or_else(|| manifest.consumer.clone());
        if consumer != manifest.consumer {
            return Err(MlStageFailure::new(
                400,
                "invalid",
                format!(
                    "{} was trained for consumer {:?}; this run serves it only there, not {consumer:?}",
                    model_key(&manifest.model),
                    manifest.consumer
                ),
            ));
        }
        let key = model_key(&manifest.model);
        let consumer_id = ConsumerId::new(consumer.clone());
        let stored = self
            .registry
            .read(&manifest.model.id, &manifest.model.version)
            .map_err(|e| MlStageFailure::new(409, "failed", format!("{key}: {e}")))?;
        let host = if is_onnx(&stored.path) {
            &self.onnx
        } else {
            &self.mlp
        };
        let previous = host.mode(&manifest.model, &consumer_id);
        if req.mode == MlMode::Active && !req.force {
            if !manifest.allows(MlMode::Active) {
                return Err(MlStageFailure::new(
                    409,
                    "needs_evidence",
                    format!(
                        "{key}: active needs the ADR-0016 §4.6 enable evidence on the manifest \
                         (or \"force\": true, which is audited)"
                    ),
                ));
            }
            if !host_conformant(host) {
                return Err(MlStageFailure::new(
                    409,
                    "not_conformant",
                    format!(
                        "{key}: provider {} has not passed the conformance suite, so it never \
                         decides anything (ADR-0007; or \"force\": true, which is audited)",
                        host.provider()
                    ),
                ));
            }
        }
        let loaded = host.loaded().iter().any(|m| model_key(&m.model) == key);
        if req.mode == MlMode::Off {
            if loaded {
                host.set_mode(&manifest.model, &consumer_id, MlMode::Off, false)
                    .map_err(|e| MlStageFailure::new(500, "failed", e.to_string()))?;
                let still_used = host
                    .modes()
                    .iter()
                    .any(|(k, _, e)| *k == key && e.mode != MlMode::Off);
                if !still_used {
                    host.unload(&manifest.model)
                        .map_err(|e| MlStageFailure::new(500, "failed", e.to_string()))?;
                }
            }
        } else {
            if !loaded {
                host.load(&stored.manifest, &stored.bytes).map_err(|e| {
                    MlStageFailure::new(409, "failed", format!("{key} does not load: {e}"))
                })?;
            }
            host.set_mode(&manifest.model, &consumer_id, req.mode, req.force)
                .map_err(|e| MlStageFailure::new(409, "failed", e.to_string()))?;
        }
        let forced = host
            .modes()
            .iter()
            .find(|(k, c, _)| *k == key && *c == consumer_id)
            .is_some_and(|(_, _, e)| e.forced);
        Ok(json!({
            "model": manifest.model.to_string(),
            "key": key,
            "consumer": consumer,
            "mode": req.mode.as_str(),
            "previous": previous.as_str(),
            "forced": forced,
            "provider": host.provider().as_str(),
        }))
    }

    fn persist(&self) -> Result<(), MlStageFailure> {
        let mut modes = Vec::new();
        for host in self.hosts() {
            for (model, consumer, e) in host.modes() {
                if e.mode != MlMode::Off {
                    modes.push(PersistedMode {
                        model,
                        consumer: consumer.to_string(),
                        mode: e.mode,
                        forced: e.forced,
                    });
                }
            }
        }
        let doc = PersistedModes { schema: 1, modes };
        let fail = |e: std::io::Error| {
            MlStageFailure::new(500, "failed", format!("persisting ML modes: {e}"))
        };
        if let Some(dir) = self.modes_path.parent() {
            std::fs::create_dir_all(dir).map_err(fail)?;
        }
        let tmp = self.modes_path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&doc).unwrap_or_default()).map_err(fail)?;
        std::fs::rename(&tmp, &self.modes_path).map_err(fail)
    }

    /// `GET /api/ml/models`: every installed model, what is loaded where, the modes in force, the
    /// hosts' counters and the producer's.
    pub fn models_json(&self) -> Value {
        let installed = self.registry.list();
        let mut loaded_on: BTreeMap<String, &ModelHost> = BTreeMap::new();
        for host in self.hosts() {
            for m in host.loaded() {
                loaded_on.insert(model_key(&m.model), host);
            }
        }
        let models: Vec<Value> = installed
            .iter()
            .map(|(key, m)| {
                let host = loaded_on.get(key).copied();
                let modes: Vec<Value> = host
                    .map(|h| {
                        h.modes()
                            .into_iter()
                            .filter(|(k, _, _)| k == key)
                            .map(|(_, c, e)| {
                                json!({"consumer": c.as_str(), "mode": e.mode.as_str(), "forced": e.forced})
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let format = self
                    .registry
                    .read(&m.model.id, &m.model.version)
                    .map(|s| if is_onnx(&s.path) { "onnx" } else { hk_ml::mlp::MLP_FORMAT })
                    .unwrap_or("unreadable");
                json!({
                    "id": m.model.id,
                    "version": m.model.version,
                    "ref": m.model.to_string(),
                    "task": m.task,
                    "consumer": m.consumer,
                    "family": m.family,
                    "labels": m.labels,
                    "precision": m.precision.as_str(),
                    "format": format,
                    "metrics_ref": m.metrics_ref,
                    "enable_evidence": m.enable_evidence,
                    "loaded": host.is_some(),
                    "provider": host.map(|h| h.provider().as_str()),
                    "conformant": host.map(host_conformant),
                    "modes": modes,
                })
            })
            .collect();
        let hosts: Vec<Value> = self
            .hosts()
            .iter()
            .map(|h| {
                let s = h.stats();
                json!({
                    "provider": h.provider().as_str(),
                    "conformant": host_conformant(h),
                    "loaded": h.loaded().len(),
                    "stats": {
                        "submitted": s.submitted,
                        "inferred": s.inferred,
                        "dropped_queue_full": s.dropped_queue_full,
                        "deadline_missed": s.deadline_missed,
                        "batches": s.batches,
                        "mean_batch": s.mean_batch(),
                        "largest_batch": s.largest_batch,
                        "errors": s.errors,
                        "shadow_records": s.shadow_records,
                        "mean_latency_ms": s.mean_latency_ms(),
                        "latency_ms_max": s.latency_ms_max,
                    },
                })
            })
            .collect();
        let p = &self.stats;
        let g = |a: &AtomicU64| a.load(Ordering::Relaxed);
        json!({
            "registry": self.registry.root().display().to_string(),
            "models": models,
            "restore_errors": lock(&self.restore_errors)
                .iter()
                .map(|(m, e)| json!({"model": m, "error": e}))
                .collect::<Vec<_>>(),
            "hosts": hosts,
            "producer": {
                "consumer": DL_CONSUMER,
                "input": format!("dl_input@{}", hk_classify::dl::DL_INPUT_VERSION),
                "offered": g(&p.offered),
                "observed": g(&p.observed),
                "not_admitted": g(&p.not_admitted),
                "no_input": g(&p.no_input),
                "no_detection": g(&p.no_detection),
                "errors": g(&p.errors),
            },
        })
    }

    /// `GET /api/ml/shadow`: matching records newest first, their per-SNR agreement aggregates,
    /// and what the store holds.
    pub fn shadow_json(&self, q: &ShadowQuery) -> Result<Value, MlStageFailure> {
        let fail = |e: std::io::Error| {
            MlStageFailure::new(500, "failed", format!("reading the shadow store: {e}"))
        };
        let records: Vec<Value> = self
            .store
            .query(q)
            .map_err(fail)?
            .into_iter()
            .map(|r| {
                let mut v = serde_json::to_value(&r).unwrap_or_default();
                // The API's time law (docs/api.md): seconds under a bare name, nanoseconds only
                // under `_ns` — so the stored `t` (ns) is served as `t_s` and `t_ns`.
                if let Some(o) = v.as_object_mut() {
                    o.remove("t");
                    o.insert("t_ns".into(), json!(r.t.as_unix_nanos()));
                    o.insert("t_s".into(), json!(r.t.as_unix_nanos() as f64 / 1e9));
                    o.insert("agrees".into(), json!(r.agrees()));
                }
                v
            })
            .collect();
        let aggregates = self.store.aggregates(q).map_err(fail)?;
        let stats = self.store.stats();
        Ok(json!({
            "records": records,
            "aggregates": aggregates,
            "store": {
                "segments": stats.segments,
                "bytes": stats.bytes,
                "records": stats.records,
                "corrupt_lines": stats.corrupt_lines,
                "segments_deleted": stats.segments_deleted,
                "oldest_s": stats.oldest.map(|t| t.as_unix_nanos() as f64 / 1e9),
                "newest_s": stats.newest.map(|t| t.as_unix_nanos() as f64 / 1e9),
                "max_bytes": stats.max_bytes,
                "max_age_s": stats.max_age_s,
            },
        }))
    }

    /// ADR-0016 §7's ML exit-gate snapshot for this run: the modes in force on both hosts (with
    /// the evidence on each manifest) and the shadow records they wrote, plus the rows and lost
    /// samples the caller measured.
    pub fn gate_snapshot(&self, rows: Vec<MlAttributedRow>, lost_samples: u64) -> MlGateSnapshot {
        let mut modes: Vec<ModeInForce> = Vec::new();
        let mut shadow_records = 0;
        for host in self.hosts() {
            let snap = MlGateSnapshot::from_host(host, Vec::new(), 0);
            modes.extend(snap.modes);
            shadow_records += snap.shadow_records;
        }
        MlGateSnapshot {
            modes,
            rows,
            shadow_records,
            lost_samples,
        }
    }
}

/// Installs an **untrained probe model** for `family` into the registry under `data_dir` and
/// returns its reference: a fixed-weight `hk-mlp@1` network over [`hk_classify::dl::DL_INPUT_DIM`]
/// inputs with the given within-family `labels`, consumer [`DL_CONSUMER`], and no metrics or
/// enable evidence.
///
/// It exists to exercise this stage's **plumbing** — registry, mode, producer, durable sink,
/// routes — end to end, in tests and as an operator smoke check. Its outputs are deterministic
/// and meaningless: it is not a classifier, it can only ever be put in `shadow` (no evidence, and
/// the MLP provider is never conformant), and its id (`probe-<family>`) says so on every record.
pub fn install_probe_model(
    data_dir: &Path,
    family: &str,
    labels: &[&str],
) -> Result<ModelRef, MlError> {
    let dim = hk_classify::dl::DL_INPUT_DIM;
    let k = labels.len();
    // A deterministic LCG in (-0.5, 0.5): fixed weights, so every run of a test sees the same
    // numbers, and nothing about them is learned.
    let mut state: u32 = 0x5eed_0844;
    let mut next = || {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (state >> 8) as f32 / (1u32 << 24) as f32 - 0.5
    };
    let weight: Vec<f32> = (0..dim * k).map(|_| next() * 0.2).collect();
    let weights = json!({
        "format": hk_ml::mlp::MLP_FORMAT,
        "input_dim": dim,
        "mean": vec![0.0_f32; dim],
        "scale": vec![1.0_f32; dim],
        "layers": [{
            "in_dim": dim,
            "out_dim": k,
            "weight": weight,
            "bias": vec![0.0_f32; k],
            "activation": "identity",
        }],
        "trained_on": "untrained probe (hk_pipeline::ml::install_probe_model)",
    });
    let bytes = serde_json::to_vec(&weights)
        .map_err(|e| MlError::Invalid(format!("probe weights do not serialise: {e}")))?;
    let sha256 = hk_ml::registry::sha256_hex(&bytes);
    let manifest = ModelManifest {
        schema: hk_ml::ML_SCHEMA,
        model: ModelRef {
            id: format!("probe-{family}"),
            version: "0.0.1".into(),
            sha8: sha256[..8].to_owned(),
        },
        sha256,
        task: ModelTask::FamilyClass,
        consumer: DL_CONSUMER.into(),
        taxonomy: Some(hk_model::classify::TaxonomyRef::current()),
        family: Some(family.to_owned()),
        labels: labels.iter().map(|l| (*l).to_owned()).collect(),
        open_set: hk_ml::OpenSetSpec {
            method: hk_ml::OpenSetMethod::Energy,
            temperature: 1.0,
            threshold: -5.0,
            calibrated_on: "uncalibrated probe".into(),
        },
        precision: hk_ml::Precision::Fp32,
        metrics_ref: None,
        enable_evidence: None,
    };
    ModelRegistry::new(registry_dir(data_dir)).install(&manifest, &bytes)?;
    Ok(manifest.model)
}

fn host_conformant(host: &ModelHost) -> bool {
    match host.provider() {
        hk_ml::MlProviderKind::CpuMlp => hk_ml::mlp::MlpProvider.conformant(),
        hk_ml::MlProviderKind::CpuTract => hk_ml::tract_provider::TractProvider.conformant(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hk_ml::exit_gate::GateViolation;
    use hk_model::classify::{
        CLASSIFICATION_SCHEMA, ClassCall, ClassProvenance, Coarse, HK_MOD_V1, LabelP, Stage,
        SuspectFlags, TaxonomyRef, UNKNOWN, entropy_norm,
    };
    use hk_model::{DetectionFlags, DetectionId, ProvenanceId, SurveyId, TimeRange};

    const FSK: [&str; 4] = ["2fsk", "gfsk", "msk", "4fsk"];
    /// 2026-09-13T12:00:20Z — a capture time well away from the wall clock.
    const T_NS: i64 = 1_789_300_820_000_000_000;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            static N: AtomicU64 = AtomicU64::new(0);
            let p = std::env::temp_dir().join(format!(
                "hk-pipeline-ml-{tag}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn detection() -> Detection {
        Detection {
            id: DetectionId::new(),
            survey_id: SurveyId::new(),
            time: TimeRange::new(
                Timestamp::from_unix_nanos(T_NS),
                Timestamp::from_unix_nanos(T_NS + 1_000_000_000),
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
        }
    }

    fn classification(family: &str) -> Classification {
        let posterior = vec![
            LabelP {
                label: family.to_owned(),
                p: 0.8,
            },
            LabelP {
                label: UNKNOWN.to_owned(),
                p: 0.2,
            },
        ];
        Classification {
            schema: CLASSIFICATION_SCHEMA,
            t: Timestamp::from_unix_nanos(T_NS),
            taxonomy: TaxonomyRef::current(),
            input: None,
            coarse: Coarse::Digital,
            entropy_norm: entropy_norm(&posterior, HK_MOD_V1.families.len() + 1),
            likelihood: posterior.clone(),
            posterior,
            prior: None,
            family: family.to_owned(),
            confidence: 0.8,
            class: Some(ClassCall {
                label: "2fsk".into(),
                p: 0.7,
                dist: Vec::new(),
                stage: Stage::FeatureTree,
            }),
            open_set_score: 0.2,
            stage: Stage::FeatureTree,
            provenance: ClassProvenance {
                rules: "hk-classify/tree@1".into(),
                features_version: hk_classify::FEATURES_VERSION,
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
        }
    }

    fn input() -> Vec<f32> {
        (0..hk_classify::dl::DL_INPUT_DIM)
            .map(|i| (i as f32 * 0.37).sin())
            .collect()
    }

    fn shadow(id: &str) -> ModeRequest {
        ModeRequest {
            id: id.into(),
            mode: MlMode::Shadow,
            ..ModeRequest::default()
        }
    }

    /// **The sink half of T-844's non-vacuity pair.** A prediction the producer makes is on disk
    /// in hk-store's shadow log, readable by a store opened afresh — not in a buffer that dies
    /// with the stage. Swap [`StoreShadowSink`] for `hk_ml::host::MemoryShadowSink` in
    /// [`MlStage::open`] and this fails; the pipeline's wiring is not involved (that half is
    /// `tests/ml_shadow.rs`).
    #[test]
    fn a_shadow_prediction_is_durable_in_hk_store_and_survives_the_stage() {
        let dir = TempDir::new("durable");
        let probe = install_probe_model(&dir.0, "fsk", &FSK).unwrap();
        let det = detection();
        let c = classification("fsk");
        {
            let stage = MlStage::open(&dir.0).unwrap();
            assert!(
                stage.is_idle(),
                "installed is not on: nothing runs until a mode is set"
            );
            assert!(!stage.wants("fsk"));
            stage.set_mode(&shadow(&probe.id)).unwrap();
            assert!(stage.wants("fsk"));
            assert_eq!(stage.observe(Some(&det), &c, Some(&input())), 1);
            assert_eq!(stage.stats.observed.load(Ordering::Relaxed), 1);
        }

        let store = ShadowStore::open(shadow_dir(&dir.0)).unwrap();
        let recs = store.query(&ShadowQuery::default()).unwrap();
        assert_eq!(recs.len(), 1, "the record is in hk-store, not in memory");
        let r = &recs[0];
        assert!(r.model.starts_with("probe-fsk@0.0.1#"), "{}", r.model);
        assert_eq!(r.consumer, DL_CONSUMER);
        assert_eq!(r.prediction.mode, "shadow");
        assert!(FSK.contains(&r.prediction.label.as_str()));
        assert_eq!(r.subject.detection, det.id.to_string());
        assert_eq!(r.classical.family, "fsk");
        assert_eq!(r.classical.class.as_deref(), Some("2fsk"));
        assert_eq!(r.classical.stage, "feature-tree");
        assert_eq!((r.snr_db, r.snr_bin_db), (Some(24.0), Some(20)));
        assert_eq!(
            r.t.as_unix_nanos(),
            T_NS,
            "keyed on the subject's capture time, not the wall clock"
        );

        // The mode was persisted: a new stage over the same data directory runs the model again
        // without anyone re-issuing the PUT, and serves the record it finds.
        let stage = MlStage::open(&dir.0).unwrap();
        assert!(stage.wants("fsk"));
        let v = stage.shadow_json(&ShadowQuery::default()).unwrap();
        assert_eq!(v["records"].as_array().unwrap().len(), 1);
        assert_eq!(v["aggregates"][0]["n"], 1);
        assert_eq!(v["aggregates"][0]["family"], "fsk");
        let m = stage.models_json();
        assert_eq!(m["models"][0]["modes"][0]["mode"], "shadow");
        assert_eq!(m["models"][0]["loaded"], true);
    }

    /// Shadow runs within the family the classical cascade named, on an admitted CFAR subject,
    /// and nowhere else — and an idle stage computes nothing.
    #[test]
    fn a_model_runs_only_inside_the_family_the_cascade_named() {
        let dir = TempDir::new("scope");
        let probe = install_probe_model(&dir.0, "fsk", &FSK).unwrap();
        let stage = MlStage::open(&dir.0).unwrap();
        let det = detection();
        assert_eq!(
            stage.observe(Some(&det), &classification("fsk"), Some(&input())),
            0
        );
        assert_eq!(
            stage.stats.offered.load(Ordering::Relaxed),
            0,
            "idle: nothing offered"
        );

        stage.set_mode(&shadow(&probe.id)).unwrap();
        assert!(!stage.wants("analog"));
        assert!(!stage.wants(UNKNOWN), "a model never chooses the family");
        assert_eq!(
            stage.observe(Some(&det), &classification("analog"), Some(&input())),
            0
        );
        assert_eq!(
            stage.observe(Some(&det), &classification(UNKNOWN), Some(&input())),
            0
        );
        // A classification from a learned stage is never a model's input.
        let mut from_dl = classification("fsk");
        from_dl.stage = Stage::Dl;
        assert_eq!(stage.observe(Some(&det), &from_dl, Some(&input())), 0);
        assert_eq!(stage.stats.not_admitted.load(Ordering::Relaxed), 1);
        assert_eq!(
            stage.observe(None, &classification("fsk"), Some(&input())),
            0
        );
        assert_eq!(stage.stats.no_detection.load(Ordering::Relaxed), 1);
        assert_eq!(stage.observe(Some(&det), &classification("fsk"), None), 0);
        assert_eq!(stage.stats.no_input.load(Ordering::Relaxed), 1);
        assert_eq!(stage.store().stats().records, 0);
    }

    /// `active` needs ADR-0016 §4.6's evidence **and** a conformant provider; `force` gets past
    /// both, is recorded on the mode, and is exactly the state §7's exit gate reports.
    #[test]
    fn active_is_refused_without_evidence_and_a_forced_active_fails_the_exit_gate() {
        let dir = TempDir::new("active");
        let probe = install_probe_model(&dir.0, "fsk", &FSK).unwrap();
        let stage = MlStage::open(&dir.0).unwrap();
        let active = ModeRequest {
            mode: MlMode::Active,
            ..shadow(&probe.id)
        };
        let e = stage.set_mode(&active).unwrap_err();
        assert_eq!((e.status, e.code), (409, "needs_evidence"), "{}", e.message);
        assert!(stage.is_idle(), "a refused change leaves nothing on");

        let e = stage
            .set_mode(&ModeRequest {
                id: "no-such-model".into(),
                ..shadow("x")
            })
            .unwrap_err();
        assert_eq!((e.status, e.code), (404, "not_found"));
        let e = stage
            .set_mode(&ModeRequest {
                consumer: Some("someone-else".into()),
                ..shadow(&probe.id)
            })
            .unwrap_err();
        assert_eq!((e.status, e.code), (400, "invalid"));

        let v = stage
            .set_mode(&ModeRequest {
                force: true,
                ..active
            })
            .unwrap();
        assert_eq!(
            (v["mode"].as_str(), v["forced"].as_bool()),
            (Some("active"), Some(true))
        );
        let snap = stage.gate_snapshot(Vec::new(), 0);
        assert!(
            snap.check()
                .iter()
                .any(|v| matches!(v, GateViolation::ActiveWithoutEnableEvidence { .. })),
            "{}",
            snap.summary()
        );

        // Off unloads it and the persisted table no longer names it.
        stage
            .set_mode(&ModeRequest {
                mode: MlMode::Off,
                ..shadow(&probe.id)
            })
            .unwrap();
        assert!(stage.is_idle());
        assert!(MlStage::open(&dir.0).unwrap().is_idle());
    }
}
