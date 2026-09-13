//! Internal DSP helpers: cached Welch engines and FFT plans, mixing, the channel filter and the
//! power-of-M spectral line search. Buffers are sized per call; no per-sample allocation.

use std::collections::HashMap;
use std::f64::consts::{LN_2, TAU};
use std::ops::Range;

use hk_dsp::fft::{CpuFft, FftBackend};
use hk_dsp::filter::design::windowed_sinc;
use hk_dsp::filter::{kaiser_beta, kaiser_taps};
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

/// A symmetric low-pass channel filter at unity DC gain.
pub(crate) struct ChannelFilter {
    taps: Vec<f32>,
    /// Equivalent noise bandwidth, Hz.
    pub enbw_hz: f64,
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
        let a = 40.0;
        let n = (kaiser_taps(a, (stop - passband_hz) / fs).clamp(3, 8191)) | 1;
        let cutoff = (passband_hz + stop) / 2.0 / fs;
        let taps = windowed_sinc(n, cutoff, kaiser_beta(a), 1.0);
        let sum: f64 = taps.iter().map(|&t| f64::from(t)).sum();
        let sum_sq: f64 = taps.iter().map(|&t| f64::from(t) * f64::from(t)).sum();
        Some(Self {
            taps,
            enbw_hz: fs * sum_sq / (sum * sum),
        })
    }

    /// Zero-delay filtering (`out.len() == x.len()`, zero outside `x`).
    pub fn apply(&self, x: &[Complex32], out: &mut Vec<Complex32>) {
        let l = self.taps.len();
        let d = l / 2;
        let n = x.len();
        out.clear();
        out.extend((0..n).map(|i| {
            // taps[m] multiplies x[i + d − m], for 0 <= i + d − m < n.
            let lo = (i + d + 1).saturating_sub(n);
            let hi = (i + d).min(l - 1);
            let mut acc = Complex32::default();
            for m in lo..=hi {
                acc += x[i + d - m] * self.taps[m];
            }
            acc
        }));
    }
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
}
