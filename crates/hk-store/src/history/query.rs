//! The central query (docs/07 §4 step 1): region × time → per-cell statistics and per-channel
//! summaries.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};

use hk_model::{FreqRange, PowerUnit, TimeRange, Timestamp};

use super::StoreError;
use super::stats::{db, round_centi, round_frac};
use super::store::Pyramid;
use super::tile::{ColumnPreview, OriginFilter, OriginMatch, ProvenanceSummary, Tile};

/// What a source/site-filtered query kept and dropped (T-133; see [`Pyramid::query_filtered`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct FilterSummary {
    /// The filter.
    pub filter: OriginFilter,
    /// Tiles read whose frames all pass.
    pub tiles_matched: usize,
    /// Tiles read holding passing and other frames.
    pub tiles_mixed: usize,
    /// Tiles read with no passing frame.
    pub tiles_other: usize,
    /// Observed cells returned as unobserved because other origins' frames are folded into them.
    pub cells_excluded: usize,
    /// Cells of mixed tiles kept because the one finer tile they roll up passes whole.
    pub cells_from_children: usize,
}

impl FilterSummary {
    /// Adds another chunk's counts (the filter is kept). Cell counts stay exact (chunks cover
    /// disjoint time rows); tile counts are **per chunk read** and are summed, so a coarse tile
    /// read by two chunks counts twice (T-136: stated, not deduplicated).
    pub fn merge(&mut self, o: &FilterSummary) {
        self.tiles_matched += o.tiles_matched;
        self.tiles_mixed += o.tiles_mixed;
        self.tiles_other += o.tiles_other;
        self.cells_excluded += o.cells_excluded;
        self.cells_from_children += o.cells_from_children;
    }
}

/// Largest result a query may return, in cells.
pub const MAX_QUERY_CELLS: usize = 20_000_000;

/// Occupancy at or above which a level-0 cell counts as fully busy when joining burst runs.
pub const FULL_CELL_OCCUPANCY: f32 = 0.95;

/// How fine a query result should be.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Resolution {
    /// The finest level whose grid over the region fits in `t × f` cells (the coarsest level if
    /// none does).
    MaxCells {
        /// Time cells.
        t: usize,
        /// Frequency cells.
        f: usize,
    },
    /// The coarsest level whose cells are no larger than this (level 0 if none).
    Cell {
        /// Largest acceptable time cell.
        t_ns: i64,
        /// Largest acceptable frequency cell, Hz.
        f_hz: f64,
    },
    /// Exactly this level.
    Level(u8),
}

/// A region-over-time query. The result grid covers every cell that starts before `time.end`
/// (and before `freq.hi_hz`) and ends after the start: an end exactly on a cell boundary does not
/// add the next cell.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RegionQuery {
    /// Frequency extent.
    pub freq: FreqRange,
    /// Time extent.
    pub time: TimeRange,
    /// Resolution request.
    pub resolution: Resolution,
}

/// Statistics of one result cell. dB values are rounded to 0.01 dB and fractions to 1/65535, the
/// stored resolution. Unobserved cells have `frames == 0` and NaN statistics: "not observed" is not
/// "quiet" (C26).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CellStats {
    /// Max-hold, dB/Hz.
    pub max_db: f32,
    /// Power mean, dB/Hz.
    pub mean_db: f32,
    /// Low percentile (noise floor), dB/Hz.
    pub p_low_db: f32,
    /// High percentile, dB/Hz.
    pub p_high_db: f32,
    /// Fraction of observed time above threshold.
    pub occupancy: f32,
    /// Highest occupancy of any level-0 cell folded into this one (short busy periods survive).
    pub occupancy_max: f32,
    /// Fraction of the cell's duration observed.
    pub coverage: f32,
    /// Noise floor estimate, dB/Hz (T-116): the low percentile corrected for its bias on
    /// averaged-periodogram noise (`floor = p_low − 10·log10(P⁻¹(n_c, p)/n_c)`, the Gamma model of
    /// `hk_dsp::radiometry::bias`; `p` is the exact order-statistic probability at level 0 and the
    /// percentile itself at rolled-up levels). NaN when the tile's cell shape `n_c` is unknown or
    /// mixed ([`ProvenanceSummary::uniform_cell_shape`]).
    pub floor_db: f32,
    /// Frames folded (see [`ProvenanceSummary`] for counting).
    pub frames: u32,
    /// Level the values came from (coarser than the query's level when finer tiles are absent);
    /// `u8::MAX` when no level has a tile here.
    pub level: u8,
}

