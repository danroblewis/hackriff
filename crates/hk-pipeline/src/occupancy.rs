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
//! 3. each band's level-0 history grid is read in **chunks** of at most `chunk_cells` cells, each
//!    under the product lock for that read only, so the history reader's queue (T-037b, 600
//!    frames) drains between chunks. Chunks **tile each 15-min interval** (a whole sub-multiple of
//!    it, never crossing one), so which visits a chunk boundary splits depends on the interval and
//!    the cell budget alone and not on the width of the band queried (T-196);
//! 4. visits come from the observation log when one is fed ([`OccupancyService::record_observation`]
//!    or the T-115 store) or else from the coverage mask ([`hk_context::occupancy::engine::coverage_visits`],
//!    at `coverage_tier`: a parked, user-tuned device observes independently of activity); a visit
//!    crossing a chunk boundary is evaluated per chunk piece. Thresholds sit over each column's
//!    local floor, measured over the band **widened by `floor.radius_hz`**
//!    (`engine::local_floors_from`) so a column's neighbourhood reference comes from the spectrum
//!    around it rather than from the extent the caller queried (T-196);
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
//! **Memory bound of a span query:** two chunk grids (the band's, and the floor read over the band
//! widened by `floor.radius_hz`; each at most `CHUNK_CELLS` × `size_of::<CellStats>()`, ≈ 44 MB)
//! plus per-column scratch (a few × band cells × 8 B) plus the visit samples, capped at
//! `MAX_SPAN_SAMPLES` × `size_of::<VisitSample>()` (≈ 96 MB; a query over it fails rather than
//! grows). Band × span is limited to `MAX_SPAN_HZ_S` (20 MHz × 6 h), which keeps 1 s coverage
//! visits of a 20 MHz band and ~90 channels inside the sample cap. A 15-min close holds the same
//! pair of chunk grids at a time as well.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use hk_context::occupancy::channels::{self, ChannelPlan, DetectionExtent, LearnConfig};
use hk_context::occupancy::engine::{
    self, EngineConfig, EvalInput, LevelSource, MemoryObservations, ObservationSource,
    SubjectContext, VisitSample,
};
use hk_model::attention::ATTENTION_SCHEMA_VERSION;
use hk_model::attention::baseline::{ChainKey, SiteKey};
use hk_model::attention::observation::{ObservationRecord, Tier};
use hk_model::attention::occupancy::{Channel, ChannelKey, OccupancyStat, OccupancySubject};
use hk_model::frames::PowerUnit;
use hk_model::{FreqRange, Region, Repository, TimeRange, Timestamp};
use hk_store::history::OriginFilter;
use hk_store::observation::{ObservationStore, RecordQuery};
use hk_store::occupancy::{
    OccupancyQuery, OccupancyRows, OccupancyStore, OccupancyStoreConfig, StoredChannelPlan,
};
use hk_store::{FloorProduct, RegionHistory};
use serde::Serialize;

use crate::attention::{AttentionService, EmitterSeen, FirstSighting, ObservedBand};
use crate::stats::Counters;
use hk_model::emitter::Emitter;

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
    /// T-314: level-0 cells excluded from the reads behind these rows because their tile pools
    /// this run's receive chain with another front end's. Such a cell is reported unobserved,
    /// never averaged and never attributed to whichever chain contributed most, so a nonzero
    /// count is coverage this chain does not have rather than a measurement it got wrong.
    pub cells_excluded: u64,
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
    /// T-172: Provenance id → tuning centre, Hz, for the DC-twin rule.
    tune_centres: std::collections::HashMap<hk_model::ids::ProvenanceId, f64>,
    unit: PowerUnit,
    stats: OccupancyServiceStats,
    finished: bool,
}

/// Provenances whose tuning centre is remembered before the cache is dropped (one per distinct
/// tune/gain state).
const TUNE_CENTRE_CACHE_MAX: usize = 65_536;

/// Level-0 history under the product lock, held for one read only, restricted to the run's own
/// receive chain (T-314: `filter`, from [`engine::chain_filter`]).
///
/// The excluded-cell count of each read is added to `excluded`, so a chain whose history is
/// pooled with another front end's says so in [`OccupancyServiceStats::cells_excluded`] rather
/// than going quietly empty.
struct ProductLevels<'a> {
    product: &'a Mutex<FloorProduct>,
    filter: OriginFilter,
    excluded: &'a AtomicU64,
}

