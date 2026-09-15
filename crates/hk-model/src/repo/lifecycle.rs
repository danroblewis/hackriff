//! Inventory lifecycle (T-078): candidate → confirmed, user delete, recurrence statistics.
//!
//! - **State** lives in `emitter.lifecycle_state` (default `candidate`); every change appends an
//!   `emitter_lifecycle` row (append-only) naming the author (`auto` rule or `user`), the actor
//!   (rule id or credential fingerprint), the reason and the time.
//! - **Transitions:** `candidate → confirmed` (auto or user), `candidate | confirmed → deleted`
//!   (user only). Nothing returns to `candidate`; confirming a confirmed emitter changes nothing.
//! - **Deleted** rows leave the inventory (`query_inventory` shows them only when asked) and entity
//!   resolution: no sighting is counted into them, merged into them, or matched to them by
//!   fingerprint, context, ledger or re-measurement. A later sighting of the same signal therefore
//!   creates a **new candidate**. A decoded identity held by a deleted row is released to the new
//!   emitter when a sighting claims it (the one-emitter-per-identity rule); decodes, links,
//!   detections, tracks, classification and status history of the deleted row are kept.
//! - The rules that decide auto confirmation live with the pipeline (`hk_pipeline::inventory`);
//!   this module stores the result and serves the evidence ([`Repository::emitter_recurrence`],
//!   [`Repository::identity_decode_evidence`]).
//! - Databases created before T-078 (same pre-release schema version) get the column and table on
//!   open.

use rusqlite::{Connection, OptionalExtension, params};

use super::{RepoError, Repository, blob, enum_parse, enum_text};
use crate::decode::CrcStatus;
use crate::emitter::{
    Appearance, DecodedIdentity, IdentityScheme, LifecycleAuthor, LifecycleChange, LifecycleState,
    Recurrence,
};
use crate::ids::EmitterId;
use crate::region::TimeRange;
use crate::time::Timestamp;

/// Longest actor or reason stored, bytes.
pub const LIFECYCLE_TEXT_MAX: usize = 512;

