//! Per-bin minimum statistics (Martin 2001): drift tracking and idle-time floors only.
//!
//! **Not a CFAR or detection reference.** A bin that is never idle within the window — any
//! continuous carrier, control channel or broadcast station — reads its signal as floor. S4
//! §3.1 measured +4 to +14 dB on emitter bins of real FM captures, while block FCME stayed
//! unbiased. Use [`NoiseFloorTracker`](super::NoiseFloorTracker)'s per-frame FCME floor for
//! thresholds; use this for the slowly varying floor of *intermittent* channels (a channel only
//! has to be idle sometime in the window) and for drift studies. Choose the window from the band
//! profile: several × the longest transmission. Reset it on any gain/tune change: its window is
//! meaningless across gain states.
//!
//! Per bin: `S_k = α·S_{k−1} + (1 − α)·P_k` (seeded with the first frame), the minimum of `S`
//! over the last `window_frames` frames (tracked as `subwindows` sub-window minima, so the
//! effective window varies between `(U−1)·V + 1` and `U·V` frames, `V = ⌈W/U⌉`), divided by a
//! bias factor `B = E[min]/μ` measured by Monte Carlo on `Gamma(n)` noise through the same
//! algorithm ([`monte_carlo_bias`]). Per frame cost `O(bins)`; no allocation after construction.

use super::FloorConfigError;
use super::gamma;
use crate::synth::Rng;

/// Minimum-statistics settings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MinStatConfig {
    /// IIR smoothing `α` per frame (default 0.7, as S4).
    pub smoothing: f64,
    /// Minimum window, frames (default 256).
    pub window_frames: usize,
    /// Sub-windows `U` (≥ 2, default 8).
    pub subwindows: usize,
    /// Bias factor override; `None` runs [`monte_carlo_bias`] at construction.
    pub bias: Option<f64>,
    /// Monte-Carlo bins simulated for the bias (default 256).
    pub bias_trials: usize,
    /// Monte-Carlo seed.
    pub bias_seed: u64,
}

impl Default for MinStatConfig {
    fn default() -> Self {
        Self {
            smoothing: 0.7,
            window_frames: 256,
            subwindows: 8,
            bias: None,
            bias_trials: 256,
            bias_seed: 0x05ee_d005,
        }
    }
}

impl MinStatConfig {
    /// Checks the settings.
    pub fn validate(&self) -> Result<(), FloorConfigError> {
        if !(0.0..1.0).contains(&self.smoothing) {
            return Err(FloorConfigError::Fraction {
                name: "minstat.smoothing",
                value: self.smoothing,
            });
        }
        if self.subwindows < 2 || self.window_frames < self.subwindows {
            return Err(FloorConfigError::Subwindows {
                subwindows: self.subwindows,
                window_frames: self.window_frames,
            });
        }
        if let Some(b) = self.bias {
            if !(b > 0.0 && b.is_finite()) {
                return Err(FloorConfigError::NonPositive {
                    name: "minstat.bias",
                    value: b,
                });
            }
        }
        Ok(())
    }

    fn frames_per_subwindow(&self) -> usize {
        self.window_frames.div_ceil(self.subwindows)
    }
}

/// Per-bin minimum-statistics floor. See the [module docs](self): never a CFAR reference.
#[derive(Clone, Debug)]
pub struct MinStatistics {
    config: MinStatConfig,
    bins: usize,
    bias: f64,
    smoothed: Vec<f32>,
    current: Vec<f32>,
    slots: Vec<f32>,
    slots_min: Vec<f32>,
    out: Vec<f32>,
    frames: u64,
    in_subwindow: usize,
    slot: usize,
}

impl MinStatistics {
    /// An estimator over `bins` bins of `n_avg`-average spectra.
    pub fn new(config: MinStatConfig, bins: usize, n_avg: f64) -> Result<Self, FloorConfigError> {
        config.validate()?;
        if bins == 0 {
            return Err(FloorConfigError::NoBins);
        }
        let bias = match config.bias {
            Some(b) => b,
            None => monte_carlo_bias(&config, n_avg),
        };
        Ok(Self::with_bias(config, bins, bias))
    }

    fn with_bias(config: MinStatConfig, bins: usize, bias: f64) -> Self {
        let stored = config.subwindows - 1;
        Self {
            config,
            bins,
            bias,
            smoothed: vec![0.0; bins],
            current: vec![f32::INFINITY; bins],
            slots: vec![f32::INFINITY; bins * stored],
            slots_min: vec![f32::INFINITY; bins],
            out: vec![0.0; bins],
            frames: 0,
            in_subwindow: 0,
            slot: 0,
        }
    }

