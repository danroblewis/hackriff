//! `hk-mlp@1`: a dependency-free reference model format and evaluator (T-204, feature `ml-mlp`).
//!
//! # Why this exists, and what it is not
//!
//! ADR-0016 §6 makes models **data**: a manifest plus a graph file, loaded at runtime. The engine
//! it names is ONNX (tract on the CPU, ort elsewhere), and **T-203 owns that** — the bake-off, the
//! registry, batching and the conformance suite. None of it exists yet, so a per-family DL stage
//! had nothing to run on.
//!
//! This module is the smallest thing that unblocks the stage without pre-empting that decision: a
//! JSON weights file for a standardised multi-layer perceptron, and about a hundred lines of
//! matrix arithmetic behind the same [`MlProvider`]/[`LoadedModel`] traits an ONNX provider will
//! implement. It adds **no inference runtime and no new dependency** to the default build (sha2,
//! already in the workspace, only under this feature).
//!
//! It is deliberately **not conformant**: [`MlpProvider::conformant`] returns `false`, for ever.
//! ADR-0007's rule is that an unconformant provider never decides anything, so this model can
//! drive a shadow stage and *structurally cannot* be promoted to an active one — the conformance
//! suite that would allow that is T-203's, against an ONNX engine. When T-203 lands, a model
//! trained here is retrained or exported to ONNX and this format can go away; nothing else in the
//! cascade needs to change, because consumers only ever see [`crate::Prediction`].

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    LoadedModel, MlError, MlProvider, MlProviderKind, ModelManifest, RawOutput, TensorBatch,
};

/// Format string of the weights file.
pub const MLP_FORMAT: &str = "hk-mlp@1";

/// A layer's non-linearity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Activation {
    /// `max(0, x)`.
    Relu,
    /// No non-linearity (the output layer produces logits, never probabilities: calibration is
    /// [`crate::predict`]'s job, and the open set is never a softmax).
    Identity,
}

/// One fully connected layer, `y = act(W·x + b)`, `W` row-major `[out_dim][in_dim]`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Layer {
    /// Input width.
    pub in_dim: usize,
    /// Output width.
    pub out_dim: usize,
    /// Weights, row-major, `out_dim × in_dim` values.
    pub weight: Vec<f32>,
    /// Bias, `out_dim` values.
    pub bias: Vec<f32>,
    /// Non-linearity applied to the layer's output.
    pub activation: Activation,
}

/// A standardised MLP: `x → (x − mean)/scale → layers → logits`.
///
/// The standardisation is part of the file rather than of the caller, so an input vector cannot be
/// normalised one way at training time and another way at inference time — the sim-to-real failure
/// mode that makes a classifier confidently wrong.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MlpWeights {
    /// [`MLP_FORMAT`].
    pub format: String,
    /// Input width.
    pub input_dim: usize,
    /// Per-feature mean subtracted before the first layer.
    pub mean: Vec<f32>,
    /// Per-feature scale divided out after the mean (never zero).
    pub scale: Vec<f32>,
    /// Layers, in order. The last one is the logit layer.
    pub layers: Vec<Layer>,
    /// How the file was produced (provenance for the report: generator, seeds, grid).
    pub trained_on: String,
}

