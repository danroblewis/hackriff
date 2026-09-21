//! The [`Pyramid`]: ingest, sealing and rollup, rolling byte budget, checkpoints and recovery.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use hk_model::{FreqRange, TileKey, TimeRange, Timestamp};

use super::StoreError;
use super::codec;
use super::config::{Geometry, PyramidConfig, RetentionOverride};
use super::frame::{FrameInput, NoiseShape, RegridPlan};
use super::stats::{db, hist_percentile};
use super::tile::{ColEntry, FrontEndState, ProvenanceStep, ProvenanceSummary, Tile};

/// Counters.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PyramidStats {
    /// Frames folded into level 0.
    pub frames_folded: u64,
    /// Frames dropped because their level-0 tile had already sealed.
    pub frames_late: u64,
    /// Frames rejected as malformed or in the wrong unit.
    pub frames_rejected: u64,
    /// Sealed tiles written.
    pub tiles_written: u64,
    /// T-453: coarse tiles built **on demand** by [`Pyramid::materialize`] rather than at a seal.
    /// Counts both the ones persisted (their time block had fully elapsed) and the transient
    /// live-edge ones, which is the point of the counter: it is the work the read path pays in
    /// exchange for the work capture no longer does.
    pub tiles_materialized: u64,
    /// T-571: **producer tiles consulted to build a coarse tile**, over the whole run. A read-time
    /// fold pays up to [`MAX_MATERIALIZE_TILES`] of these for ONE coarse tile; live maintenance
    /// pays none, because the tile it would have folded already exists.
    pub producer_tiles_folded: u64,
    /// T-571: tiles consulted to **answer a query** — open, derived or read back from disk. One
    /// per tile address the query covers is what an ordinary read costs.
    pub source_tiles_read: u64,
    /// T-571: producer rows folded into a coarse node by live maintenance. Bounded per level and
    /// per arriving row: the fold that used to happen in a batch, spread one row at a time.
    pub coarse_rows_folded: u64,
    /// T-571: producer **cells** scanned by live maintenance. This is the per-arriving-row cost
    /// the invariant says must be measured rather than assumed (T-453).
    pub coarse_cells_folded: u64,
    /// Open level-0 tiles checkpointed.
    pub checkpoints_written: u64,
    /// Bytes written (sealed tiles and checkpoints).
    pub bytes_written: u64,
    /// Bytes the same files would have taken with raw payloads (T-116): the compression ratio is
    /// `raw_bytes_written / bytes_written`.
    pub raw_bytes_written: u64,
    /// Tiles evicted by budget, quota or age (all reasons).
    pub tiles_evicted: u64,
    /// Of those, expired by age (level `max_age` or a region override).
    pub tiles_expired: u64,
    /// Of those, evicted by a level byte quota.
    pub tiles_evicted_quota: u64,
    /// Region-protected tiles rewritten with their expired unprotected cells cleared (T-126).
    pub tiles_trimmed: u64,
    /// Bytes evicted.
    pub bytes_evicted: u64,
    /// Temp, truncated or corrupt files ignored (and removed) on open or read.
    pub files_ignored: u64,
    /// T-377: sealed level-0 tiles found on open that were written **before** per-origin floor
    /// tracking (tile format < [`super::codec::FORMAT_VERSION`]). Their stored `occupancy` was
    /// decided against a floor every front end fed, and it cannot be recomputed from the tile —
    /// the level values are there, the floor they were compared with is not. Nonzero means part
    /// of the history predates the guarantee; in a pyramid only one front end ever fed, the two
    /// rules give the same answer, so this is a disclosure, not a defect count.
    pub tiles_pre_origin_floor: u64,
    /// T-377: sealed level-0 tiles that fed **no** front end's floor track because they could not
    /// name one source (two front ends interleaved inside one tile, or unrecorded origins).
    pub floor_tiles_unattributed: u64,
    /// After the last enforcement, the budget could not be met without evicting tiles that no
    /// coarser level covers yet.
    pub over_budget: bool,
    /// After the last enforcement, some level's byte quota could not be met without evicting
    /// protected or uncovered tiles.
    pub over_quota: bool,
}

/// What [`Pyramid::ingest`] did with a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IngestOutcome {
    /// Folded into level 0.
    Folded,
    /// Its level-0 tile had already sealed; dropped and counted.
    Late,
}

/// Per-(source, f-block) default floor: min over the last `w` sealed tiles' per-cell low
/// percentile, over the tiles of **one** front end.
///
/// **T-377.** A noise floor is a property of one receive chain (T-303, ADR-0012 §3.1), and this
/// tracker decides a level-0 cell's stored `occupancy` at *ingest*, where no read-side filter can
/// reach it afterwards. Keyed only by frequency block it was fed by every front end that folded
/// into the pyramid, so a tile pure in origin could still carry an occupancy decided against
/// another chain's floor. The key is therefore `(FrameInput::source, f_block)` — the same
/// `source_key(device_id)` the occupancy read and `BaselineKey`'s `ChainKey` use (T-314) — so the
/// threshold, the measurement and the baseline key are one value by construction.
#[derive(Clone, Debug)]
pub(super) struct FloorTrack {
    w: usize,
    nf: usize,
    ring: Vec<f32>,
    head: usize,
    pub floor: Vec<f32>,
}

impl FloorTrack {
    fn new(w: usize, nf: usize) -> Self {
        Self {
            w,
            nf,
            ring: vec![f32::NAN; w * nf],
            head: 0,
            floor: vec![f32::NAN; nf],
        }
    }

    fn push(&mut self, value: impl Fn(usize) -> f32) {
        for f in 0..self.nf {
            self.ring[self.head * self.nf + f] = value(f);
        }
        self.head = (self.head + 1) % self.w;
        for f in 0..self.nf {
            self.floor[f] = (0..self.w)
                .map(|k| self.ring[k * self.nf + f])
                .filter(|v| v.is_finite())
                .fold(f32::NAN, f32::min);
        }
    }
}

/// The spectrum-history pyramid under one data directory (see the [module docs](super)).
pub struct Pyramid {
    pub(super) cfg: PyramidConfig,
    pub(super) geom: Geometry,
    pub(super) root: PathBuf,
    /// Open tiles per level, keyed `(f_block, t_block)`.
    pub(super) open: Vec<HashMap<(i64, i64), Box<Tile>>>,
    /// T-453: coarse tiles built on demand whose time block has **not** elapsed — the live edge,
    /// which `docs/16` §5.2 says is computed on request precisely because it cannot be
    /// precomputed. Keyed `(level, f_block, t_block)`, never written, and dropped the moment the
    /// data they were folded from can have changed (see [`Pyramid::invalidate_derived`]), so a
    /// growing edge can never be served from a stale summary.
    pub(super) derived: HashMap<(usize, i64, i64), Box<Tile>>,
    /// Sealed tiles on disk per level, keyed `(t_block, f_block)` → bytes.
    pub(super) sealed: Vec<BTreeMap<(i64, i64), u64>>,
    /// Level-0 checkpoint files of open tiles, `(f_block, t_block)` → bytes.
    checkpoints: HashMap<(i64, i64), u64>,
    disk_bytes: u64,
    /// Sealed bytes per level.
    level_bytes: Vec<u64>,
    /// Boxed so tiles move between the open maps and the pool without copying.
    #[allow(clippy::vec_box)]
    pool: Vec<Vec<Box<Tile>>>,
    plan: RegridPlan,
    /// Default level-0 floor per `(source, f_block)` (T-377): see [`FloorTrack`].
    pub(super) floors: HashMap<(u64, i64), FloorTrack>,
    /// Every tile whose block ends at or before this has sealed.
    watermark_ns: i64,
    latest_ns: i64,
    /// T-507: when this store began recording, as it knew it **when it was opened** — the
    /// persisted [`RECORDING_BEGAN_FILE`], else (a store written before T-507) the start of the
    /// oldest block it held. `None`: opened empty.
    resumed_from_ns: Option<i64>,
    /// T-507: [`Pyramid::recording_began`] is on disk, so it is never written again.
    began_persisted: bool,
    /// T-507: the start of the earliest frame folded by this process (`i64::MAX`: none yet).
    first_folded_ns: i64,
    next_seal_ns: i64,
    last_checkpoint_ns: Option<i64>,
    scratch: Vec<f32>,
    group_hist: Vec<u32>,
    keys: Vec<(i64, i64)>,
    /// Scratch copy of a level's consumer list, so a fold may borrow `open`/`pool` mutably.
    consumers: Vec<usize>,
    /// T-571: the live row cascade's work queue, `(level, f_block, t_block, row, f_lo, f_hi)`.
    row_queue: std::collections::VecDeque<(usize, i64, i64, usize, usize, usize)>,
    /// T-571: rows one [`Tile::fold_row`] call completed.
    row_out: Vec<(usize, usize, usize)>,
    /// T-571: level-0 columns one [`Pyramid::ingest`] call closed, `(f_block, t_block, row)`.
    closed_rows: Vec<(i64, i64, usize)>,
    /// T-571: tiles consulted to answer a query. Counted behind a shared reference because the
    /// read path takes `&self`; see [`Pyramid::source_tiles_read`].
    source_tiles: std::sync::atomic::AtomicU64,
    buf: Vec<u8>,
    payload: Vec<u8>,
    /// Front-end state and resolved cell shape of the last folded frame **per source** (step
    /// detection, T-116; keyed and persisted per source, T-126).
    last_state: HashMap<u64, (FrontEndState, Option<f32>)>,
    state_dirty: bool,
    /// Retention schedule per level (T-126): `(action ns, t_block, f_block)` for sealed tiles with
    /// a finite trim or expiry deadline. Entries of evicted tiles are dropped when reached.
    due: Vec<BTreeSet<(i64, i64, i64)>>,
    /// Sealed tiles no region override protects, per level, `(t_block, f_block)`.
    unprotected: Vec<BTreeSet<(i64, i64)>>,
    /// Retention ages per `(level, f_block)`.
    ages: HashMap<(usize, i64), Arc<Ages>>,
    /// Tiles examined by retention passes (cost counter).
    pub(super) retention_visits: u64,
    /// Cell shape of the last STFT resolution seen.
    shape_cache: Option<(hk_dsp::spectrum::Resolution, f32)>,
    stats: PyramidStats,
}

/// Retention ages of one `(level, f_block)` under the region overrides (T-126).
#[derive(Debug)]
pub(super) struct Ages {
    /// Some override overlaps the block (quota-exempt, budget-last).
    protected: bool,
    /// Age after which the whole tile expires (`None`: some cell is kept forever).
    tile_age: Option<i64>,
    /// Distinct finite cell ages, ascending: each is a trim deadline, the last may be the expiry.
    deadlines: Vec<i64>,
}

/// File of the per-source front-end state (T-126), under the scheme root.
const SOURCE_STATE_FILE: &str = "front_end.state";

/// File of [`Pyramid::recording_began`] (T-507), under the scheme root: 8 bytes, the Unix ns of
/// the earliest frame the store ever folded, little-endian.
pub(super) const RECORDING_BEGAN_FILE: &str = "recording_began";

