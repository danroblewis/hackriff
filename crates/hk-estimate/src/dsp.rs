//! Internal DSP helpers: cached Welch engines and FFT plans, mixing, the channel filter and the
//! power-of-M spectral line search. Buffers are sized per call; no per-sample allocation.

use std::collections::HashMap;
use std::f64::consts::{LN_2, TAU};
use std::ops::Range;

use hk_dsp::fft::{CpuFft, FftBackend};
use hk_dsp::filter::design::windowed_sinc;
use hk_dsp::filter::{kaiser_beta, kaiser_taps, kernels};
use hk_dsp::{SegmentEngine, Spectrum, WelchConfig, WindowKind};
use num_complex::Complex32;

/// Variance inflation of Welch with Hann at 50 % overlap (`K_eff = K / 1.056`).
pub(crate) const HANN_OVERLAP_VAR: f64 = 1.056;
/// Variance of a 5-bin mean of Hann PSD bins relative to one bin (adjacent-bin power
/// correlation 4/9, next 1/36): `(5 + 2·(4·4/9 + 3/36)) / 25`.
pub(crate) const HANN_SMOOTH5_VAR: f64 = 0.349;
/// Variance factor of a mean over many adjacent Hann bins: `1 + 2·4/9 + 2/36`.
pub(crate) const HANN_MANY_VAR: f64 = 1.944;
/// Blackman equivalent noise bandwidth, bins.
const BLACKMAN_ENBW: f64 = 1.73;

/// A DC-centred Welch PSD (linear FS²/Hz, `f64`).
pub(crate) struct Psd {
    pub p: Vec<f64>,
    pub fs: f64,
    pub nfft: usize,
    pub segments: u32,
}

impl Psd {
    pub fn df(&self) -> f64 {
        self.fs / self.nfft as f64
    }

    pub fn freq(&self, k: usize) -> f64 {
        (k as f64 - (self.nfft / 2) as f64) * self.df()
    }

    /// Effective number of independent averages.
    pub fn k_eff(&self) -> f64 {
        if self.segments <= 1 {
            1.0
        } else {
            f64::from(self.segments) / HANN_OVERLAP_VAR
        }
    }

    /// Bins whose centre lies in `[lo_hz, hi_hz]`.
    pub fn bins(&self, lo_hz: f64, hi_hz: f64) -> Range<usize> {
        let df = self.df();
        let c = (self.nfft / 2) as f64;
        let a = (lo_hz / df + c).ceil().clamp(0.0, self.nfft as f64) as usize;
        let b = ((hi_hz / df + c).floor() + 1.0).clamp(0.0, self.nfft as f64) as usize;
        a.min(b)..b
    }
}

/// Largest power of two `<= n` (0 for 0).
pub(crate) fn prev_pow2(n: usize) -> usize {
    if n == 0 {
        0
    } else {
        1 << (usize::BITS - 1 - n.leading_zeros())
    }
}

/// Welch FFT length for `len` samples: S5's `2^round(log2(len/8))` clamped to 256…16384, capped
/// so at least three half-overlapping segments fit. `None` below 64 bins.
pub(crate) fn choose_nfft(len: usize) -> Option<usize> {
    if len < 128 {
        return None;
    }
    let target = 1usize << ((len.max(256) as f64 / 8.0).log2().round() as u32).clamp(8, 14);
    let n = target.min(prev_pow2(len / 2));
    (n >= 64).then_some(n)
}

/// Cached engines, plans and scratch buffers.
#[derive(Default)]
pub(crate) struct Workspace {
    welch: HashMap<usize, (SegmentEngine, Spectrum)>,
    fft: HashMap<usize, CpuFft>,
    line_buf: Vec<Complex32>,
    line_pow: Vec<f32>,
    median_scratch: Vec<f32>,
}

