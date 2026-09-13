//! ADC clip counting for rule 8 (S4: a ci8 sample is clipped when either component sits on a
//! rail, −128 or 127). The STFT does not carry clip counts, so the caller counts the samples of
//! each frame's span and passes a [`ClipCount`] with the frame.

use num_complex::Complex;

/// Clipped samples within one frame's span.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClipCount {
    /// Samples with a component on a rail.
    pub clipped: u64,
    /// Samples in the span.
    pub samples: u64,
}

impl ClipCount {
    /// No clip information (fraction 0).
    pub const NONE: ClipCount = ClipCount {
        clipped: 0,
        samples: 0,
    };

    /// `clipped` of `samples`.
    pub const fn new(clipped: u64, samples: u64) -> Self {
        Self { clipped, samples }
    }

    /// Clip fraction (0 when `samples` is 0).
    pub fn fraction(&self) -> f64 {
        if self.samples == 0 {
            0.0
        } else {
            self.clipped as f64 / self.samples as f64
        }
    }
}

/// Whether a ci8 sample has a component on a rail.
#[inline]
pub fn is_clipped_ci8(s: Complex<i8>) -> bool {
    s.re == i8::MIN || s.re == i8::MAX || s.im == i8::MIN || s.im == i8::MAX
}

/// Clipped samples in a ci8 slice.
pub fn count_clipped_ci8(samples: &[Complex<i8>]) -> u64 {
    samples.iter().filter(|&&s| is_clipped_ci8(s)).count() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rails_count_once_per_sample() {
        let s = [
            Complex::new(0i8, 0),
            Complex::new(127, 0),
            Complex::new(-128, 127),
            Complex::new(126, -127),
        ];
        assert_eq!(count_clipped_ci8(&s), 2);
        assert_eq!(ClipCount::new(2, 4).fraction(), 0.5);
        assert_eq!(ClipCount::NONE.fraction(), 0.0);
    }
}
