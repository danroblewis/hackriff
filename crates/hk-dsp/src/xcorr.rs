//! Normalised cross-correlation with a lag search (T-222, C40 content half).
//!
//! Two measurements of **one** emission arriving over two paths carry the same content, one
//! delayed and attenuated. The measurement that says so is the correlation of the two content
//! series against each other over a range of lags: one dominant peak, at the lag that is the path
//! delay. Two *different* emissions — even two stations of the same family, the same bandwidth and
//! the same modulation — carry different content, and their series do not correlate at any lag.
//!
//! # Why Pearson, per lag, over the overlap only
//!
//! The series are **not** comparable in absolute units: one path is attenuated, the two rows may
//! be measured at different SNRs, and a content series may be an envelope in dB, a discriminator
//! output or a bit stream. What is comparable is *shape*, so each lag is scored by the Pearson
//! correlation of the two series over the samples they have in common at that lag, with the mean
//! and the scale removed **at that lag** — not once over the whole series, which would let the
//! non-overlapping tails set the normalisation and make long lags look better than short ones for
//! no physical reason.
//!
//! A constant series (silence, or a saturated one) has no shape to match; its variance is zero and
//! [`normalized_xcorr`] scores that lag `0.0` rather than dividing by it.
//!
//! # The runner-up is part of the measurement, not a diagnostic
//!
//! A periodic content series correlates with itself at every multiple of its period, so a high
//! peak alone does not say *which* lag the path delay is. [`Xcorr::runner_up`] is therefore the
//! best peak at least [`PEAK_GUARD_FRACTION`] of the lag range away from the winner, and the
//! caller decides — `hk_model::multipath` refuses a claim whose peak does not dominate its
//! runner-up, because a delay it cannot pin down is not a path difference it can report.
//!
//! Cost is `O(n · lags)` with no allocation beyond the returned peaks, which is what lets the
//! caller bound it by capping both.

/// Share of the searched lag range that separates the peak from its runner-up: a second maximum
/// closer than this is the same peak's shoulder, not a competing hypothesis.
pub const PEAK_GUARD_FRACTION: f64 = 0.05;

/// Smallest guard, in lags, whatever the range: a one-sample-wide peak always has a shoulder.
pub const PEAK_GUARD_MIN_LAGS: usize = 2;

/// One local maximum of the lag search.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct XcorrPeak {
    /// Lag in samples, positive when `b` is delayed relative to `a`.
    pub lag: isize,
    /// Pearson correlation there, −1..=1.
    pub value: f64,
    /// Samples the two series had in common at that lag.
    pub overlap: usize,
}

/// The result of a lag search.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Xcorr {
    /// Strongest correlation found.
    pub peak: XcorrPeak,
    /// Best peak well away from it (see the module docs), when the range holds one.
    pub runner_up: Option<XcorrPeak>,
    /// Lags searched, `±max_lag`.
    pub lags: usize,
}

impl Xcorr {
    /// How far the peak stands above its runner-up: `peak / runner_up`, and [`f64::INFINITY`] when
    /// no runner-up exists or it is at or below zero. Never negative.
    pub fn dominance(&self) -> f64 {
        match self.runner_up {
            Some(r) if r.value > 0.0 && self.peak.value > 0.0 => self.peak.value / r.value,
            _ if self.peak.value > 0.0 => f64::INFINITY,
            _ => 0.0,
        }
    }
}

/// Pearson correlation of `a` and `b` over the samples they share when `b` is shifted by `lag`,
/// or `None` when fewer than `min_overlap` samples (or a constant series) leave nothing to
/// measure.
///
/// `lag > 0` means `b` **lags** `a`: `a[i + lag]` is compared with `b[i]`.
fn at_lag(a: &[f32], b: &[f32], lag: isize, min_overlap: usize) -> Option<XcorrPeak> {
    // The index ranges that overlap: i indexes b, i + lag indexes a.
    let lo = 0isize.max(-lag);
    let hi = (b.len() as isize).min(a.len() as isize - lag);
    if hi <= lo {
        return None;
    }
    let n = (hi - lo) as usize;
    if n < min_overlap.max(2) {
        return None;
    }
    let (mut sa, mut sb) = (0f64, 0f64);
    for i in lo..hi {
        sa += f64::from(a[(i + lag) as usize]);
        sb += f64::from(b[i as usize]);
    }
    let (ma, mb) = (sa / n as f64, sb / n as f64);
    let (mut saa, mut sbb, mut sab) = (0f64, 0f64, 0f64);
    for i in lo..hi {
        let (x, y) = (
            f64::from(a[(i + lag) as usize]) - ma,
            f64::from(b[i as usize]) - mb,
        );
        saa += x * x;
        sbb += y * y;
        sab += x * y;
    }
    // A flat series has no shape to match. Score it zero rather than dividing by zero: "nothing
    // correlates here", which is the honest reading, and one the caller's threshold rejects.
    let denom = (saa * sbb).sqrt();
    let value = if denom > 0.0 && denom.is_finite() {
        (sab / denom).clamp(-1.0, 1.0)
    } else {
        0.0
    };
    Some(XcorrPeak {
        lag,
        value,
        overlap: n,
    })
}

