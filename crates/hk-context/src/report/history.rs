//! History-tile providers (T-116/T-126 tiles): coverage fallback, interim occupancy and
//! provenance steps.
//!
//! **Interim occupancy (until T-118).** Channel rows use blind inventory extents; per grid time
//! row a channel is "revisited" when its cells were observed and "occupied" when any of its cells
//! has occupancy above zero (tile occupancy = fraction of observed time above floor + the
//! pyramid's margin). Rows include activity-driven dwells, so the ratio is `fco_all_visits` with
//! `fco: None` and `revisit_biased` (ADR-0012 §2.5). `fbo` is the coverage-weighted tile
//! occupancy. T-118 replaces this provider with its `OccupancyStat` series (true `fco`) behind
//! [`OccupancyProvider`]; the report does not change.

use hk_model::attention::ATTENTION_SCHEMA_VERSION;
use hk_model::attention::baseline::SiteKey;
use hk_model::attention::occupancy::{
    ChannelKey, MIN_GUARD_DB, OccupancyStat, OccupancySubject, ThresholdMethod, ThresholdSpec,
    TimingRegime,
};
use hk_model::attention::report::{ProvenanceStep as ReportStep, ProvenanceStepKind};
use hk_model::{FreqRange, TimeRange, Timestamp};
use hk_store::history::Geometry;
use hk_store::history::{FrontEndState, GainState, ProvenanceStep, ProvenanceSummary};
use hk_store::{Pyramid, RegionHistory, RegionQuery, Resolution};

use super::{
    CoverageProvider, OccupancyProvider, OccupancyRows, ProvenanceProvider, ReportError,
    ReportRequest,
};

/// Time rows a report grid may have at a level finer than the top.
pub const REPORT_MAX_TIME_CELLS: usize = 4096;
/// Frequency columns a report grid may have at a level finer than the top.
pub const REPORT_MAX_FREQ_CELLS: usize = 1024;
/// Cells a report grid may have at any level (the `/api/history` budget, `MAX_API_CELLS`): a box
/// that exceeds it even at the top level is refused as invalid.
pub const REPORT_MAX_CELLS: usize = 500_000;
/// Grid rows read per history-lock scope (rounded up to whole tiles of the chosen level), so the
/// ingest writer is never locked out for a whole report build.
pub const REPORT_LOCK_CHUNK_ROWS: usize = 256;

/// `(nt, nf)` of level `l`'s grid over the box (the pyramid's own rule), as `f64` so huge boxes
/// cannot overflow.
pub fn report_grid_dims(
    geom: &Geometry,
    l: usize,
    region: FreqRange,
    span: TimeRange,
) -> (f64, f64) {
    let g = &geom.levels[l];
    let nf = ((region.hi_hz / g.f_cell_hz).ceil() - (region.lo_hz / g.f_cell_hz).floor()).max(1.0);
    let t0 = span.start.as_unix_nanos().div_euclid(g.t_cell_ns);
    let t1 = span
        .end
        .as_unix_nanos()
        .saturating_add(g.t_cell_ns - 1)
        .div_euclid(g.t_cell_ns);
    ((t1 - t0).max(1) as f64, nf)
}

