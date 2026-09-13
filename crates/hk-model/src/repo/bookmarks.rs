//! User markers and bookmarks (T-050): named frequencies the control API persists.
//!
//! - **User metadata only.** A bookmark holds a frequency, an optional bandwidth, a name and a
//!   note the user typed; it never carries signal content, so it is not content-gated.
//! - **Mutable.** Unlike measurement and interpretation rows, bookmarks may be updated and
//!   deleted; `updated_at` records the last change and `created_at` never moves.
//! - **Validated here** ([`Bookmark::validate`]), so every writer (HTTP API, CLI, tests) gets the
//!   same limits: name 1–[`BOOKMARK_NAME_MAX`] characters after trimming, centre finite and
//!   positive, bandwidth finite and positive when set, note at most [`BOOKMARK_NOTE_MAX`]
//!   characters.
//! - Databases created before T-050 (same pre-release schema version) get the table on first use.

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::{RepoError, Repository, blob, enum_text};
use crate::ids::BookmarkId;
use crate::time::Timestamp;

/// Longest bookmark name, characters.
pub const BOOKMARK_NAME_MAX: usize = 120;
/// Longest bookmark note, characters.
pub const BOOKMARK_NOTE_MAX: usize = 2000;
/// Most bookmarks [`Repository::bookmarks`] returns.
pub const BOOKMARKS_MAX: usize = 10_000;

const ENSURE_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS bookmark (
    bookmark_id  BLOB    PRIMARY KEY CHECK (length(bookmark_id) = 16),
    kind         TEXT    NOT NULL CHECK (kind IN ('marker', 'bookmark')),
    name         TEXT    NOT NULL CHECK (length(name) BETWEEN 1 AND 120),
    f_center     REAL    NOT NULL CHECK (f_center > 0),
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    body         TEXT    NOT NULL
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS idx_bookmark_f_center ON bookmark (f_center);";

/// What the user placed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BookmarkKind {
    /// A transient marker on the spectrum (a frequency to come back to in this session).
    Marker,
    /// A saved bookmark.
    #[default]
    Bookmark,
}

/// A named frequency.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bookmark {
    /// Id.
    pub id: BookmarkId,
    /// Marker or bookmark.
    pub kind: BookmarkKind,
    /// Name (trimmed, 1–[`BOOKMARK_NAME_MAX`] characters).
    pub name: String,
    /// Centre frequency, Hz.
    pub f_center_hz: f64,
    /// Bandwidth, Hz; `None` for a single frequency.
    pub bandwidth_hz: Option<f64>,
    /// Free-text note.
    pub note: Option<String>,
    /// When it was created.
    pub created_at: Timestamp,
    /// When it last changed.
    pub updated_at: Timestamp,
}

impl Bookmark {
    /// A new bookmark named `name` at `f_center_hz`, created now.
    pub fn new(kind: BookmarkKind, name: impl Into<String>, f_center_hz: f64) -> Self {
        let now = Timestamp::now();
        Self {
            id: BookmarkId::new(),
            kind,
            name: name.into(),
            f_center_hz,
            bandwidth_hz: None,
            note: None,
            created_at: now,
            updated_at: now,
        }
    }

    /// Checks the limits in the module docs; the name is compared after trimming.
    pub fn validate(&self) -> Result<(), RepoError> {
        let name = self.name.trim();
        if name.is_empty() || name.chars().count() > BOOKMARK_NAME_MAX || name != self.name {
            return Err(RepoError::Invalid(format!(
                "bookmark name must be 1..={BOOKMARK_NAME_MAX} characters without surrounding \
                 whitespace"
            )));
        }
        if !(self.f_center_hz.is_finite() && self.f_center_hz > 0.0) {
            return Err(RepoError::Invalid(format!(
                "bookmark f_center_hz must be finite and positive, got {}",
                self.f_center_hz
            )));
        }
        if let Some(bw) = self.bandwidth_hz {
            if !(bw.is_finite() && bw > 0.0) {
                return Err(RepoError::Invalid(format!(
                    "bookmark bandwidth_hz must be finite and positive, got {bw}"
                )));
            }
        }
        if self
            .note
            .as_ref()
            .is_some_and(|n| n.chars().count() > BOOKMARK_NOTE_MAX)
        {
            return Err(RepoError::Invalid(format!(
                "bookmark note must be at most {BOOKMARK_NOTE_MAX} characters"
            )));
        }
        if self.updated_at < self.created_at {
            return Err(RepoError::Invalid(
                "bookmark updated_at is before created_at".into(),
            ));
        }
        Ok(())
    }
}

impl Repository {
    fn ensure_bookmark_table(&self) -> Result<(), RepoError> {
        self.conn.execute_batch(ENSURE_TABLE)?;
        Ok(())
    }

