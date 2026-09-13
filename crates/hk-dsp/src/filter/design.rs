//! Low-pass FIR design by the Kaiser-window method, with measured (not just predicted) response.
//!
//! **Method.** A windowed sinc with cutoff midway between the passband and stopband edges,
//! shaped by a Kaiser window. The Kaiser β comes from the stopband attenuation `A` (Kaiser 1974):
//! `β = 0.1102(A − 8.7)` for `A > 50`, `0.5842(A − 21)^0.4 + 0.07886(A − 21)` for
//! `21 ≤ A ≤ 50`, else 0. The order estimate is `N ≈ (A − 7.95) / (14.357·Δf/fs)` taps, with
//! `Δf` the transition width.
//!
//! **Verification loop.** The estimate is occasionally a few taps short, so
//! [`design_lowpass`] measures the response on a dense FFT grid (≥ 8× the tap count) and grows
//! the filter until the measured stopband meets `A`. Every [`FirDesign`] carries its
//! [`Response`], so callers and tests see measured numbers rather than predictions.
//!
//! **What a design guarantees** (for spec `fp`, `fst`, `A`):
//! - stopband: `|H(f)| ≤ −A dB` relative to DC for every `f ≥ fst` up to `fs/2` (measured);
//! - passband ripple: a Kaiser window puts roughly the same deviation `δ = 10^(−A/20)` in the
//!   passband, so peak-to-peak ripple is about `2δ` in amplitude, i.e. `≈ 0.017 dB` at
//!   `A = 60`, `≈ 0.0017 dB` at `A = 80` (measured and reported in [`Response`]);
//! - transition: `fst − fp`, monotone-ish, −6 dB near the midpoint;
//! - linear phase: taps are symmetric, group delay `(len − 1)/2` samples.
//!
//! Equiripple (Parks–McClellan) would save roughly 10–20% of taps at the same spec. It is not
//! implemented: the Kaiser designs are verified, deterministic and cheap to compute at runtime
//! (chains are built from data specs while capture runs), and the saving does not change the
//! real-time headroom materially.

use std::f64::consts::PI;
use std::fmt;

use num_complex::Complex32;

use crate::fft::{CpuFft, FftBackend};

/// Default stopband attenuation for channel filters, dB.
pub const DEFAULT_STOPBAND_DB: f64 = 60.0;

/// Longest filter the designer will produce.
pub const MAX_TAPS: usize = 1 << 20;

/// A low-pass specification. Frequencies are in Hz at `sample_rate_hz`; any consistent unit
/// works (the PFB prototype uses `sample_rate_hz = 1`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LowpassSpec {
    /// Sample rate the filter runs at.
    pub sample_rate_hz: f64,
    /// Passband edge: `|f| ≤ passband_hz` is passed with ripple only.
    pub passband_hz: f64,
    /// Stopband edge: `|f| ≥ stopband_hz` is attenuated by at least `stopband_db`.
    pub stopband_hz: f64,
    /// Minimum stopband attenuation, dB (positive).
    pub stopband_db: f64,
}

/// Why a filter could not be designed.
#[derive(Clone, Debug, PartialEq)]
pub enum DesignError {
    /// The specification is inconsistent (reason attached).
    InvalidSpec(String),
    /// Meeting the specification needs more than [`MAX_TAPS`] taps.
    TooManyTaps {
        /// Taps the design would need (lower bound).
        needed: usize,
    },
}

impl fmt::Display for DesignError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DesignError::InvalidSpec(why) => write!(f, "invalid low-pass spec: {why}"),
            DesignError::TooManyTaps { needed } => {
                write!(f, "filter needs {needed} taps (limit {MAX_TAPS})")
            }
        }
    }
}

impl std::error::Error for DesignError {}

impl LowpassSpec {
    /// A spec from its four numbers.
    pub fn new(sample_rate_hz: f64, passband_hz: f64, stopband_hz: f64, stopband_db: f64) -> Self {
        Self {
            sample_rate_hz,
            passband_hz,
            stopband_hz,
            stopband_db,
        }
    }

    /// Transition width `stopband_hz − passband_hz`.
    pub fn transition_hz(&self) -> f64 {
        self.stopband_hz - self.passband_hz
    }