impl CellStats {
    /// A cell with no tile at any level.
    pub const NONE: CellStats = CellStats {
        max_db: f32::NAN,
        mean_db: f32::NAN,
        p_low_db: f32::NAN,
        p_high_db: f32::NAN,
        occupancy: f32::NAN,
        occupancy_max: f32::NAN,
        coverage: 0.0,
        floor_db: f32::NAN,
        frames: 0,
        level: u8::MAX,
    };

    /// The cell has observations.
    pub fn observed(&self) -> bool {
        self.frames > 0
    }
}

/// A query result: a `nt × nf` grid (row-major, time then frequency) at one level.
#[derive(Clone, Debug, PartialEq)]
pub struct RegionHistory {
    /// Pyramid scheme/version id of the tiles (T-116).
    pub scheme: u16,
    /// Level of the grid.
    pub level: u8,
    /// Unit of the dB values (densities per Hz).
    pub unit: PowerUnit,
    /// Frequency-cell width, Hz.
    pub f_cell_hz: f64,
    /// Global index of frequency cell 0 (it covers `[f_first_cell·w, …)`).
    pub f_first_cell: i64,
    /// Frequency cells.
    pub nf: usize,
    /// Time-cell duration, ns.
    pub t_cell_ns: i64,
    /// Global index of time cell 0 (it starts at `t_first_cell·t_cell_ns` since the epoch).
    pub t_first_cell: i64,
    /// Time cells.
    pub nt: usize,
    /// Which percentiles `p_low_db` / `p_high_db` are.
    pub percentiles: (f32, f32),
    /// Cells.
    pub cells: Vec<CellStats>,
    /// Provenance of every tile that contributed.
    pub provenance: ProvenanceSummary,
    /// Distinct tiles read (memory or disk).
    pub tiles_read: usize,
    /// The source/site filter and what it excluded (T-133); `None` for an unfiltered query.
    pub filter: Option<FilterSummary>,
}

/// Largest number of gaps [`RegionHistory::coverage_summary`] lists.
pub const MAX_COVERAGE_GAPS: usize = 1000;

/// What a [`RegionHistory`] actually observed (T-116): not observed is not quiet (C26).
#[derive(Clone, Debug, PartialEq)]
pub struct CoverageSummary {
    /// Cells in the grid.
    pub cells: usize,
    /// Cells with at least one frame.
    pub observed_cells: usize,
    /// Mean coverage over all cells (unobserved cells count as 0).
    pub observed_fraction: f64,
    /// Maximal runs of time rows in which no cell was observed, in time order (at most
    /// [`MAX_COVERAGE_GAPS`]).
    pub gaps: Vec<TimeRange>,
    /// More gaps than listed.
    pub gaps_truncated: bool,
}

impl RegionHistory {
    /// The grid's coverage mask digest: observed cells, mean coverage, fully unobserved time runs.
    pub fn coverage_summary(&self) -> CoverageSummary {
        let observed_cells = self.cells.iter().filter(|c| c.observed()).count();
        let cov_sum: f64 = self.cells.iter().map(|c| f64::from(c.coverage)).sum();
        let mut gaps = Vec::new();
        let mut truncated = false;
        let mut run: Option<usize> = None;
        for t in 0..=self.nt {
            let empty = t < self.nt && !self.row(t).iter().any(CellStats::observed);
            match (empty, run) {
                (true, None) => run = Some(t),
                (false, Some(t0)) => {
                    if gaps.len() < MAX_COVERAGE_GAPS {
                        gaps.push(TimeRange::new(self.time_of(t0), self.time_of(t)));
                    } else {
                        truncated = true;
                    }
                    run = None;
                }
                _ => {}
            }
        }
        CoverageSummary {
            cells: self.cells.len(),
            observed_cells,
            observed_fraction: if self.cells.is_empty() {
                0.0
            } else {
                cov_sum / self.cells.len() as f64
            },
            gaps,
            gaps_truncated: truncated,
        }
    }

