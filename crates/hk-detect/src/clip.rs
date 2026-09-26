//! ADC clip counting for rule 8 (S4: a ci8 sample is clipped when either component sits on a
//! rail, −128 or 127). The STFT does not carry clip counts, so the caller counts the samples of
//! each frame's span and passes a [`ClipCount`] with the frame.

use std::collections::VecDeque;

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

/// Samples per ADC-peak cell in a [`ClipLedger`]: the resolution a span's peak is known to.
pub const PEAK_CELL: usize = 1024;

/// A span's clip count and ADC peak ([`ClipLedger::span`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SpanClip {
    /// Clipped samples in the span (exact).
    pub clip: ClipCount,
    /// Largest component magnitude in ADC codes, `0..=128` (128 is the −128 rail), over the
    /// [`PEAK_CELL`]-sample cells the span touches — so it may include up to one cell either side.
    pub peak_code: u8,
}

impl SpanClip {
    /// The peak as a fraction of full scale (`peak_code / 128`).
    pub fn adc_peak(&self) -> f64 {
        f64::from(self.peak_code) / 128.0
    }
}

/// T-981: per-span clip counts and ADC peaks over a raw ci8 stream, for a reader whose frames do
/// not line up with its chunks (the spectrum rows). [`Self::push`] each chunk as it is read, then
/// ask [`Self::span`] for each frame in stream order; everything before the asked span is dropped,
/// so the ledger holds at most the samples not yet framed.
#[derive(Clone, Debug, Default)]
pub struct ClipLedger {
    clips: VecDeque<u64>,
    /// `(first, end, peak_code)` per [`PEAK_CELL`]-sample cell.
    peaks: VecDeque<(u64, u64, u8)>,
}

impl ClipLedger {
    /// Records `samples`, whose first sample has stream index `first`.
    pub fn push(&mut self, first: u64, samples: &[Complex<i8>]) {
        for (k, cell) in samples.chunks(PEAK_CELL).enumerate() {
            let a = first + (k * PEAK_CELL) as u64;
            let mut peak = 0u8;
            for (i, &s) in cell.iter().enumerate() {
                peak = peak.max(s.re.unsigned_abs()).max(s.im.unsigned_abs());
                if is_clipped_ci8(s) {
                    self.clips.push_back(a + i as u64);
                }
            }
            self.peaks.push_back((a, a + cell.len() as u64, peak));
        }
    }

    /// The clip count and peak of `[a, b)`; forgets everything before `a`.
    pub fn span(&mut self, a: u64, b: u64) -> SpanClip {
        while self.clips.front().is_some_and(|&i| i < a) {
            self.clips.pop_front();
        }
        while self.peaks.front().is_some_and(|&(_, end, _)| end <= a) {
            self.peaks.pop_front();
        }
        let clipped = self.clips.partition_point(|&i| i < b) as u64;
        let peak_code = self
            .peaks
            .iter()
            .take_while(|&&(first, _, _)| first < b)
            .map(|&(_, _, p)| p)
            .max()
            .unwrap_or(0);
        SpanClip {
            clip: ClipCount::new(clipped, b.saturating_sub(a)),
            peak_code,
        }
    }

    /// Forgets everything (a discontinuity: the next span starts a new stream position).
    pub fn clear(&mut self) {
        self.clips.clear();
        self.peaks.clear();
    }
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

    #[test]
    fn the_ledger_counts_each_span_exactly_and_forgets_what_is_behind_it() {
        let mut l = ClipLedger::default();
        let mut s = vec![Complex::new(3i8, -4); 3000];
        s[10] = Complex::new(127, 0);
        s[2500] = Complex::new(0, -128);
        s[1500] = Complex::new(-90, 5);
        l.push(0, &s);
        let a = l.span(0, 1024);
        assert_eq!(a.clip, ClipCount::new(1, 1024));
        assert_eq!(a.peak_code, 127);
        let b = l.span(1024, 2048);
        assert_eq!(b.clip, ClipCount::new(0, 1024));
        assert_eq!(b.peak_code, 90);
        let c = l.span(2048, 3000);
        assert_eq!(c.clip.clipped, 1);
        assert_eq!(c.peak_code, 128);
        assert!((c.adc_peak() - 1.0).abs() < 1e-12);
        // Behind the last span: gone.
        assert_eq!(l.span(0, 1024).clip.clipped, 0);
    }
}
