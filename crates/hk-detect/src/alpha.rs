//! OS-CFAR scale factors for `Gamma(n)` cells (S4 §3.2).
//!
//! The cell under test (CUT) and the `N` reference cells are iid `Gamma(n, θ)`: a PSD bin averaged
//! over `n` periodograms of complex Gaussian noise (`n` = effective averages). `Z` is the `k`-th
//! smallest reference cell. The detector declares `CUT > α·Z`, so the per-cell false-alarm
//! probability is
//!
//! ```text
//! Pfa(α) = ∫₀¹ b(u) · Q(n, α · P⁻¹(n, u)) du,   b = Beta(k, N − k + 1) density,
//! ```
//!
//! using `U = F(Z) ~ Beta(k, N−k+1)` (the `k`-th uniform order statistic) and `P(CUT > x) =
//! Q(n, x)` in units of `θ` (the scale cancels). The integrand is smooth on (0, 1), so composite
//! Gauss–Legendre (128 panels × 8 nodes) is accurate to far better than 1 % in `Pfa` down to
//! 1e-9; `α` is found by the Illinois method on `ln Pfa(ln α)`. For `n = 1` the closed form
//! `Pfa = Π_{i<k} (N−i)/(N−i+α)` ([`os_cfar_alpha_exponential`]) cross-checks it.
//!
//! S4 values (N 32, k 24, n 10): Pfa 1e-2 → 2.14 dB, 1e-3 → 3.00, 1e-4 → 3.67, 1e-5 → 4.22,
//! 1e-6 → 4.70 dB. Results are cached per `(n, N, k, Pfa)` for the process, so a retune costs
//! nothing after the first segment with a given resolution.

use std::sync::{Mutex, OnceLock};

use hk_dsp::floor::gamma;

/// Gauss–Legendre 8-point nodes on [−1, 1] (positive half).
const GL_X: [f64; 4] = [
    0.183_434_642_495_649_8,
    0.525_532_409_916_329,
    0.796_666_477_413_626_7,
    0.960_289_856_497_536_3,
];
/// Matching weights.
const GL_W: [f64; 4] = [
    0.362_683_783_378_362,
    0.313_706_645_877_887_3,
    0.222_381_034_453_374_5,
    0.101_228_536_290_376_3,
];
const PANELS: usize = 128;

/// Quadrature model of the OS-CFAR false-alarm probability for one `(n, N, k)`.
#[derive(Clone, Debug)]
pub struct OsCfarModel {
    /// Gamma shape of every cell.
    pub n: f64,
    /// Reference cells.
    pub reference_cells: usize,
    /// Order-statistic rank (1-based: `k`-th smallest).
    pub rank: usize,
    /// `P⁻¹(n, u)` at each node.
    y: Vec<f64>,
    /// `ln(w · b(u))` at each node.
    ln_weight: Vec<f64>,
}

impl OsCfarModel {
    /// Builds the quadrature for `n` averages, `reference_cells` reference cells and rank `rank`.
    pub fn new(n: f64, reference_cells: usize, rank: usize) -> Self {
        assert!(n > 0.0, "gamma shape must be positive");
        assert!(
            rank >= 1 && rank <= reference_cells,
            "rank {rank} must be in 1..={reference_cells}"
        );
        let big_n = reference_cells as f64;
        let k = rank as f64;
        let ln_beta =
            gamma::ln_gamma(big_n + 1.0) - gamma::ln_gamma(k) - gamma::ln_gamma(big_n - k + 1.0);
        let mut y = Vec::with_capacity(PANELS * 8);
        let mut ln_weight = Vec::with_capacity(PANELS * 8);
        let h = 1.0 / PANELS as f64;
        for p in 0..PANELS {
            let mid = (p as f64 + 0.5) * h;
            for (&x, &w) in GL_X.iter().zip(&GL_W) {
                for sign in [-1.0, 1.0] {
                    let u = mid + sign * x * h / 2.0;
                    let ln_b = ln_beta + (k - 1.0) * u.ln() + (big_n - k) * (-u).ln_1p();
                    y.push(gamma::inverse_lower(n, u));
                    ln_weight.push((w * h / 2.0).ln() + ln_b);
                }
            }
        }
        Self {
            n,
            reference_cells,
            rank,
            y,
            ln_weight,
        }
    }

    /// `ln Pfa(α)` (log-sum-exp over the nodes, so tiny probabilities keep their precision).
    pub fn ln_pfa(&self, alpha: f64) -> f64 {
        let mut terms_max = f64::NEG_INFINITY;
        // First pass: the largest term, for a stable log-sum-exp.
        for (&y, &lw) in self.y.iter().zip(&self.ln_weight) {
            let t = lw + gamma::ln_regularized_upper(self.n, alpha * y);
            if t > terms_max {
                terms_max = t;
            }
        }
        if !terms_max.is_finite() {
            return terms_max;
        }
        let mut sum = 0.0;
        for (&y, &lw) in self.y.iter().zip(&self.ln_weight) {
            let t = lw + gamma::ln_regularized_upper(self.n, alpha * y);
            sum += (t - terms_max).exp();
        }
        terms_max + sum.ln()
    }

    /// Per-cell false-alarm probability at scale `alpha`.
    pub fn pfa(&self, alpha: f64) -> f64 {
        self.ln_pfa(alpha).exp()
    }

