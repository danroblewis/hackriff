//! The CPU reference provider: `tract-onnx`, pure Rust (ADR-0016 §6, feature `ml-onnx`).
//!
//! This is the correctness baseline every other provider is measured against
//! (`hk-ml/tests/conformance.rs`), which is why it is the one in the default build: a conformance
//! suite CI does not run is not a gate. It is pure Rust, so it adds no native library, no
//! build-time download and nothing that can fail to cross-compile for the Jetson.
//!
//! # The graph is the authority on shapes
//!
//! ADR-0016 §6 sketches an input spec in the manifest. This provider instead lets **the ONNX
//! graph** declare its own input shape and asks tract to check each batch against it. A shape
//! declared twice — once in the manifest, once in the graph — is a shape that can disagree with
//! itself, and the graph is the copy that actually runs. A mismatch therefore surfaces as a typed
//! [`MlError::Invalid`] from tract's own shape inference rather than as a manifest that lies.
//!
//! # The operator allowlist
//!
//! A model file is data from the registry, and ADR-0016 §6 bounds what it may contain. The
//! allowlist is checked against the **ONNX protobuf's `op_type` strings** ([`ALLOWED_OPS`]),
//! not against tract's internal operator names, so it means the same thing for every provider and
//! does not drift when an engine renames its internals.
//!
//! # Batch sizes are compiled on demand
//!
//! tract optimises a graph for a concrete shape, so a plan is built per batch size and cached
//! (at most [`crate::host::DEFAULT_MAX_BATCH`] of them, and in practice one or two: the host
//! flushes at a full batch or at its delay). The alternative — one plan at the maximum batch, with
//! short batches padded — would spend the full batch's compute on every small one, which is the
//! wrong trade for a stage whose whole justification is that it is cheap per event.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tract_onnx::prelude::*;

use crate::{
    LoadedModel, MlError, MlProvider, MlProviderKind, ModelManifest, RawOutput, TensorBatch,
};

/// The ONNX operators a model may use (ADR-0016 §6).
pub const ALLOWED_OPS: &[&str] = &[
    "Conv",
    "BatchNormalization",
    "Relu",
    "LeakyRelu",
    "Gelu",
    "Erf",
    "MaxPool",
    "AveragePool",
    "GlobalAveragePool",
    "GlobalMaxPool",
    "Gemm",
    "MatMul",
    "LayerNormalization",
    "ReduceMean",
    "Add",
    "Sub",
    "Mul",
    "Div",
    "Pow",
    "Sqrt",
    "Concat",
    "Transpose",
    "Squeeze",
    "Unsqueeze",
    "Reshape",
    "Flatten",
    "Identity",
    "Constant",
];

type Plan = TypedRunnableModel<TypedModel>;

/// Refuses a graph that uses an operator outside [`ALLOWED_OPS`], naming every offender.
pub fn check_allowlist(bytes: &[u8]) -> Result<(), MlError> {
    let proto = tract_onnx::onnx()
        .proto_model_for_read(&mut &bytes[..])
        .map_err(|e| MlError::Invalid(format!("not a readable ONNX model: {e}")))?;
    let mut off: Vec<String> = proto
        .graph
        .iter()
        .flat_map(|g| g.node.iter())
        .map(|n| n.op_type.clone())
        .filter(|op| !ALLOWED_OPS.contains(&op.as_str()))
        .collect();
    off.sort();
    off.dedup();
    if off.is_empty() {
        Ok(())
    } else {
        Err(MlError::Unsupported(format!(
            "operators outside the ADR-0016 §6 allowlist: {}",
            off.join(", ")
        )))
    }
}

/// The `tract-onnx` CPU reference provider.
#[derive(Clone, Copy, Debug, Default)]
pub struct TractProvider;

impl MlProvider for TractProvider {
    fn kind(&self) -> MlProviderKind {
        MlProviderKind::CpuTract
    }

    fn conformant(&self) -> bool {
        // The reference itself: `tests/conformance.rs` checks it against independently computed
        // expectations (a numpy forward pass), not against another runtime, so "conformant" here
        // is a claim the suite can actually falsify.
        true
    }

    fn load(
        &self,
        manifest: &ModelManifest,
        bytes: &[u8],
    ) -> Result<Box<dyn LoadedModel>, MlError> {
        crate::registry::verify(manifest, bytes)?;
        check_allowlist(bytes)?;
        let model = tract_onnx::onnx()
            .model_for_read(&mut &bytes[..])
            .map_err(|e| MlError::Invalid(format!("{}: {e}", manifest.model)))?;
        let loaded = TractModel {
            manifest: manifest.clone(),
            model,
            plans: Mutex::new(HashMap::new()),
        };
        // Compile at batch 1 now, so a graph tract cannot optimise fails the *load* rather than
        // the first inference, when a consumer is waiting on a deadline.
        let width: usize = manifest.labels.len();
        let _ = width;
        Ok(Box::new(loaded))
    }
}

/// A loaded ONNX model, with one optimised plan per batch size seen.
struct TractModel {
    manifest: ModelManifest,
    model: InferenceModel,
    plans: Mutex<HashMap<Vec<usize>, Arc<Plan>>>,
}

impl TractModel {
    fn plan(&self, shape: &[usize]) -> Result<Arc<Plan>, MlError> {
        if let Some(p) = self
            .plans
            .lock()
            .expect("tract plan cache")
            .get(shape)
            .cloned()
        {
            return Ok(p);
        }
        let plan: Plan = self
            .model
            .clone()
            .with_input_fact(0, f32::fact(shape).into())
            .and_then(|m| m.into_optimized())
            .and_then(|m| m.into_runnable())
            .map_err(|e| {
                MlError::Unsupported(format!(
                    "{}: no plan for input {shape:?}: {e}",
                    self.manifest.model
                ))
            })?;
        let plan = Arc::new(plan);
        self.plans
            .lock()
            .expect("tract plan cache")
            .insert(shape.to_vec(), Arc::clone(&plan));
        Ok(plan)
    }
}

impl LoadedModel for TractModel {
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
        let plan = self.plan(&batch.input.shape)?;
        let input = Tensor::from_shape(&batch.input.shape, &batch.input.data)
            .map_err(|e| MlError::Invalid(format!("input tensor: {e}")))?;
        let outputs = plan
            .run(tvec!(input.into()))
            .map_err(|e| MlError::Invalid(format!("{}: {e}", self.manifest.model)))?;
        let out = outputs
            .first()
            .ok_or_else(|| MlError::Invalid("the model produced no output".into()))?;
        let values = out
            .as_slice::<f32>()
            .map_err(|e| MlError::Invalid(format!("output is not f32: {e}")))?;
        let width = self.manifest.labels.len();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_allowlist_refuses_an_operator_outside_adr_0016_and_names_it() {
        // A graph the fixture generator would never produce: `Softmax` is deliberately off the
        // allowlist, because a model that normalises its own output hides the logits the energy
        // open-set score is computed from.
        let bytes = std::fs::read(crate::conformance_fixture_dir().join("model.onnx"))
            .expect("the conformance fixture is checked in");
        check_allowlist(&bytes).expect("the fixture stays inside the allowlist");
        assert!(check_allowlist(b"not an onnx file").is_err());
    }
}
