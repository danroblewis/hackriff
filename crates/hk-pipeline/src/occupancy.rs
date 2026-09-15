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
//! 2. **every band observed in the interval** is evaluated: the distinct snapped analysed extents
//!    of the observation log's dwell windows and visited sweep hops that start in it (so a retune
//!    by the user or the scheduler inside an interval loses nothing), or, without a log or any
//!    record in it, the tuned band (centre ± 40 % of the rate);
//! 3. each band's level-0 history grid is read in **chunks** of at most `chunk_cells` cells (and
//!    never across a 15-min boundary), each under the product lock for that read only, so the
//!    history reader's queue (T-037b, 600 frames) drains between chunks;
//! 4. visits come from the observation log when one is fed ([`OccupancyService::record_observation`]
//!    or the T-115 store) or else from the coverage mask ([`hk_context::occupancy::engine::coverage_visits`],
//!    at `coverage_tier`: a parked, user-tuned device observes independently of activity); a visit
//!    crossing a chunk boundary is evaluated per chunk piece. Thresholds sit over each column's
//!    local floor (`engine::local_floors`);
//! 5. band and channel visits are evaluated into compact samples kept for `horizon` (24 h + 1 h),
//!    so §2.5 widening needs no second read;
//! 6. 15-min rows (and 1-h rows at hour boundaries, recomputed from the same samples, which equals
//!    summing the 15-min counts and weights, §2.8) for every subject observed in the interval are
//!    appended to `hk_store::occupancy` in one batch.
//!
//! At the end of the run [`OccupancyService::finish`] closes the remaining (possibly partial)
//! interval. [`OccupancyService::span_stats`] computes rows over an arbitrary span on demand
//! (`/api/occupancy?interval=span`) with the same chunked reads and the final plan.
//!
//! **Memory bound of a span query:** one chunk grid (`CHUNK_CELLS` × `size_of::<CellStats>()`,
//! ≈ 44 MB) plus per-column scratch (a few × band cells × 8 B) plus the visit samples, capped at
//! `MAX_SPAN_SAMPLES` × `size_of::<VisitSample>()` (≈ 96 MB; a query over it fails rather than
//! grows). Band × span is limited to `MAX_SPAN_HZ_S` (20 MHz × 6 h), which keeps 1 s coverage
//! visits of a 20 MHz band and ~90 channels inside the sample cap. A 15-min close holds one chunk
//! grid at a time as well.

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
use hk_model::attention::baseline::SiteKey;
use hk_model::attention::observation::{ObservationRecord, Tier};
use hk_model::attention::occupancy::{Channel, ChannelKey, OccupancyStat, OccupancySubject};
use hk_model::frames::PowerUnit;
use hk_model::{FreqRange, Region, Repository, TimeRange, Timestamp};
use hk_store::observation::{ObservationStore, RecordQuery};
use hk_store::occupancy::{
    OccupancyQuery, OccupancyRows, OccupancyStore, OccupancyStoreConfig, StoredChannelPlan,
};
use hk_store::{FloorProduct, RegionHistory};
use serde::Serialize;

use crate::attention::{AttentionService, FirstSighting};
use crate::stats::Counters;

const HOUR_NS: i64 = 3_600_000_000_000;
/// Longest span `span_stats` computes.
pub const MAX_SPAN_NS: i64 = 7 * 24 * HOUR_NS;
/// Widest band `span_stats` computes, Hz.
pub const MAX_SPAN_WIDTH_HZ: f64 = 20e6;
/// Largest band width × span `span_stats` computes, Hz·s (20 MHz × 6 h, or 1 MHz × 5 days).
pub const MAX_SPAN_HZ_S: f64 = 20e6 * 6.0 * 3600.0;
/// Most visit samples one `span_stats` call keeps.
pub const MAX_SPAN_SAMPLES: usize = 2_000_000;
/// Default most cells of one history read.
pub const CHUNK_CELLS: usize = 1_000_000;

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
    /// Most cells of one history read (and of one product-lock hold), [`CHUNK_CELLS`].
    pub chunk_cells: usize,
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
            chunk_cells: CHUNK_CELLS,
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
    /// Emitters counted as first sightings over the last close's window.
    sighted: std::collections::HashSet<hk_model::ids::EmitterId>,
    repo: Option<Repository>,
    unit: PowerUnit,
    stats: OccupancyServiceStats,
    finished: bool,
}

/// Level-0 history under the product lock, held for one read only.
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
    t_cell_ns: i64,
    inner: Mutex<Inner>,
    observations: Mutex<Option<MemoryObservations>>,
    store_log: Mutex<Option<ObservationStore>>,
    stop: AtomicBool,
    thread: Mutex<Option<JoinHandle<()>>>,
    /// T-128: the run's attention service, fed at each close.
    attention: Mutex<Option<Arc<crate::attention::AttentionService>>>,
    /// T-131: the run's novelty alarm service and the feed cache (correlation context), fed with
    /// each close's folds.
    alarms: Mutex<Option<AlarmSlot>>,
}

