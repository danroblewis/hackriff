//! A sliding sample window for FIR stages: the last `keep` samples, contiguous, with converted
//! input appended in place (no per-block allocation).

use num_complex::Complex32;

use super::kernels::convert_into;
use crate::stft::IqSample;

/// The newest `keep` samples of a stream as one contiguous slice.
///
/// Storage is a linear buffer with slack; when appending would overflow it, the newest `keep`
/// samples move to the front. With slack ≥ 3·`keep` the amortised copy cost is ≤ 1/3 sample
/// per sample appended.
pub(crate) struct History {
    buf: Vec<Complex32>,
    end: usize,
    keep: usize,
    max_push: usize,
}

impl History {
    /// A window of `keep` samples fed at most `max_push` samples per [`History::push`].
    pub(crate) fn new(keep: usize, max_push: usize) -> Self {
        let keep = keep.max(1);
        let max_push = max_push.max(1);
        let cap = keep + max_push.max(3 * keep);
        Self {
            buf: vec![Complex32::default(); cap],
            end: 0,
            keep,
            max_push,
        }
    }

    /// Forgets all samples.
    pub(crate) fn clear(&mut self) {
        self.end = 0;
    }

    fn make_room(&mut self, n: usize) {
        if self.end + n > self.buf.len() {
            let keep = self.keep.min(self.end);
            self.buf.copy_within(self.end - keep..self.end, 0);
            self.end = keep;
        }
    }

    /// Appends up to `max_push` samples, converting to `Complex32`.
    #[inline]
    pub(crate) fn push<T: IqSample>(&mut self, src: &[T]) {
        debug_assert!(src.len() <= self.max_push);
        self.make_room(src.len());
        convert_into(&mut self.buf[self.end..self.end + src.len()], src);
        self.end += src.len();
    }

    /// Appends one sample.
    #[inline]
    pub(crate) fn push_one(&mut self, s: Complex32) {
        self.make_room(1);
        self.buf[self.end] = s;
        self.end += 1;
    }

    /// The newest `keep` samples, oldest first. Only valid once `keep` samples were pushed
    /// since the last [`History::clear`].
    #[inline]
    pub(crate) fn window(&self) -> &[Complex32] {
        debug_assert!(self.end >= self.keep);
        &self.buf[self.end - self.keep..self.end]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_tracks_newest_samples_across_compaction() {
        let mut h = History::new(5, 3);
        let mut all = Vec::new();
        for i in 0..100u32 {
            let s = Complex32::new(i as f32, 0.0);
            all.push(s);
            if i % 2 == 0 {
                h.push_one(s);
            } else {
                h.push(&[s]);
            }
            if all.len() >= 5 {
                assert_eq!(h.window(), &all[all.len() - 5..]);
            }
        }
    }
}
