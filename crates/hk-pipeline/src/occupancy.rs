//! Occupancy wiring (T-118, ADR-0012 §2): feeds frame samples, detections and observation totals
//! to `hk_context::occupancy::engine` off the real-time path and persists series.
//!
//! [`OccupancyService`] lives for the whole run (all segments) with its own thread
//! (`hk-occupancy`), which never touches samples: it wakes every `poll` (wall time, I/O cadence
//! only), reads **the history's sample clock** (`Pyramid::latest_frame_end`, the end of the newest
//! folded frame) and closes every 15-min interval that ended at least `settle` before it:
//!
//! 1. new detections since the last close (its own read connection to `hackriff.db`) update the
//!    learned [`ChannelPlan`]; a new version is saved (`channels.json`);
//! 2. the level-0 history grid of the tuned band (centre ± 40 % of the rate) for the interval is
//!    read under the product lock (the history reader queues frames meanwhile, T-037b);
//! 3. visits come from the observation log when one is fed ([`OccupancyService::record_observation`],
//!    the T-115 shim) or else from the coverage mask ([`hk_context::occupancy::engine::coverage_visits`],
//!    at `coverage_tier`: a parked, user-tuned device observes independently of activity);
//! 4. band and channel visits are evaluated into compact samples kept for `horizon` (24 h + 1 h),
//!    so §2.5 widening needs no second read;
//! 5. 15-min rows (and 1-h rows at hour boundaries, recomputed from the same samples, which equals
//!    summing the 15-min counts and weights, §2.8) are appended to `hk_store::occupancy` in one batch.
//!
//! At the end of the run [`OccupancyService::finish`] closes the remaining (possibly partial)
//! interval. [`OccupancyService::span_stats`] computes rows over an arbitrary span on demand
//! (`/api/occupancy?interval=span`), reading the history hourly and the final plan.
//!
//! **T-115 status.** The observation log store is not merged; the service accepts records through
//! [`OccupancyService::record_observation`] into a `MemoryObservations`. Once T-115 lands, its
//! pipeline observer (or a store reader) feeds that call; nothing else changes.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use hk_context::occupancy::channels::{ChannelPlan, DetectionExtent, LearnConfig};
use hk_context::occupancy::engine::{
    self, EngineConfig, EvalInput, LevelSource, MemoryObservations, ObservationSource,
    SubjectContext, VisitSample,
};
use hk_model::attention::ATTENTION_SCHEMA_VERSION;
use hk_model::attention::observation::{ObservationRecord, Tier};
use hk_model::attention::occupancy::{Channel, ChannelKey, OccupancyStat, OccupancySubject};
use hk_model::frames::PowerUnit;
use hk_model::{FreqRange, Region, Repository, TimeRange, Timestamp};
use hk_store::occupancy::{
    OccupancyQuery, OccupancyRows, OccupancyStore, OccupancyStoreConfig, StoredChannelPlan,
};
use hk_store::{FloorProduct, RegionHistory};
use serde::Serialize;

use crate::stats::Counters;

const HOUR_NS: i64 = 3_600_000_000_000;
/// Longest span `span_stats` computes.
pub const MAX_SPAN_NS: i64 = 7 * 24 * HOUR_NS;
/// Widest band `span_stats` computes, Hz.
pub const MAX_SPAN_WIDTH_HZ: f64 = 20e6;

/// Service settings.
#[derive(Clone, Copy, Debug)]
pub struct OccupancyConfig {
    /// Interval, ns (15 min).
    pub interval_ns: i64,
    /// Engine settings.
    pub engine: EngineConfig,
    /// Tier of coverage-derived visits (no observation log): `ScheduledPlan`.
    pub coverage_tier: Tier,
    /// Samples kept for widening, ns (25 h).
    pub horizon_ns: i64,
    /// History progress past an interval end before it closes, ns (5 s).
    pub settle_ns: i64,
    /// Channel learning.
    pub learn: LearnConfig,
    /// Thread wake-up (wall time, I/O cadence).
    pub poll: Duration,
    /// Series store limits.
    pub store: OccupancyStoreConfig,
}

impl Default for OccupancyConfig {
    fn default() -> Self {
        Self {
            interval_ns: 900_000_000_000,
            engine: EngineConfig::default(),
            coverage_tier: Tier::ScheduledPlan,
            horizon_ns: 25 * HOUR_NS,
            settle_ns: 5_000_000_000,
            learn: LearnConfig::default(),
            poll: Duration::from_millis(500),
            store: OccupancyStoreConfig::default(),
        }
    }
}

