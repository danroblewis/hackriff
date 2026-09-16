//! Chirp-rate estimation inside **one** analysis frame (T-294).
//!
//! # Why this exists
//!
//! T-255 replayed a LoRa SF9/125 kHz packet through the pipeline and found the whole sweep falls
//! inside one detection frame: `T_sym = 2^9 / 125 kHz = 4.096 ms`, and the detector's frame at
//! 500 kS/s is `K · N / fs = 4 · 512 / 500 kHz = 4.096 ms`. Exactly one. It concluded that the
//! diagonal was not merely un-drawable but **unobservable** — "nothing in the run separates the
//! chirp from a 125 kHz wideband burst of the same duration".
//!
//! That conclusion is true of the **product the detector consumes** — `K` averaged periodograms,
//! from which one `(f_lo, f_hi)` per component is formed — and **false of the IQ**. This module is
//! the measurement that separates the two, so ADR-0017 §1.3 can say which limit applies where
//! instead of implying a single one. Nothing here is wired into the pipeline and no detector
//! threshold moves; it exists so the ADR's claim is a measured fact in the repository rather than
//! an assertion in a document.
//!
//! # The estimator
//!
//! For a linear chirp `x(t) = A·exp(j2π(f₀t + αt²/2))` the **lag product**
//!
//! ```text
//! y(t) = x(t)·conj(x(t − τ)) = A²·exp(j2π(ατ·t + f₀τ − ατ²/2))
//! ```
//!
//! is a **pure tone at `ατ`**, whatever `f₀` is. One FFT of `y` therefore recovers the sweep rate
//! as `α = f_peak / τ`, and the peak-to-average power ratio (PAPR) of that spectrum says whether
//! there was a linear sweep at all. This is the delay-multiply / discrete polynomial-phase
//! estimator (Peleg & Porat); the same structure underlies the high-order ambiguity function.
//!
//! The lag product's **mean is removed first**, and that is what makes the statistic a *sweep*
//! test rather than an energy test: a stationary emitter's lag product is a constant, so removing
//! the mean leaves it with no line. A carrier scores like noise here, by construction.
//!
//! # One lag is not a chirp test
//!
//! Measured (400 trials per cell, one 4.096 ms frame, 12 dB SNR in a 125 kHz channel): a LoRa
//! chirp's PAPR is ~850 against a band-limited-Gaussian burst's ~20, but the scene's own 2-FSK
//! burst scores ~103 — an FSK lag product has lines too. "There is a peak" is not "there is a
//! sweep".
//!
//! What only a linear sweep does is give **the same `α` at every lag**, because the tone sits at
//! `ατ` by construction. [`linear_sweep`] therefore estimates at two lags and requires both a peak
//! and agreement on the rate. Measured with that rule, at 12 dB, over the four species of T-255's
//! scene:
//!
//! | species | called a sweep |
//! |---|---|
//! | LoRa SF9 chirp | **99.5 %** |
//! | band-limited wideband burst | 0.0 % |
//! | CW carrier | 0.0 % |
//! | 2-FSK burst | 2.2 % |
//!
//! and the recovered rate is within 1.6 % of `BW²/2^SF`.
//!
//! # The observability floor is an SNR, not a frame
//!
//! Same test, same single frame, against SNR **in the channel** (chirp / wideband / carrier /
//! 2-FSK called a sweep):
//!
//! | SNR | +12 dB | +3 dB | 0 dB | −3 dB | −6 dB | −9 dB |
//! |---|---|---|---|---|---|---|
//! | chirp | 99.5 % | 99.2 % | 97.8 % | 96.0 % | 27.8 % | 0.5 % |
//! | the three controls | ≤ 2.2 % | ≤ 2.2 % | ≤ 1.2 % | 0 % | 0 % | 0 % |
//!
//! So a sweep contained inside one analysis frame is recoverable down to about **−3 dB in its own
//! channel**, and collapses by −6 dB. T-255's scene puts its chirp at 12 dB.
//!
//! # Choosing the lags
//!
//! Two constraints pull against each other:
//!
//! - **`τ` must be long enough** that `ατ` lands well above one FFT bin. The first run of this
//!   measurement used `τ` = 1 sample, where `ατ` = 61 Hz against a 244 Hz bin, and every estimate
//!   came out an exact multiple of the truth — quantisation, not error.
//! - **`τ` must be short enough** that the tone stays unambiguous (`|α| < fs²/2τ_samples`) and that
//!   the `τ` samples straddling a fold — a LoRa chirp folds once per symbol — stay a small part of
//!   the frame.
//!
//! [`default_lags`] takes `n/32` and `n/16`, which is the pair measured above at `n = 2048`.
//! Resolution is `fs²/(n·τ_samples)`; both are returned so a caller can check them.

