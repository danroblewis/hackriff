//! C38 ML-runtime contract (ADR-0016 §6). **Core interface**, and a **stub**: this crate fixes
//! the shapes every consumer codes against ([`Prediction`], [`MlProvider`], [`ModelManifest`]) and
//! ships the CPU reference *identity* only. T-203 lands ONNX loading (tract), batching, the
//! registry, shadow mode and the conformance suite; T-216 the Jetson TensorRT provider.
//!
//! # What the contract guarantees before any model exists
//!
//! - **Open set, never a softmax.** A [`Prediction`] carries an energy score and a calibrated
//!   [`Prediction::unknown_score`]; `probs` is temperature-calibrated and its maximum is **not**
//!   an unknown detector (ADR-0016 §6, C15/C38 cards). [`Prediction::validate`] refuses a
//!   prediction whose `unknown_score` is outside 0–1 or whose label and logit counts disagree.
//! - **Within-family class only.** A model never chooses the family (ADR-0016 §4.6): the
//!   [`ModelTask::FamilyClass`] manifest names the family it is scoped to.
//! - **Shadow never decides.** [`MlMode::Shadow`] predictions are recorded and compared; they
//!   write no `Classification` row and change no decision. The mode lives on the prediction so a
//!   consumer cannot lose it.
//! - **Models are data.** A manifest plus an ONNX file in the registry, loaded at runtime:
//!   swapping or rolling back a model never rebuilds the pipeline (ADR-0001's hard requirement).
//! - **Nothing runs unless it is conformant.** [`MlProvider::conformant`] is the ADR-0007 rule:
//!   a provider that has not passed the conformance suite is never used for a decision.
//!
//! # What is deliberately absent
//!
//! T-203 landed the host ([`host`]), the registry ([`registry`]), the CFAR gate ([`gate`]) and
//! the CPU reference engine ([`tract_provider`], pure Rust, in the default build). Still absent,
//! deliberately:
//!
//! - **CUDA / TensorRT** (`ml-trt`): deferred with the Jetson phase (T-216). GPU work is Mac-first.
//! - **Mac acceleration is opt-in** (`ort_provider`, `ml-coreml`): `ort` downloads a binary at
//!   build time, and the bake-off (ADR-0016 §6) found no reason to pay for it — see that module.
//! - **FP16/INT8 conformance is unmeasured.** There is no quantised model to measure it on, so a
//!   provider loading one is not conformant for that precision until `tests/conformance.rs` has a
//!   fixture for it.
//! - **No route into a decision.** Nothing here writes a `Classification`. The only path that
//!   returns a prediction a consumer may act on is [`host::ModelHost::decide`], and it needs an
//!   `active` mode, which needs enable evidence *and* a conformant provider.
//!
//! # This crate has no production caller, and that is the correct state (T-363)
//!
//! [`host::ModelHost`] is **reachable only from this crate's own tests**, the way `hk-gnss` is
//! (T-274). `hk-classify` depends on this crate — T-204's per-family DL stage is written against
//! [`LoadedModel`] and [`predict::Calibrator`] — but nothing anywhere constructs a [`host::ModelHost`],
//! so [`host::HostStats`] has no production reader either. That was noticed in passing by T-283
//! while it fixed the counters' happens-before ordering, and T-363 was funded to decide whether it
//! is an oversight. **It is not.** Three independent things have to change before wiring this host
//! into the pipeline would be anything but a caller with nothing to call.
//!
//! 1. **There is no model to host.** [`registry::ModelRegistry`] is rooted at the *user's* data
//!    directory and has no built-ins: models are data, produced by an operator running
//!    `py/hkpy/ml/train_amc`, never shipped in the tree. The only model file in this repository is
//!    `tests/data/conformance/model.onnx`, a KB-sized fixture whose labels exist to compare
//!    providers against each other and are not `hk-mod@1` classes. A host wired into
//!    `hk-pipeline` today would resolve an empty registry on every run and every test — a
//!    *vacuous* caller, which is worse than none, because it looks wired.
//! 2. **No family earns a stage, and T-204 measured that rather than assuming it.** ADR-0016 §4.6
//!    puts each family in `off` / `shadow` / `active`, and `active` needs the enable evidence on
//!    the manifest. T-204 trained the grid and found large sim-to-sim class-accuracy gains
//!    (analog +0.455…+0.540, psk-qam +0.653…+0.669, fsk +0.280…+0.291) sitting on top of an open
//!    set that is *worse* than classical where it matters: on `fsk`, AUROC 0.326 against 0.824 and
//!    a false-known rate of 1.000 against 0.325. That fails §4.6's "AUROC not lower by > 0.02" and
//!    "false-known ≤ classical" outright, with no OTA labels behind the gains, so the stage landed
//!    shadow-only and nothing was enabled.
//!    That is belt-and-braces with the API's own shape: the only trained evaluator that exists is
//!    [`MlProviderKind::CpuMlp`], which never reports itself [`MlProvider::conformant`], and
//!    [`host::ModelHost::set_mode`] refuses `active` without a conformant provider (ADR-0007). So
//!    [`host::ModelHost::decide`] is unreachable for it by construction, not only by policy.
//! 3. **The shadow path's own consumer was never built.** ADR-0016 §6 puts shadow records in
//!    hk-store (`ml/shadow/`, hourly CRC-line NDJSON with per-SNR agreement aggregates) and §9
//!    serves them at `GET /api/ml/shadow`, alongside `GET /api/ml/models` and
//!    `PUT /api/ml/models/{id}/mode` — the operator surface that would read [`host::HostStats`].
//!    §10 assigns all of it to T-203; none of it landed. [`host::MemoryShadowSink`] is the only
//!    [`host::ShadowSink`] in the tree, and it is in-memory. Wiring [`host::ModelHost::observe`]
//!    into the pipeline now would record into a buffer nobody drains.
//!
//! **What would change this.** Wiring becomes correct when a model is installed in a registry
//! (1), a family's dev evaluation clears ADR-0016 §4.6 for at least `shadow` (2), and a durable
//! [`host::ShadowSink`] backed by hk-store exists to receive the records (3). (1) and (3) are
//! ordinary work; (2) is an evidence question that T-204 answered "no" on the evidence available
//! then, and only new evidence — not a new opinion — reopens it. `active` additionally requires
//! the §4.6 enable evidence file and a conformant provider, and ADR-0016 §4.5/§7 bound what an ML
//! stage may influence even then: within-family class only, never the family, and never the
//! published row while it is in shadow.
//!
//! **What stops this rotting while it waits.** Two things, and they are deliberately different
//! kinds. The *code* is guarded by `tests/conformance.rs`, which drives a real ONNX model through
//! the real [`host::ModelHost`] on the real [`tract_provider`] — load, `set_mode`, `observe`,
//! shadow record, a refused `decide`, deadline accounting, `unload` — in the **default** build, so
//! CI runs it; the host is unwired, not untested. The *claim above* is guarded by
//! `tests/no_production_caller.rs`, which fails the day a caller appears, so whoever wires it is
//! told to come back here and delete this section rather than leaving it to mislead the next
//! reader. That same tripwire is the premise of ADR-0016 §7's ML exit-gate row as
//! `tests/e2e/tests/acceptance/m3_ml.rs` measures it (T-366): the gate reports an empty
//! `(model, consumer)` mode table because nothing here is constructed, so wiring the host means
//! giving that gate a real enumeration ([`exit_gate::MlGateSnapshot::from_host`]) in the same
//! change.

