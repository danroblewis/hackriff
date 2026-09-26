//! Reading an emitter's presence intervals out of the observation ledger (docs/07 §2.27,
//! ADR-0017 stage TM-5). The rule itself is [`crate::presence`]; this is the query behind it.
//!
//! Backed by `idx_emitter_observation_time (emitter_id, t_start, t_end)` (migration 0012), which
//! is the only thing migration 0012 adds.

use rusqlite::params;

use super::{RepoError, Repository, blob};
use crate::ids::{EmitterId, TrackId};
use crate::presence::{
    IdleGap, ObservationSpan, Presence, PresenceInterval, Watched, intervals_from_spans,
    intervals_observed, presence_in_window,
};
use crate::region::TimeRange;
use crate::time::Timestamp;

/// One emitter's raw observation rows, in start order. Index-served by
/// `idx_emitter_observation_time`.
const SPANS_SQL: &str = "SELECT t_start, t_end, count, f_center, live_silence_ns \
     FROM emitter_observation WHERE emitter_id = ?1 ORDER BY t_start, t_end";

/// T-940: the ledger `source_kind` of a live-follow report for a track that has **no** `track`
/// row of its own yet — one whose entry came from a chain (a WFM station) rather than from the
/// live offer.
///
/// A separate kind, and not a `track` row with no sightings, because a `track` row decides where
/// that track's own sightings land: `cluster::resolve` sends a sighting whose source already has a
/// ledger row straight back to that row's emitter as a replay, with no same-emission discount, so
/// the track's closing sighting would count a second occurrence of one continuous emission
/// (`signal_062_session_writes`). A `track-live` row is read by presence and the time-scoped
/// listing, and by nothing that counts: it has no measurement key, `count` 0, and recurrence and
/// relate skip it.
pub const TRACK_LIVE_SOURCE: &str = "track-live";

/// Advances an existing `track` row with a report. Only its span can grow; its emitter, count
/// and measurement key are the sightings' and stay as they are.
const FOLLOW_TRACK_SQL: &str = "UPDATE emitter_observation SET \
       t_start = min(t_start, ?2), t_end = max(t_end, ?3), live_silence_ns = ?4 \
     WHERE source_kind = 'track' AND source_id = ?1";

/// Files a report for a track with no `track` row, as its [`TRACK_LIVE_SOURCE`] row.
const FOLLOW_LIVE_SQL: &str = "INSERT INTO emitter_observation \
     (source_kind, source_id, emitter_id, count, t_start, t_end, measurement, f_center, \
      live_silence_ns) VALUES ('track-live', ?1, ?2, 0, ?3, ?4, NULL, NULL, ?5) \
     ON CONFLICT (source_kind, source_id) DO UPDATE SET \
       t_start = min(t_start, excluded.t_start), t_end = max(t_end, excluded.t_end), \
       live_silence_ns = excluded.live_silence_ns";

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
                live_silence_ns: r.get::<_, Option<i64>>(4)?,
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
    /// latest interval open while the silence **observed** after it is at most `gap` — observed
    /// over `watched`, the band's coverage, or as the tracker reported it for a source it is still
    /// following (T-940, [`intervals_observed`]).
    ///
    /// `gap` is a parameter of the reading, not of the data ([`IdleGap::from_revisit_s`]), so the
    /// same rows re-derive correctly under a different revisit period.
    pub fn presence_intervals(
        &self,
        id: EmitterId,
        gap: IdleGap,
        now: Timestamp,
        watched: &Watched,
    ) -> Result<Vec<PresenceInterval>, RepoError> {
        Ok(intervals_observed(
            &self.observation_spans(id)?,
            gap,
            now,
            watched,
        ))
    }

    /// T-940: the pipeline's report on an **open track it is still following**, filed on the
    /// track's own ledger row against `emitter` — the entry the track's observations are recorded
    /// against. `seen` is the track's measured extent (first member to the end of the last burst
    /// the tracker measured); `silence_ns` is the silence the tracker has **observed** since that
    /// end (0 while a burst is in flight).
    ///
    /// This is what keeps an on-air emitter reading `live` between sightings. It creates no entry,
    /// counts no sighting and touches no lifecycle: an existing `track` row keeps its emitter and
    /// its count and only its span grows; a track with none gets a [`TRACK_LIVE_SOURCE`] row on
    /// `emitter` instead. [`Self::stop_following_track`] ends it.
    pub fn follow_track(
        &mut self,
        emitter: EmitterId,
        track: TrackId,
        seen: TimeRange,
        silence_ns: i64,
    ) -> Result<(), RepoError> {
        // The entry a merge folded `emitter` into, if any: merges move the ledger's rows
        // (`cluster::merge`), so a row filed on the absorbed id afterwards would be read by nobody.
        // An id that names no entry at all is refused rather than filed against nothing.
        let emitter =
            super::cluster::live_id(&self.conn, emitter)?.ok_or_else(|| RepoError::NotFound {
                kind: "emitter",
                id: emitter.to_string(),
            })?;
        let uuid: uuid::Uuid = track.into();
        let id = uuid.into_bytes();
        let (t1, silence) = (seen.end.as_unix_nanos(), silence_ns.max(0));
        let t0 = seen.start.as_unix_nanos().min(t1);
        let updated = self
            .conn
            .prepare_cached(FOLLOW_TRACK_SQL)?
            .execute(params![id, t0, t1, silence])?;
        if updated == 0 {
            self.conn.prepare_cached(FOLLOW_LIVE_SQL)?.execute(params![
                id,
                blob(emitter),
                t0,
                t1,
                silence
            ])?;
        }
        Ok(())
    }

    /// T-940: the pipeline no longer follows `track` (it closed, merged, or its run ended), so its
    /// row's `t_end` is final and the silence after it is read off coverage like any closed
    /// source's.
    pub fn stop_following_track(&mut self, track: TrackId) -> Result<(), RepoError> {
        let uuid: uuid::Uuid = track.into();
        self.conn
            .prepare_cached(
                "UPDATE emitter_observation SET live_silence_ns = NULL \
                 WHERE source_kind IN ('track', ?2) AND source_id = ?1 \
                   AND live_silence_ns IS NOT NULL",
            )?
            .execute(params![uuid.into_bytes(), TRACK_LIVE_SOURCE])?;
        Ok(())
    }

    /// T-940: forgets every live-follow report — at the start of a run, when no track of an earlier
    /// run is being followed any more (a run that was killed never got to stop following its
    /// own). Returns how many rows it released.
    pub fn stop_following_all(&mut self) -> Result<usize, RepoError> {
        Ok(self.conn.execute(
            "UPDATE emitter_observation SET live_silence_ns = NULL \
             WHERE live_silence_ns IS NOT NULL",
            [],
        )?)
    }

    /// The emitter's presence through one view window: intervals intersecting it, time on air
    /// inside it, liveness (`live` / `ended` / `absent`) and the hypothesis `confidence` that
    /// ranks a stopped candidate below a transmitting one (T-251). Nothing here reads the
    /// emitter's `count` (ADR-0017 §5), and nothing here writes: `gap` and `now` are parameters of
    /// the reading, so the same stored rows re-derive both closure and decay.
    pub fn presence(
        &self,
        id: EmitterId,
        window: TimeRange,
        gap: IdleGap,
        now: Timestamp,
    ) -> Result<Presence, RepoError> {
        let intervals = intervals_from_spans(&self.observation_spans(id)?, gap, now);
        Ok(presence_in_window(&intervals, window, gap))
    }
}
