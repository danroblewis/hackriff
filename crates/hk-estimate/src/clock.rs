//! Clock helpers (feed C05 calibration): precise tone frequency and the ppm of a known line.
//!
//! **Convention.** `ppm = (measured / nominal − 1)·10⁶`: the fractional error with which this
//! receiver *reads* frequencies. HackRF derives the LO and the sample clock from one crystal, so
//! a line read 6.8 ppm low (the S5 FM pilot at 18 999.871 Hz) means every absolute frequency
//! reads 6.8 ppm low, and the true frequency is `measured / (1 + ppm·10⁻⁶)`
//! ([`correct_frequency_hz`]).

use std::f64::consts::TAU;

use num_complex::{Complex, Complex32};

use crate::dsp::{LineSearch, Workspace, power_line, unwrap};
use crate::estimate::{Estimate, Evidence, Method, Reason};

/// Clock error of a measured line against its nominal frequency, ppm.
pub fn ppm_from_line(measured_hz: &Estimate, nominal_hz: f64) -> Estimate {
    if !(nominal_hz.is_finite() && nominal_hz != 0.0) {
        return Estimate::abstain(Method::ClockPpm, Reason::InvalidInput);
    }
    match *measured_hz {
        Estimate::Measured {
            value,
            sigma,
            evidence,
            ..
        } => Estimate::measured(
            (value / nominal_hz - 1.0) * 1e6,
            sigma / nominal_hz.abs() * 1e6,
            Method::ClockPpm,
        )
        .with_evidence(evidence),
        Estimate::Abstained { evidence, .. } => {
            Estimate::abstain(Method::ClockPpm, Reason::Upstream).with_evidence(evidence)
        }
    }
}

/// True frequency of a frequency read with clock error `ppm` (see the [module docs](self)).
pub fn correct_frequency_hz(measured_hz: f64, ppm: f64) -> f64 {
    measured_hz / (1.0 + ppm * 1e-6)
}

/// Frequency of the strongest tone of a real signal (e.g. an FM discriminator's MPX) within
/// `[f_lo, f_hi]` Hz. See [`tone_frequency_iq`].
pub fn tone_frequency(x: &[f32], fs: f64, f_lo: f64, f_hi: f64) -> Estimate {
    tone_impl(x, fs, f_lo, f_hi, |v| Complex::new(f64::from(v), 0.0))
}

/// Frequency of the strongest tone of complex IQ within `[f_lo, f_hi]` Hz.
///
/// The signal is mixed to the search centre and boxcar-decimated to ≥ 8× the search span; a
/// zero-padded periodogram line (significance-tested) gives the coarse frequency; the slope of
/// the unwrapped phase of 32 blocks (weighted least squares) refines it. `sigma` is the slope's
/// standard error from the phase residuals.
pub fn tone_frequency_iq(x: &[Complex32], fs: f64, f_lo: f64, f_hi: f64) -> Estimate {
    tone_impl(x, fs, f_lo, f_hi, |v| {
        Complex::new(f64::from(v.re), f64::from(v.im))
    })
}

