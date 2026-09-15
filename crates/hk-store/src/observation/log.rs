//! The segment store: buffering, flush, seal, retention and read snapshots.

use std::collections::{BTreeMap, VecDeque};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use hk_model::attention::observation::{ObservationRecord, SweepGeometry};
use serde_json::{Value, json};

use super::segment::{HOUR_NS, encode_line, hour_of, list_segments, segment_path};

/// Geometries re-written at the head of every segment.
const GEOMETRIES_KEPT: usize = 4;

/// Settings of an observation log.
#[derive(Clone, Debug, PartialEq)]
pub struct ObservationLogConfig {
    /// Directory of the segments (`<data>/observations`).
    pub root: PathBuf,
    /// Longest time unflushed lines wait (wall time), default 60 s.
    pub flush_interval: Duration,
    /// Buffered bytes that force a flush, default 256 KiB.
    pub flush_bytes: usize,
    /// Hours ending more than this before the newest record (sample time) are deleted, ns;
    /// default 30 days.
    pub max_age_ns: i64,
    /// Byte quota over all segments, default 512 MiB.
    pub max_bytes: u64,
    /// Records the writer queue holds before producers drop, default 4096.
    pub queue_len: usize,
}

impl ObservationLogConfig {
    /// Defaults (ADR-0012 §1.5) under `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            flush_interval: Duration::from_secs(60),
            flush_bytes: 256 * 1024,
            max_age_ns: 30 * 24 * HOUR_NS,
            max_bytes: 512 * 1024 * 1024,
            queue_len: 4096,
        }
    }

    /// Low-power mode: flushes stretch to 5 minutes (ADR-0012 §9).
    pub fn low_power(mut self) -> Self {
        self.flush_interval = Duration::from_secs(300);
        self
    }
}

/// Counters of an observation log (producers, writer and store).
#[derive(Debug, Default)]
pub struct ObservationLogStats {
    /// Records offered to the queue.
    pub offered: AtomicU64,
    /// Records dropped because the queue was full (or the writer had stopped).
    pub dropped: AtomicU64,
    /// Records appended to the log.
    pub written: AtomicU64,
    /// Buffer flushes to disk.
    pub flushes: AtomicU64,
    /// Segments fsynced at hour seal.
    pub sealed: AtomicU64,
    /// Write or sync errors (the buffered lines are dropped).
    pub write_errors: AtomicU64,
    /// Segments deleted by retention.
    pub segments_deleted: AtomicU64,
}

impl ObservationLogStats {
    /// A JSON object of the counters.
    pub fn to_json(&self) -> Value {
        let g = |a: &AtomicU64| a.load(Ordering::Relaxed);
        json!({
            "offered": g(&self.offered),
            "dropped": g(&self.dropped),
            "written": g(&self.written),
            "flushes": g(&self.flushes),
            "sealed": g(&self.sealed),
            "write_errors": g(&self.write_errors),
            "segments_deleted": g(&self.segments_deleted),
        })
    }
}

pub(super) struct Inner {
    cfg: ObservationLogConfig,
    open_hour: Option<i64>,
    pending: Vec<u8>,
    /// On-disk bytes per hour.
    segments: BTreeMap<i64, u64>,
    geometries: VecDeque<SweepGeometry>,
    newest_ns: i64,
    last_flush: Instant,
}

/// A shared handle on an observation log (see the module docs). Cloning shares the log.
#[derive(Clone)]
pub struct ObservationStore {
    inner: Arc<Mutex<Inner>>,
    stats: Arc<ObservationLogStats>,
}

/// Holds the store's lock (tests: a stalled writer).
pub struct StallGuard<'a>(#[allow(dead_code)] MutexGuard<'a, Inner>);

/// Files and unflushed bytes of an hour range, read without the lock.
pub(super) struct Snapshot {
    pub files: Vec<PathBuf>,
    pub pending: Vec<u8>,
}