#![deny(missing_docs)]

pub mod exit_gate;
pub mod gate;
pub mod host;
#[cfg(feature = "ml-mlp")]
pub mod mlp;
#[cfg(feature = "ml-coreml")]
pub mod ort_provider;
pub mod predict;
pub mod registry;
#[cfg(feature = "ml-onnx")]
pub mod tract_provider;

/// Where the checked-in conformance fixture lives (`model.onnx`, `manifest.json`, `cases.json`),
/// regenerated by `py/hkpy/ml/make_conformance_model.py`. Every provider is judged on it.
pub fn conformance_fixture_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/conformance")
}

use std::fmt;
use std::sync::Arc;

use hk_model::Timestamp;
use serde::{Deserialize, Serialize};

/// Schema version of [`Prediction`] and [`ModelManifest`].
pub const ML_SCHEMA: u16 = 1;

/// Which runtime executed a model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MlProviderKind {
    /// Pure-Rust CPU reference (tract), the correctness baseline every other provider is
    /// compared against.
    CpuTract,
    /// The dependency-free reference MLP ([`mlp`], feature `ml-mlp`, T-204). Not an ONNX runtime:
    /// it reads a `hk-mlp@1` weights file, and it never reports itself conformant, so it can only
    /// ever run a shadow stage.
    CpuMlp,
    /// ONNX Runtime, CPU execution provider.
    OrtCpu,
    /// ONNX Runtime, CoreML execution provider (Mac, opt-in feature `ml-coreml`).
    OrtCoreml,
    /// ONNX Runtime, TensorRT execution provider (Jetson, deferred with T-216).
    OrtTrt,
}

