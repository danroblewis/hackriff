//! Pyramid configuration and the derived per-level geometry.

use std::time::Duration;

use hk_model::{FreqRange, PowerUnit};

use super::StoreError;

/// Most levels a scheme may have.
///
/// Raised from 8 to 64 by T-434: a **ladder** needs a handful of levels, but a de-welded
/// **lattice** (§8.2 of `docs/16`) needs one node per `(level_f, level_t)` pair, so an 8 × 8 span
/// of the two axes is 64 nodes. The ceiling still exists — [`hk_model::TileKey::level`] is a `u8`
/// and every per-level index in the store is a `Vec` sized once at open.
pub const MAX_LEVELS: usize = 64;

/// One level of the pyramid.
///
/// A level is produced by folding **one** finer level ([`Self::from`]) by a frequency factor and a
/// time factor. A level may be folded by more than one coarser level, so the levels form a DAG:
/// a plain ladder is the special case where every level has exactly one consumer.
///
/// **The two factors are independent (T-434).** Before, the next level's time cell was forced to be
/// one whole tile of the level below, which welded the axes at the ratio
/// `t_cells_per_block : 1` — across scheme 1's ladder frequency coarsens ×16 while time coarsens
/// ×86 400 (`docs/16` §5.2 correction 2). [`Self::t_factor`] de-welds them, which is what lets a
/// scheme address `level_f` and `level_t` as separate coordinates.
#[derive(Clone, Debug, PartialEq)]
pub struct LevelConfig {
    /// The finer level this one is folded from. `None` means the level below (`i − 1`), which is
    /// what a plain ladder wants; level 0 has no producer and is fed by ingest. A named producer
    /// must have a **lower index**, so one pass over the levels in index order always seals a
    /// producer before its consumers.
    pub from: Option<usize>,
    /// Frequency-cell width relative to [`Self::from`] (must be 1 for level 0).
    pub f_factor: u32,
    /// Time-cell width relative to [`Self::from`]. `None` is the historical weld — one whole tile
    /// of the producer, i.e. the producer's `t_cells_per_block` — so an existing ladder is
    /// expressed unchanged and no stored tile moves.
    ///
    /// **A value other than the weld costs the percentiles.** A tile's histogram is kept per
    /// frequency cell over the *whole tile*, which is exactly the parent cell's histogram only
    /// when a child tile is one parent time cell. De-weld and it is no longer: the fold then
    /// leaves `p_low`/`p_high` **unknown** rather than inventing a distribution it cannot see
    /// (see [`super::tile::Tile::fold_child`]).
    pub t_factor: Option<u32>,
    /// Time cells per tile.
    pub t_cells_per_block: u32,
    /// Optional retention age: sealed tiles whose end is older than `watermark − max_age` are
    /// evicted (still only once **every** coarser level folded from it covers them).
    pub max_age: Option<Duration>,
    /// Optional byte quota for this level's sealed tiles (T-116): the oldest evictable tiles of
    /// the level go first; tiles protected by a [`RetentionOverride`] are never evicted by quota.
    pub byte_quota: Option<u64>,
}

impl Default for LevelConfig {
    /// The level below, welded in time, ×2 in frequency, 60 time cells per tile, kept forever.
    fn default() -> Self {
        Self {
            from: None,
            f_factor: 2,
            t_factor: None,
            t_cells_per_block: 60,
            max_age: None,
            byte_quota: None,
        }
    }
}

/// A per-region retention override (T-116), e.g. "keep 433 MHz at 1 s for 90 days": sealed tiles
/// of `level` whose frequency block overlaps `freq` are kept for `max_age` instead of the level's
/// own age, and are exempt from the level's byte quota. The global
/// [`PyramidConfig::byte_budget`] remains a hard ceiling: protected tiles are evicted by it only
/// after every unprotected candidate.
///
/// Granularity is one frequency cell (T-126): a tile overlapping the region is protected as a file
/// (quota-exempt, budget-last), but each of its cells keeps the age of the overrides overlapping
/// **that cell**, else the level's own age. Once unprotected cells pass their age the tile is
/// trimmed — rewritten with those cells cleared — so a narrow override does not keep a whole
/// 6.4 MHz default-scheme block's unrelated bins. Chosen over a finer protection index because the
/// tile stays the storage unit (no split files, no new lookup path); the cost is one tile rewrite
/// per trim deadline.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RetentionOverride {
    /// Protected frequency region.
    pub freq: FreqRange,
    /// Level the override applies to.
    pub level: u8,
    /// Retention age of protected tiles.
    pub max_age: Duration,
}

