//! Observation log store (T-115, ADR-0012 §1.5, §9): hourly append-only segment files of
//! `hk_model::attention::observation::ObservationRecord`, buffered and flushed at most once a
//! minute, bounded by age and byte quota, queried by region and time.
//!
//! Stub pre-added by T-113.
