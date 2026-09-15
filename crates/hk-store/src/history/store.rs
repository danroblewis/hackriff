//! The [`Pyramid`]: ingest, sealing and rollup, rolling byte budget, checkpoints and recovery.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use hk_model::{TileKey, Timestamp};

use super::StoreError;
use super::codec;
use super::config::{Geometry, PyramidConfig};
use super::frame::{FrameInput, NoiseShape, RegridPlan};
use super::stats::{db, hist_percentile};
use super::tile::{ColEntry, FrontEndState, ProvenanceStep, Tile};

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
    /// Bytes evicted.
    pub bytes_evicted: u64,
    /// Temp, truncated or corrupt files ignored (and removed) on open or read.
    pub files_ignored: u64,
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

/// Per-f-block default floor: min over the last `w` sealed tiles' per-cell low percentile.
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
    pub(super) floors: HashMap<i64, FloorTrack>,
    /// Every tile whose block ends at or before this has sealed.
    watermark_ns: i64,
    latest_ns: i64,
    next_seal_ns: i64,
    last_checkpoint_ns: Option<i64>,
    scratch: Vec<f32>,
    group_hist: Vec<u32>,
    keys: Vec<(i64, i64)>,
    buf: Vec<u8>,
    payload: Vec<u8>,
    /// Front-end state of the last folded frame (step detection, T-116).
    last_state: Option<FrontEndState>,
    /// Cell shape of the last STFT resolution seen.
    shape_cache: Option<(hk_dsp::spectrum::Resolution, f32)>,
    stats: PyramidStats,
}

