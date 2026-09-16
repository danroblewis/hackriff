//! The per-family DL stage (T-204, ADR-0016 §4.6), **in shadow mode**.
//!
//! ```text
//! feature tree ─▶ Classification (published, unchanged) ─┬─▶ caller
//!                            │                           │
//!                            └─ family named ─▶ DL model ─┴─▶ ShadowRecord (side channel)
//! ```
//!
//! # What this stage may do, and what it may never do
//!
//! - It runs **within a family only**. The family comes from the classical cascade; a model here
//!   chooses between that family's classes (`2fsk` vs `gfsk` vs `msk` vs `4fsk`) and never between
//!   families (ADR-0016 §4.6, "DL chooses the family" is in Options-rejected). If the classical
//!   stage abstained, nothing runs at all: there is no family to refine.
//! - Its open set is the **energy score**, never a softmax maximum ([`hk_ml::predict`]).
//! - **Shadow means shadow.** [`observe`](DlStage::observe) takes the [`Classification`] by
//!   reference and returns a [`ShadowRecord`] on the side. There is no code path by which a
//!   prediction reaches a published row — not a flag, not a reason code, not a reordering. That is
//!   asserted in this module (`shadow_mode_changes_no_published_classification`), over the whole
//!   synthetic grid, by serialising both rows and comparing them.
//! - **`Active` is refused** by [`DlStage::add`]. Two independent things are missing before a model
//!   here could decide anything: the §4.6 enable evidence on its manifest, and a *conformant*
//!   provider (ADR-0007) — and the only evaluator that exists today
//!   ([`hk_ml::mlp`](hk_ml::MlProviderKind::CpuMlp)) reports itself unconformant for ever. So the
//!   refusal is not a policy that a later caller can forget: it is the shape of this API.
//!
//! # Where the record goes
//!
//! ADR-0016 §6 puts shadow records in hk-store (`ml/shadow/`, hourly CRC-line NDJSON) with per-SNR
//! agreement aggregates, and **T-203 owns that store**. [`ShadowRecord`] is serialisable so it can
//! be appended as one NDJSON line the day that store exists; until then the evaluation binary
//! (`dl-eval`) aggregates them itself.
//!
//! # The model's input
//!
//! [`dl_input`] is a fixed-width vector of **distribution shapes** — amplitude histogram,
//! instantaneous-frequency histogram, spectrum shape, envelope autocorrelation — deliberately
//! *not* `features@1`. A model fed the same hand-designed statistics as the classical densities can
//! only re-litigate them; the point of a learned stage is to see structure the hand features throw
//! away. It is computed by this module for both training export and inference, so the two can
//! never drift apart.

use std::time::Instant;

use hk_dsp::{WelchConfig, WindowKind, welch};
use hk_ml::predict::Calibrator;
use hk_ml::{LoadedModel, MlError, MlMode, Prediction, Tensor, TensorBatch};
use hk_model::Timestamp;
use hk_model::classify::{Classification, UNKNOWN};
use num_complex::Complex32;
use serde::{Deserialize, Serialize};

use crate::classifier::{Classifier, ClassifyRequest};

/// Version of the [`dl_input`] layout. A model file records the width it was trained at; changing
/// the layout is a new version, never a silent reinterpretation of the same numbers.
pub const DL_INPUT_VERSION: u32 = 1;

/// Amplitude-histogram bins.
pub const AMP_BINS: usize = 32;
/// Instantaneous-frequency histogram bins.
pub const IF_BINS: usize = 32;
/// Spectrum-shape bins.
pub const SPEC_BINS: usize = 32;
/// Envelope autocorrelation lags.
pub const ACF_LAGS: usize = 16;

/// Width of [`dl_input`].
pub const DL_INPUT_DIM: usize = AMP_BINS + IF_BINS + SPEC_BINS + ACF_LAGS;

/// Smallest snippet the stage will look at (as [`crate::features::MIN_SAMPLES`]).
pub const MIN_SAMPLES: usize = 256;