impl LevelSource for ProductLevels<'_> {
    fn level0(&self, freq: FreqRange, span: TimeRange) -> Option<RegionHistory> {
        let p = self.product.lock().unwrap_or_else(PoisonError::into_inner);
        let h = engine::OriginLevels {
            pyramid: p.uncalibrated_pyramid(),
            filter: self.filter,
        }
        .level0(freq, span)?;
        if let Some(f) = &h.filter {
            self.excluded
                .fetch_add(f.cells_excluded as u64, Ordering::Relaxed);
        }
        Some(h)
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
    /// T-314: the origins this service's history reads may contain — the run's own receive chain,
    /// the same front end [`crate::attention::AttentionService`] keys its baselines by (T-303).
    /// Without it the key would be per-chain while the measurement behind it was the average of
    /// every front end folded into the pyramid.
    origin: OriginFilter,
    /// Level-0 cells a filtered read returned as unobserved because another front end's frames
    /// are folded into them (T-314). Nonzero means this chain's history is genuinely pooled with
    /// another's in those tiles, and the cells were **excluded** rather than attributed to the
    /// origin that contributed most.
    cells_excluded: AtomicU64,
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
    // T-196: floors are measured over the band widened by the local-floor radius, so a channel's
    // threshold comes from the spectrum around it and not from the extent the caller asked for.
    let radius = job.cfg.engine.floor.radius_hz.max(0.0);
    let floor_band = FreqRange::new(band.lo_hz - radius, band.hi_hz + radius);
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
    // The widened floor read is the larger of the two grids a chunk holds: size the chunk by it.
    let nf = ((floor_band.width_hz() / f_cell).round() as usize).max(1);
    let t_cell = job.t_cell_ns.max(1);
    let quarter = job.cfg.interval_ns.max(t_cell);
    // T-196: chunks tile the 15-min interval in `k` equal pieces, the fewest that fit the cell
    // budget. Phasing them on the interval rather than on the epoch, and quantising to a whole
    // sub-multiple of it, keeps the chunk grid (and so which visits a boundary splits into two
    // samples) from depending on the band's width.
    let ceil_div = |a: i64, b: i64| (a + b.max(1) - 1) / b.max(1);
    let quarter_cells = ceil_div(quarter, t_cell).max(1);
    let budget_cells = (job.cfg.chunk_cells / nf).max(1) as i64;
    let k = ceil_div(quarter_cells, budget_cells).max(1);
    let step = ceil_div(quarter_cells, k).saturating_mul(t_cell);
    let (s0, s1) = (range.start.as_unix_nanos(), range.end.as_unix_nanos());
    let mut added = 0usize;
    let mut a = s0;
    while a < s1 {
        let q0 = a.div_euclid(quarter) * quarter;
        let b = (q0 + ((a - q0).div_euclid(step) + 1) * step)
            .min(q0 + quarter)
            .min(s1);
        let chunk = TimeRange::new(ts(a), ts(b));
        a = b;
        // The product lock is held inside this call only.
        let Some(grid) = job.levels.level0(band, chunk) else {
            continue;
        };
        *unit = grid.unit;
        let floors = match job.levels.level0(floor_band, chunk) {
            Some(wide) => job.cfg.engine.local_floors_from(&grid, &wide),
            None => job.cfg.engine.local_floors(&grid),
        };
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

/// T-138: the observed bands of `rows` (with their bin widths) and the close's inventory
/// `emitters` ([`OccupancyService::inventory_at_close`]) seen in `iv`: first..last seen overlapping
/// the interval (a carrier still on has `last_seen` past `iv.end` by the time the close runs). An
/// emitter is suspect when the interval's detections over its extent are all suspect (§2.6); with
/// none read (inventory lag) it is not. No inventory query.
fn emitters_seen(
    iv: TimeRange,
    rows: &[OccupancyStat],
    dets: &[DetectionExtent],
    emitters: &[Emitter],
) -> (Vec<ObservedBand>, Vec<EmitterSeen>) {
    let observed = rows
        .iter()
        .filter_map(|r| match r.subject {
            OccupancySubject::Band { freq } => Some(ObservedBand {
                freq,
                rbw_hz: r.rbw_hz,
            }),
            OccupancySubject::Channel { .. } => None,
        })
        .collect();
    let seen = emitters
        .iter()
        .filter(|e| e.first_seen <= iv.end && e.last_seen >= iv.start)
        .map(|e| {
            let freq = e.freq();
            let mut over = dets.iter().filter(|d| {
                d.freq.lo_hz < freq.hi_hz
                    && d.freq.hi_hz > freq.lo_hz
                    && d.time.end >= iv.start
                    && d.time.start <= iv.end
            });
            let first = over.next();
            EmitterSeen {
                emitter: e.id,
                freq,
                first_seen: e.first_seen,
                last_seen: e.last_seen,
                suspect: first.is_some_and(|d| d.suspect) && over.all(|d| d.suspect),
            }
        })
        .collect();
    (observed, seen)
}

impl OccupancyService {
    /// Opens the series store under `dir` and restores the saved plan.
    pub fn open(
        dir: PathBuf,
        product: Arc<Mutex<FloorProduct>>,
        db_path: PathBuf,
        counters: Arc<Counters>,
        cfg: OccupancyConfig,
        chain: ChainKey,
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
                tune_centres: std::collections::HashMap::new(),
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
            origin: engine::chain_filter(chain),
            cells_excluded: AtomicU64::new(0),
        })
    }

    /// T-314: the level source of every occupancy read — the pyramid restricted to this run's
    /// own receive chain.
    fn levels(&self) -> ProductLevels<'_> {
        ProductLevels {
            product: &self.product,
            filter: self.origin,
            excluded: &self.cells_excluded,
        }
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
        // T-314: restricted to this run's own chain like every other occupancy read. A mixed
        // tile's steps are not returned, which costs nothing: its cells are excluded from the
        // measurement there is nothing left to explain.
        let history = self
            .levels()
            .level0(FreqRange::new(at - 0.5 * f_cell, at + 0.5 * f_cell), span);
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

    /// T-166: offers one close's first sightings to the armed region watches.
    fn note_watches(&self, sightings: &[FirstSighting], t: hk_model::Timestamp) {
        if sightings.is_empty() {
            return;
        }
        let guard = self.alarms.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(slot) = guard.as_ref() else { return };
        slot.service.observe_watches(sightings, t);
    }

    /// T-131: steps the alarm service with one close's folds and (T-136) its new-emitter inputs
    /// at `site`.
    #[allow(clippy::too_many_arguments)]
    fn feed_alarms(
        &self,
        folds: &[crate::attention::IntervalFold],
        new_emitters: &[crate::attention::NewEmitterInput],
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

    /// T-138 review: the close's one inventory read. Emitters seen (first..last overlapping) in
    /// the close's look-back window (the previous interval and this one) over the observed bands
    /// of `rows`: a single query over the bands' hull, filtered to the bands in memory.
    fn inventory_at_close(
        &self,
        inner: &mut Inner,
        iv: TimeRange,
        rows: &[OccupancyStat],
    ) -> Vec<Emitter> {
        let bands: Vec<FreqRange> = rows
            .iter()
            .filter_map(|r| match r.subject {
                OccupancySubject::Band { freq } => Some(freq),
                OccupancySubject::Channel { .. } => None,
            })
            .collect();
        let Some(hull) = bands
            .iter()
            .copied()
            .reduce(|a, b| FreqRange::new(a.lo_hz.min(b.lo_hz), a.hi_hz.max(b.hi_hz)))
        else {
            return Vec::new();
        };
        if inner.repo.is_none() {
            inner.repo = Repository::open(&self.db_path).ok();
        }
        let Some(repo) = inner.repo.as_ref() else {
            inner.stats.errors += 1;
            return Vec::new();
        };
        let window = TimeRange::new(iv.start.saturating_add_nanos(-iv.duration_ns()), iv.end);
        match repo.emitters_in_region(&Region::new(hull, window)) {
            Ok(es) => es
                .into_iter()
                .filter(|e| {
                    let f = e.freq();
                    bands
                        .iter()
                        .any(|b| f.lo_hz <= b.hi_hz && f.hi_hz >= b.lo_hz)
                })
                .collect(),
            Err(_) => {
                inner.stats.errors += 1;
                Vec::new()
            }
        }
    }

    /// T-138: the new sightings among `sightings` whose extent overlaps another inventory emitter
    /// first seen before them and last seen within [`PERSIST_HORIZON_S`] of their first sighting
    /// (a track split or drift re-identified as new). One query over the sightings' hull; the
    /// close runs it only on a sighting close while the persistent rule is active.
    fn churned(
        &self,
        inner: &mut Inner,
        iv: TimeRange,
        sightings: &[FirstSighting],
        emitters: &[Emitter],
    ) -> Vec<hk_model::ids::EmitterId> {
        let Some(hull) = sightings
            .iter()
            .map(|s| s.freq)
            .reduce(|a, b| FreqRange::new(a.lo_hz.min(b.lo_hz), a.hi_hz.max(b.hi_hz)))
        else {
            return Vec::new();
        };
        let Some(repo) = inner.repo.as_ref() else {
            return Vec::new();
        };
        let horizon_ns = (crate::attention::PERSIST_HORIZON_S * 1e9) as i64;
        let span = TimeRange::new(iv.start.saturating_add_nanos(-horizon_ns), iv.end);
        let known = match repo.emitters_in_region(&Region::new(hull, span)) {
            Ok(k) => k,
            Err(_) => {
                inner.stats.errors += 1;
                return Vec::new();
            }
        };
        sightings
            .iter()
            .filter(|s| {
                let Some(first) = emitters
                    .iter()
                    .find(|e| e.id == s.emitter)
                    .map(|e| e.first_seen)
                else {
                    return false;
                };
                let since = first.saturating_add_nanos(-horizon_ns);
                known.iter().any(|k| {
                    let f = k.freq();
                    k.id != s.emitter
                        && k.first_seen < first
                        && k.last_seen >= since
                        && f.lo_hz < s.freq.hi_hz
                        && f.hi_hz > s.freq.lo_hz
                })
            })
            .map(|s| s.emitter)
            .collect()
    }

    /// Inventory emitters first seen inside `iv` (or the previous interval, see below) among the
    /// close's `emitters` ([`Self::inventory_at_close`]), each counted once, with their measured
    /// extents (T-136: for the new-emitter alarms), lowest first.
    fn first_sightings(
        &self,
        inner: &mut Inner,
        iv: TimeRange,
        emitters: &[Emitter],
    ) -> Vec<FirstSighting> {
        // The window reaches back one interval: an emitter first seen in the previous interval
        // but written to the inventory after its close counts here, once (the previous window's
        // ids are remembered).
        let window = TimeRange::new(iv.start.saturating_add_nanos(-iv.duration_ns()), iv.end);
        let seen: std::collections::HashMap<_, _> = emitters
            .iter()
            .filter(|e| e.first_seen >= window.start && e.first_seen < window.end)
            .map(|e| (e.id, e.freq()))
            .collect();
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
        // T-147: read a DC-twin slack (one time cell, T-172) either side so a DC flag near the
        // span's edge can be refuted, then keep the extents overlapping `span`.
        let rule = channels::DcTwinRule {
            f_cell_hz: inner.plan.f_cell_hz(),
            slack_ns: self.t_cell_ns.max(1),
            lo_tolerance_hz: hk_detect::DcRule::default().tolerance_hz,
        };
        let wide = TimeRange::new(
            span.start.saturating_add_nanos(-rule.slack_ns),
            span.end.saturating_add_nanos(rule.slack_ns),
        );
        match repo.detections_in_region(&Region::new(FreqRange::new(0.0, 1e12), wide)) {
            Ok(d) => {
                // T-172: a twin's own tuning centre, from its Provenance (content-addressed, so
                // immutable and cached per id; failed reads are not cached).
                let cache = &mut inner.tune_centres;
                if cache.len() > TUNE_CENTRE_CACHE_MAX {
                    cache.clear();
                }
                channels::extents_of(&d, rule, |det| {
                    let id = det.provenance_ref;
                    if let Some(&c) = cache.get(&id) {
                        return Some(c);
                    }
                    let c = repo.provenance(id).ok()?.tune.center_hz;
                    cache.insert(id, c);
                    Some(c)
                })
                .into_iter()
                .filter(|e| e.time.end >= span.start && e.time.start <= span.end)
                .collect()
            }
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
            let levels = self.levels();
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
            // T-138: one inventory read serves the first sightings and what the interval saw.
            let emitters = self.inventory_at_close(inner, iv, &own);
            let sightings = self.first_sightings(inner, iv, &emitters);
            a.note_first_sightings(site, &sightings);
            // T-166: the armed region watches see the same first sightings, filtered by the
            // T-219 relationships in force on each row (an image, harmonic, intermod or
            // duplicate of a confirmed source is not new activity).
            self.note_watches(&sightings, iv.end);
            if a.has_pending_sightings(site) {
                // The churn look-back runs only on a sighting close while the rule can score.
                let churned = if sightings.is_empty() || !a.persistent_rule_active(site) {
                    Vec::new()
                } else {
                    self.churned(inner, iv, &sightings, &emitters)
                };
                let (observed, seen) = emitters_seen(iv, &own, &dets, &emitters);
                a.note_emitters_seen(site, iv, &observed, &seen, &churned);
            }
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
        self.span_stats_from(&self.levels(), freq, span)
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
        OccupancyServiceStats {
            cells_excluded: self.cells_excluded.load(Ordering::Relaxed),
            ..self.lock().stats
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::time::Instant;

    use hk_store::CellStats;
    use hk_store::history::OriginField;

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
            bias_tee: hk_model::BiasTee::Unknown,
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

    /// T-138 review: a close's inventory read (one query) against a real repository whose
    /// inventory was updated past the interval's end. The carrier still on is a first sighting
    /// and seen (overlap, not `last_seen <= iv.end`); an emitter whose detections are all suspect
    /// is suspect; one last seen before the look-back is neither; the carrier's new id over the
    /// same channel's emitter seen yesterday is churn.
    #[test]
    fn occupancy_close_inventory_seen_overlap_and_churn() {
        use hk_model::emitter::EmitterObservation;
        use hk_model::ids::EmitterId;
        let dir = std::env::temp_dir().join(format!("hk-t138-occ-{}", EmitterId::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("hk.db");
        let product = hk_store::FloorProduct::open(
            dir.join("history"),
            hk_store::FloorProductConfig::default(),
            hk_dsp::radiometry::PowerCalibrations::from_states(&[], None),
        )
        .unwrap();
        let svc = OccupancyService::open(
            dir.join("occupancy"),
            Arc::new(Mutex::new(product)),
            db.clone(),
            Arc::new(Counters::default()),
            OccupancyConfig::default(),
            ChainKey::Unknown,
        );
        let band = FreqRange::new(431e6, 433e6);
        let site = SiteKey::Site(hk_model::ids::SiteId::new());
        let rows = vec![crate::attention::tests::series_row(
            1,
            site,
            OccupancySubject::Band { freq: band },
            0.1,
        )];
        let iv = rows[0].interval;
        let at = |s: i64| iv.start.saturating_add_nanos(s * 1_000_000_000);
        let (old, carrier, spur, gone) = (
            EmitterId::new(),
            EmitterId::new(),
            EmitterId::new(),
            EmitterId::new(),
        );
        {
            let mut repo = Repository::open(&db).unwrap();
            let mut observe = |id, from: i64, to: i64, f: f64| {
                repo.upsert_emitter_observation(&EmitterObservation {
                    emitter_id: id,
                    seen: TimeRange::new(at(from), at(to)),
                    count: 1,
                    f_center_hz: f,
                    bandwidth_hz: 10e3,
                    identity: None,
                })
                .unwrap();
            };
            observe(old, -3 * 86_400, -86_400, 432.00625e6);
            observe(carrier, 600, 800, 432.00625e6);
            // Still on: the inventory is updated past the interval end (900 s) before the close.
            observe(carrier, 800, 960, 432.00625e6);
            observe(spur, 100, 200, 431.5e6);
            observe(gone, -2000, -1900, 431.7e6);
        }
        let det = |from: i64, to: i64, f: f64, suspect| DetectionExtent {
            time: TimeRange::new(at(from), at(to)),
            freq: FreqRange::centered(f, 10e3),
            obw_hz: 10e3,
            snr_db: 20.0,
            suspect,
        };
        let dets = vec![
            det(600, 960, 432.00625e6, false),
            det(100, 200, 431.5e6, true),
        ];
        let mut inner = svc.lock();
        let emitters = svc.inventory_at_close(&mut inner, iv, &rows);
        let sightings = svc.first_sightings(&mut inner, iv, &emitters);
        assert_eq!(sightings.len(), 2, "{sightings:?}");
        assert!(
            sightings
                .iter()
                .all(|s| s.emitter == carrier || s.emitter == spur)
        );
        let (observed, seen) = emitters_seen(iv, &rows, &dets, &emitters);
        assert_eq!(
            observed,
            vec![ObservedBand {
                freq: band,
                rbw_hz: 6250.0
            }]
        );
        let c = seen
            .iter()
            .find(|e| e.emitter == carrier)
            .expect("the carrier updated past the interval end is seen");
        assert!(c.last_seen > iv.end && !c.suspect, "{c:?}");
        assert!(seen.iter().find(|e| e.emitter == spur).unwrap().suspect);
        assert!(!seen.iter().any(|e| e.emitter == old || e.emitter == gone));
        assert_eq!(
            svc.churned(&mut inner, iv, &sightings, &emitters),
            vec![carrier],
            "a new id over the channel's emitter seen yesterday"
        );
        drop(inner);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn occupancy_band_snapping_is_outward_and_stable() {
        let (b, id) = snap_band(FreqRange::new(433_300_001.0, 433_699_999.0), 6250.0);
        assert!(b.lo_hz <= 433_300_001.0 && b.hi_hz >= 433_699_999.0);
        let (_, id2) = snap_band(FreqRange::new(433_300_002.0, 433_699_998.0), 6250.0);
        assert_eq!(id, id2);
    }

    // ---- T-196: the same channel must report the same FCO from any query band containing it. ----

    /// Recorded extent of the T-196 scene, level-0 cells (433.25–433.75 MHz at 6.25 kHz).
    const T196_RECORDED: (i64, i64) = (69_320, 69_400);
    /// The channel under test, cells (433.4625–433.4875 MHz).
    const T196_CH: (i64, i64) = (69_354, 69_358);
    /// Span start, on a 15-min boundary.
    const T196_T0: i64 = 1_800_000_000_000_000_000;
    /// Span, s (two 15-min intervals).
    const T196_SECS: i64 = 1800;

    fn t196_cell(mean: f64, floor: f64, occ: f32) -> CellStats {
        CellStats {
            max_db: mean as f32,
            mean_db: mean as f32,
            p_low_db: floor as f32,
            p_high_db: mean as f32,
            occupancy: occ,
            occupancy_max: occ,
            coverage: 1.0,
            floor_db: floor as f32,
            frames: 10,
            level: 0,
        }
    }

    /// The T-196 scene: noise over the recorded extent, nothing outside it, and one channel
    /// continuously on for the second half of the span and idle for the first (realized FCO 0.5).
    /// A continuously-on channel's own cells carry its signal as their floor, which is what the
    /// history stores and what makes its own-column floor useless.
    struct SceneLevels;

    impl LevelSource for SceneLevels {
        fn level0(&self, freq: FreqRange, span: TimeRange) -> Option<RegionHistory> {
            let f0 = (freq.lo_hz / F_CELL).floor() as i64;
            let nf = ((freq.hi_hz / F_CELL).ceil() as i64 - f0) as usize;
            let t0 = span.start.as_unix_nanos().div_euclid(T_CELL);
            let nt = ((span.end.as_unix_nanos() + T_CELL - 1).div_euclid(T_CELL) - t0) as usize;
            let half = T196_T0 + T196_SECS / 2 * T_CELL;
            let mut cells = Vec::with_capacity(nt * nf);
            for t in 0..nt {
                let busy_row = (t0 + t as i64) * T_CELL >= half;
                for f in 0..nf {
                    let c = f0 + f as i64;
                    cells.push(if c < T196_RECORDED.0 || c >= T196_RECORDED.1 {
                        // Never observed: outside the recording.
                        CellStats {
                            frames: 0,
                            ..t196_cell(f64::NAN, f64::NAN, 0.0)
                        }
                    } else if (T196_CH.0..T196_CH.1).contains(&c) && busy_row {
                        t196_cell(-60.0, -60.0, 1.0)
                    } else {
                        t196_cell(-99.0, -100.0, 0.0)
                    });
                }
            }
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
                cells,
                provenance: Default::default(),
                tiles_read: 0,
                filter: None,
            })
        }
    }

    /// A fixed observation log: 60 s activity-independent dwells over the recorded extent.
    fn t196_log() -> MemoryObservations {
        use hk_model::attention::observation::{DwellRecord, ObservedWindow, Reason};
        let mut m = MemoryObservations::default();
        for i in 0..(T196_SECS / 60) {
            let iv = TimeRange::new(
                ts(T196_T0 + i * 60 * T_CELL),
                ts(T196_T0 + (i + 1) * 60 * T_CELL),
            );
            m.push(ObservationRecord::Dwell(DwellRecord {
                schema: ATTENTION_SCHEMA_VERSION,
                survey_id: None,
                seq: i as u64,
                plan_version: 1,
                site: SiteKey::Unassigned,
                device_id: None,
                reason: Reason::PoiDwell { poi: 1 },
                tier: Tier::ScheduledPlan,
                window: ObservedWindow {
                    center_hz: 433.5e6,
                    sample_rate_hz: 500e3,
                    usable: FreqRange::new(
                        T196_RECORDED.0 as f64 * F_CELL,
                        T196_RECORDED.1 as f64 * F_CELL,
                    ),
                    dc_excluded: None,
                    rbw_hz: F_CELL,
                },
                rf_path: 0,
                planned: iv,
                observed: iv,
                preempted: false,
                dropped_samples: 0,
                overload: false,
                provenance_ref: None,
            }));
        }
        m
    }

    /// The channel's estimate over the whole span when queried inside `band`.
    fn t196_estimate(band: FreqRange) -> engine::WindowEstimate {
        let cfg = OccupancyConfig::default();
        let key = ChannelKey {
            scheme: 1,
            lo_cell: T196_CH.0,
            hi_cell: T196_CH.1,
        };
        let ch = Channel {
            key,
            source: hk_model::attention::occupancy::ChannelSource::Learned,
            plan_version: 1,
            first_learned: ts(T196_T0),
            evidence: 10,
            obw_hz: (T196_CH.1 - T196_CH.0) as f64 * F_CELL,
            raster_hint: None,
        };
        let (band, band_id) = snap_band(band, F_CELL);
        assert!(
            inside(key.freq(F_CELL), band),
            "channel must be in the band"
        );
        let log = t196_log();
        let job = BandJob {
            cfg: &cfg,
            levels: &SceneLevels,
            obs: Some(&log),
            dets: &[],
            f_cell: F_CELL,
            t_cell_ns: T_CELL,
            max_samples: MAX_SPAN_SAMPLES,
        };
        let range = TimeRange::new(ts(T196_T0), ts(T196_T0 + T196_SECS * T_CELL));
        let mut out = BTreeMap::new();
        let mut unit = PowerUnit::Dbfs;
        evaluate_band(&job, band, band_id, &[ch], range, &mut out, &mut unit).unwrap();
        let s = out.remove(&SubjectId::Channel(key)).unwrap_or_default();
        engine::estimate(&s, range, cfg.engine.level)
    }

    fn t196_show(name: &str, band: FreqRange, e: &engine::WindowEstimate) {
        eprintln!(
            "[T-196] {name} band {:.4}-{:.4} MHz ({} cells): fco {:?} = occupied {} / clean {} \
             (all {}, suspect {}) threshold {:?} floor {:?} source {:?} ci {:?}",
            band.lo_hz / 1e6,
            band.hi_hz / 1e6,
            (band.width_hz() / F_CELL).round() as i64,
            e.fco,
            e.n_occupied,
            e.n_revisits - e.n_suspect,
            e.n_revisits_all,
            e.n_suspect,
            e.threshold_db,
            e.floor_db,
            e.floor_source,
            e.confidence,
        );
    }

    /// T-196: a channel fully inside two different query bands reports the same FCO and the same
    /// confidence interval from both. The floor a channel is measured against is a property of the
    /// spectrum around it, not of the extent the caller happened to ask for.
    #[test]
    fn occupancy_channel_fco_does_not_depend_on_the_query_band() {
        // Both bands contain the channel (cells 69_354..69_358) whole.
        let narrow = FreqRange::new(69_322_f64 * F_CELL, 69_394_f64 * F_CELL);
        let wide = FreqRange::new(
            T196_RECORDED.0 as f64 * F_CELL,
            T196_RECORDED.1 as f64 * F_CELL,
        );
        let (a, b) = (t196_estimate(narrow), t196_estimate(wide));
        t196_show("narrow", narrow, &a);
        t196_show("wide  ", wide, &b);
        // The scene: on for the second half of the span, idle for the first.
        assert_eq!((a.fco, a.confidence), (b.fco, b.confidence));
        assert!(
            a.fco.is_some_and(|f| (f - 0.5).abs() < 0.05),
            "realized FCO is 0.5, measured {:?}",
            a.fco
        );
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

    // T-359: the bias tee reaches the occupancy row.

    /// 6.25 kHz × 1 s level-0 cells, 16 per tile: history scheme 1's geometry, in miniature.
    fn t359_pyramid(dir: &std::path::Path) -> hk_store::Pyramid {
        hk_store::Pyramid::open(
            dir,
            hk_store::PyramidConfig {
                scheme: 1,
                f_cell_hz: F_CELL,
                t_cell: Duration::from_secs(1),
                f_cells_per_block: 16,
                levels: vec![hk_store::LevelConfig {
                    f_factor: 1,
                    t_cells_per_block: 16,
                    max_age: None,
                    byte_quota: None,
                }],
                seal_lag: Duration::ZERO,
                checkpoint_interval: None,
                byte_budget: u64::MAX,
                ..hk_store::PyramidConfig::default()
            },
        )
        .unwrap()
    }

    const T359_T0: i64 = 1_789_300_800 * T_CELL; // 2026-09-13T12:00:00Z
    const T359_SECS: i64 = 240;
    /// 100.000–100.100 MHz.
    const T359_LO_CELL: i64 = 16_000;

    /// A run of [`T359_SECS`] one-second frames over 16 cells from 100 MHz, the antenna-port bias
    /// tee in state `bias(second)` and **nothing else moving**, ingested into a real pyramid,
    /// sealed and reopened (so the state round-trips the tile format), then read back through the
    /// production occupancy path: `evaluate_band` → `VisitSample`s → `rows_for` → `OccupancyStat`.
    fn t359_rows(tag: &str, bias: impl Fn(i64) -> hk_model::BiasTee) -> Vec<OccupancyStat> {
        let dir =
            std::env::temp_dir().join(format!("hk-t359-{tag}-{}", hk_model::ids::SiteId::new()));
        std::fs::create_dir_all(&dir).unwrap();
        {
            let mut p = t359_pyramid(&dir);
            // −100 dBFS/Hz noise with a carrier in the first channel, so rows have levels as well
            // as occupancy. The gain state, calibration, port and mask are identical throughout: a
            // bias-only change is invisible to every other part of the front-end state (T-331).
            let mut psd = vec![10f32.powf(-10.0); 16];
            psd[1] = 10f32.powf(-10.0) * 1000.0;
            let gain = hk_store::GainState {
                lna_db: 16.0,
                vga_db: 20.0,
                amp_on: false,
            };
            for k in 0..T359_SECS {
                let mut f = hk_store::FrameInput::new(
                    ts(T359_T0 + k * T_CELL),
                    T_CELL,
                    T359_LO_CELL as f64 * F_CELL,
                    F_CELL,
                    PowerUnit::Dbfs,
                    &psd,
                );
                f.gain = Some(gain);
                f.bias_tee = bias(k);
                p.ingest(&f).unwrap();
            }
            p.seal_through(ts(T359_T0 + T359_SECS * T_CELL)).unwrap();
        }
        let p = t359_pyramid(&dir);
        let cfg = OccupancyConfig::default();
        let key = ChannelKey {
            scheme: 1,
            lo_cell: T359_LO_CELL,
            hi_cell: T359_LO_CELL + 4,
        };
        // The learned plan is not what this test is about: one channel, stated.
        let channels = vec![Channel {
            key,
            source: hk_model::attention::occupancy::ChannelSource::Learned,
            plan_version: 1,
            first_learned: ts(T359_T0),
            evidence: 1,
            obw_hz: key.freq(F_CELL).width_hz(),
            raster_hint: None,
        }];
        let span = TimeRange::new(ts(T359_T0), ts(T359_T0 + T359_SECS * T_CELL));
        let (band, band_id) = snap_band(
            FreqRange::new(
                T359_LO_CELL as f64 * F_CELL,
                (T359_LO_CELL + 16) as f64 * F_CELL,
            ),
            F_CELL,
        );
        let job = BandJob {
            cfg: &cfg,
            levels: &p,
            obs: None,
            dets: &[],
            f_cell: F_CELL,
            t_cell_ns: T_CELL,
            max_samples: MAX_SPAN_SAMPLES,
        };
        let mut series = BTreeMap::new();
        let mut unit = PowerUnit::Dbfs;
        evaluate_band(&job, band, band_id, &channels, span, &mut series, &mut unit).unwrap();
        let rows = rows_for(&cfg, &channels, &series, span, span, unit, F_CELL);
        drop(p);
        let _ = std::fs::remove_dir_all(&dir);
        rows.into_iter()
            .filter(|r| matches!(r.subject, OccupancySubject::Channel { .. }))
            .collect()
    }

    /// The bias-tee states of `rows`, deduplicated.
    fn t359_states(rows: &[OccupancyStat]) -> Vec<hk_model::BiasTee> {
        let mut v: Vec<hk_model::BiasTee> = rows.iter().map(|r| r.bias_tee).collect();
        v.sort();
        v.dedup();
        v
    }

    /// T-359, **end to end**: the antenna-port bias-tee state rides from the ingested frame to the
    /// occupancy row and keys the baseline the row folds into — the last hop of T-325 → T-331 →
    /// T-333 → T-332, each of which was live while every fold was still `Unknown`.
    ///
    /// - **Property:** a run with the tee **on** produces rows carrying `On`, and they fold into a
    ///   different baseline than `Off` rows from the same site, subject and slot. The floor and the
    ///   gain state are identical in both runs, so nothing but the tee can separate them.
    /// - **Control:** a run whose source cannot report the state still produces `Unknown`, and two
    ///   such runs fold into **one** baseline — unknown still compares with unknown (T-333), so the
    ///   change cannot pass by making everything incomparable.
    /// - **Mixed:** a run that switches mid-span produces `Unknown`, never the longer-held state: a
    ///   row measured under two receive chains cannot claim either.
    /// - **Migration:** a row stored before T-359 (no `bias_tee` field on the wire) reads `Unknown`
    ///   and folds with the other unknowns — it is **not** promoted to `Off`.
    #[test]
    fn occupancy_rows_carry_the_bias_tee_and_key_their_own_baseline() {
        use hk_model::BiasTee;

        let on = t359_rows("on", |_| BiasTee::On);
        let off = t359_rows("off", |_| BiasTee::Off);
        let unknown = t359_rows("unknown", |_| BiasTee::Unknown);
        let unknown2 = t359_rows("unknown2", |_| BiasTee::Unknown);
        let switched = t359_rows("switch", |k| {
            if k < T359_SECS / 2 {
                BiasTee::Off
            } else {
                BiasTee::On
            }
        });
        for (name, rows) in [
            ("on", &on),
            ("off", &off),
            ("unknown", &unknown),
            ("switched", &switched),
        ] {
            assert!(!rows.is_empty(), "{name}: no channel row");
        }
        assert_eq!(t359_states(&on), vec![BiasTee::On], "the tee was on");
        assert_eq!(t359_states(&off), vec![BiasTee::Off], "the tee was off");
        assert_eq!(
            t359_states(&unknown),
            vec![BiasTee::Unknown],
            "the source could not report: unknown, never off"
        );
        assert_eq!(
            t359_states(&switched),
            vec![BiasTee::Unknown],
            "a row straddling a switch claims neither state"
        );
        // Migration: the pre-T-359 wire form of an `on` row is its JSON with the field absent.
        let legacy: Vec<OccupancyStat> = on
            .iter()
            .map(|r| {
                let mut v = serde_json::to_value(r).unwrap();
                assert!(v.get("bias_tee").is_some(), "the row states its state");
                v.as_object_mut().unwrap().remove("bias_tee");
                serde_json::from_value::<OccupancyStat>(v).unwrap()
            })
            .collect();
        assert_eq!(
            t359_states(&legacy),
            vec![BiasTee::Unknown],
            "a row written before T-359 reads unknown, never off"
        );

        // The folds. One site, one subject, one slot: only the tee can separate the cohorts.
        let a = crate::attention::AttentionService::in_memory().unwrap();
        let cur = a
            .set_current_site(crate::attention::SiteSelect {
                name: Some("bench".into()),
                ..Default::default()
            })
            .unwrap();
        let id: hk_model::ids::SiteId = cur["record"]["id"].as_str().unwrap().parse().unwrap();
        let span_end = ts(T359_T0 + T359_SECS * T_CELL);
        let mut folded = 0;
        for rows in [&on, &off, &unknown, &unknown2, &legacy] {
            let mut rows = rows.clone();
            stamp_site(&a, &mut rows, span_end);
            folded += a.ingest_interval(&rows, 0, span_end, BTreeMap::new()).len();
        }
        assert!(folded >= 5, "every run folded: {folded}");
        let b = a.baselines_json(Some(id)).unwrap();
        let cohorts: Vec<String> = b["baselines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["bias_tee"].as_str().unwrap().to_string())
            .collect();
        println!("T-359 baseline cohorts after five runs: {cohorts:?}");
        assert_eq!(
            cohorts.len(),
            3,
            "on, off and unknown — the two unknown runs and the legacy row share one: {cohorts:?}"
        );
        for state in ["on", "off", "unknown"] {
            assert!(cohorts.iter().any(|c| c == state), "{cohorts:?}");
        }
    }

    /// T-371: a `/api/baselines/slots` row **names the cohort its numbers belong to**.
    ///
    /// T-359 split occupancy cohorts by bias-tee state, so one site × subject × slot can now
    /// legitimately hold two pools with different numbers, both correct. The slots row carried
    /// only `cal`, so those pools served two rows **identical on the wire**: a reader saw two
    /// indistinguishable rows disagreeing and had nothing to resolve it with — which reads as the
    /// system being inconsistent, not as two cohorts. The row now carries `bias_tee`.
    ///
    /// The three runs differ in **nothing but the tee**: same site, same channel, same floor, same
    /// gain state, same calibration, same slot (the folds all land at [`T359_T0`]).
    ///
    /// - **Property:** rows folded under `On` and under `Off` are distinguishable on the wire, by
    ///   value — same `subject`, same `f_lo`/`f_hi`, different `bias_tee`.
    /// - **Control (literal legacy bytes):** a row stored before T-359 — its JSON with the
    ///   `bias_tee` field *removed*, not nulled, exactly as T-359 built its migration control —
    ///   still renders, in the `unknown` cohort. `unknown` is a value the route always states, not
    ///   an omission and not a null: nothing on this path lets a reader coerce it.
    /// - **The negative that matters:** the legacy rows are derived from the **`On`** run, so a
    ///   `powered().unwrap_or(false)` anywhere between the key and the wire would show up here
    ///   twice over — as a third `off` row, and as the disappearance of the `unknown` cohort.
    ///   Exactly one row reads `off`, and it is the one measured with the tee off.
    #[test]
    fn slots_rows_name_their_bias_tee_cohort_and_legacy_reads_unknown() {
        use hk_model::BiasTee;

        let on = t371_rows(BiasTee::On);
        let off = t371_rows(BiasTee::Off);
        // The pre-T-359 wire form of an `on` row: the field absent, never a null.
        let legacy: Vec<OccupancyStat> = on
            .iter()
            .map(|r| {
                let mut v = serde_json::to_value(r).unwrap();
                assert_eq!(v["bias_tee"], serde_json::json!("on"), "{v}");
                v.as_object_mut().unwrap().remove("bias_tee");
                assert!(v.get("bias_tee").is_none(), "removed, not nulled: {v}");
                serde_json::from_value::<OccupancyStat>(v).unwrap()
            })
            .collect();
        assert!(!on.is_empty() && !off.is_empty() && !legacy.is_empty());

        let a = crate::attention::AttentionService::in_memory().unwrap();
        a.set_current_site(crate::attention::SiteSelect {
            name: Some("bench".into()),
            ..Default::default()
        })
        .unwrap();
        let span_end = ts(T359_T0 + T359_SECS * T_CELL);
        for rows in [&on, &off, &legacy] {
            let mut rows = rows.clone();
            stamp_site(&a, &mut rows, span_end);
            a.ingest_interval(&rows, 0, span_end, BTreeMap::new());
        }

        let slots = a.slots_json(None, 0.0, 1e9, None, None).unwrap();
        let rows = slots["subjects"].as_array().unwrap();
        // One subject, one site: every row here is the same measurement under a different tee
        // state, which is the whole point — the cohort is the only thing that separates them.
        let subject = &rows[0]["subject"];
        for r in rows {
            assert_eq!(&r["subject"], subject, "one subject: {slots}");
            assert_eq!(r["f_lo"], rows[0]["f_lo"], "one extent: {slots}");
            assert_eq!(r["f_hi"], rows[0]["f_hi"], "one extent: {slots}");
            assert_eq!(r["cal"], rows[0]["cal"], "one calibration: {slots}");
        }
        let cohorts: Vec<&str> = rows
            .iter()
            .map(|r| {
                r["bias_tee"]
                    .as_str()
                    .unwrap_or_else(|| panic!("every row states its cohort: {r}"))
            })
            .collect();
        println!("T-371 slots cohorts for one subject: {cohorts:?}");
        assert_eq!(
            cohorts.iter().filter(|c| **c == "on").count(),
            1,
            "the tee-on run: {slots}"
        );
        assert_eq!(
            cohorts.iter().filter(|c| **c == "off").count(),
            1,
            "only the run measured with the tee off reads off: {slots}"
        );
        assert_eq!(
            cohorts.iter().filter(|c| **c == "unknown").count(),
            1,
            "the pre-T-359 row renders, as unknown — not blank, not off: {slots}"
        );
        assert_eq!(cohorts.len(), 3, "three cohorts, three rows: {slots}");
        // On the wire the cohort is a state, never a bool and never a null a reader could read as
        // off, and never absent (`BiasTee::powered()` is `None` for unknown, not `Some(false)`).
        for r in rows {
            assert!(
                !r["bias_tee"].is_null() && !r["bias_tee"].is_boolean(),
                "{r}"
            );
        }
    }

    /// Channel rows from one [`t359_rows`] run held at `bias` throughout.
    fn t371_rows(bias: hk_model::BiasTee) -> Vec<OccupancyStat> {
        t359_rows(&format!("t371-{}", bias.as_str()), |_| bias)
    }

    // T-314: the per-front-end guarantee reaches the measurement, not just the key.

    const T314_SECS: i64 = 256;
    /// Level-0 tile time block of [`t359_pyramid`], in seconds: two front ends whose frames
    /// alternate on this grid write **separate** tiles, so each is attributable.
    const T314_BLOCK: i64 = 16;
    const T314_FLOOR_A: f32 = -100.0;
    const T314_FLOOR_B: f32 = -70.0;
    const T314_DEVICE_A: &str = "hackrf:a";
    const T314_DEVICE_B: &str = "hackrf:b";

    /// A pyramid of [`T314_SECS`] one-second frames, each from the front end and at the floor
    /// `chain(second)` gives; `None` skips the second. Nothing else differs between the chains:
    /// same site, gain, calibration, port, mask and bias tee, so only the origin can separate
    /// them.
    fn t314_fixture(
        dir: &std::path::Path,
        chain: impl Fn(i64) -> Option<(&'static str, f32)>,
    ) -> hk_store::Pyramid {
        t377_fixture(dir, |k| {
            chain(k).map(|(d, fl)| (hk_store::history::source_key(d), fl))
        })
    }

    /// [`t314_fixture`] by raw source key, so a second can also be folded with **no** source
    /// stated (key 0, the [`hk_store::FrameInput::source`] default) — T-377's unknown cohort.
    fn t377_fixture(
        dir: &std::path::Path,
        chain: impl Fn(i64) -> Option<(u64, f32)>,
    ) -> hk_store::Pyramid {
        {
            let mut p = t359_pyramid(dir);
            let gain = hk_store::GainState {
                lna_db: 16.0,
                vga_db: 20.0,
                amp_on: false,
            };
            for k in 0..T314_SECS {
                let Some((source, floor)) = chain(k) else {
                    continue;
                };
                let psd = vec![10f32.powf(floor / 10.0); 16];
                let mut f = hk_store::FrameInput::new(
                    ts(T359_T0 + k * T_CELL),
                    T_CELL,
                    T359_LO_CELL as f64 * F_CELL,
                    F_CELL,
                    PowerUnit::Dbfs,
                    &psd,
                );
                f.gain = Some(gain);
                f.source = source;
                p.ingest(&f).unwrap();
            }
            p.seal_through(ts(T359_T0 + T314_SECS * T_CELL)).unwrap();
        }
        t359_pyramid(dir)
    }

    /// Two front ends 30 dB apart in **one** pyramid, alternating on the level-0 tile block, so
    /// every tile is attributable to exactly one of them.
    fn t314_alternating(k: i64) -> Option<(&'static str, f32)> {
        Some(if (k / T314_BLOCK) % 2 == 1 {
            (T314_DEVICE_B, T314_FLOOR_B)
        } else {
            (T314_DEVICE_A, T314_FLOOR_A)
        })
    }

    /// The fixture read through the production occupancy path under `filter`:
    /// `OriginLevels` → `evaluate_band` → `VisitSample`s → `rows_for` → `OccupancyStat`.
    fn t314_rows(p: &hk_store::Pyramid, filter: OriginFilter) -> Vec<OccupancyStat> {
        let cfg = OccupancyConfig::default();
        let key = ChannelKey {
            scheme: 1,
            lo_cell: T359_LO_CELL,
            hi_cell: T359_LO_CELL + 4,
        };
        let channels = vec![Channel {
            key,
            source: hk_model::attention::occupancy::ChannelSource::Learned,
            plan_version: 1,
            first_learned: ts(T359_T0),
            evidence: 1,
            obw_hz: key.freq(F_CELL).width_hz(),
            raster_hint: None,
        }];
        let span = TimeRange::new(ts(T359_T0), ts(T359_T0 + T314_SECS * T_CELL));
        let (band, band_id) = snap_band(
            FreqRange::new(
                T359_LO_CELL as f64 * F_CELL,
                (T359_LO_CELL + 16) as f64 * F_CELL,
            ),
            F_CELL,
        );
        let levels = engine::OriginLevels { pyramid: p, filter };
        let job = BandJob {
            cfg: &cfg,
            levels: &levels,
            obs: None,
            dets: &[],
            f_cell: F_CELL,
            t_cell_ns: T_CELL,
            max_samples: MAX_SPAN_SAMPLES,
        };
        let mut series = BTreeMap::new();
        let mut unit = PowerUnit::Dbfs;
        evaluate_band(&job, band, band_id, &channels, span, &mut series, &mut unit).unwrap();
        rows_for(&cfg, &channels, &series, span, span, unit, F_CELL)
            .into_iter()
            .filter(|r| matches!(r.subject, OccupancySubject::Channel { .. }))
            .collect()
    }

    /// What one read measured: representative level and floor, dB/Hz, and the excess above the
    /// floor a baseline would fold from it (NaN when the row declines to say — a suspect floor).
    #[derive(Clone, Copy, Debug)]
    struct T314Measured {
        level_db: f64,
        floor_db: f64,
        excess_db: f64,
        visits: u64,
        occupied: u64,
        /// The row's own verdict on its floor (T-377 asserts this, not only the excess: a floor
        /// called suspect is *why* the excess is withheld).
        suspect: Option<bool>,
    }

    fn t314_measured(tag: &str, rows: &[OccupancyStat]) -> Option<T314Measured> {
        let r = rows.first()?;
        let m = T314Measured {
            level_db: r.level_idle_db.or(r.level_occupied_p50_db)?,
            floor_db: r.floor_db?,
            excess_db: hk_context::occupancy::baseline::channel_level_excess(r)
                .map_or(f64::NAN, |(l, _)| l),
            visits: r.n_revisits,
            occupied: r.n_occupied,
            suspect: r.floor_suspect,
        };
        println!(
            "T-314 {tag}: {} row(s), level {:.2} dB/Hz, floor {:.2} dB/Hz, baseline excess \
             {:.2} dB, visits {} ({} occupied), floor_suspect {:?}",
            rows.len(),
            m.level_db,
            m.floor_db,
            m.excess_db,
            m.visits,
            m.occupied,
            r.floor_suspect,
        );
        Some(m)
    }

    /// **T-314, end to end.** T-303 keyed baselines by the receive chain; the measurement behind
    /// the key was still read UNFILTERED, so two front ends folded into one pyramid were already
    /// averaged in the level-0 cells before any baseline saw them. The guarantee stopped at the
    /// key. It now reaches the measurement: every occupancy read restricts the history to the
    /// run's own chain ([`engine::chain_filter`], the same device hash the baseline key uses).
    ///
    /// - **Property:** two front ends 30 dB apart in one pyramid; each chain's own filtered read
    ///   reports **its own floor** — a value in dB, not a shape — and nothing of the other's.
    /// - **Pooled control, on the same fixture:** read with [`OriginFilter::ANY`] the cells are
    ///   all still there (twice the visits), and the row compounds the two chains — chain A's
    ///   floor under chain B's level, so half the window reads occupied and the baseline folds a
    ///   30 dB excess that no emission produced. The control is what stops the property passing
    ///   by the filtered read having returned nothing.
    /// - **A level between the two floors:** with the chains only 4 dB apart, neither crosses the
    ///   other's threshold, and the pooled row's level is literally the mixture — between the two
    ///   floors, which is neither chain's measurement.
    /// - **Unattributable cells:** a level-0 tile holding both chains answers **unobserved**, not
    ///   the mixture and not the chain that contributed most (the T-359 rule, one layer down).
    ///   Bounded silence, per T-333, is the chosen behaviour.
    #[test]
    fn occupancy_reads_only_its_own_front_ends_energy() {
        let tmp = |tag: &str| {
            let d = std::env::temp_dir()
                .join(format!("hk-t314-{tag}-{}", hk_model::ids::SiteId::new()));
            std::fs::create_dir_all(&d).unwrap();
            d
        };
        let chain = |d: &str| engine::chain_filter(ChainKey::of_device(d));
        let dir = tmp("alt");
        let p = t314_fixture(&dir, t314_alternating);

        let a = t314_rows(&p, chain(T314_DEVICE_A));
        let b = t314_rows(&p, chain(T314_DEVICE_B));
        let pooled = t314_rows(&p, OriginFilter::ANY);
        let ma = t314_measured("chain A ", &a).expect("chain A measured something");
        let mb = t314_measured("chain B ", &b).expect("chain B measured something");
        let mp = t314_measured("pooled  ", &pooled).expect("the pooled control");

        // (a) The pooled control first, because it is the precondition: the fixture really does
        // fold two front ends into these cells, and the unfiltered read really does return them.
        // Read that way the row compounds the two chains — chain A's floor under chain B's level —
        // so half the window reads occupied with nothing on the air, and the excess the baseline
        // folds is the 30 dB gap between two front ends.
        assert!(
            (mp.floor_db - f64::from(T314_FLOOR_A)).abs() < 1.0,
            "the pooled floor {} is chain A's",
            mp.floor_db
        );
        assert_eq!(
            mp.occupied, 128,
            "chain B's noise read as an occupied channel"
        );
        assert!(
            mp.excess_db > 25.0,
            "the pooled excess {} is the gap between the chains, not a signal",
            mp.excess_db
        );

        // (b) The property, as values: each chain's row is its own chain's floor, and no part of
        // the other's — 30 dB away, not a fraction of it.
        for (name, m, own, other) in [
            ("A", ma, T314_FLOOR_A, T314_FLOOR_B),
            ("B", mb, T314_FLOOR_B, T314_FLOOR_A),
        ] {
            assert!(
                (m.level_db - f64::from(own)).abs() < 1.0
                    && (m.floor_db - f64::from(own)).abs() < 1.0,
                "chain {name} reads its own floor {own}: level {}, floor {}",
                m.level_db,
                m.floor_db
            );
            assert!(
                (m.level_db - f64::from(other)).abs() > 25.0,
                "chain {name}'s level {} holds the other chain's energy",
                m.level_db
            );
            // Its own noise is not an emission on its own chain: nothing stands over the floor.
            assert_eq!(
                m.occupied, 0,
                "chain {name} found occupancy in its own noise"
            );
        }
        assert!(
            ma.excess_db.abs() < 1.0,
            "chain A's own excess is its own noise, not the other chain's: {}",
            ma.excess_db
        );
        // Each chain measured its own half of the span, and the pooled read measured both: the
        // filtered reads are restrictions of a working read, not a broken one.
        assert_eq!(
            (ma.visits, mb.visits, mp.visits),
            (128, 128, 256),
            "each chain's own visits, and the pooled read's both"
        );

        // (c) A level **between** the two floors: 2 dB apart, under the floor + 3 dB clamp on
        // the threshold, so neither chain crosses the other's, no visit is called occupied, and
        // the pooled level is the mixture itself — the midpoint of the two floors.
        const NEAR_B: f32 = T314_FLOOR_A + 2.0;
        let ndir = tmp("near");
        let np = t314_fixture(&ndir, |k| {
            Some(if (k / T314_BLOCK) % 2 == 1 {
                (T314_DEVICE_B, NEAR_B)
            } else {
                (T314_DEVICE_A, T314_FLOOR_A)
            })
        });
        let na =
            t314_measured("near A  ", &t314_rows(&np, chain(T314_DEVICE_A))).expect("near chain A");
        let nb =
            t314_measured("near B  ", &t314_rows(&np, chain(T314_DEVICE_B))).expect("near chain B");
        let npooled =
            t314_measured("near mix", &t314_rows(&np, OriginFilter::ANY)).expect("near pooled");
        assert!(
            (na.level_db - f64::from(T314_FLOOR_A)).abs() < 1.0
                && (nb.level_db - f64::from(NEAR_B)).abs() < 1.0,
            "each chain still reads its own floor: A {}, B {}",
            na.level_db,
            nb.level_db
        );
        assert_eq!(
            npooled.occupied, 0,
            "neither chain crosses the other's threshold at this gap"
        );
        let mid = 0.5 * f64::from(T314_FLOOR_A + NEAR_B);
        assert!(
            (npooled.level_db - mid).abs() < 0.2,
            "the pooled level {} is the mixture of the two floors {T314_FLOOR_A} and {NEAR_B},              i.e. {mid}, which is neither chain's measurement",
            npooled.level_db
        );
        assert!(
            npooled.level_db > f64::from(T314_FLOOR_A) + 0.5
                && npooled.level_db < f64::from(NEAR_B) - 0.5,
            "and it lies strictly between them: {}",
            npooled.level_db
        );

        // (d) A cell that cannot be attributed to one chain. The same two chains, interleaved
        // second by second so every level-0 tile holds both, and three of every four seconds are
        // chain A's — a majority rule would call these tiles A's. Each per-chain read reports
        // NOTHING there: not the mixture, and not the chain that contributed most.
        let idir = tmp("mixed");
        let ip = t314_fixture(&idir, |k| {
            Some(if k % 4 == 3 {
                (T314_DEVICE_B, T314_FLOOR_B)
            } else {
                (T314_DEVICE_A, T314_FLOOR_A)
            })
        });
        let (ia, ib) = (
            t314_rows(&ip, chain(T314_DEVICE_A)),
            t314_rows(&ip, chain(T314_DEVICE_B)),
        );
        let ipooled = t314_rows(&ip, OriginFilter::ANY);
        println!(
            "T-314 interleaved: chain A {} row(s), chain B {} row(s), pooled {} row(s)",
            ia.len(),
            ib.len(),
            ipooled.len()
        );
        assert!(
            ia.is_empty() && ib.is_empty(),
            "a tile pooling both chains must measure neither, not the majority contributor"
        );
        assert!(
            !ipooled.is_empty(),
            "the pooled read still measures the mixture: the cells are there, and it is only \
             attribution that is missing"
        );

        for (p, d) in [(p, dir), (np, ndir), (ip, idir)] {
            drop(p);
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    /// **T-377: a level-0 cell's occupancy is decided at ingest against its OWN front end's
    /// floor.** T-314 made every occupancy *read* per-chain, but `CellStats::occupancy` was
    /// decided when the frame was folded, against a floor the pyramid tracked per frequency block
    /// and every front end fed. A tile pure in origin could therefore carry an occupancy decided
    /// against another chain's floor, and no read-side filter could undo it: the decision is
    /// baked into the stored cell. `Pyramid::floors` is now keyed `(FrameInput::source, f_block)`
    /// — the same `source_key(device_id)` the read filter and `ChainKey` use — and a sealed tile
    /// feeds only its own source's track.
    ///
    /// This test is T-314's residue pin, turned around. It keeps both halves:
    ///
    /// - **The property:** chain B beside chain A now folds the **same excess it folds alone**,
    ///   and its floor is no longer called suspect. Before: shared `floor_suspect = true` and
    ///   excess `NaN` against alone's `false` / 0.00 dB — B's own noise read as an occupied
    ///   channel because chain A's floor, 30 dB lower, was the threshold it was compared with.
    /// - **The dense-band control:** one chain, one emission genuinely above a floor that chain
    ///   measured itself, and the neighbourhood really is busy. Its floor is **still** called
    ///   suspect and its excess is **still** withheld. The suspect flag is correct about a dense
    ///   band; a fix that made it stop firing would have broken the detector instead of the
    ///   pooling.
    /// - **The unknown-source control:** frames that state no source (key 0) are their own
    ///   cohort, never a named chain's. Chain A's row is bit-identical whether or not they are
    ///   beside it, and they read their own level rather than inheriting A's floor. That is
    ///   T-359's rule at ingest: an unattributable measurement claims nothing and contributes
    ///   nothing, rather than joining the dominant contributor.
    ///
    /// Mutation: re-pooling the key (`floors.entry(tile.key.f_block)` / `floors.get(&fb)`) fails
    /// the property while the dense-band control still passes.
    #[test]
    fn occupancy_ingest_time_floor_is_kept_per_front_end() {
        let tmp = |tag: &str| {
            let d = std::env::temp_dir()
                .join(format!("hk-t377-{tag}-{}", hk_model::ids::SiteId::new()));
            std::fs::create_dir_all(&d).unwrap();
            d
        };
        let chain = |d: &str| engine::chain_filter(ChainKey::of_device(d));
        let named = |d: &str| OriginFilter {
            source: OriginField::Is(hk_store::history::source_key(d)),
            site: OriginField::Any,
        };
        // (a) The property. Chain B's own frames, at their own times, with no other front end in
        // the pyramid; then the identical frames with chain A's tiles beside them.
        let adir = tmp("alone");
        let ap = t314_fixture(&adir, |k| {
            ((k / T314_BLOCK) % 2 == 1).then_some((T314_DEVICE_B, T314_FLOOR_B))
        });
        let alone = t314_measured("B alone ", &t314_rows(&ap, chain(T314_DEVICE_B)))
            .expect("chain B alone");
        let sdir = tmp("shared");
        let sp = t314_fixture(&sdir, t314_alternating);
        let shared = t314_measured("B shared", &t314_rows(&sp, chain(T314_DEVICE_B)))
            .expect("chain B beside chain A");

        assert_eq!(
            (alone.level_db, alone.floor_db),
            (shared.level_db, shared.floor_db),
            "the read filter gives chain B the same level and floor either way"
        );
        assert!(
            alone.excess_db.abs() < 1.0 && shared.excess_db.abs() < 1.0,
            "chain B's noise is its own floor, alone ({}) and beside chain A ({})",
            alone.excess_db,
            shared.excess_db
        );
        assert_eq!(
            (alone.excess_db, alone.suspect),
            (shared.excess_db, shared.suspect),
            "beside another chain, chain B folds exactly what it folds alone"
        );
        assert_eq!(
            (alone.suspect, shared.suspect),
            (Some(false), Some(false)),
            "and neither reading calls chain B's own quiet band dense"
        );

        // (b) The dense-band control. One chain; two seconds in sixteen are quiet, so the chain
        // measures its own floor there, and the other fourteen carry an emission 20 dB over it.
        // The band really is busy, so the floor really is suspect and the excess is withheld.
        let ddir = tmp("dense");
        let dp = t314_fixture(&ddir, |k| {
            Some((
                T314_DEVICE_A,
                if k % T314_BLOCK < 2 { -100.0 } else { -80.0 },
            ))
        });
        let dense =
            t314_measured("A dense ", &t314_rows(&dp, chain(T314_DEVICE_A))).expect("dense chain");
        assert_eq!(
            dense.suspect,
            Some(true),
            "a genuinely dense band still flags its floor suspect"
        );
        assert!(
            dense.occupied > 0,
            "the dense band's emission reads as occupancy: {} of {} visits",
            dense.occupied,
            dense.visits
        );
        assert!(
            dense.excess_db.is_nan(),
            "and a suspect floor still withholds the excess: {}",
            dense.excess_db
        );

        // (c) The unknown-source control. Chain A at its own floor, and frames that state NO
        // source 30 dB above it. The unknown cohort must not fold into chain A's floor track,
        // and must not inherit it: it reads its own level with no excess, and chain A's row is
        // unchanged from the run where the unknown frames were never there.
        let odir = tmp("a-only");
        let op = t314_fixture(&odir, |k| {
            ((k / T314_BLOCK) % 2 == 0).then_some((T314_DEVICE_A, T314_FLOOR_A))
        });
        let a_only =
            t314_measured("A only  ", &t314_rows(&op, chain(T314_DEVICE_A))).expect("chain A only");
        let udir = tmp("unknown");
        let up = t377_fixture(&udir, |k| {
            Some(if (k / T314_BLOCK) % 2 == 1 {
                (0, T314_FLOOR_B)
            } else {
                (hk_store::history::source_key(T314_DEVICE_A), T314_FLOOR_A)
            })
        });
        let a_beside = t314_measured("A |unk  ", &t314_rows(&up, named(T314_DEVICE_A)))
            .expect("chain A beside the unknown cohort");
        let unknown = t314_measured(
            "unknown ",
            &t314_rows(
                &up,
                OriginFilter {
                    source: OriginField::Is(0),
                    site: OriginField::Any,
                },
            ),
        )
        .expect("the unknown cohort");
        assert_eq!(
            (
                a_only.level_db,
                a_only.floor_db,
                a_only.excess_db,
                a_only.suspect
            ),
            (
                a_beside.level_db,
                a_beside.floor_db,
                a_beside.excess_db,
                a_beside.suspect
            ),
            "frames that state no source changed nothing about chain A's measurement"
        );
        assert!(
            (unknown.level_db - f64::from(T314_FLOOR_B)).abs() < 1.0
                && unknown.excess_db.abs() < 1.0,
            "the unknown cohort reads its own level {} with no excess {}, rather than \
             inheriting chain A's floor",
            unknown.level_db,
            unknown.excess_db
        );
        assert_eq!(
            unknown.occupied, 0,
            "and its own noise is not an emission on its own cohort"
        );

        for (p, d) in [(ap, adir), (sp, sdir), (dp, ddir), (op, odir), (up, udir)] {
            drop(p);
            let _ = std::fs::remove_dir_all(&d);
        }
    }
}