impl MlProviderKind {
    /// The serde/label string.
    pub const fn as_str(self) -> &'static str {
        match self {
            MlProviderKind::CpuTract => "cpu-tract",
            MlProviderKind::CpuMlp => "cpu-mlp",
            MlProviderKind::OrtCpu => "ort-cpu",
            MlProviderKind::OrtCoreml => "ort-coreml",
            MlProviderKind::OrtTrt => "ort-trt",
        }
    }
}

impl fmt::Display for MlProviderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Numeric precision a model ran at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Precision {
    /// 32-bit float (the reference precision).
    Fp32,
    /// 16-bit float.
    Fp16,
    /// 8-bit integer (quantised).
    Int8,
}

impl Precision {
    /// The serde/label string.
    pub const fn as_str(self) -> &'static str {
        match self {
            Precision::Fp32 => "fp32",
            Precision::Fp16 => "fp16",
            Precision::Int8 => "int8",
        }
    }
}

/// How a model's output is used for one consumer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MlMode {
    /// Not loaded, not run.
    #[default]
    Off,
    /// Run and recorded, but it decides nothing (ADR-0016 §6).
    Shadow,
    /// Its output is used, and needs enable evidence (ADR-0016 §4.6).
    Active,
}

impl MlMode {
    /// The serde/label string.
    pub const fn as_str(self) -> &'static str {
        match self {
            MlMode::Off => "off",
            MlMode::Shadow => "shadow",
            MlMode::Active => "active",
        }
    }

    /// Whether a prediction in this mode may influence a decision. Shadow never does.
    pub const fn decides(self) -> bool {
        matches!(self, MlMode::Active)
    }
}

/// What a model is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ModelTask {
    /// Within-family class only: it never chooses the family (ADR-0016 §4.6).
    FamilyClass,
    /// An embedding for clustering or retrieval.
    Embedding,
    /// A detector.
    Detector,
    /// An anomaly score.
    Anomaly,
}

/// A model reference: `id@version#sha8`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRef {
    /// Registry id, e.g. `amc-psk-qam`.
    pub id: String,
    /// Semver version; **immutable once saved**.
    pub version: String,
    /// First 8 hex characters of the ONNX file's sha256.
    pub sha8: String,
}

impl fmt::Display for ModelRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}#{}", self.id, self.version, self.sha8)
    }
}

impl ModelRef {
    /// The reference as [`hk_model::classify::ModelRef`] stores it on a Classification.
    pub fn to_provenance(
        &self,
        provider: MlProviderKind,
        precision: Precision,
    ) -> hk_model::classify::ModelRef {
        hk_model::classify::ModelRef {
            id: self.to_string(),
            provider: provider.as_str().to_owned(),
            precision: precision.as_str().to_owned(),
        }
    }
}

/// The open-set method a model was calibrated with. Energy only: softmax maximum is never an
/// unknown detector (ADR-0016 §6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OpenSetMethod {
    /// `E = −T·logsumexp(z/T)`, thresholded at 95 % TPR on dev in-distribution data.
    Energy,
}

/// The open-set calibration of a model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenSetSpec {
    /// Method (energy only).
    pub method: OpenSetMethod,
    /// Temperature `T`.
    pub temperature: f32,
    /// Energy threshold at the operating point.
    pub threshold: f32,
    /// What it was calibrated on, e.g. `dev@amc-grid-1`.
    pub calibrated_on: String,
}

