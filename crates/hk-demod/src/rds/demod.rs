//! RDS physical layer: MPX → differentially decoded bits (EN 50067 §1).
//!
//! 1. **Subcarrier.** The 57 kHz subcarrier is phase-locked to the third harmonic of the stereo
//!    pilot, so the MPX is mixed with `e^{−j·3θ}` where θ is the pilot PLL phase: the receiver's
//!    clock error cancels. The standard allows the subcarrier in phase *or quadrature* with the
//!    harmonic, so the residual constant phase ψ is estimated from the squared baseband
//!    (BPSK: `z²` removes the data) with a slow average and unwrapped on the ±π/2 branch nearest
//!    the previous estimate. The π ambiguity left is harmless: the data is differential.
//! 2. **Channel filter.** FIR ÷10 (240 → 24 kS/s) then a 2.4 kHz low-pass that rejects the top
//!    of the stereo L−R band (53 kHz, 4 kHz below the subcarrier).
//! 3. **Biphase matched filter and timing.** Each symbol is `+a, −a` over two half-symbols; the
//!    matched statistic is `∫first half − ∫second half` (piecewise integration of the samples,
//!    fractional positions by linear interpolation of the running sum). The symbol clock
//!    (1187.5 Bd = pilot/16) is recovered non-data-aided: `timing_candidates` absolute symbol
//!    phases are scored by the mean |statistic| over each `timing_window_symbols` window,
//!    smoothed across windows; the peak (parabolically interpolated) is the timing. A
//!    half-symbol error scores 1/2 of the true phase on random data, so the peak is unambiguous.
//!    Symbols are emitted on the current phase, never closer than half a symbol to the previous
//!    one, so a phase update neither repeats nor drops symbols.
//! 4. **Differential decoding:** `b[k] = d[k] ⊕ d[k−1]` with `d = statistic > 0`.

use std::f64::consts::PI;

use num_complex::{Complex32, Complex64};
use serde::{Deserialize, Serialize};

use super::group::RDS_BITRATE_BD;
use crate::dsp::{FirDecimator, lowpass_taps};

/// RDS demodulator settings.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct RdsDemodConfig {
    /// First decimation factor.
    pub decimation: usize,
    /// First filter passband / stopband, Hz.
    pub stage1_passband_hz: f64,
    /// See `stage1_passband_hz`.
    pub stage1_stopband_hz: f64,
    /// Channel filter passband / stopband, Hz (at the decimated rate).
    pub channel_passband_hz: f64,
    /// See `channel_passband_hz`.
    pub channel_stopband_hz: f64,
    /// Filter stopband attenuation, dB.
    pub stopband_db: f64,
    /// Time constant of the residual subcarrier-phase average, s.
    pub phase_time_constant_s: f64,
    /// Symbol phases scored.
    pub timing_candidates: usize,
    /// Symbols per timing window.
    pub timing_window_symbols: usize,
    /// Weight of the newest window in the smoothed timing metric.
    pub timing_smoothing: f64,
}

impl Default for RdsDemodConfig {
    fn default() -> Self {
        Self {
            decimation: 10,
            stage1_passband_hz: 3_000.0,
            stage1_stopband_hz: 20_000.0,
            channel_passband_hz: 2_400.0,
            channel_stopband_hz: 3_800.0,
            stopband_db: 50.0,
            phase_time_constant_s: 0.25,
            timing_candidates: 16,
            timing_window_symbols: 16,
            timing_smoothing: 0.2,
        }
    }
}

/// One differentially decoded bit.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RdsBit {
    /// 0 or 1.
    pub bit: u8,
    /// |matched statistic| of the later symbol, normalised to the symbol length.
    pub soft: f32,
    /// Input (MPX) sample index of the later symbol's start.
    pub position: f64,
}

/// Streaming RDS demodulator: MPX samples plus pilot phase in, bits out.
#[derive(Clone, Debug)]
pub struct RdsDemod {
    config: RdsDemodConfig,
    sps: f64,
    stage1: FirDecimator<Complex32>,
    channel: FirDecimator<Complex32>,
    delay1: f64,
    delay2: f64,
    n_z: u64,
    z2: Complex64,
    psi: f64,
    alpha: f64,
    csum: Vec<f64>,
    csum_base: u64,
    metrics: Vec<f64>,
    have_metrics: bool,
    win_start: f64,
    last_start: f64,
    tau: f64,
    d_prev: Option<bool>,
    bits: Vec<RdsBit>,
    contrast: f64,
}

impl RdsDemod {
    /// A demodulator for MPX at `mpx_rate_hz`.
    pub fn new(config: RdsDemodConfig, mpx_rate_hz: f64) -> Result<Self, hk_dsp::DesignError> {
        let d = config.decimation.max(1);
        let fsz = mpx_rate_hz / d as f64;
        let taps1 = lowpass_taps(
            mpx_rate_hz,
            config.stage1_passband_hz,
            config.stage1_stopband_hz.min(mpx_rate_hz / 2.0),
            config.stopband_db,
        )?;
        let taps2 = lowpass_taps(
            fsz,
            config.channel_passband_hz,
            config.channel_stopband_hz.min(fsz / 2.0),
            config.stopband_db,
        )?;
        let stage1 = FirDecimator::new(taps1, d);
        let channel = FirDecimator::new(taps2, 1);
        Ok(Self {
            sps: fsz / RDS_BITRATE_BD,
            delay1: stage1.group_delay(),
            delay2: channel.group_delay(),
            stage1,
            channel,
            n_z: 0,
            z2: Complex64::new(0.0, 0.0),
            psi: 0.0,
            alpha: 1.0 / (config.phase_time_constant_s * fsz).max(1.0),
            csum: Vec::with_capacity(1 << 14),
            csum_base: 0,
            metrics: vec![0.0; config.timing_candidates.max(4)],
            have_metrics: false,
            win_start: 0.0,
            last_start: f64::NEG_INFINITY,
            tau: 0.0,
            d_prev: None,
            bits: Vec::with_capacity(1 << 12),
            contrast: 0.0,
            config,
        })
    }

