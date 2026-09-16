//! Open-set scoring (ADR-0016 §4.4): χ² tail probabilities of the per-family Mahalanobis
//! distance.
//!
//! A family's class-conditional density ([`crate::density`]) gives a squared Mahalanobis distance
//! `d²` over the `k` feature dimensions that did not abstain. Under that family's model `d²` is
//! χ²(k), so `L = P(χ²_k ≥ d²)` is a calibrated *plausibility* in 0–1 that does not grow with the
//! number of dimensions used — which matters here because different families score on different
//! feature subsets. The open-set score is `1 − max_c L_c`: far from every known family means every
//! tail probability is small.
//!
//! Softmax confidence is never used for this (ADR-0016 "Options considered": it is overconfident
//! on out-of-taxonomy inputs, C15 card pitfall).

/// `P(χ²_k ≥ x)` for `k ≥ 1` degrees of freedom, i.e. the regularised upper incomplete gamma
/// `Q(k/2, x/2)`. Returns 1 for `x ≤ 0` and 0 for a non-finite `x`.
pub fn chi2_sf(x: f64, k: usize) -> f64 {
    if !x.is_finite() || k == 0 {
        return 0.0;
    }
    if x <= 0.0 {
        return 1.0;
    }
    gamma_q(k as f64 / 2.0, x / 2.0)
}

/// Regularised upper incomplete gamma `Q(a, x) = Γ(a, x)/Γ(a)`, `a > 0`, `x ≥ 0`
/// (series below `a + 1`, Lentz continued fraction above; Numerical Recipes §6.2).
fn gamma_q(a: f64, x: f64) -> f64 {
    if x < a + 1.0 {
        1.0 - gamma_p_series(a, x)
    } else {
        gamma_q_cf(a, x)
    }
}

fn gamma_p_series(a: f64, x: f64) -> f64 {
    let mut ap = a;
    let mut sum = 1.0 / a;
    let mut del = sum;
    for _ in 0..1000 {
        ap += 1.0;
        del *= x / ap;
        sum += del;
        if del.abs() < sum.abs() * 1e-15 {
            break;
        }
    }
    (sum * (-x + a * x.ln() - ln_gamma(a)).exp()).clamp(0.0, 1.0)
}

fn gamma_q_cf(a: f64, x: f64) -> f64 {
    const TINY: f64 = 1e-300;
    let mut b = x + 1.0 - a;
    let mut c = 1.0 / TINY;
    let mut d = 1.0 / b;
    let mut h = d;
    for i in 1..1000 {
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
        if (del - 1.0).abs() < 1e-15 {
            break;
        }
    }
    ((-x + a * x.ln() - ln_gamma(a)).exp() * h).clamp(0.0, 1.0)
}

/// `ln Γ(z)` for `z > 0` (Lanczos, g = 7, n = 9; |relative error| < 1e-13 over the range used).
fn ln_gamma(z: f64) -> f64 {
    const C: [f64; 9] = [
        0.999_999_999_999_81,
        676.520_368_121_885_1,
        -1_259.139_216_722_402_8,
        771.323_428_777_653_1,
        -176.615_029_162_140_6,
        12.507_343_278_686_905,
        -0.138_571_095_265_720_12,
        9.984_369_578_019_572e-6,
        1.505_632_735_149_311_6e-7,
    ];
    let z = z - 1.0;
    let mut a = C[0];
    let t = z + 7.5;
    for (i, c) in C.iter().enumerate().skip(1) {
        a += c / (z + i as f64);
    }
    0.5 * (std::f64::consts::TAU).ln() + (z + 0.5) * t.ln() - t + a.ln()
}

/// The open-set score of a set of per-family plausibilities: `1 − max L_c`, clamped to 0–1.
/// With no family scored at all the input is maximally open (1.0).
pub fn open_set_score(plausibilities: impl IntoIterator<Item = f64>) -> f64 {
    let best = plausibilities
        .into_iter()
        .filter(|p| p.is_finite())
        .fold(0.0_f64, f64::max);
    (1.0 - best).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chi2_tails_match_published_quantiles() {
        // Textbook 5 % and 1 % critical values: P(χ²_k ≥ q) = α.
        for (k, q, alpha) in [
            (1, 3.841_46, 0.05),
            (2, 5.991_46, 0.05),
            (5, 11.070_5, 0.05),
            (10, 18.307, 0.05),
            (1, 6.634_9, 0.01),
            (5, 15.086_3, 0.01),
            (20, 37.566, 0.01),
        ] {
            let p = chi2_sf(q, k);
            assert!(
                (p - alpha).abs() < 1e-3,
                "chi2_sf({q}, {k}) = {p}, expected {alpha}"
            );
        }
        assert_eq!(chi2_sf(0.0, 4), 1.0);
        assert_eq!(chi2_sf(-1.0, 4), 1.0);
        assert!(chi2_sf(1e6, 4) < 1e-12);
        assert_eq!(chi2_sf(f64::NAN, 4), 0.0);
    }

    #[test]
    fn the_tail_is_monotone_and_dimension_normalised() {
        // Monotone in the distance.
        let mut prev = 1.0;
        for i in 1..50 {
            let p = chi2_sf(i as f64, 6);
            assert!(p <= prev, "not monotone at {i}");
            prev = p;
        }
        // A typical point (d² ≈ k) scores about the same whatever the dimension: this is why the
        // tail probability, not the raw density, is compared across families.
        for k in [2usize, 6, 12, 20] {
            let p = chi2_sf(k as f64, k);
            assert!((0.3..0.7).contains(&p), "k = {k}: {p}");
        }
    }

    #[test]
    fn the_open_set_score_is_one_minus_the_best_plausibility() {
        assert!((open_set_score([0.4, 0.9, 0.1]) - 0.1).abs() < 1e-12);
        assert_eq!(open_set_score([]), 1.0);
        assert_eq!(open_set_score([f64::NAN]), 1.0);
    }
}
