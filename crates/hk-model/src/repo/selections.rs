//! Region selections (T-052, docs/07 §2.20): named frequency extents, optionally bounded in time,
//! that the user keeps and acts on.
//!
//! - **User metadata only.** A selection holds an extent, a name, notes, tags and links to the
//!   actions taken on it (a Recording, a Demodulation, an Emitter, by id); it never carries
//!   signal content.
//! - **Mutable.** Selections may be updated and deleted; `updated_at` records the last change and
//!   `created_at` never moves.
//! - **Links are an append-only ring.** [`Repository::add_selection_link`] appends; past
//!   [`SELECTION_LINKS_MAX`] the oldest link is dropped.
//! - **Validated here** ([`Selection::validate`]) so every writer gets the same limits: name
//!   1–[`SELECTION_NAME_MAX`] characters without surrounding whitespace; `0 <= f_lo < f_hi`, both
//!   finite; `t_lo`/`t_hi` both or neither with `t_lo <= t_hi`; notes at most
//!   [`SELECTION_NOTES_MAX`] characters; at most [`SELECTION_TAGS_MAX`] distinct tags of
//!   1–[`SELECTION_TAG_MAX`] characters; link targets 1–[`SELECTION_LINK_REF_MAX`] characters.
//! - Databases created before T-052 (same pre-release schema version) get the table on first use.

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::{RepoError, Repository, blob};
use crate::ids::SelectionId;
use crate::time::Timestamp;

/// Longest selection name, characters.
pub const SELECTION_NAME_MAX: usize = 120;
/// Longest notes, characters.
pub const SELECTION_NOTES_MAX: usize = 2000;
/// Most tags on one selection.
pub const SELECTION_TAGS_MAX: usize = 32;
/// Longest tag, characters.
pub const SELECTION_TAG_MAX: usize = 64;
/// Most links kept on one selection (the oldest are dropped first).
pub const SELECTION_LINKS_MAX: usize = 256;
/// Longest link target reference or link note, characters.
pub const SELECTION_LINK_REF_MAX: usize = 128;
/// Most selections [`Repository::selections`] returns.
pub const SELECTIONS_MAX: usize = 10_000;

/// A selection's region watch (T-166, ADR-0013 §4.9 gap 9): "alert on new activity" over this
/// selection's extent.
///
/// **Armed or not, and nothing else.** There is deliberately no user threshold here. What counts
/// as activity is *measured* — a first sighting in the inventory, filtered by the T-219
/// relationship rules — not a level the user dials in, the same stance the rest of the system
/// takes towards estimated parameters. Turning the watch off (`enabled: false`, or clearing it
/// with `null`) is the whole control surface, and it never deletes an alert already raised.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionWatch {
    /// Armed. Disarming stops new alerts; past alerts keep their rows, reasoning and history.
    pub enabled: bool,
}

const ENSURE_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS selection (
    selection_id BLOB    PRIMARY KEY CHECK (length(selection_id) = 16),
    name         TEXT    NOT NULL CHECK (length(name) BETWEEN 1 AND 120),
    f_lo         REAL    NOT NULL CHECK (f_lo >= 0),
    f_hi         REAL    NOT NULL CHECK (f_hi > f_lo),
    t_lo         INTEGER,
    t_hi         INTEGER,
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    body         TEXT    NOT NULL,
    CHECK ((t_lo IS NULL) = (t_hi IS NULL) AND (t_lo IS NULL OR t_hi >= t_lo))
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS idx_selection_created ON selection (created_at);";

/// What kind of action a [`SelectionLink`] records.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SelectionLinkKind {
    /// A demodulation session (Listen, T-043; demod outputs, T-061). `target`: its id or stream.
    Demodulation,
    /// A recording (manual IQ recording, T-050; per-selection outputs, T-061). `target`: its id.
    Recording,
    /// A stored bitstream (T-061). `target`: its id.
    Bitstream,
    /// An inspection (region history plus explanations). `target`: the top emitter id, or a
    /// short description when none was inside.
    Inspection,
}