/// Producer tiles one [`Pyramid::materialize`] call may fold, over the whole recursion.
///
/// The shipped 4 × 4 view lattice's *coarsest* node needs 127 (64 level-0 tiles and the 63
/// intermediates on the canonical path), and the intermediates are sealed on the way so the next
/// address pays a fraction of it. The cap is what stops an address into a scheme with a deep ladder
/// turning one tile request into an unbounded read; it is an error rather than a silent partial,
/// because a partial fold is a tile that says *unobserved* about data the store holds.
///
/// **Public since T-482**, because a route that declares how far up it can be read has to be able
/// to ask this question *before* it answers — see [`Pyramid::materialize_cost_bound`].
pub const MAX_MATERIALIZE_TILES: usize = 1024;

fn dur_ns(d: std::time::Duration) -> i64 {
    i64::try_from(d.as_nanos()).unwrap_or(i64::MAX)
}

/// A cell `[lo, hi)`'s age: the longest override overlapping it, else the level age.
fn cell_age(ovs: &[RetentionOverride], lo: f64, hi: f64, level_age: Option<i64>) -> Option<i64> {
    ovs.iter()
        .filter(|o| o.freq.lo_hz < hi && o.freq.hi_hz > lo)
        .map(|o| dur_ns(o.max_age))
        .max()
        .or(level_age)
}

fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> StoreError + '_ {
    move |source| StoreError::Io {
        path: path.to_owned(),
        source,
    }
}

#[allow(clippy::too_many_arguments, clippy::vec_box)]
fn open_tile<'m>(
    map: &'m mut HashMap<(i64, i64), Box<Tile>>,
    pool: &mut Vec<Box<Tile>>,
    geom: &Geometry,
    level: usize,
    scheme: u16,
    bins: usize,
    fb: i64,
    tb: i64,
    next_seal: &mut i64,
) -> &'m mut Tile {
    map.entry((fb, tb)).or_insert_with(|| {
        let key = TileKey {
            scheme,
            level: level as u8,
            f_block: fb,
            t_block: tb,
        };
        let g = &geom.levels[level];
        *next_seal = (*next_seal).min(geom.block_end_ns(level, tb));
        match pool.pop() {
            Some(mut t) => {
                t.reset(key, g);
                t
            }
            None => Box::new(Tile::new(key, geom.nf, g, bins)),
        }
    })
}

impl Pyramid {
    /// Opens (or creates) the pyramid for `config.scheme` under `data_dir/history/s<scheme>/`.
    ///
    /// Scans the tile index once (headers only), removes temp and invalid files, reloads level-0
    /// checkpoints as open tiles, rebuilds open coarser tiles from their sealed children, and
    /// seals whatever is due. Fails with [`StoreError::SchemeMismatch`] if a tile of this scheme
    /// was written with different geometry.
    pub fn open(data_dir: impl AsRef<Path>, config: PyramidConfig) -> Result<Self, StoreError> {
        let geom = config.geometry()?;
        let root = data_dir
            .as_ref()
            .join("history")
            .join(format!("s{}", config.scheme));
        fs::create_dir_all(&root).map_err(io_err(&root))?;
        let n = geom.n_levels();
        let bins = usize::from(config.histogram.bins);
        let mut p = Self {
            open: (0..n).map(|_| HashMap::new()).collect(),
            derived: HashMap::new(),
            sealed: (0..n).map(|_| BTreeMap::new()).collect(),
            checkpoints: HashMap::new(),
            disk_bytes: 0,
            level_bytes: vec![0; n],
            pool: (0..n).map(|_| Vec::new()).collect(),
            plan: RegridPlan::default(),
            floors: HashMap::new(),
            watermark_ns: i64::MIN,
            latest_ns: i64::MIN,
            resumed_from_ns: None,
            began_persisted: false,
            first_folded_ns: i64::MAX,
            next_seal_ns: i64::MAX,
            last_checkpoint_ns: None,
            scratch: Vec::new(),
            group_hist: vec![0; bins],
            keys: Vec::new(),
            consumers: Vec::new(),
            row_queue: std::collections::VecDeque::new(),
            row_out: Vec::new(),
            closed_rows: Vec::new(),
            source_tiles: std::sync::atomic::AtomicU64::new(0),
            buf: Vec::new(),
            payload: Vec::new(),
            last_state: HashMap::new(),
            state_dirty: false,
            due: (0..n).map(|_| BTreeSet::new()).collect(),
            unprotected: (0..n).map(|_| BTreeSet::new()).collect(),
            ages: HashMap::new(),
            retention_visits: 0,
            shape_cache: None,
            stats: PyramidStats::default(),
            cfg: config,
            geom,
            root,
        };
        p.scan()?;
        p.load_source_states();
        p.recover()?;
        p.load_recording_began();
        Ok(p)
    }

    /// Reads [`RECORDING_BEGAN_FILE`]; without one, a store that already holds tiles (written
    /// before T-507) is bounded by the start of its oldest block — a lower bound, so it can only
    /// widen `"unknown whether we looked"`, never claim `"nothing looked"` over a recorded span.
    fn load_recording_began(&mut self) {
        let path = self.root.join(RECORDING_BEGAN_FILE);
        let persisted = fs::read(&path)
            .ok()
            .and_then(|b| <[u8; 8]>::try_from(b.as_slice()).ok())
            .map(i64::from_le_bytes);
        self.began_persisted = persisted.is_some();
        self.resumed_from_ns = persisted.or_else(|| self.earliest_held_ns());
    }

    /// Writes [`Pyramid::recording_began`] once, the first time there is one to write (temp →
    /// fsync → rename, like tiles). Rides the seal/checkpoint path that already writes the
    /// per-source state, so the capture thread pays it once per store, not per frame.
    fn save_recording_began(&mut self) -> Result<(), StoreError> {
        if self.began_persisted {
            return Ok(());
        }
        let Some(t) = self.recording_began() else {
            return Ok(());
        };
        let path = self.root.join(RECORDING_BEGAN_FILE);
        let tmp = self
            .root
            .join(format!("{RECORDING_BEGAN_FILE}.tmp{}", std::process::id()));
        let write = || -> std::io::Result<()> {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(&t.as_unix_nanos().to_le_bytes())?;
            f.sync_all()?;
            fs::rename(&tmp, &path)
        };
        if let Err(e) = write() {
            let _ = fs::remove_file(&tmp);
            return Err(StoreError::Io { path, source: e });
        }
        self.began_persisted = true;
        Ok(())
    }

    /// The start of the oldest time block any level holds, sealed or open, ns.
    fn earliest_held_ns(&self) -> Option<i64> {
        let sealed = self.sealed.iter().enumerate().filter_map(|(level, m)| {
            m.first_key_value()
                .map(|(&(tb, _), _)| tb.saturating_mul(self.geom.levels[level].t_block_ns()))
        });
        let open = self.open.iter().enumerate().flat_map(|(level, m)| {
            let t = self.geom.levels[level].t_block_ns();
            m.keys().map(move |&(_, tb)| tb.saturating_mul(t))
        });
        sealed.chain(open).min()
    }

    /// **When this store began recording** (T-507): the earliest frame it has ever folded,
    /// persisted across restarts in [`RECORDING_BEGAN_FILE`]; `None` when it has never held a
    /// frame.
    ///
    /// It is a recorded fact, **not** re-derived from the tiles held now, so evicting a tile does
    /// not move it: the store still knows it began recording then. It is the boundary the coverage
    /// map needs to tell *"nothing looked"* (before this installation recorded anything) from *"we
    /// no longer know whether we looked"* (after it, where a tune record may since have been
    /// discarded). A store written before T-507 has no file; its oldest held block's start stands
    /// in, a lower bound that can only widen the second, never claim the first.
    pub fn recording_began(&self) -> Option<Timestamp> {
        let folded = (self.first_folded_ns != i64::MAX).then_some(self.first_folded_ns);
        match (self.resumed_from_ns, folded) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
        .map(Timestamp::from_unix_nanos)
    }

    /// The front-end state and resolved level-0 cell shape of the last frame folded from `source`
    /// ([`FrameInput::source`]), including state persisted before a restart (T-126).
    pub fn source_state(&self, source: u64) -> Option<(FrontEndState, Option<f32>)> {
        self.last_state.get(&source).copied()
    }

    fn load_source_states(&mut self) {
        let path = self.root.join(SOURCE_STATE_FILE);
        let Ok(bytes) = fs::read(&path) else {
            return;
        };
        match codec::decode_source_states(&bytes) {
            Some(states) => {
                self.last_state = states.into_iter().map(|(s, st, k)| (s, (st, k))).collect();
            }
            None => {
                let _ = fs::remove_file(&path);
                self.stats.files_ignored += 1;
            }
        }
    }

    /// Writes the per-source state when it changed (temp → fsync → rename, like tiles).
    fn save_source_states(&mut self) -> Result<(), StoreError> {
        if !self.state_dirty {
            return Ok(());
        }
        let mut states: Vec<_> = self
            .last_state
            .iter()
            .map(|(&s, &(st, k))| (s, st, k))
            .collect();
        states.sort_unstable_by_key(|e| e.0);
        let bytes = codec::encode_source_states(&states);
        let path = self.root.join(SOURCE_STATE_FILE);
        let tmp = self
            .root
            .join(format!("{SOURCE_STATE_FILE}.tmp{}", std::process::id()));
        let write = || -> std::io::Result<()> {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(&bytes)?;
            f.sync_all()?;
            fs::rename(&tmp, &path)
        };
        if let Err(e) = write() {
            let _ = fs::remove_file(&tmp);
            return Err(StoreError::Io { path, source: e });
        }
        self.state_dirty = false;
        Ok(())
    }

    /// Settings.
    pub fn config(&self) -> &PyramidConfig {
        &self.cfg
    }

    /// Derived geometry.
    pub fn geometry(&self) -> &Geometry {
        &self.geom
    }