/// The report grid's level and its lock-scope time chunks: the finest level within
/// [`REPORT_MAX_TIME_CELLS`] × [`REPORT_MAX_FREQ_CELLS`] and [`REPORT_MAX_CELLS`], else the top
/// level if it fits [`REPORT_MAX_CELLS`], else [`ReportError::Invalid`]. Chunks tile the span on
/// the level's tile boundaries; querying each at that level and stitching them
/// ([`HistoryTiles::from_parts`]) gives the whole grid.
pub fn report_chunks(
    geom: &Geometry,
    region: FreqRange,
    span: TimeRange,
) -> Result<(usize, Vec<TimeRange>), ReportError> {
    super::check_box(region, span)?;
    let cap = REPORT_MAX_CELLS as f64;
    let fits = |l: usize| {
        let (nt, nf) = report_grid_dims(geom, l, region, span);
        nt <= REPORT_MAX_TIME_CELLS as f64 && nf <= REPORT_MAX_FREQ_CELLS as f64 && nt * nf <= cap
    };
    let top = geom.top();
    let level = match (0..=top).find(|&l| fits(l)) {
        Some(l) => l,
        None => {
            let (nt, nf) = report_grid_dims(geom, top, region, span);
            if nt * nf > cap {
                return Err(ReportError::Invalid(
                    "region × span too large for the report cell budget even at the coarsest \
                     history level",
                ));
            }
            top
        }
    };
    let g = &geom.levels[level];
    let rows = REPORT_LOCK_CHUNK_ROWS.div_ceil(g.nt).max(1) as i64 * g.nt as i64;
    let t_cell = g.t_cell_ns;
    let mut chunks = Vec::new();
    // Chunk edges on whole tiles of this level (a tile is never merged into two chunks).
    let mut edge = span
        .start
        .as_unix_nanos()
        .div_euclid(t_cell)
        .div_euclid(g.nt as i64)
        * g.nt as i64;
    let mut start = span.start;
    while start < span.end {
        edge += rows;
        let end = Timestamp::from_unix_nanos(edge.saturating_mul(t_cell)).min(span.end);
        if end > start {
            chunks.push(TimeRange::new(start, end));
            start = end;
        }
    }
    Ok((level, chunks))
}

/// A region × span grid from the history pyramid, with what the report needs of its geometry.
#[derive(Clone, Debug)]
pub struct HistoryTiles {
    grid: RegionHistory,
    level0_f_cell_hz: f64,
    margin_db: f64,
}

impl HistoryTiles {
    /// Queries `p` over the box at `level` in one call (one lock scope for the caller). Callers
    /// holding an ingest lock use [`report_chunks`] and [`Self::from_parts`] instead.
    pub fn query_level(
        p: &Pyramid,
        region: FreqRange,
        span: TimeRange,
        level: usize,
    ) -> Result<RegionHistory, ReportError> {
        p.query(&RegionQuery {
            freq: region,
            time: span,
            resolution: Resolution::Level(u8::try_from(level).unwrap_or(u8::MAX)),
        })
        .map_err(|e| ReportError::Provider(format!("history query failed: {e}")))
    }

    /// Stitches time-consecutive grids of one level and frequency extent (the chunks of
    /// [`report_chunks`], in order) into one.
    pub fn from_parts(
        parts: Vec<RegionHistory>,
        level0_f_cell_hz: f64,
        margin_db: f64,
    ) -> Result<Self, ReportError> {
        let mut it = parts.into_iter();
        let Some(mut grid) = it.next() else {
            return Err(ReportError::Invalid("empty history grid"));
        };
        for p in it {
            if p.level != grid.level
                || p.nf != grid.nf
                || p.f_first_cell != grid.f_first_cell
                || p.t_first_cell != grid.t_first_cell + grid.nt as i64
            {
                return Err(ReportError::Provider(
                    "history chunks do not stitch (store changed geometry mid-report)".into(),
                ));
            }
            grid.nt += p.nt;
            grid.cells.extend_from_slice(&p.cells);
            grid.provenance.merge(&p.provenance);
            grid.tiles_read += p.tiles_read;
        }
        // A coarser fallback tile read by two chunks lists its steps twice.
        grid.provenance.steps.sort_by_key(|s| s.t);
        grid.provenance.steps.dedup();
        Ok(Self::from_grid(grid, level0_f_cell_hz, margin_db))
    }

    /// Wraps an already queried grid.
    pub fn from_grid(grid: RegionHistory, level0_f_cell_hz: f64, margin_db: f64) -> Self {
        Self {
            grid,
            level0_f_cell_hz,
            margin_db,
        }
    }

    /// The grid.
    pub fn grid(&self) -> &RegionHistory {
        &self.grid
    }

