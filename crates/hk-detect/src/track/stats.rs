//! Bounded track statistics: running moments, a log-spaced length histogram, a fixed ring of
//! burst starts, observation coverage, the periodicity fold and the raster estimate. Nothing here
//! allocates; memory per track is constant however long it lives.

use super::config::PeriodConfig;

/// Burst starts kept per track for the periodicity fold.
pub(crate) const START_RING: usize = 64;

/// Running count, sum, sum of squares, min and max.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Moments {
    pub n: u64,
    sum: f64,
    sum_sq: f64,
    pub min: f64,
    pub max: f64,
}

impl Moments {
    pub fn push(&mut self, x: f64) {
        if self.n == 0 {
            self.min = x;
            self.max = x;
        } else {
            self.min = self.min.min(x);
            self.max = self.max.max(x);
        }
        self.n += 1;
        self.sum += x;
        self.sum_sq += x * x;
    }

    /// Replaces a previously pushed `old` with `new` (min/max only widen).
    pub fn replace(&mut self, old: f64, new: f64) {
        if self.n == 0 {
            self.push(new);
            return;
        }
        self.sum += new - old;
        self.sum_sq += new * new - old * old;
        self.min = self.min.min(new);
        self.max = self.max.max(new);
    }

    pub fn absorb(&mut self, o: &Moments) {
        if o.n == 0 {
            return;
        }
        if self.n == 0 {
            *self = *o;
            return;
        }
        self.n += o.n;
        self.sum += o.sum;
        self.sum_sq += o.sum_sq;
        self.min = self.min.min(o.min);
        self.max = self.max.max(o.max);
    }

    pub fn mean(&self) -> Option<f64> {
        (self.n > 0).then(|| self.sum / self.n as f64)
    }

    pub fn std(&self) -> Option<f64> {
        if self.n < 2 {
            return None;
        }
        let n = self.n as f64;
        let var = (self.sum_sq - self.sum * self.sum / n) / (n - 1.0);
        Some(var.max(0.0).sqrt())
    }
}

const HIST_BINS: usize = 64;
const HIST_LO_S: f64 = 1e-5;
const HIST_PER_DECADE: f64 = 8.0;

/// Log-spaced histogram of burst lengths: 8 bins per decade from 10 µs (64 bins, to 1000 s).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct LogHistogram {
    counts: [u32; HIST_BINS],
    total: u64,
}

impl Default for LogHistogram {
    fn default() -> Self {
        Self {
            counts: [0; HIST_BINS],
            total: 0,
        }
    }
}

impl LogHistogram {
    fn index(x: f64) -> usize {
        if x <= HIST_LO_S {
            return 0;
        }
        (((x / HIST_LO_S).log10() * HIST_PER_DECADE).floor() as usize).min(HIST_BINS - 1)
    }

    pub fn add(&mut self, x: f64) {
        let i = Self::index(x);
        self.counts[i] = self.counts[i].saturating_add(1);
        self.total += 1;
    }

    pub fn remove(&mut self, x: f64) {
        let i = Self::index(x);
        if self.counts[i] > 0 {
            self.counts[i] -= 1;
            self.total -= 1;
        }
    }

    pub fn absorb(&mut self, o: &LogHistogram) {
        for (a, b) in self.counts.iter_mut().zip(o.counts) {
            *a = a.saturating_add(b);
        }
        self.total += o.total;
    }

    /// Quantile `q` as the geometric centre of its bin (±15 %).
    pub fn quantile(&self, q: f64) -> Option<f64> {
        if self.total == 0 {
            return None;
        }
        let target = ((q * self.total as f64).ceil() as u64).max(1);
        let mut acc = 0u64;
        for (i, &c) in self.counts.iter().enumerate() {
            acc += u64::from(c);
            if acc >= target {
                return Some(HIST_LO_S * 10f64.powf((i as f64 + 0.5) / HIST_PER_DECADE));
            }
        }
        None
    }
}

/// The latest [`START_RING`] burst starts, ns.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct StartRing {
    buf: [i64; START_RING],
    len: usize,
    head: usize,
}

impl Default for StartRing {
    fn default() -> Self {
        Self {
            buf: [0; START_RING],
            len: 0,
            head: 0,
        }
    }
}

impl StartRing {
    pub fn push(&mut self, t: i64) {
        let i = (self.head + self.len) % START_RING;
        self.buf[i] = t;
        if self.len < START_RING {
            self.len += 1;
        } else {
            self.head = (self.head + 1) % START_RING;
        }
    }

