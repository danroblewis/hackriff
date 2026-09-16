//! What inference is allowed to look at (ADR-0016 §6: "Inference runs only on **CFAR-surviving,
//! classified-by-tree** events"; C38 card, "Gating").
//!
//! The gate is a **type**, not a policy a caller has to remember. [`Subject`] has no public
//! constructor: the only way to obtain one is [`admit`], which needs
//!
//! 1. a [`Detection`] — the atomic CFAR measurement (docs/07 §2.9). Nothing else in the system
//!    produces one, so "inference runs only on CFAR survivors" holds by construction: a caller
//!    cannot hand the host a spectrum frame, a live stream, or a hand-picked snippet; and
//! 2. a [`Classification`] from the **classical cascade** that named a family.
//!
//! Both conditions carry their own reason:
//!
//! - **A family, from the tree.** A per-family model refines a class *within* a family and never
//!   chooses the family (ADR-0016 §4.6). With no family there is nothing to refine, so an
//!   abstaining cascade means no inference — not a model asked to guess.
//! - **Not from a model.** A [`Stage::Dl`] row is refused as the input: a learned stage keyed off
//!   another learned stage's output would compound its own error and make the shadow comparison
//!   against the classical stage meaningless.
//!
//! The gate deliberately sets **no thresholds of its own**. SNR gates, suspect flags and
//! abstention are the classical cascade's job (ADR-0016 §2), and they have already been applied
//! to the [`Classification`] this takes: adding a second, differently-tuned filter here would be
//! a threshold invented at the wrong layer.

use hk_model::classify::{Classification, Stage, UNKNOWN};
use hk_model::{Detection, DetectionId};

use crate::MlError;

/// What a prediction is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SubjectKind {
    /// One detection box.
    Detection,
}

/// A subject inference may run on: a CFAR-surviving detection that the classical cascade has
/// classified into a family.
///
/// Constructed only by [`admit`]. The fields are what the shadow record and the host's
/// accounting need; none of them is a decision.
#[derive(Clone, Debug, PartialEq)]
pub struct Subject {
    kind: SubjectKind,
    detection: DetectionId,
    family: String,
    snr_db: Option<f64>,
    stage: Stage,
}

impl Subject {
    /// What kind of thing this is.
    pub fn kind(&self) -> SubjectKind {
        self.kind
    }

    /// The CFAR detection behind it.
    pub fn detection(&self) -> DetectionId {
        self.detection
    }

    /// The family the classical cascade named — the family a model may refine *within*.
    pub fn family(&self) -> &str {
        &self.family
    }

    /// Measured in-band SNR, dB, when the cascade measured one. Shadow aggregates are reported
    /// per SNR bin (ADR-0016 §7), so it travels with the subject.
    pub fn snr_db(&self) -> Option<f64> {
        self.snr_db
    }

    /// The classical stage that named the family.
    pub fn stage(&self) -> Stage {
        self.stage
    }
}

/// The gate. `Ok` means inference may be *requested* for this subject; the mode still decides
/// whether it runs, and shadow still decides nothing.
pub fn admit(detection: &Detection, classical: &Classification) -> Result<Subject, MlError> {
    if classical.family == UNKNOWN || classical.family.trim().is_empty() {
        return Err(MlError::Invalid(
            "the classical cascade named no family: a model never chooses one (ADR-0016 §4.6)"
                .into(),
        ));
    }
    match classical.stage {
        Stage::Dl => {
            return Err(MlError::Invalid(
                "the input classification came from a model: a learned stage never feeds itself"
                    .into(),
            ));
        }
        Stage::TrackShape => {
            return Err(MlError::Invalid(
                "track shape is not a measurement of this box: inference runs on \
                 classified-by-tree events (ADR-0016 §6)"
                    .into(),
            ));
        }
        _ => {}
    }
    Ok(Subject {
        kind: SubjectKind::Detection,
        detection: detection.id,
        family: classical.family.clone(),
        snr_db: classical.provenance.snr_db,
        stage: classical.stage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hk_model::classify::{
        CLASSIFICATION_SCHEMA, ClassProvenance, Coarse, HK_MOD_V1, LabelP, SuspectFlags,
        TaxonomyRef, entropy_norm,
    };
    use hk_model::{DetectionFlags, ProvenanceId, SurveyId, TimeRange, Timestamp};

    fn detection() -> Detection {
        Detection {
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
        }
    }

    fn classification(family: &str, stage: Stage) -> Classification {
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
            stage,
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
        }
    }

    #[test]
    fn a_classified_cfar_detection_is_admitted_and_carries_its_family_and_snr() {
        let d = detection();
        let s = admit(&d, &classification("fsk", Stage::FeatureTree)).unwrap();
        assert_eq!(s.family(), "fsk");
        assert_eq!(s.snr_db(), Some(24.0));
        assert_eq!(s.detection(), d.id);
        assert_eq!(s.kind(), SubjectKind::Detection);
        // A verifier row is still the classical cascade.
        assert!(admit(&d, &classification("psk-qam", Stage::Verifier)).is_ok());
    }

    #[test]
    fn nothing_without_a_family_from_the_classical_cascade_is_admitted() {
        // Abstained: no family to refine, so nothing runs (a model never picks the family).
        assert!(admit(&detection(), &classification(UNKNOWN, Stage::FeatureTree)).is_err());
        // A model's own output: a learned stage never feeds itself.
        assert!(admit(&detection(), &classification("fsk", Stage::Dl)).is_err());
        // Track shape is not a measurement of this box.
        assert!(admit(&detection(), &classification("fsk", Stage::TrackShape)).is_err());
    }
}
