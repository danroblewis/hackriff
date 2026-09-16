//! Turning raw model outputs into a calibrated [`Prediction`] (ADR-0016 §6).
//!
//! Two numbers come out of one set of logits, and they answer different questions:
//!
//! - **`probs`** — a temperature-scaled softmax over the model's labels. It ranks the *classes the
//!   model knows*, and it is **not** an unknown detector. A softmax is normalised over the trained
//!   labels, so an input belonging to none of them still produces a confident-looking maximum; that
//!   is the failure mode ADR-0016 rejects in "Options considered".
//! - **`energy` / `unknown_score`** — the free energy `E = −T·logsumexp(z/T)` (Liu et al., NeurIPS
//!   2020). Unlike the softmax maximum, it keeps the *scale* of the logits, which the normalisation
//!   throws away: an in-distribution input excites some unit strongly and has low (very negative)
//!   energy, while an input the model has no feature for excites nothing and sits near zero. It is
//!   the ADR's chosen open-set method, and the only one this crate implements.
//!
//! [`Calibrator::unknown_score`] maps energy onto 0–1 with a logistic about the manifest's
//! threshold, which training sets at 95 % TPR on dev in-distribution data. So `unknown_score = 0.5`
//! is exactly that operating point — "this input fits the model less well than 95 % of the data it
//! was calibrated on" — and the mapping is monotone in energy, so any threshold a consumer picks is
//! a threshold on energy.

use std::sync::Arc;

use hk_model::Timestamp;

use crate::{
    MlError, MlMode, MlProviderKind, ModelManifest, ModelRef, OpenSetMethod, OpenSetSpec,
    Precision, Prediction, RawOutput,
};

/// Temperature-scaled softmax over `logits`, numerically stabilised by the maximum logit.
pub fn softmax_t(logits: &[f32], temperature: f32) -> Vec<f32> {
    if logits.is_empty() {
        return Vec::new();
    }
    let t = if temperature.is_finite() && temperature > 0.0 {
        f64::from(temperature)
    } else {
        1.0
    };
    let scaled: Vec<f64> = logits.iter().map(|z| f64::from(*z) / t).collect();
    let max = scaled.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let exps: Vec<f64> = scaled.iter().map(|s| (s - max).exp()).collect();
    let sum: f64 = exps.iter().sum();
    if !(sum.is_finite() && sum > 0.0) {
        // Degenerate logits (all non-finite): a uniform distribution is the only honest answer.
        return vec![1.0 / logits.len() as f32; logits.len()];
    }
    exps.iter().map(|e| (e / sum) as f32).collect()
}

/// Free energy `E = −T·logsumexp(z/T)` over `logits` (Liu et al., NeurIPS 2020).
///
/// Lower (more negative) means "some class fits strongly"; near zero means the input excited
/// nothing the model knows. This is the open-set statistic — never the softmax maximum.
pub fn energy(logits: &[f32], temperature: f32) -> f32 {
    if logits.is_empty() {
        return 0.0;
    }
    let t = if temperature.is_finite() && temperature > 0.0 {
        f64::from(temperature)
    } else {
        1.0
    };
    let scaled: Vec<f64> = logits.iter().map(|z| f64::from(*z) / t).collect();
    let max = scaled.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    if !max.is_finite() {
        return 0.0;
    }
    let sum: f64 = scaled.iter().map(|s| (s - max).exp()).sum();
    (-t * (max + sum.ln())) as f32
}

/// Calibration for one loaded model: the labels, the open-set spec and the provenance every
/// [`Prediction`] it produces carries.
///
/// A consumer builds one of these per loaded model (the labels are shared, not re-allocated per
/// event) and calls [`Calibrator::predict`] on each raw output.
#[derive(Clone, Debug)]
pub struct Calibrator {
    model: ModelRef,
    labels: Arc<[String]>,
    open_set: OpenSetSpec,
    provider: MlProviderKind,
    precision: Precision,
}

