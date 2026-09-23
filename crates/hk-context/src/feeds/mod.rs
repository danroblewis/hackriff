//! Offline-first context-feed cache (C29, ADR-0008).
//!
//! # Storage (and why it is split)
//!
//! - **Events go in SQLite via [`Repository`]** ([`Repository::upsert_external_event`]). They have
//!   to: Explanation rows reference `external_event` by foreign key and the repository refuses
//!   evidence whose pinned payload hash no longer matches the cache. Identity is
//!   `(source, native_id)`, so re-ingesting a snapshot is idempotent.
//! - **Feed bookkeeping and raw snapshots go in files** under `<data_dir>/context/feeds/<source>/`:
//!   `state.json` ([`FeedState`]) and `raw/<key>/<sha256>.txt` (content-addressed, so a raw
//!   snapshot is immutable and a revised one sits next to it). This keeps a per-feed, rebuildable
//!   record out of the core schema until the Phase-3 storage ADR, lets snapshots be imported from
//!   removable media, and costs nothing if lost: re-ingesting the raw files rebuilds both.
//!   `state.json` is replaced atomically (write temp, rename), and it is written *last*, so a crash
//!   mid-ingest leaves the previous state plus idempotent upserts.
//!
//! # Network
//!
//! Fetching is behind [`FeedFetcher`]. This crate ships no network implementation: tests use
//! [`DirectoryFetcher`] (frozen snapshots on disk) and [`OfflineFetcher`]. A failed refresh marks
//! the feed stale and leaves the cache fully usable.
//!
//! # Staleness
//!
//! A feed is stale at `now` when it never succeeded, its last refresh attempt failed, or `now` is
//! past its validity window ([`FeedState::is_stale`]). Correlation still uses stale data and marks
//! the result provisional ([`crate::correlate`]).

pub mod gnss_orbits;
pub mod gpsjam;
pub mod tle;

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use hk_model::{
    ContentHash, ExternalEvent, ExternalEventId, RepoError, Repository, TimeRange, Timestamp,
};
use serde::{Deserialize, Serialize};

/// One stored raw snapshot of a feed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SnapshotRecord {
    /// SHA-256 (hex) of the snapshot text; the raw file is `raw/<key>/<sha256>.txt`.
    pub sha256: String,
    /// When it was fetched or imported.
    pub fetched_at: Timestamp,
    /// Events it produced (after the adapter's ingest filter).
    pub events: usize,
}

/// Per-feed cache bookkeeping (C29 `FeedState`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FeedState {
    /// Feed id, e.g. `gpsjam`.
    pub source: String,
    /// Adapter parser version that produced the cached events.
    pub parser_version: String,
    /// Last fetch or import attempt.
    #[serde(default)]
    pub last_attempt: Option<Timestamp>,
    /// Fetch time of the newest successfully ingested snapshot (start of the validity window).
    #[serde(default)]
    pub fetched_at: Option<Timestamp>,
    /// End of the validity window of that snapshot.
    #[serde(default)]
    pub valid_until: Option<Timestamp>,
    /// The last refresh attempt failed (the cache may be behind the feed).
    #[serde(default)]
    pub stale: bool,
    /// Last error, if the last attempt failed.
    #[serde(default)]
    pub last_error: Option<String>,
    /// Time spans the cache holds complete data for, merged and sorted. Lets correlation tell
    /// "no event" from "no data".
    #[serde(default)]
    pub coverage: Vec<TimeRange>,
    /// Latest raw snapshot per key.
    #[serde(default)]
    pub snapshots: BTreeMap<String, SnapshotRecord>,
}

impl FeedState {
    /// Empty state for a feed never fetched.
    pub fn new(source: &str, parser_version: &str) -> Self {
        Self {
            source: source.to_owned(),
            parser_version: parser_version.to_owned(),
            last_attempt: None,
            fetched_at: None,
            valid_until: None,
            stale: false,
            last_error: None,
            coverage: Vec::new(),
            snapshots: BTreeMap::new(),
        }
    }

    /// Stale at `now`: never succeeded, last attempt failed, or past the validity window.
    pub fn is_stale(&self, now: Timestamp) -> bool {
        self.fetched_at.is_none() || self.stale || self.valid_until.is_some_and(|v| now > v)
    }