    /// Cell `(t, f)`.
    pub fn cell(&self, t: usize, f: usize) -> &CellStats {
        &self.cells[t * self.nf + f]
    }

    /// One time row.
    pub fn row(&self, t: usize) -> &[CellStats] {
        &self.cells[t * self.nf..(t + 1) * self.nf]
    }

    /// Frequency extent of column `f`.
    pub fn freq_of(&self, f: usize) -> FreqRange {
        let lo = (self.f_first_cell + f as i64) as f64 * self.f_cell_hz;
        FreqRange::new(lo, lo + self.f_cell_hz)
    }

    /// Start of row `t`.
    pub fn time_of(&self, t: usize) -> Timestamp {
        Timestamp::from_unix_nanos((self.t_first_cell + t as i64) * self.t_cell_ns)
    }

    /// Frequency columns covering `channel`: overlap ≥ half of min(cell width, channel width).
    pub fn columns_for(&self, channel: FreqRange) -> Vec<usize> {
        let need = 0.5 * self.f_cell_hz.min(channel.width_hz());
        (0..self.nf)
            .filter(|&f| {
                let c = self.freq_of(f);
                (c.hi_hz.min(channel.hi_hz) - c.lo_hz.max(channel.lo_hz)) >= need
            })
            .collect()
    }

    /// Per-channel summaries over the grid. See [`ChannelSummary`] for the estimators.
    pub fn channel_summaries(&self, channels: &[FreqRange]) -> Vec<ChannelSummary> {
        channels
            .iter()
            .map(|&ch| self.channel_summary(ch))
            .collect()
    }

    fn channel_summary(&self, channel: FreqRange) -> ChannelSummary {
        let cols = self.columns_for(channel);
        let t_cell_s = self.t_cell_ns as f64 * 1e-9;
        let hour_ns = 3_600_000_000_000i64;
        let hourly = self.t_cell_ns <= hour_ns && hour_ns % self.t_cell_ns == 0;
        let mut s = ChannelSummary {
            channel,
            columns: cols.clone(),
            occupancy: vec![f32::NAN; self.nt],
            coverage: vec![0.0; self.nt],
            occupancy_fraction: None,
            peak_occupancy: 0.0,
            observed_s: 0.0,
            bursts_s: Vec::new(),
            hour_of_day: [None; 24],
        };
        let (mut exp_sum, mut occ_sum) = (0.0f64, 0.0f64);
        let mut hod = [(0.0f64, 0.0f64); 24];
        // Burst runs.
        let (mut run, mut in_run, mut can_extend) = (0.0f64, false, false);
        for t in 0..self.nt {
            let (mut occ, mut cov) = (f32::NAN, 0.0f32);
            for &f in &cols {
                let c = self.cell(t, f);
                if !c.observed() {
                    continue;
                }
                occ = if occ.is_nan() {
                    c.occupancy
                } else {
                    occ.max(c.occupancy)
                };
                cov = cov.max(c.coverage);
                s.peak_occupancy = s.peak_occupancy.max(c.occupancy_max);
            }
            s.occupancy[t] = occ;
            s.coverage[t] = cov;
            if occ.is_nan() {
                if in_run {
                    s.bursts_s.push(run);
                    in_run = false;
                }
                continue;
            }
            let exposure = f64::from(cov) * t_cell_s;
            exp_sum += exposure;
            occ_sum += f64::from(occ) * exposure;
            if hourly {
                let h =
                    (self.time_of(t).as_unix_nanos().rem_euclid(24 * hour_ns) / hour_ns) as usize;
                hod[h].0 += exposure;
                hod[h].1 += f64::from(occ) * exposure;
            }
            if occ > 0.0 {
                let busy = f64::from(occ) * exposure;
                if in_run && can_extend {
                    run += busy;
                    can_extend = occ >= FULL_CELL_OCCUPANCY;
                } else {
                    if in_run {
                        s.bursts_s.push(run);
                    }
                    run = busy;
                    in_run = true;
                    // A burst usually starts part-way through its first row.
                    can_extend = true;
                }
            } else if in_run {
                s.bursts_s.push(run);
                in_run = false;
            }
        }
        if in_run {
            s.bursts_s.push(run);
        }
        s.observed_s = exp_sum;
        s.occupancy_fraction = (exp_sum > 0.0).then(|| occ_sum / exp_sum);
        if hourly {
            for (out, (e, o)) in s.hour_of_day.iter_mut().zip(hod) {
                *out = (e > 0.0).then(|| o / e);
            }
        }
        s
    }
}

