//! Overlapping frequency blocks shared by the block estimators, and interpolation of per-block
//! values back to a per-bin trace in dB.

use std::ops::Range;

use super::FloorConfigError;

/// Block size and hop, in bins.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockConfig {
    /// Bins per block (default 256: ~1.25 MHz at 4.88 kHz bins). Clamped to the spectrum width.
    pub block_bins: usize,
    /// Hop between block starts (default 64).
    pub hop_bins: usize,
}

impl Default for BlockConfig {
    fn default() -> Self {
        Self {
            block_bins: 256,
            hop_bins: 64,
        }
    }
}

impl BlockConfig {
    /// Checks the settings.
    pub fn validate(&self) -> Result<(), FloorConfigError> {
        if self.block_bins < 8 {
            return Err(FloorConfigError::BlockTooSmall(self.block_bins));
        }
        if self.hop_bins == 0 || self.hop_bins > self.block_bins {
            return Err(FloorConfigError::BadHop {
                hop: self.hop_bins,
                block: self.block_bins,
            });
        }
        Ok(())
    }
}

/// Block geometry over a spectrum of `bins` bins: blocks start at `0, hop, 2·hop, …` while they
/// fit; the block centre is `start + (block − 1)/2`. Bins after the last full block are covered
/// by holding the last centre's value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockLayout {
    bins: usize,
    block: usize,
    hop: usize,
    count: usize,
}

const TINY: f64 = 1e-37;

impl BlockLayout {
    /// The layout for `bins` bins.
    pub fn new(bins: usize, config: BlockConfig) -> Result<Self, FloorConfigError> {
        config.validate()?;
        if bins == 0 {
            return Err(FloorConfigError::NoBins);
        }
        let block = config.block_bins.min(bins);
        Ok(Self {
            bins,
            block,
            hop: config.hop_bins,
            count: (bins - block) / config.hop_bins + 1,
        })
    }

    /// Spectrum width, bins.
    pub fn bins(&self) -> usize {
        self.bins
    }

    /// Bins per block (after clamping).
    pub fn block_bins(&self) -> usize {
        self.block
    }

    /// Hop, bins.
    pub fn hop_bins(&self) -> usize {
        self.hop
    }

    /// Number of blocks.
    pub fn count(&self) -> usize {
        self.count
    }

    /// Bin range of block `b`.
    pub fn range(&self, b: usize) -> Range<usize> {
        let s = b * self.hop;
        s..s + self.block
    }

    /// Centre of block `b`, in (fractional) bins.
    pub fn centre(&self, b: usize) -> f64 {
        (b * self.hop) as f64 + (self.block as f64 - 1.0) / 2.0
    }

    /// The bins [`interpolate`](Self::interpolate) holds constant: below the first block centre
    /// and above the last (≈ 128 bins at each end with 256-bin blocks). There the floor is the
    /// edge block's value, which **overestimates** the floor across the baseband filter
    /// roll-off (HackRF: outside ±7.5 MHz at 20 Msps) and underestimates a rising edge; treat
    /// these bins as `edge`.
    pub fn held_bins(&self) -> (Range<usize>, Range<usize>) {
        let first_end = (self.centre(0).floor() as usize + 1).min(self.bins);
        let tail = (self.centre(self.count - 1).floor() as usize + 1).min(self.bins);
        (0..first_end, tail..self.bins)
    }

    /// Interpolates positive per-block values (linear power) to every bin, linearly in dB between
    /// block centres and held constant beyond the first and last centre
    /// ([`held_bins`](Self::held_bins)). Allocation-free.
    pub fn interpolate(&self, blocks: &[f32], out: &mut [f32]) {
        assert_eq!(blocks.len(), self.count, "one value per block");
        assert_eq!(out.len(), self.bins, "one output per bin");
        let v = |b: usize| f64::from(blocks[b]).max(TINY);
        let last = self.count - 1;
        let first_end = (self.centre(0).floor() as usize + 1).min(self.bins);
        out[..first_end].fill(v(0) as f32);
        for b in 0..last {
            let (ca, cb) = (self.centre(b), self.centre(b + 1));
            let lo = ca.floor() as usize + 1;
            let hi = (cb.floor() as usize + 1).min(self.bins);
            if lo >= hi {
                continue;
            }
            // Geometric steps: value(i) = va · r^(i − ca), r = (vb/va)^(1/(cb − ca)).
            let (va, vb) = (v(b), v(b + 1));
            let r = (vb / va).powf(1.0 / (cb - ca));
            let mut x = va * r.powf(lo as f64 - ca);
            for o in &mut out[lo..hi] {
                *o = x as f32;
                x *= r;
            }
        }
        let tail = (self.centre(last).floor() as usize + 1).min(self.bins);
        out[tail..].fill(v(last) as f32);
    }
}