impl MlpWeights {
    /// Structural validation: format, widths, finiteness, positive scales.
    pub fn validate(&self) -> Result<(), MlError> {
        if self.format != MLP_FORMAT {
            return Err(MlError::Unsupported(format!(
                "weights format {:?} is not {MLP_FORMAT}",
                self.format
            )));
        }
        if self.input_dim == 0 || self.layers.is_empty() {
            return Err(MlError::Invalid("an empty network".into()));
        }
        if self.mean.len() != self.input_dim || self.scale.len() != self.input_dim {
            return Err(MlError::Invalid(
                "mean/scale width disagrees with input_dim".into(),
            ));
        }
        if self.mean.iter().any(|v| !v.is_finite()) {
            return Err(MlError::Invalid("non-finite standardisation mean".into()));
        }
        if self.scale.iter().any(|v| !v.is_finite() || *v <= 0.0) {
            return Err(MlError::Invalid(
                "a standardisation scale is zero or non-finite".into(),
            ));
        }
        let mut width = self.input_dim;
        for (i, l) in self.layers.iter().enumerate() {
            if l.in_dim != width {
                return Err(MlError::Invalid(format!(
                    "layer {i} takes {} inputs, previous width is {width}",
                    l.in_dim
                )));
            }
            if l.out_dim == 0 || l.weight.len() != l.in_dim * l.out_dim || l.bias.len() != l.out_dim
            {
                return Err(MlError::Invalid(format!(
                    "layer {i} has inconsistent shapes"
                )));
            }
            if l.weight.iter().chain(&l.bias).any(|v| !v.is_finite()) {
                return Err(MlError::Invalid(format!(
                    "layer {i} has non-finite weights"
                )));
            }
            width = l.out_dim;
        }
        if self.layers.last().map(|l| l.activation) != Some(Activation::Identity) {
            return Err(MlError::Invalid(
                "the output layer must be linear: it produces logits, not probabilities".into(),
            ));
        }
        Ok(())
    }

    /// Logit width (the last layer's output).
    pub fn output_dim(&self) -> usize {
        self.layers.last().map_or(0, |l| l.out_dim)
    }

    /// Runs one input vector, returning the logits and the penultimate activations (the embedding).
    pub fn forward(&self, x: &[f32]) -> Result<(Vec<f32>, Vec<f32>), MlError> {
        if x.len() != self.input_dim {
            return Err(MlError::Invalid(format!(
                "input width {} is not {}",
                x.len(),
                self.input_dim
            )));
        }
        let mut v: Vec<f32> = x
            .iter()
            .zip(&self.mean)
            .zip(&self.scale)
            .map(|((x, m), s)| (x - m) / s)
            .collect();
        let mut embedding = Vec::new();
        for (i, l) in self.layers.iter().enumerate() {
            if i + 1 == self.layers.len() {
                embedding = v.clone();
            }
            let mut out = Vec::with_capacity(l.out_dim);
            for o in 0..l.out_dim {
                let row = &l.weight[o * l.in_dim..(o + 1) * l.in_dim];
                let mut acc = f64::from(l.bias[o]);
                for (w, x) in row.iter().zip(&v) {
                    acc += f64::from(*w) * f64::from(*x);
                }
                let y = acc as f32;
                out.push(match l.activation {
                    Activation::Relu => y.max(0.0),
                    Activation::Identity => y,
                });
            }
            v = out;
        }
        Ok((v, embedding))
    }
}

/// A loaded [`MlpWeights`] with the manifest it was loaded under.
#[derive(Clone, Debug)]
pub struct MlpModel {
    manifest: ModelManifest,
    weights: MlpWeights,
}

impl MlpModel {
    /// The input width this model expects.
    pub fn input_dim(&self) -> usize {
        self.weights.input_dim
    }

    /// The weights (for a caller reporting provenance).
    pub fn weights(&self) -> &MlpWeights {
        &self.weights
    }
}

impl LoadedModel for MlpModel {
    fn manifest(&self) -> &ModelManifest {
        &self.manifest
    }

    fn infer(&self, batch: &TensorBatch) -> Result<Vec<RawOutput>, MlError> {
        let n = usize::from(batch.batch_size.max(1));
        let width = self.weights.input_dim;
        if batch.input.data.len() != n * width {
            return Err(MlError::Invalid(format!(
                "batch of {n} × {width} needs {} values, got {}",
                n * width,
                batch.input.data.len()
            )));
        }
        (0..n)
            .map(|i| {
                let (logits, embedding) = self
                    .weights
                    .forward(&batch.input.data[i * width..(i + 1) * width])?;
                Ok(RawOutput {
                    logits,
                    embedding: Some(embedding),
                })
            })
            .collect()
    }
}