/// One action taken on a selection.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionLink {
    /// What was done.
    pub kind: SelectionLinkKind,
    /// The object it produced or looked at (an id or short reference).
    pub target: String,
    /// When.
    pub t: Timestamp,
    /// Optional short note (e.g. "listen", the chosen mode).
    pub note: Option<String>,
}

/// A named region.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Selection {
    /// Id.
    pub id: SelectionId,
    /// Name (1–[`SELECTION_NAME_MAX`] characters, no surrounding whitespace).
    pub name: String,
    /// Lower frequency edge, Hz.
    pub f_lo_hz: f64,
    /// Upper frequency edge, Hz.
    pub f_hi_hz: f64,
    /// Start of the time extent; `None` for any time.
    pub t_lo: Option<Timestamp>,
    /// End of the time extent; set exactly when `t_lo` is.
    pub t_hi: Option<Timestamp>,
    /// Free-text notes.
    pub notes: Option<String>,
    /// User tags (distinct).
    pub tags: Vec<String>,
    /// Actions taken on it, oldest first.
    pub links: Vec<SelectionLink>,
    /// T-166: the region watch, when the user armed one; `None` = not watched. Absent in rows
    /// written before T-166, which read back unwatched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watch: Option<SelectionWatch>,
    /// When it was created.
    pub created_at: Timestamp,
    /// When it last changed.
    pub updated_at: Timestamp,
}

fn invalid(msg: impl Into<String>) -> RepoError {
    RepoError::Invalid(msg.into())
}

impl Selection {
    /// A new selection named `name` over `[f_lo_hz, f_hi_hz]`, created now.
    pub fn new(name: impl Into<String>, f_lo_hz: f64, f_hi_hz: f64) -> Self {
        let now = Timestamp::now();
        Self {
            id: SelectionId::new(),
            name: name.into(),
            f_lo_hz,
            f_hi_hz,
            t_lo: None,
            t_hi: None,
            notes: None,
            tags: Vec::new(),
            links: Vec::new(),
            watch: None,
            created_at: now,
            updated_at: now,
        }
    }

    /// Whether a region watch is armed on this selection (T-166).
    pub fn watching(&self) -> bool {
        self.watch.is_some_and(|w| w.enabled)
    }

    /// Checks the limits in the module docs.
    pub fn validate(&self) -> Result<(), RepoError> {
        let name = self.name.trim();
        if name.is_empty() || name.chars().count() > SELECTION_NAME_MAX || name != self.name {
            return Err(invalid(format!(
                "selection name must be 1..={SELECTION_NAME_MAX} characters without surrounding \
                 whitespace"
            )));
        }
        if !(self.f_lo_hz.is_finite() && self.f_hi_hz.is_finite()) {
            return Err(invalid("selection f_lo and f_hi must be finite"));
        }
        if !(self.f_lo_hz >= 0.0 && self.f_hi_hz > self.f_lo_hz) {
            return Err(invalid(format!(
                "selection needs 0 <= f_lo < f_hi, got f_lo {} f_hi {}",
                self.f_lo_hz, self.f_hi_hz
            )));
        }
        match (self.t_lo, self.t_hi) {
            (None, None) => {}
            (Some(lo), Some(hi)) if hi >= lo => {}
            (Some(_), Some(_)) => return Err(invalid("selection needs t_lo <= t_hi")),
            _ => return Err(invalid("selection needs both t_lo and t_hi, or neither")),
        }
        if self
            .notes
            .as_ref()
            .is_some_and(|n| n.chars().count() > SELECTION_NOTES_MAX)
        {
            return Err(invalid(format!(
                "selection notes must be at most {SELECTION_NOTES_MAX} characters"
            )));
        }
        if self.tags.len() > SELECTION_TAGS_MAX {
            return Err(invalid(format!(
                "a selection has at most {SELECTION_TAGS_MAX} tags"
            )));
        }
        for (i, tag) in self.tags.iter().enumerate() {
            if tag.trim() != tag || tag.is_empty() || tag.chars().count() > SELECTION_TAG_MAX {
                return Err(invalid(format!(
                    "selection tags must be 1..={SELECTION_TAG_MAX} characters without \
                     surrounding whitespace"
                )));
            }
            if self.tags[..i].contains(tag) {
                return Err(invalid(format!("duplicate selection tag {tag:?}")));
            }
        }
        if self.links.len() > SELECTION_LINKS_MAX {
            return Err(invalid(format!(
                "a selection keeps at most {SELECTION_LINKS_MAX} links"
            )));
        }
        for link in &self.links {
            link.validate()?;
        }
        if self.updated_at < self.created_at {
            return Err(invalid("selection updated_at is before created_at"));
        }
        Ok(())
    }
}