/// Service counters.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct OccupancyServiceStats {
    /// Intervals closed.
    pub intervals_closed: u64,
    /// Rows appended.
    pub rows_written: u64,
    /// Detections read for learning.
    pub detections_read: u64,
    /// Current plan version.
    pub plan_version: u32,
    /// Store/read errors (the service keeps going).
    pub errors: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum SubjectId {
    Channel(ChannelKey),
    Band(i64, i64),
}

struct Inner {
    store: Option<OccupancyStore>,
    plan: ChannelPlan,
    series: BTreeMap<SubjectId, Vec<VisitSample>>,
    next_close_ns: Option<i64>,
    first_ns: Option<i64>,
    det_cursor_ns: i64,
    repo: Option<Repository>,
    unit: PowerUnit,
    stats: OccupancyServiceStats,
    finished: bool,
}

/// Level-0 history under the product lock, one query at a time.
struct ProductLevels<'a>(&'a Mutex<FloorProduct>);

impl LevelSource for ProductLevels<'_> {
    fn level0(&self, freq: FreqRange, span: TimeRange) -> Option<RegionHistory> {
        let p = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        p.uncalibrated_pyramid().level0(freq, span)
    }
}

/// The run's occupancy engine, series and learned channel plan.
pub struct OccupancyService {
    cfg: OccupancyConfig,
    product: Arc<Mutex<FloorProduct>>,
    db_path: PathBuf,
    counters: Arc<Counters>,
    inner: Mutex<Inner>,
    observations: Mutex<Option<MemoryObservations>>,
    stop: AtomicBool,
    thread: Mutex<Option<JoinHandle<()>>>,
}

fn snap_band(freq: FreqRange, f_cell: f64) -> (FreqRange, SubjectId) {
    let lo = (freq.lo_hz / f_cell).floor() as i64;
    let hi = ((freq.hi_hz / f_cell).ceil() as i64).max(lo + 1);
    (
        FreqRange::new(lo as f64 * f_cell, hi as f64 * f_cell),
        SubjectId::Band(lo, hi),
    )
}

fn inside(inner: FreqRange, outer: FreqRange) -> bool {
    inner.lo_hz >= outer.lo_hz && inner.hi_hz <= outer.hi_hz
}

/// Evaluates one grid chunk of `band` and its `channels` into `out`.
#[allow(clippy::too_many_arguments)]
fn evaluate_chunk(
    cfg: &OccupancyConfig,
    grid: &RegionHistory,
    band: FreqRange,
    band_id: SubjectId,
    channels: &[Channel],
    obs: Option<&MemoryObservations>,
    dets: &[DetectionExtent],
    chunk: TimeRange,
    out: &mut BTreeMap<SubjectId, Vec<VisitSample>>,
) {
    let idle = engine::band_levels(grid, band);
    let visits = |f: FreqRange| -> Vec<engine::Visit> {
        let v = match obs {
            Some(o) => o.visits(f, chunk),
            None => engine::coverage_visits(grid, f, cfg.coverage_tier),
        };
        v.into_iter()
            .filter(|v| v.observed.start >= chunk.start && v.observed.start < chunk.end)
            .collect()
    };
    let f_cell = grid.f_cell_hz;
    let mut subjects: Vec<(SubjectId, FreqRange, f64)> = vec![(band_id, band, f_cell)];
    subjects.extend(channels.iter().filter_map(|c| {
        let f = c.key.freq(f_cell);
        inside(f, band).then_some((SubjectId::Channel(c.key), f, c.obw_hz))
    }));
    for (id, f, obw) in subjects {
        let v = visits(f);
        if v.is_empty() {
            continue;
        }
        let (s, _) = engine::evaluate(
            &cfg.engine.threshold,
            f,
            obw,
            EvalInput {
                grid,
                visits: &v,
                detections: dets,
                idle_levels_db: &idle,
            },
        );
        out.entry(id).or_default().extend(s);
    }
}

