//! **The last-known ledger (T-1058)**: the fog-of-war's source, maintained incrementally, never
//! searched for.
//!
//! # What it holds
//!
//! Per **source** (a front end, [`super::FrameInput::source`] = `source_key(device_id)`) and per
//! **level-0 frequency cell** of the store it lives in:
//!
//! - `last_tc` — the level-0 time cell of the newest frame that reached the cell;
//! - `first_tc` — the level-0 time cell of the first frame that ever did;
//! - `epoch` — the source's **tune epoch** when it was last reached (a counter that advances every
//!   time the source's frame window changes: a retune);
//! - per **time level** of the store (its `level_t` axis, [`super::Geometry::t_axis`]) the max-hold
//!   of every frame in the **level-`L` time cell that contains `last_tc`** — which is exactly the
//!   value the store's own cell at that level holds for the band's last row. So a shadow read from
//!   the ledger has the colour of the last live row at the viewer's own zoom (the T-911 rule), at
//!   every zoom, without reading a tile.
//!
//! # Why it exists
//!
//! Before T-1058 the canvas's shadow tier was found by a bounded newest-first **search** of the
//! pyramid per tile (`Pyramid::last_known_search`). At a fine zoom the departed band's last row is
//! many search steps away, the budget ran out first, and the tile carried no shadow; panning closer
//! brought some tiles within budget and not others (the user, 2026-09-25: "SOME of the tiles
//! resolve, but not all"). A row **is** the newest sample of every cell it covers, so the newest
//! value can be kept as the rows arrive — O(bins) per row — and read back exactly, at any zoom and
//! any distance in time.
//!
//! # What it cannot say, stated
//!
//! It keeps only the **newest** value per cell. A tile whose rows lie *before* a column's newest
//! sample (a band looked at again later) is asked about a value the ledger no longer holds; the
//! answer is [`LedgerColumn::Later`], and the caller falls back to the bounded search for exactly
//! those columns (the migration window, `docs/adr/0020` §"T-1058"). Everything at or after a
//! column's newest sample — every departed band, at any distance — is answered here.
//!
//! # Persistence
//!
//! Written to [`LEDGER_FILE`] beside the tiles (zstd, CRC-checked), on the seal and checkpoint path
//! the store's other small files ride, so it survives a restart; it is **not** evicted with tiles —
//! a band whose tiles the byte budget has removed keeps its last-known value. A ledger file that is
//! missing, corrupt or written under a different geometry is discarded and the ledger restarts
//! **incomplete** ([`Ledger::complete`] false): it then knows only what it has seen since, so its
//! "never observed" is no longer proof, and the caller searches those columns as before.

use std::collections::BTreeMap;

use hk_model::FreqRange;

use super::codec::{crc32, dq_db, q_db};
use super::config::Geometry;

/// File of the ledger, under the scheme root.
pub(super) const LEDGER_FILE: &str = "last_known.ledger";

/// Level-0 frequency cells per ledger block (allocated only where a frame ever landed).
const BLOCK: usize = 256;

const MAGIC: &[u8; 4] = b"HKLK";
const VERSION: u16 = 1;

/// What the ledger says about one column before an instant ([`Ledger::columns`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LedgerColumn {
    /// No frame ever reached any cell of the column (as far as this ledger has seen — see
    /// [`LedgerAnswer::complete`]).
    Never,
    /// The column's newest sample lies at or before `before`: this **is** the most-recent-known
    /// value there, exactly.
    Known(LedgerValue),
    /// The column's first-ever sample lies at or after `before`: nothing older exists.
    NothingBefore {
        /// Start of the first-ever sample's level-0 time cell, Unix ns.
        first_ns: i64,
    },
    /// Something reached the column before `before`, and something at or after it: the value at
    /// `before` is not the newest, and the ledger does not hold it.
    Later {
        /// End of the newest sample's level-0 time cell, Unix ns.
        newest_ns: i64,
    },
}

