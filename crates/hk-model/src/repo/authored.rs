//! Human-authored annotations (T-816 / MAP-16, docs/25 §5 and §10, ADR-0023): durable
//! time–frequency notes a researcher draws on the canvas — a text note, a box or a marker — each
//! carrying the authored-provenance stamp of docs/25 §2.
//!
//! - **User metadata, never detection input.** Nothing in the pipeline reads this table: an
//!   authored annotation mints no candidate, moves no threshold, confirms no emitter and never
//!   pre-populates the inventory (docs/25 §10.7). It lives beside bookmarks and selections in the
//!   run's user-metadata database.
//! - **Distinct from the §2.13 machine `annotation` table.** That table is the append-only record
//!   of decoder/classifier labels (and `ground-truth` from valid decodes); this one is mutable,
//!   human-authored, and on SigMF export is a `hackriff:annotation` block with `authored: true`
//!   ([`AuthoredAnnotation::to_sigmf`]), structurally apart from `hackriff:truth`.
//! - **Capture clock vs wall clock.** `t0`/`t1` and the provenance `t_capture` are capture-clock
//!   times ("when the air was"); `authored_at`, `created_at` and `updated_at` are wall-clock audit
//!   times ("when the human acted"). They are stored apart and never compared.
//! - **Windowed and paged.** [`Repository::authored_annotations_in`] answers only a box in
//!   (time × frequency), `limit`-bounded, because a long research session accumulates thousands.
//! - Validated here ([`AuthoredAnnotation::validate`]) so every writer gets the same limits.
//! - Databases created before T-816 get the table on first use (same pattern as selections), so no
//!   schema migration is needed and the MAP-17..19 stores can land in any order.

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::measurements::MeasurementProvenance;
use super::{RepoError, Repository, blob};
use crate::ids::AnnotationId;
use crate::sigmf;
use crate::time::Timestamp;

/// Longest label, characters.
pub const AUTHORED_LABEL_MAX: usize = 120;
/// Longest body text, characters.
pub const AUTHORED_BODY_MAX: usize = 4000;
/// Longest `device_id` / actor / collection reference, characters.
pub const AUTHORED_REF_MAX: usize = 128;
/// Most rows one [`Repository::authored_annotations_in`] page returns.
pub const AUTHORED_PAGE_MAX: usize = 2000;

const ENSURE_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS authored_annotation (
    annotation_id BLOB    PRIMARY KEY CHECK (length(annotation_id) = 16),
    f_lo          REAL    NOT NULL CHECK (f_lo >= 0),
    f_hi          REAL    NOT NULL CHECK (f_hi >= f_lo),
    t0            INTEGER NOT NULL,
    t1            INTEGER NOT NULL CHECK (t1 >= t0),
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL,
    body          TEXT    NOT NULL
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS idx_authored_annotation_t ON authored_annotation (t0, t1);";

/// What an authored annotation draws as.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthoredKind {
    /// A text note: usually a zero-area point plus a label.
    Text,
    /// A box over a time–frequency region (strictly positive extent in both axes).
    Box,
    /// A marker at a point (or a thin extent).
    Marker,
}

impl AuthoredKind {
    /// The wire form (`text`, `box`, `marker`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Box => "box",
            Self::Marker => "marker",
        }
    }
}

/// A durable, human-authored time–frequency note.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthoredAnnotation {
    /// Id.
    pub id: AnnotationId,
    /// The marker collection (MAP-17) it belongs to, as a UUID string; `None` = loose.
    pub collection_id: Option<String>,
    /// What it draws as.
    pub kind: AuthoredKind,
    /// Lower frequency edge, Hz.
    pub f_lo_hz: f64,
    /// Upper frequency edge, Hz.
    pub f_hi_hz: f64,
    /// Capture-clock start.
    #[serde(rename = "t0_ns")]
    pub t0: Timestamp,
    /// Capture-clock end (fixed: a human-set extent does not grow to the live edge).
    #[serde(rename = "t1_ns")]
    pub t1: Timestamp,
    /// Short label.
    pub label: String,
    /// Optional longer text.
    pub body: Option<String>,
    /// Token fingerprint of the author.
    pub author: Option<String>,
    /// The docs/25 §2 stamp.
    pub provenance: MeasurementProvenance,
    /// Wall-clock creation.
    #[serde(rename = "created_at_ns")]
    pub created_at: Timestamp,
    /// Wall-clock last change.
    #[serde(rename = "updated_at_ns")]
    pub updated_at: Timestamp,
}

