//! SQLite repository for the relational state (ADR-0006; docs/07 §3.1).
//!
//! [`Repository`] is the only way the rest of hackriff reads and writes row-shaped objects. Its
//! signatures use model types only, never engine types, so replacing SQLite (DuckDB was the
//! considered fallback) means reimplementing this module, not its callers. A trait is deferred
//! until a second engine exists.
//!
//! # Engine settings
//! - WAL journal (file databases), `synchronous = NORMAL`, foreign keys on, 5 s busy timeout.
//!   WAL keeps readers unblocked by the writer; [`Repository::checkpoint`] is for low-battery
//!   shutdown (C27).
//! - Write methods take `&mut self`; several connections (threads, processes) may share a file.
//!
//! # Transactions
//! - Every write transaction is `BEGIN IMMEDIATE`: it takes the write lock *before* its first
//!   read. A read-then-write on one connection therefore waits (busy timeout) for a writer on
//!   another connection, instead of failing with `SQLITE_BUSY_SNAPSHOT` when a deferred
//!   transaction tries to upgrade a stale snapshot.
//! - Region queries read `region_extent` and the rows inside one read transaction, i.e. one WAL
//!   snapshot, so a wide row committed between the two reads cannot be missed.
//!
//! # Migrations
//! Plain SQL files in `migrations/`, embedded at build time and applied in order inside a
//! transaction each. `PRAGMA user_version` records how many have run. A database newer than this
//! build is refused.
//!
//! # Content gating (ADR-0004, legal guardrail)
//! Metadata always flows; content is refused unless its [`ContentClass`] permits it (see
//! [`crate::content`]). [`RepoError::GatedContent`] comes from `insert_decode` and
//! `insert_annotation` (content present), `insert_recording` (always content), and
//! `insert_bitstream` (stored bits). The schema repeats the rule as CHECK constraints.
//!
//! # Identity and dedup decisions
//! - Ids are 16-byte UUIDv7 BLOBs.
//! - **Provenance** is deduplicated by value: SHA-256 of its [`canonical_json`] is a UNIQUE
//!   column. [`Repository::intern_provenance`] does `INSERT … ON CONFLICT DO NOTHING` then reads
//!   the winning row, so concurrent connections converge on one [`ProvenanceId`]; the stored
//!   canonical text is compared too, so a hash collision is an error, never a silent merge.
//! - **ExternalEvent** is keyed by `(source, native_id)`; upserts keep the first local id and
//!   store the payload hash that Explanation evidence pins.
//! - **Emitter** has at most one row per decoded identity (partial unique index). Entity
//!   resolution (`record_sighting`, T-018, rules in [`crate::cluster`]) counts each source
//!   observation once (the `emitter_observation` ledger; `record_sighting_measured` also
//!   catches re-measurements of the same IQ under new row ids, T-034) and records merges as
//!   `merged_into` plus superseded `emitter_link` rows; nothing is deleted. Merged emitters are
//!   skipped by region/inventory queries, and `upsert_emitter_observation` / `link_emitter`
//!   follow a merged id to its survivor.
//! - **Emitter identities** leave the repository only gated by content class (T-034; rules in
//!   [`crate::cluster`]): every public emitter read applies it, and the ungated read is
//!   crate-private.
//! - **Decodes and tags** are gated the same way (T-036, `gating.rs`): decode identities,
//!   metadata and identifier-bearing labels leave only when the class permits; tags on withheld
//!   rows are limited to controlled-vocabulary labels (T-038). `reclassify_identity` is the audited, authorised
//!   way to open a user's own identity.
//!
//! # Region queries
//! Region-indexed tables keep the largest frequency span and duration ever written
//! (`region_extent`). An overlap query turns into a *bounded* index range: a row whose lower edge
//! is more than one max-span below the query cannot overlap it. Exact overlap is then checked on
//! the stored edges, with the same closed-interval rule as [`crate::region`].

