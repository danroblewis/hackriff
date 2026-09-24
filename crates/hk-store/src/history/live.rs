//! **T-585: a coarse node's open tile holds its committed rows ENCODED, and only the in-progress
//! row as an accumulator.**
//!
//! T-571 made a live lattice's coarse nodes incrementally maintained and paid for it in
//! residency: every node held a whole [`Tile`] of accumulator (44 B a cell, ~3 MB at the shipped
//! 1024 × 64 tile) for the tile it was filling — 16 nodes, 16 open accumulators, measured at
//! 7.76 MB/MHz at one block and ~94 MB at a 20 MHz live edge. The tile is the write unit, so a
//! row committed into a coarse tile had to live somewhere between its commit and the tile's
//! seal, and the only place was RAM, in full precision, for the whole tile.
//!
//! This is the remedy the ticket names. A row that has been **committed** — folded upwards and
//! final — is held here in the codec's own stored form ([`codec::encode_row_cells`]: the same
//! i16 centi-dB, u16 fraction and varint count a sealed tile carries, column-wise, zstd'd when that
//! shrinks it). Only the **in-progress** row is an accumulator ([`RowAcc`], 36 B a cell over one
//! row), plus transiently the row a gap flush just completed while its consumers read it. Per node
//! that is one or two rows of accumulator and `nt` encoded rows, instead of `nt` rows of
//! accumulator: residency is O(levels × row) plus a small encoded remainder, and it no longer
//! grows with the node count the way T-453 forbade.
//!
//! # Where precision goes, and why nothing downstream can tell
//!
//! A committed row is quantised **once**, on commit, to exactly the grid the sealed tile file
//! would have quantised it to at the seal; the seal then re-encodes values already on that grid,
//! which is idempotent (a centi-dB value re-rounds to itself, a `u16` fraction likewise, counts are
//! exact). Its consumers read the row **before** it is committed — the cascade folds a completed
//! row into every consumer and only then encodes it — so the level above folds full-precision
//! values exactly as T-571 did. The sealed file therefore holds the same cells it would have held,
//! and the reviewer's cell-for-cell agreement with the on-demand control stands.
//!
//! # Not a stored-format change after all
//!
//! The ticket expected one. It is not: the encoded rows live only in memory and the tile is still
//! written whole, as [`FORMAT_VERSION`](super::codec::FORMAT_VERSION), when it seals. No scheme
//! bump, no migration, and a store written by T-571 opens unchanged.

use hk_model::TileKey;

use super::codec;
use super::config::LevelGeometry;
use super::stats::undb;
use super::tile::{
    MAX_CELL_SHAPES, MAX_GAIN_STATES, MAX_ORIGINS, MAX_PROVENANCE_STEPS, ProvenanceSummary, RowSrc,
    Tile,
};

/// Bytes one cell of [`RowAcc`] holds: `count` u32, `max` f32, `sum_lin` f64, `obs_s` f64,
/// `occ_s` f64, `occ_max` f32. The percentiles a full [`Tile`] cell also carries are never
/// written by a row fold (`docs/16` §6.2: no percentiles above node (0, 0)), so a row of
/// accumulator does not hold them.
pub const ROW_ACC_BYTES_PER_CELL: usize = 4 + 4 + 8 + 8 + 8 + 4;

/// One row of live-fold accumulator over `nf` frequency cells: the in-progress row of a coarse
/// node, adjusted as producer rows arrive (CLAUDE.md, "adjusts its in-progress top row as rows
/// arrive and commits every N").
#[derive(Clone, Debug)]
pub(crate) struct RowAcc {
    /// The tile row this accumulates.
    pub t: usize,
    /// Cells with `count > 0`, so a row whose committed footprint has been cleared is released.
    pub live: usize,
    pub count: Vec<u32>,
    pub max: Vec<f32>,
    pub sum_lin: Vec<f64>,
    pub obs_s: Vec<f64>,
    pub occ_s: Vec<f64>,
    pub occ_max: Vec<f32>,
}

