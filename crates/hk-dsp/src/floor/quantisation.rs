//! The receiver's low-gain floor, for the `quantisation_limited` flag.
//!
//! An ideal 8-bit converter with uniform rounding error adds `Δ²/12` per component, `Δ = 1/128`
//! in hk-core's ci8 normalisation (`x/128`, full scale 1). Complex: `2/(12·128²)` = −49.93 dBFS
//! over the sample-rate bandwidth, i.e. a white PSD of `−49.93 − 10·log10(fs)` dBFS/Hz
//! (−122.94 dBFS/Hz at 20 Msps). In PSD units (FS²/Hz, [`crate::spectrum`]) white noise reads the
//! same for any FFT length and window; per RBW it reads `+10·log10(ENBW·fs/N)`.
//!
//! S4 measured this HackRF One's floor at the lowest gain it captured (LNA 8 / VGA 10) as
//! **−120.3 dBFS/Hz** at 20 Msps, 4096 bins, Hann (98 and 915 MHz alike), 2.64 dB above the
//! ideal model. The excess is most likely analog noise at that gain (or ADC noise/DNL), not
//! quantisation, so it should **not** scale with `1/fs` like the quantisation term.
//! [`QuantisationFloor::HACKRF_ONE_S4`] therefore models the floor as the ideal ci8 term (scaling
//! with `fs`) plus a constant excess PSD of −123.72 dBFS/Hz (−120.3 dBFS/Hz at 20 Msps,
//! −112.59 at 2 Msps, −103.24 at 200 ksps). **Unverified away from 20 Msps:** a terminated-input
//! measurement per gain state at 2, 8 and 20 Msps is pending (needs the user and the hardware);
//! until then [`QuantisationFloor::DbfsPerHz`] can pin a measured value.

use crate::window::{Window, WindowKind};

/// Codes per unit full scale in hk-core's ci8 normalisation.
pub const CI8_CODES_PER_FULL_SCALE: f64 = 128.0;

/// S4: measured HackRF One low-gain floor, dBFS/Hz at 20 Msps.
pub const HACKRF_ONE_S4_FLOOR_DBFS_PER_HZ_20MSPS: f64 = -120.3;

/// S4's measured 20 Msps floor minus the ideal ci8 PSD, as a constant PSD, dBFS/Hz
/// (`10·log10(10^−12.03 − 10^−12.2935)`). Unverified at other rates.
pub const HACKRF_ONE_S4_EXCESS_DBFS_PER_HZ: f64 = -123.718;

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

/// Which floor the `quantisation_limited` flag compares against.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum QuantisationFloor {
    /// No flag.
    None,
    /// Ideal ci8 uniform-error noise plus `excess_db`, all scaling with the sample rate.
    Ci8 {
        /// dB above the ideal model.
        excess_db: f64,
    },
    /// Ideal ci8 noise (scaling with `fs`) plus a constant excess PSD (not scaling with `fs`).
    Ci8PlusPsd {
        /// The constant part, dBFS/Hz.
        excess_dbfs_per_hz: f64,
    },
    /// A fixed PSD, dBFS/Hz (e.g. a terminated-input measurement for this gain/rate).
    DbfsPerHz(f64),
}

impl QuantisationFloor {
    /// The ideal 8-bit model.
    pub const CI8_IDEAL: Self = QuantisationFloor::Ci8 { excess_db: 0.0 };
    /// S4's HackRF One floor: exact at 20 Msps, unverified elsewhere (see the module docs). The
    /// default.
    pub const HACKRF_ONE_S4: Self = QuantisationFloor::Ci8PlusPsd {
        excess_dbfs_per_hz: HACKRF_ONE_S4_EXCESS_DBFS_PER_HZ,
    };

