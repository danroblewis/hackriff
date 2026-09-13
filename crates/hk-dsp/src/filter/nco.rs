//! An exact phase-increment NCO for mixing referenced to absolute stream sample indices.
//!
//! The phase of sample `n` is `n · inc mod 2^64` turns, computed with wrapping `u64`
//! arithmetic, so it is exact at any index and identical however the stream is chunked. The
//! frequency is quantised to `fs / 2^64` (≈ 1e−12 Hz at 20 Msps). Only the conversion of the
//! 64-bit phase to an angle uses floating point, and that has uniform 2^−53-turn resolution. By
//! contrast `f64` `cycles_per_sample · n` loses precision as `n` grows: about −76 dB at 24 h of
//! 20 Msps samples, and it collapses near `n ≈ 4.5e15`.

use std::f64::consts::PI;

use num_complex::Complex32;

const TWO_POW_64: f64 = 18_446_744_073_709_551_616.0;

/// Exact `u64` phase-increment oscillator. See the [module docs](self).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Nco {
    inc: u64,
}

impl Nco {
    /// An NCO at `cycles_per_sample` (`f / fs`; any real, wrapped into [0, 1)).
    pub fn new(cycles_per_sample: f64) -> Self {
        let r = cycles_per_sample.rem_euclid(1.0);
        // `as` saturates; a value that rounds to 2^64 is one quantum below a full turn.
        Self {
            inc: (r * TWO_POW_64).round() as u64,
        }
    }

    /// An NCO from a raw 64-bit phase increment (turns · 2^64 per sample).
    pub fn from_increment(inc: u64) -> Self {
        Self { inc }
    }

    /// The raw phase increment.
    pub fn increment(&self) -> u64 {
        self.inc
    }

    /// The quantised frequency in turns per sample, in [0, 1).
    pub fn cycles_per_sample(&self) -> f64 {
        self.inc as f64 / TWO_POW_64
    }

    /// Phase of sample `n` in turns · 2^64.
    #[inline]
    pub fn phase(&self, n: u64) -> u64 {
        n.wrapping_mul(self.inc)
    }

    /// Phase of sample `n` in turns, in [0, 1).
    #[inline]
    pub fn turns(&self, n: u64) -> f64 {
        self.phase(n) as f64 / TWO_POW_64
    }

    /// The down-mixing rotator `e^{−j2π·phase(n)}`.
    #[inline]
    pub fn rotator(&self, n: u64) -> Complex32 {
        let (s, c) = (-2.0 * PI * self.turns(n)).sin_cos();
        Complex32::new(c as f32, s as f32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_is_exact_at_huge_indices() {
        // 3/16 turn per sample: every 16th sample is back at phase 0, at any index.
        let nco = Nco::new(3.0 / 16.0);
        assert_eq!(nco.increment(), 3u64 << 60);
        for n in [0u64, 16, 1 << 50, (1 << 62) + 32, u64::MAX - 15] {
            assert_eq!(nco.phase(n) % (1 << 60), 0, "n = {n}");
            assert_eq!(nco.phase(n), nco.phase(n % 16), "n = {n}");
        }
        let neg = Nco::new(-0.25);
        assert_eq!(neg.increment(), 3u64 << 62);
        assert!((neg.rotator(1) - Complex32::new(0.0, -1.0)).norm() > 1.9);
        assert!((Nco::new(0.25).rotator(1) - Complex32::new(0.0, -1.0)).norm() < 1e-6);
    }
}