/// A model's manifest: the data that makes a model loadable without a rebuild (ADR-0016 §6).
/// T-203 fills in the registry that reads and writes these; the shape is fixed here.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelManifest {
    /// Schema version, [`ML_SCHEMA`].
    pub schema: u16,
    /// The model this manifest describes.
    pub model: ModelRef,
    /// Full sha256 of the ONNX file (the load refuses a mismatch).
    pub sha256: String,
    /// What it is for.
    pub task: ModelTask,
    /// The consumer it was trained for, e.g. `hk-classify/dl`.
    pub consumer: String,
    /// Taxonomy its labels belong to, e.g. `hk-mod@1`.
    pub taxonomy: Option<hk_model::classify::TaxonomyRef>,
    /// The family a [`ModelTask::FamilyClass`] model is scoped to (it never chooses the family).
    pub family: Option<String>,
    /// Output labels, in logit order.
    pub labels: Vec<String>,
    /// Open-set calibration.
    pub open_set: OpenSetSpec,
    /// Precision the file is stored at.
    pub precision: Precision,
    /// Reference to the per-SNR metrics report (ADR-0016 §7).
    pub metrics_ref: Option<String>,
    /// Reference to the §4.6 enable evidence. Without it, `active` is refused unless forced, and
    /// a force is audited.
    pub enable_evidence: Option<String>,
}

impl ModelManifest {
    /// Structural validation (no file is read here).
    pub fn validate(&self) -> Result<(), MlError> {
        if self.schema != ML_SCHEMA {
            return Err(MlError::Invalid(format!(
                "manifest schema {} is not {ML_SCHEMA}",
                self.schema
            )));
        }
        for (what, s) in [
            ("model id", &self.model.id),
            ("model version", &self.model.version),
            ("sha8", &self.model.sha8),
            ("sha256", &self.sha256),
            ("consumer", &self.consumer),
        ] {
            if s.trim().is_empty() {
                return Err(MlError::Invalid(format!("{what} is empty")));
            }
        }
        if self.sha256.len() != 64 || !self.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(MlError::Invalid("sha256 is not 64 hex characters".into()));
        }
        if !self.sha256.starts_with(&self.model.sha8) {
            return Err(MlError::Invalid("sha8 does not prefix sha256".into()));
        }
        if self.labels.is_empty() || self.labels.iter().any(|l| l.trim().is_empty()) {
            return Err(MlError::Invalid("labels are required and non-empty".into()));
        }
        if self.task == ModelTask::FamilyClass && self.family.is_none() {
            return Err(MlError::Invalid(
                "a family-class model names the family it is scoped to (it never chooses one)"
                    .into(),
            ));
        }
        if !self.open_set.temperature.is_finite() || self.open_set.temperature <= 0.0 {
            return Err(MlError::Invalid("open-set temperature must be > 0".into()));
        }
        if !self.open_set.threshold.is_finite() {
            return Err(MlError::Invalid("open-set threshold must be finite".into()));
        }
        Ok(())
    }

    /// Whether `mode` may be set for this model without `force` (ADR-0016 §4.6: `active` needs
    /// the enable evidence; `shadow` and `off` never do).
    pub fn allows(&self, mode: MlMode) -> bool {
        mode != MlMode::Active || self.enable_evidence.is_some()
    }
}

/// One inference input: interleaved samples with their shape. T-203 fixes the canonical IQ layout
/// (`iq-2xN`, N 1024 / 4096) in the manifest; the batch carries whatever that manifest declares.
#[derive(Clone, Debug, PartialEq)]
pub struct Tensor {
    /// Dimensions, outermost first (the first is the batch when batched).
    pub shape: Vec<usize>,
    /// Row-major values.
    pub data: Vec<f32>,
}

impl Tensor {
    /// A tensor of `shape` over `data`; `None` when they disagree.
    pub fn new(shape: Vec<usize>, data: Vec<f32>) -> Option<Self> {
        (shape.iter().product::<usize>() == data.len()).then_some(Self { shape, data })
    }
}

/// A batch of inputs presented to a loaded model.
#[derive(Clone, Debug, PartialEq)]
pub struct TensorBatch {
    /// The batched input.
    pub input: Tensor,
    /// How many items the batch holds.
    pub batch_size: u16,
}