    /// Counters.
    /// T-571: tiles consulted to answer queries since [`Pyramid::reset_source_tiles_read`].
    pub fn source_tiles_read(&self) -> u64 {
        self.source_tiles.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Zeroes [`Pyramid::source_tiles_read`], so one read can be counted on its own.
    pub fn reset_source_tiles_read(&self) {
        self.source_tiles
            .store(0, std::sync::atomic::Ordering::Relaxed);
    }

    pub(super) fn count_source_tile(&self) {
        self.source_tiles
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn stats(&self) -> &PyramidStats {
        &self.stats
    }

    /// Bytes of tile files on disk (sealed tiles and checkpoints).
    pub fn disk_bytes(&self) -> u64 {
        self.disk_bytes
    }

    /// Bytes of sealed tiles at `level`.
    pub fn level_bytes(&self, level: usize) -> u64 {
        self.level_bytes.get(level).copied().unwrap_or(0)
    }

    /// Every tile ending at or before this instant has sealed; frames for them are late.
    pub fn watermark(&self) -> Timestamp {
        Timestamp::from_unix_nanos(self.watermark_ns)
    }

    /// The stream time the history has reached: the end of the newest folded frame (on reopen,
    /// the watermark), `None` before any. Replays and time-compressed scenes run on their own
    /// clock, so "the last N seconds" means this, not the wall clock (T-125).
    pub fn latest_frame_end(&self) -> Option<Timestamp> {
        (self.latest_ns != i64::MIN).then(|| Timestamp::from_unix_nanos(self.latest_ns))
    }

    /// Keys of the sealed tiles on disk at `level`, oldest first.
    pub fn sealed_keys(&self, level: usize) -> Vec<TileKey> {
        self.sealed.get(level).map_or_else(Vec::new, |m| {
            m.keys().map(|&(tb, fb)| self.key(level, fb, tb)).collect()
        })
    }

    /// Keys of the open (in-memory) tiles at `level`.
    pub fn open_keys(&self, level: usize) -> Vec<TileKey> {
        self.open.get(level).map_or_else(Vec::new, |m| {
            let mut v: Vec<_> = m.keys().map(|&(fb, tb)| self.key(level, fb, tb)).collect();
            v.sort_by_key(|k| (k.t_block, k.f_block));
            v
        })
    }

    fn key(&self, level: usize, f_block: i64, t_block: i64) -> TileKey {
        TileKey {
            scheme: self.cfg.scheme,
            level: level as u8,
            f_block,
            t_block,
        }
    }

    /// Path of a tile file: `<root>/L<level>/f<f_block>/t<t_block>.tile`. Addressing is the index:
    /// no file is found by scanning.
    pub fn tile_path(&self, key: TileKey) -> PathBuf {
        self.path(usize::from(key.level), key.f_block, key.t_block)
    }

    fn path(&self, level: usize, fb: i64, tb: i64) -> PathBuf {
        self.root
            .join(format!("L{level}"))
            .join(format!("f{fb}"))
            .join(format!("t{tb}.tile"))
    }

    fn pct(&self) -> (f32, f32) {
        (self.cfg.low_percentile, self.cfg.high_percentile)
    }

    /// Folds one frame into level 0 (see [`super::frame`] for the regrid rules).
    ///
    /// Allocation-free in steady state: the regrid plan is cached per frame geometry, tile
    /// accumulators are pooled, and column buffers keep their capacity. Sealing (at most once per
    /// level-0 tile duration) writes files and may allocate.
    pub fn ingest(&mut self, frame: &FrameInput<'_>) -> Result<IngestOutcome, StoreError> {
        if let Err(why) = frame.validate() {
            self.stats.frames_rejected += 1;
            return Err(StoreError::BadFrame(why));
        }
        if frame.unit != self.cfg.unit {
            self.stats.frames_rejected += 1;
            return Err(StoreError::BadFrame("unit differs from the pyramid's"));
        }
        let g0 = self.geom.levels[0];
        let nf = self.geom.nf as i64;
        let t_mid = frame
            .t
            .as_unix_nanos()
            .saturating_add(frame.duration_ns / 2);
        let tc = t_mid.div_euclid(g0.t_cell_ns);
        let tb = tc.div_euclid(g0.nt as i64);
        let t_in = (tc - tb * g0.nt as i64) as usize;
        if self.geom.block_end_ns(0, tb) <= self.watermark_ns {
            self.stats.frames_late += 1;
            return Ok(IngestOutcome::Late);
        }
        let state = FrontEndState::of(frame);
        let cell_shape = self.resolve_shape(frame.noise_shape, frame.bin_width_hz);
        let prev = self.last_state.insert(frame.source, (state, cell_shape));
        let step = match prev {
            Some((from, _)) if from.changes(&state) != 0 => Some(ProvenanceStep {
                t: frame.t,
                changed: from.changes(&state),
                from,
                to: state,
            }),
            _ => None,
        };
        if prev.is_none_or(|(s, k)| s != state || k != cell_shape) {
            self.state_dirty = true;
        }
        self.plan.ensure(
            frame.f_lo_hz,
            frame.bin_width_hz,
            frame.psd.len(),
            g0.f_cell_hz,
        );
        let dur_s = frame.duration_ns as f64 * 1e-9;
        let margin = self.cfg.occupancy_margin_db;
        let pct = self.pct();
        let hist_cfg = self.cfg.histogram;
        let scheme = self.cfg.scheme;
        let bins = usize::from(hist_cfg.bins);
        let live = self.cfg.coarse_live;
        let Self {
            plan,
            open,
            pool,
            floors,
            scratch,
            geom,
            next_seal_ns,
            closed_rows,
            ..
        } = self;
        closed_rows.clear();
        let cells = &plan.cells;
        let peak = frame.peak.unwrap_or(frame.psd);
        let mut i = 0;
        while i < cells.len() {
            let fb = cells[i].cell.div_euclid(nf);
            let mut j = i + 1;
            while j < cells.len() && cells[j].cell.div_euclid(nf) == fb {
                j += 1;
            }
            let tile = open_tile(
                &mut open[0],
                &mut pool[0],
                geom,
                0,
                scheme,
                bins,
                fb,
                tb,
                next_seal_ns,
            );
            // T-377: this frame's own chain's floor, never the pyramid's pooled one.
            let floor = floors.get(&(frame.source, fb)).map(|f| &f.floor[..]);
            if tile.col_t.is_some_and(|c| t_in > c)
                && let Some(closed) = tile.close_column(margin, pct, scratch)
                && live
            {
                // T-571: that row is final, so it is folded upwards NOW, one row at a time.
                closed_rows.push((fb, tb, closed));
            }
            let late =
                tile.col_done.is_some_and(|d| t_in <= d) || tile.col_t.is_some_and(|c| t_in < c);
            if !late && tile.col_t.is_none() {
                tile.col_t = Some(t_in);
            }
            let mut values = 0u64;
            for s in &cells[i..j] {
                let v_lin = plan.mean(s, frame.psd);
                if !v_lin.is_finite() {
                    continue;
                }
                values += 1;
                let v_db = db(v_lin);
                let pk_db = db(f64::from(plan.max(s, peak)).max(v_lin));
                let f = (s.cell - fb * nf) as usize;
                // T-377: the threshold is resolved here, against this frame's own origin's
                // floor, so the stored occupancy decision can never carry another front end's
                // floor. NaN only when neither the caller nor this chain knows a floor yet, and
                // then `column_stats`' cold-start percentile of this column stands in.
                let thr = frame.floor_db.map_or_else(
                    || floor.map_or(f32::NAN, |fl| fl[f] + margin),
                    |fl| plan.mean(s, fl) as f32 + margin,
                );
                tile.add_value(t_in, f, v_db, pk_db, v_lin, dur_s, &hist_cfg);
                if late {
                    tile.add_late_occupancy(t_in, f, v_db, thr, dur_s);
                } else {
                    tile.col.push(ColEntry {
                        f: f as u32,
                        v: v_db,
                        thr,
                        dur_s: dur_s as f32,
                    });
                }
            }
            tile.prov
                .add_frame(frame, &state, step.as_ref(), cell_shape, values);
            i = j;
        }
        if live && !self.closed_rows.is_empty() {
            let rows = std::mem::take(&mut self.closed_rows);
            let nf = self.geom.nf;
            for &(fb, tb, t) in &rows {
                self.fold_row_live(0, fb, tb, t, 0, nf);
            }
            self.closed_rows = rows;
        }
        self.stats.frames_folded += 1;
        self.first_folded_ns = self.first_folded_ns.min(frame.t.as_unix_nanos());
        // T-453: this frame has just changed level 0, so any live-edge summary folded from it is
        // stale. Cheap when nothing is cached, which is every ingest of a run nobody is watching.
        self.invalidate_derived();
        let end = frame.t.as_unix_nanos().saturating_add(frame.duration_ns);
        self.latest_ns = self.latest_ns.max(end);
        let due = self
            .latest_ns
            .saturating_sub(i64::try_from(self.cfg.seal_lag.as_nanos()).unwrap_or(i64::MAX));
        if due >= self.next_seal_ns {
            self.seal_through_ns(due)?;
        }
        if let Some(iv) = self.cfg.checkpoint_interval {
            let iv = i64::try_from(iv.as_nanos()).unwrap_or(i64::MAX);
            match self.last_checkpoint_ns {
                None => self.last_checkpoint_ns = Some(self.latest_ns),
                Some(last) if self.latest_ns.saturating_sub(last) >= iv => self.checkpoint()?,
                _ => {}
            }
        }
        Ok(IngestOutcome::Folded)
    }

    /// The Gamma shape of a level-0 cell value for `shape` (cached per STFT resolution) of a frame
    /// with bins `bin_width_hz` wide.
    fn resolve_shape(&mut self, shape: NoiseShape, bin_width_hz: f64) -> Option<f32> {
        match shape {
            NoiseShape::Unknown => None,
            NoiseShape::CellShape(k) => (k.is_finite() && k > 0.0).then_some(k),
            NoiseShape::BinShape(k) => (k.is_finite() && k > 0.0).then(|| {
                let bins_per_cell = self.geom.levels[0].f_cell_hz / bin_width_hz;
                k * bins_per_cell.max(1.0) as f32
            }),
            NoiseShape::Spectrum(r) => {
                if !(r.bin_width_hz.is_finite() && r.bin_width_hz > 0.0) {
                    return None;
                }
                if let Some((cached, k)) = self.shape_cache
                    && cached == r
                {
                    return Some(k);
                }
                let k =
                    hk_dsp::radiometry::cell_value_shape(&r, self.geom.levels[0].f_cell_hz) as f32;
                self.shape_cache = Some((r, k));
                Some(k)
            }
        }
    }

    /// Seals every tile ending at or before `t` (shutdown, tests, or an idle clock), rolls them
    /// up, and enforces the byte budget. Frames for sealed tiles are late afterwards.
    pub fn seal_through(&mut self, t: Timestamp) -> Result<(), StoreError> {
        self.seal_through_ns(t.as_unix_nanos())
    }

    fn seal_through_ns(&mut self, w: i64) -> Result<(), StoreError> {
        self.invalidate_derived();
        self.watermark_ns = self.watermark_ns.max(w);
        let w = self.watermark_ns;
        let margin = self.cfg.occupancy_margin_db;
        let pct = self.pct();
        for level in 0..self.geom.n_levels() {
            let mut keys = std::mem::take(&mut self.keys);
            keys.clear();
            keys.extend(
                self.open[level]
                    .keys()
                    .filter(|&&(_, tb)| self.geom.block_end_ns(level, tb) <= w)
                    .copied(),
            );
            keys.sort_unstable_by_key(|&(fb, tb)| (tb, fb));
            // T-571: nothing may seal with a row still in flight. A level-0 tile's last column is
            // closed here, and a coarse tile's in-progress top row is flushed here — before the
            // tile is written, and while the levels above it are still open, which the ascending
            // level order guarantees.
            if self.cfg.coarse_live {
                for &(fb, tb) in &keys {
                    let flushed = if level == 0 {
                        let Self { open, scratch, .. } = self;
                        open[0]
                            .get_mut(&(fb, tb))
                            .and_then(|t| t.close_column(margin, pct, scratch))
                            .map(|t| (t, 0, self.geom.nf))
                    } else {
                        self.open[level]
                            .get_mut(&(fb, tb))
                            .and_then(|t| t.take_pending_row())
                    };
                    if let Some((t, lo, hi)) = flushed {
                        self.fold_row_live(level, fb, tb, t, lo, hi);
                    }
                }
                // A flush at this level can open a tile one level up whose block has also ended;
                // that level's own pass picks it up, because levels are walked in ascending order.
            }
            let mut result = Ok(());
            for &(fb, tb) in &keys {
                let Some(mut tile) = self.open[level].remove(&(fb, tb)) else {
                    continue;
                };
                if level == 0 {
                    tile.close_column(margin, pct, &mut self.scratch);
                    self.update_floor(&tile);
                }
                let written = self.write_tile(level, &tile, true);
                // T-453, `docs/16` §5.2: eager only for a ladder. A lattice's coarse nodes are
                // built by `materialize` when a read asks for them, so capture writes one tile
                // series instead of ~4 (§6.4a) — and on a narrow capture, instead of ~7.5, because
                // a frequency-coarser node's tile seals on the same watermark as its producer's
                // however few frequency blocks the capture spans.
                if written.is_ok() && !self.cfg.coarse_on_demand && !self.cfg.coarse_live {
                    self.fold_into_consumers(level, &tile);
                }
                // T-571: live maintenance has already folded every CELL of this tile upwards, row
                // by row. Provenance is not a per-row quantity — merging it per row would multiply
                // every count it holds by the number of rows — so it merges once, here, as the
                // producer tile seals. A coarse tile therefore carries the provenance of its
                // sealed producers; inside the current block it carries cells without it, which is
                // the honest statement of what has been summarised so far.
                if written.is_ok() && self.cfg.coarse_live {
                    self.fold_prov_into_consumers(level, &tile);
                }
                self.pool[level].push(tile);
                if let Err(e) = written {
                    result = Err(e);
                    break;
                }
            }
            self.keys = keys;
            result?;
        }
        self.next_seal_ns = self
            .open
            .iter()
            .enumerate()
            .flat_map(|(l, m)| m.keys().map(move |&(_, tb)| (l, tb)))
            .map(|(l, tb)| self.geom.block_end_ns(l, tb))
            .min()
            .unwrap_or(i64::MAX);
        self.save_source_states()?;
        self.save_recording_began()?;
        self.enforce_budget()
    }

    /// Feeds a sealed level-0 tile's low percentile into **its own** front end's floor track
    /// (T-377).
    ///
    /// A tile whose frames came from two front ends — or whose origins were not all recorded —
    /// measures neither chain's floor, so it feeds none: the tile's histogram is the pooled
    /// mixture, and pushing it under the majority contributor would be exactly the pooling this
    /// key removes (T-359's rule). The cost is that such a tile leaves both chains a little
    /// staler, which is the same trade T-314 made at the read.
    fn update_floor(&mut self, tile: &Tile) {
        let Some(source) = tile.prov.sole_source() else {
            self.stats.floor_tiles_unattributed += 1;
            return;
        };
        let (w, nf) = (self.cfg.floor_memory_tiles, self.geom.nf);
        let hist_cfg = self.cfg.histogram;
        let q = self.cfg.low_percentile;
        self.floors
            .entry((source, tile.key.f_block))
            .or_insert_with(|| FloorTrack::new(w, nf))
            .push(|f| hist_percentile(tile.hist_row(f), &hist_cfg, q));
    }

    /// Folds a sealed tile into **every** coarser level folded from it (T-434). A ladder level has
    /// one consumer; a lattice node has one per axis it feeds, and each gets the same sealed tile,
    /// so the coarse levels all come from the one seal path and nothing is produced twice.
    fn fold_into_consumers(&mut self, level: usize, child: &Tile) {
        let mut ups = std::mem::take(&mut self.consumers);
        ups.clear();
        ups.extend_from_slice(self.geom.consumers(level));
        let hist_cfg = self.cfg.histogram;
        let pct = self.pct();
        for &up in &ups {
            let (pfb, ptb) = self
                .geom
                .fold_target(level, up, child.key.f_block, child.key.t_block);
            if self.sealed[up].contains_key(&(ptb, pfb)) {
                continue;
            }
            let parent = open_tile(
                &mut self.open[up],
                &mut self.pool[up],
                &self.geom,
                up,
                self.cfg.scheme,
                usize::from(hist_cfg.bins),
                pfb,
                ptb,
                &mut self.next_seal_ns,
            );
            parent.fold_child(
                child,
                self.geom.levels[up].f_factor,
                self.geom.levels[up].t_factor,
                &hist_cfg,
                pct,
                &mut self.group_hist,
            );
        }
        self.consumers = ups;
    }

    /// T-571: merges a sealed tile's provenance into every coarser level folded from it, leaving
    /// the cells alone — live maintenance has already folded those, one row at a time.
    fn fold_prov_into_consumers(&mut self, level: usize, child: &Tile) {
        let mut ups = std::mem::take(&mut self.consumers);
        ups.clear();
        ups.extend_from_slice(self.geom.consumers(level));
        let bins = usize::from(self.cfg.histogram.bins);
        let scheme = self.cfg.scheme;
        for &up in &ups {
            let (pfb, ptb) = self
                .geom
                .fold_target(level, up, child.key.f_block, child.key.t_block);
            if self.sealed[up].contains_key(&(ptb, pfb)) {
                continue;
            }
            let parent = open_tile(
                &mut self.open[up],
                &mut self.pool[up],
                &self.geom,
                up,
                scheme,
                bins,
                pfb,
                ptb,
                &mut self.next_seal_ns,
            );
            parent.prov.merge(&child.prov);
        }
        self.consumers = ups;
    }

    /// **Live coarse maintenance (T-571).** Folds one finished producer row — cells
    /// `[f_lo, f_hi)` of row `t` in tile `(fb, tb)` of `level` — into every consumer, and
    /// cascades wherever that completes a consumer's own row.
    ///
    /// This is the whole of the invariant CLAUDE.md states under "Live rendering, tile maintenance
    /// and playback": each level adjusts its in-progress top row as rows arrive and **commits
    /// every N**, so a coarse tile is an already-committed tile by the time a read asks for it.
    ///
    /// # What it costs, and why it cannot run away
    ///
    /// Per arriving row the work is **O(1) per level**: one pass over the footprint being folded,
    /// which **halves** at each frequency step and is visited once per `t_factor` rows at each
    /// time step. Both series converge, so the total is a small multiple of one level-0 row
    /// whatever the node count — the measurement is
    /// [`PyramidStats::coarse_cells_folded`] and `tests/live_coarse.rs` asserts it.
    ///
    /// Residency is one `(row, f_lo, f_hi)` per open tile ([`Tile::row_pending`]), not an
    /// accumulator per node, and **no tile is written per row**: a coarse tile is written when it
    /// seals, exactly as before, which is why the commit-every-N rule costs zero extra writes.
    ///
    /// # How far a coarse node trails the live edge (T-583)
    ///
    /// The cascade propagates **on commit**, so a node that downsamples time by `2^j` from the
    /// finest one only hears about a row once `2^j` of them have closed, and a chain of
    /// in-progress rows compounds: node `(i, j)` trails the finest row by up to `2^j − 1` of
    /// them, plus the finest level's own still-open column. At the shipped four time levels and a
    /// 1 s cell that is up to **7 s** at `level_t = 3`, measured at 1, 2 and 4 rows for
    /// `level_t` 1, 2 and 3. The cells are *correct* throughout — a partial row reads as fewer
    /// observed seconds, never as wrong values — and node (0, 0), which is the live edge, is not
    /// affected at all. Whether a zoomed-out pane should instead see its in-progress row is a
    /// product decision, ticketed rather than assumed.
    ///
    /// # Exactly once
    ///
    /// A cell of a coarse tile is written by exactly one producer tile, so the fold accumulates
    /// and every `(tile, row, footprint)` must reach it once. The **footprint** is what keeps that
    /// true down a frequency chain: a node's row is filled by `f_factor` producer tiles, and if
    /// each arrival re-folded the whole row upwards the level above it would count the earlier
    /// arrivals again. Carrying the footprint through the cascade folds only what just changed.
    fn fold_row_live(
        &mut self,
        level: usize,
        fb: i64,
        tb: i64,
        t: usize,
        f_lo: usize,
        f_hi: usize,
    ) {
        self.fold_row_from(level, fb, tb, t, f_lo, f_hi, true);
    }

    /// [`Pyramid::fold_row_live`], with the cascade optional.
    ///
    /// `cascade == false` folds one step and stops, which is what the reopen rebuild wants: it
    /// walks the levels in ascending order and replays each one's rows in its own turn, so letting
    /// the cascade run as well would fold every row twice.
    #[allow(clippy::too_many_arguments)]
    fn fold_row_from(
        &mut self,
        level: usize,
        fb: i64,
        tb: i64,
        t: usize,
        f_lo: usize,
        f_hi: usize,
        cascade: bool,
    ) {
        if f_lo >= f_hi {
            return;
        }
        let (scheme, bins) = (self.cfg.scheme, usize::from(self.cfg.histogram.bins));
        let mut q = std::mem::take(&mut self.row_queue);
        let mut out = std::mem::take(&mut self.row_out);
        q.clear();
        q.push_back((level, fb, tb, t, f_lo, f_hi));
        while let Some((l, fb, tb, t, f_lo, f_hi)) = q.pop_front() {
            // Taken out so the consumer's accumulator may be borrowed mutably; put back below.
            // A consumer always has a higher level index than its producer (the geometry enforces
            // it), so nothing here can alias.
            let Some(child) = self.open[l].remove(&(fb, tb)) else {
                continue;
            };
            let mut ups = std::mem::take(&mut self.consumers);
            ups.clear();
            ups.extend_from_slice(self.geom.consumers(l));
            for &up in &ups {
                let (pfb, ptb) = self.geom.fold_target(l, up, fb, tb);
                if self.sealed[up].contains_key(&(ptb, pfb)) {
                    continue;
                }
                let (ff, tf) = (self.geom.levels[up].f_factor, self.geom.levels[up].t_factor);
                out.clear();
                let parent = open_tile(
                    &mut self.open[up],
                    &mut self.pool[up],
                    &self.geom,
                    up,
                    scheme,
                    bins,
                    pfb,
                    ptb,
                    &mut self.next_seal_ns,
                );
                parent.fold_row(&child, t, f_lo, f_hi, ff, tf, &mut out);
                self.stats.coarse_rows_folded += 1;
                self.stats.coarse_cells_folded += (f_hi - f_lo) as u64;
                if cascade {
                    for &(pt, pl, ph) in &out {
                        q.push_back((up, pfb, ptb, pt, pl, ph));
                    }
                }
            }
            self.consumers = ups;
            self.open[l].insert((fb, tb), child);
        }
        self.row_queue = q;
        self.row_out = out;
    }

    /// Drops every transient live-edge summary. Called wherever the data a derived tile was folded
    /// from can have changed — a fold into level 0, or a seal that adds a sealed child — so the
    /// cache can never outlive its inputs by so much as one frame.
    fn invalidate_derived(&mut self) {
        if !self.derived.is_empty() {
            for ((level, ..), tile) in std::mem::take(&mut self.derived) {
                self.pool[level].push(tile);
            }
        }
    }

    /// The producer tiles of `(level, fb, tb)`: `(f_blocks, t_blocks)` of `level`'s producer.
    ///
    /// The exact inverse of [`Geometry::fold_target`], and it must stay that way — it is the one
    /// place a lazy build decides *what to fold*, where the eager path was told by the child which
    /// parent to fold into. `f_factor` child frequency blocks (a parent cell's children lie in one
    /// child tile, which `geometry()` enforces) × `nt / parent_cells_per_tile` child time blocks
    /// (1 when the node coarsens frequency alone, which is the whole point of the de-welding).
    fn child_blocks(
        &self,
        from: usize,
        level: usize,
        fb: i64,
        tb: i64,
    ) -> (std::ops::Range<i64>, std::ops::Range<i64>) {
        let u = &self.geom.levels[level];
        let f = i64::from(u.f_factor);
        let k = self.geom.parent_cells_per_tile(from, level).max(1);
        let per_parent_tile = (u.nt as i64).div_euclid(k).max(1);
        (
            fb * f..(fb + 1) * f,
            tb * per_parent_tile..(tb + 1) * per_parent_tile,
        )
    }

    /// Folds every existing producer tile of `(level, fb, tb)` into a fresh tile, or `None` when no
    /// producer tile holds anything (an address over unobserved time–frequency, which must stay
    /// unobserved rather than becoming an empty *measured* tile).
    ///
    /// A producer still **open** at the live edge contributes what it holds, which is everything
    /// except the occupancy of its own in-progress time column — that is decided by
    /// [`Tile::close_column`] when the column ends, and a direct level-0 read stands in for it with
    /// `column_preview`. So a live-edge summary can lag level 0's occupancy by up to one of the
    /// producer's time cells. It never disagrees with it: max-hold, counts and linear power are
    /// accumulated as each value lands.
    fn fold_children(
        &mut self,
        from: usize,
        level: usize,
        fb: i64,
        tb: i64,
    ) -> Result<Option<Box<Tile>>, StoreError> {
        let key = TileKey {
            scheme: self.cfg.scheme,
            level: level as u8,
            f_block: fb,
            t_block: tb,
        };
        let (f_range, t_range) = self.child_blocks(from, level, fb, tb);
        let (f_factor, t_factor) = (
            self.geom.levels[level].f_factor,
            self.geom.levels[level].t_factor,
        );
        let hist_cfg = self.cfg.histogram;
        let pct = self.pct();
        let mut group_hist = std::mem::take(&mut self.group_hist);
        let mut parent: Option<Box<Tile>> = None;
        let mut result = Ok(());
        'outer: for cfb in f_range {
            for ctb in t_range.clone() {
                // The producer tile, wherever it lives: still open at the live edge, already
                // derived for a coarser read, or sealed on disk.
                let owned;
                let child: Option<&Tile> = if let Some(t) = self.open[from].get(&(cfb, ctb)) {
                    Some(t)
                } else if let Some(t) = self.derived.get(&(from, cfb, ctb)) {
                    Some(t)
                } else {
                    match self.read_sealed(from, cfb, ctb) {
                        Ok(t) => {
                            owned = t;
                            owned.as_ref()
                        }
                        Err(e) => {
                            result = Err(e);
                            break 'outer;
                        }
                    }
                };
                let Some(child) = child else { continue };
                self.stats.producer_tiles_folded += 1;
                let p = parent.get_or_insert_with(|| {
                    let g = &self.geom.levels[level];
                    match self.pool[level].pop() {
                        Some(mut t) => {
                            t.reset(key, g);
                            t
                        }
                        None => {
                            Box::new(Tile::new(key, self.geom.nf, g, usize::from(hist_cfg.bins)))
                        }
                    }
                });
                p.fold_child(child, f_factor, t_factor, &hist_cfg, pct, &mut group_hist);
            }
        }
        self.group_hist = group_hist;
        result?;
        Ok(parent)
    }

    /// Ensures the tile `(level, fb, tb)` exists, building it from its producers if it does not.
    ///
    /// A tile whose time block has fully elapsed is **written sealed** — precomputed from then on,
    /// which is `docs/16` §5.2's "precomputed at seal time" applied to the levels a reader actually
    /// asks for. One at the **live edge** cannot be sealed (more frames are still due inside it), so
    /// it is folded into [`Pyramid::derived`] and thrown away the moment anything under it changes.
    ///
    /// Returns whether the tile now exists. `false` means the region holds nothing, which is a real
    /// answer and not a failure.
    fn materialize_tile(
        &mut self,
        level: usize,
        fb: i64,
        tb: i64,
        budget: &mut usize,
    ) -> Result<bool, StoreError> {
        if self.sealed[level].contains_key(&(tb, fb))
            || self.open[level].contains_key(&(fb, tb))
            || self.derived.contains_key(&(level, fb, tb))
        {
            return Ok(true);
        }
        let Some(from) = self.geom.levels[level].from else {
            return Ok(false); // a level nothing produces: level 0 is capture's own product
        };
        if *budget == 0 {
            return Err(StoreError::BadQuery(format!(
                "building level {level} here needs more than {MAX_MATERIALIZE_TILES} tiles of \
                 folding; ask for a finer level, or a smaller region"
            )));
        }
        *budget -= 1;
        let (f_range, t_range) = self.child_blocks(from, level, fb, tb);
        for cfb in f_range {
            for ctb in t_range.clone() {
                self.materialize_tile(from, cfb, ctb, budget)?;
            }
        }
        let Some(parent) = self.fold_children(from, level, fb, tb)? else {
            return Ok(false);
        };
        self.stats.tiles_materialized += 1;
        if self.geom.block_end_ns(level, tb) <= self.watermark_ns {
            let written = self.write_tile(level, &parent, true);
            self.pool[level].push(parent);
            written?;
        } else {
            self.derived.insert((level, fb, tb), parent);
        }
        Ok(true)
    }

    /// **Builds the coarse tiles a read at `level` over `(freq, time)` needs** (`docs/16` §5.2,
    /// T-453). A no-op unless [`PyramidConfig::coarse_on_demand`] is set, and a no-op for a level
    /// nothing produces.
    ///
    /// # Why this does not simply move the contention it removes
    ///
    /// The eager fold ran on **every seal, for every node, for the whole run**: its cost is
    /// O(capture duration × nodes), it is paid by the thread that gates the ring, and — this is the
    /// part that made it 5.1× on the harness — it is paid whether or not anyone ever looks. This
    /// runs O(tiles actually viewed), on a reader, **once**: the result is sealed to disk, so the
    /// second viewer of the same address pays nothing, and a node nobody opens costs nothing
    /// forever. Only the live edge is rebuilt per read, and only the live edge *can* be, which is
    /// exactly §5.2's split.
    ///
    /// The lock is the same mutex the read already takes, and callers already chunk their reads
    /// (`/api/tiles` re-acquires per whole output row), so the hold this adds is bounded the same
    /// way the read's is — by the region asked for, not by the run's length.
    pub fn materialize(
        &mut self,
        level: usize,
        freq: FreqRange,
        time: TimeRange,
    ) -> Result<usize, StoreError> {
        if !self.cfg.coarse_on_demand || level >= self.geom.n_levels() {
            return Ok(0);
        }
        if !(freq.lo_hz.is_finite() && freq.hi_hz >= freq.lo_hz) {
            return Err(StoreError::BadQuery("frequency range".into()));
        }
        if time.end < time.start {
            return Err(StoreError::BadQuery("time range".into()));
        }
        let g = self.geom.levels[level];
        let nf = self.geom.nf as i64;
        let c_lo = (freq.lo_hz / g.f_cell_hz).floor() as i64;
        let c_hi = ((freq.hi_hz / g.f_cell_hz).ceil() as i64 - 1).max(c_lo);
        let (fb_lo, fb_hi) = (c_lo.div_euclid(nf), c_hi.div_euclid(nf));
        let block = g.t_block_ns();
        let t0 = time.start.as_unix_nanos();
        let tb_lo = t0.div_euclid(block);
        let tb_hi = time
            .end
            .as_unix_nanos()
            .saturating_sub(1)
            .max(t0)
            .div_euclid(block);
        // The addresses asked for, before any folding: bounded here as well as inside the
        // recursion, so a wide region is refused rather than walked a tile at a time.
        let asked = (fb_hi - fb_lo + 1).saturating_mul(tb_hi - tb_lo + 1);
        if asked > MAX_MATERIALIZE_TILES as i64 {
            return Err(StoreError::BadQuery(format!(
                "{asked} tiles of level {level} cover this region, over the {MAX_MATERIALIZE_TILES} \
                 one request may build; ask for a coarser level, or a smaller region"
            )));
        }
        let mut budget = MAX_MATERIALIZE_TILES;
        let mut built = 0;
        for fb in fb_lo..=fb_hi {
            for tb in tb_lo..=tb_hi {
                if self.materialize_tile(level, fb, tb, &mut budget)? {
                    built += 1;
                }
            }
        }
        Ok(built)
    }

    /// Budget units one tile of `level` costs to fold **from nothing**: itself, plus every
    /// intermediate tile under it. Level-0 tiles are free, because [`Self::materialize_tile`]
    /// returns before the budget check for a level nothing produces.
    ///
    /// The subtree is uniform — every tile of a level has the same producer shape — so this is a
    /// product down the canonical path rather than a walk over the tiles themselves, which is the
    /// only reason it can be asked about a node whose subtree is astronomically large.
    fn fold_cost(&self, level: usize) -> u128 {
        let (mut cost, mut per_tile, mut l) = (0u128, 1u128, level);
        // The producer index is strictly smaller by construction, so this terminates; the bound is
        // belt-and-braces against a hand-written config.
        for _ in 0..self.geom.n_levels() {
            let Some(from) = self.geom.levels[l].from else {
                break;
            };
            cost = cost.saturating_add(per_tile);
            if cost > MAX_MATERIALIZE_TILES as u128 {
                return cost; // already hopeless; the rest would only overflow
            }
            let k = self.geom.parent_cells_per_tile(from, l).max(1);
            let per_parent = (self.geom.levels[l].nt as i64).div_euclid(k).max(1);
            per_tile = per_tile
                .saturating_mul(u128::from(self.geom.levels[l].f_factor) * per_parent as u128);
            l = from;
        }
        cost
    }

    /// The **worst case, over every time alignment**, of what [`Self::materialize`] would cost for
    /// `level` over `freq` and a window of `window_ns`, on a store holding **nothing** — `Some`
    /// budget units when that fits [`MAX_MATERIALIZE_TILES`], `None` when the call would refuse.
    ///
    /// # What this is a property of, and why each half is the shape it is
    ///
    /// It answers *would a read of this address be refused for want of folding?* without folding
    /// anything, which is what lets `/api/tiles` declare a readable ceiling instead of discovering
    /// it one 400 at a time (T-482).
    ///
    /// - **A store holding nothing is the worst case, not a special case.** A tile that already
    ///   exists costs no budget ([`Self::materialize_tile`] returns before the decrement), so every
    ///   byte of capture can only make a real call *cheaper* than this. A bound taken on an empty
    ///   store therefore stays true as the store fills, which is the whole point of quoting it in a
    ///   contract.
    /// - **Frequency is exact and time is a bound**, because that is how the caller uses them: a
    ///   tile read passes its whole frequency extent and *chunks* time into whole output rows, so
    ///   the time window's offset is not known here while the frequency range is. What the caller
    ///   *can* promise is the **grid its windows start on**: every chunk of a `/api/tiles` read
    ///   starts on a multiple of the tile's own time cell. `start_step_ns` is that grid, and the
    ///   window is charged the most blocks it can touch from **any** start on it —
    ///   `floor((window - 1 + block - g) / block) + 1` with `g = gcd(start_step, block)`, which is
    ///   exact: the worst start is `block - g` past a boundary. A caller that can promise nothing
    ///   passes `start_step_ns <= 0`, meaning every nanosecond (`g = 1`).
    ///
    /// # The block count was `floor(window / block) + 1`, and that is NOT the worst case (T-494)
    ///
    /// It is the count for a window that starts **on** a block boundary, which the caller cannot
    /// promise: a tile is chunked into whole output *rows*, a row can be finer than a block, and
    /// then chunks after the first start mid-block. Measured on a 4 x 5 store at address `(8, 3)`:
    /// the read's only candidate was level 19, whose 240 s window over a 1024 s block was charged
    /// one block for 1016 budget units, under the 1024 cap, and was then refused for real at output
    /// row 120, where the same window straddles two blocks and costs 2032. So `/api/tiles` could
    /// declare a ceiling and still 400 inside it. On the shipped 4 x 4 store the error never
    /// surfaced. That was luck, not soundness: the addresses it hit were already refused for
    /// another reason.
    ///
    /// The start grid is what keeps this from simply over-charging instead. A plain worst case
    /// over every nanosecond costs the shipped ceiling `(9, 1)`, where every chunk *is*
    /// block-aligned and the extra block is never touched. Measured, that dropped the ceiling to
    /// `(8, 2)`.
    pub fn materialize_cost_bound(
        &self,
        level: usize,
        freq: FreqRange,
        window_ns: i64,
        start_step_ns: i64,
    ) -> Option<usize> {
        if !self.cfg.coarse_on_demand || level >= self.geom.n_levels() {
            return Some(0);
        }
        if !(freq.lo_hz.is_finite() && freq.hi_hz >= freq.lo_hz) || window_ns < 0 {
            return None; // `materialize` refuses these outright
        }
        let g = self.geom.levels[level];
        let nf = self.geom.nf as i64;
        let c_lo = (freq.lo_hz / g.f_cell_hz).floor() as i64;
        let c_hi = ((freq.hi_hz / g.f_cell_hz).ceil() as i64 - 1).max(c_lo);
        let fb = c_hi.div_euclid(nf) - c_lo.div_euclid(nf) + 1;
        // Blocks a half-open window of this length touches, maximised over every start on the
        // caller's grid. The worst start is `block - g` past a boundary (see the doc comment).
        let block = g.t_block_ns().max(1);
        let step = if start_step_ns <= 0 { 1 } else { start_step_ns };
        let g_align = gcd(step, block);
        let tb = if window_ns <= 0 {
            1
        } else {
            (window_ns - 1 + block - g_align).div_euclid(block) + 1
        };
        let asked = u128::from(fb.max(1) as u64).saturating_mul(tb.max(1) as u64 as u128);
        if asked > MAX_MATERIALIZE_TILES as u128 {
            return None;
        }
        let total = asked.saturating_mul(self.fold_cost(level));
        (total <= MAX_MATERIALIZE_TILES as u128).then_some(total as usize)
    }

    /// The budget pass's step: builds the missing summary of the **oldest** sealed tile that no
    /// coarser level covers, finest level first — the order the budget evicts in, so the tile that
    /// gets a summary is the one about to need it.
    fn promote_oldest(&mut self) -> bool {
        let top = self.geom.top();
        for level in 0..top {
            let Some(&(tb, fb)) = self.sealed[level]
                .keys()
                .find(|&&(tb, fb)| !self.covered(level, fb, tb))
            else {
                continue;
            };
            if self.promote_for_retention(level, fb, tb) {
                return true;
            }
        }
        false
    }

    /// One retention step for a lazy lattice: the summary a tile about to be evicted would
    /// otherwise not have. Returns whether a coarser tile actually became sealed, so the caller's
    /// loop terminates on `false` rather than spinning.
    ///
    /// **Retention is the one place laziness cannot be honest on its own**: dropping the fine
    /// tiles of a level whose coarse summary was never built would lose the measurement outright,
    /// which is the opposite of what the byte budget is for. So the summary is produced *here*,
    /// for exactly the tile that is about to go, and nowhere else.
    fn promote_for_retention(&mut self, level: usize, fb: i64, tb: i64) -> bool {
        if !self.cfg.coarse_on_demand {
            return false;
        }
        let ups: Vec<usize> = self.geom.consumers(level).to_vec();
        let mut any = false;
        for up in ups {
            let (pfb, ptb) = self.geom.fold_target(level, up, fb, tb);
            if self.sealed[up].contains_key(&(ptb, pfb)) {
                continue;
            }
            // A consumer tile whose own block has not ended cannot be sealed, and the fine tile
            // under it must therefore be kept. That is the correct answer, not a failure.
            if self.geom.block_end_ns(up, ptb) > self.watermark_ns {
                continue;
            }
            let mut budget = MAX_MATERIALIZE_TILES;
            if matches!(self.materialize_tile(up, pfb, ptb, &mut budget), Ok(true)) {
                any = true;
            }
        }
        any && self.covered(level, fb, tb)
    }

    fn write_tile(&mut self, level: usize, tile: &Tile, sealed: bool) -> Result<(), StoreError> {
        let (fb, tb) = (tile.key.f_block, tile.key.t_block);
        let raw_bytes = codec::encode(
            tile,
            sealed,
            self.cfg.unit,
            &self.geom.levels[level],
            &self.cfg.histogram,
            self.pct(),
            self.cfg.compression_level,
            &mut self.buf,
            &mut self.payload,
        );
        let path = self.path(level, fb, tb);
        let dir = path.parent().expect("tile path has a parent");
        fs::create_dir_all(dir).map_err(io_err(dir))?;
        let tmp = dir.join(format!("t{tb}.tile.tmp{}", std::process::id()));
        let write = || -> std::io::Result<()> {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(&self.buf)?;
            f.sync_all()?;
            fs::rename(&tmp, &path)
        };
        if let Err(e) = write() {
            let _ = fs::remove_file(&tmp);
            return Err(StoreError::Io { path, source: e });
        }
        let bytes = self.buf.len() as u64;
        if level == 0
            && let Some(old) = self.checkpoints.remove(&(fb, tb))
        {
            self.disk_bytes -= old;
        }
        let old = if sealed {
            let old = self.sealed[level].insert((tb, fb), bytes);
            self.level_bytes[level] = self.level_bytes[level] - old.unwrap_or(0) + bytes;
            if old.is_none() {
                // A rewrite (retention trim) keeps its index entries.
                self.stats.tiles_written += 1;
                self.index_sealed(level, tb, fb);
            }
            old
        } else {
            self.stats.checkpoints_written += 1;
            self.checkpoints.insert((fb, tb), bytes)
        };
        self.disk_bytes = self.disk_bytes - old.unwrap_or(0) + bytes;
        self.stats.bytes_written += bytes;
        self.stats.raw_bytes_written += raw_bytes;
        Ok(())
    }

    /// **Every** coarser level folded from sealed tile `(level, fb, tb)` has its covering tile
    /// sealed on disk. Vacuously true for a level nothing is folded from — the only kind of level
    /// whose tiles may be dropped without a coarser copy existing.
    fn covered(&self, level: usize, fb: i64, tb: i64) -> bool {
        self.geom.consumers(level).iter().all(|&up| {
            let (pfb, ptb) = self.geom.fold_target(level, up, fb, tb);
            self.sealed[up].contains_key(&(ptb, pfb))
        })
    }

    fn evict(&mut self, level: usize, tb: i64, fb: i64) -> Result<(), StoreError> {
        let path = self.path(level, fb, tb);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(StoreError::Io { path, source: e }),
        }
        if let Some(bytes) = self.sealed[level].remove(&(tb, fb)) {
            self.disk_bytes -= bytes;
            self.level_bytes[level] -= bytes;
            self.stats.bytes_evicted += bytes;
        }
        self.unprotected[level].remove(&(tb, fb));
        self.stats.tiles_evicted += 1;
        // A parent waiting for its children to go is re-checked from its first deadline.
        let mut ups = std::mem::take(&mut self.consumers);
        ups.clear();
        ups.extend_from_slice(self.geom.consumers(level));
        for &up in &ups {
            let (pfb, ptb) = self.geom.fold_target(level, up, fb, tb);
            if self.sealed[up].contains_key(&(ptb, pfb)) {
                let ages = self.ages(up, pfb);
                if let Some(&a) = ages.deadlines.first() {
                    let end = self.geom.block_end_ns(up, ptb);
                    self.due[up].insert((end.saturating_add(a), ptb, pfb));
                }
            }
        }
        self.consumers = ups;
        Ok(())
    }