    /// Cache age at `now`, seconds (`None` if never fetched).
    pub fn cache_age_s(&self, now: Timestamp) -> Option<f64> {
        self.fetched_at
            .map(|f| (now.as_unix_nanos() - f.as_unix_nanos()) as f64 / 1e9)
    }

    /// Whether `range` lies entirely inside one coverage span.
    pub fn covers(&self, range: &TimeRange) -> bool {
        self.coverage
            .iter()
            .any(|c| c.start <= range.start && c.end >= range.end)
    }

    /// Adds a covered span, merging overlapping or adjacent (1 ns apart) spans.
    pub fn add_coverage(&mut self, span: TimeRange) {
        self.coverage.push(span);
        self.coverage.sort_by_key(|r| (r.start, r.end));
        let mut merged: Vec<TimeRange> = Vec::with_capacity(self.coverage.len());
        for r in self.coverage.drain(..) {
            match merged.last_mut() {
                Some(last)
                    if r.start.as_unix_nanos() <= last.end.as_unix_nanos().saturating_add(1) =>
                {
                    last.end = last.end.max(r.end);
                }
                _ => merged.push(r),
            }
        }
        self.coverage = merged;
    }
}

/// A snapshot the adapter could not parse. Schema changes fail loudly, naming the line.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("line {line}: {message}")]
pub struct ParseError {
    /// 1-based line (0 for the snapshot as a whole).
    pub line: usize,
    /// What is wrong.
    pub message: String,
}

/// Why a fetch failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum FetchError {
    /// No connectivity (or fetching disabled).
    #[error("offline")]
    Offline,
    /// The feed has no snapshot for the key.
    #[error("no snapshot {0}")]
    NotFound(String),
    /// Anything else.
    #[error("{0}")]
    Other(String),
}

/// Feed-cache errors.
#[derive(Debug, thiserror::Error)]
pub enum FeedError {
    /// Filesystem.
    #[error("{path}: {source}")]
    Io {
        /// Path involved.
        path: PathBuf,
        /// Cause.
        #[source]
        source: std::io::Error,
    },
    /// `state.json` unreadable or unwritable.
    #[error("feed state: {0}")]
    Json(#[from] serde_json::Error),
    /// Repository.
    #[error(transparent)]
    Repo(#[from] RepoError),
    /// Snapshot did not parse.
    #[error("{source_id} snapshot {key}: {error}")]
    Parse {
        /// Feed.
        source_id: String,
        /// Snapshot key.
        key: String,
        /// Parse failure.
        error: ParseError,
    },
    /// Fetch failed; the feed is now marked stale.
    #[error("{source_id} fetch {key}: {error}")]
    Fetch {
        /// Feed.
        source_id: String,
        /// Snapshot key.
        key: String,
        /// Fetch failure.
        error: FetchError,
    },
    /// A source id or key that is not a safe path component.
    #[error("invalid feed name {0:?} (use [A-Za-z0-9._-], not starting with '.')")]
    InvalidName(String),
}

fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> FeedError + '_ {
    move |source| FeedError::Io {
        path: path.to_owned(),
        source,
    }
}

fn check_name(name: &str) -> Result<(), FeedError> {
    let ok = !name.is_empty()
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if ok {
        Ok(())
    } else {
        Err(FeedError::InvalidName(name.to_owned()))
    }
}

/// Writes `bytes` to `path` atomically (temp file in the same directory, fsync, rename).
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), FeedError> {
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir).map_err(io_err(dir))?;
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    {
        let mut f = fs::File::create(&tmp).map_err(io_err(&tmp))?;
        f.write_all(bytes).map_err(io_err(&tmp))?;
        f.sync_all().map_err(io_err(&tmp))?;
    }
    fs::rename(&tmp, path).map_err(io_err(path))
}

/// The on-disk feed cache under `<data_dir>/context/feeds/`.
#[derive(Clone, Debug)]
pub struct FeedCache {
    root: PathBuf,
}

impl FeedCache {
    /// Opens (creating) the cache under `data_dir`.
    pub fn open(data_dir: impl AsRef<Path>) -> Result<Self, FeedError> {
        let root = data_dir.as_ref().join("context").join("feeds");
        fs::create_dir_all(&root).map_err(io_err(&root))?;
        Ok(Self { root })
    }

