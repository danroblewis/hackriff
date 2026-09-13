//! Bounded track statistics: running moments, a log-spaced length histogram, a fixed ring of
//! burst starts, observation coverage, the periodicity fold and the raster estimates. Nothing here
//! allocates except [`robust_raster`], which runs when a hop set is summarised (not per burst);
//! memory per track is constant however long it lives.

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

/// One hop-set channel for [`robust_raster`].
#[derive(Clone, Copy, Debug)]
pub(crate) struct RasterChannel {
    /// Centre estimate, Hz.
    pub fc: f64,
    /// Bandwidth estimate, Hz (scales the centre uncertainty).
    pub bw: f64,
    /// Bursts seen on the channel.
    pub bursts: u64,
    /// Mean burst SNR, dB.
    pub snr_db: f64,
}

/// Common channel rasters, Hz: a tiebreak between lattices that explain the centres equally.
const STANDARD_RASTERS_HZ: [f64; 17] = [
    5e3, 6.25e3, 8.33e3, 10e3, 12.5e3, 20e3, 25e3, 50e3, 100e3, 125e3, 200e3, 250e3, 400e3, 500e3,
    1e6, 2e6, 5e6,
];

/// Lattice fit of the channel centres to a raster `step`.
#[derive(Clone, Copy, Debug)]
struct LatticeFit {
    step: f64,
    offset: f64,
    /// Inlier weight / total weight.
    inlier_weight: f64,
    /// `inlier_weight` corrected for inliers expected by chance (share of the line within
    /// tolerance).
    score: f64,
    inliers: usize,
}

/// Burst count and SNR both weigh in, each compressed (square root) so that one strong member
/// whose centre estimate is off cannot outvote the rest.
fn channel_weight(c: &RasterChannel) -> f64 {
    (c.bursts.clamp(1, 16) as f64).sqrt() * (c.snr_db.clamp(3.0, 30.0) / 10.0).sqrt()
}

/// Steps scoring within this of the best are ties; the largest tie wins (sub-multiples of the
/// raster always fit).
const RASTER_TIE: f64 = 0.05;
/// A common raster within this of the best score is preferred to a larger, incommensurate step.
const RASTER_PRIOR: f64 = 0.15;

/// Centre tolerance: the centre-estimate uncertainty (a quarter of the bandwidth, at least the
/// bin tolerance), capped at `step / 8` so a lattice never covers more than a quarter of the line.
fn channel_tol(c: &RasterChannel, step: f64, base_tol_hz: f64) -> f64 {
    base_tol_hz.max(0.25 * c.bw).min(step / 8.0)
}

fn wrap(x: f64, step: f64) -> f64 {
    x - step * (x / step).round()
}

/// `x.rem_euclid(y)`, bit for bit, without `fmod` for the dividends a raster fit sees (channel
/// centres ≥ the step; T-058: `fmod` was about half of the tracker's time in dense bands).
///
/// With `q = trunc(x / y)` (an exact integer below 2^52; `q ∈ {Q, Q+1}` for the true `Q = ⌊x/y⌋`
/// because rounding is monotone and both are representable), `q·y = p + e` exactly (fused
/// multiply-add), `x − p` is exact (Sterbenz: `p/2 ≤ x ≤ 2p`), and `x − q·y` is a multiple of
/// `ulp(y)` smaller than `y` in magnitude, hence representable, so `(x − p) − e` rounds to it
/// exactly. When `q = Q+1` that remainder is negative and adding `y` gives the true one, again
/// representable. Everything else (negative, tiny, huge or non-finite operands) takes
/// `rem_euclid`.
#[inline]
fn rem_euclid_exact(x: f64, y: f64) -> f64 {
    if !(x >= y && x < 1e300 && y >= 1e-290) {
        return x.rem_euclid(y);
    }
    // `qf` is finite here (both operands are in range), so `>=` is the negated `<`.
    let qf = x / y;
    if qf >= 4_503_599_627_370_496.0 {
        return x.rem_euclid(y);
    }
    let q = qf.trunc();
    let p = q * y;
    let e = q.mul_add(y, -p);
    let r = (x - p) - e;
    if r < 0.0 {
        r + y
    } else if r < y {
        r
    } else {
        x.rem_euclid(y)
    }
}