/// Occupancy of one channel over a [`RegionHistory`].
///
/// Estimators:
/// - per time row, the channel's occupancy is the **maximum** over its frequency columns (ITU
///   FCO: a revisit is occupied if any sample in the channel exceeds the threshold), coverage the
///   maximum coverage;
/// - `occupancy_fraction` and `hour_of_day` are exposure-weighted means (exposure = coverage × cell
///   duration); `hour_of_day` is by UTC hour of the row start and only filled when rows are at most
///   an hour and divide it;
/// - `bursts_s` approximates burst durations from run lengths: a run starts at an occupied row;
///   the next occupied row joins it if the previous row was the run's first row (a burst usually
///   starts part-way through a row) or was fully busy (occupancy ≥ [`FULL_CELL_OCCUPANCY`]), so a
///   partly busy row inside a run ends it (one burst ended, another began). A run lasts
///   Σ occupancy × exposure. Meaningful at level 0; bursts separated by less than a row can merge.
#[derive(Clone, Debug, PartialEq)]
pub struct ChannelSummary {
    /// The channel.
    pub channel: FreqRange,
    /// Grid columns used.
    pub columns: Vec<usize>,
    /// Occupancy per time row (NaN unobserved).
    pub occupancy: Vec<f32>,
    /// Coverage per time row.
    pub coverage: Vec<f32>,
    /// Exposure-weighted occupancy over the grid.
    pub occupancy_fraction: Option<f64>,
    /// Highest max-occupancy of any contributing cell.
    pub peak_occupancy: f32,
    /// Observed seconds.
    pub observed_s: f64,
    /// Estimated burst durations, seconds, in time order.
    pub bursts_s: Vec<f64>,
    /// Occupancy by UTC hour of day.
    pub hour_of_day: [Option<f64>; 24],
}

/// Counts `durations` into bins `[edges[i], edges[i+1])` (the last edge may be infinite).
pub fn burst_histogram(durations: &[f64], edges: &[f64]) -> Vec<usize> {
    let mut out = vec![0; edges.len().saturating_sub(1)];
    for &d in durations {
        if let Some(i) = edges.windows(2).position(|w| d >= w[0] && d < w[1]) {
            out[i] += 1;
        }
    }
    out
}

enum Source<'a> {
    Mem(&'a Tile, Option<ColumnPreview>),
    Disk(Box<Tile>),
}

impl Source<'_> {
    fn tile(&self) -> &Tile {
        match self {
            Source::Mem(t, _) => t,
            Source::Disk(t) => t,
        }
    }
}

