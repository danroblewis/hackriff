//! Forward consecutive mean excision (FCME) on frequency blocks: the primary floor estimator.
//!
//! Per block of `L` bins, each `Gamma(n)` with noise mean `μ`:
//! 0. Bins that are not finite and positive (NaN of either sign, ±∞, exact zeros from a
//!    digital-zero gap) are excised. With fewer than `max(8, min_valid_fraction·L)` usable bins
//!    the block is **invalid** (`valid = false`, `floor = NaN`); the tracker fills it from the
//!    nearest valid block.
//! 1. `m = max(1, ⌊init_fraction·L'⌋)` of the `L'` usable bins; start from the mean of the `m`
//!    smallest.
//! 2. Repeat: `thr = T_CME·mean`; the clean set is the bins `< thr` (at least the initial `m`);
//!    `mean` = the clean set's mean; stop when the set size stops changing, or after
//!    `max_iterations` (default 20; `converged = false`). Each iteration is one `O(L)` pass.
//! 3. `floor = mean / c`, `c = P(n+1, n·T_CME) / P(n, n·T_CME)` (the clean set is a truncated
//!    sample, so its mean reads low by `c`).
//!
//! `T_CME = Q⁻¹(n, pfa)/n` from the **Gamma(n)** tail, not the exponential one (S4 §5): with
//! `n = 10`, `pfa = 1e-3` it is 2.265 (3.55 dB) and `c = 0.99858`. Unbiased to ≥ 80 % occupancy
//! on synthetic spectra (S4 §3.1), because the excision needs only the lowest ~10 % of a block to
//! be noise. Typically 3–6 iterations on real spectra.

use super::FloorConfigError;
use super::gamma;

/// FCME settings (the block geometry is [`BlockConfig`](super::BlockConfig)).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FcmeConfig {
    /// Design false-alarm probability of the excision threshold (default 1e-3).
    pub pfa: f64,
    /// Fraction of smallest bins used to start (default 0.1).
    pub init_fraction: f64,
    /// Iteration cap (default 20): bounds the worst-case per-block cost.
    pub max_iterations: usize,
    /// A block needs at least this fraction of finite, positive bins (default 0.5; never fewer
    /// than 8).
    pub min_valid_fraction: f64,
}

impl Default for FcmeConfig {
    fn default() -> Self {
        Self {
            pfa: 1e-3,
            init_fraction: 0.1,
            max_iterations: 20,
            min_valid_fraction: 0.5,
        }
    }
}

impl FcmeConfig {
    /// Checks the settings.
    pub fn validate(&self) -> Result<(), FloorConfigError> {
        super::check_probability("fcme.pfa", self.pfa)?;
        super::check_fraction("fcme.init_fraction", self.init_fraction)?;
        super::check_fraction("fcme.min_valid_fraction", self.min_valid_fraction)?;
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
    /// Bias-corrected floor, same unit as the input (linear); NaN when `!valid`.
    pub floor: f64,
    /// Bins in the final clean set.
    pub clean: usize,
    /// Finite, positive bins used.
    pub used: usize,
    /// Iterations run.
    pub iterations: usize,
    /// The clean set stopped changing before the cap.
    pub converged: bool,
    /// Enough usable bins.
    pub valid: bool,
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
        self.scratch.clear();
        self.scratch.extend_from_slice(values);
        // Fast path: one pass to check; compact in place only when something is unusable.
        if !values.iter().all(|x| x.is_finite() && *x > 0.0) {
            self.scratch.retain(|x| x.is_finite() && *x > 0.0);
        }
        let used = self.scratch.len();
        let need = ((self.config.min_valid_fraction * l as f64).ceil() as usize)
            .max(8)
            .min(l);
        if used < need {
            return FcmeBlock {
                floor: f64::NAN,
                clean: 0,
                used,
                iterations: 0,
                converged: false,
                valid: false,
            };
        }
        let m = ((self.config.init_fraction * used as f64) as usize).clamp(1, used);
        if m < used {
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
            for &x in &self.scratch {
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
            floor: mean / self.bias,
            clean: count,
            used,
            iterations,
            converged,
            valid: true,
        }
    }
}

/// One-shot per-bin block-FCME floor (allocates; for offline use and tests). Invalid blocks take
/// the nearest valid block's value; `None` when no block is valid.
pub fn fcme_floor(
    psd: &[f32],
    n_avg: f64,
    blocks: super::BlockConfig,
    config: FcmeConfig,
) -> Result<Option<Vec<f32>>, FloorConfigError> {
    let layout = super::BlockLayout::new(psd.len(), blocks)?;
    let mut fcme = BlockFcme::new(config, n_avg)?;
    let est: Vec<FcmeBlock> = (0..layout.count())
        .map(|b| fcme.estimate_block(&psd[layout.range(b)]))
        .collect();
    let mut block_floor: Vec<f32> = est.iter().map(|e| e.floor as f32).collect();
    let valid: Vec<bool> = est.iter().map(|e| e.valid).collect();
    if super::blocks::fill_invalid(&mut block_floor, &valid) == 0 {
        return Ok(None);
    }
    let mut out = vec![0.0; psd.len()];
    layout.interpolate(&block_floor, &mut out);
    Ok(Some(out))
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
        assert!(r.converged && r.valid && r.clean == 256 && r.iterations <= 2);
        assert!((r.floor * f.bias_factor() - 1.0).abs() < 1e-6);
        // Half the block is a strong carrier: excised.
        v[100..228].fill(100.0);
        let r = f.estimate_block(&v);
        assert_eq!(r.clean, 128);
        assert!((r.floor * f.bias_factor() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn non_finite_and_zero_bins_are_excised() {
        let mut f = BlockFcme::new(FcmeConfig::default(), 10.0).unwrap();
        let mut v = vec![2.0f32; 256];
        v[3] = -f32::NAN; // sorts first under total_cmp
        v[4] = f32::NAN;
        v[5] = f32::INFINITY;
        v[6] = f32::NEG_INFINITY;
        v[10..70].fill(0.0); // 23 % exact zeros
        let r = f.estimate_block(&v);
        assert!(r.valid && r.used == 256 - 64, "{r:?}");
        assert!((r.floor * f.bias_factor() - 2.0).abs() < 1e-6);
        v[..200].fill(0.0);
        let r = f.estimate_block(&v);
        assert!(!r.valid && r.floor.is_nan());
        // One-shot: invalid blocks take the nearest valid value.
        let mut psd = vec![1.0f32; 1024];
        psd[..300].fill(0.0);
        let floor = fcme_floor(
            &psd,
            10.0,
            super::super::BlockConfig::default(),
            FcmeConfig::default(),
        )
        .unwrap()
        .unwrap();
        assert!(
            floor
                .iter()
                .all(|&x| (x * f.bias_factor() as f32 - 1.0).abs() < 1e-4)
        );
        assert!(
            fcme_floor(
                &[0.0; 512],
                10.0,
                super::super::BlockConfig::default(),
                FcmeConfig::default()
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn iteration_cap_bounds_adversarial_blocks() {
        // Geometric ramp: each pass admits a few more bins.
        let mut f = BlockFcme::new(FcmeConfig::default(), 1.0).unwrap();
        let v: Vec<f32> = (0..256).map(|i| 1.05f32.powi(i)).collect();
        let r = f.estimate_block(&v);
        assert!(r.iterations <= 20);
        if !r.converged {
            assert_eq!(r.iterations, 20);
        }
    }
}