    /// Level-0 frequency cell width (channel keys), Hz.
    pub fn level0_f_cell_hz(&self) -> f64 {
        self.level0_f_cell_hz
    }

    fn columns(&self, f: FreqRange) -> Vec<usize> {
        let cols = self.grid.columns_for(f);
        if !cols.is_empty() {
            return cols;
        }
        (0..self.grid.nf)
            .filter(|&c| self.grid.freq_of(c).overlaps(&f))
            .collect()
    }

    fn row_stats(&self, cols: &[usize], span: TimeRange) -> RowStats {
        let g = &self.grid;
        let t_cell_s = g.t_cell_ns as f64 * 1e-9;
        let mut s = RowStats::default();
        let (mut occ_w, mut cov_w) = (0.0f64, 0.0f64);
        for t in 0..g.nt {
            let start = g.time_of(t);
            let end = start.saturating_add_nanos(g.t_cell_ns);
            if end <= span.start || start >= span.end {
                continue;
            }
            let (mut occ, mut cov, mut seen) = (0.0f32, 0.0f32, false);
            for &f in cols {
                let c = g.cell(t, f);
                if !c.observed() {
                    continue;
                }
                seen = true;
                if c.occupancy.is_finite() {
                    occ = occ.max(c.occupancy);
                    occ_w += f64::from(c.occupancy) * f64::from(c.coverage);
                }
                cov = cov.max(c.coverage);
                cov_w += f64::from(c.coverage);
                if c.floor_db.is_finite() {
                    s.floors.push(c.floor_db);
                } else if c.p_low_db.is_finite() {
                    s.floors.push(c.p_low_db);
                }
            }
            if seen {
                s.n_revisits += 1;
                if occ > 0.0 {
                    s.n_occupied += 1;
                }
                s.observed_s += f64::from(cov) * t_cell_s;
                s.starts_ns.push(start.as_unix_nanos());
            }
        }
        s.fbo = (cov_w > 0.0).then(|| (occ_w / cov_w).clamp(0.0, 1.0));
        s
    }

    fn stat(
        &self,
        req: &ReportRequest,
        subject: OccupancySubject,
        s: RowStats,
        obw_hz: Option<f64>,
    ) -> OccupancyStat {
        let mut floors = s.floors;
        floors.sort_by(f32::total_cmp);
        let threshold_db = floors
            .get(floors.len() / 2)
            .map_or(-300.0, |f| f64::from(*f) + self.margin_db)
            .clamp(-300.0, 100.0);
        let gaps: Vec<f64> = s
            .starts_ns
            .windows(2)
            .map(|w| (w[1] - w[0]) as f64 / 1e9)
            .collect();
        // Tile rows mix activity-driven dwells with sweeps: this is an all-visits figure, never
        // the activity-independent `fco` (ADR-0012 §2.5: never substitute).
        let fco_all_visits = (s.n_revisits > 0).then(|| s.n_occupied as f64 / s.n_revisits as f64);
        OccupancyStat {
            schema: ATTENTION_SCHEMA_VERSION,
            site: req.site,
            subject,
            interval: req.span,
            fco: None,
            fco_all_visits,
            fco_suspect_upper: None,
            fbo: s.fbo,
            sro: None,
            // No activity-independent revisits are known from tiles.
            n_revisits: 0,
            n_occupied: 0,
            n_suspect: 0,
            n_revisits_all: s.n_revisits,
            observed_s: s.observed_s,
            revisit_max_s: gaps.iter().copied().reduce(f64::max),
            revisit_mean_s: (!gaps.is_empty())
                .then(|| gaps.iter().sum::<f64>() / gaps.len() as f64),
            timing: TimingRegime::Unknown,
            threshold: ThresholdSpec {
                method: ThresholdMethod::HistoryTile {
                    margin_db: self.margin_db,
                },
                guard_db: self.margin_db.clamp(MIN_GUARD_DB, 20.0),
                rbw_correction: false,
            },
            threshold_db,
            guard_clamped: false,
            rbw_hz: self.grid.f_cell_hz.max(f64::MIN_POSITIVE),
            obw_hz,
            unit: self.grid.unit,
            calibration: self.grid.provenance.calibration,
            confidence: None,
            revisit_biased: true,
        }
    }
}