impl RowAcc {
    pub fn new(nf: usize) -> Self {
        Self {
            t: 0,
            live: 0,
            count: vec![0; nf],
            max: vec![f32::NEG_INFINITY; nf],
            sum_lin: vec![0.0; nf],
            obs_s: vec![0.0; nf],
            occ_s: vec![0.0; nf],
            occ_max: vec![0.0; nf],
        }
    }

    pub fn clear(&mut self) {
        self.live = 0;
        self.count.fill(0);
        self.max.fill(f32::NEG_INFINITY);
        self.sum_lin.fill(0.0);
        self.obs_s.fill(0.0);
        self.occ_s.fill(0.0);
        self.occ_max.fill(0.0);
    }

    fn clear_cell(&mut self, f: usize) {
        if self.count[f] > 0 {
            self.live -= 1;
        }
        self.count[f] = 0;
        self.max[f] = f32::NEG_INFINITY;
        self.sum_lin[f] = 0.0;
        self.obs_s[f] = 0.0;
        self.occ_s[f] = 0.0;
        self.occ_max[f] = 0.0;
    }

    /// Accumulates a cell's stored-form values (a decoded segment) into cell `f`: the same rule
    /// as the live fold — counts, linear power and seconds add, maxima max.
    #[allow(clippy::too_many_arguments)]
    fn add_decoded(
        &mut self,
        f: usize,
        count: u32,
        max_db: f32,
        mean_db: f32,
        occupancy: f32,
        occ_max: f32,
        coverage: f32,
        t_cell_s: f64,
    ) {
        if count == 0 {
            return;
        }
        if self.count[f] == 0 {
            self.live += 1;
        }
        self.count[f] =
            (u64::from(self.count[f]) + u64::from(count)).min(u64::from(u32::MAX)) as u32;
        self.max[f] = self.max[f].max(max_db);
        self.sum_lin[f] += undb(mean_db) * f64::from(count);
        let obs = f64::from(coverage) * t_cell_s;
        self.obs_s[f] += obs;
        self.occ_s[f] += f64::from(occupancy) * obs;
        self.occ_max[f] = self.occ_max[f].max(occ_max);
    }

    /// This row as a fold source, placed in `tile`'s geometry.
    fn src<'a>(&'a self, tile: &LiveTile) -> RowSrc<'a> {
        RowSrc {
            f_cell0: tile.f_cell0,
            t_cell0: tile.t_cell0,
            nf: tile.nf,
            nt: tile.nt,
            t_cell_s: tile.t_cell_s,
            ct: self.t,
            count: &self.count,
            max: &self.max,
            sum_lin: &self.sum_lin,
            obs_s: &self.obs_s,
            occ_s: &self.occ_s,
            occ_max: &self.occ_max,
        }
    }
}

/// One committed footprint of one row, in stored form: cells `[f_lo, f_hi)` of row `t`.
#[derive(Clone, Debug)]
pub(crate) struct Segment {
    pub t: u32,
    pub f_lo: u32,
    pub f_hi: u32,
    /// Length of the raw (uncompressed) encoding; `bytes` is zstd when it differs from
    /// `bytes.len()`.
    raw_len: u32,
    zstd: bool,
    bytes: Box<[u8]>,
}

/// Raw encodings shorter than this are never handed to zstd: the frame overhead exceeds the
/// saving, and the compressor's own cost is paid on every arriving row.
const MIN_COMPRESS_LEN: usize = 64;

/// A coarse node's open tile under live maintenance.
///
/// Every cell of one of these is written by exactly one producer tile (`Tile::fold_child`'s
/// rule, carried by T-571's footprints), so a committed `(row, footprint)` is final and can be
/// encoded the moment its consumers have read it. What remains an accumulator is
/// [`Self::row_pending`] — the row producer rows are still being folded into — and, between a
/// gap flush and the cascade's read of it, the row that flush completed.
#[derive(Clone, Debug)]
pub(crate) struct LiveTile {
    pub key: TileKey,
    pub nf: usize,
    pub nt: usize,
    /// Global cell index of frequency cell 0 / time cell 0.
    pub f_cell0: i64,
    pub t_cell0: i64,
    pub t_cell_s: f64,
    /// Merged once per sealed producer tile, never per row (T-571).
    pub prov: ProvenanceSummary,
    /// In-progress rows. Normally one; two while a gap flush's completed row waits to be read.
    acc: Vec<RowAcc>,
    /// Released rows kept for reuse, so the steady state allocates nothing.
    spare: Vec<RowAcc>,
    /// Committed rows, in commit order (ascending `t` under live operation).
    committed: Vec<Segment>,
    committed_bytes: usize,
    committed_raw_bytes: usize,
    /// T-571: the in-progress top row of a node that downsamples time, as `(row, f_lo, f_hi)`.
    /// A node that coarsens frequency alone never sets it — each producer row completes one of
    /// its rows outright.
    pub row_pending: Option<(usize, usize, usize)>,
}

