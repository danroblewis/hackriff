//! Tracks and the Emitter inventory (C10, C27).

use rusqlite::{Connection, OptionalExtension, Row, params};
use uuid::Uuid;

use super::{
    RepoError, Repository, blob, body_by_id, bump_extent, enum_parse, enum_text, finite, int,
    opt_blob, region_bounds,
};
use crate::detection::{Track, TrackState};
use crate::emitter::{
    Classification, DecodedIdentity, Emitter, EmitterLink, EmitterObservation, Identity,
    KnownStatus, KnownStatusChange, LinkTarget, StatusAuthor,
};
use crate::ids::{
    AnnotationId, AnomalyId, DecodeId, DemodulationId, DetectionId, EmitterId, ExplanationId,
    RecordingId, TrackId,
};
use crate::region::{FreqRange, Region};
use crate::time::Timestamp;

/// Result of [`Repository::upsert_emitter_observation`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EmitterUpsert {
    /// Emitter the observation was merged into. Differs from the observation's `emitter_id`
    /// when its decoded identity already named another emitter.
    pub emitter_id: EmitterId,
    /// A new inventory entry was created.
    pub created: bool,
}

/// Columns read for an Emitter; the last is the current status from the append-only history.
macro_rules! emitter_select {
    () => {
        "emitter_id, f_center, bandwidth, first_seen, last_seen, count, fingerprint, \
         identity_scheme, identity_value, \
         (SELECT s.status FROM emitter_status s WHERE s.emitter_id = emitter.emitter_id \
          ORDER BY s.status_id DESC LIMIT 1)"
    };
}

const EMITTER_INSERT_COLUMNS: &str = "emitter_id, f_center, bandwidth, first_seen, last_seen, \
     count, fingerprint, identity_scheme, identity_value, f_lo, f_hi";

/// The emitter region query. Parameters: f_lo min, query hi, query lo, query t0, query t1.
pub(super) const EMITTER_REGION_SQL: &str = concat!(
    "SELECT ",
    emitter_select!(),
    " FROM emitter \
     WHERE f_lo BETWEEN ?1 AND ?2 AND f_hi >= ?3 AND last_seen >= ?4 AND first_seen <= ?5 \
     AND merged_into IS NULL \
     ORDER BY last_seen DESC, emitter_id"
);

type EmitterRaw = (
    [u8; 16],
    f64,
    f64,
    i64,
    i64,
    i64,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

fn emitter_raw(r: &Row<'_>) -> rusqlite::Result<EmitterRaw> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
        r.get(7)?,
        r.get(8)?,
        r.get(9)?,
    ))
}

pub(super) fn link_kind(target: &LinkTarget) -> (&'static str, Uuid) {
    match *target {
        LinkTarget::Track(id) => ("track", id.into()),
        LinkTarget::Detection(id) => ("detection", id.into()),
        LinkTarget::Recording(id) => ("recording", id.into()),
        LinkTarget::Demodulation(id) => ("demodulation", id.into()),
        LinkTarget::Decode(id) => ("decode", id.into()),
        LinkTarget::Anomaly(id) => ("anomaly", id.into()),
        LinkTarget::Explanation(id) => ("explanation", id.into()),
        LinkTarget::Annotation(id) => ("annotation", id.into()),
    }
}

pub(super) fn link_target(kind: &str, id: Uuid) -> Result<LinkTarget, RepoError> {
    Ok(match kind {
        "track" => LinkTarget::Track(TrackId::from_uuid(id)),
        "detection" => LinkTarget::Detection(DetectionId::from_uuid(id)),
        "recording" => LinkTarget::Recording(RecordingId::from_uuid(id)),
        "demodulation" => LinkTarget::Demodulation(DemodulationId::from_uuid(id)),
        "decode" => LinkTarget::Decode(DecodeId::from_uuid(id)),
        "anomaly" => LinkTarget::Anomaly(AnomalyId::from_uuid(id)),
        "explanation" => LinkTarget::Explanation(ExplanationId::from_uuid(id)),
        "annotation" => LinkTarget::Annotation(AnnotationId::from_uuid(id)),
        other => return Err(RepoError::Invalid(format!("unknown link kind {other:?}"))),
    })
}