/// Rows of `interval` for every subject with samples.
fn rows_for(
    cfg: &OccupancyConfig,
    channels: &[Channel],
    series: &BTreeMap<SubjectId, Vec<VisitSample>>,
    interval: TimeRange,
    data_span: TimeRange,
    unit: PowerUnit,
    f_cell: f64,
) -> Vec<OccupancyStat> {
    let obw = |k: &ChannelKey| {
        channels
            .iter()
            .find(|c| c.key == *k)
            .map_or(k.freq(f_cell).width_hz(), |c| c.obw_hz)
    };
    let ctx = |subject, obw_hz| SubjectContext {
        subject,
        rbw_hz: f_cell,
        obw_hz,
        unit,
        calibration: None,
    };
    let mut ch_rows = Vec::new();
    for (id, s) in series {
        if let SubjectId::Channel(key) = id {
            let c = ctx(OccupancySubject::Channel { key: *key }, Some(obw(key)));
            ch_rows.extend(engine::stat(&cfg.engine, &c, interval, s, data_span, None));
        }
    }
    let mut rows = Vec::new();
    for (id, s) in series {
        if let SubjectId::Band(lo, hi) = id {
            let band = FreqRange::new(*lo as f64 * f_cell, *hi as f64 * f_cell);
            let inner: Vec<OccupancyStat> = ch_rows
                .iter()
                .filter(|r| match r.subject {
                    OccupancySubject::Channel { key } => inside(key.freq(f_cell), band),
                    OccupancySubject::Band { .. } => false,
                })
                .cloned()
                .collect();
            let c = ctx(OccupancySubject::Band { freq: band }, None);
            rows.extend(engine::stat(
                &cfg.engine,
                &c,
                interval,
                s,
                data_span,
                engine::sro(&inner),
            ));
        }
    }
    rows.extend(ch_rows);
    rows
}

fn ts(ns: i64) -> Timestamp {
    Timestamp::from_unix_nanos(ns)
}