impl LiveTile {
    pub fn new(key: TileKey, nf: usize, g: &LevelGeometry) -> Self {
        let mut t = Self {
            key,
            nf,
            nt: g.nt,
            f_cell0: 0,
            t_cell0: 0,
            t_cell_s: 0.0,
            prov: ProvenanceSummary {
                gain_states: Vec::with_capacity(MAX_GAIN_STATES),
                steps: Vec::with_capacity(MAX_PROVENANCE_STEPS),
                origins: Vec::with_capacity(MAX_ORIGINS),
                cell_shapes: Vec::with_capacity(MAX_CELL_SHAPES),
                ..Default::default()
            },
            acc: Vec::new(),
            spare: Vec::new(),
            committed: Vec::new(),
            committed_bytes: 0,
            committed_raw_bytes: 0,
            row_pending: None,
        };
        t.reset(key, g);
        t
    }

    /// Clears the tile for reuse under `key` (same dimensions), keeping its row buffers.
    pub fn reset(&mut self, key: TileKey, g: &LevelGeometry) {
        debug_assert_eq!(self.nt, g.nt);
        self.key = key;
        self.f_cell0 = key.f_block * self.nf as i64;
        self.t_cell0 = key.t_block * self.nt as i64;
        self.t_cell_s = g.t_cell_ns as f64 * 1e-9;
        self.prov.clear();
        for mut r in self.acc.drain(..) {
            r.clear();
            self.spare.push(r);
        }
        self.committed.clear();
        self.committed_bytes = 0;
        self.committed_raw_bytes = 0;
        self.row_pending = None;
    }

    /// The accumulator row for tile row `t`, created (from a spare) if it is not in progress.
    fn acc_row_mut(&mut self, t: usize) -> &mut RowAcc {
        if let Some(i) = self.acc.iter().position(|r| r.t == t) {
            return &mut self.acc[i];
        }
        let mut r = self.spare.pop().unwrap_or_else(|| RowAcc::new(self.nf));
        r.clear();
        r.t = t;
        self.acc.push(r);
        self.acc.last_mut().expect("just pushed")
    }

