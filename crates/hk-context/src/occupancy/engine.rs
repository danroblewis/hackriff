//! Occupancy engine (T-118, ADR-0012 §2): FCO/FBO/SRO per learned channel and band per interval
//! from frame samples, detections (suspect masking) and the observation log (time weighting).
//!
//! **Pipeline of a statistic.**
//! 1. **Visits** ([`Visit`]) say when a subject's whole extent was actually observed, and by which
//!    [`Tier`]: from the T-115 observation log ([`ObservationSource`], [`MemoryObservations`] builds
//!    them from `ObservationRecord`s) or, without a log, from the history coverage mask
//!    ([`coverage_visits`]: every level-0 time row in which all the subject's cells were observed,
//!    at a caller-chosen tier).
//! 2. **Evaluation** ([`evaluate`]) reads the level-0 history grid under each visit: the visit's
//!    level is the highest cell `mean_db` (power mean of the frames folded into the 1 s cell) over
//!    the subject's cells and rows; it is **occupied** when that exceeds the threshold (SM.1880
//!    "any sample in the channel"). The threshold is resolved once per evaluated grid
//!    ([`super::threshold::resolve`]: corrected history floor, else the 80 % method over the band;
//!    guard; RBW = the level-0 cell width, corrected when it is below the channel OBW). A crossing
//!    under `overload` or coinciding with a §2.6 suspect detection is **suspect**. The outcome is a
//!    compact [`VisitSample`], so widened windows need no second read of the history.
//! 3. **Estimation** ([`estimate`]) over a window, then [`stat`] with the §2.5 widening ladder.
//!
//! **Time weighting (§2.5, verified against SM.2256-1 Annex 1 §A5.1.2).** Each visit weighs half
//! the gap to its predecessor plus half the gap to its successor, each half capped at the mean
//! revisit gap (so the total is ≤ 2 × nominal T_R); an edge visit mirrors its one interior half.
//! Summed over a sequence this equals the Annex's rule for unstable revisit times (δT > 10 %):
//! T_AI += T_Rj per interval, T_O += T_Rj when both ends are occupied and T_Rj/2 on a changeover,
//! SOCR = T_O/T_AI (A8–A11).
//!
//! **Confidence (§2.4).** Wilson score on `n_eff = n(1−ρ)/(1+ρ)` (`effective_samples`). The ADR
//! derives ρ from C10 on/off timing, which a blind engine rarely has; T-118 measures ρ instead as
//! the lag-1 autocorrelation of the visit state sequence and passes the equivalent
//! τ_c = −T̄_R/ln ρ, so `effective_samples` and its formula are unchanged. ρ ≤ 0 gives `n_eff = n`
//! (measured, not assumed); a constant sequence (FCO 0 or 1) leaves ρ undefined and
//! `independence_assumed` set. SM.2256-1 Annex 1 itself gives sample-size rules rather than an
//! interval: A18/A19 for pulsed signals is the binomial normal approximation J = SO(1−SO)(x_p/ΔSO)²
//! (so Wilson is its well-behaved form), and A12/A16 for lengthy signals
//! J = x_p/(2ΔSO)·√(V_avr(1.06+δT²)), an error bound driven by the number of state changes V
//! ([`sm2256_lengthy_samples`], [`sm2256_pulsed_samples`], checked against Tables A1/A2).

use std::collections::HashMap;

use hk_model::attention::ATTENTION_SCHEMA_VERSION;
use hk_model::attention::baseline::SiteKey;
use hk_model::attention::observation::{ObservationRecord, SweepRecord, Tier};
use hk_model::attention::occupancy::{
    ConfidenceInterval, ConfidenceLevel, OccupancyStat, OccupancySubject, ThresholdSpec,
    TimingRegime, effective_samples, fraction_interval,
};
use hk_model::frames::PowerUnit;
use hk_model::ids::CalibrationStateId;
use hk_model::{FreqRange, TimeRange, Timestamp};
use hk_store::{RegionHistory, RegionQuery, Resolution};

use super::channels::DetectionExtent;
use super::threshold::{self, AppliedThreshold, median_finite};

/// Activity-independent visits an interval needs before it is estimated on its own (§2.5).
pub const MIN_ACTIVITY_INDEPENDENT_VISITS: u64 = 30;
/// `fco_all_visits` stratum, ns (§2.5 rule 3).
pub const STRATUM_NS: i64 = 60_000_000_000;
/// The aligned widening ladder after the interval itself, ns (§2.5 rule 4); the data span follows.
pub const LADDER_NS: [i64; 4] = [
    900_000_000_000,
    3_600_000_000_000,
    21_600_000_000_000,
    86_400_000_000_000,
];

const NS: f64 = 1e-9;

/// One observation of a frequency range: when, at which tier, and whether the front end was
/// overloaded.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Visit {
    /// Settled observed interval.
    pub observed: TimeRange,
    /// Scheduler tier.
    pub tier: Tier,
    /// Overload during the visit.
    pub overload: bool,
}

/// Visits that observed **all** of `freq` (the §1.4 "entirely inside a covered extent" rule),
/// starting inside `span`, in time order.
pub trait ObservationSource {
    /// The visits.
    fn visits(&self, freq: FreqRange, span: TimeRange) -> Vec<Visit>;
}

fn covers(covered: &[FreqRange], f: FreqRange) -> bool {
    covered
        .iter()
        .any(|c| c.lo_hz <= f.lo_hz && f.hi_hz <= c.hi_hz)
}

fn starts_in(t: Timestamp, span: TimeRange) -> bool {
    t >= span.start && t < span.end
}