/// The learned stage's input vector, or `None` when the snippet is too short or unscaled to
/// measure — an abstention, never a vector of zeros (a fabricated input is what makes a classifier
/// confidently wrong, T-235).
///
/// Layout, all finite and O(1) in magnitude:
/// 1. `AMP_BINS` amplitude histogram of `|x|/rms` over 0–3, as a density (mean 1);
/// 2. `IF_BINS` histogram of the instantaneous frequency normalised by the occupied bandwidth;
/// 3. `SPEC_BINS` spectrum shape, log power relative to the peak, mapped onto 0–1;
/// 4. `ACF_LAGS` normalised autocorrelation of the envelope power at lags 1…16.
pub fn dl_input(
    samples: &[Complex32],
    sample_rate_hz: f64,
    obw_hz: Option<f64>,
) -> Option<Vec<f32>> {
    if samples.len() < MIN_SAMPLES || !(sample_rate_hz.is_finite() && sample_rate_hz > 0.0) {
        return None;
    }
    let amp: Vec<f64> = samples.iter().map(|s| f64::from(s.norm())).collect();
    let mean_power: f64 = amp.iter().map(|a| a * a).sum::<f64>() / amp.len() as f64;
    let rms = mean_power.sqrt();
    if !(rms.is_finite() && rms > 0.0) {
        return None;
    }

    let mut v = Vec::with_capacity(DL_INPUT_DIM);

    // 1. Amplitude distribution: separates constant-envelope from keyed and multi-level waveforms.
    let mut hist = vec![0.0_f64; AMP_BINS];
    for a in &amp {
        let r = (a / rms / 3.0).clamp(0.0, 0.999_999);
        hist[(r * AMP_BINS as f64) as usize] += 1.0;
    }
    push_density(&mut v, &hist);

    // 2. Instantaneous frequency, normalised by the occupied bandwidth so the shape is a property
    // of the modulation rather than of the analysis rate.
    let scale_hz = obw_hz
        .filter(|o| o.is_finite() && *o > 0.0)
        .unwrap_or(sample_rate_hz / 4.0)
        .max(1e-9);
    let mut ifh = vec![0.0_f64; IF_BINS];
    for w in samples.windows(2) {
        let d = (w[1] * w[0].conj()).arg();
        let hz = f64::from(d) / std::f64::consts::TAU * sample_rate_hz;
        let u = (hz / scale_hz).clamp(-2.0, 1.999_999);
        let bin = ((u + 2.0) / 4.0 * IF_BINS as f64) as usize;
        ifh[bin.min(IF_BINS - 1)] += 1.0;
    }
    push_density(&mut v, &ifh);

    // 3. Spectrum shape, relative to its own peak: skirts, carriers, multicarrier structure.
    let fft_len = (samples.len() / 8)
        .next_power_of_two()
        .clamp(64, 1024)
        .min(samples.len());
    let cfg = WelchConfig {
        fft_len,
        overlap: fft_len / 2,
        window: WindowKind::Hann,
        holds: false,
        spectral_kurtosis: false,
    };
    match welch(samples, sample_rate_hz, 0.0, &cfg) {
        Ok(spectrum) if !spectrum.psd.is_empty() => {
            let psd: Vec<f64> = spectrum
                .psd
                .iter()
                .map(|p| f64::from(*p).max(1e-30))
                .collect();
            let peak = psd.iter().copied().fold(f64::MIN_POSITIVE, f64::max);
            let per = psd.len().div_ceil(SPEC_BINS).max(1);
            for b in 0..SPEC_BINS {
                let lo = (b * per).min(psd.len());
                let hi = ((b + 1) * per).min(psd.len());
                let mean = if hi > lo {
                    psd[lo..hi].iter().sum::<f64>() / (hi - lo) as f64
                } else {
                    1e-30
                };
                // −60 dB below the peak is the floor; 0 dB is the peak.
                let db = 10.0 * (mean / peak).log10();
                v.push(((db.clamp(-60.0, 0.0) + 60.0) / 60.0) as f32);
            }
        }
        // No spectrum: the input is not measurable, so nothing is guessed.
        _ => return None,
    }

    // 4. Envelope periodicity: keying, pulse trains and symbol structure the amplitude histogram
    // cannot see because it is order-free.
    let power: Vec<f64> = amp.iter().map(|a| a * a).collect();
    let mean: f64 = power.iter().sum::<f64>() / power.len() as f64;
    let centred: Vec<f64> = power.iter().map(|p| p - mean).collect();
    let denom: f64 = centred.iter().map(|c| c * c).sum();
    for lag in 1..=ACF_LAGS {
        let r = if denom > 0.0 && centred.len() > lag {
            centred
                .iter()
                .zip(centred.iter().skip(lag))
                .map(|(a, b)| a * b)
                .sum::<f64>()
                / denom
        } else {
            0.0
        };
        v.push(r.clamp(-1.0, 1.0) as f32);
    }

    debug_assert_eq!(v.len(), DL_INPUT_DIM);
    v.iter().all(|x| x.is_finite()).then_some(v)
}