use num_complex::{Complex, Complex32};

use crate::fft::{CpuFft, FftBackend};
use crate::window::{Window, WindowKind};

/// One lag's estimate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChirpRate {
    /// Sweep rate `α`, Hz per second. Signed: negative is a down-chirp.
    pub rate_hz_per_s: f64,
    /// Peak-to-average power ratio of the lag-product spectrum (1 for a flat spectrum).
    pub papr: f64,
    /// Smallest rate difference this lag can resolve, `fs²/(n·τ_samples)`, Hz per second.
    pub resolution_hz_per_s: f64,
}

/// A two-lag verdict: a linear sweep was found and both lags agree on its rate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SweepTest {
    /// Mean of the two lags' rates, Hz per second.
    pub rate_hz_per_s: f64,
    /// The weaker of the two lags' PAPRs.
    pub papr: f64,
    /// `|α₁ − α₂| / |α₁|`: how well the two lags agree.
    pub disagreement: f64,
}

/// PAPR below which no sweep is claimed (40).
///
/// Measured as the 99th percentile of a band-limited Gaussian burst's PAPR over 400 trials at each
/// SNR from +18 to −9 dB, which ranged 31.6–39.2. This is a parameter of *this* function; it is
/// not a detector threshold and nothing in the detection path reads it.
pub const SWEEP_MIN_PAPR: f64 = 40.0;

/// Largest `|α₁ − α₂|/|α₁|` still called agreement (0.10).
///
/// A true linear sweep's two lags agree to within their coarser resolution, which at the
/// [`default_lags`] is a few percent; 2-FSK's spurious lines do not agree at all.
pub const SWEEP_MAX_DISAGREEMENT: f64 = 0.10;

/// The measured lag pair for a frame of `n` samples: `(n/32, n/16)`, each at least 1.
pub fn default_lags(n: usize) -> (usize, usize) {
    ((n / 32).max(1), (n / 16).max(1))
}

/// Estimates the sweep rate of `x` at one lag, or `None` if `x` is too short or degenerate.
///
/// `lag` is in samples. See the module docs for how to choose it; [`default_lags`] is the
/// measured pair.
pub fn chirp_rate(x: &[Complex32], sample_rate_hz: f64, lag: usize) -> Option<ChirpRate> {
    if lag == 0 || x.len() <= lag + 1 || sample_rate_hz <= 0.0 || sample_rate_hz.is_nan() {
        return None;
    }
    let n = x.len() - lag;
    let mut y: Vec<Complex32> = (0..n).map(|i| x[i + lag] * x[i].conj()).collect();

    // Remove the mean: a stationary emitter's lag product is a constant, so this is what makes the
    // statistic answer "is there a NON-ZERO sweep rate" rather than "is there energy".
    let sum = y.iter().fold(Complex::<f64>::new(0.0, 0.0), |a, v| {
        a + Complex::new(f64::from(v.re), f64::from(v.im))
    });
    let mean = sum / n as f64;
    let mean = Complex32::new(mean.re as f32, mean.im as f32);
    let window = Window::new(WindowKind::Hann, n);
    for (v, c) in y.iter_mut().zip(window.coefficients()) {
        *v = (*v - mean) * *c;
    }

    CpuFft::new(n).forward(&mut y);
    let (mut peak, mut peak_bin, mut total) = (0.0f64, 0usize, 0.0f64);
    for (k, v) in y.iter().enumerate() {
        let p = f64::from(v.norm_sqr());
        total += p;
        if p > peak {
            peak = p;
            peak_bin = k;
        }
    }
    if total <= 0.0 || !total.is_finite() {
        return None;
    }

    // FFT order: DC at 0, bins above n/2 are negative frequencies, so a down-chirp reads negative.
    let k = if peak_bin <= n / 2 {
        peak_bin as f64
    } else {
        peak_bin as f64 - n as f64
    };
    let tau_s = lag as f64 / sample_rate_hz;
    let bin_hz = sample_rate_hz / n as f64;
    Some(ChirpRate {
        rate_hz_per_s: k * bin_hz / tau_s,
        papr: peak / (total / n as f64),
        resolution_hz_per_s: bin_hz / tau_s,
    })
}

