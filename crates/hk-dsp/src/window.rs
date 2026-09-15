//! Analysis windows and their figures of merit.
//!
//! All windows are **periodic** (DFT-even): `w[n]` for `n = 0..N` is one period of the
//! symmetric `N + 1`-point window with its last point dropped. That is the right form for
//! spectral analysis, and it makes 50% overlap-add of Hann exactly constant.
//!
//! | Window | Definition (`x = 2πn/N`) | ENBW (bins) | Coherent gain | Scalloping loss |
//! |---|---|---|---|---|
//! | Hann | `0.5 − 0.5·cos x` | 1.500 | 0.500 (−6.02 dB) | 1.42 dB |
//! | Blackman-Harris (4-term, −92 dB) | `0.35875 − 0.48829·cos x + 0.14128·cos 2x − 0.01168·cos 3x` | 2.004 | 0.359 (−8.90 dB) | 0.83 dB |
//! | Flat-top (ISO 18431-2 / MATLAB `flattopwin`) | `0.21557895 − 0.41663158·cos x + 0.277263158·cos 2x − 0.083578947·cos 3x + 0.006947368·cos 4x` | 3.770 | 0.216 (−13.33 dB) | < 0.01 dB |
//!
//! [`WindowMetrics`] computes these numerically from the actual `f32` coefficients:
//!
//! - **ENBW** (equivalent noise bandwidth, bins) `= N·Σw² / (Σw)²`. RBW `= ENBW · fs / N`.
//! - **Coherent gain** `= Σw / N`: the amplitude a bin-centred tone is scaled by.
//! - **Scalloping loss** `= −20·log10(|Σ w[n]·e^{−jπn/N}| / Σw)`: how much a tone half-way
//!   between two bins reads low at its peak bin.

use std::fmt;

use serde::{Deserialize, Serialize};

/// The window family. Serialises/deserialises as [`WindowKind::name`] (`"hann"`,
/// `"blackman-harris"`, `"flat-top"`), e.g. for the control API's display settings (T-067).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WindowKind {
    /// Hann: the default. Good leakage/resolution balance for PSD estimation.
    #[default]
    Hann,
    /// 4-term Blackman-Harris (−92 dB sidelobes): weak signals next to strong ones.
    BlackmanHarris,
    /// Flat-top: amplitude-accurate tone levels regardless of bin offset (wide main lobe).
    FlatTop,
}

impl WindowKind {
    /// Cosine-series coefficients `a_k`; `w = Σ (−1)^k a_k cos(k·2πn/N)`.
    fn coefficients(self) -> &'static [f64] {
        match self {
            WindowKind::Hann => &[0.5, 0.5],
            WindowKind::BlackmanHarris => &[0.35875, 0.48829, 0.14128, 0.01168],
            WindowKind::FlatTop => &[
                0.215_578_95,
                0.416_631_58,
                0.277_263_158,
                0.083_578_947,
                0.006_947_368,
            ],
        }
    }

    /// A short lowercase name (`hann`, `blackman-harris`, `flat-top`).
    pub fn name(self) -> &'static str {
        match self {
            WindowKind::Hann => "hann",
            WindowKind::BlackmanHarris => "blackman-harris",
            WindowKind::FlatTop => "flat-top",
        }
    }

    /// Parses [`WindowKind::name`] (also accepting `_` for `-`, e.g. from a form field);
    /// `None` for anything else.
    pub fn from_name(s: &str) -> Option<Self> {
        match s.replace('_', "-").as_str() {
            "hann" => Some(WindowKind::Hann),
            "blackman-harris" => Some(WindowKind::BlackmanHarris),
            "flat-top" => Some(WindowKind::FlatTop),
            _ => None,
        }
    }

    /// Every window kind, in [`WindowKind::name`] order (for listing, e.g. control-API state
    /// limits, T-067).
    pub const ALL: [WindowKind; 3] = [
        WindowKind::Hann,
        WindowKind::BlackmanHarris,
        WindowKind::FlatTop,
    ];
}

impl fmt::Display for WindowKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Figures of merit of a window of a given length. See the [module docs](self).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WindowMetrics {
    /// Equivalent noise bandwidth, bins.
    pub enbw_bins: f64,
    /// Coherent (amplitude) gain `Σw / N`.
    pub coherent_gain: f64,
    /// Coherent gain, dB (`20·log10`).
    pub coherent_gain_db: f64,
    /// Worst-case tone peak loss at a half-bin offset, dB (positive).
    pub scalloping_loss_db: f64,
    /// Incoherent (noise power) gain `Σw² / N`.
    pub noise_power_gain: f64,
}