/// `(w·cos φ, w·sin φ)`, defined once out of line for [`fit_raster`] and its reference: on Apple
/// targets LLVM may fuse `sin` and `cos` of one argument into `__sincos_stret`, whose last bit can
/// differ from separate calls, so the fused-or-not choice must not depend on the caller (T-058).
#[inline(never)]
fn weighted_phasor(w: f64, ph: f64) -> (f64, f64) {
    (w * ph.cos(), w * ph.sin())
}

/// What every candidate step of one [`robust_raster`] call shares, computed once with the same
/// expressions as [`channel_weight`] and [`channel_tol`] (T-058).
struct Prepared {
    fc: Vec<f64>,
    weight: Vec<f64>,
    /// `base_tol_hz.max(0.25·bw)`: [`channel_tol`] at a step is this `.min(step / 8)`.
    tol_base: Vec<f64>,
    total: f64,
}

impl Prepared {
    fn new(ch: &[RasterChannel], base_tol_hz: f64) -> Self {
        Self {
            fc: ch.iter().map(|c| c.fc).collect(),
            weight: ch.iter().map(channel_weight).collect(),
            tol_base: ch.iter().map(|c| base_tol_hz.max(0.25 * c.bw)).collect(),
            total: ch.iter().map(channel_weight).sum(),
        }
    }
}

/// Buffers of one [`fit_raster`] worker.
#[derive(Default)]
struct FitScratch {
    /// `weight·cos(phase)` and `weight·sin(phase)` per channel.
    x: Vec<f64>,
    y: Vec<f64>,
    tol: Vec<f64>,
    /// The inlier set of the latest offset iteration.
    mask: Vec<bool>,
    next: Vec<bool>,
}

/// Lattice fit of the channel centres to a raster `step`: the offset is the weighted circular mean
/// of the centre phases, refitted on its inliers (up to three iterations), then scored.
///
/// Bit-identical to the direct form (kept as `reference::fit_raster` in the tests), which
/// recomputed each channel's weight, phase, sine and cosine in every iteration: here they are
/// computed once per channel, and the iteration stops as soon as an inlier set repeats (its sums,
/// hence the offset and every later set, would repeat exactly).
fn fit_raster(pre: &Prepared, step: f64, s: &mut FitScratch) -> LatticeFit {
    let n = pre.fc.len();
    let tau = std::f64::consts::TAU;
    s.x.clear();
    s.y.clear();
    s.tol.clear();
    let cap = step / 8.0;
    for i in 0..n {
        let (w, ph) = (
            pre.weight[i],
            tau * rem_euclid_exact(pre.fc[i], step) / step,
        );
        let (x, y) = weighted_phasor(w, ph);
        s.x.push(x);
        s.y.push(y);
        s.tol.push(pre.tol_base[i].min(cap));
    }
    s.mask.clear();
    s.mask.resize(n, true);
    s.next.clear();
    s.next.resize(n, false);
    let mut offset: Option<f64> = None;
    // `mask` is the inlier set of the final offset (so the scoring pass can reuse it).
    let mut known = false;
    let mut iterations = 0;
    while iterations < 3 {
        iterations += 1;
        if let Some(a) = offset {
            let mut same = true;
            for i in 0..n {
                let inlier = wrap(pre.fc[i] - a, step).abs() <= s.tol[i];
                same &= inlier == s.mask[i];
                s.next[i] = inlier;
            }
            std::mem::swap(&mut s.mask, &mut s.next);
            if same {
                known = true;
                break;
            }
        }
        let (mut sx, mut sy) = (0.0, 0.0);
        for i in 0..n {
            if s.mask[i] {
                sx += s.x[i];
                sy += s.y[i];
            }
        }
        if sx == 0.0 && sy == 0.0 {
            known = offset.is_some();
            break;
        }
        offset = Some(sy.atan2(sx).rem_euclid(tau) / tau * step);
    }
    let a = offset.unwrap_or(0.0);
    let (mut w_in, mut inliers, mut p) = (0.0, 0, 0.0);
    for i in 0..n {
        let tol = s.tol[i];
        p += 2.0 * tol / step;
        let inlier = if known {
            s.mask[i]
        } else {
            wrap(pre.fc[i] - a, step).abs() <= tol
        };
        if inlier {
            w_in += pre.weight[i];
            inliers += 1;
        }
    }
    let p = p / n as f64;
    let f = if pre.total > 0.0 {
        w_in / pre.total
    } else {
        0.0
    };
    LatticeFit {
        step,
        offset: a,
        inlier_weight: f,
        score: (f - p) / (1.0 - p),
        inliers,
    }
}