/// Replaces each block with `valid[b] == false` by the value of the nearest valid block (ties go
/// to the lower index). Returns the number of valid blocks; with none, `values` is unchanged.
/// Allocation-free.
pub fn fill_invalid(values: &mut [f32], valid: &[bool]) -> usize {
    assert_eq!(values.len(), valid.len(), "one validity flag per block");
    let n = valid.iter().filter(|&&v| v).count();
    if n == 0 || n == values.len() {
        return n;
    }
    for b in 0..values.len() {
        if valid[b] {
            continue;
        }
        let mut d = 1;
        loop {
            if b >= d && valid[b - d] {
                values[b] = values[b - d];
                break;
            }
            if b + d < values.len() && valid[b + d] {
                values[b] = values[b + d];
                break;
            }
            d += 1;
        }
    }
    n
}

/// Sliding minimum over `±half_width` entries (allocation-free, `O(len·width)`).
pub fn sliding_min(values: &[f32], half_width: usize, out: &mut [f32]) {
    assert_eq!(values.len(), out.len(), "input/output length mismatch");
    let n = values.len();
    for (b, o) in out.iter_mut().enumerate() {
        let lo = b.saturating_sub(half_width);
        let hi = (b + half_width + 1).min(n);
        *o = values[lo..hi].iter().copied().fold(f32::INFINITY, f32::min);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_and_sliding_min() {
        let mut v = [9.0, 1.0, 9.0, 9.0, 2.0, 9.0];
        let valid = [false, true, false, false, true, false];
        assert_eq!(fill_invalid(&mut v, &valid), 2);
        assert_eq!(v, [1.0, 1.0, 1.0, 2.0, 2.0, 2.0]);
        let mut out = [0.0; 6];
        sliding_min(&[5.0, 4.0, 3.0, 6.0, 7.0, 8.0], 1, &mut out);
        assert_eq!(out, [4.0, 3.0, 3.0, 3.0, 6.0, 7.0]);
        let l = BlockLayout::new(4096, BlockConfig::default()).unwrap();
        assert_eq!(l.held_bins(), (0..128, 3968..4096));
    }

    #[test]
    fn layout_counts_and_centres() {
        let l = BlockLayout::new(4096, BlockConfig::default()).unwrap();
        assert_eq!(l.count(), 61);
        assert_eq!(l.range(60), 3840..4096);
        assert_eq!(l.centre(0), 127.5);
        let small = BlockLayout::new(100, BlockConfig::default()).unwrap();
        assert_eq!((small.count(), small.block_bins()), (1, 100));
        let odd = BlockLayout::new(1000, BlockConfig::default()).unwrap();
        assert_eq!(odd.count(), 12);
        assert!(
            BlockLayout::new(
                10,
                BlockConfig {
                    block_bins: 16,
                    hop_bins: 0
                }
            )
            .is_err()
        );
    }

    #[test]
    fn interpolation_is_linear_in_db_and_exact_at_centres() {
        let l = BlockLayout::new(1024, BlockConfig::default()).unwrap();
        let blocks: Vec<f32> = (0..l.count()).map(|b| 10f32.powi(b as i32 % 3)).collect();
        let mut out = vec![0.0; 1024];
        l.interpolate(&blocks, &mut out);
        let db = |x: f64| 10.0 * x.log10();
        for b in 0..l.count() - 1 {
            let (ca, cb) = (l.centre(b), l.centre(b + 1));
            let (a, next) = (db(f64::from(blocks[b])), db(f64::from(blocks[b + 1])));
            let (lo, hi) = (ca.floor() as usize + 1, cb.floor() as usize + 1);
            for (i, &v) in out.iter().enumerate().take(hi).skip(lo) {
                let want = a + (i as f64 - ca) / (cb - ca) * (next - a);
                assert!((db(f64::from(v)) - want).abs() < 1e-3, "block {b} bin {i}");
            }
        }
        assert_eq!(out[0], blocks[0]);
        assert_eq!(out[1023], *blocks.last().unwrap());
        // Constant input stays constant.
        l.interpolate(&vec![3.0; l.count()], &mut out);
        assert!(out.iter().all(|&x| (x - 3.0).abs() < 1e-5));
    }
}