fn invalid(msg: impl Into<String>) -> RepoError {
    RepoError::Invalid(msg.into())
}

fn short_ref(what: &str, v: Option<&String>) -> Result<(), RepoError> {
    if let Some(s) = v
        && (s.trim() != s || s.is_empty() || s.chars().count() > AUTHORED_REF_MAX)
    {
        return Err(invalid(format!(
            "{what} must be 1..={AUTHORED_REF_MAX} characters without surrounding whitespace"
        )));
    }
    Ok(())
}

impl AuthoredAnnotation {
    /// Checks every limit in the module docs.
    pub fn validate(&self) -> Result<(), RepoError> {
        let label = self.label.trim();
        if label.is_empty() || label != self.label || label.chars().count() > AUTHORED_LABEL_MAX {
            return Err(invalid(format!(
                "annotation label must be 1..={AUTHORED_LABEL_MAX} characters without \
                 surrounding whitespace"
            )));
        }
        if self
            .body
            .as_ref()
            .is_some_and(|b| b.chars().count() > AUTHORED_BODY_MAX)
        {
            return Err(invalid(format!(
                "annotation body must be at most {AUTHORED_BODY_MAX} characters"
            )));
        }
        if !(self.f_lo_hz.is_finite() && self.f_hi_hz.is_finite()) {
            return Err(invalid("annotation f_lo_hz and f_hi_hz must be finite"));
        }
        if !(self.f_lo_hz >= 0.0 && self.f_hi_hz >= self.f_lo_hz) {
            return Err(invalid(format!(
                "annotation needs 0 <= f_lo_hz <= f_hi_hz, got {} .. {}",
                self.f_lo_hz, self.f_hi_hz
            )));
        }
        if self.t1 < self.t0 {
            return Err(invalid("annotation needs t0_s <= t1_s"));
        }
        if self.kind == AuthoredKind::Box && !(self.f_hi_hz > self.f_lo_hz && self.t1 > self.t0) {
            return Err(invalid(
                "a box annotation needs a positive extent in both frequency and time",
            ));
        }
        if let Some(c) = &self.collection_id
            && c.parse::<uuid::Uuid>().is_err()
        {
            return Err(invalid("annotation collection_id must be a UUID"));
        }
        short_ref("annotation author", self.author.as_ref())?;
        self.provenance.validate()?;
        if self.updated_at < self.created_at {
            return Err(invalid("annotation updated_at is before created_at"));
        }
        Ok(())
    }

    /// The SigMF-adjacent export shape (docs/25 §5, docs/sigmf-extension.md): a SigMF
    /// `annotations` entry relative to a recording that starts (sample 0) at `recording_start` on
    /// the capture clock and runs at `sample_rate_hz`. Standard keys carry the geometry, label and
    /// body; the `hackriff:annotation` block carries what SigMF has no home for — `author`,
    /// `provenance`, `collection_id`, `kind` and `authored: true` — so an importer can never
    /// confuse it with a `hackriff:truth` ground-truth entry.
    ///
    /// `None` when the annotation ends before the recording starts or the rate is not usable. An
    /// annotation that starts before the recording is clipped to sample 0.
    pub fn to_sigmf(
        &self,
        recording_start: Timestamp,
        sample_rate_hz: f64,
    ) -> Option<sigmf::Annotation> {
        if !(sample_rate_hz.is_finite() && sample_rate_hz > 0.0) {
            return None;
        }
        let rel = |t: Timestamp| (t.as_unix_nanos() - recording_start.as_unix_nanos()) as f64 / 1e9;
        let (s0, s1) = (rel(self.t0).max(0.0), rel(self.t1));
        if s1 < 0.0 {
            return None;
        }
        let start = (s0 * sample_rate_hz).round() as u64;
        let end = (s1 * sample_rate_hz).round() as u64;
        let prov = &self.provenance;
        let secs = |t: Timestamp| t.as_unix_nanos() as f64 / 1e9;
        let mut extra = Map::new();
        extra.insert(
            sigmf::AUTHORED_ANNOTATION_KEY.to_owned(),
            json!({
                "authored": true,
                "kind": self.kind.as_str(),
                "id": self.id.to_string(),
                "author": self.author,
                "collection_id": self.collection_id,
                "provenance": {
                    "device_id": prov.device_id,
                    "center_hz": prov.center_hz,
                    "span_hz": prov.span_hz,
                    "sample_rate_hz": prov.sample_rate_hz,
                    "t_capture": [secs(prov.t_capture[0]), secs(prov.t_capture[1])],
                    "tier": prov.tier.as_str(),
                    "authored_s": secs(prov.authored_at),
                    "actor": prov.actor,
                    "authored": true,
                },
            }),
        );
        Some(sigmf::Annotation {
            sample_start: start,
            sample_count: (end > start).then_some(end - start),
            freq_lower_edge: Some(self.f_lo_hz),
            freq_upper_edge: Some(self.f_hi_hz),
            label: Some(self.label.clone()),
            comment: self.body.clone(),
            truth: None,
            extra,
        })
    }
}