    /// Checks `0 ≤ fp < fst ≤ fs/2` and `10 ≤ A ≤ 200`.
    pub fn validate(&self) -> Result<(), DesignError> {
        let bad = |why: String| Err(DesignError::InvalidSpec(why));
        let all = [
            self.sample_rate_hz,
            self.passband_hz,
            self.stopband_hz,
            self.stopband_db,
        ];
        if all.iter().any(|v| !v.is_finite()) {
            return bad(format!("non-finite value in {self:?}"));
        }
        if self.sample_rate_hz <= 0.0 {
            return bad(format!(
                "sample rate {} must be positive",
                self.sample_rate_hz
            ));
        }
        if self.passband_hz < 0.0 || self.passband_hz >= self.stopband_hz {
            return bad(format!(
                "need 0 <= passband ({}) < stopband ({})",
                self.passband_hz, self.stopband_hz
            ));
        }
        if self.stopband_hz > self.sample_rate_hz / 2.0 * (1.0 + 1e-12) {
            return bad(format!(
                "stopband edge {} is above Nyquist {}",
                self.stopband_hz,
                self.sample_rate_hz / 2.0
            ));
        }
        if !(10.0..=200.0).contains(&self.stopband_db) {
            return bad(format!(
                "stopband attenuation {} dB outside 10..=200",
                self.stopband_db
            ));
        }
        Ok(())
    }

    /// Kaiser β for this attenuation.
    pub fn kaiser_beta(&self) -> f64 {
        kaiser_beta(self.stopband_db)
    }

    /// Kaiser tap-count estimate (before verification).
    pub fn estimate_taps(&self) -> usize {
        kaiser_taps(self.stopband_db, self.transition_hz() / self.sample_rate_hz)
    }
}

/// Kaiser β for stopband attenuation `a` dB.
pub fn kaiser_beta(a: f64) -> f64 {
    if a > 50.0 {
        0.1102 * (a - 8.7)
    } else if a >= 21.0 {
        0.5842 * (a - 21.0).powf(0.4) + 0.07886 * (a - 21.0)
    } else {
        0.0
    }
}

/// Kaiser tap-count estimate for attenuation `a` dB and transition width `transition` as a
/// fraction of the sample rate.
pub fn kaiser_taps(a: f64, transition: f64) -> usize {
    let d = if a > 21.0 {
        (a - 7.95) / 14.357
    } else {
        0.9222
    };
    let n = (d / transition.max(1e-12)).ceil();
    if n >= MAX_TAPS as f64 {
        MAX_TAPS + 1
    } else {
        n as usize + 1
    }
}

/// Zeroth-order modified Bessel function of the first kind (power series).
pub fn bessel_i0(x: f64) -> f64 {
    let q = x * x / 4.0;
    let mut term = 1.0;
    let mut sum = 1.0;
    for k in 1..200 {
        term *= q / (k * k) as f64;
        sum += term;
        if term < sum * 1e-17 {
            break;
        }
    }
    sum
}

/// Kaiser window of `len` points with shape `beta`.
pub fn kaiser_window(len: usize, beta: f64) -> Vec<f64> {
    if len == 1 {
        return vec![1.0];
    }
    let denom = bessel_i0(beta);
    let m = (len - 1) as f64;
    (0..len)
        .map(|n| {
            let r = 2.0 * n as f64 / m - 1.0;
            bessel_i0(beta * (1.0 - r * r).max(0.0).sqrt()) / denom
        })
        .collect()
}

/// Kaiser-windowed sinc of `len` taps, cutoff `cutoff` (fraction of the sample rate, −6 dB
/// point), normalised to DC gain `gain`.
pub fn windowed_sinc(len: usize, cutoff: f64, beta: f64, gain: f64) -> Vec<f32> {
    let w = kaiser_window(len, beta);
    let mid = (len as f64 - 1.0) / 2.0;
    let h: Vec<f64> = w
        .iter()
        .enumerate()
        .map(|(n, wn)| {
            let t = n as f64 - mid;
            let s = if t.abs() < 1e-12 {
                2.0 * cutoff
            } else {
                (2.0 * PI * cutoff * t).sin() / (PI * t)
            };
            s * wn
        })
        .collect();
    let sum: f64 = h.iter().sum();
    h.iter().map(|v| (v * gain / sum) as f32).collect()
}

