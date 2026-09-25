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
use super::deferred::{Body, EncodeArgs, Job, PendingWrites, SmallFile, Unwritten, WrittenBatch};
use super::frame::{FrameInput, NoiseShape, RegridPlan};
use super::live::{LiveTile, ROW_ACC_BYTES_PER_CELL, RowAcc};
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
    /// T-584: frames folded into a level-0 time cell whose column had **already closed** — frame
    /// disorder, or a frame arriving in the cell a checkpoint just closed. Their values reach the
    /// cell in place (percentiles excepted, which are not revisited), and they reach the coarse
    /// nodes too, because a row is folded upwards only once the ingest clock has left it by
    /// `seal_lag`. Nonzero is normal; it is the size of the window that rule is holding open.
    pub frames_out_of_order: u64,
    /// Sealed tiles written.
    pub tiles_written: u64,
    /// T-942: tiles **reopened** at [`Pyramid::open`] because they had been sealed past the
    /// store's own last frame by an end-of-run [`Pyramid::seal_all`]. The restarted run keeps
    /// filling them instead of finding their time closed.
    pub tiles_reopened: u64,
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
    /// T-585: `(row, footprint)`s a coarse node **committed** — folded into every consumer and
    /// then held in stored form, releasing the accumulator row. Bounded per arriving row the same
    /// way `coarse_rows_folded` is.
    pub coarse_rows_committed: u64,
    /// T-585: observed cells those commits encoded.
    pub coarse_cells_committed: u64,
    /// T-585: bytes those commits produced before compression, and as held. Their ratio is the
    /// in-memory compression ratio; `coarse_row_bytes_stored / coarse_cells_committed` the bytes
    /// per committed cell.
    pub coarse_row_bytes_raw: u64,
    pub coarse_row_bytes_stored: u64,
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
/// **What the open tiles hold in RAM, measured** (T-585): the number the residency tests assert
/// against, taken from the buffers themselves rather than from an open-tile count times an
/// assumed size.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResidentBytes {
    /// Open tiles held in full ([`Tile`]): level 0, and every level of a scheme that is not
    /// live-maintained.
    pub full_tiles: usize,
    pub full_bytes: usize,
    /// Open coarse tiles of a live scheme ([`LiveTile`]).
    pub live_tiles: usize,
    /// Their accumulator rows, in use and spare.
    pub live_acc_rows: usize,
    pub live_acc_bytes: usize,
    /// Their committed rows, in stored form.
    pub live_segments: usize,
    pub live_encoded_bytes: usize,
}

impl ResidentBytes {
    /// Everything the open tiles hold.
    pub fn total(&self) -> usize {
        self.full_bytes + self.live_acc_bytes + self.live_encoded_bytes
    }

    /// The coarse nodes' share.
    pub fn live_bytes(&self) -> usize {
        self.live_acc_bytes + self.live_encoded_bytes
    }
}

/// An open tile: a full accumulator, or (T-585) a live-maintained coarse node holding its
/// committed rows encoded and only its in-progress row as accumulator.
#[derive(Clone, Debug)]
pub(super) enum OpenTile {
    Full(Box<Tile>),
    Live(Box<LiveTile>),
}

impl OpenTile {
    pub(super) fn prov(&self) -> &ProvenanceSummary {
        match self {
            OpenTile::Full(t) => &t.prov,
            OpenTile::Live(t) => &t.prov,
        }
    }

    fn prov_mut(&mut self) -> &mut ProvenanceSummary {
        match self {
            OpenTile::Full(t) => &mut t.prov,
            OpenTile::Live(t) => &mut t.prov,
        }
    }

    /// The full accumulator, where this is one. Level 0 always is; so is every level of a scheme
    /// without live coarse maintenance.
    fn full_mut(&mut self) -> Option<&mut Tile> {
        match self {
            OpenTile::Full(t) => Some(t),
            OpenTile::Live(_) => None,
        }
    }

    /// Bytes this open tile holds, measured from its buffers.
    fn resident_into(&self, r: &mut ResidentBytes) {
        match self {
            OpenTile::Full(t) => {
                r.full_tiles += 1;
                r.full_bytes += t.resident_bytes();
            }
            OpenTile::Live(t) => {
                let (acc, enc) = t.resident_bytes();
                r.live_tiles += 1;
                r.live_acc_rows += t.acc_rows();
                r.live_acc_bytes += acc;
                r.live_segments += t.committed_stats().0;
                r.live_encoded_bytes += enc;
            }
        }
    }
}

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

/// T-583: a preview tile at some level of a producer chain, as `(f_block, t_block, tile, rows)` —
/// the rows being the ones of it that have **not** propagated to the level above. The tile is a
/// clone of the node in its own open form (T-585): level 0 a full accumulator, a coarse node a
/// [`LiveTile`], which is also the cheaper of the two to clone.
type PreviewTile = (i64, i64, OpenTile, Vec<(usize, usize, usize)>);