/// Pushes a histogram as a density with mean 1 (so the vector's scale does not depend on how many
/// samples were counted).
fn push_density(v: &mut Vec<f32>, hist: &[f64]) {
    let total: f64 = hist.iter().sum();
    let k = hist.len() as f64;
    for h in hist {
        v.push(if total > 0.0 {
            (h / total * k) as f32
        } else {
            0.0
        });
    }
}

/// One shadow observation: what the model said, what the classical cascade said, and enough
/// context to aggregate per family and per SNR bin (ADR-0016 §6).
///
/// It is **not** part of any [`Classification`]. Nothing downstream of the classifier reads it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShadowRecord {
    /// When the observation was made.
    pub t: Timestamp,
    /// Family the classical cascade named (the model only ever refines within it).
    pub family: String,
    /// Measured in-band SNR, dB, when the caller measured one.
    pub snr_db: Option<f64>,
    /// Class the classical cascade named, if it was above the class gate.
    pub classical_class: Option<String>,
    /// Probability it gave that class.
    pub classical_p: Option<f64>,
    /// The classical χ² open-set score of the published row.
    pub classical_open_set: f64,
    /// Class the model named (its highest-probability label).
    pub dl_class: String,
    /// Temperature-calibrated probability of that class. **Not** an unknown score.
    pub dl_p: f64,
    /// Energy `E = −T·logsumexp(z/T)`.
    pub dl_energy: f64,
    /// Calibrated open-set score, 0–1 (0.5 = the dev 95 %-TPR operating point).
    pub dl_unknown_score: f64,
    /// Whether the two stages named the same class (only meaningful when the classical stage
    /// named one at all).
    pub agrees: Option<bool>,
    /// `id@version#sha8` of the model.
    pub model: String,
    /// Inference latency, ms.
    pub latency_ms: f32,
    /// The mode it ran in. Always `shadow` today.
    pub mode: String,
}

/// One family's loaded model.
struct FamilyModel {
    family: String,
    model: Box<dyn LoadedModel>,
    calibrator: Calibrator,
    mode: MlMode,
}

/// The per-family DL stage: zero or more within-family models, each in `off` or `shadow` mode.
///
/// An empty stage is the default and costs nothing: [`DlStage::observe`] returns `None`
/// immediately, which is also what happens when no model is loaded for the family the classical
/// cascade named.
#[derive(Default)]
pub struct DlStage {
    models: Vec<FamilyModel>,
}

impl DlStage {
    /// A stage with no models: it observes nothing.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `model` for `family` in `mode`.
    ///
    /// Refuses:
    /// - [`MlMode::Active`] — a model may decide something only with the §4.6 enable evidence on
    ///   its manifest **and** a conformant provider (ADR-0007). Neither exists in M3, and T-203
    ///   owns both, so this is refused here rather than left to a caller to remember;
    /// - a manifest that is not a within-family class model, or that names a different family: a
    ///   model that could choose the family is exactly what ADR-0016 rejected.
    pub fn add(
        &mut self,
        family: &str,
        model: Box<dyn LoadedModel>,
        mode: MlMode,
    ) -> Result<(), MlError> {
        if mode == MlMode::Active {
            return Err(MlError::Invalid(format!(
                "{family}: active mode needs ADR-0016 §4.6 enable evidence and a conformant \
                 provider (ADR-0007); T-204 ships the stage in shadow only"
            )));
        }
        let manifest = model.manifest();
        if manifest.task != hk_ml::ModelTask::FamilyClass {
            return Err(MlError::Invalid(format!(
                "{family}: task {:?} is not a within-family class model",
                manifest.task
            )));
        }
        if manifest.family.as_deref() != Some(family) {
            return Err(MlError::Invalid(format!(
                "{family}: manifest is scoped to {:?}",
                manifest.family
            )));
        }
        if self.models.iter().any(|m| m.family == family) {
            return Err(MlError::Invalid(format!("{family}: already loaded")));
        }
        let calibrator =
            Calibrator::from_manifest(manifest, hk_ml::MlProviderKind::CpuMlp, manifest.precision)?;
        self.models.push(FamilyModel {
            family: family.to_owned(),
            model,
            calibrator,
            mode,
        });
        Ok(())
    }