impl ObservationStore {
    /// Opens (creating) the log at `cfg.root`, indexing existing segments and truncating a torn
    /// tail of the newest one.
    pub fn open(cfg: ObservationLogConfig) -> io::Result<Self> {
        std::fs::create_dir_all(&cfg.root)?;
        let found = list_segments(&cfg.root);
        if let Some((_, path, _)) = found.last() {
            repair_tail(path)?;
        }
        let segments = list_segments(&cfg.root)
            .into_iter()
            .map(|(h, _, b)| (h, b))
            .collect();
        Ok(Self {
            inner: Arc::new(Mutex::new(Inner {
                cfg,
                open_hour: None,
                pending: Vec::new(),
                segments,
                geometries: VecDeque::new(),
                newest_ns: i64::MIN,
                last_flush: Instant::now(),
            })),
            stats: Arc::new(ObservationLogStats::default()),
        })
    }

    /// Counters.
    pub fn stats(&self) -> &Arc<ObservationLogStats> {
        &self.stats
    }

    /// Settings.
    pub fn config(&self) -> ObservationLogConfig {
        self.lock().cfg.clone()
    }

    /// Root directory.
    pub fn root(&self) -> PathBuf {
        self.lock().cfg.root.clone()
    }

    pub(super) fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Holds the store's lock until the guard drops: the writer thread stalls on its next record
    /// (tests of the never-blocks rule).
    #[doc(hidden)]
    pub fn stall(&self) -> StallGuard<'_> {
        StallGuard(self.lock())
    }

    /// Appends one record (buffered); flushes when the buffer is full, seals the previous hour
    /// and applies retention when the hour changes.
    pub fn append(&self, rec: &ObservationRecord) {
        let mut g = self.lock();
        g.append(rec, &self.stats);
    }

    /// Flushes when the flush interval has passed since the last flush.
    pub fn maybe_flush(&self) {
        let mut g = self.lock();
        if !g.pending.is_empty() && g.last_flush.elapsed() >= g.cfg.flush_interval {
            g.flush(&self.stats);
        }
    }

    /// Flushes buffered lines now (checkpoint, shutdown).
    pub fn flush(&self) {
        self.lock().flush(&self.stats);
    }

    /// Flushes and fsyncs the open segment (shutdown, low battery).
    pub fn seal(&self) {
        let mut g = self.lock();
        if let Some(h) = g.open_hour {
            g.seal(h, &self.stats);
        }
    }

    /// Applies retention now against the newest record's sample time.
    pub fn retain(&self) {
        self.lock().retain(&self.stats);
    }

    /// Bytes of the log: on-disk segments plus the unflushed buffer.
    pub fn bytes(&self) -> u64 {
        let g = self.lock();
        g.segments.values().sum::<u64>() + g.pending.len() as u64
    }

    /// Hours present (on disk or buffered), oldest first.
    pub fn hours(&self) -> Vec<i64> {
        let g = self.lock();
        let mut h: Vec<i64> = g.segments.keys().copied().collect();
        if let Some(o) = g.open_hour {
            if !g.segments.contains_key(&o) && !g.pending.is_empty() {
                h.push(o);
            }
        }
        h
    }

    pub(super) fn snapshot(&self, first_hour: i64, last_hour: i64) -> Snapshot {
        let g = self.lock();
        let files = g
            .segments
            .range(first_hour..=last_hour)
            .map(|(h, _)| segment_path(&g.cfg.root, *h))
            .collect();
        let pending = match g.open_hour {
            Some(o) if (first_hour..=last_hour).contains(&o) => g.pending.clone(),
            _ => Vec::new(),
        };
        Snapshot { files, pending }
    }
}