fn tone_impl<S: Copy>(
    x: &[S],
    fs: f64,
    f_lo: f64,
    f_hi: f64,
    to_c: impl Fn(S) -> Complex<f64>,
) -> Estimate {
    let method = Method::ToneFrequency;
    if !(fs.is_finite() && fs > 0.0 && f_lo.is_finite() && f_hi > f_lo && f_hi - f_lo < fs) {
        return Estimate::abstain(method, Reason::InvalidInput);
    }
    let span = f_hi - f_lo;
    let f_mid = 0.5 * (f_lo + f_hi);
    let d = ((fs / (8.0 * span)).floor() as usize).max(1);
    let fd = fs / d as f64;
    let m = x.len() / d;
    if m < 64 {
        return Estimate::abstain(method, Reason::TooShort);
    }
    // Mix to the search centre, boxcar-decimate by d.
    let step = -f_mid / fs;
    let mut z = Vec::with_capacity(m);
    let mut acc = Complex::new(0.0, 0.0);
    for (n, &v) in x[..m * d].iter().enumerate() {
        let ph = (step * n as f64).fract() * TAU;
        acc += to_c(v) * Complex::new(ph.cos(), ph.sin());
        if (n + 1) % d == 0 {
            let s = acc / d as f64;
            z.push(Complex32::new(s.re as f32, s.im as f32));
            acc = Complex::new(0.0, 0.0);
        }
    }
    let mut ws = Workspace::default();
    let Some(hit) = power_line(
        &mut ws,
        &z,
        fd,
        LineSearch {
            power: 1,
            f_lo: -span / 2.0,
            f_hi: span / 2.0,
            min_significance_db: 12.0,
            pfa: 1e-3,
            max_samples: usize::MAX,
        },
    ) else {
        return Estimate::abstain(method, Reason::TooShort);
    };
    let evidence = Evidence {
        significance_db: Some(hit.significance_db),
        threshold_db: Some(hit.threshold_db),
        coherence: Some(hit.coherence),
        second_line_ratio: Some(hit.second_ratio),
        samples: Some(x.len() as u64),
        ..Default::default()
    };
    if !hit.significant() {
        return Estimate::abstain(method, Reason::NoLine).with_evidence(evidence);
    }
    let f1 = hit.freq_hz;
    // Phase slope over blocks of the decimated signal with the coarse tone removed.
    let blocks = 32.min(m / 4).max(4);
    let l = m / blocks;
    let mut phase = Vec::with_capacity(blocks);
    let mut weight = Vec::with_capacity(blocks);
    let mut times = Vec::with_capacity(blocks);
    for j in 0..blocks {
        let mut s = Complex::new(0.0, 0.0);
        for (i, v) in z.iter().enumerate().skip(j * l).take(l) {
            let ph = -TAU * f1 * i as f64 / fd;
            s += Complex::new(f64::from(v.re), f64::from(v.im)) * Complex::new(ph.cos(), ph.sin());
        }
        phase.push(s.arg());
        weight.push(s.norm());
        times.push((j * l) as f64 / fd + (l as f64 - 1.0) / (2.0 * fd));
    }
    unwrap(&mut phase);
    let w_sum: f64 = weight.iter().sum();
    let coarse = Estimate::measured(f_mid + f1, hit.sigma_hz, method).with_evidence(evidence);
    if w_sum <= 0.0 {
        return coarse;
    }
    let t_mean = times.iter().zip(&weight).map(|(t, w)| t * w).sum::<f64>() / w_sum;
    let p_mean = phase.iter().zip(&weight).map(|(p, w)| p * w).sum::<f64>() / w_sum;
    let mut sxy = 0.0;
    let mut sxx = 0.0;
    for ((t, p), w) in times.iter().zip(&phase).zip(&weight) {
        sxy += w * (t - t_mean) * (p - p_mean);
        sxx += w * (t - t_mean) * (t - t_mean);
    }
    if sxx <= 0.0 {
        return coarse;
    }
    let slope = sxy / sxx;
    let df = slope / TAU;
    if df.abs() > fd / (4.0 * l as f64) {
        return coarse; // phase slope ambiguous across blocks
    }
    let mut rss = 0.0;
    for ((t, p), w) in times.iter().zip(&phase).zip(&weight) {
        let r = p - (p_mean + slope * (t - t_mean));
        rss += w * r * r;
    }
    let dof = (blocks as f64 - 2.0).max(1.0);
    let var_res = rss / w_sum * blocks as f64 / dof;
    let sigma_slope = (var_res / (sxx / w_sum * blocks as f64)).sqrt();
    Estimate::measured(f_mid + f1 + df, sigma_slope / TAU, method).with_evidence(evidence)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pilot_ppm_from_a_real_tone() {
        let fs = 250_000.0;
        let f = 18_999.871;
        let x: Vec<f32> = (0..500_000)
            .map(|n| {
                (0.05 * (TAU * f * n as f64 / fs).sin()
                    + 0.3 * (TAU * 1000.0 * n as f64 / fs).sin()) as f32
            })
            .collect();
        let est = tone_frequency(&x, fs, 18_900.0, 19_100.0);
        let v = est.value().unwrap();
        assert!((v - f).abs() < 1e-3, "{est:?}");
        let ppm = ppm_from_line(&est, 19_000.0);
        assert!((ppm.value().unwrap() + 6.789).abs() < 0.05, "{ppm:?}");
        assert!((correct_frequency_hz(v, ppm.value().unwrap()) - 19_000.0).abs() < 1e-6);
    }

    #[test]
    fn noise_has_no_tone() {
        let mut rng = hk_dsp::synth::Rng::new(3);
        let x = hk_dsp::synth::complex_noise(&mut rng, 200_000, 1.0);
        let est = tone_frequency_iq(&x, 100_000.0, 1000.0, 1200.0);
        assert_eq!(est.reason(), Some(Reason::NoLine), "{est:?}");
    }
}
