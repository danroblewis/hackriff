//! T-904: the run's detection-retention thread, and the `/api/status` `storage` block.
//!
//! [`hk_model::Repository::prune_detections`] holds the policy (what is kept and why); this module
//! only runs it **off the real-time path**: its own thread, its own database connection, a pass
//! every [`RetentionSettings::interval`], and a pause between batches so the detector's writer gets
//! the lock back (T-453: nothing here runs on the capture thread, and the worst lock hold of each
//! pass is measured and served, not assumed). It also refreshes the storage figures the status
//! route reports — database, WAL and free-list bytes, detection and rollup rows, the oldest
//! detection and when the next pass is due — whether or not pruning is enabled, so the growth the
//! policy exists to bound is always visible.
//!
//! Library runs ([`crate::PipelineConfig::new`]) leave pruning **off**, exactly as before; the
//! composed daemons (`hk serve`, `hk run`, `hackriffd`) turn it on with
//! [`RetentionSettings::from_env`].

use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use hk_model::{DetectionRetention, PruneReport, Repository, Timestamp};
use serde_json::{Value, json};

use crate::stats::Counters;

/// How the run applies [`DetectionRetention`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RetentionSettings {
    /// Prune at all. Off still reports the storage figures.
    pub enabled: bool,
    /// The policy each pass applies.
    pub policy: DetectionRetention,
    /// Time between passes.
    pub interval: Duration,
    /// Delay before the first pass (the run's start-up is left alone); a shorter
    /// [`Self::interval`] wins.
    pub first_after: Duration,
    /// Pause between two batches of one pass: the lock is free for the detector's writer then.
    pub pause: Duration,
    /// Time between two refreshes of the storage figures.
    pub refresh: Duration,
}

impl Default for RetentionSettings {
    /// Pruning off (the library default); the policy's defaults; 10 min passes, the first after
    /// 1 min; 20 ms between batches; storage figures every 60 s.
    fn default() -> Self {
        Self {
            enabled: false,
            policy: DetectionRetention::default(),
            interval: Duration::from_secs(600),
            first_after: Duration::from_secs(60),
            pause: Duration::from_millis(20),
            refresh: Duration::from_secs(60),
        }
    }
}

/// The retention age environment variable: a duration (`1h`, `1d`, `30m`), or `off`/`0` to keep
/// every row.
pub const ENV_RETENTION: &str = "HK_DETECTION_RETENTION";
/// `on` (default) folds pruned rows into rollups first; `off` just deletes them.
pub const ENV_ROLLUP: &str = "HK_DETECTION_ROLLUP";
/// Time between passes (a duration; default 10 min).
pub const ENV_INTERVAL: &str = "HK_DETECTION_PRUNE_INTERVAL";

impl RetentionSettings {
    /// The composed daemon's settings: pruning **on** at the policy's defaults (1 h, rolled up),
    /// overridden by [`ENV_RETENTION`], [`ENV_ROLLUP`] and [`ENV_INTERVAL`]. An unreadable value
    /// is reported on stderr and ignored — retention never fails a run.
    pub fn from_env() -> Self {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    /// [`Self::from_env`] over any lookup (tests).
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let mut s = Self {
            enabled: true,
            ..Self::default()
        };
        let duration = |key: &str| -> Option<f64> {
            let text = get(key)?;
            match hk_store::iqbuffer::parse_duration_s(&text) {
                Ok(v) => Some(v),
                Err(e) => {
                    eprintln!("hk-pipeline: ignoring {key}: {e}");
                    None
                }
            }
        };
        if let Some(age) = duration(ENV_RETENTION) {
            if age > 0.0 {
                s.policy.max_age_ns = (age * 1e9) as i64;
            } else {
                s.enabled = false;
            }
        }
        if let Some(i) = duration(ENV_INTERVAL).filter(|&i| i > 0.0) {
            s.interval = Duration::from_secs_f64(i);
        }
        if let Some(text) = get(ENV_ROLLUP) {
            match text.trim().to_ascii_lowercase().as_str() {
                "on" | "1" | "true" | "yes" => s.policy.rollup = true,
                "off" | "0" | "false" | "no" => s.policy.rollup = false,
                other => eprintln!("hk-pipeline: ignoring {ENV_ROLLUP}: {other:?} is not on/off"),
            }
        }
        s
    }