/// Fixed-bin dB histogram used for mergeable percentiles.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HistogramConfig {
    /// Lower edge of bin 0, dB. Values below clamp into bin 0.
    pub lo_db: f32,
    /// Bin width, dB. Merged percentiles are within one step of the pooled sample percentile.
    pub step_db: f32,
    /// Number of bins. Values above the top edge clamp into the last bin.
    pub bins: u16,
}

impl HistogramConfig {
    /// Bin index of `v_db` (clamped).
    #[inline]
    pub fn bin(&self, v_db: f32) -> usize {
        let b = ((v_db - self.lo_db) / self.step_db).floor();
        if b.is_nan() || b < 0.0 {
            0
        } else {
            (b as usize).min(self.bins as usize - 1)
        }
    }

    /// Upper edge of the last bin, dB.
    pub fn hi_db(&self) -> f32 {
        self.lo_db + self.step_db * f32::from(self.bins)
    }
}

/// Pyramid settings. [`PyramidConfig::default`] is the documented default scheme.
///
/// The stored quantity is **power spectral density in dB/Hz** of `unit` (dBFS/Hz, or dBm/Hz when
/// calibrated). Density is intensive, so frames with different bin widths regrid onto one grid
/// without rescaling.
#[derive(Clone, Debug, PartialEq)]
pub struct PyramidConfig {
    /// Scheme/version id carried by every [`hk_model::TileKey`] and tile header. Change it whenever
    /// the geometry or regrid rules change; tiles are stored under a per-scheme directory and a
    /// tile whose header geometry disagrees with this config is refused.
    pub scheme: u16,
    /// Unit of the stored densities. Frames in another unit are rejected.
    pub unit: PowerUnit,
    /// Level-0 frequency-cell width, Hz. Cell `c` covers `[c·w, (c+1)·w)`.
    pub f_cell_hz: f64,
    /// Level-0 time-cell duration. Cell `k` covers `[k·d, (k+1)·d)` from the Unix epoch.
    pub t_cell: Duration,
    /// Frequency cells per tile, at every level.
    pub f_cells_per_block: u32,
    /// The level ladder, finest first.
    pub levels: Vec<LevelConfig>,
    /// Percentile histogram.
    pub histogram: HistogramConfig,
    /// Low percentile kept per cell (noise floor; docs/07 p10).
    pub low_percentile: f32,
    /// High percentile kept per cell.
    pub high_percentile: f32,
    /// Occupancy threshold = floor + this margin, dB (ITU guard ≥ 3–5 dB; docs/04 §3.9).
    pub occupancy_margin_db: f32,
    /// Default floor when the caller supplies none: the minimum of the per-cell low percentile over
    /// this many recent level-0 tiles.
    pub floor_memory_tiles: usize,
    /// Tiles seal when the newest frame end is this far past their end (tolerates small disorder).
    pub seal_lag: Duration,
    /// How often open level-0 tiles are checkpointed to disk (bounds loss on a crash). `None`:
    /// only on [`super::Pyramid::checkpoint`] / [`super::Pyramid::close`].
    pub checkpoint_interval: Option<Duration>,
    /// Rolling byte budget for all tile files of this scheme.
    pub byte_budget: u64,
    /// Per-region retention overrides (T-116).
    pub retention_overrides: Vec<RetentionOverride>,
    /// zstd level for tile payloads (T-116, format 2); `None` stores payloads uncompressed.
    pub compression_level: Option<i32>,
    /// **Coarse nodes are produced on demand, not at every seal** (`docs/16` §5.2, T-453).
    ///
    /// `false` (a ladder): every seal folds the sealed tile into its consumers immediately, so the
    /// whole ladder is a by-product of capture. That is right for scheme 1, whose coarser levels
    /// step ×60, ×15, ×4, ×24 in time and so seal almost nothing — the whole ladder totals ≈1.02×
    /// level 0's tile writes.
    ///
    /// `true` (a de-welded lattice): capture writes **only the levels nothing produces** — node
    /// (0, 0) — and every coarser node is produced when a read asks for it, by
    /// [`super::Pyramid::materialize`]. A ×2 lattice has one tile series per time level and one per
    /// frequency level, so eager folding costs ≈4× a ladder's tile writes *per second of capture*
    /// (`docs/16` §6.4a) — and on a narrow capture it is worse than that, because a
    /// frequency-coarser node's tile seals on the same watermark as its producer's however few
    /// frequency blocks the capture actually spans. **On demand is what §5.2 decided, and what
    /// makes the lattice's node count a reach decision rather than a gate-time one.**
    pub coarse_on_demand: bool,
    /// **Coarse nodes are maintained LIVE and INCREMENTALLY, per downsample interval** (T-571,
    /// CLAUDE.md "Live rendering, tile maintenance and playback"). Mutually exclusive with
    /// [`PyramidConfig::coarse_on_demand`].
    ///
    /// Each node keeps its **in-progress top row** and adjusts it as producer rows arrive,
    /// committing every `t_factor` of them — the same rule at 2, 4, 8, 16, 32 — so a coarse tile
    /// is an ordinary already-committed tile by the time anything reads it. Per arriving row the
    /// work is O(1) per level and the residency is one `(row, f_lo, f_hi)` per open tile, not a
    /// second accumulator per node.
    ///
    /// This replaces the read-time fold for a de-welded lattice. On demand, one coarse tile was
    /// folded out of up to [`super::MAX_MATERIALIZE_TILES`] producer tiles **while the reader
    /// waited**, and the live edge was gated on that batch work; that is exactly the
    /// "batch materialize-on-demand" the invariant forbids.
    ///
    /// Requires every producing level to coarsen **one** axis (`f_factor == 1 || t_factor == 1`)
    /// and not to be welded — see [`super::tile::Tile::fold_row`] for why, and
    /// [`ViewLattice::levels`] for the shape that satisfies it by construction.
    pub coarse_live: bool,
}

