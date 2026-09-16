//! C17 classification priors: the *source* half of ADR-0016 §3's prior fusion (T-212).
//!
//! `hk_model::classify::fuse` is final and pure (T-211/T-218); this module only builds the
//! [`hk_model::classify::FamilyPriorSet`] it consumes, from the band-plan data [`crate::band_table`]
//! already carries. See [`family_prior`].

pub mod family_prior;

pub use family_prior::BandPlanFamilyPriors;