    /// The overrides at `level`.
    fn level_overrides(&self, level: usize) -> Vec<RetentionOverride> {
        self.cfg
            .retention_overrides
            .iter()
            .filter(|o| usize::from(o.level) == level)
            .copied()
            .collect()
    }

    /// Retention ages of `(level, fb)`: per frequency cell, the longest override overlapping the
    /// cell, else the level's `max_age` (cached; the config is fixed while open).
    fn ages(&mut self, level: usize, fb: i64) -> Arc<Ages> {
        if let Some(a) = self.ages.get(&(level, fb)) {
            return Arc::clone(a);
        }
        let level_age = self.cfg.levels[level].max_age.map(dur_ns);
        let (nf, w) = (self.geom.nf, self.geom.levels[level].f_cell_hz);
        let (lo, hi) = (fb as f64 * nf as f64 * w, (fb + 1) as f64 * nf as f64 * w);
        let ovs: Vec<_> = self
            .level_overrides(level)
            .into_iter()
            .filter(|o| o.freq.lo_hz < hi && o.freq.hi_hz > lo)
            .collect();
        let ages = if ovs.is_empty() {
            Ages {
                protected: false,
                tile_age: level_age,
                deadlines: level_age.into_iter().collect(),
            }
        } else {
            let mut forever = false;
            let mut set = BTreeSet::new();
            for f in 0..nf {
                let c_lo = (fb * nf as i64 + f as i64) as f64 * w;
                match cell_age(&ovs, c_lo, c_lo + w, level_age) {
                    Some(a) => {
                        set.insert(a);
                    }
                    None => forever = true,
                }
            }
            Ages {
                protected: true,
                tile_age: if forever { None } else { set.last().copied() },
                deadlines: set.into_iter().collect(),
            }
        };
        let ages = Arc::new(ages);
        self.ages.insert((level, fb), Arc::clone(&ages));
        ages
    }

