//! Gamma-distribution utilities shared by the floor estimators (T-005) and CFAR (T-006).
//!
//! A PSD bin averaged over `n` independent periodograms of complex Gaussian noise with mean `μ`
//! is `Gamma(shape n, scale μ/n)`: mean `μ`, variance `μ²/n` (a scaled `χ²_{2n}`). With `X` in
//! units of its mean (`μ = 1`), `P(X ≤ t) = P(n, n·t)` where `P` is the regularised lower
//! incomplete gamma function. Everything here works for non-integer `n` (the effective averaging
//! count of overlapped segments, [`effective_averages`](super::effective_averages)).
//!
//! | Function | Meaning |
//! |---|---|
//! | [`regularized_lower`] `P(a, x)`, [`regularized_upper`] `Q(a, x)` | `γ(a,x)/Γ(a)`, `Γ(a,x)/Γ(a)` |
//! | [`inverse_lower`], [`inverse_upper`] | `x` with `P(a,x) = p` / `Q(a,x) = q` |
//! | [`mean_threshold`] `T(n, pfa)` | `Q⁻¹(n, pfa)/n`: `P(X > T·μ) = pfa`. Gamma(10): 1e-3 → 3.55 dB, 1e-6 → 5.15 dB |
//! | [`mean_quantile`] `(n, p)` | `P⁻¹(n, p)/n`: the `p`-quantile in units of the mean (percentile bias) |
//! | [`truncated_mean_ratio`] `(n, t)` | `E[X | X < t·μ]/μ = P(n+1, n·t)/P(n, n·t)` (FCME bias) |
//! | [`exceedance`] `(n, t)` | `P(X > t·μ) = Q(n, n·t)` |
//!
//! Implementation: Lanczos `ln Γ` (g = 7, 9 terms, ~1e-15 relative); `P` by its power series
//! when `x < a + 1` and `Q` by the modified-Lentz continued fraction otherwise (each computed
//! directly in the tail where it is small, so tail probabilities keep full relative precision);
//! inverses by Wilson–Hilferty start + safeguarded Newton on whichever tail is smaller. Written
//! from the standard formulas (Abramowitz & Stegun 6.5.29, 6.5.31; 26.4.17); no dependency.

use std::f64::consts::PI;

const EPS: f64 = 1e-15;
const TINY: f64 = 1e-300;
const MAX_ITER: usize = 100_000;

const LANCZOS_G: f64 = 7.0;
const LANCZOS: [f64; 9] = [
    0.999_999_999_999_809_9,
    676.520_368_121_885_1,
    -1_259.139_216_722_402_8,
    771.323_428_777_653_1,
    -176.615_029_162_140_6,
    12.507_343_278_686_905,
    -0.138_571_095_265_720_12,
    9.984_369_578_019_572e-6,
    1.505_632_735_149_311_6e-7,
];

/// `ln Γ(x)` for `x > 0` (reflection for `x < 0.5`).
pub fn ln_gamma(x: f64) -> f64 {
    if x < 0.5 {
        // Γ(x)Γ(1−x) = π / sin(πx)
        return (PI / (PI * x).sin().abs()).ln() - ln_gamma(1.0 - x);
    }
    let x = x - 1.0;
    let mut a = LANCZOS[0];
    for (i, &c) in LANCZOS.iter().enumerate().skip(1) {
        a += c / (x + i as f64);
    }
    let t = x + LANCZOS_G + 0.5;
    0.5 * (2.0 * PI).ln() + (x + 0.5) * t.ln() - t + a.ln()
}

/// `ln(x^a e^{−x} / Γ(a))`, the common prefactor.
fn ln_prefactor(a: f64, x: f64) -> f64 {
    a * x.ln() - x - ln_gamma(a)
}

/// Series for `P(a, x)`; converges quickly for `x < a + 1`.
fn lower_series(a: f64, x: f64) -> f64 {
    let mut ap = a;
    let mut del = 1.0 / a;
    let mut sum = del;
    for _ in 0..MAX_ITER {
        ap += 1.0;
        del *= x / ap;
        sum += del;
        if del.abs() < sum.abs() * EPS {
            break;
        }
    }
    (sum.ln() + ln_prefactor(a, x)).exp()
}