/// Raw per-item model output, before calibration into a [`Prediction`].
#[derive(Clone, Debug, PartialEq)]
pub struct RawOutput {
    /// Logits, in the manifest's label order.
    pub logits: Vec<f32>,
    /// Optional embedding.
    pub embedding: Option<Vec<f32>>,
}

/// A calibrated prediction (ADR-0016 §6).
#[derive(Clone, Debug, PartialEq)]
pub struct Prediction {
    /// The model that produced it.
    pub model: ModelRef,
    /// Runtime it ran on.
    pub provider: MlProviderKind,
    /// Precision it ran at.
    pub precision: Precision,
    /// Labels, in logit order (shared: the manifest owns them).
    pub labels: Arc<[String]>,
    /// Raw logits.
    pub logits: Vec<f32>,
    /// Temperature-calibrated probabilities over `labels`. **Not** an open-set score.
    pub probs: Vec<f32>,
    /// Energy `E = −T·logsumexp(z/T)`.
    pub energy: f32,
    /// Calibrated open-set score, 0–1: higher = less like anything the model was trained on.
    pub unknown_score: f32,
    /// Optional embedding.
    pub embedding: Option<Vec<f32>>,
    /// Whether this prediction may decide anything.
    pub mode: MlMode,
    /// Wall-clock inference latency, ms.
    pub latency_ms: f32,
    /// Batch it ran in.
    pub batch_size: u16,
    /// When it was produced.
    pub t: Timestamp,
}

impl Prediction {
    /// The highest-probability label and its probability. This is a **class** call, never an
    /// unknown detector: read [`Prediction::unknown_score`] for that.
    pub fn top(&self) -> Option<(&str, f32)> {
        let (i, p) = self
            .probs
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.total_cmp(b))?;
        Some((self.labels.get(i)?.as_str(), *p))
    }

    /// Whether this prediction is allowed to influence a decision (ADR-0016 §6: shadow never is).
    pub fn decides(&self) -> bool {
        self.mode.decides()
    }

    /// Checks the invariants a consumer relies on: matching label/logit/probability counts, finite
    /// values, probabilities summing to 1, and an `unknown_score` in 0–1.
    pub fn validate(&self) -> Result<(), MlError> {
        if self.labels.len() != self.logits.len() || self.labels.len() != self.probs.len() {
            return Err(MlError::Invalid(
                "labels, logits and probs must have the same length".into(),
            ));
        }
        if self.labels.is_empty() {
            return Err(MlError::Invalid("a prediction has no labels".into()));
        }
        if self
            .logits
            .iter()
            .chain(&self.probs)
            .any(|v| !v.is_finite())
            || !self.energy.is_finite()
        {
            return Err(MlError::Invalid(
                "non-finite logits, probs or energy".into(),
            ));
        }
        if self.probs.iter().any(|p| !(0.0..=1.0).contains(p)) {
            return Err(MlError::Invalid("a probability is outside 0..=1".into()));
        }
        let sum: f32 = self.probs.iter().sum();
        if (sum - 1.0).abs() > 1e-3 {
            return Err(MlError::Invalid(format!("probs sum to {sum}")));
        }
        if !(0.0..=1.0).contains(&self.unknown_score) || !self.unknown_score.is_finite() {
            return Err(MlError::Invalid(
                "unknown_score is outside 0..=1 (it is calibrated, not a softmax maximum)".into(),
            ));
        }
        if self.batch_size == 0 {
            return Err(MlError::Invalid("batch_size is 0".into()));
        }
        Ok(())
    }
}

/// Why an ML call failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MlError {
    /// A manifest, prediction or request broke its contract.
    #[error("invalid: {0}")]
    Invalid(String),
    /// The model file does not match its manifest's hash.
    #[error("model hash mismatch: {0}")]
    HashMismatch(String),
    /// The graph uses an operator or a shape the provider does not support.
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// The provider is present but the feature it needs is not built in.
    #[error("provider {0} is not built into this binary")]
    ProviderUnavailable(MlProviderKind),
    /// The request missed its deadline and was dropped (counted; the consumer falls back to the
    /// classical stage).
    #[error("deadline missed")]
    DeadlineMissed,
    /// The stub: this crate fixes the contract; T-203 implements it.
    #[error("not implemented yet: {0}")]
    NotImplemented(&'static str),
}

