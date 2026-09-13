//! Floor-referenced detection thresholds for T-006 (C09).
//!
//! The floor branch of the S4 detector declares a cell when `P > T·floor`, `T = Q⁻¹(n, pfa)/n`
//! (Gamma(10): 1e-3 → 3.55 dB, 1e-6 → 5.15 dB). The OS-CFAR branch additionally requires
//! `P > guard·floor` (default guard 3 dB; the acceptance range is 3–5 dB) so a clutter-edge or
//! low reference never declares cells within the floor's model uncertainty. The OS-CFAR `α`
//! (order statistics of `Gamma(n)` reference cells) is T-006's; it builds on [`super::gamma`].

use super::gamma;

/// Default guard margin above the floor, dB.
pub const DEFAULT_GUARD_DB: f64 = 3.0;

/// A floor-referenced threshold for one `(n_avg, pfa)` pair plus a guard margin.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FloorThreshold {
    /// Gamma shape (effective averages).
    pub n_avg: f64,
    /// Design per-cell false-alarm probability.
    pub pfa: f64,
    /// `T` (linear).
    pub multiplier: f64,
    /// Guard margin, dB.
    pub guard_db: f64,
}

impl FloorThreshold {
    /// `T` for `n_avg` and `pfa`, with the default 3 dB guard.
    pub fn new(n_avg: f64, pfa: f64) -> Self {
        assert!(n_avg > 0.0, "n_avg must be positive");
        assert!(pfa > 0.0 && pfa < 1.0, "pfa must be in (0, 1)");
        Self {
            n_avg,
            pfa,
            multiplier: gamma::mean_threshold(n_avg, pfa),
            guard_db: DEFAULT_GUARD_DB,
        }
    }

    /// Sets the guard margin.
    pub fn with_guard_db(mut self, guard_db: f64) -> Self {
        self.guard_db = guard_db;
        self
    }

    /// `T` in dB.
    pub fn multiplier_db(&self) -> f64 {
        10.0 * self.multiplier.log10()
    }

    /// The guard as a linear factor.
    pub fn guard(&self) -> f64 {
        10f64.powf(self.guard_db / 10.0)
    }

    /// The floor-branch factor actually applied: `max(T, guard)`.
    pub fn floor_branch_factor(&self) -> f64 {
        self.multiplier.max(self.guard())
    }

    /// The per-cell exceedance probability of the floor branch on noise with a known floor.
    pub fn design_pfa(&self) -> f64 {
        gamma::exceedance(self.n_avg, self.floor_branch_factor())
    }

    /// Floor-branch threshold for one floor value (linear).
    #[inline]
    pub fn level(&self, floor: f32) -> f32 {
        floor * self.floor_branch_factor() as f32
    }

    /// Guard level for one floor value (linear): the OS-CFAR branch's lower bound.
    #[inline]
    pub fn guard_level(&self, floor: f32) -> f32 {
        floor * self.guard() as f32
    }

    /// Writes the floor-branch threshold for every bin (allocation-free).
    pub fn write_levels(&self, floor: &[f32], out: &mut [f32]) {
        assert_eq!(floor.len(), out.len(), "floor/output length mismatch");
        let k = self.floor_branch_factor() as f32;
        for (o, &f) in out.iter_mut().zip(floor) {
            *o = f * k;
        }
    }
}

/// Energy-detection SNR wall for a floor uncertainty of `±uncertainty_db`: `(ρ² − 1)/ρ`,
/// `ρ = 10^(u/10)`, in dB. 1 dB → −3.3 dB; 0.5 dB → −6.4 dB.
pub fn snr_wall_db(uncertainty_db: f64) -> f64 {
    let rho = 10f64.powf(uncertainty_db / 10.0);
    10.0 * ((rho * rho - 1.0) / rho).log10()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s4_numbers() {
        let on = FloorThreshold::new(10.0, 1e-6);
        let off = FloorThreshold::new(10.0, 1e-3);
        assert!((on.multiplier_db() - 5.15).abs() < 0.02);
        assert!((off.multiplier_db() - 3.55).abs() < 0.02);
        assert!((on.design_pfa() / 1e-6 - 1.0).abs() < 1e-6);
        // A 5 dB guard dominates the 1e-3 multiplier.
        let g = off.with_guard_db(5.0);
        assert!((10.0 * g.floor_branch_factor().log10() - 5.0).abs() < 1e-9);
        assert!(g.design_pfa() < 1e-3);
        let mut out = [0.0f32; 2];
        on.write_levels(&[1.0, 2.0], &mut out);
        assert!((out[1] / 2.0 - on.multiplier as f32).abs() < 1e-6);
        assert!((on.guard_level(1.0) - 1.995).abs() < 1e-3);
        assert!((snr_wall_db(1.0) + 3.33).abs() < 0.01);
    }
}