/// A column's most-recent-known value, from the ledger.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LedgerValue {
    /// Max-hold over the column's cells and over the query time level's cell holding the newest
    /// sample, dB, at the stored 0.01 dB — the value the store's own cell at that level holds for
    /// the band's last row.
    pub max_db: f32,
    /// End of the newest sample's level-0 time cell, Unix ns: when it was last seen.
    pub last_ns: i64,
    /// Start of the first-ever sample's level-0 time cell, Unix ns.
    pub first_ns: i64,
    /// The source's tune epoch when the newest sample was taken.
    pub epoch: u32,
    /// The source ([`super::source_key`]) of the newest sample.
    pub source: u64,
}

/// [`Ledger::columns`]' answer over a frequency window.
#[derive(Clone, Debug, PartialEq)]
pub struct LedgerAnswer {
    /// Low edge of column 0, Hz.
    pub f_lo_hz: f64,
    /// Column width, Hz.
    pub f_cell_hz: f64,
    /// The ledger's own frequency cell (the store's level 0), Hz. A column narrower than this
    /// replicates one cell's value.
    pub source_f_cell_hz: f64,
    /// The time cell the max-hold is over (the store's `level_t` axis entry used), ns.
    pub t_cell_ns: i64,
    /// Whether the ledger has seen every frame the store ever folded, so [`LedgerColumn::Never`]
    /// and [`LedgerColumn::NothingBefore`] are proof. False for a ledger begun on a store that
    /// already held history (written before T-1058, or whose ledger file was lost).
    pub complete: bool,
    /// Per column.
    pub columns: Vec<LedgerColumn>,
    /// Ledger cells read.
    pub cells_read: usize,
}

impl LedgerAnswer {
    /// Columns with a [`LedgerColumn::Known`] value.
    pub fn known(&self) -> usize {
        self.columns
            .iter()
            .filter(|c| matches!(c, LedgerColumn::Known(_)))
            .count()
    }
}

/// Size and state of a ledger ([`super::Pyramid::ledger_stats`]).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LedgerStats {
    /// Sources with at least one cell.
    pub sources: usize,
    /// Cells ever reached, over every source.
    pub cells: usize,
    /// Resident bytes (blocks allocated).
    pub resident_bytes: usize,
    /// Bytes of the ledger file as last written (0: never written).
    pub file_bytes: u64,
    /// See [`LedgerAnswer::complete`].
    pub complete: bool,
    /// Rows noted since open.
    pub rows_noted: u64,
    /// Whether the ledger was read back from its file at open.
    pub loaded: bool,
}

#[derive(Clone, Debug)]
struct Block {
    /// Per slot; `i64::MIN` = never reached.
    last_tc: Vec<i64>,
    first_tc: Vec<i64>,
    epoch: Vec<u32>,
    /// Per slot × time level.
    db: Vec<f32>,
}

impl Block {
    fn new(nt: usize) -> Self {
        Block {
            last_tc: vec![i64::MIN; BLOCK],
            first_tc: vec![i64::MAX; BLOCK],
            epoch: vec![0; BLOCK],
            db: vec![f32::NAN; BLOCK * nt],
        }
    }

    fn bytes(nt: usize) -> usize {
        BLOCK * (8 + 8 + 4 + 4 * nt) + std::mem::size_of::<Self>()
    }
}

#[derive(Clone, Debug, Default)]
struct SourceLedger {
    epoch: u32,
    /// The window (level-0 cells, `[lo, hi)`) of this source's newest frame; `None` before any.
    window: Option<(i64, i64)>,
    blocks: BTreeMap<i64, Block>,
}

/// The ledger of one store (see the [module docs](self)).
#[derive(Clone, Debug)]
pub(super) struct Ledger {
    f_cell_hz: f64,
    t0_ns: i64,
    /// The store's `level_t` axis, finest first; `[0] == t0_ns`.
    t_cells: Vec<i64>,
    /// Per time level: its cell in level-0 cells, when a whole number of them.
    ratio: Vec<Option<i64>>,
    complete: bool,
    loaded: bool,
    sources: BTreeMap<u64, SourceLedger>,
    cells: usize,
    blocks: usize,
    rows_noted: u64,
    /// Changed since last persisted.
    pub dirty: bool,
    /// Bytes of the file as last written.
    pub file_bytes: u64,
    /// Data time of the last persist, ns (`i64::MIN`: never).
    pub saved_at_ns: i64,
    /// Scratch: the current row's block index per time level.
    row_blk: Vec<i64>,
    /// One-entry memo `(last_tc, block per level)` — in steady state every cell of a row shares
    /// the same previous row, so a row costs one set of divisions, not one per cell.
    memo_tc: i64,
    memo_blk: Vec<i64>,
}

