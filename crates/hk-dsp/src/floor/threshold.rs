//! Floor-referenced detection thresholds for T-006 (C09).
//!
//! S4's detector ORs two branches, with hysteresis (seed at `pfa_on`, extend at `pfa_off`):
//! - **Floor branch:** `P > T·floor`, `T = Q⁻¹(n, pfa)/n` (Gamma(10): on 1e-6 → 5.15 dB, off
//!   1e-3 → 3.55 dB). No guard is applied to this branch: `T` already is the design threshold.
//! - **OS-CFAR branch:** `P > α·Z_(k)` **and** `P > guard·floor` (default guard 3 dB, acceptance
//!   range 3–5 dB), so a low order statistic never declares cells within the floor's model
//!   uncertainty. The OS `α` (order statistics of `Gamma(n)` reference cells) is T-006's; it
//!   builds on [`super::gamma`].
//!
//! **Which floor.** Inside a flat signal wider than about one block (≳ 200 of 256 bins) the
//! per-frame block floor ([`FloorKind::Frame`](super::FloorKind)) reads the signal itself (the
//! review measured +10.4/+20.0 dB inside 1024- and 2048-bin signals at +10/+20 dB), so the floor
//! branch fires on only 9–18 % of interior bins and the guard lets only 11–21 % through: both
//! branches lose the interior. Use the wide-signal reference
//! [`FloorKind::Wide`](super::FloorKind) for both the floor branch and the guard. It equals the
//! per-frame floor except inside step-like elevated regions (a rise of more than 3.5 dB over a
//! slope-limited envelope), where it reads the surrounding floor; both are shaped by the learned
//! response, so tilts, baseband roll-off and notches keep the floor-branch Pfa within 1.5× design
//! (`tests/floor_wide_reference.rs`).
//!
//! SNR wall ([`snr_wall_db`]): ±0.5 dB floor uncertainty → −6.4 dB; ±1 dB → −3.3 dB. (S4 §5's
//! "near −9 dB" corresponds to ±0.25 dB.)

use super::gamma;

/// Default guard margin above the floor for the OS-CFAR branch, dB.
pub const DEFAULT_GUARD_DB: f64 = 3.0;

/// Floor-branch thresholds for one `n_avg` with on (seed) and off (extend) false-alarm
/// probabilities, plus the OS-branch guard.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FloorThreshold {
    /// Gamma shape (effective averages).
    pub n_avg: f64,
    /// Seed per-cell false-alarm probability.
    pub pfa_on: f64,
    /// Extend (hysteresis-off) per-cell false-alarm probability.
    pub pfa_off: f64,
    /// `T_on` (linear).
    pub on: f64,
    /// `T_off` (linear).
    pub off: f64,
    /// OS-branch guard margin, dB.
    pub guard_db: f64,
}

impl FloorThreshold {
    /// `T_on`, `T_off` for `n_avg`, with the default 3 dB guard.
    pub fn new(n_avg: f64, pfa_on: f64, pfa_off: f64) -> Self {
        assert!(n_avg > 0.0, "n_avg must be positive");
        for p in [pfa_on, pfa_off] {
            assert!(p > 0.0 && p < 1.0, "pfa must be in (0, 1)");
        }
        Self {
            n_avg,
            pfa_on,
            pfa_off,
            on: gamma::mean_threshold(n_avg, pfa_on),
            off: gamma::mean_threshold(n_avg, pfa_off),
            guard_db: DEFAULT_GUARD_DB,
        }
    }

    /// S4's recommended profile: on 1e-6, off 1e-3.
    pub fn s4(n_avg: f64) -> Self {
        Self::new(n_avg, 1e-6, 1e-3)
    }

    /// One probability for both on and off.
    pub fn single(n_avg: f64, pfa: f64) -> Self {
        Self::new(n_avg, pfa, pfa)
    }

    /// Sets the OS-branch guard margin.
    pub fn with_guard_db(mut self, guard_db: f64) -> Self {
        self.guard_db = guard_db;
        self
    }

    /// `T_on` in dB.
    pub fn on_db(&self) -> f64 {
        10.0 * self.on.log10()
    }