const ENSURE_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS emitter_lifecycle (
    lifecycle_id  INTEGER PRIMARY KEY,
    emitter_id    BLOB    NOT NULL REFERENCES emitter (emitter_id),
    state         TEXT    NOT NULL CHECK (state IN ('confirmed', 'deleted')),
    previous      TEXT    NOT NULL CHECK (previous IN ('candidate', 'confirmed')),
    author        TEXT    NOT NULL CHECK (author IN ('auto', 'user')),
    actor         TEXT    NOT NULL CHECK (length(actor) > 0),
    reason        TEXT    NOT NULL CHECK (length(reason) > 0),
    t             INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_emitter_lifecycle_emitter
    ON emitter_lifecycle (emitter_id, lifecycle_id);
CREATE TRIGGER IF NOT EXISTS emitter_lifecycle_append_only BEFORE UPDATE ON emitter_lifecycle
    BEGIN SELECT RAISE(ABORT, 'lifecycle history is append-only'); END;";

/// Adds the lifecycle column and history table to a database created before T-078.
pub(super) fn ensure_schema(conn: &Connection) -> Result<(), RepoError> {
    let has_column = conn
        .prepare("SELECT 1 FROM pragma_table_info('emitter') WHERE name = 'lifecycle_state'")?
        .exists([])?;
    if !has_column {
        conn.execute_batch(
            "ALTER TABLE emitter ADD COLUMN lifecycle_state TEXT NOT NULL DEFAULT 'candidate' \
             CHECK (lifecycle_state IN ('candidate', 'confirmed', 'deleted'));",
        )?;
    }
    conn.execute_batch(ENSURE_TABLE)?;
    Ok(())
}

/// Current lifecycle state of an existing emitter row.
pub(super) fn state_of(conn: &Connection, id: EmitterId) -> Result<LifecycleState, RepoError> {
    let s: String = conn
        .prepare_cached("SELECT lifecycle_state FROM emitter WHERE emitter_id = ?1")?
        .query_row([blob(id)], |r| r.get(0))
        .optional()?
        .ok_or_else(|| RepoError::NotFound {
            kind: "emitter",
            id: id.to_string(),
        })?;
    enum_parse(s)
}

/// Whether an emitter row is deleted.
pub(super) fn is_deleted(conn: &Connection, id: EmitterId) -> Result<bool, RepoError> {
    Ok(state_of(conn, id)? == LifecycleState::Deleted)
}

/// Releases `identity` from a deleted emitter holding it, so a new sighting claiming it creates
/// (or reaches) a live emitter. Returns the deleted row it was released from.
pub(super) fn release_deleted_identity(
    conn: &Connection,
    identity: &DecodedIdentity,
) -> Result<Option<EmitterId>, RepoError> {
    let Some(holder) = super::inventory::emitter_id_by_identity(conn, identity)? else {
        return Ok(None);
    };
    if !is_deleted(conn, holder)? {
        return Ok(None);
    }
    conn.prepare_cached(
        "UPDATE emitter SET identity_scheme = NULL, identity_value = NULL, identity_class = NULL \
         WHERE emitter_id = ?1",
    )?
    .execute([blob(holder)])?;
    Ok(Some(holder))
}

/// T-082: a confirmed emitter merged into a candidate confirms the survivor. The survivor's
/// history gets the absorbed row's latest confirmation (author, actor, reason) with the merge
/// named in the reason.
pub(super) fn carry_confirmation(
    conn: &Connection,
    from: EmitterId,
    into: EmitterId,
    t: Timestamp,
) -> Result<(), RepoError> {
    let last: Option<(String, String, String)> = conn
        .prepare_cached(
            "SELECT author, actor, reason FROM emitter_lifecycle \
             WHERE emitter_id = ?1 AND state = 'confirmed' ORDER BY lifecycle_id DESC LIMIT 1",
        )?
        .query_row([blob(from)], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()?;
    let (author, actor, why) = last.unwrap_or_else(|| {
        (
            enum_text(&LifecycleAuthor::Auto).unwrap_or_else(|_| "auto".into()),
            "hk-model/merge".into(),
            "confirmed".into(),
        )
    });
    let mut reason = format!("{why} (merged from emitter {from})");
    if reason.len() > LIFECYCLE_TEXT_MAX {
        let mut n = LIFECYCLE_TEXT_MAX;
        while !reason.is_char_boundary(n) {
            n -= 1;
        }
        reason.truncate(n);
    }
    conn.prepare_cached(
        "INSERT INTO emitter_lifecycle (emitter_id, state, previous, author, actor, reason, t) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?
    .execute(params![
        blob(into),
        enum_text(&LifecycleState::Confirmed)?,
        enum_text(&LifecycleState::Candidate)?,
        author,
        actor,
        reason,
        t.as_unix_nanos()
    ])?;
    conn.prepare_cached("UPDATE emitter SET lifecycle_state = ?1 WHERE emitter_id = ?2")?
        .execute(params![enum_text(&LifecycleState::Confirmed)?, blob(into)])?;
    Ok(())
}

fn check_text(what: &str, v: &str) -> Result<(), RepoError> {
    if v.trim().is_empty() || v.len() > LIFECYCLE_TEXT_MAX {
        return Err(RepoError::Invalid(format!(
            "{what} must be non-empty and at most {LIFECYCLE_TEXT_MAX} bytes"
        )));
    }
    Ok(())
}

impl Repository {
    /// Moves an emitter (a merged id stands for its survivor) to `to`.
    ///
    /// Returns the appended history entry, or `None` when the emitter is already in `to`
    /// (confirming a confirmed emitter). Errors: [`RepoError::NotFound`] for a missing or deleted
    /// emitter; [`RepoError::Invalid`] for a move back to `candidate`, a delete by an auto rule,
    /// or an empty/oversized actor or reason.
    pub fn change_emitter_lifecycle(
        &mut self,
        id: EmitterId,
        to: LifecycleState,
        author: LifecycleAuthor,
        actor: &str,
        reason: &str,
        t: Timestamp,
    ) -> Result<Option<LifecycleChange>, RepoError> {
        check_text("actor", actor)?;
        check_text("reason", reason)?;
        match (to, author) {
            (LifecycleState::Candidate, _) => {
                return Err(RepoError::Invalid(
                    "an inventory entry cannot return to candidate".into(),
                ));
            }
            (LifecycleState::Deleted, LifecycleAuthor::Auto) => {
                return Err(RepoError::Invalid(
                    "only a user deletes an inventory entry".into(),
                ));
            }
            _ => {}
        }
        let tx = self.write_tx()?;
        let not_found = || RepoError::NotFound {
            kind: "emitter",
            id: id.to_string(),
        };
        let live = super::cluster::live_id(&tx, id)?.ok_or_else(not_found)?;
        let previous = state_of(&tx, live)?;
        if previous == LifecycleState::Deleted {
            return Err(not_found());
        }
        if previous == to {
            return Ok(None);
        }
        tx.prepare_cached(
            "INSERT INTO emitter_lifecycle (emitter_id, state, previous, author, actor, reason, t) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?
        .execute(params![
            blob(live),
            enum_text(&to)?,
            enum_text(&previous)?,
            enum_text(&author)?,
            actor,
            reason,
            t.as_unix_nanos()
        ])?;
        tx.prepare_cached("UPDATE emitter SET lifecycle_state = ?1 WHERE emitter_id = ?2")?
            .execute(params![enum_text(&to)?, blob(live)])?;
        tx.commit()?;
        Ok(Some(LifecycleChange {
            emitter_id: live,
            state: to,
            previous,
            author,
            actor: actor.to_owned(),
            reason: reason.to_owned(),
            t,
        }))
    }

    /// Current lifecycle state of an emitter (a merged id reads its survivor's).
    pub fn emitter_lifecycle_state(&self, id: EmitterId) -> Result<LifecycleState, RepoError> {
        let live = super::cluster::live_id(&self.conn, id)?.ok_or_else(|| RepoError::NotFound {
            kind: "emitter",
            id: id.to_string(),
        })?;
        state_of(&self.conn, live)
    }

    /// An emitter's lifecycle history, oldest first (empty while it is an untouched candidate).
    pub fn emitter_lifecycle_history(
        &self,
        id: EmitterId,
    ) -> Result<Vec<LifecycleChange>, RepoError> {
        type Raw = (String, String, String, String, String, i64);
        let rows: Vec<Raw> = {
            let mut stmt = self.conn.prepare_cached(
                "SELECT state, previous, author, actor, reason, t FROM emitter_lifecycle \
                 WHERE emitter_id = ?1 ORDER BY lifecycle_id",
            )?;
            stmt.query_map([blob(id)], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })?
            .collect::<Result<_, _>>()?
        };
        rows.into_iter()
            .map(|(state, previous, author, actor, reason, t)| {
                Ok(LifecycleChange {
                    emitter_id: id,
                    state: enum_parse(state)?,
                    previous: enum_parse(previous)?,
                    author: enum_parse(author)?,
                    actor,
                    reason,
                    t: Timestamp::from_unix_nanos(t),
                })
            })
            .collect()
    }

    /// Recurrence statistics of an emitter from its observation ledger, with the `recent` latest
    /// appearances. Appearances are its track observations, or every source observation when it
    /// has no tracks (e.g. an emitter known only from decodes).
    pub fn emitter_recurrence(
        &self,
        id: EmitterId,
        recent: usize,
    ) -> Result<Recurrence, RepoError> {
        type Raw = (String, i64, i64, i64, Option<f64>);
        let tx = self.read_tx()?;
        let count: i64 = tx
            .prepare_cached("SELECT count FROM emitter WHERE emitter_id = ?1")?
            .query_row([blob(id)], |r| r.get(0))
            .optional()?
            .ok_or_else(|| RepoError::NotFound {
                kind: "emitter",
                id: id.to_string(),
            })?;
        let rows: Vec<Raw> = {
            let mut stmt = tx.prepare_cached(
                "SELECT o.source_kind, o.t_start, o.t_end, o.count, \
                   CASE WHEN o.source_kind = 'track' THEN \
                     (SELECT json_extract(k.body, '$.timing.duty_cycle') FROM track k \
                      WHERE k.track_id = o.source_id) END \
                 FROM emitter_observation o WHERE o.emitter_id = ?1 \
                 ORDER BY o.t_start, o.t_end",
            )?;
            stmt.query_map([blob(id)], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<Result<_, _>>()?
        };
        let any_track = rows.iter().any(|r| r.0 == "track");
        let apps: Vec<Appearance> = rows
            .into_iter()
            .filter(|r| !any_track || r.0 == "track")
            .map(|(_, t0, t1, n, duty)| Appearance {
                time: TimeRange::new(
                    Timestamp::from_unix_nanos(t0),
                    Timestamp::from_unix_nanos(t1),
                ),
                count: n.max(0) as u64,
                duty_cycle: duty.filter(|d| d.is_finite()).map(|d| d.clamp(0.0, 1.0)),
            })
            .collect();
        let first = apps.iter().map(|a| a.time.start).min();
        let last = apps.iter().map(|a| a.time.end).max();
        let span_s = match (first, last) {
            (Some(a), Some(b)) => (b.as_unix_nanos() - a.as_unix_nanos()).max(0) as f64 / 1e9,
            _ => 0.0,
        };
        let on_air_s: f64 = apps
            .iter()
            .map(|a| a.time.duration_ns() as f64 / 1e9 * a.duty_cycle.unwrap_or(0.0))
            .sum();
        let appearances = apps.len() as u64;
        let mut latest = apps;
        latest.sort_by(|a, b| b.time.start.cmp(&a.time.start));
        latest.truncate(recent);
        Ok(Recurrence {
            occurrences: count.max(0) as u64,
            appearances,
            span_s,
            on_air_s,
            duty_cycle: (span_s > 0.0).then(|| (on_air_s / span_s).min(1.0)),
            recent: latest,
        })
    }

    /// The scheme of an emitter's decoded identity and how many CRC-valid decodes carry that
    /// identity, or `None` without an identity. Metadata only: the value never leaves this call.
    pub fn identity_decode_evidence(
        &self,
        id: EmitterId,
    ) -> Result<Option<(IdentityScheme, u64)>, RepoError> {
        let tx = self.read_tx()?;
        let row: Option<(Option<String>, Option<String>)> = tx
            .prepare_cached(
                "SELECT identity_scheme, identity_value FROM emitter WHERE emitter_id = ?1",
            )?
            .query_row([blob(id)], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        let Some((Some(scheme), Some(value))) = row else {
            return Ok(None);
        };
        let n: i64 = tx
            .prepare_cached(
                "SELECT count(*) FROM decode WHERE identity_scheme = ?1 AND identity_value = ?2 \
                 AND crc_status = ?3",
            )?
            .query_row(params![scheme, value, enum_text(&CrcStatus::Valid)?], |r| {
                r.get(0)
            })?;
        Ok(Some((
            scheme.parse().map_err(RepoError::Invalid)?,
            n.max(0) as u64,
        )))
    }
}