    /// The floor PSD at `sample_rate_hz`, dBFS/Hz.
    pub fn dbfs_per_hz(&self, sample_rate_hz: f64) -> Option<f64> {
        let ideal = || 10.0 * (ci8_quantisation_noise_power() / sample_rate_hz).log10();
        match *self {
            QuantisationFloor::None => None,
            QuantisationFloor::Ci8 { excess_db } => Some(ideal() + excess_db),
            QuantisationFloor::Ci8PlusPsd { excess_dbfs_per_hz } => Some(
                10.0 * (10f64.powf(ideal() / 10.0) + 10f64.powf(excess_dbfs_per_hz / 10.0)).log10(),
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
        assert!(
            (q.dbfs_per_bin - (q.dbfs_per_hz + 10.0 * (1.5f64 * 20e6 / 4096.0).log10())).abs()
                < 1e-3
        );
        let s4 = QuantisationFloor::HACKRF_ONE_S4.dbfs_per_hz(20e6).unwrap();
        assert!(
            (s4 - HACKRF_ONE_S4_FLOOR_DBFS_PER_HZ_20MSPS).abs() < 0.005,
            "{s4}"
        );
        // The excess does not scale with 1/fs.
        let at_2m = QuantisationFloor::HACKRF_ONE_S4.dbfs_per_hz(2e6).unwrap();
        assert!((at_2m + 112.59).abs() < 0.01, "{at_2m}");
        let ideal_2m = QuantisationFloor::CI8_IDEAL.dbfs_per_hz(2e6).unwrap();
        assert!((ideal_2m + 112.94).abs() < 0.01);
        assert_eq!(QuantisationFloor::None.dbfs_per_hz(1e6), None);
    }
}

// ---- ADC fill (T-625, ADR-0015 §13.3) -------------------------------------------------------

/// The measured ADC fill of one evaluation window: σ in LSB, and how much of it clipped.
///
/// [`hk_model::FillBucket::classify`] turns this into the bucket that decides whether a
/// calibration table exists for the window at all.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AdcFill {
    /// Per-component noise standard deviation, in ADC LSB. `None` for an empty window — and
    /// "not measured" is [`hk_model::FillBucket::UnderFilled`], never nominal.
    pub sigma_lsb: Option<f32>,
    /// Fraction of samples with either component at full scale.
    pub clip_fraction: f64,
    /// Samples examined.
    pub samples: usize,
}

/// Per-component σ in LSB and the clip fraction of an interleaved `ci8` window (T-625).
///
/// The cheap runtime rule of ADR-0015 §13.3: one pass over samples already in cache, once per
/// evaluation window and not per metric. σ is the **pooled** per-component deviation,
/// `sqrt((var(re) + var(im)) / 2)`, each component taken about its own mean so a DC offset does
/// not read as fill. The integer samples *are* the LSB units, so nothing here needs a scale
/// constant — which is the point of measuring before the normalisation rather than after it.
///
/// It measures the window it is given: on a window containing a strong emission this is the
/// signal-plus-noise deviation, which is why the caller picks a window it believes is noise (a
/// null window, a guard band) exactly as the calibration tables are built on one.
pub fn adc_fill_ci8(interleaved: &[i8]) -> AdcFill {
    let pairs = interleaved.len() / 2;
    if pairs == 0 {
        return AdcFill {
            sigma_lsb: None,
            clip_fraction: 0.0,
            samples: 0,
        };
    }
    let (mut sum_i, mut sum_q) = (0f64, 0f64);
    let (mut sq_i, mut sq_q) = (0f64, 0f64);
    let mut clipped = 0usize;
    for c in interleaved[..pairs * 2].chunks_exact(2) {
        let (i, q) = (f64::from(c[0]), f64::from(c[1]));
        sum_i += i;
        sum_q += q;
        sq_i += i * i;
        sq_q += q * q;
        if c[0] == i8::MIN || c[0] == i8::MAX || c[1] == i8::MIN || c[1] == i8::MAX {
            clipped += 1;
        }
    }
    let n = pairs as f64;
    let var_i = (sq_i / n - (sum_i / n).powi(2)).max(0.0);
    let var_q = (sq_q / n - (sum_q / n).powi(2)).max(0.0);
    AdcFill {
        sigma_lsb: Some((0.5 * (var_i + var_q)).sqrt() as f32),
        clip_fraction: clipped as f64 / n,
        samples: pairs,
    }
}

/// The same measurement on samples already normalised to full scale ±1 (hk-core's `x/128`).
///
/// Prefer [`adc_fill_ci8`] where the integer samples still exist: this form re-derives LSB from
/// [`CI8_CODES_PER_FULL_SCALE`] and cannot see the difference between a sample that clipped and
/// one that merely reached full scale.
pub fn adc_fill_unit_scale(iq: &[num_complex::Complex<f32>]) -> AdcFill {
    if iq.is_empty() {
        return AdcFill {
            sigma_lsb: None,
            clip_fraction: 0.0,
            samples: 0,
        };
    }
    let (mut sum_i, mut sum_q, mut sq_i, mut sq_q) = (0f64, 0f64, 0f64, 0f64);
    let mut clipped = 0usize;
    for s in iq {
        let (i, q) = (f64::from(s.re), f64::from(s.im));
        sum_i += i;
        sum_q += q;
        sq_i += i * i;
        sq_q += q * q;
        if i.abs() >= 1.0 || q.abs() >= 1.0 {
            clipped += 1;
        }
    }
    let n = iq.len() as f64;
    let var_i = (sq_i / n - (sum_i / n).powi(2)).max(0.0);
    let var_q = (sq_q / n - (sum_q / n).powi(2)).max(0.0);
    AdcFill {
        sigma_lsb: Some(((0.5 * (var_i + var_q)).sqrt() * CI8_CODES_PER_FULL_SCALE) as f32),
        clip_fraction: clipped as f64 / n,
        samples: iq.len(),
    }
}

#[cfg(test)]
mod adc_fill_tests {
    use super::*;
    use hk_model::{FillBucket, UNDER_FILL_SIGMA_LSB};

