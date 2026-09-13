//! Welch PSD: windowed, overlapped, averaged periodograms (plus per-bin SK and holds).
//!
//! [`SegmentEngine`] is the per-segment kernel shared by the one-shot [`welch`] and the
//! streaming [`StftProcessor`](crate::stft::StftProcessor): window → FFT → DC-centred `|X|²` →
//! linear accumulators (`ΣP`, `ΣP²`, max, min). Nothing allocates after construction.

use std::fmt;

use num_complex::Complex32;

use crate::fft::{CpuFft, FftBackend};
use crate::sk;
use crate::spectrum::{Hold, HoldKind, Resolution, Spectrum};
use crate::window::{Window, WindowKind};

/// Invalid spectral-estimation settings or input.
#[derive(Clone, Debug, PartialEq)]
pub enum ConfigError {
    /// `fft_len` must be at least 4.
    FftLenTooSmall(usize),
    /// `overlap` must be below `fft_len`.
    OverlapTooLarge {
        /// Requested overlap.
        overlap: usize,
        /// FFT length.
        fft_len: usize,
    },
    /// `averages` must be at least 1.
    ZeroAverages,
    /// Spectral kurtosis needs at least 2 averaged segments.
    SkNeedsTwoAverages,
    /// The FFT backend's length differs from `fft_len`.
    BackendLength {
        /// Backend length.
        backend: usize,
        /// Configured FFT length.
        fft_len: usize,
    },
    /// Invalid persistence settings.
    Persistence(&'static str),
    /// Sample rate must be finite and positive.
    SampleRate(f64),
    /// Fewer samples than one segment.
    NotEnoughSamples {
        /// Samples given.
        got: usize,
        /// Samples needed.
        need: usize,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::FftLenTooSmall(n) => write!(f, "fft_len {n} is below 4"),
            ConfigError::OverlapTooLarge { overlap, fft_len } => {
                write!(f, "overlap {overlap} must be below fft_len {fft_len}")
            }
            ConfigError::ZeroAverages => f.write_str("averages must be >= 1"),
            ConfigError::SkNeedsTwoAverages => f.write_str("spectral kurtosis needs averages >= 2"),
            ConfigError::BackendLength { backend, fft_len } => {
                write!(f, "FFT backend length {backend} != fft_len {fft_len}")
            }
            ConfigError::Persistence(why) => f.write_str(why),
            ConfigError::SampleRate(fs) => write!(f, "invalid sample rate {fs}"),
            ConfigError::NotEnoughSamples { got, need } => {
                write!(f, "{got} samples is fewer than one {need}-sample segment")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

/// Per-segment settings shared by [`welch`] and the STFT.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WelchConfig {
    /// FFT length `N` = bins (>= 4). Powers of two are fastest.
    pub fft_len: usize,
    /// Samples shared by consecutive segments (< `fft_len`). Default 50%.
    pub overlap: usize,
    /// Window family.
    pub window: WindowKind,
    /// Keep per-bin max- and min-hold over the averaged segments.
    pub holds: bool,
    /// Accumulate per-bin spectral kurtosis over the averaged segments.
    pub spectral_kurtosis: bool,
}

impl Default for WelchConfig {
    /// 4096 bins, Hann, 50% overlap, holds and SK on.
    fn default() -> Self {
        Self::new(4096)
    }
}

impl WelchConfig {
    /// `fft_len` bins, Hann, 50% overlap, holds and SK on.
    pub fn new(fft_len: usize) -> Self {
        Self {
            fft_len,
            overlap: fft_len / 2,
            window: WindowKind::Hann,
            holds: true,
            spectral_kurtosis: true,
        }
    }

    /// Segment hop, samples.
    pub fn hop(&self) -> usize {
        self.fft_len - self.overlap
    }

    /// Checks the settings.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.fft_len < 4 {
            return Err(ConfigError::FftLenTooSmall(self.fft_len));
        }
        if self.overlap >= self.fft_len {
            return Err(ConfigError::OverlapTooLarge {
                overlap: self.overlap,
                fft_len: self.fft_len,
            });
        }
        Ok(())
    }
}

/// The per-segment kernel. See the [module docs](self).
pub struct SegmentEngine {
    config: WelchConfig,
    window: Window,
    fft: Box<dyn FftBackend>,
    buf: Vec<Complex32>,
    /// DC-centred `|X|²` of the last segment.
    power: Vec<f32>,
    sum: Vec<f64>,
    sum_sq: Vec<f64>,
    max: Hold,
    min: Hold,
    count: u32,
}

impl SegmentEngine {
    /// A CPU engine.
    pub fn new(config: WelchConfig) -> Result<Self, ConfigError> {
        config.validate()?;
        Self::with_backend(config, Box::new(CpuFft::new(config.fft_len)))
    }

    /// An engine over a given FFT backend (length must equal `fft_len`).
    pub fn with_backend(
        config: WelchConfig,
        fft: Box<dyn FftBackend>,
    ) -> Result<Self, ConfigError> {
        config.validate()?;
        let n = config.fft_len;
        if fft.len() != n {
            return Err(ConfigError::BackendLength {
                backend: fft.len(),
                fft_len: n,
            });
        }
        let holds = if config.holds { n } else { 0 };
        Ok(Self {
            config,
            window: Window::new(config.window, n),
            fft,
            buf: vec![Complex32::default(); n],
            power: vec![0.0; n],
            sum: vec![0.0; n],
            sum_sq: if config.spectral_kurtosis {
                vec![0.0; n]
            } else {
                Vec::new()
            },
            max: Hold::new(HoldKind::Max, holds),
            min: Hold::new(HoldKind::Min, holds),
            count: 0,
        })
    }

