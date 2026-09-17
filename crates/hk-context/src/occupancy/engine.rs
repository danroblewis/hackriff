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
//!    "any sample in the channel"). The threshold is resolved per grid column over its **local**
//!    floor ([`local_floors`]: the column's own 80 % floor of history floors, or of levels, unless
//!    it sits more than the guard above the 20th percentile of column floors within ±1 MHz; dense
//!    neighbourhoods flag the floor suspect) with [`super::threshold::resolve`] (guard; RBW = the
//!    level-0 cell width, corrected when it is below the channel OBW). A crossing
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
use hk_model::attention::baseline::{ChainKey, SiteKey};
use hk_model::attention::observation::{ObservationRecord, SweepRecord, Tier};
use hk_model::attention::occupancy::{
    ConfidenceInterval, ConfidenceLevel, FloorSource, OccupancyStat, OccupancySubject,
    ThresholdMethod, ThresholdSpec, TimingRegime, effective_samples, fraction_interval,
};
use hk_model::frames::PowerUnit;
use hk_model::ids::CalibrationStateId;
use hk_model::{BiasTee, FreqRange, TimeRange, Timestamp};
use hk_store::history::{OriginField, OriginFilter};
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

    /// Analysed extents (usable band of each dwell window or visited sweep hop, DC notch
    /// included) of the observations starting in `span`, unordered and possibly repeated: the
    /// bands an interval must evaluate. Empty when the source cannot tell.
    fn bands(&self, span: TimeRange) -> Vec<FreqRange> {
        let _ = span;
        Vec::new()
    }
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

    fn bands(&self, span: TimeRange) -> Vec<FreqRange> {
        let hull = |c: &[FreqRange]| {
            let lo = c.iter().map(|r| r.lo_hz).fold(f64::INFINITY, f64::min);
            let hi = c.iter().map(|r| r.hi_hz).fold(f64::NEG_INFINITY, f64::max);
            (hi > lo).then(|| FreqRange::new(lo, hi))
        };
        let mut out: Vec<FreqRange> = self
            .dwells
            .iter()
            .filter(|(_, v)| starts_in(v.observed.start, span))
            .filter_map(|(c, _)| hull(c))
            .collect();
        for s in &self.sweeps {
            let Some(hops) = self.geometries.get(&s.geometry) else {
                continue;
            };
            for hv in &s.visits {
                let start = s
                    .span
                    .start
                    .saturating_add_nanos(i64::from(hv.start_ms) * 1_000_000);
                if starts_in(start, span)
                    && let Some(b) = hops.get(hv.hop as usize).and_then(|c| hull(c))
                {
                    out.push(b);
                }
            }
        }
        out
    }
}

/// Level-0 history for a region (the evaluation input).
///
/// **T-314: a level source decides which front ends its grid may contain, and the caller must
/// know which it chose.** Occupancy is measured *through* a receive chain — the level it reports
/// is that chain's antenna, cable, LNA and mixer as much as it is the air — so a grid pooling two
/// front ends produces a statistic that belongs to neither, and keying the baseline it folds into
/// (T-303) cannot undo the pooling that already happened in the measurement. The plain
/// implementation for [`hk_store::Pyramid`] pools **every** origin, which is right for a
/// whole-history question and wrong for a per-chain one; [`OriginLevels`] restricts the read.
pub trait LevelSource {
    /// Level-0 grid over `freq` × `span`, `None` when unreadable.
    fn level0(&self, freq: FreqRange, span: TimeRange) -> Option<RegionHistory>;
}

/// Every origin the pyramid holds, pooled — the unfiltered read (see [`OriginLevels`] for the
/// per-chain one).
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

/// T-314: a pyramid read restricted to one origin — the front end (and/or site) whose frames the
/// caller's statistic is allowed to contain.
///
/// The restriction is the T-133 [`OriginFilter`], and its rule for a level-0 cell that **cannot**
/// be attributed to one origin is the one this task needs: a tile whose frames come from more
/// than one origin answers its level-0 cells as **unobserved** (counted in
/// [`hk_store::history::FilterSummary::cells_excluded`]), never as the mixture and never as the
/// origin that contributed most. That is deliberately the same choice T-359 made one layer up for
/// the bias tee — a measurement taken through two receive chains claims neither — and it is why a
/// per-chain read is honest rather than merely narrower. The price is coverage: while two front
/// ends' frames interleave inside one tile, neither chain can be measured there at all, and its
/// cohort stays [`hk_model::attention::baseline::Maturity::Immature`] until it has tiles of its
/// own (T-333: bounded silence is the chosen behaviour).
///
/// Unknown-origin frames (tiles written before history format 3, and frames whose source was
/// never recorded) pass only [`hk_store::history::OriginField::Any`] or
/// [`hk_store::history::OriginField::Unknown`]. `OriginField` has no "this source **or** unknown",
/// so a per-chain read cannot adopt origin-less history: such tiles read as another origin's and
/// are excluded. Filtering on nothing ([`OriginFilter::ANY`]) is the pooled read, identical to
/// [`hk_store::Pyramid`]'s own.
pub struct OriginLevels<'a> {
    /// The pyramid to read.
    pub pyramid: &'a hk_store::Pyramid,
    /// Which origins' frames the grid may contain.
    pub filter: OriginFilter,
}

impl LevelSource for OriginLevels<'_> {
    fn level0(&self, freq: FreqRange, span: TimeRange) -> Option<RegionHistory> {
        self.pyramid
            .query_filtered(
                &RegionQuery {
                    freq,
                    time: span,
                    resolution: Resolution::Level(0),
                },
                &self.filter,
            )
            .ok()
    }
}