/// A page of [`Repository::authored_annotations_in`].
#[derive(Clone, Debug, PartialEq)]
pub struct AuthoredPage {
    /// The rows, newest capture time first.
    pub rows: Vec<AuthoredAnnotation>,
    /// How many rows the whole window holds.
    pub matched: u64,
}

impl Repository {
    fn ensure_authored_table(&self) -> Result<(), RepoError> {
        self.conn.execute_batch(ENSURE_TABLE)?;
        Ok(())
    }

    /// Stores a new annotation (validated). Its id must be new ([`RepoError::Engine`] otherwise;
    /// check with [`Repository::authored_annotation`] first to tell a duplicate apart).
    pub fn insert_authored_annotation(&mut self, a: &AuthoredAnnotation) -> Result<(), RepoError> {
        a.validate()?;
        self.ensure_authored_table()?;
        let tx = self.write_tx()?;
        tx.execute(
            "INSERT INTO authored_annotation (annotation_id, f_lo, f_hi, t0, t1, created_at, \
             updated_at, body) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                blob(a.id),
                a.f_lo_hz,
                a.f_hi_hz,
                a.t0.as_unix_nanos(),
                a.t1.as_unix_nanos(),
                a.created_at.as_unix_nanos(),
                a.updated_at.as_unix_nanos(),
                serde_json::to_string(a)?
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Replaces a stored annotation (validated). `created_at` is kept from the stored row;
    /// `updated_at` never goes below it.
    pub fn update_authored_annotation(
        &mut self,
        a: &AuthoredAnnotation,
    ) -> Result<AuthoredAnnotation, RepoError> {
        let stored = self.authored_annotation(a.id)?;
        let mut next = a.clone();
        next.created_at = stored.created_at;
        if next.updated_at < next.created_at {
            next.updated_at = next.created_at;
        }
        next.validate()?;
        let tx = self.write_tx()?;
        tx.execute(
            "UPDATE authored_annotation SET f_lo = ?2, f_hi = ?3, t0 = ?4, t1 = ?5, \
             updated_at = ?6, body = ?7 WHERE annotation_id = ?1",
            params![
                blob(next.id),
                next.f_lo_hz,
                next.f_hi_hz,
                next.t0.as_unix_nanos(),
                next.t1.as_unix_nanos(),
                next.updated_at.as_unix_nanos(),
                serde_json::to_string(&next)?
            ],
        )?;
        tx.commit()?;
        Ok(next)
    }

    /// Deletes an annotation; returns what was deleted.
    pub fn delete_authored_annotation(
        &mut self,
        id: AnnotationId,
    ) -> Result<AuthoredAnnotation, RepoError> {
        let stored = self.authored_annotation(id)?;
        let tx = self.write_tx()?;
        tx.execute(
            "DELETE FROM authored_annotation WHERE annotation_id = ?1",
            [blob(id)],
        )?;
        tx.commit()?;
        Ok(stored)
    }

    /// One annotation.
    pub fn authored_annotation(&self, id: AnnotationId) -> Result<AuthoredAnnotation, RepoError> {
        self.ensure_authored_table()?;
        let body: Option<String> = self
            .conn
            .prepare_cached("SELECT body FROM authored_annotation WHERE annotation_id = ?1")?
            .query_row([blob(id)], |r| r.get(0))
            .optional()?;
        match body {
            Some(b) => Ok(serde_json::from_str(&b)?),
            None => Err(RepoError::NotFound {
                kind: "annotation",
                id: id.to_string(),
            }),
        }
    }

    /// The annotations that intersect the box `[f_lo_hz, f_hi_hz] × [t0, t1]` (closed on both
    /// axes, so a zero-area note on the edge is inside), newest capture time first (`t1`, then
    /// `t0`, then id — a total order, so paging is stable), skipping `offset` and returning at
    /// most `limit` (capped at [`AUTHORED_PAGE_MAX`]), plus how many the whole window holds.
    pub fn authored_annotations_in(
        &self,
        f_lo_hz: f64,
        f_hi_hz: f64,
        t0: Timestamp,
        t1: Timestamp,
        offset: usize,
        limit: usize,
    ) -> Result<AuthoredPage, RepoError> {
        self.ensure_authored_table()?;
        const WHERE: &str = "f_lo <= ?2 AND f_hi >= ?1 AND t0 <= ?4 AND t1 >= ?3";
        let (lo, hi, a, b) = (f_lo_hz, f_hi_hz, t0.as_unix_nanos(), t1.as_unix_nanos());
        let matched: i64 = self
            .conn
            .prepare_cached(&format!(
                "SELECT COUNT(*) FROM authored_annotation WHERE {WHERE}"
            ))?
            .query_row(params![lo, hi, a, b], |r| r.get(0))?;
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT body FROM authored_annotation WHERE {WHERE} \
             ORDER BY t1 DESC, t0 DESC, annotation_id LIMIT ?5 OFFSET ?6"
        ))?;
        let texts = stmt
            .query_map(
                params![
                    lo,
                    hi,
                    a,
                    b,
                    limit.min(AUTHORED_PAGE_MAX) as i64,
                    offset as i64
                ],
                |r| r.get::<_, String>(0),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        let rows = texts
            .iter()
            .map(|t| serde_json::from_str(t).map_err(RepoError::from))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(AuthoredPage {
            rows,
            matched: matched.max(0) as u64,
        })
    }
}

/// `Value` of the SigMF block key, for callers that build JSON by hand.
pub fn authored_block(a: &sigmf::Annotation) -> Option<&Value> {
    a.extra.get(sigmf::AUTHORED_ANNOTATION_KEY)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: f64) -> Timestamp {
        Timestamp::from_unix_nanos((s * 1e9) as i64)
    }

