//! **Deferred writes (T-901): a seal that does its file work outside whatever lock guards the
//! pyramid.**
//!
//! # The stall this exists to remove
//!
//! [`Pyramid::ingest`] seals inline: when the ingest clock passes a tile's block end, every tile
//! due is encoded (a zstd pass), created, written, **fsync'd** and renamed before `ingest`
//! returns. On the view lattice that is ~24 tiles per level-0 block (64 rows, ~2.6 s at the
//! display row rate), and measured on the dev box under a normal agent load it took **1.0-4.5 s
//! per seal** — 100-370 ms per tile, almost all of it the encode. The view writer held the
//! pyramid's mutex across it, and every `/ws/tiles/rows` step, every `/api/tiles` read and the
//! coverage evidence behind both take that mutex. So the row pushes of a live subscription stopped
//! for the whole seal, every 64 rows: the T-901 finding, and a direct violation of "rows append
//! in real time as they are recorded".
//!
//! # The mode
//!
//! With [`Pyramid::set_deferred_writes`] on, a tile that seals is **indexed as sealed at once and
//! kept in memory** until its file lands; every read that would open the file
//! ([`Pyramid::read_sealed`], a query's source lookup, [`Pyramid::tile_provenance`]) answers from
//! the in-memory copy instead, so there is no instant at which a sealed tile reads *unobserved*.
//! The work that touches the disk — encoding, compressing, writing, fsync, rename — is queued as a
//! [`PendingWrites`], which the caller takes under its lock ([`Pyramid::take_writes`], a move),
//! **performs with no lock held** ([`PendingWrites::perform`]) and hands back under the lock
//! ([`Pyramid::land_writes`], bookkeeping only). The file bytes are exactly what the inline path
//! writes: the same encoder over the same tile.
//!
//! # What it costs, stated
//!
//! - **Byte accounting lags by one flush.** A queued tile counts 0 bytes towards the budget until
//!   it lands, so [`Pyramid::disk_bytes`] and the eviction it drives can trail by one batch.
//! - **One flusher.** Jobs are performed in queue order, and a rewrite of the same tile (a
//!   retention trim) is only ordered behind the first write if the same thread performs both. The
//!   view writer is the only thread that takes writes.
//! - **A crash loses what was queued**, exactly as a crash mid-seal loses the seal in progress:
//!   the level-0 checkpoint on disk is recovered as an open tile and seals again on reopen.
//! - **Memory**: a queued tile is a clone held until it lands — one seal's worth, about the
//!   resident size of the tiles that sealed.
//!
//! Off by default: scheme 1 and every test that opens a pyramid write inline, as they always did.
//! [`Pyramid::close`], turning the mode off, and dropping the pyramid all flush what is queued.
//!
//! [`Pyramid::ingest`]: super::Pyramid::ingest
//! [`Pyramid::set_deferred_writes`]: super::Pyramid::set_deferred_writes
//! [`Pyramid::read_sealed`]: super::Pyramid
//! [`Pyramid::tile_provenance`]: super::Pyramid
//! [`Pyramid::take_writes`]: super::Pyramid::take_writes
//! [`Pyramid::land_writes`]: super::Pyramid::land_writes
//! [`Pyramid::disk_bytes`]: super::Pyramid::disk_bytes
//! [`Pyramid::close`]: super::Pyramid::close

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use hk_model::PowerUnit;

use super::codec;
use super::config::{HistogramConfig, LevelGeometry};
use super::live::LiveTile;
use super::tile::{ProvenanceSummary, Tile};

/// A sealed tile whose file has not landed, **in the form it sealed in**: a full accumulator, or a
/// live coarse node (T-585) still holding its committed rows encoded. The live form is not
/// materialised under the lock — that is ~0.1-0.6 s of a seal on the view lattice — but when the
/// write is performed, and on the (rare) read that reaches it first, exactly as an open live node
/// is read.
#[derive(Clone, Debug)]
pub(super) enum Unwritten {
    Full(Arc<Tile>),
    Live(Arc<LiveTile>),
}