    /// `T_off` in dB.
    pub fn off_db(&self) -> f64 {
        10.0 * self.off.log10()
    }

    /// The guard as a linear factor.
    pub fn guard(&self) -> f64 {
        10f64.powf(self.guard_db / 10.0)
    }

    /// Per-cell exceedance of `T_on` on noise with a known floor (= `pfa_on`).
    pub fn design_pfa_on(&self) -> f64 {
        gamma::exceedance(self.n_avg, self.on)
    }

    /// Per-cell exceedance of `T_off` on noise with a known floor (= `pfa_off`).
    pub fn design_pfa_off(&self) -> f64 {
        gamma::exceedance(self.n_avg, self.off)
    }

    /// Per-cell exceedance of the guard alone on noise.
    pub fn guard_exceedance(&self) -> f64 {
        gamma::exceedance(self.n_avg, self.guard())
    }

    /// Floor-branch seed level for one floor value (linear).
    #[inline]
    pub fn level_on(&self, floor: f32) -> f32 {
        floor * self.on as f32
    }

    /// Floor-branch extend level for one floor value (linear).
    #[inline]
    pub fn level_off(&self, floor: f32) -> f32 {
        floor * self.off as f32
    }

    /// OS-branch guard level for one floor value (linear).
    #[inline]
    pub fn guard_level(&self, floor: f32) -> f32 {
        floor * self.guard() as f32
    }

    /// Writes `floor · factor` for every bin (allocation-free).
    fn write(floor: &[f32], factor: f64, out: &mut [f32]) {
        assert_eq!(floor.len(), out.len(), "floor/output length mismatch");
        let k = factor as f32;
        for (o, &f) in out.iter_mut().zip(floor) {
            *o = f * k;
        }
    }

    /// Floor-branch seed levels for every bin.
    pub fn write_on_levels(&self, floor: &[f32], out: &mut [f32]) {
        Self::write(floor, self.on, out);
    }

    /// Floor-branch extend levels for every bin.
    pub fn write_off_levels(&self, floor: &[f32], out: &mut [f32]) {
        Self::write(floor, self.off, out);
    }

    /// OS-branch guard levels for every bin.
    pub fn write_guard_levels(&self, floor: &[f32], out: &mut [f32]) {
        Self::write(floor, self.guard(), out);
    }
}

/// Energy-detection SNR wall for a floor uncertainty of `±uncertainty_db`: `(ρ² − 1)/ρ`,
/// `ρ = 10^(u/10)`, in dB. ±1 dB → −3.3 dB; ±0.5 dB → −6.4 dB; ±0.25 dB → −9.4 dB.
pub fn snr_wall_db(uncertainty_db: f64) -> f64 {
    let rho = 10f64.powf(uncertainty_db / 10.0);
    10.0 * ((rho * rho - 1.0) / rho).log10()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s4_numbers_and_separate_guard() {
        let t = FloorThreshold::s4(10.0);
        assert!((t.on_db() - 5.1469).abs() < 1e-3);
        assert!((t.off_db() - 3.5521).abs() < 1e-3);
        assert!((t.design_pfa_on() / 1e-6 - 1.0).abs() < 1e-6);
        assert!((t.design_pfa_off() / 1e-3 - 1.0).abs() < 1e-6);
        // A 5 dB guard does not touch the floor branch.
        let g = t.with_guard_db(5.0);
        assert_eq!((g.on, g.off), (t.on, t.off));
        assert!((g.guard_level(1.0) - 3.162).abs() < 1e-3);
        let mut out = [0.0f32; 2];
        g.write_off_levels(&[1.0, 2.0], &mut out);
        assert!((out[1] / 2.0 - t.off as f32).abs() < 1e-6);
        g.write_guard_levels(&[1.0, 2.0], &mut out);
        assert!((out[0] - 3.162).abs() < 1e-3);
        assert_eq!(
            FloorThreshold::single(10.0, 1e-4).on,
            FloorThreshold::single(10.0, 1e-4).off
        );
        assert!((snr_wall_db(1.0) + 3.33).abs() < 0.01);
        assert!((snr_wall_db(0.5) + 6.37).abs() < 0.01);
    }
}
