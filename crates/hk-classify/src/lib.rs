//! C15 modulation classifier (ADR-0016). **Core interface**: changes are reviewed before merge.
//!
//! T-211 skeleton: no classifier logic yet. The contracts live in [`hk_model::classify`] (so the
//! repository stores and ranks them without a dependency cycle) and are re-exported here:
//! - [`taxonomy`]: `hk-mod@1` as data, lookup ([`family_of`]) and validation;
//! - [`Classification`]: posterior and likelihood-only distributions (both including `unknown`),
//!   open-set score, normalised entropy, deciding [`Stage`] and provenance;
//! - [`rank`]: the arbitration rank ([`ArbRank`]) that picks an emitter's current family.
//!
//! Implementation files (ADR-0016 §10): `features`, `tree`, `density`, `openset`, `pipeline`
//! (T-199), `verify` (T-200), `dl` (T-204).

pub use hk_model::classify::*;

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
    }
}
