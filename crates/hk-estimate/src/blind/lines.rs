//! Cyclic lines (S5 `spectral_line`, `rate_line`, `carrier_lines`).
//!
//! A rate line is the strongest discrete line of a real feature series in `[f_min, f_max]`,
//! judged against a **locally whitened** periodogram: Blackman window, 4× zero padding, block
//! medians linearly interpolated. A line on a sloped continuum (the PSK |x|² line at Rs) is judged
//! against its neighbourhood, not the band maximum (S5 pitfall 2). The mean is removed first.
//!
//! # The whitening block width is given in Hz, not in bins (T-327)
//!
//! Until T-327 the block was **24 native bins**, i.e. `24·fs/n` Hz: its width was a function of how
//! long the record happened to be. The same emitter watched for 1/8 as long was judged against a
//! floor estimated over an 8× wider slice of spectrum, so its `significance_db` was not the same
//! measurement — the defect T-310 found behind `cyclic_db`'s window dependence. `spectral_line`
//! therefore takes `block_hz`, a width the caller pins from the **signal**
//! (`BlindConfig::whiten_block_obw × OBW99`), never from `n`.
//!
//! Two bounds meet in that width, and they pull opposite ways:
//!
//! - **Fidelity (upper bound).** The median has to follow the continuum it is flattening. Every
//!   spectral scale in these feature series is set by the occupied bandwidth — the |x|² shoulder,
//!   the IF-noise shelf, the pulse-shape rolloff, and the search band's own upper edge at
//!   `1.2·OBW99` — so the width is stated as a fraction of OBW99. Too *wide* a block stops tracking
//!   the slope and judges a line in a trough against its neighbours' continuum instead of its own
//!   (S5 pitfall 2 returning). **Measured, not asserted:** the hardest such line in the real
//!   fixtures is broadcast FM's 19 kHz stereo pilot, which sits at 0.085·OBW99 on the steep low
//!   edge of a 225 kHz emission and which C14's card names as a false-line pitfall. Over the 24
//!   analog windows of `fm_100p8M_2p4M_l32g30a1_t1p5_5s`, a block of **OBW99/4 gives that pilot a
//!   trusted symbol rate in 1 window of 24**; OBW99/8 and every narrower width give 0 of 24
//!   (`blind_real::aware_036_blind_fm_broadcast_is_untrusted` is that check). So the fidelity bound
//!   sits between OBW99/4 and OBW99/8, and the shipped OBW99/8 keeps a full factor of two below
//!   where a real emission was measured to break.
//!
//!   Going *narrower* than OBW99/8 buys nothing and costs: on the dev grid it is strictly worse on
//!   every window-consistency measure (mixed-length class separation `F` 54.6 at OBW99/8 against
//!   51.3 at OBW99/16 and 47.7 at OBW99/32; sd of `cyclic_db` across N/8…N 4.47 dB against 4.60 and
//!   4.71), because the statistical floor below starts binding instead.
//! - **Statistics (lower bound, [`MIN_WHITEN_NATIVE_BINS`]).** Whitened periodogram bins are
//!   exponential; only *native* bins are independent, since 4× zero padding interpolates rather
//!   than adds information. The relative sd of the median of `m` exponentials is about
//!   `1/(√m · ln 2)`, so `m = 24` costs about 1.2 dB of jitter on the floor estimate and `m = 6`
//!   about 2.5 dB. Too *narrow* a block also starts following the line itself: a line occupies
//!   [`BLACKMAN_ENBW`] ≈ 1.73 native bins, which is 7 % of a 24-bin block and 29 % of a 6-bin one.
//!
//! The lower bound is the one place `n` survives, and it cannot be removed: a record that holds
//! fewer than `MIN_WHITEN_NATIVE_BINS` cells inside the pinned width genuinely cannot estimate a
//! floor there. When it binds, the block is widened to the statistical minimum and the caller is
//! told ([`RawLine::whiten_clamped`]) rather than silently measured at a different geometry.

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
    /// The record was too short to hold [`MIN_WHITEN_NATIVE_BINS`] native cells inside the pinned
    /// `block_hz`, so the block was widened to the statistical minimum: this significance was
    /// **not** measured at the pinned geometry.
    pub whiten_clamped: bool,
}

/// Blackman equivalent noise bandwidth, bins.
const BLACKMAN_ENBW: f64 = 1.73;

/// Independent (native) periodogram cells a whitening block must hold for its median to be a floor
/// estimate rather than a sample of one.
///
/// Whitened bins are exponential, and the median of `m` of them has relative sd ≈ `1/(√m · ln 2)`:
/// 1.2 dB at 24, 1.7 dB at 12, 2.5 dB at 6. 24 is where that jitter stops being comparable with
/// the 12 dB line-counting threshold this significance is read against, and it also keeps a line
/// (≈ [`BLACKMAN_ENBW`] = 1.73 native bins) down to 7 % of its own block, so the block does not
/// whiten away what it is judging. Raising it makes the floor smoother but forces the clamp below
/// to bind on longer records; lowering it lets the floor estimate wander by more than the
/// significances it produces are being compared at.
const MIN_WHITEN_NATIVE_BINS: f64 = 24.0;

/// Native bins at the low end of the search that a mean-removed Blackman periodogram cannot report
/// a line in, because what sits there is the window's own response to DC and to any trend.
///
/// The Blackman mainlobe is ±3 bins wide; removing the mean nulls the centre of it but not its
/// skirts, and a feature series always carries slow structure (AGC, fading, envelope drift) for
/// those skirts to act on. 4 is the first integer clear of the mainlobe.
///
/// This is a property of the **transform**, not of the search band — which is why the search band
/// no longer carries it (T-327). Too small and one cycle over the record is reported as a symbol
/// rate; too large and a genuinely resolvable slow line just above the mainlobe is refused, and
/// C14 reports no line instead of the rate it could have measured.
const DC_GUARD_NATIVE_BINS: f64 = 4.0;

