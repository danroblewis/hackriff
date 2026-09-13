//! Spectrum outputs: units, bin geometry, and hold accumulators.
//!
//! # Normalisation conventions
//!
//! Samples are complex, normalised so full scale is `|x| = 1` (hk-core `format`: ci8 `/128`).
//! A **full-scale complex sinusoid `e^{jωn}` has power 1 = 0 dBFS**.
//!
//! With window `w` (length `N`), `X[k] = Σ w[n]·x[n]·e^{−j2πkn/N}` and sample rate `fs`:
//!
//! | Quantity | Formula | Reads |
//! |---|---|---|
//! | PSD (stored, linear) `S[k]` | `|X[k]|² / (fs · Σw²)`, averaged over segments | FS²/Hz |
//! | dBFS/Hz | `10·log10 S[k]` | Complex noise of variance `σ²` reads `σ²/fs` in every bin |
//! | dBFS/bin (per RBW) | `10·log10(S[k] · RBW)`, `RBW = ENBW·fs/N` = `|X[k]|²/(Σw)²` | A bin-centred tone reads its true power (a full-scale tone reads 0 dBFS); noise reads `σ²·ENBW/N` |
//!
//! - **Parseval:** `Σ_k S[k]·(fs/N) = Σ|w·x|²/Σw²` exactly for one segment, so integrating the
//!   PSD times the bin width gives mean power (dBFS) for stationary input. Use
//!   [`Spectrum::integrated_power`]; do not sum dBFS/bin values (that overcounts noise by ENBW).
//! - **Tones off bin centre** read low at the peak bin by up to the window's scalloping loss
//!   (Hann 1.42 dB, flat-top < 0.01 dB), but their integrated power is exact.
//! - **Averaging, max/min-hold and SK work in linear power**, never dB.
//!
//! # Bin order
//!
//! DC-centred (fftshift): bin `i` is at offset `(i − N/2)·fs/N` from `f_center_hz`. Bin `N/2` is
//! DC; bin 0 is `−fs/2` for even `N`.

use std::ops::Range;

use crate::sk;
use crate::window::{WindowKind, WindowMetrics};

/// Which dB scale to convert a linear PSD trace to. See the [module docs](self).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PowerUnit {
    /// Power spectral density, dB relative to full scale per hertz.
    DbfsPerHz,
    /// Power within one RBW (`ENBW·fs/N`): tone-calibrated, a bin-centred tone reads its power.
    DbfsPerBin,
}

/// How a spectrum was resolved: maps onto the docs/07 SpectrumFrame resolution/window fields.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Resolution {
    /// Window family.
    pub window: WindowKind,
    /// FFT length `N` = number of bins.
    pub fft_len: usize,
    /// Samples shared by consecutive segments.
    pub overlap: usize,
    /// Segments averaged (`K`); also the SK `M`.
    pub n_avg: u32,
    /// Bin spacing `fs/N`, Hz.
    pub bin_width_hz: f64,
    /// Resolution bandwidth `ENBW·fs/N`, Hz.
    pub rbw_hz: f64,
    /// The window's figures of merit.
    pub window_metrics: WindowMetrics,
}

impl Resolution {
    /// Segment hop, samples.
    pub fn hop(&self) -> usize {
        self.fft_len - self.overlap
    }
}

/// A DC-centred power spectrum with optional holds and SK. Per-bin arrays are `f32`.
#[derive(Clone, Debug, PartialEq)]
pub struct Spectrum {
    /// RF centre frequency, Hz (bin `N/2`).
    pub f_center_hz: f64,
    /// Complex sample rate = span, Hz.
    pub sample_rate_hz: f64,
    /// Resolution descriptor.
    pub resolution: Resolution,
    /// Averaged PSD, linear FS²/Hz.
    pub psd: Vec<f32>,
    /// Per-bin maximum over the averaged segments, linear FS²/Hz (empty when holds are off).
    pub max_hold: Vec<f32>,
    /// Per-bin minimum over the averaged segments, linear FS²/Hz (empty when holds are off).
    pub min_hold: Vec<f32>,
    /// Per-bin spectral kurtosis with `M = n_avg` (empty when SK is off). See [`crate::sk`].
    pub sk: Vec<f32>,
}

impl Spectrum {
    pub(crate) fn empty(resolution: Resolution, holds: bool, sk: bool) -> Self {
        let n = resolution.fft_len;
        let sized = |on: bool| if on { vec![0.0; n] } else { Vec::new() };
        Self {
            f_center_hz: 0.0,
            sample_rate_hz: 0.0,
            resolution,
            psd: vec![0.0; n],
            max_hold: sized(holds),
            min_hold: sized(holds),
            sk: sized(sk),
        }
    }

    /// Number of bins.
    pub fn bins(&self) -> usize {
        self.psd.len()
    }

    /// Bin spacing, Hz.
    pub fn bin_width_hz(&self) -> f64 {
        self.resolution.bin_width_hz
    }

    /// Span (= sample rate), Hz.
    pub fn span_hz(&self) -> f64 {
        self.sample_rate_hz
    }