    /// Families with a model loaded.
    pub fn families(&self) -> impl Iterator<Item = &str> {
        self.models.iter().map(|m| m.family.as_str())
    }

    /// Whether any model is loaded.
    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }

    /// Runs `family`'s model over a snippet directly, with no [`Classification`] involved.
    ///
    /// This exists for **evaluation** (`dl-eval`), where the open set has to be measured on inputs
    /// the classical cascade *rejected* as well as ones it accepted: scoring only what the cascade
    /// accepted makes its own false-known rate 1.0 by construction, which is a property of the
    /// measurement rather than of either stage.
    ///
    /// It is not a decision path. The mode travels on the returned [`Prediction`], so
    /// [`Prediction::decides`] is false for a shadow model exactly as it is everywhere else.
    pub fn score(
        &self,
        family: &str,
        samples: &[Complex32],
        sample_rate_hz: f64,
        obw_hz: Option<f64>,
        t: Timestamp,
    ) -> Option<Prediction> {
        let fm = self.models.iter().find(|m| m.family == family)?;
        if fm.mode == MlMode::Off {
            return None;
        }
        let x = dl_input(samples, sample_rate_hz, obw_hz)?;
        let started = Instant::now();
        let batch = TensorBatch {
            input: Tensor::new(vec![1, x.len()], x)?,
            batch_size: 1,
        };
        let raw = fm.model.infer(&batch).ok()?;
        let latency_ms = started.elapsed().as_secs_f32() * 1e3;
        fm.calibrator
            .predict(raw.first()?, fm.mode, latency_ms, 1, t)
            .ok()
    }

    /// Runs the model for the family `c` named, if there is one, and returns what it saw.
    ///
    /// `c` is **borrowed, never returned and never modified**: this signature is the shadow-mode
    /// guarantee. `None` means nothing ran (no family, no model, mode off, or an unmeasurable
    /// snippet).
    pub fn observe(
        &self,
        c: &Classification,
        samples: &[Complex32],
        sample_rate_hz: f64,
        obw_hz: Option<f64>,
    ) -> Option<ShadowRecord> {
        if c.family == UNKNOWN {
            // The stage never chooses a family: with no family there is nothing to refine.
            return None;
        }
        let prediction = self.score(&c.family, samples, sample_rate_hz, obw_hz, c.t)?;
        let latency_ms = prediction.latency_ms;
        let (label, p) = prediction.top()?;
        Some(ShadowRecord {
            t: c.t,
            family: c.family.clone(),
            snr_db: c.provenance.snr_db,
            classical_class: c.class.as_ref().map(|cc| cc.label.clone()),
            classical_p: c.class.as_ref().map(|cc| cc.p),
            classical_open_set: c.open_set_score,
            agrees: c.class.as_ref().map(|cc| cc.label == label),
            dl_class: label.to_owned(),
            dl_p: f64::from(p),
            dl_energy: f64::from(prediction.energy),
            dl_unknown_score: f64::from(prediction.unknown_score),
            model: prediction.model.to_string(),
            latency_ms,
            mode: prediction.mode.as_str().to_owned(),
        })
    }
}