    /// Adds a newly sealed tile to the retention indexes.
    fn index_sealed(&mut self, level: usize, tb: i64, fb: i64) {
        let ages = self.ages(level, fb);
        if !ages.protected {
            self.unprotected[level].insert((tb, fb));
        }
        if let Some(&a) = ages.deadlines.first() {
            let end = self.geom.block_end_ns(level, tb);
            self.due[level].insert((end.saturating_add(a), tb, fb));
        }
    }

    /// Registers a sealed tile without a file (retention cost tests).
    #[cfg(test)]
    pub(super) fn index_fake_sealed(&mut self, level: usize, fb: i64, tb: i64, bytes: u64) {
        self.sealed[level].insert((tb, fb), bytes);
        self.level_bytes[level] += bytes;
        self.disk_bytes += bytes;
        self.index_sealed(level, tb, fb);
    }

    /// Clears the cells of protected tile `(level, fb, tb)` whose own age has passed at `w` and
    /// rewrites the tile if any held data (T-126).
    fn trim(&mut self, level: usize, fb: i64, tb: i64, w: i64) -> Result<(), StoreError> {
        let end = self.geom.block_end_ns(level, tb);
        let level_age = self.cfg.levels[level].max_age.map(dur_ns);
        let ovs = self.level_overrides(level);
        let (nf, cw) = (self.geom.nf, self.geom.levels[level].f_cell_hz);
        let expired: Vec<usize> = (0..nf)
            .filter(|&f| {
                let lo = (fb * nf as i64 + f as i64) as f64 * cw;
                cell_age(&ovs, lo, lo + cw, level_age).is_some_and(|a| end.saturating_add(a) <= w)
            })
            .collect();
        if expired.is_empty() {
            return Ok(());
        }
        let Some(mut tile) = self.read_sealed(level, fb, tb)? else {
            return Ok(());
        };
        let mut cleared = false;
        for f in expired {
            cleared |= tile.clear_freq(f);
        }
        if cleared {
            self.write_tile(level, &tile, true)?;
            self.stats.tiles_trimmed += 1;
        }
        Ok(())
    }