/// Candidates × channels from which [`fit_all`] fans out over the shared compute pool (about
/// 0.3 ms of sequential work; smaller calls are not worth the hand-off).
#[cfg(feature = "cpu-mt")]
const PARALLEL_MIN_WORK: usize = 16_384;

/// Fits every candidate step, in candidate order. The fits are independent, so the multi-threaded
/// path (feature `cpu-mt`, large calls, over [`hk_dsp::compute::pool`]) returns exactly the
/// sequential result.
fn fit_all(pre: &Prepared, cands: &[f64]) -> Vec<LatticeFit> {
    #[cfg(feature = "cpu-mt")]
    if cands.len() * pre.fc.len() >= PARALLEL_MIN_WORK
        && let Ok(pool) = hk_dsp::compute::pool::shared(None)
    {
        return fit_all_parallel(&pool, pre, cands);
    }
    fit_all_sequential(pre, cands)
}

fn fit_all_sequential(pre: &Prepared, cands: &[f64]) -> Vec<LatticeFit> {
    let mut s = FitScratch::default();
    cands.iter().map(|&c| fit_raster(pre, c, &mut s)).collect()
}

#[cfg(feature = "cpu-mt")]
fn fit_all_parallel(pool: &rayon::ThreadPool, pre: &Prepared, cands: &[f64]) -> Vec<LatticeFit> {
    use rayon::prelude::*;
    pool.install(|| {
        cands
            .par_iter()
            .with_min_len(64)
            .map_init(FitScratch::default, |s, &c| fit_raster(pre, c, s))
            .collect()
    })
}

/// Raster step of noisy hop-set channel centres (T-035), or `None` when no lattice explains most
/// of them.
///
/// Each candidate step (pairwise centre differences divided by 1–32, plus the common rasters) is
/// fitted as a lattice `offset + k·step`; a centre is an inlier within its uncertainty
/// ([`channel_tol`]), weighted by burst count and SNR ([`channel_weight`]). The score is the
/// inlier weight corrected for chance hits. Among steps that explain at least 60 % of the weight
/// with 3+ inliers, the largest scoring within [`RASTER_TIE`] of the best wins (sub-multiples of
/// the raster always fit). If that step is not itself a common raster, the largest common raster
/// scoring within [`RASTER_PRIOR`] of the best, of which the step is not a multiple, is preferred
/// (a larger incommensurate lattice fitting a few noisy centres is a coincidence). The winner is
/// refined by weighted least squares on its inliers.
///
/// With few, noisy channels the data alone can be ambiguous (the real 915 MHz hop set fits both
/// 200 and 300 kHz with 6 of 7 channels); the common-raster prior decides such cases.
///
/// **Cost (T-058).** Every candidate is fitted: `O(candidates · channels)`, about `125·n²` channel
/// fits for `n` channels, called once per drain of a changed hop set. Each fit computes one exact
/// remainder, sine and cosine per channel ([`fit_raster`]), and large calls fan out over the
/// shared compute pool (feature `cpu-mt`). The result is bit-identical to the direct form, which
/// feature `parity-check` (and every test build) re-runs and compares on each call.
pub(crate) fn robust_raster(ch: &[RasterChannel], base_tol_hz: f64) -> Option<f64> {
    let r = robust_raster_fast(ch, base_tol_hz);
    #[cfg(any(test, feature = "parity-check"))]
    reference::cross_check(ch, base_tol_hz, r);
    r
}

