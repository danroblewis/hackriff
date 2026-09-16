//! M3 classification contracts (ADR-0016, T-211). **Core interface**: changes are reviewed before
//! merge; renames and semantic changes amend ADR-0016.
//!
//! - [`taxonomy`]: the modulation taxonomy `hk-mod@1` as data, with lookup and validation.
//! - [`Classification`]: one C15 output: posterior **and** likelihood-only distributions over
//!   families, each including `unknown`; open-set score, normalised entropy, deciding stage and
//!   provenance. Stored additively on `emitter_classification` (migration 0007) by
//!   `Repository::record_classification`.
//! - [`rank`]: the arbitration rank that picks an emitter's current family.
//! - [`fuse`]: the pure fusion of evidence with a C17 band-plan prior, and the three rules that
//!   bound what a prior may do (T-218).
//! - [`thresholds`]: `thresholds@1`, the per-family SNR gates and reporting floors (T-218).
//!
//! This module holds types and invariants only; no classifier logic (T-199 owns `hk-classify`).
//! The legacy [`crate::emitter::Classification`] (family, confidence, open-set score, model
//! version) stays the shape of the legacy columns and of existing writers.

pub mod fuse;
pub mod rank;
pub mod seed; // T-215 (ADR-0016 §8): the MAUTO seed interface
pub mod taxonomy;
pub mod thresholds;

use serde::{Deserialize, Serialize};

use crate::emitter::LinkTarget;
use crate::time::Timestamp;

pub use fuse::{FamilyPriorSet, FamilyPriors, Fused, InvalidPrior, NoPriors, StaticPriors, fuse};
pub use rank::{ArbRank, DECODER_RULES_PREFIX};
pub use seed::{
    BudgetHint, ClusterPipeline, ClusterSeed, Hypothesis, OPEN_SEARCH_MIN_SHARE,
    PRUNE_LIKELIHOOD_SHARE, SEED_SCHEMA, SearchSeed, SeedBoost, SeedInputs,
};
pub use taxonomy::{Coarse, HK_MOD_V1, Taxonomy, TaxonomyRef, UNKNOWN, family_of};
pub use thresholds::{
    EVIDENCE_DOMINANCE_RATIO, FamilyThresholds, THRESHOLDS, THRESHOLDS_VERSION, passes_gate,
    thresholds_of,
};

/// Schema version of [`Classification`].
pub const CLASSIFICATION_SCHEMA: u16 = 1;

/// The [`ClassProvenance::features_version`] of a row whose feature set **cannot be identified**
/// (T-290).
///
/// Every classification written before T-290 carries `1`, whatever feature vector actually
/// produced it: `hk-classify` restated its provenance constant as `1` while the feature vector
/// moved to `2` (T-248) and `3` (T-286). Those versions differ in what the `symmetry` dimension
/// means and, from `3`, in whether it is measured at all, so `1` on a stored row means
/// **indeterminate** — it is not "features@1", it does not order against a current version, and it
/// does not imply the row is old.
///
/// Nothing is migrated: `emitter_classification` is append-only, and the writing code left no way
/// to tell which of the three vectors a given row used. Rows written since T-290 name the vector
/// exactly, so [`ClassProvenance::names_a_feature_set`] separates the two cases.
pub const FEATURES_VERSION_INDETERMINATE: u32 = 1;

/// Maximum reported confidence: no call is certain.
pub const MAX_CONFIDENCE: f64 = 0.999;

/// Minimum uniform weight λ₀ of a family prior (ADR-0016 §3).
pub const LAMBDA0_MIN: f64 = 0.1;

/// Tolerance on a distribution summing to 1.
pub const SUM_TOLERANCE: f64 = 1e-6;

/// The stage that decided a classification.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Stage {
    /// Classical feature tree with class-conditional densities (T-199).
    FeatureTree,
    /// Post-sync likelihood verifier, ALRT/GLRT (T-200).
    Verifier,
    /// Per-family deep-learning stage, within-family class only (T-204).
    Dl,
    /// A CRC-valid decode.
    Decoder,
    /// A user's explicit reclassification.
    User,
    /// A demodulator chain's label (pre-M3 rows derive this).
    Chain,
    /// Track occupancy shape.
    TrackShape,
}

