//! DPX-style persistence: a bins × power-levels hit histogram with exponential decay.
//!
//! Every raw FFT segment (not the averaged frame) adds one hit per bin at the level its power
//! falls in, and the whole image decays with time constant `τ`:
//!
//! ```text
//! H[bin, level] ← β·H[bin, level] + hit,   β = exp(−Δt/τ),  Δt = segment hop / fs
//! ```
//!
//! A bin hit every segment converges to `1/(1 − β) ≈ τ/Δt`; a single burst fades by `e` every
//! `τ`. Levels are dBFS per RBW (tone-calibrated, [`PowerUnit::DbfsPerBin`]), uniformly spaced
//! over `[min_db, max_db)`; powers outside the range clamp to the first or last level.
//!
//! **Cost:** O(bins) per segment. Decay is lazy: instead of multiplying every cell by `β`, the
//! hit increment grows by `1/β` each segment and the image is renormalised (one O(bins × levels)
//! pass) only when the increment exceeds 1000, i.e. every `≈ 6.9·τ/Δt` segments. The dB level
//! uses a 256-entry log2 table (error < 0.01 dB), not `log10`. The UI may still do its own
//! persistence on the GPU (spike S3); this is the core-side option.
//!
//! [`PowerUnit::DbfsPerBin`]: crate::spectrum::PowerUnit::DbfsPerBin

/// Persistence settings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PersistenceConfig {
    /// Number of power levels (>= 1).
    pub levels: usize,
    /// Lower edge of the first level, dBFS per RBW.
    pub min_db: f32,
    /// Upper edge of the last level, dBFS per RBW (> `min_db`).
    pub max_db: f32,
    /// Decay time constant, seconds (> 0; `f64::INFINITY` = no decay).
    pub tau_s: f64,
}

impl Default for PersistenceConfig {
    fn default() -> Self {
        Self {
            levels: 100,
            min_db: -120.0,
            max_db: 0.0,
            tau_s: 0.5,
        }
    }
}

impl PersistenceConfig {
    /// Checks the settings; returns a reason on failure.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.levels == 0 {
            return Err("persistence levels must be >= 1");
        }
        if !(self.min_db.is_finite() && self.max_db.is_finite() && self.max_db > self.min_db) {
            return Err("persistence max_db must be finite and above min_db");
        }
        if self.tau_s.is_nan() || self.tau_s <= 0.0 {
            return Err("persistence tau_s must be > 0");
        }
        Ok(())
    }
}

/// Renormalise once the lazy-decay increment exceeds this.
const RENORM_GAIN: f32 = 1e3;
/// Cells below this after renormalisation are zeroed (avoids subnormal floats).
const FLOOR: f32 = 1e-20;
/// `10·log10(2)`.
const DB_PER_LOG2: f32 = 3.010_3;

/// A decaying persistence image. See the [module docs](self).
#[derive(Clone, Debug)]
pub struct Persistence {
    bins: usize,
    config: PersistenceConfig,
    inv_step: f32,
    beta: f32,
    /// Cells scaled by `gain`: the true value is `acc / gain`.
    acc: Vec<f32>,
    gain: f32,
    lut: [f32; 256],
    updates: u64,
}

impl Persistence {
    /// An empty image of `bins` × `config.levels`, updated once per `frame_period_s` seconds.
    ///
    /// # Panics
    /// If `config` is invalid (see [`PersistenceConfig::validate`]).
    pub fn new(bins: usize, config: PersistenceConfig, frame_period_s: f64) -> Self {
        if let Err(why) = config.validate() {
            panic!("{why}");
        }
        let mut lut = [0f32; 256];
        for (i, v) in lut.iter_mut().enumerate() {
            *v = (1.0 + (i as f64 + 0.5) / 256.0).log2() as f32;
        }
        let mut p = Self {
            bins,
            config,
            inv_step: config.levels as f32 / (config.max_db - config.min_db),
            beta: 1.0,
            acc: vec![0.0; bins * config.levels],
            gain: 1.0,
            lut,
            updates: 0,
        };
        p.set_frame_period(frame_period_s);
        p
    }