    /// A sealed tile of the level this one is folded from lies inside tile `(level, fb, tb)`.
    fn has_children(&self, level: usize, fb: i64, tb: i64) -> bool {
        let g = &self.geom.levels[level];
        let Some(down) = g.from else {
            return false;
        };
        // Child tiles per parent tile in time: the parent holds `nt` time cells and one child tile
        // spans `k` of them. Welded, k = 1 and this is the old `nt` child blocks.
        let k = self.geom.parent_cells_per_tile(down, level);
        let per_tile = g.nt as i64 / k;
        let (t0, t1) = (tb * per_tile, (tb + 1) * per_tile);
        let factor = i64::from(g.f_factor);
        let (f0, f1) = (fb * factor, (fb + 1) * factor);
        self.sealed[down]
            .range((t0, i64::MIN)..(t1, i64::MIN))
            .any(|(&(_, cfb), _)| (f0..f1).contains(&cfb))
    }

    /// The oldest tile of `level` the byte budget may evict: unprotected tiles only, from their
    /// own index, unless `allow_protected`. Adds the tiles examined to `visits`.
    fn oldest_victim(
        &self,
        level: usize,
        allow_protected: bool,
        visits: &mut u64,
    ) -> Option<(i64, i64)> {
        let check = |&(tb, fb): &(i64, i64)| {
            *visits += 1;
            if !self.covered(level, fb, tb) {
                // Parents seal in time order: later tiles are no more covered.
                return Some(None);
            }
            (!self.has_children(level, fb, tb)).then_some(Some((tb, fb)))
        };
        if allow_protected {
            self.sealed[level].keys().find_map(check).flatten()
        } else {
            self.unprotected[level].iter().find_map(check).flatten()
        }
    }