pub mod alarms; // T-122
mod bookmarks;
mod classify; // T-211
#[cfg(test)]
mod classify_rank_tests;
mod cluster;
#[cfg(test)]
mod cluster_tests;
mod clusters; // T-202 C18 clusters of unknown emissions
mod gating;
mod harmonic; // T-374 (C40): harmonic families
#[cfg(test)]
mod harmonic_tests;
mod interpret;
mod inventory;
mod lifecycle;
#[cfg(test)]
mod lifecycle_tests;
mod measure;
mod presence; // T-262 (ADR-0017 TM-5) presence intervals
#[cfg(test)]
mod presence_tests;
mod refined;
mod relate; // T-219
#[cfg(test)]
mod relate_tests;
#[cfg(test)]
mod same_emission_tests;
mod selections;
#[cfg(test)]
mod signature_tests; // T-218
mod signatures; // T-201
pub mod sites; // T-119
#[cfg(test)]
mod tests;
mod trunking; // T-266 C23 trunking metadata
mod user_band; // T-191
mod verification;

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::calibration::{CalibrationState, SpurMask};
use crate::content::ContentClass;
use crate::detection::{Detection, Track, TrackSegment};
use crate::hash::{ContentHash, canonical_json};
use crate::ids::{DetectionId, EmitterId, ExternalEventId, ProvenanceId, TrackId};
use crate::provenance::Provenance;
use crate::region::Region;
use crate::time::Timestamp;

pub use bookmarks::{BOOKMARK_NAME_MAX, BOOKMARK_NOTE_MAX, BOOKMARKS_MAX, Bookmark, BookmarkKind};
pub use harmonic::{HarmonicFamilyRow, MAX_FAMILY_CANDIDATES};
pub use inventory::EmitterUpsert;
pub use lifecycle::LIFECYCLE_TEXT_MAX;
pub use refined::{REFINED_BY_OUTPUT_ANALYSIS, REFINED_HISTORY_MAX, RefinedTuning};
pub use relate::{MAX_ARTIFACT_SOURCES, MAX_EVIDENCE_DETECTIONS, MAX_NEIGHBOURS, OverlapOutcome};
pub use selections::{
    SELECTION_LINK_REF_MAX, SELECTION_LINKS_MAX, SELECTION_NAME_MAX, SELECTION_NOTES_MAX,
    SELECTION_TAG_MAX, SELECTION_TAGS_MAX, SELECTIONS_MAX, Selection, SelectionLink,
    SelectionLinkKind, SelectionWatch,
};
pub use user_band::{USER_BAND_MAX_GAP_HZ, USER_BAND_MAX_WIDTH_HZ, UserBand};
pub use verification::{TrustTest, TrustVerdict};

/// Embedded migrations, applied in order.
const MIGRATIONS: &[&str] = &[
    include_str!("migrations/0001_init.sql"),
    include_str!("migrations/0002_attention.sql"), // T-119 sites, attention_weights
    include_str!("migrations/0003_anomaly_detail.sql"), // T-122 alarm detail + lifecycle
    include_str!("migrations/0004_site_assignment.sql"), // T-136 persisted site assignment
    include_str!("migrations/0005_content_checks_optional.sql"), // T-143 gating opt-in
    include_str!("migrations/0006_user_band.sql"), // T-191 user band override
    include_str!("migrations/0007_classification.sql"), // T-211 M3 classification columns
    include_str!("migrations/0008_emitter_relation.sql"), // T-219 C40 signal relationships
    include_str!("migrations/0009_signature.sql"), // T-218 C18 signature storage
    include_str!("migrations/0010_emission_features.sql"), // T-201 C18 measured features
    include_str!("migrations/0011_signature_cluster.sql"), // T-202 C18 clusters of unknowns
    // 0012 was reserved by ADR-0017 §9 (TM-5) for `idx_emitter_observation_time`, so T-266 took
    // 0013 first. This array, not the file name, decides what runs and in what order: 0012 simply
    // appends after 0013 (T-262), and databases at either version migrate correctly.
    include_str!("migrations/0013_trunking.sql"), // T-266 C23 trunking metadata (no call audio)
    include_str!("migrations/0012_observation_time_index.sql"), // T-262 ADR-0017 TM-5 (index only)
    include_str!("migrations/0014_harmonic_family.sql"), // T-374 C40 harmonic families
];

