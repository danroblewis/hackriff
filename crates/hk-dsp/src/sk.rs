//! Spectral kurtosis (SK): a per-bin test for "is this bin Gaussian noise?".
//!
//! # Estimator
//!
//! For one FFT bin, take `M` power estimates `P_i = |X_i|²` from consecutive (windowed) FFT
//! segments and accumulate
//!
//! ```text
//! S1 = Σ P_i        S2 = Σ P_i²        (i = 1..M, linear power, never dB)
//! ```
//!
//! The unbiased SK estimator (Nita & Gary 2010, "The generalized spectral kurtosis estimator",
//! MNRAS 406 L60, with `N = 1` power per accumulation and shape `d = 1`; equivalent to Antoni's
//! SK with the finite-`M` bias removed) is
//!
//! ```text
//! SK = (M + 1)/(M − 1) · (M·S2/S1² − 1)
//! ```
//!
//! - **Gaussian noise:** each `P_i` is exponential (χ² with 2 DOF), `E[P²] = 2E[P]²`, and
//!   `E[SK] = 1` exactly for any `M >= 2`.
//! - **Constant envelope (CW, FM carrier):** `P_i` constant, `M·S2 = S1²`, `SK = 0`. With noise
//!   underneath, `0 < SK < 1`.
//! - **Intermittent (bursts, pulses, OOK):** heavy-tailed `P_i`, `SK > 1`. A constant-envelope
//!   burst present in a fraction `δ` of segments, far above the noise, gives
//!   `SK = (M + 1)/(M − 1)·(1/δ − 1)`, so `δ = 1 / (1 + SK·(M − 1)/(M + 1))` (a noise-like burst
//!   gives `SK ≈ 2/δ − 1`).
//!
//! # Variance (for thresholding in T-006)
//!
//! For Gaussian noise and independent segments,
//!
//! ```text
//! Var[SK] = 4M² / ((M − 1)(M + 2)(M + 3))  ≈ 4/M for large M
//! ```
//!
//! (Nita & Gary 2010 eq. 9 with `N = d = 1`). The distribution is skewed for small `M`; a
//! detector wanting a stated false-alarm rate should use Pearson type III/IV thresholds on this
//! mean and variance (Nita & Gary 2010), or `1 ± 3σ` as a first cut for `M >= 100`.
//!
//! **Overlap caveat:** overlapping segments are not independent. With 50% Hann overlap the
//! adjacent-segment power correlation is `|ρ|² = 1/36`, which biases `E[SK]` low by about
//! `2|ρ|²/M` (negligible) and inflates the variance by roughly `1 + 2|ρ|²`. Use overlap 0 when the
//! SK threshold must be exact.

/// SK from `m` accumulated powers with sums `s1 = ΣP` and `s2 = ΣP²`. `NaN` when `s1 == 0` (a
/// bin that saw no power at all) or `m < 2`.
#[inline]
pub fn estimate(m: u32, s1: f64, s2: f64) -> f32 {
    if m < 2 || s1 <= 0.0 {
        return f32::NAN;
    }
    let m = f64::from(m);
    ((m + 1.0) / (m - 1.0) * (m * s2 / (s1 * s1) - 1.0)) as f32
}

/// Expected SK for Gaussian noise: exactly 1.
pub const EXPECTED_NOISE_SK: f64 = 1.0;

/// Variance of the SK estimator on Gaussian noise with independent segments,
/// `4M² / ((M − 1)(M + 2)(M + 3))`. `NaN` for `m < 2`.
pub fn variance(m: u32) -> f64 {
    if m < 2 {
        return f64::NAN;
    }
    let m = f64::from(m);
    4.0 * m * m / ((m - 1.0) * (m + 2.0) * (m + 3.0))
}

/// Standard deviation of the SK estimator on Gaussian noise, `sqrt(variance(m))`.
pub fn std_dev(m: u32) -> f64 {
    variance(m).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formulas() {
        // Constant power: SK = 0.
        assert_eq!(estimate(10, 10.0, 10.0), 0.0);
        // Exponential moments exactly: E[S1²] = M(M+1)μ², E[S2] = 2Mμ² gives SK = 1 in the
        // ratio-of-expectations sense.
        assert!(variance(1000) > 0.0039 && variance(1000) < 0.0041);
        assert!(estimate(1, 1.0, 1.0).is_nan());
        assert!(estimate(4, 0.0, 0.0).is_nan());
    }
}