    /// Retention (T-116, indexed and region-precise in T-126), after every seal, in three passes
    /// over **sealed** tiles:
    ///
    /// 1. **Age.** Each frequency cell has an age: the longest [`super::RetentionOverride`]
    ///    overlapping **the cell** at its level, else the level's `max_age` (no age: kept). A tile
    ///    expires once its end is older than `watermark − age` for every cell. Before that, a
    ///    protected tile whose unprotected cells have passed their age is **trimmed**: rewritten
    ///    with those cells cleared (they read unobserved at that level; coarser levels keep their
    ///    summary). The pass visits only tiles in the `due` index whose next deadline has passed,
    ///    so its cost does not grow with the number of protected tiles kept.
    /// 2. **Quota.** While a level exceeds its `byte_quota`, its oldest unprotected tiles go (from
    ///    the level's unprotected index).
    /// 3. **Budget.** While all files exceed `byte_budget`: the oldest unprotected tile of the
    ///    finest level that has one, then the oldest unprotected top-level tile, and only then
    ///    protected tiles (finest first).
    ///
    /// In every pass a tile is evicted or trimmed only if a coarser level covers it (a sealed
    /// parent, or it is top-level) and **children go first**: a tile with a sealed finer tile still
    /// inside it is kept (and re-checked when a child goes), so history never has a finer tile
    /// without its coarser summary. The clock is the data watermark, so [`Pyramid::seal_through`]
    /// with a later time fast-forwards retention.
    fn enforce_budget(&mut self) -> Result<(), StoreError> {
        let top = self.geom.top();
        let w = self.watermark_ns;
        for level in 0..=top {
            let due: Vec<(i64, i64, i64)> = self.due[level]
                .range(..=(w, i64::MAX, i64::MAX))
                .copied()
                .collect();
            for e in &due {
                self.due[level].remove(e);
            }
            for (_, tb, fb) in due {
                self.retention_visits += 1;
                if !self.sealed[level].contains_key(&(tb, fb)) {
                    continue;
                }
                if !self.covered(level, fb, tb) && !self.promote_for_retention(level, fb, tb) {
                    // Re-check once the last consumer still missing this tile has sealed past it.
                    let at = self
                        .geom
                        .consumers(level)
                        .iter()
                        .map(|&up| {
                            let (_, ptb) = self.geom.fold_target(level, up, fb, tb);
                            self.geom.block_end_ns(up, ptb)
                        })
                        .max()
                        .unwrap_or(i64::MIN)
                        .max(w.saturating_add(1));
                    self.due[level].insert((at, tb, fb));
                    continue;
                }
                if self.has_children(level, fb, tb) {
                    continue; // re-queued by `evict` when a child goes
                }
                let end = self.geom.block_end_ns(level, tb);
                let ages = self.ages(level, fb);
                if ages.tile_age.is_some_and(|a| end.saturating_add(a) <= w) {
                    self.evict(level, tb, fb)?;
                    self.stats.tiles_expired += 1;
                    continue;
                }
                if ages.protected {
                    self.trim(level, fb, tb, w)?;
                }
                if let Some(&a) = ages.deadlines.iter().find(|&&a| end.saturating_add(a) > w) {
                    self.due[level].insert((end.saturating_add(a), tb, fb));
                }
            }
        }
        self.stats.over_quota = false;
        for level in 0..=top {
            let Some(quota) = self.cfg.levels[level].byte_quota else {
                continue;
            };
            let Some(mut excess) = self.level_bytes[level].checked_sub(quota) else {
                continue;
            };
            let mut victims = Vec::new();
            let mut visits = 0;
            let keys: Vec<(i64, i64)> = self.unprotected[level].iter().copied().collect();
            for (tb, fb) in keys {
                if excess == 0 {
                    break;
                }
                if level < top
                    && !self.covered(level, fb, tb)
                    && !self.promote_for_retention(level, fb, tb)
                {
                    break;
                }
                visits += 1;
                if self.has_children(level, fb, tb) {
                    continue;
                }
                victims.push((tb, fb));
                excess = excess.saturating_sub(self.sealed[level][&(tb, fb)]);
            }
            self.retention_visits += visits;
            for (tb, fb) in victims {
                self.evict(level, tb, fb)?;
                self.stats.tiles_evicted_quota += 1;
            }
            if self.level_bytes[level] > quota {
                self.stats.over_quota = true;
            }
        }
        self.stats.over_budget = false;
        while self.disk_bytes > self.cfg.byte_budget {
            let mut visits = 0;
            let victim = (0..top)
                .find_map(|l| self.oldest_victim(l, false, &mut visits).map(|v| (l, v)))
                .or_else(|| {
                    self.oldest_victim(top, false, &mut visits)
                        .map(|v| (top, v))
                })
                .or_else(|| {
                    (0..=top).find_map(|l| self.oldest_victim(l, true, &mut visits).map(|v| (l, v)))
                });
            self.retention_visits += visits;
            match victim {
                Some((level, (tb, fb))) => self.evict(level, tb, fb)?,
                // A lazy lattice has no coarse summary until something asks for one, so the budget
                // pass builds the one it is about to need. `promote_oldest` returns false as soon
                // as it cannot make progress, which is what terminates this loop.
                None if self.cfg.coarse_on_demand && self.promote_oldest() => continue,
                None => {
                    self.stats.over_budget = true;
                    break;
                }
            }
        }
        Ok(())
    }

    /// Writes every open level-0 tile as an unsealed checkpoint (closing its in-progress time
    /// column first). Coarser open tiles are not written: they are rebuilt from their sealed
    /// children on open.
    ///
    /// **T-571: the column this closes is a finished row and is folded upwards like any other.**
    /// It was not, and the loss was silent and permanent: under `coarse_live` the only paths into
    /// [`Pyramid::fold_row_live`] are the column advance in [`Pyramid::ingest`] and the pre-seal
    /// flush, and a column closed here reaches neither — the next frame finds `col_t` at `None`
    /// and the seal's own `close_column` returns `None`. With the shipped 60 s checkpoint interval
    /// over a 1 s time cell that is **one row in sixty missing from every coarse node, on disk**,
    /// so a burst inside a dropped second is present at the finest zoom and vanishes when you zoom
    /// out — the max-hold that would have carried it was never folded.
    ///
    /// One residual is left, and it is the checkpoint's own shape rather than the cascade's:
    /// frames that arrive in the **same** time cell after a checkpoint has closed it take
    /// [`Tile::add_late_occupancy`]'s late path, which updates level 0 in place and is invisible
    /// to every coarser node (T-584). A checkpoint that did not close the column would trade that
    /// for an unrecoverable in-progress column on a crash, which is what the column close is for.
    pub fn checkpoint(&mut self) -> Result<(), StoreError> {
        let margin = self.cfg.occupancy_margin_db;
        let pct = self.pct();
        let mut keys = std::mem::take(&mut self.keys);
        keys.clear();
        keys.extend(self.open[0].keys().copied());
        let mut result = Ok(());
        for &k in &keys {
            let Some(mut tile) = self.open[0].remove(&k) else {
                continue;
            };
            let closed = tile.close_column(margin, pct, &mut self.scratch);
            let written = self.write_tile(0, &tile, false);
            self.open[0].insert(k, tile);
            if self.cfg.coarse_live
                && let Some(t) = closed
            {
                self.fold_row_live(0, k.0, k.1, t, 0, self.geom.nf);
            }
            if let Err(e) = written {
                result = Err(e);
                break;
            }
        }
        self.keys = keys;
        self.last_checkpoint_ns = Some(self.latest_ns);
        result?;
        self.save_source_states()?;
        self.save_recording_began()
    }

    /// Checkpoints and closes. Dropping without `close` loses at most one checkpoint interval of
    /// level-0 data (the crash case).
    pub fn close(mut self) -> Result<(), StoreError> {
        self.checkpoint()
    }