/// An in-memory observation log built from `ObservationRecord`s: the test source, and the shape
/// the T-115 store adapter fills (records in, visits out).
#[derive(Clone, Debug, Default)]
pub struct MemoryObservations {
    dwells: Vec<(Vec<FreqRange>, Visit)>,
    geometries: HashMap<u64, Vec<Vec<FreqRange>>>,
    sweeps: Vec<SweepRecord>,
}

impl MemoryObservations {
    /// Adds a record (a sweep's geometry must be pushed before visits can resolve its hops).
    pub fn push(&mut self, rec: ObservationRecord) {
        match rec {
            ObservationRecord::Dwell(d) => self.dwells.push((
                d.window.covered(),
                Visit {
                    observed: d.observed,
                    tier: d.tier,
                    overload: d.overload,
                },
            )),
            ObservationRecord::Geometry(g) => {
                self.geometries
                    .insert(g.id, g.hops.iter().map(|h| h.covered()).collect());
            }
            ObservationRecord::Sweep(s) => self.sweeps.push(s),
        }
    }

    /// Drops records that ended before `t` (sample clock).
    pub fn prune_before(&mut self, t: Timestamp) {
        self.dwells.retain(|(_, v)| v.observed.end >= t);
        self.sweeps.retain(|s| s.span.end >= t);
    }
}

impl ObservationSource for MemoryObservations {
    fn visits(&self, freq: FreqRange, span: TimeRange) -> Vec<Visit> {
        let mut out: Vec<Visit> = self
            .dwells
            .iter()
            .filter(|(c, v)| starts_in(v.observed.start, span) && covers(c, freq))
            .map(|(_, v)| *v)
            .collect();
        for s in &self.sweeps {
            let Some(hops) = self.geometries.get(&s.geometry) else {
                continue;
            };
            for hv in &s.visits {
                let Some(c) = hops.get(hv.hop as usize) else {
                    continue;
                };
                let start = s
                    .span
                    .start
                    .saturating_add_nanos(i64::from(hv.start_ms) * 1_000_000);
                if !starts_in(start, span) || !covers(c, freq) {
                    continue;
                }
                out.push(Visit {
                    observed: TimeRange::new(
                        start,
                        start.saturating_add_nanos(i64::from(hv.observed_ms) * 1_000_000),
                    ),
                    tier: Tier::BackgroundSweep,
                    overload: s.overload_hops > 0,
                });
            }
        }
        out.sort_by_key(|v| v.observed.start);
        out
    }
}

/// Level-0 history for a region (the evaluation input).
pub trait LevelSource {
    /// Level-0 grid over `freq` × `span`, `None` when unreadable.
    fn level0(&self, freq: FreqRange, span: TimeRange) -> Option<RegionHistory>;
}

impl LevelSource for hk_store::Pyramid {
    fn level0(&self, freq: FreqRange, span: TimeRange) -> Option<RegionHistory> {
        self.query(&RegionQuery {
            freq,
            time: span,
            resolution: Resolution::Level(0),
        })
        .ok()
    }
}

/// Column range `[a, b)` of `freq`'s cells, snapped outward, when wholly inside the grid.
fn cells_of(grid: &RegionHistory, freq: FreqRange) -> Option<(usize, usize)> {
    if freq.hi_hz.is_nan() || freq.hi_hz <= freq.lo_hz || grid.nf == 0 {
        return None;
    }
    let eps = 1e-6;
    let a = (freq.lo_hz / grid.f_cell_hz + eps).floor() as i64 - grid.f_first_cell;
    let b = (freq.hi_hz / grid.f_cell_hz - eps).ceil() as i64 - grid.f_first_cell;
    (a >= 0 && b > a && b as usize <= grid.nf).then_some((a as usize, b as usize))
}

fn cell_ok(grid: &RegionHistory, t: usize, f: usize) -> bool {
    let c = grid.cell(t, f);
    c.observed() && c.level == grid.level && c.mean_db.is_finite()
}

fn row_observed(grid: &RegionHistory, t: usize, (a, b): (usize, usize)) -> bool {
    (a..b).all(|f| cell_ok(grid, t, f))
}

/// Without an observation log: one visit per level-0 row in which every cell of `freq` was
/// observed, at `tier`.
pub fn coverage_visits(grid: &RegionHistory, freq: FreqRange, tier: Tier) -> Vec<Visit> {
    let Some(cols) = cells_of(grid, freq) else {
        return Vec::new();
    };
    (0..grid.nt)
        .filter(|&t| row_observed(grid, t, cols))
        .map(|t| {
            let s = grid.time_of(t);
            Visit {
                observed: TimeRange::new(s, s.saturating_add_nanos(grid.t_cell_ns)),
                tier,
                overload: false,
            }
        })
        .collect()
}

/// Level samples (`mean_db` of every observed cell) of `freq`, for the 80 % method.
pub fn band_levels(grid: &RegionHistory, freq: FreqRange) -> Vec<f64> {
    let Some((a, b)) = cells_of(grid, freq) else {
        return Vec::new();
    };
    let mut v = Vec::new();
    for t in 0..grid.nt {
        for f in a..b {
            if cell_ok(grid, t, f) {
                v.push(f64::from(grid.cell(t, f).mean_db));
            }
        }
    }
    v
}

/// The band's noise floor: the 80 % method (discard the highest `discard_fraction`, linearly
/// average the rest) over the history's bias-corrected `floor_db` of every observed cell of `freq`.
/// `None` when no cell has a finite floor.
pub fn band_floor_db(grid: &RegionHistory, freq: FreqRange, discard_fraction: f64) -> Option<f64> {
    let (a, b) = cells_of(grid, freq)?;
    let mut v = Vec::new();
    for t in 0..grid.nt {
        for f in a..b {
            if cell_ok(grid, t, f) {
                v.push(f64::from(grid.cell(t, f).floor_db));
            }
        }
    }
    threshold::eighty_percent_floor_db(&v, discard_fraction)
}