fn robust_raster_fast(ch: &[RasterChannel], base_tol_hz: f64) -> Option<f64> {
    if ch.len() < 3 {
        return None;
    }
    let mut fc: Vec<f64> = ch.iter().map(|c| c.fc).collect();
    fc.sort_unstable_by(f64::total_cmp);
    let (min_step, max_step) = (4.0 * base_tol_hz.max(1.0), fc[fc.len() - 1] - fc[0]);
    if max_step < min_step {
        return None;
    }
    let mut cands: Vec<f64> = STANDARD_RASTERS_HZ
        .iter()
        .copied()
        .filter(|s| (min_step..=max_step).contains(s))
        .collect();
    for i in 0..fc.len() {
        for j in i + 1..fc.len().min(i + 6) {
            let d = fc[j] - fc[i];
            for k in 1..=32 {
                let step = d / f64::from(k);
                if step < min_step {
                    break;
                }
                cands.push(step);
            }
        }
    }
    let mut fits = fit_all(&Prepared::new(ch, base_tol_hz), &cands);
    fits.retain(|f| f.inliers >= 3 && f.inlier_weight >= 0.6);
    let best = fits
        .iter()
        .map(|f| f.score)
        .fold(f64::NEG_INFINITY, f64::max);
    let largest = fits
        .iter()
        .filter(|f| f.score >= best - RASTER_TIE)
        .max_by(|a, b| a.step.total_cmp(&b.step))?;
    let is_standard = |s: f64| {
        STANDARD_RASTERS_HZ
            .iter()
            .any(|r| (s / r - 1.0).abs() <= 0.02)
    };
    let chosen = if is_standard(largest.step) {
        largest
    } else {
        fits.iter()
            .filter(|f| STANDARD_RASTERS_HZ.contains(&f.step) && f.score >= best - RASTER_PRIOR)
            .filter(|f| {
                let m = largest.step / f.step;
                (m - m.round()).abs() > 0.05
            })
            .max_by(|a, b| a.step.total_cmp(&b.step))
            .unwrap_or(largest)
    };
    Some(refine_step(ch, chosen, base_tol_hz))
}

/// Weighted least squares `fc ≈ a + k·step` over the inliers of `fit`, with `k` fixed by
/// rounding. Keeps the fitted step when the regression is degenerate or strays > 10 %.
fn refine_step(ch: &[RasterChannel], fit: &LatticeFit, base_tol_hz: f64) -> f64 {
    let (step, a) = (fit.step, fit.offset);
    let (mut sw, mut sk, mut sy, mut skk, mut sky) = (0.0, 0.0, 0.0, 0.0, 0.0);
    for c in ch {
        if wrap(c.fc - a, step).abs() > channel_tol(c, step, base_tol_hz) {
            continue;
        }
        let w = channel_weight(c);
        let k = ((c.fc - a) / step).round();
        let y = c.fc - a;
        sw += w;
        sk += w * k;
        sy += w * y;
        skk += w * k * k;
        sky += w * k * y;
    }
    let den = sw * skk - sk * sk;
    if sw <= 0.0 || den <= f64::EPSILON * sw * skk.max(1.0) {
        return step;
    }
    let refined = (sw * sky - sk * sy) / den;
    if refined.is_finite() && (refined / step - 1.0).abs() <= 0.1 {
        refined
    } else {
        step
    }
}

/// The direct raster fit as it was before T-058: the parity reference of [`fit_raster`] and
/// [`robust_raster`] (tests, and feature `parity-check`). It evaluates the phasors through
/// [`weighted_phasor`] like the fast path, so both make the same libm choice in any build.
#[cfg(any(test, feature = "parity-check"))]
mod reference {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{
        LatticeFit, RASTER_PRIOR, RASTER_TIE, RasterChannel, STANDARD_RASTERS_HZ, channel_tol,
        channel_weight, refine_step, weighted_phasor, wrap,
    };

    static CHECKED: AtomicU64 = AtomicU64::new(0);