/// A loaded model, ready for inference. One worker thread per loaded model (T-203); never a ring
/// or DSP thread.
pub trait LoadedModel: Send + Sync {
    /// The manifest it was loaded from.
    fn manifest(&self) -> &ModelManifest;

    /// Runs one batch. The outputs are in batch order, one per item.
    fn infer(&self, batch: &TensorBatch) -> Result<Vec<RawOutput>, MlError>;
}

/// A runtime that can load and execute ONNX models.
pub trait MlProvider: Send + Sync {
    /// Which runtime this is.
    fn kind(&self) -> MlProviderKind;

    /// Whether it has passed the conformance suite (ADR-0007's rule: an unconformant provider is
    /// never used for a decision).
    fn conformant(&self) -> bool;

    /// Loads `bytes` (an ONNX graph) under `manifest`.
    fn load(&self, manifest: &ModelManifest, bytes: &[u8])
    -> Result<Box<dyn LoadedModel>, MlError>;
}

/// The CPU reference provider (ADR-0016 §6: pure-Rust tract, the baseline every other provider is
/// compared against).
///
/// With the default `ml-onnx` feature this **is** [`tract_provider::TractProvider`], and the name
/// is kept so consumers written against the T-218 contract need no change. Without it — a build
/// that deliberately carries no engine — it keeps the stub's behaviour: not conformant, and
/// [`MlError::ProviderUnavailable`] to every load, so a consumer falls back to the classical
/// stage, which is what it must do whenever a provider is unavailable anyway.
#[derive(Clone, Copy, Debug, Default)]
pub struct CpuReference;

impl MlProvider for CpuReference {
    fn kind(&self) -> MlProviderKind {
        MlProviderKind::CpuTract
    }

    fn conformant(&self) -> bool {
        cfg!(feature = "ml-onnx")
    }