#[derive(Default)]
struct RowStats {
    n_revisits: u64,
    n_occupied: u64,
    observed_s: f64,
    fbo: Option<f64>,
    starts_ns: Vec<i64>,
    floors: Vec<f32>,
}

impl CoverageProvider for HistoryTiles {
    fn name(&self) -> &'static str {
        "history tiles (per grid cell; no observation log for this box)"
    }

    /// A cell counts as observed in a grid row for the least-covered of its columns' share of the
    /// row (a column never observed makes the row unobserved for the cell), from the row start.
    fn observed(
        &self,
        cells: &[FreqRange],
        _span: TimeRange,
    ) -> Result<Option<Vec<Vec<TimeRange>>>, ReportError> {
        let g = &self.grid;
        let out = cells
            .iter()
            .map(|&cell| {
                let cols = self.columns(cell);
                (0..g.nt)
                    .filter_map(|t| {
                        let cov = cols
                            .iter()
                            .map(|&f| {
                                let c = g.cell(t, f);
                                if c.observed() { c.coverage } else { 0.0 }
                            })
                            .fold(f32::INFINITY, f32::min);
                        (!cols.is_empty() && cov > 0.0).then(|| {
                            let start = g.time_of(t);
                            let len = (f64::from(cov.min(1.0)) * g.t_cell_ns as f64) as i64;
                            TimeRange::new(start, start.saturating_add_nanos(len.max(1)))
                        })
                    })
                    .collect()
            })
            .collect();
        Ok(Some(out))
    }
}

impl OccupancyProvider for HistoryTiles {
    fn occupancy(
        &self,
        req: &ReportRequest,
        channels: &[FreqRange],
    ) -> Result<OccupancyRows, ReportError> {
        let all: Vec<usize> = self.columns(req.region);
        let band = self.stat(
            req,
            OccupancySubject::Band { freq: req.region },
            self.row_stats(&all, req.span),
            None,
        );
        let mut rows = OccupancyRows {
            bands: vec![band],
            channels: Vec::with_capacity(channels.len()),
            warnings: vec![format!(
                "occupancy from history-tile occupancy (floor + {:.1} dB) over blind inventory \
                 extents at {:.0} s × {:.0} Hz cells: fco_all_visits only (tile rows include \
                 activity-driven dwells, so no unbiased fco); interim until the occupancy engine \
                 (T-118)",
                self.margin_db,
                self.grid.t_cell_ns as f64 / 1e9,
                self.grid.f_cell_hz
            )],
        };
        if matches!(req.site, SiteKey::Site(_)) {
            rows.warnings.push(
                "history tiles are not keyed by site before T-119: rows include every site".into(),
            );
        }
        for &ch in channels {
            let Some(key) = ChannelKey::snap(self.grid.scheme, self.level0_f_cell_hz, ch) else {
                continue;
            };
            let cols = self.columns(ch);
            let s = self.row_stats(&cols, req.span);
            rows.channels.push((
                ch,
                self.stat(
                    req,
                    OccupancySubject::Channel { key },
                    s,
                    Some(ch.width_hz()),
                ),
            ));
        }
        Ok(rows)
    }
}

impl ProvenanceProvider for HistoryTiles {
    fn steps(&self, req: &ReportRequest) -> Result<(Vec<ReportStep>, Vec<String>), ReportError> {
        Ok(steps_from_summary(&self.grid.provenance, req.span))
    }
}

fn gain_text(g: Option<GainState>) -> String {
    g.map_or_else(
        || "unknown".into(),
        |g| {
            format!(
                "lna {} dB, vga {} dB, amp {}",
                g.lna_db,
                g.vga_db,
                if g.amp_on { "on" } else { "off" }
            )
        },
    )
}