impl Workspace {
    /// Hann, 50 % overlap Welch PSD of `x` (`x.len() >= nfft`).
    pub fn welch(&mut self, x: &[Complex32], fs: f64, nfft: usize) -> Psd {
        debug_assert!(x.len() >= nfft);
        let (engine, spectrum) = self.welch.entry(nfft).or_insert_with(|| {
            let config = WelchConfig {
                fft_len: nfft,
                overlap: nfft / 2,
                window: WindowKind::Hann,
                holds: false,
                spectral_kurtosis: false,
            };
            let engine = SegmentEngine::new(config).expect("valid Welch config");
            let spectrum = engine.empty_spectrum();
            (engine, spectrum)
        });
        engine.reset();
        let hop = nfft / 2;
        let mut start = 0;
        while start + nfft <= x.len() {
            engine.process(&x[start..start + nfft]);
            start += hop;
        }
        engine.finish_into(fs, 0.0, spectrum);
        Psd {
            p: spectrum.psd.iter().map(|&v| f64::from(v)).collect(),
            fs,
            nfft,
            segments: engine.count(),
        }
    }
}

/// `out = x · e^{−j2π f n / fs}`, phase-continuous from `n = 0`.
pub(crate) fn mix_into(x: &[Complex32], f_hz: f64, fs: f64, out: &mut Vec<Complex32>) {
    out.clear();
    out.reserve(x.len());
    let step = -f_hz / fs;
    out.extend(x.iter().enumerate().map(|(n, &v)| {
        let ph = (step * n as f64).fract() * TAU;
        v * Complex32::new(ph.cos() as f32, ph.sin() as f32)
    }));
}

/// Moving average of `v` over `w` samples, centred, renormalised at the edges.
pub(crate) fn smooth(v: &[f64], w: usize, out: &mut Vec<f64>) {
    out.clear();
    let n = v.len();
    if n == 0 {
        return;
    }
    let half = w / 2;
    let mut prefix = Vec::with_capacity(n + 1);
    prefix.push(0.0);
    let mut acc = 0.0;
    for &x in v {
        acc += x;
        prefix.push(acc);
    }
    out.extend((0..n).map(|i| {
        let a = i.saturating_sub(half);
        let b = (i + w - half).min(n);
        (prefix[b] - prefix[a]) / (b - a) as f64
    }));
}

/// Longest direct channel filter, taps (the S5-era clamp; it also bounds the kernel's time span
/// in the multirate form).
const CHANNEL_FILTER_MAX_TAPS: usize = 8191;
/// Channel filter stopband attenuation, dB.
const CHANNEL_FILTER_DB: f64 = 40.0;
/// Multirate form: the intermediate rate is at least this many times the stopband edge.
const MULTIRATE_OVERSAMPLE: f64 = 8.0;
/// Multirate form only from this decimation factor up (wider channels filter directly).
const MULTIRATE_MIN_DECIMATION: usize = 4;
/// Attenuation of the multirate anti-alias / anti-image filter, dB.
const MULTIRATE_AA_DB: f64 = 70.0;

/// A symmetric low-pass channel filter at unity DC gain.
///
/// **Cost (T-081).** A narrow passband at a wide snippet rate needs a long kernel: a 214 Hz
/// carrier in a 500 kHz snippet clamps at 8191 taps, 2·10⁹ multiply-adds over a 0.5 s probe
/// (1.4 s, the whole WFM/RDS chain's cost on the dense urban replay). When the rate is at least
/// [`MULTIRATE_MIN_DECIMATION`] × [`MULTIRATE_OVERSAMPLE`] × the stopband edge, the same Kaiser
/// kernel (same cutoff, β and time span, so the same passband, transition and ENBW) runs at
/// `fs/D` between a zero-phase anti-alias decimator and the matching interpolator
/// (≥ [`MULTIRATE_AA_DB`] dB outside `±(fs/D − stop)`, flat inside `±stop`). The output stays at
/// `fs`, zero-delay, as the filtered zero-extended input; it differs from the direct form only
/// by residual aliases/images of the 70 dB decimator and the tap rounding of the shorter
/// kernel: −53 to −61 dB relative output error, ENBW within 0.2 % (unit test
/// `multirate_channel_filter_matches_the_reference_within_tolerance`). Wider channels (every WFM
/// box) filter directly with the same kernel, the products summed in eight SIMD lanes (float
/// rounding only, below −100 dB).
pub(crate) struct ChannelFilter {
    /// Kernel (at `fs`, or at `fs/D` in the multirate form).
    taps: Vec<f32>,
    multirate: Option<Multirate>,
    /// Equivalent noise bandwidth, Hz.
    pub enbw_hz: f64,
}

/// Decimate by `d` → kernel → interpolate by `d`.
struct Multirate {
    d: usize,
    /// Anti-alias / anti-image low-pass at `fs`, symmetric, odd length, DC gain 1.
    aa: Vec<f32>,
}