impl Default for PyramidConfig {
    /// Scheme 1: 6.25 kHz × 1 s cells at level 0 (half the narrowest common 12.5 kHz channel
    /// raster), 1024 frequency cells per tile, and the ladder
    ///
    /// | Level | Cell | Tile (time) | Tile (freq) |
    /// |---|---|---|---|
    /// | 0 | 6.25 kHz × 1 s | 1 min | 6.4 MHz |
    /// | 1 | 12.5 kHz × 1 min | 15 min | 12.8 MHz |
    /// | 2 | 25 kHz × 15 min | 1 h | 25.6 MHz |
    /// | 3 | 50 kHz × 1 h | 1 day | 51.2 MHz |
    /// | 4 | 100 kHz × 1 day | 1 week | 102.4 MHz |
    ///
    /// Histogram −200…+20 dB in 0.5 dB bins; p10/p90; 6 dB occupancy margin; 8 GiB budget; no
    /// per-level ages, quotas or region overrides; zstd level 3 payloads.
    fn default() -> Self {
        let level = |f_factor, t_cells_per_block| LevelConfig {
            f_factor,
            t_cells_per_block,
            ..LevelConfig::default()
        };
        Self {
            scheme: 1,
            unit: PowerUnit::Dbfs,
            f_cell_hz: 6250.0,
            t_cell: Duration::from_secs(1),
            f_cells_per_block: 1024,
            levels: vec![
                level(1, 60),
                level(2, 15),
                level(2, 4),
                level(2, 24),
                level(2, 7),
            ],
            histogram: HistogramConfig {
                lo_db: -200.0,
                step_db: 0.5,
                bins: 440,
            },
            low_percentile: 10.0,
            high_percentile: 90.0,
            occupancy_margin_db: 6.0,
            floor_memory_tiles: 10,
            seal_lag: Duration::from_secs(2),
            checkpoint_interval: Some(Duration::from_secs(60)),
            byte_budget: 8 << 30,
            retention_overrides: Vec::new(),
            compression_level: Some(3),
            coarse_on_demand: false,
            coarse_live: false,
        }
    }
}

/// Derived geometry of one level.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LevelGeometry {
    /// Frequency-cell width, Hz.
    pub f_cell_hz: f64,
    /// Time-cell duration, ns.
    pub t_cell_ns: i64,
    /// Time cells per tile.
    pub nt: usize,
    /// Frequency factor relative to [`Self::from`].
    pub f_factor: u32,
    /// Time factor relative to [`Self::from`] (1 for level 0).
    pub t_factor: u32,
    /// The finer level this one is folded from; `None` for level 0, which ingest feeds.
    pub from: Option<usize>,
}

impl LevelGeometry {
    /// Tile duration, ns.
    pub fn t_block_ns(&self) -> i64 {
        self.t_cell_ns * self.nt as i64
    }

    /// Tile width, Hz, for `nf` cells per tile.
    pub fn f_block_hz(&self, nf: usize) -> f64 {
        self.f_cell_hz * nf as f64
    }
}

/// Geometry of every level.
#[derive(Clone, Debug, PartialEq)]
pub struct Geometry {
    /// Frequency cells per tile.
    pub nf: usize,
    /// Per level, finest first; every level's producer has a lower index than the level itself.
    pub levels: Vec<LevelGeometry>,
    /// Per level, the coarser levels folded **from** it, in index order. A ladder has one entry
    /// per level and an empty list at the top; a lattice node may feed two (one per axis).
    consumers: Vec<Vec<usize>>,
}

impl Geometry {
    /// Number of levels.
    pub fn n_levels(&self) -> usize {
        self.levels.len()
    }