/// Schema version this build creates and understands.
pub const SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64;

/// Repository errors. Engine errors are boxed so no engine type appears in the API.
#[derive(Debug, thiserror::Error)]
pub enum RepoError {
    /// The storage engine failed, including constraint and immutability-trigger violations.
    #[error("storage engine: {0}")]
    Engine(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// A stored JSON body could not be read or written.
    #[error("serialisation: {0}")]
    Json(#[from] serde_json::Error),
    /// No row with that id.
    #[error("{kind} {id} not found")]
    NotFound {
        /// Object kind.
        kind: &'static str,
        /// Id or key looked up.
        id: String,
    },
    /// The request breaks a model rule (bad version number, lifecycle transition, extent...).
    #[error("invalid: {0}")]
    Invalid(String),
    /// Content was offered under a class that does not permit it (ADR-0004 gating). The
    /// metadata-only form of the object is accepted.
    #[error("{object} cannot carry content under content class {class:?} (ADR-0004 gating)")]
    GatedContent {
        /// Object kind: `decode`, `annotation`, `recording` or `bitstream`.
        object: &'static str,
        /// The gated class.
        class: ContentClass,
    },
    /// A detection with clipped samples, or under an overloaded provenance, lacks
    /// `flags.clipped`.
    #[error("detection {detection} has clipping or overload but flags.clipped is not set")]
    UnflaggedClipping {
        /// The detection.
        detection: DetectionId,
    },
    /// Explanation evidence pins an ExternalEvent payload hash that no longer matches the cache.
    #[error("evidence pins external event {event} payload {pinned}, cache holds {current}")]
    StaleEvidence {
        /// The event.
        event: ExternalEventId,
        /// Hash in the evidence.
        pinned: ContentHash,
        /// Hash of the cached payload.
        current: ContentHash,
    },
    /// A decoded identity already names a different emitter. Entity resolution (T-018) must
    /// merge the two before the observation can be recorded.
    #[error("identity {identity} already belongs to emitter {existing}")]
    IdentityConflict {
        /// The identity's scheme, `scheme:<withheld>`. Never the value: errors leave the process
        /// ungated (legal guardrail, [`crate::cluster`]).
        identity: String,
        /// Emitter that holds it.
        existing: EmitterId,
    },
    /// `reclassify_identity` refused to open an identity (T-036 legal guardrail). The reason is a
    /// fixed string; it never names the identity.
    #[error("identity reclassification refused: {reason}")]
    ReclassificationRefused {
        /// Why.
        reason: &'static str,
    },
    /// The database was written by a newer build.
    #[error("database schema version {found} is newer than this build supports ({supported})")]
    SchemaTooNew {
        /// Version found.
        found: i64,
        /// Newest version this build knows.
        supported: i64,
    },
}

impl From<rusqlite::Error> for RepoError {
    fn from(e: rusqlite::Error) -> Self {
        RepoError::Engine(Box::new(e))
    }
}

/// A resolved provenance chain: Detection/Recording → Provenance → CalibrationState / SpurMask
/// (docs/07 §2.6).
#[derive(Clone, Debug, PartialEq)]
pub struct ProvenanceChain {
    /// Provenance row id.
    pub id: ProvenanceId,
    /// The trust record.
    pub provenance: Provenance,
    /// Calibration version it names, if any.
    pub calibration: Option<CalibrationState>,
    /// Spur-mask version it names, if any.
    pub spur_mask: Option<SpurMask>,
}

/// The relational store.
pub struct Repository {
    conn: Connection,
}

impl Repository {
    /// Opens (creating if needed) a database file in WAL mode and applies pending migrations.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, RepoError> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        let mode: String =
            conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(RepoError::Invalid(format!(
                "could not enable WAL journal (got {mode})"
            )));
        }
        Self::init(conn)
    }