impl ChannelFilter {
    /// Kaiser low-pass passing `±passband_hz`, stopband at `4/3·passband` (the S5 ±0.75·OBW
    /// filter has its stopband at ±OBW), 40 dB. `None` when the passband already spans the
    /// band.
    pub fn new(fs: f64, passband_hz: f64) -> Option<Self> {
        if !(passband_hz > 0.0 && passband_hz < 0.45 * fs) {
            return None;
        }
        let stop = (passband_hz * 4.0 / 3.0).min(0.5 * fs);
        let a = CHANNEL_FILTER_DB;
        let n = (kaiser_taps(a, (stop - passband_hz) / fs).clamp(3, CHANNEL_FILTER_MAX_TAPS)) | 1;
        let cutoff = (passband_hz + stop) / 2.0;
        let d = (fs / (MULTIRATE_OVERSAMPLE * stop)).floor() as usize;
        if d >= MULTIRATE_MIN_DECIMATION {
            // The direct kernel's time span (n/fs) at fs/d, so a clamped design keeps its
            // (wider) transition and ENBW.
            let r = fs / d as f64;
            let nc = (n.div_ceil(d).max(3)) | 1;
            let taps = windowed_sinc(nc, cutoff / r, kaiser_beta(a), 1.0);
            let aa_n = (kaiser_taps(MULTIRATE_AA_DB, (r - 2.0 * stop) / fs).max(3)) | 1;
            let aa = windowed_sinc(aa_n, 0.5 / d as f64, kaiser_beta(MULTIRATE_AA_DB), 1.0);
            return Some(Self {
                enbw_hz: enbw(&taps, r),
                taps,
                multirate: Some(Multirate { d, aa }),
            });
        }
        let taps = windowed_sinc(n, cutoff / fs, kaiser_beta(a), 1.0);
        Some(Self {
            enbw_hz: enbw(&taps, fs),
            taps,
            multirate: None,
        })
    }

    /// Kernel length (at the kernel's rate).
    #[cfg(test)]
    pub fn taps_len(&self) -> usize {
        self.taps.len()
    }

    /// Whether the multirate form runs.
    #[cfg(test)]
    pub fn is_multirate(&self) -> bool {
        self.multirate.is_some()
    }

    /// Zero-delay filtering (`out.len() == x.len()`, zero outside `x`).
    pub fn apply(&self, x: &[Complex32], out: &mut Vec<Complex32>) {
        match &self.multirate {
            None => fir_zero_phase(&self.taps, x, out),
            Some(m) => self.apply_multirate(m, x, out),
        }
    }

    fn apply_multirate(&self, m: &Multirate, x: &[Complex32], out: &mut Vec<Complex32>) {
        let n = x.len();
        out.clear();
        if n == 0 {
            return;
        }
        let d = m.d as isize;
        let ha = (m.aa.len() / 2) as isize;
        let hc = (self.taps.len() / 2) as isize;
        // Interpolated output i reads kernel outputs at positions k·d within ±ha of i; each of
        // those reads decimated samples within ±hc; a decimated sample at k·d reads x within ±ha.
        let k_min = -(ha.div_euclid(d) + 1);
        let k_max = (n as isize - 1 + ha).div_euclid(d) + 1;
        let (y_lo, y_hi) = (k_min - hc, k_max + hc);
        let y: Vec<Complex32> = (y_lo..=y_hi)
            .map(|k| dot_zero_extended(&m.aa, x, k * d - ha))
            .collect();
        let z: Vec<Complex32> = (k_min..=k_max)
            .map(|k| dot_zero_extended(&self.taps, &y, k - hc - y_lo))
            .collect();
        // out[i] = d · Σ_k z[k] · aa[ha + i − k·d] (the interpolator has DC gain d).
        let gain = d as f32;
        out.extend((0..n as isize).map(|i| {
            let k0 = (i - ha + d - 1).div_euclid(d).max(k_min);
            let k1 = (i + ha).div_euclid(d).min(k_max);
            let mut acc = Complex32::default();
            for k in k0..=k1 {
                acc += z[(k - k_min) as usize] * m.aa[(ha + i - k * d) as usize];
            }
            acc * gain
        }));
    }
}