/// T-314: the history read that measures the receive chain `chain` only.
///
/// [`ChainKey::of_device`] hashes the device id exactly as `hk_store::history::source_key` does
/// (a test in hk-store pins the two together), so the filter selects the frames whose provenance
/// names the same front end the baseline is keyed by: the key and the measurement are the same
/// value, which is the whole of T-314. [`ChainKey::Unknown`] — no device named — restricts
/// nothing, which is what the pooled read was before.
pub fn chain_filter(chain: ChainKey) -> OriginFilter {
    OriginFilter {
        source: match chain.id() {
            Some(id) => OriginField::Is(id),
            None => OriginField::Any,
        },
        site: OriginField::Any,
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

/// Local noise-floor settings (T-118 review).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LocalFloorConfig {
    /// Half-width of a column's neighbourhood, Hz (±1 MHz).
    pub radius_hz: f64,
    /// Percentile of the neighbourhood's column floors taken as its reference (0.2).
    pub percentile: f64,
    /// Mean history occupancy of the neighbourhood above which its floor is suspect (0.8).
    pub dense_fraction: f64,
}

impl Default for LocalFloorConfig {
    fn default() -> Self {
        Self {
            radius_hz: 1e6,
            percentile: 0.2,
            dense_fraction: 0.8,
        }
    }
}

/// Per-column noise floors of one grid (index = grid column).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ColumnFloors {
    /// Floor, dB/Hz; NaN when the column has no observed cell.
    pub floor_db: Vec<f64>,
    /// Its source.
    pub source: Vec<FloorSource>,
    /// The neighbourhood is dense (its floor may be signal).
    pub suspect: Vec<bool>,
}

impl ColumnFloors {
    /// The `len` columns starting at `offset`.
    fn window(mut self, offset: usize, len: usize) -> Self {
        self.floor_db = self.floor_db[offset..offset + len].to_vec();
        self.source = self.source[offset..offset + len].to_vec();
        self.suspect = self.suspect[offset..offset + len].to_vec();
        self
    }
}

/// Local noise floors of every column of `grid` (T-118 review: a whole-band floor is pulled low by
/// passband ripple and edge roll-off, and in a dense band its lowest fifth is signal).
///
/// 1. **Column floor:** the 80 % method (discard the highest `discard_fraction`, linearly average
///    the rest) over the column's observed cells' bias-corrected `floor_db` (source `History`), or
///    over their `mean_db` when no cell has a floor (source `EightyPercent`). A channel idle for
///    at least a fifth of the time gets its noise here.
/// 2. **Reference:** the `percentile` (20th) of the column floors within `radius_hz` below the
///    column and, separately, above it; the higher of the two (a side with fewer than a quarter of
///    its columns measured is ignored).
/// 3. **Floor:** the column's own floor unless it is more than `guard_db` above the reference,
///    i.e. above both sides (it follows ripple and roll-off, which a percentile over ±1 MHz
///    cannot); else the reference (the column is busy nearly all the time, so its own floor is
///    signal). A continuous signal less than `guard_db` above the reference could not cross a
///    reference threshold either.
/// 4. **Suspect:** the neighbourhood's mean history occupancy (fraction of time above the tracker
///    threshold) exceeds `dense_fraction`: the reference itself is then likely signal. (A bias-model
///    noise level for the gain state is not in the history grid, so the occupancy rule is used.)
pub fn local_floors(
    grid: &RegionHistory,
    discard_fraction: f64,
    guard_db: f64,
    cfg: &LocalFloorConfig,
) -> ColumnFloors {
    let nf = grid.nf;
    let mut col = vec![f64::NAN; nf];
    let mut src = vec![FloorSource::History; nf];
    let mut occ = vec![f64::NAN; nf];
    let mut scratch = Vec::with_capacity(grid.nt);
    for f in 0..nf {
        scratch.clear();
        let (mut occ_sum, mut occ_n) = (0.0, 0u32);
        for t in 0..grid.nt {
            if cell_ok(grid, t, f) {
                let c = grid.cell(t, f);
                if c.floor_db.is_finite() {
                    scratch.push(f64::from(c.floor_db));
                }
                if c.occupancy.is_finite() {
                    occ_sum += f64::from(c.occupancy);
                    occ_n += 1;
                }
            }
        }
        if scratch.is_empty() {
            src[f] = FloorSource::EightyPercent;
            scratch.extend(
                (0..grid.nt)
                    .filter(|&t| cell_ok(grid, t, f))
                    .map(|t| f64::from(grid.cell(t, f).mean_db)),
            );
        }
        if let Some(v) = threshold::eighty_percent_floor_db(&scratch, discard_fraction) {
            col[f] = v;
        }
        if occ_n > 0 {
            occ[f] = occ_sum / f64::from(occ_n);
        }
    }
    let r = if grid.f_cell_hz > 0.0 {
        (cfg.radius_hz.max(0.0) / grid.f_cell_hz).round() as usize
    } else {
        0
    };
    let pct = cfg.percentile.clamp(0.0, 1.0);
    let mut out = ColumnFloors {
        floor_db: vec![f64::NAN; nf],
        source: src.clone(),
        suspect: vec![false; nf],
    };
    let min_side = (r / 4).max(1);
    let mut win: Vec<(f64, FloorSource)> = Vec::with_capacity(r + 1);
    let mut side = |range: std::ops::Range<usize>| {
        win.clear();
        win.extend(
            range
                .filter(|&k| col[k].is_finite())
                .map(|k| (col[k], src[k])),
        );
        if win.len() < min_side {
            return None;
        }
        win.sort_by(|a, b| a.0.total_cmp(&b.0));
        Some(win[((win.len() - 1) as f64 * pct).round() as usize])
    };
    for f in 0..nf {
        let (lo, hi) = (f.saturating_sub(r), (f + r + 1).min(nf));
        let (mut occ_sum, mut occ_n) = (0.0, 0u32);
        for &o in occ[lo..hi].iter().filter(|o| o.is_finite()) {
            occ_sum += o;
            occ_n += 1;
        }
        out.suspect[f] = occ_n > 0 && occ_sum / f64::from(occ_n) > cfg.dense_fraction;
        // Busy only when above both sides: a roll-off or slope on one side cannot pull the
        // reference under a noise column.
        let reference = match (side(lo..f), side(f + 1..hi)) {
            (Some(l), Some(h)) => Some(if l.0 >= h.0 { l } else { h }),
            (one, None) | (None, one) => one,
        };
        (out.floor_db[f], out.source[f]) = match reference {
            Some(rf) if !col[f].is_finite() || col[f] > rf.0 + guard_db => rf,
            _ if col[f].is_finite() => (col[f], src[f]),
            _ => continue,
        };
    }
    out
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
    /// Visit level: the highest cell `mean_db` over the subject's cells and rows, dB/Hz.
    pub level_db: f32,
    /// Floor under the threshold (median over the subject's columns), dB/Hz.
    pub floor_db: f32,
    /// Its source.
    pub floor_source: FloorSource,
    /// Most of the subject's columns have a suspect (dense) local floor.
    pub floor_suspect: bool,
    /// Front-end gain-state key of the grid the visit was read from (the dominant gain state of
    /// its tiles; 0 = unknown), T-132.
    pub gain_key: u32,
    /// Antenna-port bias-tee state of the grid the visit was read from (T-359).
    ///
    /// [`BiasTee::Unknown`] when the grid's tiles pooled more than one state
    /// (`ProvenanceSummary::bias_tee_mixed`) as well as when the source could not report one: in
    /// both cases no single state can be attributed to the visit, and claiming one would put a
    /// measurement of two receive chains into one cohort — the masking T-333 refuses. Never `Off`
    /// by default (T-325).
    pub bias_tee: BiasTee,
}

