//! Interpretations (Annotation, Demodulation, Decode, Bitstream, Anomaly, Explanation) and the
//! ExternalEvent cache. Interpretations are insert-only; a new version is a new row.

use rusqlite::{OptionalExtension, params};

use super::{
    RepoError, Repository, blob, bodies, body_by_id, bump_extent, enum_text, finite, opt_blob,
    region_bounds,
};
use crate::context::{
    Anomaly, AnomalyStatus, AnomalyStatusChange, AnomalySubject, Cause, Explanation, ExternalEvent,
};
use crate::decode::{Bitstream, Decode, Demodulation};
use crate::emitter::DecodedIdentity;
use crate::ids::{
    AnnotationId, AnomalyId, BitstreamId, DecodeId, DemodulationId, ExplanationId, ExternalEventId,
};
use crate::recording::{Annotation, AnnotationTarget};
use crate::region::Region;
use crate::time::Timestamp;

/// Region query for anomaly and explanation (same shape). Parameters: f_lo min, query hi,
/// query lo, t_start min, query t1, query t0.
pub(super) fn event_region_sql(table: &str) -> String {
    format!(
        "SELECT body FROM {table} \
         WHERE f_lo BETWEEN ?1 AND ?2 AND f_hi >= ?3 \
           AND t_start BETWEEN ?4 AND ?5 AND t_end >= ?6 \
         ORDER BY t_start, {table}_id"
    )
}

fn annotation_target_columns(target: &AnnotationTarget) -> (&'static str, Option<[u8; 16]>) {
    match target {
        AnnotationTarget::Detection(id) => ("detection", Some(blob(*id))),
        AnnotationTarget::Emitter(id) => ("emitter", Some(blob(*id))),
        AnnotationTarget::Recording(span) => ("recording", Some(blob(span.recording_id))),
        AnnotationTarget::Anomaly(id) => ("anomaly", Some(blob(*id))),
        AnnotationTarget::Explanation(id) => ("explanation", Some(blob(*id))),
        AnnotationTarget::Region(_) => ("region", None),
    }
}

impl Repository {
    // ---- Annotation ----

