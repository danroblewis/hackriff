//! Block percentile floor: the cross-check / fallback estimator.
//!
//! `floor = q_p / (P⁻¹(n, p)/n)`: the `p`-quantile of a block (linear interpolation between order
//! statistics) divided by the `p`-quantile of `Gamma(n)` in units of its mean. Default `p = 0.2`
//! (Gamma(10): factor 0.729, −1.37 dB; the exponential median needs ÷ ln 2 instead).
//!
//! Percentiles read signal as floor once enough of the block is occupied: S4 §3.1 measured p20
//! at +0.7 dB (41 % occupancy), +1.5 dB (61 %) and +6 dB (80 %), and p50 fails from 40 %. Each
//! block therefore reports its **occupancy** — the fraction of bins above `T_occ·floor`
//! (`T_occ` from `occupancy_pfa`), less the expected noise exceedance — and is **invalid when
//! occupancy ≥ `max_occupancy`** (default 0.4). Occupancy measured against the percentile's own
//! floor underestimates once the floor is biased, so the tracker also checks it against the FCME
//! occupancy. Never use p50 in dense bands.

use super::gamma;
use super::{FloorConfigError, occupancy_from_count};

/// Percentile settings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PercentileConfig {
    /// Quantile `p` (default 0.2).
    pub quantile: f64,
    /// Occupancy at or above which the estimate is invalid (default 0.4).
    pub max_occupancy: f64,
    /// False-alarm probability of the occupancy threshold (default 1e-2).
    pub occupancy_pfa: f64,
}

impl Default for PercentileConfig {
    fn default() -> Self {
        Self {
            quantile: 0.2,
            max_occupancy: 0.4,
            occupancy_pfa: 1e-2,
        }
    }
}

impl PercentileConfig {
    /// Checks the settings.
    pub fn validate(&self) -> Result<(), FloorConfigError> {
        super::check_fraction("percentile.quantile", self.quantile)?;
        super::check_fraction("percentile.max_occupancy", self.max_occupancy)?;
        super::check_probability("percentile.occupancy_pfa", self.occupancy_pfa)
    }
}

/// One block's percentile result.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PercentileBlock {
    /// Bias-corrected floor (linear).
    pub floor: f64,
    /// Occupied fraction relative to this floor.
    pub occupancy: f64,
    /// `occupancy < max_occupancy`.
    pub valid: bool,
}

/// The percentile kernel for a given averaging count.
#[derive(Clone, Debug)]
pub struct BlockPercentile {
    config: PercentileConfig,
    n_avg: f64,
    factor: f64,
    occupancy_threshold: f64,
    scratch: Vec<f32>,
}

impl BlockPercentile {
    /// A kernel for bins that are means of `n_avg` periodograms.
    pub fn new(config: PercentileConfig, n_avg: f64) -> Result<Self, FloorConfigError> {
        config.validate()?;
        let mut s = Self {
            config,
            n_avg: 0.0,
            factor: 1.0,
            occupancy_threshold: 1.0,
            scratch: Vec::new(),
        };
        s.set_n_avg(n_avg);
        Ok(s)
    }

    /// Settings.
    pub fn config(&self) -> &PercentileConfig {
        &self.config
    }

    /// Changes the averaging count (no allocation).
    pub fn set_n_avg(&mut self, n_avg: f64) {
        assert!(n_avg > 0.0, "n_avg must be positive");
        if n_avg != self.n_avg {
            self.n_avg = n_avg;
            self.factor = gamma::mean_quantile(n_avg, self.config.quantile);
            self.occupancy_threshold = gamma::mean_threshold(n_avg, self.config.occupancy_pfa);
        }
    }

    /// The Gamma quantile factor the raw percentile is divided by.
    pub fn bias_factor(&self) -> f64 {
        self.factor
    }

    /// Estimates one block of linear bins.
    pub fn estimate_block(&mut self, values: &[f32]) -> PercentileBlock {
        assert!(!values.is_empty(), "empty block");
        self.scratch.clear();
        self.scratch.extend_from_slice(values);
        if !values.iter().all(|x| x.is_finite() && *x > 0.0) {
            self.scratch.retain(|x| x.is_finite() && *x > 0.0);
        }
        let l = self.scratch.len();
        if l == 0 {
            return PercentileBlock {
                floor: f64::NAN,
                occupancy: 1.0,
                valid: false,
            };
        }
        let pos = self.config.quantile * (l - 1) as f64;
        let lo = pos.floor() as usize;
        let frac = pos - lo as f64;
        let (_, &mut a, upper) = self.scratch.select_nth_unstable_by(lo, f32::total_cmp);
        let q = if frac > 0.0 && !upper.is_empty() {
            let b = upper.iter().copied().fold(f32::INFINITY, f32::min);
            f64::from(a) + frac * (f64::from(b) - f64::from(a))
        } else {
            f64::from(a)
        };
        let floor = (q / self.factor).max(1e-37);
        let thr = (floor * self.occupancy_threshold) as f32;
        let above = self.scratch.iter().filter(|&&x| x > thr).count();
        let occupancy = occupancy_from_count(above, l, self.config.occupancy_pfa);
        PercentileBlock {
            floor,
            occupancy,
            valid: occupancy < self.config.max_occupancy,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn factor_and_flat_block() {
        let mut p = BlockPercentile::new(PercentileConfig::default(), 10.0).unwrap();
        assert!((gamma::regularized_lower(10.0, 10.0 * p.bias_factor()) - 0.2).abs() < 1e-12);
        assert!(p.bias_factor() < 1.0, "{}", p.bias_factor());
        let r = p.estimate_block(&[2.0; 256]);
        assert!((r.floor * p.bias_factor() - 2.0).abs() < 1e-6);
        assert!(r.valid && r.occupancy == 0.0);
        let mut v = vec![1.0f32; 256];
        v[..128].fill(1000.0);
        assert!(!p.estimate_block(&v).valid);
    }
}