/// The gain-state key most of `samples`' observed time was under (0 when none is known): an
/// interval's baseline gain-state key (T-132).
pub fn dominant_gain_key<'a>(samples: impl IntoIterator<Item = &'a VisitSample>) -> u32 {
    let mut by: std::collections::BTreeMap<u32, i64> = std::collections::BTreeMap::new();
    for s in samples {
        if s.gain_key != 0 {
            *by.entry(s.gain_key).or_default() += s.dur_ns.max(1);
        }
    }
    by.into_iter().max_by_key(|(_, d)| *d).map_or(0, |(k, _)| k)
}

/// The bias-tee state of `samples` when **every** one of them agrees, else [`BiasTee::Unknown`]
/// (T-359): the state an occupancy row was measured under.
///
/// Unanimity, not a majority, and deliberately unlike [`dominant_gain_key`]: a row that straddles a
/// switch was measured under two receive chains, and labelling it with the longer one would fold a
/// mixture into a pure cohort — exactly the pooling T-333 refuses, where a 12 dB rise scores 1.000
/// against its own cohort and 0.000 against the pooled one. `Unknown` says no single state can be
/// attributed to the row, which is true of a straddling row and of a row from a source that cannot
/// report; it never means off. An empty sample set is `Unknown` for the same reason.
pub fn interval_bias_tee<'a>(samples: impl IntoIterator<Item = &'a VisitSample>) -> BiasTee {
    let mut seen: Option<BiasTee> = None;
    for s in samples {
        match seen {
            Some(b) if b != s.bias_tee => return BiasTee::Unknown,
            _ => seen = Some(s.bias_tee),
        }
    }
    seen.unwrap_or(BiasTee::Unknown)
}

/// The bias-tee state to attribute to visits read from `p`: its state, or [`BiasTee::Unknown`] when
/// its tiles pooled more than one (T-359).
pub fn grid_bias_tee(p: &hk_store::ProvenanceSummary) -> BiasTee {
    if p.bias_tee_mixed {
        BiasTee::Unknown
    } else {
        p.bias_tee
    }
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
    /// Local floors of `grid`'s columns ([`local_floors`]), preferred over the subject's own
    /// cells: a busy channel's low-percentile floor is its own signal level.
    pub floors: &'a ColumnFloors,
}

/// Evaluates `visits` of the subject `freq` (occupied bandwidth `obw_hz`) against `grid`, each
/// cell against the threshold over its column's local floor. Returns the samples and a
/// representative threshold (medians over the subject's columns; none when no floor could be
/// resolved).
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
    let fl = input.floors;
    let mut thr_col = vec![f64::NAN; b - a];
    let mut floors = Vec::with_capacity(b - a);
    let (mut clamped, mut n_suspect) = (false, 0usize);
    let mut sources = [0usize; 3];
    for (k, f) in (a..b).enumerate() {
        let measured = fl.floor_db.get(f).copied().filter(|x| x.is_finite());
        let Some(t) = threshold::resolve(spec, measured, &[], obw_hz, grid.f_cell_hz) else {
            continue;
        };
        thr_col[k] = t.threshold_db;
        floors.push(t.floor_db);
        clamped |= t.guard_clamped;
        let source = match measured {
            Some(_) => fl.source.get(f).copied().unwrap_or(FloorSource::History),
            None => t.source,
        };
        sources[source as usize] += 1;
        n_suspect += usize::from(fl.suspect.get(f).copied().unwrap_or(false));
    }
    let Some(thr_rep) = median_finite(thr_col.iter().copied()) else {
        return (Vec::new(), None);
    };
    thr_col
        .iter_mut()
        .filter(|x| !x.is_finite())
        .for_each(|x| *x = thr_rep);
    let floor_source = [
        FloorSource::History,
        FloorSource::EightyPercent,
        FloorSource::Assumed,
    ][(0..3).max_by_key(|&i| (sources[i], 3 - i)).unwrap_or(0)];
    let thr = AppliedThreshold {
        floor_db: median_finite(floors).unwrap_or(f64::NAN),
        threshold_db: thr_rep,
        guard_clamped: clamped,
        source: floor_source,
    };
    let floor_suspect = 2 * n_suspect > b - a;
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
                above[k] |= m > thr_col[k];
            }
        }
        if !any_row {
            continue;
        }
        let occupied = above.iter().any(|&x| x);
        let window = TimeRange::new(
            v.observed.start.saturating_add_nanos(-slack),
            v.observed.end.saturating_add_nanos(slack),
        );
        // §2.6: a crossing is suspect where it coincides with a suspect detection (visit window
        // ± one time cell; the detection's extent widened by one cell, so a spur's leakage into
        // the adjacent cell is covered); the visit is suspect when all its crossings are. One
        // clean crossing makes it occupied and not suspect (T-129).
        let suspect = occupied
            && (v.overload || {
                let widen = grid.f_cell_hz;
                let active = |d: &DetectionExtent| {
                    d.suspect
                        && d.freq.lo_hz - widen < freq.hi_hz
                        && d.freq.hi_hz + widen > freq.lo_hz
                        && d.time.start <= window.end
                        && d.time.end >= window.start
                };
                input.detections.iter().any(active)
                    && (a..b).zip(&above).all(|(f, &x)| {
                        let lo = (grid.f_first_cell + f as i64) as f64 * grid.f_cell_hz;
                        let hi = lo + grid.f_cell_hz;
                        !x || input.detections.iter().any(|d| {
                            active(d) && d.freq.lo_hz - widen < hi && d.freq.hi_hz + widen > lo
                        })
                    })
            });
        out.push(VisitSample {
            start_ns: s,
            dur_ns: (e - s).max(0),
            tier: v.tier,
            occupied,
            suspect,
            above_fraction: above.iter().filter(|&&x| x).count() as f32 / above.len() as f32,
            threshold_db: thr.threshold_db as f32,
            guard_clamped: thr.guard_clamped,
            level_db: level as f32,
            floor_db: thr.floor_db as f32,
            floor_source,
            floor_suspect,
            gain_key: grid.provenance.dominant_gain_key(),
            bias_tee: grid_bias_tee(&grid.provenance),
        });
    }
    (out, Some(thr))
}