    /// Asserts that the fast result equals the reference one bit for bit.
    pub(super) fn cross_check(ch: &[RasterChannel], base_tol_hz: f64, got: Option<f64>) {
        let want = robust_raster(ch, base_tol_hz);
        assert_eq!(
            got.map(f64::to_bits),
            want.map(f64::to_bits),
            "robust_raster {got:?} differs from the reference {want:?} on {ch:?} (tolerance {base_tol_hz})"
        );
        let n = CHECKED.fetch_add(1, Ordering::Relaxed) + 1;
        if cfg!(feature = "parity-check") && n.is_power_of_two() {
            eprintln!("hk-detect parity-check: {n} hop-set rasters identical to the reference");
        }
    }

    pub(super) fn fit_raster(ch: &[RasterChannel], step: f64, base_tol_hz: f64) -> LatticeFit {
        let total: f64 = ch.iter().map(channel_weight).sum();
        let tau = std::f64::consts::TAU;
        // Offset: weighted circular mean of the centre phases, refitted on inliers.
        let inlier = |offset: Option<f64>, c: &RasterChannel| match offset {
            None => true,
            Some(a) => wrap(c.fc - a, step).abs() <= channel_tol(c, step, base_tol_hz),
        };
        let mut offset = None;
        for _ in 0..3 {
            let (mut sx, mut sy) = (0.0, 0.0);
            for c in ch.iter().filter(|c| inlier(offset, c)) {
                let (w, ph) = (channel_weight(c), tau * c.fc.rem_euclid(step) / step);
                let (x, y) = weighted_phasor(w, ph);
                sx += x;
                sy += y;
            }
            if sx == 0.0 && sy == 0.0 {
                break;
            }
            offset = Some(sy.atan2(sx).rem_euclid(tau) / tau * step);
        }
        let a = offset.unwrap_or(0.0);
        let (mut w_in, mut inliers, mut p) = (0.0, 0, 0.0);
        for c in ch {
            let tol = channel_tol(c, step, base_tol_hz);
            p += 2.0 * tol / step;
            if wrap(c.fc - a, step).abs() <= tol {
                w_in += channel_weight(c);
                inliers += 1;
            }
        }
        let p = p / ch.len() as f64;
        let f = if total > 0.0 { w_in / total } else { 0.0 };
        LatticeFit {
            step,
            offset: a,
            inlier_weight: f,
            score: (f - p) / (1.0 - p),
            inliers,
        }
    }

