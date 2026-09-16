//! The classifier's own rule constants, plus the `thresholds@1` contract re-exported.
//!
//! **`thresholds@1` itself lives in [`hk_model::classify::thresholds`]** (T-218): ADR-0016's
//! decision table puts the taxonomy, the `Classification` contract, `fuse` and the thresholds in
//! `hk_model::classify`, because the control API serves them at `GET /api/taxonomy` and C17's
//! prior source (hk-context, T-212) needs the same evidence-dominance constant — and neither may
//! depend on this crate. Every item is re-exported here, so `crate::thresholds::…` paths are
//! unchanged.
//!
//! What stays here is what belongs to *this* rule set rather than to the contract: the rule-set
//! version, the feature-set version (re-exported from [`crate::features`], which defines it) and
//! the abstention thresholds of the feature tree.

pub use hk_model::classify::thresholds::{
    EVIDENCE_DOMINANCE_RATIO, FamilyThresholds, THRESHOLDS, THRESHOLDS_VERSION, passes_gate,
    thresholds_of,
};

/// Rule-set version written into [`hk_model::classify::ClassProvenance::rules`].
pub const RULES_VERSION: &str = "hk-classify/tree@1";

/// Feature-set version written into [`hk_model::classify::ClassProvenance::features_version`]:
/// **the same constant** as [`crate::features::FEATURES_VERSION`], re-exported rather than
/// restated (T-290).
///
/// It was a second `const` here until T-290, and it read `1` while the feature vector had moved to
/// `2` (T-248) and `3` (T-286). So every `Classification` this crate produced named a feature set
/// that does not exist, while [`crate::density`] stamped its fitted models with the real version —
/// the rows and the densities they were scored against disagreed, and nothing could notice. The
/// version of the feature vector is the feature module's to declare; there is now one definition
/// and no way for the two to drift.
pub use crate::features::FEATURES_VERSION;

/// Normalised entropy above which an unconfident top family abstains (ADR-0016 §4.4).
pub const ABSTAIN_ENTROPY: f64 = 0.9;

/// Likelihood share below which an unconfident top family abstains (ADR-0016 §4.4).
pub const ABSTAIN_TOP_SHARE: f64 = 0.5;

#[cfg(test)]
mod tests {
    use super::*;
    use hk_model::classify::HK_MOD_V1;

    #[test]
    fn every_hk_mod_family_has_thresholds_and_the_s5_floors_are_kept() {
        for f in HK_MOD_V1.families {
            let t = thresholds_of(f.name).unwrap_or_else(|| panic!("no thresholds for {}", f.name));
            assert!((0.5..=0.95).contains(&t.min_confidence), "{}", f.name);
        }
        assert_eq!(thresholds_of("fsk").unwrap().snr_gate_db, Some(20.0));
        assert_eq!(thresholds_of("ook-ask").unwrap().snr_gate_db, Some(20.0));
        assert_eq!(thresholds_of("psk-qam").unwrap().snr_gate_db, Some(15.0));
        assert_eq!(thresholds_of("noise-like").unwrap().snr_gate_db, None);
    }

    /// T-290: the version a `Classification` reports has to be a feature set that exists.
    ///
    /// Three things have to agree, and until T-290 the first disagreed with the other two: the
    /// constant written into provenance, the feature vector's own version, and the version stamped
    /// on the shipped densities those rows are scored against. It also has to be distinguishable
    /// from the marker a pre-T-290 row carries, or a current row reads as an unidentifiable one.
    #[test]
    fn provenance_reports_the_feature_vector_the_shipped_densities_were_fitted_on() {
        assert_eq!(FEATURES_VERSION, crate::features::FEATURES_VERSION);
        assert_eq!(
            crate::density::DensityModel::builtin().features_version,
            FEATURES_VERSION
        );
        assert_eq!(
            crate::density::DensityModel::builtin_below_gate().features_version,
            FEATURES_VERSION
        );
        const {
            assert!(
                FEATURES_VERSION > hk_model::classify::FEATURES_VERSION_INDETERMINATE,
                "a row this build writes must never read as a pre-T-290 row of unknown features"
            )
        };
    }

    #[test]
    fn a_gate_refuses_an_unmeasured_snr_but_not_an_ungated_family() {
        assert!(!passes_gate("fsk", None));
        assert!(!passes_gate("fsk", Some(19.9)));
        assert!(passes_gate("fsk", Some(20.0)));
        assert!(passes_gate("noise-like", None));
        assert!(
            passes_gate("not-a-family", None),
            "unknown families are ungated"
        );
    }
}