/// `fs · Σt² / (Σt)²`, Hz.
fn enbw(taps: &[f32], fs: f64) -> f64 {
    let sum: f64 = taps.iter().map(|&t| f64::from(t)).sum();
    let sum_sq: f64 = taps.iter().map(|&t| f64::from(t) * f64::from(t)).sum();
    fs * sum_sq / (sum * sum)
}

/// `Σ_j taps[j] · x[start + j]` with `x` zero outside its bounds (symmetric `taps`).
fn dot_zero_extended(taps: &[f32], x: &[Complex32], start: isize) -> Complex32 {
    let l = taps.len() as isize;
    let a = start.max(0);
    let b = (start + l).min(x.len() as isize);
    if b <= a {
        return Complex32::default();
    }
    let t0 = (a - start) as usize;
    kernels::dot_real(&taps[t0..], &x[a as usize..b as usize])
}

/// Zero-delay FIR of a symmetric odd-length kernel over zero-extended `x`.
fn fir_zero_phase(taps: &[f32], x: &[Complex32], out: &mut Vec<Complex32>) {
    let h = (taps.len() / 2) as isize;
    out.clear();
    out.extend((0..x.len() as isize).map(|i| dot_zero_extended(taps, x, i - h)));
}

/// Settings of one spectral line search.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LineSearch {
    /// Search `x^power`.
    pub power: u32,
    /// Search range of the `x^power` line, Hz.
    pub f_lo: f64,
    /// Search range of the `x^power` line, Hz.
    pub f_hi: f64,
    /// Floor on the significance threshold, dB.
    pub min_significance_db: f64,
    /// False-alarm probability for the adaptive threshold over the search bins.
    pub pfa: f64,
    /// Longest input used (centre part), samples.
    pub max_samples: usize,
}

/// The strongest line found (possibly below threshold: check `significance_db`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct LineHit {
    /// Line frequency in the `x^power` domain, Hz.
    pub freq_hz: f64,
    /// One-sigma frequency uncertainty in that domain (tone CRLB approximation), Hz.
    pub sigma_hz: f64,
    /// Whitened peak over its local median, dB.
    pub significance_db: f64,
    /// Adaptive threshold for this search, dB.
    pub threshold_db: f64,
    /// `|X(f)| / Σ w|x|^p`.
    pub coherence: f64,
    /// Amplitude ratio of the next line (outside ±3 native bins) to this one.
    pub second_ratio: f64,
    /// Samples used.
    pub samples: usize,
}

impl LineHit {
    pub fn significant(&self) -> bool {
        self.significance_db >= self.threshold_db
    }
}

