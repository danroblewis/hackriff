//! Per-frame cell classification: OS-CFAR across frequency OR the floor branch, with separate
//! seed (on) and extend (off) thresholds (S4 §2.3).
//!
//! ```text
//! g      = P > guard · F                                   (OS branch only)
//! seed   = (g ∧ P > α_on ·Z) ∨ P > T_on ·F
//! region = (g ∧ P > α_off·Z) ∨ P > T_off·F ∨ seed
//! ```
//!
//! `F` is the floor reference ([`FloorReference`](crate::FloorReference)), `Z` the `k`-th smallest
//! of the `N` reference cells at offsets `±(G+1)…±(G+N/2)` (mirrored at the band edges, like
//! S4's `rank_filter(mode="mirror")`), `T = Q⁻¹(n, pfa)/n` and `α` from [`crate::alpha`].
//!
//! The OS tests need no sort: `P > α·Z` exactly when at least `k` reference cells are below
//! `P/α` (`Z` is the `k`-th smallest), so each cell that passes the guard costs one branch-free
//! count over its 32 reference cells, for both thresholds at once. On noise ≈ 0.5 % of cells pass
//! the guard at `n = 10`. [`CfarEngine::order_statistic`] and
//! [`CfarEngine::sliding_order_statistics`] compute `Z` itself for diagnostics.

use hk_dsp::floor::gamma;

use crate::alpha::os_cfar_alpha;
use crate::config::{Branches, CfarWindow, DetectionProfile, Hysteresis};

/// Not detected.
pub const CELL_NONE: u8 = 0;
/// Above an extend threshold.
pub const CELL_REGION: u8 = 1;
/// Above a seed threshold.
pub const CELL_SEED: u8 = 2;

/// Thresholds for one `(n, profile, window, guard)`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Thresholds {
    /// Gamma shape (effective averages).
    pub n_avg: f64,
    /// Seed per-cell probability.
    pub pfa_on: f64,
    /// Extend per-cell probability (`NaN` for the fixed-dB regression mode).
    pub pfa_off: f64,
    /// OS seed scale (linear).
    pub alpha_on: f64,
    /// OS extend scale (linear).
    pub alpha_off: f64,
    /// Floor-branch seed multiplier (linear).
    pub t_on: f64,
    /// Floor-branch extend multiplier (linear).
    pub t_off: f64,
    /// OS guard multiplier (linear).
    pub guard: f64,
}

impl Thresholds {
    /// Computes the thresholds (α numerically, cached).
    pub fn new(n_avg: f64, window: &CfarWindow, profile: &DetectionProfile, guard_db: f64) -> Self {
        let n_ref = window.reference_cells();
        let alpha_on = os_cfar_alpha(n_avg, n_ref, window.rank, profile.pfa_on);
        let t_on = gamma::mean_threshold(n_avg, profile.pfa_on);
        let (pfa_off, alpha_off, t_off) = match profile.hysteresis {
            Hysteresis::Pfa(p) => (
                p,
                os_cfar_alpha(n_avg, n_ref, window.rank, p),
                gamma::mean_threshold(n_avg, p),
            ),
            Hysteresis::FixedDb(d) => {
                let h = 10f64.powf(-d / 10.0);
                (f64::NAN, alpha_on * h, t_on * h)
            }
        };
        Self {
            n_avg,
            pfa_on: profile.pfa_on,
            pfa_off,
            alpha_on,
            alpha_off,
            t_on,
            t_off,
            guard: 10f64.powf(guard_db / 10.0),
        }
    }

    /// `(α_on, α_off, T_on, T_off, guard)` in dB.
    pub fn db(&self) -> [f64; 5] {
        [
            self.alpha_on,
            self.alpha_off,
            self.t_on,
            self.t_off,
            self.guard,
        ]
        .map(crate::alpha::db)
    }
}

/// Counts from one classification.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClassifyStats {
    /// Seed cells.
    pub seeds: usize,
    /// Region cells (including seeds).
    pub region: usize,
    /// Cells whose OS test ran (above the guard and not already a floor-branch seed).
    pub guard_passed: usize,
}

/// OS-CFAR order statistics and classification; buffers sized once per resolution.
#[derive(Clone, Debug)]
pub struct CfarEngine {
    window: CfarWindow,
    sorted: Vec<f32>,
    scratch: Vec<f32>,
    work: Vec<f32>,
}

impl CfarEngine {
    /// An engine for `bins` bins.
    pub fn new(window: CfarWindow, bins: usize) -> Self {
        let n = window.reference_cells();
        Self {
            window,
            sorted: Vec::with_capacity(n + 1),
            scratch: vec![0.0; n],
            work: Vec::with_capacity(bins),
        }
    }

    /// Resizes for `bins` bins (the classifier itself needs no per-bin buffer).
    pub fn resize(&mut self, _bins: usize) {}