fn dur_ns(d: std::time::Duration) -> i64 {
    i64::try_from(d.as_nanos()).unwrap_or(i64::MAX)
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
            sealed: (0..n).map(|_| BTreeMap::new()).collect(),
            checkpoints: HashMap::new(),
            disk_bytes: 0,
            level_bytes: vec![0; n],
            pool: (0..n).map(|_| Vec::new()).collect(),
            plan: RegridPlan::default(),
            floors: HashMap::new(),
            watermark_ns: i64::MIN,
            latest_ns: i64::MIN,
            next_seal_ns: i64::MAX,
            last_checkpoint_ns: None,
            scratch: Vec::new(),
            group_hist: vec![0; bins],
            keys: Vec::new(),
            buf: Vec::new(),
            payload: Vec::new(),
            last_state: None,
            shape_cache: None,
            stats: PyramidStats::default(),
            cfg: config,
            geom,
            root,
        };
        p.scan()?;
        p.recover()?;
        Ok(p)
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
        let step = match self.last_state {
            Some(prev) if prev.changes(&state) != 0 => Some(ProvenanceStep {
                t: frame.t,
                changed: prev.changes(&state),
                from: prev,
                to: state,
            }),
            _ => None,
        };
        self.last_state = Some(state);
        let cell_shape = self.resolve_shape(frame.noise_shape);
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
        let Self {
            plan,
            open,
            pool,
            floors,
            scratch,
            geom,
            next_seal_ns,
            ..
        } = self;
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
            let floor = floors.get(&fb).map(|f| &f.floor[..]);
            if tile.col_t.is_some_and(|c| t_in > c) {
                tile.close_column(floor, margin, pct, scratch);
            }
            let late =
                tile.col_done.is_some_and(|d| t_in <= d) || tile.col_t.is_some_and(|c| t_in < c);
            if !late && tile.col_t.is_none() {
                tile.col_t = Some(t_in);
            }
            for s in &cells[i..j] {
                let v_lin = plan.mean(s, frame.psd);
                if !v_lin.is_finite() {
                    continue;
                }
                let v_db = db(v_lin);
                let pk_db = db(f64::from(plan.max(s, peak)).max(v_lin));
                let f = (s.cell - fb * nf) as usize;
                let thr = frame
                    .floor_db
                    .map_or(f32::NAN, |fl| plan.mean(s, fl) as f32 + margin);
                tile.add_value(t_in, f, v_db, pk_db, v_lin, dur_s, &hist_cfg);
                if late {
                    let thr = if thr.is_nan() {
                        floor.map_or(f32::NAN, |fl| fl[f] + margin)
                    } else {
                        thr
                    };
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
                .add_frame(frame, &state, step.as_ref(), cell_shape);
            i = j;
        }
        self.stats.frames_folded += 1;
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

    /// The Gamma shape of a level-0 cell value for `shape` (cached per STFT resolution).
    fn resolve_shape(&mut self, shape: NoiseShape) -> Option<f32> {
        match shape {
            NoiseShape::Unknown => None,
            NoiseShape::CellShape(k) => (k.is_finite() && k > 0.0).then_some(k),
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
            let mut result = Ok(());
            for &(fb, tb) in &keys {
                let Some(mut tile) = self.open[level].remove(&(fb, tb)) else {
                    continue;
                };
                if level == 0 {
                    let floor = self.floors.get(&fb).map(|f| &f.floor[..]);
                    tile.close_column(floor, margin, pct, &mut self.scratch);
                    self.update_floor(&tile);
                }
                let written = self.write_tile(level, &tile, true);
                if written.is_ok() && level < self.geom.top() {
                    self.fold_into_parent(level, &tile);
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
        self.enforce_budget()
    }

    fn update_floor(&mut self, tile: &Tile) {
        let (w, nf) = (self.cfg.floor_memory_tiles, self.geom.nf);
        let hist_cfg = self.cfg.histogram;
        let q = self.cfg.low_percentile;
        self.floors
            .entry(tile.key.f_block)
            .or_insert_with(|| FloorTrack::new(w, nf))
            .push(|f| hist_percentile(tile.hist_row(f), &hist_cfg, q));
    }

    fn fold_into_parent(&mut self, level: usize, child: &Tile) {
        let (pfb, ptb) = self
            .geom
            .parent(level, child.key.f_block, child.key.t_block);
        if self.sealed[level + 1].contains_key(&(ptb, pfb)) {
            return;
        }
        let hist_cfg = self.cfg.histogram;
        let pct = self.pct();
        let parent = open_tile(
            &mut self.open[level + 1],
            &mut self.pool[level + 1],
            &self.geom,
            level + 1,
            self.cfg.scheme,
            usize::from(hist_cfg.bins),
            pfb,
            ptb,
            &mut self.next_seal_ns,
        );
        parent.fold_child(
            child,
            self.geom.levels[level + 1].f_factor,
            &hist_cfg,
            pct,
            &mut self.group_hist,
        );
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
            self.stats.tiles_written += 1;
            let old = self.sealed[level].insert((tb, fb), bytes);
            self.level_bytes[level] = self.level_bytes[level] - old.unwrap_or(0) + bytes;
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

    /// The parent of sealed tile `(level, fb, tb)` is sealed on disk.
    fn covered(&self, level: usize, fb: i64, tb: i64) -> bool {
        let (pfb, ptb) = self.geom.parent(level, fb, tb);
        self.sealed[level + 1].contains_key(&(ptb, pfb))
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
        self.stats.tiles_evicted += 1;
        Ok(())
    }

    /// The longest region-override age protecting tile `(level, fb)`, ns.
    fn override_age_ns(&self, level: usize, fb: i64) -> Option<i64> {
        if self.cfg.retention_overrides.is_empty() {
            return None;
        }
        let w = self.geom.levels[level].f_block_hz(self.geom.nf);
        let (lo, hi) = (fb as f64 * w, (fb + 1) as f64 * w);
        self.cfg
            .retention_overrides
            .iter()
            .filter(|o| usize::from(o.level) == level && o.freq.lo_hz < hi && o.freq.hi_hz >= lo)
            .map(|o| dur_ns(o.max_age))
            .max()
    }

    /// A sealed tile one level finer lies inside tile `(level, fb, tb)`.
    fn has_children(&self, level: usize, fb: i64, tb: i64) -> bool {
        if level == 0 {
            return false;
        }
        let g = &self.geom.levels[level];
        let (t0, t1) = (tb * g.nt as i64, (tb + 1) * g.nt as i64);
        let factor = i64::from(g.f_factor);
        let (f0, f1) = (fb * factor, (fb + 1) * factor);
        self.sealed[level - 1]
            .range((t0, i64::MIN)..(t1, i64::MIN))
            .any(|(&(_, cfb), _)| (f0..f1).contains(&cfb))
    }

    /// Covered by a sealed parent (or top level) and no finer tile left inside it.
    fn evictable(&self, level: usize, fb: i64, tb: i64) -> bool {
        (level == self.geom.top() || self.covered(level, fb, tb))
            && !self.has_children(level, fb, tb)
    }

    /// The oldest tile of `level` the byte budget may evict.
    fn oldest_victim(&self, level: usize, allow_protected: bool) -> Option<(i64, i64)> {
        let top = self.geom.top();
        for &(tb, fb) in self.sealed[level].keys() {
            if level < top && !self.covered(level, fb, tb) {
                // Parents seal in time order: later tiles are no more covered.
                return None;
            }
            if (!allow_protected && self.override_age_ns(level, fb).is_some())
                || self.has_children(level, fb, tb)
            {
                continue;
            }
            return Some((tb, fb));
        }
        None
    }

    /// Retention (T-116), after every seal, in three passes over **sealed** tiles:
    ///
    /// 1. **Age.** A tile expires once its end is older than `watermark − age`, where `age` is the
    ///    longest [`super::RetentionOverride`] covering its block at its level, else the level's
    ///    `max_age` (no age: kept).
    /// 2. **Quota.** While a level exceeds its `byte_quota`, its oldest unprotected tiles go.
    /// 3. **Budget.** While all files exceed `byte_budget`: the oldest unprotected tile of the
    ///    finest level that has one, then the oldest unprotected top-level tile, and only then
    ///    protected tiles (finest first).
    ///
    /// In every pass a tile is evicted only if a coarser level covers it (a sealed parent, or it
    /// is top-level) and **children go first**: a tile with a sealed finer tile still inside it is
    /// kept, so history never has a finer tile without its coarser summary. The clock is the data
    /// watermark, so [`Pyramid::seal_through`] with a later time fast-forwards retention.
    fn enforce_budget(&mut self) -> Result<(), StoreError> {
        let top = self.geom.top();
        let w = self.watermark_ns;
        for level in 0..=top {
            let level_age = self.cfg.levels[level].max_age.map(dur_ns);
            let min_age = self
                .cfg
                .retention_overrides
                .iter()
                .filter(|o| usize::from(o.level) == level)
                .map(|o| dur_ns(o.max_age))
                .chain(level_age)
                .min();
            let Some(min_age) = min_age else {
                continue;
            };
            let limit = w.saturating_sub(min_age);
            let mut victims = Vec::new();
            for &(tb, fb) in self.sealed[level].keys() {
                let end = self.geom.block_end_ns(level, tb);
                if end > limit {
                    break;
                }
                let Some(age) = self.override_age_ns(level, fb).or(level_age) else {
                    continue;
                };
                if end <= w.saturating_sub(age) && self.evictable(level, fb, tb) {
                    victims.push((tb, fb));
                }
            }
            for (tb, fb) in victims {
                self.evict(level, tb, fb)?;
                self.stats.tiles_expired += 1;
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
            for (&(tb, fb), &bytes) in &self.sealed[level] {
                if excess == 0 || (level < top && !self.covered(level, fb, tb)) {
                    break;
                }
                if self.override_age_ns(level, fb).is_some() || self.has_children(level, fb, tb) {
                    continue;
                }
                victims.push((tb, fb));
                excess = excess.saturating_sub(bytes);
            }
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
            let victim = (0..top)
                .find_map(|l| self.oldest_victim(l, false).map(|v| (l, v)))
                .or_else(|| self.oldest_victim(top, false).map(|v| (top, v)))
                .or_else(|| (0..=top).find_map(|l| self.oldest_victim(l, true).map(|v| (l, v))));
            match victim {
                Some((level, (tb, fb))) => self.evict(level, tb, fb)?,
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
            let floor = self.floors.get(&k.0).map(|f| &f.floor[..]);
            tile.close_column(floor, margin, pct, &mut self.scratch);
            let written = self.write_tile(0, &tile, false);
            self.open[0].insert(k, tile);
            if let Err(e) = written {
                result = Err(e);
                break;
            }
        }
        self.keys = keys;
        self.last_checkpoint_ns = Some(self.latest_ns);
        result
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
                            if h.sealed {
                                self.sealed[level].insert((tb, fb), len);
                                self.level_bytes[level] += len;
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
        for level in 0..top {
            let newest_parent_end = self.sealed[level + 1]
                .last_key_value()
                .map_or(i64::MIN, |(&(tb, _), _)| {
                    self.geom.block_end_ns(level + 1, tb)
                });
            let children: Vec<(i64, i64)> = self.sealed[level]
                .keys()
                .rev()
                .take_while(|&&(tb, fb)| {
                    let (_, ptb) = self.geom.parent(level, fb, tb);
                    self.geom.block_end_ns(level + 1, ptb) > newest_parent_end
                })
                .copied()
                .collect();
            for (tb, fb) in children {
                match self.read_sealed(level, fb, tb)? {
                    Some(child) => self.fold_into_parent(level, &child),
                    None => {
                        let _ = fs::remove_file(self.path(level, fb, tb));
                        if let Some(b) = self.sealed[level].remove(&(tb, fb)) {
                            self.disk_bytes -= b;
                            self.level_bytes[level] -= b;
                        }
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