impl Pyramid {
    /// Chooses the level for a query.
    pub fn choose_level(&self, q: &RegionQuery) -> usize {
        let top = self.geom.top();
        let dims = |l: usize| {
            let g = &self.geom.levels[l];
            let nf = ((q.freq.hi_hz / g.f_cell_hz).ceil() - (q.freq.lo_hz / g.f_cell_hz).floor())
                .max(1.0) as usize;
            let t0 = q.time.start.as_unix_nanos().div_euclid(g.t_cell_ns);
            let t1 = (q.time.end.as_unix_nanos() + g.t_cell_ns - 1).div_euclid(g.t_cell_ns);
            ((t1 - t0).max(1) as usize, nf)
        };
        match q.resolution {
            Resolution::Level(l) => usize::from(l).min(top),
            Resolution::MaxCells { t, f } => (0..=top)
                .find(|&l| {
                    let (nt, nf) = dims(l);
                    nt <= t && nf <= f
                })
                .unwrap_or(top),
            Resolution::Cell { t_ns, f_hz } => (0..=top)
                .rev()
                .find(|&l| {
                    let g = &self.geom.levels[l];
                    g.t_cell_ns <= t_ns && g.f_cell_hz <= f_hz
                })
                .unwrap_or(0),
        }
    }

    /// Answers a region-over-time query from open (in-memory) and sealed tiles.
    ///
    /// Work is bounded by the result: each output cell's tile is addressed by key
    /// `(level, f_block, t_block)` and loaded at most once; nothing is scanned. Where the chosen
    /// level has no tile (evicted, or never written), the cell is filled from the finest coarser
    /// level that has one, and [`CellStats::level`] says so.
    pub fn query(&self, q: &RegionQuery) -> Result<RegionHistory, StoreError> {
        self.query_filtered(q, &OriginFilter::ANY)
    }