/// The `hk-mlp@1` provider.
///
/// **Never conformant.** ADR-0007's rule then keeps it out of every decision path: it can run a
/// shadow stage, and cannot be promoted to an active one until T-203's conformance suite exists to
/// judge a real ONNX engine.
#[derive(Clone, Copy, Debug, Default)]
pub struct MlpProvider;

impl MlProvider for MlpProvider {
    fn kind(&self) -> MlProviderKind {
        MlProviderKind::CpuMlp
    }

    fn conformant(&self) -> bool {
        false
    }

    fn load(
        &self,
        manifest: &ModelManifest,
        bytes: &[u8],
    ) -> Result<Box<dyn LoadedModel>, MlError> {
        manifest.validate()?;
        let digest = hex(&Sha256::digest(bytes));
        if !digest.eq_ignore_ascii_case(&manifest.sha256) {
            return Err(MlError::HashMismatch(format!(
                "file sha256 {digest} is not the manifest's {}",
                manifest.sha256
            )));
        }
        let weights: MlpWeights = serde_json::from_slice(bytes)
            .map_err(|e| MlError::Invalid(format!("weights file does not parse: {e}")))?;
        weights.validate()?;
        if weights.output_dim() != manifest.labels.len() {
            return Err(MlError::Invalid(format!(
                "model produces {} logits for {} manifest labels",
                weights.output_dim(),
                manifest.labels.len()
            )));
        }
        Ok(Box::new(MlpModel {
            manifest: manifest.clone(),
            weights,
        }))
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ModelRef, ModelTask, OpenSetMethod, OpenSetSpec, Precision, Tensor};

    /// A two-layer network whose arithmetic is checkable by hand.
    fn weights() -> MlpWeights {
        MlpWeights {
            format: MLP_FORMAT.into(),
            input_dim: 2,
            mean: vec![0.0, 0.0],
            scale: vec![1.0, 1.0],
            layers: vec![
                Layer {
                    in_dim: 2,
                    out_dim: 2,
                    weight: vec![1.0, 0.0, 0.0, 1.0],
                    bias: vec![0.0, -1.0],
                    activation: Activation::Relu,
                },
                Layer {
                    in_dim: 2,
                    out_dim: 2,
                    weight: vec![2.0, 0.0, 0.0, 3.0],
                    bias: vec![0.5, 0.0],
                    activation: Activation::Identity,
                },
            ],
            trained_on: "unit test".into(),
        }
    }

    fn manifest(bytes: &[u8]) -> ModelManifest {
        let sha = hex(&Sha256::digest(bytes));
        ModelManifest {
            schema: crate::ML_SCHEMA,
            model: ModelRef {
                id: "amc-test".into(),
                version: "0.1.0".into(),
                sha8: sha[..8].to_owned(),
            },
            sha256: sha,
            task: ModelTask::FamilyClass,
            consumer: "hk-classify/dl".into(),
            taxonomy: Some(hk_model::classify::TaxonomyRef::current()),
            family: Some("fsk".into()),
            labels: vec!["2fsk".into(), "gfsk".into()],
            open_set: OpenSetSpec {
                method: OpenSetMethod::Energy,
                temperature: 1.0,
                threshold: -2.0,
                calibrated_on: "dev@amc-grid-1".into(),
            },
            precision: Precision::Fp32,
            metrics_ref: None,
            enable_evidence: None,
        }
    }

    #[test]
    fn the_forward_pass_is_the_arithmetic_it_claims_to_be() {
        let w = weights();
        // x = [2, 3]: hidden = relu([2, 2]) = [2, 2]; logits = [2*2 + 0.5, 3*2] = [4.5, 6].
        let (logits, embedding) = w.forward(&[2.0, 3.0]).unwrap();
        assert_eq!(logits, vec![4.5, 6.0]);
        assert_eq!(embedding, vec![2.0, 2.0], "penultimate activations");
        // relu really clips: x = [-5, 0] → hidden [0, 0] → logits [0.5, 0].
        assert_eq!(w.forward(&[-5.0, 0.0]).unwrap().0, vec![0.5, 0.0]);
        // Standardisation is inside the file, so it cannot be applied differently by a caller.
        let mut s = weights();
        s.mean = vec![2.0, 3.0];
        s.scale = vec![2.0, 1.0];
        assert_eq!(s.forward(&[2.0, 3.0]).unwrap().0, vec![0.5, 0.0]);
        assert!(w.forward(&[1.0]).is_err(), "a wrong input width is refused");
    }