    fn to_json(self) -> Value {
        let s = |ns: i64| ns as f64 / 1e9;
        json!({
            "enabled": self.enabled,
            "max_age_s": self.enabled.then(|| s(self.policy.max_age_ns)),
            "rollup": self.policy.rollup,
            "keep_per_emitter": self.policy.keep_per_emitter,
            "batch": self.policy.batch,
            "interval_s": self.interval.as_secs_f64(),
            "rollup_gap_s": s(self.policy.rollup_gap_ns),
            "rollup_span_s": s(self.policy.rollup_span_ns),
        })
    }
}

/// The `/api/status` `storage` snapshot, written by the retention thread (`null` until its first
/// refresh, and in a run with no retention thread).
#[derive(Debug, Default)]
pub struct StorageCounters {
    snapshot: Mutex<Value>,
}

impl StorageCounters {
    /// The current snapshot.
    pub fn to_json(&self) -> Value {
        self.snapshot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn set(&self, v: Value) {
        *self.snapshot.lock().unwrap_or_else(PoisonError::into_inner) = v;
    }
}

fn unix_s(t: Timestamp) -> f64 {
    t.as_unix_nanos() as f64 / 1e9
}

fn report_json(r: &PruneReport, at: Timestamp, took: Duration, error: Option<&str>) -> Value {
    json!({
        "t_s": unix_s(at),
        "duration_s": took.as_secs_f64(),
        "watermark_s": r.watermark.map(unix_s),
        "cutoff_s": r.cutoff.map(unix_s),
        "examined": r.examined,
        "deleted": r.deleted,
        "kept_pinned": r.kept_pinned,
        "kept_tail": r.kept_tail,
        "kept_moved": r.kept_moved,
        "rollups_inserted": r.rollups_inserted,
        "rollups_extended": r.rollups_extended,
        "batches": r.batches,
        "lock_ms_max": r.lock_ns_max as f64 / 1e6,
        "lock_ms_total": r.lock_ns_total as f64 / 1e6,
        "complete": r.complete,
        "error": error,
    })
}

#[derive(Default)]
struct State {
    stop: bool,
    last_prune: Value,
    passes: u64,
    deleted_total: u64,
    errors: u64,
    next_prune: Option<Timestamp>,
}

/// The run's retention thread ([`Self::start`], [`Self::finish`]).
pub struct RetentionService {
    settings: RetentionSettings,
    db_path: PathBuf,
    counters: Arc<Counters>,
    state: Mutex<State>,
    wake: Condvar,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for RetentionService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetentionService")
            .field("settings", &self.settings)
            .field("db_path", &self.db_path)
            .finish()
    }
}

impl RetentionService {
    /// Starts the `hk-retention` thread over the run's database.
    pub fn start(
        db_path: PathBuf,
        settings: RetentionSettings,
        counters: Arc<Counters>,
    ) -> Arc<Self> {
        let me = Arc::new(Self {
            settings,
            db_path,
            counters,
            state: Mutex::new(State {
                last_prune: Value::Null,
                ..State::default()
            }),
            wake: Condvar::new(),
            thread: Mutex::new(None),
        });
        let run = Arc::clone(&me);
        match std::thread::Builder::new()
            .name("hk-retention".into())
            .spawn(move || run.body())
        {
            Ok(t) => *me.thread.lock().unwrap_or_else(PoisonError::into_inner) = Some(t),
            Err(e) => eprintln!("hk-pipeline: detection retention disabled: {e}"),
        }
        me
    }