    /// Top (coarsest) level index — the last, which the ordering rule makes a level no other
    /// level is folded from.
    pub fn top(&self) -> usize {
        self.levels.len() - 1
    }

    /// The coarser levels folded from `level`. Empty for a level nothing consumes, which is the
    /// only kind of level the byte budget may evict without a coverage check.
    pub fn consumers(&self, level: usize) -> &[usize] {
        &self.consumers[level]
    }

    /// The distinct frequency-cell widths across the levels, finest first: the **`level_f` axis**.
    pub fn f_axis(&self) -> Vec<f64> {
        let mut v: Vec<f64> = self.levels.iter().map(|l| l.f_cell_hz).collect();
        v.sort_by(|a, b| a.partial_cmp(b).expect("cell widths are finite"));
        v.dedup();
        v
    }

    /// The distinct time-cell durations across the levels, finest first: the **`level_t` axis**.
    pub fn t_axis(&self) -> Vec<i64> {
        let mut v: Vec<i64> = self.levels.iter().map(|l| l.t_cell_ns).collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    /// The `(level_f, level_t)` coordinates of `level` — its rank on each axis independently.
    ///
    /// This is the de-welded addressing (`docs/16` §8.2) read off the geometry rather than carried
    /// beside it, so it is defined for **every** scheme. A welded ladder comes out as the
    /// **diagonal** `(n, n)`, which is the honest statement of what a ladder is: a lattice you can
    /// only move through by coarsening both axes at once.
    pub fn axes_of(&self, level: usize) -> (usize, usize) {
        let g = &self.levels[level];
        let f = self
            .f_axis()
            .iter()
            .position(|&w| w == g.f_cell_hz)
            .unwrap_or(0);
        let t = self
            .t_axis()
            .iter()
            .position(|&d| d == g.t_cell_ns)
            .unwrap_or(0);
        (f, t)
    }

    /// The level serving `(level_f, level_t)`, or `None` where the scheme has no such node — which
    /// is most of the grid for a ladder, and is the answer a route owes its caller rather than
    /// snapping silently to a level with a different time or frequency cell.
    pub fn level_at(&self, level_f: usize, level_t: usize) -> Option<usize> {
        let (fa, ta) = (self.f_axis(), self.t_axis());
        let (&f_cell, &t_cell) = (fa.get(level_f)?, ta.get(level_t)?);
        (0..self.levels.len())
            .find(|&l| self.levels[l].f_cell_hz == f_cell && self.levels[l].t_cell_ns == t_cell)
    }

    /// `up`'s cells are at least as coarse as `level`'s on **both** axes, so a cell of `up` is a
    /// legitimate stand-in for a missing cell of `level`.
    ///
    /// In a ladder every higher index satisfies this, which is why a fallback could walk the
    /// indexes. In a de-welded lattice it cannot: index order is not a coarseness order — node
    /// (1, 0) outranks (0, 3) in index while being *finer* in time — and filling a missing cell
    /// from it would answer a coarse time question with a fine time cell.
    pub fn coarsens_or_equals(&self, level: usize, up: usize) -> bool {
        let (a, b) = (&self.levels[level], &self.levels[up]);
        b.f_cell_hz >= a.f_cell_hz && b.t_cell_ns >= a.t_cell_ns
    }

    /// Time cells of `up` spanned by one whole tile of `level` (`up` must be a consumer of
    /// `level`). 1 is the welded case — a child tile is exactly one parent time column.
    pub fn parent_cells_per_tile(&self, level: usize, up: usize) -> i64 {
        self.levels[level].nt as i64 / i64::from(self.levels[up].t_factor)
    }

    /// The `(f_block, t_block)` at `up` that tile `(f_block, t_block)` of `level` folds into.
    ///
    /// Both axes are resolved through **cells**, not blocks: the old form divided `t_block` by the
    /// parent's `nt` directly, which is only right when one child tile is one parent time cell —
    /// exactly the weld T-434 removed.
    pub fn fold_target(&self, level: usize, up: usize, f_block: i64, t_block: i64) -> (i64, i64) {
        let u = &self.levels[up];
        let nf = self.nf as i64;
        let f_cell = (f_block * nf).div_euclid(i64::from(u.f_factor));
        let t_cell = t_block.saturating_mul(self.parent_cells_per_tile(level, up));
        (f_cell.div_euclid(nf), t_cell.div_euclid(u.nt as i64))
    }

    /// End (exclusive) of time block `t_block` at `level`, ns.
    pub fn block_end_ns(&self, level: usize, t_block: i64) -> i64 {
        (t_block + 1).saturating_mul(self.levels[level].t_block_ns())
    }
}

/// Shape of a de-welded view lattice (T-434, `docs/16` §6.2/§8.2).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewLattice {
    /// Scheme id. Must differ from every other scheme in the same data directory.
    pub scheme: u16,
    /// Frequency cell of node (0, 0), Hz.
    pub f_cell_hz: f64,
    /// Time cell of node (0, 0).
    pub t_cell: Duration,
    /// Cells per tile on both axes — uniform, so a client's tile **count** budget and its
    /// **byte** budget are the same statement (`docs/16` §5.5).
    pub cells_per_block: u32,
    /// Frequency levels, each ×2 the one before (including the finest).
    pub f_levels: usize,
    /// Time levels, each ×2 the one before (including the finest).
    pub t_levels: usize,
}

