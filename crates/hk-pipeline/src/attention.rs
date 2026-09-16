//! Baseline/novelty/score wiring (T-119, ADR-0012 §3–§4): runs the C12 scoring loop and publishes
//! `CandidateSet` snapshots to the scheduler's `InterestingnessProvider`.
//!
//! [`AttentionService`] owns, for one run:
//! - the site state machine (`hk_context::occupancy::site`), persisted to the run database;
//! - the baselines (`hk_context::occupancy::baseline`) over `<data>/baselines` (hk-store);
//! - the score weights (versioned rows, migration 0002) and the [`SharedInterestingness`] the
//!   scheduler (T-120) reads;
//! - the JSON views the control API serves (`hk_api::attention`, adapted in hk-cli).
//!
//! **Inputs.** [`AttentionService::ingest_occupancy`] is the thin adapter for T-118's
//! `OccupancyStat` rows (called once per interval close); [`AttentionService::observe`] takes a
//! prepared fold (history cells, tests); [`AttentionService::on_fix`] takes C06 fixes;
//! [`AttentionService::score`] ranks measured candidates.
//!
//! **Pipeline wiring (T-128).** The run opens one service ([`AttentionService::open_for_run`]).
//! T-118's occupancy thread calls [`AttentionService::ingest_interval`] at each 15-min close: the
//! channel rows fold into the baselines (with real levels above the floor), the inventory's first
//! sightings in the interval feed the site's [`FirstSightingRate`] (new-emitter novelty), and the
//! candidates are re-scored. The control thread feeds the candidate table (confirmed tracks,
//! members with SNR and suspect flags, recipe matches, trust verdicts;
//! [`crate::candidates`]) and calls [`AttentionService::publish_candidates`] each step boundary;
//! the scheduler's bandit reads [`AttentionService::provider`]. Neither thread is the sample path.
//!
//! All contract time is the sample clock handed in by the caller (ADR-0012 §0); the service's
//! `clock` (stream time) is used only to stamp API-created sites and weight rows.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use hk_context::occupancy::alarm::{AlarmConfig, AlarmInput, PoolContext, new_emitter_input};
use hk_context::occupancy::baseline::{
    BaselineConfig, BaselineCopy, Baselines, FoldOutcome, IntervalObservation, PoolStats,
    from_occupancy_stat, in_pool, maturity, pools,
};
use hk_context::occupancy::baseline::{REFERENCE_LEARN_MAX_NOVELTY, res_index};
use hk_context::occupancy::novelty::NoveltyConfig;
use hk_context::occupancy::novelty::{FirstSightingRate, persistent_single_novelty};
use hk_context::occupancy::score::{CandidateInput, Scorer};
use hk_context::occupancy::site::{Fix, SiteAssigner};
use hk_model::Repository;
use hk_model::attention::alarm::{AlarmKind, AlarmSubject};
use hk_model::attention::baseline::MATURITY_MIN_OBSERVED_S;
use hk_model::attention::baseline::{
    BaselineResolution, CalKey, ChainKey, HourOfWeek, Maturity, SiteConfig, SiteKey, SiteRecord,
    SiteSource,
};
use hk_model::attention::occupancy::{ChannelKey, OccupancyStat, OccupancySubject};
use hk_model::attention::report::{BaselineComparison, ChangeEntry, ComparisonStatus};
use hk_model::attention::score::new_emitter_novelty;
use hk_model::attention::score::{InterestingnessProvider, ScoreWeights, SharedInterestingness};
use hk_model::ids::{EmitterId, SiteId};
use hk_model::time::Timestamp;
use hk_model::{FreqRange, Region, TimeRange, TrackId};
use hk_store::baseline::{BaselineStore, BaselineSubject, SubjectBaseline};
use serde_json::{Value, json};

use crate::candidates::{
    CandidateTable, ChannelContext, EvidenceSource, MemberEvidence, class_entropy,
};
use crate::chains::TrackDecodes;
use crate::stats::Counters;

/// How far back scoring looks for an inventory classification of a track, ns.
const CLASS_LOOKBACK_NS: i64 = 24 * 3_600_000_000_000;
/// Minimum sample-clock gap between class-entropy snapshot refreshes, ns.
const CLASS_REFRESH_NS: i64 = 10_000_000_000;
/// Most inventory emitters one class-entropy refresh reads (the most recently seen).
pub const CLASS_ROWS_MAX: usize = 2_000;
/// Longest row `compare_report` compares against a single hour-of-week slot, ns.
const COMPARE_ROW_MAX_NS: i64 = 3_600_000_000_000;

/// Classified inventory emitters for class entropy: extent and entropy of the latest
/// classification. Read only by the class-entropy worker thread, never by the control thread.
pub trait ClassSource: Send + Sync {
    /// Classified emitters overlapping `region` (at most `limit`, most recently seen first).
    fn classified(&self, region: &Region, limit: usize) -> Vec<(FreqRange, f64)>;
}

/// [`ClassSource`] over the run's inventory database.
struct RepoClasses(Arc<Mutex<Repository>>);

impl ClassSource for RepoClasses {
    fn classified(&self, region: &Region, limit: usize) -> Vec<(FreqRange, f64)> {
        let Ok(emitters) = lock(&self.0).emitters_in_region_limited(region, Some(limit)) else {
            return Vec::new();
        };
        emitters
            .iter()
            .filter_map(|e| {
                e.classifications.last().map(|c| {
                    (
                        e.freq(),
                        class_entropy(&c.family, c.confidence, c.open_set_score),
                    )
                })
            })
            .collect()
    }
}

/// T-128 review: the class-entropy snapshot scoring reads. A worker thread refreshes it; the
/// control thread only swaps an `Arc` out (no DB I/O, no waiting on a query).
struct ClassCache {
    source: Mutex<Arc<dyn ClassSource>>,
    snapshot: Mutex<Arc<Vec<(FreqRange, f64)>>>,
    in_flight: AtomicBool,
    last_request_ns: AtomicI64,
    refreshes: AtomicU64,
}

impl ClassCache {
    fn new(source: Arc<dyn ClassSource>) -> Self {
        Self {
            source: Mutex::new(source),
            snapshot: Mutex::new(Arc::new(Vec::new())),
            in_flight: AtomicBool::new(false),
            last_request_ns: AtomicI64::new(i64::MIN),
            refreshes: AtomicU64::new(0),
        }
    }

    /// Spawns the refresh worker; it exits when the returned sender drops (with the service).
    fn spawn_worker(self: &Arc<Self>) -> Option<SyncSender<Region>> {
        let (tx, rx) = sync_channel::<Region>(1);
        let cache = Arc::clone(self);
        std::thread::Builder::new()
            .name("hk-attn-class".into())
            .spawn(move || {
                while let Ok(region) = rx.recv() {
                    let source = Arc::clone(&*lock(&cache.source));
                    let v = Arc::new(source.classified(&region, CLASS_ROWS_MAX));
                    *lock(&cache.snapshot) = v;
                    cache.refreshes.fetch_add(1, Ordering::Relaxed);
                    cache.in_flight.store(false, Ordering::Release);
                }
            })
            .ok()?;
        Some(tx)
    }
}

/// Class entropy of each track from the snapshot emitter covering most of its extent (≥ ½).
/// Tracks without one are unclassified.
fn class_entropies(
    classified: &[(FreqRange, f64)],
    tracks: &[(TrackId, FreqRange)],
) -> HashMap<TrackId, f64> {
    let mut out = HashMap::new();
    if classified.is_empty() {
        return out;
    }
    for (id, f) in tracks {
        let best = classified
            .iter()
            .map(|(ef, h)| {
                let overlap = (ef.hi_hz.min(f.hi_hz) - ef.lo_hz.max(f.lo_hz)).max(0.0);
                (overlap / f.width_hz().max(1.0), *h)
            })
            .filter(|(share, _)| *share >= 0.5)
            .max_by(|a, b| a.0.total_cmp(&b.0));
        if let Some((_, h)) = best {
            out.insert(*id, h);
        }
    }
    out
}

/// Stouffer combination of per-interval z-scores with weights `w` (`Σ wᵢzᵢ / √Σ wᵢ²`): N(0, 1)
/// under no change when the intervals are independent.
fn stouffer(zw: &[(f64, f64)]) -> Option<f64> {
    let (num, den) = zw
        .iter()
        .filter(|(z, w)| z.is_finite() && w.is_finite() && *w > 0.0)
        .fold((0.0, 0.0), |(n, d), (z, w)| (n + w * z, d + w * w));
    (den > 0.0).then(|| num / den.sqrt())
}

/// FCO of each mature reference occupancy pool of `sub` at `slot`.
fn mature_pool_fcos(sub: &SubjectBaseline, slot: HourOfWeek) -> Vec<f64> {
    let (_, occ) = pools(sub, slot, None, BaselineCopy::Reference);
    occ.iter()
        .filter(|p| p.observed_s >= MATURITY_MIN_OBSERVED_S)
        .filter_map(PoolStats::fco)
        .collect()
}

/// The bias-tee state a fold from an occupancy row is keyed under (T-333).
///
/// [`hk_model::BiasTee::Unknown`], and stated once here rather than spelt at each fold, because the
/// **occupancy row** still cannot say what the DC was doing. Unknown is the honest value — and it is
/// the value every baseline already on disk carries, so folding under it changes nothing today and
/// orphans nothing.
///
/// This is not a service-lifetime constant like the receive chain: a bias tee is switched **during**
/// a run, so the state belongs to the measurement, not to the service.
///
/// **T-332 laid the carrier**: `FrameInput::bias_tee` → `FrontEndState::bias_tee` →
/// `ProvenanceSummary::bias_tee` / `bias_tee_mixed` (tile format 5), and a switch is now a
/// `ProvenanceStep::BIAS_TEE` the report lists as a `bias-tee` step. The remaining hop is
/// **T-359**: `hk_context::occupancy::engine::VisitSample` reads its `gain_key` from
/// `grid.provenance.dominant_gain_key()` at the same point it can read `grid.provenance.bias_tee`,
/// and carrying that into `SubjectContext` / `OccupancyStat` is what replaces this constant with
/// the row's own value. Until then the key, its file format and its migration are in place and the
/// cohorts separate the moment a caller supplies `Off` or `On` — as `Baselines::observe` already
/// does for any caller.
const FOLD_BIAS_TEE: hk_model::BiasTee = hk_model::BiasTee::Unknown;

/// The time a row's visits represent, s (as [`from_occupancy_stat`] weighs a fold).
pub(crate) fn represented_s(stat: &OccupancyStat) -> f64 {
    let interval_s = stat.interval.duration_ns() as f64 / 1e9;
    if stat.n_revisits_all == 0 {
        return 0.0;
    }
    stat.revisit_mean_s
        .map_or(interval_s, |m| m * stat.n_revisits_all as f64)
        .min(interval_s)
}

/// T-131: one channel row's baseline fold with what the alarm service needs (ADR-0012 §7): the
/// site and calibration it was folded under, the observation, the site's UTC offset and the
/// reference pool its novelty was measured against.
#[derive(Clone, Copy, Debug)]
pub struct IntervalFold {
    /// The row's subject.
    pub subject: OccupancySubject,
    /// Site assignment at measurement time.
    pub site: SiteKey,
    /// Calibration key.
    pub cal: CalKey,
    /// The folded observation.
    pub obs: IntervalObservation,
    /// The site's UTC offset, minutes.
    pub utc_offset_min: i16,
    /// Reference pool (default when immature).
    pub pool: PoolContext,
    /// The fold's outcome.
    pub fold: FoldOutcome,
}

/// Level-0 cell width of history scheme 1, Hz.
pub const SCHEME_1_CELL_HZ: f64 = 6250.0;
/// Baseline cell factor (100 kHz on scheme 1).
pub const CELL_FACTOR: u16 = 16;
/// Most subjects `/api/baselines/slots` returns.
pub const SLOTS_MAX: usize = 2_000;
/// Default and maximum candidates `/api/candidates` returns.
pub const CANDIDATES_DEFAULT: usize = 100;
/// Maximum candidates `/api/candidates` returns.
pub const CANDIDATES_MAX: usize = 1_000;

/// A refused or failed attention call: HTTP status, stable code, message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttentionError {
    /// HTTP status.
    pub status: u16,
    /// Stable code (`invalid`, `not_found`, `conflict`, `failed`).
    pub code: &'static str,
    /// Message (never echoes values).
    pub message: String,
}

impl AttentionError {
    fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    fn invalid(message: impl Into<String>) -> Self {
        Self::new(400, "invalid", message)
    }

    fn failed(what: &str, e: impl std::fmt::Display) -> Self {
        Self::new(500, "failed", format!("{what}: {e}"))
    }
}

/// What `PUT /api/sites/current` asks for.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SiteSelect {
    /// Select a known site.
    pub id: Option<SiteId>,
    /// Select the site of this name, or create it.
    pub name: Option<String>,
    /// Centroid of a created site.
    pub lat_deg: Option<f64>,
    /// Centroid of a created site.
    pub lon_deg: Option<f64>,
    /// Radius of a created site, m.
    pub radius_m: Option<f64>,
    /// UTC offset of a created site, minutes.
    pub utc_offset_min: Option<i16>,
    /// Release the pin (fixes decide again).
    pub release: bool,
}

/// C12 for one run.
pub struct AttentionService {
    repo: Arc<Mutex<Repository>>,
    sites: Mutex<SiteAssigner>,
    baselines: Mutex<Baselines>,
    scorer: Mutex<Scorer>,
    weights: Mutex<ScoreWeights>,
    provider: Arc<SharedInterestingness>,
    counters: Option<Arc<Counters>>,
    clock: Arc<dyn Fn() -> Timestamp + Send + Sync>,
    /// T-303: the receive chain every fold of this service is stamped with — the front end whose
    /// frames its history and occupancy were built from. `Unknown` when no device was named, which
    /// keys baselines exactly as they were keyed before T-303.
    chain: ChainKey,
    /// T-128: candidate evidence, channel novelty and the site's first-sighting rate.
    cands: Mutex<CandidateState>,
    /// Class-entropy snapshot, refreshed off the control thread.
    classes: Arc<ClassCache>,
    class_tx: Mutex<Option<SyncSender<Region>>>,
}