    /// Mirror index (`d c b | a b c d`), repeated for spans shorter than the window.
    #[inline]
    fn mirror(j: isize, n: usize) -> usize {
        let last = n as isize - 1;
        let mut j = j;
        loop {
            if j < 0 {
                j = -j;
            } else if j > last {
                j = 2 * last - j;
            } else {
                return j as usize;
            }
            if last == 0 {
                return 0;
            }
        }
    }

    /// Copies the (mirrored) reference cells of bin `i` into `scratch`.
    fn gather(&mut self, psd: &[f32], i: usize) {
        let n = psd.len();
        let g = self.window.guard_per_side as isize;
        let r = self.window.reference_per_side as isize;
        let ii = i as isize;
        let mut w = 0;
        for d in (g + 1)..=(g + r) {
            self.scratch[w] = psd[Self::mirror(ii - d, n)];
            self.scratch[w + 1] = psd[Self::mirror(ii + d, n)];
            w += 2;
        }
    }

    /// `Z` for one cell: the `rank`-th smallest reference value (diagnostics and tests; the
    /// classifier counts instead).
    pub fn order_statistic(&mut self, psd: &[f32], i: usize) -> f32 {
        self.gather(psd, i);
        let k = self.window.rank - 1;
        let (_, &mut z, _) = self.scratch.select_nth_unstable_by(k, f32::total_cmp);
        z
    }

    /// Reference cells below `lim_on` and below `lim_off` (branch-free; NaN counts as above).
    #[inline]
    fn count_below(a: &[f32], b: &[f32], lim_on: f32, lim_off: f32) -> (usize, usize) {
        let (mut on, mut off) = (0u32, 0u32);
        for &x in a {
            on += u32::from(x < lim_on);
            off += u32::from(x < lim_off);
        }
        for &x in b {
            on += u32::from(x < lim_on);
            off += u32::from(x < lim_off);
        }
        (on as usize, off as usize)
    }

    /// `Z` for every bin into `out` with a sliding sorted window. NaN cells sort as +∞ (as the
    /// per-cell `total_cmp` selection places them above every number).
    pub fn sliding_order_statistics(&mut self, psd: &[f32], out: &mut [f32]) {
        let n = psd.len();
        assert_eq!(out.len(), n);
        let Self {
            window,
            sorted,
            work,
            ..
        } = self;
        work.clear();
        work.extend(
            psd.iter()
                .map(|&v| if v.is_nan() { f32::INFINITY } else { v }),
        );
        let g = window.guard_per_side as isize;
        let r = window.reference_per_side as isize;
        let reach = g + r;
        let k = window.rank - 1;
        let val = |j: isize| work[Self::mirror(j, n)];
        sorted.clear();
        for d in (g + 1)..=reach {
            sorted.push(val(-d));
            sorted.push(val(d));
        }
        sorted.sort_unstable_by(f32::total_cmp);
        for i in 0..n as isize {
            out[i as usize] = sorted[k];
            if i + 1 < n as isize {
                // Left window [i−reach, i−g−1] → [i+1−reach, i−g]; right [i+g+1, i+reach] →
                // [i+g+2, i+1+reach].
                Self::replace(sorted, val(i - reach), val(i - g));
                Self::replace(sorted, val(i + g + 1), val(i + 1 + reach));
            }
        }
    }

    /// Replaces one `old` with `new`, keeping `sorted` ordered, with one shift (no allocation,
    /// no NaN: the caller maps NaN to +∞).
    #[inline]
    fn replace(sorted: &mut [f32], old: f32, new: f32) {
        let p_old = sorted.partition_point(|&x| x < old);
        debug_assert!(p_old < sorted.len() && sorted[p_old] == old);
        let p_new = sorted.partition_point(|&x| x < new);
        if p_new > p_old {
            sorted.copy_within(p_old + 1..p_new, p_old);
            sorted[p_new - 1] = new;
        } else {
            sorted.copy_within(p_new..p_old, p_new + 1);
            sorted[p_new] = new;
        }
    }