    /// Copies the starts into `out`, sorted; returns how many.
    pub fn sorted_into(&self, out: &mut [i64; START_RING]) -> usize {
        for (k, o) in out.iter_mut().enumerate().take(self.len) {
            *o = self.buf[(self.head + k) % START_RING];
        }
        out[..self.len].sort_unstable();
        self.len
    }

    /// Merges `other` in, keeping the latest [`START_RING`] starts.
    pub fn absorb(&mut self, other: &StartRing) {
        let mut a = [0i64; START_RING];
        let mut b = [0i64; START_RING];
        let na = self.sorted_into(&mut a);
        let nb = other.sorted_into(&mut b);
        let mut all = [0i64; 2 * START_RING];
        all[..na].copy_from_slice(&a[..na]);
        all[na..na + nb].copy_from_slice(&b[..nb]);
        let n = na + nb;
        all[..n].sort_unstable();
        *self = StartRing::default();
        for &t in &all[n.saturating_sub(START_RING)..n] {
            self.push(t);
        }
    }
}

const COVERAGE_SPANS: usize = 64;

#[derive(Clone, Copy, Debug, Default)]
struct Span {
    start: i64,
    end: i64,
    /// Observed ns before this span.
    before: i64,
}

/// Observed time (C10 pitfall: observation gaps look like silence). The latest
/// [`COVERAGE_SPANS`] contiguous spans; time before the oldest is assumed observed, time after the
/// newest is extrapolation (future horizon).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Coverage {
    spans: [Span; COVERAGE_SPANS],
    len: usize,
    head: usize,
}

impl Default for Coverage {
    fn default() -> Self {
        Self {
            spans: [Span::default(); COVERAGE_SPANS],
            len: 0,
            head: 0,
        }
    }
}

impl Coverage {
    fn at(&self, k: usize) -> &Span {
        &self.spans[(self.head + k) % COVERAGE_SPANS]
    }

    /// Adds `[t0, t1)`; spans within `slack` of the newest extend it.
    pub fn add(&mut self, t0: i64, t1: i64, slack: i64) {
        if t1 <= t0 {
            return;
        }
        let mut before = 0;
        if self.len > 0 {
            let i = (self.head + self.len - 1) % COVERAGE_SPANS;
            let last = &mut self.spans[i];
            if t0 <= last.end + slack {
                if t0 >= last.start {
                    last.end = last.end.max(t1);
                }
                return;
            }
            before = last.before + (last.end - last.start);
        }
        let span = Span {
            start: t0,
            end: t1,
            before,
        };
        if self.len < COVERAGE_SPANS {
            self.spans[(self.head + self.len) % COVERAGE_SPANS] = span;
            self.len += 1;
        } else {
            self.spans[self.head] = span;
            self.head = (self.head + 1) % COVERAGE_SPANS;
        }
    }

    fn cum(&self, t: i64) -> i64 {
        let newest = self.at(self.len - 1);
        if t >= newest.end {
            return newest.before + (newest.end - newest.start) + (t - newest.end);
        }
        for k in (0..self.len).rev() {
            let s = self.at(k);
            if t >= s.start {
                return s.before + (t.min(s.end) - s.start);
            }
        }
        let oldest = self.at(0);
        oldest.before - (oldest.start - t)
    }

    /// Observed ns in `[a, b]` (wall time when nothing was observed).
    pub fn observed(&self, a: i64, b: i64) -> i64 {
        if b <= a {
            return 0;
        }
        if self.len == 0 {
            return b - a;
        }
        (self.cum(b) - self.cum(a)).max(0)
    }
}

/// A repetition period recovered by the arrival-time fold.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Periodicity {
    /// Period, s (least-squares slope of arrival time on lattice index).
    pub period_s: f64,
    /// Inlier fraction × lattice-slot coverage, 0–1 (a sub-multiple of the true period scores
    /// its coverage, e.g. 0.5 for P/2).
    pub confidence: f64,
    /// RMS residual of the inlier arrivals about the lattice, s.
    pub jitter_s: f64,
    /// Bursts on the lattice.
    pub bursts: usize,
}