/// Linearly interpolated `p` quantile of the finite values.
fn quantile_finite(values: impl IntoIterator<Item = f64>, p: f64) -> Option<f64> {
    let mut v: Vec<f64> = values.into_iter().filter(|x| x.is_finite()).collect();
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    let pos = p.clamp(0.0, 1.0) * (v.len() - 1) as f64;
    let (i, frac) = (pos.floor() as usize, pos.fract());
    Some(match v.get(i + 1) {
        Some(n) => v[i] + frac * (n - v[i]),
        None => v[i],
    })
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
    /// Median floor under the thresholds, dB/Hz.
    pub floor_db: Option<f64>,
    /// Most common floor source.
    pub floor_source: Option<FloorSource>,
    /// The bias-tee state every visit of the window agreed on, else [`BiasTee::Unknown`]
    /// ([`interval_bias_tee`], T-359).
    pub bias_tee: BiasTee,
    /// Any visit's floor was suspect.
    pub floor_suspect: Option<bool>,
    /// Median / 90th percentile level of the occupied `fco` visits (activity-independent, not
    /// suspect), dB/Hz.
    pub level_occupied_p50_db: Option<f64>,
    /// See `level_occupied_p50_db`.
    pub level_occupied_p90_db: Option<f64>,
    /// Median level of the idle `fco` visits, dB/Hz.
    pub level_idle_db: Option<f64>,
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
    // then weighted by their observed duration.
    let mut strata: HashMap<i64, (f64, f64)> = HashMap::new();
    for s in sel.iter().filter(|s| !s.suspect) {
        let e = strata.entry(s.mid_ns().div_euclid(STRATUM_NS)).or_default();
        let d = s.dur_ns.max(1) as f64;
        e.0 += d * f64::from(u8::from(s.occupied));
        e.1 += d;
    }
    let fco_all_visits = (!strata.is_empty()).then(|| {
        let (num, den) = strata
            .values()
            .fold((0.0, 0.0), |(n, w), (o, d)| (n + (o / d) * d, w + d));
        num / den
    });
    let mut sources = [0usize; 3];
    for s in &sel {
        sources[s.floor_source as usize] += 1;
    }
    let floor_source = (!sel.is_empty()).then(|| {
        [
            FloorSource::History,
            FloorSource::EightyPercent,
            FloorSource::Assumed,
        ][(0..3).max_by_key(|&i| (sources[i], 3 - i)).unwrap_or(0)]
    });
    let level_of = |s: &&VisitSample| f64::from(s.level_db);
    let occupied_levels: Vec<f64> = clean.iter().filter(|s| s.occupied).map(level_of).collect();
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
        floor_db: median_finite(sel.iter().map(|s| f64::from(s.floor_db))),
        floor_source,
        // T-359: over every visit of the window, of any tier — the row's measurement context is
        // what the front end was doing while it was measured, not what the clean visits were.
        bias_tee: interval_bias_tee(sel.iter().copied()),
        floor_suspect: (!sel.is_empty()).then(|| sel.iter().any(|s| s.floor_suspect)),
        level_occupied_p50_db: quantile_finite(occupied_levels.iter().copied(), 0.5),
        level_occupied_p90_db: quantile_finite(occupied_levels.iter().copied(), 0.9),
        level_idle_db: median_finite(clean.iter().filter(|s| !s.occupied).map(level_of)),
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
    /// Local floor.
    pub floor: LocalFloorConfig,
}

impl EngineConfig {
    /// Share of samples the 80 % method discards (the dynamic `idle_fraction`, else 0.8).
    pub fn discard_fraction(&self) -> f64 {
        match self.threshold.method {
            ThresholdMethod::Dynamic { idle_fraction } => idle_fraction,
            ThresholdMethod::PreSet { .. } | ThresholdMethod::HistoryTile { .. } => 0.8,
        }
    }

    /// [`local_floors`] of `grid` under these settings.
    pub fn local_floors(&self, grid: &RegionHistory) -> ColumnFloors {
        local_floors(
            grid,
            self.discard_fraction(),
            self.threshold.guard_db,
            &self.floor,
        )
    }

