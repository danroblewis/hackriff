//! Tracks and the Emitter inventory (C10, C27).

use rusqlite::{Connection, OptionalExtension, Row, params};
use uuid::Uuid;

use super::{
    RepoError, Repository, blob, bodies, body_by_id, bump_extent, enum_parse, enum_text, finite,
    int, opt_blob, region_bounds,
};
use crate::cluster::{IdentityAccess, InventoryEntry, InventoryIdentity};
use crate::detection::{
    MAX_TRACK_PAGE, PageRequest, Track, TrackFilter, TrackKind, TrackPage, TrackSegment, TrackState,
};
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

/// T-158: `(snr_peak_db, peak_level_dbfs)` of the most recently started [`crate::Detection`]
/// reachable from an emitter — directly linked, or through one of its currently-linked
/// [`crate::detection::Track`]s (`emitter_link.superseded_by IS NULL`; a merge re-points links to
/// the survivor, so a merged-away emitter id contributes nothing here). Both current-link paths
/// use the `emitter_link` primary key (`emitter_id, target_kind, target_id`) and the
/// `track_detection`/`detection` primary keys, so this is index-only, no table scan. Parameter
/// `?1` is the emitter id, given twice (once per source path).
const EMITTER_LATEST_DETECTION_SQL: &str = "\
     SELECT snr_peak, peak_dbfs FROM ( \
       SELECT d.snr_peak AS snr_peak, d.peak_dbfs AS peak_dbfs, d.t_start AS t_start \
       FROM emitter_link el \
       JOIN track_detection td ON td.track_id = el.target_id \
       JOIN detection d ON d.detection_id = td.detection_id \
       WHERE el.emitter_id = ?1 AND el.target_kind = 'track' AND el.superseded_by IS NULL \
       UNION ALL \
       SELECT d.snr_peak AS snr_peak, d.peak_dbfs AS peak_dbfs, d.t_start AS t_start \
       FROM emitter_link el \
       JOIN detection d ON d.detection_id = el.target_id \
       WHERE el.emitter_id = ?1 AND el.target_kind = 'detection' AND el.superseded_by IS NULL \
     ) ORDER BY t_start DESC LIMIT 1";

