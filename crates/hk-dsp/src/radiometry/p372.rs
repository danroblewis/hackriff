//! Optional comparison of a measured floor with ITU-R P.372 median external noise (docs/07 §5.1).
//!
//! `F_am(f) = c − d·log10(f / 1 MHz)`, dB above `kT₀b`, for a short vertical monopole over perfect
//! ground. **The coefficients and validity ranges below are UNVERIFIED** (recalled from P.372's
//! man-made noise table and galactic-noise line; check against the current P.372 before any
//! scientific use).
//!
//! A calibrated floor is a *system* noise factor at the calibration plane, `f_sys = f_a + f_rx − 1`
//! (linear, lossless antenna and feed). With a receiver noise figure, `f_a = f_sys − (f_rx − 1)`;
//! when the receiver term exceeds `f_a`, the reading says little about external noise
//! (`receiver_dominated`). The antenna factor, feed loss and pattern are not modelled.

/// A P.372 environment curve.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NoiseEnvironment {
    /// Business/city man-made noise.
    City,
    /// Residential man-made noise.
    Residential,
    /// Rural man-made noise.
    Rural,
    /// Quiet rural man-made noise.
    QuietRural,
    /// Galactic noise.
    Galactic,
}

impl NoiseEnvironment {
    /// `(c, d)` of `F_am = c − d·log10(f_MHz)`. UNVERIFIED.
    pub const fn coefficients(self) -> (f64, f64) {
        match self {
            NoiseEnvironment::City => (76.8, 27.7),
            NoiseEnvironment::Residential => (72.5, 27.7),
            NoiseEnvironment::Rural => (67.2, 27.7),
            NoiseEnvironment::QuietRural => (53.6, 28.6),
            NoiseEnvironment::Galactic => (52.0, 23.0),
        }
    }

    /// Frequencies the curve is given for, Hz. UNVERIFIED.
    pub const fn valid_hz(self) -> (f64, f64) {
        match self {
            NoiseEnvironment::Galactic => (10e6, 1e9),
            _ => (0.3e6, 250e6),
        }
    }

    /// Median `F_am` at `f_hz`, dB above `kT₀b`, inside the curve's range.
    pub fn median_fa_db(self, f_hz: f64) -> Option<f64> {
        let (lo, hi) = self.valid_hz();
        if !(f_hz >= lo && f_hz <= hi) {
            return None;
        }
        let (c, d) = self.coefficients();
        Some(c - d * (f_hz / 1e6).log10())
    }
}

/// A measured floor against a P.372 curve.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct P372Comparison {
    /// Curve.
    pub environment: NoiseEnvironment,
    /// Frequency, Hz.
    pub f_hz: f64,
    /// The curve's median `F_am`, dB.
    pub fa_model_db: f64,
    /// Measured system noise factor (dB above `kT₀`).
    pub system_f_db: f64,
    /// Estimated external noise factor `F_a`, dB (`None` when the receiver term exceeds the
    /// system reading).
    pub fa_measured_db: Option<f64>,
    /// `fa_measured − fa_model`, dB.
    pub excess_db: Option<f64>,
    /// The receiver contributes more noise than the external estimate.
    pub receiver_dominated: bool,
}

/// Compares a calibrated floor (dB above `kT₀`) at `f_hz` with `environment`, optionally
/// removing a receiver noise figure. `None` outside the curve's range.
pub fn compare_p372(
    db_above_kt0: f64,
    f_hz: f64,
    environment: NoiseEnvironment,
    receiver_nf_db: Option<f64>,
) -> Option<P372Comparison> {
    let fa_model_db = environment.median_fa_db(f_hz)?;
    let f_sys = 10f64.powf(db_above_kt0 / 10.0);
    let rx_excess = receiver_nf_db.map_or(0.0, |nf| 10f64.powf(nf / 10.0) - 1.0);
    let fa = f_sys - rx_excess;
    let fa_measured_db = (fa > 0.0).then(|| 10.0 * fa.log10());
    Some(P372Comparison {
        environment,
        f_hz,
        fa_model_db,
        system_f_db: db_above_kt0,
        fa_measured_db,
        excess_db: fa_measured_db.map(|m| m - fa_model_db),
        receiver_dominated: rx_excess > fa.max(0.0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curves_and_receiver_removal() {
        let fa = NoiseEnvironment::Residential.median_fa_db(10e6).unwrap();
        assert!((fa - (72.5 - 27.7)).abs() < 1e-12);
        assert_eq!(NoiseEnvironment::Rural.median_fa_db(1e9), None);
        // 30 dB system reading, 10 dB NF (receiver term 9 ≪ 1000): F_a ≈ 29.96 dB.
        let c = compare_p372(30.0, 10e6, NoiseEnvironment::Residential, Some(10.0)).unwrap();
        assert!((c.fa_measured_db.unwrap() - 10.0 * 991f64.log10()).abs() < 1e-9);
        assert!(!c.receiver_dominated);
        // A 5 dB reading with a 10 dB NF: receiver-dominated, no F_a.
        let r = compare_p372(5.0, 100e6, NoiseEnvironment::Galactic, Some(10.0)).unwrap();
        assert!(r.receiver_dominated && r.fa_measured_db.is_none());
    }
}