    /// [`Pyramid::query`] over the frames of one source and/or site only (T-133).
    ///
    /// Tiles are not split by origin (the grid is fixed), so the filter works on what each tile
    /// recorded ([`ProvenanceSummary::origins`]):
    /// - a tile whose frames **all** pass answers its cells as usual;
    /// - a tile with **no** passing frame answers its cells as unobserved at its level (no coarser
    ///   fallback: a coarser cell there rolls up the same frames);
    /// - a **mixed** tile's cell is kept only when the one finer tile it rolls up (the tile of the
    ///   level below that forms its time column) exists and passes whole; otherwise the cell is
    ///   unobserved. Level-0 cells of a mixed tile are unobserved. So a site change costs one
    ///   finer tile's duration of coverage, never mixes another site's data in.
    ///
    /// Frames of unknown origin (tiles written before format 3, frames without a site) pass only
    /// an unfiltered query or an [`super::OriginField::Unknown`] field. Excluded cells are "not
    /// observed", never "quiet"; [`RegionHistory::filter`] counts them. The result's provenance
    /// merges only the tiles whose data it returns.
    pub fn query_filtered(
        &self,
        q: &RegionQuery,
        filter: &OriginFilter,
    ) -> Result<RegionHistory, StoreError> {
        if !(q.freq.lo_hz.is_finite() && q.freq.hi_hz >= q.freq.lo_hz) {
            return Err(StoreError::BadQuery("frequency range".into()));
        }
        if q.time.end < q.time.start {
            return Err(StoreError::BadQuery("time range".into()));
        }
        let level = self.choose_level(q);
        let g = self.geom.levels[level];
        let c_lo = (q.freq.lo_hz / g.f_cell_hz).floor() as i64;
        let c_hi = ((q.freq.hi_hz / g.f_cell_hz).ceil() as i64 - 1).max(c_lo);
        let t_lo = q.time.start.as_unix_nanos().div_euclid(g.t_cell_ns);
        let t_hi =
            ((q.time.end.as_unix_nanos() + g.t_cell_ns - 1).div_euclid(g.t_cell_ns) - 1).max(t_lo);
        let (nf, nt) = ((c_hi - c_lo + 1) as usize, (t_hi - t_lo + 1) as usize);
        if nf.saturating_mul(nt) > MAX_QUERY_CELLS {
            return Err(StoreError::BadQuery(format!(
                "{nt} × {nf} cells at level {level} exceeds {MAX_QUERY_CELLS}; ask for a coarser resolution"
            )));
        }
        let margin = self.cfg.occupancy_margin_db;
        let pct = (self.cfg.low_percentile, self.cfg.high_percentile);
        let tile_nf = self.geom.nf as i64;
        let mut cells = vec![CellStats::NONE; nf * nt];
        let mut cache: HashMap<(usize, i64, i64), Option<(Source<'_>, OriginMatch)>> =
            HashMap::new();
        let mut provenance = ProvenanceSummary::default();
        let mut bias = BiasCache::default();
        let filtered = !filter.is_any();
        let mut summary = FilterSummary {
            filter: *filter,
            ..FilterSummary::default()
        };
        // Finer tiles consulted for mixed tiles' cells: their origin match, and whether their
        // provenance is already merged into the result.
        let mut children: HashMap<(usize, i64, i64), Option<(OriginMatch, ProvenanceSummary)>> =
            HashMap::new();
        let mut merged_children: HashSet<(usize, i64, i64)> = HashSet::new();
        for ti in 0..nt {
            let t_start = (t_lo + ti as i64) * g.t_cell_ns;
            for fi in 0..nf {
                let f_centre = ((c_lo + fi as i64) as f64 + 0.5) * g.f_cell_hz;
                for l in level..=self.geom.top() {
                    let gl = &self.geom.levels[l];
                    let tc = t_start.div_euclid(gl.t_cell_ns);
                    let tb = tc.div_euclid(gl.nt as i64);
                    let t_in = (tc - tb * gl.nt as i64) as usize;
                    let fc = (f_centre / gl.f_cell_hz).floor() as i64;
                    let fb = fc.div_euclid(tile_nf);
                    let f_in = (fc - fb * tile_nf) as usize;
                    let key = (l, fb, tb);
                    if let Entry::Vacant(slot) = cache.entry(key) {
                        let src = self.load_source(l, fb, tb, margin, pct)?.map(|s| {
                            let m = if filtered {
                                s.tile().prov.origin_match(filter)
                            } else {
                                OriginMatch::All
                            };
                            match m {
                                OriginMatch::All => {
                                    provenance.merge(&s.tile().prov);
                                    summary.tiles_matched += 1;
                                }
                                OriginMatch::Mixed => summary.tiles_mixed += 1,
                                OriginMatch::None => summary.tiles_other += 1,
                            }
                            (s, m)
                        });
                        slot.insert(src);
                    }
                    let Some((src, m)) = &cache[&key] else {
                        continue;
                    };
                    let keep = match m {
                        OriginMatch::All => true,
                        OriginMatch::None => false,
                        OriginMatch::Mixed if l == 0 => false,
                        OriginMatch::Mixed => {
                            // The cell rolls up the level-(l−1) tile(s) at time block `tc`
                            // holding its `f_factor` finer frequency cells.
                            let factor = i64::from(gl.f_factor);
                            let (b0, b1) = (
                                (fc * factor).div_euclid(tile_nf),
                                ((fc + 1) * factor - 1).div_euclid(tile_nf),
                            );
                            let mut ok = true;
                            for cb in b0..=b1 {
                                let ck = (l - 1, cb, tc);
                                if let Entry::Vacant(slot) = children.entry(ck) {
                                    let p = self.tile_provenance(l - 1, cb, tc)?;
                                    slot.insert(p.map(|p| (p.origin_match(filter), p)));
                                }
                                ok &= matches!(children[&ck], Some((OriginMatch::All, _)));
                            }
                            if ok {
                                summary.cells_from_children += 1;
                                for cb in b0..=b1 {
                                    let ck = (l - 1, cb, tc);
                                    if merged_children.insert(ck)
                                        && let Some((_, p)) = &children[&ck]
                                    {
                                        provenance.merge(p);
                                    }
                                }
                            }
                            ok
                        }
                    };
                    cells[ti * nf + fi] = if keep {
                        cell_stats(src, l, t_in, f_in, pct.0, &mut bias)
                    } else {
                        let tile = src.tile();
                        if tile.count[t_in * tile.nf + f_in] > 0 {
                            summary.cells_excluded += 1;
                        }
                        CellStats {
                            level: l as u8,
                            ..CellStats::NONE
                        }
                    };
                    break;
                }
            }
        }
        let tiles_read = cache.values().filter(|s| s.is_some()).count();
        Ok(RegionHistory {
            scheme: self.cfg.scheme,
            level: level as u8,
            unit: self.cfg.unit,
            f_cell_hz: g.f_cell_hz,
            f_first_cell: c_lo,
            nf,
            t_cell_ns: g.t_cell_ns,
            t_first_cell: t_lo,
            nt,
            percentiles: pct,
            cells,
            provenance,
            tiles_read,
            filter: filtered.then_some(summary),
        })
    }

    fn load_source(
        &self,
        level: usize,
        fb: i64,
        tb: i64,
        margin: f32,
        pct: (f32, f32),
    ) -> Result<Option<Source<'_>>, StoreError> {
        if let Some(t) = self.open[level].get(&(fb, tb)) {
            let preview = if level == 0 {
                let floor = self.floors.get(&fb).map(|f| &f.floor[..]);
                t.column_preview(floor, margin, pct)
            } else {
                None
            };
            return Ok(Some(Source::Mem(t, preview)));
        }
        Ok(self
            .read_sealed(level, fb, tb)?
            .map(|t| Source::Disk(Box::new(t))))
    }
}

