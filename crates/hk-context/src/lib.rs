//! hackriff context. It keeps an offline-first cache of external feeds: space weather, gpsjam,
//! lightning, TLEs, SondeHub (C29, ADR-0008). It looks up known-signal priors such as band plans
//! and licence extracts, to separate known from unknown (C17). It correlates local Anomalies with
//! ExternalEvents into ranked Explanations for the attack map (C30).
//!
//! - [`band_table`]: the compact 47 CFR 2.106 allocation table and frequency-interval lookup
//!   (T-019).
//! - [`known_status`]: the family → allocation matcher that turns a lookup into a
//!   [`hk_model::KnownStatus`] (T-019).
//! - [`priors`]: `hk_model::classify::FamilyPriors` sources for ADR-0016 §3's classification-fusion
//!   prior, built from [`band_table`] (T-212).
//! - [`feeds`]: the feed cache (state + raw snapshots on disk, events in the repository), the
//!   [`feeds::FeedFetcher`] seam and the [`feeds::gpsjam`] adapter (T-020).
//! - [`anomaly`]: noise-floor episodes → `Anomaly(noise-floor-rise)` lifecycle (T-020).
//! - [`correlate`]: Anomaly × cached events → ranked Explanations (T-020).
//! - [`gnss_service`]: C36's measured GNSS-service statement reaching C30 as evidence on
//!   blindly-opened anomalies, never as a detection (T-322, ADR-0018).
//! - [`watch`]: the selection-scoped region watch (T-166): new activity inside a watched extent
//!   raises an alert, unless the T-219 relationship rules say the row defers to another one.
//! - [`geo`], [`utc`]: site/distance and UTC date helpers.

pub mod anomaly;
pub mod band_table;
pub mod correlate;
pub mod feeds;
pub mod geo;
pub mod gnss_service;
pub mod known_status;
pub mod priors;
pub mod utc;

// ADR-0012 §11 (pre-added by T-113; the owners fill them in).
pub mod occupancy; // T-118 (engine), T-119 (baseline/novelty/score/site), T-122 (alarm)
pub mod report; // T-121
pub mod signature; // T-201 (ADR-0016 §5): C18 feature aggregation and signature matching
pub mod watch; // T-166 (ADR-0013 §4.9 gap 9): selection-scoped region watch

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
pub use gnss_service::{GnssServiceEvidence, GnssServiceVerdict};
pub use known_status::{PART15_FAMILIES, PriorMatch, is_service_family, match_known_status};
pub use priors::BandPlanFamilyPriors;
pub use watch::{
    StandingRelation, WatchActivity, WatchDecision, WatchRegion, WatchSkip, WatchSkipReason,
    armed_regions, deferring_relation, evaluate as evaluate_watch,
};

#[cfg(test)]
mod tests {
    #[test]
    fn links_against_the_model() {
        let _ = hk_model::ExplanationId::new();
    }
}
