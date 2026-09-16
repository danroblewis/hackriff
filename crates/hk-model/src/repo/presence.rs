//! Reading an emitter's presence intervals out of the observation ledger (docs/07 §2.27,
//! ADR-0017 stage TM-5). The rule itself is [`crate::presence`]; this is the query behind it.
//!
//! Backed by `idx_emitter_observation_time (emitter_id, t_start, t_end)` (migration 0012), which
//! is the only thing migration 0012 adds.

use rusqlite::params;

use super::{RepoError, Repository, blob};
use crate::ids::EmitterId;
use crate::presence::{
    IdleGap, ObservationSpan, Presence, PresenceInterval, intervals_from_spans, presence_in_window,
};
use crate::region::TimeRange;
use crate::time::Timestamp;

/// One emitter's raw observation rows, in start order. Index-served by
/// `idx_emitter_observation_time`.
const SPANS_SQL: &str = "SELECT t_start, t_end, count, f_center FROM emitter_observation \
     WHERE emitter_id = ?1 ORDER BY t_start, t_end";

/// [`Repository::observation_spans`] on any connection or transaction.
pub(super) fn spans_on(
    conn: &rusqlite::Connection,
    id: EmitterId,
) -> Result<Vec<ObservationSpan>, RepoError> {
    let mut stmt = conn.prepare_cached(SPANS_SQL)?;
    let rows = stmt
        .query_map(params![blob(id)], |r| {
            Ok(ObservationSpan {
                time: TimeRange::new(
                    Timestamp::from_unix_nanos(r.get(0)?),
                    Timestamp::from_unix_nanos(r.get(1)?),
                ),
                count: r.get::<_, i64>(2)?.max(0) as u64,
                f_center_hz: r.get::<_, Option<f64>>(3)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

impl Repository {
    /// The emitter's raw observation rows (one per sighting source), in start order — before
    /// normalisation. An unknown or unobserved emitter has none; that is not an error, because a
    /// row created by a legacy writer carries no interval at all.
    pub fn observation_spans(&self, id: EmitterId) -> Result<Vec<ObservationSpan>, RepoError> {
        spans_on(&self.conn, id)
    }

    /// The emitter's ordered set of **disjoint presence intervals** (docs/07 §2.27): overlapping
    /// source rows normalised together, a silence longer than `gap` closing an interval, and the
    /// latest interval open while `now − t_end ≤ gap`.
    ///
    /// `gap` is a parameter of the reading, not of the data ([`IdleGap::from_revisit_s`]), so the
    /// same rows re-derive correctly under a different revisit period.
    pub fn presence_intervals(
        &self,
        id: EmitterId,
        gap: IdleGap,
        now: Timestamp,
    ) -> Result<Vec<PresenceInterval>, RepoError> {
        Ok(intervals_from_spans(&self.observation_spans(id)?, gap, now))
    }

    /// The emitter's presence through one view window: intervals intersecting it, time on air
    /// inside it, and liveness (`live` / `ended` / `absent`). Nothing here reads the emitter's
    /// `count` (ADR-0017 §5).
    pub fn presence(
        &self,
        id: EmitterId,
        window: TimeRange,
        gap: IdleGap,
        now: Timestamp,
    ) -> Result<Presence, RepoError> {
        let intervals = self.presence_intervals(id, gap, now)?;
        Ok(presence_in_window(&intervals, window))
    }
}
