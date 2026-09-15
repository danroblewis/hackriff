//! Attention + memory contracts (ADR-0012; C04 attention scheduler, C12 occupancy baseline, C26
//! spectrum history; M2).
//!
//! These are the shared types every M2 task codes against. They live in hk-model because the
//! producers and consumers sit in crates that must not depend on each other: the scheduler
//! (hk-core) emits observation records and consumes the interestingness trait; the occupancy,
//! baseline and novelty engines (hk-context) produce stats, candidates and alarms; the stores
//! (hk-store) persist them; the API (hk-api) serves them. hk-model is the one crate all of them
//! already link.
//!
//! - [`observation`]: where and when the radio actually observed, and why ([`DwellRecord`],
//!   [`SweepRecord`], [`Reason`], [`Tier`]) — ADR-0012 §1.
//! - [`occupancy`]: ITU-R SM.1880 / SM.2256 occupancy ([`OccupancyStat`], thresholds, RBW
//!   correction, confidence interval, learned channels) — §2.
//! - [`baseline`]: site keying, hour-of-week slots, mergeable slot statistics, maturity — §3.
//! - [`score`]: novelty, the interestingness score S, [`Candidate`] sets and the
//!   [`InterestingnessProvider`] trait C12 implements and C04 consumes — §4.
//! - [`schedule`]: bandit contract (arms, reward per dwell-second, UCB, dwell length) and POI
//!   accounting — §5.
//! - [`report`]: the survey report schema — §6.
//! - [`alarm`]: novelty alarms, hysteresis, dedupe keys, suppression — §7.
//!
//! Wire types reject unknown fields: they are persisted and served, and a misspelt key must fail
//! loudly rather than silently take a default. Every record type has a `validate` that the
//! producer calls before persisting and the store calls on read.
//!
//! [`DwellRecord`]: observation::DwellRecord
//! [`SweepRecord`]: observation::SweepRecord
//! [`Reason`]: observation::Reason
//! [`Tier`]: observation::Tier
//! [`OccupancyStat`]: occupancy::OccupancyStat
//! [`Candidate`]: score::Candidate
//! [`InterestingnessProvider`]: score::InterestingnessProvider

pub mod alarm;
pub mod baseline;
pub mod observation;
pub mod occupancy;
pub mod report;
pub mod schedule;
pub mod score;

/// Schema version carried by every persisted attention/memory record (`schema` field). Bump on
/// any incompatible change; readers refuse newer versions.
pub const ATTENTION_SCHEMA_VERSION: u32 = 1;

/// A record or config failed validation: which field, and why. Messages never echo values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{field}: {reason}")]
pub struct ValidationError {
    /// Offending field (dotted path).
    pub field: &'static str,
    /// What is wrong.
    pub reason: &'static str,
}

/// `Ok` when `ok`, else a [`ValidationError`].
pub(crate) fn ensure(
    ok: bool,
    field: &'static str,
    reason: &'static str,
) -> Result<(), ValidationError> {
    if ok {
        Ok(())
    } else {
        Err(ValidationError { field, reason })
    }
}

/// A finite value in `[lo, hi]`.
pub(crate) fn ensure_in(
    v: f64,
    lo: f64,
    hi: f64,
    field: &'static str,
) -> Result<(), ValidationError> {
    ensure(
        v.is_finite() && v >= lo && v <= hi,
        field,
        "must be finite and in range",
    )
}

/// An optional finite value in `[lo, hi]`.
pub(crate) fn ensure_opt_in(
    v: Option<f64>,
    lo: f64,
    hi: f64,
    field: &'static str,
) -> Result<(), ValidationError> {
    v.map_or(Ok(()), |v| ensure_in(v, lo, hi, field))
}

/// The record's schema version is this build's.
pub(crate) fn ensure_schema(schema: u32) -> Result<(), ValidationError> {
    ensure(
        schema == ATTENTION_SCHEMA_VERSION,
        "schema",
        "unsupported schema version",
    )
}