/// The spectrum-history pyramid under one data directory (see the [module docs](super)).
pub struct Pyramid {
    pub(super) cfg: PyramidConfig,
    pub(super) geom: Geometry,
    pub(super) root: PathBuf,
    /// Open tiles per level, keyed `(f_block, t_block)`. Level 0 is always [`OpenTile::Full`];
    /// a live scheme's coarse levels are [`OpenTile::Live`] (T-585).
    pub(super) open: Vec<HashMap<(i64, i64), OpenTile>>,
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
    /// Boxed so tiles move between the open maps and the pool without copying. At a live coarse
    /// level this holds the scratch a [`LiveTile`] is materialised into for its seal write.
    #[allow(clippy::vec_box)]
    pool: Vec<Vec<Box<Tile>>>,
    /// T-585: pooled [`LiveTile`]s per level.
    #[allow(clippy::vec_box)]
    live_pool: Vec<Vec<Box<LiveTile>>>,
    /// T-585: one row of accumulator a committed row is decoded into when a fold reads it.
    row_scratch: RowAcc,
    /// T-585: encode scratch for a row commit.
    seg_buf: Vec<u8>,
    plan: RegridPlan,
    /// Default level-0 floor per `(source, f_block)` (T-377): see [`FloorTrack`].
    pub(super) floors: HashMap<(u64, i64), FloorTrack>,
    /// Every tile whose block ends at or before this has sealed.
    watermark_ns: i64,
    latest_ns: i64,
    /// T-942: `(latest, watermark)` from [`EDGE_FILE`] as it was read at open (`None`: absent —
    /// a store written before T-942, or one that has never folded a frame).
    resumed_edge: Option<(i64, i64)>,
    /// T-942: the pair [`EDGE_FILE`] holds on disk, so it is rewritten only when the edge moves.
    edge_persisted: (i64, i64),
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
    /// T-571/T-584: level-0 tiles one [`Pyramid::ingest`] call touched, `(f_block, t_block)`.
    /// Each is checked for rows the ingest clock has now left, which are the rows to fold.
    touched: Vec<(i64, i64)>,
    /// T-571: tiles consulted to answer a query. Counted behind a shared reference because the
    /// read path takes `&self`; see [`Pyramid::source_tiles_read`].
    source_tiles: std::sync::atomic::AtomicU64,
    /// T-583: producer rows folded on the READ path by [`Pyramid::live_preview`] to show a
    /// coarse node's in-progress row. Behind a shared reference for the same reason.
    preview_rows: std::sync::atomic::AtomicU64,
    buf: Vec<u8>,
    payload: Vec<u8>,
    /// T-901: seals queue their file work instead of doing it (see [`super::deferred`]).
    defer_writes: bool,
    /// T-901: sealed tiles whose file has not landed yet, `(level, f_block, t_block)`. Every read
    /// of a sealed tile consults this before the disk, so a tile is never unreadable between its
    /// seal and its write.
    pub(super) unwritten: HashMap<(usize, i64, i64), Unwritten>,
    /// T-901: the queued file work, in the order it must be performed.
    write_queue: Vec<Job>,
    /// T-901: a batch has been taken and not landed. While it is out, [`Pyramid::take_writes`]
    /// hands out nothing, so two flushers (a multi-device run has one view writer per front end)
    /// can never perform writes of one tile out of order, or share a temp file.
    writes_in_flight: bool,
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

/// File of this store's **edge** (T-942), under the scheme root: 16 bytes, two little-endian
/// Unix-ns i64s — the end of the newest frame it folded ([`Pyramid::latest_frame_end`]), then its
/// watermark ([`Pyramid::watermark`]).
///
/// Both are read back by [`Pyramid::recover`], and the pair is what makes a restart honest. The
/// watermark is a *decision* the store made, not a property of the tiles on disk: a tile can be
/// sealed past it ([`Pyramid::seal_all`], the end-of-run gesture), and without this file the next
/// run had to infer a watermark from tile time blocks — which rounds *up*, by a whole hour at
/// scheme 1's level 2, and then refuses everything the next run records as late (the T-942
/// defect). Written on the same seal/checkpoint path as [`RECORDING_BEGAN_FILE`].
pub(super) const EDGE_FILE: &str = "edge";

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
    map: &'m mut HashMap<(i64, i64), OpenTile>,
    pool: &mut Vec<Box<Tile>>,
    live_pool: &mut Vec<Box<LiveTile>>,
    geom: &Geometry,
    level: usize,
    scheme: u16,
    bins: usize,
    fb: i64,
    tb: i64,
    live: bool,
    next_seal: &mut i64,
) -> &'m mut OpenTile {
    map.entry((fb, tb)).or_insert_with(|| {
        let key = TileKey {
            scheme,
            level: level as u8,
            f_block: fb,
            t_block: tb,
        };
        let g = &geom.levels[level];
        *next_seal = (*next_seal).min(geom.block_end_ns(level, tb));
        // T-585: a live scheme's coarse node holds its committed rows encoded; level 0 is
        // capture's own accumulator and stays full.
        if live && level > 0 {
            OpenTile::Live(match live_pool.pop() {
                Some(mut t) => {
                    t.reset(key, g);
                    t
                }
                None => Box::new(LiveTile::new(key, geom.nf, g)),
            })
        } else {
            OpenTile::Full(match pool.pop() {
                Some(mut t) => {
                    t.reset(key, g);
                    t
                }
                None => Box::new(Tile::new(key, geom.nf, g, bins)),
            })
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
            live_pool: (0..n).map(|_| Vec::new()).collect(),
            row_scratch: RowAcc::new(geom.nf),
            seg_buf: Vec::new(),
            plan: RegridPlan::default(),
            floors: HashMap::new(),
            watermark_ns: i64::MIN,
            latest_ns: i64::MIN,
            resumed_edge: None,
            edge_persisted: (i64::MIN, i64::MIN),
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
            touched: Vec::new(),
            source_tiles: std::sync::atomic::AtomicU64::new(0),
            preview_rows: std::sync::atomic::AtomicU64::new(0),
            buf: Vec::new(),
            payload: Vec::new(),
            defer_writes: false,
            unwritten: HashMap::new(),
            write_queue: Vec::new(),
            writes_in_flight: false,
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
        p.load_edge();
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
        if self.defer_writes {
            self.write_queue.push(Job {
                path,
                tmp,
                body: Body::Bytes {
                    what: SmallFile::RecordingBegan,
                    bytes: t.as_unix_nanos().to_le_bytes().to_vec(),
                },
            });
            self.began_persisted = true;
            return Ok(());
        }
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

    /// Reads [`EDGE_FILE`] (T-942). Absent in a store written before T-942, or in one that has
    /// never folded a frame, in which case [`Pyramid::recover`] falls back to the tiles on disk.
    fn load_edge(&mut self) {
        let path = self.root.join(EDGE_FILE);
        self.resumed_edge = fs::read(&path)
            .ok()
            .and_then(|b| <[u8; 16]>::try_from(b.as_slice()).ok())
            .map(|b| {
                let half = |i: usize| i64::from_le_bytes(b[i..i + 8].try_into().expect("8 bytes"));
                (half(0), half(8))
            });
        self.edge_persisted = self.resumed_edge.unwrap_or((i64::MIN, i64::MIN));
    }

    /// Writes [`EDGE_FILE`] when the edge has moved since it was last written (temp → fsync →
    /// rename, like tiles), on the same seal/checkpoint path as
    /// [`Pyramid::save_recording_began`] — so the capture thread pays it once per seal or
    /// checkpoint, not per frame.
    fn save_edge(&mut self) -> Result<(), StoreError> {
        let edge = (self.latest_ns, self.watermark_ns);
        if edge.0 == i64::MIN || edge == self.edge_persisted {
            return Ok(());
        }
        let mut bytes = Vec::with_capacity(16);
        bytes.extend_from_slice(&edge.0.to_le_bytes());
        bytes.extend_from_slice(&edge.1.to_le_bytes());
        let path = self.root.join(EDGE_FILE);
        let tmp = self
            .root
            .join(format!("{EDGE_FILE}.tmp{}", std::process::id()));
        if self.defer_writes {
            self.write_queue.push(Job {
                path,
                tmp,
                body: Body::Bytes {
                    what: SmallFile::Edge,
                    bytes,
                },
            });
            self.edge_persisted = edge;
            return Ok(());
        }
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
        self.edge_persisted = edge;
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
        if self.defer_writes {
            self.write_queue.push(Job {
                path,
                tmp,
                body: Body::Bytes {
                    what: SmallFile::SourceStates,
                    bytes,
                },
            });
            self.state_dirty = false;
            return Ok(());
        }
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

    /// T-583: producer rows folded on the read path to show coarse nodes' in-progress rows,
    /// over the life of this pyramid. Bounded by one row per level in the producer chain per open
    /// tile a read consults, and **zero** for any read of elapsed time.
    pub fn preview_rows_folded(&self) -> u64 {
        self.preview_rows.load(std::sync::atomic::Ordering::Relaxed)
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

    /// The stream time the history has reached: the end of the newest folded frame — on reopen,
    /// the persisted one ([`EDGE_FILE`], T-942), else the watermark — and `None` before any.
    /// Replays and time-compressed scenes run on their own clock, so "the last N seconds" means
    /// this, not the wall clock (T-125).
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

    /// **What the open tiles hold in RAM, measured from their buffers** (T-585). The residency
    /// tests assert against this rather than against an open-tile count times an assumed size.
    pub fn resident_bytes(&self) -> ResidentBytes {
        let mut r = ResidentBytes::default();
        for m in &self.open {
            for t in m.values() {
                t.resident_into(&mut r);
            }
        }
        r
    }

    /// Bytes one cell of a live node's accumulator row holds ([`ROW_ACC_BYTES_PER_CELL`]),
    /// re-exported so a test's bound is arithmetic over the same constant.
    pub const ROW_ACC_BYTES_PER_CELL: usize = ROW_ACC_BYTES_PER_CELL;

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
        // T-584: the ingest clock, this frame included, and the store's declared tolerance for
        // frames arriving out of order — the same `seal_lag` a tile's own seal waits out.
        let lag_ns = self.lag_ns();
        let now = self
            .latest_ns
            .max(frame.t.as_unix_nanos().saturating_add(frame.duration_ns));
        let Self {
            plan,
            open,
            pool,
            live_pool,
            floors,
            scratch,
            geom,
            next_seal_ns,
            touched,
            ..
        } = self;
        touched.clear();
        let cells = &plan.cells;
        let peak = frame.peak.unwrap_or(frame.psd);
        // T-584: whether this frame landed in an already-closed time cell anywhere.
        let mut out_of_order = false;
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
                &mut live_pool[0],
                geom,
                0,
                scheme,
                bins,
                fb,
                tb,
                live,
                next_seal_ns,
            )
            .full_mut()
            .expect("level 0 is always a full accumulator");
            // T-377: this frame's own chain's floor, never the pyramid's pooled one.
            let floor = floors.get(&(frame.source, fb)).map(|f| &f.floor[..]);
            if tile.col_t.is_some_and(|c| t_in > c) {
                tile.close_column(margin, pct, scratch);
            }
            if live {
                // T-571: rows are folded upwards one at a time, as they finish. T-584: a row
                // finishes when the ingest clock leaves it, NOT when its column closes — see
                // `fold_finished_rows`. The fold runs below, once this frame is in the cells.
                touched.push((fb, tb));
            }
            let late =
                tile.col_done.is_some_and(|d| t_in <= d) || tile.col_t.is_some_and(|c| t_in < c);
            out_of_order |= late;
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
        if live && !self.touched.is_empty() {
            // T-584: this frame's own values — including the in-place update of a cell whose
            // column has already closed — are in the tile by now, so a row folded here carries
            // them. Taken out and put back so `fold_finished_rows` may borrow `self` mutably;
            // it reuses the buffer's capacity, so ingest stays allocation-free.
            //
            // **T-901: every open level-0 tile, not only the ones this frame landed in.** Once
            // frames cross into the next time block, the previous tile is never touched again,
            // so the `seal_lag` of rows it still held back (2 s — ~50 rows at the display rate)
            // used to wait for its SEAL and fold through the whole cascade in one go: measured
            // at 0.8-1.8 s of a seal on the view lattice, under the lock every row push takes.
            // Folded here instead, each row goes up the frame after the clock leaves it, oldest
            // tile first, so the fold stays O(one row) per frame and the seal finds ~nothing
            // left. The rows and their order within a tile are exactly the same; only *when*
            // the older tile's last rows fold changes, and it moves earlier.
            let mut touched = std::mem::take(&mut self.touched);
            touched.clear();
            touched.extend(self.open[0].keys().copied());
            touched.sort_unstable_by_key(|&(fb, tb)| (tb, fb));
            let through = now.saturating_sub(lag_ns);
            for &(fb, tb) in &touched {
                self.fold_finished_rows(fb, tb, through);
            }
            self.touched = touched;
        }
        self.stats.frames_folded += 1;
        self.stats.frames_out_of_order += u64::from(out_of_order);
        self.first_folded_ns = self.first_folded_ns.min(frame.t.as_unix_nanos());
        // T-453: this frame has just changed level 0, so any live-edge summary folded from it is
        // stale. Cheap when nothing is cached, which is every ingest of a run nobody is watching.
        self.invalidate_derived();
        self.latest_ns = now;
        let due = self.latest_ns.saturating_sub(lag_ns);
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
        self.seal_upto(t.as_unix_nanos(), false)
    }

    /// **Seals every open tile whatever time block it is in, and leaves the watermark at `t`**
    /// (T-942) — the end-of-run gesture, so a finished run's history is complete on disk at every
    /// level.
    ///
    /// This is [`Pyramid::seal_through`] with the *forcing* separated from the *clock*, and the
    /// separation is the fix. Forcing used to be expressed as sealing through the last frame plus
    /// an hour, which put the watermark — a monotonic, persisted quantity — an hour into a future
    /// that had not been recorded. Scheme 1's level 2 is a one-hour tile, so the next run on the
    /// same data dir resumed with a watermark on the next hour boundary and `ingest` refused
    /// **every** frame it folded as late, silently, until wall clock caught up (staging,
    /// 2026-09-25: `frames_ingested 0`, `frames_late 2024`). The tiles this writes still end in
    /// the future — that is what forcing means — but [`Pyramid::open`] knows it: a sealed tile
    /// whose block ends after the store's last frame is reopened (level 0) or rebuilt from its
    /// children (coarser), so the next run keeps filling it.
    pub fn seal_all(&mut self, t: Timestamp) -> Result<(), StoreError> {
        self.seal_upto(t.as_unix_nanos(), true)
    }

    fn seal_through_ns(&mut self, w: i64) -> Result<(), StoreError> {
        self.seal_upto(w, false)
    }

    /// Seals open tiles whose block has ended at `w` — or, with `force`, all of them (T-942) —
    /// and advances the watermark to `w` either way.
    fn seal_upto(&mut self, w: i64, force: bool) -> Result<(), StoreError> {
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
                    .filter(|&&(_, tb)| force || self.geom.block_end_ns(level, tb) <= w)
                    .copied(),
            );
            keys.sort_unstable_by_key(|&(fb, tb)| (tb, fb));
            // T-571: nothing may seal with a row still in flight. A level-0 tile's last column is
            // closed here, and a coarse tile's in-progress top row is flushed here — before the
            // tile is written, and while the levels above it are still open, which the ascending
            // level order guarantees.
            if self.cfg.coarse_live {
                for &(fb, tb) in &keys {
                    if level == 0 {
                        // T-584: the tile is about to seal, so every remaining row is finished
                        // whatever the clock says — a frame for it is refused as late from here
                        // on — and rows held back by `seal_lag` must all go up now.
                        {
                            let Self { open, scratch, .. } = self;
                            if let Some(t) = open[0].get_mut(&(fb, tb)).and_then(|t| t.full_mut()) {
                                t.close_column(margin, pct, scratch);
                            }
                        }
                        self.fold_finished_rows(fb, tb, i64::MAX);
                        continue;
                    }
                    // T-585: a coarse node's in-progress row lives in its `LiveTile`.
                    let flushed = match self.open[level].get_mut(&(fb, tb)) {
                        Some(OpenTile::Live(t)) => t.take_pending_row(),
                        _ => None,
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
                let Some(open) = self.open[level].remove(&(fb, tb)) else {
                    continue;
                };
                if self.defer_writes {
                    self.seal_deferred(level, open, margin, pct);
                    continue;
                }
                // T-585: a live coarse node is materialised into pooled scratch for its one
                // write — the tile is still the write unit — and the scratch goes back below.
                let (mut tile, live_tile) = match open {
                    OpenTile::Full(t) => (t, None),
                    OpenTile::Live(l) => {
                        let g = &self.geom.levels[level];
                        let mut t = match self.pool[level].pop() {
                            Some(t) => t,
                            None => Box::new(Tile::new(
                                l.key,
                                self.geom.nf,
                                g,
                                usize::from(self.cfg.histogram.bins),
                            )),
                        };
                        l.materialize_into(&mut t, g);
                        (t, Some(l))
                    }
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
                    self.fold_prov_into_consumers(level, &tile.prov, tile.key);
                }
                self.pool[level].push(tile);
                if let Some(l) = live_tile {
                    self.live_pool[level].push(l);
                }
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
        self.save_edge()?;
        self.enforce_budget()
    }

    /// T-901: one tile of [`Pyramid::seal_through_ns`]'s loop with deferred writes — the inline
    /// path's steps in its order, minus the file: the tile is indexed and queued, and a live coarse
    /// node is queued in its live form rather than materialised here, under the caller's lock.
    /// The consumers are folded without waiting for the write, which is the one difference from
    /// the inline path (there a failed write skips them); a failed write is still reported, by
    /// [`Pyramid::land_writes`].
    fn seal_deferred(&mut self, level: usize, open: OpenTile, margin: f32, pct: (f32, f32)) {
        match open {
            OpenTile::Live(l) => {
                if self.cfg.coarse_live {
                    self.fold_prov_into_consumers(level, &l.prov, l.key);
                }
                self.queue(level, Unwritten::Live(Arc::from(l)), true);
            }
            OpenTile::Full(mut t) => {
                if level == 0 {
                    t.close_column(margin, pct, &mut self.scratch);
                    self.update_floor(&t);
                }
                if !self.cfg.coarse_on_demand && !self.cfg.coarse_live {
                    self.fold_into_consumers(level, &t);
                }
                if self.cfg.coarse_live {
                    self.fold_prov_into_consumers(level, &t.prov, t.key);
                }
                self.queue(level, Unwritten::Full(Arc::from(t)), true);
            }
        }
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
            let Some(parent) = open_tile(
                &mut self.open[up],
                &mut self.pool[up],
                &mut self.live_pool[up],
                &self.geom,
                up,
                self.cfg.scheme,
                usize::from(hist_cfg.bins),
                pfb,
                ptb,
                false,
                &mut self.next_seal_ns,
            )
            .full_mut() else {
                debug_assert!(false, "a seal-time fold into a live-maintained node");
                continue;
            };
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
    fn fold_prov_into_consumers(&mut self, level: usize, prov: &ProvenanceSummary, key: TileKey) {
        let mut ups = std::mem::take(&mut self.consumers);
        ups.clear();
        ups.extend_from_slice(self.geom.consumers(level));
        let bins = usize::from(self.cfg.histogram.bins);
        let (scheme, live) = (self.cfg.scheme, self.cfg.coarse_live);
        for &up in &ups {
            let (pfb, ptb) = self.geom.fold_target(level, up, key.f_block, key.t_block);
            if self.sealed[up].contains_key(&(ptb, pfb)) {
                continue;
            }
            let parent = open_tile(
                &mut self.open[up],
                &mut self.pool[up],
                &mut self.live_pool[up],
                &self.geom,
                up,
                scheme,
                bins,
                pfb,
                ptb,
                live,
                &mut self.next_seal_ns,
            );
            parent.prov_mut().merge(prov);
        }
        self.consumers = ups;
    }

    /// The store's tolerance for frames arriving out of order, ns: [`PyramidConfig::seal_lag`].
    fn lag_ns(&self) -> i64 {
        i64::try_from(self.cfg.seal_lag.as_nanos()).unwrap_or(i64::MAX)
    }

    /// **When a level-0 row is finished, and so may be folded upwards (T-584).**
    ///
    /// Folds every not-yet-folded row of level-0 tile `(fb, tb)` whose column has closed *and*
    /// whose time cell ended at or before `through_ns`, oldest first, exactly once each.
    /// `i64::MAX` forces every closed row, which is what the pre-seal flush wants.
    ///
    /// # Why closing the column is not the same as finishing the row
    ///
    /// T-571 folded a row the moment its column closed. But a closed column is not a closed
    /// **cell**: a frame whose time cell has already closed is still folded into level 0 — in
    /// place, by [`Tile::add_value`] and [`Tile::add_late_occupancy`] — and it arrives after the
    /// fold has been and gone. The value was then on disk at level 0 and absent from every zoom
    /// above it: the same "present at the finest zoom, gone when you zoom out" shape T-571 fixed
    /// for checkpoints. Two windows produced it, and **both are ordinary, not pathological**:
    ///
    /// - mild frame disorder inside a cell, which the store already tolerates everywhere else —
    ///   `seal_lag` exists to hold a *tile* open for exactly this;
    /// - every frame arriving in the cell a [`Pyramid::checkpoint`] has just closed, which at the
    ///   shipped 60 s interval over a 1 s cell is one cell in sixty, for the rest of its second.
    ///
    /// Measured over a 600 s run at four frames a cell, against the on-demand control that
    /// re-folds the open producer on every read: **1.1 % of frames missing from every coarse node
    /// for the checkpoint window alone, 2.2 % for disorder at one boundary in ten, 3.3 % for
    /// both** — while node (0, 0) held all of them. `tests/live_coarse.rs` holds the measurement
    /// and the control.
    ///
    /// # The rule, and why it needs no new configuration
    ///
    /// A row is finished when the ingest clock has left its cell by `seal_lag` — the store's own
    /// declared disorder tolerance, the same number that decides when a *tile* may seal, applied
    /// one level down. Past that point a frame for the cell is refused as late anyway, so nothing
    /// can change the row afterwards and the fold is both exactly-once and complete. The
    /// alternatives the ticket listed are what this avoids: no subtract-then-re-add (the cascade
    /// accumulates, and `max`/`occ_max` do not invert), no pending late delta, and a checkpoint
    /// still closes its column, so a crash still loses nothing it did not lose before.
    ///
    /// With `seal_lag` at zero — no tolerance declared, which is what most tests configure — a
    /// row is finished as soon as the clock passes its cell end, so behaviour is exactly T-571's.
    /// With the shipped 2 s, a coarse node trails the live edge by a further 2 s on top of the
    /// in-progress-row lag T-583 measured; node (0, 0), the live edge itself, is untouched.
    fn fold_finished_rows(&mut self, fb: i64, tb: i64, through_ns: i64) {
        let (nf, t_cell_ns) = (self.geom.nf, self.geom.levels[0].t_cell_ns);
        loop {
            let Some(tile) = self.open[0].get_mut(&(fb, tb)).and_then(|t| t.full_mut()) else {
                return;
            };
            let t = tile.folded_through.map_or(0, |t| t + 1);
            if tile.col_done.is_none_or(|d| t > d)
                || (tile.t_cell0 + t as i64 + 1).saturating_mul(t_cell_ns) > through_ns
            {
                return;
            }
            tile.folded_through = Some(t);
            if tile.row_has_data(t) {
                self.fold_row_live(0, fb, tb, t, 0, nf);
            }
        }
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
    /// **Residency (T-585).** A coarse node holds its committed rows in stored form and only its
    /// in-progress row as accumulator ([`LiveTile`]): a completed row is folded into every
    /// consumer at full precision and *then* committed — encoded, its accumulator row released —
    /// so the node is one row of accumulator plus an encoded remainder, not a whole tile.
    /// Measured at the shipped geometry: 0.8 MB over 18 open coarse tiles where T-571 held
    /// 52.6 MB (`hk-pipeline/tests/live_edge_tiles.rs`). **No tile is written per row**: a coarse
    /// tile is written once, when it seals, exactly as before, which is why the commit-every-N
    /// rule costs zero extra writes.
    ///
    /// # How far a coarse node trails the live edge (T-583)
    ///
    /// The cascade propagates **on commit**, so a node that downsamples time by `2^j` from the
    /// finest one only hears about a row once `2^j` of them have closed, and a chain of
    /// in-progress rows compounds: node `(i, j)` trails the finest row by up to `2^j − 1` of
    /// them, plus the finest level's own still-open column, plus (T-584) the `seal_lag` a
    /// finished level-0 row waits out before it may fold — 2 s in the shipped config, zero in
    /// most tests. At the shipped four time levels and a 1 s cell the first two terms are up to
    /// **8 s** at `level_t = 3` (measured on the 4 x 4 test lattice at 0, 2, 4 and 8 finest rows
    /// for `level_t` 0 to 3, in the worst phase), and `seal_lag` adds to that. The cells are
    /// *correct* throughout — a partial row reads as fewer observed seconds, never as wrong
    /// values.
    ///
    /// **That trail stays here, and is undone on the read (T-583).** Propagating on commit is
    /// what makes this fold exactly-once, and doing more on the capture thread is what T-453
    /// forbids; but a pane must still *see* the rows, so [`Pyramid::live_preview`] folds what is
    /// in flight into the answer when a reader asks, the way a level-0 read stands in for its own
    /// open column with [`Tile::column_preview`].
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
        let (live, compression) = (self.cfg.coarse_live, self.cfg.compression_level);
        let mut q = std::mem::take(&mut self.row_queue);
        let mut out = std::mem::take(&mut self.row_out);
        let mut row_scratch = std::mem::replace(&mut self.row_scratch, RowAcc::new(0));
        let mut seg_buf = std::mem::take(&mut self.seg_buf);
        q.clear();
        q.push_back((level, fb, tb, t, f_lo, f_hi));
        while let Some((l, fb, tb, t, f_lo, f_hi)) = q.pop_front() {
            // Taken out so the consumer's accumulator may be borrowed mutably; put back below.
            // A consumer always has a higher level index than its producer (the geometry enforces
            // it), so nothing here can alias.
            let Some(mut child) = self.open[l].remove(&(fb, tb)) else {
                continue;
            };
            let mut ups = std::mem::take(&mut self.consumers);
            ups.clear();
            ups.extend_from_slice(self.geom.consumers(l));
            // T-585: the producer row as a fold source — a full tile's row, a live node's
            // in-progress row, or a committed one decoded into scratch (the reopen replay).
            let src = match &child {
                OpenTile::Full(c) => (t < c.nt).then(|| c.row_src(t)),
                OpenTile::Live(c) => c.row_src(t, &mut row_scratch),
            };
            if let Some(src) = src {
                for &up in &ups {
                    let (pfb, ptb) = self.geom.fold_target(l, up, fb, tb);
                    if self.sealed[up].contains_key(&(ptb, pfb)) {
                        continue;
                    }
                    let (ff, tf) = (self.geom.levels[up].f_factor, self.geom.levels[up].t_factor);
                    out.clear();
                    let Some(parent) = (match open_tile(
                        &mut self.open[up],
                        &mut self.pool[up],
                        &mut self.live_pool[up],
                        &self.geom,
                        up,
                        scheme,
                        bins,
                        pfb,
                        ptb,
                        live,
                        &mut self.next_seal_ns,
                    ) {
                        OpenTile::Live(p) => Some(p),
                        OpenTile::Full(_) => None,
                    }) else {
                        debug_assert!(false, "a live row fold into a full-accumulator node");
                        continue;
                    };
                    parent.fold_row(&src, f_lo, f_hi, ff, tf, &mut out);
                    self.stats.coarse_rows_folded += 1;
                    self.stats.coarse_cells_folded += (f_hi - f_lo) as u64;
                    if cascade {
                        for &(pt, pl, ph) in &out {
                            q.push_back((up, pfb, ptb, pt, pl, ph));
                        }
                    } else {
                        // Nothing reads these before the level above gets its own replay turn,
                        // which decodes them: commit at once.
                        for &(pt, pl, ph) in &out {
                            if let Some((raw, stored)) =
                                parent.commit(pt, pl, ph, compression, &mut seg_buf)
                            {
                                self.stats.coarse_rows_committed += 1;
                                self.stats.coarse_cells_committed += (ph - pl) as u64;
                                self.stats.coarse_row_bytes_raw += raw as u64;
                                self.stats.coarse_row_bytes_stored += stored as u64;
                            }
                        }
                    }
                }
            }
            // T-585: every consumer has now folded this completed row, so it is COMMITTED — held
            // in stored form from here to the seal, and its accumulator row released.
            if let OpenTile::Live(c) = &mut child
                && let Some((raw, stored)) = c.commit(t, f_lo, f_hi, compression, &mut seg_buf)
            {
                self.stats.coarse_rows_committed += 1;
                self.stats.coarse_cells_committed += (f_hi - f_lo) as u64;
                self.stats.coarse_row_bytes_raw += raw as u64;
                self.stats.coarse_row_bytes_stored += stored as u64;
            }
            self.consumers = ups;
            self.open[l].insert((fb, tb), child);
        }
        self.row_queue = q;
        self.row_out = out;
        self.row_scratch = row_scratch;
        self.seg_buf = seg_buf;
    }

    /// The `(f_block, t_block)` at the end of `chain` that block `(fb, tb)` of `chain[0]` folds
    /// into. `chain` must be a producer chain (each level the producer of the next).
    fn ascend(&self, chain: &[usize], fb: i64, tb: i64) -> (i64, i64) {
        let (mut fb, mut tb) = (fb, tb);
        for w in chain.windows(2) {
            (fb, tb) = self.geom.fold_target(w[0], w[1], fb, tb);
        }
        (fb, tb)
    }

    /// **A coarse node's in-progress row, folded at READ time (T-583).**
    ///
    /// [`Pyramid::fold_row_live`] propagates a row to the level above only when that level
    /// **commits** — the "commits every N" rule, and what makes the fold exactly-once — so a
    /// chain of in-progress rows compounds: node `(i, j)` trailed the finest closed row by up to
    /// `2^j − 1` of them, plus level 0's own still-open column. At four time levels and a 1 s cell
    /// that was up to 7 s of recorded, held, *unshown* data at the top of a zoomed-out pane.
    ///
    /// **The product decision T-571 left open (answered here: show it).** CLAUDE.md is explicit
    /// twice over — *"whenever data exists for that window it must be shown; a surface may render
    /// grey/empty only where data genuinely does not exist"*, and *"a level that downsamples every
    /// N rows **adjusts its in-progress top row as rows arrive**"*. A coarse cell that fills in
    /// under the viewer as its remaining producer rows land is the same thing the live edge
    /// already does at level 0 with [`Tile::column_preview`], and a partial row reads as fewer
    /// observed seconds — honest, never a wrong value. Trailing quietly is the failure case the
    /// display invariant names.
    ///
    /// **Why a read-time fold and not a push.** T-453's constraint binds the other end: work on
    /// the capture thread is paid whether or not anyone looks, and that thread gates the ring.
    /// Propagating an in-progress row eagerly would also have to be undone before the next one,
    /// which is a per-node running copy of a producer row — the residency [`Tile::fold_row`]
    /// refuses. So this runs **only when a reader asks**, exactly like `column_preview`, and costs
    /// capture nothing.
    ///
    /// **Why it cannot double-count.** It never touches the pyramid: it folds into *clones*,
    /// which are discarded with the answer. What it folds is only what has **not** propagated —
    /// level 0's open column (which folds upward when it closes) and each node's
    /// [`LiveTile::row_pending`] (which folds upward when it commits) — so the committed cells it
    /// starts from are each still written exactly once by the live cascade.
    ///
    /// Returns `None` when nothing is in flight below this address, which is every read of
    /// elapsed time; the caller then serves the open tile itself.
    pub(super) fn live_preview(
        &self,
        level: usize,
        fb: i64,
        tb: i64,
        margin: f32,
        pct: (f32, f32),
    ) -> Option<Box<Tile>> {
        if !self.cfg.coarse_live || level == 0 || level >= self.geom.n_levels() {
            return None;
        }
        // The producer chain, finest first. Every node has exactly one producer, so it is a path.
        let mut chain = vec![level];
        while let Some(from) = self.geom.levels[chain[chain.len() - 1]].from {
            chain.push(from);
        }
        chain.reverse();
        if chain[0] != 0 {
            return None;
        }
        let bins = usize::from(self.cfg.histogram.bins);
        let mut scratch = Vec::new();
        let mut out = Vec::new();
        // T-585: a committed producer row is decoded into this to be read.
        let mut row_scratch = RowAcc::new(self.geom.nf);
        // Preview tiles at the current level, each with the rows of it that have not propagated.
        let mut cur: Vec<PreviewTile> = Vec::new();
        for (k, &l) in chain.iter().enumerate() {
            // Seed with the open tiles of this level that hold an un-propagated row and lead to
            // this address — level 0's open column, or a coarser node's in-progress row. A node
            // whose own producer is momentarily idle is reached here rather than missed.
            let mut keys: Vec<(i64, i64)> = self.open[l]
                .keys()
                .copied()
                .filter(|&(cfb, ctb)| {
                    !cur.iter().any(|e| (e.0, e.1) == (cfb, ctb))
                        && self.ascend(&chain[k..], cfb, ctb) == (fb, tb)
                })
                .collect();
            keys.sort_unstable();
            for key in keys {
                let (tile, rows) = match &self.open[l][&key] {
                    OpenTile::Full(src) if l == 0 => {
                        if src.col_t.is_none() {
                            continue;
                        }
                        // The column as it would stand if it ended now: `column_preview`'s
                        // statement, taken on a clone so the occupancy and percentiles it decides
                        // fold upward too.
                        let mut t = Box::new((**src).clone());
                        let Some(row) = t.close_column(margin, pct, &mut scratch) else {
                            continue;
                        };
                        (OpenTile::Full(t), vec![(row, 0, self.geom.nf)])
                    }
                    // T-585: a coarse node's in-progress row lives in its `LiveTile`.
                    OpenTile::Live(src) if l > 0 => {
                        let Some(r) = src.row_pending else {
                            continue;
                        };
                        (OpenTile::Live(src.clone()), vec![r])
                    }
                    _ => continue,
                };
                self.count_source_tile();
                cur.push((key.0, key.1, tile, rows));
            }
            let Some(&up) = chain.get(k + 1) else { break };
            let (ff, tf) = (self.geom.levels[up].f_factor, self.geom.levels[up].t_factor);
            let mut next: Vec<PreviewTile> = Vec::new();
            for (cfb, ctb, child, rows) in cur.drain(..) {
                let (pfb, ptb) = self.geom.fold_target(l, up, cfb, ctb);
                if self.sealed[up].contains_key(&(ptb, pfb)) {
                    // Sealed: it is already the whole of its block, and nothing may change it.
                    continue;
                }
                let i = match next.iter().position(|e| (e.0, e.1) == (pfb, ptb)) {
                    Some(i) => i,
                    None => {
                        let t = match self.open[up].get(&(pfb, ptb)) {
                            Some(OpenTile::Live(t)) => {
                                self.count_source_tile();
                                t.clone()
                            }
                            // Every coarse node of a live scheme is a `LiveTile` (T-585).
                            Some(OpenTile::Full(_)) => {
                                debug_assert!(false, "a live-maintained node held in full");
                                continue;
                            }
                            // Its first producer row is still in flight, so it does not exist yet.
                            None => Box::new(LiveTile::new(
                                TileKey {
                                    scheme: self.cfg.scheme,
                                    level: up as u8,
                                    f_block: pfb,
                                    t_block: ptb,
                                },
                                self.geom.nf,
                                &self.geom.levels[up],
                            )),
                        };
                        next.push((pfb, ptb, OpenTile::Live(t), Vec::new()));
                        next.len() - 1
                    }
                };
                let e = &mut next[i];
                let OpenTile::Live(parent) = &mut e.2 else {
                    unreachable!("a preview parent is always a LiveTile");
                };
                for (t, lo, hi) in rows {
                    // T-585: the producer row as a fold source — a full tile's row, a live node's
                    // in-progress row, or a committed one decoded into scratch.
                    let src = match &child {
                        OpenTile::Full(c) => (t < c.nt).then(|| c.row_src(t)),
                        OpenTile::Live(c) => c.row_src(t, &mut row_scratch),
                    };
                    let Some(src) = src else { continue };
                    out.clear();
                    parent.fold_row(&src, lo, hi, ff, tf, &mut out);
                    self.preview_rows
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    e.3.extend(out.iter().copied());
                }
            }
            if up != level {
                // The row each of these is still accumulating has not propagated either — that is
                // the whole lag — so it carries on up with the rows this step completed.
                for e in &mut next {
                    if let OpenTile::Live(t) = &e.2
                        && let Some(r) = t.row_pending
                    {
                        e.3.push(r);
                    }
                }
                next.retain(|e| !e.3.is_empty());
            }
            cur = next;
        }
        cur.into_iter()
            .find(|e| (e.0, e.1) == (fb, tb))
            .map(|e| match e.2 {
                OpenTile::Full(t) => t,
                // T-585: served materialised, exactly as an open live node is read.
                OpenTile::Live(l) => {
                    let g = &self.geom.levels[level];
                    let mut t = Box::new(Tile::new(l.key, self.geom.nf, g, bins));
                    l.materialize_into(&mut t, g);
                    t
                }
            })
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
                    match t {
                        OpenTile::Full(t) => Some(t),
                        // T-585: an open live node is read materialised, like a sealed one.
                        OpenTile::Live(l) => {
                            let g = &self.geom.levels[from];
                            let mut t =
                                Tile::new(l.key, self.geom.nf, g, usize::from(hist_cfg.bins));
                            l.materialize_into(&mut t, g);
                            owned = Some(t);
                            owned.as_ref()
                        }
                    }
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

    /// **T-901: defer every file write a seal or checkpoint makes** to [`Pyramid::take_writes`],
    /// so a caller can do the encode and the disk work with its lock released — see
    /// [`super::deferred`] for the stall this removes and what it costs. A sealed tile stays
    /// readable from memory until it lands. Turning the mode off flushes what is queued, inline;
    /// do that with no batch out ([`Pyramid::take_writes`]), or the rest lands on the next take.
    pub fn set_deferred_writes(&mut self, on: bool) -> Result<(), StoreError> {
        if !on && self.defer_writes {
            self.defer_writes = false;
            return self.flush_writes();
        }
        self.defer_writes = on;
        Ok(())
    }

    /// Whether seals queue their writes ([`Pyramid::set_deferred_writes`]).
    pub fn deferred_writes(&self) -> bool {
        self.defer_writes
    }

    /// Files queued and not yet taken.
    pub fn queued_writes(&self) -> usize {
        self.write_queue.len()
    }

    /// Sealed tiles held in memory because their file has not landed yet.
    pub fn unwritten_tiles(&self) -> usize {
        self.unwritten.len()
    }

    /// Takes the queued file work — a move, no I/O — to be [performed](PendingWrites::perform)
    /// **outside** the lock and handed back to [`Pyramid::land_writes`]. **Empty while an earlier
    /// batch is still out**: one batch performs at a time, so writes land in the order they were
    /// queued whichever thread performs them. What is left waits for the next take.
    pub fn take_writes(&mut self) -> PendingWrites {
        if self.writes_in_flight || self.write_queue.is_empty() {
            return PendingWrites::default();
        }
        self.writes_in_flight = true;
        PendingWrites {
            jobs: std::mem::take(&mut self.write_queue),
        }
    }

    /// Books a performed batch: bytes into the budget, each landed tile out of memory, a file
    /// written for a tile evicted in the meantime removed. Returns the first write error, after
    /// booking everything else; a tile whose write failed is dropped from the index, as a failed
    /// inline write never enters it.
    pub fn land_writes(&mut self, batch: WrittenBatch) -> Result<(), StoreError> {
        if !batch.results.is_empty() {
            self.writes_in_flight = false;
        }
        let mut first_err = None;
        for (job, result) in batch.results {
            match job.body {
                Body::Tile {
                    level,
                    tile,
                    sealed,
                    ..
                } => {
                    let k = tile.key();
                    let (fb, tb) = (k.f_block, k.t_block);
                    let key = (level, fb, tb);
                    // Still the newest version of this tile? A retention rewrite queued behind it
                    // does its own booking when it lands.
                    let current = self.unwritten.get(&key).is_some_and(|t| t.same(&tile));
                    if current {
                        self.unwritten.remove(&key);
                    }
                    let (bytes, raw) = match result {
                        Ok(v) => v,
                        Err(source) => {
                            if sealed
                                && current
                                && let Some(b) = self.sealed[level].remove(&(tb, fb))
                            {
                                self.disk_bytes -= b;
                                self.level_bytes[level] -= b;
                                self.unprotected[level].remove(&(tb, fb));
                            }
                            first_err.get_or_insert(StoreError::Io {
                                path: job.path,
                                source,
                            });
                            continue;
                        }
                    };
                    self.stats.bytes_written += bytes;
                    self.stats.raw_bytes_written += raw;
                    if !sealed {
                        self.stats.checkpoints_written += 1;
                        let old = self.checkpoints.insert((fb, tb), bytes);
                        self.disk_bytes = self.disk_bytes - old.unwrap_or(0) + bytes;
                        continue;
                    }
                    match self.sealed[level].get_mut(&(tb, fb)) {
                        // Evicted while its write was in flight: the file must not outlive it.
                        None => {
                            let _ = fs::remove_file(&job.path);
                        }
                        Some(b) if current => {
                            let old = std::mem::replace(b, bytes);
                            self.level_bytes[level] = self.level_bytes[level] - old + bytes;
                            self.disk_bytes = self.disk_bytes - old + bytes;
                            if level == 0
                                && let Some(cp) = self.checkpoints.remove(&(fb, tb))
                            {
                                self.disk_bytes -= cp;
                            }
                        }
                        Some(_) => {}
                    }
                }
                Body::Bytes { what, .. } => {
                    if let Err(source) = result {
                        match what {
                            SmallFile::SourceStates => self.state_dirty = true,
                            SmallFile::RecordingBegan => self.began_persisted = false,
                            SmallFile::Edge => self.edge_persisted = (i64::MIN, i64::MIN),
                        }
                        first_err.get_or_insert(StoreError::Io {
                            path: job.path,
                            source,
                        });
                    }
                }
            }
        }
        first_err.map_or(Ok(()), Err)
    }

    /// Performs and lands whatever is queued, **inline** — the lock-holding path, for the end of a
    /// run or a caller with no lock to release. A no-op while another batch is out (its flusher
    /// takes the rest next).
    pub fn flush_writes(&mut self) -> Result<(), StoreError> {
        let done = self.take_writes().perform();
        self.land_writes(done)
    }

    /// [`Pyramid::flush_writes`] for an owner — [`Pyramid::close`] and drop — that no other
    /// thread can be flushing for, because nothing else holds the pyramid. A batch still marked
    /// out was taken and never landed, so it is gone; what is queued behind it is not.
    fn flush_owned(&mut self) -> Result<(), StoreError> {
        self.writes_in_flight = false;
        self.flush_writes()
    }

    /// T-901: [`Pyramid::write_tile`]'s deferred half. A sealed tile is indexed now, with 0
    /// bytes until it lands, so retention and every read see it as sealed from this instant.
    fn queue_tile(&mut self, level: usize, tile: &Tile, sealed: bool) {
        self.queue(level, Unwritten::Full(Arc::new(tile.clone())), sealed);
    }

    /// Queues `tile`'s file; a sealed one is indexed and readable from memory until it lands.
    fn queue(&mut self, level: usize, tile: Unwritten, sealed: bool) {
        let k = tile.key();
        let (fb, tb) = (k.f_block, k.t_block);
        if sealed {
            self.unwritten.insert((level, fb, tb), tile.clone());
            if let std::collections::btree_map::Entry::Vacant(e) =
                self.sealed[level].entry((tb, fb))
            {
                e.insert(0);
                self.stats.tiles_written += 1;
                self.index_sealed(level, tb, fb);
            }
        }
        let path = self.path(level, fb, tb);
        let tmp = path.with_file_name(format!("t{tb}.tile.tmp{}", std::process::id()));
        let enc = EncodeArgs {
            unit: self.cfg.unit,
            geom: self.geom.levels[level],
            hist: self.cfg.histogram,
            pct: self.pct(),
            compression: self.cfg.compression_level,
            nf: self.geom.nf,
            bins: usize::from(self.cfg.histogram.bins),
        };
        self.write_queue.push(Job {
            path,
            tmp,
            body: Body::Tile {
                level,
                tile,
                sealed,
                enc,
            },
        });
    }

    fn write_tile(&mut self, level: usize, tile: &Tile, sealed: bool) -> Result<(), StoreError> {
        if self.defer_writes {
            self.queue_tile(level, tile, sealed);
            return Ok(());
        }
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
        // T-901: a tile evicted before its file landed; `land_writes` removes the file it finds
        // written for a tile no longer indexed.
        self.unwritten.remove(&(level, fb, tb));
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
    /// **T-571: the column this closes is a row like any other, and must still reach the coarse
    /// nodes.** It did not, and the loss was silent and permanent: the only paths into
    /// [`Pyramid::fold_row_live`] were the column advance in [`Pyramid::ingest`] and the pre-seal
    /// flush, and a column closed here reached neither — the next frame found `col_t` at `None`
    /// and the seal's own `close_column` returned `None`. With the shipped 60 s checkpoint interval
    /// over a 1 s time cell that is **one row in sixty missing from every coarse node, on disk**,
    /// so a burst inside a dropped second is present at the finest zoom and vanishes when you zoom
    /// out — the max-hold that would have carried it was never folded.
    ///
    /// **T-584: the column this closes is on disk, but it is not a finished row.** Frames that
    /// arrive in the **same** time cell afterwards take [`Tile::add_late_occupancy`]'s late path,
    /// which updates level 0 in place — and at the shipped 60 s interval over a 1 s cell that is
    /// one cell in sixty, exposed for the rest of its second, measured at 1.1 % of frames absent
    /// from every coarse node. So the checkpoint still closes the column (a crash must not find
    /// an unrecoverable in-progress column, which is what the close is for) but no longer folds
    /// it upwards: the row goes up when the ingest clock has left it, like every other row. See
    /// [`Pyramid::fold_finished_rows`].
    pub fn checkpoint(&mut self) -> Result<(), StoreError> {
        let margin = self.cfg.occupancy_margin_db;
        let pct = self.pct();
        let mut keys = std::mem::take(&mut self.keys);
        keys.clear();
        keys.extend(self.open[0].keys().copied());
        let mut result = Ok(());
        for &k in &keys {
            let Some(OpenTile::Full(mut tile)) = self.open[0].remove(&k) else {
                continue;
            };
            tile.close_column(margin, pct, &mut self.scratch);
            let written = self.write_tile(0, &tile, false);
            self.open[0].insert(k, OpenTile::Full(tile));
            if self.cfg.coarse_live {
                // T-584: the column this closed is on disk, but it is NOT finished — frames may
                // still arrive in that cell, and they did. It folds upwards when the ingest
                // clock has left it, like every other row.
                let through = self.latest_ns.saturating_sub(self.lag_ns());
                self.fold_finished_rows(k.0, k.1, through);
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
        self.save_recording_began()?;
        self.save_edge()
    }

    /// Checkpoints and closes. Dropping without `close` loses at most one checkpoint interval of
    /// level-0 data (the crash case).
    pub fn close(mut self) -> Result<(), StoreError> {
        self.checkpoint()?;
        self.flush_owned()
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
            return Ok(Some(t.prov().clone()));
        }
        if !self.sealed[level].contains_key(&(tb, fb)) {
            return Ok(None);
        }
        if let Some(t) = self.unwritten.get(&(level, fb, tb)) {
            return Ok(Some(t.prov().clone()));
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
        if let Some(t) = self.unwritten.get(&(level, fb, tb)) {
            let bins = usize::from(self.cfg.histogram.bins);
            return Ok(Some(t.to_tile(
                self.geom.nf,
                &self.geom.levels[level],
                bins,
            )));
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

    /// **T-942: undoes the end-of-run FORCED seal** ([`Pyramid::seal_all`]), so a restart
    /// continues the tiles the run before it was in the middle of instead of leaving them frozen
    /// with a watermark standing over them.
    ///
    /// A tile whose time block ends after the store's newest frame was sealed because a run
    /// ended, not because its time was over. There are two kinds and they need opposite
    /// treatment:
    ///
    /// - **Level 0** is fed by `ingest` and holds measurements nothing else can reproduce: it is
    ///   read back and reopened, keeping what it holds, and its file is rewritten when the block
    ///   genuinely ends.
    /// - **A coarser tile is a fold of its children**, and its children include the level-0 tile
    ///   just reopened — which will fold into it *again* when it really seals. So it is dropped,
    ///   not reopened, and [`Pyramid::recover`]'s rebuild below re-folds it from the sealed
    ///   children it still has. Reopening both would count the reopened tile's rows twice.
    ///
    /// A `coarse_live` lattice is left alone at coarse levels: it never forces a seal past its
    /// data (its run-end seal is through the last frame), and its coarse nodes are rebuilt by
    /// replaying rows, not by folding sealed children.
    fn reopen_sealed_ahead(&mut self) -> Result<(), StoreError> {
        if self.watermark_ns == i64::MIN {
            return Ok(());
        }
        let cut = self.watermark_ns;
        for level in 0..self.geom.n_levels() {
            if level > 0 && self.cfg.coarse_live {
                continue;
            }
            let ahead: Vec<(i64, i64)> = self.sealed[level]
                .keys()
                .filter(|&&(tb, _)| self.geom.block_end_ns(level, tb) > cut)
                .copied()
                .collect();
            for (tb, fb) in ahead {
                let tile = if level == 0 {
                    self.read_sealed(0, fb, tb)?
                } else {
                    None
                };
                self.drop_sealed_index(level, tb, fb);
                match tile {
                    Some(t) => {
                        self.next_seal_ns =
                            self.next_seal_ns.min(self.geom.block_end_ns(level, tb));
                        self.open[level].insert((fb, tb), OpenTile::Full(Box::new(t)));
                        self.stats.tiles_reopened += 1;
                    }
                    // A coarser tile (dropped, to be re-folded), or a level-0 file that would not
                    // decode — in which case the index is the only thing that claimed it existed.
                    None => {
                        let _ = fs::remove_file(self.path(level, fb, tb));
                    }
                }
            }
        }
        Ok(())
    }

    /// Forgets a sealed tile's index entries (T-942), leaving the file alone: the caller either
    /// reopens the tile — whose next write replaces the file — or removes it.
    fn drop_sealed_index(&mut self, level: usize, tb: i64, fb: i64) {
        if let Some(bytes) = self.sealed[level].remove(&(tb, fb)) {
            self.disk_bytes -= bytes;
            self.level_bytes[level] -= bytes;
        }
        self.unprotected[level].remove(&(tb, fb));
        self.due[level].retain(|&(_, t, f)| t != tb || f != fb);
    }

    fn recover(&mut self) -> Result<(), StoreError> {
        let top = self.geom.top();
        // **T-942: the resumed watermark is READ BACK, not inferred from the tiles.**
        //
        // It used to be the newest sealed block end of *every* level — a time block, not a
        // measurement, and a block end is its lattice's rounding-**up** of the data inside it.
        // Scheme 1's level 2 is a one-hour tile, so a store whose last run ended at 04:26 came
        // back claiming everything to 05:00 had sealed, and `ingest` answers `Late` for every
        // frame whose level-0 block ends at or before the watermark: the whole of the next run's
        // history, refused, silently, for 34 minutes. `latest_frame_end` is resumed here too, so
        // the store also reported its newest recorded row half an hour in the future, which is
        // what `/api/navigation` and `/api/analysis/strongest` then served. Measured on staging
        // by the explorer, 2026-09-25 (`frames_ingested 0`, `frames_late 2024`).
        //
        // The watermark is a decision, so it is persisted with the data's own edge
        // ([`EDGE_FILE`]) and read back exactly. That also makes "sealed past the watermark"
        // meaningful, which is what [`Pyramid::reopen_sealed_ahead`] undoes.
        match self.resumed_edge {
            Some((latest, watermark)) => {
                self.watermark_ns = watermark;
                self.latest_ns = latest;
                // Every tile past the watermark was sealed by force, not by time.
                self.reopen_sealed_ahead()?;
            }
            // **A store written before T-942 has no record of its own edge**, so the tiles are
            // all there is to read it from — and a tile's time block rounds *up*. Level 0's
            // newest sealed block end is the tightest of those bounds and the only one `ingest`
            // tests: up to one level-0 block (a minute, at scheme 1) past the data if the last
            // run forced a seal, counted as late and healed at the next block, against the hour
            // of silence the coarse levels used to buy. Nothing is reopened here, because
            // without the watermark there is no way to tell a forced seal from an asked-for one
            // — so a legacy store loses at most its last coarse block's summary, once, and is
            // written with an edge from this run on.
            None => {
                self.watermark_ns = self.sealed[0]
                    .last_key_value()
                    .map_or(i64::MIN, |(&(tb, _), _)| self.geom.block_end_ns(0, tb));
                self.latest_ns = self.watermark_ns;
            }
        }
        let bins = usize::from(self.cfg.histogram.bins);
        let mut cps: Vec<(i64, i64)> = self.checkpoints.keys().copied().collect();
        cps.sort_unstable();
        for (fb, tb) in cps {
            let path = self.path(0, fb, tb);
            match codec::decode(&path, &self.geom.levels[0], bins).map_err(io_err(&path))? {
                Some(tile) => {
                    self.next_seal_ns = self.next_seal_ns.min(self.geom.block_end_ns(0, tb));
                    self.open[0].insert((fb, tb), OpenTile::Full(Box::new(tile)));
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
                        self.open[level].insert((fb, tb), OpenTile::Full(Box::new(t)));
                    }
                    // Ascending time within a tile: the cascade's in-progress row is flushed by a
                    // later row arriving, so out-of-order rows would strand it.
                    //
                    // T-585: a live node replays exactly its COMMITTED footprints — the same
                    // `(row, f_lo, f_hi)` its consumers were fed live — and never its pending
                    // row. That row is still being adjusted: it is emitted upwards when it
                    // completes (or at its tile's seal), and replaying it here as well would fold
                    // its partial contents into the level above twice.
                    let rows: Vec<(usize, usize, usize)> = match &self.open[level][&(fb, tb)] {
                        OpenTile::Full(tile) => (0..tile.nt)
                            .filter(|&t| (0..tile.nf).any(|f| tile.count[t * tile.nf + f] > 0))
                            .map(|t| (t, 0, nf))
                            .collect(),
                        OpenTile::Live(l) => l.committed_footprints(),
                    };
                    for (t, lo, hi) in rows {
                        self.fold_row_from(level, fb, tb, t, lo, hi, false);
                    }
                    if !was_open && let Some(OpenTile::Full(t)) = self.open[level].remove(&(fb, tb))
                    {
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

/// T-901: a pyramid dropped with writes still queued performs them, so deferring can lose no more
/// than an inline seal would. The error has nowhere to go; [`Pyramid::close`] returns it.
impl Drop for Pyramid {
    fn drop(&mut self) {
        if !self.write_queue.is_empty() {
            let _ = self.flush_owned();
        }
    }
}