/// One evaluated visit.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VisitSample {
    /// Visit start, ns since the epoch (sample clock).
    pub start_ns: i64,
    /// Observed duration, ns.
    pub dur_ns: i64,
    /// Tier.
    pub tier: Tier,
    /// Any cell above threshold.
    pub occupied: bool,
    /// Suspect crossing (§2.6).
    pub suspect: bool,
    /// Fraction of the subject's cells above threshold (FBO sample).
    pub above_fraction: f32,
    /// Threshold applied, dB/Hz.
    pub threshold_db: f32,
    /// The threshold hit the floor + 3 dB clamp.
    pub guard_clamped: bool,
}

impl VisitSample {
    fn mid_ns(&self) -> i64 {
        self.start_ns.saturating_add(self.dur_ns / 2)
    }
}

/// What [`evaluate`] reads.
#[derive(Clone, Copy)]
pub struct EvalInput<'a> {
    /// Level-0 grid covering the subject and the visits.
    pub grid: &'a RegionHistory,
    /// The subject's visits.
    pub visits: &'a [Visit],
    /// Detections overlapping the grid (suspect masking).
    pub detections: &'a [DetectionExtent],
    /// Band level samples for the 80 % fallback (may be empty).
    pub idle_levels_db: &'a [f64],
    /// Band noise floor ([`band_floor_db`]), preferred over the subject's own cells: a busy
    /// channel's low-percentile floor is its own signal level (SM.2256: the calculated threshold
    /// only works over a band).
    pub band_floor_db: Option<f64>,
}

/// Evaluates `visits` of the subject `freq` (occupied bandwidth `obw_hz`) against `grid`.
/// Returns the samples and the threshold (none when no floor could be resolved).
pub fn evaluate(
    spec: &ThresholdSpec,
    freq: FreqRange,
    obw_hz: f64,
    input: EvalInput<'_>,
) -> (Vec<VisitSample>, Option<AppliedThreshold>) {
    let grid = input.grid;
    let Some(cols) = cells_of(grid, freq) else {
        return (Vec::new(), None);
    };
    let (a, b) = cols;
    let floor = median_finite((0..grid.nt).flat_map(|t| {
        (a..b)
            .filter(move |&f| cell_ok(grid, t, f))
            .map(move |f| f64::from(grid.cell(t, f).floor_db))
    }));
    let floor = input.band_floor_db.or(floor);
    let Some(thr) = threshold::resolve(spec, floor, input.idle_levels_db, obw_hz, grid.f_cell_hz)
    else {
        return (Vec::new(), None);
    };
    let t0_ns = grid.t_first_cell.saturating_mul(grid.t_cell_ns);
    let slack = grid.t_cell_ns;
    let mut out = Vec::with_capacity(input.visits.len());
    let mut above = vec![false; b - a];
    for v in input.visits {
        let (s, e) = (
            v.observed.start.as_unix_nanos(),
            v.observed.end.as_unix_nanos(),
        );
        let r0 = (s - t0_ns).div_euclid(grid.t_cell_ns).max(0);
        let r1 = ((e - t0_ns + grid.t_cell_ns - 1).div_euclid(grid.t_cell_ns)).max(r0 + 1);
        let (r0, r1) = (r0 as usize, (r1 as usize).min(grid.nt));
        above.iter_mut().for_each(|x| *x = false);
        let mut any_row = false;
        let mut level = f64::NEG_INFINITY;
        for t in r0..r1 {
            if !row_observed(grid, t, cols) {
                continue;
            }
            any_row = true;
            for (k, f) in (a..b).enumerate() {
                let m = f64::from(grid.cell(t, f).mean_db);
                level = level.max(m);
                above[k] |= m > thr.threshold_db;
            }
        }
        if !any_row {
            continue;
        }
        let occupied = level > thr.threshold_db;
        let window = TimeRange::new(
            v.observed.start.saturating_add_nanos(-slack),
            v.observed.end.saturating_add_nanos(slack),
        );
        let suspect = occupied
            && (v.overload
                || input.detections.iter().any(|d| {
                    d.suspect
                        && d.freq.lo_hz < freq.hi_hz
                        && d.freq.hi_hz > freq.lo_hz
                        && d.time.start <= window.end
                        && d.time.end >= window.start
                }));
        out.push(VisitSample {
            start_ns: s,
            dur_ns: (e - s).max(0),
            tier: v.tier,
            occupied,
            suspect,
            above_fraction: above.iter().filter(|&&x| x).count() as f32 / above.len() as f32,
            threshold_db: thr.threshold_db as f32,
            guard_clamped: thr.guard_clamped,
        });
    }
    (out, Some(thr))
}

/// Estimates over one window.
#[derive(Clone, Debug, PartialEq)]
pub struct WindowEstimate {
    /// The window.
    pub window: TimeRange,
    /// Time-weighted FCO from activity-independent, non-suspect visits.
    pub fco: Option<f64>,
    /// Same with suspect crossings counted occupied.
    pub fco_suspect_upper: Option<f64>,
    /// All tiers, 1-min strata.
    pub fco_all_visits: Option<f64>,
    /// Time-weighted fraction of cell samples above threshold.
    pub fbo: Option<f64>,
    /// Activity-independent visits (suspect included).
    pub n_revisits: u64,
    /// Occupied, non-suspect.
    pub n_occupied: u64,
    /// Suspect.
    pub n_suspect: u64,
    /// All visits.
    pub n_revisits_all: u64,
    /// Observed seconds, all tiers.
    pub observed_s: f64,
    /// Longest activity-independent gap (window edges count), s.
    pub revisit_max_s: Option<f64>,
    /// Mean activity-independent revisit, s.
    pub revisit_mean_s: Option<f64>,
    /// Wilson interval on effective samples.
    pub confidence: Option<ConfidenceInterval>,
    /// Median applied threshold, dB/Hz.
    pub threshold_db: Option<f64>,
    /// Any applied threshold was clamped.
    pub guard_clamped: bool,
}