impl Default for ViewLattice {
    /// `docs/16` §6.2's proposal: 100 kHz × 128 s at the finest, 256 × 256 tiles, 8 × 8 nodes.
    /// Frequency reaches 12.8 MHz cells (6 GHz in 469 of them) and time 4.6 h cells (30 days in
    /// 158 rows), so the whole device range over a month is a couple of tiles either way.
    fn default() -> Self {
        Self {
            scheme: 2,
            f_cell_hz: 100_000.0,
            t_cell: Duration::from_secs(128),
            cells_per_block: 256,
            f_levels: 8,
            t_levels: 8,
        }
    }
}

impl ViewLattice {
    /// Flattened level index of node `(level_f, level_t)`.
    pub fn index(&self, level_f: usize, level_t: usize) -> usize {
        level_f * self.t_levels + level_t
    }

    /// `(level_f, level_t)` of a flattened level index.
    pub fn coords(&self, level: usize) -> (usize, usize) {
        (level / self.t_levels, level % self.t_levels)
    }

    /// The level ladder — really a DAG — for this lattice.
    ///
    /// Every node has exactly **one** producer, so the existing seal path folds each sealed tile
    /// into its consumers and nothing is produced twice: node `(0, j)` is the previous time level
    /// folded ×2 in time only, and node `(i, j)` for `i > 0` is `(i − 1, j)` folded ×2 in
    /// frequency only. Choosing time-first-then-frequency fixes a **canonical path** through the
    /// lattice, which matters because not every plane is path-independent: max-hold, frame count,
    /// linear-power sum, `obs_s` and `occ_max` fold the same either way, but the occupancy
    /// *ratio* (a time-weighted mean over the child's rows, then the best over the child's
    /// frequency cells) does not commute, so the order has to be named rather than assumed.
    pub fn levels(&self) -> Vec<LevelConfig> {
        let nt = self.cells_per_block;
        let mut out = Vec::with_capacity(self.f_levels * self.t_levels);
        for i in 0..self.f_levels {
            for j in 0..self.t_levels {
                out.push(if i == 0 && j == 0 {
                    LevelConfig {
                        from: None,
                        f_factor: 1,
                        t_factor: Some(1),
                        t_cells_per_block: nt,
                        ..LevelConfig::default()
                    }
                } else if i == 0 {
                    LevelConfig {
                        from: Some(self.index(0, j - 1)),
                        f_factor: 1,
                        t_factor: Some(2),
                        t_cells_per_block: nt,
                        ..LevelConfig::default()
                    }
                } else {
                    LevelConfig {
                        from: Some(self.index(i - 1, j)),
                        f_factor: 2,
                        t_factor: Some(1),
                        t_cells_per_block: nt,
                        ..LevelConfig::default()
                    }
                });
            }
        }
        out
    }
}

impl PyramidConfig {
    /// A de-welded **view lattice**: `f_levels × t_levels` nodes whose frequency and time levels
    /// are independent coordinates (`docs/16` §8.2), so a pane may zoom one axis without the
    /// other. The welded ladder cannot express this — its frequency coarsens ×16 across five
    /// levels while time coarsens ×86 400 — so the big view gets its own scheme rather than more
    /// levels on scheme 1, and the expensive fine levels stay where they already are.
    ///
    /// **Percentiles are not carried above node (0, 0)**: see [`LevelConfig::t_factor`]. A view
    /// tile answers *where have I looked, and how strong was it*; the noise-floor distribution
    /// stays a scheme-1 question.
    pub fn view_lattice(shape: ViewLattice) -> Self {
        Self {
            scheme: shape.scheme,
            f_cell_hz: shape.f_cell_hz,
            t_cell: shape.t_cell,
            f_cells_per_block: shape.cells_per_block,
            levels: shape.levels(),
            // T-571: a lattice's coarse nodes are maintained LIVE, row by row, so a read of one
            // is a read. `docs/16` §5.2 and T-453 had them folded when a read asked for them,
            // which made a cold screen wait on up to `MAX_MATERIALIZE_TILES` producer tiles and
            // gated the live edge on batch tile generation — the invariant CLAUDE.md states under
            // "Live rendering, tile maintenance and playback" forbids exactly that. Eager folding
            // AT SEAL (`coarse_on_demand: false`) is not the alternative and never was: a node
            // (0, 0) tile seals once per 64 s, so every coarse node lagged the live edge by a
            // whole tile and the read-time fold had to cover the difference anyway.
            coarse_on_demand: false,
            coarse_live: true,
            ..Self::default()
        }
    }