/// Folds sorted burst starts (ns) onto the best lattice `a + k·P`: candidates are the median and
/// lower-quartile inter-arrival and the median's sub-multiples (missed bursts); each is chained
/// from one of the first three starts (jitter tolerance `2·jitter_fraction`), fitted by least
/// squares, re-folded globally with tolerance `jitter_fraction · P` and refitted twice.
pub(crate) fn fold_period(starts: &[i64], cfg: &PeriodConfig) -> Option<Periodicity> {
    let n = starts.len().min(START_RING);
    let starts = &starts[..n];
    if n < cfg.min_bursts.max(3) {
        return None;
    }
    let min_p = cfg.min_period_s * 1e9;
    let mut deltas = [0f64; START_RING];
    let mut m = 0;
    for w in starts.windows(2) {
        let d = (w[1] - w[0]) as f64;
        if d >= min_p {
            deltas[m] = d;
            m += 1;
        }
    }
    if m < 2 {
        return None;
    }
    let ds = &mut deltas[..m];
    ds.sort_unstable_by(f64::total_cmp);
    let med = ds[m / 2];
    let q25 = ds[m / 4];
    let mut best: Option<Periodicity> = None;
    for cand in [med, q25, med / 2.0, med / 3.0] {
        if cand < min_p {
            continue;
        }
        for anchor in 0..n.min(3) {
            if let Some(p) = fit_lattice(starts, anchor, cand, cfg) {
                let better = match best {
                    None => true,
                    Some(b) => p.confidence > b.confidence + 1e-9,
                };
                if better {
                    best = Some(p);
                }
            }
        }
    }
    best.filter(|b| b.confidence >= cfg.min_confidence && b.bursts >= cfg.min_bursts)
}

fn fit_lattice(s: &[i64], anchor: usize, p0: f64, cfg: &PeriodConfig) -> Option<Periodicity> {
    const OUT: i64 = i64::MIN;
    let n = s.len();
    let base = s[anchor];
    let chain_tol = 2.0 * cfg.jitter_fraction;
    let mut ks = [OUT; START_RING];
    ks[anchor] = 0;
    let (mut at, mut ak) = (base, 0i64);
    for i in anchor + 1..n {
        let d = (s[i] - at) as f64 / p0;
        let r = d.round();
        if r >= 1.0 && (d - r).abs() <= chain_tol {
            ks[i] = ak + r as i64;
            at = s[i];
            ak = ks[i];
        }
    }
    let (mut at, mut ak) = (base, 0i64);
    for i in (0..anchor).rev() {
        let d = (at - s[i]) as f64 / p0;
        let r = d.round();
        if r >= 1.0 && (d - r).abs() <= chain_tol {
            ks[i] = ak - r as i64;
            at = s[i];
            ak = ks[i];
        }
    }
    let min_p = cfg.min_period_s * 1e9;
    let (mut p, mut inliers, mut jitter) = (p0, 0usize, 0.0);
    for _ in 0..3 {
        let (mut sx, mut sy, mut sxx, mut sxy, mut c) = (0.0, 0.0, 0.0, 0.0, 0.0);
        for i in 0..n {
            if ks[i] != OUT {
                let x = ks[i] as f64;
                let y = (s[i] - base) as f64;
                sx += x;
                sy += y;
                sxx += x * x;
                sxy += x * y;
                c += 1.0;
            }
        }
        if c < 3.0 {
            return None;
        }
        let den = c * sxx - sx * sx;
        if den <= 0.0 {
            return None;
        }
        p = (c * sxy - sx * sy) / den;
        if p.is_nan() || p < min_p {
            return None;
        }
        let a = (sy - p * sx) / c;
        let tol = cfg.jitter_fraction * p;
        inliers = 0;
        let mut ss = 0.0;
        for i in 0..n {
            let y = (s[i] - base) as f64;
            let k = ((y - a) / p).round();
            let r = y - a - k * p;
            if r.abs() <= tol {
                ks[i] = k as i64;
                inliers += 1;
                ss += r * r;
            } else {
                ks[i] = OUT;
            }
        }
        if inliers == 0 {
            return None;
        }
        jitter = (ss / inliers as f64).sqrt();
    }
    let (mut kmin, mut kmax, mut distinct, mut last) = (i64::MAX, i64::MIN, 0usize, OUT);
    for &k in ks.iter().take(n) {
        if k == OUT {
            continue;
        }
        kmin = kmin.min(k);
        kmax = kmax.max(k);
        if k != last {
            distinct += 1;
            last = k;
        }
    }
    let _ = inliers;
    let slots = (kmax - kmin + 1).max(1) as f64;
    let coverage = (distinct as f64 / slots).min(1.0);
    Some(Periodicity {
        period_s: p / 1e9,
        confidence: distinct as f64 / n as f64 * coverage,
        jitter_s: jitter / 1e9,
        bursts: distinct,
    })
}

