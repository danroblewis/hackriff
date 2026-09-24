//! Marker collections (T-817, MAP-17; docs/25 §3 and §10): **time-frequency markers grouped into
//! named, toggleable collections**, the durable research state the map UI's collection layer and
//! table read.
//!
//! - **User metadata only.** A marker is a place a human chose — a frequency, optionally a time —
//!   with a name and a note. It mints no candidate, sets no family and never feeds blind detection
//!   (docs/25 §10.7): nothing in the detection path reads these tables.
//! - **A marker is a time–frequency place.** `t_center` absent is a *frequency-only pin* (a band
//!   you always want marked, drawn full height); present, it places a point (`duration_s` absent)
//!   or a box (`duration_s` set) on the capture-time axis — the ADR-0017 model, where a signal is a
//!   time–frequency region and not a carrier.
//! - **Provenance is evidence, not input** ([`AuthoredProvenance`], docs/25 §2). The view context
//!   the author was on (tune, span, capture-clock window, honesty tier, device) is recorded beside
//!   the wall-clock instant and actor of authoring, and the two clocks are stored apart.
//! - **Bookmarks are a facade over one reserved collection** ([`BOOKMARKS_COLLECTION`], docs/25
//!   §10.6). The T-050 bookmark rows and that collection's frequency-only markers are the *same
//!   rows*: `repo/bookmarks.rs` reads and writes them through this module. The reserved collection
//!   cannot be deleted and holds frequency-only markers only, because a bookmark has no time.
//! - **Tables on first use, not a numbered migration.** Like bookmarks and selections, the DDL is
//!   `CREATE … IF NOT EXISTS` run by every entry point, and the one-time move of legacy `bookmark`
//!   rows is idempotent. The four MMAP stores (MAP-16..19) land in parallel, and a numbered
//!   migration each would race for the same `user_version` slot.
//! - **Paged, never unbounded** (docs/25 §10.3): list queries take an offset and a limit and
//!   report how many matched.

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::bookmarks::BookmarkKind;
use super::{RepoError, Repository, blob};
use crate::ids::{CollectionId, MarkerId};
use crate::time::Timestamp;

/// Longest collection or marker name, characters.
pub const COLLECTION_NAME_MAX: usize = 120;
/// Longest collection or marker note, characters.
pub const COLLECTION_NOTE_MAX: usize = 2000;
/// Longest `device_id` / `actor` a provenance stamp stores, characters.
pub const PROVENANCE_TEXT_MAX: usize = 200;
/// Most collections one database holds.
pub const COLLECTIONS_MAX: usize = 10_000;
/// Most markers one collection holds.
pub const MARKERS_PER_COLLECTION_MAX: usize = 100_000;

/// The reserved, un-deletable collection `/api/bookmarks` is a facade over (docs/25 §10.6).
///
/// A fixed UUIDv7-shaped id with a zero timestamp, so it sorts before every collection a user makes.
pub const BOOKMARKS_COLLECTION: CollectionId =
    CollectionId::from_uuid(Uuid::from_u128(0x0000_0000_0000_7000_8000_0000_0000_0b00));
/// The reserved collection's name.
pub const BOOKMARKS_COLLECTION_NAME: &str = "Bookmarks";
/// The reserved collection's colour: the bookmark accent the UI already draws bookmarks in.
pub const BOOKMARKS_COLLECTION_COLOR: &str = "#f5b942";

const ENSURE_TABLES: &str = "\
CREATE TABLE IF NOT EXISTS marker_collection (
    collection_id BLOB    PRIMARY KEY CHECK (length(collection_id) = 16),
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL,
    body          TEXT    NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS marker (
    marker_id     BLOB    PRIMARY KEY CHECK (length(marker_id) = 16),
    collection_id BLOB    NOT NULL CHECK (length(collection_id) = 16),
    f_center      REAL    NOT NULL CHECK (f_center > 0),
    f_lo          REAL    NOT NULL,
    f_hi          REAL    NOT NULL,
    t_lo          INTEGER,
    t_hi          INTEGER,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL,
    body          TEXT    NOT NULL
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS idx_marker_collection ON marker (collection_id, created_at, marker_id);
CREATE INDEX IF NOT EXISTS idx_marker_f_lo ON marker (f_lo);";

/// The honesty tier a pane was drawn at when a mark was authored (docs/25 §2; docs/14 T-341).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ViewTier {
    /// Live-IQ detail.
    LiveIq,
    /// Spectrum history (the tile pyramid).
    SpectrumHistory,
    /// Survey overview (reduced, coarse).
    SurveyOverview,
}