/// A measured low-pass response, relative to the DC gain.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Response {
    /// Minimum passband gain, dB.
    pub passband_min_db: f64,
    /// Maximum passband gain, dB.
    pub passband_max_db: f64,
    /// Peak-to-peak passband ripple, dB.
    pub passband_ripple_db: f64,
    /// Minimum stopband attenuation (positive dB): the worst stopband gain is `−stopband_db`.
    pub stopband_db: f64,
    /// Frequency of the worst stopband response, Hz.
    pub worst_stopband_hz: f64,
}

/// Measures `taps` against `spec` on a dense FFT grid (at least 8× the tap count, at least
/// 8192 points).
pub fn measure_response(taps: &[f32], spec: &LowpassSpec) -> Response {
    let grid = (taps.len() * 8).max(8192).next_power_of_two();
    let mut buf = vec![Complex32::default(); grid];
    for (b, &t) in buf.iter_mut().zip(taps) {
        b.re = t;
    }
    CpuFft::new(grid).forward(&mut buf);
    let dc = f64::from(buf[0].norm()).max(f64::MIN_POSITIVE);
    let fs = spec.sample_rate_hz;
    let mut pmin = f64::INFINITY;
    let mut pmax = f64::NEG_INFINITY;
    let mut smax = 0.0f64;
    let mut worst = spec.stopband_hz;
    for (k, v) in buf.iter().enumerate().take(grid / 2 + 1) {
        let f = k as f64 * fs / grid as f64;
        let g = f64::from(v.norm()) / dc;
        if f <= spec.passband_hz {
            pmin = pmin.min(g);
            pmax = pmax.max(g);
        } else if f >= spec.stopband_hz && g > smax {
            smax = g;
            worst = f;
        }
    }
    let db = |g: f64| 20.0 * g.max(1e-15).log10();
    Response {
        passband_min_db: db(pmin),
        passband_max_db: db(pmax),
        passband_ripple_db: db(pmax) - db(pmin),
        stopband_db: -db(smax),
        worst_stopband_hz: worst,
    }
}

/// A designed, verified low-pass FIR.
#[derive(Clone, Debug, PartialEq)]
pub struct FirDesign {
    /// The specification it meets.
    pub spec: LowpassSpec,
    /// Symmetric taps.
    pub taps: Vec<f32>,
    /// Kaiser β used.
    pub beta: f64,
    /// DC gain the taps are normalised to (1, or `P` for a `P`-phase polyphase prototype).
    pub gain: f64,
    /// Measured response.
    pub response: Response,
}

impl FirDesign {
    /// Number of taps.
    pub fn len(&self) -> usize {
        self.taps.len()
    }

    /// Always false: a design has at least one tap.
    pub fn is_empty(&self) -> bool {
        self.taps.is_empty()
    }

    /// Group delay in samples at the design rate: `(len − 1) / 2`.
    pub fn group_delay_samples(&self) -> f64 {
        (self.taps.len() as f64 - 1.0) / 2.0
    }
}

/// Designs an odd-length, unity-DC-gain low-pass meeting `spec` (measured).
pub fn design_lowpass(spec: LowpassSpec) -> Result<FirDesign, DesignError> {
    design_lowpass_with(spec, 1, 1.0)
}

/// Designs a low-pass meeting `spec` whose length is a multiple of `multiple` (a `P`-phase
/// polyphase prototype uses `multiple = P`, `gain = P`), or odd when `multiple == 1`.
pub fn design_lowpass_with(
    spec: LowpassSpec,
    multiple: usize,
    gain: f64,
) -> Result<FirDesign, DesignError> {
    spec.validate()?;
    let multiple = multiple.max(1);
    let fit = |n: usize| {
        if multiple == 1 {
            n.max(3) | 1
        } else {
            n.max(multiple).div_ceil(multiple) * multiple
        }
    };
    let beta = spec.kaiser_beta();
    let cutoff = (spec.passband_hz + spec.stopband_hz) / 2.0 / spec.sample_rate_hz;
    let mut len = fit(spec.estimate_taps());
    for _ in 0..40 {
        if len > MAX_TAPS {
            return Err(DesignError::TooManyTaps { needed: len });
        }
        let taps = windowed_sinc(len, cutoff, beta, gain);
        let response = measure_response(&taps, &spec);
        if response.stopband_db >= spec.stopband_db {
            return Ok(FirDesign {
                spec,
                taps,
                beta,
                gain,
                response,
            });
        }
        len = fit(len + (len / 50).max(2));
    }
    Err(DesignError::TooManyTaps { needed: len })
}