    /// Cache root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn state_path(&self, source: &str) -> PathBuf {
        self.root.join(source).join("state.json")
    }

    /// A feed's state, or `None` if never recorded.
    pub fn state(&self, source: &str) -> Result<Option<FeedState>, FeedError> {
        check_name(source)?;
        let path = self.state_path(source);
        match fs::read(&path) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io_err(&path)(e)),
        }
    }

    /// Replaces a feed's state atomically.
    pub fn save_state(&self, state: &FeedState) -> Result<(), FeedError> {
        check_name(&state.source)?;
        write_atomic(
            &self.state_path(&state.source),
            &serde_json::to_vec_pretty(state)?,
        )
    }

    /// Stores a raw snapshot immutably at `raw/<key>/<sha256>.txt` (a no-op if that content is
    /// already stored). Returns its path and hash.
    pub fn store_snapshot(
        &self,
        source: &str,
        key: &str,
        body: &str,
    ) -> Result<(PathBuf, ContentHash), FeedError> {
        check_name(source)?;
        check_name(key)?;
        let hash = ContentHash::of_text(body);
        let path = self
            .root
            .join(source)
            .join("raw")
            .join(key)
            .join(format!("{}.txt", hash.to_hex()));
        if !path.exists() {
            write_atomic(&path, body.as_bytes())?;
        }
        Ok((path, hash))
    }

    /// The latest stored raw snapshot for `key`, if any.
    pub fn snapshot(&self, source: &str, key: &str) -> Result<Option<String>, FeedError> {
        let Some(state) = self.state(source)? else {
            return Ok(None);
        };
        let Some(record) = state.snapshots.get(key) else {
            return Ok(None);
        };
        check_name(key)?;
        let path = self
            .root
            .join(source)
            .join("raw")
            .join(key)
            .join(format!("{}.txt", record.sha256));
        fs::read_to_string(&path).map(Some).map_err(io_err(&path))
    }
}

/// What to fetch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FetchRequest {
    /// Feed id.
    pub source: String,
    /// Snapshot key, e.g. a date.
    pub key: String,
    /// The feed's file name for that key (the last URL path segment).
    pub file_name: String,
}

/// A fetched snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fetched {
    /// Snapshot text.
    pub body: String,
    /// When it was fetched.
    pub fetched_at: Timestamp,
}

/// Gets a feed snapshot. Implementations that touch the network are never used in tests.
pub trait FeedFetcher {
    /// Fetches one snapshot.
    fn fetch(&mut self, request: &FetchRequest) -> Result<Fetched, FetchError>;
}

/// Always offline.
#[derive(Clone, Copy, Debug, Default)]
pub struct OfflineFetcher;

impl FeedFetcher for OfflineFetcher {
    fn fetch(&mut self, _: &FetchRequest) -> Result<Fetched, FetchError> {
        Err(FetchError::Offline)
    }
}

/// Reads `<dir>/<file_name>`: frozen test snapshots, or a manual import from removable media.
#[derive(Clone, Debug)]
pub struct DirectoryFetcher {
    /// Directory holding snapshot files.
    pub dir: PathBuf,
    /// Fetch time to record (the copy time of the snapshot).
    pub fetched_at: Timestamp,
}

