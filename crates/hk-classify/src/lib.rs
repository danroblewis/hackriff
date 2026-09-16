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
//!        ├─ features.rs   Azzouz–Nandi statistics, cumulants, IF shape, spectral moments,
//!        │                cyclic lines, cyclic-prefix correlation — each a value or an abstention
//!        ├─ tree.rs       coarse split and per-family admissibility (physics, not tuning)
//!        ├─ density.rs    class-conditional diagonal Gaussians fitted on the synthetic dev grid
//!        ├─ openset.rs    d² → P(χ²_k ≥ d²); open-set score = 1 − max plausibility
//!        ├─ fuse.rs       C17 prior fusion: λ₀ ≥ 0.1, `unknown` untouched, 10:1 evidence wins
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
//! - **Report certainty.** The posterior is capped at [`MAX_CONFIDENCE`].
//!
//! # Evaluation
//!
//! [`synth`] generates the dev grid (fits the densities) and the acceptance grid (measures
//! accuracy) from **disjoint seed ranges**, plus held-out generators outside the taxonomy that
//! must come back `unknown`. [`eval`] reports accuracy per family and per SNR bin, never as one
//! averaged number. The real-fixture runs are in `tests/`.

pub mod classifier;
pub mod density;
pub mod eval;
pub mod features;
pub mod fuse;
pub mod openset;
pub mod synth;
pub mod thresholds;
pub mod tree;

pub use hk_model::classify::*;

pub use classifier::{Classifier, ClassifyRequest};
pub use density::{DensityModel, FamilyScore};
pub use eval::EvalReport;
pub use features::{FeatureInput, Features, features};
pub use fuse::{FamilyPriorSet, FamilyPriors, NoPriors, StaticPriors, fuse};
pub use thresholds::{FEATURES_VERSION, RULES_VERSION, THRESHOLDS_VERSION, thresholds_of};

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