    fn note(kind: AuthoredKind, f: (f64, f64), t: (f64, f64), label: &str) -> AuthoredAnnotation {
        let now = Timestamp::now();
        AuthoredAnnotation {
            id: AnnotationId::new(),
            collection_id: None,
            kind,
            f_lo_hz: f.0,
            f_hi_hz: f.1,
            t0: ts(t.0),
            t1: ts(t.1),
            label: label.to_owned(),
            body: None,
            author: Some("tok-abc".into()),
            provenance: MeasurementProvenance {
                device_id: Some("hackrf:0001".into()),
                center_hz: 100.3e6,
                span_hz: 2.4e6,
                sample_rate_hz: Some(2e6),
                t_capture: [ts(t.0 - 5.0), ts(t.1 + 5.0)],
                tier: crate::MeasurementTier::LiveIq,
                authored_at: now,
                actor: Some("tok-abc".into()),
                authored: true,
            },
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn round_trip_update_delete_and_window_paging() {
        let mut repo = Repository::open_in_memory().unwrap();
        let a = note(
            AuthoredKind::Box,
            (100e6, 100.2e6),
            (1000.0, 1010.0),
            "pager?",
        );
        let b = note(
            AuthoredKind::Text,
            (100.1e6, 100.1e6),
            (1005.0, 1005.0),
            "note",
        );
        let far = note(
            AuthoredKind::Marker,
            (433.9e6, 433.9e6),
            (1005.0, 1005.0),
            "far",
        );
        for n in [&a, &b, &far] {
            repo.insert_authored_annotation(n).unwrap();
        }
        assert_eq!(repo.authored_annotation(a.id).unwrap(), a);

        // The window selects by intersection in both axes; newest capture time first.
        let page = repo
            .authored_annotations_in(99e6, 101e6, ts(900.0), ts(2000.0), 0, 10)
            .unwrap();
        assert_eq!(page.matched, 2);
        assert_eq!(
            page.rows.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![a.id, b.id]
        );
        // Paging: one at a time, stable.
        let p1 = repo
            .authored_annotations_in(99e6, 101e6, ts(900.0), ts(2000.0), 1, 1)
            .unwrap();
        assert_eq!((p1.matched, p1.rows[0].id), (2, b.id));
        // Outside in time: nothing.
        let none = repo
            .authored_annotations_in(99e6, 101e6, ts(0.0), ts(999.0), 0, 10)
            .unwrap();
        assert_eq!((none.matched, none.rows.len()), (0, 0));

        let mut edited = a.clone();
        edited.label = "pager (POCSAG)".into();
        edited.updated_at = Timestamp::now();
        let saved = repo.update_authored_annotation(&edited).unwrap();
        assert_eq!(saved.created_at, a.created_at);
        assert_eq!(
            repo.authored_annotation(a.id).unwrap().label,
            "pager (POCSAG)"
        );

        assert_eq!(repo.delete_authored_annotation(a.id).unwrap().id, a.id);
        assert!(matches!(
            repo.authored_annotation(a.id),
            Err(RepoError::NotFound { .. })
        ));
    }

    #[test]
    fn validation_refuses_bad_geometry_and_labels() {
        let mut repo = Repository::open_in_memory().unwrap();
        let flat_box = note(AuthoredKind::Box, (1e6, 1e6), (0.0, 1.0), "x");
        assert!(matches!(
            repo.insert_authored_annotation(&flat_box),
            Err(RepoError::Invalid(_))
        ));
        let inverted = note(AuthoredKind::Text, (2e6, 1e6), (0.0, 0.0), "x");
        assert!(inverted.validate().is_err());
        let blank = note(AuthoredKind::Text, (1e6, 1e6), (0.0, 0.0), " ");
        assert!(blank.validate().is_err());
        let mut bad_collection = note(AuthoredKind::Text, (1e6, 1e6), (0.0, 0.0), "x");
        bad_collection.collection_id = Some("not-a-uuid".into());
        assert!(bad_collection.validate().is_err());
        let mut unauthored = note(AuthoredKind::Text, (1e6, 1e6), (0.0, 0.0), "x");
        unauthored.provenance.authored = false;
        assert!(unauthored.validate().is_err());
    }

    #[test]
    fn sigmf_export_is_authored_and_never_truth() {
        let mut a = note(
            AuthoredKind::Box,
            (100e6, 100.2e6),
            (1002.0, 1003.5),
            "burst",
        );
        a.body = Some("seen twice".into());
        let s = a.to_sigmf(ts(1000.0), 2e6).unwrap();
        assert_eq!(s.sample_start, 4_000_000);
        assert_eq!(s.sample_count, Some(3_000_000));
        assert_eq!(s.freq_lower_edge, Some(100e6));
        assert_eq!(s.freq_upper_edge, Some(100.2e6));
        assert_eq!(s.label.as_deref(), Some("burst"));
        assert_eq!(s.comment.as_deref(), Some("seen twice"));
        assert!(s.truth.is_none(), "an authored note is never ground truth");
        let block = authored_block(&s).unwrap();
        assert_eq!(block["authored"], true);
        assert_eq!(block["provenance"]["tier"], "live-iq");
        assert_eq!(block["author"], "tok-abc");
        // It survives a SigMF JSON round trip under its own key.
        let text = serde_json::to_string(&s).unwrap();
        assert!(text.contains(sigmf::AUTHORED_ANNOTATION_KEY));
        assert!(!text.contains(sigmf::TRUTH_KEY));
        // Wholly before the recording: nothing to export.
        assert!(a.to_sigmf(ts(2000.0), 2e6).is_none());
    }
}
