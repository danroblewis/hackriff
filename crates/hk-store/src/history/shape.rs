//! Noise-shape estimation for sweep and `hackrf_sweep` rows (T-126).
//!
//! The bias-corrected floor ([`super::CellStats::floor_db`]) needs the Gamma shape of the values
//! behind a cell. An STFT frame knows it from its resolution; a sweep row usually does not (how
//! many FFTs `hackrf_sweep` averaged into a bin depends on its version and flags). The shape is
//! therefore **estimated from the data**:
//!
//! # Method
//!
//! A noise-only bin's linear power over time is Gamma(`k`, θ). Its dB value has variance
//! `(10/ln 10)² · ψ′(k)` (trigamma), **independent of θ** — so the floor level, the front-end
//! frequency response across a tune step and slow gain differences between bins do not matter.
//! [`NoiseShapeEstimator`] keeps running (Welford) mean and variance of the dB value of every bin
//! of every row position (rows keyed by their start frequency), then:
//!
//! 1. keeps bins seen in at least [`MIN_FRAMES`] rows;
//! 2. per row position, drops bins whose mean is more than `3 dB + 3·sd(mean)` above the median
//!    bin mean (carriers and busy channels);
//! 3. drops bins whose variance exceeds 4× the median variance (fluctuating signals, bursts);
//! 4. pools the rest: `var = Σ squared deviations / Σ (n − 1)`, and inverts the trigamma
//!    relation for `k`.
//!
//! # Accuracy
//!
//! Measured by the unit tests below and `tests/followups.rs` on synthetic k-look noise with 5 %
//! of bins carrying signals: with 16 rows × ≥ 256 noise bins, `k` is within 5 % for k = 1…16
//! (pooled estimate; statistical error ≲ 1 %, the truncation in step 3 biases `k` up by ≲ 2 %).
//! A floor that drifts during the estimate (AGC, gain stepping, temperature) inflates the variance
//! and so underestimates `k`, which over-corrects: the floor then reads high. Feed the estimator
//! rows taken under one front-end state. Signals within ~3 dB of the floor that fluctuate little
//! pass the filters and bias `k` slightly high.

use std::collections::HashMap;

use hk_model::SweepFrame;

/// Rows a bin needs before it contributes.
pub const MIN_FRAMES: u32 = 8;
/// Pooled noise bins needed for an estimate.
pub const MIN_BINS: usize = 32;

const DB_PER_NEPER2: f64 = (10.0 / std::f64::consts::LN_10) * (10.0 / std::f64::consts::LN_10);

/// Trigamma ψ′(x) for x > 0 (recurrence to x ≥ 6, then the asymptotic series; error < 1e-10).
pub fn trigamma(mut x: f64) -> f64 {
    let mut acc = 0.0;
    while x < 6.0 {
        acc += 1.0 / (x * x);
        x += 1.0;
    }
    let x2 = 1.0 / (x * x);
    acc + 1.0 / x
        + x2 / 2.0
        + x2 / x * (1.0 / 6.0 - x2 * (1.0 / 30.0 - x2 * (1.0 / 42.0 - x2 / 30.0)))
}