impl Stage {
    /// The serde/column string.
    pub const fn as_str(self) -> &'static str {
        match self {
            Stage::FeatureTree => "feature-tree",
            Stage::Verifier => "verifier",
            Stage::Dl => "dl",
            Stage::Decoder => "decoder",
            Stage::User => "user",
            Stage::Chain => "chain",
            Stage::TrackShape => "track-shape",
        }
    }
}

/// One label and its probability.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LabelP {
    /// Family or class label (`unknown` allowed where the distribution includes it).
    pub label: String,
    /// Probability, 0–1.
    pub p: f64,
}

/// The C17 family prior a posterior was fused with.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PriorUse {
    /// The prior set it came from.
    pub prior_ref: String,
    /// Mixture weights `[λ₀ uniform, λ₁ allocation, λ₂ licence, λ₃ history]`, summing to 1, with
    /// λ₀ ≥ [`LAMBDA0_MIN`].
    pub lambda: [f64; 4],
    /// P(family ∣ f, ℓ) over known families only: priors never carry `unknown` mass.
    pub dist: Vec<LabelP>,
}

/// A within-family class call.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClassCall {
    /// Class label within the classification's family.
    pub label: String,
    /// Its probability.
    pub p: f64,
    /// Distribution over the family's classes (may be empty).
    #[serde(default)]
    pub dist: Vec<LabelP>,
    /// Stage that decided the class.
    pub stage: Stage,
}

/// A model reference for a DL-decided classification.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRef {
    /// `id@version#sha8`, e.g. `amc-psk-qam@0.2.0#1a2b3c4d`.
    pub id: String,
    /// Provider kind, e.g. `cpu-tract`.
    pub provider: String,
    /// Precision, e.g. `fp32`.
    pub precision: String,
}

/// Suspect-input flags carried from the Detection and its Provenance.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SuspectFlags {
    /// ADC clipping.
    pub clipped: bool,
    /// Suspected intermodulation product.
    pub suspect_imd: bool,
    /// Image candidate.
    pub image_candidate: bool,
    /// Known spur.
    pub spur: bool,
}

impl SuspectFlags {
    /// Any flag set.
    pub fn any(&self) -> bool {
        self.clipped || self.suspect_imd || self.image_candidate || self.spur
    }
}

/// Provenance of a classification.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClassProvenance {
    /// Rule set and version, e.g. `hk-classify/tree@1`, or `decoder:<id>` for a decoder row.
    pub rules: String,
    /// Version of the **C15 feature vector** the classifier measured with
    /// (`hk_classify::FEATURES_VERSION`, `features@N` of ADR-0016 §4.2).
    ///
    /// Not the C18 `EmissionFeatures` field set, which this doc used to name and which versions
    /// separately ([`crate::signature::EMISSION_FEATURES_VERSION`]).
    /// [`FEATURES_VERSION_INDETERMINATE`] is the one value that names no feature set.
    pub features_version: u32,
    /// The EmissionFeatures snapshot used, if stored (opaque reference until T-201).
    pub features_ref: Option<String>,
    /// The model, only when a DL stage decided the family or the class.
    pub ml: Option<ModelRef>,
    /// Measured in-band SNR of the analysed extent, dB.
    pub snr_db: Option<f64>,
    /// SNR gate of the decided family, dB.
    pub snr_gate_db: f64,
    /// Whether a gate withheld likelihood mass.
    pub gated: bool,
    /// Threshold set, e.g. `thresholds@1`.
    pub thresholds: String,
    /// Suspect-input flags.
    #[serde(default)]
    pub suspect: SuspectFlags,
    /// Power mode it ran in, if reported.
    pub power_mode: Option<String>,
}

impl ClassProvenance {
    /// Whether [`Self::features_version`] identifies the feature vector behind this row.
    ///
    /// `false` only for [`FEATURES_VERSION_INDETERMINATE`], which every pre-T-290 writer stamped
    /// on every row whatever vector it used. A reader that shows, filters or compares feature-set
    /// versions checks this first: `1` is "unknown", never "the first version".
    pub fn names_a_feature_set(&self) -> bool {
        self.features_version > FEATURES_VERSION_INDETERMINATE
    }
}

