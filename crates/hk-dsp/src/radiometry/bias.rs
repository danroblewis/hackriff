//! The percentile bias of spectrum-history cells on noise, derived from the Gamma model (no
//! fitted constant).
//!
//! # Model
//!
//! A PSD bin averaged over `K` windowed segments of white complex Gaussian noise has mean `μ`.
//! For circular Gaussian noise the covariance of two periodogram powers is the squared
//! magnitude of the covariance of the FFT coefficients (Isserlis), so bins `l` apart in
//! segments `s` samples apart have power correlation
//!
//! `ρ(s, l) = |Σ_n w[n]·w[n+s]·e^{−2πi·l·n/N}|² / (Σ w²)²` ([`bin_power_correlation`]).
//!
//! Averaging `K` segments hop `h` apart gives the bin covariance (units of `μ²`)
//!
//! `R(l) = (1/K)·[ρ(0, l) + 2·Σ_{j=1}^{K−1} (1 − j/K)·ρ(j·h, l)]` ([`averaged_bin_covariance`]),
//!
//! so `R(0) = 1/n_eff` exactly as [`effective_averages`](crate::floor::effective_averages). A
//! spectrum-history level-0 cell of width `w` takes the overlap-weighted mean of the `ω_i`-Hz
//! overlaps of the bins it covers (hk-store's regrid), so its value `v` has
//!
//! `Var(v/μ) = Σ_{i,i'} ω_i·ω_{i'}·R(|i−i'|) / (Σ ω)²`,
//!
//! averaged over [`ALIGNMENT_OFFSETS`] positions of the cell on the bin grid. Moment matching
//! gives `v/μ ~ Gamma(n_c, 1/n_c)` with `n_c = 1/Var(v/μ)` ([`cell_value_shape`]).
//!
//! # Correction
//!
//! A `p`-quantile of such values reads `q_p = P⁻¹(n_c, p)/n_c` times the true floor, so
//!
//! `floor_dB = percentile_dB − 10·log10(P⁻¹(n_c, p)/n_c)` ([`percentile_bias_db`] is the
//! subtracted term, negative below the median).
//!
//! **Which `p`.** Level-0 cells store the numpy-interpolated order statistic at 0-based position
//! `h = q·(n−1)` of their `n` frame values. The CDF value of the `k`-th of `n` order statistics
//! has expectation `k/(n+1)`, linear between ranks, so `p = (h + 1)/(n + 1)`
//! ([`exact_percentile_probability`]; 61 frames and `q` = 10 % give `p` = 0.1129). Rolled-up
//! cells take the pooled histogram rank `q·N` with uniform in-bin interpolation, so `p = q`; their
//! residual in-bin error is bounded by one histogram step.
//!
//! **Worked number** (T-017's SPACE-050 geometry: 1 Msps, 1024 bins, Hann, 50 % overlap,
//! `K` = 32, 6.25 kHz × 1 s cells, 61 frames of 16 384 samples per cell): `n_eff` = 30.4,
//! `n_c` = 109.4, `p` = 0.1129, bias = −0.527 dB (−0.557 dB at `p` = 0.1). The noise replay in
//! `hk-store/tests/space_050_radiometry.rs` reads a raw p10 of −0.530 dB, corrected to −0.003 dB.
//! The approximations are the Gamma moment match (the sum of correlated exponentials is slightly
//! less skewed than Gamma) and the rank expectation; both are small at `n_c` ≳ 30.

use std::f64::consts::PI;

use crate::floor::gamma;
use crate::spectrum::Resolution;
use crate::window::Window;

/// Cell alignments averaged by [`cell_value_shape`].
pub const ALIGNMENT_OFFSETS: usize = 64;

/// Longest bin lag [`averaged_bin_covariance`] evaluates.
pub const MAX_LAG: usize = 256;

/// Lags whose covariance is below this fraction of `R(0)` (twice in a row) end the evaluation.
const NEGLIGIBLE: f64 = 1e-9;

fn rho(w: &[f32], sum_sq: f64, shift: usize, lag: usize) -> f64 {
    let n = w.len();
    if shift >= n || sum_sq <= 0.0 {
        return 0.0;
    }
    let step = -2.0 * PI * lag as f64 / n as f64;
    let (mut re, mut im) = (0.0f64, 0.0f64);
    for (i, (&a, &b)) in w[..n - shift].iter().zip(&w[shift..]).enumerate() {
        let x = f64::from(a) * f64::from(b);
        let ph = step * i as f64;
        re += x * ph.cos();
        im += x * ph.sin();
    }
    (re * re + im * im) / (sum_sq * sum_sq)
}

