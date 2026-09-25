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
    /// percentile itself at rolled-up levels). A mixed-shape tile uses the bias of the Gamma
    /// mixture of its values per shape (T-141, [`ProvenanceSummary::cell_shape_mixture`]). NaN
    /// when a shape is unknown, or a mixed tile recorded no per-shape counts (format < 4).
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

/// One cell of an [`Overview`] (T-338). Unobserved when `sources == 0`: not observed is not quiet
/// (C26), so an empty cell is `NaN`, never a floor or a zero.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OverviewCell {
    /// Max-hold over the folded cells, dB/Hz; `NaN` when none was observed.
    pub max_db: f32,
    /// Highest `occupancy_max` folded; `NaN` when none was observed.
    pub occupancy_max: f32,
    /// Observed fraction of **this output cell's own** time–frequency extent (0 when none was
    /// observed): each folded source cell's `coverage` weighted by the fraction of the output cell
    /// it overlaps, summed (T-419). Never a plain mean over source cells — see
    /// [`OverviewCell::fold`].
    pub coverage: f32,
    /// Frames behind the cell.
    pub frames: u64,
    /// Observed source cells folded in. `0` = unobserved.
    pub sources: u32,
}

impl OverviewCell {
    /// A cell nothing was folded into.
    pub const UNOBSERVED: OverviewCell = OverviewCell {
        max_db: f32::NAN,
        occupancy_max: f32::NAN,
        coverage: 0.0,
        frames: 0,
        sources: 0,
    };

    /// Whether anything was observed here.
    pub fn observed(&self) -> bool {
        self.sources > 0
    }

    /// Folds one source cell in, `w` being the fraction of **this output cell's** extent that the
    /// source cell overlaps (time × frequency, in `[0, 1]`).
    ///
    /// **Coverage is weighted by extent, not counted per source (T-419).** `overview` lays a
    /// *fractional* grid on the requested window (`t_cell_ns = (t1 − t0)/nt`) and folds every
    /// source cell that **overlaps** an output cell, so the old `+= coverage` then `/= sources`
    /// was a mean that let a source cell overlapping by 1 % count as much as one overlapping by
    /// 100 % — and, worse, let a single observed source cell inside an otherwise unobserved output
    /// cell report the whole cell covered. It was exact only when every source cell had equal
    /// duration *and* lay wholly inside the output cell; the first holds (one level answers one
    /// query), the second does not. Weighting by overlap makes the divisor the **output cell's own
    /// extent**: source cells are disjoint, so the weights sum to at most 1, and the parts of the
    /// output cell no source cell reaches correctly count as unobserved rather than being averaged
    /// away. Where the output grid *is* an exact coarsening of the source grid the two rules agree
    /// exactly, which is why this is a strict fix rather than a change of meaning.
    ///
    /// The max-holds keep their own rule — replicating a measured value across the cells it covers
    /// is T-334's safe direction — so only `coverage` is weighted. Folding must never *lower* a
    /// measurement and never *raise* coverage: both rules exist for the same reason, and having
    /// one does not give you the other.
    fn fold(&mut self, c: &CellStats, w: f32) {
        self.max_db = if self.sources == 0 {
            c.max_db
        } else {
            self.max_db.max(c.max_db)
        };
        self.occupancy_max = if self.sources == 0 {
            c.occupancy_max
        } else {
            self.occupancy_max.max(c.occupancy_max)
        };
        self.coverage += c.coverage * w;
        self.frames += u64::from(c.frames);
        self.sources += 1;
    }
}

/// A [`RegionHistory`] compressed onto a fixed `nt × nf` grid laid on a requested window
/// ([`RegionHistory::overview`], T-338).
///
/// Unlike a [`RegionHistory`], whose grid is the pyramid level's and snaps outward, an overview's
/// cells are exactly `window / nt` by `(hi − lo) / nf`, so the grid **is** the window it was asked
/// for: the timeline that draws it spans the capture window and nothing else.
#[derive(Clone, Debug, PartialEq)]
pub struct Overview {
    /// Unit of `max_db` and `range_db` — the source grid's, carried rather than re-derived (T-342).
    ///
    /// A folded value whose scale is not carried alongside it is a number a consumer has to guess
    /// the meaning of, and "max-hold **and** the scale, stated" is the whole point of serving the
    /// fold from here instead of reducing in the client. Densities per Hz, as in [`RegionHistory`].
    pub unit: PowerUnit,
    /// Time cells.
    pub nt: usize,
    /// Frequency cells.
    pub nf: usize,
    /// Start of time cell 0, Unix ns (the window's start exactly).
    pub t0_ns: i64,
    /// Time-cell duration, ns (fractional: the window divided by `nt`, not a pyramid tier).
    pub t_cell_ns: f64,
    /// Low edge of frequency cell 0, Hz.
    pub f_lo_hz: f64,
    /// Frequency-cell width, Hz.
    pub f_cell_hz: f64,
    /// Cells, row-major: time then frequency.
    pub cells: Vec<OverviewCell>,
    /// Cells into which at least one observed source cell was folded.
    pub observed_cells: usize,
    /// Time cells of the source grid (what the pyramid level gave).
    pub src_nt: usize,
    /// Frequency cells of the source grid.
    pub src_nf: usize,
    /// `(min, max)` of the observed `max_db`, the grid's own dynamic range; `None` when nothing was
    /// observed. Served so the client never decides a colour scale from the numbers it happens to
    /// hold — that choice is a measurement too.
    pub range_db: Option<(f32, f32)>,
}

/// Output cells `[a, b)` of `n` cells of width `step` from `origin` that `[lo, hi)` overlaps.
/// Half-open, so a boundary landing exactly on an output boundary reaches the later cell only —
/// [`RegionHistory::overview`]'s rule.
fn cell_span(lo: f64, hi: f64, origin: f64, step: f64, n: usize) -> (usize, usize) {
    let a = ((lo - origin) / step).floor().max(0.0) as usize;
    let b = (((hi - origin) / step).ceil().max(0.0) as usize).min(n);
    (a.min(n), b.max(a.min(n)))
}

/// The most-recent-known value of one frequency column (T-519): the **last-known / stale** tier.
///
/// Not a measurement *of the time it is drawn at* — a measurement of `t_ns` and earlier, carried
/// forward. That is why it carries its own timestamp: a value without the instant it was last true
/// is exactly the "implying detail the front end can't deliver" the view invariants forbid.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LastKnownCell {
    /// Max-hold of the newest observed source cell, dB/Hz; `NaN` when none was found.
    pub max_db: f32,
    /// When it was last seen: the end of that source cell, clamped to the search's `before`, Unix
    /// ns. An upper bound — the cell's frames all lie at or before it. `i64::MIN` when none.
    pub t_ns: i64,
    /// Store level the source cell came from; `u8::MAX` when none.
    pub level: u8,
}

impl LastKnownCell {
    /// Nothing observed in the searched reach.
    pub const NONE: LastKnownCell = LastKnownCell {
        max_db: f32::NAN,
        t_ns: i64::MIN,
        level: u8::MAX,
    };

    /// Whether a value was found.
    pub fn found(&self) -> bool {
        self.level != u8::MAX
    }
}