    /// Index of the DC bin.
    pub fn center_bin(&self) -> usize {
        self.bins() / 2
    }

    /// Frequency of `bin` relative to the centre, Hz.
    pub fn bin_offset_hz(&self, bin: usize) -> f64 {
        (bin as f64 - self.center_bin() as f64) * self.bin_width_hz()
    }

    /// Absolute frequency of `bin`, Hz.
    pub fn bin_frequency_hz(&self, bin: usize) -> f64 {
        self.f_center_hz + self.bin_offset_hz(bin)
    }

    /// Nearest bin to an offset from the centre, if inside the span.
    pub fn bin_for_offset_hz(&self, offset_hz: f64) -> Option<usize> {
        let i = (offset_hz / self.bin_width_hz()).round() + self.center_bin() as f64;
        (i >= 0.0 && i < self.bins() as f64).then_some(i as usize)
    }

    /// Lower edge of the first bin, Hz (the docs/07 SweepFrame `f_lo`).
    pub fn f_lo_hz(&self) -> f64 {
        self.bin_frequency_hz(0) - self.bin_width_hz() / 2.0
    }

    /// Upper edge of the last bin, Hz (the docs/07 SweepFrame `f_hi`).
    pub fn f_hi_hz(&self) -> f64 {
        self.bin_frequency_hz(self.bins() - 1) + self.bin_width_hz() / 2.0
    }

    /// Power integrated over `bins` of the PSD (`Σ S·fs/N`), linear FS².
    pub fn integrated_power(&self, bins: Range<usize>) -> f64 {
        self.psd[bins].iter().map(|&s| f64::from(s)).sum::<f64>() * self.bin_width_hz()
    }

    /// Power integrated over the whole span, linear FS² (Parseval: the mean sample power).
    pub fn total_power(&self) -> f64 {
        self.integrated_power(0..self.bins())
    }

    /// dB offset added to `10·log10(S)` for `unit`.
    pub fn db_offset(&self, unit: PowerUnit) -> f32 {
        match unit {
            PowerUnit::DbfsPerHz => 0.0,
            PowerUnit::DbfsPerBin => (10.0 * self.resolution.rbw_hz.log10()) as f32,
        }
    }

    /// Converts a linear FS²/Hz trace (`psd`, `max_hold`, `min_hold`) to dB in `unit`.
    pub fn write_db(&self, trace: &[f32], unit: PowerUnit, out: &mut [f32]) {
        assert_eq!(trace.len(), out.len(), "trace/output length mismatch");
        let off = self.db_offset(unit);
        for (o, &v) in out.iter_mut().zip(trace) {
            *o = 10.0 * v.log10() + off;
        }
    }

    /// Allocating form of [`Spectrum::write_db`].
    pub fn to_db(&self, trace: &[f32], unit: PowerUnit) -> Vec<f32> {
        let mut out = vec![0.0; trace.len()];
        self.write_db(trace, unit, &mut out);
        out
    }

    /// SK variance on Gaussian noise for this spectrum's `M` ([`sk::variance`]).
    pub fn sk_variance(&self) -> f64 {
        sk::variance(self.resolution.n_avg)
    }

    /// SK standard deviation on Gaussian noise for this spectrum's `M`.
    pub fn sk_std_dev(&self) -> f64 {
        sk::std_dev(self.resolution.n_avg)
    }
}

/// Which extreme a [`Hold`] keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HoldKind {
    /// Per-bin maximum.
    Max,
    /// Per-bin minimum.
    Min,
}

/// A per-bin max- or min-hold over successive traces (linear units). Allocation-free after
/// construction.
#[derive(Clone, Debug, PartialEq)]
pub struct Hold {
    kind: HoldKind,
    values: Vec<f32>,
    updates: u64,
}

impl Hold {
    /// An empty hold of `bins` bins.
    pub fn new(kind: HoldKind, bins: usize) -> Self {
        let mut h = Self {
            kind,
            values: vec![0.0; bins],
            updates: 0,
        };
        h.reset();
        h
    }

    /// Clears the hold (max → −∞, min → +∞).
    pub fn reset(&mut self) {
        let init = match self.kind {
            HoldKind::Max => f32::NEG_INFINITY,
            HoldKind::Min => f32::INFINITY,
        };
        self.values.fill(init);
        self.updates = 0;
    }

    /// Folds in one trace.
    #[inline]
    pub fn update(&mut self, trace: &[f32]) {
        assert_eq!(trace.len(), self.values.len(), "hold length mismatch");
        match self.kind {
            HoldKind::Max => {
                for (h, &v) in self.values.iter_mut().zip(trace) {
                    *h = h.max(v);
                }
            }
            HoldKind::Min => {
                for (h, &v) in self.values.iter_mut().zip(trace) {
                    *h = h.min(v);
                }
            }
        }
        self.updates += 1;
    }

    /// The held values.
    pub fn values(&self) -> &[f32] {
        &self.values
    }

    /// Traces folded in since the last reset.
    pub fn updates(&self) -> u64 {
        self.updates
    }

    /// Kind.
    pub fn kind(&self) -> HoldKind {
        self.kind
    }
}
