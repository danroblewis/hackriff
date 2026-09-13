//! The ADC quantisation floor, for the `quantisation_limited` flag.
//!
//! An ideal 8-bit converter with uniform rounding error adds `Δ²/12` per component, `Δ = 1/128`
//! in hk-core's ci8 normalisation (`x/128`, full scale 1). Complex: `2/(12·128²)` = −49.93 dBFS
//! over the sample-rate bandwidth, i.e. a white PSD of `−49.93 − 10·log10(fs)` dBFS/Hz
//! (−122.94 dBFS/Hz at 20 Msps). In PSD units (FS²/Hz, [`crate::spectrum`]) white noise reads the
//! same for any FFT length and window; per RBW it reads `+10·log10(ENBW·fs/N)`.
//!
//! S4 measured this HackRF One's floor at the lowest gain (LNA 8 / VGA 10) as **−120.3 dBFS/Hz**
//! at 20 Msps, 4096 bins, Hann (98 and 915 MHz alike): 2.64 dB above the ideal model (the ADC's
//! own noise and DNL, below ~8 effective bits). [`QuantisationFloor::HACKRF_ONE_S4`] is that
//! measurement scaled to other sample rates as white noise. The ADC-referred floor does not
//! depend on gain; a per-gain-state terminated-input measurement (C05) can replace it with
//! [`QuantisationFloor::DbfsPerHz`].

use crate::window::{Window, WindowKind};

/// Codes per unit full scale in hk-core's ci8 normalisation.
pub const CI8_CODES_PER_FULL_SCALE: f64 = 128.0;

/// S4: measured HackRF One low-gain floor, dBFS/Hz at 20 Msps.
pub const HACKRF_ONE_S4_FLOOR_DBFS_PER_HZ_20MSPS: f64 = -120.3;

/// S4's measured floor minus the ideal ci8 model at 20 Msps, dB.
pub const HACKRF_ONE_S4_EXCESS_DB: f64 = 2.636;

/// Ideal ci8 quantisation-noise power, FS² (complex, both rails).
pub fn ci8_quantisation_noise_power() -> f64 {
    2.0 / (12.0 * CI8_CODES_PER_FULL_SCALE * CI8_CODES_PER_FULL_SCALE)
}

/// Quantisation noise in the three units of [`crate::spectrum`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct QuantisationNoise {
    /// Total over the sample-rate bandwidth, dBFS.
    pub dbfs: f64,
    /// PSD, dBFS/Hz.
    pub dbfs_per_hz: f64,
    /// Per RBW (`ENBW·fs/N`), dBFS.
    pub dbfs_per_bin: f64,
}

/// The ideal ci8 quantisation noise for sample rate `fs`, `fft_len` bins and `window`, plus
/// `excess_db`. Allocates a window (for ENBW): not for the per-frame path.
pub fn ci8_quantisation_noise(
    sample_rate_hz: f64,
    fft_len: usize,
    window: WindowKind,
    excess_db: f64,
) -> QuantisationNoise {
    let dbfs = 10.0 * ci8_quantisation_noise_power().log10() + excess_db;
    let dbfs_per_hz = dbfs - 10.0 * sample_rate_hz.log10();
    let enbw = Window::new(window, fft_len.max(2)).metrics().enbw_bins;
    QuantisationNoise {
        dbfs,
        dbfs_per_hz,
        dbfs_per_bin: dbfs_per_hz + 10.0 * (enbw * sample_rate_hz / fft_len as f64).log10(),
    }
}

/// Which quantisation floor the `quantisation_limited` flag compares against.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum QuantisationFloor {
    /// No flag.
    None,
    /// Ideal ci8 uniform-error noise plus `excess_db`, scaled with the sample rate.
    Ci8 {
        /// dB above the ideal model.
        excess_db: f64,
    },
    /// A fixed PSD, dBFS/Hz (e.g. a terminated-input measurement for this gain/rate).
    DbfsPerHz(f64),
}

impl QuantisationFloor {
    /// The ideal 8-bit model.
    pub const CI8_IDEAL: Self = QuantisationFloor::Ci8 { excess_db: 0.0 };
    /// S4's measured HackRF One floor (−120.3 dBFS/Hz at 20 Msps). The default.
    pub const HACKRF_ONE_S4: Self = QuantisationFloor::Ci8 {
        excess_db: HACKRF_ONE_S4_EXCESS_DB,
    };

    /// The floor PSD at `sample_rate_hz`, dBFS/Hz.
    pub fn dbfs_per_hz(&self, sample_rate_hz: f64) -> Option<f64> {
        match *self {
            QuantisationFloor::None => None,
            QuantisationFloor::Ci8 { excess_db } => Some(
                10.0 * ci8_quantisation_noise_power().log10() + excess_db
                    - 10.0 * sample_rate_hz.log10(),
            ),
            QuantisationFloor::DbfsPerHz(v) => Some(v),
        }
    }
}

impl Default for QuantisationFloor {
    fn default() -> Self {
        Self::HACKRF_ONE_S4
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ideal_and_s4_levels() {
        let q = ci8_quantisation_noise(20e6, 4096, WindowKind::Hann, 0.0);
        assert!((q.dbfs + 49.93).abs() < 0.01);
        assert!((q.dbfs_per_hz + 122.94).abs() < 0.01);
        // Per RBW: + 10·log10(1.5 · 20e6 / 4096).
        assert!(
            (q.dbfs_per_bin - (q.dbfs_per_hz + 10.0 * (1.5f64 * 20e6 / 4096.0).log10())).abs()
                < 1e-3
        );
        let s4 = QuantisationFloor::HACKRF_ONE_S4.dbfs_per_hz(20e6).unwrap();
        assert!(
            (s4 - HACKRF_ONE_S4_FLOOR_DBFS_PER_HZ_20MSPS).abs() < 0.01,
            "{s4}"
        );
        let at_2m = QuantisationFloor::HACKRF_ONE_S4.dbfs_per_hz(2e6).unwrap();
        assert!((at_2m - (s4 + 10.0)).abs() < 1e-9);
        assert_eq!(QuantisationFloor::None.dbfs_per_hz(1e6), None);
    }
}