/// T-128: what scoring reads besides the baselines.
#[derive(Default)]
struct CandidateState {
    table: CandidateTable,
    /// Latest fold per learned channel: extent and context.
    channels: BTreeMap<ChannelKey, (FreqRange, ChannelContext)>,
    /// T-136: first-sighting state per site, so another site's sightings never score or alarm
    /// under the site in force.
    sightings: BTreeMap<SiteKey, SiteSightings>,
    /// The site of the latest closed interval (whose new-emitter novelty the candidates read).
    sighting_site: Option<SiteKey>,
    decodes: Option<Arc<TrackDecodes>>,
}

impl CandidateState {
    /// The new-emitter novelty of the latest close's site (`None` while immature).
    fn current_new_emitter(&self) -> Option<f64> {
        self.sighting_site
            .and_then(|s| self.sightings.get(&s))
            .and_then(|s| s.rate.novelty())
    }
}

/// T-136: one site's first sightings.
#[derive(Default)]
struct SiteSightings {
    rate: FirstSightingRate,
    /// The emitters behind the recent first sightings (new-emitter alarm inputs).
    recent: VecDeque<FirstSighting>,
    /// End of the last close that built new-emitter inputs: sightings after it are fresh.
    last_inputs_end: Option<Timestamp>,
    /// T-138: first sightings waiting for (or holding) the persistent single-emitter rule, by
    /// emitter; bounded by [`PERSIST_HORIZON_S`] and [`PERSIST_MAX_PENDING`].
    pending: BTreeMap<EmitterId, PendingEmitter>,
    /// T-138: what the latest close observed (`note_emitters_seen`).
    seen: Option<SeenAtClose>,
}

/// T-138: what one close observed, for the persistent single-emitter rule.
struct SeenAtClose {
    iv: TimeRange,
    observed: Vec<ObservedBand>,
    seen: Vec<EmitterSeen>,
    churned: Vec<EmitterId>,
}

/// T-138: how long after its first sighting a new emitter can still be confirmed (a later visit);
/// also the churn look-back (another emitter over its extent seen this recently).
pub const PERSIST_HORIZON_S: f64 = 7.0 * 86_400.0;
/// T-138: pending first sightings kept per site (oldest dropped first).
pub const PERSIST_MAX_PENDING: usize = 512;
/// T-138: an observed band "covers" a pending emitter (so its absence feeds 0) only when the
/// band's bin width is at most this multiple of the emitter's bandwidth: a coarse sweep that
/// cannot resolve it neither confirms nor resets.
pub const PERSIST_COVER_MAX_RBW_RATIO: f64 = 1.0;

/// T-138: one band a close observed, with the bin width it was measured at.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ObservedBand {
    /// Extent.
    pub freq: FreqRange,
    /// Resolution bandwidth of its occupancy row, Hz.
    pub rbw_hz: f64,
}

/// T-138: one first sighting tracked by the persistent single-emitter rule.
#[derive(Clone, Copy, Debug)]
struct PendingEmitter {
    /// Extent at first sighting (the alarm subject, stable across closes).
    freq: FreqRange,
    /// Close that counted the first sighting.
    sighted: Timestamp,
    /// Seen suspect (spur, IMD, image, clipped/compressed…): never scored by the rule.
    disqualified: bool,
    /// Fed a rule score at least once (so observed closes without it feed 0 to reset or clear).
    armed: bool,
    /// Consecutive zero inputs fed since last scored.
    zeros: u32,
    /// Consecutive closes scored at or above `on` (mirrors the engine's raise streak).
    streak: u32,
    /// One-shot: the streak reached the raise count, so the rule never scores it again.
    spent: bool,
}

/// T-138: an inventory emitter seen in a closed interval, for the persistent single-emitter rule.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EmitterSeen {
    /// The emitter.
    pub emitter: EmitterId,
    /// Its current extent.
    pub freq: FreqRange,
    /// Inventory first sighting.
    pub first_seen: Timestamp,
    /// Inventory latest sighting (may already be past the interval's end when the close runs).
    pub last_seen: Timestamp,
    /// Its detections in the interval are all suspect (§2.6: spur, IMD, image, clipped,
    /// compressed…), so the sighting cannot confirm anything.
    pub suspect: bool,
}

/// T-136: one new-emitter alarm input and whether its sighting is new at this close (counted
/// once; an input still in the window is re-fed at later closes for hysteresis, uncounted).
#[derive(Clone, Copy, Debug)]
pub struct NewEmitterInput {
    /// The input.
    pub input: AlarmInput,
    /// The sighting was first recorded in the interval this close ends.
    pub fresh: bool,
}

/// T-136: one inventory emitter first seen in a closed occupancy interval.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FirstSighting {
    /// The emitter.
    pub emitter: EmitterId,
    /// Its measured extent.
    pub freq: FreqRange,
    /// End of the interval it was counted in (sample clock).
    pub t: Timestamp,
}

/// The evidence one scoring pass reads.
struct PassEvidence<'a> {
    channels: &'a BTreeMap<ChannelKey, (FreqRange, ChannelContext)>,
    new_emitter: Option<f64>,
    decodes: Option<&'a TrackDecodes>,
    entropy: HashMap<TrackId, f64>,
}

