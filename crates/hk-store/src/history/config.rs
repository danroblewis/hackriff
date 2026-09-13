//! Pyramid configuration and the derived per-level geometry.

use std::time::Duration;

use hk_model::PowerUnit;

use super::StoreError;

/// Most levels a scheme may have.
pub const MAX_LEVELS: usize = 8;

/// One level of the pyramid ladder.
#[derive(Clone, Debug, PartialEq)]
pub struct LevelConfig {
    /// Frequency-cell width relative to the level below (must be 1 for level 0).
    pub f_factor: u32,
    /// Time cells per tile. **The next level's time cell is one whole tile of this level**, so a
    /// tile rolls up into exactly one time column of its parent.
    pub t_cells_per_block: u32,
    /// Optional retention age: sealed tiles whose end is older than `watermark − max_age` are
    /// evicted (still only once a coarser level covers them).
    pub max_age: Option<Duration>,
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
    /// Histogram −200…+20 dB in 0.5 dB bins; p10/p90; 6 dB occupancy margin; 8 GiB budget.
    fn default() -> Self {
        let level = |f_factor, t_cells_per_block| LevelConfig {
            f_factor,
            t_cells_per_block,
            max_age: None,
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
    /// Frequency factor relative to the level below.
    pub f_factor: u32,
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
    /// Per level, finest first.
    pub levels: Vec<LevelGeometry>,
}

impl Geometry {
    /// Number of levels.
    pub fn n_levels(&self) -> usize {
        self.levels.len()
    }

    /// Top (coarsest) level index.
    pub fn top(&self) -> usize {
        self.levels.len() - 1
    }

    /// The parent `(f_block, t_block)` at `level + 1` of tile `(f_block, t_block)` at `level`.
    pub fn parent(&self, level: usize, f_block: i64, t_block: i64) -> (i64, i64) {
        let up = &self.levels[level + 1];
        let parent_cell = (f_block * self.nf as i64).div_euclid(i64::from(up.f_factor));
        (
            parent_cell.div_euclid(self.nf as i64),
            t_block.div_euclid(up.nt as i64),
        )
    }

    /// End (exclusive) of time block `t_block` at `level`, ns.
    pub fn block_end_ns(&self, level: usize, t_block: i64) -> i64 {
        (t_block + 1).saturating_mul(self.levels[level].t_block_ns())
    }
}

impl PyramidConfig {
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
        let nf = self.f_cells_per_block as usize;
        let mut levels = Vec::with_capacity(self.levels.len());
        let (mut f_cell, mut t_cell) = (self.f_cell_hz, t0);
        for (i, l) in self.levels.iter().enumerate() {
            if l.t_cells_per_block == 0 {
                return bad(format!("level {i}: t_cells_per_block must be >= 1"));
            }
            if i == 0 && l.f_factor != 1 {
                return bad("level 0 f_factor must be 1".into());
            }
            if i > 0 {
                if l.f_factor == 0 || self.f_cells_per_block % l.f_factor != 0 {
                    return bad(format!(
                        "level {i}: f_factor must divide f_cells_per_block (so a parent cell's \
                         children lie in one child tile)"
                    ));
                }
                let prev: &LevelGeometry = &levels[i - 1];
                t_cell = prev.t_block_ns();
                f_cell *= f64::from(l.f_factor);
            }
            let nt = l.t_cells_per_block as usize;
            if t_cell.checked_mul(nt as i64).is_none() {
                return bad(format!("level {i}: tile duration overflows"));
            }
            levels.push(LevelGeometry {
                f_cell_hz: f_cell,
                t_cell_ns: t_cell,
                nt,
                f_factor: l.f_factor,
            });
        }
        Ok(Geometry { nf, levels })
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
        assert_eq!(g.parent(0, 3, 10), (1, 0));
        assert_eq!(g.parent(0, 2, 29), (1, 1));
        assert_eq!(g.parent(0, -1, -1), (-1, -1));
    }

    #[test]
    fn rejects_bad_factor() {
        let mut c = PyramidConfig::default();
        c.levels[1].f_factor = 3;
        assert!(c.geometry().is_err());
    }
}