/// Strongest discrete line of `x^power` in a range: Blackman periodogram zero-padded 4×,
/// whitened by block medians over 24 native bins (S5 `spectral_line`), log-parabolic peak
/// interpolation. The threshold adapts to the search size: `(ln S + ln 1/pfa) / ln 2` over the
/// median for exponential bins, floored at `min_significance_db`. `None` when too short or the
/// range holds fewer than 5 bins.
pub(crate) fn power_line(
    ws: &mut Workspace,
    x: &[Complex32],
    fs: f64,
    s: LineSearch,
) -> Option<LineHit> {
    let x = if x.len() > s.max_samples {
        let start = (x.len() - s.max_samples) / 2;
        &x[start..start + s.max_samples]
    } else {
        x
    };
    let n = x.len();
    if n < 32 || s.f_hi.partial_cmp(&s.f_lo) != Some(std::cmp::Ordering::Greater) {
        return None;
    }
    let big_n = (n * 4).next_power_of_two();
    let pad = big_n / n;
    let fft = ws.fft.entry(big_n).or_insert_with(|| CpuFft::new(big_n));
    let buf = &mut ws.line_buf;
    buf.clear();
    buf.resize(big_n, Complex32::default());
    let m = (n - 1).max(1) as f64;
    let mut weight_sum = 0.0f64;
    for (i, (&v, b)) in x.iter().zip(buf.iter_mut()).enumerate() {
        let r = i as f64 / m;
        let w = 0.42 - 0.5 * (TAU * r).cos() + 0.08 * (2.0 * TAU * r).cos();
        let z = match s.power {
            1 => v,
            2 => v * v,
            4 => {
                let q = v * v;
                q * q
            }
            p => v.powu(p),
        };
        weight_sum += w * f64::from(z.norm());
        *b = z * w as f32;
    }
    fft.forward(buf);
    // fftshift into power.
    let pw = &mut ws.line_pow;
    pw.clear();
    pw.extend(
        buf[big_n / 2..]
            .iter()
            .chain(&buf[..big_n / 2])
            .map(|c| c.norm_sqr()),
    );
    let df = fs / big_n as f64;
    let c = (big_n / 2) as f64;
    let i_lo = (s.f_lo / df + c).ceil().max(0.0) as usize;
    let i_hi = ((s.f_hi / df + c).floor() as usize).min(big_n - 1);
    if i_hi < i_lo + 4 {
        return None;
    }
    // Block medians over 24 native bins (at least 8 padded bins).
    let blk = (24 * pad).max(8).min(big_n);
    let nb = big_n / blk;
    if nb < 3 {
        return None;
    }
    let scratch = &mut ws.median_scratch;
    let mut medians = Vec::with_capacity(nb);
    for j in 0..nb {
        scratch.clear();
        scratch.extend_from_slice(&pw[j * blk..(j + 1) * blk]);
        let mid = blk / 2;
        let (_, med, _) = scratch.select_nth_unstable_by(mid, f32::total_cmp);
        medians.push(f64::from(*med).max(1e-30));
    }
    let local = |i: usize| -> f64 {
        let pos = (i as f64 + 0.5) / blk as f64 - 0.5;
        if pos <= 0.0 {
            medians[0]
        } else if pos >= (nb - 1) as f64 {
            medians[nb - 1]
        } else {
            let j = pos.floor() as usize;
            let t = pos - j as f64;
            medians[j] * (1.0 - t) + medians[j + 1] * t
        }
    };
    let mut best = (i_lo, 0.0f64);
    for (i, &p) in pw.iter().enumerate().take(i_hi + 1).skip(i_lo) {
        let r = f64::from(p) / local(i);
        if r > best.1 {
            best = (i, r);
        }
    }
    let (k, r) = best;
    let guard = 3 * pad + 1;
    let mut second = 0.0f64;
    for (i, &p) in pw.iter().enumerate().take(i_hi + 1).skip(i_lo) {
        if i.abs_diff(k) > guard {
            second = second.max(f64::from(p));
        }
    }
    let d = if k > 0 && k + 1 < big_n {
        let (a, b, cc) = (
            f64::from(pw[k - 1]).max(1e-30).ln(),
            f64::from(pw[k]).max(1e-30).ln(),
            f64::from(pw[k + 1]).max(1e-30).ln(),
        );
        let den = a - 2.0 * b + cc;
        if den < 0.0 {
            (0.5 * (a - cc) / den).clamp(-0.5, 0.5)
        } else {
            0.0
        }
    } else {
        0.0
    };
    let search_native = ((i_hi - i_lo + 1) as f64 / pad as f64).max(1.0);
    let thr_lin = (search_native.ln() + (1.0 / s.pfa).ln()) / LN_2;
    let threshold_db = s.min_significance_db.max(10.0 * thr_lin.log10());
    let line_to_noise = (r * LN_2 - 1.0).max(1e-6);
    let sigma_hz = fs * (12.0 * BLACKMAN_ENBW / line_to_noise).sqrt() / (TAU * n as f64);
    Some(LineHit {
        freq_hz: (k as f64 - c + d) * df,
        sigma_hz: sigma_hz.max(df * 0.05),
        significance_db: 10.0 * r.max(1e-30).log10(),
        threshold_db,
        coherence: f64::from(pw[k]).sqrt() / weight_sum.max(1e-30),
        second_ratio: (second / f64::from(pw[k]).max(1e-30)).sqrt(),
        samples: n,
    })
}