    /// Checks the settings and derives the geometry.
    pub fn geometry(&self) -> Result<Geometry, StoreError> {
        let bad = |m: String| Err(StoreError::Config(m));
        if self.levels.is_empty() || self.levels.len() > MAX_LEVELS {
            return bad(format!("need 1..={MAX_LEVELS} levels"));
        }
        if !(self.f_cell_hz.is_finite() && self.f_cell_hz > 0.0) {
            return bad("f_cell_hz must be positive".into());
        }
        let t0 = i64::try_from(self.t_cell.as_nanos()).unwrap_or(0);
        if t0 <= 0 {
            return bad("t_cell must be positive".into());
        }
        if self.f_cells_per_block == 0 || self.f_cells_per_block > 1 << 16 {
            return bad("f_cells_per_block must be in 1..=65536".into());
        }
        let h = &self.histogram;
        if h.bins < 2 || h.step_db.is_nan() || h.step_db <= 0.0 || !h.lo_db.is_finite() {
            return bad("histogram needs >= 2 bins and a positive step".into());
        }
        for p in [self.low_percentile, self.high_percentile] {
            if !(0.0..=100.0).contains(&p) {
                return bad("percentiles must be in 0..=100".into());
            }
        }
        if self.floor_memory_tiles == 0 {
            return bad("floor_memory_tiles must be >= 1".into());
        }
        for o in &self.retention_overrides {
            if usize::from(o.level) >= self.levels.len() {
                return bad(format!(
                    "retention override level {} does not exist",
                    o.level
                ));
            }
            if !(o.freq.lo_hz.is_finite() && o.freq.hi_hz >= o.freq.lo_hz) {
                return bad("retention override needs a valid frequency range".into());
            }
        }
        if self
            .compression_level
            .is_some_and(|l| !zstd::compression_level_range().contains(&l))
        {
            return bad("compression_level outside zstd's range".into());
        }
        if self.coarse_live && self.coarse_on_demand {
            return bad(
                "coarse_live and coarse_on_demand are alternatives: a coarse node is either \
                 maintained as rows arrive or folded when a read asks for it, never both"
                    .into(),
            );
        }
        let nf = self.f_cells_per_block as usize;
        let mut levels: Vec<LevelGeometry> = Vec::with_capacity(self.levels.len());
        let mut consumers: Vec<Vec<usize>> = vec![Vec::new(); self.levels.len()];
        for (i, l) in self.levels.iter().enumerate() {
            if l.t_cells_per_block == 0 {
                return bad(format!("level {i}: t_cells_per_block must be >= 1"));
            }
            let nt = l.t_cells_per_block as usize;
            if i == 0 {
                if l.f_factor != 1 {
                    return bad("level 0 f_factor must be 1".into());
                }
                if l.from.is_some() {
                    return bad("level 0 is fed by ingest and has no producer".into());
                }
                if l.t_factor.is_some_and(|t| t != 1) {
                    return bad("level 0 t_factor must be 1".into());
                }
                if t0.checked_mul(nt as i64).is_none() {
                    return bad("level 0: tile duration overflows".into());
                }
                levels.push(LevelGeometry {
                    f_cell_hz: self.f_cell_hz,
                    t_cell_ns: t0,
                    nt,
                    f_factor: 1,
                    t_factor: 1,
                    from: None,
                });
                continue;
            }
            let from = l.from.unwrap_or(i - 1);
            if from >= i {
                return bad(format!(
                    "level {i}: producer {from} must have a lower index, so one seal pass in \
                     index order folds a producer before its consumers"
                ));
            }
            if l.f_factor == 0 || self.f_cells_per_block % l.f_factor != 0 {
                return bad(format!(
                    "level {i}: f_factor must divide f_cells_per_block (so a parent cell's \
                     children lie in one child tile)"
                ));
            }
            let prev = levels[from];
            let t_factor = l.t_factor.unwrap_or(prev.nt as u32);
            if t_factor == 0 {
                return bad(format!("level {i}: t_factor must be >= 1"));
            }
            if prev.nt as u32 % t_factor != 0 {
                return bad(format!(
                    "level {i}: t_factor must divide level {from}'s t_cells_per_block (so a child \
                     tile is a whole number of parent time cells)"
                ));
            }
            // T-571: live maintenance folds ONE producer row at a time, and the occupancy ratio
            // only survives that when a node coarsens a single axis — see `Tile::fold_row`. The
            // weld (a child tile that is one parent time cell) is excluded for the same reason it
            // is the one case `fold_child` carries percentiles through: that fold needs the
            // child's finished tile-wide histogram, which no single row can supply.
            if self.coarse_live {
                if l.f_factor != 1 && t_factor != 1 {
                    return bad(format!(
                        "level {i}: live coarse maintenance needs a node to coarsen one axis \
                         (f_factor {} and t_factor {t_factor} both exceed 1)",
                        l.f_factor
                    ));
                }
                if prev.nt as u32 / t_factor == 1 {
                    return bad(format!(
                        "level {i}: live coarse maintenance cannot fold a WELDED level (one \
                         producer tile per time cell); its percentiles need the producer's \
                         finished tile histogram, which a row fold does not have"
                    ));
                }
            }
            // Parent time cells one producer tile spans. 1 is the weld.
            let k = prev.nt / t_factor as usize;
            if nt % k != 0 {
                return bad(format!(
                    "level {i}: one tile of level {from} spans {k} time cells here, which must \
                     divide t_cells_per_block (so a child tile never straddles two parent tiles)"
                ));
            }
            if l.f_factor == 1 && t_factor == 1 {
                return bad(format!("level {i}: coarsens neither axis"));
            }
            let f_cell = prev.f_cell_hz * f64::from(l.f_factor);
            let Some(t_cell) = prev.t_cell_ns.checked_mul(i64::from(t_factor)) else {
                return bad(format!("level {i}: time cell overflows"));
            };
            if t_cell.checked_mul(nt as i64).is_none() {
                return bad(format!("level {i}: tile duration overflows"));
            }
            consumers[from].push(i);
            levels.push(LevelGeometry {
                f_cell_hz: f_cell,
                t_cell_ns: t_cell,
                nt,
                f_factor: l.f_factor,
                t_factor,
                from: Some(from),
            });
        }
        Ok(Geometry {
            nf,
            levels,
            consumers,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_ladder() {
        let g = PyramidConfig::default().geometry().unwrap();
        let s = 1_000_000_000i64;
        let cells: Vec<(f64, i64)> = g
            .levels
            .iter()
            .map(|l| (l.f_cell_hz, l.t_cell_ns))
            .collect();
        assert_eq!(
            cells,
            vec![
                (6250.0, s),
                (12500.0, 60 * s),
                (25000.0, 900 * s),
                (50000.0, 3600 * s),
                (100000.0, 86400 * s)
            ]
        );
        assert_eq!(g.levels[4].t_block_ns(), 7 * 86400 * s);
        // Child tile (f 3, t 10) at L0: 6.4 MHz blocks; parent cell width 12.5 kHz.
        assert_eq!(g.fold_target(0, 1, 3, 10), (1, 0));
        assert_eq!(g.fold_target(0, 1, 2, 29), (1, 1));
        assert_eq!(g.fold_target(0, 1, -1, -1), (-1, -1));
        // The ladder is the lattice with one consumer per level and the weld still in place:
        // every level's time factor is the producer's whole tile, so one child tile is one
        // parent time column.
        for l in 0..g.n_levels() {
            let want: &[usize] = if l == g.top() { &[] } else { &[l + 1] };
            assert_eq!(g.consumers(l), want, "level {l}");
            if l > 0 {
                assert_eq!(g.levels[l].t_factor, g.levels[l - 1].nt as u32);
                assert_eq!(g.parent_cells_per_tile(l - 1, l), 1);
            }
        }
    }

    #[test]
    fn rejects_bad_factor() {
        let mut c = PyramidConfig::default();
        c.levels[1].f_factor = 3;
        assert!(c.geometry().is_err());
    }

    /// T-434: the two axes are independent coordinates, and the flattened level index is exactly
    /// `(level_f, level_t)`.
    #[test]
    fn view_lattice_de_welds_the_axes() {
        let shape = ViewLattice::default();
        let g = PyramidConfig::view_lattice(shape).geometry().unwrap();
        assert_eq!(g.n_levels(), shape.f_levels * shape.t_levels);
        let s = 1_000_000_000i64;
        for i in 0..shape.f_levels {
            for j in 0..shape.t_levels {
                let l = &g.levels[shape.index(i, j)];
                // Each axis coarsens on its own: ×2^i in frequency, ×2^j in time, independently.
                assert_eq!(l.f_cell_hz, 100_000.0 * f64::from(1u32 << i), "({i},{j})");
                assert_eq!(l.t_cell_ns, 128 * s * i64::from(1u32 << j), "({i},{j})");
            }
        }
        // Node (0, j) is fed by folding time only; (i, j) for i > 0 by folding frequency only.
        assert_eq!(g.levels[shape.index(0, 3)].from, Some(shape.index(0, 2)));
        assert_eq!(g.levels[shape.index(0, 3)].f_factor, 1);
        assert_eq!(g.levels[shape.index(0, 3)].t_factor, 2);
        assert_eq!(g.levels[shape.index(2, 3)].from, Some(shape.index(1, 3)));
        assert_eq!(g.levels[shape.index(2, 3)].f_factor, 2);
        assert_eq!(g.levels[shape.index(2, 3)].t_factor, 1);
        // A node on the finest frequency row feeds two consumers — one per axis. That is the
        // whole point: a ladder node feeds one.
        assert_eq!(
            g.consumers(shape.index(0, 0)),
            &[shape.index(0, 1), shape.index(1, 0)]
        );
        // A node off that row feeds only the next frequency level.
        assert_eq!(g.consumers(shape.index(1, 0)), &[shape.index(2, 0)]);
        // The coarsest frequency row is consumed by nothing: 8 leaves, not one top.
        for j in 0..shape.t_levels {
            assert!(g.consumers(shape.index(shape.f_levels - 1, j)).is_empty());
        }
        // A time fold puts two child tiles in one parent tile; a frequency fold, one.
        assert_eq!(
            g.parent_cells_per_tile(shape.index(0, 0), shape.index(0, 1)),
            128
        );
        assert_eq!(
            g.parent_cells_per_tile(shape.index(0, 0), shape.index(1, 0)),
            256
        );
        assert_eq!(
            g.fold_target(shape.index(0, 0), shape.index(0, 1), 5, 3),
            (5, 1)
        );
        assert_eq!(
            g.fold_target(shape.index(0, 0), shape.index(1, 0), 5, 3),
            (2, 3)
        );
    }

    /// The addressing T-438's route needs, and the shape T-437's client LRU keys on:
    /// `(level_f, level_t, f_block, t_block)`. It is derived from the geometry, so it is defined
    /// for a ladder too — as the diagonal.
    #[test]
    fn per_axis_addressing_is_a_grid_for_a_lattice_and_a_diagonal_for_a_ladder() {
        let shape = ViewLattice::default();
        let g = PyramidConfig::view_lattice(shape).geometry().unwrap();
        assert_eq!(g.f_axis().len(), shape.f_levels);
        assert_eq!(g.t_axis().len(), shape.t_levels);
        for i in 0..shape.f_levels {
            for j in 0..shape.t_levels {
                // Every (level_f, level_t) exists, and round-trips through the flattened index.
                let l = g.level_at(i, j).expect("the lattice fills its grid");
                assert_eq!(l, shape.index(i, j), "({i},{j})");
                assert_eq!(g.axes_of(l), (i, j));
            }
        }
        assert_eq!(g.level_at(shape.f_levels, 0), None);

        // A welded ladder occupies only the diagonal: asking for a coarse frequency at a fine time
        // has no answer, and the geometry says so instead of returning a level whose time cell is
        // a day.
        let g = PyramidConfig::default().geometry().unwrap();
        for l in 0..g.n_levels() {
            assert_eq!(g.axes_of(l), (l, l), "level {l}");
            assert_eq!(g.level_at(l, l), Some(l));
        }
        assert_eq!(g.level_at(3, 0), None);
        assert_eq!(g.level_at(0, 3), None);
    }

    #[test]
    fn rejects_a_producer_that_seals_later() {
        let mut c = PyramidConfig::default();
        c.levels[1].from = Some(3);
        let e = c.geometry().unwrap_err().to_string();
        assert!(e.contains("lower index"), "{e}");
    }

    #[test]
    fn rejects_a_child_tile_that_straddles_two_parent_tiles() {
        let mut c = PyramidConfig::default();
        // L0 tiles hold 60 time cells; a t_factor of 8 does not divide 60.
        c.levels[1].t_factor = Some(8);
        assert!(c.geometry().is_err());
        // 4 divides 60, but the resulting 15 parent cells per child tile must also divide L1's
        // own t_cells_per_block (15) — it does, so this one is legal.
        c.levels[1].t_factor = Some(4);
        assert!(c.geometry().is_ok());
        // 2 divides 60, giving 30 parent cells per child tile, which does not divide 15.
        c.levels[1].t_factor = Some(2);
        assert!(c.geometry().is_err());
    }

    #[test]
    fn rejects_a_level_that_coarsens_nothing() {
        let mut c = PyramidConfig::default();
        c.levels[1].f_factor = 1;
        c.levels[1].t_factor = Some(1);
        assert!(c.geometry().is_err());
    }
}