    /// Stores a new bookmark (validated). Its id must be new.
    pub fn insert_bookmark(&mut self, bookmark: &Bookmark) -> Result<(), RepoError> {
        bookmark.validate()?;
        self.ensure_bookmark_table()?;
        let tx = self.write_tx()?;
        tx.execute(
            "INSERT INTO bookmark (bookmark_id, kind, name, f_center, created_at, updated_at, body) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                blob(bookmark.id),
                enum_text(&bookmark.kind)?,
                bookmark.name,
                bookmark.f_center_hz,
                bookmark.created_at.as_unix_nanos(),
                bookmark.updated_at.as_unix_nanos(),
                serde_json::to_string(bookmark)?
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Replaces a stored bookmark (validated). `created_at` is kept from the stored row.
    pub fn update_bookmark(&mut self, bookmark: &Bookmark) -> Result<Bookmark, RepoError> {
        self.ensure_bookmark_table()?;
        let stored = self.bookmark(bookmark.id)?;
        let mut next = bookmark.clone();
        next.created_at = stored.created_at;
        if next.updated_at < next.created_at {
            next.updated_at = next.created_at;
        }
        next.validate()?;
        let tx = self.write_tx()?;
        let n = tx.execute(
            "UPDATE bookmark SET kind = ?2, name = ?3, f_center = ?4, updated_at = ?5, body = ?6 \
             WHERE bookmark_id = ?1",
            params![
                blob(next.id),
                enum_text(&next.kind)?,
                next.name,
                next.f_center_hz,
                next.updated_at.as_unix_nanos(),
                serde_json::to_string(&next)?
            ],
        )?;
        tx.commit()?;
        if n == 0 {
            return Err(not_found(next.id));
        }
        Ok(next)
    }

    /// Deletes a bookmark; returns what was deleted.
    pub fn delete_bookmark(&mut self, id: BookmarkId) -> Result<Bookmark, RepoError> {
        self.ensure_bookmark_table()?;
        let stored = self.bookmark(id)?;
        let tx = self.write_tx()?;
        tx.execute("DELETE FROM bookmark WHERE bookmark_id = ?1", [blob(id)])?;
        tx.commit()?;
        Ok(stored)
    }

    /// One bookmark.
    pub fn bookmark(&self, id: BookmarkId) -> Result<Bookmark, RepoError> {
        self.ensure_bookmark_table()?;
        let body: Option<String> = self
            .conn
            .prepare_cached("SELECT body FROM bookmark WHERE bookmark_id = ?1")?
            .query_row([blob(id)], |r| r.get(0))
            .optional()?;
        match body {
            Some(b) => Ok(serde_json::from_str(&b)?),
            None => Err(not_found(id)),
        }
    }

    /// Every bookmark, by centre frequency then creation (at most [`BOOKMARKS_MAX`]).
    pub fn bookmarks(&self) -> Result<Vec<Bookmark>, RepoError> {
        self.ensure_bookmark_table()?;
        let mut stmt = self.conn.prepare_cached(
            "SELECT body FROM bookmark ORDER BY f_center, created_at, bookmark_id LIMIT ?1",
        )?;
        let texts = stmt
            .query_map([BOOKMARKS_MAX as i64], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        texts
            .iter()
            .map(|t| serde_json::from_str(t).map_err(RepoError::from))
            .collect()
    }
}

fn not_found(id: BookmarkId) -> RepoError {
    RepoError::NotFound {
        kind: "bookmark",
        id: id.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bookmarks_round_trip_update_and_delete() {
        let mut repo = Repository::open_in_memory().unwrap();
        assert!(repo.bookmarks().unwrap().is_empty());
        let mut a = Bookmark::new(BookmarkKind::Bookmark, "FM 101.3", 101.3e6);
        a.bandwidth_hz = Some(200e3);
        a.note = Some("strong local station".into());
        let b = Bookmark::new(BookmarkKind::Marker, "pager?", 930.5e6);
        repo.insert_bookmark(&b).unwrap();
        repo.insert_bookmark(&a).unwrap();
        assert_eq!(repo.bookmarks().unwrap(), vec![a.clone(), b.clone()]);
        assert!(repo.insert_bookmark(&a).is_err(), "duplicate id");

        let mut renamed = a.clone();
        renamed.name = "FM 101.3 (RDS)".into();
        renamed.created_at = Timestamp::from_unix_nanos(1);
        renamed.updated_at = Timestamp::now();
        let stored = repo.update_bookmark(&renamed).unwrap();
        assert_eq!(stored.created_at, a.created_at, "created_at never moves");
        assert_eq!(repo.bookmark(a.id).unwrap().name, "FM 101.3 (RDS)");

        assert_eq!(repo.delete_bookmark(b.id).unwrap().name, "pager?");
        assert!(matches!(
            repo.bookmark(b.id),
            Err(RepoError::NotFound { .. })
        ));
        assert!(repo.delete_bookmark(b.id).is_err());
        assert!(repo.update_bookmark(&b).is_err());
    }

    #[test]
    fn invalid_bookmarks_are_refused() {
        let mut repo = Repository::open_in_memory().unwrap();
        for bad in [
            Bookmark::new(BookmarkKind::Marker, "", 100e6),
            Bookmark::new(BookmarkKind::Marker, " padded ", 100e6),
            Bookmark::new(BookmarkKind::Marker, "x".repeat(121), 100e6),
            Bookmark::new(BookmarkKind::Marker, "nan", f64::NAN),
            Bookmark::new(BookmarkKind::Marker, "neg", -1.0),
            Bookmark {
                bandwidth_hz: Some(0.0),
                ..Bookmark::new(BookmarkKind::Marker, "bw", 100e6)
            },
            Bookmark {
                note: Some("n".repeat(2001)),
                ..Bookmark::new(BookmarkKind::Marker, "note", 100e6)
            },
        ] {
            assert!(
                matches!(repo.insert_bookmark(&bad), Err(RepoError::Invalid(_))),
                "{bad:?}"
            );
        }
        assert!(repo.bookmarks().unwrap().is_empty());
    }

    #[test]
    fn a_database_without_the_table_gets_it_on_first_use() {
        let mut repo = Repository::open_in_memory().unwrap();
        repo.conn.execute_batch("DROP TABLE bookmark").unwrap();
        let b = Bookmark::new(BookmarkKind::Bookmark, "ADS-B", 1090e6);
        repo.insert_bookmark(&b).unwrap();
        assert_eq!(repo.bookmarks().unwrap(), vec![b]);
    }
}