    /// Deterministic Gaussian-ish noise at a target σ in LSB, quantised as the ADC would.
    fn noise_ci8(sigma_lsb: f64, n: usize) -> Vec<i8> {
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut next = || {
            // xorshift64* → uniform, then Irwin–Hall(12) → σ = 1 Gaussian approximation.
            let mut u = 0.0f64;
            for _ in 0..12 {
                state ^= state >> 12;
                state ^= state << 25;
                state ^= state >> 27;
                u += (state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64;
            }
            u - 6.0
        };
        (0..n * 2)
            .map(|_| (next() * sigma_lsb).round().clamp(-128.0, 127.0) as i8)
            .collect()
    }

    /// The ladder's two ends, measured: 0.21 LSB is under-filled and 2.0 LSB (where four of
    /// docs/21 §6's five real captures sit) is not. Counts and states, never wall-clock.
    #[test]
    fn measured_fill_separates_the_under_filled_end_from_the_nominal_one() {
        let under = adc_fill_ci8(&noise_ci8(0.21, 4096));
        let nominal = adc_fill_ci8(&noise_ci8(2.0, 4096));
        assert!(
            under.sigma_lsb.unwrap() < UNDER_FILL_SIGMA_LSB,
            "{:?}",
            under
        );
        assert!(
            (nominal.sigma_lsb.unwrap() - 2.0).abs() < 0.2,
            "{nominal:?}"
        );
        assert_eq!(under.samples, 4096);
        assert_eq!(under.clip_fraction, 0.0);
        assert_eq!(
            FillBucket::classify(under.sigma_lsb, Some(under.clip_fraction)),
            FillBucket::UnderFilled
        );
        assert_eq!(
            FillBucket::classify(nominal.sigma_lsb, Some(nominal.clip_fraction)),
            FillBucket::Nominal
        );
    }

    /// An empty window measures nothing, and nothing is `under_filled` — not nominal.
    #[test]
    fn an_empty_window_is_unmeasured_and_therefore_under_filled() {
        let fill = adc_fill_ci8(&[]);
        assert_eq!(fill.sigma_lsb, None);
        assert_eq!(fill.samples, 0);
        assert_eq!(
            FillBucket::classify(fill.sigma_lsb, Some(fill.clip_fraction)),
            FillBucket::UnderFilled
        );
    }

    /// Clipping is counted, and a DC offset is not fill: the same σ reads the same on a stream
    /// parked off centre.
    #[test]
    fn clip_fraction_counts_rails_and_dc_offset_is_not_fill() {
        let mut iq = noise_ci8(2.0, 1000);
        for c in iq.chunks_exact_mut(2).take(400) {
            c[0] = i8::MAX;
        }
        let fill = adc_fill_ci8(&iq);
        assert_eq!(fill.clip_fraction, 0.4);

        let base = noise_ci8(2.0, 4096);
        let offset: Vec<i8> = base.iter().map(|v| v.saturating_add(40)).collect();
        let (a, b) = (adc_fill_ci8(&base), adc_fill_ci8(&offset));
        assert!(
            (a.sigma_lsb.unwrap() - b.sigma_lsb.unwrap()).abs() < 0.05,
            "{a:?} vs {b:?}"
        );
    }

    /// The unit-scale form agrees with the integer form it re-derives LSB for.
    #[test]
    fn unit_scale_form_agrees_with_the_integer_form() {
        let iq = noise_ci8(2.0, 4096);
        let unit: Vec<num_complex::Complex<f32>> = iq
            .chunks_exact(2)
            .map(|c| {
                num_complex::Complex::new(
                    f32::from(c[0]) / CI8_CODES_PER_FULL_SCALE as f32,
                    f32::from(c[1]) / CI8_CODES_PER_FULL_SCALE as f32,
                )
            })
            .collect();
        let (a, b) = (adc_fill_ci8(&iq), adc_fill_unit_scale(&unit));
        assert!(
            (a.sigma_lsb.unwrap() - b.sigma_lsb.unwrap()).abs() < 1e-3,
            "{a:?} vs {b:?}"
        );
    }
}