    /// Opens a private in-memory database (tests, replay scratch).
    pub fn open_in_memory() -> Result<Self, RepoError> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> Result<Self, RepoError> {
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        migrate(&mut conn)?;
        lifecycle::ensure_schema(&conn)?;
        Ok(Self { conn })
    }

    /// Schema version of the open database.
    pub fn schema_version(&self) -> Result<i64, RepoError> {
        Ok(self
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))?)
    }

    /// Journal mode (`wal` for file databases, `memory` in memory).
    pub fn journal_mode(&self) -> Result<String, RepoError> {
        Ok(self
            .conn
            .pragma_query_value(None, "journal_mode", |r| r.get(0))?)
    }

    /// Checkpoints the WAL into the main file and truncates it (call before a low-battery
    /// shutdown, C27).
    pub fn checkpoint(&mut self) -> Result<(), RepoError> {
        self.conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
        Ok(())
    }

    /// Runs `f` in **one** `BEGIN IMMEDIATE` write transaction and commits if it returns `Ok`.
    /// If `f` (or the commit) fails, nothing it wrote is kept: the transaction rolls back and the
    /// error is returned. [`RepoBatch`] exposes typed writes only (no SQL), with the same rules
    /// as the corresponding [`Repository`] methods.
    pub fn batch<T, F>(&mut self, f: F) -> Result<T, RepoError>
    where
        F: FnOnce(&mut RepoBatch<'_>) -> Result<T, RepoError>,
    {
        let tx = self.write_tx()?;
        let out = {
            let mut b = RepoBatch { conn: &tx };
            f(&mut b)?
        };
        tx.commit()?;
        Ok(out)
    }

    /// Opens a write batch (T-112): every write on this connection until
    /// [`Self::commit_write_batch`] lands in one IMMEDIATE transaction. Writes that open their
    /// own transaction nest as savepoints, so a failed one rolls back only itself. Refused when a
    /// transaction is already open.
    pub fn begin_write_batch(&mut self) -> Result<(), RepoError> {
        if !self.conn.is_autocommit() {
            return Err(RepoError::Invalid("a transaction is already open".into()));
        }
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        Ok(())
    }

    /// Commits the batch [`Self::begin_write_batch`] opened (rolled back if the commit fails, or
    /// an error if SQLite already rolled it back).
    pub fn commit_write_batch(&mut self) -> Result<(), RepoError> {
        if self.conn.is_autocommit() {
            return Err(RepoError::Invalid("no write batch is open".into()));
        }
        if let Err(e) = self.conn.execute_batch("COMMIT") {
            if !self.conn.is_autocommit() {
                let _ = self.conn.execute_batch("ROLLBACK");
            }
            return Err(e.into());
        }
        Ok(())
    }

    /// A write transaction that holds the write lock from its first statement, or a savepoint
    /// inside an open write batch.
    fn write_tx(&mut self) -> Result<Tx<'_>, RepoError> {
        if self.conn.is_autocommit() {
            Ok(Tx::Own(self.conn.transaction_with_behavior(
                TransactionBehavior::Immediate,
            )?))
        } else {
            Ok(Tx::Savepoint(self.conn.savepoint()?))
        }
    }

    /// A read transaction: every read inside it sees one snapshot. Dropping it ends it. Inside
    /// an open write batch the reads join it (one snapshot, the batch's own writes included).
    fn read_tx(&self) -> Result<Tx<'_>, RepoError> {
        if self.conn.is_autocommit() {
            Ok(Tx::Own(self.conn.unchecked_transaction()?))
        } else {
            Ok(Tx::Joined(&self.conn))
        }
    }
}