    /// [`local_floors`] of `grid`'s columns, measured over `wide`: the same cells and span, widened
    /// to cover those columns' ±`floor.radius_hz` neighbourhoods (T-196).
    ///
    /// A column's reference is a percentile of the column floors within that radius on each side,
    /// so measuring it on a grid clipped to the caller's query band takes the neighbourhood from
    /// the query extent instead of from the spectrum: a band narrower than the radius leaves a
    /// column with no usable side at all, and it falls back to its own floor — which for a busy
    /// channel is its own signal, putting the threshold above the emission and reading it idle.
    /// The floor must be a property of the spectrum around a channel, not of the extent asked for.
    ///
    /// Falls back to `grid` when `wide` does not align with it or does not contain it.
    pub fn local_floors_from(&self, grid: &RegionHistory, wide: &RegionHistory) -> ColumnFloors {
        let offset = grid.f_first_cell - wide.f_first_cell;
        if wide.f_cell_hz != grid.f_cell_hz
            || offset < 0
            || offset as usize + grid.nf > wide.nf
            || wide.nf == 0
        {
            return self.local_floors(grid);
        }
        self.local_floors(wide).window(offset as usize, grid.nf)
    }
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            threshold: ThresholdSpec::default(),
            level: ConfidenceLevel::P95,
            min_visits: MIN_ACTIVITY_INDEPENDENT_VISITS,
            site: SiteKey::Unassigned,
            floor: LocalFloorConfig::default(),
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
        // T-359: from the visits the row was estimated over, not from the caller — a bias tee is
        // switched during a run, so it belongs to the measurement.
        bias_tee: e.bias_tee,
        confidence: e.confidence,
        revisit_biased: false,
        fco_window: Some(e.window),
        floor_db: e.floor_db,
        floor_source: e.floor_source,
        floor_suspect: e.floor_suspect,
        level_occupied_p50_db: e.level_occupied_p50_db,
        level_occupied_p90_db: e.level_occupied_p90_db,
        level_idle_db: e.level_idle_db,
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
            level_db: if occupied { -80.0 } else { -104.0 },
            floor_db: -105.0,
            floor_source: FloorSource::History,
            floor_suspect: false,
            gain_key: 0,
            bias_tee: BiasTee::Unknown,
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
            device_id: None,
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
            device_id: None,
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
        // The bands an interval covers: both visited hops and the dwell's window.
        let b = m.bands(all);
        assert_eq!(b.len(), 3, "{b:?}");
        assert!(b.contains(&FreqRange::centered(100e6, 1.6e6)));
        assert!(b.contains(&FreqRange::centered(102e6, 1.6e6)));
        let late = TimeRange::new(t0.saturating_add_nanos(1_500_000_000), all.end);
        assert_eq!(m.bands(late), vec![FreqRange::centered(100e6, 1.6e6)]);
    }

    const F_FIRST_CELL: i64 = 16_000; // 100 MHz at 6.25 kHz

    /// A level-0 grid of `nt` 1 s rows × `nf` 6.25 kHz columns; `cell(t, f)` gives
    /// `(mean_db, floor_db, occupancy)`.
    fn grid(nt: usize, nf: usize, cell: impl Fn(usize, usize) -> (f64, f64, f32)) -> RegionHistory {
        let mut cells = Vec::with_capacity(nt * nf);
        for t in 0..nt {
            for f in 0..nf {
                let (mean, floor, occ) = cell(t, f);
                cells.push(hk_store::CellStats {
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
                });
            }
        }
        RegionHistory {
            scheme: 1,
            level: 0,
            unit: PowerUnit::Dbfs,
            f_cell_hz: 6250.0,
            f_first_cell: F_FIRST_CELL,
            nf,
            t_cell_ns: 1_000_000_000,
            t_first_cell: 1_000_000,
            nt,
            percentiles: (0.1, 0.9),
            cells,
            provenance: Default::default(),
            tiles_read: 0,
            filter: None,
        }
    }

    fn whole(g: &RegionHistory) -> (FreqRange, TimeRange) {
        (
            FreqRange::new(
                g.f_first_cell as f64 * g.f_cell_hz,
                (g.f_first_cell + g.nf as i64) as f64 * g.f_cell_hz,
            ),
            span(1e6, 1e6 + g.nt as f64),
        )
    }

    #[test]
    fn occupancy_local_floor_follows_ripple_and_roll_off_and_keeps_noise_idle() {
        // 10 MHz band: noise −100 dB with ±2.5 dB passband ripple and a 15 dB roll-off over the
        // outer 400 kHz each side. 16 channels of 50 kHz, 20 dB up: even ones always on, odd ones
        // on one row in four.
        let (nt, nf) = (40usize, 1600usize);
        let noise = |f: usize| {
            let d = f.min(nf - 1 - f) as f64;
            let roll = if d < 64.0 {
                15.0 * (1.0 - d / 64.0)
            } else {
                0.0
            };
            -100.0 + 2.5 * (2.0 * std::f64::consts::PI * f as f64 / 400.0).sin() - roll
        };
        let jitter = |t: usize, f: usize| (((t * 31 + f * 17) % 7) as f64 - 3.0) * 0.1;
        let chan = |f: usize| (50..58).contains(&(f % 100)).then_some(f / 100);
        let busy = |t: usize, f: usize| chan(f).is_some_and(|k| k % 2 == 0 || t % 4 == 0);
        let g = grid(nt, nf, |t, f| {
            if busy(t, f) {
                (noise(f) + 20.0, noise(f) + 20.0, 1.0)
            } else {
                (noise(f) + 1.0 + jitter(t, f), noise(f) + jitter(t, f), 0.0)
            }
        });
        let cfg = EngineConfig::default();
        let fl = cfg.local_floors(&g);
        let mut worst: f64 = 0.0;
        for f in (0..nf).filter(|&f| chan(f).is_none()) {
            worst = worst.max((fl.floor_db[f] - noise(f)).abs());
        }
        assert!(worst < 1.0, "noise-column floor error {worst:.2} dB");
        assert!(fl.suspect.iter().all(|s| !s), "sparse band flagged suspect");
        assert!(fl.source.iter().all(|s| *s == FloorSource::History));

        // Every row: exactly the busy cells are above threshold, at the edges and the centre.
        let (band, _) = whole(&g);
        let visits = coverage_visits(&g, band, Tier::ScheduledPlan);
        let input = EvalInput {
            grid: &g,
            visits: &visits,
            detections: &[],
            floors: &fl,
        };
        let (s, thr) = evaluate(&cfg.threshold, band, 6250.0, input);
        assert_eq!(s.len(), nt);
        for (t, v) in s.iter().enumerate() {
            let want = (0..nf).filter(|&f| busy(t, f)).count() as f32 / nf as f32;
            assert!(
                (v.above_fraction - want).abs() < 1e-6,
                "row {t}: {v:?} vs {want}"
            );
            // The strongest busy cell: 20 dB over the ripple, at most at its +2.5 dB crest.
            assert!((-80.5..=-77.5).contains(&v.level_db), "{v:?}");
        }
        assert_eq!(thr.unwrap().source, FloorSource::History);
        for (lo, hi) in [(5, 13), (790, 798), (1590, 1598)] {
            let f = FreqRange::new(
                (F_FIRST_CELL + lo) as f64 * 6250.0,
                (F_FIRST_CELL + hi) as f64 * 6250.0,
            );
            let (s, _) = evaluate(&cfg.threshold, f, 6250.0, input);
            assert!(
                s.iter().all(|v| !v.occupied),
                "noise {lo}..{hi} read occupied"
            );
        }
        // The whole-band 80 % floor this replaces reads ripple peaks as occupied.
        let all_floors: Vec<f64> = g.cells.iter().map(|c| f64::from(c.floor_db)).collect();
        let old = threshold::eighty_percent_floor_db(&all_floors, 0.8).unwrap() + 5.0;
        let phantom = (0..nt * nf)
            .filter(|&i| !busy(i / nf, i % nf) && f64::from(g.cells[i].mean_db) > old)
            .count();
        eprintln!("whole-band threshold {old:.1} dB: {phantom} phantom noise cells; local: 0");
        assert!(phantom > 0);
    }

    /// T-129 review (§2.6): one clean crossing makes a visit occupied and not suspect; a
    /// spur-only visit is suspect, including the spur's leakage into the adjacent cell.
    #[test]
    fn occupancy_visit_is_suspect_only_when_every_crossing_is_a_suspect_detection() {
        let (nt, nf) = (10usize, 200usize);
        let cfg = EngineConfig::default();
        // A station in cells 40..48 on rows 0..5; a spur in cell 150 leaking into 151 on every row.
        let g = grid(nt, nf, |t, f| match f {
            40..48 if t < 5 => (-80.0, -80.0, 1.0),
            150 => (-80.0, -80.0, 1.0),
            151 => (-88.0, -100.0, 0.0),
            _ => (-99.0, -100.0, 0.0),
        });
        let (band, iv) = whole(&g);
        let fl = cfg.local_floors(&g);
        let visits = coverage_visits(&g, band, Tier::ScheduledPlan);
        // The spur's detection: a 200 Hz line at the centre of cell 150.
        let spur_hz = (F_FIRST_CELL + 150) as f64 * 6250.0 + 3125.0;
        let spur = DetectionExtent {
            time: iv,
            freq: FreqRange::centered(spur_hz, 200.0),
            obw_hz: 200.0,
            snr_db: 20.0,
            suspect: true,
        };
        let run = |dets: &[DetectionExtent]| {
            let input = EvalInput {
                grid: &g,
                visits: &visits,
                detections: dets,
                floors: &fl,
            };
            evaluate(&cfg.threshold, band, 6250.0, input).0
        };
        let s = run(&[spur]);
        assert_eq!(s.len(), nt);
        for (t, v) in s.iter().enumerate() {
            assert!(v.occupied, "row {t}: {v:?}");
            assert_eq!(v.suspect, t >= 5, "row {t}: {v:?}");
        }
        // A spur detection that ended before the visit window (± one time cell) does not apply.
        let old = DetectionExtent {
            time: span(1e6 - 10.0, 1e6 - 5.0),
            ..spur
        };
        assert!(run(&[old]).iter().all(|v| v.occupied && !v.suspect));
    }

    /// T-147 (§2.6): a DC flag is per tuning. Hop A's LO carries a true DC spur that moves with
    /// it (cell 150, retuned to cell 100 at row 5); hop B's LO carries one at cell 20. A real
    /// carrier 2 cells from hop A's first LO is DC-flagged by hop A and clean from hop B 55 ms
    /// later. The carrier's DC flags are refuted, so its visits count; the spurs stay suspect,
    /// never reach FCO and never become a channel.
    #[test]
    fn occupancy_dc_flag_is_per_tuning_and_a_true_dc_spur_stays_suspect() {
        use super::super::channels::{
            ChannelPlan, DC_TWIN_LO_TOLERANCE_HZ, DcTwinRule, LearnConfig, dc_only_suspect,
            refute_dc_suspects,
        };
        use hk_model::DetectionFlags;
        use hk_model::detection::SpurReason;

        let dc_flags = DetectionFlags {
            spur_candidate: true,
            spur_reason: Some(SpurReason::Dc),
            ..Default::default()
        };
        assert!(dc_only_suspect(&dc_flags));
        assert!(!dc_only_suspect(&DetectionFlags {
            clipped: true,
            ..dc_flags
        }));
        assert!(!dc_only_suspect(&DetectionFlags {
            spur_reason: Some(SpurReason::RefHarmonic),
            ..dc_flags
        }));

        let (nt, nf) = (10usize, 200usize);
        let cfg = EngineConfig::default();
        let lo_a = |t: usize| if t < 5 { 150 } else { 100 };
        let g = grid(nt, nf, |t, f| {
            if f == lo_a(t) || f == 20 || f == 152 {
                (-80.0, -80.0, 1.0)
            } else {
                (-99.0, -100.0, 0.0)
            }
        });
        let (_, iv) = whole(&g);
        let fl = cfg.local_floors(&g);
        let cell_centre = |f: usize| (F_FIRST_CELL + f as i64) as f64 * 6250.0 + 3125.0;
        let cell_band = |f: usize| FreqRange::centered(cell_centre(f), 6250.0);
        let at = |f: usize, obw: f64, from: f64, to: f64, suspect: bool| DetectionExtent {
            time: span(from, to),
            freq: FreqRange::centered(cell_centre(f), obw),
            obw_hz: obw,
            snr_db: 20.0,
            suspect,
        };
        let (mut dets, mut dc) = (Vec::new(), Vec::new());
        for t in 0..nt {
            let row = 1e6 + t as f64;
            // Hop A (10–50 ms): its own DC spur, and the carrier when it sits within DC reach.
            dets.push(at(lo_a(t), 200.0, row + 0.01, row + 0.05, true));
            dc.push(true);
            let near = t < 5;
            dets.push(at(152, 3000.0, row + 0.01, row + 0.05, near));
            dc.push(near);
            // Hop B (65–100 ms): its own DC spur at cell 20, the carrier clean.
            dets.push(at(20, 200.0, row + 0.065, row + 0.1, true));
            dc.push(true);
            dets.push(at(152, 3000.0, row + 0.065, row + 0.1, false));
            dc.push(false);
        }
        let raw = dets.clone();
        // Each detection's own LO: hop A's, hop A's, hop B's, hop B's per row.
        let lo: Vec<f64> = (0..nt)
            .flat_map(|t| {
                let (a, b) = (cell_centre(lo_a(t)), cell_centre(20));
                [a, a, b, b]
            })
            .collect();
        let rule = DcTwinRule {
            f_cell_hz: 6250.0,
            slack_ns: g.t_cell_ns,
            lo_tolerance_hz: DC_TWIN_LO_TOLERANCE_HZ,
        };
        refute_dc_suspects(&mut dets, &dc, rule, |j| Some(lo[j]));
        for (i, (d, r)) in dets.iter().zip(&raw).enumerate() {
            let carrier = (d.freq.lo_hz + d.freq.hi_hz) / 2.0 == cell_centre(152);
            assert_eq!(d.suspect, !carrier && r.suspect, "detection {i}: {d:?}");
        }

        let visits = coverage_visits(&g, whole(&g).0, Tier::ScheduledPlan);
        let run = |band: FreqRange, dets: &[DetectionExtent]| {
            let input = EvalInput {
                grid: &g,
                visits: &visits,
                detections: dets,
                floors: &fl,
            };
            evaluate(&cfg.threshold, band, 3000.0, input).0
        };
        // Before the fix the carrier's hop-A rows were suspect; now every visit counts.
        let before = run(cell_band(152), &raw);
        assert!((0..5).all(|t| before[t].suspect), "{before:?}");
        let carrier = run(cell_band(152), &dets);
        assert_eq!(carrier.len(), nt);
        assert!(
            carrier.iter().all(|v| v.occupied && !v.suspect),
            "{carrier:?}"
        );
        let e = estimate(&carrier, iv, ConfidenceLevel::P95);
        assert_eq!((e.fco, e.n_suspect), (Some(1.0), 0));
        // The spurs: every occupied visit suspect, none occupied in FCO.
        for (cell, rows) in [(150, 0..5), (100, 5..10), (20, 0..10)] {
            let s = run(cell_band(cell), &dets);
            for (t, v) in s.iter().enumerate() {
                assert_eq!(v.occupied, rows.contains(&t), "cell {cell} row {t}: {v:?}");
                assert_eq!(v.suspect, v.occupied, "cell {cell} row {t}: {v:?}");
            }
            let e = estimate(&s, iv, ConfidenceLevel::P95);
            assert_eq!(e.n_occupied, 0, "cell {cell}: {e:?}");
            assert_eq!(e.n_suspect, rows.len() as u64, "cell {cell}: {e:?}");
        }
        // Learning: the carrier is a channel, no spur is.
        let mut plan = ChannelPlan::new(1, 6250.0, LearnConfig::default());
        plan.learn(&dets);
        let ch = plan.channels();
        assert_eq!(ch.len(), 1, "{ch:?}");
        let k = ch[0].key.freq(6250.0);
        assert!(
            k.lo_hz <= cell_centre(152) && cell_centre(152) <= k.hi_hz,
            "{ch:?}"
        );
        // Hop A's 10 (5 refuted DC flags) and hop B's 10.
        assert_eq!(ch[0].evidence, 20);
    }

    #[test]
    fn occupancy_dense_band_flags_its_local_floor_suspect() {
        let (nt, nf) = (20usize, 800usize);
        let cfg = EngineConfig::default();
        let row = |g: &RegionHistory| {
            let (band, iv) = whole(g);
            let fl = cfg.local_floors(g);
            let visits = coverage_visits(g, band, Tier::ScheduledPlan);
            let (s, _) = evaluate(
                &cfg.threshold,
                band,
                6250.0,
                EvalInput {
                    grid: g,
                    visits: &visits,
                    detections: &[],
                    floors: &fl,
                },
            );
            let ctx = SubjectContext {
                subject: OccupancySubject::Band { freq: band },
                rbw_hz: 6250.0,
                obw_hz: None,
                unit: PowerUnit::Dbfs,
                calibration: None,
            };
            (fl, stat(&cfg, &ctx, iv, &s, iv, None).unwrap())
        };
        // FM-like: 90 % of the columns carry an always-on station; every tenth is a gap.
        let dense = grid(nt, nf, |_, f| {
            if f % 10 == 0 {
                (-99.0, -100.0, 0.0)
            } else {
                (-80.0, -80.0, 1.0)
            }
        });
        let (fl, r) = row(&dense);
        assert!(fl.suspect[nf / 2] && fl.suspect[10]);
        assert_eq!(r.floor_suspect, Some(true));
        // The same stations sparse: floor is the noise, not suspect, stations occupied.
        let sparse = grid(nt, nf, |_, f| {
            if f % 10 == 5 {
                (-80.0, -80.0, 1.0)
            } else {
                (-99.0, -100.0, 0.0)
            }
        });
        let (fl, r) = row(&sparse);
        assert!(fl.suspect.iter().all(|s| !s));
        assert_eq!(r.floor_suspect, Some(false));
        assert_eq!(r.floor_source, Some(FloorSource::History));
        assert!(
            (r.floor_db.unwrap() + 100.0).abs() < 1e-3,
            "{:?}",
            r.floor_db
        );
        assert_eq!(r.fco, Some(1.0));
        assert_eq!(r.level_occupied_p50_db, Some(-80.0));
        assert_eq!(r.level_idle_db, None);
    }

    #[test]
    fn occupancy_level_fields_summarise_the_fco_visits() {
        let mut s: Vec<VisitSample> = (0..40)
            .map(|k| {
                let occupied = k % 4 == 0;
                VisitSample {
                    level_db: if occupied {
                        -70.0 - k as f32 * 0.25
                    } else {
                        -104.0
                    },
                    ..sample(f64::from(k) * 10.0, Tier::BackgroundSweep, occupied, false)
                }
            })
            .collect();
        // A bandit dwell does not enter the fco level summary.
        s.push(VisitSample {
            level_db: -20.0,
            ..sample(5.0, Tier::Bandit, true, false)
        });
        let e = estimate(&s, span(0.0, 400.0), ConfidenceLevel::P95);
        // Occupied levels −79 … −70 dB.
        assert!((e.level_occupied_p50_db.unwrap() + 74.5).abs() < 1e-9);
        assert!((e.level_occupied_p90_db.unwrap() + 70.9).abs() < 1e-9);
        assert_eq!(e.level_idle_db, Some(-104.0));
        assert_eq!(e.floor_db, Some(-105.0));
        assert_eq!(
            (e.floor_source, e.floor_suspect),
            (Some(FloorSource::History), Some(false))
        );
    }

    #[test]
    fn occupancy_all_visits_strata_are_weighted_by_observed_duration() {
        // Minute 0: a 1 s idle sweep visit. Minute 1: a 30 s occupied dwell. Equal strata weights
        // would give 0.5; duration weights give 30/31.
        let mut a = sample(10.0, Tier::BackgroundSweep, false, false);
        a.dur_ns = 1_000_000_000;
        let mut b = sample(70.0, Tier::Bandit, true, false);
        b.dur_ns = 30_000_000_000;
        let e = estimate(&[a, b], span(0.0, 120.0), ConfidenceLevel::P95);
        assert!((e.fco_all_visits.unwrap() - 30.0 / 31.0).abs() < 1e-9);
    }

    /// T-359: a row's bias-tee state is what **every** visit of its window agreed on. A window's
    /// visits can come from several history reads (the chunks tiling an interval), so a switch
    /// between two chunks reaches this as two pure sets, not as one mixed grid — and the row must
    /// then claim neither state, not the longer-held one, because it was measured under two
    /// receive chains (T-333: pooling them masks a real rise rather than raising a false alarm).
    ///
    /// The **control** is a window all of whose visits agree: it keeps its state, including
    /// `Unknown`, so the rule cannot pass by always answering unknown.
    #[test]
    fn occupancy_row_bias_tee_needs_every_visit_to_agree() {
        let with = |t: f64, bias: BiasTee, dur_ns: i64| VisitSample {
            bias_tee: bias,
            dur_ns,
            ..sample(t, Tier::BackgroundSweep, false, false)
        };
        // Control: one state throughout, at each of the three values.
        for held in [BiasTee::Off, BiasTee::On, BiasTee::Unknown] {
            let s: Vec<VisitSample> = (0..10).map(|k| with(f64::from(k), held, 1)).collect();
            assert_eq!(interval_bias_tee(&s), held, "{held:?}");
            assert_eq!(
                estimate(&s, span(0.0, 20.0), ConfidenceLevel::P95).bias_tee,
                held
            );
        }
        // A switch between chunks: 9 s of `Off` and 1 s of `On` is not an `Off` row.
        let mut s: Vec<VisitSample> = (0..9)
            .map(|k| with(f64::from(k), BiasTee::Off, 1_000_000_000))
            .collect();
        s.push(with(9.0, BiasTee::On, 1_000_000_000));
        assert_eq!(interval_bias_tee(&s), BiasTee::Unknown);
        assert_eq!(
            estimate(&s, span(0.0, 20.0), ConfidenceLevel::P95).bias_tee,
            BiasTee::Unknown,
            "a majority is not agreement"
        );
        // Learning the state, or losing it, is the same kind of disagreement (T-332).
        let mixed = [with(0.0, BiasTee::Unknown, 1), with(1.0, BiasTee::On, 1)];
        assert_eq!(interval_bias_tee(&mixed), BiasTee::Unknown);
        // Nothing observed states nothing — and never `Off`.
        assert_eq!(interval_bias_tee(&[]), BiasTee::Unknown);
    }

    /// T-359: a grid whose tiles pooled more than one state attributes none to its visits. The
    /// **control** is an unmixed grid, whose state (including `Unknown`) is carried as it is.
    #[test]
    fn occupancy_visit_bias_tee_is_unknown_when_the_grid_pooled_two_states() {
        for held in [BiasTee::Off, BiasTee::On, BiasTee::Unknown] {
            let p = hk_store::ProvenanceSummary {
                bias_tee: held,
                bias_tee_mixed: false,
                ..Default::default()
            };
            assert_eq!(grid_bias_tee(&p), held, "{held:?}");
            let mixed = hk_store::ProvenanceSummary {
                bias_tee_mixed: true,
                ..p
            };
            assert_eq!(
                grid_bias_tee(&mixed),
                BiasTee::Unknown,
                "{held:?} pooled with another state"
            );
        }
    }
}
