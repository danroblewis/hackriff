//! The averaging count `n` the Gamma model uses for a spectrum's bins.

use crate::spectrum::Resolution;
use crate::window::Window;

/// How to pick the Gamma shape `n` for a spectrum.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum AveragingModel {
    /// [`effective_averages`]: `K` reduced for overlapped segments (default).
    #[default]
    Effective,
    /// The nominal `K` (`Resolution::n_avg`), exact only without overlap.
    Nominal,
    /// A fixed value (e.g. measured `mean²/var` on terminated input).
    Fixed(f64),
}

impl AveragingModel {
    /// The shape `n` for `resolution`.
    pub fn n_avg(&self, resolution: &Resolution) -> f64 {
        match *self {
            AveragingModel::Effective => effective_averages(resolution),
            AveragingModel::Nominal => f64::from(resolution.n_avg.max(1)),
            AveragingModel::Fixed(n) => n,
        }
    }
}

/// Equivalent number of independent averages for `K` overlapped windowed segments (Welch 1967):
///
/// `n_eff = K / (1 + 2·Σ_{j=1}^{K−1} (1 − j/K)·ρ(j·hop))`, `ρ(s) = (Σ w[i]·w[i+s])² / (Σ w²)²`.
///
/// `ρ` is the correlation between the periodograms of segments `s` samples apart for white
/// complex Gaussian noise (the sum runs over the `N − s` overlapping samples). Hann at 50 %
/// overlap: `ρ = (1/6)² = 1/36`, so `K = 10` gives `n_eff = 9.52` and `K = 16` gives `15.2`.
/// Without overlap `n_eff = K`. The averaged bin is then modelled as
/// `Gamma(n_eff)` (moment-matched: exact mean and variance). Allocates a window: call it when
/// the resolution changes, not per frame.
pub fn effective_averages(resolution: &Resolution) -> f64 {
    let k = resolution.n_avg.max(1) as usize;
    let n = resolution.fft_len;
    let hop = resolution.hop();
    if k == 1 || resolution.overlap == 0 || n < 2 {
        return k as f64;
    }
    let window = Window::new(resolution.window, n);
    let w = window.coefficients();
    let sum_sq = window.sum_sq();
    let mut denom = 1.0;
    for j in 1..k {
        let shift = j * hop;
        if shift >= n {
            break;
        }
        let cross: f64 = w[..n - shift]
            .iter()
            .zip(&w[shift..])
            .map(|(&a, &b)| f64::from(a) * f64::from(b))
            .sum();
        let rho = (cross / sum_sq).powi(2);
        denom += 2.0 * (1.0 - j as f64 / k as f64) * rho;
    }
    k as f64 / denom
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::welch::{SegmentEngine, WelchConfig};

    fn resolution(fft_len: usize, overlap: usize, k: u32) -> Resolution {
        let mut cfg = WelchConfig::new(fft_len);
        cfg.overlap = overlap;
        let mut r = SegmentEngine::new(cfg).unwrap().resolution(1e6);
        r.n_avg = k;
        r
    }

    #[test]
    fn hann_half_overlap() {
        // ρ(N/2) = (Σ_{overlap} w·w / Σw²)² = ((N/2 · 1/8)/(N · 3/8))² = 1/36.
        let r = resolution(1024, 512, 10);
        let want = 10.0 / (1.0 + 2.0 * 0.9 / 36.0);
        assert!(
            (effective_averages(&r) - want).abs() < 1e-3,
            "{}",
            effective_averages(&r)
        );
        assert_eq!(effective_averages(&resolution(1024, 0, 10)), 10.0);
        assert_eq!(AveragingModel::Nominal.n_avg(&r), 10.0);
        assert_eq!(AveragingModel::Fixed(7.5).n_avg(&r), 7.5);
    }
}