    /// Settings.
    pub fn config(&self) -> &WelchConfig {
        &self.config
    }

    /// The window.
    pub fn window(&self) -> &Window {
        &self.window
    }

    /// FFT backend name.
    pub fn backend_name(&self) -> &'static str {
        self.fft.name()
    }

    /// Segments accumulated since the last reset.
    pub fn count(&self) -> u32 {
        self.count
    }

    /// DC-centred raw `|X|²` of the most recent segment (multiply by `1/(Σw)²` for power per
    /// RBW, by `1/(fs·Σw²)` for density).
    pub fn last_power(&self) -> &[f32] {
        &self.power
    }

    /// dB offset turning `10·log10(|X|²)` into dBFS per RBW: `−20·log10(Σw)`.
    pub fn per_rbw_offset_db(&self) -> f32 {
        (-20.0 * self.window.sum().log10()) as f32
    }

    /// Clears the accumulators.
    pub fn reset(&mut self) {
        self.sum.fill(0.0);
        self.sum_sq.fill(0.0);
        self.max.reset();
        self.min.reset();
        self.count = 0;
    }

    /// Processes one raw segment (`raw.len() == fft_len`, unwindowed, full scale 1).
    #[inline]
    pub fn process(&mut self, raw: &[Complex32]) {
        let n = self.config.fft_len;
        assert_eq!(raw.len(), n, "segment length != fft_len");
        for ((o, &x), &w) in self.buf.iter_mut().zip(raw).zip(self.window.coefficients()) {
            *o = x.scale(w);
        }
        self.fft.forward(&mut self.buf);

        // fftshift: bin i ↔ FFT index (i + ceil(N/2)) mod N, so bin N/2 is DC.
        let split = n - n / 2;
        let (neg_out, pos_out) = self.power.split_at_mut(n / 2);
        for (p, x) in neg_out.iter_mut().zip(&self.buf[split..]) {
            *p = x.norm_sqr();
        }
        for (p, x) in pos_out.iter_mut().zip(&self.buf[..split]) {
            *p = x.norm_sqr();
        }

        for (s, &p) in self.sum.iter_mut().zip(&self.power) {
            *s += f64::from(p);
        }
        if self.config.spectral_kurtosis {
            for (s, &p) in self.sum_sq.iter_mut().zip(&self.power) {
                let p = f64::from(p);
                *s += p * p;
            }
        }
        if self.config.holds {
            self.max.update(&self.power);
            self.min.update(&self.power);
        }
        self.count += 1;
    }

    /// The resolution descriptor at `sample_rate_hz` for the current count.
    pub fn resolution(&self, sample_rate_hz: f64) -> Resolution {
        let n = self.config.fft_len;
        let metrics = self.window.metrics();
        let bin_width_hz = sample_rate_hz / n as f64;
        Resolution {
            window: self.config.window,
            fft_len: n,
            overlap: self.config.overlap,
            n_avg: self.count,
            bin_width_hz,
            rbw_hz: metrics.enbw_bins * bin_width_hz,
            window_metrics: metrics,
        }
    }

    /// An empty [`Spectrum`] sized for this engine.
    pub fn empty_spectrum(&self) -> Spectrum {
        Spectrum::empty(
            self.resolution(1.0),
            self.config.holds,
            self.config.spectral_kurtosis,
        )
    }

    /// Writes the accumulated result into `out` (sized by [`SegmentEngine::empty_spectrum`]).
    /// Allocation-free. Requires `count() >= 1`.
    pub fn finish_into(&self, sample_rate_hz: f64, f_center_hz: f64, out: &mut Spectrum) {
        assert!(self.count > 0, "no segments accumulated");
        let density = 1.0 / (sample_rate_hz * self.window.sum_sq());
        out.f_center_hz = f_center_hz;
        out.sample_rate_hz = sample_rate_hz;
        out.resolution = self.resolution(sample_rate_hz);
        let mean_scale = density / f64::from(self.count);
        for (o, &s) in out.psd.iter_mut().zip(&self.sum) {
            *o = (s * mean_scale) as f32;
        }
        if self.config.holds {
            for (o, &v) in out.max_hold.iter_mut().zip(self.max.values()) {
                *o = (f64::from(v) * density) as f32;
            }
            for (o, &v) in out.min_hold.iter_mut().zip(self.min.values()) {
                *o = (f64::from(v) * density) as f32;
            }
        }
        if self.config.spectral_kurtosis {
            for ((o, &s1), &s2) in out.sk.iter_mut().zip(&self.sum).zip(&self.sum_sq) {
                *o = sk::estimate(self.count, s1, s2);
            }
        }
    }
}

/// One-shot Welch estimate over every complete segment of `samples` (allocates; off the hot
/// path). SK is `NaN` when only one segment fits.
pub fn welch(
    samples: &[Complex32],
    sample_rate_hz: f64,
    f_center_hz: f64,
    config: &WelchConfig,
) -> Result<Spectrum, ConfigError> {
    if !(sample_rate_hz.is_finite() && sample_rate_hz > 0.0) {
        return Err(ConfigError::SampleRate(sample_rate_hz));
    }
    let mut engine = SegmentEngine::new(*config)?;
    let n = config.fft_len;
    if samples.len() < n {
        return Err(ConfigError::NotEnoughSamples {
            got: samples.len(),
            need: n,
        });
    }
    let mut start = 0;
    while start + n <= samples.len() {
        engine.process(&samples[start..start + n]);
        start += config.hop();
    }
    let mut out = engine.empty_spectrum();
    engine.finish_into(sample_rate_hz, f_center_hz, &mut out);
    Ok(out)
}