/// One stage of a [`LastKnown`] search: a time window read at one level, or skipped.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LastKnownStage {
    /// Store level of the stage.
    pub level: u8,
    /// Window start, Unix ns.
    pub from_ns: i64,
    /// Window end (exclusive), Unix ns.
    pub to_ns: i64,
    /// Source cells read (0 when skipped).
    pub source_cells: usize,
    /// Columns this stage resolved.
    pub found: usize,
    /// Not read: its grid would not fit the work budget, or the window is not aligned to the
    /// level's cell. A skipped window is **not searched**, and says so, rather than being reported
    /// as empty.
    pub skipped: bool,
}

/// Per-column most-recent-known values before an instant ([`Pyramid::last_known`], T-519).
#[derive(Clone, Debug, PartialEq)]
pub struct LastKnown {
    /// The instant the values are known at or before, Unix ns.
    pub before_ns: i64,
    /// Low edge of column 0, Hz.
    pub f_lo_hz: f64,
    /// Column width, Hz.
    pub f_cell_hz: f64,
    /// Columns.
    pub nf: usize,
    /// Per column.
    pub cells: Vec<LastKnownCell>,
    /// Stages, newest window first.
    pub stages: Vec<LastKnownStage>,
    /// The oldest instant the search reached, Unix ns. A column with no value was not observed in
    /// `[searched_from_ns, before_ns)` outside any skipped stage — **nothing is claimed about
    /// earlier**.
    pub searched_from_ns: i64,
    /// Source cells read, over every stage.
    pub source_cells: usize,
}

impl LastKnown {
    fn empty(freq: FreqRange, before_ns: i64, nf: usize) -> Self {
        let nf = nf.max(1);
        let hi = freq.hi_hz.max(freq.lo_hz + f64::EPSILON);
        LastKnown {
            before_ns,
            f_lo_hz: freq.lo_hz,
            f_cell_hz: (hi - freq.lo_hz) / nf as f64,
            nf,
            cells: vec![LastKnownCell::NONE; nf],
            stages: Vec::new(),
            searched_from_ns: before_ns,
            source_cells: 0,
        }
    }

    /// Columns with a value.
    pub fn found(&self) -> usize {
        self.cells.iter().filter(|c| c.found()).count()
    }

    /// Carries the values forward down an `nt`-row grid starting at `before_ns` (T-519): the
    /// **shadow** plane, as runs per column.
    ///
    /// Row `r` covers `[before_ns + r·Δt, …)`. Down each column the carried value starts as this
    /// search's value, and is **replaced** by `grid`'s own value at every row where `grid` holds
    /// one — so a band observed part-way down a tile and then departed carries the value it was last
    /// seen with, not the older one from before the tile. A row where `grid` holds a value gets
    /// **no** run: the shadow never stands in for a measurement of that row. Rows starting at or
    /// after `edge_ns` (the store's data edge) get no run either: carrying forward past the newest
    /// frame would paint the future.
    ///
    /// # Every gap in an observed column is filled (T-527)
    ///
    /// A column observed only *part-way down* the grid used to leave the rows **above** its first
    /// sample grey, because nothing older than them existed to carry forward — and grey means
    /// *nothing was ever observed here*, which for such a column is false. So the stretch before a
    /// column's **first-ever** sample takes that first sample, marked [`ShadowFill::Backward`]:
    ///
    /// - a gap **after** any sample takes the nearest **past** sample ([`ShadowFill::Forward`]);
    /// - the stretch **before the first-ever** sample takes that first sample — **the one and only
    ///   backward read in time**, and only where this search found nothing older (a column with a
    ///   before-tile value has no first-ever sample here, so its head is a forward carry — and
    ///   "first-ever" is only as strong as the search was: where a stage was skipped, something
    ///   older may live in a window that was not read, which `LastKnown::stages` states);
    /// - a column with **no sample and no before-tile value carries no run at all**: it was never
    ///   observed, and grey is exactly the right answer for it.
    ///
    /// The two directions are not interchangeable and travel separately on the wire: a forward run
    /// claims *this is what it looked like when we last saw it*, a backward run *this is what it
    /// looked like when we first saw it*. Each reports the instant of the boundary **nearest** its
    /// own rows — a forward run the source cell's end, a backward run the source cell's start — so
    /// `|row time − t_ns|` is in both directions the smallest age the evidence supports.
    ///
    /// `grid`, when given, must be `nt × nf` on the same columns; `None` is a grid holding
    /// nothing (the coverage-map short-circuit's tile) — which is also why that path pays nothing
    /// for the backward fill: with no sample in the grid there is no first-ever sample to read back
    /// from, and the walk is the one T-523 budgeted.
    pub fn carry_forward(
        &self,
        grid: Option<&Overview>,
        t_cell_ns: f64,
        nt: usize,
        edge_ns: Option<i64>,
    ) -> Vec<ShadowRun> {
        self.carry_forward_to(grid, t_cell_ns, nt, edge_ns, None)
    }

    /// [`Self::carry_forward`], carried on **past the store's data edge** where the caller's tune
    /// record proves the radio was not looking (T-881).
    ///
    /// The data edge is the newest frame the store has *folded*, and the fold trails capture: the
    /// rows between it and the newest instant the tune record reaches are real time the radio spent
    /// **somewhere else** for a departed band — nothing will ever be measured there, so the value
    /// last seen is exactly as true of them as of the rows below the edge. Stopping at the edge
    /// left them with no run, and a cell the coverage map calls unobserved with no run over it is
    /// THE grey: the newest rows of every departed band, drawn as never observed.
    ///
    /// `beyond` is `(reach_ns, unobserved)`: rows starting in `[edge_ns, reach_ns)` are covered
    /// only at cells whose `unobserved[r * nf + f]` is true (the coverage plane, laid on this
    /// grid). A column meets a cell past the edge that is **not** unobserved, and it stops there
    /// for good: the radio looked at it, what it measured is not folded yet, and carrying an older
    /// value over or past that would stand an old number in for a newer measurement. Rows at or
    /// after `reach_ns` are the future and are never covered. A mask of the wrong length is no
    /// extension at all.
    pub fn carry_forward_to(
        &self,
        grid: Option<&Overview>,
        t_cell_ns: f64,
        nt: usize,
        edge_ns: Option<i64>,
        beyond: Option<(i64, &[bool])>,
    ) -> Vec<ShadowRun> {
        let grid = grid.filter(|g| g.nt == nt && g.nf == self.nf && g.cells.len() == nt * g.nf);
        let edge = edge_ns.unwrap_or(i64::MAX);
        let beyond = beyond.filter(|(reach, m)| *reach > edge && m.len() == nt * self.nf);
        let mut runs = Vec::new();
        for f in 0..self.nf {
            let seed = self.cells[f];
            let mut cur: Option<(f32, i64, Option<u8>)> =
                seed.found()
                    .then_some((seed.max_db, seed.t_ns, Some(seed.level)));
            let mut open: Option<ShadowRun> = None;
            // T-527: rows above the column's first-ever sample, still waiting for it. Set only when
            // the search found nothing older than the grid — a column with a before-tile value has
            // its head carried FORWARD from that value, and nothing else ever reads backward.
            let mut head = cur.is_none();
            for r in 0..nt {
                let row_start = self.before_ns + (r as f64 * t_cell_ns).round() as i64;
                if row_start >= edge
                    && !beyond.is_some_and(|(reach, m)| row_start < reach && m[r * self.nf + f])
                {
                    break;
                }
                let here = grid.map(|g| &g.cells[r * g.nf + f]);
                if let Some(c) = here.filter(|c| c.observed()) {
                    runs.extend(open.take());
                    if std::mem::take(&mut head) && r > 0 {
                        // The one backward read: the stretch before the first-ever sample takes
                        // that sample, timestamped at its cell's START — the earliest instant the
                        // value can be true, and the boundary of the stretch that carries it.
                        runs.push(ShadowRun {
                            f,
                            row: 0,
                            rows: r,
                            max_db: c.max_db,
                            t_ns: row_start,
                            level: None,
                            fill: ShadowFill::Backward,
                        });
                    }
                    let row_end = self.before_ns + ((r + 1) as f64 * t_cell_ns).round() as i64;
                    cur = Some((c.max_db, row_end.min(edge), None));
                    continue;
                }
                let Some((db, t, level)) = cur else {
                    continue;
                };
                match open.as_mut() {
                    Some(o) if o.row + o.rows == r && o.t_ns == t && o.level == level => {
                        o.rows += 1;
                    }
                    _ => {
                        runs.extend(open.take());
                        open = Some(ShadowRun {
                            f,
                            row: r,
                            rows: 1,
                            max_db: db,
                            t_ns: t,
                            level,
                            fill: ShadowFill::Forward,
                        });
                    }
                }
            }
            runs.extend(open.take());
        }
        runs
    }
}

