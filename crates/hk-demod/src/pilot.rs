//! 19 kHz stereo-pilot PLL on the FM MPX.
//!
//! The MPX is mixed with the NCO (`x·e^{−jθ}`) and averaged over blocks of `D` samples
//! (`update_rate_hz`); the block average's angle is the phase error `e` and `2|avg|` the pilot
//! deviation. A second-order loop (natural frequency `loop_bandwidth_hz`, damping ζ) steers the
//! NCO; the first update aligns the phase directly. Near-pilot interference: mono audio stops at
//! 15 kHz and the L−R band starts at 23 kHz, both 4 kHz away, which the block average places near
//! a null (D chosen so `fs/D ≈ 4 kHz`) and the narrow loop removes.
//!
//! **Lock:** an exponential average of `cos e` (≈ 1 locked, ≈ 0 on noise) with hysteresis, and
//! the averaged deviation above `min_deviation_hz`. **Frequency:** the NCO's total phase
//! advance over the locked samples divided by their count, which is far more precise than the
//! loop's instantaneous frequency. Its `sigma_hz` is a first-order figure from the phase-error
//! RMS.
//!
//! When locked the pilot is `a·cos θ`; the 38 kHz and 57 kHz carriers are `2θ` and `3θ` up to a
//! fixed phase.

use std::f64::consts::TAU;

use num_complex::Complex64;
use serde::{Deserialize, Serialize};

/// Pilot PLL settings.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct PilotConfig {
    /// Nominal pilot, Hz.
    pub nominal_hz: f64,
    /// Largest frequency the loop may pull to, ± Hz from nominal.
    pub pull_range_hz: f64,
    /// Phase-detector update rate, Hz.
    pub update_rate_hz: f64,
    /// Loop natural frequency, Hz.
    pub loop_bandwidth_hz: f64,
    /// Loop damping.
    pub damping: f64,
    /// Lock when the averaged `cos e` exceeds this.
    pub lock_on: f64,
    /// Unlock when it falls below this.
    pub lock_off: f64,
    /// Lock-detector averaging time, s.
    pub lock_time_constant_s: f64,
    /// Smallest pilot deviation accepted as a pilot, Hz (nominal 6.75 kHz).
    pub min_deviation_hz: f64,
}

impl Default for PilotConfig {
    fn default() -> Self {
        Self {
            nominal_hz: 19_000.0,
            pull_range_hz: 100.0,
            update_rate_hz: 4_000.0,
            loop_bandwidth_hz: 10.0,
            damping: 0.707,
            lock_on: 0.8,
            lock_off: 0.5,
            lock_time_constant_s: 0.05,
            min_deviation_hz: 500.0,
        }
    }
}

/// What the pilot PLL saw.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct PilotReport {
    /// Locked for at least half the samples after the first `2·lock_time_constant_s`.
    pub present: bool,
    /// Locked at the end of the input.
    pub locked: bool,
    /// Fraction of samples processed while locked.
    pub locked_fraction: f64,
    /// Mean pilot frequency over the locked samples, Hz (receiver clock).
    pub frequency_hz: Option<f64>,
    /// First-order one-sigma uncertainty of `frequency_hz`, Hz.
    pub sigma_hz: Option<f64>,
    /// Mean pilot deviation while locked, Hz.
    pub deviation_hz: Option<f64>,
    /// Mean `cos e` while locked (0–1): the Demodulation lock quality.
    pub lock_quality: Option<f64>,
    /// RMS phase error while locked, rad.
    pub phase_error_rms_rad: Option<f64>,
    /// Time of first lock from the start of the input, s.
    pub first_lock_s: Option<f64>,
}

/// Streaming pilot PLL.
#[derive(Clone, Debug)]
pub struct PilotPll {
    config: PilotConfig,
    fs: f64,
    d: usize,
    kp: f64,
    ki: f64,
    w0: f64,
    w_max: f64,
    theta: f64,
    w: f64,
    acc: Complex64,
    count: usize,
    first: bool,
    q: f64,
    amp: f64,
    a_lock: f64,
    locked: bool,
    ever_locked: bool,
    n: u64,
    seg_start: Option<(u64, f64)>,
    seg_dn: u64,
    seg_dtheta: f64,
    e_sq: f64,
    q_sum: f64,
    amp_sum: f64,
    upd_locked: u64,
    first_lock: Option<u64>,
}

impl PilotPll {
    /// A PLL for MPX at `sample_rate_hz`.
    pub fn new(config: PilotConfig, sample_rate_hz: f64) -> Self {
        let d = (sample_rate_hz / config.update_rate_hz).round().max(1.0) as usize;
        let t = d as f64 / sample_rate_hz;
        let wn = TAU * config.loop_bandwidth_hz;
        let w0 = TAU * config.nominal_hz / sample_rate_hz;
        Self {
            fs: sample_rate_hz,
            d,
            kp: 2.0 * config.damping * wn * t,
            ki: (wn * t).powi(2),
            w0,
            w_max: TAU * config.pull_range_hz / sample_rate_hz,
            theta: 0.0,
            w: w0,
            acc: Complex64::new(0.0, 0.0),
            count: 0,
            first: true,
            q: 0.0,
            amp: 0.0,
            a_lock: (d as f64 / (sample_rate_hz * config.lock_time_constant_s)).min(1.0),
            locked: false,
            ever_locked: false,
            n: 0,
            seg_start: None,
            seg_dn: 0,
            seg_dtheta: 0.0,
            e_sq: 0.0,
            q_sum: 0.0,
            amp_sum: 0.0,
            upd_locked: 0,
            first_lock: None,
            config,
        }
    }