/// T-131: the alarm service the occupancy thread feeds, with its feed cache.
struct AlarmSlot {
    service: Arc<crate::alarms::AlarmService>,
    feeds: Option<(hk_context::Correlator, hk_context::FeedCache)>,
}

/// The T-115 observation log as a visit source (overload is not in its visits; §2.6 suspect
/// masking then rests on detection flags).
struct StoreObservations<'a>(&'a ObservationStore);

impl ObservationSource for StoreObservations<'_> {
    fn visits(&self, freq: FreqRange, span: TimeRange) -> Vec<engine::Visit> {
        self.0
            .observations_of(freq, span)
            .into_iter()
            .filter(|v| v.observed.start >= span.start && v.observed.start < span.end)
            .map(|v| engine::Visit {
                observed: v.observed,
                tier: v.tier,
                overload: false,
            })
            .collect()
    }

    fn bands(&self, span: TimeRange) -> Vec<FreqRange> {
        let starts_in = |t: Timestamp| t >= span.start && t < span.end;
        let mut out = Vec::new();
        let mut cursor = 0;
        loop {
            let page = self.0.query(&RecordQuery {
                freq: FreqRange::new(0.0, 1e12),
                span,
                tier: None,
                cursor,
                limit: 10_000,
            });
            for r in &page.records {
                match r {
                    ObservationRecord::Dwell(d) if starts_in(d.observed.start) => {
                        out.push(d.window.usable);
                    }
                    ObservationRecord::Sweep(s) => {
                        let Some(g) = page.geometries.iter().find(|g| g.id == s.geometry) else {
                            continue;
                        };
                        for v in &s.visits {
                            let t = s
                                .span
                                .start
                                .saturating_add_nanos(i64::from(v.start_ms) * 1_000_000);
                            if let Some(h) = g.hops.get(v.hop as usize)
                                && starts_in(t)
                            {
                                out.push(h.usable);
                            }
                        }
                    }
                    _ => {}
                }
            }
            match page.next_cursor {
                Some(c) if c > cursor => cursor = c,
                _ => break,
            }
        }
        out
    }
}

/// Records pushed through the shim win over the store log.
fn pick<'a>(
    mem: Option<&'a MemoryObservations>,
    log: Option<&'a StoreObservations<'a>>,
) -> Option<&'a dyn ObservationSource> {
    match (mem, log) {
        (Some(m), _) => Some(m),
        (None, Some(l)) => Some(l),
        (None, None) => None,
    }
}

/// T-131/T-136: stamps `rows` with the site assigned at the close `t` and returns it with its
/// geometry. The occupancy close is the only live caller that advances (and may persist) the site
/// state machine; history frames, which run ahead of it, only peek ([`crate::history::frame_site`]).
fn stamp_site(
    a: &AttentionService,
    rows: &mut [OccupancyStat],
    t: Timestamp,
) -> (SiteKey, Option<hk_context::geo::Site>) {
    let (site, geo) = a.site_at(t);
    for r in rows {
        r.site = site;
    }
    (site, geo)
}

/// T-132: each channel's dominant gain-state key over its own visits starting in `iv` (gain tables
/// are per band, so channels on different bands can differ within one interval).
fn channel_gain_keys(
    series: &BTreeMap<SubjectId, Vec<VisitSample>>,
    iv: TimeRange,
) -> BTreeMap<ChannelKey, u32> {
    let (s0, s1) = (iv.start.as_unix_nanos(), iv.end.as_unix_nanos());
    series
        .iter()
        .filter_map(|(id, visits)| match id {
            SubjectId::Channel(key) => Some((
                *key,
                engine::dominant_gain_key(
                    visits
                        .iter()
                        .filter(|s| s.start_ns >= s0 && s.start_ns < s1),
                ),
            )),
            SubjectId::Band(..) => None,
        })
        .collect()
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

/// `v` restricted to `chunk` when it starts there or crosses into it.
fn clip(v: &engine::Visit, chunk: TimeRange) -> Option<engine::Visit> {
    let (s, e) = (
        v.observed.start.as_unix_nanos(),
        v.observed.end.as_unix_nanos(),
    );
    let (c0, c1) = (chunk.start.as_unix_nanos(), chunk.end.as_unix_nanos());
    let starts = s >= c0 && s < c1;
    let crosses = s < c0 && e > c0;
    (starts || crosses).then(|| {
        let a = s.max(c0);
        engine::Visit {
            observed: TimeRange::new(ts(a), ts(e.min(c1).max(a))),
            ..*v
        }
    })
}

/// What [`evaluate_band`] reads.
struct BandJob<'a> {
    cfg: &'a OccupancyConfig,
    levels: &'a dyn LevelSource,
    obs: Option<&'a dyn ObservationSource>,
    dets: &'a [DetectionExtent],
    f_cell: f64,
    t_cell_ns: i64,
    /// Samples this call may add before it fails.
    max_samples: usize,
}