/// Whether `x` carries a linear sweep, by agreement between two lags.
///
/// Returns `None` when either lag fails to peak above `min_papr`, when the rate is below what the
/// finer lag can resolve (a stationary emitter), or when the two lags disagree by more than
/// `max_disagreement`. [`SWEEP_MIN_PAPR`] and [`SWEEP_MAX_DISAGREEMENT`] are the measured values.
pub fn linear_sweep(
    x: &[Complex32],
    sample_rate_hz: f64,
    lags: (usize, usize),
    min_papr: f64,
    max_disagreement: f64,
) -> Option<SweepTest> {
    let a = chirp_rate(x, sample_rate_hz, lags.0)?;
    let b = chirp_rate(x, sample_rate_hz, lags.1)?;
    let papr = a.papr.min(b.papr);
    if papr < min_papr {
        return None;
    }
    // A rate finer than either lag can resolve is not a sweep that was measured; it is a bin.
    let floor = a.resolution_hz_per_s.min(b.resolution_hz_per_s);
    if a.rate_hz_per_s.abs() < floor {
        return None;
    }
    let disagreement = (a.rate_hz_per_s - b.rate_hz_per_s).abs() / a.rate_hz_per_s.abs();
    if disagreement > max_disagreement {
        return None;
    }
    Some(SweepTest {
        rate_hz_per_s: 0.5 * (a.rate_hz_per_s + b.rate_hz_per_s),
        papr,
        disagreement,
    })
}

#[cfg(test)]
mod tests {
    use std::f64::consts::TAU;

    use super::*;
    use crate::synth::Rng;

    /// The SF9/125 kHz geometry after channelisation: one symbol, one frame, the whole channel.
    /// `fs` is the channel width, so noise white over `fs` is noise in the channel and no
    /// filtering is needed anywhere in these tests.
    const FS: f64 = 125e3;
    const N: usize = 512;
    const FRAME_S: f64 = N as f64 / FS; // 4.096 ms, as in the pipeline at 500 kS/s
    const ALPHA: f64 = FS / FRAME_S; // 3.0518e7 Hz/s = BW²/2^SF

    fn lags() -> (usize, usize) {
        default_lags(N)
    }

    fn from_frequency(freq: impl Fn(usize) -> f64, phase0: f64) -> Vec<Complex32> {
        let mut phase = phase0;
        (0..N)
            .map(|i| {
                let v = Complex32::new(phase.cos() as f32, phase.sin() as f32);
                phase += TAU * freq(i) / FS;
                v
            })
            .collect()
    }

    /// An unfolded linear sweep through the channel.
    fn sweep(rate: f64, phase0: f64) -> Vec<Complex32> {
        from_frequency(|i| -FS / 2.0 + rate * i as f64 / FS, phase0)
    }

    /// A LoRa-shaped chirp: the same slope, cyclically folded once per symbol from a random start.
    fn folded_chirp(rng: &mut Rng, offset: f64) -> Vec<Complex32> {
        from_frequency(
            |i| {
                let u = (offset + (i as f64 / FS) * ALPHA / FS).rem_euclid(1.0);
                (u - 0.5) * FS
            },
            rng.unit() * TAU,
        )
    }

    /// A genuinely wideband burst: Gaussian, filling the same band, same duration and power.
    fn wideband(rng: &mut Rng) -> Vec<Complex32> {
        let x = crate::synth::complex_noise(rng, N, 1.0);
        let p = x.iter().map(|v| f64::from(v.norm_sqr())).sum::<f64>() / N as f64;
        let g = (1.0 / p.max(1e-30)).sqrt() as f32;
        x.iter().map(|v| v * g).collect()
    }

    /// A stable-frequency control somewhere in the band.
    fn carrier(rng: &mut Rng) -> Vec<Complex32> {
        let f0 = (rng.unit() - 0.5) * 0.8 * FS;
        from_frequency(|_| f0, rng.unit() * TAU)
    }

    /// Modulated but not swept: 2-FSK at the scene's deviation, scaled to this channel.
    fn fsk(rng: &mut Rng) -> Vec<Complex32> {
        let (rate_bd, dev) = (4800.0, 9600.0);
        let sps = (FS / rate_bd).round().max(1.0) as usize;
        let bits: Vec<f64> = (0..N / sps + 2)
            .map(|_| if rng.unit() < 0.5 { -dev } else { dev })
            .collect();
        from_frequency(|i| bits[(i / sps).min(bits.len() - 1)], rng.unit() * TAU)
    }

