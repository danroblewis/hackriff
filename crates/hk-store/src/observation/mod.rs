//! Observation log store (T-115, ADR-0012 §1.5, §9): hourly append-only segment files of
//! `hk_model::attention::observation::ObservationRecord`, buffered and flushed at most once a
//! minute, bounded by age and byte quota, queried by region and time.
//!
//! - **Layout** ([`segment`]): `<root>/YYYY/MM/DD/HH.log`, one sample-clock hour each (UTC);
//!   lines are `<crc32-hex8> <json>`. A record goes into the hour of its end time; a record that
//!   closes late (a sweep record emitted after a later dwell) goes into the open hour, never into
//!   a sealed one. Each segment starts with the sweep geometries in force, so every segment is
//!   self-contained and retention never orphans a sweep record's hop windows.
//! - **Writes** ([`ObservationStore`]): lines buffer in memory and reach the file when the buffer
//!   reaches [`ObservationLogConfig::flush_bytes`] (256 KiB) or [`ObservationLogConfig::flush_interval`]
//!   (60 s wall time, the only wall-clock use; 5 min in low-power mode) has passed; a segment is
//!   fsynced when its hour seals. Queries read the files plus the unflushed buffer, so they are
//!   never stale.
//! - **Crash recovery:** a torn tail line fails its CRC and is skipped on read; on open the newest
//!   segment's torn tail is truncated, so appends start on a line boundary. At most one flush
//!   interval is lost.
//! - **Retention:** whole hours older than [`ObservationLogConfig::max_age_ns`] (30 days) before
//!   the newest record's **sample time** are deleted, then oldest hours until the log fits
//!   [`ObservationLogConfig::max_bytes`] (512 MiB). Replay time never keys retention on wall time.
//! - **Never blocks the pipeline** ([`ObservationWriter`]): producers hand records to an
//!   [`ObservationQueue`] (bounded; `offer` never waits: a full queue drops and counts) drained by
//!   one writer thread.
//! - **Queries** ([`ObservationStore::query`], [`ObservationStore::totals`]): records overlapping a
//!   frequency × time box, and per-range [`ObservationTotals`](hk_model::attention::observation::ObservationTotals)
//!   (visits, activity-independent visits, observed seconds per tier, longest gap, mean revisit)
//!   under the ADR-0012 §1.4 rule: a range is observed only while it lies entirely inside a covered
//!   extent (usable span minus DC notch).

mod log;
mod query;
pub mod segment;
mod writer;

#[cfg(test)]
mod tests;

pub use log::{ObservationLogConfig, ObservationLogStats, ObservationStore, StallGuard};
pub use query::{
    DEFAULT_RECORD_LIMIT, MAX_RECORD_LIMIT, RecordPage, RecordQuery, Visit, coverage_gaps,
    totals_from_visits,
};
pub use writer::{ObservationQueue, ObservationWriter, RecordTap};