/// Which way in time a [`ShadowRun`] carries its value (T-527).
///
/// The two are **different claims**, so they are different values rather than a detail of the
/// timestamp: a client may draw them differently, and a reader must never take one for the other.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShadowFill {
    /// The value is from **before** the run's rows: the nearest past sample, carried forward.
    /// *This is what it looked like when we last saw it.*
    Forward,
    /// The value is the column's **first-ever** sample, which lies **after** the run's rows — the
    /// only backward read there is. *This is what it looked like when we first saw it.*
    Backward,
}

/// One run of a carried value down one column of a grid ([`LastKnown::carry_forward`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShadowRun {
    /// Column.
    pub f: usize,
    /// First row.
    pub row: usize,
    /// Rows.
    pub rows: usize,
    /// The carried value, dB/Hz.
    pub max_db: f32,
    /// When the value was true, Unix ns: for [`ShadowFill::Forward`] when it was last seen (the
    /// source cell's end, see [`LastKnownCell::t_ns`]), for [`ShadowFill::Backward`] when it was
    /// **first** seen (the source cell's start, which lies *after* these rows). Either way it is
    /// the boundary nearest the run, so the age it implies is the smallest one the evidence
    /// supports.
    pub t_ns: i64,
    /// The store level of a value carried in from before the grid, or `None` for a value the grid
    /// itself holds (a band seen part-way down the grid, then departed — or, for a backward fill,
    /// first arrived).
    pub level: Option<u8>,
    /// Which way in time this run reads. A backward fill is always the grid's own first sample, so
    /// it always carries `level: None`; the two are nonetheless reported separately, because
    /// `level` states *resolution* and this states *the direction of the claim*.
    pub fill: ShadowFill,
}

/// Time blocks a pinned search ([`Pyramid::last_known_search_at`]) may look back over, from its
/// `before` (T-911). Bounds the tile-index scan that finds the newest block holding data: on the
/// view lattice's finest level (64 × 40 ms blocks) that is ~11 minutes, past which the ladder
/// answers.
pub const PINNED_REACH_BLOCKS: i64 = 256;

/// Rows a [`LastKnown`] search's final stage may read, whatever the budget allows.
pub const LAST_KNOWN_MAX_TOP_ROWS: usize = 4096;

/// What is known about the time **after** a search's `before`, which is what lets a source cell
/// straddling `before` be used (T-519).
///
/// A coarse cell containing `before` folds frames from both sides of it. If nothing reached a
/// column between `before` and the cell's end, every frame the cell holds for that column is from
/// before — so its value is a legitimate last-known one. The caller knows that because it is about
/// to draw exactly that interval: a tile's own grid says, per column, when it first holds a value.
#[derive(Clone, Debug, PartialEq)]
pub struct StraddleGuard {
    /// Per output column, the earliest instant at or after `before` at which the column holds any
    /// observation, Unix ns; `i64::MAX` when none is known up to `known_until_ns`.
    pub first_after: Vec<i64>,
    /// How far `first_after` is known, Unix ns (`i64::MAX` when it is known to the end of the data,
    /// because the newest frame lies inside it).
    pub known_until_ns: i64,
}

impl StraddleGuard {
    /// Output columns `[a, b)` saw nothing from `before` up to `end`, as far as this guard knows.
    fn admits(&self, a: usize, b: usize, end: i64) -> bool {
        end <= self.known_until_ns && self.first_after[a..b].iter().all(|&t| t >= end)
    }
}

/// One stage being read, a frequency slice per step.
#[derive(Clone, Copy, Debug)]
struct ActiveStage {
    level: usize,
    lo: i64,
    hi: i64,
    c_hi: i64,
    slice_cols: i64,
    c_next: i64,
    stage: usize,
}

/// A [`Pyramid::last_known`] search in progress, run a step at a time by
/// [`Pyramid::last_known_step`] so a caller can release the store's lock between steps.
#[derive(Clone, Debug)]
pub struct LastKnownSearch {
    out: LastKnown,
    freq: FreqRange,
    guard: Option<StraddleGuard>,
    chain: Vec<usize>,
    /// Start of the oldest tile held: nothing before it can be found.
    oldest_ns: Option<i64>,
    next: usize,
    end_ns: i64,
    per_step: usize,
    remaining: usize,
    active: Option<ActiveStage>,
    /// Columns resolved by an earlier (newer) stage: final.
    frozen: Vec<bool>,
    done: bool,
    /// T-911: the search is pinned to `chain[0]` alone ([`Pyramid::last_known_search_at`]) and
    /// walks that one level back block by block, skipping blocks that hold nothing for free.
    pinned: bool,
    /// T-911: the output grid's time cell, ns, when it is a whole multiple of the pinned level's
    /// (0 otherwise). "Newest" is then decided per OUTPUT row, so every source cell inside the
    /// newest output row folds by max-hold — the value the output grid's own cell holds.
    quantum_ns: i64,
    /// T-911: the oldest time block a pinned search may look at (its reach). Older is left to the
    /// ladder, so the index scan that finds the newest block holding data is bounded however much
    /// the store holds.
    reach_tb: i64,
}

impl LastKnownSearch {
    /// Whether no step remains.
    pub fn done(&self) -> bool {
        self.done
    }

    /// The result so far.
    pub fn finish(self) -> LastKnown {
        self.out
    }
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