    /// Classifies every cell of `psd` against `floor` into `codes` ([`CELL_NONE`],
    /// [`CELL_REGION`], [`CELL_SEED`]).
    pub fn classify(
        &mut self,
        psd: &[f32],
        floor: &[f32],
        th: &Thresholds,
        branches: Branches,
        floor_ok: Option<&[bool]>,
        codes: &mut [u8],
    ) -> ClassifyStats {
        if let Some(m) = floor_ok {
            assert_eq!(m.len(), psd.len(), "psd/floor-branch mask length mismatch");
        }
        let n = psd.len();
        assert_eq!(floor.len(), n, "psd/floor length mismatch");
        assert_eq!(codes.len(), n, "psd/codes length mismatch");
        let guard = th.guard as f32;
        let (a_on, a_off) = (th.alpha_on as f32, th.alpha_off as f32);
        let (t_on, t_off) = (th.t_on as f32, th.t_off as f32);
        let use_os = branches != Branches::FloorOnly;
        let use_floor = branches != Branches::OsOnly;
        let k = self.window.rank;
        let g = self.window.guard_per_side;
        let reach = g + self.window.reference_per_side;
        let mut stats = ClassifyStats::default();
        for i in 0..n {
            let p = psd[i];
            let f = floor[i];
            let mut code = CELL_NONE;
            if p.is_finite() && f > 0.0 && f.is_finite() {
                let (mut seed, mut region) = (false, false);
                if use_floor && floor_ok.is_none_or(|m| m[i]) {
                    seed = p > t_on * f;
                    region = p > t_off * f;
                }
                if use_os && !seed && p > guard * f {
                    stats.guard_passed += 1;
                    // P > α·Z ⇔ Z < P/α ⇔ at least k reference cells are below P/α.
                    let (lim_on, lim_off) = (p / a_on, p / a_off);
                    let (on, off) = if i >= reach && i + reach < n {
                        Self::count_below(
                            &psd[i - reach..i - g],
                            &psd[i + g + 1..=i + reach],
                            lim_on,
                            lim_off,
                        )
                    } else {
                        self.gather(psd, i);
                        Self::count_below(&self.scratch, &[], lim_on, lim_off)
                    };
                    seed = on >= k;
                    region |= off >= k;
                }
                if seed {
                    code = CELL_SEED;
                    stats.seeds += 1;
                    stats.region += 1;
                } else if region {
                    code = CELL_REGION;
                    stats.region += 1;
                }
            }
            codes[i] = code;
        }
        stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hk_dsp::synth::Rng;

    #[test]
    fn counting_classifier_matches_the_order_statistic_rule() {
        let mut rng = Rng::new(11);
        let n = 600;
        let floor = vec![1.0f32; n];
        let th = Thresholds::new(
            10.0,
            &CfarWindow::default(),
            &DetectionProfile::standard(),
            3.0,
        );
        let mut e = CfarEngine::new(CfarWindow::default(), n);
        let mut codes = vec![0u8; n];
        for frame in 0..20 {
            let psd: Vec<f32> = (0..n)
                .map(|b| {
                    let noise = -(rng.unit().ln()) as f32 * 1.2;
                    if (b + 7 * frame) % 50 < 6 {
                        noise * 8.0
                    } else {
                        noise
                    }
                })
                .collect();
            for branches in [Branches::Or, Branches::OsOnly] {
                e.classify(&psd, &floor, &th, branches, None, &mut codes);
                for i in 0..n {
                    let p = psd[i];
                    let floor_seed = branches == Branches::Or && p > th.t_on as f32;
                    let floor_region = branches == Branches::Or && p > th.t_off as f32;
                    let (os_seed, os_region) = if !floor_seed && p > th.guard as f32 {
                        let z = e.order_statistic(&psd, i);
                        (p > th.alpha_on as f32 * z, p > th.alpha_off as f32 * z)
                    } else {
                        (false, false)
                    };
                    let want = if floor_seed || os_seed {
                        CELL_SEED
                    } else if floor_region || os_region {
                        CELL_REGION
                    } else {
                        CELL_NONE
                    };
                    assert_eq!(codes[i], want, "frame {frame} bin {i} {branches:?}");
                }
            }
        }
    }

    #[test]
    fn sliding_and_per_cell_order_statistics_agree() {
        let mut rng = Rng::new(7);
        let psd: Vec<f32> = (0..300).map(|_| rng.unit() as f32).collect();
        let mut e = CfarEngine::new(CfarWindow::default(), psd.len());
        let mut z = vec![0.0; psd.len()];
        e.sliding_order_statistics(&psd, &mut z);
        for (i, &zi) in z.iter().enumerate() {
            assert_eq!(zi, e.order_statistic(&psd, i), "bin {i}");
        }
    }

    #[test]
    fn thresholds_match_s4() {
        let t = Thresholds::new(
            10.0,
            &CfarWindow::default(),
            &DetectionProfile::standard(),
            3.0,
        );
        let [a_on, a_off, t_on, t_off, g] = t.db();
        assert!((a_on - 4.70).abs() <= 0.02 && (a_off - 3.00).abs() <= 0.02);
        assert!((t_on - 5.15).abs() <= 0.02 && (t_off - 3.55).abs() <= 0.02);
        assert!((g - 3.0).abs() < 1e-9);
        let mut p = DetectionProfile::standard();
        p.hysteresis = Hysteresis::FixedDb(3.0);
        let f = Thresholds::new(10.0, &CfarWindow::default(), &p, 3.0);
        assert!((crate::alpha::db(f.t_off) - (t_on - 3.0)).abs() < 1e-9);
    }
}