impl WindowEstimate {
    fn clean_n(&self) -> u64 {
        self.n_revisits - self.n_suspect
    }
}

/// Time weights of visits at `mids` (sorted): half-gaps each side capped at the mean gap.
fn weights(mids: &[i64]) -> Vec<f64> {
    let n = mids.len();
    if n <= 1 {
        return vec![1.0; n];
    }
    let gaps: Vec<f64> = mids.windows(2).map(|p| (p[1] - p[0]) as f64).collect();
    // Nominal T_R = the mean revisit (a median would collapse to a dwell cluster's spacing).
    let cap = Some((mids[n - 1] - mids[0]) as f64 / (n - 1) as f64)
        .filter(|g| *g > 0.0)
        .unwrap_or(1.0);
    (0..n)
        .map(|i| {
            let prev = (i > 0).then(|| (0.5 * gaps[i - 1]).min(cap));
            let next = (i + 1 < n).then(|| (0.5 * gaps[i]).min(cap));
            let w = match (prev, next) {
                (Some(p), Some(q)) => p + q,
                (Some(p), None) => 2.0 * p,
                (None, Some(q)) => 2.0 * q,
                (None, None) => 1.0,
            };
            w.max(f64::MIN_POSITIVE)
        })
        .collect()
}

fn weighted_mean(mids: &[i64], values: &[f64]) -> Option<f64> {
    if mids.is_empty() {
        return None;
    }
    let w = weights(mids);
    let sw: f64 = w.iter().sum();
    Some((w.iter().zip(values).map(|(w, v)| w * v).sum::<f64>() / sw).clamp(0.0, 1.0))
}

/// Lag-1 autocorrelation of a state sequence; `None` when constant or too short.
fn lag1_rho(x: &[f64]) -> Option<f64> {
    if x.len() < 3 {
        return None;
    }
    let p = x.iter().sum::<f64>() / x.len() as f64;
    let var: f64 = x.iter().map(|v| (v - p) * (v - p)).sum();
    if var <= 0.0 {
        return None;
    }
    let c: f64 = x.windows(2).map(|w| (w[0] - p) * (w[1] - p)).sum();
    Some(c / var)
}

/// Estimates over `window` from `samples` (any order) whose midpoint lies in it.
pub fn estimate(
    samples: &[VisitSample],
    window: TimeRange,
    level: ConfidenceLevel,
) -> WindowEstimate {
    let (ws, we) = (window.start.as_unix_nanos(), window.end.as_unix_nanos());
    let mut sel: Vec<&VisitSample> = samples
        .iter()
        .filter(|s| (ws..we).contains(&s.mid_ns()))
        .collect();
    sel.sort_by_key(|s| s.mid_ns());
    let ai: Vec<&VisitSample> = sel
        .iter()
        .copied()
        .filter(|s| s.tier.activity_independent())
        .collect();
    let clean: Vec<&VisitSample> = ai.iter().copied().filter(|s| !s.suspect).collect();
    let mids = |v: &[&VisitSample]| v.iter().map(|s| s.mid_ns()).collect::<Vec<_>>();
    let occ = |v: &[&VisitSample]| {
        v.iter()
            .map(|s| f64::from(u8::from(s.occupied)))
            .collect::<Vec<_>>()
    };
    let clean_mids = mids(&clean);
    let clean_occ = occ(&clean);
    let fco = weighted_mean(&clean_mids, &clean_occ);
    let fco_suspect_upper = weighted_mean(&mids(&ai), &occ(&ai));
    let fbo = weighted_mean(
        &clean_mids,
        &clean
            .iter()
            .map(|s| f64::from(s.above_fraction))
            .collect::<Vec<_>>(),
    );
    // §2.5 rule 3: occupied time fraction per 1-min stratum from all non-suspect visits, strata
    // weighted equally (by their represented minute), so a long dwell counts as one minute.
    let mut strata: HashMap<i64, (f64, f64)> = HashMap::new();
    for s in sel.iter().filter(|s| !s.suspect) {
        let e = strata.entry(s.mid_ns().div_euclid(STRATUM_NS)).or_default();
        let d = s.dur_ns.max(1) as f64;
        e.0 += d * f64::from(u8::from(s.occupied));
        e.1 += d;
    }
    let fco_all_visits = (!strata.is_empty())
        .then(|| strata.values().map(|(o, d)| o / d).sum::<f64>() / strata.len() as f64);
    let ai_mids = mids(&ai);
    let revisit_mean_s = (ai_mids.len() >= 2).then(|| {
        (ai_mids[ai_mids.len() - 1] - ai_mids[0]) as f64 * NS / (ai_mids.len() - 1) as f64
    });
    let revisit_max_s = (!ai_mids.is_empty()).then(|| {
        let mut m = (ai_mids[0] - ws).max(we - ai_mids[ai_mids.len() - 1]);
        for p in ai_mids.windows(2) {
            m = m.max(p[1] - p[0]);
        }
        m.max(0) as f64 * NS
    });
    let confidence = fco.and_then(|p| {
        let n = clean.len() as u64;
        let (n_eff, assumed) = match (lag1_rho(&clean_occ), revisit_mean_s) {
            (Some(r), Some(tr)) if r > 0.0 => {
                let tau = tr / -(r.min(0.999).ln());
                effective_samples(n, Some(tr), Some(tau))
            }
            (Some(_), _) => (n as f64, false),
            (None, _) => effective_samples(n, None, None),
        };
        fraction_interval(p, n_eff, level, assumed)
    });
    WindowEstimate {
        window,
        fco,
        fco_suspect_upper,
        fco_all_visits,
        fbo,
        n_revisits: ai.len() as u64,
        n_occupied: clean.iter().filter(|s| s.occupied).count() as u64,
        n_suspect: (ai.len() - clean.len()) as u64,
        n_revisits_all: sel.len() as u64,
        observed_s: sel.iter().map(|s| s.dur_ns as f64 * NS).sum(),
        revisit_max_s,
        revisit_mean_s,
        confidence,
        threshold_db: median_finite(sel.iter().map(|s| f64::from(s.threshold_db))),
        guard_clamped: sel.iter().any(|s| s.guard_clamped),
    }
}