/// Evaluates `band` and its `channels` over `range` into `out`, reading the history in chunks of
/// at most `cfg.chunk_cells` cells that never cross a 15-min boundary (see the module notes).
/// Sets `unit` from the grids read. Fails once more than `max_samples` samples were added.
fn evaluate_band(
    job: &BandJob<'_>,
    band: FreqRange,
    band_id: SubjectId,
    channels: &[Channel],
    range: TimeRange,
    out: &mut BTreeMap<SubjectId, Vec<VisitSample>>,
    unit: &mut PowerUnit,
) -> Result<(), String> {
    let f_cell = job.f_cell;
    let mut subjects: Vec<(SubjectId, FreqRange, f64)> = vec![(band_id, band, f_cell)];
    subjects.extend(channels.iter().filter_map(|c| {
        let f = c.key.freq(f_cell);
        inside(f, band).then_some((SubjectId::Channel(c.key), f, c.obw_hz))
    }));
    // Logged visits starting in the range (small); the coverage mask when the log has none.
    let logged: Vec<Vec<engine::Visit>> = subjects
        .iter()
        .map(|(_, f, _)| job.obs.map(|o| o.visits(*f, range)).unwrap_or_default())
        .collect();
    let nf = ((band.width_hz() / f_cell).round() as usize).max(1);
    let t_cell = job.t_cell_ns.max(1);
    let step = ((job.cfg.chunk_cells / nf).max(1) as i64).saturating_mul(t_cell);
    let quarter = job.cfg.interval_ns.max(t_cell);
    let (s0, s1) = (range.start.as_unix_nanos(), range.end.as_unix_nanos());
    let mut added = 0usize;
    let mut a = s0;
    while a < s1 {
        let b = ((a.div_euclid(step) + 1) * step)
            .min((a.div_euclid(quarter) + 1) * quarter)
            .min(s1);
        let chunk = TimeRange::new(ts(a), ts(b));
        a = b;
        // The product lock is held inside this call only.
        let Some(grid) = job.levels.level0(band, chunk) else {
            continue;
        };
        *unit = grid.unit;
        let floors = job.cfg.engine.local_floors(&grid);
        for ((id, f, obw), log) in subjects.iter().zip(&logged) {
            let v: Vec<engine::Visit> = if log.is_empty() {
                engine::coverage_visits(&grid, *f, job.cfg.coverage_tier)
                    .into_iter()
                    .filter(|v| v.observed.start >= chunk.start && v.observed.start < chunk.end)
                    .collect()
            } else {
                log.iter().filter_map(|v| clip(v, chunk)).collect()
            };
            if v.is_empty() {
                continue;
            }
            let (s, _) = engine::evaluate(
                &job.cfg.engine.threshold,
                *f,
                *obw,
                EvalInput {
                    grid: &grid,
                    visits: &v,
                    detections: job.dets,
                    floors: &floors,
                },
            );
            added += s.len();
            out.entry(*id).or_default().extend(s);
        }
        if added > job.max_samples {
            return Err(format!(
                "span needs more than {} visit samples; narrow the band or the span",
                job.max_samples
            ));
        }
    }
    Ok(())
}

/// Band × span limits of `span_stats`.
fn check_span(freq: FreqRange, span: TimeRange) -> Result<(), String> {
    if span.duration_ns() <= 0 || span.duration_ns() > MAX_SPAN_NS {
        return Err("span must be positive and at most 7 days".into());
    }
    if freq.hi_hz.is_nan() || freq.hi_hz <= freq.lo_hz || freq.width_hz() > MAX_SPAN_WIDTH_HZ {
        return Err("band must be positive and at most 20 MHz".into());
    }
    if freq.width_hz() * span.duration_ns() as f64 * 1e-9 > MAX_SPAN_HZ_S {
        return Err("band × span must be at most 120 MHz·h (e.g. 20 MHz × 6 h)".into());
    }
    Ok(())
}

