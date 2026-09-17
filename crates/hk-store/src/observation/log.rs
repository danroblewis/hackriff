//! The segment store: buffering, flush, seal, retention and read snapshots.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use hk_model::attention::observation::{ObservationRecord, SweepGeometry};
use serde_json::{Value, json};

use super::segment::{HOUR_NS, encode_line, hour_of, list_segments, segment_path};

/// Geometries kept in memory. Each segment repeats every kept geometry its sweep records
/// reference (just before the first such record), so a segment decodes on its own.
const GEOMETRIES_KEPT: usize = 32;

/// Default age horizon of the log, ns — **180 days** (T-406, `docs/16` §5.4).
///
/// # Why it is no longer 30 days
///
/// `docs/16` §5.4 calls lengthening this *"the cheapest correctness purchase in the whole design"*,
/// and names the failure it buys off. The spectrum-history pyramid has **no age limit at all** — a
/// rolling 8 GiB byte budget — while this log expired at 30 days, so on a quiet installation the two
/// crossed: the pyramid held measurements for spectrum whose coverage record had already been
/// discarded. A cell like that is not `unobserved` (nothing looked) and cannot honestly be greyed;
/// it is the **fourth state**, *we no longer know whether we looked*
/// ([`crate::CoverageGrid::unknown_rows_before`], T-423). Every day of horizon added here is a day
/// that state does not have to be reached for, and §5.4's recommendation is to make the coverage
/// record the **longest** horizon rather than the middle one, because an interval-and-a-band is
/// orders of magnitude cheaper per unit of time covered than a spectrum cell with a histogram.
///
/// It also decides what T-406's iterative scan is worth: *"a region the sweep cleared last week"* is
/// distinguishable from *"a region the sweep has not reached"* only while a record survives to say
/// so, and a survey whose pass takes hours is answering questions about weeks.
///
/// # The arithmetic, which is measured rather than assumed
///
/// §5.4's own estimate was explicitly unverified. `observation::tests::
/// a_dwell_records_line_cost_decides_which_retention_bound_binds` measures the encoded line and
/// derives both bounds from it, and the honest summary is that **which bound binds depends on the
/// policy**:
///
/// - **Dwelling** (T-406's iterative scan) writes one line per step. At the 10 s floor of the user's
///   range that is ~8.6 k lines/day, a few MB/day, so [`DEFAULT_MAX_BYTES`] holds roughly a year and
///   this age bound binds first — which is the intended order.
/// - **Sweeping** at 50 ms hops writes one aggregated record per pass or 60 s, but each carries up
///   to ~1.5 k hop visits, so it is two orders of magnitude denser per day and the **byte** quota
///   binds long before 180 days. Raising the age alone would not have lengthened that horizon at
///   all; both had to move, and even so a continuous sweep keeps a byte-bounded horizon.
///
/// Both are settings: an installation with a small disk lowers them
/// ([`ObservationLogConfig::with_retention`], `ScanPlan.extra.pipeline.observation_retention_days`).
pub const DEFAULT_MAX_AGE_NS: i64 = 180 * 24 * HOUR_NS;

