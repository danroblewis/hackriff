//! Per-family gates and decision thresholds, `thresholds@1` (ADR-0016 §2). **Core interface.**
//!
//! Every number here is **a priori**: S5-measured where the spike measured it, otherwise a stated
//! literature guess marked *unverified* in the ADR. They are never tuned against the acceptance
//! split (the blind rule, docs/10 §3.2); changing one needs an ADR amendment citing dev evidence.
//!
//! These live in `hk-model` (not in the classifier crate) because ADR-0016's decision table puts
//! the taxonomy, the `Classification` contract, `fuse` **and** the thresholds in `hk_model::
//! classify`: the control API serves them at `GET /api/taxonomy`, C17's prior source (hk-context,
//! T-212) needs the same evidence-dominance constant, and neither may depend on `hk-classify`.
//! `hk_classify::thresholds` re-exports every item here, so the classifier's own paths are
//! unchanged.

/// Threshold-set version written into [`super::ClassProvenance::thresholds`].
pub const THRESHOLDS_VERSION: &str = "thresholds@1";

/// Gates and reporting floors of one family.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FamilyThresholds {
    /// Family label in `hk-mod@1`.
    pub family: &'static str,
    /// In-band SNR gate of the analysed extent, dB (C13 definition). Below it the family
    /// contributes no likelihood mass and its share moves to `unknown` with reason `low_snr`.
    /// `None` = no SNR gate (the noise-like test is a shape test, not an SNR test).
    pub snr_gate_db: Option<f64>,
    /// Extra dB above the SNR gate before a within-family class is called.
    pub class_gate_db: f64,
    /// Smallest posterior at which the family is reported instead of `unknown`.
    pub min_confidence: f64,
    /// Largest open-set score at which the family may still be reported.
    pub open_set_max: f64,
}

/// `thresholds@1` (ADR-0016 §2). FSK/OOK 20 dB and PSK 15 dB are the S5 floors
/// (`spikes/s5-blind-estimation/REPORT.md` §3); analog, CSS, OFDM, pulsed and DSSS are
/// *unverified* literature guesses.
pub const THRESHOLDS: &[FamilyThresholds] = &[
    FamilyThresholds {
        family: "analog",
        snr_gate_db: Some(10.0),
        class_gate_db: 0.0,
        min_confidence: 0.6,
        open_set_max: 0.5,
    },
    FamilyThresholds {
        family: "ook-ask",
        snr_gate_db: Some(20.0),
        class_gate_db: 3.0,
        min_confidence: 0.6,
        open_set_max: 0.5,
    },
    FamilyThresholds {
        family: "fsk",
        snr_gate_db: Some(20.0),
        class_gate_db: 3.0,
        min_confidence: 0.6,
        open_set_max: 0.5,
    },
    FamilyThresholds {
        family: "psk-qam",
        snr_gate_db: Some(15.0),
        class_gate_db: 5.0,
        min_confidence: 0.6,
        open_set_max: 0.5,
    },
    FamilyThresholds {
        family: "ofdm",
        snr_gate_db: Some(10.0),
        class_gate_db: 0.0,
        min_confidence: 0.6,
        open_set_max: 0.5,
    },
    FamilyThresholds {
        family: "css",
        snr_gate_db: Some(10.0),
        class_gate_db: 0.0,
        min_confidence: 0.6,
        open_set_max: 0.5,
    },
    FamilyThresholds {
        family: "dsss",
        snr_gate_db: Some(10.0),
        class_gate_db: 0.0,
        min_confidence: 0.7,
        open_set_max: 0.4,
    },
    FamilyThresholds {
        family: "pulsed",
        snr_gate_db: Some(10.0),
        class_gate_db: 0.0,
        min_confidence: 0.6,
        open_set_max: 0.5,
    },
    FamilyThresholds {
        family: "noise-like",
        snr_gate_db: None,
        class_gate_db: 0.0,
        min_confidence: 0.7,
        open_set_max: 1.0,
    },
];

/// The thresholds of `family`, or `None` when it is not in `hk-mod@1`.
pub fn thresholds_of(family: &str) -> Option<&'static FamilyThresholds> {
    THRESHOLDS.iter().find(|t| t.family == family)
}

/// Whether a family's SNR gate admits `snr_db` (an unmeasured SNR never passes a gated family:
/// "not measured" is not "ruled out", so the mass goes to `unknown`, not to the family).
pub fn passes_gate(family: &str, snr_db: Option<f64>) -> bool {
    match thresholds_of(family).and_then(|t| t.snr_gate_db) {
        None => true,
        Some(gate) => snr_db.is_some_and(|s| s >= gate),
    }
}

/// Likelihood ratio at which evidence dominates a prior (ADR-0016 §3): a prior can never flip a
/// call the likelihood makes at 10:1 or better.
pub const EVIDENCE_DOMINANCE_RATIO: f64 = 10.0;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::HK_MOD_V1;

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
        // Every threshold row names a family of the current taxonomy (no orphans).
        for t in THRESHOLDS {
            assert!(HK_MOD_V1.is_family(t.family), "{}", t.family);
        }
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