impl Ledger {
    pub fn new(geom: &Geometry) -> Self {
        let t_cells = geom.t_axis();
        let t0 = geom.levels[0].t_cell_ns;
        let ratio = t_cells
            .iter()
            .map(|&t| (t % t0 == 0).then_some(t / t0))
            .collect();
        let nt = t_cells.len();
        Ledger {
            f_cell_hz: geom.levels[0].f_cell_hz,
            t0_ns: t0,
            t_cells,
            ratio,
            complete: true,
            loaded: false,
            sources: BTreeMap::new(),
            cells: 0,
            blocks: 0,
            rows_noted: 0,
            dirty: false,
            file_bytes: 0,
            saved_at_ns: i64::MIN,
            row_blk: vec![0; nt],
            memo_tc: i64::MIN,
            memo_blk: vec![0; nt],
        }
    }

    fn nt(&self) -> usize {
        self.t_cells.len()
    }

    /// Block index of level-0 time cell `tc` at time level `l`.
    fn blk(&self, tc: i64, l: usize) -> i64 {
        match self.ratio[l] {
            Some(r) => tc.div_euclid(r),
            None => {
                let ns = i128::from(tc) * i128::from(self.t0_ns);
                ns.div_euclid(i128::from(self.t_cells[l])) as i64
            }
        }
    }

    /// Marks the ledger as begun on a store that already held history it never saw.
    pub fn set_incomplete(&mut self) {
        if self.complete {
            self.complete = false;
            self.dirty = true;
        }
    }

    pub fn stats(&self) -> LedgerStats {
        LedgerStats {
            sources: self
                .sources
                .values()
                .filter(|s| !s.blocks.is_empty())
                .count(),
            cells: self.cells,
            resident_bytes: self.blocks * Block::bytes(self.nt()),
            file_bytes: self.file_bytes,
            complete: self.complete,
            rows_noted: self.rows_noted,
            loaded: self.loaded,
        }
    }

    /// **Notes one row** — O(cells), on the capture thread (the T-453 path): every cell in `cells`
    /// (global level-0 frequency cell, the max-hold dB the store folded into it) has just been
    /// reached at level-0 time cell `tc`. `window` is the frame's level-0 cell span; a window
    /// different from the source's previous one is a retune, and opens a new tune epoch.
    ///
    /// A frame older than a cell's newest (disorder) only raises the max-holds whose time cell it
    /// shares with the newest; it never moves `last_tc` back.
    pub fn note_row(&mut self, source: u64, tc: i64, window: (i64, i64), cells: &[(i64, f32)]) {
        if cells.is_empty() {
            return;
        }
        let nt = self.nt();
        for l in 0..nt {
            self.row_blk[l] = self.blk(tc, l);
        }
        let Self {
            sources,
            row_blk,
            memo_tc,
            memo_blk,
            ratio,
            t_cells,
            t0_ns,
            cells: n_cells,
            blocks: n_blocks,
            ..
        } = self;
        let blk_of = |tc: i64, l: usize| match ratio[l] {
            Some(r) => tc.div_euclid(r),
            None => {
                let ns = i128::from(tc) * i128::from(*t0_ns);
                ns.div_euclid(i128::from(t_cells[l])) as i64
            }
        };
        let src = sources.entry(source).or_default();
        match src.window {
            Some(w) if w == window => {}
            Some(_) => {
                src.epoch = src.epoch.wrapping_add(1);
                src.window = Some(window);
            }
            None => src.window = Some(window),
        }
        let epoch = src.epoch;
        let mut i = 0;
        while i < cells.len() {
            let b = cells[i].0.div_euclid(BLOCK as i64);
            let block = src.blocks.entry(b).or_insert_with(|| {
                *n_blocks += 1;
                Block::new(nt)
            });
            while i < cells.len() && cells[i].0.div_euclid(BLOCK as i64) == b {
                let (c, v) = cells[i];
                i += 1;
                if !v.is_finite() {
                    continue;
                }
                let slot = (c - b * BLOCK as i64) as usize;
                let last = block.last_tc[slot];
                let dbs = &mut block.db[slot * nt..(slot + 1) * nt];
                if last == i64::MIN {
                    block.last_tc[slot] = tc;
                    block.first_tc[slot] = tc;
                    block.epoch[slot] = epoch;
                    dbs.fill(v);
                    *n_cells += 1;
                    continue;
                }
                if last == tc {
                    for d in dbs.iter_mut() {
                        *d = d.max(v);
                    }
                    block.epoch[slot] = epoch;
                    continue;
                }
                if *memo_tc != last {
                    *memo_tc = last;
                    for (l, m) in memo_blk.iter_mut().enumerate() {
                        *m = blk_of(last, l);
                    }
                }
                if tc > last {
                    for (l, d) in dbs.iter_mut().enumerate() {
                        *d = if row_blk[l] == memo_blk[l] {
                            d.max(v)
                        } else {
                            v
                        };
                    }
                    block.last_tc[slot] = tc;
                    block.epoch[slot] = epoch;
                } else {
                    for (l, d) in dbs.iter_mut().enumerate() {
                        if row_blk[l] == memo_blk[l] {
                            *d = d.max(v);
                        }
                    }
                }
                if tc < block.first_tc[slot] {
                    block.first_tc[slot] = tc;
                }
            }
        }
        self.rows_noted += 1;
        self.dirty = true;
    }