    /// **Streaming row fold (T-571, held per row since T-585).** Folds producer row `src`,
    /// frequency cells `[f_lo, f_hi)` of it, into this tile's covering row, **accumulating**;
    /// pushes onto `out` every row of this tile that is complete as a result, with the frequency
    /// footprint that row covers.
    ///
    /// This is `Tile::fold_child` one row at a time, and it agrees with it cell for cell, with one
    /// deliberate restriction: **a node may coarsen time or frequency, not both**
    /// (`f_factor == 1 || t_factor == 1`), which [`super::PyramidConfig::geometry`] enforces
    /// wherever live maintenance is on. The reason is the occupancy *ratio*: `fold_child` takes
    /// the best ratio over the producer's frequency cells **after** summing each one over the
    /// producer's rows, and that maximum cannot be updated from one row at a time without keeping
    /// a per-producer-cell running `(obs, occ)` — residency proportional to a producer row, per
    /// node, for a shape no scheme uses. With one axis per node it is exact:
    ///
    /// - `f_factor == 1`: the group is a single producer cell, so the best ratio is that cell's,
    ///   and `obs_s`/`occ_s` accumulate outright (`occ_s / obs_s` recovers the ratio).
    /// - `t_factor == 1`: the whole group is inside this one producer row, so the maximum is
    ///   taken here and the row is final the moment it is folded.
    ///
    /// **Exactly once.** Each of this tile's cells is written by exactly one producer tile, and a
    /// caller must fold each `(producer tile, row, footprint)` once — the footprint is what keeps
    /// that true down a frequency chain, where a parent row is filled by `f_factor` producer
    /// tiles and must not be re-folded upwards as a whole each time one of them arrives.
    ///
    /// Provenance is **not** merged here; it is merged once, when the producer tile seals (see
    /// [`super::Pyramid`]), because a summary is not a per-row quantity and merging it per row
    /// would inflate every count it holds.
    pub fn fold_row(
        &mut self,
        src: &RowSrc<'_>,
        f_lo: usize,
        f_hi: usize,
        f_factor: u32,
        t_factor: u32,
        out: &mut Vec<(usize, usize, usize)>,
    ) {
        debug_assert!(
            f_factor <= 1 || t_factor <= 1,
            "a live-maintained node coarsens one axis"
        );
        let m = i64::from(f_factor.max(1));
        let n = i64::from(t_factor.max(1));
        let cg_t = src.t_cell0 + src.ct as i64;
        let tp = cg_t.div_euclid(n) - self.t_cell0;
        if tp < 0 || tp >= self.nt as i64 || src.ct >= src.nt || f_lo >= f_hi {
            debug_assert!(f_lo >= f_hi, "producer row outside the consumer tile");
            return;
        }
        let tp = tp as usize;
        // This row's footprint in **this** tile's frequency cells.
        let p_lo = ((src.f_cell0 + f_lo as i64).div_euclid(m) - self.f_cell0)
            .clamp(0, self.nf as i64) as usize;
        let p_hi = ((src.f_cell0 + f_hi as i64 - 1).div_euclid(m) + 1 - self.f_cell0)
            .clamp(0, self.nf as i64) as usize;
        // A row earlier than this one can receive nothing further: producer rows close in
        // increasing order, so a skipped producer row (a gap) still lets its consumer row out.
        if let Some((prev, lo, hi)) = self.row_pending
            && prev != tp
        {
            out.push((prev, lo, hi));
            self.row_pending = None;
        }
        let f_cell0 = self.f_cell0;
        let row = self.acc_row_mut(tp);
        for fp in p_lo..p_hi {
            let c0 = (f_cell0 + fp as i64) * m - src.f_cell0;
            let (mut count, mut max, mut sum_lin, mut occ_max) =
                (0u64, f32::NEG_INFINITY, 0.0f64, 0f32);
            let (mut sum_obs, mut best) = (0.0f64, 0.0f64);
            for k in 0..m {
                let cf = c0 + k;
                if cf < 0 || cf >= src.nf as i64 {
                    continue;
                }
                let cf = cf as usize;
                let cnt = src.count[cf];
                if cnt == 0 {
                    continue;
                }
                count += u64::from(cnt);
                max = max.max(src.max[cf]);
                sum_lin += src.sum_lin[cf];
                occ_max = occ_max.max(src.occ_max[cf]);
                let (o, c) = src.cell_obs(cf);
                if o > 0.0 {
                    sum_obs += o;
                    best = best.max(c / o);
                }
            }
            if count == 0 {
                continue;
            }
            if row.count[fp] == 0 {
                row.live += 1;
            }
            row.count[fp] = (u64::from(row.count[fp]) + count).min(u64::from(u32::MAX)) as u32;
            row.max[fp] = row.max[fp].max(max);
            row.sum_lin[fp] += sum_lin;
            row.occ_max[fp] = row.occ_max[fp].max(occ_max);
            // Coverage is the observed fraction of THIS cell's own extent, so a producer cell that
            // was never observed pulls it below 1 — the same rule as `fold_child`.
            let obs = sum_obs / m as f64;
            row.obs_s[fp] += obs;
            row.occ_s[fp] += best * obs;
        }
        let (lo, hi) = match self.row_pending {
            Some((_, l, h)) => (l.min(p_lo), h.max(p_hi)),
            None => (p_lo, p_hi),
        };
        if n == 1 || (cg_t + 1).rem_euclid(n) == 0 {
            self.row_pending = None;
            out.push((tp, lo, hi));
        } else {
            self.row_pending = Some((tp, lo, hi));
        }
    }

