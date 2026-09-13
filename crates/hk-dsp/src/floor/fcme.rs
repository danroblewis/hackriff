//! Forward consecutive mean excision (FCME) on frequency blocks: the primary floor estimator.
//!
//! Per block of `L` bins, each `Gamma(n)` with noise mean `μ`:
//! 1. `m = max(1, ⌊init_fraction·L⌋)`; start from the mean of the `m` smallest bins.
//! 2. Repeat: `thr = T_CME·mean`; the clean set is the bins `< thr` (at least the initial `m`);
//!    `mean` = the clean set's mean; stop when the set size stops changing.
//! 3. `floor = mean / c`, `c = P(n+1, n·T_CME) / P(n, n·T_CME)` (the clean set is a truncated
//!    sample, so its mean reads low by `c`).
//!
//! `T_CME = Q⁻¹(n, pfa)/n` from the **Gamma(n)** tail, not the exponential one (S4 §5): with
//! `n = 10`, `pfa = 1e-3` it is 2.265 (3.55 dB) and `c = 0.9986`. Unbiased to ≥ 80 % occupancy
//! on synthetic spectra (S4 §3.1), because the excision needs only the lowest ~10 % of a block to
//! be noise. Cost per block is `O(L)` per iteration (no sort: a selection for the start, then
//! threshold passes), typically 3–6 iterations.

use super::FloorConfigError;
use super::gamma;

/// FCME settings (the block geometry is [`BlockConfig`](super::BlockConfig)).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FcmeConfig {
    /// Design false-alarm probability of the excision threshold (default 1e-3).
    pub pfa: f64,
    /// Fraction of smallest bins used to start (default 0.1).
    pub init_fraction: f64,
    /// Iteration cap (default 50).
    pub max_iterations: usize,
}

impl Default for FcmeConfig {
    fn default() -> Self {
        Self {
            pfa: 1e-3,
            init_fraction: 0.1,
            max_iterations: 50,
        }
    }
}

impl FcmeConfig {
    /// Checks the settings.
    pub fn validate(&self) -> Result<(), FloorConfigError> {
        super::check_probability("fcme.pfa", self.pfa)?;
        super::check_fraction("fcme.init_fraction", self.init_fraction)?;
        if self.max_iterations == 0 {
            return Err(FloorConfigError::NonPositive {
                name: "fcme.max_iterations",
                value: 0.0,
            });
        }
        Ok(())
    }
}

/// One block's FCME result.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FcmeBlock {
    /// Bias-corrected floor, same unit as the input (linear).
    pub floor: f64,
    /// Bins in the final clean set.
    pub clean: usize,
    /// Iterations run.
    pub iterations: usize,
    /// The clean set stopped changing before the cap.
    pub converged: bool,
}

/// The FCME kernel for a given averaging count. Reuses one scratch buffer.
#[derive(Clone, Debug)]
pub struct BlockFcme {
    config: FcmeConfig,
    n_avg: f64,
    threshold: f64,
    bias: f64,
    scratch: Vec<f32>,
}

impl BlockFcme {
    /// A kernel for bins that are means of `n_avg` periodograms.
    pub fn new(config: FcmeConfig, n_avg: f64) -> Result<Self, FloorConfigError> {
        config.validate()?;
        let mut s = Self {
            config,
            n_avg: 0.0,
            threshold: 1.0,
            bias: 1.0,
            scratch: Vec::new(),
        };
        s.set_n_avg(n_avg);
        Ok(s)
    }

    /// Settings.
    pub fn config(&self) -> &FcmeConfig {
        &self.config
    }

    /// Changes the averaging count (recomputes `T_CME` and the bias factor; no allocation).
    pub fn set_n_avg(&mut self, n_avg: f64) {
        assert!(n_avg > 0.0, "n_avg must be positive");
        if n_avg != self.n_avg {
            self.n_avg = n_avg;
            self.threshold = gamma::mean_threshold(n_avg, self.config.pfa);
            self.bias = gamma::truncated_mean_ratio(n_avg, self.threshold);
        }
    }

    /// The averaging count in use.
    pub fn n_avg(&self) -> f64 {
        self.n_avg
    }