    /// The time level a tile of `t_cell_ns` rows reads: the coarsest of the store's time cells
    /// that divides it (its own, when the store has one), else the finest.
    fn level_for(&self, t_cell_ns: i64) -> usize {
        (0..self.nt())
            .rev()
            .find(|&l| {
                let t = self.t_cells[l];
                t > 0 && t <= t_cell_ns && t_cell_ns % t == 0
            })
            .unwrap_or(0)
    }

    /// **Per column of `freq` (`nf` of them), what is known before `before_ns`** — see
    /// [`LedgerColumn`]. `source` scopes to one front end; `None` is every source, the newest
    /// winning. `t_cell_ns` is the row the caller draws at: the max-hold is over the store's time
    /// cell of that size, so the value is the one the store's own cell holds for the last row.
    /// `before_ns = i64::MAX` asks for the newest value, whenever it was.
    pub fn columns(
        &self,
        source: Option<u64>,
        freq: FreqRange,
        nf: usize,
        t_cell_ns: i64,
        before_ns: i64,
    ) -> LedgerAnswer {
        let nf = nf.max(1);
        let span = (freq.hi_hz - freq.lo_hz).max(0.0);
        let w = span / nf as f64;
        let l = self.level_for(t_cell_ns);
        let nt = self.nt();
        let f0 = self.f_cell_hz;
        #[derive(Clone, Copy)]
        struct Acc {
            any: bool,
            best_blk: i64,
            best_db: f32,
            newest_tc: i64,
            epoch: u32,
            source: u64,
            first_tc: i64,
        }
        let mut acc = vec![
            Acc {
                any: false,
                best_blk: i64::MIN,
                best_db: f32::NAN,
                newest_tc: i64::MIN,
                epoch: 0,
                source: 0,
                first_tc: i64::MAX,
            };
            nf
        ];
        let mut read = 0usize;
        let eps = 1e-9;
        for (&key, src) in &self.sources {
            if source.is_some_and(|s| s != key) || src.blocks.is_empty() {
                continue;
            }
            for (k, a) in acc.iter_mut().enumerate() {
                let lo = freq.lo_hz + k as f64 * w;
                let hi = lo + w;
                let c0 = (lo / f0 + eps).floor() as i64;
                let c1 = ((hi / f0 - eps).ceil() as i64).max(c0 + 1);
                let (b0, b1) = (
                    c0.div_euclid(BLOCK as i64),
                    (c1 - 1).div_euclid(BLOCK as i64),
                );
                for (&b, block) in src.blocks.range(b0..=b1) {
                    let base = b * BLOCK as i64;
                    let s0 = (c0 - base).clamp(0, BLOCK as i64) as usize;
                    let s1 = (c1 - base).clamp(0, BLOCK as i64) as usize;
                    for slot in s0..s1 {
                        let last = block.last_tc[slot];
                        if last == i64::MIN {
                            continue;
                        }
                        read += 1;
                        let d = block.db[slot * nt + l];
                        let bk = self.blk(last, l);
                        a.any = true;
                        if bk > a.best_blk {
                            a.best_blk = bk;
                            a.best_db = d;
                        } else if bk == a.best_blk {
                            a.best_db = a.best_db.max(d);
                        }
                        if last > a.newest_tc {
                            a.newest_tc = last;
                            a.epoch = block.epoch[slot];
                            a.source = key;
                        }
                        a.first_tc = a.first_tc.min(block.first_tc[slot]);
                    }
                }
            }
        }
        let t0 = self.t0_ns;
        let columns = acc
            .iter()
            .map(|a| {
                if !a.any {
                    return LedgerColumn::Never;
                }
                let newest_ns = a.newest_tc.saturating_add(1).saturating_mul(t0);
                let first_ns = a.first_tc.saturating_mul(t0);
                if newest_ns <= before_ns {
                    LedgerColumn::Known(LedgerValue {
                        // At the store's stored resolution (0.01 dB), exactly as every cell a
                        // query returns — so a shadow is the very number its last row was drawn
                        // with, sealed tile or open.
                        max_db: dq_db(q_db(a.best_db)),
                        last_ns: newest_ns,
                        first_ns,
                        epoch: a.epoch,
                        source: a.source,
                    })
                } else if first_ns >= before_ns {
                    LedgerColumn::NothingBefore { first_ns }
                } else {
                    LedgerColumn::Later { newest_ns }
                }
            })
            .collect();
        LedgerAnswer {
            f_lo_hz: freq.lo_hz,
            f_cell_hz: w,
            source_f_cell_hz: f0,
            t_cell_ns: self.t_cells[l],
            complete: self.complete,
            columns,
            cells_read: read,
        }
    }