    /// T-571: the in-progress coarse row, taken. Called when a coarse tile is about to seal, so
    /// the rows it has accumulated but not yet declared complete still reach its own consumers.
    pub fn take_pending_row(&mut self) -> Option<(usize, usize, usize)> {
        self.row_pending.take()
    }

    /// **Commits** cells `[lo, hi)` of row `t`: encodes them from the accumulator into a
    /// [`Segment`] and clears them, releasing the row once nothing of it is left in progress.
    /// Called after every consumer has folded the row (the cascade) — or at once, when nothing
    /// will read it before it is next needed (the reopen replay).
    ///
    /// Returns `(raw, stored)` byte lengths of the segment, `None` when there was nothing to
    /// commit (the row is not in progress, or the footprint holds no observed cell). The pending
    /// row is never committed: it is still being adjusted.
    pub fn commit(
        &mut self,
        t: usize,
        lo: usize,
        hi: usize,
        compression: Option<i32>,
        scratch: &mut Vec<u8>,
    ) -> Option<(usize, usize)> {
        if self.row_pending.is_some_and(|(p, _, _)| p == t) {
            return None;
        }
        let idx = self.acc.iter().position(|r| r.t == t)?;
        let (lo, hi) = (lo.min(self.nf), hi.min(self.nf));
        let row = &mut self.acc[idx];
        let observed = row.count[lo..hi].iter().filter(|&&c| c > 0).count();
        let sizes = if lo < hi && observed > 0 {
            let src = row.src_geometry(self.f_cell0, self.t_cell0, self.nf, self.nt, self.t_cell_s);
            codec::encode_row_cells(&src, lo, hi, scratch);
            let raw_len = scratch.len();
            let compressed = compression
                .filter(|_| raw_len >= MIN_COMPRESS_LEN)
                .and_then(|level| zstd::bulk::compress(scratch, level).ok())
                .filter(|c| c.len() < raw_len);
            let zstd = compressed.is_some();
            let bytes: Box<[u8]> = compressed
                .unwrap_or_else(|| scratch.clone())
                .into_boxed_slice();
            let stored = bytes.len();
            self.committed_bytes += stored;
            self.committed_raw_bytes += raw_len;
            self.committed.push(Segment {
                t: t as u32,
                f_lo: lo as u32,
                f_hi: hi as u32,
                raw_len: raw_len as u32,
                zstd,
                bytes,
            });
            let row = &mut self.acc[idx];
            for f in lo..hi {
                row.clear_cell(f);
            }
            Some((raw_len, stored))
        } else {
            None
        };
        if self.acc[idx].live == 0 {
            let r = self.acc.swap_remove(idx);
            self.spare.push(r);
        }
        sizes
    }

    /// Every committed `(row, f_lo, f_hi)`, ascending by row then frequency: what a reopen
    /// replays into the level above, one footprint at a time, exactly as the live cascade fed
    /// it. The pending row is deliberately not among them — it is emitted when it completes.
    pub fn committed_footprints(&self) -> Vec<(usize, usize, usize)> {
        let mut v: Vec<(usize, usize, usize)> = self
            .committed
            .iter()
            .map(|s| (s.t as usize, s.f_lo as usize, s.f_hi as usize))
            .collect();
        v.sort_unstable();
        v
    }

    /// Row `t` as a fold source: straight from the accumulator while the row is in progress,
    /// else decoded from its committed segments into `scratch`. `None` when the row holds
    /// nothing.
    pub fn row_src<'a>(&'a self, t: usize, scratch: &'a mut RowAcc) -> Option<RowSrc<'a>> {
        if let Some(r) = self.acc.iter().find(|r| r.t == t) {
            return (r.live > 0).then(|| r.src(self));
        }
        scratch.clear();
        scratch.t = t;
        let t_cell_s = self.t_cell_s;
        for seg in self.committed.iter().filter(|s| s.t as usize == t) {
            let lo = seg.f_lo as usize;
            self.decode_segment(seg, |j, count, max, mean, occ, occ_max, cov| {
                scratch.add_decoded(lo + j, count, max, mean, occ, occ_max, cov, t_cell_s);
            });
        }
        (scratch.live > 0).then(|| scratch.src(self))
    }