impl OccupancyService {
    /// Opens the series store under `dir` and restores the saved plan.
    pub fn open(
        dir: PathBuf,
        product: Arc<Mutex<FloorProduct>>,
        db_path: PathBuf,
        counters: Arc<Counters>,
        cfg: OccupancyConfig,
    ) -> Arc<Self> {
        let (scheme, f_cell) = {
            let p = product.lock().unwrap_or_else(PoisonError::into_inner);
            (p.config().pyramid.scheme, p.config().pyramid.f_cell_hz)
        };
        let mut stats = OccupancyServiceStats::default();
        let store = OccupancyStore::open(dir, cfg.store)
            .map_err(|_| stats.errors += 1)
            .ok();
        let plan = store
            .as_ref()
            .and_then(|s| s.load_plan().ok().flatten())
            .filter(|p| p.scheme == scheme && p.f_cell_hz == f_cell)
            .map_or_else(
                || ChannelPlan::new(scheme, f_cell, cfg.learn),
                |p| ChannelPlan::from_channels(scheme, f_cell, p.version, &p.channels, cfg.learn),
            );
        stats.plan_version = plan.version();
        Arc::new(Self {
            cfg,
            product,
            db_path,
            counters,
            inner: Mutex::new(Inner {
                store,
                plan,
                series: BTreeMap::new(),
                next_close_ns: None,
                first_ns: None,
                det_cursor_ns: i64::MIN,
                repo: None,
                unit: PowerUnit::Dbfs,
                stats,
                finished: false,
            }),
            observations: Mutex::new(None),
            stop: AtomicBool::new(false),
            thread: Mutex::new(None),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Starts the closing thread.
    pub fn start(self: &Arc<Self>) {
        let me = Arc::clone(self);
        let t = std::thread::Builder::new()
            .name("hk-occupancy".into())
            .spawn(move || {
                while !me.stop.load(Ordering::SeqCst) {
                    std::thread::sleep(me.cfg.poll);
                    if let Some(now) = me.history_time_ns() {
                        me.close_due(now - me.cfg.settle_ns);
                    }
                }
            });
        if let Ok(t) = t {
            *self.thread.lock().unwrap_or_else(PoisonError::into_inner) = Some(t);
        }
    }

    fn history_time_ns(&self) -> Option<i64> {
        let p = self.product.lock().unwrap_or_else(PoisonError::into_inner);
        p.uncalibrated_pyramid()
            .latest_frame_end()
            .map(Timestamp::as_unix_nanos)
    }

    /// The T-115 shim: an observation record for visit timing and tiers. Once any record arrives,
    /// visits come from the log instead of the coverage mask.
    pub fn record_observation(&self, rec: ObservationRecord) {
        let mut o = self
            .observations
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        o.get_or_insert_with(MemoryObservations::default).push(rec);
    }

    /// Stops the thread and closes everything up to the history's end, including a partial last
    /// interval. Idempotent.
    pub fn finish(&self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self
            .thread
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            let _ = t.join();
        }
        let Some(end) = self.history_time_ns() else {
            return;
        };
        self.close_due(end);
        let mut inner = self.lock();
        if inner.finished {
            return;
        }
        inner.finished = true;
        if let Some(next) = inner.next_close_ns {
            let start = next - self.cfg.interval_ns;
            if end > start {
                self.close(&mut inner, TimeRange::new(ts(start), ts(end)), true);
            }
        }
    }

    fn close_due(&self, through_ns: i64) {
        let mut inner = self.lock();
        if inner.finished {
            return;
        }
        let i = self.cfg.interval_ns;
        let next = *inner.next_close_ns.get_or_insert_with(|| {
            // The first interval is the one holding the history's first frame.
            let first = self.first_frame_ns().unwrap_or(through_ns);
            first.div_euclid(i) * i + i
        });
        let mut next = next;
        while next <= through_ns {
            let iv = TimeRange::new(ts(next - i), ts(next));
            self.close(&mut inner, iv, next % HOUR_NS == 0);
            next += i;
            inner.next_close_ns = Some(next);
        }
    }

    fn first_frame_ns(&self) -> Option<i64> {
        // The counters' stream time starts at the source's first block; good enough for alignment.
        let t = self
            .counters
            .stream_time_ns
            .load(std::sync::atomic::Ordering::Relaxed);
        (t > 0).then_some(t)
    }

    fn detections(&self, inner: &mut Inner, span: TimeRange) -> Vec<DetectionExtent> {
        if inner.repo.is_none() {
            inner.repo = Repository::open(&self.db_path).ok();
        }
        let Some(repo) = inner.repo.as_ref() else {
            inner.stats.errors += 1;
            return Vec::new();
        };
        match repo.detections_in_region(&Region::new(FreqRange::new(0.0, 1e12), span)) {
            Ok(d) => d.iter().map(DetectionExtent::of).collect(),
            Err(_) => {
                inner.stats.errors += 1;
                Vec::new()
            }
        }
    }

    fn close(&self, inner: &mut Inner, iv: TimeRange, hour: bool) {
        let f_cell = inner.plan.f_cell_hz();
        // 1. Learn from detections that started since the last close.
        let cursor = inner.det_cursor_ns.max(iv.start.as_unix_nanos() - self.cfg.horizon_ns);
        let dets: Vec<DetectionExtent> = self
            .detections(inner, TimeRange::new(ts(cursor), iv.end))
            .into_iter()
            .filter(|d| d.time.start.as_unix_nanos() >= cursor && d.time.start < iv.end)
            .collect();
        inner.det_cursor_ns = iv.end.as_unix_nanos();
        inner.stats.detections_read += dets.len() as u64;
        if inner.plan.learn(&dets) {
            let stored = StoredChannelPlan {
                schema: ATTENTION_SCHEMA_VERSION,
                version: inner.plan.version(),
                scheme: inner.plan.scheme(),
                f_cell_hz: f_cell,
                channels: inner.plan.channels(),
            };
            if let Some(s) = inner.store.as_mut()
                && s.save_plan(&stored).is_err()
            {
                inner.stats.errors += 1;
            }
            inner.stats.plan_version = inner.plan.version();
        }
        // 2–4. Evaluate the tuned band's grid for the interval.
        let (center, rate) = self.counters.tune();
        if rate > 0.0 && center > 0.0 {
            let (band, band_id) = snap_band(FreqRange::centered(center, 0.8 * rate), f_cell);
            if let Some(grid) = ProductLevels(&self.product).level0(band, iv) {
                inner.unit = grid.unit;
                let channels = inner.plan.channels_in(band);
                let obs = self
                    .observations
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                let mut series = std::mem::take(&mut inner.series);
                evaluate_chunk(
                    &self.cfg,
                    &grid,
                    band,
                    band_id,
                    &channels,
                    obs.as_ref(),
                    &dets,
                    iv,
                    &mut series,
                );
                inner.series = series;
            }
        }
        let keep_from = iv.end.as_unix_nanos() - self.cfg.horizon_ns;
        for s in inner.series.values_mut() {
            s.retain(|v| v.start_ns >= keep_from);
            if let Some(f) = s.first() {
                let f = f.start_ns;
                inner.first_ns = Some(inner.first_ns.map_or(f, |x| x.min(f)));
            }
        }
        inner.series.retain(|_, s| !s.is_empty());
        // 5. Rows.
        let Some(first) = inner.first_ns else {
            inner.stats.intervals_closed += 1;
            return;
        };
        let data_span = TimeRange::new(ts(first.min(iv.start.as_unix_nanos())), iv.end);
        let channels = inner.plan.channels();
        let mut rows = rows_for(&self.cfg, &channels, &inner.series, iv, data_span, inner.unit, f_cell);
        if hour {
            let e = iv.end.as_unix_nanos();
            let h = TimeRange::new(ts((e - 1).div_euclid(HOUR_NS) * HOUR_NS), iv.end);
            if h.duration_ns() > self.cfg.interval_ns {
                rows.extend(rows_for(&self.cfg, &channels, &inner.series, h, data_span, inner.unit, f_cell));
            }
        }
        inner.stats.intervals_closed += 1;
        if rows.is_empty() {
            return;
        }
        match inner.store.as_mut().map(|s| s.append(&rows)) {
            Some(Ok(n)) => inner.stats.rows_written += n as u64,
            _ => inner.stats.errors += 1,
        }
    }

    /// The learned plan's channels overlapping `freq`: `(version, scheme, f_cell_hz, channels)`.
    pub fn channels(&self, freq: FreqRange) -> (u32, u16, f64, Vec<Channel>) {
        let inner = self.lock();
        let p = &inner.plan;
        (p.version(), p.scheme(), p.f_cell_hz(), p.channels_in(freq))
    }

    /// Plan version and level-0 cell width.
    pub fn plan_info(&self) -> (u32, f64) {
        let inner = self.lock();
        (inner.plan.version(), inner.plan.f_cell_hz())
    }

    /// Stored rows.
    pub fn query(&self, q: &OccupancyQuery) -> Result<OccupancyRows, String> {
        let inner = self.lock();
        let s = inner.store.as_ref().ok_or("occupancy store unavailable")?;
        s.query(q).map_err(|e| e.to_string())
    }

    /// Rows over `span` for the band `freq` and the plan's channels inside it, computed on demand
    /// from the history (hourly reads) and the run's detections; no widening beyond `span`.
    pub fn span_stats(&self, freq: FreqRange, span: TimeRange) -> Result<Vec<OccupancyStat>, String> {
        if span.duration_ns() <= 0 || span.duration_ns() > MAX_SPAN_NS {
            return Err("span must be positive and at most 7 days".into());
        }
        if !(freq.hi_hz > freq.lo_hz) || freq.width_hz() > MAX_SPAN_WIDTH_HZ {
            return Err("band must be positive and at most 20 MHz".into());
        }
        let (channels, f_cell) = {
            let inner = self.lock();
            (inner.plan.channels(), inner.plan.f_cell_hz())
        };
        let (band, band_id) = snap_band(freq, f_cell);
        let channels: Vec<Channel> = channels
            .into_iter()
            .filter(|c| inside(c.key.freq(f_cell), band))
            .collect();
        let dets = {
            let mut inner = self.lock();
            self.detections(&mut inner, span)
        };
        let obs = self
            .observations
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let levels = ProductLevels(&self.product);
        let mut series = BTreeMap::new();
        let mut unit = PowerUnit::Dbfs;
        let (s0, s1) = (span.start.as_unix_nanos(), span.end.as_unix_nanos());
        let mut a = s0;
        while a < s1 {
            let b = ((a.div_euclid(HOUR_NS) + 1) * HOUR_NS).min(s1);
            let chunk = TimeRange::new(ts(a), ts(b));
            if let Some(grid) = levels.level0(band, chunk) {
                unit = grid.unit;
                evaluate_chunk(
                    &self.cfg,
                    &grid,
                    band,
                    band_id,
                    &channels,
                    obs.as_ref(),
                    &dets,
                    chunk,
                    &mut series,
                );
            }
            a = b;
        }
        Ok(rows_for(&self.cfg, &channels, &series, span, span, unit, f_cell))
    }

    /// Counters.
    pub fn stats(&self) -> OccupancyServiceStats {
        self.lock().stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn occupancy_band_snapping_is_outward_and_stable() {
        let (b, id) = snap_band(FreqRange::new(433_300_001.0, 433_699_999.0), 6250.0);
        assert!(b.lo_hz <= 433_300_001.0 && b.hi_hz >= 433_699_999.0);
        let (_, id2) = snap_band(FreqRange::new(433_300_002.0, 433_699_998.0), 6250.0);
        assert_eq!(id, id2);
    }
}
