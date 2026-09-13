//! Cyclic lines (S5 `spectral_line`, `rate_line`, `carrier_lines`).
//!
//! A rate line is the strongest discrete line of a real feature series in `[f_min, f_max]`,
//! judged against a **locally whitened** periodogram: Blackman window, 4× zero padding, block
//! medians over 24 native bins, linearly interpolated. A line on a sloped continuum (the PSK
//! |x|² line at Rs) is judged against its neighbourhood, not the band maximum (S5 pitfall 2).
//! The mean is removed first.

use std::collections::HashMap;
use std::f64::consts::TAU;

use hk_dsp::fft::{CpuFft, FftBackend};
use num_complex::Complex32;

/// FFT plans by length.
#[derive(Default)]
pub(crate) struct Plans {
    fft: HashMap<usize, CpuFft>,
    buf: Vec<Complex32>,
}

impl Plans {
    fn fft(&mut self, n: usize) -> (&mut CpuFft, &mut Vec<Complex32>) {
        let f = self.fft.entry(n).or_insert_with(|| CpuFft::new(n));
        (f, &mut self.buf)
    }
}

/// A raw line: frequency, whitened significance (dB) and a CRLB-style frequency sigma.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct RawLine {
    pub freq_hz: f64,
    pub significance_db: f64,
    pub sigma_hz: f64,
}

/// Blackman equivalent noise bandwidth, bins.
const BLACKMAN_ENBW: f64 = 1.73;