    /// Adds AWGN for `snr_db` measured over the channel (which is the whole band here).
    fn noisy(x: &[Complex32], snr_db: f64, rng: &mut Rng) -> Vec<Complex32> {
        let p = x.iter().map(|v| f64::from(v.norm_sqr())).sum::<f64>() / x.len() as f64;
        let n = crate::synth::complex_noise(rng, x.len(), p / 10f64.powf(snr_db / 10.0));
        x.iter().zip(&n).map(|(a, b)| a + b).collect()
    }

    fn called_a_sweep(x: &[Complex32]) -> Option<SweepTest> {
        linear_sweep(x, FS, lags(), SWEEP_MIN_PAPR, SWEEP_MAX_DISAGREEMENT)
    }

    /// The identity the whole module rests on: the lag-product tone sits at `ατ`.
    #[test]
    fn the_rate_of_an_unfolded_sweep_is_exact() {
        for mult in [-1.0, -0.5, 0.5, 1.0] {
            let rate = mult * ALPHA;
            let est = chirp_rate(&sweep(rate, 0.0), FS, lags().1).expect("an estimate");
            let err = (est.rate_hz_per_s - rate).abs() / rate.abs();
            assert!(
                err < 0.05,
                "α = {rate:.4e} Hz/s estimated as {:.4e} ({:.1} % out, resolution {:.2e})",
                est.rate_hz_per_s,
                100.0 * err,
                est.resolution_hz_per_s
            );
        }
    }

    /// The real case: a chirp that folds inside the frame, which is what a LoRa symbol does and
    /// what leaves a carrier-tracking estimator with nothing to lock to.
    #[test]
    fn the_rate_survives_the_fold() {
        let mut rng = Rng::new(294);
        for _ in 0..16 {
            let offset = rng.unit();
            let x = folded_chirp(&mut rng, offset);
            let got = called_a_sweep(&x).expect("a folded chirp is still a linear sweep");
            let err = (got.rate_hz_per_s - ALPHA).abs() / ALPHA;
            assert!(
                err < 0.10,
                "folded α estimated as {:.4e} against {ALPHA:.4e} ({:.1} % out)",
                got.rate_hz_per_s,
                100.0 * err
            );
        }
    }

    /// **The measurement ADR-0017 §1.3 cites.** Inside ONE 4.096 ms frame, at the SNR T-255's
    /// scene uses, a sweep is separable from the three things it is otherwise confused with.
    #[test]
    fn a_sweep_inside_one_frame_is_separable_from_a_wideband_burst() {
        let trials = 60;
        let mut rng = Rng::new(2940);
        let mut hits = [0usize; 4];
        for _ in 0..trials {
            let offset = rng.unit();
            let species: [Vec<Complex32>; 4] = [
                folded_chirp(&mut rng, offset),
                wideband(&mut rng),
                carrier(&mut rng),
                fsk(&mut rng),
            ];
            for (h, x) in hits.iter_mut().zip(&species) {
                if called_a_sweep(&noisy(x, 12.0, &mut rng)).is_some() {
                    *h += 1;
                }
            }
        }
        let [chirp, wide, cw, fsk_hits] = hits;
        eprintln!(
            "[T-294] one {:.3} ms frame at 12 dB: chirp {chirp}/{trials}, wideband burst \
             {wide}/{trials}, carrier {cw}/{trials}, 2-FSK {fsk_hits}/{trials}",
            FRAME_S * 1e3
        );
        assert!(
            chirp >= trials * 9 / 10,
            "a chirp inside one frame was called a sweep only {chirp}/{trials} times"
        );
        for (name, n) in [
            ("wideband burst", wide),
            ("carrier", cw),
            ("2-FSK", fsk_hits),
        ] {
            assert!(
                n <= trials / 10,
                "a {name} was called a sweep {n}/{trials} times"
            );
        }
    }

    /// The limit is an SNR, and it is recorded as a test so it is not quietly overstated: by
    /// −9 dB in the channel this estimator has nothing left either.
    #[test]
    fn the_separation_has_an_snr_floor() {
        let trials = 60;
        let mut rng = Rng::new(29400);
        let mut hits = 0usize;
        for _ in 0..trials {
            let offset = rng.unit();
            let x = folded_chirp(&mut rng, offset);
            if called_a_sweep(&noisy(&x, -9.0, &mut rng)).is_some() {
                hits += 1;
            }
        }
        eprintln!("[T-294] one frame at −9 dB: chirp {hits}/{trials}");
        assert!(
            hits <= trials / 2,
            "at −9 dB the estimator still found {hits}/{trials}: the floor is overstated"
        );
    }
}
