//! hackriff context. It keeps an offline-first cache of external feeds: space weather, gpsjam,
//! lightning, TLEs, SondeHub (C29, ADR-0008). It looks up known-signal priors such as band plans
//! and licence extracts, to separate known from unknown (C17). It correlates local Anomalies with
//! ExternalEvents into ranked Explanations for the attack map (C30).
//!
//! - [`band_table`]: the compact 47 CFR 2.106 allocation table and frequency-interval lookup
//!   (T-019).
//! - [`known_status`]: the family → allocation matcher that turns a lookup into a
//!   [`hk_model::KnownStatus`] (T-019).
//! - [`feeds`]: the feed cache (state + raw snapshots on disk, events in the repository), the
//!   [`feeds::FeedFetcher`] seam and the [`feeds::gpsjam`] adapter (T-020).
//! - [`anomaly`]: noise-floor episodes → `Anomaly(noise-floor-rise)` lifecycle (T-020).
//! - [`correlate`]: Anomaly × cached events → ranked Explanations (T-020).
//! - [`geo`], [`utc`]: site/distance and UTC date helpers.

pub mod anomaly;
pub mod band_table;
pub mod correlate;
pub mod feeds;
pub mod geo;
pub mod known_status;
pub mod utc;

pub use anomaly::{
    EpisodeClass, EpisodeExtent, EpisodeSignal, FloorAnomalies, FloorAnomalyConfig,
    LifecycleReport, close_orphaned, signal_from_floor_event,
};
pub use band_table::{AllocationRow, BandTable, FederalStatus, LoadError, Region};
pub use correlate::{
    Candidate, CorrelateError, CorrelationOutcome, Correlator, CorrelatorConfig, StaleEvidence,
};
pub use feeds::{
    DirectoryFetcher, FeedAdapter, FeedCache, FeedError, FeedFetcher, FeedState, OfflineFetcher,
    ingest_snapshot, refresh,
};
pub use geo::Site;
pub use known_status::{PART15_FAMILIES, PriorMatch, is_service_family, match_known_status};

#[cfg(test)]
mod tests {
    #[test]
    fn links_against_the_model() {
        let _ = hk_model::ExplanationId::new();
    }
}