    fn scan(&mut self) -> Result<(), StoreError> {
        for level in 0..self.geom.n_levels() {
            let ldir = self.root.join(format!("L{level}"));
            let Ok(fdirs) = fs::read_dir(&ldir) else {
                continue;
            };
            for fdir in fdirs {
                let fdir = fdir.map_err(io_err(&ldir))?;
                let Some(fb) = fdir
                    .file_name()
                    .to_str()
                    .and_then(|s| s.strip_prefix('f'))
                    .and_then(|s| s.parse::<i64>().ok())
                else {
                    continue;
                };
                let fpath = fdir.path();
                for entry in fs::read_dir(&fpath).map_err(io_err(&fpath))? {
                    let path = entry.map_err(io_err(&fpath))?.path();
                    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                        continue;
                    };
                    if name.contains(".tmp") {
                        let _ = fs::remove_file(&path);
                        self.stats.files_ignored += 1;
                        continue;
                    }
                    let Some(tb) = name
                        .strip_prefix('t')
                        .and_then(|s| s.strip_suffix(".tile"))
                        .and_then(|s| s.parse::<i64>().ok())
                    else {
                        continue;
                    };
                    match codec::read_header(&path).map_err(io_err(&path))? {
                        Some((h, len)) => {
                            self.check_header(&h, level, fb, tb, &path)?;
                            // T-377: a level-0 tile written before per-origin floor tracking
                            // carries an occupancy decided against a pooled floor, and nothing in
                            // the tile can recompute it. Counted, never silently adopted.
                            if level == 0 && h.format < codec::FORMAT_VERSION {
                                self.stats.tiles_pre_origin_floor += 1;
                            }
                            if h.sealed {
                                self.sealed[level].insert((tb, fb), len);
                                self.level_bytes[level] += len;
                                self.index_sealed(level, tb, fb);
                            } else if level == 0 {
                                self.checkpoints.insert((fb, tb), len);
                            } else {
                                let _ = fs::remove_file(&path);
                                self.stats.files_ignored += 1;
                                continue;
                            }
                            self.disk_bytes += len;
                        }
                        None => {
                            let _ = fs::remove_file(&path);
                            self.stats.files_ignored += 1;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn check_header(
        &self,
        h: &codec::Header,
        level: usize,
        fb: i64,
        tb: i64,
        path: &Path,
    ) -> Result<(), StoreError> {
        let g = &self.geom.levels[level];
        let c = &self.cfg;
        let mismatch = if h.scheme != c.scheme {
            Some(format!("scheme {} != {}", h.scheme, c.scheme))
        } else if usize::from(h.level) != level || h.f_block != fb || h.t_block != tb {
            Some("header key disagrees with the file's path".into())
        } else if h.f_cell_hz != g.f_cell_hz
            || h.t_cell_ns != g.t_cell_ns
            || h.nf as usize != self.geom.nf
            || h.nt as usize != g.nt
        {
            Some(format!(
                "geometry {} Hz × {} ns × {}×{} != config {} Hz × {} ns × {}×{}",
                h.f_cell_hz, h.t_cell_ns, h.nf, h.nt, g.f_cell_hz, g.t_cell_ns, self.geom.nf, g.nt
            ))
        } else if h.hist != c.histogram
            || h.pct != (c.low_percentile, c.high_percentile)
            || h.unit != c.unit
        {
            Some("histogram, percentile or unit settings differ".into())
        } else {
            None
        };
        match mismatch {
            Some(detail) => Err(StoreError::SchemeMismatch {
                path: path.to_owned(),
                detail,
            }),
            None => Ok(()),
        }
    }

    /// Reads a sealed tile from disk; an invalid file is removed from the index.
    /// The provenance summary of tile `(level, fb, tb)`, open or sealed (a sealed tile's header
    /// only), `None` when there is no such tile (T-133 filtered queries).
    pub(super) fn tile_provenance(
        &self,
        level: usize,
        fb: i64,
        tb: i64,
    ) -> Result<Option<ProvenanceSummary>, StoreError> {
        if let Some(t) = self.open[level].get(&(fb, tb)) {
            return Ok(Some(t.prov.clone()));
        }
        if !self.sealed[level].contains_key(&(tb, fb)) {
            return Ok(None);
        }
        let path = self.path(level, fb, tb);
        match codec::read_header(&path) {
            Ok(h) => Ok(h.map(|(h, _)| h.prov)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(StoreError::Io { path, source: e }),
        }
    }

    pub(super) fn read_sealed(
        &self,
        level: usize,
        fb: i64,
        tb: i64,
    ) -> Result<Option<Tile>, StoreError> {
        if !self.sealed[level].contains_key(&(tb, fb)) {
            return Ok(None);
        }
        let path = self.path(level, fb, tb);
        match codec::decode(
            &path,
            &self.geom.levels[level],
            usize::from(self.cfg.histogram.bins),
        ) {
            Ok(t) => Ok(t),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(StoreError::Io { path, source: e }),
        }
    }

    fn recover(&mut self) -> Result<(), StoreError> {
        let top = self.geom.top();
        for level in 0..=top {
            if let Some((&(tb, _), _)) = self.sealed[level].last_key_value() {
                self.watermark_ns = self.watermark_ns.max(self.geom.block_end_ns(level, tb));
            }
        }
        self.latest_ns = self.watermark_ns;
        let bins = usize::from(self.cfg.histogram.bins);
        let mut cps: Vec<(i64, i64)> = self.checkpoints.keys().copied().collect();
        cps.sort_unstable();
        for (fb, tb) in cps {
            let path = self.path(0, fb, tb);
            match codec::decode(&path, &self.geom.levels[0], bins).map_err(io_err(&path))? {
                Some(tile) => {
                    self.next_seal_ns = self.next_seal_ns.min(self.geom.block_end_ns(0, tb));
                    self.open[0].insert((fb, tb), Box::new(tile));
                }
                None => {
                    let _ = fs::remove_file(&path);
                    if let Some(b) = self.checkpoints.remove(&(fb, tb)) {
                        self.disk_bytes -= b;
                    }
                    self.stats.files_ignored += 1;
                }
            }
        }
        // **T-571: a live lattice's coarse tiles are rebuilt by REPLAYING ROWS, level by level.**
        //
        // The seal-time rebuild below re-folds whole sealed child tiles into their consumers,
        // which is everything a seal-time scheme is ever fed by. A live scheme is fed by **rows**,
        // and two of its cases have no sealed child at all:
        //
        // 1. The currently open level-0 tile comes back from its checkpoint still open. Its closed
        //    rows had reached every coarse node before the restart and reach none of them after
        //    it. At the shipped 64 x 1 s level-0 tile that is up to 64 consecutive seconds of
        //    **grey over time that was observed** — the display invariant's own failure case.
        // 2. A coarse tile rebuilt here is itself a producer. Folding sealed level-0 tiles into an
        //    open node (0, 1) leaves those rows short of node (0, 2), because (0, 1) is not
        //    sealed and so is never walked as a child. Under the seal-time scheme that was
        //    consistent (an unsealed (0, 1) had contributed nothing to (0, 2) yet); under a live
        //    one it is a hole.
        //
        // So each level is replayed in ascending index order, from whatever holds its rows — the
        // open tile, or a sealed tile read back — with the cascade OFF, because the level above
        // gets its own turn once this one has been rebuilt.
        //
        // **Bounded.** Only time a coarse tile could still be open needs replaying: the coarsest
        // block containing the watermark. At the shipped lattice that is 512 s, eight level-0
        // tiles per frequency block, read once at open.
        //
        // **Exactly once.** `fold_row_from` skips a consumer whose covering tile is already
        // sealed, and every open coarse tile at this point was created by this loop.
        if self.cfg.coarse_live {
            let nf = self.geom.nf;
            // The earliest time a coarse tile could still be open: the start of each level's own
            // block containing the watermark, whichever is earliest. Taken per level rather than
            // from the coarsest block alone, because block boundaries on different levels do not
            // nest unless the epoch happens to align.
            let from_ns = (0..=top)
                .map(|l| {
                    let b = self.geom.levels[l].t_block_ns().max(1);
                    self.watermark_ns.div_euclid(b).saturating_mul(b)
                })
                .min()
                .unwrap_or(self.watermark_ns);
            for level in 0..top {
                if self.geom.consumers(level).is_empty() {
                    continue;
                }
                let mut keys: Vec<(i64, i64)> = self.open[level].keys().copied().collect();
                keys.extend(
                    self.sealed[level]
                        .keys()
                        .filter(|&&(tb, _)| self.geom.block_end_ns(level, tb) > from_ns)
                        .map(|&(tb, fb)| (fb, tb)),
                );
                keys.sort_unstable_by_key(|&(fb, tb)| (tb, fb));
                keys.dedup();
                for (fb, tb) in keys {
                    // A sealed producer is read back and parked in `open` for the length of its
                    // replay: the cascade takes its producer out of `open` while it borrows the
                    // consumer mutably, so a tile it cannot find there folds nothing at all.
                    let was_open = self.open[level].contains_key(&(fb, tb));
                    if !was_open {
                        let Some(t) = self.read_sealed(level, fb, tb)? else {
                            continue;
                        };
                        self.open[level].insert((fb, tb), Box::new(t));
                    }
                    // Ascending time within a tile: the cascade's in-progress row is flushed by a
                    // later row arriving, so out-of-order rows would strand it.
                    let rows: Vec<usize> = {
                        let tile = &self.open[level][&(fb, tb)];
                        (0..tile.nt)
                            .filter(|&t| (0..tile.nf).any(|f| tile.count[t * tile.nf + f] > 0))
                            .collect()
                    };
                    for t in rows {
                        self.fold_row_from(level, fb, tb, t, 0, nf, false);
                    }
                    if !was_open && let Some(t) = self.open[level].remove(&(fb, tb)) {
                        self.pool[level].push(t);
                    }
                }
            }
        }
        // T-453: a lazy lattice has nothing to re-fold. A coarse tile of a lazy scheme is only ever
        // written once its own time block has elapsed, after which no further child can seal into
        // it (a frame for a sealed level-0 tile is refused as late), so every coarse tile on disk
        // is already complete and every one that is missing will be built when a read asks.
        for level in 0..=top {
            if self.cfg.coarse_on_demand || self.cfg.coarse_live {
                break;
            }
            let ups: Vec<usize> = self.geom.consumers(level).to_vec();
            if ups.is_empty() {
                continue;
            }
            // Per consumer, the end of its newest sealed tile: a child past that one was sealed
            // after the consumer last wrote, so its contribution was lost and must be re-folded.
            let newest: Vec<i64> = ups
                .iter()
                .map(|&up| {
                    self.sealed[up]
                        .last_key_value()
                        .map_or(i64::MIN, |(&(tb, _), _)| self.geom.block_end_ns(up, tb))
                })
                .collect();
            let children: Vec<(i64, i64)> = self.sealed[level]
                .keys()
                .rev()
                .take_while(|&&(tb, fb)| {
                    ups.iter().zip(&newest).any(|(&up, &end)| {
                        let (_, ptb) = self.geom.fold_target(level, up, fb, tb);
                        self.geom.block_end_ns(up, ptb) > end
                    })
                })
                .copied()
                .collect();
            for (tb, fb) in children {
                match self.read_sealed(level, fb, tb)? {
                    Some(child) => self.fold_into_consumers(level, &child),
                    None => {
                        let _ = fs::remove_file(self.path(level, fb, tb));
                        if let Some(b) = self.sealed[level].remove(&(tb, fb)) {
                            self.disk_bytes -= b;
                            self.level_bytes[level] -= b;
                        }
                        self.unprotected[level].remove(&(tb, fb));
                        self.stats.files_ignored += 1;
                    }
                }
            }
        }
        if self.watermark_ns > i64::MIN {
            self.seal_through_ns(self.watermark_ns)?;
        }
        Ok(())
    }
}

/// Greatest common divisor of two positive integers.
fn gcd(mut a: i64, mut b: i64) -> i64 {
    while b != 0 {
        (a, b) = (b, a.rem_euclid(b));
    }
    a.abs().max(1)
}