    /// Decodes one segment, cell by cell: `(j, count, max_db, mean_db, occupancy, occ_max,
    /// coverage)` with `j` relative to the segment's `f_lo`.
    fn decode_segment(&self, seg: &Segment, cell: impl FnMut(usize, u32, f32, f32, f32, f32, f32)) {
        let m = (seg.f_hi - seg.f_lo) as usize;
        let inflated;
        let raw: &[u8] = if seg.zstd {
            match zstd::bulk::decompress(&seg.bytes, seg.raw_len as usize) {
                Ok(b) => {
                    inflated = b;
                    &inflated
                }
                // Unreachable for bytes this process produced; a segment that cannot be read
                // reads as unobserved rather than as anything invented.
                Err(_) => return,
            }
        } else {
            &seg.bytes
        };
        let _ = codec::decode_row_cells(raw, m, cell);
    }

    /// **Materialises** this tile into `tile` (reset first): every committed segment and every
    /// in-progress row, accumulated by the live fold's own rule, plus the provenance. What a
    /// query, a seal write, or the on-demand fold reads.
    pub fn materialize_into(&self, tile: &mut Tile, g: &LevelGeometry) {
        tile.reset(self.key, g);
        tile.prov = self.prov.clone();
        let (nf, t_cell_s) = (self.nf, self.t_cell_s);
        for seg in &self.committed {
            let base = seg.t as usize * nf + seg.f_lo as usize;
            self.decode_segment(seg, |j, count, max, mean, occ, occ_max, cov| {
                let obs = f64::from(cov) * t_cell_s;
                tile.accumulate_cell(
                    base + j,
                    count,
                    max,
                    undb(mean) * f64::from(count),
                    obs,
                    f64::from(occ) * obs,
                    occ_max,
                );
            });
        }
        for r in &self.acc {
            let base = r.t * nf;
            for f in 0..nf {
                if r.count[f] == 0 {
                    continue;
                }
                tile.accumulate_cell(
                    base + f,
                    r.count[f],
                    r.max[f],
                    r.sum_lin[f],
                    r.obs_s[f],
                    r.occ_s[f],
                    r.occ_max[f],
                );
            }
        }
    }

    /// Resident bytes: `(accumulator, encoded)`. Accumulator rows in use and spare, plus every
    /// committed segment's bytes and its bookkeeping.
    pub fn resident_bytes(&self) -> (usize, usize) {
        let rows = self.acc.len() + self.spare.len();
        (
            rows * self.nf * ROW_ACC_BYTES_PER_CELL,
            self.committed_bytes + self.committed.len() * std::mem::size_of::<Segment>(),
        )
    }

    /// Committed segments held, and their raw (pre-compression) bytes.
    pub fn committed_stats(&self) -> (usize, usize, usize) {
        (
            self.committed.len(),
            self.committed_raw_bytes,
            self.committed_bytes,
        )
    }

    /// Rows currently held as accumulator.
    pub fn acc_rows(&self) -> usize {
        self.acc.len()
    }

    /// The newest row holding anything, committed or still in progress — what the live cascade
    /// has put in this node, before any read-time preview (T-583's trail measurement).
    #[cfg(test)]
    pub fn newest_row(&self) -> Option<usize> {
        self.committed
            .iter()
            .map(|s| s.t as usize)
            .chain(self.acc.iter().filter(|r| r.live > 0).map(|r| r.t))
            .max()
    }
}

impl RowAcc {
    /// This row as a fold source, given its tile's geometry by value (for use while the tile is
    /// mutably borrowed).
    fn src_geometry(
        &self,
        f_cell0: i64,
        t_cell0: i64,
        nf: usize,
        nt: usize,
        t_cell_s: f64,
    ) -> RowSrc<'_> {
        RowSrc {
            f_cell0,
            t_cell0,
            nf,
            nt,
            t_cell_s,
            ct: self.t,
            count: &self.count,
            max: &self.max,
            sum_lin: &self.sum_lin,
            obs_s: &self.obs_s,
            occ_s: &self.occ_s,
            occ_max: &self.occ_max,
        }
    }
}