    /// Currently locked.
    pub fn is_locked(&self) -> bool {
        self.locked
    }

    /// Locked at some point.
    pub fn ever_locked(&self) -> bool {
        self.ever_locked
    }

    /// Pushes one MPX sample; returns the NCO phase θ for that sample.
    #[inline]
    pub fn step(&mut self, x: f32) -> f64 {
        let th = self.theta;
        let x = f64::from(x);
        self.acc += Complex64::new(x * th.cos(), -x * th.sin());
        self.theta += self.w;
        self.n += 1;
        self.count += 1;
        if self.count == self.d {
            self.update();
        }
        th
    }

    fn update(&mut self) {
        let e = self.acc.im.atan2(self.acc.re);
        let amp = 2.0 * self.acc.norm() / self.d as f64;
        self.acc = Complex64::new(0.0, 0.0);
        self.count = 0;
        self.q += self.a_lock * (e.cos() - self.q);
        self.amp += self.a_lock * (amp - self.amp);
        if self.first {
            self.theta += e;
            self.first = false;
        } else {
            self.w = (self.w + self.ki * e / self.d as f64)
                .clamp(self.w0 - self.w_max, self.w0 + self.w_max);
            self.theta += self.kp * e;
        }
        let dev_ok = self.amp >= self.config.min_deviation_hz;
        if !self.locked && self.q > self.config.lock_on && dev_ok {
            self.locked = true;
            self.ever_locked = true;
            self.first_lock.get_or_insert(self.n);
            self.seg_start = Some((self.n, self.theta));
        } else if self.locked && (self.q < self.config.lock_off || !dev_ok) {
            self.locked = false;
            self.close_segment();
        }
        if self.locked {
            self.e_sq += e * e;
            self.q_sum += e.cos();
            self.amp_sum += amp;
            self.upd_locked += 1;
        }
    }

    fn close_segment(&mut self) {
        if let Some((n0, th0)) = self.seg_start.take() {
            self.seg_dn += self.n - n0;
            self.seg_dtheta += self.theta - th0;
        }
    }

    /// Results so far.
    pub fn report(&self) -> PilotReport {
        let (mut dn, mut dth) = (self.seg_dn, self.seg_dtheta);
        if let Some((n0, th0)) = self.seg_start {
            dn += self.n - n0;
            dth += self.theta - th0;
        }
        let settle = (2.0 * self.config.lock_time_constant_s * self.fs) as u64;
        let eligible = self.n.saturating_sub(settle);
        let locked_fraction = if self.n > 0 {
            dn as f64 / self.n as f64
        } else {
            0.0
        };
        let u = self.upd_locked as f64;
        let rms = (self.upd_locked > 0).then(|| (self.e_sq / u).sqrt());
        let frequency_hz = (dn > 0).then(|| dth / dn as f64 * self.fs / TAU);
        let sigma_hz = match (rms, dn > 0 && self.upd_locked > 2) {
            (Some(r), true) => {
                let span_s = dn as f64 / self.fs;
                Some(r * (12.0 / u).sqrt() / (TAU * span_s))
            }
            _ => None,
        };
        PilotReport {
            present: eligible > 0 && dn as f64 >= 0.5 * eligible as f64,
            locked: self.locked,
            locked_fraction,
            frequency_hz,
            sigma_hz,
            deviation_hz: (self.upd_locked > 0).then(|| self.amp_sum / u),
            lock_quality: (self.upd_locked > 0).then(|| self.q_sum / u),
            phase_error_rms_rad: rms,
            first_lock_s: self.first_lock.map(|n| n as f64 / self.fs),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hk_dsp::synth::Rng;

    #[test]
    fn locks_to_offset_pilot_and_measures_frequency() {
        let fs = 240_000.0;
        let f = 18_999.871;
        let mut pll = PilotPll::new(PilotConfig::default(), fs);
        let mut rng = Rng::new(5);
        for n in 0..(fs as usize) {
            let t = n as f64 / fs;
            let x = 6750.0 * (TAU * f * t + 1.0).cos()
                + 20_000.0 * (TAU * 1000.0 * t).sin()
                + 2000.0 * rng.gaussian_pair().0;
            pll.step(x as f32);
        }
        let r = pll.report();
        assert!(r.present && r.locked, "{r:?}");
        let fe = r.frequency_hz.unwrap();
        assert!((fe - f).abs() < 0.05, "{fe}");
        assert!((r.deviation_hz.unwrap() - 6750.0).abs() < 200.0, "{r:?}");
    }

    #[test]
    fn does_not_lock_on_noise_or_mono_audio() {
        let fs = 240_000.0;
        let mut pll = PilotPll::new(PilotConfig::default(), fs);
        let mut rng = Rng::new(9);
        for n in 0..(fs as usize) {
            let t = n as f64 / fs;
            let x = 30_000.0 * (TAU * 400.0 * t).sin() + 5000.0 * rng.gaussian_pair().0;
            pll.step(x as f32);
        }
        let r = pll.report();
        assert!(!r.present, "{r:?}");
    }
}
