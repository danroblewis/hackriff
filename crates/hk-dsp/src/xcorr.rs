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
    /// Lag in samples, in the sense `a[i + lag]` is compared with `b[i]` — so a **positive** lag
    /// means `b`'s content sits EARLIER on the shared grid (`b` arrives first), and a delayed `b`
    /// peaks at a **negative** lag. Stated this way round because it is the index algebra
    /// [`normalized_xcorr`] actually performs; a caller that wants "how late is b" negates it, once
    /// and visibly.
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
/// `a[i + lag]` is compared with `b[i]`, so a positive `lag` shifts `b` **earlier** — see
/// [`XcorrPeak::lag`].
fn at_lag(a: &[f32], b: &[f32], lag: isize, min_overlap: usize) -> Option<XcorrPeak> {
    let (lo, hi) = overlap_range(a.len(), b.len(), lag)?;
    let n = (hi - lo) as usize;
    if n < min_overlap.max(2) {
        return None;
    }
    Some(XcorrPeak {
        lag,
        value: pearson(a, b, lag, lo, hi),
        overlap: n,
    })
}

/// The half-open index range of `b` that has a counterpart in `a` at `lag` (`i` indexes `b`,
/// `i + lag` indexes `a`), or `None` when the two do not overlap there.
fn overlap_range(a_len: usize, b_len: usize, lag: isize) -> Option<(isize, isize)> {
    let lo = 0isize.max(-lag);
    let hi = (b_len as isize).min(a_len as isize - lag);
    (hi > lo).then_some((lo, hi))
}