/// Default byte quota of the log — **2 GiB** (T-406, `docs/16` §5.4).
///
/// Raised with [`DEFAULT_MAX_AGE_NS`], because on a sweeping installation the byte quota is the
/// bound that actually binds (see there). It is a **ceiling reached after months**, not an
/// allocation, and it is a quarter of the spectrum-history pyramid's own 8 GiB budget for records
/// that cover orders of magnitude more time per byte than the cells they explain.
pub const DEFAULT_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;

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
    /// default [`DEFAULT_MAX_AGE_NS`] (180 days).
    pub max_age_ns: i64,
    /// Byte quota over all segments, default [`DEFAULT_MAX_BYTES`] (2 GiB).
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
            max_age_ns: DEFAULT_MAX_AGE_NS,
            max_bytes: DEFAULT_MAX_BYTES,
            queue_len: 4096,
        }
    }

    /// The retention bounds overridden (T-406): an age in **days** and a quota in **MiB**, each
    /// `None` to keep the default.
    ///
    /// Both defaults are ceilings sized for a device that has the disk (see [`DEFAULT_MAX_AGE_NS`]);
    /// this is how an installation that has not says so. A non-positive or non-finite value is
    /// ignored rather than applied, because a zero horizon would delete the coverage record the
    /// whole map is derived from.
    pub fn with_retention(mut self, max_age_days: Option<f64>, max_bytes_mb: Option<f64>) -> Self {
        if let Some(d) = max_age_days.filter(|d| d.is_finite() && *d > 0.0) {
            self.max_age_ns = (d * 24.0 * HOUR_NS as f64).min(i64::MAX as f64) as i64;
        }
        if let Some(mb) = max_bytes_mb.filter(|m| m.is_finite() && *m > 0.0) {
            self.max_bytes = (mb * 1024.0 * 1024.0).min(u64::MAX as f64) as u64;
        }
        self
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
    /// Geometry ids already in the open segment (written or buffered).
    written_geometries: HashSet<u64>,
    /// Hours whose tail this session checked (and repaired) before appending to them.
    checked_hours: HashSet<i64>,
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
                written_geometries: HashSet::new(),
                checked_hours: HashSet::new(),
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
        let mut opened = false;
        if let Some(end) = end {
            let hour = hour_of(end);
            self.newest_ns = self.newest_ns.max(end.as_unix_nanos());
            match self.open_hour {
                None => {
                    self.open(hour);
                    opened = true;
                }
                Some(open) if hour > open => {
                    self.seal(open, stats);
                    self.open(hour);
                    opened = true;
                }
                // The same hour, or a late record: the open segment.
                Some(_) => {}
            }
        } else if self.open_hour.is_none() {
            // Written in the first segment, before the first sweep record that references it.
            stats.written.fetch_add(1, Ordering::Relaxed);
            return;
        }
        match rec {
            ObservationRecord::Geometry(g) => {
                self.written_geometries.insert(g.id);
            }
            ObservationRecord::Sweep(s) => self.repeat_geometry(s.geometry),
            ObservationRecord::Dwell(_) => {}
        }
        self.pending.extend_from_slice(encode_line(rec).as_bytes());
        stats.written.fetch_add(1, Ordering::Relaxed);
        if opened {
            // After the new hour's first lines are buffered, so the quota counts them.
            self.retain(stats);
        }
        if self.pending.len() >= self.cfg.flush_bytes {
            self.flush(stats);
        }
    }

    fn open(&mut self, hour: i64) {
        self.open_hour = Some(hour);
        self.written_geometries.clear();
    }

    /// Writes geometry `id` into the open segment unless it is already there.
    fn repeat_geometry(&mut self, id: u64) {
        if self.written_geometries.contains(&id) {
            return;
        }
        if let Some(g) = self.geometries.iter().find(|g| g.id == id) {
            let line = encode_line(&ObservationRecord::Geometry(g.clone()));
            self.pending.extend_from_slice(line.as_bytes());
            self.written_geometries.insert(id);
        }
    }

    fn flush(&mut self, stats: &ObservationLogStats) {
        self.last_flush = Instant::now();
        let Some(hour) = self.open_hour else { return };
        if self.pending.is_empty() {
            return;
        }
        let path = segment_path(&self.cfg.root, hour);
        // This session's first append to an hour (or the first after a failed, possibly partial
        // write) truncates a torn tail first, so a new line never joins a partial one: the newest
        // hour after a crash, or an older hour a late or replayed record reopens.
        if self.checked_hours.insert(hour) {
            if let Ok(Some(len)) = repair_tail(&path) {
                self.segments.insert(hour, len);
            }
        }
        let r = append_bytes(&path, &self.pending);
        match r {
            Ok(()) => {
                *self.segments.entry(hour).or_insert(0) += self.pending.len() as u64;
                stats.flushes.fetch_add(1, Ordering::Relaxed);
            }
            Err(_) => {
                stats.write_errors.fetch_add(1, Ordering::Relaxed);
                self.checked_hours.remove(&hour);
                // The lost buffer may have held this segment's geometries.
                self.written_geometries.clear();
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

/// Truncates a segment to its last complete line; returns its length (`None`: no such file).
/// An intact tail costs one byte read.
fn repair_tail(path: &Path) -> io::Result<Option<u64>> {
    let mut f = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let len = f.metadata()?.len();
    if len == 0 {
        return Ok(Some(0));
    }
    let mut last = [0u8; 1];
    f.seek(SeekFrom::Start(len - 1))?;
    f.read_exact(&mut last)?;
    if last[0] == b'\n' {
        return Ok(Some(len));
    }
    let mut bytes = Vec::new();
    f.seek(SeekFrom::Start(0))?;
    f.read_to_end(&mut bytes)?;
    let keep = bytes.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1) as u64;
    f.set_len(keep)?;
    Ok(Some(keep))
}