impl ViewTier {
    /// The wire name: `live-iq`, `spectrum-history`, `survey-overview`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LiveIq => "live-iq",
            Self::SpectrumHistory => "spectrum-history",
            Self::SurveyOverview => "survey-overview",
        }
    }

    /// Parses a wire name.
    pub fn parse(s: &str) -> Option<Self> {
        [Self::LiveIq, Self::SpectrumHistory, Self::SurveyOverview]
            .into_iter()
            .find(|t| t.as_str() == s)
    }
}

/// **What produced an authored mark** (docs/25 §2): the view context the author was on plus who
/// authored it and when. Written by the backend; a client supplies only the view part.
///
/// `t_capture` is on the **capture clock** (when the air was); `authored_at` is on the **wall
/// clock** (when the human acted). They are different quantities and never substituted for each
/// other — the bug the UI found five times (T-379/T-384/T-389).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthoredProvenance {
    /// The front end whose coverage the mark rests on, as the pane named it; `None` when the pane
    /// named none. Never a placeholder.
    pub device_id: Option<String>,
    /// The view's centre frequency when authored, Hz.
    pub center_hz: Option<f64>,
    /// The view's frequency span when authored, Hz.
    pub span_hz: Option<f64>,
    /// The named front end's sample rate at authoring time, Hz, when the server could resolve it.
    pub sample_rate_hz: Option<f64>,
    /// The capture-clock window the mark refers to (`start <= end`).
    #[serde(rename = "t_capture_ns")]
    pub t_capture: Option<(Timestamp, Timestamp)>,
    /// The honesty tier the pane was drawn at.
    pub tier: Option<ViewTier>,
    /// Wall-clock instant of authoring.
    #[serde(rename = "authored_at_ns")]
    pub authored_at: Timestamp,
    /// Who authored it: a token fingerprint, never the token. `None` when unknown.
    pub actor: Option<String>,
}

impl AuthoredProvenance {
    /// A stamp with no view context, authored at `at` by `actor` (the bookmark facade's stamp: a
    /// bookmark is a frequency-only pin with no pane behind it).
    pub fn bare(at: Timestamp, actor: Option<String>) -> Self {
        Self {
            device_id: None,
            center_hz: None,
            span_hz: None,
            sample_rate_hz: None,
            t_capture: None,
            tier: None,
            authored_at: at,
            actor,
        }
    }

    fn validate(&self) -> Result<(), RepoError> {
        for (key, v) in [
            ("center_hz", self.center_hz),
            ("span_hz", self.span_hz),
            ("sample_rate_hz", self.sample_rate_hz),
        ] {
            if let Some(v) = v {
                if !(v.is_finite() && v > 0.0) {
                    return Err(invalid(format!(
                        "provenance {key} must be finite and positive, got {v}"
                    )));
                }
            }
        }
        if let Some((a, b)) = self.t_capture {
            if b < a {
                return Err(invalid("provenance t_capture ends before it starts"));
            }
        }
        for (key, v) in [("device_id", &self.device_id), ("actor", &self.actor)] {
            if v.as_ref()
                .is_some_and(|s| s.is_empty() || s.chars().count() > PROVENANCE_TEXT_MAX)
            {
                return Err(invalid(format!(
                    "provenance {key} must be 1..={PROVENANCE_TEXT_MAX} characters"
                )));
            }
        }
        Ok(())
    }
}

/// A named, toggleable collection of markers (docs/25 §3).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Collection {
    /// Id.
    pub id: CollectionId,
    /// Name (trimmed, 1–[`COLLECTION_NAME_MAX`] characters).
    pub name: String,
    /// Free-text note.
    pub note: Option<String>,
    /// Display colour, `#rrggbb`; `None` lets the client pick.
    pub color: Option<String>,
    /// Whether the collection's layer is shown — the stored default the layers menu toggles.
    pub visible: bool,
    /// The reserved bookmarks collection ([`BOOKMARKS_COLLECTION`]): cannot be deleted, and holds
    /// frequency-only markers only.
    pub reserved: bool,
    /// When it was created (wall clock).
    #[serde(rename = "created_at_ns")]
    pub created_at: Timestamp,
    /// When it last changed (wall clock).
    #[serde(rename = "updated_at_ns")]
    pub updated_at: Timestamp,
}

impl Collection {
    /// A new, visible, user collection named `name`, created now.
    pub fn new(name: impl Into<String>) -> Self {
        let now = Timestamp::now();
        Self {
            id: CollectionId::new(),
            name: name.into(),
            note: None,
            color: None,
            visible: true,
            reserved: false,
            created_at: now,
            updated_at: now,
        }
    }