fn emitter_exists(conn: &Connection, id: EmitterId) -> Result<bool, RepoError> {
    Ok(conn
        .prepare_cached("SELECT 1 FROM emitter WHERE emitter_id = ?1")?
        .query_row([blob(id)], |_| Ok(()))
        .optional()?
        .is_some())
}

pub(super) fn emitter_id_by_identity(
    conn: &Connection,
    identity: &DecodedIdentity,
) -> Result<Option<EmitterId>, RepoError> {
    Ok(conn
        .prepare_cached(
            "SELECT emitter_id FROM emitter WHERE identity_scheme = ?1 AND identity_value = ?2",
        )?
        .query_row(params![identity.scheme.as_string(), identity.value], |r| {
            r.get::<_, [u8; 16]>(0)
        })
        .optional()?
        .map(|b| EmitterId::from_uuid(Uuid::from_bytes(b))))
}

fn identity_columns(identity: &Identity) -> (Option<String>, Option<String>) {
    match identity {
        Identity::Unknown => (None, None),
        Identity::Decoded(d) => (Some(d.scheme.as_string()), Some(d.value.clone())),
    }
}

pub(super) fn identity_label(identity: &DecodedIdentity) -> String {
    format!("{}:{}", identity.scheme, identity.value)
}

fn insert_classification(
    conn: &Connection,
    emitter_id: EmitterId,
    c: &Classification,
) -> Result<(), RepoError> {
    conn.prepare_cached(
        "INSERT INTO emitter_classification (emitter_id, t, family, confidence, \
         open_set_score, model_version) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?
    .execute(params![
        blob(emitter_id),
        c.t.as_unix_nanos(),
        c.family,
        finite(c.confidence, "confidence")?,
        finite(c.open_set_score, "open_set_score")?,
        c.model_version
    ])?;
    Ok(())
}

pub(super) fn insert_status(conn: &Connection, c: &KnownStatusChange) -> Result<(), RepoError> {
    conn.prepare_cached(
        "INSERT INTO emitter_status (emitter_id, status, prior_ref, reason, t, author) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?
    .execute(params![
        blob(c.emitter_id),
        enum_text(&c.status)?,
        c.prior_ref,
        c.reason,
        c.t.as_unix_nanos(),
        enum_text(&c.author)?
    ])?;
    Ok(())
}

impl Repository {
    // ---- Track ----

    /// Inserts or replaces a Track aggregate (it grows as detections arrive).
    pub fn upsert_track(&mut self, track: &Track) -> Result<(), RepoError> {
        let (state, merged_into) = match track.state {
            TrackState::Open => ("open", None),
            TrackState::Closed => ("closed", None),
            TrackState::MergedInto(t) => ("merged", Some(blob(t))),
        };
        self.conn.execute(
            "INSERT INTO track (track_id, state, merged_into, split_from, t_start, t_end, \
             f_center, updated_at, body) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
             ON CONFLICT (track_id) DO UPDATE SET state = excluded.state, \
             merged_into = excluded.merged_into, split_from = excluded.split_from, \
             t_start = excluded.t_start, t_end = excluded.t_end, f_center = excluded.f_center, \
             updated_at = excluded.updated_at, body = excluded.body",
            params![
                blob(track.id),
                state,
                merged_into,
                opt_blob(track.split_from),
                track.time.start.as_unix_nanos(),
                track.time.end.as_unix_nanos(),
                track.f_center_hz,
                track.updated_at.as_unix_nanos(),
                serde_json::to_string(track)?
            ],
        )?;
        Ok(())
    }

    /// One Track.
    pub fn track(&self, id: TrackId) -> Result<Track, RepoError> {
        body_by_id(
            &self.conn,
            "SELECT body FROM track WHERE track_id = ?1",
            blob(id),
            "track",
        )
    }

    /// Appends detections to a track's membership (idempotent).
    pub fn link_detections_to_track(
        &mut self,
        track_id: TrackId,
        detections: &[DetectionId],
        linked_at: Timestamp,
    ) -> Result<(), RepoError> {
        let tx = self.write_tx()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR IGNORE INTO track_detection (track_id, detection_id, linked_at) \
                 VALUES (?1, ?2, ?3)",
            )?;
            for d in detections {
                stmt.execute(params![blob(track_id), blob(*d), linked_at.as_unix_nanos()])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Member detections of a track, ordered by detection start time (then id).
    pub fn track_detections(&self, track_id: TrackId) -> Result<Vec<DetectionId>, RepoError> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT td.detection_id FROM track_detection td \
             JOIN detection d ON d.detection_id = td.detection_id \
             WHERE td.track_id = ?1 ORDER BY d.t_start, td.detection_id",
        )?;
        let ids = stmt
            .query_map([blob(track_id)], |r| r.get::<_, [u8; 16]>(0))?
            .map(|b| b.map(|b| DetectionId::from_uuid(Uuid::from_bytes(b))))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    // ---- Emitter ----

    /// Inserts a new Emitter with its classification history and tags. `e.known_status` becomes
    /// the first status-history entry (author `system`, at `first_seen`).
    pub fn insert_emitter(&mut self, e: &Emitter) -> Result<(), RepoError> {
        let tx = self.write_tx()?;
        if let Identity::Decoded(d) = &e.identity
            && let Some(existing) = emitter_id_by_identity(&tx, d)?
        {
            return Err(RepoError::IdentityConflict {
                identity: identity_label(d),
                existing,
            });
        }
        let freq = e.freq();
        let (scheme, value) = identity_columns(&e.identity);
        tx.prepare_cached(&format!(
            "INSERT INTO emitter ({EMITTER_INSERT_COLUMNS}) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)"
        ))?
        .execute(params![
            blob(e.id),
            finite(e.f_center_hz, "f_center_hz")?,
            finite(e.bandwidth_hz, "bandwidth_hz")?,
            e.first_seen.as_unix_nanos(),
            e.last_seen.as_unix_nanos(),
            int(e.count, "count")?,
            (!e.fingerprint.is_null())
                .then(|| serde_json::to_string(&e.fingerprint))
                .transpose()?,
            scheme,
            value,
            freq.lo_hz,
            freq.hi_hz
        ])?;
        insert_status(
            &tx,
            &KnownStatusChange {
                emitter_id: e.id,
                status: e.known_status,
                prior_ref: None,
                reason: "initial status at insert".into(),
                t: e.first_seen,
                author: StatusAuthor::System,
            },
        )?;
        for c in &e.classifications {
            insert_classification(&tx, e.id, c)?;
        }
        for tag in &e.tags {
            tx.prepare_cached("INSERT INTO emitter_tag (emitter_id, tag) VALUES (?1, ?2)")?
                .execute(params![blob(e.id), tag])?;
        }
        bump_extent(&tx, "emitter", freq.width_hz(), 0)?;
        tx.commit()?;
        Ok(())
    }

    /// Merges sightings into the inventory (docs/07 §2.11; groundwork for T-018).
    ///
    /// The target emitter is, in order: the emitter already holding the observation's decoded
    /// identity, else `obs.emitter_id`. If the target exists, `count` is summed, `first_seen` /
    /// `last_seen` widen to span both, the current frequency/bandwidth follow the most recent
    /// sighting, and an unknown identity is filled in. Otherwise a new emitter is created with
    /// an initial `known_status: unknown` history entry. Replaying an out-of-order older session
    /// keeps the newer frequency.
    ///
    /// Fails with [`RepoError::IdentityConflict`] if the identity belongs to a different emitter
    /// than an existing `obs.emitter_id`, or the target already has a different identity; the
    /// merge decision belongs to entity resolution.
    pub fn upsert_emitter_observation(
        &mut self,
        obs: &EmitterObservation,
    ) -> Result<EmitterUpsert, RepoError> {
        if obs.seen.end < obs.seen.start {
            return Err(RepoError::Invalid(
                "observation ends before it starts".into(),
            ));
        }
        finite(obs.f_center_hz, "f_center_hz")?;
        finite(obs.bandwidth_hz, "bandwidth_hz")?;
        let tx = self.write_tx()?;
        // A merged emitter id (T-018) stands for its survivor.
        let requested = super::cluster::live_id(&tx, obs.emitter_id)?.unwrap_or(obs.emitter_id);
        let by_identity = match &obs.identity {
            Some(identity) => emitter_id_by_identity(&tx, identity)?,
            None => None,
        };
        let target = match by_identity {
            Some(holder) if holder != requested && emitter_exists(&tx, requested)? => {
                return Err(RepoError::IdentityConflict {
                    identity: identity_label(obs.identity.as_ref().expect("matched by identity")),
                    existing: holder,
                });
            }
            Some(holder) => holder,
            None => requested,
        };
        let freq = FreqRange::centered(obs.f_center_hz, obs.bandwidth_hz);
        let (scheme, value) = match &obs.identity {
            Some(d) => (Some(d.scheme.as_string()), Some(d.value.clone())),
            None => (None, None),
        };
        let created = if emitter_exists(&tx, target)? {
            if let Some(d) = &obs.identity {
                let current: (Option<String>, Option<String>) = tx.query_row(
                    "SELECT identity_scheme, identity_value FROM emitter WHERE emitter_id = ?1",
                    [blob(target)],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?;
                if current.0.is_some() && (current.0 != scheme || current.1 != value) {
                    return Err(RepoError::IdentityConflict {
                        identity: identity_label(d),
                        existing: target,
                    });
                }
            }
            // SET expressions all see the pre-update row, so `last_seen` below is the old value.
            tx.prepare_cached(
                "UPDATE emitter SET \
                   count = count + ?1, \
                   first_seen = min(first_seen, ?2), \
                   last_seen = max(last_seen, ?3), \
                   f_center = CASE WHEN ?3 >= last_seen THEN ?4 ELSE f_center END, \
                   bandwidth = CASE WHEN ?3 >= last_seen THEN ?5 ELSE bandwidth END, \
                   f_lo = CASE WHEN ?3 >= last_seen THEN ?6 ELSE f_lo END, \
                   f_hi = CASE WHEN ?3 >= last_seen THEN ?7 ELSE f_hi END, \
                   identity_scheme = coalesce(identity_scheme, ?8), \
                   identity_value = coalesce(identity_value, ?9) \
                 WHERE emitter_id = ?10",
            )?
            .execute(params![
                int(obs.count, "count")?,
                obs.seen.start.as_unix_nanos(),
                obs.seen.end.as_unix_nanos(),
                obs.f_center_hz,
                obs.bandwidth_hz,
                freq.lo_hz,
                freq.hi_hz,
                scheme,
                value,
                blob(target)
            ])?;
            false
        } else {
            tx.prepare_cached(&format!(
                "INSERT INTO emitter ({EMITTER_INSERT_COLUMNS}) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7, ?8, ?9, ?10)"
            ))?
            .execute(params![
                blob(target),
                obs.f_center_hz,
                obs.bandwidth_hz,
                obs.seen.start.as_unix_nanos(),
                obs.seen.end.as_unix_nanos(),
                int(obs.count, "count")?,
                scheme,
                value,
                freq.lo_hz,
                freq.hi_hz
            ])?;
            insert_status(
                &tx,
                &KnownStatusChange {
                    emitter_id: target,
                    status: KnownStatus::Unknown,
                    prior_ref: None,
                    reason: "created from observation".into(),
                    t: obs.seen.start,
                    author: StatusAuthor::System,
                },
            )?;
            true
        };
        bump_extent(&tx, "emitter", freq.width_hz(), 0)?;
        tx.commit()?;
        Ok(EmitterUpsert {
            emitter_id: target,
            created,
        })
    }

    fn emitter_from_raw(&self, raw: EmitterRaw) -> Result<Emitter, RepoError> {
        let (id, f_center, bw, first, last, count, fp, scheme, value, status) = raw;
        let identity = match (scheme, value) {
            (Some(s), Some(v)) => Identity::Decoded(DecodedIdentity {
                scheme: s.parse().map_err(RepoError::Invalid)?,
                value: v,
            }),
            _ => Identity::Unknown,
        };
        let status = status.ok_or_else(|| {
            RepoError::Invalid(format!(
                "emitter {} has no status history",
                Uuid::from_bytes(id)
            ))
        })?;
        let classifications = {
            let mut stmt = self.conn.prepare_cached(
                "SELECT t, family, confidence, open_set_score, model_version \
                 FROM emitter_classification WHERE emitter_id = ?1 ORDER BY classification_id",
            )?;
            stmt.query_map([id], |r| {
                Ok(Classification {
                    t: Timestamp::from_unix_nanos(r.get(0)?),
                    family: r.get(1)?,
                    confidence: r.get(2)?,
                    open_set_score: r.get(3)?,
                    model_version: r.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?
        };
        let tags = {
            let mut stmt = self
                .conn
                .prepare_cached("SELECT tag FROM emitter_tag WHERE emitter_id = ?1")?;
            stmt.query_map([id], |r| r.get::<_, String>(0))?
                .collect::<Result<_, _>>()?
        };
        Ok(Emitter {
            id: EmitterId::from_uuid(Uuid::from_bytes(id)),
            f_center_hz: f_center,
            bandwidth_hz: bw,
            first_seen: Timestamp::from_unix_nanos(first),
            last_seen: Timestamp::from_unix_nanos(last),
            count: count as u64,
            fingerprint: fp
                .map(|s| serde_json::from_str(&s))
                .transpose()?
                .unwrap_or_default(),
            identity,
            known_status: enum_parse(status)?,
            classifications,
            tags,
        })
    }

    /// One Emitter with its classification history, current status and tags.
    pub fn emitter(&self, id: EmitterId) -> Result<Emitter, RepoError> {
        let raw = self
            .conn
            .prepare_cached(concat!(
                "SELECT ",
                emitter_select!(),
                " FROM emitter WHERE emitter_id = ?1"
            ))?
            .query_row([blob(id)], emitter_raw)
            .optional()?
            .ok_or_else(|| RepoError::NotFound {
                kind: "emitter",
                id: id.to_string(),
            })?;
        self.emitter_from_raw(raw)
    }

    /// The emitter holding a decoded identity, if any.
    pub fn emitter_by_identity(
        &self,
        identity: &DecodedIdentity,
    ) -> Result<Option<Emitter>, RepoError> {
        emitter_id_by_identity(&self.conn, identity)?
            .map(|id| self.emitter(id))
            .transpose()
    }

    /// Emitters overlapping `region` in frequency whose first–last-seen span overlaps its time
    /// (docs/07 §4 step 3), most recently seen first.
    pub fn emitters_in_region(&self, region: &Region) -> Result<Vec<Emitter>, RepoError> {
        let tx = self.read_tx()?;
        let b = region_bounds(&tx, "emitter", region)?;
        let raws = {
            let mut stmt = tx.prepare_cached(EMITTER_REGION_SQL)?;
            stmt.query_map(params![b.f_lo_min, b.hi, b.lo, b.t0, b.t1], emitter_raw)?
                .collect::<Result<Vec<_>, _>>()?
        };
        raws.into_iter()
            .map(|raw| self.emitter_from_raw(raw))
            .collect()
    }

    /// Appends a classification to an emitter's history.
    pub fn append_classification(
        &mut self,
        emitter_id: EmitterId,
        classification: &Classification,
    ) -> Result<(), RepoError> {
        insert_classification(&self.conn, emitter_id, classification)
    }

    /// Appends a known-status change (C17 priors, decoders, users). The emitter's current
    /// `known_status` becomes this entry; earlier entries are kept.
    pub fn append_known_status(&mut self, change: &KnownStatusChange) -> Result<(), RepoError> {
        let tx = self.write_tx()?;
        if !emitter_exists(&tx, change.emitter_id)? {
            return Err(RepoError::NotFound {
                kind: "emitter",
                id: change.emitter_id.to_string(),
            });
        }
        insert_status(&tx, change)?;
        tx.commit()?;
        Ok(())
    }

    /// An emitter's known-status history, oldest first.
    pub fn known_status_history(
        &self,
        emitter_id: EmitterId,
    ) -> Result<Vec<KnownStatusChange>, RepoError> {
        type Raw = (String, Option<String>, String, i64, String);
        let rows: Vec<Raw> = {
            let mut stmt = self.conn.prepare_cached(
                "SELECT status, prior_ref, reason, t, author FROM emitter_status \
                 WHERE emitter_id = ?1 ORDER BY status_id",
            )?;
            stmt.query_map([blob(emitter_id)], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<Result<_, _>>()?
        };
        rows.into_iter()
            .map(|(status, prior_ref, reason, t, author)| {
                Ok(KnownStatusChange {
                    emitter_id,
                    status: enum_parse(status)?,
                    prior_ref,
                    reason,
                    t: Timestamp::from_unix_nanos(t),
                    author: enum_parse(author)?,
                })
            })
            .collect()
    }

    /// Adds a tag (no-op if present).
    pub fn add_emitter_tag(&mut self, emitter_id: EmitterId, tag: &str) -> Result<(), RepoError> {
        self.conn.execute(
            "INSERT OR IGNORE INTO emitter_tag (emitter_id, tag) VALUES (?1, ?2)",
            params![blob(emitter_id), tag],
        )?;
        Ok(())
    }

    /// Removes a tag; returns whether it was present.
    pub fn remove_emitter_tag(
        &mut self,
        emitter_id: EmitterId,
        tag: &str,
    ) -> Result<bool, RepoError> {
        Ok(self.conn.execute(
            "DELETE FROM emitter_tag WHERE emitter_id = ?1 AND tag = ?2",
            params![blob(emitter_id), tag],
        )? > 0)
    }

    /// Appends a link from an emitter (idempotent: the first `linked_at` is kept). A merged
    /// emitter id links to its survivor.
    pub fn link_emitter(&mut self, link: &EmitterLink) -> Result<(), RepoError> {
        let (kind, id) = link_kind(&link.target);
        let emitter =
            super::cluster::live_id(&self.conn, link.emitter_id)?.unwrap_or(link.emitter_id);
        self.conn.execute(
            "INSERT OR IGNORE INTO emitter_link (emitter_id, target_kind, target_id, linked_at) \
             VALUES (?1, ?2, ?3, ?4)",
            params![
                blob(emitter),
                kind,
                id.into_bytes(),
                link.linked_at.as_unix_nanos()
            ],
        )?;
        Ok(())
    }

    /// Current links from an emitter, oldest first. Links a merge re-pointed elsewhere are
    /// superseded and omitted (see `emitter_link_history`).
    pub fn emitter_links(&self, emitter_id: EmitterId) -> Result<Vec<EmitterLink>, RepoError> {
        let rows = {
            let mut stmt = self.conn.prepare_cached(
                "SELECT target_kind, target_id, linked_at FROM emitter_link \
                 WHERE emitter_id = ?1 AND superseded_by IS NULL \
                 ORDER BY linked_at, target_kind, target_id",
            )?;
            stmt.query_map([blob(emitter_id)], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, [u8; 16]>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?
        };
        rows.into_iter()
            .map(|(kind, id, at)| {
                Ok(EmitterLink {
                    emitter_id,
                    target: link_target(&kind, Uuid::from_bytes(id))?,
                    linked_at: Timestamp::from_unix_nanos(at),
                })
            })
            .collect()
    }
}