impl EvidenceSource for PassEvidence<'_> {
    fn channel(&self, freq: FreqRange) -> Option<ChannelContext> {
        // The learned channel overlapping the track most (its share of the track's extent ≥ ½).
        self.channels
            .values()
            .map(|(f, c)| {
                let overlap = (f.hi_hz.min(freq.hi_hz) - f.lo_hz.max(freq.lo_hz)).max(0.0);
                (overlap / freq.width_hz().max(1.0), c)
            })
            .filter(|(share, _)| *share >= 0.5)
            .max_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(_, c)| c.clone())
    }
    fn new_emitter(&self) -> Option<f64> {
        self.new_emitter
    }
    fn decodes(&self, track: TrackId) -> u64 {
        self.decodes.map_or(0, |d| d.get(track))
    }
    fn class_entropy(&self, track: TrackId) -> Option<f64> {
        self.entropy.get(&track).copied()
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn secs(t: Timestamp) -> f64 {
    t.as_unix_nanos() as f64 / 1e9
}

fn subject_extent(s: &BaselineSubject) -> (f64, f64) {
    match s {
        BaselineSubject::Cell { index } => {
            let w = SCHEME_1_CELL_HZ * f64::from(CELL_FACTOR);
            (*index as f64 * w, (*index + 1) as f64 * w)
        }
        BaselineSubject::Channel { key } => (
            key.lo_cell as f64 * SCHEME_1_CELL_HZ,
            key.hi_cell as f64 * SCHEME_1_CELL_HZ,
        ),
    }
}

fn site_json(s: &SiteRecord) -> Value {
    json!({
        "id": s.id.to_string(),
        "name": s.name,
        "lat_deg": s.lat_deg,
        "lon_deg": s.lon_deg,
        "radius_m": s.radius_m,
        "utc_offset_min": s.utc_offset_min,
        "source": s.source,
        "first_seen": secs(s.first_seen),
        "last_seen": secs(s.last_seen),
        "observed_s": s.observed_s,
    })
}

fn opt_finite(v: Option<f64>) -> Value {
    v.filter(|x| x.is_finite())
        .map_or(Value::Null, |x| json!(x))
}

fn pool_json(p: &PoolStats, resolution: BaselineResolution) -> Value {
    json!({
        "resolution": resolution,
        "n": p.n,
        "observed_s": p.observed_s,
        "mean_db": opt_finite(p.mean_db()),
        "std_db": opt_finite(p.std_db()),
        "fco": opt_finite(p.fco()),
        "max_db": opt_finite(Some(p.max_db)),
    })
}

/// T-132: default bound on the loaded baselines (ADR-0012 §9); `HK_BASELINE_MEMORY_MB` overrides
/// it (`0` = unbounded).
pub const DEFAULT_BASELINE_MEMORY_CAP_BYTES: usize = 256 << 20;

fn baseline_memory_cap() -> Option<usize> {
    match std::env::var("HK_BASELINE_MEMORY_MB")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
    {
        Some(0) => None,
        Some(mb) => Some(mb.saturating_mul(1 << 20)),
        None => Some(DEFAULT_BASELINE_MEMORY_CAP_BYTES),
    }
}

/// T-132: the gain-state key of each folded channel row: one key for all (`u32`) or per channel
/// (gains are per band; a channel missing from the map is unknown, 0).
pub trait SubjectGainKeys {
    /// Key of `subject`.
    fn gain_key(&self, subject: &OccupancySubject) -> u32;
}

impl SubjectGainKeys for u32 {
    fn gain_key(&self, _: &OccupancySubject) -> u32 {
        *self
    }
}

impl SubjectGainKeys for BTreeMap<ChannelKey, u32> {
    fn gain_key(&self, subject: &OccupancySubject) -> u32 {
        match subject {
            OccupancySubject::Channel { key } => self.get(key).copied().unwrap_or(0),
            OccupancySubject::Band { .. } => 0,
        }
    }
}

impl AttentionService {
    /// Opens the service over `data_dir/baselines` and the run database (sites and weights).
    ///
    /// `chain` (T-303) is the front end whose measurements this service folds; its baselines are
    /// keyed by it, so another front end at the same site accrues its own noise floor instead of
    /// averaging into this one's.
    pub fn open(
        data_dir: &Path,
        repo: Arc<Mutex<Repository>>,
        chain: ChainKey,
        counters: Option<Arc<Counters>>,
        clock: Arc<dyn Fn() -> Timestamp + Send + Sync>,
    ) -> anyhow::Result<Self> {
        let (sites, weights) = {
            let r = lock(&repo);
            (r.sites()?, r.score_weights()?)
        };
        let store = BaselineStore::open(data_dir.join("baselines"))?;
        Ok(Self::with_store(
            repo,
            sites,
            weights,
            Some(store),
            chain,
            counters,
            clock,
        ))
    }

    /// T-128: the run's service over `data_dir` and its own connection to `db_path`, stamped with
    /// the run's stream time (the wall clock before any frame) and its receive chain (T-303).
    pub fn open_for_run(
        data_dir: &Path,
        db_path: &Path,
        chain: ChainKey,
        counters: Arc<Counters>,
    ) -> anyhow::Result<Self> {
        let repo = Arc::new(Mutex::new(Repository::open(db_path)?));
        let clock_counters = Arc::clone(&counters);
        let clock = Arc::new(move || {
            let ns = clock_counters.stream_time_ns.load(Ordering::Relaxed);
            if ns > 0 {
                Timestamp::from_unix_nanos(ns)
            } else {
                Timestamp::now()
            }
        });
        Self::open(data_dir, repo, chain, Some(counters), clock)
    }

    /// T-128: a memory-only service (no baseline store, in-memory database), for a scheduler
    /// driven without a run (tests).
    pub fn in_memory() -> anyhow::Result<Self> {
        let repo = Arc::new(Mutex::new(Repository::open_in_memory()?));
        let (sites, weights) = {
            let r = lock(&repo);
            (r.sites()?, r.score_weights()?)
        };
        Ok(Self::with_store(
            repo,
            sites,
            weights,
            None,
            ChainKey::Unknown,
            None,
            Arc::new(|| Timestamp::from_unix_nanos(0)),
        ))
    }

    fn with_store(
        repo: Arc<Mutex<Repository>>,
        sites: Vec<SiteRecord>,
        weights: ScoreWeights,
        store: Option<BaselineStore>,
        chain: ChainKey,
        counters: Option<Arc<Counters>>,
        clock: Arc<dyn Fn() -> Timestamp + Send + Sync>,
    ) -> Self {
        let classes = Arc::new(ClassCache::new(Arc::new(RepoClasses(Arc::clone(&repo)))));
        let class_tx = Mutex::new(classes.spawn_worker());
        // T-136: the stored assignment (a pin, or a fixed site within its hold) survives a restart.
        let mut assigner = SiteAssigner::new(SiteConfig::default(), sites);
        {
            let r = lock(&repo);
            // A stored assignment whose site row is gone is stale: drop it.
            if let Ok(Some(a)) = r.site_assignment()
                && assigner.restore(a).is_err()
            {
                let _ = r.set_site_assignment(None);
            }
        }
        Self {
            classes,
            class_tx,
            repo,
            sites: Mutex::new(assigner),
            baselines: Mutex::new({
                let b = Baselines::new(BaselineConfig::default(), 1, CELL_FACTOR, store);
                match baseline_memory_cap() {
                    Some(cap) => b.with_memory_cap(cap),
                    None => b,
                }
            }),
            scorer: Mutex::new(Scorer::default()),
            weights: Mutex::new(weights),
            provider: Arc::new(SharedInterestingness::default()),
            counters,
            clock,
            chain,
            cands: Mutex::new(CandidateState::default()),
        }
    }

    /// The provider the scheduler reads (T-120).
    pub fn provider(&self) -> Arc<SharedInterestingness> {
        Arc::clone(&self.provider)
    }

    fn bump(&self, f: impl FnOnce(&Counters)) {
        if let Some(c) = &self.counters {
            f(c);
        }
    }

    /// T-132: publishes the baseline memory bound's counters and gauge (`/api/status`).
    fn sync_baseline_gauges(&self, b: &Baselines) {
        self.bump(|c| {
            let a = &c.attention;
            a.refused_folds.store(b.refused_folds(), Ordering::Relaxed);
            a.unloaded_engines
                .store(b.unloaded_engines(), Ordering::Relaxed);
            a.gain_overflow_folds
                .store(b.gain_overflow_folds(), Ordering::Relaxed);
            a.memory_bytes
                .store(b.memory_bytes() as u64, Ordering::Relaxed);
        });
    }

    /// T-132: replaces the baseline memory cap (`None` = unbounded); applies from the next fold.
    pub fn set_baseline_memory_cap(&self, bytes: Option<usize>) {
        lock(&self.baselines).set_memory_cap(bytes);
    }

    /// Writes changed site records, then (T-136) the assignment in force when it changed, so a
    /// restart restores it (the records first: the assignment references its site).
    fn persist_sites(&self, sites: &mut SiteAssigner) {
        let dirty = sites.take_dirty();
        let assignment = sites.take_assignment_change();
        if dirty.is_empty() && assignment.is_none() {
            return;
        }
        let repo = lock(&self.repo);
        let mut errors = 0u64;
        for s in dirty {
            errors += u64::from(repo.upsert_site(&s).is_err());
        }
        if let Some(a) = assignment {
            errors += u64::from(repo.set_site_assignment(a.as_ref()).is_err());
        }
        if errors > 0 {
            self.bump(|c| {
                c.attention.errors.fetch_add(errors, Ordering::Relaxed);
            });
        }
    }

    /// T-136: the site assignment at sample time `t` **without** advancing the state machine: no
    /// expiry, no cleared fixes, no database write. For readers that run ahead of the occupancy
    /// close (history frames); only [`Self::site_at`] on the close path advances site state.
    pub fn site_at_peek(&self, t: Timestamp) -> SiteKey {
        lock(&self.sites).peek(t)
    }

    /// T-131: the site assignment at sample time `t` (the state machine ticked to `t`), stamped on
    /// the occupancy rows of an interval closing at `t`, and its geometry when known. This
    /// advances (and may persist) site state irreversibly: call it only from the occupancy close.
    pub fn site_at(&self, t: Timestamp) -> (SiteKey, Option<hk_context::geo::Site>) {
        let mut sites = lock(&self.sites);
        let before = sites.current();
        let key = sites.tick(t);
        let geo = match key {
            SiteKey::Site(id) => sites.site(id).and_then(|s| match (s.lat_deg, s.lon_deg) {
                (Some(lat), Some(lon)) => Some(hk_context::geo::Site::new(lat, lon)),
                _ => None,
            }),
            _ => None,
        };
        // Observation accounting persists in `observe`; here only a site change is written.
        if key != before {
            self.persist_sites(&mut sites);
        }
        (key, geo)
    }

    /// Folds a C06 fix.
    pub fn on_fix(&self, fix: Fix) -> SiteKey {
        let mut sites = lock(&self.sites);
        let key = sites.on_fix(fix);
        self.persist_sites(&mut sites);
        key
    }

    /// Folds one prepared observation under the current site.
    pub fn observe(&self, cal: CalKey, obs: &IntervalObservation) -> Option<FoldOutcome> {
        let (site, offset) = {
            let mut sites = lock(&self.sites);
            let site = sites.tick(obs.t);
            if site.accrues_baseline() {
                sites.record_observation(obs.t, obs.observed_s);
            }
            (site, sites.utc_offset_min(site))
        };
        let out = {
            let mut b = lock(&self.baselines);
            let out = b.observe(site, offset, cal, self.chain, FOLD_BIAS_TEE, obs);
            self.sync_baseline_gauges(&b);
            if let Ok(n) = b.flush(obs.t, false) {
                self.bump(|c| {
                    c.attention
                        .baseline_writes
                        .fetch_add(n as u64, Ordering::Relaxed);
                });
            }
            out
        };
        let mut sites = lock(&self.sites);
        self.persist_sites(&mut sites);
        match out {
            Ok(o) => {
                self.bump(|c| {
                    c.attention.folds.fetch_add(1, Ordering::Relaxed);
                    if o.novelty.novelty > 0.0 {
                        c.attention.novel_folds.fetch_add(1, Ordering::Relaxed);
                    }
                    if o.change_point.is_some() {
                        c.attention.change_points.fetch_add(1, Ordering::Relaxed);
                    }
                });
                Some(o)
            }
            Err(_) => {
                self.bump(|c| {
                    c.attention.errors.fetch_add(1, Ordering::Relaxed);
                });
                None
            }
        }
    }

    /// T-118 adapter: folds one `OccupancyStat` row. The row's own `site` is the assignment at
    /// measurement time; a row whose site does not accrue folds nothing.
    pub fn ingest_occupancy(&self, stat: &OccupancyStat, gain: u32) -> Option<FoldOutcome> {
        self.fold_row(stat, gain).map(|f| f.fold)
    }

    /// One channel row's fold with the alarm context (T-131).
    fn fold_row(&self, stat: &OccupancyStat, gain: u32) -> Option<IntervalFold> {
        let (site, cal, obs) = from_occupancy_stat(stat, gain)?;
        let offset = lock(&self.sites).utc_offset_min(site);
        let (out, pool_fcos, pool) = {
            let mut b = lock(&self.baselines);
            let out = b
                .observe(site, offset, cal, self.chain, FOLD_BIAS_TEE, &obs)
                .ok();
            self.sync_baseline_gauges(&b);
            let sub = b
                .key(site, cal, self.chain, FOLD_BIAS_TEE)
                .and_then(|key| b.engines().find(|e| e.state.key == key))
                .and_then(|e| e.state.subjects.get(&obs.subject));
            let slot = HourOfWeek::of(obs.t, offset);
            let pool_fcos = sub
                .map(|sub| mature_pool_fcos(sub, slot))
                .unwrap_or_default();
            // The reference pool the fold's novelty used (its finest mature resolution).
            let pool = match (sub, out.as_ref().map(|o| o.novelty.maturity)) {
                (Some(sub), Some(Maturity::Mature { resolution })) => {
                    let (level, occ) = pools(sub, slot, None, BaselineCopy::Reference);
                    let ri = res_index(resolution);
                    PoolContext {
                        resolution: Some(resolution),
                        level: level[ri]
                            .mean_db()
                            .map(|m| (m, level[ri].std_db().unwrap_or(1.0).max(1.0))),
                        occupancy: occ[ri]
                            .fco()
                            .map(|p| (p, (p * (1.0 - p) / obs.n_eff.max(1.0)).sqrt().max(1e-6))),
                    }
                }
                _ => PoolContext::default(),
            };
            if let Ok(n) = b.flush(obs.t, false) {
                self.bump(|c| {
                    c.attention
                        .baseline_writes
                        .fetch_add(n as u64, Ordering::Relaxed);
                });
            }
            (out, pool_fcos, pool)
        };
        let o = out?;
        self.bump(|c| {
            c.attention.folds.fetch_add(1, Ordering::Relaxed);
            if o.novelty.novelty > 0.0 {
                c.attention.novel_folds.fetch_add(1, Ordering::Relaxed);
            }
            if o.change_point.is_some() {
                c.attention.change_points.fetch_add(1, Ordering::Relaxed);
            }
        });
        // T-128: the channel's latest novelty and mature pools, for the candidates over it.
        if let BaselineSubject::Channel { key } = obs.subject {
            let (lo, hi) = subject_extent(&obs.subject);
            lock(&self.cands).channels.insert(
                key,
                (
                    FreqRange::new(lo, hi),
                    ChannelContext {
                        novelty: o.novelty,
                        pool_fcos,
                    },
                ),
            );
        }
        Some(IntervalFold {
            subject: stat.subject,
            site,
            cal,
            obs,
            utc_offset_min: offset,
            pool,
            fold: o,
        })
    }

    /// T-128: one closed occupancy interval from T-118's occupancy thread. Folds its channel rows
    /// ([`Self::ingest_occupancy`]), records `first_sightings` (inventory emitters first seen in
    /// the interval) over the interval's represented observation time into the site's
    /// first-sighting rate, and re-scores the candidates. The sightings accrue to the baseline
    /// rate only under a site that accrues baselines and when they are not themselves novel
    /// (< the alarm "on" level), so a burst of new emitters does not teach the rate. Returns the
    /// channel rows' folds with their alarm context (T-131: [`crate::alarms::AlarmService`]).
    pub fn ingest_interval(
        &self,
        rows: &[OccupancyStat],
        first_sightings: u64,
        t_end: Timestamp,
        gain: impl SubjectGainKeys,
    ) -> Vec<IntervalFold> {
        let mut folded = Vec::new();
        let (mut observed_s, mut accrues, mut site) = (0.0_f64, false, None);
        for r in rows {
            match r.subject {
                OccupancySubject::Band { .. } => {
                    observed_s = observed_s.max(represented_s(r));
                    accrues |= r.site.accrues_baseline();
                    site.get_or_insert(r.site);
                }
                OccupancySubject::Channel { .. } => {
                    if let Some(f) = self.fold_row(r, gain.gain_key(&r.subject)) {
                        folded.push(f);
                    }
                }
            }
        }
        if let Some(site) = site
            && observed_s > 0.0
        {
            let mut c = lock(&self.cands);
            c.sighting_site = Some(site);
            let s = &mut c.sightings.entry(site).or_default().rate;
            let novel = s.rate().is_some_and(|rate| {
                new_emitter_novelty(first_sightings, rate, observed_s)
                    >= REFERENCE_LEARN_MAX_NOVELTY
            });
            s.record(t_end, first_sightings, observed_s, accrues && !novel);
        }
        self.publish_candidates(t_end, true);
        folded
    }

    /// T-128: a survey report's change vs baseline (ADR-0012 §6.1). Each channel's 15-min (or
    /// hourly) rows are compared one by one against the reference pool of the row's **own**
    /// hour-of-week slot under `site` and the row's calibration (finest mature pool), and the
    /// per-interval z-scores are combined per subject (Stouffer: occupancy weighted by √n_eff,
    /// level equally), so a long span neither shrinks the variance nor collapses onto one slot.
    /// Rows longer than 1 h (a span rollup) are not comparable and are skipped. `no-baseline` for
    /// mobile/unassigned sites or when no row has a baseline subject; `immature` when subjects exist
    /// but none is mature; otherwise `available` with every level change (`level-above-baseline`,
    /// combined z ≥ 3, dB above the floor) and occupancy change with |combined z| ≥ 3
    /// (`busier-than-usual` when positive, `quieter-than-usual` when negative; FCO means weighted
    /// by the rows' observation weight), largest |z| first.
    pub fn compare_report(&self, site: SiteKey, rows: &[OccupancyStat]) -> BaselineComparison {
        let empty = |status| BaselineComparison {
            status,
            baseline: None,
            resolution: None,
            changes: Vec::new(),
        };
        let SiteKey::Site(id) = site else {
            return empty(ComparisonStatus::NoBaseline);
        };
        let Some(offset) = lock(&self.sites).site(id).map(|s| s.utc_offset_min) else {
            return empty(ComparisonStatus::NoBaseline);
        };
        // T-132: disk reads outside the baselines lock.
        if Baselines::load_site_outside_lock(&self.baselines, id, offset).is_err() {
            return empty(ComparisonStatus::Unavailable);
        }
        let b = lock(&self.baselines);
        let z_min = NoveltyConfig::default().z_min;
        /// One subject's per-interval evidence.
        #[derive(Default)]
        struct Acc {
            subject: Option<OccupancySubject>,
            occ_zw: Vec<(f64, f64)>,
            occ_base: f64,
            occ_obs: f64,
            occ_w: f64,
            lvl_zw: Vec<(f64, f64)>,
            lvl_base: f64,
            lvl_obs: f64,
        }
        let (mut key_used, mut finest) = (None, None);
        let mut subjects: BTreeMap<BaselineSubject, Acc> = BTreeMap::new();
        for r in rows {
            if r.interval.duration_ns() > COMPARE_ROW_MAX_NS {
                continue;
            }
            let Some((_, cal, obs)) = from_occupancy_stat(r, 0) else {
                continue;
            };
            let Some(key) = b.key(site, cal, self.chain, FOLD_BIAS_TEE) else {
                continue;
            };
            let Some(e) = b.engines().find(|e| e.state.key == key) else {
                continue;
            };
            let Some(sub) = e.state.subjects.get(&obs.subject) else {
                continue;
            };
            key_used.get_or_insert(key);
            let n = e.novelty_of(&obs);
            let Maturity::Mature { resolution } = n.maturity else {
                continue;
            };
            finest = Some(finest.map_or(resolution, |f: BaselineResolution| f.min(resolution)));
            let (level, occ) = pools(sub, e.slot(obs.t), None, BaselineCopy::Reference);
            let ri = res_index(resolution);
            let acc = subjects.entry(obs.subject).or_default();
            acc.subject.get_or_insert(r.subject);
            if let (Some(z), Some(l), Some(m)) = (n.level_z, obs.level_db, level[ri].mean_db()) {
                acc.lvl_zw.push((z, 1.0));
                acc.lvl_base += m;
                acc.lvl_obs += l;
            }
            if let (Some(z), Some(f), Some(p)) = (n.occupancy_z, obs.fco(), occ[ri].fco()) {
                acc.occ_zw.push((z, obs.n_eff.max(0.0).sqrt()));
                acc.occ_base += p * obs.weight_s;
                acc.occ_obs += f * obs.weight_s;
                acc.occ_w += obs.weight_s;
            }
        }
        let mut changes = Vec::new();
        for acc in subjects.values() {
            let Some(subject) = acc.subject else {
                continue;
            };
            if let Some(z) = stouffer(&acc.lvl_zw)
                && z >= z_min
            {
                let k = acc.lvl_zw.len() as f64;
                changes.push(ChangeEntry {
                    subject,
                    kind: AlarmKind::LevelAboveBaseline,
                    baseline: acc.lvl_base / k,
                    observed: acc.lvl_obs / k,
                    z,
                });
            }
            if let Some(z) = stouffer(&acc.occ_zw)
                && z.abs() >= z_min
                && acc.occ_w > 0.0
            {
                changes.push(ChangeEntry {
                    subject,
                    kind: if z > 0.0 {
                        AlarmKind::BusierThanUsual
                    } else {
                        AlarmKind::QuieterThanUsual
                    },
                    baseline: acc.occ_base / acc.occ_w,
                    observed: acc.occ_obs / acc.occ_w,
                    z,
                });
            }
        }
        changes.sort_by(|a, b| b.z.abs().total_cmp(&a.z.abs()));
        let status = match (finest, key_used) {
            (Some(_), _) => ComparisonStatus::Available,
            (None, Some(_)) => ComparisonStatus::Immature,
            (None, None) => ComparisonStatus::NoBaseline,
        };
        if status != ComparisonStatus::Available {
            changes.clear();
        }
        BaselineComparison {
            status,
            baseline: key_used,
            resolution: finest,
            changes,
        }
    }

    /// T-128: the current site's new-emitter novelty (the site of the latest closed interval;
    /// `None` while its rate is immature).
    pub fn new_emitter_novelty(&self) -> Option<f64> {
        lock(&self.cands).current_new_emitter()
    }

    /// T-136: the emitters behind a close's first sightings under `site` (their count goes to
    /// [`Self::ingest_interval`]); [`Self::new_emitter_inputs`] turns them into alarm inputs.
    pub fn note_first_sightings(&self, site: SiteKey, sightings: &[FirstSighting]) {
        if sightings.is_empty() {
            return;
        }
        let mut c = lock(&self.cands);
        let st = c.sightings.entry(site).or_default();
        st.recent.extend(sightings.iter().copied());
        // T-138: only a discrete site can hold the persistent single-emitter rule (§7.3: mobile and
        // unassigned sites never alarm).
        if matches!(site, SiteKey::Site(_)) {
            for s in sightings {
                st.pending.entry(s.emitter).or_insert(PendingEmitter {
                    freq: s.freq,
                    sighted: s.t,
                    disqualified: false,
                    armed: false,
                    zeros: 0,
                    streak: 0,
                    spent: false,
                });
            }
            while st.pending.len() > PERSIST_MAX_PENDING {
                // The oldest unarmed sighting first (an armed one may still have to clear).
                let Some(id) = st
                    .pending
                    .iter()
                    .min_by_key(|(_, p)| (p.armed, p.sighted))
                    .map(|(id, _)| *id)
                else {
                    break;
                };
                st.pending.remove(&id);
            }
        }
    }

    /// T-138: whether `site` holds first sightings the persistent single-emitter rule still tracks
    /// (the close then reports [`Self::note_emitters_seen`]).
    pub fn has_pending_sightings(&self, site: SiteKey) -> bool {
        lock(&self.cands)
            .sightings
            .get(&site)
            .is_some_and(|s| !s.pending.is_empty())
    }

    /// T-138: whether the persistent single-emitter rule can score at `site` now (a discrete site
    /// whose mature rate puts the rule score at or above the alarm off level). The close runs its
    /// churn look-back only then.
    pub fn persistent_rule_active(&self, site: SiteKey) -> bool {
        matches!(site, SiteKey::Site(_))
            && lock(&self.cands)
                .sightings
                .get(&site)
                .and_then(|s| s.rate.rate())
                .is_some_and(|r| {
                    persistent_single_novelty(r) >= AlarmConfig::default().hysteresis.off
                })
    }

    /// T-138: what the close of `iv` under `site` observed, for the persistent single-emitter rule
    /// in [`Self::new_emitter_inputs`] at the same close: the observed bands (with their bin
    /// widths), the inventory emitters seen in the interval (with their suspect state) and the
    /// first sightings that are churn (their extent overlaps another emitter seen within
    /// [`PERSIST_HORIZON_S`]: a track split or drift, never scored).
    pub fn note_emitters_seen(
        &self,
        site: SiteKey,
        iv: TimeRange,
        observed: &[ObservedBand],
        seen: &[EmitterSeen],
        churned: &[EmitterId],
    ) {
        let mut c = lock(&self.cands);
        if let Some(st) = c.sightings.get_mut(&site)
            && !st.pending.is_empty()
        {
            st.seen = Some(SeenAtClose {
                iv,
                observed: observed.to_vec(),
                seen: seen.to_vec(),
                churned: churned.to_vec(),
            });
        }
    }

    /// T-136 (ADR-0012 §7.1): the new-emitter alarm inputs at a close ending `t_end` under `site`,
    /// after [`Self::ingest_interval`] recorded its first sightings. Only `site`'s own sightings
    /// are read. The emitters seen within two windows ([`FirstSightingRate::WINDOW_S`]) are
    /// grouped as the alarm engine merges cells (extents within its merge gap); each group is
    /// scored on its **own** first sightings in the window, as the Poisson tail against the count
    /// the site's usual rate expects in the window (the whole site's expectation, so a group is
    /// never more novel than the site and a lone sighting scores as before). Every emitter in the
    /// window carries its group's novelty; once it leaves the window it is fed for one more
    /// window with novelty 0 so an open alarm clears. The subject is the emitter's measured extent
    /// as scheme-1 level-0 cells, so the engine merges a burst on one channel into one alarm.
    /// `fresh` marks sightings from the interval this close ends (counted once); while the rate is
    /// immature only fresh inputs are returned: counted as `immature-baseline` suppressions,
    /// never raised (§7.3).
    ///
    /// T-138 (§7.1 rule "single persistent new emitter"): on a discrete site with a mature rate,
    /// a first sighting tracked for [`PERSIST_HORIZON_S`] scores [`persistent_single_novelty`] at
    /// closes that saw it clean ([`Self::note_emitters_seen`]: its sighting close, or a later close
    /// once the inventory has seen it at least one full interval after its first sighting), and 0
    /// at a close whose band resolves it ([`PERSIST_COVER_MAX_RBW_RATIO`]) without it once scored.
    /// The usual 2-interval hysteresis then raises only when it was seen in two consecutive closes
    /// of the site (the same visit, or the next visit's first close), so a one-off transient or
    /// one transmission shorter than an interval never raises, and a busy site (P(≥1 | μ) > α)
    /// never reaches the on level. One-shot: once its streak reaches the raise count it is never
    /// scored again (zeros clear the alarm). Suspect and churn sightings are never scored. The
    /// emitter's input takes the larger of the two scores.
    pub fn new_emitter_inputs(&self, site: SiteKey, t_end: Timestamp) -> Vec<NewEmitterInput> {
        let offset = lock(&self.sites).utc_offset_min(site);
        let mut c = lock(&self.cands);
        let Some(st) = c.sightings.get_mut(&site) else {
            return Vec::new();
        };
        let window_ns = (FirstSightingRate::WINDOW_S * 1e9) as i64;
        let end = t_end.as_unix_nanos();
        let since = st.last_inputs_end.replace(t_end).map(|t| t.as_unix_nanos());
        st.recent
            .retain(|s| s.t.as_unix_nanos() > end.saturating_sub(2 * window_ns));
        let seen = st.seen.take().filter(|s| s.iv.end == t_end);
        if st.recent.is_empty() && st.pending.is_empty() {
            return Vec::new();
        }
        let rate = st.rate.rate();
        let (_, window_s) = st.rate.window_totals();
        let maturity = match rate {
            Some(_) => Maturity::Mature {
                resolution: BaselineResolution::AllHours,
            },
            None => Maturity::Immature {
                observed_s: st.rate.baseline_observed_s(),
            },
        };
        let expected = rate.map_or(0.0, |r| r * window_s);
        let slot = HourOfWeek::of(t_end, offset);
        let in_window = |s: &FirstSighting| s.t.as_unix_nanos() > end.saturating_sub(window_ns);
        let mut cells: Vec<(i64, i64, FirstSighting)> = st
            .recent
            .iter()
            .map(|s| {
                let lo = (s.freq.lo_hz / SCHEME_1_CELL_HZ).floor() as i64;
                let hi = ((s.freq.hi_hz / SCHEME_1_CELL_HZ).ceil() as i64).max(lo + 1);
                (lo, hi, *s)
            })
            .collect();
        cells.sort_by_key(|c| (c.0, c.1));
        let cfg = AlarmConfig::default();
        let gap = cfg.merge_gap_cells * cfg.cell_factor;
        let mut out = Vec::new();
        // T-138: the input index of each emitter fed by the group rule.
        let mut by_emitter: HashMap<EmitterId, usize> = HashMap::new();
        let mut i = 0;
        while i < cells.len() {
            let (mut j, mut group_hi) = (i + 1, cells[i].1);
            while j < cells.len() && cells[j].0 - group_hi <= gap {
                group_hi = group_hi.max(cells[j].1);
                j += 1;
            }
            let group = &cells[i..j];
            let k = group.iter().filter(|c| in_window(&c.2)).count() as u64;
            let novelty = rate.map_or(0.0, |r| new_emitter_novelty(k, r, window_s));
            for &(lo, hi, s) in group {
                let live = in_window(&s);
                let t = s.t.as_unix_nanos();
                let fresh = since.is_none_or(|p| t > p) && t <= end;
                // An immature rate never raises: its inputs are only counted, once, when fresh
                // (so out of the window there is nothing to clear either).
                if rate.is_none() && !(live && fresh) {
                    continue;
                }
                let mut input = new_emitter_input(
                    s.emitter,
                    s.freq,
                    CalKey::Uncalibrated,
                    slot,
                    maturity,
                    k,
                    expected,
                    if live { novelty } else { 0.0 },
                    window_s,
                );
                input.subject = AlarmSubject::Cells {
                    scheme: 1,
                    lo_cell: lo,
                    hi_cell: hi,
                };
                by_emitter.insert(s.emitter, out.len());
                out.push(NewEmitterInput { input, fresh });
            }
            i = j;
        }
        // T-138: the persistent single-emitter rule.
        let (interval_ns, observed, seen, churned) = seen
            .map_or((i64::MAX, Vec::new(), Vec::new(), Vec::new()), |s| {
                (s.iv.duration_ns().max(1), s.observed, s.seen, s.churned)
            });
        let score = rate
            .filter(|_| matches!(site, SiteKey::Site(_)))
            .map(persistent_single_novelty);
        let mu = rate.map_or(0.0, |r| r * FirstSightingRate::WINDOW_S);
        let horizon_ns = (PERSIST_HORIZON_S * 1e9) as i64;
        let hcfg = cfg.hysteresis;
        let clear_after = hcfg.off_intervals;
        st.pending.retain(|id, p| {
            let expired = end.saturating_sub(p.sighted.as_unix_nanos()) > horizon_ns;
            let here = seen.iter().find(|e| e.emitter == *id);
            if here.is_some_and(|e| e.suspect) || churned.contains(id) {
                p.disqualified = true;
            }
            // Its sighting close, or a later close once the inventory saw it a full interval after
            // its first sighting: one transmission shorter than an interval that straddles a
            // boundary shows at two closes but never confirms at the second.
            let confirms = here.is_some_and(|e| {
                !e.suspect
                    && (p.sighted == t_end
                        || e.last_seen
                            .as_unix_nanos()
                            .saturating_sub(e.first_seen.as_unix_nanos())
                            >= interval_ns)
            });
            let width = p.freq.hi_hz - p.freq.lo_hz;
            let covered = here.is_some()
                || observed.iter().any(|b| {
                    b.freq.lo_hz < p.freq.hi_hz
                        && b.freq.hi_hz > p.freq.lo_hz
                        && b.rbw_hz <= PERSIST_COVER_MAX_RBW_RATIO * width
                });
            let value = match score {
                // A score below `off` is cold: it could neither raise nor hold, so it arms nothing
                // (a busy site's sightings feed no zero inputs later).
                Some(n) if confirms && !p.disqualified && !p.spent && !expired && n >= hcfg.off => {
                    p.armed = true;
                    p.zeros = 0;
                    if n >= hcfg.on {
                        p.streak += 1;
                        p.spent = p.streak >= hcfg.on_intervals;
                    } else {
                        p.streak = 0;
                    }
                    Some(n)
                }
                _ if p.armed && covered && p.zeros < clear_after => {
                    p.zeros = p.zeros.saturating_add(1);
                    p.streak = 0;
                    Some(0.0)
                }
                _ => {
                    // The group rule's own (sub-on) input steps the key: the engine's streak resets.
                    if by_emitter.contains_key(id) {
                        p.streak = 0;
                    }
                    None
                }
            };
            if let Some(v) = value {
                match by_emitter.get(id) {
                    Some(&i) => {
                        let input = &mut out[i].input;
                        input.novelty = input.novelty.max(v);
                    }
                    None => {
                        let lo = (p.freq.lo_hz / SCHEME_1_CELL_HZ).floor() as i64;
                        let hi = ((p.freq.hi_hz / SCHEME_1_CELL_HZ).ceil() as i64).max(lo + 1);
                        let mut input = new_emitter_input(
                            *id,
                            p.freq,
                            CalKey::Uncalibrated,
                            slot,
                            maturity,
                            1,
                            mu,
                            v,
                            FirstSightingRate::WINDOW_S,
                        );
                        input.subject = AlarmSubject::Cells {
                            scheme: 1,
                            lo_cell: lo,
                            hi_cell: hi,
                        };
                        out.push(NewEmitterInput {
                            input,
                            fresh: false,
                        });
                    }
                }
            }
            // Kept while it can still confirm, and an armed one until it has fed enough zeros to
            // clear an open alarm.
            (!expired && !p.disqualified && !p.spent) || (p.armed && p.zeros < clear_after)
        });
        out
    }

    /// T-128: a member detection of `track` (control thread). Returns whether the track is new
    /// and its latest scored novelty.
    pub fn on_track_member(&self, track: TrackId, m: &MemberEvidence) -> (bool, f64) {
        lock(&self.cands).table.on_member(track, m)
    }

    /// T-174: a suspect member of `track` had its DC flag refuted by a clean twin (§2.6).
    pub fn on_track_member_refuted(&self, track: TrackId) {
        lock(&self.cands).table.on_member_refuted(track);
    }

    /// T-128: the detector confirmed `track`.
    pub fn on_track_confirmed(&self, track: TrackId, freq: FreqRange, recipe_match: bool) {
        lock(&self.cands)
            .table
            .on_confirmed(track, freq, recipe_match);
    }

    /// T-128: a verification group ended (any verdict): the next pass publishes, so the bandit
    /// sees whether the candidate is still flagged (ban) or was cleared.
    pub fn request_publish(&self) {
        lock(&self.cands).table.mark_changed();
    }

    /// T-128: `track` closed. It stays a candidate, with a decaying confidence, until the
    /// re-check horizon retires it (T-251; `crate::candidates` module docs).
    pub fn on_track_closed(&self, track: TrackId) {
        lock(&self.cands).table.on_closed(track);
    }

    /// T-251: the reading's idle gap, derived from the scheduler's revisit period. It sets how
    /// fast a stopped candidate's confidence decays and how long it is kept as a re-check
    /// request. Unset it is the conservative 60 s, which claims no absence that was not observed.
    pub fn set_idle_gap(&self, gap: hk_model::IdleGap) {
        lock(&self.cands).table.set_idle_gap(gap);
    }

    /// T-128: a trust-test verdict reached the candidate with bandit key `key`.
    pub fn on_trust_tested(&self, key: u64) -> Option<TrackId> {
        lock(&self.cands).table.on_trust_tested(key)
    }

    /// T-128: decodes per track (the segment's table).
    pub(crate) fn set_track_decodes(&self, decodes: Arc<TrackDecodes>) {
        lock(&self.cands).decodes = Some(decodes);
    }

    /// T-128: builds the candidates from the table and publishes them through [`Self::score`] when
    /// a pass is due (every 10 s of sample clock), at once when a confirmed track appeared or
    /// closed or when `force`. Returns the published version.
    pub fn publish_candidates(&self, t: Timestamp, force: bool) -> Option<u64> {
        let inputs = {
            let mut c = lock(&self.cands);
            let urgent = c.table.take_changed() || force;
            {
                let mut s = lock(&self.scorer);
                if urgent {
                    s.expire();
                } else if !s.due(t, false) {
                    return None;
                }
            }
            let tracks = c.table.confirmed();
            self.request_class_refresh(&tracks, t);
            let snapshot = Arc::clone(&*lock(&self.classes.snapshot));
            let entropy = class_entropies(&snapshot, &tracks);
            let new_emitter = c.current_new_emitter();
            let CandidateState {
                table,
                channels,
                decodes,
                ..
            } = &mut *c;
            let src = PassEvidence {
                channels,
                new_emitter,
                decodes: decodes.as_deref(),
                entropy,
            };
            table.inputs(t.as_unix_nanos(), &src)
        };
        self.score(t, false, &inputs)
    }

    /// Asks the class-entropy worker for a fresh snapshot over the confirmed tracks' extent (the
    /// last day's classified emitters) when none is in flight and the last request is at least
    /// [`CLASS_REFRESH_NS`] of sample clock old. Never blocks: a full queue drops the request.
    fn request_class_refresh(&self, tracks: &[(TrackId, FreqRange)], t: Timestamp) {
        if tracks.is_empty() {
            return;
        }
        let now = t.as_unix_nanos();
        let last = self.classes.last_request_ns.load(Ordering::Relaxed);
        if last != i64::MIN && now >= last && now - last < CLASS_REFRESH_NS {
            return;
        }
        if self.classes.in_flight.swap(true, Ordering::AcqRel) {
            return;
        }
        let lo = tracks
            .iter()
            .map(|x| x.1.lo_hz)
            .fold(f64::INFINITY, f64::min);
        let hi = tracks
            .iter()
            .map(|x| x.1.hi_hz)
            .fold(f64::NEG_INFINITY, f64::max);
        let span = TimeRange::new(
            t.saturating_add_nanos(-CLASS_LOOKBACK_NS),
            t.saturating_add_nanos(1),
        );
        let region = Region::new(FreqRange::new(lo, hi), span);
        let sent = lock(&self.class_tx)
            .as_ref()
            .is_some_and(|tx| tx.try_send(region).is_ok());
        if sent {
            self.classes.last_request_ns.store(now, Ordering::Relaxed);
        } else {
            self.classes.in_flight.store(false, Ordering::Release);
        }
    }

    /// Class-entropy snapshot refreshes completed (tests, diagnostics).
    pub fn class_refreshes(&self) -> u64 {
        self.classes.refreshes.load(Ordering::Relaxed)
    }

    /// Replaces the class-entropy source (tests).
    pub fn set_class_source(&self, source: Arc<dyn ClassSource>) {
        *lock(&self.classes.source) = source;
    }

    /// Scores `inputs` at `t` and publishes when due and changed.
    pub fn score(&self, t: Timestamp, low_power: bool, inputs: &[CandidateInput]) -> Option<u64> {
        let site = lock(&self.sites).current();
        let w = *lock(&self.weights);
        let published = lock(&self.scorer)
            .run(t, low_power, site, w, inputs, &self.provider)
            .ok()
            .flatten();
        if published.is_some() {
            self.bump(|c| {
                c.attention.publishes.fetch_add(1, Ordering::Relaxed);
            });
        }
        published
    }

    /// Saves every dirty baseline (shutdown / low battery).
    pub fn checkpoint(&self, t: Timestamp) {
        let _ = lock(&self.baselines).flush(t, true);
    }

    // ---- API views ----

    /// `GET /api/sites`.
    pub fn sites_json(&self) -> Value {
        let s = lock(&self.sites);
        json!({
            "sites": s.sites().iter().map(site_json).collect::<Vec<_>>(),
            "current": s.current(),
        })
    }

    /// `GET /api/sites/current`.
    pub fn current_site_json(&self) -> Value {
        let s = lock(&self.sites);
        let record = match s.current() {
            SiteKey::Site(id) => s.site(id).map_or(Value::Null, site_json),
            _ => Value::Null,
        };
        json!({
            "site": s.current(),
            "set_by": s.set_by(),
            "pinned": s.is_pinned(),
            "accrues_baseline": s.current().accrues_baseline(),
            "record": record,
        })
    }

    /// `PUT /api/sites/current`.
    pub fn set_current_site(&self, sel: SiteSelect) -> Result<Value, AttentionError> {
        let t = (self.clock)();
        {
            let mut sites = lock(&self.sites);
            if sel.release {
                sites.unpin();
            } else {
                let id = match (sel.id, &sel.name) {
                    (Some(id), None) => id,
                    (None, Some(name)) => {
                        match sites
                            .sites()
                            .iter()
                            .find(|s| s.name.as_deref() == Some(name))
                        {
                            Some(s) => s.id,
                            None => {
                                let record = SiteRecord {
                                    id: SiteId::new(),
                                    name: Some(name.clone()),
                                    lat_deg: sel.lat_deg,
                                    lon_deg: sel.lon_deg,
                                    radius_m: sel
                                        .radius_m
                                        .unwrap_or(SiteConfig::default().radius_m),
                                    utc_offset_min: sel.utc_offset_min.unwrap_or(0),
                                    source: SiteSource::User,
                                    first_seen: t,
                                    last_seen: t,
                                    observed_s: 0.0,
                                };
                                let id = record.id;
                                sites
                                    .upsert(record)
                                    .map_err(|e| AttentionError::invalid(e.to_string()))?;
                                id
                            }
                        }
                    }
                    _ => {
                        return Err(AttentionError::invalid(
                            "give exactly one of id or name (or release: true)",
                        ));
                    }
                };
                sites
                    .pin(id, SiteSource::User, t)
                    .map_err(|_| AttentionError::new(404, "not_found", "no such site"))?;
            }
            self.persist_sites(&mut sites);
        }
        Ok(self.current_site_json())
    }

    /// `PUT /api/sites/{id}`: rename (`Some(None)` clears) and/or set the UTC offset.
    pub fn update_site(
        &self,
        id: SiteId,
        name: Option<Option<String>>,
        utc_offset_min: Option<i16>,
    ) -> Result<(Value, Value), AttentionError> {
        let mut sites = lock(&self.sites);
        let old = sites
            .site(id)
            .cloned()
            .ok_or_else(|| AttentionError::new(404, "not_found", "no such site"))?;
        if let Some(Some(n)) = &name
            && sites
                .sites()
                .iter()
                .any(|s| s.id != id && s.name.as_deref() == Some(n.as_str()))
        {
            return Err(AttentionError::new(
                409,
                "conflict",
                "another site has that name",
            ));
        }
        let mut new = old.clone();
        if let Some(n) = name {
            new.name = n;
        }
        if let Some(o) = utc_offset_min {
            new.utc_offset_min = o;
        }
        sites
            .upsert(new.clone())
            .map_err(|e| AttentionError::invalid(e.to_string()))?;
        self.persist_sites(&mut sites);
        Ok((site_json(&old), site_json(&new)))
    }

    fn site_or_current(&self, site: Option<SiteId>) -> Result<(SiteId, i16), AttentionError> {
        let s = lock(&self.sites);
        let id = match (site, s.current()) {
            (Some(id), _) | (None, SiteKey::Site(id)) => id,
            (None, _) => {
                return Err(AttentionError::new(
                    409,
                    "conflict",
                    "no current site: give site=<id> (mobile and unassigned build no baselines)",
                ));
            }
        };
        let offset = s
            .site(id)
            .ok_or_else(|| AttentionError::new(404, "not_found", "no such site"))?
            .utc_offset_min;
        Ok((id, offset))
    }

    /// `GET /api/baselines`.
    pub fn baselines_json(&self, site: Option<SiteId>) -> Result<Value, AttentionError> {
        let (id, offset) = self.site_or_current(site)?;
        // T-132: disk reads outside the baselines lock.
        Baselines::load_site_outside_lock(&self.baselines, id, offset)
            .map_err(|e| AttentionError::failed("baseline store", e))?;
        let b = lock(&self.baselines);
        // Maturity at the sample clock (ADR-0012 §0): the site's latest folded visit; the
        // service clock only for a site with no baseline yet.
        let now = b
            .engines()
            .filter(|e| e.state.key.site == id)
            .map(|e| e.state.last_visit)
            .max()
            .unwrap_or_else(|| (self.clock)());
        let slot = HourOfWeek::of(now, offset);
        let keys: Vec<Value> = b
            .engines()
            .filter(|e| e.state.key.site == id)
            .map(|e| {
                let mut finest: Option<BaselineResolution> = None;
                let mut mature = 0;
                let mut change_points = Vec::new();
                for (subject, sub) in &e.state.subjects {
                    if let Maturity::Mature { resolution } = maturity(sub, slot) {
                        mature += 1;
                        finest = Some(finest.map_or(resolution, |f| f.min(resolution)));
                    }
                    if let Some(cp) = sub.change_point {
                        let (lo, hi) = subject_extent(subject);
                        change_points.push(json!({
                            "subject": subject, "f_lo": lo, "f_hi": hi, "t": secs(cp.t),
                            "statistic": cp.statistic, "direction": cp.direction, "cusum": cp.cusum,
                        }));
                    }
                }
                json!({
                    "site": id.to_string(),
                    "cal": e.state.key.cal,
                    "chain": e.state.key.chain,
                    // T-333: always present, so a reader never has to infer which cohort a
                    // baseline belongs to. "unknown" is a cohort, not a missing value.
                    "bias_tee": e.state.key.bias_tee.as_str(),
                    "scheme": e.state.key.scheme,
                    "cell_factor": e.state.key.cell_factor,
                    "subjects": e.state.subjects.len(),
                    "mature_subjects": mature,
                    "finest_resolution": finest,
                    "last_visit": secs(e.state.last_visit),
                    "change_points": change_points,
                })
            })
            .collect();
        Ok(json!({ "site": id.to_string(), "slot": slot.index(), "baselines": keys }))
    }

    /// `GET /api/baselines/slots`.
    pub fn slots_json(
        &self,
        site: Option<SiteId>,
        f_lo: f64,
        f_hi: f64,
        slot: Option<HourOfWeek>,
        resolution: Option<BaselineResolution>,
    ) -> Result<Value, AttentionError> {
        let (id, offset) = self.site_or_current(site)?;
        let slot = slot.unwrap_or_else(|| HourOfWeek::of((self.clock)(), offset));
        // T-132: disk reads outside the baselines lock.
        Baselines::load_site_outside_lock(&self.baselines, id, offset)
            .map_err(|e| AttentionError::failed("baseline store", e))?;
        let b = lock(&self.baselines);
        let mut rows = Vec::new();
        let mut truncated = false;
        for e in b.engines().filter(|e| e.state.key.site == id) {
            for (subject, sub) in &e.state.subjects {
                let (lo, hi) = subject_extent(subject);
                if hi <= f_lo || lo >= f_hi {
                    continue;
                }
                if rows.len() >= SLOTS_MAX {
                    truncated = true;
                    break;
                }
                let m = maturity(sub, slot);
                let res = resolution.unwrap_or(match m {
                    Maturity::Mature { resolution } => resolution,
                    Maturity::Immature { .. } => BaselineResolution::AllHours,
                });
                let r = BaselineResolution::FINEST_FIRST
                    .iter()
                    .position(|x| *x == res)
                    .expect("listed");
                // Level from the most-visited gain state; occupancy over all.
                let gain = (0..sub.gains.len()).max_by_key(|gi| {
                    sub.gains[*gi]
                        .reference
                        .iter()
                        .filter(|(i, _)| in_pool(*i, slot, res))
                        .map(|(_, s)| s.n_visits)
                        .sum::<u64>()
                });
                let (rl, ro) = pools(sub, slot, gain, BaselineCopy::Reference);
                let (al, ao) = pools(sub, slot, gain, BaselineCopy::Adaptive);
                let merge = |l: &PoolStats, o: &PoolStats| l.with_occupancy_of(o);
                rows.push(json!({
                    "subject": subject,
                    "f_lo": lo,
                    "f_hi": hi,
                    "cal": e.state.key.cal,
                    "maturity": m,
                    "mixed": sub.mixed,
                    "gain_states": sub.gains.len(),
                    "reference": pool_json(&merge(&rl[r], &ro[r]), res),
                    "adaptive": pool_json(&merge(&al[r], &ao[r]), res),
                    "change_point": sub.change_point.map(|cp| json!({
                        "t": secs(cp.t), "statistic": cp.statistic,
                        "direction": cp.direction, "cusum": cp.cusum,
                    })),
                    "refrozen_at": sub.refrozen_at.map(secs),
                }));
            }
        }
        Ok(json!({
            "site": id.to_string(),
            "slot": slot.index(),
            "subjects": rows,
            "truncated": truncated,
        }))
    }

    /// `POST /api/baselines/refreeze`: copies adaptive → reference for the site's subjects in
    /// `[f_lo, f_hi)` (all when absent). Returns `(response, count)`.
    pub fn refreeze(
        &self,
        site: Option<SiteId>,
        f_lo: Option<f64>,
        f_hi: Option<f64>,
    ) -> Result<Value, AttentionError> {
        let (id, offset) = self.site_or_current(site)?;
        let t = (self.clock)();
        let (lo, hi) = (f_lo.unwrap_or(f64::MIN), f_hi.unwrap_or(f64::MAX));
        // T-132: disk reads outside the baselines lock.
        Baselines::load_site_outside_lock(&self.baselines, id, offset)
            .map_err(|e| AttentionError::failed("baseline store", e))?;
        let mut b = lock(&self.baselines);
        let mut n = 0;
        for e in b.engines_mut().filter(|e| e.state.key.site == id) {
            n += e.refreeze(t, |s| {
                let (a, z) = subject_extent(s);
                z > lo && a < hi
            });
        }
        // T-134: a re-freeze can grow the references (sparse slots).
        self.sync_baseline_gauges(&b);
        b.flush(t, true)
            .map_err(|e| AttentionError::failed("baseline store", e))?;
        Ok(json!({ "site": id.to_string(), "refrozen": n }))
    }

    /// `GET /api/candidates`.
    pub fn candidates_json(&self, f_lo: Option<f64>, f_hi: Option<f64>, limit: usize) -> Value {
        let set = self.provider.snapshot();
        let (lo, hi) = (f_lo.unwrap_or(f64::MIN), f_hi.unwrap_or(f64::MAX));
        let matching: Vec<_> = set
            .candidates
            .iter()
            .filter(|c| c.freq.hi_hz > lo && c.freq.lo_hz < hi)
            .collect();
        let truncated = matching.len() > limit;
        let candidates: Vec<Value> = matching
            .into_iter()
            .take(limit)
            .map(|c| {
                let mut v = serde_json::to_value(c).unwrap_or(Value::Null);
                if let Some(eta) = c.next_burst_eta {
                    v["next_burst_eta"] = json!(secs(eta));
                }
                v
            })
            .collect();
        json!({
            "version": self.provider.version(),
            "t": secs(set.t),
            "site": set.site,
            "weights": set.weights,
            "candidates": candidates,
            "truncated": truncated,
        })
    }

    /// `GET /api/attention/weights`.
    pub fn weights_json(&self) -> Result<Value, AttentionError> {
        let history = lock(&self.repo)
            .score_weights_history()
            .map_err(|e| AttentionError::failed("weights store", e))?;
        Ok(json!({
            "weights": *lock(&self.weights),
            "defaults": ScoreWeights::default(),
            "history": history.iter().map(|v| json!({
                "version": v.weights.version,
                "created": secs(v.created_at),
                "author": v.author,
            })).collect::<Vec<_>>(),
        }))
    }

    /// `PUT /api/attention/weights`: stores the next version; returns `(old, new)`. Takes effect at
    /// the next scoring pass.
    pub fn set_weights(
        &self,
        w: ScoreWeights,
        author: &str,
    ) -> Result<(ScoreWeights, ScoreWeights), AttentionError> {
        let probe = ScoreWeights { version: 1, ..w };
        probe
            .validate()
            .map_err(|e| AttentionError::invalid(e.to_string()))?;
        let new = lock(&self.repo)
            .insert_score_weights(&probe, author, (self.clock)())
            .map_err(|e| AttentionError::failed("weights store", e))?;
        let old = std::mem::replace(&mut *lock(&self.weights), new);
        Ok((old, new))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use hk_context::occupancy::score::BoringEvidence;
    use hk_model::attention::baseline::BaselineResolution;
    use hk_model::attention::score::InterestingnessProvider as _;
    use hk_model::attention::score::{CandidateSubject, NoveltyScore};
    use hk_model::region::FreqRange;
    use hk_store::baseline::BaselineSubject;

    use super::*;

    fn service(dir: &Path) -> AttentionService {
        let repo = Arc::new(Mutex::new(Repository::open_in_memory().unwrap()));
        AttentionService::open(
            dir,
            repo,
            ChainKey::Unknown,
            Some(Arc::new(Counters::default())),
            Arc::new(|| Timestamp::from_unix_nanos(7 * 86_400 * 1_000_000_000)),
        )
        .unwrap()
    }

    /// T-132 review: the service caps the loaded baselines by default, and a fold the cap refuses
    /// is visible in `/api/status` (`attention.refused_folds`, `memory_bytes`, …) while still being
    /// scored.
    #[test]
    fn attention_memory_cap_refusals_show_in_status() {
        let dir = std::env::temp_dir().join(format!("hk-att-cap-{}", SiteId::new()));
        let counters = Arc::new(Counters::default());
        let s = AttentionService::open(
            &dir,
            Arc::new(Mutex::new(Repository::open_in_memory().unwrap())),
            ChainKey::Unknown,
            Some(Arc::clone(&counters)),
            Arc::new(|| Timestamp::from_unix_nanos(7 * 86_400 * 1_000_000_000)),
        )
        .unwrap();
        if std::env::var_os("HK_BASELINE_MEMORY_MB").is_none() {
            assert_eq!(
                lock(&s.baselines).memory_cap(),
                Some(DEFAULT_BASELINE_MEMORY_CAP_BYTES)
            );
        }
        let cur = s
            .set_current_site(SiteSelect {
                name: Some("home".into()),
                ..SiteSelect::default()
            })
            .unwrap();
        let site = SiteKey::Site(cur["record"]["id"].as_str().unwrap().parse().unwrap());
        let key = ChannelKey {
            scheme: 1,
            lo_cell: 69_000,
            hi_cell: 69_004,
        };
        let row = series_row(0, site, OccupancySubject::Channel { key }, 0.1);
        s.ingest_occupancy(&row, 0).expect("scored");
        let a = counters.to_json()["attention"].clone();
        assert!(a["memory_bytes"].as_u64().unwrap() > 0, "{a}");
        assert_eq!(a["refused_folds"], 0, "{a}");
        // A cap below what is loaded: a new subject's fold is refused but still scored.
        s.set_baseline_memory_cap(Some(1));
        let other = ChannelKey {
            lo_cell: 70_000,
            hi_cell: 70_004,
            ..key
        };
        let row = series_row(1, site, OccupancySubject::Channel { key: other }, 0.1);
        let out = s
            .ingest_occupancy(&row, 0)
            .expect("refused folds are still scored");
        assert!(!out.accrued);
        let a = counters.to_json()["attention"].clone();
        println!("T-132 status after a refused fold: {a}");
        assert_eq!(a["refused_folds"], 1, "{a}");
        for field in ["unloaded_engines", "gain_overflow_folds", "memory_bytes"] {
            assert!(a[field].is_u64(), "{field}: {a}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A parked run of `hours` over 9 700 baseline cells (a 30–1000 MHz plan, ADR-0012 §3.1)
    /// under two front-end gain states, time-compressed to one 30-min fold per cell per gain state
    /// (memory depends on the slots touched, not the fold count). 30 % of the cells carry a
    /// daytime intermittent emitter, so they hold idle and occupied level series under both gain
    /// states (4 series), the rest idle only (2 series). Memory only: nothing can be unloaded, so
    /// the cap refuses growth. Returns the set and wall-clock seconds.
    fn parked_cells_run(hours: i64, cap: Option<usize>) -> (Baselines, f64) {
        const CELLS: i64 = 9_700;
        // Monday 00:00 UTC (1970-01-05) + 52 weeks.
        const T0_NS: i64 = (4 + 364) * 86_400 * 1_000_000_000;
        let site = SiteKey::Site(SiteId::new());
        let mut b = Baselines::new(BaselineConfig::default(), 1, 16, None);
        b.set_memory_cap(cap);
        let started = std::time::Instant::now();
        for h in 0..hours {
            for (half, gain) in [(0_i64, 0xA_u32), (1, 0xB)] {
                let t =
                    Timestamp::from_unix_nanos(T0_NS + (h * 3600 + half * 1800) * 1_000_000_000);
                for c in 0..CELLS {
                    let busy = c % 10 < 3;
                    let on = busy && (8..20).contains(&(h % 24)) && (c + h) % 3 != 0;
                    let jitter = ((c * 7 + h * 13 + half) % 11) as f64 * 0.1 - 0.5;
                    let obs = IntervalObservation {
                        subject: BaselineSubject::Cell { index: c },
                        t,
                        gain,
                        level_db: Some(if on { 15.0 } else { 2.0 } + jitter),
                        max_db: None,
                        occupied_weight_s: if on { 900.0 } else { 0.0 },
                        weight_s: 1800.0,
                        observed_s: 1800.0,
                        n_eff: 24.0,
                        suspect_fraction: 0.0,
                        provenance_explained: false,
                    };
                    b.observe(
                        site,
                        0,
                        CalKey::Uncalibrated,
                        ChainKey::Unknown,
                        FOLD_BIAS_TEE,
                        &obs,
                    )
                    .unwrap();
                }
            }
        }
        (b, started.elapsed().as_secs_f64())
    }

    /// T-134 (AWARE-042): baseline memory per key at 9 700 cells × 2 gain states, and the default
    /// memory cap on a parked 48 h run. Dense slots (T-132) held 369 453 672 B for the 2-series
    /// cells alone (measured with this generator at 2 h, when dense memory is already final), past
    /// the 256 MiB cap; sparse slots hold only the 48 observed hour-of-week slots per series.
    #[test]
    fn attention_baseline_default_cap_does_not_refuse_a_parked_48h_run_at_9700_cells() {
        let (b, secs) = parked_cells_run(48, Some(DEFAULT_BASELINE_MEMORY_CAP_BYTES));
        let subs: Vec<&SubjectBaseline> = b
            .engines()
            .flat_map(|e| e.state.subjects.values())
            .collect();
        let series: usize = subs.iter().map(|s| s.gains.len()).sum();
        let slots: usize = subs
            .iter()
            .flat_map(|s| &s.gains)
            .map(|g| g.adaptive.len())
            .sum();
        let per_key = b.memory_bytes();
        // ADR-0012 §3.1: ~56 B per copy per observed slot (both copies 112 B) for 9 700 cells
        // over all 168 hour-of-week slots.
        let adr_week_budget = 9_700 * HourOfWeek::SLOTS * 2 * 56;
        println!(
            "T-134 parked 48 h, 9700 cells x 2 gain states: {per_key} B per key ({:.1} MiB), \
             {series} series, {slots} stored slots ({:.1} B per stored slot incl. overhead), cap \
             {} B, {} folds refused, ADR full-week budget {adr_week_budget} B, {secs:.1} s",
            per_key as f64 / f64::from(1 << 20),
            per_key as f64 / slots as f64,
            DEFAULT_BASELINE_MEMORY_CAP_BYTES,
            b.refused_folds(),
        );
        assert_eq!(subs.len(), 9_700);
        assert_eq!(
            series,
            9_700 * 2 + 2_910 * 2,
            "idle A/B + busy occupied A/B"
        );
        assert_eq!(
            slots,
            9_700 * 2 * 48,
            "each cell × gain state × hour folds into one class series: 48 stored slots"
        );
        assert_eq!(b.refused_folds(), 0, "the default cap refuses nothing");
        assert!(
            per_key <= adr_week_budget,
            "{per_key} B > {adr_week_budget} B"
        );
    }

    /// Where a baseline set's bytes go (T-135 measurement): subject nodes, `seq_hod`, the series
    /// vectors, and each copy's stored slots (with growth slack) per level class.
    fn baseline_breakdown(b: &Baselines) -> String {
        use hk_store::baseline::{CusumState, GainSeries, LevelClass};
        let subs: Vec<&SubjectBaseline> = b
            .engines()
            .flat_map(|e| e.state.subjects.values())
            .collect();
        let node = subs.len()
            * (std::mem::size_of::<BaselineSubject>()
                + std::mem::size_of::<SubjectBaseline>()
                + 16);
        let hod: usize = subs
            .iter()
            .map(|s| s.seq_hod.capacity() * std::mem::size_of::<CusumState>())
            .sum();
        let vecs: usize = subs
            .iter()
            .map(|s| s.gains.capacity() * std::mem::size_of::<GainSeries>())
            .sum();
        let mut out = format!(
            "{} subjects: nodes {node} B, seq_hod {hod} B, series vecs {vecs} B",
            subs.len()
        );
        for class in [LevelClass::Idle, LevelClass::Occupied] {
            let gs: Vec<&GainSeries> = subs
                .iter()
                .flat_map(|s| &s.gains)
                .filter(|g| g.class == class)
                .collect();
            let (rl, rh) = gs.iter().fold((0, 0), |(l, h), g| {
                (l + g.reference.len(), h + g.reference.heap_bytes())
            });
            let (al, ah) = gs.iter().fold((0, 0), |(l, h), g| {
                (l + g.adaptive.len(), h + g.adaptive.heap_bytes())
            });
            out += &format!(
                "; {class:?}: {} series, reference {rl} slots {rh} B, adaptive {al} slots {ah} B",
                gs.len()
            );
        }
        out + &format!("; total {} B", b.memory_bytes())
    }

    /// T-135 (AWARE-042): a parked week (all 168 hour-of-week slots touched) at 9 700 cells × 2
    /// gain states stays under the default cap: no fold is refused.
    #[test]
    fn attention_baseline_default_cap_does_not_refuse_a_parked_week_at_9700_cells() {
        let (b, secs) = parked_cells_run(7 * 24, Some(DEFAULT_BASELINE_MEMORY_CAP_BYTES));
        println!(
            "T-135 parked 7 d, 9700 cells x 2 gain states: {} ({:.1} MiB), cap {} B, {} folds \
             refused, {secs:.1} s",
            baseline_breakdown(&b),
            b.memory_bytes() as f64 / f64::from(1 << 20),
            DEFAULT_BASELINE_MEMORY_CAP_BYTES,
            b.refused_folds(),
        );
        let slots: usize = b
            .engines()
            .flat_map(|e| e.state.subjects.values())
            .flat_map(|s| &s.gains)
            .map(|g| g.adaptive.len())
            .sum();
        assert_eq!(slots, 9_700 * 2 * 168, "every slot of the week is stored");
        assert_eq!(b.refused_folds(), 0, "the default cap refuses nothing");
        assert!(b.memory_bytes() <= DEFAULT_BASELINE_MEMORY_CAP_BYTES);
    }

    /// A 15-min T-118 row at `q` quarter-hours of `subject` under `site` (12 revisits, 75 s apart:
    /// 900 s represented) with FCO `fco`.
    pub(crate) fn series_row(
        q: i64,
        site: SiteKey,
        subject: OccupancySubject,
        fco: f64,
    ) -> OccupancyStat {
        let key = ChannelKey {
            scheme: 1,
            lo_cell: 69_000,
            hi_cell: 69_004,
        };
        let t0 = 7 * 86_400 * 1_000_000_000i64;
        let q_ns = 900 * 1_000_000_000i64;
        let json = serde_json::json!({
            "schema": 1,
            "site": {"kind": "mobile"},
            "subject": {"kind": "channel", "key": key},
            "interval": TimeRange::new(
                Timestamp::from_unix_nanos(t0 + q * q_ns),
                Timestamp::from_unix_nanos(t0 + (q + 1) * q_ns),
            ),
            "fco": fco, "fco_all_visits": fco,
            "n_revisits": 12, "n_occupied": (fco * 12.0).round() as u64, "n_suspect": 0,
            "n_revisits_all": 12, "observed_s": 6.0, "revisit_mean_s": 75.0,
            "timing": "statistical",
            "threshold": serde_json::to_value(
                hk_model::attention::occupancy::ThresholdSpec::default()
            ).unwrap(),
            "threshold_db": -95.0, "guard_clamped": false, "rbw_hz": 6250.0,
            "unit": "dbfs", "revisit_biased": false,
        });
        let mut r: OccupancyStat = serde_json::from_value(json).expect("OccupancyStat shape");
        r.site = site;
        r.subject = subject;
        if matches!(subject, OccupancySubject::Band { .. }) {
            r.fbo = Some(fco);
            r.sro = Some(fco);
        }
        r
    }

    /// T-128 item 4: inventory first sightings reach `FirstSightingRate` through the occupancy
    /// close (`ingest_interval`), so new-emitter novelty is produced, and a track first seen in
    /// the window carries it into the published candidates.
    #[test]
    fn attention_first_sightings_produce_new_emitter_novelty_for_recent_tracks() {
        let dir = std::env::temp_dir().join(format!("hk-t128-sight-{}", SiteId::new()));
        let s = service(&dir);
        let cur = s
            .set_current_site(SiteSelect {
                name: Some("home".into()),
                ..SiteSelect::default()
            })
            .unwrap();
        let id: SiteId = cur["record"]["id"].as_str().unwrap().parse().unwrap();
        let site = SiteKey::Site(id);
        let band = OccupancySubject::Band {
            freq: FreqRange::new(431e6, 433e6),
        };
        let t_end = |q: i64| {
            Timestamp::from_unix_nanos(7 * 86_400 * 1_000_000_000 + (q + 1) * 900_000_000_000)
        };
        // 30 h: one new emitter every 12 h is the site's usual rate.
        let mut before = None;
        for q in 0..120 {
            let k = u64::from(q % 48 == 0);
            s.ingest_interval(&[series_row(q, site, band, 0.1)], k, t_end(q), 0);
            if q < 90 {
                assert_eq!(
                    s.new_emitter_novelty(),
                    None,
                    "immature before 24 h at q={q}"
                );
            }
            before = s.new_emitter_novelty();
        }
        let before = before.expect("mature after 30 h");
        // A track is seen and confirmed, then six new emitters appear in the next interval.
        let track = TrackId::new();
        let t_track = t_end(120).as_unix_nanos() - 60_000_000_000;
        s.on_track_member(
            track,
            &MemberEvidence {
                f_lo_hz: 432.0e6,
                f_hi_hz: 432.01e6,
                t_ns: t_track,
                continues: false,
                snr_db: Some(15.0),
                suspect: false,
            },
        );
        s.on_track_confirmed(track, FreqRange::new(432.0e6, 432.01e6), false);
        s.ingest_interval(&[series_row(120, site, band, 0.1)], 6, t_end(120), 0);
        let after = s.new_emitter_novelty().unwrap();
        let set = s.provider().snapshot();
        let c = set
            .candidates
            .iter()
            .find(|c| c.subject == CandidateSubject::Track { id: track })
            .expect("the confirmed track is a candidate");
        println!(
            "T-128 first sightings: usual-rate novelty {before:.3}, after 6 new emitters              {after:.3}; candidate novelty {:.3} (new_emitter {:?})",
            c.novelty.novelty, c.novelty.new_emitter
        );
        assert!(before < 0.1, "the usual rate is not novel: {before}");
        assert!(after > 0.9, "six new emitters in 15 min are: {after}");
        assert!(c.novelty.novelty > 0.9 && c.novelty.new_emitter == Some(after));
        assert!(c.components.class_entropy.is_none(), "unclassified");
        set.validate().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// T-136 review: first-sighting state is per site. After a site change the previous site's
    /// sightings neither feed the new site's new-emitter inputs nor its count, and they are kept
    /// under their own site for when it is back.
    #[test]
    fn attention_first_sightings_are_per_site() {
        let s = AttentionService::in_memory().unwrap();
        let pin = |name: &str| {
            let cur = s
                .set_current_site(SiteSelect {
                    name: Some(name.into()),
                    ..SiteSelect::default()
                })
                .unwrap();
            SiteKey::Site(cur["record"]["id"].as_str().unwrap().parse().unwrap())
        };
        let band = OccupancySubject::Band {
            freq: FreqRange::new(431e6, 433e6),
        };
        let feed = |site: SiteKey, q: i64, k: u32| {
            let row = series_row(q, site, band, 0.1);
            let t = row.interval.end;
            let sightings: Vec<FirstSighting> = (0..k)
                .map(|i| FirstSighting {
                    emitter: EmitterId::new(),
                    freq: FreqRange::centered(432.00625e6 + f64::from(i) * 100.0, 10e3),
                    t,
                })
                .collect();
            s.note_first_sightings(site, &sightings);
            s.ingest_interval(std::slice::from_ref(&row), u64::from(k), t, 0);
            s.new_emitter_inputs(site, t)
        };
        let home = pin("home");
        let at_home = feed(home, 0, 6);
        assert_eq!(at_home.len(), 6);
        assert!(at_home.iter().all(|n| n.fresh && n.input.observed == 6.0));
        let away = pin("away");
        let at_away = feed(away, 1, 1);
        assert_eq!(at_away.len(), 1, "only the away site's own sighting");
        assert!(at_away[0].fresh);
        assert_eq!(
            at_away[0].input.observed, 1.0,
            "home's burst is not counted"
        );
        let c = lock(&s.cands);
        assert_eq!(c.sighting_site, Some(away));
        assert_eq!(c.sightings[&home].recent.len(), 6, "home keeps its own");
        assert_eq!(c.sightings[&away].recent.len(), 1);
    }

    /// A class source that blocks in its "query" until released, recording the calling threads.
    struct GatedClasses {
        gate: (Mutex<bool>, std::sync::Condvar),
        calls: AtomicU64,
        threads: Mutex<Vec<std::thread::ThreadId>>,
    }

    impl ClassSource for GatedClasses {
        fn classified(&self, _region: &Region, limit: usize) -> Vec<(FreqRange, f64)> {
            assert!(limit <= CLASS_ROWS_MAX);
            lock(&self.threads).push(std::thread::current().id());
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut open = lock(&self.gate.0);
            while !*open {
                open = self.gate.1.wait(open).unwrap();
            }
            vec![(FreqRange::new(431.99e6, 432.02e6), 0.2)]
        }
    }

    fn wait_until(what: &str, f: impl Fn() -> bool) {
        let t0 = std::time::Instant::now();
        while !f() {
            assert!(t0.elapsed().as_secs() < 10, "timed out waiting for {what}");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// T-128 review item 1: `publish_candidates` on the control path does no inventory I/O. Class
    /// entropies come from a snapshot a worker thread refreshes; while that refresh is stuck in its
    /// query, member evidence and publishing still return at once.
    #[test]
    fn attention_candidate_publish_does_no_db_io_on_the_control_path() {
        let dir = std::env::temp_dir().join(format!("hk-t128-class-{}", SiteId::new()));
        let s = service(&dir);
        let src = Arc::new(GatedClasses {
            gate: (Mutex::new(false), std::sync::Condvar::new()),
            calls: AtomicU64::new(0),
            threads: Mutex::new(Vec::new()),
        });
        s.set_class_source(src.clone());
        let t0 = 7 * 86_400 * 1_000_000_000i64;
        let member = |t_ns: i64, lo: f64| MemberEvidence {
            f_lo_hz: lo,
            f_hi_hz: lo + 10e3,
            t_ns,
            continues: false,
            snr_db: Some(15.0),
            suspect: false,
        };
        let track = TrackId::new();
        s.on_track_member(track, &member(t0, 432.0e6));
        s.on_track_confirmed(track, FreqRange::new(432.0e6, 432.01e6), false);
        let start = std::time::Instant::now();
        assert!(
            s.publish_candidates(Timestamp::from_unix_nanos(t0), false)
                .is_some()
        );
        let publish_ms = start.elapsed().as_secs_f64() * 1e3;
        // The refresh is now blocked inside the query on the worker thread.
        wait_until("the refresh to start", || {
            src.calls.load(Ordering::SeqCst) == 1
        });
        let start = std::time::Instant::now();
        let other = TrackId::new();
        s.on_track_member(other, &member(t0 + 20_000_000_000, 433.0e6));
        let member_ms = start.elapsed().as_secs_f64() * 1e3;
        s.on_track_confirmed(other, FreqRange::new(433.0e6, 433.01e6), false);
        let start = std::time::Instant::now();
        assert!(
            s.publish_candidates(Timestamp::from_unix_nanos(t0 + 20_000_000_000), false)
                .is_some()
        );
        let blocked_publish_ms = start.elapsed().as_secs_f64() * 1e3;
        let entropy = |s: &AttentionService| {
            s.provider()
                .snapshot()
                .candidates
                .iter()
                .find(|c| c.subject == CandidateSubject::Track { id: track })
                .map(|c| c.components.class_entropy)
        };
        assert_eq!(entropy(&s), Some(None), "no snapshot yet: unclassified");
        assert_eq!(src.calls.load(Ordering::SeqCst), 1, "one refresh in flight");
        // Release the query: the next pass reads the refreshed snapshot.
        *lock(&src.gate.0) = true;
        src.gate.1.notify_all();
        wait_until("the refresh to land", || s.class_refreshes() >= 1);
        assert!(
            s.publish_candidates(Timestamp::from_unix_nanos(t0 + 25_000_000_000), true)
                .is_some()
        );
        println!(
            "T-128 class cache: publish {publish_ms:.3} ms, on_track_member during a stuck \
             refresh {member_ms:.3} ms, publish during it {blocked_publish_ms:.3} ms; \
             refreshes {} (calls {}), entropy after {:?}",
            s.class_refreshes(),
            src.calls.load(Ordering::SeqCst),
            entropy(&s)
        );
        assert!(
            entropy(&s).flatten().is_some(),
            "classified from the snapshot"
        );
        let me = std::thread::current().id();
        assert!(
            lock(&src.threads).iter().all(|t| *t != me),
            "the query never ran on the calling (control) thread"
        );
        assert!(member_ms < 50.0 && blocked_publish_ms < 50.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// T-131: `compare_report` statuses directly: unassigned → `no-baseline`; a pinned site with
    /// too little observation → `immature` (baseline named, no changes); rows longer than 1 h (a
    /// span rollup) are never compared.
    #[test]
    fn attention_compare_report_statuses() {
        let dir = std::env::temp_dir().join(format!("hk-t131-cmp-{}", SiteId::new()));
        let s = service(&dir);
        let key = ChannelKey {
            scheme: 1,
            lo_cell: 69_000,
            hi_cell: 69_004,
        };
        let subject = OccupancySubject::Channel { key };
        let unassigned = s.compare_report(
            SiteKey::Unassigned,
            &[series_row(0, SiteKey::Unassigned, subject, 0.5)],
        );
        assert_eq!(unassigned.status, ComparisonStatus::NoBaseline);
        let cur = s
            .set_current_site(SiteSelect {
                name: Some("bench".into()),
                ..SiteSelect::default()
            })
            .unwrap();
        let id: SiteId = cur["record"]["id"].as_str().unwrap().parse().unwrap();
        let site = SiteKey::Site(id);
        assert_eq!(
            s.compare_report(site, &[series_row(0, site, subject, 0.5)])
                .status,
            ComparisonStatus::NoBaseline,
            "no subject folded yet"
        );
        for q in 0..8 {
            s.ingest_occupancy(&series_row(q, site, subject, 0.25), 0);
        }
        let rows: Vec<_> = (8..16).map(|q| series_row(q, site, subject, 0.9)).collect();
        let immature = s.compare_report(site, &rows);
        assert_eq!(immature.status, ComparisonStatus::Immature);
        assert!(immature.baseline.is_some() && immature.changes.is_empty());
        let mut rollup = series_row(16, site, subject, 0.9);
        rollup.interval = TimeRange::new(
            rollup.interval.start,
            rollup
                .interval
                .start
                .saturating_add_nanos(2 * 3_600_000_000_000),
        );
        assert_eq!(
            s.compare_report(site, &[rollup]).status,
            ComparisonStatus::NoBaseline,
            "a 2 h rollup row is not compared against one slot"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// T-131: the occupancy close path's alarm wiring without a device: `ingest_interval` folds
    /// carry site, calibration, observation and reference pool, and `observe_interval` raises a
    /// `busier-than-usual` alarm (listed with explanations) once a mature channel stays busy for
    /// two scored intervals; an unassigned site's folds raise nothing.
    #[test]
    fn attention_interval_folds_raise_busier_alarm() {
        let dir = std::env::temp_dir().join(format!("hk-t131-alarm-{}", SiteId::new()));
        let s = service(&dir);
        let cur = s
            .set_current_site(SiteSelect {
                name: Some("home".into()),
                ..SiteSelect::default()
            })
            .unwrap();
        let id: SiteId = cur["record"]["id"].as_str().unwrap().parse().unwrap();
        let site = SiteKey::Site(id);
        let subject = OccupancySubject::Channel {
            key: ChannelKey {
                scheme: 1,
                lo_cell: 69_000,
                hi_cell: 69_004,
            },
        };
        let row = |q: i64, k: u64| {
            let mut r = series_row(q, site, subject, k as f64 / 12.0);
            r.n_occupied = k;
            r
        };
        let repo = Arc::new(Mutex::new(Repository::open_in_memory().unwrap()));
        let alarms = crate::alarms::AlarmService::open(
            repo,
            None,
            Arc::new(|| Timestamp::from_unix_nanos(0)),
        )
        .unwrap();
        let (mut raised, mut last_novelty) = (Vec::new(), None);
        for q in 0..300 {
            // ≈ 3 % FCO for 3 days, then fully occupied: per-interval z ≈ 17 at 12 revisits (a
            // channel busy from 25 % reaches only z ≈ 6, novelty 0.43, below the "on" level).
            let r = row(
                q,
                if q < 288 {
                    [0, 1, 0][q as usize % 3]
                } else {
                    12
                },
            );
            let folds = s.ingest_interval(std::slice::from_ref(&r), 0, r.interval.end, 0);
            if q == 0 {
                assert_eq!(folds.len(), 1);
                assert_eq!((folds[0].site, folds[0].subject), (site, subject));
            }
            last_novelty = folds
                .first()
                .map(|f| (f.fold.novelty.novelty, f.fold.novelty.occupancy_z));
            let events = alarms.observe_interval(&folds, &[]);
            if q < 288 {
                assert!(events.is_empty(), "interval {q}: {events:?}");
            }
            raised.extend(events);
        }
        let first = raised.first().unwrap_or_else(|| {
            panic!(
                "a busy mature channel raises an alarm; last fold (novelty, occupancy_z) \
                 {last_novelty:?}, suppressions {}",
                alarms.suppressions()
            )
        });
        assert_eq!(
            first.row.key.kind,
            hk_model::attention::alarm::AlarmKind::BusierThanUsual
        );
        let (listed, _) = alarms
            .list(&hk_model::repo::alarms::AnomalyQuery {
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(listed.len(), 1, "one open alarm, extended, not duplicated");
        assert!(!listed[0].explanations.is_empty());
        let mut unassigned = row(301, 12);
        unassigned.site = SiteKey::Unassigned;
        let folds = s.ingest_interval(
            std::slice::from_ref(&unassigned),
            0,
            unassigned.interval.end,
            0,
        );
        assert!(alarms.observe_interval(&folds, &[]).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// T-128 review item 2: a 24 h report compares each 15-min row against its own slot and
    /// combines the evidence, so an unchanged channel shows no change, and busier / quieter
    /// channels are labelled by the sign.
    #[test]
    fn attention_compare_report_per_interval_over_24h() {
        let dir = std::env::temp_dir().join(format!("hk-t128-cmp-{}", SiteId::new()));
        let s = service(&dir);
        let cur = s
            .set_current_site(SiteSelect {
                name: Some("home".into()),
                ..SiteSelect::default()
            })
            .unwrap();
        let id: SiteId = cur["record"]["id"].as_str().unwrap().parse().unwrap();
        let site = SiteKey::Site(id);
        let key = ChannelKey {
            scheme: 1,
            lo_cell: 69_000,
            hi_cell: 69_004,
        };
        let subject = OccupancySubject::Channel { key };
        // `k` of 12 revisits occupied.
        let row = |q: i64, k: u64| {
            let mut r = series_row(q, site, subject, k as f64 / 12.0);
            r.n_occupied = k;
            r
        };
        // 3 days learning: 1, 3, 5 of 12 (mean FCO 0.25, varying interval to interval).
        for q in 0..288 {
            s.ingest_occupancy(&row(q, [1, 3, 5][q as usize % 3]), 0);
        }
        let day = |cycle: [u64; 3]| -> Vec<OccupancyStat> {
            (288..384)
                .map(|q| row(q, cycle[(q as usize + 1) % 3]))
                .collect()
        };
        let unchanged = s.compare_report(site, &day([5, 1, 3]));
        let busier = s.compare_report(site, &day([5, 7, 9]));
        let quieter = s.compare_report(site, &day([0, 0, 1]));
        println!(
            "T-128 compare 24 h: unchanged {:?}; busier {:?}; quieter {:?}",
            unchanged.changes, busier.changes, quieter.changes
        );
        assert_eq!(unchanged.status, ComparisonStatus::Available);
        assert!(unchanged.changes.is_empty(), "{:?}", unchanged.changes);
        assert_eq!(busier.changes.len(), 1);
        assert_eq!(busier.changes[0].kind, AlarmKind::BusierThanUsual);
        assert!(busier.changes[0].z >= 3.0 && busier.changes[0].observed > 0.5);
        assert_eq!(quieter.changes.len(), 1);
        assert_eq!(quieter.changes[0].kind, AlarmKind::QuieterThanUsual);
        assert!(quieter.changes[0].z <= -3.0 && quieter.changes[0].observed < 0.05);
        // A span rollup row cannot be compared against one slot.
        let rolled = crate::reports::roll_up(
            &day([5, 7, 9]).iter().collect::<Vec<_>>(),
            TimeRange::new(row(288, 0).interval.start, row(383, 0).interval.end),
        )
        .unwrap();
        assert!(s.compare_report(site, &[rolled]).changes.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// T-136 (T-131 follow-up): a pinned site survives a service restart on the same database
    /// (same site id, still pinned, no `unassigned` gap on the sample clock); a release and a
    /// GNSS-fixed site's no-fix hold survive it too.
    #[test]
    fn attention_site_pin_survives_restart() {
        let dir = std::env::temp_dir().join(format!("hk-t136-site-{}", SiteId::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("hackriff.db");
        let t0 = 7 * 86_400 * 1_000_000_000i64;
        let at = move |s: i64| Timestamp::from_unix_nanos(t0 + s * 1_000_000_000);
        let open = || {
            AttentionService::open(
                &dir,
                Arc::new(Mutex::new(Repository::open(&db).unwrap())),
                ChainKey::Unknown,
                None,
                Arc::new(move || at(0)),
            )
            .unwrap()
        };
        let id = {
            let s = open();
            let cur = s
                .set_current_site(SiteSelect {
                    name: Some("home".into()),
                    ..SiteSelect::default()
                })
                .unwrap();
            let id: SiteId = cur["record"]["id"].as_str().unwrap().parse().unwrap();
            assert_eq!(s.site_at(at(900)).0, SiteKey::Site(id));
            id
        };
        let s = open();
        let cur = s.current_site_json();
        assert_eq!(cur["site"]["id"], id.to_string(), "{cur}");
        assert_eq!(
            (cur["pinned"].as_bool(), cur["set_by"].as_str()),
            (Some(true), Some("user"))
        );
        for sec in [0, 1, 900, 3 * 86_400] {
            assert_eq!(s.site_at_peek(at(sec)), SiteKey::Site(id));
            assert_eq!(s.site_at(at(sec)).0, SiteKey::Site(id), "no gap at {sec} s");
        }
        s.set_current_site(SiteSelect {
            release: true,
            ..SiteSelect::default()
        })
        .unwrap();
        drop(s);
        let s = open();
        assert_eq!(
            s.current_site_json()["pinned"],
            false,
            "the release persisted"
        );
        // A GNSS-fixed site keeps its no-fix hold across a restart, and its expiry persists.
        s.on_fix(Fix {
            t: at(4 * 86_400),
            lat_deg: 51.5,
            lon_deg: 0.0,
            speed_m_s: Some(0.0),
        });
        let gnss = s.on_fix(Fix {
            t: at(4 * 86_400 + 70),
            lat_deg: 51.5,
            lon_deg: 0.0,
            speed_m_s: Some(0.0),
        });
        assert!(matches!(gnss, SiteKey::Site(g) if g != id), "{gnss:?}");
        drop(s);
        let s = open();
        assert_eq!(s.site_at(at(4 * 86_400 + 600)).0, gnss, "within the hold");
        assert_eq!(s.site_at(at(4 * 86_400 + 700)).0, SiteKey::Unassigned);
        drop(s);
        assert_eq!(
            open().site_at_peek(at(4 * 86_400 + 700)),
            SiteKey::Unassigned
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn attention_service_sites_weights_baselines_and_candidates() {
        let dir = std::env::temp_dir().join(format!("hk-t119-svc-{}", SiteId::new()));
        let s = service(&dir);
        assert_eq!(s.current_site_json()["site"]["kind"], "unassigned");
        assert_eq!(s.baselines_json(None).unwrap_err().status, 409);
        let cur = s
            .set_current_site(SiteSelect {
                name: Some("home".into()),
                utc_offset_min: Some(60),
                ..SiteSelect::default()
            })
            .unwrap();
        assert_eq!(
            (cur["set_by"].as_str(), cur["pinned"].as_bool()),
            (Some("user"), Some(true))
        );
        let id: SiteId = cur["record"]["id"].as_str().unwrap().parse().unwrap();
        assert_eq!(lock(&s.repo).sites().unwrap().len(), 1, "persisted");
        // Fold 30 h of one quiet channel, then an emitter.
        let subject = BaselineSubject::Cell { index: 1000 };
        let fold = |h: i64, occ: f64| IntervalObservation {
            subject,
            t: Timestamp::from_unix_nanos((7 * 24 + h) * 3_600_000_000_000),
            gain: 0,
            level_db: Some(-100.0 + 20.0 * occ),
            max_db: None,
            occupied_weight_s: occ * 3600.0,
            weight_s: 3600.0,
            observed_s: 3600.0,
            n_eff: 40.0,
            suspect_fraction: 0.0,
            provenance_explained: false,
        };
        for h in 0..30 {
            s.observe(CalKey::Uncalibrated, &fold(h, 0.0)).unwrap();
        }
        let out = s.observe(CalKey::Uncalibrated, &fold(30, 0.8)).unwrap();
        assert_eq!(out.novelty.novelty, 1.0);
        let b = s.baselines_json(Some(id)).unwrap();
        assert_eq!(b["baselines"][0]["subjects"], 1);
        let slots = s
            .slots_json(None, 0.0, 1e9, None, Some(BaselineResolution::AllHours))
            .unwrap();
        assert_eq!(slots["subjects"][0]["f_lo"], 100e6);
        assert!(
            slots["subjects"][0]["reference"]["observed_s"]
                .as_f64()
                .unwrap()
                >= 30.0 * 3600.0
        );
        assert!(s.refreeze(None, None, None).unwrap()["refrozen"].as_u64() == Some(1));
        // Weights: versioned, used by the next scoring pass.
        let (old, new) = s
            .set_weights(
                ScoreWeights {
                    novelty: 4.0,
                    ..ScoreWeights::default()
                },
                "tok",
            )
            .unwrap();
        assert_eq!((old.version, new.version), (1, 2));
        assert_eq!(s.weights_json().unwrap()["history"][0]["version"], 2);
        let input = CandidateInput {
            subject: CandidateSubject::Cells {
                scheme: 1,
                lo_cell: 16_000,
                hi_cell: 16_016,
            },
            freq: FreqRange::new(100e6, 100.1e6),
            snr_db: Some(20.0),
            novelty: out.novelty,
            class_entropy: None,
            decoder_available: false,
            periodicity: None,
            boring: BoringEvidence::default(),
            suspect_fraction: 0.0,
            trust_tested: false,
            expected_interval_s: None,
            min_on_off_s: None,
            next_burst_eta: None,
        };
        let _: NoveltyScore = input.novelty;
        assert_eq!(
            s.score(Timestamp::from_unix_nanos(1), false, &[input]),
            Some(1)
        );
        let c = s.candidates_json(Some(99e6), Some(101e6), 10);
        assert_eq!(
            (c["version"].as_u64(), c["weights"]["version"].as_u64()),
            (Some(1), Some(2))
        );
        assert_eq!(c["candidates"].as_array().unwrap().len(), 1);
        assert_eq!(
            s.candidates_json(Some(200e6), None, 10)["candidates"],
            json!([])
        );
        s.checkpoint(Timestamp::from_unix_nanos(1));
        let _ = std::fs::remove_dir_all(dir);
    }
}