/// `ρ(shift, lag)`: power correlation between PSD bins `lag` apart in windowed segments `shift`
/// samples apart, for white complex Gaussian noise.
pub fn bin_power_correlation(window: &Window, shift: usize, lag: usize) -> f64 {
    rho(window.coefficients(), window.sum_sq(), shift, lag)
}

/// `R(l)` for `l = 0..` until two consecutive lags are negligible or `max_lag` is reached (see
/// the [module docs](self)). Allocates a window: call when the resolution changes.
pub fn averaged_bin_covariance(resolution: &Resolution, max_lag: usize) -> Vec<f64> {
    let n = resolution.fft_len.max(1);
    let k = resolution.n_avg.max(1) as usize;
    let hop = resolution.hop().max(1);
    let window = Window::new(resolution.window, n);
    let (w, sum_sq) = (window.coefficients(), window.sum_sq());
    let mut out: Vec<f64> = Vec::new();
    let mut negligible = 0;
    for l in 0..=max_lag {
        let mut r = rho(w, sum_sq, 0, l);
        for j in 1..k {
            let shift = j * hop;
            if shift >= n {
                break;
            }
            r += 2.0 * (1.0 - j as f64 / k as f64) * rho(w, sum_sq, shift, l);
        }
        r /= k as f64;
        out.push(r);
        if l > 0 && r < NEGLIGIBLE * out[0] {
            negligible += 1;
            if negligible >= 2 {
                break;
            }
        } else {
            negligible = 0;
        }
    }
    out
}

/// `n_c`: the Gamma shape of a spectrum-history cell of `cell_width_hz` built from spectra of
/// `resolution` (see the [module docs](self)). Allocates: call when the geometry changes.
pub fn cell_value_shape(resolution: &Resolution, cell_width_hz: f64) -> f64 {
    let bw = resolution.bin_width_hz;
    assert!(
        bw > 0.0 && cell_width_hz > 0.0,
        "bin and cell widths must be positive"
    );
    let m = cell_width_hz / bw;
    let span = m.ceil() as usize + 1;
    let r = averaged_bin_covariance(resolution, span.min(MAX_LAG));
    let mut weights = vec![0.0f64; span + 1];
    let mut var_sum = 0.0;
    for k in 0..ALIGNMENT_OFFSETS {
        let lo = (k as f64 + 0.5) / ALIGNMENT_OFFSETS as f64;
        let hi = lo + m;
        let nb = (hi.ceil() as usize).min(weights.len());
        let mut total = 0.0;
        for (i, wt) in weights[..nb].iter_mut().enumerate() {
            *wt = (hi.min(i as f64 + 1.0) - lo.max(i as f64)).max(0.0);
            total += *wt;
        }
        let mut v = 0.0;
        for i in 0..nb {
            let wi = weights[i];
            if wi == 0.0 {
                continue;
            }
            v += wi * wi * r[0];
            for (l, &rl) in r.iter().enumerate().skip(1) {
                if i + l >= nb {
                    break;
                }
                v += 2.0 * wi * weights[i + l] * rl;
            }
        }
        var_sum += v / (total * total);
    }
    ALIGNMENT_OFFSETS as f64 / var_sum
}

/// `10·log10(P⁻¹(shape, p)/shape)`, dB: what a `p`-quantile of Gamma(`shape`) values reads
/// relative to their mean. Subtract it from a percentile to estimate the mean.
pub fn percentile_bias_db(shape: f64, p: f64) -> f64 {
    10.0 * gamma::mean_quantile(shape, p).log10()
}

/// The expected CDF value of a numpy-interpolated `q_percent` sample percentile of `frames`
/// values: `(1 + q·(n − 1))/(n + 1)`.
pub fn exact_percentile_probability(q_percent: f64, frames: u32) -> f64 {
    let n = f64::from(frames.max(1));
    (1.0 + q_percent / 100.0 * (n - 1.0)) / (n + 1.0)
}