/// Continued fraction for `Q(a, x)`; converges quickly for `x ≥ a + 1`.
fn upper_fraction(a: f64, x: f64) -> f64 {
    let mut b = x + 1.0 - a;
    let mut c = 1.0 / TINY;
    let mut d = 1.0 / b;
    let mut h = d;
    for i in 1..MAX_ITER {
        let an = -(i as f64) * (i as f64 - a);
        b += 2.0;
        d = an * d + b;
        if d.abs() < TINY {
            d = TINY;
        }
        c = b + an / c;
        if c.abs() < TINY {
            c = TINY;
        }
        d = 1.0 / d;
        let del = d * c;
        h *= del;
        if (del - 1.0).abs() < EPS {
            break;
        }
    }
    (h.ln() + ln_prefactor(a, x)).exp()
}

/// Regularised lower incomplete gamma `P(a, x) = γ(a, x)/Γ(a)`, `a > 0`, `x ≥ 0`.
pub fn regularized_lower(a: f64, x: f64) -> f64 {
    assert!(a > 0.0, "gamma shape must be positive");
    if x <= 0.0 {
        0.0
    } else if x.is_infinite() {
        1.0
    } else if x < a + 1.0 {
        lower_series(a, x)
    } else {
        1.0 - upper_fraction(a, x)
    }
}

/// Regularised upper incomplete gamma `Q(a, x) = 1 − P(a, x)`, accurate in the upper tail.
pub fn regularized_upper(a: f64, x: f64) -> f64 {
    assert!(a > 0.0, "gamma shape must be positive");
    if x <= 0.0 {
        1.0
    } else if x.is_infinite() {
        0.0
    } else if x < a + 1.0 {
        1.0 - lower_series(a, x)
    } else {
        upper_fraction(a, x)
    }
}