/// The §2.5 widening ladder for `interval`: itself, the enclosing aligned 15 min / 1 h / 6 h /
/// 24 h windows that are larger, then the hull of `interval` and `data_span` if larger still.
pub fn widen_windows(interval: TimeRange, data_span: TimeRange) -> Vec<TimeRange> {
    let (s, e) = (interval.start.as_unix_nanos(), interval.end.as_unix_nanos());
    let mut out = vec![interval];
    let mut push = |a: i64, b: i64| {
        let last = out[out.len() - 1];
        if a <= s
            && b >= e
            && (b - a) > last.duration_ns()
            && a <= last.start.as_unix_nanos()
            && b >= last.end.as_unix_nanos()
        {
            out.push(TimeRange::new(
                Timestamp::from_unix_nanos(a),
                Timestamp::from_unix_nanos(b),
            ));
        }
    };
    for l in LADDER_NS {
        let a = s.div_euclid(l) * l;
        let b = (e + l - 1).div_euclid(l) * l;
        push(a, b);
    }
    push(
        s.min(data_span.start.as_unix_nanos()),
        e.max(data_span.end.as_unix_nanos()),
    );
    out
}

/// Engine settings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EngineConfig {
    /// Threshold method.
    pub threshold: ThresholdSpec,
    /// Interval confidence level (95 %).
    pub level: ConfidenceLevel,
    /// Activity-independent visits before widening stops (30).
    pub min_visits: u64,
    /// Site of the measurements.
    pub site: SiteKey,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            threshold: ThresholdSpec::default(),
            level: ConfidenceLevel::P95,
            min_visits: MIN_ACTIVITY_INDEPENDENT_VISITS,
            site: SiteKey::Unassigned,
        }
    }
}

/// The subject a statistic describes and its measurement context.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SubjectContext {
    /// Channel or band.
    pub subject: OccupancySubject,
    /// Resolution bandwidth (level-0 cell width), Hz.
    pub rbw_hz: f64,
    /// Channel OBW, Hz.
    pub obw_hz: Option<f64>,
    /// Unit of the levels.
    pub unit: PowerUnit,
    /// Calibration in force.
    pub calibration: Option<CalibrationStateId>,
}

/// The `OccupancyStat` row of `interval` from `samples`, widening per §2.5 within `data_span`.
/// `sro` is set on band rows. `None` when no visit of the widest window was evaluated (not
/// observed is not quiet: no row, not a zero row).
pub fn stat(
    cfg: &EngineConfig,
    ctx: &SubjectContext,
    interval: TimeRange,
    samples: &[VisitSample],
    data_span: TimeRange,
    sro: Option<f64>,
) -> Option<OccupancyStat> {
    let mut chosen = None;
    for w in widen_windows(interval, data_span) {
        let e = estimate(samples, w, cfg.level);
        let enough = e.clean_n() >= cfg.min_visits;
        chosen = Some(e);
        if enough {
            break;
        }
    }
    let e = chosen?;
    let threshold_db = e.threshold_db?;
    let is_band = matches!(ctx.subject, OccupancySubject::Band { .. });
    Some(OccupancyStat {
        schema: ATTENTION_SCHEMA_VERSION,
        site: cfg.site,
        subject: ctx.subject,
        interval,
        fco: e.fco,
        fco_all_visits: e.fco_all_visits,
        fco_suspect_upper: e.fco_suspect_upper,
        fbo: e.fbo,
        sro: if is_band { sro } else { None },
        n_revisits: e.n_revisits,
        n_occupied: e.n_occupied,
        n_suspect: e.n_suspect,
        n_revisits_all: e.n_revisits_all,
        observed_s: e.observed_s,
        revisit_max_s: e.revisit_max_s,
        revisit_mean_s: e.revisit_mean_s,
        timing: TimingRegime::classify(e.revisit_max_s, None),
        threshold: cfg.threshold,
        threshold_db,
        guard_clamped: e.guard_clamped,
        rbw_hz: ctx.rbw_hz,
        obw_hz: ctx.obw_hz,
        unit: ctx.unit,
        calibration: ctx.calibration,
        confidence: e.confidence,
        revisit_biased: false,
        fco_window: Some(e.window),
    })
}

/// SRO: mean FCO over the band's channel rows that have one.
pub fn sro(channel_rows: &[OccupancyStat]) -> Option<f64> {
    let v: Vec<f64> = channel_rows.iter().filter_map(|r| r.fco).collect();
    (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64)
}

/// SM.2256-1 Annex 1 (A12): samples needed for channels with lengthy signals, given the expected
/// number of signals `v_avr`, absolute error `delta_so`, revisit instability `delta_t` and the
/// percentage point `x_p`: `x_p/(2Δ)·√(V(1.06+δT²))`. The Report's A16 writes the 95 %/0.5 % case
/// with constant 194.2 (x_p ≈ 1.942), which reproduces Table A1.
pub fn sm2256_lengthy_samples(v_avr: f64, delta_so: f64, delta_t: f64, x_p: f64) -> f64 {
    x_p / (2.0 * delta_so) * (v_avr * (1.06 + delta_t * delta_t)).sqrt()
}