/// [`EMITTER_REGION_SQL`] with a row limit (`?6`).
const EMITTER_REGION_LIMIT_SQL: &str = concat!(
    "SELECT ",
    emitter_select!(),
    " FROM emitter \
     WHERE f_lo BETWEEN ?1 AND ?2 AND f_hi >= ?3 AND last_seen >= ?4 AND first_seen <= ?5 \
     AND merged_into IS NULL \
     ORDER BY last_seen DESC, emitter_id LIMIT ?6"
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

/// The track region query. Parameters: f_lo min, query hi, query lo, t_start min, query t1,
/// query t0, kind (NULL = any), include merged, min detections, hop set (NULL = any), limit,
/// offset.
pub(super) const TRACK_REGION_SQL: &str = "SELECT body FROM track \
     WHERE f_lo BETWEEN ?1 AND ?2 AND f_hi >= ?3 \
       AND t_start BETWEEN ?4 AND ?5 AND t_end >= ?6 \
       AND (?7 IS NULL OR kind = ?7) AND (?8 OR state != 'merged') \
       AND detection_count >= ?9 AND (?10 IS NULL OR hop_set = ?10) \
     ORDER BY t_start, track_id LIMIT ?11 OFFSET ?12";

/// [`Repository::upsert_track`] inside an open write transaction.
pub(super) fn upsert_track_on(conn: &Connection, track: &Track) -> Result<(), RepoError> {
    let (state, merged_into) = match track.state {
        TrackState::Open => ("open", None),
        TrackState::Closed => ("closed", None),
        TrackState::MergedInto(t) => ("merged", Some(blob(t))),
    };
    let fc = finite(track.f_center_hz, "f_center_hz")?;
    let bw = finite(track.bandwidth_hz, "bandwidth_hz")?;
    if bw < 0.0 {
        return Err(RepoError::Invalid(format!(
            "track {} bandwidth {bw} is negative",
            track.id
        )));
    }
    let kind = if track.timing.hop_set_hz.is_empty() {
        "channel"
    } else {
        "hop-set"
    };
    conn.prepare_cached(
        "INSERT INTO track (track_id, state, merged_into, split_from, t_start, t_end, \
         f_center, f_lo, f_hi, kind, detection_count, hop_set, updated_at, body) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14) \
         ON CONFLICT (track_id) DO UPDATE SET state = excluded.state, \
         merged_into = excluded.merged_into, split_from = excluded.split_from, \
         t_start = excluded.t_start, t_end = excluded.t_end, f_center = excluded.f_center, \
         f_lo = excluded.f_lo, f_hi = excluded.f_hi, kind = excluded.kind, \
         detection_count = excluded.detection_count, hop_set = excluded.hop_set, \
         updated_at = excluded.updated_at, body = excluded.body",
    )?
    .execute(params![
        blob(track.id),
        state,
        merged_into,
        opt_blob(track.split_from),
        track.time.start.as_unix_nanos(),
        track.time.end.as_unix_nanos(),
        fc,
        fc - bw / 2.0,
        fc + bw / 2.0,
        kind,
        int(track.detection_count, "detection_count")?,
        opt_blob(track.timing.hop_set),
        track.updated_at.as_unix_nanos(),
        serde_json::to_string(track)?
    ])?;
    bump_extent(conn, "track", bw, track.time.duration_ns())?;
    Ok(())
}

/// [`Repository::link_detections_to_track`] inside an open write transaction.
pub(super) fn link_detections_on(
    conn: &Connection,
    track_id: TrackId,
    detections: &[DetectionId],
    linked_at: Timestamp,
) -> Result<(), RepoError> {
    let mut stmt = conn.prepare_cached(
        "INSERT OR IGNORE INTO track_detection (track_id, detection_id, linked_at) \
         VALUES (?1, ?2, ?3)",
    )?;
    for d in detections {
        stmt.execute(params![blob(track_id), blob(*d), linked_at.as_unix_nanos()])?;
    }
    Ok(())
}

/// [`Repository::track_detections`] on any connection or transaction.
pub(super) fn track_detections_on(
    conn: &Connection,
    track_id: TrackId,
) -> Result<Vec<DetectionId>, RepoError> {
    let mut stmt = conn.prepare_cached(
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

/// [`Repository::append_track_segments`] inside an open write transaction.
pub(super) fn append_track_segments_on(
    conn: &Connection,
    segments: &[TrackSegment],
) -> Result<(), RepoError> {
    let mut stmt = conn.prepare_cached(
        "INSERT INTO track_segment (track_id, at, kind) VALUES (?1, ?2, ?3) \
         ON CONFLICT DO NOTHING",
    )?;
    for s in segments {
        stmt.execute(params![
            blob(s.track),
            s.at.as_unix_nanos(),
            enum_text(&s.kind)?
        ])?;
    }
    Ok(())
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

/// Error label for an identity: the scheme only. Errors can leave the process (API responses,
/// logs) and the class is not known here, so the value is always withheld.
pub(super) fn identity_label(identity: &DecodedIdentity) -> String {
    format!("{}:<withheld>", identity.scheme)
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
        let tx = self.write_tx()?;
        upsert_track_on(&tx, track)?;
        tx.commit()?;
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
        link_detections_on(&tx, track_id, detections, linked_at)?;
        tx.commit()?;
        Ok(())
    }

    /// Member detections of a track, ordered by detection start time (then id).
    pub fn track_detections(&self, track_id: TrackId) -> Result<Vec<DetectionId>, RepoError> {
        track_detections_on(&self.conn, track_id)
    }

    /// Appends segment boundaries (idempotent; the tracks must exist).
    pub fn append_track_segments(&mut self, segments: &[TrackSegment]) -> Result<(), RepoError> {
        let tx = self.write_tx()?;
        append_track_segments_on(&tx, segments)?;
        tx.commit()?;
        Ok(())
    }

    /// Segment boundaries of a track, in time order.
    pub fn track_segments(&self, track_id: TrackId) -> Result<Vec<TrackSegment>, RepoError> {
        let raw = {
            let mut stmt = self.conn.prepare_cached(
                "SELECT at, kind FROM track_segment WHERE track_id = ?1 ORDER BY at, kind",
            )?;
            stmt.query_map([blob(track_id)], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?
        };
        raw.into_iter()
            .map(|(at, kind)| {
                Ok(TrackSegment {
                    track: track_id,
                    at: Timestamp::from_unix_nanos(at),
                    kind: enum_parse(kind)?,
                })
            })
            .collect()
    }

    /// Tracks whose frequency × time box (`f_center ± bandwidth/2` × `time`) overlaps `region`
    /// (closed intervals) and that pass `filter`, ordered by start time (then id), one page at a
    /// time. Index-bounded like the other region queries (AWARE-042).
    pub fn tracks_in_region(
        &self,
        region: &Region,
        filter: &TrackFilter,
        page: PageRequest,
    ) -> Result<TrackPage, RepoError> {
        let limit = page.limit.clamp(1, MAX_TRACK_PAGE);
        let tx = self.read_tx()?;
        let b = region_bounds(&tx, "track", region)?;
        let kind = match filter.kind {
            TrackKind::Any => None,
            TrackKind::Channel => Some("channel"),
            TrackKind::HopSet => Some("hop-set"),
        };
        let mut tracks: Vec<Track> = bodies(
            &tx,
            TRACK_REGION_SQL,
            params![
                b.f_lo_min,
                b.hi,
                b.lo,
                b.t_start_min,
                b.t1,
                b.t0,
                kind,
                filter.include_merged,
                int(filter.min_detections, "min_detections")?,
                opt_blob(filter.hop_set),
                i64::from(limit) + 1,
                i64::try_from(page.offset).unwrap_or(i64::MAX)
            ],
        )?;
        let more = tracks.len() > limit as usize;
        tracks.truncate(limit as usize);
        Ok(TrackPage {
            tracks,
            next_offset: more.then(|| page.offset + u64::from(limit)),
        })
    }

    // ---- Emitter ----

    /// Inserts a new Emitter with its classification history and tags. `e.known_status` becomes
    /// the first status-history entry (author `system`, at `first_seen`).
    pub fn insert_emitter(&mut self, e: &Emitter) -> Result<(), RepoError> {
        // T-036: an identity written here has no class (withheld), so tags must be labels.
        if let Identity::Decoded(d) = &e.identity {
            super::gating::check_tags(&mut e.tags.iter(), Some((d, None)))?;
        }
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
    /// **Legacy.** It records no identity class, so an identity written here stays withheld on
    /// every read unless linked decodes supply the class, and it does not deduplicate. New
    /// writers use `record_sighting` / `record_sighting_measured`.
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
        // T-040: an identity filled in here is unclassified, so earlier free-text tags go.
        super::gating::purge_withheld_tags(&tx, target)?;
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

    /// One Emitter with its classification history, current status and tags. The identity is
    /// gated at [`IdentityAccess::Standard`] (legal guardrail, [`crate::cluster`]): a withheld
    /// identity reads as `Identity::Unknown`. See [`Self::emitter_with_access`].
    pub fn emitter(&self, id: EmitterId) -> Result<Emitter, RepoError> {
        Ok(self
            .emitter_with_access(id, IdentityAccess::Standard)?
            .emitter)
    }

    /// One emitter as an inventory row, its identity gated for `access` (fail closed).
    pub fn emitter_with_access(
        &self,
        id: EmitterId,
        access: IdentityAccess,
    ) -> Result<InventoryEntry, RepoError> {
        let tx = self.read_tx()?;
        let emitter = self.emitter_ungated(id)?;
        super::cluster::gate_entry(&tx, emitter, access)
    }

    /// One Emitter with its identity in clear whatever its class. Crate-private: every path out
    /// of the process goes through a gated read.
    pub(crate) fn emitter_ungated(&self, id: EmitterId) -> Result<Emitter, RepoError> {
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

    /// The emitter holding a decoded identity, gated at [`IdentityAccess::Standard`]: `None`
    /// unless that identity would be shown in clear (a lookup must not confirm a withheld
    /// identity is held).
    pub fn emitter_by_identity(
        &self,
        identity: &DecodedIdentity,
    ) -> Result<Option<Emitter>, RepoError> {
        Ok(self
            .emitter_by_identity_with_access(identity, IdentityAccess::Standard)?
            .map(|e| e.emitter))
    }

    /// The emitter holding a decoded identity as an inventory row, if `access` reveals it.
    pub fn emitter_by_identity_with_access(
        &self,
        identity: &DecodedIdentity,
        access: IdentityAccess,
    ) -> Result<Option<InventoryEntry>, RepoError> {
        let tx = self.read_tx()?;
        let Some(id) = emitter_id_by_identity(&tx, identity)? else {
            return Ok(None);
        };
        let entry = super::cluster::gate_entry(&tx, self.emitter_ungated(id)?, access)?;
        Ok(matches!(entry.identity, InventoryIdentity::Clear { .. }).then_some(entry))
    }

    /// Emitters overlapping `region` in frequency whose first–last-seen span overlaps its time
    /// (docs/07 §4 step 3), most recently seen first. Identities gated at
    /// [`IdentityAccess::Standard`].
    pub fn emitters_in_region(&self, region: &Region) -> Result<Vec<Emitter>, RepoError> {
        self.emitters_in_region_limited(region, None)
    }

    /// [`Self::emitters_in_region`], at most `limit` rows (the most recently seen) when given.
    pub fn emitters_in_region_limited(
        &self,
        region: &Region,
        limit: Option<usize>,
    ) -> Result<Vec<Emitter>, RepoError> {
        let tx = self.read_tx()?;
        let b = region_bounds(&tx, "emitter", region)?;
        let raws = match limit {
            None => {
                let mut stmt = tx.prepare_cached(EMITTER_REGION_SQL)?;
                stmt.query_map(params![b.f_lo_min, b.hi, b.lo, b.t0, b.t1], emitter_raw)?
                    .collect::<Result<Vec<_>, _>>()?
            }
            Some(n) => {
                let n = i64::try_from(n).unwrap_or(i64::MAX);
                let mut stmt = tx.prepare_cached(EMITTER_REGION_LIMIT_SQL)?;
                stmt.query_map(params![b.f_lo_min, b.hi, b.lo, b.t0, b.t1, n], emitter_raw)?
                    .collect::<Result<Vec<_>, _>>()?
            }
        };
        raws.into_iter()
            .map(|raw| {
                let e = self.emitter_from_raw(raw)?;
                Ok(super::cluster::gate_entry(&tx, e, IdentityAccess::Standard)?.emitter)
            })
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

    /// Adds a tag (no-op if present). T-038 ([`crate::cluster`] tag rules): on an emitter whose
    /// identity no access level reveals (restricted, metadata-only or unclassified), a tag
    /// outside [`crate::TAG_VOCABULARY`] is refused with [`RepoError::Invalid`]; the refusal
    /// depends only on that class, never on the tag's relation to the value.
    /// A merged emitter id tags its survivor (T-040).
    pub fn add_emitter_tag(&mut self, emitter_id: EmitterId, tag: &str) -> Result<(), RepoError> {
        let tx = self.write_tx()?;
        let target = super::gating::tag_write_target(&tx, emitter_id, tag)?;
        tx.execute(
            "INSERT OR IGNORE INTO emitter_tag (emitter_id, tag) VALUES (?1, ?2)",
            params![blob(target), tag],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Removes a tag; returns whether it was present. A merged emitter id stands for its
    /// survivor. T-040: gated like [`Self::add_emitter_tag`], so on an emitter whose identity no
    /// access level reveals a tag outside [`crate::TAG_VOCABULARY`] is refused whether or not it
    /// is stored (the answer cannot confirm a guessed hidden tag).
    pub fn remove_emitter_tag(
        &mut self,
        emitter_id: EmitterId,
        tag: &str,
    ) -> Result<bool, RepoError> {
        let tx = self.write_tx()?;
        let target = super::gating::tag_write_target(&tx, emitter_id, tag)?;
        let removed = tx.execute(
            "DELETE FROM emitter_tag WHERE emitter_id = ?1 AND tag = ?2",
            params![blob(target), tag],
        )? > 0;
        tx.commit()?;
        Ok(removed)
    }

    /// Appends a link from an emitter (idempotent: the first `linked_at` is kept). A merged
    /// emitter id links to its survivor.
    pub fn link_emitter(&mut self, link: &EmitterLink) -> Result<(), RepoError> {
        let (kind, id) = link_kind(&link.target);
        let tx = self.write_tx()?;
        let live = super::cluster::live_id(&tx, link.emitter_id)?;
        tx.execute(
            "INSERT OR IGNORE INTO emitter_link (emitter_id, target_kind, target_id, linked_at) \
             VALUES (?1, ?2, ?3, ?4)",
            params![
                blob(live.unwrap_or(link.emitter_id)),
                kind,
                id.into_bytes(),
                link.linked_at.as_unix_nanos()
            ],
        )?;
        // T-040: a linked decode can tighten an unclassified-row identity's derived class.
        if let Some(live) = live {
            super::gating::purge_withheld_tags(&tx, live)?;
        }
        tx.commit()?;
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

    /// T-158: `(snr_peak_db, peak_level_dbfs)` of the emitter's latest (highest `t_start`) linked
    /// detection, `None` when it has none (e.g. an emitter seen only through a decode sighting, or
    /// a candidate whose track has not yet been offered as a sighting). See
    /// [`EMITTER_LATEST_DETECTION_SQL`] for how "linked" is reached.
    pub fn emitter_latest_measurement(
        &self,
        emitter_id: EmitterId,
    ) -> Result<Option<(f64, f64)>, RepoError> {
        Ok(self
            .conn
            .prepare_cached(EMITTER_LATEST_DETECTION_SQL)?
            .query_row([blob(emitter_id)], |r| {
                Ok((r.get::<_, f64>(0)?, r.get::<_, f64>(1)?))
            })
            .optional()?)
    }
}