/// A window of a fixed length with precomputed sums.
#[derive(Clone, Debug)]
pub struct Window {
    kind: WindowKind,
    coeffs: Vec<f32>,
    sum: f64,
    sum_sq: f64,
    metrics: WindowMetrics,
}

impl Window {
    /// Builds a periodic window of `len` points (`len >= 2`).
    pub fn new(kind: WindowKind, len: usize) -> Self {
        assert!(len >= 2, "window length must be at least 2");
        let a = kind.coefficients();
        let coeffs: Vec<f32> = (0..len)
            .map(|n| {
                let x = 2.0 * std::f64::consts::PI * n as f64 / len as f64;
                a.iter()
                    .enumerate()
                    .map(|(k, &ak)| {
                        let sign = if k % 2 == 0 { 1.0 } else { -1.0 };
                        sign * ak * (k as f64 * x).cos()
                    })
                    .sum::<f64>() as f32
            })
            .collect();
        let sum: f64 = coeffs.iter().map(|&w| f64::from(w)).sum();
        let sum_sq: f64 = coeffs.iter().map(|&w| f64::from(w) * f64::from(w)).sum();
        // Response at a half-bin offset.
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (n, &w) in coeffs.iter().enumerate() {
            let phi = -std::f64::consts::PI * n as f64 / len as f64;
            re += f64::from(w) * phi.cos();
            im += f64::from(w) * phi.sin();
        }
        let n = len as f64;
        let metrics = WindowMetrics {
            enbw_bins: n * sum_sq / (sum * sum),
            coherent_gain: sum / n,
            coherent_gain_db: 20.0 * (sum / n).log10(),
            scalloping_loss_db: -20.0 * (re.hypot(im) / sum).log10(),
            noise_power_gain: sum_sq / n,
        };
        Self {
            kind,
            coeffs,
            sum,
            sum_sq,
            metrics,
        }
    }

    /// The window family.
    pub fn kind(&self) -> WindowKind {
        self.kind
    }

    /// Number of points.
    pub fn len(&self) -> usize {
        self.coeffs.len()
    }

    /// Always false (length is at least 2).
    pub fn is_empty(&self) -> bool {
        self.coeffs.is_empty()
    }

    /// The coefficients.
    pub fn coefficients(&self) -> &[f32] {
        &self.coeffs
    }

    /// `Σw`.
    pub fn sum(&self) -> f64 {
        self.sum
    }

    /// `Σw²`.
    pub fn sum_sq(&self) -> f64 {
        self.sum_sq
    }

    /// Figures of merit.
    pub fn metrics(&self) -> WindowMetrics {
        self.metrics
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_match_theory() {
        let n = 4096;
        let hann = Window::new(WindowKind::Hann, n).metrics();
        assert!((hann.enbw_bins - 1.5).abs() < 1e-4, "{hann:?}");
        assert!((hann.coherent_gain - 0.5).abs() < 1e-6);
        assert!((hann.scalloping_loss_db - 1.4236).abs() < 0.002, "{hann:?}");

        let bh = Window::new(WindowKind::BlackmanHarris, n).metrics();
        assert!((bh.enbw_bins - 2.0044).abs() < 0.002, "{bh:?}");
        assert!((bh.coherent_gain - 0.35875).abs() < 1e-5);
        assert!((bh.scalloping_loss_db - 0.826).abs() < 0.01, "{bh:?}");

        let ft = Window::new(WindowKind::FlatTop, n).metrics();
        assert!((ft.enbw_bins - 3.770).abs() < 0.005, "{ft:?}");
        assert!((ft.coherent_gain - 0.21557895).abs() < 1e-5);
        assert!(ft.scalloping_loss_db.abs() < 0.01, "{ft:?}");
    }

    #[test]
    fn from_name_round_trips_every_kind_and_rejects_the_rest() {
        for k in WindowKind::ALL {
            assert_eq!(WindowKind::from_name(k.name()), Some(k), "{k}");
        }
        assert_eq!(
            WindowKind::from_name("blackman_harris"),
            Some(WindowKind::BlackmanHarris),
            "underscore accepted"
        );
        assert_eq!(WindowKind::from_name("kaiser"), None);
        assert_eq!(WindowKind::from_name(""), None);
    }

    #[test]
    fn hann_is_periodic() {
        let w = Window::new(WindowKind::Hann, 8);
        assert_eq!(w.coefficients()[0], 0.0);
        assert!((w.coefficients()[4] - 1.0).abs() < 1e-7);
        // 50% overlap-add is constant.
        for n in 0..4 {
            let s = w.coefficients()[n] + w.coefficients()[n + 4];
            assert!((s - 1.0).abs() < 1e-6);
        }
    }
}