impl Unwritten {
    /// The same queued version (a rewrite queued behind replaces the entry).
    pub fn same(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Full(a), Self::Full(b)) => Arc::ptr_eq(a, b),
            (Self::Live(a), Self::Live(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }

    pub fn key(&self) -> hk_model::TileKey {
        match self {
            Self::Full(t) => t.key,
            Self::Live(t) => t.key,
        }
    }

    pub fn prov(&self) -> &ProvenanceSummary {
        match self {
            Self::Full(t) => &t.prov,
            Self::Live(t) => &t.prov,
        }
    }

    /// An owned full tile: a clone, or the live node materialised onto its level's grid.
    pub fn to_tile(&self, nf: usize, g: &LevelGeometry, bins: usize) -> Tile {
        match self {
            Self::Full(t) => (**t).clone(),
            Self::Live(l) => {
                let mut t = Tile::new(l.key, nf, g, bins);
                l.materialize_into(&mut t, g);
                t
            }
        }
    }
}

/// The encoder's arguments, captured when the tile was queued so the encode needs no pyramid.
#[derive(Clone, Copy, Debug)]
pub(super) struct EncodeArgs {
    pub unit: PowerUnit,
    pub geom: LevelGeometry,
    pub hist: HistogramConfig,
    pub pct: (f32, f32),
    pub compression: Option<i32>,
    /// Frequency cells per tile and histogram bins, to materialise a live node.
    pub nf: usize,
    pub bins: usize,
}

/// A small non-tile file the seal path also persists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SmallFile {
    /// The per-source front-end state (T-126).
    SourceStates,
    /// When this store began recording (T-507).
    RecordingBegan,
}

#[derive(Debug)]
pub(super) enum Body {
    /// A tile file. `sealed` is false for a level-0 checkpoint of an open tile.
    Tile {
        level: usize,
        tile: Unwritten,
        sealed: bool,
        enc: EncodeArgs,
    },
    /// Bytes already encoded (small, so encoding them under the lock costs nothing).
    Bytes { what: SmallFile, bytes: Vec<u8> },
}

/// One queued file write.
#[derive(Debug)]
pub(super) struct Job {
    pub path: PathBuf,
    pub tmp: PathBuf,
    pub body: Body,
}

/// File work a seal queued, to be performed **with no lock held**.
#[derive(Debug, Default)]
#[must_use = "queued writes are lost unless performed and landed"]
pub struct PendingWrites {
    pub(super) jobs: Vec<Job>,
}

/// What [`PendingWrites::perform`] did, to be handed back to [`super::Pyramid::land_writes`].
#[derive(Debug, Default)]
#[must_use = "a written batch must be landed, or its tiles stay pinned in memory"]
pub struct WrittenBatch {
    /// Each job with `(file bytes, raw bytes)` or the error that stopped it.
    pub(super) results: Vec<(Job, io::Result<(u64, u64)>)>,
}

impl PendingWrites {
    /// Nothing queued.
    pub fn is_empty(&self) -> bool {
        self.jobs.is_empty()
    }

    /// Files queued.
    pub fn len(&self) -> usize {
        self.jobs.len()
    }

    /// Encodes and writes every queued file, in queue order: temp → fsync → rename, the inline
    /// path's own discipline. Touches no pyramid, so the caller holds no lock while it runs.
    pub fn perform(self) -> WrittenBatch {
        let (mut buf, mut payload) = (Vec::new(), Vec::new());
        let results = self
            .jobs
            .into_iter()
            .map(|job| {
                let r = match &job.body {
                    Body::Tile {
                        tile, sealed, enc, ..
                    } => {
                        let owned;
                        let tile: &Tile = match tile {
                            Unwritten::Full(t) => t,
                            Unwritten::Live(_) => {
                                owned = tile.to_tile(enc.nf, &enc.geom, enc.bins);
                                &owned
                            }
                        };
                        let raw = codec::encode(
                            tile,
                            *sealed,
                            enc.unit,
                            &enc.geom,
                            &enc.hist,
                            enc.pct,
                            enc.compression,
                            &mut buf,
                            &mut payload,
                        );
                        write_atomic(&job.path, &job.tmp, &buf, true)
                            .map(|()| (buf.len() as u64, raw))
                    }
                    Body::Bytes { bytes, .. } => write_atomic(&job.path, &job.tmp, bytes, false)
                        .map(|()| (bytes.len() as u64, bytes.len() as u64)),
                };
                (job, r)
            })
            .collect();
        WrittenBatch { results }
    }
}

/// `bytes` to `path` through `tmp`: create, write, fsync, rename. The temp is removed on failure.
fn write_atomic(path: &Path, tmp: &Path, bytes: &[u8], mkdir: bool) -> io::Result<()> {
    if mkdir && let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let write = || -> io::Result<()> {
        let mut f = fs::File::create(tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(tmp, path)
    };
    write().inspect_err(|_| {
        let _ = fs::remove_file(tmp);
    })
}