/// Standard deviation of one cell value in dB: `10/ln 10 / √shape` (delta method).
pub fn cell_value_sd_db(shape: f64) -> f64 {
    10.0 / std::f64::consts::LN_10 / shape.sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synth::Rng;
    use crate::window::WindowKind;

    fn resolution(window: WindowKind, n: usize, overlap: usize, k: u32, fs: f64) -> Resolution {
        let metrics = Window::new(window, n).metrics();
        Resolution {
            window,
            fft_len: n,
            overlap,
            n_avg: k,
            bin_width_hz: fs / n as f64,
            rbw_hz: metrics.enbw_bins * fs / n as f64,
            window_metrics: metrics,
        }
    }

    #[test]
    fn hann_correlations_and_limits() {
        let w = Window::new(WindowKind::Hann, 4096);
        // Adjacent bins of one Hann segment: |−2/3|² = 4/9; two apart (1/6)² = 1/36.
        assert!((bin_power_correlation(&w, 0, 1) - 4.0 / 9.0).abs() < 1e-3);
        assert!((bin_power_correlation(&w, 0, 2) - 1.0 / 36.0).abs() < 1e-3);
        // 50 % overlap, same bin: (1/6)².
        assert!((bin_power_correlation(&w, 2048, 0) - 1.0 / 36.0).abs() < 1e-3);
        // R(0) is the Welch effective count.
        let r = resolution(WindowKind::Hann, 1024, 512, 32, 1e6);
        let cov = averaged_bin_covariance(&r, 16);
        let n_eff = crate::floor::effective_averages(&r);
        assert!((1.0 / cov[0] - n_eff).abs() < 1e-6 * n_eff);
        assert!(cov.len() <= 17);
        // The worked number in the module docs.
        let shape = cell_value_shape(&r, 6250.0);
        let p = exact_percentile_probability(10.0, 61);
        eprintln!(
            "T-017 geometry: n_eff {n_eff:.2}, n_c {shape:.1}, p {p:.4}, bias {:.3} dB (p = 0.1: {:.3} dB)",
            percentile_bias_db(shape, p),
            percentile_bias_db(shape, 0.1)
        );
        assert!((30.0..31.0).contains(&n_eff));
        // One bin per cell, no overlap: aligned it would be the bin (shape K); averaged over
        // alignments it straddles two correlated bins, so K < shape < 2K.
        let one = resolution(WindowKind::Hann, 1024, 0, 16, 1024.0);
        let s1 = cell_value_shape(&one, 1.0);
        assert!(s1 > 16.0 && s1 < 32.0, "{s1}");
        // Median of Gamma is below its mean; the level-0 rank rule.
        assert!(percentile_bias_db(100.0, 0.5) < 0.0);
        assert!((exact_percentile_probability(10.0, 59) - 6.8 / 60.0).abs() < 1e-12);
        assert_eq!(exact_percentile_probability(10.0, 1), 0.5);
    }

    /// Monte Carlo: white noise → Hann periodograms (50 % overlap, K = 8) → overlap-weighted
    /// means over 3.4-bin cells; the empirical variance of the cell values matches `1/n_c`.
    #[test]
    fn cell_shape_matches_monte_carlo() {
        use num_complex::Complex32;
        let (n, k, fs) = (64usize, 8u32, 64.0);
        let r = resolution(WindowKind::Hann, n, n / 2, k, fs);
        let cell = 3.4;
        let shape = cell_value_shape(&r, cell);
        let mut engine = crate::welch::SegmentEngine::new(crate::welch::WelchConfig {
            fft_len: n,
            overlap: n / 2,
            window: WindowKind::Hann,
            holds: false,
            spectral_kurtosis: false,
        })
        .unwrap();
        let _ = &mut engine;
        let window = Window::new(WindowKind::Hann, n);
        let w = window.coefficients();
        let mut planner = rustfft::FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(n);
        let mut rng = Rng::new(7);
        let trials = 6000;
        let samples = (k as usize - 1) * (n / 2) + n;
        let (mut s1, mut s2) = (0.0f64, 0.0f64);
        let mut count = 0usize;
        for trial in 0..trials {
            let x = crate::synth::complex_noise(&mut rng, samples, 1.0);
            let mut psd = vec![0.0f64; n];
            for seg in 0..k as usize {
                let mut buf: Vec<Complex32> = (0..n).map(|i| x[seg * n / 2 + i] * w[i]).collect();
                fft.process(&mut buf);
                for (p, c) in psd.iter_mut().zip(&buf) {
                    *p += f64::from(c.norm_sqr());
                }
            }
            // One cell per trial at a varying alignment, away from DC.
            let lo = 10.0 + (trial % 16) as f64 / 16.0;
            let hi = lo + cell;
            let (mut num, mut den) = (0.0, 0.0);
            for (i, &p) in psd
                .iter()
                .enumerate()
                .take(hi.ceil() as usize)
                .skip(lo.floor() as usize)
            {
                let o = (hi.min(i as f64 + 1.0) - lo.max(i as f64)).max(0.0);
                num += o * p;
                den += o;
            }
            let v = num / den;
            s1 += v;
            s2 += v * v;
            count += 1;
        }
        let mean = s1 / count as f64;
        let var = s2 / count as f64 / (mean * mean) - 1.0;
        let empirical = 1.0 / var;
        // Relative standard error of a variance from 6000 samples ≈ √(2/6000) ≈ 1.8 %.
        assert!(
            (empirical / shape - 1.0).abs() < 0.08,
            "shape {shape:.2} vs Monte Carlo {empirical:.2}"
        );
    }
}