/// Searches `±max_lag` samples for the lag at which `a` and `b` correlate best, reporting the peak
/// and the best competing peak away from it (see the module docs).
///
/// `None` when no lag in the range leaves `min_overlap` samples in common — the two series were
/// not observed over a shared enough window to be compared at all, which is an abstention and not
/// a "they do not match".
pub fn normalized_xcorr(a: &[f32], b: &[f32], max_lag: usize, min_overlap: usize) -> Option<Xcorr> {
    let max_lag = max_lag.min(a.len().max(b.len()));
    let mut peaks: Vec<XcorrPeak> = Vec::with_capacity(2 * max_lag + 1);
    for lag in -(max_lag as isize)..=(max_lag as isize) {
        if let Some(p) = at_lag(a, b, lag, min_overlap) {
            peaks.push(p);
        }
    }
    let peak = *peaks.iter().max_by(|x, y| {
        x.value
            .total_cmp(&y.value)
            .then(y.lag.abs().cmp(&x.lag.abs()))
    })?;
    let guard = ((max_lag as f64 * PEAK_GUARD_FRACTION).ceil() as usize).max(PEAK_GUARD_MIN_LAGS);
    let runner_up = peaks
        .iter()
        .filter(|p| (p.lag - peak.lag).unsigned_abs() > guard)
        .max_by(|x, y| x.value.total_cmp(&y.value))
        .copied();
    Some(Xcorr {
        peak,
        runner_up,
        lags: max_lag,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic pseudo-random content series: no test may depend on an RNG crate here.
    fn series(n: usize, seed: u64) -> Vec<f32> {
        let mut s = seed | 1;
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                ((s >> 40) as f32 / 8_388_608.0) - 1.0
            })
            .collect()
    }

    #[test]
    fn a_delayed_attenuated_copy_peaks_at_the_delay() {
        let a = series(2000, 0xC40);
        let delay = 37usize;
        // The copy: same content, delayed, a quarter the amplitude, with an offset — neither the
        // scale nor the offset may move the peak, which is the point of normalising per lag.
        let b: Vec<f32> = (0..a.len())
            .map(|i| {
                if i >= delay {
                    0.25 * a[i - delay] + 3.0
                } else {
                    3.0
                }
            })
            .collect();
        let x = normalized_xcorr(&a, &b, 200, 64).expect("overlap");
        assert_eq!(x.peak.lag, -(delay as isize), "b is delayed by {delay}");
        assert!(x.peak.value > 0.9, "peak {:?}", x.peak);
        assert!(x.dominance() > 2.0, "dominant: {x:?}");
    }

    #[test]
    fn independent_series_do_not_correlate_at_any_lag() {
        let a = series(2000, 0xAAA);
        let b = series(2000, 0xBBB);
        let x = normalized_xcorr(&a, &b, 200, 64).expect("overlap");
        assert!(
            x.peak.value < 0.3,
            "two different contents must not match: {:?}",
            x.peak
        );
    }

    #[test]
    fn a_periodic_series_has_no_dominant_lag() {
        // Self-similar every 50 samples: the peak is real but so are its repeats, so the search
        // must report a runner-up that denies it dominance.
        let a: Vec<f32> = (0..2000).map(|i| (i % 50) as f32 / 50.0).collect();
        let x = normalized_xcorr(&a, &a, 200, 64).expect("overlap");
        assert!((x.peak.value - 1.0).abs() < 1e-9);
        assert!(
            x.dominance() < 1.2,
            "a periodic series must not pin a lag: {x:?}"
        );
    }

    #[test]
    fn a_constant_series_scores_zero_rather_than_dividing_by_zero() {
        let a = vec![1.0f32; 500];
        let b = series(500, 7);
        let x = normalized_xcorr(&a, &b, 50, 16).expect("overlap");
        assert_eq!(x.peak.value, 0.0);
    }

    #[test]
    fn too_little_overlap_abstains() {
        let a = series(10, 1);
        let b = series(10, 2);
        assert!(normalized_xcorr(&a, &b, 5, 64).is_none());
    }

    #[test]
    fn empty_input_abstains() {
        assert!(normalized_xcorr(&[], &[], 4, 1).is_none());
        assert!(normalized_xcorr(&series(10, 1), &[], 4, 1).is_none());
    }
}
