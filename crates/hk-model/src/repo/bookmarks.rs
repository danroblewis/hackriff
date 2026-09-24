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
//! - **A facade over the reserved `Bookmarks` collection (T-817, docs/25 §10.6).** A bookmark
//!   *is* a frequency-only marker of [`super::collections::BOOKMARKS_COLLECTION`] — the same row,
//!   read and written through `repo/collections.rs` — so `/api/bookmarks` and `/api/collections`
//!   can never disagree. Rows in the legacy T-050 `bookmark` table move into that collection on
//!   first use, keeping id, kind, name, note and timestamps.

use serde::{Deserialize, Serialize};

use super::collections::{AuthoredProvenance, BOOKMARKS_COLLECTION, Marker};
use super::{RepoError, Repository};
use crate::ids::{BookmarkId, MarkerId};
use crate::time::Timestamp;

/// Longest bookmark name, characters.
pub const BOOKMARK_NAME_MAX: usize = 120;
/// Longest bookmark note, characters.
pub const BOOKMARK_NOTE_MAX: usize = 2000;
/// Most bookmarks [`Repository::bookmarks`] returns.
pub const BOOKMARKS_MAX: usize = 10_000;

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

/// The reserved-collection marker a bookmark is (T-817): same id, same row.
pub(super) fn marker_from_bookmark(b: &Bookmark, provenance: AuthoredProvenance) -> Marker {
    Marker {
        id: MarkerId::from_uuid(*b.id.as_uuid()),
        collection_id: BOOKMARKS_COLLECTION,
        name: b.name.clone(),
        note: b.note.clone(),
        f_center_hz: b.f_center_hz,
        bandwidth_hz: b.bandwidth_hz,
        t_center: None,
        duration_s: None,
        bookmark_kind: Some(b.kind),
        provenance,
        created_at: b.created_at,
        updated_at: b.updated_at,
    }
}

/// The bookmark view of a reserved-collection marker.
fn bookmark_from_marker(m: &Marker) -> Bookmark {
    Bookmark {
        id: BookmarkId::from_uuid(*m.id.as_uuid()),
        kind: m.bookmark_kind.unwrap_or_default(),
        name: m.name.clone(),
        f_center_hz: m.f_center_hz,
        bandwidth_hz: m.bandwidth_hz,
        note: m.note.clone(),
        created_at: m.created_at,
        updated_at: m.updated_at,
    }
}

impl Repository {
    /// The reserved-collection marker behind bookmark `id`; a marker in any other collection is
    /// not a bookmark and reads as not found.
    fn bookmark_marker(&self, id: BookmarkId) -> Result<Marker, RepoError> {
        match self.marker(MarkerId::from_uuid(*id.as_uuid())) {
            Ok(m) if m.collection_id == BOOKMARKS_COLLECTION => Ok(m),
            Ok(_) | Err(RepoError::NotFound { .. }) => Err(not_found(id)),
            Err(e) => Err(e),
        }
    }

    /// Stores a new bookmark (validated) as a frequency-only marker of the reserved collection.
    /// Its id must be new.
    pub fn insert_bookmark(&mut self, bookmark: &Bookmark) -> Result<(), RepoError> {
        self.insert_bookmark_authored(bookmark, None)
    }

    /// [`Repository::insert_bookmark`], recording who authored it (a token fingerprint, never the
    /// token) in the marker's provenance.
    pub fn insert_bookmark_authored(
        &mut self,
        bookmark: &Bookmark,
        actor: Option<String>,
    ) -> Result<(), RepoError> {
        bookmark.validate()?;
        let m = marker_from_bookmark(
            bookmark,
            AuthoredProvenance::bare(bookmark.created_at, actor),
        );
        self.insert_marker(&m)
    }

    /// Replaces a stored bookmark (validated). `created_at` is kept from the stored row, and so is
    /// the marker's provenance.
    pub fn update_bookmark(&mut self, bookmark: &Bookmark) -> Result<Bookmark, RepoError> {
        let stored = self.bookmark_marker(bookmark.id)?;
        let mut next = bookmark.clone();
        next.created_at = stored.created_at;
        if next.updated_at < next.created_at {
            next.updated_at = next.created_at;
        }
        next.validate()?;
        let m = marker_from_bookmark(&next, stored.provenance);
        let saved = self.update_marker(&m)?;
        Ok(bookmark_from_marker(&saved))
    }

    /// Deletes a bookmark; returns what was deleted.
    pub fn delete_bookmark(&mut self, id: BookmarkId) -> Result<Bookmark, RepoError> {
        let stored = self.bookmark_marker(id)?;
        self.delete_marker(stored.id)
            .map(|m| bookmark_from_marker(&m))
    }

    /// One bookmark.
    pub fn bookmark(&self, id: BookmarkId) -> Result<Bookmark, RepoError> {
        self.bookmark_marker(id).map(|m| bookmark_from_marker(&m))
    }

    /// Every bookmark, by centre frequency then creation (at most [`BOOKMARKS_MAX`]).
    pub fn bookmarks(&self) -> Result<Vec<Bookmark>, RepoError> {
        Ok(self
            .bookmark_markers(BOOKMARKS_MAX)?
            .iter()
            .map(bookmark_from_marker)
            .collect())
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
    fn a_database_without_the_legacy_table_still_stores_bookmarks() {
        let mut repo = Repository::open_in_memory().unwrap();
        repo.conn.execute_batch("DROP TABLE bookmark").unwrap();
        let b = Bookmark::new(BookmarkKind::Bookmark, "ADS-B", 1090e6);
        repo.insert_bookmark(&b).unwrap();
        assert_eq!(repo.bookmarks().unwrap(), vec![b]);
    }
}