/// Classification flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClassFlag {
    /// The prior changed the top label within the evidence-dominance margin.
    PriorTiebreak,
    /// The prior's top disagrees with the likelihood's top.
    PriorMismatch,
    /// The top family is below its SNR gate.
    BelowGate,
    /// The input detection is suspect (clipped, IMD, image, spur).
    SuspectInput,
    /// A shadow DL model disagreed.
    DlShadowDisagrees,
}

/// One C15 classification (ADR-0016 §2).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Classification {
    /// Schema version, [`CLASSIFICATION_SCHEMA`].
    pub schema: u16,
    /// When it was produced.
    #[serde(rename = "t_ns", alias = "t")]
    pub t: Timestamp,
    /// Taxonomy the labels belong to, e.g. `hk-mod@1`.
    pub taxonomy: TaxonomyRef,
    /// The observation it ran on.
    pub input: Option<LinkTarget>,
    /// Coarse call.
    pub coarse: Coarse,
    /// Posterior over families plus `unknown`; sums to 1; no entry is exactly 1.
    pub posterior: Vec<LabelP>,
    /// Evidence-only (uniform prior) distribution over the same labels.
    pub likelihood: Vec<LabelP>,
    /// The prior used, or `None` (no C17 data: posterior = likelihood).
    pub prior: Option<PriorUse>,
    /// Top posterior label (may be `unknown`).
    pub family: String,
    /// Posterior of `family`, ≤ [`MAX_CONFIDENCE`].
    pub confidence: f64,
    /// Within-family class call, `None` below its gate.
    pub class: Option<ClassCall>,
    /// Open-set score, 0–1; higher = further from every known class.
    pub open_set_score: f64,
    /// H(posterior) / ln K, K = the taxonomy's families + 1 (`unknown`).
    pub entropy_norm: f64,
    /// Deciding stage.
    pub stage: Stage,
    /// Provenance.
    pub provenance: ClassProvenance,
    /// Flags.
    #[serde(default)]
    pub flags: Vec<ClassFlag>,
    /// Machine reason codes, e.g. `low_snr`, `too_short`.
    #[serde(default)]
    pub reasons: Vec<String>,
}

/// A [`Classification`] (or a part of one) broke the contract.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid classification: {0}")]
pub struct InvalidClassification(pub String);

fn bad<T>(msg: impl Into<String>) -> Result<T, InvalidClassification> {
    Err(InvalidClassification(msg.into()))
}

fn unit(x: f64, what: &str) -> Result<(), InvalidClassification> {
    if x.is_finite() && (0.0..=1.0).contains(&x) {
        Ok(())
    } else {
        bad(format!("{what} {x} is not in [0, 1]"))
    }
}

/// Checks `dist` has unique labels accepted by `label_ok`, probabilities in [0, 1] and a sum of
/// 1 ± [`SUM_TOLERANCE`].
fn distribution(
    dist: &[LabelP],
    what: &str,
    label_ok: impl Fn(&str) -> bool,
) -> Result<(), InvalidClassification> {
    let mut sum = 0.0;
    for (i, lp) in dist.iter().enumerate() {
        if !label_ok(&lp.label) {
            return bad(format!("{what}: label {:?} not allowed", lp.label));
        }
        if dist[..i].iter().any(|o| o.label == lp.label) {
            return bad(format!("{what}: label {:?} repeated", lp.label));
        }
        unit(lp.p, &format!("{what}[{}]", lp.label))?;
        sum += lp.p;
    }
    if (sum - 1.0).abs() > SUM_TOLERANCE {
        return bad(format!("{what} sums to {sum}"));
    }
    Ok(())
}

fn p_of(dist: &[LabelP], label: &str) -> Option<f64> {
    dist.iter().find(|lp| lp.label == label).map(|lp| lp.p)
}

fn max_p(dist: &[LabelP]) -> f64 {
    dist.iter().map(|lp| lp.p).fold(0.0, f64::max)
}

/// H(dist) / ln K over `k` labels (missing labels have p = 0).
pub fn entropy_norm(dist: &[LabelP], k: usize) -> f64 {
    if k < 2 {
        return 0.0;
    }
    let h: f64 = dist
        .iter()
        .filter(|lp| lp.p > 0.0)
        .map(|lp| -lp.p * lp.p.ln())
        .sum();
    (h / (k as f64).ln()).clamp(0.0, 1.0)
}