/// Classifies `request` and, if `stage` has a model for the family that came out, observes it.
///
/// The returned [`Classification`] is exactly what [`Classifier::classify`] produced — the
/// module's tests assert that, byte for byte, with the stage loaded and unloaded.
pub fn classify_shadowed(
    classifier: &Classifier,
    stage: &DlStage,
    request: &ClassifyRequest<'_>,
) -> (Classification, Option<ShadowRecord>) {
    let c = classifier.classify(request);
    let record = stage.observe(&c, request.samples, request.sample_rate_hz, request.obw_hz);
    (c, record)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::symbols::SymbolEstimator;
    use crate::synth::{ACCEPTANCE_SEED_BASE, Class, SynthConfig, generate};
    use crate::thresholds::thresholds_of;
    use hk_ml::{
        ModelManifest, ModelRef, ModelTask, OpenSetMethod, OpenSetSpec, Precision, RawOutput,
    };

    /// A model that is **confidently wrong on purpose**: whatever it is shown, it shouts the last
    /// label with huge logits. If a shadow prediction could leak into a published row anywhere,
    /// this is the model that would prove it.
    struct Shouter {
        manifest: ModelManifest,
    }

    impl LoadedModel for Shouter {
        fn manifest(&self) -> &ModelManifest {
            &self.manifest
        }
        fn infer(&self, batch: &TensorBatch) -> Result<Vec<RawOutput>, MlError> {
            let k = self.manifest.labels.len();
            Ok((0..usize::from(batch.batch_size.max(1)))
                .map(|_| RawOutput {
                    logits: (0..k)
                        .map(|i| if i + 1 == k { 40.0 } else { -40.0 })
                        .collect(),
                    embedding: None,
                })
                .collect())
        }
    }

    fn manifest(family: &str, labels: &[&str]) -> ModelManifest {
        ModelManifest {
            schema: hk_ml::ML_SCHEMA,
            model: ModelRef {
                id: format!("amc-{family}"),
                version: "0.0.1".into(),
                sha8: "0123abcd".into(),
            },
            sha256: format!("0123abcd{}", "0".repeat(56)),
            task: ModelTask::FamilyClass,
            consumer: "hk-classify/dl".into(),
            taxonomy: Some(hk_model::classify::TaxonomyRef::current()),
            family: Some(family.to_owned()),
            labels: labels.iter().map(|l| (*l).to_owned()).collect(),
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

    fn shouting_stage() -> DlStage {
        let mut stage = DlStage::new();
        for (family, labels) in [
            ("fsk", &["2fsk", "gfsk", "msk", "4fsk"][..]),
            ("analog", &["am", "nbfm", "wfm", "ssb", "cw"][..]),
            ("psk-qam", &["bpsk", "qpsk", "8psk", "qam16", "qam64"][..]),
            ("ook-ask", &["ook", "ask4"][..]),
            ("pulsed", &["ppm", "pulse"][..]),
        ] {
            stage
                .add(
                    family,
                    Box::new(Shouter {
                        manifest: manifest(family, labels),
                    }),
                    MlMode::Shadow,
                )
                .expect("a shadow model loads");
        }
        stage
    }

    /// **The shadow-mode proof.** Identical inputs, stage on and stage off, must publish identical
    /// classifications — over the whole taxonomy, at three SNRs, with a model that disagrees as
    /// loudly as it can.
    #[test]
    fn shadow_mode_changes_no_published_classification() {
        let stage = shouting_stage();
        let off = DlStage::new();
        let mut c14 = SymbolEstimator::new();
        let mut observed = 0;

        for class in Class::TAXONOMY.iter().chain(Class::HELD_OUT) {
            let gate = class
                .family()
                .and_then(thresholds_of)
                .and_then(|t| t.snr_gate_db)
                .unwrap_or(10.0);
            for offset in [-5.0, 0.0, 10.0] {
                let snr = gate + offset;
                let s = generate(*class, &SynthConfig::new(snr, ACCEPTANCE_SEED_BASE + 7));
                let symbols = c14.from_samples(
                    &s.symbol_samples,
                    s.symbol_sample_rate_hz,
                    Some(s.obw_hz),
                    Some(snr),
                );
                let mut req =
                    ClassifyRequest::new(&s.samples, s.sample_rate_hz, Timestamp::UNIX_EPOCH);
                req.obw_hz = Some(s.obw_hz);
                req.snr_db = Some(snr);
                req.symbols = symbols.as_ref();
                // Drive the *whole* current cascade, T-200's post-sync verifier included: the
                // verifier re-ranks within-family classes, which is the same thing this stage
                // would do, so the proof has to hold with it in the loop and not only over the
                // feature tree.
                req.symbol_samples = Some(&s.symbol_samples);
                req.symbol_sample_rate_hz = Some(s.symbol_sample_rate_hz);

                let classifier = Classifier::new();
                let plain = classifier.classify(&req);
                let (with_stage, record) = classify_shadowed(&classifier, &stage, &req);
                let (without_stage, none) = classify_shadowed(&classifier, &off, &req);

                assert_eq!(
                    with_stage,
                    plain,
                    "{} at {snr} dB: the shadow stage changed the published row",
                    class.label()
                );
                assert_eq!(
                    serde_json::to_string(&with_stage).unwrap(),
                    serde_json::to_string(&plain).unwrap(),
                    "{} at {snr} dB: serialised rows differ",
                    class.label()
                );
                assert_eq!(with_stage, without_stage);
                assert!(none.is_none(), "an empty stage observes nothing");
                if let Some(r) = record {
                    observed += 1;
                    // It really did run, and really did disagree: the row above is unchanged in
                    // spite of that, not because the model was silent.
                    assert_eq!(r.family, plain.family);
                    assert!(r.dl_p > 0.99, "the shouter is confident: {}", r.dl_p);
                    assert_eq!(r.mode, "shadow");
                    if let Some(classical) = &r.classical_class {
                        assert_eq!(r.agrees, Some(*classical == r.dl_class));
                    }
                }
            }
        }
        assert!(
            observed >= 10,
            "only {observed} shadow observations: the proof needs the stage to actually run"
        );
    }

    #[test]
    fn an_active_mode_is_refused_and_so_is_a_model_that_could_choose_a_family() {
        let mut stage = DlStage::new();
        let m = manifest("fsk", &["2fsk", "gfsk"]);
        assert!(
            stage
                .add(
                    "fsk",
                    Box::new(Shouter {
                        manifest: m.clone()
                    }),
                    MlMode::Active
                )
                .is_err(),
            "active needs enable evidence and a conformant provider"
        );
        // Scoped to another family: refused, because the loader is what binds a model to a family.
        assert!(
            stage
                .add(
                    "psk-qam",
                    Box::new(Shouter {
                        manifest: m.clone()
                    }),
                    MlMode::Shadow
                )
                .is_err()
        );
        // An embedding model is not a within-family classifier.
        let mut embed = m.clone();
        embed.task = ModelTask::Embedding;
        assert!(
            stage
                .add("fsk", Box::new(Shouter { manifest: embed }), MlMode::Shadow)
                .is_err()
        );
        stage
            .add("fsk", Box::new(Shouter { manifest: m }), MlMode::Shadow)
            .unwrap();
        assert_eq!(stage.families().collect::<Vec<_>>(), vec!["fsk"]);
    }

    #[test]
    fn the_stage_never_runs_without_a_family_or_on_an_unmeasurable_snippet() {
        let stage = shouting_stage();
        let classifier = Classifier::new();

        // Below every gate the cascade abstains; with no family there is nothing to refine.
        let s = generate(
            Class::Fsk2,
            &SynthConfig::new(5.0, ACCEPTANCE_SEED_BASE + 11),
        );
        let mut req = ClassifyRequest::new(&s.samples, s.sample_rate_hz, Timestamp::UNIX_EPOCH);
        req.obw_hz = Some(s.obw_hz);
        req.snr_db = Some(5.0);
        let (c, record) = classify_shadowed(&classifier, &stage, &req);
        assert_eq!(c.family, UNKNOWN);
        assert!(record.is_none(), "no family, no DL call");

        // Too short to measure: an abstention, never a zero vector.
        assert!(dl_input(&[Complex32::new(0.5, 0.5); 64], 1e6, Some(100e3)).is_none());
        assert!(dl_input(&[Complex32::new(0.0, 0.0); 4096], 1e6, Some(100e3)).is_none());
        assert!(dl_input(&[Complex32::new(0.5, 0.5); 4096], 0.0, None).is_none());
    }

    #[test]
    fn the_input_vector_is_bounded_deterministic_and_actually_discriminates() {
        let vector = |class: Class| {
            let s = generate(class, &SynthConfig::new(25.0, ACCEPTANCE_SEED_BASE + 3));
            dl_input(&s.samples, s.sample_rate_hz, Some(s.obw_hz)).expect("measurable")
        };
        let a = vector(Class::Fsk2);
        assert_eq!(a.len(), DL_INPUT_DIM);
        assert!(a.iter().all(|v| v.is_finite() && v.abs() <= 40.0));
        assert_eq!(a, vector(Class::Fsk2), "same waveform, same vector");

        // Two classes inside one family must not map to the same point, or the stage could never
        // separate them however it were trained.
        let b = vector(Class::Fsk4);
        let d: f32 = a.iter().zip(&b).map(|(x, y)| (x - y).powi(2)).sum();
        assert!(d > 1e-3, "2fsk and 4fsk collapse to the same input: {d}");
    }
}