    /// Checks name, note, colour and timestamps.
    pub fn validate(&self) -> Result<(), RepoError> {
        check_name("collection", &self.name)?;
        check_note("collection", self.note.as_deref())?;
        if let Some(c) = &self.color {
            if !is_hex_color(c) {
                return Err(invalid(format!(
                    "collection color must be \"#rrggbb\", got {c:?}"
                )));
            }
        }
        if self.reserved != (self.id == BOOKMARKS_COLLECTION) {
            return Err(invalid(
                "only the bookmarks collection is reserved, and it always is",
            ));
        }
        if self.updated_at < self.created_at {
            return Err(invalid("collection updated_at is before created_at"));
        }
        Ok(())
    }
}

/// A collection plus how many markers it holds.
#[derive(Clone, Debug, PartialEq)]
pub struct CollectionSummary {
    /// The collection.
    pub collection: Collection,
    /// Its member count.
    pub member_count: u64,
}

/// A time–frequency marker (docs/25 §3).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Marker {
    /// Id.
    pub id: MarkerId,
    /// The collection it belongs to (fixed at creation).
    pub collection_id: CollectionId,
    /// Name (trimmed, 1–[`COLLECTION_NAME_MAX`] characters).
    pub name: String,
    /// Free-text note.
    pub note: Option<String>,
    /// Centre frequency, Hz.
    pub f_center_hz: f64,
    /// Bandwidth, Hz; `None` for a single frequency.
    pub bandwidth_hz: Option<f64>,
    /// Time centre on the capture clock; `None` for a frequency-only pin.
    #[serde(rename = "t_center_ns")]
    pub t_center: Option<Timestamp>,
    /// Time extent, seconds, centred on `t_center`; `None` for a point. Needs `t_center`.
    pub duration_s: Option<f64>,
    /// The T-050 `kind` a bookmark carries, kept so the `/api/bookmarks` facade answers the kind it
    /// was given. `None` reads as `bookmark`. Meaningful only in the reserved collection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bookmark_kind: Option<BookmarkKind>,
    /// What produced it.
    pub provenance: AuthoredProvenance,
    /// When it was created (wall clock).
    #[serde(rename = "created_at_ns")]
    pub created_at: Timestamp,
    /// When it last changed (wall clock).
    #[serde(rename = "updated_at_ns")]
    pub updated_at: Timestamp,
}

impl Marker {
    /// A new frequency-only marker in `collection_id`, created now, with a bare provenance stamp.
    pub fn new(collection_id: CollectionId, name: impl Into<String>, f_center_hz: f64) -> Self {
        let now = Timestamp::now();
        Self {
            id: MarkerId::new(),
            collection_id,
            name: name.into(),
            note: None,
            f_center_hz,
            bandwidth_hz: None,
            t_center: None,
            duration_s: None,
            bookmark_kind: None,
            provenance: AuthoredProvenance::bare(now, None),
            created_at: now,
            updated_at: now,
        }
    }

    /// Checks the marker's own limits (the collection rules are checked on write).
    pub fn validate(&self) -> Result<(), RepoError> {
        check_name("marker", &self.name)?;
        check_note("marker", self.note.as_deref())?;
        if !(self.f_center_hz.is_finite() && self.f_center_hz > 0.0) {
            return Err(invalid(format!(
                "marker f_center_hz must be finite and positive, got {}",
                self.f_center_hz
            )));
        }
        if let Some(bw) = self.bandwidth_hz {
            if !(bw.is_finite() && bw > 0.0) {
                return Err(invalid(format!(
                    "marker bandwidth_hz must be finite and positive, got {bw}"
                )));
            }
            if bw / 2.0 >= self.f_center_hz {
                return Err(invalid(
                    "marker bandwidth_hz reaches below 0 Hz around its centre",
                ));
            }
        }
        match (self.t_center, self.duration_s) {
            (None, Some(_)) => {
                return Err(invalid(
                    "marker duration_s needs t_center_s: a frequency-only pin has no time extent",
                ));
            }
            (_, Some(d)) if !(0.0..1e9).contains(&d) => {
                return Err(invalid(format!(
                    "marker duration_s must be finite and in 0..1e9, got {d}"
                )));
            }
            _ => {}
        }
        self.provenance.validate()?;
        if self.updated_at < self.created_at {
            return Err(invalid("marker updated_at is before created_at"));
        }
        Ok(())
    }

    /// `[f_lo, f_hi]`, Hz.
    pub fn f_range(&self) -> (f64, f64) {
        let half = self.bandwidth_hz.unwrap_or(0.0) / 2.0;
        (self.f_center_hz - half, self.f_center_hz + half)
    }

