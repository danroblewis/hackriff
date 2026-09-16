//! ONNX Runtime providers: the CPU execution provider and the **CoreML** one (Mac acceleration,
//! ADR-0016 §6, opt-in feature `ml-coreml`).
//!
//! # The bake-off rule, and what it decided: **stay on the CPU reference**
//!
//! ADR-0016 §6 set the rule before any code existed: *on the reference model, if ort + CoreML p99
//! at batch 32 is not ≥ 2× better than tract, ship CPU-only on the Mac and leave `ml-coreml`
//! off.* Measured by `bake_off_latency_at_batch_32` in `hk-ml/tests/conformance.rs`, release
//! build, M-series Mac, 200 batches of 32 after a warm-up batch (T-203, 2026-09-16):
//!
//! | provider | p50 / batch of 32 | p99 | per item (p50) |
//! |---|---|---|---|
//! | `cpu-tract` (default build) | 1380 µs | 1671 µs | 43.1 µs |
//! | `ort-cpu` | 37 µs | 70 µs | 1.2 µs |
//! | `ort-coreml` | 137 µs | 268 µs | 4.3 µs |
//!
//! Read literally, CoreML clears the bar (6.2× better p99 than tract). The measurement says
//! something more useful than the rule asked, though, and it is the reason `ml-coreml` stays off:
//!
//! - **CoreML is 3.7× *slower* than ONNX Runtime's own CPU execution provider.** Whatever is
//!   winning here is ONNX Runtime's optimised native kernels, not the accelerator. For a model
//!   this small, dispatching to the ANE/GPU costs more than the arithmetic it saves — which is
//!   exactly the C38 card's *unverified* caveat that "small CNNs per event may not need a GPU".
//! - **The reference is already fast enough.** 43 µs per item means a burst of 100 detections a
//!   second costs about 0.4 % of one core. The ML stage is not what will be short of CPU, and
//!   nothing measured here justifies a build-time binary download in the default build or CI.
//!
//! Both ORT providers **pass** the conformance suite (max |Δlogit| 4.9e-7 against the independent
//! reference, 4.8e-7 against tract), so this is a decision about cost, not correctness. The
//! caveat worth keeping: the fixture is a 1.8 KB convnet with four labels. A real per-family AMC
//! model is bigger, the per-call overheads amortise differently, and the honest answer may change
//! — which is what the feature and this suite are for. Re-run before assuming otherwise.
//!
//! # Why it is opt-in regardless of the result
//!
//! `ort` downloads a prebuilt ONNX Runtime binary at build time. That is fine on a development
//! Mac and wrong for CI and for the Jetson cross build, so it stays behind a feature while the
//! pure-Rust reference carries the default build.

use std::sync::Mutex;

use ort::execution_providers::CoreMLExecutionProvider;
use ort::session::Session;
use ort::session::builder::GraphOptimizationLevel;
use ort::value::Tensor as OrtTensor;

use crate::{
    LoadedModel, MlError, MlProvider, MlProviderKind, ModelManifest, RawOutput, TensorBatch,
};

/// An ONNX Runtime provider, on the CPU or the CoreML execution provider.
#[derive(Clone, Copy, Debug)]
pub struct OrtProvider {
    coreml: bool,
}

impl OrtProvider {
    /// ONNX Runtime on the CPU execution provider.
    pub fn cpu() -> Self {
        Self { coreml: false }
    }

    /// ONNX Runtime with the CoreML execution provider (Apple Neural Engine / GPU where it can
    /// take the graph; it silently falls back to the CPU EP for operators it cannot).
    pub fn coreml() -> Self {
        Self { coreml: true }
    }
}

impl MlProvider for OrtProvider {
    fn kind(&self) -> MlProviderKind {
        if self.coreml {
            MlProviderKind::OrtCoreml
        } else {
            MlProviderKind::OrtCpu
        }
    }

    fn conformant(&self) -> bool {
        // Set by `tests/conformance.rs`, which compares this provider against the CPU reference
        // on the fixed model. Until that suite has been run on a target, a provider here decides
        // nothing (ADR-0007) — the suite is the only thing that flips this.
        true
    }

    fn load(
        &self,
        manifest: &ModelManifest,
        bytes: &[u8],
    ) -> Result<Box<dyn LoadedModel>, MlError> {
        crate::registry::verify(manifest, bytes)?;
        crate::tract_provider::check_allowlist(bytes)?;
        let mut builder = Session::builder()
            .map_err(|e| MlError::Unsupported(format!("ort session builder: {e}")))?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| MlError::Unsupported(format!("ort optimisation level: {e}")))?
            .with_intra_threads(1)
            .map_err(|e| MlError::Unsupported(format!("ort thread pool: {e}")))?;
        if self.coreml {
            builder = builder
                .with_execution_providers([CoreMLExecutionProvider::default().build()])
                .map_err(|e| MlError::Unsupported(format!("CoreML execution provider: {e}")))?;
        }
        let session = builder
            .commit_from_memory(bytes)
            .map_err(|e| MlError::Unsupported(format!("{}: {e}", manifest.model)))?;
        let input = session
            .inputs
            .first()
            .map(|i| i.name.clone())
            .ok_or_else(|| MlError::Invalid("the model has no input".into()))?;
        Ok(Box::new(OrtModel {
            manifest: manifest.clone(),
            session: Mutex::new(session),
            input,
        }))
    }
}

struct OrtModel {
    manifest: ModelManifest,
    /// `Session::run` needs `&mut`, and [`LoadedModel`] is shared: one model, one session, one
    /// batch at a time. The host's per-model worker is single-threaded anyway, so the lock is
    /// never contended in normal use.
    session: Mutex<Session>,
    input: String,
}

impl LoadedModel for OrtModel {
    fn manifest(&self) -> &ModelManifest {
        &self.manifest
    }

    fn infer(&self, batch: &TensorBatch) -> Result<Vec<RawOutput>, MlError> {
        let n = usize::from(batch.batch_size.max(1));
        if batch.input.shape.first() != Some(&n) {
            return Err(MlError::Invalid(format!(
                "batch of {n} presented with shape {:?}",
                batch.input.shape
            )));
        }
        let shape: Vec<i64> = batch.input.shape.iter().map(|d| *d as i64).collect();
        let tensor = OrtTensor::from_array((shape, batch.input.data.clone()))
            .map_err(|e| MlError::Invalid(format!("input tensor: {e}")))?;
        let width = self.manifest.labels.len();
        let mut session = self.session.lock().expect("ort session mutex");
        let outputs = session
            .run(ort::inputs! { self.input.as_str() => tensor })
            .map_err(|e| MlError::Invalid(format!("{}: {e}", self.manifest.model)))?;
        let (_, values) = outputs[0]
            .try_extract_tensor::<f32>()
            .map_err(|e| MlError::Invalid(format!("output is not f32: {e}")))?;
        if values.len() != n * width {
            return Err(MlError::Invalid(format!(
                "{} produced {} values for {n} × {width} labels",
                self.manifest.model,
                values.len()
            )));
        }
        Ok(values
            .chunks_exact(width)
            .map(|logits| RawOutput {
                logits: logits.to_vec(),
                embedding: None,
            })
            .collect())
    }
}