/// Strongest locally whitened line of the real series `y` in `[f_min, f_max]`, judged against a
/// floor estimated over blocks `block_hz` wide (see the [module docs](self)).
///
/// `None` when the series is shorter than 32 samples or the range holds fewer than 5 bins.
pub(crate) fn spectral_line(
    plans: &mut Plans,
    y: &[f64],
    fs: f64,
    f_min: f64,
    f_max: f64,
    block_hz: f64,
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
    // The whitening block, pinned in Hz and floored at the statistical minimum. `pad` converts a
    // native cell (fs/n) into padded bins; only native cells are independent.
    let pad = big_n as f64 / n as f64;
    let native_hz = fs / n as f64;
    let want_native = if block_hz.is_finite() && block_hz > 0.0 {
        block_hz / native_hz
    } else {
        MIN_WHITEN_NATIVE_BINS
    };
    let whiten_clamped = want_native < MIN_WHITEN_NATIVE_BINS;
    let blk_native = want_native.max(MIN_WHITEN_NATIVE_BINS);
    let blk = ((blk_native * pad).round() as usize).clamp(8, big_n / 3);
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
    // The search band is pinned by the caller; this raises only the first *usable* bin, which is a
    // limit of the transform over this record rather than a change of geometry.
    let f_lo = f_min.max(DC_GUARD_NATIVE_BINS * native_hz);
    let i_lo = ((f_lo / df) + half as f64).ceil().max(0.0) as usize;
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
        whiten_clamped,
    })
}

/// A complex series: the stronger of the real-part and imaginary-part lines (S5 `rate_line`).
pub(crate) fn complex_line(
    plans: &mut Plans,
    y: &[num_complex::Complex<f64>],
    fs: f64,
    f_min: f64,
    f_max: f64,
    block_hz: f64,
) -> Option<RawLine> {
    let re: Vec<f64> = y.iter().map(|c| c.re).collect();
    let im: Vec<f64> = y.iter().map(|c| c.im).collect();
    let a = spectral_line(plans, &re, fs, f_min, f_max, block_hz);
    let b = spectral_line(plans, &im, fs, f_min, f_max, block_hz);
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
        // Block width pinned in Hz: 40 Hz is well inside the red-noise slope and comfortably above
        // the 24-native-bin minimum (n = 8192 gives a native cell of 0.12 Hz).
        let l = spectral_line(&mut plans, &y, fs, 5.0, 400.0, 40.0).unwrap();
        assert!((l.freq_hz - 123.4).abs() < 0.1, "{l:?}");
        assert!(l.significance_db > 14.0, "{l:?}");
        assert!(!l.whiten_clamped, "{l:?}");
    }

    /// **The whitening block is pinned in Hz, so it does not move with the record** (T-327).
    ///
    /// The same tone in the same red noise, measured over the full series and over an eighth of
    /// it: the block covers the same span of spectrum in both, and neither reading is clamped.
    /// Before T-327 the block was 24 *native* bins — `24·fs/n` Hz — and the short record was
    /// judged against a floor estimated over 8× more spectrum.
    #[test]
    fn the_whitening_block_width_does_not_follow_the_record_length() {
        let fs = 1000.0;
        let n = 8192;
        let mut rng = hk_dsp::synth::Rng::new(7);
        let mut acc = 0.0;
        let y: Vec<f64> = (0..n)
            .map(|i| {
                let (g, _) = rng.gaussian_pair();
                acc = 0.995 * acc + g;
                acc + 1.2 * (TAU * 123.4 * i as f64 / fs).sin()
            })
            .collect();
        let mut plans = Plans::default();
        let long = spectral_line(&mut plans, &y, fs, 5.0, 400.0, 40.0).unwrap();
        let short = spectral_line(&mut plans, &y[..n / 8], fs, 5.0, 400.0, 40.0).unwrap();
        // 40 Hz over a native cell of fs/n: 328 cells at n, 41 at n/8 — both above the minimum.
        assert!(
            !long.whiten_clamped && !short.whiten_clamped,
            "{long:?} {short:?}"
        );
        assert!((long.freq_hz - 123.4).abs() < 0.2, "{long:?}");
        assert!((short.freq_hz - 123.4).abs() < 2.0, "{short:?}");
        // An eighth of the record at the same block width: the floor the line is judged against is
        // the same one, so the significance differs by the noise of a shorter record, not by the
        // 8× change of whitening span the old geometry imposed.
        assert!(
            (long.significance_db - short.significance_db).abs() < 8.0,
            "{long:?} {short:?}"
        );
    }

    /// A record too short to hold [`MIN_WHITEN_NATIVE_BINS`] cells inside the pinned width **says
    /// so** rather than being silently measured at a wider block.
    #[test]
    fn a_record_too_short_for_the_pinned_block_reports_the_clamp() {
        let fs = 1000.0;
        let mut rng = hk_dsp::synth::Rng::new(9);
        // n = 256 gives a native cell of 3.9 Hz, so 40 Hz holds only ~10 cells: under the minimum.
        let y: Vec<f64> = (0..256)
            .map(|i| {
                let (g, _) = rng.gaussian_pair();
                g + 1.2 * (TAU * 123.4 * i as f64 / fs).sin()
            })
            .collect();
        let mut plans = Plans::default();
        let l = spectral_line(&mut plans, &y, fs, 5.0, 400.0, 40.0).unwrap();
        assert!(l.whiten_clamped, "{l:?}");
    }
}