impl Inner {
    fn append(&mut self, rec: &ObservationRecord, stats: &ObservationLogStats) {
        let end = match rec {
            ObservationRecord::Dwell(d) => Some(d.planned.end.max(d.observed.end)),
            ObservationRecord::Sweep(s) => Some(s.span.end),
            ObservationRecord::Geometry(g) => {
                self.geometries.retain(|k| k.id != g.id);
                if self.geometries.len() == GEOMETRIES_KEPT {
                    self.geometries.pop_front();
                }
                self.geometries.push_back(g.clone());
                None
            }
        };
        if let Some(end) = end {
            let hour = hour_of(end);
            self.newest_ns = self.newest_ns.max(end.as_unix_nanos());
            match self.open_hour {
                None => {
                    self.open(hour);
                    self.retain(stats);
                }
                Some(open) if hour > open => {
                    self.seal(open, stats);
                    self.open(hour);
                    self.retain(stats);
                }
                // The same hour, or a late record: the open segment.
                Some(_) => {}
            }
        } else if self.open_hour.is_none() {
            // Written at the head of the first segment.
            stats.written.fetch_add(1, Ordering::Relaxed);
            return;
        }
        self.pending.extend_from_slice(encode_line(rec).as_bytes());
        stats.written.fetch_add(1, Ordering::Relaxed);
        if self.pending.len() >= self.cfg.flush_bytes {
            self.flush(stats);
        }
    }

    fn open(&mut self, hour: i64) {
        self.open_hour = Some(hour);
        let head: Vec<u8> = self
            .geometries
            .iter()
            .flat_map(|g| encode_line(&ObservationRecord::Geometry(g.clone())).into_bytes())
            .collect();
        self.pending.extend_from_slice(&head);
    }

    fn flush(&mut self, stats: &ObservationLogStats) {
        self.last_flush = Instant::now();
        let Some(hour) = self.open_hour else { return };
        if self.pending.is_empty() {
            return;
        }
        let path = segment_path(&self.cfg.root, hour);
        let r = append_bytes(&path, &self.pending);
        match r {
            Ok(()) => {
                *self.segments.entry(hour).or_insert(0) += self.pending.len() as u64;
                stats.flushes.fetch_add(1, Ordering::Relaxed);
            }
            Err(_) => {
                stats.write_errors.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.pending.clear();
    }

    fn seal(&mut self, hour: i64, stats: &ObservationLogStats) {
        self.flush(stats);
        let path = segment_path(&self.cfg.root, hour);
        if path.exists() {
            match std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .and_then(|f| f.sync_all())
            {
                Ok(()) => stats.sealed.fetch_add(1, Ordering::Relaxed),
                Err(_) => stats.write_errors.fetch_add(1, Ordering::Relaxed),
            };
        }
    }

    fn retain(&mut self, stats: &ObservationLogStats) {
        if self.newest_ns == i64::MIN {
            return;
        }
        let cutoff = self.newest_ns.saturating_sub(self.cfg.max_age_ns);
        let open = self.open_hour;
        let aged: Vec<i64> = self
            .segments
            .keys()
            .copied()
            .filter(|h| Some(*h) != open && (h + 1).saturating_mul(HOUR_NS) <= cutoff)
            .collect();
        for h in aged {
            self.delete(h, stats);
        }
        let mut total: u64 = self.segments.values().sum::<u64>() + self.pending.len() as u64;
        while total > self.cfg.max_bytes {
            let Some((&h, &b)) = self.segments.iter().find(|(h, _)| Some(**h) != open) else {
                break;
            };
            self.delete(h, stats);
            total -= b;
        }
    }

    fn delete(&mut self, hour: i64, stats: &ObservationLogStats) {
        self.segments.remove(&hour);
        let path = segment_path(&self.cfg.root, hour);
        if std::fs::remove_file(&path).is_ok() {
            stats.segments_deleted.fetch_add(1, Ordering::Relaxed);
        }
        // Empty day/month/year directories go too (best effort).
        let mut dir = path.parent();
        for _ in 0..3 {
            let Some(d) = dir else { break };
            if d == self.cfg.root || std::fs::remove_dir(d).is_err() {
                break;
            }
            dir = d.parent();
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        let stats = ObservationLogStats::default();
        if let Some(h) = self.open_hour {
            self.seal(h, &stats);
        }
    }
}

fn append_bytes(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    f.write_all(bytes)
}

/// Truncates a segment to its last complete line.
fn repair_tail(path: &Path) -> io::Result<()> {
    let bytes = std::fs::read(path)?;
    if bytes.is_empty() || bytes.ends_with(b"\n") {
        return Ok(());
    }
    let keep = bytes.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)?
        .set_len(keep as u64)
}
