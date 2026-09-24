//! Saved views (T-819 / MAP-19, docs/25 §6 and §10, ADR-0023 §5): a **named, restorable (time ×
//! frequency) window extent** — the ArcGIS spatial-bookmark analogue on the canvas plane.
//!
//! - **A saved view is a named point in view-arithmetic state**, nothing more: the frequency
//!   extent `(center_f_hz, span_f_hz)`, the time extent — either `follow_live` (pinned to the
//!   growing edge, optionally with a `span_t_s` depth) or a frozen `(center_t, span_t_s)` window on
//!   the capture clock — and an optional client-owned `pane_layout`. It is **not a marker** (it has
//!   no frequency *centre* in a marker's sense, docs/25 §10.6) and **not a device command**:
//!   restoring one is client view arithmetic, and only a frequency extent outside the tuned window
//!   raises the ordinary gated retune offer, with no exemption (ADR-0023 §5). Nothing here reaches
//!   a front end.
//! - **Provenance is the shared docs/25 §2 stamp** ([`MeasurementProvenance`]), written by the
//!   server; the capture clock (`center_t`, `t_capture`) and the wall clock (`authored_at`,
//!   `created_at`, `updated_at`) are stored apart and never compared.
//! - **User metadata, never detection input** (docs/25 §10.7), in the run's user-metadata database
//!   beside bookmarks, selections and measurements.
//! - **Paged** ([`SAVED_VIEW_PAGE_MAX`]) with an optional window filter; "durable" is not
//!   "unbounded" (docs/25 §10.3).
//! - The table is ensured on first use (the selections pattern), so no schema migration is needed
//!   and the MAP-16..19 stores land in any order.

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::measurements::MeasurementProvenance;
use super::{RepoError, Repository, blob};
use crate::ids::SavedViewId;
use crate::time::Timestamp;

/// Longest name, characters.
pub const SAVED_VIEW_NAME_MAX: usize = 200;
/// Longest note, characters.
pub const SAVED_VIEW_NOTE_MAX: usize = 4000;
/// Largest serialised `pane_layout`, bytes. The layout is client presentation state (the N-pane
/// arrangement), stored opaquely but bounded.
pub const SAVED_VIEW_LAYOUT_MAX: usize = 16 * 1024;
/// Most rows one [`Repository::saved_views_in`] page returns.
pub const SAVED_VIEW_PAGE_MAX: usize = 2000;

const ENSURE_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS saved_view (
    view_id    BLOB    PRIMARY KEY CHECK (length(view_id) = 16),
    f_lo       REAL    NOT NULL,
    f_hi       REAL    NOT NULL CHECK (f_hi >= f_lo),
    t0         INTEGER,
    t1         INTEGER CHECK (t1 >= t0),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    body       TEXT    NOT NULL
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS idx_saved_view_created ON saved_view (created_at);";

fn invalid(msg: impl Into<String>) -> RepoError {
    RepoError::Invalid(msg.into())
}

/// A named, restorable (time × frequency) window extent (docs/25 §6).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedView {
    /// Id.
    pub id: SavedViewId,
    /// Name, 1..=[`SAVED_VIEW_NAME_MAX`] characters without surrounding whitespace.
    pub name: String,
    /// Optional note.
    pub note: Option<String>,
    /// Centre of the frequency extent, Hz.
    pub center_f_hz: f64,
    /// Width of the frequency extent, Hz (positive).
    pub span_f_hz: f64,
    /// Capture-clock centre of a frozen time window; `None` when [`Self::follow_live`].
    #[serde(rename = "center_t_ns")]
    pub center_t: Option<Timestamp>,
    /// Time depth shown, seconds: required for a frozen window, optional when following live.
    pub span_t_s: Option<f64>,
    /// Pinned to the growing live edge rather than frozen on a window.
    pub follow_live: bool,
    /// The N-pane arrangement to restore, as the client serialised it (object or array).
    pub pane_layout: Option<Value>,
    /// The docs/25 §2 stamp, written by the server.
    pub provenance: MeasurementProvenance,
    /// Wall-clock creation.
    #[serde(rename = "created_at_ns")]
    pub created_at: Timestamp,
    /// Wall-clock last change.
    #[serde(rename = "updated_at_ns")]
    pub updated_at: Timestamp,
}

impl SavedView {
    /// The frequency extent `[lo, hi]`, Hz.
    pub fn f_range(&self) -> (f64, f64) {
        (
            self.center_f_hz - self.span_f_hz / 2.0,
            self.center_f_hz + self.span_f_hz / 2.0,
        )
    }