/// Pearson correlation of `a` and `b` at `lag` over `b[lo..hi]` alone, with the mean and the scale
/// removed **over that range** — which is what lets a segment be scored on its own.
///
/// A flat range has no shape to match, so it scores `0.0` rather than dividing by zero: "nothing
/// correlates here", which is the honest reading and one every caller's threshold rejects.
fn pearson(a: &[f32], b: &[f32], lag: isize, lo: isize, hi: isize) -> f64 {
    let n = (hi - lo) as f64;
    if n < 2.0 {
        return 0.0;
    }
    let (mut sa, mut sb) = (0f64, 0f64);
    for i in lo..hi {
        sa += f64::from(a[(i + lag) as usize]);
        sb += f64::from(b[i as usize]);
    }
    let (ma, mb) = (sa / n, sb / n);
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
    let denom = (saa * sbb).sqrt();
    if denom > 0.0 && denom.is_finite() {
        (sab / denom).clamp(-1.0, 1.0)
    } else {
        0.0
    }
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

/// **How many INDEPENDENT parts of the overlap each show the alignment at `lag`.**
///
/// A high correlation says the two series match; it does not say the match was *earned*. One
/// coincidence — two sensors that each happened to key once, 50 ms apart — correlates 1.00, and so
/// does one that a hundred separate events support. Only the second is evidence of a path delay,
/// and the two are indistinguishable from the peak value alone.
///
/// So the overlap is cut into `segments` equal parts and each is scored **on its own** (its own
/// mean, its own scale), counting the parts that reach `min_value`. A part with no content in
/// either series is flat, scores `0.0`, and supports nothing — which is the honest reading: a
/// silent stretch of the window is not evidence for a lag.
///
/// Costs one pass over the overlap, whatever `segments` is.
pub fn segment_support(a: &[f32], b: &[f32], lag: isize, segments: usize, min_value: f64) -> usize {
    let Some((lo, hi)) = overlap_range(a.len(), b.len(), lag) else {
        return 0;
    };
    let segments = segments.max(1) as isize;
    let span = hi - lo;
    if span < 2 * segments {
        return 0;
    }
    (0..segments)
        .filter(|k| {
            let (s, e) = (lo + k * span / segments, lo + (k + 1) * span / segments);
            pearson(a, b, lag, s, e) >= min_value
        })
        .count()
}

/// Largest number of decimated points [`self_similarity`] probes, which is what bounds it: the
/// probe is `O(points²)`, so a series longer than this is max-pooled down to it first.
pub const SELF_SIMILARITY_MAX_POINTS: usize = 1024;

/// Share of a series that must remain in common before a self-similarity lag is believed. Without
/// it the longest lags compare a handful of samples and reach 1.0 by accident.
pub const SELF_SIMILARITY_MIN_OVERLAP_FRACTION: f64 = 0.25;

/// **Does the series repeat itself, at a period the lag search cannot see?**
///
/// [`normalized_xcorr`] reports a runner-up only from inside `±max_lag`, so a repeat period
/// **longer than the searched range is structurally invisible to it** — and a series that repeats
/// every `P` matches a shifted copy of itself at `lag`, `lag ± P`, `lag ± 2P`… equally well. Its
/// lag is then ambiguous modulo `P` and cannot be called a delay, however high and however
/// dominant the peak looked inside the window that was searched.
///
/// This is the measurement of that: the strongest correlation of `x` with itself at any lag of at
/// least `min_lag` (in original samples). The caller passes its own search range as `min_lag`, so
/// the two tests together cover every lag rather than the same small band twice.
///
/// Bounded by max-pooling to [`SELF_SIMILARITY_MAX_POINTS`] before probing: pooling widens
/// features, which can only make a repeat easier to see, never harder — the safe direction for a
/// test whose job is to refuse.
///
/// `None` when the series is too short for any lag at or past `min_lag` to leave enough overlap.
pub fn self_similarity(x: &[f32], min_lag: usize) -> Option<XcorrPeak> {
    let decimate = x.len().div_ceil(SELF_SIMILARITY_MAX_POINTS).max(1);
    let pooled: Vec<f32> = x
        .chunks(decimate)
        .map(|c| c.iter().copied().fold(f32::NEG_INFINITY, f32::max))
        .collect();
    let n = pooled.len();
    let min_overlap = ((n as f64 * SELF_SIMILARITY_MIN_OVERLAP_FRACTION).ceil() as usize).max(2);
    let first = (min_lag.div_ceil(decimate)).max(1);
    let last = n.saturating_sub(min_overlap);
    (first..=last)
        .filter_map(|lag| at_lag(&pooled, &pooled, lag as isize, min_overlap))
        .max_by(|p, q| p.value.total_cmp(&q.value))
        .map(|p| XcorrPeak {
            // Back to the caller's units: the pooled lag counts pooled points.
            lag: p.lag * decimate as isize,
            ..p
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
    fn one_coincidence_correlates_perfectly_and_supports_nothing() {
        // Two sparse series with ONE event each, 10 samples apart: the classic false positive.
        // The peak is 1.00 and says nothing, and the support count is what says so.
        let mut a = vec![0f32; 800];
        let mut b = vec![0f32; 800];
        for i in 400..430 {
            a[i] = 1.0;
            b[i + 10] = 1.0;
        }
        let x = normalized_xcorr(&a, &b, 30, 64).expect("overlap");
        assert!(
            x.peak.value > 0.99,
            "one event still correlates: {:?}",
            x.peak
        );
        assert!(
            segment_support(&a, &b, x.peak.lag, 8, 0.75) <= 1,
            "one coincidence supports at most one segment"
        );
    }

    #[test]
    fn a_match_spread_over_the_window_is_supported_throughout() {
        let a = series(800, 0x5150);
        let lag = 10usize;
        let b: Vec<f32> = (0..a.len())
            .map(|i| if i >= lag { 0.3 * a[i - lag] } else { 0.0 })
            .collect();
        let x = normalized_xcorr(&a, &b, 30, 64).expect("overlap");
        assert_eq!(
            segment_support(&a, &b, x.peak.lag, 8, 0.75),
            8,
            "content everywhere supports every segment"
        );
    }

    #[test]
    fn a_silent_segment_supports_nothing() {
        // Both series are flat over the first half: it matches trivially and must not count.
        let mut a = vec![0f32; 800];
        let mut b = vec![0f32; 800];
        let noise = series(400, 9);
        a[400..].copy_from_slice(&noise);
        b[400..].copy_from_slice(&noise);
        assert_eq!(segment_support(&a, &b, 0, 8, 0.75), 4);
    }

    #[test]
    fn a_long_period_repeat_is_invisible_to_the_lag_search_and_visible_here() {
        // The review's false positive, in miniature: two INDEPENDENT emitters keying on the same
        // 500-sample cadence, 10 samples out of phase. Inside a search bounded at 30 the peak is
        // perfect and nothing competes with it, because the only thing that could - the cadence
        // itself - repeats 500 samples away, where the search never looks.
        let (mut a, mut b) = (vec![0f32; 4000], vec![0f32; 4000]);
        for k in 0..8 {
            for i in 0..7 {
                a[k * 500 + i] = 1.0;
                b[k * 500 + 10 + i] = 1.0;
            }
        }
        let inside = normalized_xcorr(&a, &b, 30, 64).expect("overlap");
        assert!(inside.peak.value > 0.99, "perfect: {:?}", inside.peak);
        assert!(
            inside.dominance() > 1.5,
            "and the bounded search calls it dominant, which is the trap: {inside:?}"
        );
        // Beyond that range the cadence is there to be seen, and it is what makes the lag
        // ambiguous: 10 samples, or 510, or 1010.
        let repeat = self_similarity(&a, 31).expect("long enough");
        assert!(
            repeat.value > 0.9,
            "the repeat is what denies the lag: {repeat:?}"
        );
        assert_eq!(repeat.lag % 500, 0, "found at the period: {repeat:?}");
    }

    #[test]
    fn an_aperiodic_series_does_not_repeat_itself() {
        let x = series(4000, 0xABCD);
        let repeat = self_similarity(&x, 31).expect("long enough");
        assert!(
            repeat.value < 0.5,
            "nothing in a random series pins another lag: {repeat:?}"
        );
    }

    #[test]
    fn a_series_too_short_to_probe_abstains() {
        assert!(self_similarity(&series(8, 1), 100).is_none());
    }

    #[test]
    fn empty_input_abstains() {
        assert!(normalized_xcorr(&[], &[], 4, 1).is_none());
        assert!(normalized_xcorr(&series(10, 1), &[], 4, 1).is_none());
    }
}