/// A transaction scope: its own transaction, a savepoint in an open batch, or the batch itself.
enum Tx<'a> {
    Own(Transaction<'a>),
    Savepoint(rusqlite::Savepoint<'a>),
    Joined(&'a Connection),
}

impl Tx<'_> {
    fn commit(self) -> rusqlite::Result<()> {
        match self {
            Tx::Own(t) => t.commit(),
            Tx::Savepoint(s) => s.commit(),
            Tx::Joined(_) => Ok(()),
        }
    }
}

impl std::ops::Deref for Tx<'_> {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        match self {
            Tx::Own(t) => t,
            Tx::Savepoint(s) => s,
            Tx::Joined(c) => c,
        }
    }
}

/// Typed writes inside one [`Repository::batch`] transaction.
pub struct RepoBatch<'a> {
    conn: &'a Connection,
}

impl RepoBatch<'_> {
    /// [`Repository::insert_detections`] in this transaction.
    pub fn insert_detections(&mut self, detections: &[Detection]) -> Result<(), RepoError> {
        measure::insert_detections_on(self.conn, detections)
    }

    /// [`Repository::upsert_track`] in this transaction.
    pub fn upsert_track(&mut self, track: &Track) -> Result<(), RepoError> {
        inventory::upsert_track_on(self.conn, track)
    }

    /// [`Repository::link_detections_to_track`] in this transaction.
    pub fn link_detections_to_track(
        &mut self,
        track_id: TrackId,
        detections: &[DetectionId],
        linked_at: Timestamp,
    ) -> Result<(), RepoError> {
        inventory::link_detections_on(self.conn, track_id, detections, linked_at)
    }

    /// [`Repository::append_track_segments`] in this transaction.
    pub fn append_track_segments(&mut self, segments: &[TrackSegment]) -> Result<(), RepoError> {
        inventory::append_track_segments_on(self.conn, segments)
    }

    /// [`Repository::track_detections`] as seen inside this transaction (its own writes
    /// included).
    pub fn track_detections(&self, track_id: TrackId) -> Result<Vec<DetectionId>, RepoError> {
        inventory::track_detections_on(self.conn, track_id)
    }
}

fn migrate(conn: &mut Connection) -> Result<(), RepoError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if current > SCHEMA_VERSION {
        return Err(RepoError::SchemaTooNew {
            found: current,
            supported: SCHEMA_VERSION,
        });
    }
    for (index, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", index as i64 + 1)?;
    }
    tx.commit()?;
    Ok(())
}

// ---- codec and rule helpers shared by the submodules ----

/// Id → 16-byte BLOB.
fn blob<I: Into<uuid::Uuid>>(id: I) -> [u8; 16] {
    id.into().into_bytes()
}

/// Optional id → optional BLOB.
fn opt_blob<I: Into<uuid::Uuid>>(id: Option<I>) -> Option<[u8; 16]> {
    id.map(blob)
}

/// Unit-variant enum → its serde string (the TEXT column form).
fn enum_text<T: Serialize>(value: &T) -> Result<String, RepoError> {
    match serde_json::to_value(value)? {
        Value::String(s) => Ok(s),
        other => Err(RepoError::Invalid(format!(
            "expected a unit enum variant, got {other}"
        ))),
    }
}

/// TEXT column → unit-variant enum.
fn enum_parse<T: DeserializeOwned>(text: String) -> Result<T, RepoError> {
    Ok(serde_json::from_value(Value::String(text))?)
}

/// u64 → INTEGER, refusing values SQLite cannot hold.
fn int(value: u64, what: &str) -> Result<i64, RepoError> {
    i64::try_from(value).map_err(|_| RepoError::Invalid(format!("{what} {value} exceeds i64")))
}

/// Rejects a non-finite float (serde_json would store NaN/∞ as null and fail to read it back).
fn finite(value: f64, what: &str) -> Result<f64, RepoError> {
    if value.is_finite() {
        Ok(value)
    } else {
        Err(RepoError::Invalid(format!(
            "{what} must be finite, got {value}"
        )))
    }
}