    /// `[t_lo, t_hi]` on the capture clock; `None` for a frequency-only pin.
    pub fn t_range(&self) -> Option<(Timestamp, Timestamp)> {
        let c = self.t_center?.as_unix_nanos();
        let half = (self.duration_s.unwrap_or(0.0) * 1e9 / 2.0).round() as i64;
        Some((
            Timestamp::from_unix_nanos(c.saturating_sub(half)),
            Timestamp::from_unix_nanos(c.saturating_add(half)),
        ))
    }
}

/// A window box over markers (docs/25 §10.3): frequency and/or capture-time overlap.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MarkerWindow {
    /// `[f_lo, f_hi]`, Hz: markers whose frequency extent overlaps it.
    pub freq: Option<(f64, f64)>,
    /// `[t0, t1]`: markers whose time extent overlaps it. A frequency-only pin matches every time
    /// window — it is a mark for *all* time, not for none.
    pub time: Option<(Timestamp, Timestamp)>,
}

/// One page of a list query.
#[derive(Clone, Debug, PartialEq)]
pub struct StorePage<T> {
    /// The items on this page.
    pub items: Vec<T>,
    /// How many matched in total.
    pub matched: u64,
}

impl Repository {
    /// Creates the collection tables, the reserved bookmarks collection and moves any legacy
    /// T-050 `bookmark` rows into it. Idempotent; cheap once done.
    pub(super) fn ensure_collections(&self) -> Result<(), RepoError> {
        self.conn.execute_batch(ENSURE_TABLES)?;
        let exists: bool = self
            .conn
            .prepare_cached(
                "SELECT EXISTS (SELECT 1 FROM marker_collection WHERE collection_id = ?1)",
            )?
            .query_row([blob(BOOKMARKS_COLLECTION)], |r| r.get(0))?;
        if !exists {
            let now = Timestamp::now();
            let c = Collection {
                id: BOOKMARKS_COLLECTION,
                name: BOOKMARKS_COLLECTION_NAME.into(),
                note: Some(
                    "Frequency-only bookmarks; /api/bookmarks reads and writes these rows.".into(),
                ),
                color: Some(BOOKMARKS_COLLECTION_COLOR.into()),
                visible: true,
                reserved: true,
                created_at: now,
                updated_at: now,
            };
            self.conn.execute(
                "INSERT OR IGNORE INTO marker_collection (collection_id, created_at, updated_at, body) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    blob(c.id),
                    c.created_at.as_unix_nanos(),
                    c.updated_at.as_unix_nanos(),
                    serde_json::to_string(&c)?
                ],
            )?;
        }
        self.migrate_legacy_bookmarks()
    }

    /// Moves rows from the T-050 `bookmark` table (if it exists and holds any) into the reserved
    /// collection, preserving id, kind, name, bandwidth, note and timestamps. Runs in a savepoint,
    /// so it is all-or-nothing and nests inside an open transaction.
    fn migrate_legacy_bookmarks(&self) -> Result<(), RepoError> {
        let has_table: bool = self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'bookmark')",
            [],
            |r| r.get(0),
        )?;
        if !has_table {
            return Ok(());
        }
        let bodies = self
            .conn
            .prepare_cached("SELECT body FROM bookmark")?
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        if bodies.is_empty() {
            return Ok(());
        }
        self.conn.execute_batch("SAVEPOINT legacy_bookmarks")?;
        let moved = (|| -> Result<(), RepoError> {
            for body in &bodies {
                let b: super::bookmarks::Bookmark = serde_json::from_str(body)?;
                let m = super::bookmarks::marker_from_bookmark(
                    &b,
                    AuthoredProvenance::bare(b.created_at, None),
                );
                insert_marker_row(&self.conn, &m, "INSERT OR IGNORE")?;
            }
            self.conn.execute("DELETE FROM bookmark", [])?;
            Ok(())
        })();
        match moved {
            Ok(()) => {
                self.conn.execute_batch("RELEASE legacy_bookmarks")?;
                Ok(())
            }
            Err(e) => {
                let _ = self
                    .conn
                    .execute_batch("ROLLBACK TO legacy_bookmarks; RELEASE legacy_bookmarks");
                Err(e)
            }
        }
    }

    /// Stores a new collection (validated). Its id must be new.
    pub fn insert_collection(&mut self, c: &Collection) -> Result<(), RepoError> {
        c.validate()?;
        if c.reserved {
            return Err(invalid("the bookmarks collection already exists"));
        }
        self.ensure_collections()?;
        let n: i64 = self
            .conn
            .query_row("SELECT count(*) FROM marker_collection", [], |r| r.get(0))?;
        if n as usize >= COLLECTIONS_MAX {
            return Err(invalid(format!(
                "at most {COLLECTIONS_MAX} collections; delete one first"
            )));
        }
        let tx = self.write_tx()?;
        tx.execute(
            "INSERT INTO marker_collection (collection_id, created_at, updated_at, body) \
             VALUES (?1, ?2, ?3, ?4)",
            params![
                blob(c.id),
                c.created_at.as_unix_nanos(),
                c.updated_at.as_unix_nanos(),
                serde_json::to_string(c)?
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Replaces a stored collection (validated). `created_at` and `reserved` are kept.
    pub fn update_collection(&mut self, c: &Collection) -> Result<Collection, RepoError> {
        let stored = self.collection(c.id)?;
        let mut next = c.clone();
        next.created_at = stored.created_at;
        next.reserved = stored.reserved;
        next.updated_at = next.updated_at.max(next.created_at);
        next.validate()?;
        let tx = self.write_tx()?;
        tx.execute(
            "UPDATE marker_collection SET updated_at = ?2, body = ?3 WHERE collection_id = ?1",
            params![
                blob(next.id),
                next.updated_at.as_unix_nanos(),
                serde_json::to_string(&next)?
            ],
        )?;
        tx.commit()?;
        Ok(next)
    }

    /// Deletes a collection and every marker in it; returns it and how many markers went with it.
    /// The reserved bookmarks collection cannot be deleted.
    pub fn delete_collection(&mut self, id: CollectionId) -> Result<(Collection, u64), RepoError> {
        let stored = self.collection(id)?;
        if stored.reserved {
            return Err(invalid(
                "the bookmarks collection is reserved and cannot be deleted",
            ));
        }
        let tx = self.write_tx()?;
        let members = tx.execute("DELETE FROM marker WHERE collection_id = ?1", [blob(id)])?;
        tx.execute(
            "DELETE FROM marker_collection WHERE collection_id = ?1",
            [blob(id)],
        )?;
        tx.commit()?;
        Ok((stored, members as u64))
    }

    /// One collection.
    pub fn collection(&self, id: CollectionId) -> Result<Collection, RepoError> {
        self.ensure_collections()?;
        let body: Option<String> = self
            .conn
            .prepare_cached("SELECT body FROM marker_collection WHERE collection_id = ?1")?
            .query_row([blob(id)], |r| r.get(0))
            .optional()?;
        match body {
            Some(b) => Ok(serde_json::from_str(&b)?),
            None => Err(RepoError::NotFound {
                kind: "collection",
                id: id.to_string(),
            }),
        }
    }

    /// How many markers a collection holds (0 for an unknown one).
    pub fn collection_member_count(&self, id: CollectionId) -> Result<u64, RepoError> {
        self.ensure_collections()?;
        let n: i64 = self
            .conn
            .prepare_cached("SELECT count(*) FROM marker WHERE collection_id = ?1")?
            .query_row([blob(id)], |r| r.get(0))?;
        Ok(n as u64)
    }

    /// A page of collections in creation order (the reserved one first), with member counts.
    pub fn collections(
        &self,
        offset: usize,
        limit: usize,
    ) -> Result<StorePage<CollectionSummary>, RepoError> {
        self.ensure_collections()?;
        let matched: i64 =
            self.conn
                .query_row("SELECT count(*) FROM marker_collection", [], |r| r.get(0))?;
        let rows = self
            .conn
            .prepare_cached(
                "SELECT c.body, (SELECT count(*) FROM marker m WHERE m.collection_id = c.collection_id) \
                 FROM marker_collection c ORDER BY c.created_at, c.collection_id LIMIT ?1 OFFSET ?2",
            )?
            .query_map(params![limit as i64, offset as i64], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let items = rows
            .into_iter()
            .map(|(b, n)| {
                Ok(CollectionSummary {
                    collection: serde_json::from_str(&b)?,
                    member_count: n as u64,
                })
            })
            .collect::<Result<Vec<_>, RepoError>>()?;
        Ok(StorePage {
            items,
            matched: matched as u64,
        })
    }

    /// Stores a new marker (validated) in its collection, which must exist. Its id must be new.
    pub fn insert_marker(&mut self, m: &Marker) -> Result<(), RepoError> {
        m.validate()?;
        let c = self.collection(m.collection_id)?;
        check_membership(&c, m)?;
        if self.collection_member_count(c.id)? as usize >= MARKERS_PER_COLLECTION_MAX {
            return Err(invalid(format!(
                "a collection holds at most {MARKERS_PER_COLLECTION_MAX} markers"
            )));
        }
        let tx = self.write_tx()?;
        insert_marker_row(&tx, m, "INSERT")?;
        tx.commit()?;
        Ok(())
    }

    /// Replaces a stored marker (validated). `created_at` and `collection_id` are kept from the
    /// stored row — a marker does not move between collections.
    pub fn update_marker(&mut self, m: &Marker) -> Result<Marker, RepoError> {
        let stored = self.marker(m.id)?;
        let mut next = m.clone();
        next.created_at = stored.created_at;
        next.collection_id = stored.collection_id;
        next.updated_at = next.updated_at.max(next.created_at);
        next.validate()?;
        let c = self.collection(next.collection_id)?;
        check_membership(&c, &next)?;
        let (f_lo, f_hi) = next.f_range();
        let t = next.t_range();
        let tx = self.write_tx()?;
        tx.execute(
            "UPDATE marker SET f_center = ?2, f_lo = ?3, f_hi = ?4, t_lo = ?5, t_hi = ?6, \
             updated_at = ?7, body = ?8 WHERE marker_id = ?1",
            params![
                blob(next.id),
                next.f_center_hz,
                f_lo,
                f_hi,
                t.map(|(a, _)| a.as_unix_nanos()),
                t.map(|(_, b)| b.as_unix_nanos()),
                next.updated_at.as_unix_nanos(),
                serde_json::to_string(&next)?
            ],
        )?;
        tx.commit()?;
        Ok(next)
    }

    /// Deletes a marker; returns what was deleted.
    pub fn delete_marker(&mut self, id: MarkerId) -> Result<Marker, RepoError> {
        let stored = self.marker(id)?;
        let tx = self.write_tx()?;
        tx.execute("DELETE FROM marker WHERE marker_id = ?1", [blob(id)])?;
        tx.commit()?;
        Ok(stored)
    }

    /// One marker.
    pub fn marker(&self, id: MarkerId) -> Result<Marker, RepoError> {
        self.ensure_collections()?;
        let body: Option<String> = self
            .conn
            .prepare_cached("SELECT body FROM marker WHERE marker_id = ?1")?
            .query_row([blob(id)], |r| r.get(0))
            .optional()?;
        match body {
            Some(b) => Ok(serde_json::from_str(&b)?),
            None => Err(RepoError::NotFound {
                kind: "marker",
                id: id.to_string(),
            }),
        }
    }

    /// A page of markers — of one collection, or of every collection when `collection` is `None`
    /// — overlapping `window`, in creation order.
    pub fn markers(
        &self,
        collection: Option<CollectionId>,
        window: MarkerWindow,
        offset: usize,
        limit: usize,
    ) -> Result<StorePage<Marker>, RepoError> {
        self.ensure_collections()?;
        let (f_lo, f_hi) = window.freq.unwrap_or((f64::NEG_INFINITY, f64::INFINITY));
        let (t0, t1) = window.time.map_or((i64::MIN, i64::MAX), |(a, b)| {
            (a.as_unix_nanos(), b.as_unix_nanos())
        });
        let filter = "(?1 IS NULL OR collection_id = ?1) AND f_hi >= ?2 AND f_lo <= ?3 \
                      AND (t_lo IS NULL OR (t_hi >= ?4 AND t_lo <= ?5))";
        let coll = collection.map(blob);
        let matched: i64 = self
            .conn
            .prepare_cached(&format!("SELECT count(*) FROM marker WHERE {filter}"))?
            .query_row(params![coll, f_lo, f_hi, t0, t1], |r| r.get(0))?;
        let bodies = self
            .conn
            .prepare_cached(&format!(
                "SELECT body FROM marker WHERE {filter} \
                 ORDER BY created_at, marker_id LIMIT ?6 OFFSET ?7"
            ))?
            .query_map(
                params![coll, f_lo, f_hi, t0, t1, limit as i64, offset as i64],
                |r| r.get::<_, String>(0),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        let items = bodies
            .iter()
            .map(|b| serde_json::from_str(b).map_err(RepoError::from))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(StorePage {
            items,
            matched: matched as u64,
        })
    }

    /// The reserved collection's markers by centre frequency then creation (the bookmark facade's
    /// order), at most `limit`.
    pub(super) fn bookmark_markers(&self, limit: usize) -> Result<Vec<Marker>, RepoError> {
        self.ensure_collections()?;
        let bodies = self
            .conn
            .prepare_cached(
                "SELECT body FROM marker WHERE collection_id = ?1 \
                 ORDER BY f_center, created_at, marker_id LIMIT ?2",
            )?
            .query_map(params![blob(BOOKMARKS_COLLECTION), limit as i64], |r| {
                r.get::<_, String>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        bodies
            .iter()
            .map(|b| serde_json::from_str(b).map_err(RepoError::from))
            .collect()
    }
}

fn insert_marker_row(conn: &rusqlite::Connection, m: &Marker, verb: &str) -> Result<(), RepoError> {
    let (f_lo, f_hi) = m.f_range();
    let t = m.t_range();
    conn.execute(
        &format!(
            "{verb} INTO marker (marker_id, collection_id, f_center, f_lo, f_hi, t_lo, t_hi, \
             created_at, updated_at, body) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"
        ),
        params![
            blob(m.id),
            blob(m.collection_id),
            m.f_center_hz,
            f_lo,
            f_hi,
            t.map(|(a, _)| a.as_unix_nanos()),
            t.map(|(_, b)| b.as_unix_nanos()),
            m.created_at.as_unix_nanos(),
            m.updated_at.as_unix_nanos(),
            serde_json::to_string(m)?
        ],
    )?;
    Ok(())
}

/// The rules a marker must satisfy for the collection it is in.
fn check_membership(c: &Collection, m: &Marker) -> Result<(), RepoError> {
    if c.reserved && m.t_center.is_some() {
        return Err(invalid(
            "the bookmarks collection holds frequency-only markers: a bookmark has no time \
             (put a timed marker in another collection)",
        ));
    }
    Ok(())
}

fn check_name(what: &str, name: &str) -> Result<(), RepoError> {
    let t = name.trim();
    if t.is_empty() || t.chars().count() > COLLECTION_NAME_MAX || t != name {
        return Err(invalid(format!(
            "{what} name must be 1..={COLLECTION_NAME_MAX} characters without surrounding \
             whitespace"
        )));
    }
    Ok(())
}

fn check_note(what: &str, note: Option<&str>) -> Result<(), RepoError> {
    if note.is_some_and(|n| n.chars().count() > COLLECTION_NOTE_MAX) {
        return Err(invalid(format!(
            "{what} note must be at most {COLLECTION_NOTE_MAX} characters"
        )));
    }
    Ok(())
}

fn is_hex_color(c: &str) -> bool {
    c.len() == 7 && c.starts_with('#') && c[1..].chars().all(|ch| ch.is_ascii_hexdigit())
}

fn invalid(m: impl Into<String>) -> RepoError {
    RepoError::Invalid(m.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::{Bookmark, BookmarkKind};

    fn at(s: i64) -> Timestamp {
        Timestamp::from_unix_nanos(s * 1_000_000_000)
    }

    #[test]
    fn a_fresh_store_has_only_the_reserved_bookmarks_collection() {
        let repo = Repository::open_in_memory().unwrap();
        let page = repo.collections(0, 500).unwrap();
        assert_eq!(page.matched, 1);
        let c = &page.items[0].collection;
        assert_eq!(c.id, BOOKMARKS_COLLECTION);
        assert!(c.reserved && c.visible);
        assert_eq!(c.name, BOOKMARKS_COLLECTION_NAME);
    }

    #[test]
    fn collections_and_time_frequency_markers_round_trip() {
        let mut repo = Repository::open_in_memory().unwrap();
        let mut c = Collection::new("902 ISM bursts");
        c.color = Some("#33aaff".into());
        repo.insert_collection(&c).unwrap();
        assert!(repo.insert_collection(&c).is_err(), "duplicate id");

        let mut pin = Marker::new(c.id, "LoRa channel", 903.9e6);
        pin.bandwidth_hz = Some(125e3);
        let mut burst = Marker::new(c.id, "one-off burst", 915.2e6);
        burst.t_center = Some(at(1_726_480_000));
        burst.duration_s = Some(0.4);
        burst.provenance.tier = Some(ViewTier::LiveIq);
        burst.provenance.t_capture = Some((at(1_726_479_990), at(1_726_480_010)));
        repo.insert_marker(&pin).unwrap();
        repo.insert_marker(&burst).unwrap();
        assert_eq!(repo.marker(burst.id).unwrap(), burst);
        assert_eq!(repo.collection_member_count(c.id).unwrap(), 2);

        // A time window far from the burst still matches the frequency-only pin (a mark for all
        // time), never the burst.
        let far = MarkerWindow {
            freq: None,
            time: Some((at(1_000), at(2_000))),
        };
        let page = repo.markers(Some(c.id), far, 0, 500).unwrap();
        assert_eq!(page.items, vec![pin.clone()]);
        // A box over the burst's time and frequency finds it; a frequency box off both finds none.
        let on = MarkerWindow {
            freq: Some((915.0e6, 915.3e6)),
            time: Some((at(1_726_480_000), at(1_726_480_001))),
        };
        assert_eq!(
            repo.markers(None, on, 0, 500).unwrap().items,
            vec![burst.clone()]
        );
        let off = MarkerWindow {
            freq: Some((100e6, 101e6)),
            time: None,
        };
        assert_eq!(repo.markers(None, off, 0, 500).unwrap().matched, 0);

        // Paging: matched counts every row, the page holds `limit`.
        let p = repo
            .markers(Some(c.id), MarkerWindow::default(), 1, 1)
            .unwrap();
        assert_eq!((p.matched, p.items.len()), (2, 1));
        assert_eq!(p.items[0].id, burst.id);

        let mut hidden = c.clone();
        hidden.visible = false;
        hidden.updated_at = Timestamp::now();
        assert!(!repo.update_collection(&hidden).unwrap().visible);

        let (deleted, n) = repo.delete_collection(c.id).unwrap();
        assert_eq!((deleted.id, n), (c.id, 2));
        assert!(matches!(
            repo.marker(pin.id),
            Err(RepoError::NotFound { .. })
        ));
    }

    #[test]
    fn invalid_markers_and_collections_are_refused() {
        let mut repo = Repository::open_in_memory().unwrap();
        let c = Collection::new("c");
        repo.insert_collection(&c).unwrap();
        let base = Marker::new(c.id, "m", 100e6);
        for bad in [
            Marker {
                duration_s: Some(1.0),
                ..base.clone()
            },
            Marker {
                t_center: Some(at(1)),
                duration_s: Some(-1.0),
                ..base.clone()
            },
            Marker {
                bandwidth_hz: Some(300e6),
                ..base.clone()
            },
            Marker {
                name: " x ".into(),
                ..base.clone()
            },
            Marker {
                f_center_hz: f64::NAN,
                ..base.clone()
            },
        ] {
            assert!(
                matches!(repo.insert_marker(&bad), Err(RepoError::Invalid(_))),
                "{bad:?}"
            );
        }
        let orphan = Marker::new(CollectionId::new(), "o", 100e6);
        assert!(matches!(
            repo.insert_marker(&orphan),
            Err(RepoError::NotFound { .. })
        ));
        let mut bad_color = Collection::new("x");
        bad_color.color = Some("red".into());
        assert!(repo.insert_collection(&bad_color).is_err());
        // The reserved collection is un-deletable and holds no timed marker.
        assert!(repo.delete_collection(BOOKMARKS_COLLECTION).is_err());
        let timed = Marker {
            t_center: Some(at(5)),
            ..Marker::new(BOOKMARKS_COLLECTION, "t", 100e6)
        };
        assert!(matches!(
            repo.insert_marker(&timed),
            Err(RepoError::Invalid(_))
        ));
    }

    /// docs/25 §10.6: a bookmark and a frequency-only marker of the reserved collection are the
    /// same row, whichever route wrote it.
    #[test]
    fn bookmarks_are_the_reserved_collections_markers() {
        let mut repo = Repository::open_in_memory().unwrap();
        let b = Bookmark::new(BookmarkKind::Marker, "pager?", 930.5e6);
        repo.insert_bookmark(&b).unwrap();
        let as_marker = repo.marker(MarkerId::from_uuid(*b.id.as_uuid())).unwrap();
        assert_eq!(as_marker.collection_id, BOOKMARKS_COLLECTION);
        assert_eq!(as_marker.name, "pager?");
        assert!(as_marker.t_center.is_none());

        let m = Marker::new(BOOKMARKS_COLLECTION, "FM", 101.3e6);
        repo.insert_marker(&m).unwrap();
        let all = repo.bookmarks().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].name, "FM");
        assert_eq!(all[0].kind, BookmarkKind::Bookmark);
        assert_eq!(repo.collections(0, 10).unwrap().items[0].member_count, 2);
    }

    /// A database written before T-817 keeps its bookmarks: legacy rows move into the reserved
    /// collection with their id, kind, name, note and timestamps.
    #[test]
    fn legacy_bookmark_rows_migrate_into_the_reserved_collection() {
        let repo = Repository::open_in_memory().unwrap();
        let mut b = Bookmark::new(BookmarkKind::Marker, "old", 145.8e6);
        b.note = Some("from before collections".into());
        b.bandwidth_hz = Some(25e3);
        b.created_at = at(1_700_000_000);
        b.updated_at = at(1_700_000_100);
        repo.conn
            .execute(
                "INSERT INTO bookmark (bookmark_id, kind, name, f_center, created_at, updated_at, body) \
                 VALUES (?1, 'marker', ?2, ?3, ?4, ?5, ?6)",
                params![
                    blob(b.id),
                    b.name,
                    b.f_center_hz,
                    b.created_at.as_unix_nanos(),
                    b.updated_at.as_unix_nanos(),
                    serde_json::to_string(&b).unwrap()
                ],
            )
            .unwrap();
        assert_eq!(repo.bookmarks().unwrap(), vec![b.clone()]);
        let n: i64 = repo
            .conn
            .query_row("SELECT count(*) FROM bookmark", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "moved, not copied: one row, one store");
        // Idempotent.
        assert_eq!(repo.bookmarks().unwrap(), vec![b]);
    }
}