    /// The scale `α` with `Pfa(α) = pfa`.
    pub fn alpha(&self, pfa: f64) -> f64 {
        assert!(pfa > 0.0 && pfa < 1.0, "pfa must be in (0, 1)");
        let target = pfa.ln();
        let g = |la: f64| self.ln_pfa(la.exp()) - target;
        // Pfa(1) is large (≈ P(CUT > Z) ~ 0.3 for k = 3N/4); Pfa decreases with α.
        let (mut a, mut b) = (0.0f64, 1.0f64);
        let (mut ga, mut gb) = (g(a), g(b));
        while gb > 0.0 {
            a = b;
            ga = gb;
            b *= 2.0;
            gb = g(b);
            assert!(b < 64.0, "OS-CFAR alpha bracket failed for pfa {pfa}");
        }
        if ga < 0.0 {
            // pfa above Pfa(α = 1): α < 1 is not a useful threshold; clamp.
            return 1.0;
        }
        // Illinois (modified regula falsi).
        let mut side = 0i8;
        for _ in 0..200 {
            let c = (a * gb - b * ga) / (gb - ga);
            let gc = g(c);
            if gc.abs() < 1e-12 || (b - a).abs() < 1e-14 {
                return c.exp();
            }
            if gc > 0.0 {
                a = c;
                ga = gc;
                if side == -1 {
                    gb /= 2.0;
                }
                side = -1;
            } else {
                b = c;
                gb = gc;
                if side == 1 {
                    ga /= 2.0;
                }
                side = 1;
            }
        }
        ((a + b) / 2.0).exp()
    }
}

/// Closed-form OS-CFAR `α` for exponential cells (`n = 1`): solves
/// `Π_{i=0}^{k−1} (N−i)/(N−i+α) = pfa`.
pub fn os_cfar_alpha_exponential(reference_cells: usize, rank: usize, pfa: f64) -> f64 {
    let n = reference_cells as f64;
    let ln_pfa = |a: f64| -> f64 {
        (0..rank)
            .map(|i| {
                let m = n - i as f64;
                (m / (m + a)).ln()
            })
            .sum()
    };
    let target = pfa.ln();
    let (mut lo, mut hi) = (0.0f64, 1.0f64);
    while ln_pfa(hi) > target {
        hi *= 2.0;
    }
    for _ in 0..200 {
        let mid = 0.5 * (lo + hi);
        if ln_pfa(mid) > target {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    0.5 * (lo + hi)
}

type CacheKey = (u64, usize, usize, u64);

fn cache() -> &'static Mutex<Vec<(CacheKey, f64)>> {
    static CACHE: OnceLock<Mutex<Vec<(CacheKey, f64)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(Vec::new()))
}

/// `α` for `Gamma(n)` cells, `reference_cells` reference cells, rank `rank`, per-cell `pfa`
/// (cached for the process).
pub fn os_cfar_alpha(n: f64, reference_cells: usize, rank: usize, pfa: f64) -> f64 {
    let key = (n.to_bits(), reference_cells, rank, pfa.to_bits());
    if let Some(&(_, a)) = cache()
        .lock()
        .expect("alpha cache poisoned")
        .iter()
        .find(|(k, _)| *k == key)
    {
        return a;
    }
    let a = OsCfarModel::new(n, reference_cells, rank).alpha(pfa);
    cache().lock().expect("alpha cache poisoned").push((key, a));
    a
}

/// `10·log10(x)`.
pub fn db(x: f64) -> f64 {
    10.0 * x.log10()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exponential_closed_form_matches_s4() {
        // S4 §3.2: numeric α for n = 1 matches the closed form, 14.3985.
        let a = os_cfar_alpha_exponential(32, 24, 1e-6);
        assert!((a - 14.3985).abs() < 5e-4, "{a}");
        let model = OsCfarModel::new(1.0, 32, 24);
        for pfa in [1e-2, 1e-4, 1e-6, 1e-8] {
            let closed = os_cfar_alpha_exponential(32, 24, pfa);
            let numeric = model.alpha(pfa);
            assert!(
                (numeric / closed - 1.0).abs() < 1e-4,
                "pfa {pfa}: numeric {numeric} vs closed {closed}"
            );
        }
    }

    #[test]
    fn s4_alpha_table_for_gamma_10() {
        // S4 §3.2 / §5 (N 32, k 24, n 10), ±0.02 dB.
        for (pfa, want_db) in [
            (1e-2, 2.14),
            (1e-3, 3.00),
            (1e-4, 3.67),
            (1e-5, 4.22),
            (1e-6, 4.70),
        ] {
            let got = db(os_cfar_alpha(10.0, 32, 24, pfa));
            assert!(
                (got - want_db).abs() <= 0.02,
                "pfa {pfa}: α {got:.4} dB vs S4 {want_db}"
            );
        }
    }

    #[test]
    fn pfa_is_monotone_and_inverts() {
        let m = OsCfarModel::new(10.0, 32, 24);
        let a = m.alpha(1e-7);
        assert!((m.pfa(a) / 1e-7 - 1.0).abs() < 1e-6);
        assert!(m.pfa(a * 1.01) < m.pfa(a));
    }
}