/// The analysis prototype for an `channels`-channel, 2× oversampled polyphase filter bank.
///
/// With channel spacing `Δ = fs/M` and output rate `2Δ`, the prototype (normalised to
/// `fs = 1`) has passband edge `Δ/2` and stopband edge `Δ`:
/// - every frequency within `±Δ/2` of a channel centre is passed flat (ripple only), so a
///   signal on a channel boundary appears in full in both neighbouring channels;
/// - the transition band `(Δ/2, Δ)` stays inside the output Nyquist band `±Δ`, so nothing
///   aliases except content already `≥ A` dB down;
/// - an adjacent channel's centre (`±Δ`) is at the stopband edge, so adjacent-channel leakage
///   is at most `−A` dB.
///
/// The length is about `(A − 8)/(14.357/(2M))`, i.e. ≈ `7.25·M` taps at 60 dB and ≈ `10·M`
/// at 80 dB.
pub fn pfb_prototype(channels: usize, stopband_db: f64) -> Result<FirDesign, DesignError> {
    if channels < 2 || channels % 2 != 0 {
        return Err(DesignError::InvalidSpec(format!(
            "channel count {channels} must be even and >= 2"
        )));
    }
    let m = channels as f64;
    design_lowpass(LowpassSpec::new(1.0, 0.5 / m, 1.0 / m, stopband_db))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kaiser_beta_matches_published_values() {
        assert!((kaiser_beta(60.0) - 5.65326).abs() < 1e-4);
        assert!((kaiser_beta(30.0) - (0.5842 * 9f64.powf(0.4) + 0.07886 * 9.0)).abs() < 1e-12);
        assert_eq!(kaiser_beta(15.0), 0.0);
    }

    #[test]
    fn bessel_i0_known_values() {
        assert!((bessel_i0(0.0) - 1.0).abs() < 1e-15);
        assert!((bessel_i0(1.0) - 1.266_065_877_752_008_4).abs() < 1e-12);
        assert!((bessel_i0(5.0) - 27.239_871_823_604_45).abs() < 1e-9);
    }

    #[test]
    fn design_meets_spec_and_is_symmetric() {
        let spec = LowpassSpec::new(48_000.0, 4_000.0, 6_000.0, 70.0);
        let d = design_lowpass(spec).unwrap();
        assert!(d.len() % 2 == 1);
        assert!(d.response.stopband_db >= 70.0, "{:?}", d.response);
        assert!(d.response.passband_ripple_db < 0.01, "{:?}", d.response);
        let n = d.len();
        for i in 0..n / 2 {
            assert_eq!(d.taps[i], d.taps[n - 1 - i]);
        }
        let dc: f64 = d.taps.iter().map(|&t| f64::from(t)).sum();
        assert!((dc - 1.0).abs() < 1e-5);
        // Estimate within a few percent of the verified length.
        assert!((d.len() as f64) < spec.estimate_taps() as f64 * 1.1);
    }

    #[test]
    fn polyphase_length_is_a_multiple() {
        let spec = LowpassSpec::new(8.0 * 100.0, 10.0, 25.0, 60.0);
        let d = design_lowpass_with(spec, 8, 8.0).unwrap();
        assert_eq!(d.len() % 8, 0);
        let dc: f64 = d.taps.iter().map(|&t| f64::from(t)).sum();
        assert!((dc - 8.0).abs() < 1e-4);
    }

    #[test]
    fn rejects_bad_specs() {
        assert!(design_lowpass(LowpassSpec::new(1.0, 0.3, 0.2, 60.0)).is_err());
        assert!(design_lowpass(LowpassSpec::new(1.0, 0.1, 0.6, 60.0)).is_err());
        assert!(pfb_prototype(13, 60.0).is_err());
        assert!(pfb_prototype(12, 60.0).is_ok());
    }
}