    /// Appends an annotation.
    pub fn insert_annotation(&mut self, a: &Annotation) -> Result<(), RepoError> {
        finite(a.confidence, "confidence")?;
        let (kind, id) = annotation_target_columns(&a.target);
        self.conn.execute(
            "INSERT INTO annotation (annotation_id, target_kind, target_id, author, kind, \
             supersedes, content_class, t, exported, body) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                blob(a.id),
                kind,
                id,
                enum_text(&a.author)?,
                enum_text(&a.kind)?,
                opt_blob(a.supersedes),
                enum_text(&a.content_class)?,
                a.t.as_unix_nanos(),
                a.exported,
                serde_json::to_string(a)?
            ],
        )?;
        Ok(())
    }

    /// One annotation. `exported` reflects the current bookkeeping flag.
    pub fn annotation(&self, id: AnnotationId) -> Result<Annotation, RepoError> {
        let row: Option<(String, bool)> = self
            .conn
            .prepare_cached("SELECT body, exported FROM annotation WHERE annotation_id = ?1")?
            .query_row([blob(id)], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        let (body, exported) = row.ok_or_else(|| RepoError::NotFound {
            kind: "annotation",
            id: id.to_string(),
        })?;
        let mut a: Annotation = serde_json::from_str(&body)?;
        a.exported = exported;
        Ok(a)
    }

    /// Annotations on the same object as `target`, oldest first. A recording target matches
    /// every annotation on that recording, whatever its span; a region target matches all
    /// free-region annotations.
    pub fn annotations_for(&self, target: &AnnotationTarget) -> Result<Vec<Annotation>, RepoError> {
        let (kind, id) = annotation_target_columns(target);
        let rows: Vec<(String, bool)> = {
            let mut stmt = self.conn.prepare_cached(
                "SELECT body, exported FROM annotation \
                 WHERE target_kind = ?1 AND target_id IS ?2 ORDER BY t, annotation_id",
            )?;
            stmt.query_map(params![kind, id], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<Result<_, _>>()?
        };
        rows.into_iter()
            .map(|(body, exported)| {
                let mut a: Annotation = serde_json::from_str(&body)?;
                a.exported = exported;
                Ok(a)
            })
            .collect()
    }

    /// Marks annotations as included in a labelled export (the only annotation update).
    pub fn mark_annotations_exported(&mut self, ids: &[AnnotationId]) -> Result<(), RepoError> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt =
                tx.prepare_cached("UPDATE annotation SET exported = 1 WHERE annotation_id = ?1")?;
            for id in ids {
                stmt.execute([blob(*id)])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    // ---- Demodulation / Decode / Bitstream ----

    /// Appends a demodulation session.
    pub fn insert_demodulation(&mut self, d: &Demodulation) -> Result<(), RepoError> {
        self.conn.execute(
            "INSERT INTO demodulation (demod_id, emitter_id, detection_id, recording_id, mode, \
             t_start, t_end, demod_version, body) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                blob(d.id),
                opt_blob(d.emitter_ref),
                opt_blob(d.detection_ref),
                opt_blob(d.recording_ref),
                d.mode,
                d.time.start.as_unix_nanos(),
                d.time.end.as_unix_nanos(),
                d.demod_version,
                serde_json::to_string(d)?
            ],
        )?;
        Ok(())
    }

    /// One demodulation.
    pub fn demodulation(&self, id: DemodulationId) -> Result<Demodulation, RepoError> {
        body_by_id(
            &self.conn,
            "SELECT body FROM demodulation WHERE demod_id = ?1",
            blob(id),
            "demodulation",
        )
    }

    /// Appends a decode.
    pub fn insert_decode(&mut self, d: &Decode) -> Result<(), RepoError> {
        let (scheme, value) = match &d.identity {
            Some(i) => (Some(i.scheme.as_string()), Some(i.value.as_str())),
            None => (None, None),
        };
        self.conn.execute(
            "INSERT INTO decode (decode_id, demod_id, recording_id, decoder_id, crc_status, \
             identity_scheme, identity_value, content_class, t, body) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                blob(d.id),
                opt_blob(d.demodulation_ref),
                opt_blob(d.recording_ref),
                d.decoder_id,
                enum_text(&d.crc_status)?,
                scheme,
                value,
                enum_text(&d.content_class)?,
                d.t.as_unix_nanos(),
                serde_json::to_string(d)?
            ],
        )?;
        Ok(())
    }

    /// One decode.
    pub fn decode(&self, id: DecodeId) -> Result<Decode, RepoError> {
        body_by_id(
            &self.conn,
            "SELECT body FROM decode WHERE decode_id = ?1",
            blob(id),
            "decode",
        )
    }

    /// Decodes naming an identity, oldest first.
    pub fn decodes_for_identity(
        &self,
        identity: &DecodedIdentity,
    ) -> Result<Vec<Decode>, RepoError> {
        bodies(
            &self.conn,
            "SELECT body FROM decode WHERE identity_scheme = ?1 AND identity_value = ?2 \
             ORDER BY t, decode_id",
            params![identity.scheme.as_string(), identity.value],
        )
    }

    /// Appends a bitstream descriptor.
    pub fn insert_bitstream(&mut self, b: &Bitstream) -> Result<(), RepoError> {
        self.conn.execute(
            "INSERT INTO bitstream (bitstream_id, emitter_id, demod_id, provenance_id, \
             content_class, t_start, body) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                blob(b.id),
                opt_blob(b.emitter_ref),
                opt_blob(b.demodulation_ref),
                opt_blob(b.provenance_ref),
                enum_text(&b.content_class)?,
                b.time.start.as_unix_nanos(),
                serde_json::to_string(b)?
            ],
        )?;
        Ok(())
    }

    /// One bitstream descriptor.
    pub fn bitstream(&self, id: BitstreamId) -> Result<Bitstream, RepoError> {
        body_by_id(
            &self.conn,
            "SELECT body FROM bitstream WHERE bitstream_id = ?1",
            blob(id),
            "bitstream",
        )
    }

    // ---- ExternalEvent ----

    /// Stores or refreshes a cached external event by `(source, native_id)`. Returns the stored
    /// id: the first id ever stored for that key, whatever `event.id` says.
    pub fn upsert_external_event(
        &mut self,
        event: &ExternalEvent,
    ) -> Result<ExternalEventId, RepoError> {
        let tx = self.conn.transaction()?;
        let existing: Option<[u8; 16]> = tx
            .query_row(
                "SELECT event_id FROM external_event WHERE source = ?1 AND native_id = ?2",
                params![event.source, event.native_id],
                |r| r.get(0),
            )
            .optional()?;
        let id = existing
            .map(|b| ExternalEventId::from_uuid(uuid::Uuid::from_bytes(b)))
            .unwrap_or(event.id);
        let mut stored = event.clone();
        stored.id = id;
        let body = serde_json::to_string(&stored)?;
        let args = params![
            blob(id),
            event.source,
            event.native_id,
            event.event_type,
            event.time.start.as_unix_nanos(),
            event.time.end.as_unix_nanos(),
            event.fetched_at.as_unix_nanos(),
            event.valid_until.map(Timestamp::as_unix_nanos),
            body
        ];
        if existing.is_some() {
            tx.execute(
                "UPDATE external_event SET source = ?2, native_id = ?3, event_type = ?4, \
                 t_start = ?5, t_end = ?6, fetched_at = ?7, valid_until = ?8, body = ?9 \
                 WHERE event_id = ?1",
                args,
            )?;
        } else {
            tx.execute(
                "INSERT INTO external_event (event_id, source, native_id, event_type, t_start, \
                 t_end, fetched_at, valid_until, body) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                args,
            )?;
        }
        tx.commit()?;
        Ok(id)
    }

    /// One cached external event.
    pub fn external_event(&self, id: ExternalEventId) -> Result<ExternalEvent, RepoError> {
        body_by_id(
            &self.conn,
            "SELECT body FROM external_event WHERE event_id = ?1",
            blob(id),
            "external event",
        )
    }

    // ---- Anomaly ----

    /// Appends an anomaly and its initial `open` status.
    pub fn insert_anomaly(&mut self, a: &Anomaly) -> Result<(), RepoError> {
        if a.region.time.end < a.region.time.start || a.region.freq.hi_hz < a.region.freq.lo_hz {
            return Err(RepoError::Invalid("anomaly region is inverted".into()));
        }
        let (subject_kind, subject_id) = match a.subject {
            AnomalySubject::Detection(id) => ("detection", Some(blob(id))),
            AnomalySubject::Emitter(id) => ("emitter", Some(blob(id))),
            AnomalySubject::Region => ("region", None),
        };
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO anomaly (anomaly_id, kind, subject_kind, subject_id, f_lo, f_hi, \
             t_start, t_end, score, t, body) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                blob(a.id),
                enum_text(&a.kind)?,
                subject_kind,
                subject_id,
                finite(a.region.freq.lo_hz, "f_lo")?,
                finite(a.region.freq.hi_hz, "f_hi")?,
                a.region.time.start.as_unix_nanos(),
                a.region.time.end.as_unix_nanos(),
                a.score,
                a.t.as_unix_nanos(),
                serde_json::to_string(a)?
            ],
        )?;
        tx.execute(
            "INSERT INTO anomaly_status (anomaly_id, status, t, note) VALUES (?1, 'open', ?2, NULL)",
            params![blob(a.id), a.t.as_unix_nanos()],
        )?;
        bump_extent(
            &tx,
            "anomaly",
            a.region.freq.width_hz(),
            a.region.time.duration_ns(),
        )?;
        tx.commit()?;
        Ok(())
    }

    /// One anomaly.
    pub fn anomaly(&self, id: AnomalyId) -> Result<Anomaly, RepoError> {
        body_by_id(
            &self.conn,
            "SELECT body FROM anomaly WHERE anomaly_id = ?1",
            blob(id),
            "anomaly",
        )
    }

    /// Appends a status change to an anomaly's history.
    pub fn append_anomaly_status(&mut self, change: &AnomalyStatusChange) -> Result<(), RepoError> {
        self.conn.execute(
            "INSERT INTO anomaly_status (anomaly_id, status, t, note) VALUES (?1, ?2, ?3, ?4)",
            params![
                blob(change.anomaly_id),
                enum_text(&change.status)?,
                change.t.as_unix_nanos(),
                change.note
            ],
        )?;
        Ok(())
    }

    /// An anomaly's status history, oldest first. The last entry is the current status.
    pub fn anomaly_status_history(
        &self,
        id: AnomalyId,
    ) -> Result<Vec<AnomalyStatusChange>, RepoError> {
        let rows: Vec<(String, i64, Option<String>)> = {
            let mut stmt = self.conn.prepare_cached(
                "SELECT status, t, note FROM anomaly_status WHERE anomaly_id = ?1 \
                 ORDER BY status_id",
            )?;
            stmt.query_map([blob(id)], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<Result<_, _>>()?
        };
        rows.into_iter()
            .map(|(status, t, note)| {
                Ok(AnomalyStatusChange {
                    anomaly_id: id,
                    status: super::enum_parse::<AnomalyStatus>(status)?,
                    t: Timestamp::from_unix_nanos(t),
                    note,
                })
            })
            .collect()
    }

    /// Anomalies whose region overlaps `region` (docs/07 §4 step 4).
    pub fn anomalies_in_region(&self, region: &Region) -> Result<Vec<Anomaly>, RepoError> {
        let b = region_bounds(&self.conn, "anomaly", region)?;
        bodies(
            &self.conn,
            &event_region_sql("anomaly"),
            params![b.f_lo_min, b.hi, b.lo, b.t_start_min, b.t1, b.t0],
        )
    }

    // ---- Explanation ----

    /// Appends an explanation. Its region/time columns are copied from the anomaly.
    pub fn insert_explanation(&mut self, e: &Explanation) -> Result<(), RepoError> {
        finite(e.score, "score")?;
        let tx = self.conn.transaction()?;
        let extent: Option<(f64, f64, i64, i64)> = tx
            .query_row(
                "SELECT f_lo, f_hi, t_start, t_end FROM anomaly WHERE anomaly_id = ?1",
                [blob(e.anomaly_ref)],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let (f_lo, f_hi, t_start, t_end) = extent.ok_or_else(|| RepoError::NotFound {
            kind: "anomaly",
            id: e.anomaly_ref.to_string(),
        })?;
        let (cause_kind, event_id, emitter_id) = match &e.cause {
            Cause::ExternalEvent { id } => ("external-event", Some(blob(*id)), None),
            Cause::Emitter { id } => ("emitter", None, Some(blob(*id))),
            Cause::OwnHistory { .. } => ("own-history", None, None),
            Cause::SelfInflicted { .. } => ("self-inflicted", None, None),
            Cause::Unexplained => ("unexplained", None, None),
        };
        tx.execute(
            "INSERT INTO explanation (explanation_id, anomaly_id, supersedes, cause_kind, \
             cause_event_id, cause_emitter_id, correlation_type, score, f_lo, f_hi, t_start, \
             t_end, t, body) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                blob(e.id),
                blob(e.anomaly_ref),
                opt_blob(e.supersedes),
                cause_kind,
                event_id,
                emitter_id,
                enum_text(&e.correlation_type)?,
                e.score,
                f_lo,
                f_hi,
                t_start,
                t_end,
                e.t.as_unix_nanos(),
                serde_json::to_string(e)?
            ],
        )?;
        bump_extent(&tx, "explanation", f_hi - f_lo, t_end - t_start)?;
        tx.commit()?;
        Ok(())
    }

    /// One explanation.
    pub fn explanation(&self, id: ExplanationId) -> Result<Explanation, RepoError> {
        body_by_id(
            &self.conn,
            "SELECT body FROM explanation WHERE explanation_id = ?1",
            blob(id),
            "explanation",
        )
    }

    /// Explanations of one anomaly, best score first.
    pub fn explanations_for_anomaly(&self, id: AnomalyId) -> Result<Vec<Explanation>, RepoError> {
        bodies(
            &self.conn,
            "SELECT body FROM explanation WHERE anomaly_id = ?1 ORDER BY score DESC, t",
            [blob(id)],
        )
    }

    /// Explanations whose anomaly region overlaps `region` (the attack-map view).
    pub fn explanations_in_region(&self, region: &Region) -> Result<Vec<Explanation>, RepoError> {
        let b = region_bounds(&self.conn, "explanation", region)?;
        bodies(
            &self.conn,
            &event_region_sql("explanation"),
            params![b.f_lo_min, b.hi, b.lo, b.t_start_min, b.t1, b.t0],
        )
    }
}