/// 1-D k-means with `k` clusters initialised at evenly spaced quantiles. Returns sorted
/// `(centre, variance, count)`.
pub(crate) fn kmeans_1d(v: &[f64], k: usize, iters: usize) -> Vec<(f64, f64, usize)> {
    if v.is_empty() || k == 0 {
        return Vec::new();
    }
    let mut sorted = v.to_vec();
    sorted.sort_by(f64::total_cmp);
    let mut centres: Vec<f64> = (0..k)
        .map(|j| sorted[((j as f64 + 0.5) / k as f64 * (sorted.len() - 1) as f64) as usize])
        .collect();
    let mut sums = vec![0.0; k];
    let mut sq = vec![0.0; k];
    let mut counts = vec![0usize; k];
    for _ in 0..iters {
        sums.fill(0.0);
        sq.fill(0.0);
        counts.fill(0);
        for &x in v {
            let j = nearest(&centres, x);
            sums[j] += x;
            sq[j] += x * x;
            counts[j] += 1;
        }
        let mut moved = false;
        for j in 0..k {
            if counts[j] > 0 {
                let c = sums[j] / counts[j] as f64;
                moved |= (c - centres[j]).abs() > 1e-9 * c.abs().max(1.0);
                centres[j] = c;
            }
        }
        if !moved {
            break;
        }
    }
    let mut out: Vec<(f64, f64, usize)> = (0..k)
        .map(|j| {
            let n = counts[j].max(1) as f64;
            let mean = sums[j] / n;
            (centres[j], (sq[j] / n - mean * mean).max(0.0), counts[j])
        })
        .collect();
    out.sort_by(|a, b| a.0.total_cmp(&b.0));
    out
}

fn nearest(centres: &[f64], x: f64) -> usize {
    let mut best = 0;
    for (j, c) in centres.iter().enumerate() {
        if (x - c).abs() < (x - centres[best]).abs() {
            best = j;
        }
    }
    best
}

/// Wraps an offset into `[−fs/2, fs/2)`.
pub(crate) fn wrap_offset(f: f64, fs: f64) -> f64 {
    (f + fs / 2.0).rem_euclid(fs) - fs / 2.0
}

/// `10·log10`.
pub(crate) fn db(x: f64) -> f64 {
    10.0 * x.log10()
}