    /// Changes the update period (e.g. after a sample-rate change); keeps the image.
    pub fn set_frame_period(&mut self, frame_period_s: f64) {
        self.beta = (-frame_period_s / self.config.tau_s).exp() as f32;
    }

    /// Per-update decay factor `β`.
    pub fn beta(&self) -> f32 {
        self.beta
    }

    /// Number of bins.
    pub fn bins(&self) -> usize {
        self.bins
    }

    /// Number of levels.
    pub fn levels(&self) -> usize {
        self.config.levels
    }

    /// Settings.
    pub fn config(&self) -> &PersistenceConfig {
        &self.config
    }

    /// Updates applied since creation or [`Persistence::clear`].
    pub fn updates(&self) -> u64 {
        self.updates
    }

    /// Lower edge of `level`, dB.
    pub fn level_db(&self, level: usize) -> f32 {
        self.config.min_db + level as f32 / self.inv_step
    }

    /// Clears the image.
    pub fn clear(&mut self) {
        self.acc.fill(0.0);
        self.gain = 1.0;
        self.updates = 0;
    }

    /// Adds one segment: `power[bin]` is linear power, and its level is
    /// `10·log10(power) + offset_db`. Allocation-free.
    pub fn update(&mut self, power: &[f32], offset_db: f32) {
        assert_eq!(power.len(), self.bins, "power length != persistence bins");
        if self.beta < 1.0 {
            self.gain /= self.beta;
            if self.gain > RENORM_GAIN {
                let inv = 1.0 / self.gain;
                for v in &mut self.acc {
                    *v *= inv;
                    if *v < FLOOR {
                        *v = 0.0;
                    }
                }
                self.gain = 1.0;
            }
        }
        let levels = self.config.levels;
        let top = (levels - 1) as f32;
        let (min_db, inv_step, gain) = (self.config.min_db, self.inv_step, self.gain);
        for (row, &p) in self.acc.chunks_exact_mut(levels).zip(power) {
            let db = fast_log2(p, &self.lut) * DB_PER_LOG2 + offset_db;
            let level = ((db - min_db) * inv_step).clamp(0.0, top) as usize;
            row[level] += gain;
        }
        self.updates += 1;
    }

    /// Decayed hit count at `(bin, level)`.
    pub fn value(&self, bin: usize, level: usize) -> f32 {
        self.acc[bin * self.config.levels + level] / self.gain
    }

    /// Writes the image row-major (`out[bin * levels + level]`), decayed hit counts.
    pub fn write_image(&self, out: &mut [f32]) {
        assert_eq!(out.len(), self.acc.len(), "image buffer size mismatch");
        let inv = 1.0 / self.gain;
        for (o, &a) in out.iter_mut().zip(&self.acc) {
            *o = a * inv;
        }
    }
}

/// `log2(x)` for positive finite `x`, via the exponent bits and a 256-entry mantissa table.
/// Zero and subnormals return −1100 (≈ −3300 dB); infinities and NaN return large values.
#[inline]
fn fast_log2(x: f32, lut: &[f32; 256]) -> f32 {
    let bits = x.to_bits();
    let exp = ((bits >> 23) & 0xff) as i32;
    if exp == 0 {
        return -1100.0;
    }
    (exp - 127) as f32 + lut[((bits >> 15) & 0xff) as usize]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_log2_is_accurate() {
        let p = Persistence::new(1, PersistenceConfig::default(), 1e-3);
        for &x in &[1e-12f32, 3.3e-7, 0.01, 0.5, 1.0, 1.7, 123.4, 9.9e8] {
            let err_db = (fast_log2(x, &p.lut) - x.log2()).abs() * DB_PER_LOG2;
            assert!(err_db < 0.01, "{x}: {err_db} dB");
        }
        assert!(fast_log2(0.0, &p.lut) < -1000.0);
    }
}