    /// The frozen capture-clock window `[t0, t1]`, or `None` for a follow-live view.
    pub fn t_range(&self) -> Option<(Timestamp, Timestamp)> {
        let c = self.center_t?;
        let half = (self.span_t_s? * 0.5e9).round() as i64;
        let c = c.as_unix_nanos();
        Some((
            Timestamp::from_unix_nanos(c.saturating_sub(half)),
            Timestamp::from_unix_nanos(c.saturating_add(half)),
        ))
    }

    /// Checks every limit and the time-extent rule: a frozen view carries both `center_t` and
    /// `span_t_s`; a follow-live view carries no `center_t` (its time is the live edge).
    pub fn validate(&self) -> Result<(), RepoError> {
        let n = self.name.chars().count();
        if self.name.trim() != self.name || n == 0 || n > SAVED_VIEW_NAME_MAX {
            return Err(invalid(format!(
                "view name must be 1..={SAVED_VIEW_NAME_MAX} characters without surrounding \
                 whitespace"
            )));
        }
        if self
            .note
            .as_ref()
            .is_some_and(|b| b.chars().count() > SAVED_VIEW_NOTE_MAX)
        {
            return Err(invalid(format!(
                "view note must be at most {SAVED_VIEW_NOTE_MAX} characters"
            )));
        }
        if !(self.span_f_hz.is_finite() && self.span_f_hz > 0.0) {
            return Err(invalid("span_f_hz must be a finite, positive Hz"));
        }
        if !(self.center_f_hz.is_finite() && self.f_range().0 >= 0.0) {
            return Err(invalid(
                "center_f_hz must be finite, with the extent center_f_hz ± span_f_hz/2 at or \
                 above 0 Hz",
            ));
        }
        if self
            .span_t_s
            .is_some_and(|s| !(s.is_finite() && s > 0.0 && s < 1e9))
        {
            return Err(invalid(
                "span_t_s must be a finite, positive number of seconds",
            ));
        }
        if self.follow_live {
            if self.center_t.is_some() {
                return Err(invalid(
                    "a follow_live view has no center_t_s: its time position is the live edge",
                ));
            }
        } else if self.center_t.is_none() || self.span_t_s.is_none() {
            return Err(invalid(
                "a frozen view (follow_live: false) needs center_t_s and span_t_s: the window it \
                 is frozen on",
            ));
        }
        if let Some(l) = &self.pane_layout {
            if !(l.is_object() || l.is_array()) {
                return Err(invalid("pane_layout must be a JSON object or array"));
            }
            if serde_json::to_string(l)?.len() > SAVED_VIEW_LAYOUT_MAX {
                return Err(invalid(format!(
                    "pane_layout must serialise to at most {SAVED_VIEW_LAYOUT_MAX} bytes"
                )));
            }
        }
        self.provenance.validate()?;
        if self.updated_at < self.created_at {
            return Err(invalid("view updated_at is before created_at"));
        }
        Ok(())
    }
}

/// The optional filter of [`Repository::saved_views_in`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SavedViewFilter {
    /// Only views whose frequency extent intersects `[f_lo, f_hi]` and — for a frozen view —
    /// whose time window intersects `[t0, t1]` (closed on both axes). A follow-live view has no
    /// fixed time window and matches on frequency alone.
    pub window: Option<(f64, f64, Timestamp, Timestamp)>,
}

/// A page of [`Repository::saved_views_in`].
#[derive(Clone, Debug, PartialEq)]
pub struct SavedViewPage {
    /// The rows, newest created first.
    pub rows: Vec<SavedView>,
    /// How many rows the whole filter matches.
    pub matched: u64,
}

impl Repository {
    fn ensure_saved_view_table(&self) -> Result<(), RepoError> {
        self.conn.execute_batch(ENSURE_TABLE)?;
        Ok(())
    }

    fn view_columns(v: &SavedView) -> (f64, f64, Option<i64>, Option<i64>) {
        let (lo, hi) = v.f_range();
        let t = v.t_range();
        (
            lo,
            hi,
            t.map(|t| t.0.as_unix_nanos()),
            t.map(|t| t.1.as_unix_nanos()),
        )
    }

