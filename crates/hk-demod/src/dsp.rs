//! Small streaming DSP blocks shared by the demodulators: FIR decimators whose state carries
//! across calls, the quadrature discriminator and single-pole de-emphasis.
//!
//! Allocation happens at construction only; `push` never allocates.

use std::f64::consts::TAU;
use std::ops::{AddAssign, Mul};

use hk_dsp::{DesignError, LowpassSpec, design_lowpass};
use num_complex::Complex32;

/// A sample type a real-tap FIR can filter.
pub trait FirSample: Copy + Default + AddAssign + Mul<f32, Output = Self> {}
impl FirSample for f32 {}
impl FirSample for Complex32 {}

/// Unity-DC-gain Kaiser low-pass taps meeting the spec (hk-dsp design, measured).
pub fn lowpass_taps(
    sample_rate_hz: f64,
    passband_hz: f64,
    stopband_hz: f64,
    stopband_db: f64,
) -> Result<Vec<f32>, DesignError> {
    Ok(design_lowpass(LowpassSpec::new(
        sample_rate_hz,
        passband_hz,
        stopband_hz,
        stopband_db,
    ))?
    .taps)
}

/// Streaming FIR low-pass followed by keep-one-in-`factor` decimation. Only the kept outputs
/// are computed.
#[derive(Clone, Debug)]
pub struct FirDecimator<T: FirSample> {
    taps: Vec<f32>,
    /// History, stored twice so the newest `len` samples are always contiguous.
    hist: Vec<T>,
    pos: usize,
    factor: usize,
    phase: usize,
}

impl<T: FirSample> FirDecimator<T> {
    /// A decimator with the given taps and factor (≥ 1).
    pub fn new(taps: Vec<f32>, factor: usize) -> Self {
        let len = taps.len().max(1);
        Self {
            taps,
            hist: vec![T::default(); 2 * len],
            pos: 0,
            factor: factor.max(1),
            phase: 0,
        }
    }

    /// Clears the history and decimation phase, as if newly constructed (no allocation).
    pub fn clear(&mut self) {
        self.hist.fill(T::default());
        self.pos = 0;
        self.phase = 0;
    }

    /// Zeroes the history but keeps the decimation phase, so a filter cloned from a running
    /// one emits on the same input samples without inheriting its signal (no allocation).
    pub fn clear_history(&mut self) {
        self.hist.fill(T::default());
    }

    /// Group delay in input samples.
    pub fn group_delay(&self) -> f64 {
        (self.taps.len() as f64 - 1.0) / 2.0
    }

    /// Decimation factor.
    pub fn factor(&self) -> usize {
        self.factor
    }

    /// Pushes one input sample; returns an output every `factor` inputs. Output `k` is the
    /// filtered value at input index `(k + 1)·factor − 1` (before removing the group delay).
    #[inline]
    pub fn push(&mut self, x: T) -> Option<T> {
        let len = self.taps.len();
        self.hist[self.pos] = x;
        self.hist[self.pos + len] = x;
        self.pos = (self.pos + 1) % len;
        self.phase += 1;
        if self.phase < self.factor {
            return None;
        }
        self.phase = 0;
        // Oldest sample first: hist[pos..pos+len].
        let mut acc = T::default();
        for (h, &t) in self.hist[self.pos..self.pos + len].iter().zip(&self.taps) {
            acc += *h * t;
        }
        Some(acc)
    }
}

/// Quadrature (polar) discriminator: `arg(x[n]·conj(x[n−1]))·fs/2π`, in Hz of deviation.
#[derive(Clone, Copy, Debug)]
pub struct Discriminator {
    prev: Complex32,
    scale: f64,
}

impl Discriminator {
    /// A discriminator for samples at `sample_rate_hz`.
    pub fn new(sample_rate_hz: f64) -> Self {
        Self {
            prev: Complex32::new(0.0, 0.0),
            scale: sample_rate_hz / TAU,
        }
    }

    /// Instantaneous frequency of `x`, Hz.
    #[inline]
    pub fn push(&mut self, x: Complex32) -> f32 {
        let d = x * self.prev.conj();
        self.prev = x;
        (f64::from(d.im.atan2(d.re)) * self.scale) as f32
    }
}

/// Single-pole de-emphasis `H(s) = 1 / (1 + sτ)` (impulse-invariant).
#[derive(Clone, Copy, Debug)]
pub struct Deemphasis {
    a: f32,
    y: f32,
}

impl Deemphasis {
    /// τ seconds at `sample_rate_hz`. τ ≤ 0 is a pass-through.
    pub fn new(tau_s: f64, sample_rate_hz: f64) -> Self {
        let a = if tau_s > 0.0 {
            1.0 - (-1.0 / (tau_s * sample_rate_hz)).exp()
        } else {
            1.0
        };
        Self {
            a: a as f32,
            y: 0.0,
        }
    }

    /// Filters one sample.
    #[inline]
    pub fn push(&mut self, x: f32) -> f32 {
        self.y += self.a * (x - self.y);
        self.y
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimator_passes_dc_and_rejects_stopband() {
        let fs = 240_000.0;
        let taps = lowpass_taps(fs, 15_000.0, 19_000.0, 60.0).unwrap();
        let mut dc = FirDecimator::<f32>::new(taps.clone(), 5);
        let mut last = 0.0;
        for _ in 0..5000 {
            if let Some(y) = dc.push(1.0) {
                last = y;
            }
        }
        assert!((last - 1.0).abs() < 1e-3);
        let mut tone = FirDecimator::<f32>::new(taps, 5);
        let mut peak = 0.0f32;
        for n in 0..20_000 {
            let x = (TAU * 25_000.0 * n as f64 / fs).cos() as f32;
            if let Some(y) = tone.push(x)
                && n > 2000
            {
                peak = peak.max(y.abs());
            }
        }
        assert!(peak < 2e-3, "stopband leak {peak}");
    }

    #[test]
    fn decimator_clear_matches_a_fresh_decimator() {
        let taps = lowpass_taps(48_000.0, 3_000.0, 6_000.0, 60.0).unwrap();
        let mut used = FirDecimator::<f32>::new(taps.clone(), 3);
        for n in 0..1_001 {
            used.push((n as f32 * 0.37).sin());
        }
        used.clear();
        let mut fresh = FirDecimator::<f32>::new(taps, 3);
        for n in 0..500 {
            let x = (n as f32 * 0.11).cos();
            assert_eq!(used.push(x), fresh.push(x), "sample {n}");
        }
    }

    #[test]
    fn discriminator_reads_tone_frequency() {
        let fs = 240_000.0;
        let mut d = Discriminator::new(fs);
        let mut out = 0.0;
        for n in 0..100 {
            let ph = TAU * 12_345.0 * n as f64 / fs;
            out = d.push(Complex32::new(ph.cos() as f32, ph.sin() as f32));
        }
        assert!((out - 12_345.0).abs() < 0.5, "{out}");
    }
}