    /// Samples per symbol at the decimated rate.
    pub fn samples_per_symbol(&self) -> f64 {
        self.sps
    }

    /// Current symbol phase, decimated samples in `[0, sps)`.
    pub fn timing_phase(&self) -> f64 {
        self.tau
    }

    /// Smoothed timing metric peak / mean (≈ 1 on noise; ≈ 1.3+ with a biphase signal).
    pub fn timing_contrast(&self) -> f64 {
        self.contrast
    }

    /// Pushes one MPX sample with the pilot PLL phase θ for that sample.
    #[inline]
    pub fn push(&mut self, mpx: f32, pilot_phase: f64) {
        let ph = -3.0 * pilot_phase;
        let z = Complex32::new(mpx * ph.cos() as f32, mpx * ph.sin() as f32);
        if let Some(a) = self.stage1.push(z)
            && let Some(b) = self.channel.push(a)
        {
            self.on_sample(b);
        }
    }

    /// Bits decoded since the last call.
    pub fn take_bits(&mut self) -> Vec<RdsBit> {
        std::mem::take(&mut self.bits)
    }

    fn input_index_of(&self, pos_z: f64) -> f64 {
        let d = self.config.decimation.max(1) as f64;
        (pos_z - self.delay2 + 1.0) * d - 1.0 - self.delay1
    }

    fn on_sample(&mut self, b: Complex32) {
        let b = Complex64::new(f64::from(b.re), f64::from(b.im));
        let a = self.alpha.max(1.0 / (self.n_z + 1) as f64);
        self.z2 += (b * b - self.z2) * a;
        let est = 0.5 * self.z2.im.atan2(self.z2.re);
        let mut d = est - self.psi;
        d -= PI * (d / PI).round();
        self.psi += d;
        let r = b.re * self.psi.cos() + b.im * self.psi.sin();
        if self.csum.is_empty() {
            self.csum_base = self.n_z;
            self.csum.push(0.0);
        }
        let last = *self.csum.last().expect("non-empty");
        self.csum.push(last + r);
        self.n_z += 1;
        self.run_timing();
    }

    fn prefix(&self, x: f64) -> f64 {
        let rel = x - self.csum_base as f64;
        let max_i = self.csum.len().saturating_sub(2);
        let i = (rel.floor().max(0.0) as usize).min(max_i);
        let f = (rel - i as f64).clamp(0.0, 1.0);
        self.csum[i] + f * (self.csum[i + 1] - self.csum[i])
    }

    fn statistic(&self, s: f64) -> f64 {
        let h = self.sps / 2.0;
        let a = self.prefix(s);
        let m = self.prefix(s + h);
        let e = self.prefix(s + self.sps);
        (m - a) - (e - m)
    }

    fn run_timing(&mut self) {
        let sps = self.sps;
        let m = self.metrics.len();
        loop {
            let buffer_end = (self.csum_base + self.csum.len() as u64 - 1) as f64;
            let we = self.win_start + self.config.timing_window_symbols as f64 * sps;
            if buffer_end < we + sps + 2.0 {
                return;
            }
            let base = self.csum_base as f64;
            let beta = if self.have_metrics {
                self.config.timing_smoothing
            } else {
                1.0
            };
            for k in 0..m {
                let tau_k = k as f64 * sps / m as f64;
                let j0 = ((self.win_start - tau_k) / sps).ceil();
                let mut s = tau_k + j0 * sps;
                let (mut sum, mut cnt) = (0.0, 0u32);
                while s < we {
                    if s >= base {
                        sum += self.statistic(s).abs();
                        cnt += 1;
                    }
                    s += sps;
                }
                if cnt > 0 {
                    let v = sum / f64::from(cnt);
                    self.metrics[k] += beta * (v - self.metrics[k]);
                }
            }
            self.have_metrics = true;
            let (kmax, &mmax) = self
                .metrics
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.total_cmp(b.1))
                .expect("candidates");
            let mean = self.metrics.iter().sum::<f64>() / m as f64;
            self.contrast = if mean > 0.0 { mmax / mean } else { 0.0 };
            let lo = self.metrics[(kmax + m - 1) % m];
            let hi = self.metrics[(kmax + 1) % m];
            let den = lo - 2.0 * mmax + hi;
            let delta = if den < 0.0 {
                (0.5 * (lo - hi) / den).clamp(-0.5, 0.5)
            } else {
                0.0
            };
            self.tau = ((kmax as f64 + delta) * sps / m as f64).rem_euclid(sps);

            // Emit symbols on the current phase.
            let from = if self.last_start.is_finite() {
                self.last_start + sps / 2.0
            } else {
                self.win_start.max(base)
            };
            let mut s = self.tau + ((from - self.tau) / sps).ceil() * sps;
            while s < we {
                let stat = self.statistic(s);
                let d = stat > 0.0;
                if let Some(prev) = self.d_prev {
                    self.bits.push(RdsBit {
                        bit: u8::from(d ^ prev),
                        soft: (stat.abs() / sps) as f32,
                        position: self.input_index_of(s),
                    });
                }
                self.d_prev = Some(d);
                self.last_start = s;
                s += sps;
            }
            self.win_start = we;

            // Drop integrated samples no longer needed.
            let keep = self.last_start.min(self.win_start) - 2.0 * sps;
            if keep > base + 16_384.0 {
                let drop = (keep - base).floor() as usize;
                self.csum.drain(..drop);
                self.csum_base += drop as u64;
            }
        }
    }
}