impl Calibrator {
    /// The calibrator a manifest describes.
    pub fn from_manifest(
        manifest: &ModelManifest,
        provider: MlProviderKind,
        precision: Precision,
    ) -> Result<Self, MlError> {
        manifest.validate()?;
        Ok(Self {
            model: manifest.model.clone(),
            labels: Arc::from(manifest.labels.clone()),
            open_set: manifest.open_set.clone(),
            provider,
            precision,
        })
    }

    /// The labels, in logit order.
    pub fn labels(&self) -> &Arc<[String]> {
        &self.labels
    }

    /// The open-set calibration in force.
    pub fn open_set(&self) -> &OpenSetSpec {
        &self.open_set
    }

    /// The open-set score of an energy: a logistic about the calibrated threshold, so 0.5 is the
    /// 95 %-TPR operating point training measured on dev in-distribution data, and the mapping is
    /// monotone in energy (a consumer thresholding this is thresholding energy).
    pub fn unknown_score(&self, energy: f32) -> f32 {
        let OpenSetMethod::Energy = self.open_set.method;
        let t = if self.open_set.temperature.is_finite() && self.open_set.temperature > 0.0 {
            f64::from(self.open_set.temperature)
        } else {
            1.0
        };
        let e = f64::from(energy);
        let threshold = f64::from(self.open_set.threshold);
        if !(e.is_finite() && threshold.is_finite()) {
            // Nothing measurable came back: maximally open is the safe answer, never "known".
            return 1.0;
        }
        (1.0 / (1.0 + (-(e - threshold) / t).exp())) as f32
    }