    /// Compresses the grid onto an `nt × nf` overview covering exactly `window × freq` (T-338).
    ///
    /// This is the **backend** half of the timeline's compressed "sideways" overview waterfall. The
    /// pyramid's ladder couples its two axes — a level coarse enough in frequency to fit a thin
    /// strip's rows (100 kHz cells) has one-day time cells — so no single level can serve a grid
    /// that is fine in time and coarse in frequency. Choosing which measured value stands for an
    /// output cell is a **measurement**, and T-334 put measurements here rather than in the client;
    /// this is that reduction, done once, over cells the pyramid already returned.
    ///
    /// **Every statistic folded here folds exactly**, which is why only these four are carried:
    /// the max of max-holds is the max-hold, the max of `occupancy_max` is the peak occupancy,
    /// frames sum, and `coverage` is the **extent-weighted** sum of the source cells' coverage —
    /// the observed fraction of the output cell's own extent (T-419; the grid here is fractional,
    /// so a plain mean over source cells was exact only when they were equal-duration *and* wholly
    /// inside the output cell, and it let one observed cell claim a whole collapsed column). See
    /// [`OverviewCell::fold`]. Nothing is invented: a percentile (`p_low_db`, `floor_db`) cannot be
    /// folded from cell values at all, so it is not offered rather than approximated.
    ///
    /// **The fold is band-collapsing on both axes, and it is the same fold either way** (T-342).
    /// `nf = 1` collapses a whole band to one activity-vs-time column per time step — the series
    /// the client used to reduce for itself — and `nt = 1` collapses a whole window to one
    /// max-hold per frequency cell, which is the survey strip's column. Neither can be had from
    /// the pyramid alone (its coarsest frequency cell is 100 kHz, so no level collapses a
    /// MHz-wide band to one value), and both must be honest about the same thing: **the max of
    /// nothing is unobserved, not zero.** [`OverviewCell::UNOBSERVED`] is the only cell an empty
    /// fold can produce, its `max_db` is `NaN` rather than a floor, and `sources == 0` is the
    /// structural difference between *never looked* and *looked and it was quiet* — the same
    /// distinction [`crate::Coverage::of`] refuses to let be spelled away.
    ///
    /// The output grid is laid on `window`/`freq`, not on the source's outward-snapped grid: output
    /// cell `(t, f)` covers `[window.start + t·Δt, …)` × `[freq.lo_hz + f·Δf, …)` and takes every
    /// source cell that **overlaps** it. A source cell coarser than an output cell therefore
    /// replicates across the output cells it covers — T-334's safe direction (a measured value
    /// repeated), never an interpolated one.
    ///
    /// `nt` and `nf` must be at least 1; `window` must have positive duration.
    pub fn overview(&self, window: TimeRange, freq: FreqRange, nt: usize, nf: usize) -> Overview {
        let (nt, nf) = (nt.max(1), nf.max(1));
        let (t0_ns, t1_ns) = (
            window.start.as_unix_nanos(),
            window
                .end
                .as_unix_nanos()
                .max(window.start.as_unix_nanos() + 1),
        );
        let (f_lo, f_hi) = (freq.lo_hz, freq.hi_hz.max(freq.lo_hz + f64::EPSILON));
        let dt = (t1_ns - t0_ns) as f64 / nt as f64;
        let df = (f_hi - f_lo) / nf as f64;
        let mut cells = vec![OverviewCell::UNOBSERVED; nt * nf];
        // Source cell (t, f) → the output cells it overlaps. Half-open on both axes, so a source
        // boundary landing exactly on an output boundary contributes to the later cell only.
        let span = |lo: f64, hi: f64, origin: f64, step: f64, n: usize| -> (usize, usize) {
            let a = ((lo - origin) / step).floor().max(0.0) as usize;
            let b = (((hi - origin) / step).ceil().max(0.0) as usize).min(n);
            (a.min(n), b.max(a.min(n)))
        };
        // Fraction of output cell `i` (of `n` cells of width `step` from `origin`) that
        // `[lo, hi)` covers. Source cells are disjoint, so these sum to at most 1 per output cell:
        // that is what makes the weighted sum an observed fraction of the output cell's own extent
        // rather than a mean over whichever source cells happened to touch it (T-419).
        let weight = |lo: f64, hi: f64, origin: f64, step: f64, i: usize| -> f64 {
            let (a, b) = (origin + i as f64 * step, origin + (i + 1) as f64 * step);
            ((hi.min(b) - lo.max(a)).max(0.0) / step).min(1.0)
        };
        for t in 0..self.nt {
            let cell_t0 = (self.t_first_cell + t as i64) * self.t_cell_ns;
            let (s_t0, s_t1) = (
                (cell_t0 - t0_ns) as f64,
                (cell_t0 + self.t_cell_ns - t0_ns) as f64,
            );
            let (ta, tb) = span(s_t0, s_t1, 0.0, dt, nt);
            if ta >= tb {
                continue;
            }
            for f in 0..self.nf {
                let src = self.cell(t, f);
                if !src.observed() {
                    continue;
                }
                let cell_f0 = (self.f_first_cell + f as i64) as f64 * self.f_cell_hz;
                let (fa, fb) = span(cell_f0, cell_f0 + self.f_cell_hz, f_lo, df, nf);
                for ot in ta..tb {
                    let wt = weight(s_t0, s_t1, 0.0, dt, ot);
                    if wt <= 0.0 {
                        continue;
                    }
                    for of in fa..fb {
                        let w = wt * weight(cell_f0, cell_f0 + self.f_cell_hz, f_lo, df, of);
                        if w <= 0.0 {
                            continue;
                        }
                        cells[ot * nf + of].fold(src, w as f32);
                    }
                }
            }
        }
        let (mut lo_db, mut hi_db) = (f32::INFINITY, f32::NEG_INFINITY);
        let mut observed_cells = 0;
        for c in &mut cells {
            if c.sources == 0 {
                continue;
            }
            observed_cells += 1;
            // Already an observed fraction of this cell's own extent: the weights were the divisor.
            // Clamped only against float error in the weights, never to rescue an over-claim.
            c.coverage = c.coverage.min(1.0);
            lo_db = lo_db.min(c.max_db);
            hi_db = hi_db.max(c.max_db);
        }
        Overview {
            unit: self.unit,
            nt,
            nf,
            t0_ns,
            t_cell_ns: dt,
            f_lo_hz: f_lo,
            f_cell_hz: df,
            cells,
            observed_cells,
            src_nt: self.nt,
            src_nf: self.nf,
            range_db: (observed_cells > 0).then_some((lo_db, hi_db)),
        }
    }

    /// Folds this grid into a carry-forward search (T-519): for each of `out`'s columns, the
    /// **newest** observed cell here that ends at or before `out.before_ns` — last-write-wins by
    /// time. Returns the columns this grid gave a value that had none.
    ///
    /// This is [`Self::overview`]'s sibling and folds by the same geometry (a source cell reaches
    /// every output column it overlaps; a coarser source replicates, T-334's safe direction), but
    /// keeps a different value: `overview` keeps the *strongest* in an extent, this keeps the
    /// *latest*. Newer beats older by the source cell's end; cells ending together fold by
    /// max-hold, exactly as `overview` would. So one grid, or several folded in any order, give the
    /// same answer. Columns marked in `frozen` are left alone: a search freezes what a newer stage
    /// already found, because a coarse fallback cell's *end* can be later than a fine cell's while
    /// its data is older.
    ///
    /// **Never carries backward.** A cell whose true extent (recomputed from `level_t_cells` at the
    /// level that *answered* it — a coarser fallback can stand in for an evicted tile) ends after
    /// `before_ns` may hold frames from after it. It is used only when `guard` proves it cannot:
    /// its end is within what the guard knows and no output column it reaches was observed between
    /// `before_ns` and its end. Otherwise it is skipped — the conservative direction for a value
    /// that would come from the future.
    pub fn fold_newest(
        &self,
        out: &mut LastKnown,
        level_t_cells: &[i64],
        guard: Option<&StraddleGuard>,
        frozen: &[bool],
    ) -> usize {
        self.fold_newest_in(out, level_t_cells, guard, frozen, 0)
    }