    /// `T_CME` (linear multiplier on the clean mean).
    pub fn threshold(&self) -> f64 {
        self.threshold
    }

    /// The truncated-mean bias factor `c` (the raw clean mean is divided by it).
    pub fn bias_factor(&self) -> f64 {
        self.bias
    }

    /// Estimates the floor of one block of linear bins.
    pub fn estimate_block(&mut self, values: &[f32]) -> FcmeBlock {
        let l = values.len();
        assert!(l > 0, "empty block");
        let m = ((self.config.init_fraction * l as f64) as usize).clamp(1, l);
        self.scratch.clear();
        self.scratch.extend_from_slice(values);
        if m < l {
            self.scratch.select_nth_unstable_by(m - 1, f32::total_cmp);
        }
        let init_sum: f64 = self.scratch[..m].iter().map(|&x| f64::from(x)).sum();
        let mut count = m;
        let mut mean = init_sum / m as f64;
        let mut iterations = 0;
        let mut converged = false;
        while iterations < self.config.max_iterations {
            iterations += 1;
            let thr = (self.threshold * mean) as f32;
            let (mut c, mut s) = (0usize, 0.0f64);
            for &x in values {
                if x < thr {
                    c += 1;
                    s += f64::from(x);
                }
            }
            if c < m {
                (c, s) = (m, init_sum);
            }
            mean = s / c as f64;
            if c == count {
                converged = true;
                break;
            }
            count = c;
        }
        FcmeBlock {
            floor: (mean / self.bias).max(1e-37),
            clean: count,
            iterations,
            converged,
        }
    }
}

/// One-shot per-bin block-FCME floor (allocates; for offline use and tests).
pub fn fcme_floor(
    psd: &[f32],
    n_avg: f64,
    blocks: super::BlockConfig,
    config: FcmeConfig,
) -> Result<Vec<f32>, FloorConfigError> {
    let layout = super::BlockLayout::new(psd.len(), blocks)?;
    let mut fcme = BlockFcme::new(config, n_avg)?;
    let block_floor: Vec<f32> = (0..layout.count())
        .map(|b| fcme.estimate_block(&psd[layout.range(b)]).floor as f32)
        .collect();
    let mut out = vec![0.0; psd.len()];
    layout.interpolate(&block_floor, &mut out);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_for_gamma_10() {
        let f = BlockFcme::new(FcmeConfig::default(), 10.0).unwrap();
        assert!((10.0 * f.threshold().log10() - 3.55).abs() < 0.02);
        // c = P(11, 10T)/P(10, 10T) = (1 − Q(11, 10T))/(1 − 1e-3), Q(11, x) = Q(10, x) + x¹⁰e⁻ˣ/10!
        let x = 10.0 * f.threshold();
        let q11 = 1e-3 + (10.0 * x.ln() - x - gamma::ln_gamma(11.0)).exp();
        assert!(
            (f.bias_factor() - (1.0 - q11) / (1.0 - 1e-3)).abs() < 1e-9,
            "{}",
            f.bias_factor()
        );
        let e = BlockFcme::new(FcmeConfig::default(), 1.0).unwrap();
        // Exponential: T = ln(1000) = 6.91, c = 1 − T·e^−T/(1 − e^−T).
        assert!((e.threshold() - 1000f64.ln()).abs() < 1e-9);
        let t = e.threshold();
        let c = 1.0 - t * (-t).exp() / (1.0 - (-t).exp());
        assert!((e.bias_factor() - c).abs() < 1e-9);
    }

    #[test]
    fn flat_block_and_excision() {
        let mut f = BlockFcme::new(FcmeConfig::default(), 10.0).unwrap();
        let mut v = vec![1.0f32; 256];
        let r = f.estimate_block(&v);
        assert!(r.converged && r.clean == 256);
        assert!((r.floor * f.bias_factor() - 1.0).abs() < 1e-6);
        // Half the block is a strong carrier: excised.
        v[100..228].fill(100.0);
        let r = f.estimate_block(&v);
        assert_eq!(r.clean, 128);
        assert!((r.floor * f.bias_factor() - 1.0).abs() < 1e-6);
    }
}