    #[test]
    fn a_batch_of_singles_equals_one_batch() {
        let bytes = serde_json::to_vec(&weights()).unwrap();
        let m = manifest(&bytes);
        let model = MlpProvider.load(&m, &bytes).unwrap();

        let inputs = [[2.0_f32, 3.0], [-1.0, 4.0], [0.5, 0.5]];
        let batched = model
            .infer(&TensorBatch {
                input: Tensor::new(vec![3, 2], inputs.concat()).unwrap(),
                batch_size: 3,
            })
            .unwrap();
        for (i, x) in inputs.iter().enumerate() {
            let single = model
                .infer(&TensorBatch {
                    input: Tensor::new(vec![1, 2], x.to_vec()).unwrap(),
                    batch_size: 1,
                })
                .unwrap();
            assert_eq!(single[0], batched[i], "item {i} differs when batched");
        }
        // A batch whose data does not match its declared size is refused, never truncated.
        assert!(
            model
                .infer(&TensorBatch {
                    input: Tensor::new(vec![2, 2], vec![0.0; 4]).unwrap(),
                    batch_size: 3,
                })
                .is_err()
        );
    }

    #[test]
    fn a_load_refuses_a_hash_mismatch_a_bad_format_and_a_label_width_disagreement() {
        let bytes = serde_json::to_vec(&weights()).unwrap();
        let good = manifest(&bytes);
        assert!(MlpProvider.load(&good, &bytes).is_ok());

        // The manifest's hash is the file's identity: a changed file is refused.
        let tampered = serde_json::to_vec(&MlpWeights {
            trained_on: "something else".into(),
            ..weights()
        })
        .unwrap();
        assert!(matches!(
            MlpProvider.load(&good, &tampered),
            Err(MlError::HashMismatch(_))
        ));

        // A file that is not this format is refused rather than guessed at.
        let alien = serde_json::to_vec(&MlpWeights {
            format: "onnx".into(),
            ..weights()
        })
        .unwrap();
        assert!(matches!(
            MlpProvider.load(&manifest(&alien), &alien),
            Err(MlError::Unsupported(_))
        ));

        // Manifest labels and logit width must agree, or every label would be off by one.
        let mut wrong_labels = good.clone();
        wrong_labels.labels = vec!["2fsk".into(), "gfsk".into(), "4fsk".into()];
        assert!(MlpProvider.load(&wrong_labels, &bytes).is_err());
    }

    #[test]
    fn the_provider_is_never_conformant_so_it_can_never_decide() {
        assert_eq!(MlpProvider.kind(), MlProviderKind::CpuMlp);
        assert!(
            !MlpProvider.conformant(),
            "ADR-0007: an unconformant provider never decides. This one is shadow-only by \
             construction until T-203's conformance suite judges a real ONNX engine."
        );
    }

    #[test]
    fn validation_catches_every_way_the_shapes_can_disagree() {
        type Break = fn(&mut MlpWeights);
        let cases: &[(&str, Break)] = &[
            ("input width", |w| w.input_dim = 3),
            ("zero scale", |w| w.scale[0] = 0.0),
            ("non-finite weight", |w| w.layers[0].weight[0] = f32::NAN),
            ("layer chain", |w| w.layers[1].in_dim = 5),
            ("weight count", |w| w.layers[0].weight.push(1.0)),
            ("softmax output", |w| {
                w.layers[1].activation = Activation::Relu
            }),
            ("no layers", |w| w.layers.clear()),
        ];
        for (name, f) in cases {
            let mut w = weights();
            f(&mut w);
            assert!(w.validate().is_err(), "{name} should be refused");
        }
        weights().validate().unwrap();
    }
}