    /// The bias factor `B` in use.
    pub fn bias(&self) -> f64 {
        self.bias
    }

    /// Frames folded in since the last reset.
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// True once a full window has been seen.
    pub fn is_ready(&self) -> bool {
        self.frames >= self.config.window_frames as u64
    }

    /// Clears the state (call on gain/tune changes).
    pub fn reset(&mut self) {
        self.current.fill(f32::INFINITY);
        self.slots.fill(f32::INFINITY);
        self.slots_min.fill(f32::INFINITY);
        self.frames = 0;
        self.in_subwindow = 0;
        self.slot = 0;
    }

    /// Folds in one linear PSD frame.
    pub fn update(&mut self, psd: &[f32]) {
        assert_eq!(psd.len(), self.bins, "bin count mismatch");
        let a = self.config.smoothing as f32;
        if self.frames == 0 {
            self.smoothed.copy_from_slice(psd);
        } else {
            for (s, &p) in self.smoothed.iter_mut().zip(psd) {
                *s = a * *s + (1.0 - a) * p;
            }
        }
        let inv_bias = (1.0 / self.bias) as f32;
        for (((c, &s), o), &m) in self
            .current
            .iter_mut()
            .zip(&self.smoothed)
            .zip(&mut self.out)
            .zip(&self.slots_min)
        {
            *c = c.min(s);
            *o = c.min(m) * inv_bias;
        }
        self.in_subwindow += 1;
        if self.in_subwindow == self.config.frames_per_subwindow() {
            let stored = self.config.subwindows - 1;
            let start = self.slot * self.bins;
            self.slots[start..start + self.bins].copy_from_slice(&self.current);
            self.slot = (self.slot + 1) % stored;
            self.slots_min.fill(f32::INFINITY);
            for chunk in self.slots.chunks_exact(self.bins) {
                for (m, &v) in self.slots_min.iter_mut().zip(chunk) {
                    *m = m.min(v);
                }
            }
            self.current.fill(f32::INFINITY);
            self.in_subwindow = 0;
        }
        self.frames += 1;
    }

    /// The bias-compensated per-bin floor (linear), valid after [`is_ready`](Self::is_ready).
    pub fn floor(&self) -> &[f32] {
        &self.out
    }
}

/// `E[min]/μ` of the minimum-statistics output on `Gamma(round(n_avg))` noise with unit mean,
/// measured by running the estimator itself over `bias_trials` simulated bins for three windows
/// and averaging the last window. Deterministic for a given seed. (The Gamma shape is rounded to
/// an integer for sampling.) Allocates; runs at construction, not per frame.
pub fn monte_carlo_bias(config: &MinStatConfig, n_avg: f64) -> f64 {
    let trials = config.bias_trials.max(16);
    let shape = n_avg.round().max(1.0) as u32;
    let mut est = MinStatistics::with_bias(*config, trials, 1.0);
    let mut rng = Rng::new(config.bias_seed);
    let mut frame = vec![0.0f32; trials];
    let w = config.window_frames;
    let (mut acc, mut count) = (0.0f64, 0usize);
    for k in 0..3 * w {
        for x in &mut frame {
            *x = gamma::sample_unit_mean(&mut rng, shape) as f32;
        }
        est.update(&frame);
        if k >= 2 * w {
            acc += est.floor().iter().map(|&v| f64::from(v)).sum::<f64>() / trials as f64;
            count += 1;
        }
    }
    acc / count as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bias_compensates_on_noise() {
        let cfg = MinStatConfig {
            window_frames: 64,
            ..MinStatConfig::default()
        };
        let mut m = MinStatistics::new(cfg, 512, 10.0).unwrap();
        assert!(m.bias() < 1.0 && m.bias() > 0.5, "{}", m.bias());
        let mut rng = Rng::new(99);
        let mut frame = vec![0.0f32; 512];
        let mut acc = 0.0;
        let mut n = 0;
        for k in 0..400 {
            for x in &mut frame {
                *x = 4.0 * gamma::sample_unit_mean(&mut rng, 10) as f32;
            }
            m.update(&frame);
            if k >= 128 {
                acc += m.floor().iter().map(|&v| f64::from(v)).sum::<f64>() / 512.0;
                n += 1;
            }
        }
        let err_db = 10.0 * (acc / n as f64 / 4.0).log10();
        assert!(
            err_db.abs() < 0.2,
            "min-stat bias after compensation {err_db} dB"
        );
        m.reset();
        assert!(!m.is_ready());
    }
}