    /// The ledger file's bytes: CRC-32 of the rest, then a zstd frame of the payload.
    pub fn encode(&self) -> Vec<u8> {
        let nt = self.nt();
        let mut p = Vec::with_capacity(64 + self.cells * (20 + 2 * nt));
        p.extend_from_slice(MAGIC);
        p.extend_from_slice(&VERSION.to_le_bytes());
        p.push(u8::from(self.complete));
        p.extend_from_slice(&self.f_cell_hz.to_le_bytes());
        p.extend_from_slice(&self.t0_ns.to_le_bytes());
        p.extend_from_slice(&(nt as u16).to_le_bytes());
        for t in &self.t_cells {
            p.extend_from_slice(&t.to_le_bytes());
        }
        p.extend_from_slice(&(self.sources.len() as u32).to_le_bytes());
        for (&key, src) in &self.sources {
            p.extend_from_slice(&key.to_le_bytes());
            p.extend_from_slice(&src.epoch.to_le_bytes());
            let (w0, w1) = src.window.unwrap_or((i64::MIN, i64::MIN));
            p.extend_from_slice(&w0.to_le_bytes());
            p.extend_from_slice(&w1.to_le_bytes());
            p.extend_from_slice(&(src.blocks.len() as u32).to_le_bytes());
            for (&b, block) in &src.blocks {
                p.extend_from_slice(&b.to_le_bytes());
                let mut bitmap = [0u8; BLOCK / 8];
                for s in 0..BLOCK {
                    if block.last_tc[s] != i64::MIN {
                        bitmap[s / 8] |= 1 << (s % 8);
                    }
                }
                p.extend_from_slice(&bitmap);
                for s in 0..BLOCK {
                    if block.last_tc[s] == i64::MIN {
                        continue;
                    }
                    p.extend_from_slice(&block.last_tc[s].to_le_bytes());
                    p.extend_from_slice(&block.first_tc[s].to_le_bytes());
                    p.extend_from_slice(&block.epoch[s].to_le_bytes());
                    for d in &block.db[s * nt..(s + 1) * nt] {
                        p.extend_from_slice(&q_db(*d).to_le_bytes());
                    }
                }
            }
        }
        let z = zstd::bulk::compress(&p, 1).unwrap_or_default();
        let mut out = Vec::with_capacity(z.len() + 12);
        out.extend_from_slice(&crc32(&z).to_le_bytes());
        out.extend_from_slice(&(p.len() as u64).to_le_bytes());
        out.extend_from_slice(&z);
        out
    }