/// Standard normal quantile (Acklam's rational approximation, ~1e-9; only a starting point).
fn normal_quantile(p: f64) -> f64 {
    const A: [f64; 6] = [
        -3.969_683_028_665_376e1,
        2.209_460_984_245_205e2,
        -2.759_285_104_469_687e2,
        1.383_577_518_672_69e2,
        -3.066_479_806_614_716e1,
        2.506_628_277_459_239,
    ];
    const B: [f64; 5] = [
        -5.447_609_879_822_406e1,
        1.615_858_368_580_409e2,
        -1.556_989_798_598_866e2,
        6.680_131_188_771_972e1,
        -1.328_068_155_288_572e1,
    ];
    const C: [f64; 6] = [
        -7.784_894_002_430_293e-3,
        -3.223_964_580_411_365e-1,
        -2.400_758_277_161_838,
        -2.549_732_539_343_734,
        4.374_664_141_464_968,
        2.938_163_982_698_783,
    ];
    const D: [f64; 4] = [
        7.784_695_709_041_462e-3,
        3.224_671_290_700_398e-1,
        2.445_134_137_142_996,
        3.754_408_661_907_416,
    ];
    let tail = |q: f64| {
        let q = (-2.0 * q.ln()).sqrt();
        (((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    };
    if p < 0.02425 {
        tail(p)
    } else if p > 1.0 - 0.02425 {
        -tail(1.0 - p)
    } else {
        let q = p - 0.5;
        let r = q * q;
        (((((A[0] * r + A[1]) * r + A[2]) * r + A[3]) * r + A[4]) * r + A[5]) * q
            / (((((B[0] * r + B[1]) * r + B[2]) * r + B[3]) * r + B[4]) * r + 1.0)
    }
}

/// Solves `P(a, x) = p` (`lower = true`) or `Q(a, x) = p` (`lower = false`) for `x`.
fn invert(a: f64, p: f64, lower: bool) -> f64 {
    assert!(a > 0.0, "gamma shape must be positive");
    assert!((0.0..=1.0).contains(&p), "probability must be in [0, 1]");
    // Work on the tail with the smaller target probability for relative precision.
    let (target, use_lower) = match (lower, p <= 0.5) {
        (true, true) => (p, true),
        (true, false) => (1.0 - p, false),
        (false, true) => (p, false),
        (false, false) => (1.0 - p, true),
    };
    if target <= 0.0 {
        return if use_lower { 0.0 } else { f64::INFINITY };
    }
    let p_lower = if use_lower { target } else { 1.0 - target };
    // Wilson–Hilferty start.
    let z = normal_quantile(p_lower);
    let s = 1.0 / (9.0 * a);
    let mut x = a * (1.0 - s + z * s.sqrt()).powi(3);
    if !(x.is_finite() && x > 0.0) {
        x = if p_lower < 0.5 {
            (p_lower * (ln_gamma(a + 1.0)).exp())
                .powf(1.0 / a)
                .max(1e-300)
        } else {
            a + 1.0
        };
    }
    // Bracket: f(x) = tail(x) − target is monotone (lower tail increasing, upper decreasing).
    let f = |x: f64| {
        if use_lower {
            regularized_lower(a, x) - target
        } else {
            regularized_upper(a, x) - target
        }
    };
    let (mut lo, mut hi) = (0.0f64, f64::INFINITY);
    for _ in 0..200 {
        let fx = f(x);
        let below = if use_lower { fx < 0.0 } else { fx > 0.0 };
        if below {
            lo = x;
        } else {
            hi = x;
        }
        if fx.abs() <= target * 1e-13 {
            return x;
        }
        // d tail/dx = ± x^(a−1) e^(−x) / Γ(a)
        let pdf = ((a - 1.0) * x.ln() - x - ln_gamma(a)).exp();
        let slope = if use_lower { pdf } else { -pdf };
        let mut next = if slope != 0.0 {
            x - fx / slope
        } else {
            f64::NAN
        };
        if !(next.is_finite() && next > lo && next < hi) {
            next = if hi.is_finite() {
                0.5 * (lo + hi)
            } else {
                2.0 * x.max(1.0)
            };
        }
        if (next - x).abs() <= x * 1e-15 {
            return next;
        }
        x = next;
    }
    x
}

/// `x` with `P(a, x) = p`.
pub fn inverse_lower(a: f64, p: f64) -> f64 {
    invert(a, p, true)
}

/// `x` with `Q(a, x) = q`; accurate for tiny `q` (e.g. a false-alarm probability).
pub fn inverse_upper(a: f64, q: f64) -> f64 {
    invert(a, q, false)
}

/// Threshold multiplier `T` on the mean of an `n`-average bin such that `P(X > T·μ) = pfa`.
pub fn mean_threshold(n: f64, pfa: f64) -> f64 {
    inverse_upper(n, pfa) / n
}

/// The `p`-quantile of an `n`-average bin in units of its mean (divide a percentile by this).
pub fn mean_quantile(n: f64, p: f64) -> f64 {
    inverse_lower(n, p) / n
}

/// `E[X | X < t·μ] / μ` for an `n`-average bin: the FCME truncated-mean bias factor.
pub fn truncated_mean_ratio(n: f64, t: f64) -> f64 {
    regularized_lower(n + 1.0, n * t) / regularized_lower(n, n * t)
}

/// `P(X > t·μ)` for an `n`-average bin.
pub fn exceedance(n: f64, t: f64) -> f64 {
    regularized_upper(n, n * t)
}

/// A `Gamma(n)` variate with unit mean (the mean of `n` unit exponentials), for Monte Carlo
/// (bias tables, tests). Not the real-time path.
pub fn sample_unit_mean(rng: &mut crate::synth::Rng, n: u32) -> f64 {
    let n = n.max(1);
    let mut log_sum = 0.0;
    let mut left = n;
    while left > 0 {
        // Product of up to 64 uniforms stays far above f64 underflow.
        let chunk = left.min(64);
        let mut prod = 1.0;
        for _ in 0..chunk {
            prod *= rng.unit();
        }
        log_sum += prod.ln();
        left -= chunk;
    }
    -log_sum / f64::from(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64, rel: f64) -> bool {
        (a - b).abs() <= rel * b.abs().max(1e-300)
    }

    #[test]
    fn ln_gamma_matches_factorials_and_half_integers() {
        let mut f = 1.0f64;
        for n in 1..30 {
            assert!((ln_gamma(n as f64) - f.ln()).abs() < 1e-12, "n = {n}");
            f *= n as f64;
        }
        assert!(close(ln_gamma(0.5), PI.sqrt().ln(), 1e-13));
        assert!(close(ln_gamma(1e5), 1_051_287.708_973_657_5, 1e-14));
    }

    #[test]
    fn incomplete_gamma_reference_values() {
        // Exponential: P(1, x) = 1 − e^−x.
        for x in [1e-6, 0.1, 1.0, 5.0, 30.0] {
            assert!(close(regularized_lower(1.0, x), -(-x).exp_m1(), 1e-12));
            assert!(close(regularized_upper(1.0, x), (-x).exp(), 1e-12));
        }
        // Integer shape: Q(n, x) = e^−x Σ_{k<n} x^k/k!
        for (n, x) in [(10u32, 5.0f64), (10, 22.65), (10, 40.0), (3, 0.2)] {
            let mut term = 1.0;
            let mut sum = 1.0;
            for k in 1..n {
                term *= x / f64::from(k);
                sum += term;
            }
            let q = (-x).exp() * sum;
            assert!(
                close(regularized_upper(f64::from(n), x), q, 1e-11),
                "Q({n},{x})"
            );
        }
        // Chi-square(2·5) median etc. via P + Q = 1.
        for (a, x) in [(0.3, 0.1), (7.7, 7.0), (100.0, 110.0), (2.5, 2.5)] {
            assert!((regularized_lower(a, x) + regularized_upper(a, x) - 1.0).abs() < 1e-13);
        }
    }

    #[test]
    fn inverses_round_trip() {
        for a in [0.5, 1.0, 2.0, 7.7, 10.0, 16.0, 100.0, 2000.0] {
            for p in [1e-12, 1e-6, 1e-3, 0.1, 0.2, 0.5, 0.9, 0.999] {
                let x = inverse_lower(a, p);
                assert!(close(regularized_lower(a, x), p, 1e-9), "P⁻¹({a},{p})");
                let x = inverse_upper(a, p);
                assert!(close(regularized_upper(a, x), p, 1e-9), "Q⁻¹({a},{p})");
            }
        }
        // Exponential closed form.
        assert!(close(inverse_upper(1.0, 1e-6), 1e6f64.ln(), 1e-12));
    }

    #[test]
    fn s4_floor_branch_thresholds() {
        // S4 §3.2: Gamma(10) floor branch 1e-3 → 3.55 dB, 1e-6 → 5.15 dB (±0.02 dB).
        let db = |x: f64| 10.0 * x.log10();
        let t3 = db(mean_threshold(10.0, 1e-3));
        let t6 = db(mean_threshold(10.0, 1e-6));
        assert!((t3 - 3.55).abs() < 0.02, "{t3}");
        assert!((t6 - 5.15).abs() < 0.02, "{t6}");
        assert!(close(
            exceedance(10.0, mean_threshold(10.0, 1e-6)),
            1e-6,
            1e-9
        ));
    }

    #[test]
    fn exponential_special_cases() {
        // Median of an exponential bin is ln 2 of its mean.
        assert!(close(mean_quantile(1.0, 0.5), 2f64.ln(), 1e-12));
        // E[X | X < t] for Exp(1) = (1 − (1+t)e^−t)/(1 − e^−t).
        let t: f64 = 2.3;
        let want = (1.0 - (1.0 + t) * (-t).exp()) / (1.0 - (-t).exp());
        assert!(close(truncated_mean_ratio(1.0, t), want, 1e-12));
    }
}