/// ADR-0004 gate: refuses content under a class that does not permit it.
fn gate(object: &'static str, class: ContentClass, carries_content: bool) -> Result<(), RepoError> {
    if carries_content && !class.permits_content() {
        Err(RepoError::GatedContent { object, class })
    } else {
        Ok(())
    }
}

/// Canonical JSON and its hash, refusing values that do not round-trip (e.g. NaN stored as null).
fn canonical_with_hash<T>(value: &T, what: &str) -> Result<(String, ContentHash), RepoError>
where
    T: Serialize + DeserializeOwned + PartialEq,
{
    let canonical = canonical_json(value)?;
    let round_trips = serde_json::from_str::<T>(&canonical)
        .map(|back| back == *value)
        .unwrap_or(false);
    if !round_trips {
        return Err(RepoError::Invalid(format!(
            "{what} does not round-trip through JSON (non-finite float?)"
        )));
    }
    let hash = ContentHash::of_text(&canonical);
    Ok((canonical, hash))
}

/// Reads a JSON `body` column by primary key.
fn body_by_id<T: DeserializeOwned>(
    conn: &Connection,
    sql: &str,
    id: [u8; 16],
    kind: &'static str,
) -> Result<T, RepoError> {
    let body: Option<String> = conn
        .prepare_cached(sql)?
        .query_row([id], |r| r.get(0))
        .optional()?;
    match body {
        Some(b) => Ok(serde_json::from_str(&b)?),
        None => Err(RepoError::NotFound {
            kind,
            id: uuid::Uuid::from_bytes(id).to_string(),
        }),
    }
}

/// Reads every JSON `body` a query returns.
fn bodies<T: DeserializeOwned, P: rusqlite::Params>(
    conn: &Connection,
    sql: &str,
    params: P,
) -> Result<Vec<T>, RepoError> {
    let mut stmt = conn.prepare_cached(sql)?;
    let texts = stmt
        .query_map(params, |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    texts
        .iter()
        .map(|t| serde_json::from_str(t).map_err(RepoError::from))
        .collect()
}

/// Raises a table's recorded maximum frequency span and duration.
fn bump_extent(conn: &Connection, table: &str, f_span: f64, t_span: i64) -> Result<(), RepoError> {
    conn.prepare_cached(
        "UPDATE region_extent SET max_f_span = max(max_f_span, ?1), \
         max_t_span = max(max_t_span, ?2) WHERE table_name = ?3",
    )?
    .execute(rusqlite::params![f_span, t_span, table])?;
    Ok(())
}

/// Index range bounds for an overlap query on `table`.
#[derive(Clone, Copy, Debug)]
struct RegionBounds {
    /// Query frequency edges.
    lo: f64,
    hi: f64,
    /// Smallest lower edge a matching row can have (`lo` − max span − slack).
    f_lo_min: f64,
    /// Smallest start time a matching row can have (`t0` − max duration).
    t_start_min: i64,
    /// Query time ends.
    t0: i64,
    t1: i64,
    /// Largest recorded frequency span (with slack), for centre-frequency bounds.
    f_span: f64,
}

/// Reads the extent bounds. Call inside the same read transaction as the query that uses them.
fn region_bounds(
    conn: &Connection,
    table: &str,
    region: &Region,
) -> Result<RegionBounds, RepoError> {
    let (max_f_span, max_t_span): (f64, i64) = conn
        .prepare_cached("SELECT max_f_span, max_t_span FROM region_extent WHERE table_name = ?1")?
        .query_row([table], |r| Ok((r.get(0)?, r.get(1)?)))?;
    // Slack absorbs float rounding between stored centre and edges.
    let f_span = max_f_span * (1.0 + 1e-9) + 1.0;
    let t0 = region.time.start.as_unix_nanos();
    Ok(RegionBounds {
        lo: region.freq.lo_hz,
        hi: region.freq.hi_hz,
        f_lo_min: region.freq.lo_hz - f_span,
        t_start_min: t0.saturating_sub(max_t_span),
        t0,
        t1: region.time.end.as_unix_nanos(),
        f_span,
    })
}