    pub(super) fn robust_raster(ch: &[RasterChannel], base_tol_hz: f64) -> Option<f64> {
        if ch.len() < 3 {
            return None;
        }
        let mut fc: Vec<f64> = ch.iter().map(|c| c.fc).collect();
        fc.sort_unstable_by(f64::total_cmp);
        let (min_step, max_step) = (4.0 * base_tol_hz.max(1.0), fc[fc.len() - 1] - fc[0]);
        if max_step < min_step {
            return None;
        }
        let mut cands: Vec<f64> = STANDARD_RASTERS_HZ
            .iter()
            .copied()
            .filter(|s| (min_step..=max_step).contains(s))
            .collect();
        for i in 0..fc.len() {
            for j in i + 1..fc.len().min(i + 6) {
                let d = fc[j] - fc[i];
                for k in 1..=32 {
                    let step = d / f64::from(k);
                    if step < min_step {
                        break;
                    }
                    cands.push(step);
                }
            }
        }
        let fits: Vec<LatticeFit> = cands
            .iter()
            .map(|&s| fit_raster(ch, s, base_tol_hz))
            .filter(|f| f.inliers >= 3 && f.inlier_weight >= 0.6)
            .collect();
        let best = fits
            .iter()
            .map(|f| f.score)
            .fold(f64::NEG_INFINITY, f64::max);
        let largest = fits
            .iter()
            .filter(|f| f.score >= best - RASTER_TIE)
            .max_by(|a, b| a.step.total_cmp(&b.step))?;
        let is_standard = |s: f64| {
            STANDARD_RASTERS_HZ
                .iter()
                .any(|r| (s / r - 1.0).abs() <= 0.02)
        };
        let chosen = if is_standard(largest.step) {
            largest
        } else {
            fits.iter()
                .filter(|f| STANDARD_RASTERS_HZ.contains(&f.step) && f.score >= best - RASTER_PRIOR)
                .filter(|f| {
                    let m = largest.step / f.step;
                    (m - m.round()).abs() > 0.05
                })
                .max_by(|a, b| a.step.total_cmp(&b.step))
                .unwrap_or(largest)
        };
        Some(refine_step(ch, chosen, base_tol_hz))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bits(r: Option<f64>) -> Option<u64> {
        r.map(f64::to_bits)
    }

    fn same_fit(a: &LatticeFit, b: &LatticeFit) -> bool {
        a.step.to_bits() == b.step.to_bits()
            && a.offset.to_bits() == b.offset.to_bits()
            && a.inlier_weight.to_bits() == b.inlier_weight.to_bits()
            && a.score.to_bits() == b.score.to_bits()
            && a.inliers == b.inliers
    }

    #[test]
    fn rem_euclid_exact_matches_rem_euclid() {
        let mut st = 58u64;
        let check = |x: f64, y: f64| {
            let (got, want) = (rem_euclid_exact(x, y), x.rem_euclid(y));
            assert!(
                got.to_bits() == want.to_bits() || (got.is_nan() && want.is_nan()),
                "{x:e} rem {y:e}: {got:e} != {want:e}"
            );
        };
        for _ in 0..300_000 {
            let x = 10f64.powf(1.0 + 9.0 * rng(&mut st));
            let y = 10f64.powf(9.0 * rng(&mut st) - 2.0);
            check(x, y);
            // Around multiples, where the quotient rounds across an integer.
            let m = (x / y).round().max(1.0) * y;
            for d in -3i64..=3 {
                check(f64::from_bits((m.to_bits() as i64 + d) as u64), y);
            }
        }
        for e in -30..60 {
            for f in -30..60 {
                check(2f64.powi(e) * 3.0, 2f64.powi(f));
                check(2f64.powi(e), 2f64.powi(f) * 1.5);
                check(2f64.powi(e), 2f64.powi(f));
            }
        }
        for (x, y) in [
            (0.0, 1.0),
            (-0.0, 3.0),
            (-5.0, 3.0),
            (2.0, 3.0),
            (f64::MAX, 3.0),
            (1e20, 1e-5),
            (7.0, f64::INFINITY),
            (f64::INFINITY, 7.0),
            (f64::NAN, 2.0),
            (2.0, f64::NAN),
            (9.0, 0.0),
            (4_503_599_627_370_497.0, 1.0),
            (9_007_199_254_740_993.0, 2.0),
            (1.0, 2f64.powi(-1000)),
            (915_200_000.0, 199_999.999_999_999_97),
        ] {
            check(x, y);
        }
    }

    /// Channel sets: noisy lattices, duplicated channels (re-opened tracks), no lattice, exact
    /// lattices.
    fn channels(st: &mut u64, n: usize, kind: u32) -> Vec<RasterChannel> {
        let base = 80e6 + 900e6 * rng(st);
        let raster = [12.5e3, 25e3, 100e3, 200e3, 300e3, 1e6][(rng(st) * 6.0) as usize];
        (0..n)
            .map(|i| {
                let fc = match kind {
                    0 => base + (rng(st) * 60.0).floor() * raster + (rng(st) - 0.5) * 0.1 * raster,
                    1 => base + (i / 3) as f64 * raster + (rng(st) - 0.5) * 2e3,
                    2 => base + rng(st) * 20e6,
                    _ => base + (rng(st) * 40.0).floor() * raster,
                };
                RasterChannel {
                    fc,
                    bw: 5e3 + rng(st) * 2.0 * raster,
                    bursts: (rng(st) * 40.0) as u64,
                    snr_db: rng(st) * 40.0,
                }
            })
            .collect()
    }

    #[test]
    fn fast_raster_fits_are_bit_identical_to_the_reference() {
        let mut st = 1u64;
        let mut s = FitScratch::default();
        for case in 0..240u32 {
            let span = if case % 4 == 0 { 90.0 } else { 20.0 };
            let n = 3 + (rng(&mut st) * span) as usize;
            let ch = channels(&mut st, n, case % 4);
            let tol = [2.4e3, 7.5e3, 14.6e3][case as usize % 3];
            assert_eq!(
                bits(robust_raster_fast(&ch, tol)),
                bits(reference::robust_raster(&ch, tol)),
                "case {case} n {n}"
            );
            let pre = Prepared::new(&ch, tol);
            for _ in 0..40 {
                let step = 10f64.powf(3.0 + 4.0 * rng(&mut st));
                let (a, b) = (
                    fit_raster(&pre, step, &mut s),
                    reference::fit_raster(&ch, step, tol),
                );
                assert!(same_fit(&a, &b), "case {case} step {step}: {a:?} vs {b:?}");
            }
        }
    }

    #[test]
    fn parallel_raster_fits_equal_the_sequential_ones() {
        let mut st = 5u64;
        for case in 0..12u32 {
            let ch = channels(&mut st, 60, case % 4);
            let pre = Prepared::new(&ch, 7.5e3);
            let cands: Vec<f64> = (0..3000)
                .map(|_| 10f64.powf(3.5 + 3.0 * rng(&mut st)))
                .collect();
            let seq = fit_all_sequential(&pre, &cands);
            let all = fit_all(&pre, &cands);
            assert_eq!(seq.len(), all.len());
            assert!(
                seq.iter().zip(&all).all(|(a, b)| same_fit(a, b)),
                "case {case}"
            );
            #[cfg(feature = "cpu-mt")]
            {
                let pool = hk_dsp::compute::pool::shared(None).unwrap();
                let par = fit_all_parallel(&pool, &pre, &cands);
                assert!(
                    seq.iter().zip(&par).all(|(a, b)| same_fit(a, b)),
                    "case {case}"
                );
            }
        }
    }

    fn chan(fc: f64, bw: f64, bursts: u64) -> RasterChannel {
        RasterChannel {
            fc,
            bw,
            bursts,
            snr_db: 12.0,
        }
    }

    #[test]
    fn robust_raster_of_precise_channels() {
        let c: Vec<_> = [915.2e6, 915.6e6, 916.4e6, 917.0e6, 917.2e6]
            .iter()
            .map(|&f| chan(f + 1.5e3, 50e3, 20))
            .collect();
        let r = robust_raster(&c, 7.5e3).unwrap();
        assert!((r - 200e3).abs() < 1e3, "{r}");
        // A non-standard raster is kept, not replaced by a standard sub-multiple.
        let c: Vec<_> = (0..6)
            .map(|k| chan(900e6 + [0, 1, 3, 4, 7, 9][k] as f64 * 300e3, 60e3, 10))
            .collect();
        let r = robust_raster(&c, 7.5e3).unwrap();
        assert!((r - 300e3).abs() < 1e3, "{r}");
    }

    #[test]
    fn robust_raster_of_noisy_real_915_centres() {
        // The tracker's hop-set members on fixtures/hackrf/2026-09-13/ism_915M_10M_l24g30a1_t42p3_1p2s
        // at close (centre, bandwidth, bursts, mean SNR); true raster 200 kHz. `raster` gave
        // 40.3 kHz. These data alone fit 200 and 300 kHz with 6 of 7 channels each: the result
        // rests on the common-raster prior, and the end-to-end check is the fixture report test.
        let real = |fc: f64, bw: f64, bursts: u64, snr_db: f64| RasterChannel {
            fc,
            bw,
            bursts,
            snr_db,
        };
        let c = [
            real(910_392_446.4, 117_786.9, 6, 10.01),
            real(918_790_346.3, 117_187.5, 1, 7.86),
            real(919_964_351.1, 102_539.1, 4, 16.53),
            real(919_396_955.5, 126_953.1, 1, 13.45),
            real(919_195_052.7, 175_781.3, 1, 10.17),
            real(912_498_589.1, 185_546.9, 1, 20.45),
            real(911_574_426.7, 165_652.8, 1, 16.12),
        ];
        let r = robust_raster(&c, 1.5 * 10e6 / 1024.0).unwrap();
        assert!((r - 200e3).abs() / 200e3 <= 0.05, "{r}");
    }

    #[test]
    fn robust_raster_needs_three_channels() {
        assert!(robust_raster(&[chan(915e6, 50e3, 3), chan(915.2e6, 50e3, 3)], 5e3).is_none());
    }

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