/// Rows of `interval` for every subject with samples in it (not observed is not quiet: no row).
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
    let (i0, i1) = (interval.start.as_unix_nanos(), interval.end.as_unix_nanos());
    let observed = |s: &[VisitSample]| {
        s.iter()
            .any(|v| (i0..i1).contains(&v.start_ns.saturating_add(v.dur_ns / 2)))
    };
    let series: Vec<(&SubjectId, &Vec<VisitSample>)> =
        series.iter().filter(|(_, s)| observed(s)).collect();
    let mut ch_rows = Vec::new();
    for (id, s) in &series {
        if let SubjectId::Channel(key) = id {
            let c = ctx(OccupancySubject::Channel { key: *key }, Some(obw(key)));
            ch_rows.extend(engine::stat(&cfg.engine, &c, interval, s, data_span, None));
        }
    }
    let mut rows = Vec::new();
    for (id, s) in &series {
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
        let (scheme, f_cell, t_cell_ns) = {
            let p = product.lock().unwrap_or_else(PoisonError::into_inner);
            let py = &p.config().pyramid;
            (
                py.scheme,
                py.f_cell_hz,
                i64::try_from(py.t_cell.as_nanos()).unwrap_or(1_000_000_000),
            )
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
                |p| {
                    ChannelPlan::from_channels(
                        scheme,
                        f_cell,
                        p.version,
                        &p.channels,
                        &p.evidence,
                        cfg.learn,
                    )
                },
            );
        stats.plan_version = plan.version();
        Arc::new(Self {
            cfg,
            product,
            db_path,
            counters,
            t_cell_ns,
            inner: Mutex::new(Inner {
                store,
                plan,
                series: BTreeMap::new(),
                next_close_ns: None,
                first_ns: None,
                det_cursor_ns: i64::MIN,
                sighted: std::collections::HashSet::new(),
                repo: None,
                unit: PowerUnit::Dbfs,
                stats,
                finished: false,
            }),
            observations: Mutex::new(None),
            store_log: Mutex::new(None),
            stop: AtomicBool::new(false),
            thread: Mutex::new(None),
            attention: Mutex::new(None),
            alarms: Mutex::new(None),
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

    /// Uses the run's T-115 observation log for visit timing, tiers and the bands to evaluate.
    pub fn set_observation_store(&self, store: ObservationStore) {
        *self
            .store_log
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(store);
    }

    fn log(&self) -> Option<ObservationStore> {
        self.store_log
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Records pushed directly (tests, or a source without the store): once any arrives, visits
    /// come from these instead of the store log or the coverage mask.
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

    /// T-128: feeds each close to `attention` (baselines, first sightings, candidates).
    pub fn set_attention(&self, attention: Option<Arc<crate::attention::AttentionService>>) {
        *self
            .attention
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = attention;
    }

    fn attention(&self) -> Option<Arc<crate::attention::AttentionService>> {
        self.attention
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// T-131: feeds each close's baseline folds to `alarms` (with the device's provenance steps
    /// and site/feed context). `feeds_dir` is the context-feed cache (staleness for correlation).
    pub fn set_alarms(
        &self,
        alarms: Option<Arc<crate::alarms::AlarmService>>,
        feeds_dir: Option<&std::path::Path>,
    ) {
        *self.alarms.lock().unwrap_or_else(PoisonError::into_inner) =
            alarms.map(|service| AlarmSlot {
                service,
                feeds: feeds_dir.and_then(|d| {
                    hk_context::FeedCache::open(d)
                        .ok()
                        .map(|c| (hk_context::Correlator::default(), c))
                }),
            });
    }

    /// T-131 (ADR-0012 §6.3, §7.4): the front-end provenance steps in `[start − lookback, end]`
    /// from the history's tile provenance (gain steps carry their dB delta in the detail). The
    /// steps are front-end wide, so one level-0 column at `at` is read, keeping the product lock
    /// short.
    fn device_steps(
        &self,
        at: f64,
        f_cell: f64,
        span: TimeRange,
    ) -> Vec<hk_context::occupancy::alarm::DeviceStep> {
        let history = {
            let p = self.product.lock().unwrap_or_else(PoisonError::into_inner);
            p.uncalibrated_pyramid()
                .level0(FreqRange::new(at - 0.5 * f_cell, at + 0.5 * f_cell), span)
        };
        // The summary's sample-drop step is an aggregate count with no time of its own (stamped at
        // the query's first frame), so any gap in the looked-back span would "explain" the whole
        // interval. A drop removes observation (already out of the visit counts and `n_eff`); it
        // does not shift levels or occupancy, so only timestamped front-end steps are passed.
        history.map_or_else(Vec::new, |h| {
            hk_context::report::steps_from_summary(&h.provenance, span)
                .0
                .iter()
                .filter(|s| s.kind != hk_model::attention::report::ProvenanceStepKind::SampleDrop)
                .map(hk_context::occupancy::alarm::DeviceStep::from_report)
                .collect()
        })
    }

    /// T-131: steps the alarm service with one close's folds and (T-136) its new-emitter inputs
    /// at `site`.
    #[allow(clippy::too_many_arguments)]
    fn feed_alarms(
        &self,
        folds: &[crate::attention::IntervalFold],
        new_emitters: &[hk_context::occupancy::alarm::AlarmInput],
        site: SiteKey,
        rows: &[OccupancyStat],
        iv: TimeRange,
        f_cell: f64,
        geo: Option<hk_context::Site>,
    ) {
        if folds.is_empty() && new_emitters.is_empty() {
            return;
        }
        let guard = self.alarms.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(slot) = guard.as_ref() else { return };
        let states = slot
            .feeds
            .as_ref()
            .and_then(|(c, cache)| c.feed_states(Some(cache)).ok())
            .unwrap_or_default();
        slot.service.set_context(geo, states);
        let at = rows.iter().find_map(|r| match r.subject {
            OccupancySubject::Band { freq } => Some(freq.center_hz()),
            OccupancySubject::Channel { .. } => None,
        });
        let lookback_ns = (hk_context::occupancy::alarm::AlarmConfig::default()
            .provenance_lookback_s
            * 1e9) as i64;
        let steps = at.map_or_else(Vec::new, |at| {
            self.device_steps(
                at,
                f_cell,
                TimeRange::new(iv.start.saturating_add_nanos(-lookback_ns), iv.end),
            )
        });
        let service = Arc::clone(&slot.service);
        drop(guard);
        service.observe_interval_with_new_emitters(folds, site, iv.end, new_emitters, &steps);
    }

    /// Inventory emitters first seen inside `iv` within the observed bands of `rows`, each counted
    /// once, with their measured extents (T-136: for the new-emitter alarms), lowest first.
    fn first_sightings(
        &self,
        inner: &mut Inner,
        iv: TimeRange,
        rows: &[OccupancyStat],
    ) -> Vec<FirstSighting> {
        if inner.repo.is_none() {
            inner.repo = Repository::open(&self.db_path).ok();
        }
        let Some(repo) = inner.repo.as_ref() else {
            inner.stats.errors += 1;
            return Vec::new();
        };
        // The window reaches back one interval: an emitter first seen in the previous interval
        // but written to the inventory after its close counts here, once (the previous window's
        // ids are remembered).
        let window = TimeRange::new(iv.start.saturating_add_nanos(-iv.duration_ns()), iv.end);
        let mut seen = std::collections::HashMap::new();
        for r in rows {
            let OccupancySubject::Band { freq } = r.subject else {
                continue;
            };
            match repo.emitters_in_region(&Region::new(freq, window)) {
                Ok(es) => seen.extend(
                    es.iter()
                        .filter(|e| e.first_seen >= window.start && e.first_seen < window.end)
                        .map(|e| (e.id, e.freq())),
                ),
                Err(_) => inner.stats.errors += 1,
            }
        }
        let mut new: Vec<FirstSighting> = seen
            .iter()
            .filter(|(id, _)| !inner.sighted.contains(*id))
            .map(|(id, freq)| FirstSighting {
                emitter: *id,
                freq: *freq,
                t: iv.end,
            })
            .collect();
        new.sort_by(|a, b| a.freq.lo_hz.total_cmp(&b.freq.lo_hz));
        inner.sighted = seen.into_keys().collect();
        new
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
        let cursor = inner
            .det_cursor_ns
            .max(iv.start.as_unix_nanos() - self.cfg.horizon_ns);
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
                evidence: inner.plan.evidence(),
            };
            if let Some(s) = inner.store.as_mut()
                && s.save_plan(&stored).is_err()
            {
                inner.stats.errors += 1;
            }
            inner.stats.plan_version = inner.plan.version();
        }
        // 2–5. Every band observed in the interval, else the tuned band.
        {
            let mem = self
                .observations
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let log = self.log();
            let log = log.as_ref().map(StoreObservations);
            let obs = pick(mem.as_ref(), log.as_ref());
            let mut bands: BTreeMap<SubjectId, FreqRange> = obs
                .map(|o| o.bands(iv))
                .unwrap_or_default()
                .into_iter()
                .filter_map(|b| {
                    // Inward: an outward-snapped edge cell lies outside the analysed extent, so
                    // no coverage row of the band would be fully observed.
                    let lo = (b.lo_hz / f_cell - 1e-6).ceil() as i64;
                    let hi = (b.hi_hz / f_cell + 1e-6).floor() as i64;
                    (hi > lo).then(|| {
                        (
                            SubjectId::Band(lo, hi),
                            FreqRange::new(lo as f64 * f_cell, hi as f64 * f_cell),
                        )
                    })
                })
                .collect();
            if bands.is_empty() {
                let (center, rate) = self.counters.tune();
                if rate > 0.0 && center > 0.0 {
                    let (f, id) = snap_band(FreqRange::centered(center, 0.8 * rate), f_cell);
                    bands.insert(id, f);
                }
            }
            let levels = ProductLevels(&self.product);
            let job = BandJob {
                cfg: &self.cfg,
                levels: &levels,
                obs,
                dets: &dets,
                f_cell,
                t_cell_ns: self.t_cell_ns,
                max_samples: usize::MAX,
            };
            let mut series = std::mem::take(&mut inner.series);
            let mut unit = inner.unit;
            for (id, band) in bands {
                let channels = inner.plan.channels_in(band);
                // Unbounded samples: cannot fail.
                let _ = evaluate_band(&job, band, id, &channels, iv, &mut series, &mut unit);
            }
            inner.series = series;
            inner.unit = unit;
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
        // 6. Rows.
        let Some(first) = inner.first_ns else {
            inner.stats.intervals_closed += 1;
            return;
        };
        let data_span = TimeRange::new(ts(first.min(iv.start.as_unix_nanos())), iv.end);
        let channels = inner.plan.channels();
        let mut rows = rows_for(
            &self.cfg,
            &channels,
            &inner.series,
            iv,
            data_span,
            inner.unit,
            f_cell,
        );
        if hour {
            let e = iv.end.as_unix_nanos();
            let h = TimeRange::new(ts((e - 1).div_euclid(HOUR_NS) * HOUR_NS), iv.end);
            if h.duration_ns() > self.cfg.interval_ns {
                rows.extend(rows_for(
                    &self.cfg,
                    &channels,
                    &inner.series,
                    h,
                    data_span,
                    inner.unit,
                    f_cell,
                ));
            }
        }
        inner.stats.intervals_closed += 1;
        if rows.is_empty() {
            return;
        }
        // T-128: the interval's own rows (not the hour rollup) fold into the baselines, the
        // inventory's first sightings feed the new-emitter rate, and candidates are re-scored.
        // This thread never touches samples. T-132: each channel's gain-state key is the dominant
        // front-end gain state of its own visits (gains are per band).
        // T-131: rows carry the site assigned at the close (a pinned or fixed site accrues
        // baselines; unassigned/mobile never do), and the folds step the novelty alarms with the
        // history's provenance steps around the interval.
        // T-136: the close is the only live path that advances site state (history frames peek),
        // and its first sightings feed the new-emitter alarms as well as the rate.
        if let Some(a) = self.attention() {
            let (site, geo) = stamp_site(&a, &mut rows, iv.end);
            let own: Vec<OccupancyStat> =
                rows.iter().filter(|r| r.interval == iv).cloned().collect();
            let sightings = self.first_sightings(inner, iv, &own);
            a.note_first_sightings(&sightings);
            let gains = channel_gain_keys(&inner.series, iv);
            let folds = a.ingest_interval(&own, sightings.len() as u64, iv.end, gains);
            let new_emitters = a.new_emitter_inputs(site, iv.end);
            self.feed_alarms(&folds, &new_emitters, site, &own, iv, f_cell, geo);
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
    /// from the history (chunked reads, see the module notes) and the run's detections; no
    /// widening beyond `span`. Limits: band ≤ 20 MHz, span ≤ 7 days, band × span ≤
    /// [`MAX_SPAN_HZ_S`], at most [`MAX_SPAN_SAMPLES`] visit samples.
    pub fn span_stats(
        &self,
        freq: FreqRange,
        span: TimeRange,
    ) -> Result<Vec<OccupancyStat>, String> {
        self.span_stats_from(&ProductLevels(&self.product), freq, span)
    }

    fn span_stats_from(
        &self,
        levels: &dyn LevelSource,
        freq: FreqRange,
        span: TimeRange,
    ) -> Result<Vec<OccupancyStat>, String> {
        check_span(freq, span)?;
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
        let mem = self
            .observations
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let log = self.log();
        let log = log.as_ref().map(StoreObservations);
        let job = BandJob {
            cfg: &self.cfg,
            levels,
            obs: pick(mem.as_ref(), log.as_ref()),
            dets: &dets,
            f_cell,
            t_cell_ns: self.t_cell_ns,
            max_samples: MAX_SPAN_SAMPLES,
        };
        let mut series = BTreeMap::new();
        let mut unit = PowerUnit::Dbfs;
        evaluate_band(&job, band, band_id, &channels, span, &mut series, &mut unit)?;
        Ok(rows_for(
            &self.cfg, &channels, &series, span, span, unit, f_cell,
        ))
    }

    /// Counters.
    pub fn stats(&self) -> OccupancyServiceStats {
        self.lock().stats
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::time::Instant;

    use hk_store::CellStats;

    use super::*;

    const F_CELL: f64 = 6250.0;
    const T_CELL: i64 = 1_000_000_000;

    /// T-132 review: gains are per band, so two channels on bands with different gain states get
    /// different keys in the same interval (and a visit outside the interval does not count).
    #[test]
    fn occupancy_gain_key_is_per_subject_within_one_interval() {
        let visit = |start_s: i64, dur_s: i64, gain_key: u32| VisitSample {
            start_ns: start_s * T_CELL,
            dur_ns: dur_s * T_CELL,
            tier: Tier::BackgroundSweep,
            occupied: false,
            suspect: false,
            above_fraction: 0.0,
            threshold_db: -100.0,
            guard_clamped: false,
            level_db: -104.0,
            floor_db: -105.0,
            floor_source: hk_model::attention::occupancy::FloorSource::History,
            floor_suspect: false,
            gain_key,
        };
        let key = |lo| ChannelKey {
            scheme: 1,
            lo_cell: lo,
            hi_cell: lo + 4,
        };
        let (band_a, band_b) = (key(16_000), key(64_000));
        let mut series = BTreeMap::new();
        // Band A (LNA 16 dB): mostly key 0xA in the interval; a long 0xB visit before it.
        series.insert(
            SubjectId::Channel(band_a),
            vec![
                visit(0, 5000, 0xB),
                visit(1000, 1, 0xA),
                visit(1500, 1, 0xA),
            ],
        );
        // Band B (LNA 32 dB): key 0xB.
        series.insert(
            SubjectId::Channel(band_b),
            vec![visit(1100, 1, 0xB), visit(1600, 1, 0xB)],
        );
        series.insert(SubjectId::Band(0, 1), vec![visit(1100, 1, 0xC)]);
        let iv = TimeRange::new(
            Timestamp::from_unix_nanos(900 * T_CELL),
            Timestamp::from_unix_nanos(1800 * T_CELL),
        );
        let keys = channel_gain_keys(&series, iv);
        assert_eq!(keys.len(), 2, "channels only: {keys:?}");
        assert_eq!(keys[&band_a], 0xA);
        assert_eq!(keys[&band_b], 0xB);
        use crate::attention::SubjectGainKeys;
        assert_ne!(
            keys.gain_key(&OccupancySubject::Channel { key: band_a }),
            keys.gain_key(&OccupancySubject::Channel { key: band_b })
        );
        assert_eq!(keys.gain_key(&OccupancySubject::Channel { key: key(9) }), 0);
    }

    /// T-136 (T-133 review): history frames run ahead of the occupancy close. Past a fixed site's
    /// no-fix hold they only peek, so the close of the earlier interval still stamps its rows with
    /// the fixed site (not `unassigned`); only the close advances (and persists) site state.
    #[test]
    fn occupancy_close_behind_history_frames_keeps_the_fixed_site() {
        let a = AttentionService::in_memory().unwrap();
        let t = |s: f64| Timestamp::from_unix_nanos(1_800_000_000_000_000_000 + (s * 1e9) as i64);
        let fix = |s: f64| hk_context::occupancy::site::Fix {
            t: t(s),
            lat_deg: 51.5,
            lon_deg: 0.0,
            speed_m_s: Some(0.0),
        };
        a.on_fix(fix(0.0));
        let key = a.on_fix(fix(70.0));
        assert!(matches!(key, SiteKey::Site(_)), "a still fix founds a site");
        // History frames up to 330 s past the 600 s hold after the last in-site fix (70 s).
        assert_eq!(crate::history::frame_site(Some(&a), t(400.0)), key);
        for s in [700.0, 900.0, 1000.0] {
            assert_eq!(
                crate::history::frame_site(Some(&a), t(s)),
                SiteKey::Unassigned
            );
        }
        assert_eq!(
            crate::history::frame_site(None, t(400.0)),
            SiteKey::Unassigned
        );
        // The close of the interval ending at 600 s, behind those frames.
        let band = OccupancySubject::Band {
            freq: FreqRange::new(431e6, 433e6),
        };
        let mut rows = vec![crate::attention::tests::series_row(
            0,
            SiteKey::Unassigned,
            band,
            0.1,
        )];
        let (site, _) = stamp_site(&a, &mut rows, t(600.0));
        assert_eq!(site, key, "the close behind the frames sees the fixed site");
        assert!(rows.iter().all(|r| r.site == key));
        assert_eq!(a.current_site_json()["site"]["kind"], "site");
        // The next close past the hold is what expires it.
        assert_eq!(stamp_site(&a, &mut rows, t(900.0)).0, SiteKey::Unassigned);
        assert_eq!(a.current_site_json()["site"]["kind"], "unassigned");
    }

    #[test]
    fn occupancy_band_snapping_is_outward_and_stable() {
        let (b, id) = snap_band(FreqRange::new(433_300_001.0, 433_699_999.0), 6250.0);
        assert!(b.lo_hz <= 433_300_001.0 && b.hi_hz >= 433_699_999.0);
        let (_, id2) = snap_band(FreqRange::new(433_300_002.0, 433_699_998.0), 6250.0);
        assert_eq!(id, id2);
    }

    /// A history that synthesises a fully observed noise grid for any read, under `lock` (the
    /// product lock stand-in), recording the largest grid it built.
    struct SynthLevels {
        lock: Mutex<()>,
        max_cells: AtomicUsize,
        reads: AtomicUsize,
    }

    impl LevelSource for SynthLevels {
        fn level0(&self, freq: FreqRange, span: TimeRange) -> Option<RegionHistory> {
            let _held = self.lock.lock().unwrap();
            let f0 = (freq.lo_hz / F_CELL).floor() as i64;
            let nf = ((freq.hi_hz / F_CELL).ceil() as i64 - f0) as usize;
            let t0 = span.start.as_unix_nanos().div_euclid(T_CELL);
            let nt = ((span.end.as_unix_nanos() + T_CELL - 1).div_euclid(T_CELL) - t0) as usize;
            let cell = CellStats {
                max_db: -99.0,
                mean_db: -99.0,
                p_low_db: -100.0,
                p_high_db: -99.0,
                occupancy: 0.0,
                occupancy_max: 0.0,
                coverage: 1.0,
                floor_db: -100.0,
                frames: 10,
                level: 0,
            };
            self.max_cells.fetch_max(nt * nf, Ordering::SeqCst);
            self.reads.fetch_add(1, Ordering::SeqCst);
            Some(RegionHistory {
                scheme: 1,
                level: 0,
                unit: PowerUnit::Dbfs,
                f_cell_hz: F_CELL,
                f_first_cell: f0,
                nf,
                t_cell_ns: T_CELL,
                t_first_cell: t0,
                nt,
                percentiles: (0.1, 0.9),
                cells: vec![cell; nt * nf],
                provenance: Default::default(),
                tiles_read: 0,
                filter: None,
            })
        }
    }

    fn synth() -> SynthLevels {
        SynthLevels {
            lock: Mutex::new(()),
            max_cells: AtomicUsize::new(0),
            reads: AtomicUsize::new(0),
        }
    }

    fn run(
        levels: &SynthLevels,
        cfg: &OccupancyConfig,
        width_hz: f64,
        secs: i64,
        max_samples: usize,
    ) -> (Result<(), String>, BTreeMap<SubjectId, Vec<VisitSample>>) {
        let (band, id) = snap_band(FreqRange::new(100e6, 100e6 + width_hz), F_CELL);
        let t0 = 1_800_000_000_000_000_000_i64;
        let job = BandJob {
            cfg,
            levels,
            obs: None,
            dets: &[],
            f_cell: F_CELL,
            t_cell_ns: T_CELL,
            max_samples,
        };
        let mut out = BTreeMap::new();
        let mut unit = PowerUnit::Dbfs;
        let r = evaluate_band(
            &job,
            band,
            id,
            &[],
            TimeRange::new(ts(t0), ts(t0 + secs * T_CELL)),
            &mut out,
            &mut unit,
        );
        (r, out)
    }

    #[test]
    fn occupancy_span_reads_are_chunked_within_the_cell_budget_and_bounded() {
        // 20 MHz × 1 h in one read is 3200 × 3600 cells (~11.5 M, ~500 MB of CellStats).
        let cfg = OccupancyConfig {
            chunk_cells: 20_000,
            ..OccupancyConfig::default()
        };
        let levels = synth();
        let (r, out) = run(&levels, &cfg, 20e6, 3600, MAX_SPAN_SAMPLES);
        r.unwrap();
        let max = levels.max_cells.load(Ordering::SeqCst);
        let reads = levels.reads.load(Ordering::SeqCst);
        eprintln!("chunked: {reads} reads, largest {max} cells");
        assert!(max <= 20_000, "largest read {max} cells");
        assert!(reads >= 3600 / 6);
        // Every 1 s row of the band evaluated exactly once, idle against its local floor.
        let band = out.values().next().unwrap();
        assert_eq!(band.len(), 3600);
        assert!(band.iter().all(|v| !v.occupied
            && v.floor_source == hk_model::attention::occupancy::FloorSource::History));
        // Default budget: 15-min-bounded chunks of ≤ 1 M cells.
        let levels = synth();
        run(
            &levels,
            &OccupancyConfig::default(),
            20e6,
            1800,
            MAX_SPAN_SAMPLES,
        )
        .0
        .unwrap();
        assert!(levels.max_cells.load(Ordering::SeqCst) <= CHUNK_CELLS);
        // The documented peak: one chunk grid plus the sample cap.
        let grid_mb = (CHUNK_CELLS * std::mem::size_of::<CellStats>()) as f64 / 1e6;
        let samples_mb = (MAX_SPAN_SAMPLES * std::mem::size_of::<VisitSample>()) as f64 / 1e6;
        eprintln!("peak bound: grid {grid_mb:.0} MB + samples {samples_mb:.0} MB");
        assert!(grid_mb <= 64.0 && samples_mb <= 128.0);
        // Too many samples fails instead of growing.
        let levels = synth();
        let (r, _) = run(&levels, &cfg, 1e6, 600, 100);
        assert!(r.is_err());
        // Band × span limits.
        let span_h = |h: i64| TimeRange::new(ts(0), ts(h * HOUR_NS));
        assert!(check_span(FreqRange::new(0.0, 20e6), span_h(6)).is_ok());
        assert!(check_span(FreqRange::new(0.0, 20e6), span_h(7)).is_err());
        assert!(check_span(FreqRange::new(0.0, 1e6), span_h(120)).is_ok());
        assert!(check_span(FreqRange::new(0.0, 1e6), span_h(24 * 7 + 1)).is_err());
    }

    #[test]
    fn occupancy_span_query_never_starves_history_ingest() {
        // The history reader folds a frame under `try_lock` and queues it otherwise, dropping the
        // oldest beyond HISTORY_QUEUE_FRAMES (T-037b). Simulate 200 frames/s against a 20 MHz ×
        // 30 min span query at the default chunk budget: the queue never overflows.
        let levels = Arc::new(synth());
        let cfg = OccupancyConfig::default();
        let done = Arc::new(AtomicBool::new(false));
        let worker = {
            let (levels, done) = (Arc::clone(&levels), Arc::clone(&done));
            std::thread::spawn(move || {
                let r = run(&levels, &cfg, 20e6, 1800, MAX_SPAN_SAMPLES).0;
                done.store(true, Ordering::SeqCst);
                r
            })
        };
        let (mut queued, mut max_queued, mut folded) = (0usize, 0usize, 0u64);
        let mut longest = Duration::ZERO;
        let mut blocked_since: Option<Instant> = None;
        while !done.load(Ordering::SeqCst) {
            match levels.lock.try_lock() {
                Ok(_g) => {
                    folded += 1 + queued as u64;
                    queued = 0;
                    if let Some(s) = blocked_since.take() {
                        longest = longest.max(s.elapsed());
                    }
                }
                Err(_) => {
                    queued += 1;
                    max_queued = max_queued.max(queued);
                    blocked_since.get_or_insert_with(Instant::now);
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        worker.join().unwrap().unwrap();
        eprintln!(
            "ingest during span query: {folded} folded, max queue {max_queued}, longest hold {:.0} ms, {} reads",
            longest.as_secs_f64() * 1e3,
            levels.reads.load(Ordering::SeqCst)
        );
        assert!(max_queued < crate::history::HISTORY_QUEUE_FRAMES / 4);
        assert!(folded > 0);
    }
}