    /// Reads a ledger file back. `None` when it is corrupt or was written under another geometry —
    /// the caller then starts an incomplete ledger (see the [module docs](self)).
    pub fn decode(&self, bytes: &[u8]) -> Option<Ledger> {
        let crc = u32::from_le_bytes(bytes.get(0..4)?.try_into().ok()?);
        let raw_len = u64::from_le_bytes(bytes.get(4..12)?.try_into().ok()?);
        let z = bytes.get(12..)?;
        if crc32(z) != crc || raw_len > (1 << 32) {
            return None;
        }
        let p = zstd::bulk::decompress(z, raw_len as usize).ok()?;
        let mut r = Reader { b: &p, at: 0 };
        if r.take(4)? != MAGIC || r.u16()? != VERSION {
            return None;
        }
        let complete = r.take(1)?[0] != 0;
        let f0 = f64::from_le_bytes(r.take(8)?.try_into().ok()?);
        let t0 = r.i64()?;
        let nt = usize::from(r.u16()?);
        let mut t_cells = Vec::with_capacity(nt);
        for _ in 0..nt {
            t_cells.push(r.i64()?);
        }
        if f0 != self.f_cell_hz || t0 != self.t0_ns || t_cells != self.t_cells {
            return None;
        }
        let mut out = Ledger::new_like(self);
        out.complete = complete;
        let n_src = r.u32()?;
        for _ in 0..n_src {
            let key = r.u64()?;
            let epoch = r.u32()?;
            let (w0, w1) = (r.i64()?, r.i64()?);
            let n_blocks = r.u32()?;
            let mut src = SourceLedger {
                epoch,
                window: (w0 != i64::MIN).then_some((w0, w1)),
                blocks: BTreeMap::new(),
            };
            for _ in 0..n_blocks {
                let b = r.i64()?;
                let bitmap: [u8; BLOCK / 8] = r.take(BLOCK / 8)?.try_into().ok()?;
                let mut block = Block::new(nt);
                for s in 0..BLOCK {
                    if bitmap[s / 8] & (1 << (s % 8)) == 0 {
                        continue;
                    }
                    block.last_tc[s] = r.i64()?;
                    block.first_tc[s] = r.i64()?;
                    block.epoch[s] = r.u32()?;
                    for l in 0..nt {
                        block.db[s * nt + l] = dq_db(r.u16()? as i16);
                    }
                    out.cells += 1;
                }
                out.blocks += 1;
                src.blocks.insert(b, block);
            }
            out.sources.insert(key, src);
        }
        (r.at == p.len()).then_some(())?;
        out.loaded = true;
        out.file_bytes = bytes.len() as u64;
        Some(out)
    }

    fn new_like(other: &Ledger) -> Ledger {
        let nt = other.nt();
        Ledger {
            f_cell_hz: other.f_cell_hz,
            t0_ns: other.t0_ns,
            t_cells: other.t_cells.clone(),
            ratio: other.ratio.clone(),
            complete: true,
            loaded: false,
            sources: BTreeMap::new(),
            cells: 0,
            blocks: 0,
            rows_noted: 0,
            dirty: false,
            file_bytes: 0,
            saved_at_ns: i64::MIN,
            row_blk: vec![0; nt],
            memo_tc: i64::MIN,
            memo_blk: vec![0; nt],
        }
    }
}

struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.b.get(self.at..self.at.checked_add(n)?)?;
        self.at += n;
        Some(s)
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn i64(&mut self) -> Option<i64> {
        Some(i64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
}