impl FeedFetcher for DirectoryFetcher {
    fn fetch(&mut self, request: &FetchRequest) -> Result<Fetched, FetchError> {
        let path = self.dir.join(&request.file_name);
        match fs::read_to_string(&path) {
            Ok(body) => Ok(Fetched {
                body,
                fetched_at: self.fetched_at,
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(FetchError::NotFound(path.display().to_string()))
            }
            Err(e) => Err(FetchError::Other(format!("{}: {e}", path.display()))),
        }
    }
}

/// A parsed snapshot.
#[derive(Clone, Debug, PartialEq)]
pub struct Parsed {
    /// Events to cache, in a deterministic order.
    pub events: Vec<ExternalEvent>,
    /// Time span the snapshot covers completely.
    pub coverage: TimeRange,
    /// Records read but not cached (below the adapter's ingest filter).
    pub skipped: usize,
}

/// One feed's parser.
pub trait FeedAdapter {
    /// Feed id (the ExternalEvent `source`).
    fn source(&self) -> &str;
    /// Parser version, recorded in the state.
    fn parser_version(&self) -> &str;
    /// The feed's file name for a key.
    fn file_name(&self, key: &str) -> String;
    /// Parses a snapshot fetched at `fetched_at`.
    fn parse(&self, key: &str, body: &str, fetched_at: Timestamp) -> Result<Parsed, ParseError>;
}

/// Result of an ingest.
#[derive(Clone, Debug, PartialEq)]
pub struct IngestReport {
    /// Snapshot key.
    pub key: String,
    /// Raw snapshot hash.
    pub snapshot_hash: ContentHash,
    /// Cached events: stored id and payload hash, in parse order.
    pub events: Vec<(ExternalEventId, ContentHash)>,
    /// Records not cached.
    pub skipped: usize,
    /// Covered span.
    pub coverage: TimeRange,
}

/// Parses and caches a snapshot (fetched or imported). Order: parse (a failure records the error,
/// marks the feed stale and caches nothing) → raw file → event upserts → state. Idempotent.
pub fn ingest_snapshot(
    cache: &FeedCache,
    repo: &mut Repository,
    adapter: &dyn FeedAdapter,
    key: &str,
    body: &str,
    fetched_at: Timestamp,
) -> Result<IngestReport, FeedError> {
    let source = adapter.source().to_owned();
    check_name(key)?;
    let mut state = cache
        .state(&source)?
        .unwrap_or_else(|| FeedState::new(&source, adapter.parser_version()));
    state.last_attempt = Some(state.last_attempt.map_or(fetched_at, |t| t.max(fetched_at)));
    let parsed = match adapter.parse(key, body, fetched_at) {
        Ok(p) => p,
        Err(error) => {
            state.stale = true;
            state.last_error = Some(format!("parse {key}: {error}"));
            cache.save_state(&state)?;
            return Err(FeedError::Parse {
                source_id: source,
                key: key.to_owned(),
                error,
            });
        }
    };
    let (_, snapshot_hash) = cache.store_snapshot(&source, key, body)?;
    let mut events = Vec::with_capacity(parsed.events.len());
    let mut valid_until: Option<Timestamp> = None;
    for event in &parsed.events {
        events.push(repo.upsert_external_event(event)?);
        valid_until = match (valid_until, event.valid_until) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
    }
    // A snapshot older than the newest one does not move the validity window back.
    if state.fetched_at.is_none_or(|f| fetched_at >= f) {
        state.fetched_at = Some(fetched_at);
        state.valid_until = valid_until;
    }
    state.parser_version = adapter.parser_version().to_owned();
    state.stale = false;
    state.last_error = None;
    state.add_coverage(parsed.coverage);
    state.snapshots.insert(
        key.to_owned(),
        SnapshotRecord {
            sha256: snapshot_hash.to_hex(),
            fetched_at,
            events: events.len(),
        },
    );
    cache.save_state(&state)?;
    Ok(IngestReport {
        key: key.to_owned(),
        snapshot_hash,
        events,
        skipped: parsed.skipped,
        coverage: parsed.coverage,
    })
}

/// Fetches and ingests one snapshot. A fetch failure records the attempt at `now`, marks the feed
/// stale, and returns [`FeedError::Fetch`]; everything already cached keeps working.
pub fn refresh(
    cache: &FeedCache,
    repo: &mut Repository,
    fetcher: &mut dyn FeedFetcher,
    adapter: &dyn FeedAdapter,
    key: &str,
    now: Timestamp,
) -> Result<IngestReport, FeedError> {
    check_name(key)?;
    let request = FetchRequest {
        source: adapter.source().to_owned(),
        key: key.to_owned(),
        file_name: adapter.file_name(key),
    };
    match fetcher.fetch(&request) {
        Ok(fetched) => {
            ingest_snapshot(cache, repo, adapter, key, &fetched.body, fetched.fetched_at)
        }
        Err(error) => {
            let mut state = cache
                .state(&request.source)?
                .unwrap_or_else(|| FeedState::new(&request.source, adapter.parser_version()));
            state.last_attempt = Some(now);
            state.stale = true;
            state.last_error = Some(format!("fetch {key}: {error}"));
            cache.save_state(&state)?;
            Err(FeedError::Fetch {
                source_id: request.source,
                key: key.to_owned(),
                error,
            })
        }
    }
}

#[cfg(test)]
mod tests;

impl fmt::Display for FeedState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} (parser {}): fetched {:?}, valid until {:?}, stale {}, {} coverage span(s)",
            self.source,
            self.parser_version,
            self.fetched_at.map(Timestamp::as_unix_nanos),
            self.valid_until.map(Timestamp::as_unix_nanos),
            self.stale,
            self.coverage.len()
        )
    }
}