impl Classification {
    /// The taxonomy it names (validated to be released).
    pub fn resolved_taxonomy(&self) -> Result<&'static Taxonomy, InvalidClassification> {
        self.taxonomy.resolve().ok_or_else(|| {
            InvalidClassification(format!("taxonomy {} not released", self.taxonomy))
        })
    }

    /// The legacy `model_version` column: the DL model id when a DL stage decided the family,
    /// else `provenance.rules` (which is `decoder:<id>` for a decoder row).
    pub fn legacy_model_version(&self) -> &str {
        match (&self.stage, &self.provenance.ml) {
            (Stage::Dl, Some(m)) => &m.id,
            _ => &self.provenance.rules,
        }
    }

    /// The top `n` posterior labels, highest first (ties by label).
    pub fn top(&self, n: usize) -> Vec<LabelP> {
        let mut v = self.posterior.clone();
        v.sort_by(|a, b| b.p.total_cmp(&a.p).then_with(|| a.label.cmp(&b.label)));
        v.truncate(n);
        v
    }

    /// Checks every contract invariant of ADR-0016 §2–§3 that a single row can carry.
    pub fn validate(&self) -> Result<(), InvalidClassification> {
        if self.schema != CLASSIFICATION_SCHEMA {
            return bad(format!(
                "schema {} is not {CLASSIFICATION_SCHEMA}",
                self.schema
            ));
        }
        let tax = self.resolved_taxonomy()?;
        let family_label = |l: &str| l == UNKNOWN || tax.is_family(l);

        distribution(&self.posterior, "posterior", family_label)?;
        if p_of(&self.posterior, UNKNOWN).is_none() {
            return bad("posterior has no unknown entry");
        }
        if self.posterior.iter().any(|lp| lp.p >= 1.0) {
            return bad("posterior has an entry of exactly 1");
        }
        distribution(&self.likelihood, "likelihood", family_label)?;
        let same_labels = self.likelihood.len() == self.posterior.len()
            && self
                .likelihood
                .iter()
                .all(|lp| p_of(&self.posterior, &lp.label).is_some());
        if !same_labels {
            return bad("likelihood and posterior label sets differ");
        }

        let Some(p_family) = p_of(&self.posterior, &self.family) else {
            return bad(format!("family {:?} is not a posterior label", self.family));
        };
        if p_family + 1e-12 < max_p(&self.posterior) {
            return bad(format!("family {:?} is not the posterior top", self.family));
        }
        unit(self.confidence, "confidence")?;
        if (self.confidence - p_family).abs() > 1e-9 {
            return bad("confidence differs from the family's posterior");
        }
        if self.confidence > MAX_CONFIDENCE {
            return bad(format!(
                "confidence {} exceeds {MAX_CONFIDENCE}",
                self.confidence
            ));
        }
        if self.family != UNKNOWN && tax.coarse_of(&self.family) != Some(self.coarse) {
            return bad(format!(
                "coarse {:?} does not match family {}",
                self.coarse, self.family
            ));
        }

        if let Some(class) = &self.class {
            let Some(fam) = tax.family(&self.family) else {
                return bad("a class call needs a known family");
            };
            let in_family = |l: &str| fam.classes.contains(&l);
            if !in_family(&class.label) {
                return bad(format!(
                    "class {:?} not in family {}",
                    class.label, fam.name
                ));
            }
            unit(class.p, "class p")?;
            if !class.dist.is_empty() {
                distribution(&class.dist, "class dist", in_family)?;
                let top = p_of(&class.dist, &class.label).unwrap_or(-1.0);
                if (top - class.p).abs() > 1e-9 || top + 1e-12 < max_p(&class.dist) {
                    return bad("class label is not its distribution's top");
                }
            }
        }

        unit(self.open_set_score, "open_set_score")?;
        unit(self.entropy_norm, "entropy_norm")?;
        let expected = entropy_norm(&self.posterior, tax.families.len() + 1);
        if (self.entropy_norm - expected).abs() > 1e-6 {
            return bad(format!(
                "entropy_norm {} is not {expected}",
                self.entropy_norm
            ));
        }

        if let Some(prior) = &self.prior {
            if prior.prior_ref.trim().is_empty() {
                return bad("prior_ref is empty");
            }
            let mut sum = 0.0;
            for l in prior.lambda {
                if !l.is_finite() || l < 0.0 {
                    return bad(format!("prior lambda {l} is invalid"));
                }
                sum += l;
            }
            if (sum - 1.0).abs() > SUM_TOLERANCE {
                return bad(format!("prior lambdas sum to {sum}"));
            }
            if prior.lambda[0] < LAMBDA0_MIN {
                return bad(format!(
                    "prior lambda0 {} is below {LAMBDA0_MIN}",
                    prior.lambda[0]
                ));
            }
            distribution(&prior.dist, "prior dist", |l| tax.is_family(l))?;
        }

        let prov = &self.provenance;
        if prov.rules.trim().is_empty() || prov.thresholds.trim().is_empty() {
            return bad("provenance rules and thresholds are required");
        }
        // T-290: provenance names a feature set. `0` names none at all; `1` is the indeterminate
        // marker of a pre-T-290 writer, which is a legal stored value and never a legal new one —
        // a writer only reaches it by leaving `features_version` at a default it never set.
        if prov.features_version == 0 {
            return bad("provenance.features_version names no feature set");
        }
        if !prov.snr_gate_db.is_finite() || prov.snr_db.is_some_and(|s| !s.is_finite()) {
            return bad("provenance SNR is not finite");
        }
        let is_decoder_rules = prov.rules.starts_with(DECODER_RULES_PREFIX);
        if is_decoder_rules != (self.stage == Stage::Decoder) {
            return bad(format!(
                "stage decoder requires, and only it may use, rules {DECODER_RULES_PREFIX}<id>"
            ));
        }
        let dl_used =
            self.stage == Stage::Dl || self.class.as_ref().is_some_and(|c| c.stage == Stage::Dl);
        if dl_used != prov.ml.is_some() {
            return bad("provenance.ml is required exactly when a DL stage decided");
        }
        if let Some(m) = &prov.ml {
            if m.id.trim().is_empty()
                || m.provider.trim().is_empty()
                || m.precision.trim().is_empty()
            {
                return bad("provenance.ml fields are required");
            }
        }
        if self.reasons.iter().any(|r| r.trim().is_empty()) {
            return bad("empty reason code");
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::ids::TrackId;

    fn lp(label: &str, p: f64) -> LabelP {
        LabelP {
            label: label.into(),
            p,
        }
    }

    /// A valid rank-3 feature-tree FSK classification (shared with the repository tests).
    pub(crate) fn sample(family: &str, stage: Stage) -> Classification {
        let posterior = if family == UNKNOWN {
            vec![lp("fsk", 0.3), lp("unknown", 0.7)]
        } else {
            vec![lp(family, 0.8), lp("unknown", 0.2)]
        };
        let tax = &HK_MOD_V1;
        let rules = match stage {
            Stage::Decoder => "decoder:test".to_owned(),
            _ => "hk-classify/tree@1".to_owned(),
        };
        let confidence = if family == UNKNOWN { 0.7 } else { 0.8 };
        Classification {
            schema: CLASSIFICATION_SCHEMA,
            t: Timestamp::from_unix_nanos(1_789_300_820_000_000_000),
            taxonomy: TaxonomyRef::current(),
            input: None,
            coarse: tax.coarse_of(family).unwrap(),
            entropy_norm: entropy_norm(&posterior, tax.families.len() + 1),
            likelihood: posterior.clone(),
            posterior,
            prior: None,
            family: family.into(),
            confidence,
            class: None,
            open_set_score: 1.0 - confidence,
            stage,
            provenance: ClassProvenance {
                rules,
                // Determinate (T-290): this fixture stands for a row a current writer produced,
                // not a pre-T-290 one, whose `FEATURES_VERSION_INDETERMINATE` names no vector.
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
        }
    }

    /// T-290: `features_version` 1 is the pre-T-290 indeterminate marker rather than "features@1",
    /// such a row still reads back (it is stored, and the table is append-only), and a provenance
    /// naming no feature set at all is refused.
    #[test]
    fn a_features_version_of_one_is_indeterminate_and_zero_is_refused() {
        let mut c = sample("fsk", Stage::FeatureTree);
        assert!(
            c.provenance.names_a_feature_set(),
            "a row a current writer produced names its vector"
        );
        c.provenance.features_version = FEATURES_VERSION_INDETERMINATE;
        assert!(
            !c.provenance.names_a_feature_set(),
            "1 says only `some vector, unrecorded`"
        );
        c.validate()
            .expect("a pre-T-290 row still reads back: it is stored, and never rewritten");
        c.provenance.features_version = 0;
        assert!(c.validate().is_err(), "0 names no feature set at all");
    }

    /// Equality up to float parse precision: serde_json (without `float_roundtrip`) may read a
    /// float back one ulp off, so stored `detail` JSON round-trips to within 1e-12 relative.
    pub(crate) fn assert_close(a: &Classification, b: &Classification) {
        fn close(a: &serde_json::Value, b: &serde_json::Value) -> bool {
            use serde_json::Value as V;
            match (a, b) {
                (V::Number(x), V::Number(y)) => match (x.as_f64(), y.as_f64()) {
                    (Some(x), Some(y)) => (x - y).abs() <= 1e-12 * x.abs().max(y.abs()).max(1.0),
                    _ => x == y,
                },
                (V::Array(x), V::Array(y)) => {
                    x.len() == y.len() && x.iter().zip(y).all(|(x, y)| close(x, y))
                }
                (V::Object(x), V::Object(y)) => {
                    x.len() == y.len()
                        && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| close(v, w)))
                }
                _ => a == b,
            }
        }
        let (va, vb) = (
            serde_json::to_value(a).unwrap(),
            serde_json::to_value(b).unwrap(),
        );
        assert!(close(&va, &vb), "classifications differ:\n{va}\n{vb}");
    }

    /// Every optional part populated.
    fn full() -> Classification {
        let mut c = sample("fsk", Stage::Dl);
        c.input = Some(LinkTarget::Track(TrackId::new()));
        c.class = Some(ClassCall {
            label: "2fsk".into(),
            p: 0.7,
            dist: vec![lp("2fsk", 0.7), lp("gfsk", 0.3)],
            stage: Stage::Dl,
        });
        c.prior = Some(PriorUse {
            prior_ref: "band-plan/us@1".into(),
            lambda: [0.1, 0.6, 0.2, 0.1],
            dist: vec![lp("fsk", 0.9), lp("analog", 0.1)],
        });
        c.provenance.ml = Some(ModelRef {
            id: "amc-fsk@0.1.0#1a2b3c4d".into(),
            provider: "cpu-tract".into(),
            precision: "fp32".into(),
        });
        c.provenance.features_ref = Some("features:0199".into());
        c.provenance.suspect.clipped = true;
        c.provenance.power_mode = Some("low".into());
        c.flags = vec![ClassFlag::PriorMismatch, ClassFlag::SuspectInput];
        c.reasons = vec!["clipped".into()];
        c
    }

    #[test]
    fn samples_validate_and_round_trip_through_serde() {
        for c in [
            sample("fsk", Stage::FeatureTree),
            sample(UNKNOWN, Stage::FeatureTree),
            sample("analog", Stage::Chain),
            sample("pulsed", Stage::Decoder),
            full(),
        ] {
            c.validate().unwrap_or_else(|e| panic!("{e}: {c:?}"));
            let json = serde_json::to_string(&c).unwrap();
            let back: Classification = serde_json::from_str(&json).unwrap();
            assert_close(&back, &c);
            back.validate().unwrap();
        }
        let v = serde_json::to_value(full()).unwrap();
        assert_eq!(v["taxonomy"], "hk-mod@1");
        assert_eq!(v["stage"], "dl");
        assert_eq!(v["coarse"], "digital");
        assert_eq!(v["flags"][0], "prior-mismatch");
        assert_eq!(v["input"]["kind"], "track");
        assert_eq!(full().legacy_model_version(), "amc-fsk@0.1.0#1a2b3c4d");
        assert_eq!(
            sample("fsk", Stage::FeatureTree).legacy_model_version(),
            "hk-classify/tree@1"
        );
    }

    #[test]
    fn unknown_fields_and_unreleased_taxonomies_are_refused() {
        let mut v = serde_json::to_value(sample("fsk", Stage::FeatureTree)).unwrap();
        v["extra"] = serde_json::json!(1);
        assert!(serde_json::from_value::<Classification>(v).is_err());
        let mut c = sample("fsk", Stage::FeatureTree);
        c.taxonomy = "hk-mod@2".parse().unwrap();
        assert!(c.validate().is_err());
    }

    #[test]
    fn top_is_sorted_and_includes_unknown() {
        let c = sample(UNKNOWN, Stage::FeatureTree);
        assert_eq!(c.top(5), vec![lp("unknown", 0.7), lp("fsk", 0.3)]);
        assert_eq!(c.top(1).len(), 1);
    }

    #[test]
    fn validation_catches_each_broken_invariant() {
        type Break = fn(&mut Classification);
        let cases: &[(&str, Break)] = &[
            ("schema", |c| c.schema = 2),
            ("no unknown", |c| {
                c.posterior = vec![lp("fsk", 0.999), lp("analog", 0.001)]
            }),
            ("posterior 1.0", |c| {
                c.posterior = vec![lp("fsk", 1.0), lp("unknown", 0.0)];
                c.likelihood = c.posterior.clone();
            }),
            ("posterior sum", |c| c.posterior[0].p = 0.5),
            ("service label", |c| c.posterior[0].label = "adsb".into()),
            ("class label in posterior", |c| {
                c.posterior[0].label = "2fsk".into()
            }),
            ("repeated", |c| c.posterior[1].label = "fsk".into()),
            ("likelihood labels", |c| {
                c.likelihood[0].label = "analog".into()
            }),
            ("family not top", |c| {
                c.family = UNKNOWN.into();
                c.confidence = 0.2;
            }),
            ("confidence mismatch", |c| c.confidence = 0.7),
            ("coarse", |c| c.coarse = Coarse::Analog),
            ("class outside family", |c| {
                c.class = Some(ClassCall {
                    label: "bpsk".into(),
                    p: 0.9,
                    dist: vec![],
                    stage: Stage::FeatureTree,
                })
            }),
            ("open set", |c| c.open_set_score = 1.5),
            ("entropy", |c| c.entropy_norm = 0.0),
            ("lambda0", |c| {
                c.prior = Some(PriorUse {
                    prior_ref: "p".into(),
                    lambda: [0.0, 1.0, 0.0, 0.0],
                    dist: vec![lp("fsk", 1.0)],
                })
            }),
            ("prior carries unknown", |c| {
                c.prior = Some(PriorUse {
                    prior_ref: "p".into(),
                    lambda: [0.4, 0.6, 0.0, 0.0],
                    dist: vec![lp("fsk", 0.5), lp("unknown", 0.5)],
                })
            }),
            ("decoder rules on a tree row", |c| {
                c.provenance.rules = "decoder:x".into()
            }),
            ("decoder stage without decoder rules", |c| {
                c.stage = Stage::Decoder
            }),
            ("dl without model", |c| c.stage = Stage::Dl),
            ("model without dl", |c| {
                c.provenance.ml = Some(ModelRef {
                    id: "m@1#x".into(),
                    provider: "cpu-tract".into(),
                    precision: "fp32".into(),
                })
            }),
            ("empty reason", |c| c.reasons = vec![" ".into()]),
            ("nan snr", |c| c.provenance.snr_db = Some(f64::NAN)),
        ];
        for (name, f) in cases {
            let mut c = sample("fsk", Stage::FeatureTree);
            f(&mut c);
            assert!(c.validate().is_err(), "{name} should be refused: {c:?}");
        }
        // Capping: a family at exactly MAX_CONFIDENCE is fine, above is not.
        let mut c = sample("fsk", Stage::FeatureTree);
        c.posterior = vec![lp("fsk", 0.9995), lp("unknown", 0.0005)];
        c.likelihood = c.posterior.clone();
        c.confidence = 0.9995;
        c.entropy_norm = entropy_norm(&c.posterior, HK_MOD_V1.families.len() + 1);
        assert!(c.validate().is_err());
    }
}