    /// Calibrates one raw output into a [`Prediction`].
    ///
    /// `mode` travels with the prediction so a consumer cannot lose it: a `Shadow` prediction is
    /// recorded and compared, and decides nothing (ADR-0016 §6).
    #[allow(clippy::too_many_arguments)]
    pub fn predict(
        &self,
        raw: &RawOutput,
        mode: MlMode,
        latency_ms: f32,
        batch_size: u16,
        t: Timestamp,
    ) -> Result<Prediction, MlError> {
        if raw.logits.len() != self.labels.len() {
            return Err(MlError::Invalid(format!(
                "model produced {} logits for {} labels",
                raw.logits.len(),
                self.labels.len()
            )));
        }
        let temperature = self.open_set.temperature;
        let e = energy(&raw.logits, temperature);
        let p = Prediction {
            model: self.model.clone(),
            provider: self.provider,
            precision: self.precision,
            labels: Arc::clone(&self.labels),
            logits: raw.logits.clone(),
            probs: softmax_t(&raw.logits, temperature),
            energy: e,
            unknown_score: self.unknown_score(e),
            embedding: raw.embedding.clone(),
            mode,
            latency_ms,
            batch_size: batch_size.max(1),
            t,
        };
        p.validate()?;
        Ok(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ModelTask, OpenSetMethod};

    fn manifest() -> ModelManifest {
        ModelManifest {
            schema: crate::ML_SCHEMA,
            model: ModelRef {
                id: "amc-fsk".into(),
                version: "0.1.0".into(),
                sha8: "abcdef01".into(),
            },
            sha256: format!("abcdef01{}", "0".repeat(56)),
            task: ModelTask::FamilyClass,
            consumer: "hk-classify/dl".into(),
            taxonomy: Some(hk_model::classify::TaxonomyRef::current()),
            family: Some("fsk".into()),
            labels: vec!["2fsk".into(), "gfsk".into(), "4fsk".into()],
            open_set: OpenSetSpec {
                method: OpenSetMethod::Energy,
                temperature: 1.0,
                threshold: -4.0,
                calibrated_on: "dev@amc-grid-1".into(),
            },
            precision: Precision::Fp32,
            metrics_ref: None,
            enable_evidence: None,
        }
    }

    fn calibrator() -> Calibrator {
        Calibrator::from_manifest(&manifest(), MlProviderKind::CpuTract, Precision::Fp32).unwrap()
    }

    #[test]
    fn energy_separates_what_a_softmax_maximum_cannot() {
        // A softmax is shift-invariant: it throws away the *scale* of the logits and keeps only
        // their differences. These two inputs therefore have byte-identical probabilities — a
        // strongly excited in-distribution input, and one four nats weaker that excites nothing —
        // so no threshold on any probability can separate them. Energy keeps exactly what the
        // normalisation discarded.
        let strong = [8.0_f32, 1.0, 1.0];
        let weak = [3.0_f32, -4.0, -4.0];
        let p_strong = softmax_t(&strong, 1.0);
        let p_weak = softmax_t(&weak, 1.0);
        for (a, b) in p_strong.iter().zip(&p_weak) {
            assert!(
                (a - b).abs() < 1e-6,
                "softmax cannot tell them apart: {a} vs {b}"
            );
        }
        assert!(
            p_strong[0] > 0.99,
            "and it is confident in both: {p_strong:?}"
        );

        let e_strong = energy(&strong, 1.0);
        let e_weak = energy(&weak, 1.0);
        assert!(
            e_strong < e_weak - 4.9,
            "in-distribution energy {e_strong} must be well below OOD {e_weak}"
        );

        let c = calibrator();
        assert!(
            c.unknown_score(e_strong) < 0.5,
            "strong logits: {}",
            c.unknown_score(e_strong)
        );
        assert!(
            c.unknown_score(e_weak) > 0.5,
            "weak logits: {}",
            c.unknown_score(e_weak)
        );
        // Monotone in energy, so thresholding the score is thresholding energy.
        assert!(c.unknown_score(-10.0) < c.unknown_score(-4.0));
        assert!(c.unknown_score(-4.0) < c.unknown_score(0.0));
        assert!(
            (c.unknown_score(-4.0) - 0.5).abs() < 1e-6,
            "0.5 at threshold"
        );
    }

    #[test]
    fn a_calibrated_prediction_validates_and_keeps_its_mode() {
        let c = calibrator();
        let raw = RawOutput {
            logits: vec![3.0, 0.5, -1.0],
            embedding: Some(vec![0.1, 0.2]),
        };
        let p = c
            .predict(&raw, MlMode::Shadow, 1.25, 8, Timestamp::UNIX_EPOCH)
            .unwrap();
        p.validate().unwrap();
        assert!(!p.decides(), "shadow decides nothing");
        assert_eq!(p.top().unwrap().0, "2fsk");
        assert!((p.probs.iter().sum::<f32>() - 1.0).abs() < 1e-5);
        assert_eq!(p.embedding, Some(vec![0.1, 0.2]));

        // A model whose output width disagrees with its manifest is refused, not reshaped.
        let bad = RawOutput {
            logits: vec![1.0, 2.0],
            embedding: None,
        };
        assert!(
            c.predict(&bad, MlMode::Shadow, 1.0, 1, Timestamp::UNIX_EPOCH)
                .is_err()
        );
    }

    #[test]
    fn degenerate_outputs_never_produce_a_confident_known_answer() {
        let c = calibrator();
        // Non-finite logits: uniform probabilities, and maximally unknown rather than a claim.
        let probs = softmax_t(&[f32::NAN, f32::NAN, f32::NAN], 1.0);
        assert!(probs.iter().all(|p| (p - 1.0 / 3.0).abs() < 1e-6));
        assert_eq!(c.unknown_score(f32::NAN), 1.0);
        assert_eq!(energy(&[], 1.0), 0.0);
        // A temperature that is not a temperature falls back to 1 rather than dividing by zero.
        assert!((energy(&[1.0, 1.0], 0.0) - energy(&[1.0, 1.0], 1.0)).abs() < 1e-6);
    }
}