/// Floor bias per `(shape bits, frames)` (`frames` = 0 for rolled-up cells).
#[derive(Default)]
struct BiasCache(HashMap<(u32, u32), f32>);

impl BiasCache {
    fn bias_db(&mut self, shape: f32, level: usize, frames: u32, q: f32) -> f32 {
        let n = if level == 0 { frames } else { 0 };
        *self.0.entry((shape.to_bits(), n)).or_insert_with(|| {
            let p = if level == 0 {
                hk_dsp::radiometry::exact_percentile_probability(f64::from(q), frames)
            } else {
                f64::from(q) / 100.0
            };
            hk_dsp::radiometry::percentile_bias_db(f64::from(shape), p) as f32
        })
    }
}

fn cell_stats(
    src: &Source<'_>,
    level: usize,
    t_in: usize,
    f_in: usize,
    q_low: f32,
    bias: &mut BiasCache,
) -> CellStats {
    let tile = src.tile();
    let i = t_in * tile.nf + f_in;
    let n = tile.count[i];
    if n == 0 {
        return CellStats {
            level: level as u8,
            ..CellStats::NONE
        };
    }
    let (mut p_lo, mut p_hi) = (tile.p_lo[i], tile.p_hi[i]);
    let mut occ_raw = tile.occ_s[i];
    let mut occ_max = tile.occ_max[i];
    let mut previewed = false;
    if let Source::Mem(_, Some((pt, pv))) = src
        && *pt == t_in
    {
        let (a, b, o) = pv[f_in];
        if a.is_finite() {
            p_lo = a;
            p_hi = b;
        }
        occ_raw += o;
        previewed = true;
    }
    let raw_obs = tile.obs_s[i];
    let obs = raw_obs.min(tile.t_cell_s);
    let occupancy = if raw_obs > 0.0 {
        (occ_raw / raw_obs).min(1.0)
    } else {
        0.0
    };
    if previewed {
        // An open level-0 column: its max-occupancy is its own occupancy so far.
        occ_max = occ_max.max(occupancy as f32);
    }
    let floor_db = match tile.prov.uniform_cell_shape() {
        Some(shape) if p_lo.is_finite() => {
            round_centi(round_centi(p_lo) - bias.bias_db(shape, level, n, q_low))
        }
        _ => f32::NAN,
    };
    CellStats {
        floor_db,
        max_db: round_centi(tile.max[i]),
        mean_db: round_centi(db(tile.sum_lin[i] / f64::from(n))),
        p_low_db: round_centi(p_lo),
        p_high_db: round_centi(p_hi),
        occupancy: round_frac(occupancy as f32),
        occupancy_max: round_frac(occ_max),
        coverage: round_frac((obs / tile.t_cell_s) as f32),
        frames: n,
        level: level as u8,
    }
}
