//! hackriff context. It keeps an offline-first cache of external feeds: space weather, gpsjam,
//! lightning, TLEs, SondeHub (C29, ADR-0008). It looks up known-signal priors such as band plans
//! and licence extracts, to separate known from unknown (C17). It correlates local Anomalies with
//! ExternalEvents into ranked Explanations for the attack map (C30).
//!
//! - [`band_table`]: the compact 47 CFR 2.106 allocation table and frequency-interval lookup
//!   (T-019).
//! - [`known_status`]: the family → allocation matcher that turns a lookup into a
//!   [`hk_model::KnownStatus`] (T-019).

pub mod band_table;
pub mod known_status;

pub use band_table::{AllocationRow, BandTable, FederalStatus, LoadError, Region};
pub use known_status::{PriorMatch, match_known_status};

#[cfg(test)]
mod tests {
    #[test]
    fn links_against_the_model() {
        let _ = hk_model::ExplanationId::new();
    }
}