    /// Stores a new view (validated). Its id must be new ([`RepoError::Engine`] otherwise; check
    /// with [`Repository::saved_view`] first to tell a duplicate apart).
    pub fn insert_saved_view(&mut self, v: &SavedView) -> Result<(), RepoError> {
        v.validate()?;
        self.ensure_saved_view_table()?;
        let (lo, hi, t0, t1) = Self::view_columns(v);
        let tx = self.write_tx()?;
        tx.execute(
            "INSERT INTO saved_view (view_id, f_lo, f_hi, t0, t1, created_at, updated_at, body) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                blob(v.id),
                lo,
                hi,
                t0,
                t1,
                v.created_at.as_unix_nanos(),
                v.updated_at.as_unix_nanos(),
                serde_json::to_string(v)?
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Replaces a stored view (validated). `created_at` is kept from the stored row; `updated_at`
    /// never goes below it.
    pub fn update_saved_view(&mut self, v: &SavedView) -> Result<SavedView, RepoError> {
        let stored = self.saved_view(v.id)?;
        let mut next = v.clone();
        next.created_at = stored.created_at;
        if next.updated_at < next.created_at {
            next.updated_at = next.created_at;
        }
        next.validate()?;
        let (lo, hi, t0, t1) = Self::view_columns(&next);
        let tx = self.write_tx()?;
        tx.execute(
            "UPDATE saved_view SET f_lo = ?2, f_hi = ?3, t0 = ?4, t1 = ?5, updated_at = ?6, \
             body = ?7 WHERE view_id = ?1",
            params![
                blob(next.id),
                lo,
                hi,
                t0,
                t1,
                next.updated_at.as_unix_nanos(),
                serde_json::to_string(&next)?
            ],
        )?;
        tx.commit()?;
        Ok(next)
    }

    /// Deletes a view; returns what was deleted.
    pub fn delete_saved_view(&mut self, id: SavedViewId) -> Result<SavedView, RepoError> {
        let stored = self.saved_view(id)?;
        let tx = self.write_tx()?;
        tx.execute("DELETE FROM saved_view WHERE view_id = ?1", [blob(id)])?;
        tx.commit()?;
        Ok(stored)
    }

    /// One view.
    pub fn saved_view(&self, id: SavedViewId) -> Result<SavedView, RepoError> {
        self.ensure_saved_view_table()?;
        let body: Option<String> = self
            .conn
            .prepare_cached("SELECT body FROM saved_view WHERE view_id = ?1")?
            .query_row([blob(id)], |r| r.get(0))
            .optional()?;
        match body {
            Some(b) => Ok(serde_json::from_str(&b)?),
            None => Err(RepoError::NotFound {
                kind: "saved view",
                id: id.to_string(),
            }),
        }
    }

    /// The views matching `filter`, newest created first (`created_at`, then id — a total order,
    /// so paging is stable), skipping `offset` and returning at most `limit` (capped at
    /// [`SAVED_VIEW_PAGE_MAX`]), plus how many the whole filter matches.
    pub fn saved_views_in(
        &self,
        filter: &SavedViewFilter,
        offset: usize,
        limit: usize,
    ) -> Result<SavedViewPage, RepoError> {
        self.ensure_saved_view_table()?;
        const WHERE: &str = "(?1 IS NULL OR (f_lo <= ?2 AND f_hi >= ?1 AND \
             (t0 IS NULL OR (t0 <= ?4 AND t1 >= ?3))))";
        let (lo, hi, a, b) = match filter.window {
            Some((lo, hi, a, b)) => (
                Some(lo),
                Some(hi),
                Some(a.as_unix_nanos()),
                Some(b.as_unix_nanos()),
            ),
            None => (None, None, None, None),
        };
        let matched: i64 = self
            .conn
            .prepare_cached(&format!("SELECT COUNT(*) FROM saved_view WHERE {WHERE}"))?
            .query_row(params![lo, hi, a, b], |r| r.get(0))?;
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT body FROM saved_view WHERE {WHERE} \
             ORDER BY created_at DESC, view_id LIMIT ?5 OFFSET ?6"
        ))?;
        let texts = stmt
            .query_map(
                params![
                    lo,
                    hi,
                    a,
                    b,
                    limit.min(SAVED_VIEW_PAGE_MAX) as i64,
                    offset as i64
                ],
                |r| r.get::<_, String>(0),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        let rows = texts
            .iter()
            .map(|t| serde_json::from_str(t).map_err(RepoError::from))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(SavedViewPage {
            rows,
            matched: matched.max(0) as u64,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MeasurementTier;
    use serde_json::json;

    fn ts(s: f64) -> Timestamp {
        Timestamp::from_unix_nanos((s * 1e9).round() as i64)
    }

    fn prov() -> MeasurementProvenance {
        MeasurementProvenance {
            device_id: None,
            center_hz: 100e6,
            span_hz: 20e6,
            sample_rate_hz: None,
            t_capture: [ts(990.0), ts(1010.0)],
            tier: MeasurementTier::SpectrumHistory,
            authored_at: Timestamp::now(),
            actor: Some("tok-abc".into()),
            authored: true,
        }
    }

    fn view(name: &str, center_f: f64, span_f: f64, t: Option<(f64, f64)>) -> SavedView {
        let now = Timestamp::now();
        SavedView {
            id: SavedViewId::new(),
            name: name.into(),
            note: None,
            center_f_hz: center_f,
            span_f_hz: span_f,
            center_t: t.map(|t| ts(t.0)),
            span_t_s: t.map(|t| t.1),
            follow_live: t.is_none(),
            pane_layout: None,
            provenance: prov(),
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn the_time_extent_rule_and_limits_are_enforced() {
        assert!(view("fm", 100e6, 20e6, None).validate().is_ok());
        assert!(
            view("burst", 433.92e6, 2e6, Some((1000.0, 10.0)))
                .validate()
                .is_ok()
        );
        let mut v = view("frozen-no-t", 100e6, 1e6, None);
        v.follow_live = false;
        assert!(v.validate().is_err(), "a frozen view needs its window");
        let mut v = view("live-with-t", 100e6, 1e6, Some((1000.0, 10.0)));
        v.follow_live = true;
        assert!(
            v.validate().is_err(),
            "a live view has no fixed centre time"
        );
        let mut v = view("live-depth", 100e6, 1e6, None);
        v.span_t_s = Some(30.0);
        assert!(v.validate().is_ok(), "a live view may keep its depth");
        assert!(view(" padded", 100e6, 1e6, None).validate().is_err());
        assert!(view("", 100e6, 1e6, None).validate().is_err());
        assert!(view("zero", 100e6, 0.0, None).validate().is_err());
        assert!(view("below-dc", 1e6, 4e6, None).validate().is_err());
        let mut v = view("layout", 100e6, 1e6, None);
        v.pane_layout = Some(json!("a string"));
        assert!(v.validate().is_err());
        v.pane_layout = Some(json!({ "panes": ["x".repeat(SAVED_VIEW_LAYOUT_MAX)] }));
        assert!(v.validate().is_err());
        v.pane_layout = Some(json!({ "panes": [{ "center_f_hz": 1e8 }] }));
        assert!(v.validate().is_ok());
        let mut v = view("unstamped", 100e6, 1e6, None);
        v.provenance.authored = false;
        assert!(v.validate().is_err());
    }

    #[test]
    fn round_trip_update_delete_and_windowed_paging() {
        let mut repo = Repository::open_in_memory().unwrap();
        let fm = view("fm band live", 98e6, 20e6, None);
        let mut ism = view("ism burst", 433.92e6, 2e6, Some((1000.0, 10.0)));
        ism.created_at = Timestamp::from_unix_nanos(fm.created_at.as_unix_nanos() + 1);
        ism.updated_at = ism.created_at;
        ism.pane_layout = Some(json!([{ "center_f_hz": 433.92e6, "span_f_hz": 2e6 }]));
        let mut late = view("fm later", 100e6, 1e6, Some((5000.0, 60.0)));
        late.created_at = Timestamp::from_unix_nanos(fm.created_at.as_unix_nanos() + 2);
        late.updated_at = late.created_at;
        for v in [&fm, &ism, &late] {
            repo.insert_saved_view(v).unwrap();
        }
        assert_eq!(repo.saved_view(ism.id).unwrap(), ism);

        let all = repo
            .saved_views_in(&SavedViewFilter::default(), 0, 10)
            .unwrap();
        assert_eq!(all.matched, 3);
        assert_eq!(
            all.rows.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![late.id, ism.id, fm.id],
            "newest created first"
        );
        // FM, 900..1100 s: the live FM view matches on frequency alone; the frozen FM view's
        // window (4970..5030 s) does not intersect.
        let win = SavedViewFilter {
            window: Some((99e6, 101e6, ts(900.0), ts(1100.0))),
        };
        let page = repo.saved_views_in(&win, 0, 10).unwrap();
        assert_eq!(
            (page.matched, page.rows[0].id),
            (1, fm.id),
            "{:?}",
            page.rows
        );
        let ism_win = SavedViewFilter {
            window: Some((433e6, 434e6, ts(1004.0), ts(1004.5))),
        };
        assert_eq!(repo.saved_views_in(&ism_win, 0, 10).unwrap().matched, 1);
        let p1 = repo
            .saved_views_in(&SavedViewFilter::default(), 1, 1)
            .unwrap();
        assert_eq!(p1.rows[0].id, ism.id);

        let mut moved = ism.clone();
        moved.center_f_hz = 915e6;
        moved.updated_at = Timestamp::now();
        let saved = repo.update_saved_view(&moved).unwrap();
        assert_eq!(saved.created_at, ism.created_at);
        assert_eq!(repo.saved_view(ism.id).unwrap().center_f_hz, 915e6);
        assert_eq!(repo.saved_views_in(&ism_win, 0, 10).unwrap().matched, 0);

        assert_eq!(repo.delete_saved_view(ism.id).unwrap().id, ism.id);
        assert!(matches!(
            repo.saved_view(ism.id),
            Err(RepoError::NotFound { .. })
        ));
    }
}
