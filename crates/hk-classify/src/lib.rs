//! C15 modulation classifier (ADR-0016). **Core interface**: changes are reviewed before merge.
//!
//! The contracts live in [`hk_model::classify`] (so the repository stores and ranks them without a
//! dependency cycle) and are re-exported here: the taxonomy `hk-mod@1` ([`taxonomy`]), the
//! [`Classification`] row and the arbitration [`rank`].
//!
//! # The classical cascade (T-199)
//!
//! ```text
//!   normalised snippet + C13 ParameterSet + C14 SymbolParameters
//!        │
//!        ├─ fm.rs         the broadcast-FM pre-classification rule: a 19 kHz pilot and its
//!        │                suppressed 38/57 kHz subcarriers, priced by their own false-alarm
//!        │                probability (T-970) — a likelihood, beside the SNR gate, never a veto
//!        ├─ features.rs   Azzouz–Nandi statistics, cumulants, IF shape, spectral moments,
//!        │                cyclic lines, cyclic-prefix correlation — each a value or an abstention
//!        ├─ tree.rs       coarse split and per-family admissibility (physics, not tuning)
//!        ├─ density.rs    class-conditional diagonal Gaussians fitted on the synthetic dev grid
//!        ├─ openset.rs    d² → P(χ²_k ≥ d²); open-set score = 1 − max plausibility
//!        ├─ hk_model::classify::fuse  C17 prior fusion: λ₀ ≥ 0.1, `unknown` untouched,
//!        │                10:1 evidence wins (re-exported as [`fuse`])
//!        └─ classifier.rs gates, abstention rules, within-family class, provenance → Classification
//! ```
//!
//! What a caller gets is a [`Classification`] with **both** distributions (evidence-only and
//! posterior), an explicit `unknown`, the deciding stage and full provenance. It is written to an
//! emitter with `Repository::record_classification` at [`ArbRank::Classifier`] (rank 3): below a
//! CRC-valid decode and a lock-verified chain label, above track shape.
//!
//! # What this stage will not do
//!
//! - **Claim a family below its SNR gate.** The share goes to `unknown` (reason `low_snr`).
//! - **Snap an out-of-taxonomy signal to the nearest family.** The χ² open set answers `unknown`.
//! - **Let a band-plan prior decide against the evidence.** [`fuse`](fuse::fuse) tempers the prior
//!   until a 10:1 likelihood call stands, and flags `prior-mismatch`.
//! - **Report certainty.** The posterior is capped at [`MAX_CONFIDENCE`], and an **abstention** at
//!   [`MAX_UNKNOWN_CONFIDENCE`]: `unknown` is a real outcome, not a confident claim about the
//!   world, so it is never reported on the scale a positive call reaches (T-970).
//!
//! # Evaluation
//!
//! [`synth`] generates the dev grid (fits the densities) and the acceptance grid (measures
//! accuracy) from **disjoint seed ranges**, plus held-out generators outside the taxonomy that
//! must come back `unknown`. [`eval`] reports accuracy per family and per SNR bin, never as one
//! averaged number. The real-fixture runs are in `tests/`.

pub mod classifier;
pub mod density;
pub mod dl;
pub mod eval;
pub mod features;
pub mod fm;
pub mod harness;
pub mod openset;
pub mod structure;
pub mod symbols;
pub mod synth;
pub mod thresholds;
pub mod tree;
pub mod verify;

pub use hk_model::classify::*;

/// C17 prior fusion. It lives in [`hk_model::classify::fuse`] (T-218) because both sides of the
/// fusion need it — this crate produces the likelihood, `hk-context` (T-212) the prior — and is
/// re-exported here so `hk_classify::fuse::…` keeps working.
pub use hk_model::classify::fuse;

/// The MAUTO seed interface (T-215, ADR-0016 §8): ordered decode hypotheses with a prune flag and
/// the reserved open-search share. It lives in [`hk_model::classify::seed`] with the other
/// contracts and is re-exported here. **Interface only — it runs no search.**
pub use hk_model::classify::seed::{
    BudgetHint, ClusterPipeline, ClusterSeed, Hypothesis, OPEN_SEARCH_MIN_SHARE,
    PRUNE_LIKELIHOOD_SHARE, SearchSeed, SeedBoost, SeedInputs,
};

pub use classifier::{Classifier, ClassifyRequest};
pub use density::{DensityModel, FamilyScore};
pub use dl::{DL_INPUT_DIM, DlStage, ShadowRecord, classify_shadowed, dl_input};
pub use eval::EvalReport;
pub use features::{FeatureInput, Features, features};
pub use fm::{MpxEvidence, WfmCall, mpx_evidence, wfm_rule};
// `FamilyPriorSet`, `FamilyPriors`, `NoPriors`, `StaticPriors` and the `fuse` function come
// through the `hk_model::classify` re-export above (T-218).
pub use harness::{
    CoverageGap, FamilyCoverage, GridSize, Harness, OtaTruth, Report, RunMeta, SeedGuard, Snippet,
    Split, VerifierRow, VerifierUnaccounted,
};
pub use structure::{ENVELOPE_SNR_UNCERTAINTY_DB, ModulationStructure, modulation_structure};
pub use symbols::{SymbolEstimator, SymbolWindow};
pub use thresholds::{FEATURES_VERSION, RULES_VERSION, THRESHOLDS_VERSION, thresholds_of};
pub use verify::{
    SkipReason, VERIFIER_CONFIRMED, VERIFIER_RERANKED, VERIFIER_VERSION, VerifyInput,
    VerifyOutcome, verify,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contracts_are_reachable_through_the_crate() {
        taxonomy::HK_MOD_V1.validate().unwrap();
        assert_eq!(TaxonomyRef::current().to_string(), "hk-mod@1");
        assert_eq!(family_of("2fsk", &TaxonomyRef::current()), Some("fsk"));
        assert!(ArbRank::User < ArbRank::TrackShape);
        assert_eq!(Stage::FeatureTree.as_str(), "feature-tree");
        assert_eq!(RULES_VERSION, "hk-classify/tree@1");
    }
}