    /// [`Self::fold_newest`], deciding "newest" per `quantum_ns` row when that is coarser than a
    /// source cell (T-911, [`Pyramid::last_known_search_at`]): every cell of the newest such row
    /// ties, and ties fold by max-hold.
    fn fold_newest_in(
        &self,
        out: &mut LastKnown,
        level_t_cells: &[i64],
        guard: Option<&StraddleGuard>,
        frozen: &[bool],
        quantum_ns: i64,
    ) -> usize {
        let nf = out.nf;
        let mut found = 0;
        for t in 0..self.nt {
            let row_start = (self.t_first_cell + t as i64) * self.t_cell_ns;
            for f in 0..self.nf {
                let c = self.cell(t, f);
                if !c.observed() {
                    continue;
                }
                let tc = level_t_cells
                    .get(usize::from(c.level))
                    .copied()
                    .unwrap_or(self.t_cell_ns)
                    .max(1)
                    .max(quantum_ns);
                let end = (row_start.div_euclid(tc) + 1) * tc;
                let f0 = (self.f_first_cell + f as i64) as f64 * self.f_cell_hz;
                let (a, b) = cell_span(f0, f0 + self.f_cell_hz, out.f_lo_hz, out.f_cell_hz, nf);
                if a >= b {
                    continue;
                }
                if end > out.before_ns && !guard.is_some_and(|g| g.admits(a, b, end)) {
                    continue;
                }
                let t_ns = end.min(out.before_ns);
                for (of, cur) in out.cells[a..b].iter_mut().enumerate() {
                    if frozen.get(a + of).copied().unwrap_or(false) {
                        continue;
                    }
                    if !cur.found() {
                        found += 1;
                        *cur = LastKnownCell {
                            max_db: c.max_db,
                            t_ns,
                            level: c.level,
                        };
                    } else if t_ns > cur.t_ns {
                        *cur = LastKnownCell {
                            max_db: c.max_db,
                            t_ns,
                            level: c.level,
                        };
                    } else if t_ns == cur.t_ns {
                        cur.max_db = cur.max_db.max(c.max_db);
                    }
                }
            }
        }
        found
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
    /// T-583: an open coarse tile **plus the rows still in flight below it**, folded for this
    /// read into a clone ([`Pyramid::live_preview`]). Owned, and discarded with the answer.
    Preview(Box<Tile>),
    Disk(Box<Tile>),
}

impl Source<'_> {
    fn tile(&self) -> &Tile {
        match self {
            Source::Mem(t, _) => t,
            Source::Preview(t) | Source::Disk(t) => t,
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
                for l in
                    (level..=self.geom.top()).filter(|&l| self.geom.coarsens_or_equals(level, l))
                {
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

    /// The levels a [`Self::last_known`] search walks, finest first: from level 0, each next level
    /// is the one with the smallest time cell that is a whole multiple of the current one and
    /// coarsens it on both axes (ties to the finer frequency cell). On a welded ladder that is the
    /// ladder itself; on a view lattice it is the `level_f = 0` time column.
    fn time_chain(&self) -> Vec<usize> {
        let lv = &self.geom.levels;
        let mut chain = vec![0usize];
        if self.cfg.coarse_on_demand {
            // A lattice's coarse nodes exist only once a read has materialised them, and this
            // search reads without building (it takes `&self`): level 0 is the one level capture
            // always fills, so it is the only one walked, as far back as the budget reaches.
            return chain;
        }
        loop {
            let cur = *chain.last().expect("non-empty");
            let tc = lv[cur].t_cell_ns;
            let next = (0..lv.len())
                .filter(|&l| {
                    lv[l].t_cell_ns > tc
                        && lv[l].t_cell_ns % tc == 0
                        && self.geom.coarsens_or_equals(cur, l)
                })
                .min_by(|&a, &b| {
                    (lv[a].t_cell_ns, lv[a].f_cell_hz)
                        .partial_cmp(&(lv[b].t_cell_ns, lv[b].f_cell_hz))
                        .expect("cell sizes are finite")
                });
            match next {
                Some(l) => chain.push(l),
                None => return chain,
            }
        }
    }

    /// Start of the oldest tile the pyramid holds at any level, open or sealed; `None` when it holds
    /// none. What the search's final stage need not reach past.
    fn oldest_block_ns(&self) -> Option<i64> {
        let mut oldest: Option<i64> = None;
        for (l, g) in self.geom.levels.iter().enumerate() {
            let sealed = self.sealed[l].keys().next().map(|&(tb, _)| tb);
            let open = self.open[l].keys().map(|&(_, tb)| tb).min();
            for tb in sealed.into_iter().chain(open) {
                let t = tb.saturating_mul(g.t_block_ns());
                oldest = Some(oldest.map_or(t, |o| o.min(t)));
            }
        }
        oldest
    }

    /// Whether any tile — open, derived or sealed — at `level` or a level that may stand in for it
    /// ([`Geometry::coarsens_or_equals`], the query's own fallback set) overlaps `freq × [t0, t1)`.
    /// `false` means [`Self::query`] over that box can only answer unobserved cells, so the search
    /// can say so from the tile index without walking them: the empty case, which across 6 GHz is
    /// the common one, costs a key lookup rather than a cell walk (T-461's rule, applied here).
    fn holds_any(&self, level: usize, freq: FreqRange, t0: i64, t1: i64) -> bool {
        let nf = self.geom.nf as i64;
        (0..self.geom.n_levels())
            .filter(|&l| self.geom.coarsens_or_equals(level, l))
            .any(|l| {
                let g = &self.geom.levels[l];
                let fb_lo = ((freq.lo_hz / g.f_cell_hz).floor() as i64).div_euclid(nf);
                let fb_hi = (((freq.hi_hz / g.f_cell_hz).ceil() as i64 - 1).max(0)).div_euclid(nf);
                let blk = g.t_block_ns().max(1);
                let (tb_lo, tb_hi) = (t0.div_euclid(blk), (t1 - 1).max(t0).div_euclid(blk));
                let fits = |fb: i64, tb: i64| {
                    (fb_lo..=fb_hi).contains(&fb) && (tb_lo..=tb_hi).contains(&tb)
                };
                self.sealed[l]
                    .range((tb_lo, i64::MIN)..=(tb_hi, i64::MAX))
                    .any(|(&(tb, fb), _)| fits(fb, tb))
                    || self.open[l].keys().any(|&(fb, tb)| fits(fb, tb))
                    || self
                        .derived
                        .keys()
                        .any(|&(dl, fb, tb)| dl == l && fits(fb, tb))
            })
    }

    /// Starts a carry-forward search (T-519): per column of `nf` over `freq`, the **newest**
    /// observed max-hold at or before `before`, over whatever the pyramid retains.
    ///
    /// # How it stays cheap: newest-first, fine-to-coarse, aligned
    ///
    /// The walk goes **backward** from `before` through [`Self::time_chain`]. Each stage reads one
    /// level over `[align_down(end, next level's cell), end)` and hands everything older to the
    /// next, coarser level — so a fine level only ever covers the newest part-cell of the level
    /// above it, and each coarser stage covers exactly the whole cells the finer one did not. On
    /// scheme 1's ladder that is ≤ 59 one-second rows, ≤ 14 minutes, ≤ 3 quarter-hours, ≤ 23
    /// hours, then days: the whole retained horizon in a few hundred rows per column, whatever its
    /// length. It is also the ladder's seal cadence — a level's cell is complete when the finer
    /// level's tile seals — so each stage reads a level that already holds its window. The search
    /// ends as soon as every column has a value. Nothing is maintained for this and ingest pays
    /// nothing (the T-453 constraint): it reads tiles the pyramid already holds.
    ///
    /// # The bounds, and what they cost
    ///
    /// `total_cells` bounds the whole search. A stage that would exceed what is left of it is not
    /// read, and its window is **merged into the next, coarser stage**, which then starts at its
    /// own cell boundary *at or after* `before`: that top cell straddles `before` and is used only
    /// where `guard` proves the part after `before` is empty (see [`StraddleGuard`]). What cannot be
    /// proven is reported as a skipped window, never as empty. On a very wide region the fine
    /// stages are the expensive ones, so the cost of the bound is time resolution on the newest
    /// part of the past, not reach. Each call to [`Self::last_known_step`] reads at most
    /// `per_step_cells` source cells — a stage wider than that is read in frequency slices, which
    /// fold independently because columns do — so a caller that releases the lock between steps
    /// bounds every hold.
    pub fn last_known_search(
        &self,
        freq: FreqRange,
        before: Timestamp,
        nf: usize,
        guard: Option<StraddleGuard>,
        per_step_cells: usize,
        total_cells: usize,
    ) -> LastKnownSearch {
        let before_ns = before.as_unix_nanos();
        let out = LastKnown::empty(freq, before_ns, nf);
        let guard = guard.filter(|g| g.first_after.len() == out.nf);
        LastKnownSearch {
            out,
            freq,
            guard,
            chain: self.time_chain(),
            oldest_ns: self.oldest_block_ns(),
            next: 0,
            end_ns: before_ns,
            per_step: per_step_cells.max(1),
            remaining: total_cells,
            active: None,
            frozen: Vec::new(),
            done: !(freq.lo_hz.is_finite() && freq.hi_hz > freq.lo_hz),
            pinned: false,
            quantum_ns: 0,
            reach_tb: i64::MIN,
        }
    }

    /// Starts a carry-forward search **pinned to one store level** (T-911): per column of `nf` over
    /// `freq`, the newest observed max-hold at or before `before`, read at `level`'s own cells and
    /// nowhere else.
    ///
    /// # Why a tile needs this before [`Self::last_known_search`]
    ///
    /// The shadow is judged against the band's **last live row**, and that row is a cell of the
    /// tile's own level: the max-hold over exactly that level's time–frequency box. A value found at
    /// any other level is a max-hold over a *different* box — measured on the mock SDR, the
    /// spectrum-history pyramid's 6.25 kHz × 1 s cell read a departed FM band's noise floor 10–15 dB
    /// hotter than the 586 Hz × 40 ms cells the pane had just drawn it with, so the shadow changed
    /// colour at the first tile boundary after the band was left. Read at the tile's own level,
    /// the carried value **is** the last live row's cell, whatever the fold.
    ///
    /// # How it reaches without paying for the gap
    ///
    /// Newest-first, one block of `level` at a time: each stage jumps straight to the newest block
    /// at or before its end that holds **any** tile over `freq` — a range scan of the tile index
    /// bounded to the last [`PINNED_REACH_BLOCKS`] blocks, never a cell walk — so the time a
    /// departed band spent unobserved costs no cell read to cross (T-461's rule, applied to the
    /// search). Nothing older than that reach is searched here: it is the ladder's, which is
    /// time-deep. The scan runs under the store's lock, and an unbounded one walked every older
    /// sealed tile of the level across all frequencies on a miss. Each stage reads that block's newest part the remaining budget affords; a
    /// block it cannot afford even one row of is reported unsearched and ends the search. The search
    /// ends when every column has a value, the budget is spent, or nothing older is held. No
    /// straddle arises: every stage ends at or before `before`, which callers align to the level's
    /// time cell (a tile's start always is).
    ///
    /// # When the level is finer than the output grid
    ///
    /// A tile may be answered by folding a finer level into its cells (max-hold over each cell's
    /// box). `out_t_cell_ns` is the output grid's time cell: when it is a whole multiple of the
    /// level's, "newest" is decided per **output row** — every source cell inside the newest output
    /// row folds by max-hold, exactly as the tile's own cell did — and every stage is aligned to
    /// the output row, so none is split. Otherwise the level's own cell decides, as in
    /// [`Self::last_known_search`].
    #[allow(clippy::too_many_arguments)]
    pub fn last_known_search_at(
        &self,
        level: usize,
        freq: FreqRange,
        before: Timestamp,
        nf: usize,
        out_t_cell_ns: i64,
        per_step_cells: usize,
        total_cells: usize,
    ) -> LastKnownSearch {
        let mut s = self.last_known_search(freq, before, nf, None, per_step_cells, total_cells);
        s.chain = vec![level];
        s.pinned = true;
        match self.geom.levels.get(level) {
            Some(g) => {
                let cell = g.t_cell_ns.max(1);
                let blk = g.t_block_ns().max(1);
                s.reach_tb = before
                    .as_unix_nanos()
                    .div_euclid(blk)
                    .saturating_sub(PINNED_REACH_BLOCKS);
                if out_t_cell_ns > cell && out_t_cell_ns % cell == 0 {
                    s.quantum_ns = out_t_cell_ns;
                }
            }
            None => s.done = true,
        }
        s
    }

    /// The newest time block of `level` whose start lies before `end_ns` and that holds any tile
    /// — open, derived or sealed — over `freq`, no older than block `min_tb`; `None` when there is
    /// none. Reads the tile index only, over `[min_tb, end)` of the sealed map.
    fn newest_block_before(
        &self,
        level: usize,
        freq: FreqRange,
        end_ns: i64,
        min_tb: i64,
    ) -> Option<i64> {
        let g = &self.geom.levels[level];
        let nf = self.geom.nf as i64;
        let fb_lo = ((freq.lo_hz / g.f_cell_hz).floor() as i64).div_euclid(nf);
        let fb_hi = (((freq.hi_hz / g.f_cell_hz).ceil() as i64 - 1).max(0)).div_euclid(nf);
        let fits = |fb: i64| (fb_lo..=fb_hi).contains(&fb);
        let blk = g.t_block_ns().max(1);
        let tb_max = (end_ns - 1).div_euclid(blk);
        if tb_max < min_tb {
            return None;
        }
        let sealed = self.sealed[level]
            .range((min_tb, i64::MIN)..=(tb_max, i64::MAX))
            .rev()
            .find(|((_, fb), _)| fits(*fb))
            .map(|((tb, _), _)| *tb);
        let open = self.open[level]
            .keys()
            .filter(|&&(fb, tb)| fits(fb) && (min_tb..=tb_max).contains(&tb))
            .map(|&(_, tb)| tb)
            .max();
        let derived = self
            .derived
            .keys()
            .filter(|&&(dl, fb, tb)| dl == level && fits(fb) && (min_tb..=tb_max).contains(&tb))
            .map(|&(_, _, tb)| tb)
            .max();
        [sealed, open, derived].into_iter().flatten().max()
    }

    /// [`Self::last_known_plan`] for a search pinned to one level ([`Self::last_known_search_at`]).
    fn last_known_plan_pinned(&self, s: &mut LastKnownSearch) -> bool {
        while s.active.is_none() && !s.done {
            let l = s.chain[0];
            let g = self.geom.levels[l];
            let cell = g.t_cell_ns.max(1);
            // The row every bound is aligned to: the output row when it is coarser (see
            // `quantum_ns`), else the level's own cell.
            let q = s.quantum_ns.max(cell);
            let blk = g.t_block_ns().max(1);
            let Some(tb) = self.newest_block_before(l, s.freq, s.end_ns, s.reach_tb) else {
                // Nothing older is held at this level within the reach: the search is complete,
                // and anything older is the ladder's.
                s.done = true;
                break;
            };
            let b0 = tb.saturating_mul(blk);
            // Both bounds on the output row grid, so no stage splits a row. The top is the row
            // holding the block's end rounded UP — the row the tile drew the block's last cells in;
            // nothing newer than the block is held over `freq`, so the part of that row past the
            // block reads nothing extra — but never past the row holding `end`. A part-row before
            // an unaligned `end` cannot be read without folding frames from after it, and is
            // reported unsearched rather than empty.
            let raw = s.end_ns.min(b0.saturating_add(blk));
            let hi = raw
                .div_euclid(q)
                .saturating_add(i64::from(raw.rem_euclid(q) != 0))
                .saturating_mul(q)
                .min(s.end_ns.div_euclid(q) * q);
            let floor = b0.div_euclid(q) * q;
            if raw > hi {
                s.out.stages.push(LastKnownStage {
                    level: l as u8,
                    from_ns: hi,
                    to_ns: raw,
                    source_cells: 0,
                    found: 0,
                    skipped: true,
                });
            }
            if hi <= floor {
                s.end_ns = floor;
                continue;
            }
            let (c_lo, c_hi) = (
                (s.freq.lo_hz / g.f_cell_hz).floor() as i64,
                ((s.freq.hi_hz / g.f_cell_hz).ceil() as i64)
                    .max((s.freq.lo_hz / g.f_cell_hz).floor() as i64 + 1),
            );
            let cols = (c_hi - c_lo) as usize;
            // Whole output rows only: a partly-read row would fold a max over part of its box.
            let rows = (s.remaining / cols.max(1)) as i64;
            let span = rows.saturating_mul(cell).div_euclid(q) * q;
            if span == 0 {
                s.out.stages.push(LastKnownStage {
                    level: l as u8,
                    from_ns: floor,
                    to_ns: hi,
                    source_cells: 0,
                    found: 0,
                    skipped: true,
                });
                s.done = true;
                break;
            }
            let lo = hi.saturating_sub(span).max(floor);
            let n_rows = ((hi - lo) / cell) as usize;
            let slice_cols = (s.per_step / n_rows.max(1)).max(1) as i64;
            s.frozen = s.out.cells.iter().map(LastKnownCell::found).collect();
            s.active = Some(ActiveStage {
                level: l,
                lo,
                hi,
                c_hi,
                slice_cols,
                c_next: c_lo,
                stage: s.out.stages.len(),
            });
            s.out.stages.push(LastKnownStage {
                level: l as u8,
                from_ns: lo,
                to_ns: hi,
                source_cells: 0,
                found: 0,
                skipped: false,
            });
            s.end_ns = lo;
        }
        s.active.is_some()
    }

    /// Plans the next stage of `s`, or finishes it. Returns whether a stage is active.
    fn last_known_plan(&self, s: &mut LastKnownSearch) -> bool {
        if s.pinned {
            return self.last_known_plan_pinned(s);
        }
        while s.active.is_none() && !s.done {
            let Some(&l) = s.chain.get(s.next) else {
                s.done = true;
                break;
            };
            let Some(oldest) = s.oldest_ns.filter(|&o| o < s.end_ns) else {
                // Nothing older than `end` is held at any level: the search is complete, not cut.
                s.done = true;
                break;
            };
            let last = s.next + 1 == s.chain.len();
            s.next += 1;
            let g = self.geom.levels[l];
            let cell = g.t_cell_ns.max(1);
            // With a guard the top cell may straddle `end` (it is admitted per column); without
            // one it may not, and the part-cell is reported unsearched.
            let hi = if s.guard.is_some() {
                (s.end_ns + cell - 1).div_euclid(cell) * cell
            } else {
                s.end_ns.div_euclid(cell) * cell
            };
            let (c_lo, c_hi) = (
                (s.freq.lo_hz / g.f_cell_hz).floor() as i64,
                ((s.freq.hi_hz / g.f_cell_hz).ceil() as i64)
                    .max((s.freq.lo_hz / g.f_cell_hz).floor() as i64 + 1),
            );
            let cols = (c_hi - c_lo) as usize;
            let lo = if last {
                let rows = (s.remaining / cols).min(LAST_KNOWN_MAX_TOP_ROWS);
                hi.saturating_sub(rows as i64 * cell)
                    .max(oldest.div_euclid(cell) * cell)
            } else {
                let next_cell = self.geom.levels[s.chain[s.next]].t_cell_ns.max(1);
                s.end_ns.div_euclid(next_cell) * next_cell
            }
            .max(0);
            if hi <= lo {
                // No whole cell of this level here; the next stage covers the window.
                s.done |= last;
                continue;
            }
            let rows = ((hi - lo) / cell) as usize;
            let cost = rows.saturating_mul(cols);
            if cost > s.remaining {
                if last {
                    s.out.stages.push(LastKnownStage {
                        level: l as u8,
                        from_ns: lo,
                        to_ns: hi.min(s.end_ns),
                        source_cells: 0,
                        found: 0,
                        skipped: true,
                    });
                    s.done = true;
                }
                // Not last: `end` stays, so the next (coarser) stage takes this window.
                continue;
            }
            let straddle_unprovable = s
                .guard
                .as_ref()
                .is_some_and(|g| hi > s.end_ns && hi > g.known_until_ns);
            // A straddling top cell of a level folded from a finer one is complete only once the
            // finer tiles under it have sealed. Past the watermark it may hold part of its window
            // or none of it, so that part is reported unsearched rather than read as empty.
            let unfolded =
                hi > s.end_ns && g.from.is_some() && hi > self.watermark().as_unix_nanos();
            if straddle_unprovable || unfolded {
                // The top cell cannot be admitted (it reaches past what the guard knows) or may not
                // hold its window yet: its part before `end` is unsearched, and says so.
                s.out.stages.push(LastKnownStage {
                    level: l as u8,
                    from_ns: hi - cell,
                    to_ns: s.end_ns,
                    source_cells: 0,
                    found: 0,
                    skipped: true,
                });
            }
            if hi.min(s.end_ns) < s.end_ns {
                // Unguarded and unaligned: the newest part-cell cannot be read without folding
                // frames from after `before`.
                s.out.stages.push(LastKnownStage {
                    level: l as u8,
                    from_ns: hi,
                    to_ns: s.end_ns,
                    source_cells: 0,
                    found: 0,
                    skipped: true,
                });
            }
            let slice_cols = (s.per_step / rows.max(1)).max(1) as i64;
            s.frozen = s.out.cells.iter().map(LastKnownCell::found).collect();
            s.active = Some(ActiveStage {
                level: l,
                lo,
                hi,
                c_hi,
                slice_cols,
                c_next: c_lo,
                stage: s.out.stages.len(),
            });
            s.out.stages.push(LastKnownStage {
                level: l as u8,
                from_ns: lo,
                to_ns: hi,
                source_cells: 0,
                found: 0,
                skipped: false,
            });
            s.end_ns = lo;
        }
        s.active.is_some()
    }

    /// Runs the next step of `s`: one frequency slice of one stage (a no-op once it is done).
    pub fn last_known_step(&self, s: &mut LastKnownSearch) -> Result<(), StoreError> {
        if !self.last_known_plan(s) {
            return Ok(());
        }
        let a = s.active.expect("planned");
        let g = self.geom.levels[a.level];
        let c1 = (a.c_next + a.slice_cols).min(a.c_hi);
        let freq = FreqRange::new(
            (a.c_next as f64 * g.f_cell_hz).max(s.freq.lo_hz),
            (c1 as f64 * g.f_cell_hz).min(s.freq.hi_hz),
        );
        let (found, read) = if self.holds_any(a.level, freq, a.lo, a.hi) {
            let h = self.query(&RegionQuery {
                freq,
                time: TimeRange::new(
                    Timestamp::from_unix_nanos(a.lo),
                    Timestamp::from_unix_nanos(a.hi),
                ),
                resolution: Resolution::Level(a.level as u8),
            })?;
            let t_cells: Vec<i64> = self.geom.levels.iter().map(|g| g.t_cell_ns).collect();
            let found = h.fold_newest_in(
                &mut s.out,
                &t_cells,
                s.guard.as_ref(),
                &s.frozen,
                s.quantum_ns,
            );
            (found, h.nt * h.nf)
        } else {
            (0, 0)
        };
        s.remaining = s.remaining.saturating_sub(read);
        s.out.source_cells += read;
        s.out.searched_from_ns = s.out.searched_from_ns.min(a.lo);
        let st = &mut s.out.stages[a.stage];
        st.source_cells += read;
        st.found += found;
        if c1 >= a.c_hi {
            s.active = None;
            if s.out.cells.iter().all(LastKnownCell::found) {
                s.done = true;
            }
        } else {
            s.active = Some(ActiveStage { c_next: c1, ..a });
        }
        Ok(())
    }

    /// [`Self::last_known_search`] run to completion under one borrow.
    pub fn last_known(
        &self,
        freq: FreqRange,
        before: Timestamp,
        nf: usize,
        guard: Option<StraddleGuard>,
        per_step_cells: usize,
        total_cells: usize,
    ) -> Result<LastKnown, StoreError> {
        let mut s = self.last_known_search(freq, before, nf, guard, per_step_cells, total_cells);
        while !s.done() {
            self.last_known_step(&mut s)?;
        }
        Ok(s.finish())
    }

    fn load_source(
        &self,
        level: usize,
        fb: i64,
        tb: i64,
        margin: f32,
        pct: (f32, f32),
    ) -> Result<Option<Source<'_>>, StoreError> {
        // T-571: every tile this read consults, wherever it lives. A coarse tile of a live
        // lattice costs ONE of these; the read-time fold it replaced cost one per producer tile,
        // up to `MAX_MATERIALIZE_TILES` of them for a single address.
        self.count_source_tile();
        // T-583: a coarse node's in-progress row, which the live cascade only propagates on
        // commit. Folded here, on the read, so the capture thread pays nothing for it; `None`
        // whenever nothing is in flight below this address, which is every read of elapsed time.
        if let Some(t) = self.live_preview(level, fb, tb, margin, pct) {
            return Ok(Some(Source::Preview(t)));
        }
        if let Some(t) = self.open[level].get(&(fb, tb)) {
            return Ok(Some(match t {
                super::store::OpenTile::Full(t) => {
                    let preview = if level == 0 {
                        t.column_preview(margin, pct)
                    } else {
                        None
                    };
                    Source::Mem(t, preview)
                }
                // T-585: a live coarse node holds its committed rows encoded and its in-progress
                // row as accumulator; a read materialises both into one owned tile — still ONE
                // source tile — so the in-progress row (T-583) is visible exactly as before.
                super::store::OpenTile::Live(l) => {
                    let g = &self.geom.levels[level];
                    let mut tile =
                        Tile::new(l.key, self.geom.nf, g, usize::from(self.cfg.histogram.bins));
                    l.materialize_into(&mut tile, g);
                    Source::Disk(Box::new(tile))
                }
            }));
        }
        // T-453: a live-edge coarse summary built on demand by `Pyramid::materialize`. Level 0 is
        // never derived — it is capture's own product — and a derived tile carries no open column,
        // so there is no preview to take.
        if level > 0
            && let Some(t) = self.derived.get(&(level, fb, tb))
        {
            return Ok(Some(Source::Mem(t, None)));
        }
        Ok(self
            .read_sealed(level, fb, tb)?
            .map(|t| Source::Disk(Box::new(t))))
    }
}

/// Weight quantum of a mixture signature: weights are value fractions rounded to 1/1024.
const MIXTURE_WEIGHT_LEVELS: f64 = 1024.0;

/// A tile's mixture signature: `(shape bits, quantised weight)` for each shape with weight > 0.
type MixtureSignature = Vec<(u32, u16)>;

/// Floor bias per `(shape bits, frames)` (`frames` = 0 for rolled-up cells), and per mixture
/// signature and `frames` (T-141).
#[derive(Default)]
struct BiasCache {
    single: HashMap<(u32, u32), f32>,
    mixture: HashMap<MixtureSignature, HashMap<u32, f32>>,
    sig: MixtureSignature,
}

impl BiasCache {
    /// T-141: the floor bias of a cell pooling values of the `(shape, values)` mixture. The
    /// weights are quantised to 1/1024 before solving, so a signature's cached bias does not
    /// depend on which tile computed it first.
    fn mixture_bias_db(
        &mut self,
        mixture: &[(f32, u64, u64)],
        level: usize,
        frames: u32,
        q: f32,
    ) -> f32 {
        // Saturating: a corrupt tile's counts must not overflow (weights then merely skew).
        let total = mixture
            .iter()
            .fold(0u64, |acc, &(_, n, _)| acc.saturating_add(n))
            .max(1);
        self.sig.clear();
        for &(shape, n, _) in mixture {
            let w = (n as f64 / total as f64 * MIXTURE_WEIGHT_LEVELS).round() as u16;
            if w > 0 {
                self.sig.push((shape.to_bits(), w));
            }
        }
        let n = if level == 0 { frames } else { 0 };
        if let Some(&b) = self.mixture.get(&self.sig[..]).and_then(|m| m.get(&n)) {
            return b;
        }
        let p = if level == 0 {
            hk_dsp::radiometry::exact_percentile_probability(f64::from(q), frames)
        } else {
            f64::from(q) / 100.0
        };
        let comps: Vec<(f64, f64)> = self
            .sig
            .iter()
            .map(|&(s, w)| (f64::from(f32::from_bits(s)), f64::from(w)))
            .collect();
        let b = hk_dsp::radiometry::mixture_percentile_bias_db(&comps, p) as f32;
        self.mixture
            .entry(self.sig.clone())
            .or_default()
            .insert(n, b);
        b
    }

    fn bias_db(&mut self, shape: f32, level: usize, frames: u32, q: f32) -> f32 {
        let n = if level == 0 { frames } else { 0 };
        *self.single.entry((shape.to_bits(), n)).or_insert_with(|| {
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
    // A uniform tile corrects with its shape's bias (unchanged since T-116). T-141: a mixed-shape
    // tile corrects with the bias of the Gamma mixture its level-0 values pooled, weighted by the
    // values folded per shape over the whole tile (the pooled sample a cell's frames are drawn
    // from), only when every shape's frames covered equally many cells; a mixed tile without a
    // valid recorded mixture (format < 4, or coverage differing by shape) gives no floor.
    let floor_db = if p_lo.is_finite() {
        match (
            tile.prov.uniform_cell_shape(),
            tile.prov.cell_shape_mixture(),
        ) {
            (Some(shape), _) => {
                round_centi(round_centi(p_lo) - bias.bias_db(shape, level, n, q_low))
            }
            (None, Some(mix)) => {
                round_centi(round_centi(p_lo) - bias.mixture_bias_db(mix, level, n, q_low))
            }
            (None, None) => f32::NAN,
        }
    } else {
        f32::NAN
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