/// SM.2256-1 Annex 1 (A18): samples needed for channels with pulsed signals at occupancy `so`:
/// `SO(1−SO)(x_p/Δ)²`, the binomial normal approximation.
pub fn sm2256_pulsed_samples(so: f64, delta_so: f64, x_p: f64) -> f64 {
    so * (1.0 - so) * (x_p / delta_so).powi(2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hk_model::attention::occupancy::ChannelKey;

    /// Deterministic xorshift in [0, 1).
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> f64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 11) as f64 / (1u64 << 53) as f64
        }
        fn exp(&mut self, mean: f64) -> f64 {
            -mean * (1.0 - self.next()).ln()
        }
    }

    /// A two-state Markov channel over `span_s`: (start, end) on intervals, realized FCO.
    fn markov(rng: &mut Rng, span_s: f64, fco: f64, mean_on_s: f64) -> (Vec<(f64, f64)>, f64) {
        if fco >= 1.0 {
            return (vec![(0.0, span_s)], 1.0);
        }
        let mean_off = mean_on_s * (1.0 - fco) / fco;
        let mut on = rng.next() < fco;
        let (mut t, mut ivs, mut on_s) = (0.0, Vec::new(), 0.0);
        while t < span_s {
            let d = rng.exp(if on { mean_on_s } else { mean_off });
            let e = (t + d).min(span_s);
            if on {
                ivs.push((t, e));
                on_s += e - t;
            }
            t = e;
            on = !on;
        }
        (ivs, on_s / span_s)
    }

    fn state(ivs: &[(f64, f64)], t: f64) -> bool {
        ivs.iter().any(|&(a, b)| t >= a && t < b)
    }

    fn sample(t_s: f64, tier: Tier, occupied: bool, suspect: bool) -> VisitSample {
        VisitSample {
            start_ns: (t_s * 1e9) as i64,
            dur_ns: 500_000_000,
            tier,
            occupied,
            suspect,
            above_fraction: f32::from(u8::from(occupied)),
            threshold_db: -100.0,
            guard_clamped: false,
        }
    }

    fn span(a_s: f64, b_s: f64) -> TimeRange {
        TimeRange::new(
            Timestamp::from_unix_nanos((a_s * 1e9) as i64),
            Timestamp::from_unix_nanos((b_s * 1e9) as i64),
        )
    }

    #[test]
    fn occupancy_fco_on_markov_channels_under_irregular_revisits_is_inside_the_interval() {
        // 1/10/50/100 % channels, mean on 60 s, 48 h, irregular revisits (exponential gaps, mean
        // 300 s, min 5 s) and, separately, bursty revisits (clusters of 5 visits 2 s apart), the
        // correlated case n_eff exists for. Per seed and channel the 95 % interval must hold the
        // realized FCO; over 20 seeds × 4 channels × 2 schedules at least 90 % must.
        let span_s = 48.0 * 3600.0;
        let (mut inside, mut total) = (0, 0);
        for seed in 1..=20u64 {
            let mut rng = Rng(0x9e37_79b9_7f4a_7c15 ^ seed.wrapping_mul(0x2545_f491));
            for (chn, &fco) in [0.01, 0.10, 0.50, 1.00].iter().enumerate() {
                let (ivs, realized) = markov(&mut rng, span_s, fco, 60.0);
                for bursty in [false, true] {
                    let mut t = 0.0;
                    let mut samples = Vec::new();
                    while t < span_s {
                        let k = if bursty { 5 } else { 1 };
                        for j in 0..k {
                            let tj = t + f64::from(j) * 2.0;
                            if tj < span_s {
                                samples.push(sample(
                                    tj,
                                    Tier::BackgroundSweep,
                                    state(&ivs, tj),
                                    false,
                                ));
                            }
                        }
                        t += rng.exp(300.0).max(5.0) + if bursty { 10.0 } else { 0.0 };
                    }
                    let e = estimate(&samples, span(0.0, span_s), ConfidenceLevel::P95);
                    let ci = e.confidence.unwrap();
                    total += 1;
                    if realized >= ci.lo - 1e-12 && realized <= ci.hi + 1e-12 {
                        inside += 1;
                    } else {
                        eprintln!(
                            "seed {seed} ch {chn} bursty {bursty}: realized {realized:.4} fco \
                             {:.4} ci [{:.4}, {:.4}] n_eff {:.0}",
                            e.fco.unwrap(),
                            ci.lo,
                            ci.hi,
                            ci.n_eff
                        );
                    }
                    if bursty && fco > 0.05 && fco < 1.0 {
                        assert!(ci.n_eff < e.n_revisits as f64, "correlation not corrected");
                    }
                }
            }
        }
        assert!(
            inside * 10 >= total * 9,
            "{inside}/{total} intervals hold the truth"
        );
    }

    #[test]
    fn occupancy_time_weighting_matches_sm2256_a8_a11_and_resists_revisit_clustering() {
        // Uneven revisits: the trapezoid rule of Annex 1 A8–A11 equals the half-gap weights.
        let times = [0.0, 10.0, 15.0, 45.0, 50.0, 90.0];
        let occ = [true, true, false, false, true, true];
        let samples: Vec<VisitSample> = times
            .iter()
            .zip(occ)
            .map(|(&t, o)| VisitSample {
                dur_ns: 0,
                ..sample(t, Tier::ScheduledPlan, o, false)
            })
            .collect();
        let e = estimate(&samples, span(0.0, 100.0), ConfidenceLevel::P95);
        let mids: Vec<i64> = times.iter().map(|t| (t * 1e9) as i64).collect();
        let w = weights(&mids);
        let expect = w
            .iter()
            .zip(occ)
            .filter(|(_, o)| *o)
            .map(|(w, _)| w)
            .sum::<f64>()
            / w.iter().sum::<f64>();
        assert!((e.fco.unwrap() - expect).abs() < 1e-12);
        // Uncapped half-gap weights = trapezoid T_O/T_AI over the interior.
        let (mut tai, mut to) = (0.0, 0.0);
        for j in 1..times.len() {
            let tr = times[j] - times[j - 1];
            tai += tr;
            to += match (occ[j - 1], occ[j]) {
                (true, true) => tr,
                (false, false) => 0.0,
                _ => tr / 2.0,
            };
        }
        let half: Vec<f64> = (0..times.len())
            .map(|i| {
                let p = if i > 0 {
                    (times[i] - times[i - 1]) / 2.0
                } else {
                    0.0
                };
                let q = if i + 1 < times.len() {
                    (times[i + 1] - times[i]) / 2.0
                } else {
                    0.0
                };
                p + q
            })
            .collect();
        let hw = half
            .iter()
            .zip(occ)
            .filter(|(_, o)| *o)
            .map(|(w, _)| w)
            .sum::<f64>()
            / half.iter().sum::<f64>();
        assert!((hw - to / tai).abs() < 1e-12, "{hw} vs {}", to / tai);
        // A cluster of 50 visits inside one busy minute does not dominate: plain counting gives
        // ~0.9, time weighting stays near the evenly-sampled 0.5.
        let mut s: Vec<VisitSample> = (0..40)
            .map(|k| {
                sample(
                    f64::from(k) * 60.0,
                    Tier::BackgroundSweep,
                    k % 2 == 0,
                    false,
                )
            })
            .collect();
        s.extend((0..50).map(|k| {
            sample(
                600.0 + f64::from(k) * 0.5 + 1.0,
                Tier::BackgroundSweep,
                true,
                false,
            )
        }));
        let e = estimate(&s, span(0.0, 2400.0), ConfidenceLevel::P95);
        assert!((e.fco.unwrap() - 0.5).abs() < 0.08, "{:?}", e.fco);
    }

    #[test]
    fn occupancy_fco_ignores_bandit_visits_and_all_visits_is_information_only() {
        // Sweep visits every 60 s see 20 % occupancy; the bandit dwells only while busy.
        let mut s: Vec<VisitSample> = (0..100)
            .map(|k| {
                sample(
                    f64::from(k) * 60.0,
                    Tier::BackgroundSweep,
                    k % 5 == 0,
                    false,
                )
            })
            .collect();
        s.extend((0..200).map(|k| sample(f64::from(k) * 30.0 + 7.0, Tier::Bandit, true, false)));
        let e = estimate(&s, span(0.0, 6000.0), ConfidenceLevel::P95);
        assert!((e.fco.unwrap() - 0.2).abs() < 0.02, "{:?}", e.fco);
        assert!(e.fco_all_visits.unwrap() > 0.5);
        assert_eq!((e.n_revisits, e.n_revisits_all), (100, 300));
    }

    #[test]
    fn occupancy_suspect_crossings_are_excluded_and_bound_from_above() {
        let mut s: Vec<VisitSample> = (0..100)
            .map(|k| {
                sample(
                    f64::from(k) * 10.0,
                    Tier::BackgroundSweep,
                    k % 4 == 0,
                    false,
                )
            })
            .collect();
        for k in (1..100).step_by(4) {
            s[k].occupied = true;
            s[k].suspect = true;
        }
        let e = estimate(&s, span(0.0, 1000.0), ConfidenceLevel::P95);
        assert_eq!(e.n_suspect, 25);
        assert_eq!(e.n_occupied, 25);
        // Suspect visits are unobserved: their time goes to the neighbours (15+15+10 per cycle).
        assert!((e.fco.unwrap() - 15.0 / 40.0).abs() < 0.02, "{:?}", e.fco);
        assert!((e.fco_suspect_upper.unwrap() - 0.5).abs() < 0.02);
        assert!(e.fco.unwrap() <= e.fco_suspect_upper.unwrap());
    }

    #[test]
    fn occupancy_sparse_intervals_widen_never_substitute() {
        let cfg = EngineConfig::default();
        let ctx = SubjectContext {
            subject: OccupancySubject::Channel {
                key: ChannelKey {
                    scheme: 1,
                    lo_cell: 0,
                    hi_cell: 2,
                },
            },
            rbw_hz: 6250.0,
            obw_hz: Some(12_500.0),
            unit: PowerUnit::Dbfs,
            calibration: None,
        };
        let day = 86_400.0;
        // Sweep visits every 80 s (11 per 15 min) seeing 25 %; the bandit dwells while busy.
        let mut s: Vec<VisitSample> = (0..1080)
            .map(|k| {
                sample(
                    f64::from(k) * 80.0 + 3.0,
                    Tier::BackgroundSweep,
                    k % 4 == 0,
                    false,
                )
            })
            .collect();
        s.extend((0..2000).map(|k| sample(f64::from(k) * 40.0 + 11.0, Tier::Bandit, true, false)));
        let iv = span(day * 0.5, day * 0.5 + 900.0);
        let row = stat(&cfg, &ctx, iv, &s, span(0.0, day), None).unwrap();
        row.validate().unwrap();
        let w = row.fco_window.unwrap();
        assert_eq!(
            w.duration_ns(),
            3_600_000_000_000,
            "widened to the aligned hour"
        );
        assert!(row.n_revisits >= 30 && !row.revisit_biased);
        assert!((row.fco.unwrap() - 0.25).abs() < 0.1, "{:?}", row.fco);
        assert!(row.fco_all_visits.unwrap() > row.fco.unwrap());
        // No window reaches 30: fco stays activity-independent with a wide interval.
        let few: Vec<VisitSample> = (0..8)
            .map(|k| sample(f64::from(k) * 3000.0, Tier::ScheduledPlan, k == 0, false))
            .chain((0..500).map(|k| sample(f64::from(k) * 40.0 + 1.0, Tier::Bandit, true, false)))
            .collect();
        let row = stat(
            &cfg,
            &ctx,
            span(0.0, 900.0),
            &few,
            span(0.0, 24_000.0),
            None,
        )
        .unwrap();
        row.validate().unwrap();
        assert_eq!(row.n_revisits, 8);
        assert!((row.fco.unwrap() - 0.125).abs() < 0.05, "{:?}", row.fco);
        let ci = row.confidence.unwrap();
        assert!(ci.hi - ci.lo > 0.3);
        // Nothing observed: no row.
        assert!(stat(&cfg, &ctx, span(0.0, 900.0), &[], span(0.0, 900.0), None).is_none());
    }

    #[test]
    fn occupancy_widening_ladder_is_aligned_and_monotone() {
        let iv = span(3600.0 * 25.0 + 900.0, 3600.0 * 25.0 + 1800.0);
        let w = widen_windows(iv, span(0.0, 3600.0 * 48.0));
        let d: Vec<i64> = w.iter().map(|w| w.duration_ns() / 1_000_000_000).collect();
        assert_eq!(d, [900, 3600, 21600, 86400, 172800]);
        assert!(w.iter().all(|x| x.start <= iv.start && x.end >= iv.end));
    }

    #[test]
    fn occupancy_sm2256_annex1_sample_counts_reproduce_tables_a1_and_a2() {
        // Table A1 (lengthy signals, 95 %, ΔSO 0.5 %, δT 0.5): A16 constant 194.2.
        for (v, j) in [
            (10.0, 703.0),
            (30.0, 1217.0),
            (50.0, 1572.0),
            (100.0, 2223.0),
            (300.0, 3850.0),
            (500.0, 4970.0),
        ] {
            let got = sm2256_lengthy_samples(v, 0.005, 0.5, 1.942);
            assert!((got - j).abs() <= 1.0, "V {v}: {got} vs {j}");
        }
        // Table A2 (pulsed signals, 95 % two-sided x_p 1.96, ΔSO 0.5 %): A19 = 153 664·SO(1−SO).
        for (so, j) in [
            (0.05, 7300.0),
            (0.10, 13830.0),
            (0.20, 24586.0),
            (0.35, 34960.0),
            (0.50, 38416.0),
        ] {
            let got = sm2256_pulsed_samples(so, 0.005, 1.96);
            assert!((got - j).abs() / j < 0.001, "SO {so}: {got} vs {j}");
        }
    }

    #[test]
    fn occupancy_memory_observations_resolve_dwells_and_sweep_hops_by_full_coverage() {
        use hk_model::attention::observation::{
            DwellRecord, HopVisit, ObservedWindow, Reason, SweepGeometry,
        };
        let t0 = Timestamp::from_unix_nanos(1_000_000_000_000);
        let win = |c: f64| ObservedWindow {
            center_hz: c,
            sample_rate_hz: 2e6,
            usable: FreqRange::centered(c, 1.6e6),
            dc_excluded: Some(FreqRange::centered(c, 20e3)),
            rbw_hz: 1e3,
        };
        let mut m = MemoryObservations::default();
        m.push(ObservationRecord::Geometry(SweepGeometry {
            schema: ATTENTION_SCHEMA_VERSION,
            id: 7,
            plan_version: 1,
            hops: vec![win(100e6), win(102e6)],
        }));
        m.push(ObservationRecord::Sweep(SweepRecord {
            schema: ATTENTION_SCHEMA_VERSION,
            survey_id: None,
            plan_version: 1,
            site: SiteKey::Unassigned,
            geometry: 7,
            span: TimeRange::new(t0, t0.saturating_add_nanos(1_000_000_000)),
            visits: vec![
                HopVisit {
                    hop: 0,
                    start_ms: 0,
                    observed_ms: 50,
                },
                HopVisit {
                    hop: 1,
                    start_ms: 50,
                    observed_ms: 50,
                },
            ],
            preempted_hops: 0,
            dropped_samples: 0,
            overload_hops: 0,
        }));
        m.push(ObservationRecord::Dwell(DwellRecord {
            schema: ATTENTION_SCHEMA_VERSION,
            survey_id: None,
            seq: 1,
            plan_version: 1,
            site: SiteKey::Unassigned,
            reason: Reason::PoiDwell { poi: 1 },
            tier: Tier::Bandit,
            window: win(100e6),
            rf_path: 0,
            planned: TimeRange::new(
                t0.saturating_add_nanos(2_000_000_000),
                t0.saturating_add_nanos(3_000_000_000),
            ),
            observed: TimeRange::new(
                t0.saturating_add_nanos(2_000_000_000),
                t0.saturating_add_nanos(3_000_000_000),
            ),
            preempted: false,
            dropped_samples: 0,
            overload: true,
            provenance_ref: None,
        }));
        let all = TimeRange::new(t0, t0.saturating_add_nanos(10_000_000_000));
        let v = m.visits(FreqRange::centered(100.3e6, 25e3), all);
        assert_eq!(v.len(), 2);
        assert_eq!(
            (v[0].tier, v[1].tier, v[1].overload),
            (Tier::BackgroundSweep, Tier::Bandit, true)
        );
        // Straddling the DC notch or the hop edge is not observed.
        assert!(m.visits(FreqRange::centered(100e6, 25e3), all).is_empty());
        assert!(m.visits(FreqRange::new(100.79e6, 101.21e6), all).is_empty());
        assert_eq!(m.visits(FreqRange::centered(101.5e6, 25e3), all).len(), 1);
    }
}