    /// Stops the thread (an in-flight pass stops at its next batch boundary). Idempotent.
    pub fn finish(&self) {
        self.lock().stop = true;
        self.wake.notify_all();
        if let Some(t) = self
            .thread
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            let _ = t.join();
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Sleeps up to `d`; `false` once the service is stopping.
    fn sleep(&self, d: Duration) -> bool {
        let deadline = Instant::now() + d;
        let mut st = self.lock();
        while !st.stop {
            let now = Instant::now();
            if now >= deadline {
                return true;
            }
            st = self
                .wake
                .wait_timeout(st, deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        false
    }

    fn body(&self) {
        let mut repo: Option<Repository> = None;
        let mut next_prune = self
            .settings
            .enabled
            .then(|| Instant::now() + self.settings.first_after.min(self.settings.interval));
        let mut next_refresh = Instant::now();
        loop {
            self.lock().next_prune = next_prune.map(|at| {
                Timestamp::now().saturating_add_nanos(
                    at.saturating_duration_since(Instant::now()).as_nanos() as i64,
                )
            });
            let due = next_prune.map_or(next_refresh, |p| p.min(next_refresh));
            if !self.sleep(due.saturating_duration_since(Instant::now())) {
                return;
            }
            if repo.is_none() {
                repo = Repository::open(&self.db_path)
                    .map_err(|e| {
                        self.lock().errors += 1;
                        eprintln!("hk-pipeline: retention cannot open the database: {e}");
                    })
                    .ok();
            }
            let Some(r) = repo.as_mut() else {
                next_refresh = Instant::now() + self.settings.refresh;
                continue;
            };
            if next_prune.is_some_and(|p| Instant::now() >= p) {
                let started = Instant::now();
                let at = Timestamp::now();
                let result =
                    r.prune_detections(&self.settings.policy, || self.sleep(self.settings.pause));
                let took = started.elapsed();
                let mut st = self.lock();
                st.passes += 1;
                st.last_prune = match &result {
                    Ok(rep) => {
                        st.deleted_total += rep.deleted;
                        report_json(rep, at, took, None)
                    }
                    Err(e) => {
                        st.errors += 1;
                        eprintln!("hk-pipeline: detection retention pass failed: {e}");
                        report_json(&PruneReport::default(), at, took, Some(&e.to_string()))
                    }
                };
                drop(st);
                next_prune = Some(Instant::now() + self.settings.interval);
                next_refresh = Instant::now(); // show the pass's effect now
                continue;
            }
            if Instant::now() >= next_refresh {
                self.refresh(r);
                next_refresh = Instant::now() + self.settings.refresh;
            }
        }
    }

    fn refresh(&self, repo: &Repository) {
        let measured = Timestamp::now();
        let storage = repo.detection_storage();
        let st = self.lock();
        let figures = match &storage {
            Ok(s) => json!({
                "db_bytes": s.db_bytes,
                "free_bytes": s.free_bytes,
                "wal_bytes": s.wal_bytes,
                "detection_rows": s.detection_rows,
                "rollup_rows": s.rollup_rows,
                "oldest_detection_s": s.oldest_detection.map(|t| unix_s(t.end)),
                "newest_detection_s": s.newest_detection_end.map(unix_s),
                "error": Value::Null,
            }),
            Err(e) => json!({ "error": e.to_string() }),
        };
        let mut v = json!({
            "measured_s": unix_s(measured),
            "next_prune_s": st.next_prune.map(unix_s),
            "retention": self.settings.to_json(),
            "last_prune": st.last_prune,
            "passes": st.passes,
            "deleted_total": st.deleted_total,
            "errors": st.errors,
        });
        if let (Some(o), Value::Object(f)) = (v.as_object_mut(), figures) {
            o.extend(f);
        }
        drop(st);
        self.counters.storage.set(v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_turns_pruning_on_and_reads_age_rollup_and_interval() {
        let none = RetentionSettings::from_lookup(|_| None);
        assert!(none.enabled && none.policy.rollup);
        assert_eq!(none.policy.max_age_ns, 3_600_000_000_000);
        let set = RetentionSettings::from_lookup(|k| match k {
            ENV_RETENTION => Some("12h".into()),
            ENV_ROLLUP => Some("off".into()),
            ENV_INTERVAL => Some("5m".into()),
            _ => None,
        });
        assert!(set.enabled && !set.policy.rollup);
        assert_eq!(set.policy.max_age_ns, 12 * 3_600_000_000_000);
        assert_eq!(set.interval, Duration::from_secs(300));
        let off = RetentionSettings::from_lookup(|k| (k == ENV_RETENTION).then(|| "off".into()));
        assert!(!off.enabled);
        let junk = RetentionSettings::from_lookup(|_| Some("soon".into()));
        assert_eq!(
            (junk.enabled, junk.policy.max_age_ns, junk.policy.rollup),
            (true, 3_600_000_000_000, true)
        );
        assert!(
            !RetentionSettings::default().enabled,
            "library runs stay as they were"
        );
    }
}