    fn load(
        &self,
        manifest: &ModelManifest,
        bytes: &[u8],
    ) -> Result<Box<dyn LoadedModel>, MlError> {
        #[cfg(feature = "ml-onnx")]
        {
            tract_provider::TractProvider.load(manifest, bytes)
        }
        #[cfg(not(feature = "ml-onnx"))]
        {
            let _ = (manifest, bytes);
            Err(MlError::ProviderUnavailable(MlProviderKind::CpuTract))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> ModelManifest {
        ModelManifest {
            schema: ML_SCHEMA,
            model: ModelRef {
                id: "amc-psk-qam".into(),
                version: "0.2.0".into(),
                sha8: "1a2b3c4d".into(),
            },
            sha256: format!("1a2b3c4d{}", "0".repeat(56)),
            task: ModelTask::FamilyClass,
            consumer: "hk-classify/dl".into(),
            taxonomy: Some(hk_model::classify::TaxonomyRef::current()),
            family: Some("psk-qam".into()),
            labels: vec!["bpsk".into(), "qpsk".into()],
            open_set: OpenSetSpec {
                method: OpenSetMethod::Energy,
                temperature: 1.0,
                threshold: -5.0,
                calibrated_on: "dev@amc-grid-1".into(),
            },
            precision: Precision::Fp32,
            metrics_ref: None,
            enable_evidence: None,
        }
    }

    fn prediction(mode: MlMode) -> Prediction {
        Prediction {
            model: manifest().model,
            provider: MlProviderKind::CpuTract,
            precision: Precision::Fp32,
            labels: Arc::from(vec!["bpsk".to_owned(), "qpsk".to_owned()]),
            logits: vec![2.0, 0.5],
            probs: vec![0.8, 0.2],
            energy: -3.5,
            unknown_score: 0.2,
            embedding: None,
            mode,
            latency_ms: 1.5,
            batch_size: 32,
            t: Timestamp::from_unix_nanos(1_789_300_820_000_000_000),
        }
    }

    #[test]
    fn a_manifest_round_trips_and_its_invariants_are_checked() {
        let m = manifest();
        m.validate().unwrap();
        let json = serde_json::to_string(&m).unwrap();
        let back: ModelManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, m);
        assert_eq!(m.model.to_string(), "amc-psk-qam@0.2.0#1a2b3c4d");

        type Break = fn(&mut ModelManifest);
        let cases: &[(&str, Break)] = &[
            ("schema", |m| m.schema = 2),
            ("sha", |m| m.sha256 = "nothex".into()),
            ("sha8 prefix", |m| m.model.sha8 = "deadbeef".into()),
            ("labels", |m| m.labels.clear()),
            ("family-class without a family", |m| m.family = None),
            ("temperature", |m| m.open_set.temperature = 0.0),
            ("empty consumer", |m| m.consumer = " ".into()),
        ];
        for (name, f) in cases {
            let mut m = manifest();
            f(&mut m);
            assert!(m.validate().is_err(), "{name} should be refused");
        }
    }

    #[test]
    fn active_needs_enable_evidence_but_shadow_and_off_do_not() {
        let mut m = manifest();
        assert!(!m.allows(MlMode::Active), "no evidence, no active stage");
        assert!(m.allows(MlMode::Shadow) && m.allows(MlMode::Off));
        m.enable_evidence = Some("dev/amc-eval@1".into());
        assert!(m.allows(MlMode::Active));
    }

    #[test]
    fn a_shadow_prediction_never_decides_and_validation_catches_bad_outputs() {
        let shadow = prediction(MlMode::Shadow);
        shadow.validate().unwrap();
        assert!(!shadow.decides(), "shadow mode never decides (ADR-0016 §6)");
        assert!(prediction(MlMode::Active).decides());
        assert!(!prediction(MlMode::Off).decides());
        assert_eq!(shadow.top(), Some(("bpsk", 0.8)));

        type Break = fn(&mut Prediction);
        let cases: &[(&str, Break)] = &[
            ("length", |p| p.probs.push(0.0)),
            ("sum", |p| p.probs = vec![0.5, 0.2]),
            ("range", |p| p.probs = vec![1.5, -0.5]),
            ("unknown score", |p| p.unknown_score = 1.5),
            ("non-finite", |p| p.logits[0] = f32::NAN),
            ("batch", |p| p.batch_size = 0),
        ];
        for (name, f) in cases {
            let mut p = prediction(MlMode::Active);
            f(&mut p);
            assert!(p.validate().is_err(), "{name} should be refused");
        }
    }

    #[test]
    fn the_cpu_reference_reports_itself_and_never_fabricates_an_answer() {
        let p = CpuReference;
        assert_eq!(p.kind(), MlProviderKind::CpuTract);
        assert_eq!(
            p.conformant(),
            cfg!(feature = "ml-onnx"),
            "conformance follows the engine: without one there is nothing that could have \
             passed the suite (ADR-0007)"
        );
        // Either way, a manifest whose bytes are not the ones it names is refused rather than
        // loaded: an engine that is present must not run bytes whose provenance would be a lie,
        // and one that is absent must say so.
        match p.load(&manifest(), &[]) {
            // An engine is present, so it refuses bytes that are not the manifest's…
            Err(MlError::HashMismatch(_)) => assert!(p.conformant()),
            // …and without one it says so, rather than answering anyway.
            Err(MlError::ProviderUnavailable(k)) => {
                assert!(!p.conformant());
                assert_eq!(k, MlProviderKind::CpuTract);
            }
            Err(e) => panic!("the reference provider must not pretend: {e}"),
            Ok(_) => panic!("bytes that are not the manifest's must never load"),
        }
    }

    #[test]
    fn a_tensor_checks_its_own_shape() {
        assert!(Tensor::new(vec![2, 3], vec![0.0; 6]).is_some());
        assert!(Tensor::new(vec![2, 3], vec![0.0; 5]).is_none());
    }

    #[test]
    fn a_model_ref_becomes_classification_provenance() {
        let r = manifest()
            .model
            .to_provenance(MlProviderKind::OrtCoreml, Precision::Fp16);
        assert_eq!(r.id, "amc-psk-qam@0.2.0#1a2b3c4d");
        assert_eq!(r.provider, "ort-coreml");
        assert_eq!(r.precision, "fp16");
    }
}