/// The Gamma shape `k` whose dB values have variance `var_db2` (dB²): the inverse of
/// `(10/ln 10)² · ψ′(k)`, clamped to `[0.01, 1e6]`.
pub fn shape_from_db_variance(var_db2: f64) -> f64 {
    let v = var_db2 / DB_PER_NEPER2;
    let (mut lo, mut hi) = (0.01f64.ln(), 1e6f64.ln());
    if !(v.is_finite() && v > 0.0) || v <= trigamma(hi.exp()) {
        return 1e6;
    }
    if v >= trigamma(lo.exp()) {
        return 0.01;
    }
    for _ in 0..80 {
        let mid = 0.5 * (lo + hi);
        if trigamma(mid.exp()) > v {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    (0.5 * (lo + hi)).exp()
}

#[derive(Clone, Debug, Default)]
struct Group {
    rows: u32,
    n: Vec<u32>,
    mean: Vec<f64>,
    ss: Vec<f64>,
}

/// Estimates the Gamma shape of sweep-bin values from the rows themselves (see the
/// [module docs](self) for the method and its accuracy).
#[derive(Clone, Debug, Default)]
pub struct NoiseShapeEstimator {
    groups: HashMap<u64, Group>,
}

fn median(v: &mut [f64]) -> f64 {
    let mid = v.len() / 2;
    let (_, m, _) = v.select_nth_unstable_by(mid, f64::total_cmp);
    *m
}

impl NoiseShapeEstimator {
    /// An empty estimator.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds one row of per-bin dB values starting at `f_lo_hz` (rows at the same start frequency
    /// are the same bins over time; a row of a different length restarts that position).
    pub fn observe(&mut self, f_lo_hz: f64, db: &[f32]) {
        let g = self.groups.entry(f_lo_hz.to_bits()).or_default();
        if g.n.len() != db.len() {
            *g = Group {
                rows: 0,
                n: vec![0; db.len()],
                mean: vec![0.0; db.len()],
                ss: vec![0.0; db.len()],
            };
        }
        g.rows += 1;
        for (i, &d) in db.iter().enumerate() {
            if !d.is_finite() {
                continue;
            }
            let x = f64::from(d);
            g.n[i] += 1;
            let delta = x - g.mean[i];
            g.mean[i] += delta / f64::from(g.n[i]);
            g.ss[i] += delta * (x - g.mean[i]);
        }
    }

    /// Adds a dB-valued [`SweepFrame`] row.
    pub fn observe_sweep(&mut self, f: &SweepFrame) {
        self.observe(f.freq.lo_hz, &f.power);
    }

    /// Most rows seen at one start frequency (sweeps observed).
    pub fn sweeps(&self) -> u32 {
        self.groups.values().map(|g| g.rows).max().unwrap_or(0)
    }

    /// The estimated bin shape `k`, or `None` with fewer than [`MIN_BINS`] usable noise bins.
    pub fn bin_shape(&self) -> Option<f32> {
        // (squared deviations, degrees of freedom, variance) of candidate bins.
        let mut cand: Vec<(f64, f64, f64)> = Vec::new();
        for g in self.groups.values() {
            let idx: Vec<usize> = (0..g.n.len()).filter(|&i| g.n[i] >= MIN_FRAMES).collect();
            if idx.is_empty() {
                continue;
            }
            let var = |i: usize| g.ss[i] / f64::from(g.n[i] - 1);
            let mut means: Vec<f64> = idx.iter().map(|&i| g.mean[i]).collect();
            let mut vars: Vec<f64> = idx.iter().map(|&i| var(i)).collect();
            let m_med = median(&mut means);
            let v_med = median(&mut vars);
            let n_min = idx.iter().map(|&i| g.n[i]).min().unwrap_or(MIN_FRAMES);
            let guard = 3.0 + 3.0 * (v_med / f64::from(n_min)).sqrt();
            cand.extend(
                idx.iter()
                    .filter(|&&i| g.mean[i] <= m_med + guard)
                    .map(|&i| (g.ss[i], f64::from(g.n[i] - 1), var(i))),
            );
        }
        if cand.len() < MIN_BINS {
            return None;
        }
        let mut vars: Vec<f64> = cand.iter().map(|c| c.2).collect();
        let cut = 4.0 * median(&mut vars);
        let (ss, dof, kept) = cand
            .iter()
            .filter(|c| c.2 <= cut)
            .fold((0.0, 0.0, 0usize), |(s, d, k), c| (s + c.0, d + c.1, k + 1));
        (kept >= MIN_BINS && dof > 0.0).then(|| shape_from_db_variance(ss / dof) as f32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Rng(u64);
    impl Rng {
        fn unit(&mut self) -> f64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 11) as f64 / (1u64 << 53) as f64
        }
        fn gamma(&mut self, k: u32) -> f64 {
            (0..k).map(|_| -self.unit().max(1e-12).ln()).sum::<f64>() / f64::from(k)
        }
    }

    #[test]
    fn trigamma_matches_known_values_and_inverts() {
        assert!((trigamma(1.0) - std::f64::consts::PI.powi(2) / 6.0).abs() < 1e-9);
        assert!((trigamma(0.5) - std::f64::consts::PI.powi(2) / 2.0).abs() < 1e-9);
        for k in [0.3, 1.0, 2.5, 8.0, 64.0, 1000.0] {
            let v = DB_PER_NEPER2 * trigamma(k);
            assert!((shape_from_db_variance(v) / k - 1.0).abs() < 1e-6, "{k}");
        }
    }

    #[test]
    fn recovers_look_count_with_signals_present() {
        for k in [1u32, 2, 4, 16] {
            let mut rng = Rng(7 + u64::from(k));
            let mut est = NoiseShapeEstimator::new();
            let mut row = vec![0f32; 128];
            for _ in 0..16 {
                for lo in [0.0, 1e6, 2e6] {
                    for (i, v) in row.iter_mut().enumerate() {
                        // A sloped response across the step; every 20th bin a fluctuating signal.
                        let floor = -90.0 + 0.02 * i as f64;
                        let mut p = 10f64.powf(floor / 10.0) * rng.gamma(k);
                        if i % 20 == 3 {
                            p += 10f64.powf(-70.0 / 10.0) * rng.unit() * 2.0;
                        }
                        *v = (10.0 * p.log10()) as f32;
                    }
                    est.observe(lo, &row);
                }
            }
            let got = est.bin_shape().unwrap();
            eprintln!("T-126 shape estimate: injected k = {k}, estimated {got:.3}");
            assert!((got / k as f32 - 1.0).abs() < 0.05, "k {k}: {got}");
        }
        assert_eq!(NoiseShapeEstimator::new().bin_shape(), None);
    }
}
