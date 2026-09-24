//! The durable half of C38 shadow mode (ADR-0016 §6, T-844).
//!
//! - [`shadow`]: `<data dir>/ml/shadow/`, one CRC-line NDJSON segment per sample-clock hour, each
//!   line `{Prediction, classical decision, snr_bin, subject}`, bounded at 256 MiB and 30 days,
//!   with per-SNR agreement aggregates maintained as records arrive.
//!
//! This crate does **not** depend on `hk-ml`: a record here is plain data (strings and numbers),
//! and the `hk_ml::host::ShadowSink` implementation that maps a live prediction onto it lives with
//! the producer in `hk-pipeline`. The store therefore cannot be the thing that decides whether a
//! model runs — it only keeps what ran.

pub mod shadow;

pub use shadow::{
    Agreement, AgreementRow, ClassicalDecision, DEFAULT_MAX_AGE_NS, DEFAULT_MAX_BYTES,
    DEFAULT_SHADOW_LIMIT, MAX_SHADOW_LIMIT, SHADOW_SCHEMA, SNR_BIN_DB, ShadowPrediction,
    ShadowQuery, ShadowRecord, ShadowStore, ShadowStoreConfig, ShadowStoreStats, ShadowSubject,
    snr_bin_db,
};