/// Strongest locally whitened line of the real series `y` in `[f_min, f_max]`. `None` when the
/// series is shorter than 32 samples or the range holds fewer than 5 bins.
pub(crate) fn spectral_line(
    plans: &mut Plans,
    y: &[f64],
    fs: f64,
    f_min: f64,
    f_max: f64,
) -> Option<RawLine> {
    let n = y.len();
    if n < 32 || f_max.partial_cmp(&f_min) != Some(std::cmp::Ordering::Greater) {
        return None;
    }
    let big_n = (n * 4).next_power_of_two();
    let m = y.iter().sum::<f64>() / n as f64;
    let (fft, buf) = plans.fft(big_n);
    buf.clear();
    buf.resize(big_n, Complex32::default());
    let d = (n - 1).max(1) as f64;
    for (i, (&v, b)) in y.iter().zip(buf.iter_mut()).enumerate() {
        let r = i as f64 / d;
        let w = 0.42 - 0.5 * (TAU * r).cos() + 0.08 * (2.0 * TAU * r).cos();
        *b = Complex32::new(((v - m) * w) as f32, 0.0);
    }
    fft.forward(buf);
    // fftshift: index i ↔ frequency (i − N/2)·fs/N.
    let half = big_n / 2;
    let pw: Vec<f64> = buf[half..]
        .iter()
        .chain(&buf[..half])
        .map(|c| f64::from(c.norm_sqr()))
        .collect();
    let blk = ((24 * big_n) / n).max(8);
    let nb = big_n / blk;
    if nb < 3 {
        return None;
    }
    let mut scratch = Vec::with_capacity(blk);
    let medians: Vec<f64> = (0..nb)
        .map(|j| {
            scratch.clear();
            scratch.extend_from_slice(&pw[j * blk..(j + 1) * blk]);
            super::util::median(&scratch)
        })
        .collect();
    // np.interp(arange(N), (arange(nb) + 0.5)·blk, medians): clamped at the ends.
    let local = |i: usize| -> f64 {
        let pos = (i as f64 - 0.5 * blk as f64) / blk as f64;
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
    let df = fs / big_n as f64;
    let i_lo = ((f_min / df) + half as f64).ceil().max(0.0) as usize;
    let i_hi = (((f_max / df) + half as f64).floor().max(0.0) as usize).min(big_n - 1);
    if i_hi < i_lo + 4 {
        return None;
    }
    let mut best = (i_lo, f64::MIN);
    for (i, &p) in pw.iter().enumerate().take(i_hi + 1).skip(i_lo) {
        let r = p / (local(i) + 1e-30);
        if r > best.1 {
            best = (i, r);
        }
    }
    let (k, r) = best;
    let dk = if k > 0 && k + 1 < big_n {
        let (a, b, c) = (
            (pw[k - 1] + 1e-30).ln(),
            (pw[k] + 1e-30).ln(),
            (pw[k + 1] + 1e-30).ln(),
        );
        let den = a - 2.0 * b + c;
        if den != 0.0 {
            (0.5 * (a - c) / den).clamp(-0.5, 0.5)
        } else {
            0.0
        }
    } else {
        0.0
    };
    // Whitened bins are exponential with median ln 2: line-to-noise ≈ r·ln 2 − 1.
    let lnr = (r * std::f64::consts::LN_2 - 1.0).max(1e-6);
    let sigma_hz = (fs * (12.0 * BLACKMAN_ENBW / lnr).sqrt() / (TAU * n as f64)).max(0.05 * df);
    Some(RawLine {
        freq_hz: (k as f64 - half as f64 + dk) * df,
        significance_db: 10.0 * r.max(1e-30).log10(),
        sigma_hz,
    })
}

/// A complex series: the stronger of the real-part and imaginary-part lines (S5 `rate_line`).
pub(crate) fn complex_line(
    plans: &mut Plans,
    y: &[num_complex::Complex<f64>],
    fs: f64,
    f_min: f64,
    f_max: f64,
) -> Option<RawLine> {
    let re: Vec<f64> = y.iter().map(|c| c.re).collect();
    let im: Vec<f64> = y.iter().map(|c| c.im).collect();
    let a = spectral_line(plans, &re, fs, f_min, f_max);
    let b = spectral_line(plans, &im, fs, f_min, f_max);
    match (a, b) {
        (Some(a), Some(b)) => Some(if a.significance_db >= b.significance_db {
            a
        } else {
            b
        }),
        (a, b) => a.or(b),
    }
}

/// Power-of-p carrier line of `x^p` (S5 `carrier_lines`): coherence `max|FFT| / Σ|x|^p`
/// (≈ 1 for a pure carrier of `x^p`), the carrier frequency / p, and the amplitude ratio of the
/// second-largest line (outside ±3 native bins, circular) to the largest. Unwindowed, 2× pad.
pub(crate) fn carrier_line(plans: &mut Plans, x: &[Complex32], fs: f64, p: u32) -> (f64, f64, f64) {
    let n = x.len();
    if n < 8 {
        return (0.0, 0.0, 1.0);
    }
    let big_n = (n * 2).next_power_of_two();
    let (fft, buf) = plans.fft(big_n);
    buf.clear();
    buf.resize(big_n, Complex32::default());
    let mut abs_sum = 0.0f64;
    for (&v, b) in x.iter().zip(buf.iter_mut()) {
        let z = match p {
            1 => v,
            2 => v * v,
            4 => {
                let q = v * v;
                q * q
            }
            _ => v.powu(p),
        };
        abs_sum += f64::from(z.norm());
        *b = z;
    }
    fft.forward(buf);
    let mag: Vec<f64> = buf.iter().map(|c| f64::from(c.norm())).collect();
    let (mut k, mut peak) = (0usize, 0.0f64);
    for (i, &m) in mag.iter().enumerate() {
        if m > peak {
            peak = m;
            k = i;
        }
    }
    let guard = 3 * big_n / n + 1;
    let mut second = 0.0f64;
    for (i, &m) in mag.iter().enumerate() {
        let d = i.abs_diff(k);
        if d.min(big_n - d) > guard {
            second = second.max(m);
        }
    }
    let f = if k < big_n / 2 {
        k as f64
    } else {
        k as f64 - big_n as f64
    } * fs
        / big_n as f64;
    (
        peak / (abs_sum + 1e-12),
        f / f64::from(p),
        second / (peak + 1e-12),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_a_line_on_a_sloped_continuum() {
        let fs = 1000.0;
        let n = 8192;
        let mut rng = hk_dsp::synth::Rng::new(1);
        // Red noise (strong slope) plus a weak tone at 123.4 Hz.
        let mut acc = 0.0;
        let y: Vec<f64> = (0..n)
            .map(|i| {
                let (g, _) = rng.gaussian_pair();
                acc = 0.995 * acc + g;
                acc + 0.8 * (TAU * 123.4 * i as f64 / fs).sin()
            })
            .collect();
        let mut plans = Plans::default();
        let l = spectral_line(&mut plans, &y, fs, 5.0, 400.0).unwrap();
        assert!((l.freq_hz - 123.4).abs() < 0.1, "{l:?}");
        assert!(l.significance_db > 14.0, "{l:?}");
    }
}