impl SelectionLink {
    /// A link made now.
    pub fn new(kind: SelectionLinkKind, target: impl Into<String>) -> Self {
        Self {
            kind,
            target: target.into(),
            t: Timestamp::now(),
            note: None,
        }
    }

    /// Target and note limits (module docs).
    pub fn validate(&self) -> Result<(), RepoError> {
        let t = self.target.trim();
        if t.is_empty() || t != self.target || t.chars().count() > SELECTION_LINK_REF_MAX {
            return Err(invalid(format!(
                "selection link target must be 1..={SELECTION_LINK_REF_MAX} characters without \
                 surrounding whitespace"
            )));
        }
        if self
            .note
            .as_ref()
            .is_some_and(|n| n.chars().count() > SELECTION_LINK_REF_MAX)
        {
            return Err(invalid(format!(
                "selection link note must be at most {SELECTION_LINK_REF_MAX} characters"
            )));
        }
        Ok(())
    }
}

impl Repository {
    fn ensure_selection_table(&self) -> Result<(), RepoError> {
        self.conn.execute_batch(ENSURE_TABLE)?;
        Ok(())
    }

    /// Stores a new selection (validated). Its id must be new ([`RepoError::Engine`] otherwise;
    /// check with [`Repository::selection`] first to tell a duplicate apart).
    pub fn insert_selection(&mut self, selection: &Selection) -> Result<(), RepoError> {
        selection.validate()?;
        self.ensure_selection_table()?;
        let s = selection;
        let tx = self.write_tx()?;
        tx.execute(
            "INSERT INTO selection (selection_id, name, f_lo, f_hi, t_lo, t_hi, created_at, \
             updated_at, body) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                blob(s.id),
                s.name,
                s.f_lo_hz,
                s.f_hi_hz,
                s.t_lo.map(Timestamp::as_unix_nanos),
                s.t_hi.map(Timestamp::as_unix_nanos),
                s.created_at.as_unix_nanos(),
                s.updated_at.as_unix_nanos(),
                serde_json::to_string(s)?
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Replaces a stored selection (validated). `created_at` is kept from the stored row;
    /// `updated_at` never goes below it.
    pub fn update_selection(&mut self, selection: &Selection) -> Result<Selection, RepoError> {
        self.ensure_selection_table()?;
        let stored = self.selection(selection.id)?;
        let mut next = selection.clone();
        next.created_at = stored.created_at;
        if next.updated_at < next.created_at {
            next.updated_at = next.created_at;
        }
        next.validate()?;
        let tx = self.write_tx()?;
        let n = tx.execute(
            "UPDATE selection SET name = ?2, f_lo = ?3, f_hi = ?4, t_lo = ?5, t_hi = ?6, \
             updated_at = ?7, body = ?8 WHERE selection_id = ?1",
            params![
                blob(next.id),
                next.name,
                next.f_lo_hz,
                next.f_hi_hz,
                next.t_lo.map(Timestamp::as_unix_nanos),
                next.t_hi.map(Timestamp::as_unix_nanos),
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

    /// Appends a link (validated); drops the oldest past [`SELECTION_LINKS_MAX`]. Returns the
    /// updated selection.
    pub fn add_selection_link(
        &mut self,
        id: SelectionId,
        link: SelectionLink,
    ) -> Result<Selection, RepoError> {
        link.validate()?;
        let mut next = self.selection(id)?;
        next.links.push(link);
        let excess = next.links.len().saturating_sub(SELECTION_LINKS_MAX);
        next.links.drain(..excess);
        next.updated_at = Timestamp::now();
        self.update_selection(&next)
    }

    /// Deletes a selection; returns what was deleted.
    pub fn delete_selection(&mut self, id: SelectionId) -> Result<Selection, RepoError> {
        self.ensure_selection_table()?;
        let stored = self.selection(id)?;
        let tx = self.write_tx()?;
        tx.execute("DELETE FROM selection WHERE selection_id = ?1", [blob(id)])?;
        tx.commit()?;
        Ok(stored)
    }

    /// One selection.
    pub fn selection(&self, id: SelectionId) -> Result<Selection, RepoError> {
        self.ensure_selection_table()?;
        let body: Option<String> = self
            .conn
            .prepare_cached("SELECT body FROM selection WHERE selection_id = ?1")?
            .query_row([blob(id)], |r| r.get(0))
            .optional()?;
        match body {
            Some(b) => Ok(serde_json::from_str(&b)?),
            None => Err(not_found(id)),
        }
    }

    /// Every selection in creation order (at most [`SELECTIONS_MAX`]).
    pub fn selections(&self) -> Result<Vec<Selection>, RepoError> {
        self.ensure_selection_table()?;
        let mut stmt = self.conn.prepare_cached(
            "SELECT body FROM selection ORDER BY created_at, selection_id LIMIT ?1",
        )?;
        let texts = stmt
            .query_map([SELECTIONS_MAX as i64], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        texts
            .iter()
            .map(|t| serde_json::from_str(t).map_err(RepoError::from))
            .collect()
    }
}

fn not_found(id: SelectionId) -> RepoError {
    RepoError::NotFound {
        kind: "selection",
        id: id.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "hk-model-sel-{tag}-{}-{}.db",
            std::process::id(),
            Timestamp::now().as_unix_nanos()
        ))
    }

    #[test]
    fn several_selections_round_trip_update_link_and_delete() {
        let mut repo = Repository::open_in_memory().unwrap();
        assert!(repo.selections().unwrap().is_empty());
        let mut a = Selection::new("FM 101.3", 101.2e6, 101.4e6);
        a.notes = Some("strong local station".into());
        a.tags = vec!["broadcast".into(), "fm".into()];
        let mut b = Selection::new("pager burst", 930.4e6, 930.6e6);
        b.t_lo = Some(Timestamp::from_unix_nanos(1_000_000_000));
        b.t_hi = Some(Timestamp::from_unix_nanos(3_500_000_000));
        b.created_at = a.created_at.saturating_add_nanos(1);
        b.updated_at = b.created_at;
        repo.insert_selection(&b).unwrap();
        repo.insert_selection(&a).unwrap();
        assert_eq!(
            repo.selections().unwrap(),
            vec![a.clone(), b.clone()],
            "creation order"
        );
        assert!(repo.insert_selection(&a).is_err(), "duplicate id");

        let mut renamed = a.clone();
        renamed.name = "FM 101.3 (RDS)".into();
        renamed.created_at = Timestamp::from_unix_nanos(1);
        renamed.updated_at = Timestamp::now();
        let stored = repo.update_selection(&renamed).unwrap();
        assert_eq!(stored.created_at, a.created_at, "created_at never moves");
        assert_eq!(repo.selection(a.id).unwrap().name, "FM 101.3 (RDS)");

        let mut link = SelectionLink::new(SelectionLinkKind::Demodulation, "listen:fm");
        link.note = Some("listen".into());
        let linked = repo.add_selection_link(a.id, link.clone()).unwrap();
        assert_eq!(linked.links, vec![link]);
        assert_eq!(repo.selection(a.id).unwrap().links.len(), 1);

        assert_eq!(repo.delete_selection(b.id).unwrap().name, "pager burst");
        assert!(matches!(
            repo.selection(b.id),
            Err(RepoError::NotFound { .. })
        ));
        assert!(repo.delete_selection(b.id).is_err());
        assert!(repo.update_selection(&b).is_err());
        assert!(
            repo.add_selection_link(b.id, SelectionLink::new(SelectionLinkKind::Recording, "r"))
                .is_err()
        );
    }

    #[test]
    fn links_are_a_bounded_ring() {
        let mut repo = Repository::open_in_memory().unwrap();
        let s = Selection::new("ring", 1.0, 2.0);
        repo.insert_selection(&s).unwrap();
        for i in 0..SELECTION_LINKS_MAX + 3 {
            repo.add_selection_link(
                s.id,
                SelectionLink::new(SelectionLinkKind::Inspection, format!("i{i}")),
            )
            .unwrap();
        }
        let links = repo.selection(s.id).unwrap().links;
        assert_eq!(links.len(), SELECTION_LINKS_MAX);
        assert_eq!(links[0].target, "i3", "oldest dropped first");
    }

    #[test]
    fn invalid_selections_and_links_are_refused() {
        let mut repo = Repository::open_in_memory().unwrap();
        let t = |n| Some(Timestamp::from_unix_nanos(n));
        let base = || Selection::new("x", 1e6, 2e6);
        for bad in [
            Selection::new("", 1e6, 2e6),
            Selection::new(" padded ", 1e6, 2e6),
            Selection::new("x".repeat(121), 1e6, 2e6),
            Selection::new("eq", 2e6, 2e6),
            Selection::new("neg", -1.0, 2e6),
            Selection::new("nan", f64::NAN, 2e6),
            Selection::new("inf", 1.0, f64::INFINITY),
            Selection {
                t_lo: t(5),
                ..base()
            },
            Selection {
                t_lo: t(5),
                t_hi: t(4),
                ..base()
            },
            Selection {
                notes: Some("n".repeat(2001)),
                ..base()
            },
            Selection {
                tags: vec!["a".into(), "a".into()],
                ..base()
            },
            Selection {
                tags: vec![" a".into()],
                ..base()
            },
            Selection {
                tags: (0..33).map(|i| format!("t{i}")).collect(),
                ..base()
            },
            Selection {
                links: vec![SelectionLink::new(SelectionLinkKind::Recording, "")],
                ..base()
            },
        ] {
            assert!(
                matches!(repo.insert_selection(&bad), Err(RepoError::Invalid(_))),
                "{bad:?}"
            );
        }
        assert!(repo.selections().unwrap().is_empty());
        let ok = Selection {
            t_lo: t(5),
            t_hi: t(5),
            ..base()
        };
        repo.insert_selection(&ok).unwrap();
        assert!(matches!(
            repo.add_selection_link(
                ok.id,
                SelectionLink::new(SelectionLinkKind::Recording, "r".repeat(129))
            ),
            Err(RepoError::Invalid(_))
        ));
    }

    #[test]
    fn selections_survive_reopening_the_database() {
        let path = temp_db("reopen");
        let s = Selection::new("kept", 433.0e6, 434.8e6);
        {
            let mut repo = Repository::open(&path).unwrap();
            repo.insert_selection(&s).unwrap();
        }
        let repo = Repository::open(&path).unwrap();
        assert_eq!(repo.selections().unwrap(), vec![s]);
        drop(repo);
        for ext in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{ext}", path.display()));
        }
    }

    #[test]
    fn a_database_without_the_table_gets_it_on_first_use() {
        let mut repo = Repository::open_in_memory().unwrap();
        repo.conn.execute_batch("DROP TABLE selection").unwrap();
        let s = Selection::new("late", 1090e6, 1091e6);
        repo.insert_selection(&s).unwrap();
        assert_eq!(repo.selections().unwrap(), vec![s]);
    }
}