/// Unwraps `phase` in place.
pub(crate) fn unwrap(phase: &mut [f64]) {
    for i in 1..phase.len() {
        let mut d = phase[i] - phase[i - 1];
        d -= TAU * (d / TAU).round();
        phase[i] = phase[i - 1] + d;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nfft_rule() {
        assert_eq!(choose_nfft(100), None);
        assert_eq!(choose_nfft(200), Some(64));
        assert_eq!(choose_nfft(2048), Some(256));
        assert_eq!(choose_nfft(1 << 20), Some(16384));
    }

    #[test]
    fn line_search_finds_a_tone() {
        let fs = 10_000.0;
        let f0 = 1234.56;
        let x: Vec<Complex32> = (0..4096)
            .map(|n| {
                let ph = TAU * f0 * n as f64 / fs;
                Complex32::new(ph.cos() as f32, ph.sin() as f32)
            })
            .collect();
        let mut ws = Workspace::default();
        let hit = power_line(
            &mut ws,
            &x,
            fs,
            LineSearch {
                power: 1,
                f_lo: 0.0,
                f_hi: 3000.0,
                min_significance_db: 12.0,
                pfa: 1e-3,
                max_samples: 1 << 20,
            },
        )
        .unwrap();
        assert!(hit.significant());
        assert!((hit.freq_hz - f0).abs() < 0.05, "{}", hit.freq_hz);
        let sq = power_line(
            &mut ws,
            &x,
            fs,
            LineSearch {
                power: 2,
                f_lo: 0.0,
                f_hi: 4999.0,
                min_significance_db: 12.0,
                pfa: 1e-3,
                max_samples: 1 << 20,
            },
        )
        .unwrap();
        assert!((sq.freq_hz - 2.0 * f0).abs() < 0.1, "{}", sq.freq_hz);
    }

    #[test]
    fn kmeans_separates_two_levels() {
        let v: Vec<f64> = (0..1000)
            .map(|i| if i % 3 == 0 { -5.0 } else { 7.0 } + ((i * 37) % 11) as f64 * 0.01)
            .collect();
        let c = kmeans_1d(&v, 2, 30);
        assert!(
            (c[0].0 + 4.95).abs() < 0.1 && (c[1].0 - 7.05).abs() < 0.1,
            "{c:?}"
        );
    }

    #[test]
    fn wraps_offsets() {
        assert_eq!(wrap_offset(6e6, 10e6), -4e6);
        assert_eq!(wrap_offset(-5e6, 10e6), -5e6);
        assert_eq!(wrap_offset(5e6, 10e6), -5e6);
    }

    /// The pre-T-081 channel filter: direct Kaiser kernel at `fs`, scalar loop.
    fn reference_filter(fs: f64, passband_hz: f64, x: &[Complex32]) -> (Vec<Complex32>, f64) {
        let stop = (passband_hz * 4.0 / 3.0).min(0.5 * fs);
        let n = (kaiser_taps(40.0, (stop - passband_hz) / fs).clamp(3, 8191)) | 1;
        let taps = windowed_sinc(n, (passband_hz + stop) / 2.0 / fs, kaiser_beta(40.0), 1.0);
        let (l, d, len) = (taps.len(), taps.len() / 2, x.len());
        let out = (0..len)
            .map(|i| {
                let lo = (i + d + 1).saturating_sub(len);
                let hi = (i + d).min(l - 1);
                let mut acc = Complex32::default();
                for m in lo..=hi {
                    acc += x[i + d - m] * taps[m];
                }
                acc
            })
            .collect();
        (out, enbw(&taps, fs))
    }

    /// Noise, an in-band tone, a gated in-band tone (a burst edge at n/3 and 2n/3) and a strong
    /// out-of-band tone.
    fn filter_scene(n: usize, fs: f64, passband_hz: f64) -> Vec<Complex32> {
        let mut s = 0x9E37_79B9_7F4A_7C15u64;
        let mut u = move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            (s >> 11) as f64 / (1u64 << 53) as f64 - 0.5
        };
        (0..n)
            .map(|i| {
                let t = i as f64 / fs;
                let ph = |f: f64| Complex32::from_polar(1.0, (TAU * f * t) as f32);
                let gate = if (n / 3..2 * n / 3).contains(&i) {
                    0.5
                } else {
                    0.0
                };
                ph(0.3 * passband_hz) * 0.8
                    + ph(-0.6 * passband_hz) * gate
                    + ph(5.0 * passband_hz) * 2.0
                    + Complex32::new(u() as f32, u() as f32)
            })
            .collect()
    }

    fn rel_error_db(a: &[Complex32], b: &[Complex32]) -> f64 {
        let e: f64 = a
            .iter()
            .zip(b)
            .map(|(x, y)| f64::from((x - y).norm_sqr()))
            .sum();
        let p: f64 = b.iter().map(|y| f64::from(y.norm_sqr())).sum();
        10.0 * (e / p).log10()
    }

    #[test]
    fn direct_channel_filter_matches_the_reference() {
        let fs = 500e3;
        for pass in [40e3, 100e3, 150e3] {
            let x = filter_scene(30_000, fs, pass);
            let f = ChannelFilter::new(fs, pass).unwrap();
            assert!(!f.is_multirate(), "{pass}");
            let mut y = Vec::new();
            f.apply(&x, &mut y);
            let (r, enbw_ref) = reference_filter(fs, pass, &x);
            assert_eq!(y.len(), r.len());
            let err = rel_error_db(&y, &r);
            assert!(err < -100.0, "{pass} Hz: {err:.1} dB");
            assert!((f.enbw_hz / enbw_ref - 1.0).abs() < 1e-9);
        }
    }

    #[test]
    fn multirate_channel_filter_matches_the_reference_within_tolerance() {
        let fs = 500e3;
        // 160 Hz: the clamped 8191-tap case of the dense urban replay (T-081).
        for (pass, n) in [
            (160.0, 20_000),
            (750.0, 40_000),
            (3e3, 40_000),
            (10e3, 40_000),
        ] {
            let x = filter_scene(n, fs, pass);
            let f = ChannelFilter::new(fs, pass).unwrap();
            assert!(f.is_multirate(), "{pass}");
            let mut y = Vec::new();
            f.apply(&x, &mut y);
            let (r, enbw_ref) = reference_filter(fs, pass, &x);
            assert_eq!(y.len(), n);
            let err = rel_error_db(&y, &r);
            eprintln!(
                "{pass} Hz: {} kernel taps, output differs by {err:.1} dB, ENBW {:.2} vs {enbw_ref:.2}",
                f.taps_len(),
                f.enbw_hz
            );
            assert!(err < -40.0, "{pass} Hz: output differs by {err:.1} dB");
            assert!((f.enbw_hz / enbw_ref - 1.0).abs() < 0.01, "{pass} Hz ENBW");
        }
    }
}