/// Raster step of sorted channel centres: the smallest spacing or a sub-multiple (≤ 8) that every
/// consecutive spacing is an integer multiple of (within `max(tol_hz, 5 %)`), refined by least
/// squares. Quantising to bins first (C10 pitfall) is the tolerance.
pub(crate) fn raster(centres: &[f64], tol_hz: f64) -> Option<f64> {
    if centres.len() < 3 {
        return None;
    }
    let dmin = centres
        .windows(2)
        .map(|w| w[1] - w[0])
        .filter(|&d| d > tol_hz)
        .fold(f64::INFINITY, f64::min);
    if !dmin.is_finite() {
        return None;
    }
    for m in 1..=8 {
        let step = dmin / f64::from(m);
        if step < 2.0 * tol_hz {
            break;
        }
        let t = tol_hz.max(0.05 * step);
        let (mut num, mut den, mut ok) = (0.0, 0.0, true);
        for w in centres.windows(2) {
            let d = w[1] - w[0];
            let k = (d / step).round();
            if k < 1.0 || (d - k * step).abs() > t {
                ok = false;
                break;
            }
            num += d * k;
            den += k * k;
        }
        if ok && den > 0.0 {
            return Some(num / den);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rng(state: &mut u64) -> f64 {
        *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = *state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        ((z ^ (z >> 31)) >> 11) as f64 / (1u64 << 53) as f64
    }

    #[test]
    fn fold_recovers_a_jittered_period_with_missed_bursts() {
        let cfg = PeriodConfig::default();
        let mut st = 7u64;
        let p = 0.12e9;
        let mut starts = Vec::new();
        for k in 0..40 {
            if k % 5 == 3 {
                continue; // missed
            }
            starts.push((0.03e9 + k as f64 * p + (rng(&mut st) - 0.5) * 0.02e9) as i64);
        }
        let got = fold_period(&starts, &cfg).expect("period");
        assert!((got.period_s - 0.12).abs() / 0.12 < 0.01, "{got:?}");
        assert!(got.confidence > 0.6 && got.jitter_s < 0.01, "{got:?}");
    }

    #[test]
    fn fold_reports_no_period_for_poisson_arrivals() {
        let cfg = PeriodConfig::default();
        for seed in 1..20u64 {
            let mut st = seed;
            let mut t = 0.0;
            let starts: Vec<i64> = (0..60)
                .map(|_| {
                    t += -(rng(&mut st).max(1e-12)).ln() * 0.1e9;
                    t as i64
                })
                .collect();
            assert!(fold_period(&starts, &cfg).is_none(), "seed {seed}");
        }
    }

    #[test]
    fn raster_of_a_channel_subset() {
        let c = [915.2e6, 915.6e6, 916.4e6, 917.0e6, 917.2e6];
        let r = raster(&c, 5e3).unwrap();
        assert!((r - 200e3).abs() < 1e3, "{r}");
    }

    #[test]
    fn coverage_excludes_gaps() {
        let mut c = Coverage::default();
        c.add(0, 10, 0);
        c.add(10, 20, 0);
        c.add(100, 110, 0);
        assert_eq!(c.observed(0, 110), 30);
        assert_eq!(c.observed(5, 105), 20);
        assert_eq!(c.observed(105, 120), 15);
    }

    #[test]
    fn histogram_quantiles_and_ring_merge() {
        let mut h = LogHistogram::default();
        for _ in 0..10 {
            h.add(0.02);
        }
        let q = h.quantile(0.5).unwrap();
        assert!((q / 0.02 - 1.0).abs() < 0.2, "{q}");
        let mut a = StartRing::default();
        let mut b = StartRing::default();
        for i in 0..50 {
            a.push(2 * i);
            b.push(2 * i + 1);
        }
        a.absorb(&b);
        let mut out = [0; START_RING];
        let n = a.sorted_into(&mut out);
        assert_eq!(n, START_RING);
        assert_eq!(out[n - 1], 99);
        assert_eq!(out[0], 100 - START_RING as i64);
    }
}