fn opt<T: ToString>(v: Option<T>) -> String {
    v.map_or_else(|| "none".into(), |v| v.to_string())
}

fn step(t: Timestamp, kind: ProvenanceStepKind, detail: String) -> ReportStep {
    ReportStep {
        t,
        kind,
        freq: None,
        detail,
    }
}

fn front_end_steps(s: &ProvenanceStep, out: &mut Vec<ReportStep>) {
    let (a, b): (&FrontEndState, &FrontEndState) = (&s.from, &s.to);
    if s.changed & ProvenanceStep::GAIN != 0 {
        let detail = match (a.gain, b.gain) {
            (Some(x), Some(y)) => {
                let mut parts = Vec::new();
                if x.lna_db != y.lna_db {
                    parts.push(format!("lna {}→{} dB", x.lna_db, y.lna_db));
                }
                if x.vga_db != y.vga_db {
                    parts.push(format!("vga {}→{} dB", x.vga_db, y.vga_db));
                }
                if x.amp_on != y.amp_on {
                    parts.push(format!("amp {}→{}", x.amp_on, y.amp_on));
                }
                parts.join(", ")
            }
            (x, y) => format!("{} → {}", gain_text(x), gain_text(y)),
        };
        out.push(step(s.t, ProvenanceStepKind::Gain, detail));
    }
    if s.changed & ProvenanceStep::GAIN_TABLE != 0 {
        out.push(step(
            s.t,
            ProvenanceStepKind::Gain,
            format!(
                "gain table {}→{}",
                opt(a.front_end.gain_table),
                opt(b.front_end.gain_table)
            ),
        ));
    }
    if s.changed & ProvenanceStep::CALIBRATION != 0 {
        out.push(step(
            s.t,
            ProvenanceStepKind::Calibration,
            format!("calibration {}→{}", opt(a.calibration), opt(b.calibration)),
        ));
    }
    if s.changed & ProvenanceStep::FILTER != 0 {
        out.push(step(
            s.t,
            ProvenanceStepKind::AntennaPort,
            format!(
                "filter/port {}→{}",
                opt(a.front_end.filter),
                opt(b.front_end.filter)
            ),
        ));
    }
    if s.changed & ProvenanceStep::SPUR_MASK != 0 {
        out.push(step(
            s.t,
            ProvenanceStepKind::SpurMask,
            format!(
                "spur mask {}→{}",
                opt(a.front_end.spur_mask),
                opt(b.front_end.spur_mask)
            ),
        ));
    }
}

/// Report provenance steps (§6.3) and warnings from a history [`ProvenanceSummary`]: every
/// front-end step inside `span` (gain, gain table, calibration, filter/port, spur mask), a
/// sample-drop step when samples were lost, and warnings for overload share, mixed calibration and
/// dropped step records.
pub fn steps_from_summary(
    p: &ProvenanceSummary,
    span: TimeRange,
) -> (Vec<ReportStep>, Vec<String>) {
    let mut steps = Vec::new();
    for s in p
        .steps
        .iter()
        .filter(|s| s.t >= span.start && s.t <= span.end)
    {
        front_end_steps(s, &mut steps);
    }
    if p.dropped_samples > 0 {
        steps.push(step(
            p.first_frame.unwrap_or(span.start).max(span.start),
            ProvenanceStepKind::SampleDrop,
            format!(
                "{} samples dropped before frames in the span",
                p.dropped_samples
            ),
        ));
    }
    let mut warnings = Vec::new();
    if p.suspect_frames > 0 {
        warnings.push(format!(
            "{:.1} % of history frames flagged overloaded/suspect",
            p.suspect_fraction() * 100.0
        ));
    }
    if p.calibration_mixed {
        warnings.push("calibration mixed within the span".into());
    }
    if p.steps_dropped > 0 {
        warnings.push(format!(
            "{} further provenance steps not listed",
            p.steps_dropped
        ));
    }
    (steps, warnings)
}
