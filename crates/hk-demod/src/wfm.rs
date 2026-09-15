//! Broadcast WFM: complex baseband at the MPX rate → quadrature discriminator → MPX, then
//!
//! - pilot PLL (lock indicator, pilot frequency and deviation; stereo flag),
//! - mono audio: 15 kHz FIR low-pass and ÷5 to 48 kS/s, de-emphasis (75 µs Americas default,
//!   50 µs configurable), scaled so ±75 kHz deviation is ±1,
//! - RDS on the 57 kHz subcarrier, phase-locked to 3 × the pilot.
//!
//! Stereo L−R decoding is not implemented (optional in T-012): the stereo flag reports the pilot.
//! State carries across [`WfmDemod::process`] calls; allocation is per call (output buffers),
//! never per sample.

use num_complex::Complex32;
use serde::{Deserialize, Serialize};

use crate::dsp::{Deemphasis, Discriminator, FirDecimator, lowpass_taps};
use crate::pilot::{PilotConfig, PilotPll, PilotReport};
use crate::rds::{RdsConfig, RdsDecoder, RdsDemod, RdsReport};

/// MPX sample rate the receiver down-converts WFM channels to, Hz (÷5 → 48 kS/s audio,
/// ÷10 → 24 kS/s RDS).
pub const MPX_RATE_HZ: f64 = 240_000.0;

/// De-emphasis time constants.
pub mod deemphasis {
    /// Americas, South Korea, s.
    pub const US_75_US: f64 = 75e-6;
    /// Europe and most other regions, s.
    pub const EU_50_US: f64 = 50e-6;
}

/// WFM demodulator settings.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct WfmConfig {
    /// De-emphasis τ, s (0 disables).
    pub deemphasis_tau_s: f64,
    /// MPX → audio decimation.
    pub audio_decimation: usize,
    /// Audio low-pass passband, Hz.
    pub audio_passband_hz: f64,
    /// Audio low-pass stopband, Hz (below the pilot).
    pub audio_stopband_hz: f64,
    /// Audio low-pass attenuation, dB.
    pub audio_stopband_db: f64,
    /// Deviation mapped to audio full scale, Hz.
    pub full_scale_deviation_hz: f64,
    /// Pilot PLL.
    pub pilot: PilotConfig,
    /// RDS (`None` disables).
    pub rds: Option<RdsConfig>,
}

impl Default for WfmConfig {
    fn default() -> Self {
        Self {
            deemphasis_tau_s: deemphasis::US_75_US,
            audio_decimation: 5,
            audio_passband_hz: 15_000.0,
            audio_stopband_hz: 18_500.0,
            audio_stopband_db: 60.0,
            full_scale_deviation_hz: 75_000.0,
            pilot: PilotConfig::default(),
            rds: Some(RdsConfig::default()),
        }
    }
}

/// What the WFM chain measured and decoded.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WfmReport {
    /// MPX rate, Hz.
    pub mpx_rate_hz: f64,
    /// MPX samples processed.
    pub mpx_samples: u64,
    /// Mean discriminator output: residual carrier offset of the channel, Hz.
    pub carrier_offset_hz: f64,
    /// 99.9th percentile of |deviation| about the carrier, Hz (1 kHz bins).
    pub peak_deviation_hz: Option<f64>,
    /// Pilot PLL.
    pub pilot: PilotReport,
    /// Stereo broadcast (pilot present).
    pub stereo: bool,
    /// De-emphasis τ applied, s.
    pub deemphasis_tau_s: f64,
    /// Audio rate, Hz.
    pub audio_rate_hz: f64,
    /// RDS results (`None` when disabled).
    pub rds: Option<RdsReport>,
    /// RDS timing-metric contrast (≈ 1 without RDS).
    pub rds_timing_contrast: Option<f64>,
}

const DEV_BINS: usize = 256;
const DEV_BIN_HZ: f64 = 1000.0;

/// Streaming WFM demodulator.
#[derive(Clone, Debug)]
pub struct WfmDemod {
    config: WfmConfig,
    fs: f64,
    disc: Discriminator,
    pll: PilotPll,
    audio_fir: FirDecimator<f32>,
    deemph: Deemphasis,
    rds: Option<(RdsDemod, RdsDecoder)>,
    audio: Vec<f32>,
    n: u64,
    mean_sum: f64,
    mean_prev: f64,
    dev_hist: [u64; DEV_BINS],
}

impl WfmDemod {
    /// A demodulator for complex baseband at `mpx_rate_hz` (an integer multiple of the audio
    /// rate is expected; [`MPX_RATE_HZ`] is the receiver's choice).
    pub fn new(config: WfmConfig, mpx_rate_hz: f64) -> Result<Self, hk_dsp::DesignError> {
        let dec = config.audio_decimation.max(1);
        let audio_rate = mpx_rate_hz / dec as f64;
        let taps = lowpass_taps(
            mpx_rate_hz,
            config.audio_passband_hz,
            config.audio_stopband_hz.min(audio_rate / 2.0),
            config.audio_stopband_db,
        )?;
        let rds = match config.rds {
            Some(rc) => Some((
                RdsDemod::new(rc.demod, mpx_rate_hz)?,
                RdsDecoder::new(rc.groups, mpx_rate_hz),
            )),
            None => None,
        };
        Ok(Self {
            fs: mpx_rate_hz,
            disc: Discriminator::new(mpx_rate_hz),
            pll: PilotPll::new(config.pilot, mpx_rate_hz),
            audio_fir: FirDecimator::new(taps, dec),
            deemph: Deemphasis::new(config.deemphasis_tau_s, audio_rate),
            rds,
            audio: Vec::new(),
            n: 0,
            mean_sum: 0.0,
            mean_prev: 0.0,
            dev_hist: [0; DEV_BINS],
            config,
        })
    }

    /// Audio rate, Hz.
    pub fn audio_rate_hz(&self) -> f64 {
        self.fs / self.audio_fir.factor() as f64
    }

    /// MPX index that audio sample 0 represents (the audio filter's delay removed).
    pub fn audio_mpx_offset(&self) -> f64 {
        (self.audio_fir.factor() - 1) as f64 - self.audio_fir.group_delay()
    }

    /// Demodulates contiguous baseband samples.
    pub fn process(&mut self, iq: &[Complex32]) {
        self.audio
            .reserve(iq.len() / self.audio_fir.factor().max(1) + 1);
        let scale = (1.0 / self.config.full_scale_deviation_hz) as f32;
        let mean = self.mean_prev as f32;
        let run_rds = self.rds.is_some();
        for &x in iq {
            let f = self.disc.push(x);
            let theta = self.pll.step(f);
            if run_rds
                && self.pll.ever_locked()
                && let Some((demod, _)) = self.rds.as_mut()
            {
                demod.push(f, theta);
            }
            if let Some(a) = self.audio_fir.push(f) {
                self.audio.push(self.deemph.push((a - mean) * scale));
            }
            let bin = ((f - mean).abs() as f64 / DEV_BIN_HZ) as usize;
            self.dev_hist[bin.min(DEV_BINS - 1)] += 1;
            self.mean_sum += f64::from(f);
        }
        self.n += iq.len() as u64;
        if self.n > 0 {
            self.mean_prev = self.mean_sum / self.n as f64;
        }
        if let Some((demod, dec)) = self.rds.as_mut() {
            for b in demod.take_bits() {
                dec.push_bit(b.bit, b.position);
            }
        }
    }

    /// Audio produced since the last call (48 kS/s mono, f32, ±1 = full-scale deviation).
    pub fn take_audio(&mut self) -> Vec<f32> {
        std::mem::take(&mut self.audio)
    }

    /// RDS groups parsed since the last call (drains the decoder's buffer; empty without RDS).
    /// Positions are MPX sample indexes. The report's totals are unaffected.
    pub fn take_rds_groups(&mut self) -> Vec<crate::rds::RdsGroup> {
        self.rds
            .as_mut()
            .map(|(_, dec)| dec.take_groups())
            .unwrap_or_default()
    }

    /// Results so far.
    pub fn report(&self) -> WfmReport {
        let total: u64 = self.dev_hist.iter().sum();
        let peak = (total > 0).then(|| {
            let target = (total as f64 * 0.999).ceil() as u64;
            let mut acc = 0;
            let mut k = DEV_BINS - 1;
            for (i, c) in self.dev_hist.iter().enumerate() {
                acc += c;
                if acc >= target {
                    k = i;
                    break;
                }
            }
            (k + 1) as f64 * DEV_BIN_HZ
        });
        let pilot = self.pll.report();
        WfmReport {
            mpx_rate_hz: self.fs,
            mpx_samples: self.n,
            carrier_offset_hz: self.mean_prev,
            peak_deviation_hz: peak,
            stereo: pilot.present,
            pilot,
            deemphasis_tau_s: self.config.deemphasis_tau_s,
            audio_rate_hz: self.audio_rate_hz(),
            rds: self.rds.as_ref().map(|(_, dec)| dec.report()),
            rds_timing_contrast: self.rds.as_ref().map(|(d, _)| d.timing_contrast()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hk_dsp::synth::{Rng, complex_noise};

    #[test]
    fn noise_gives_no_pilot_and_no_pi() {
        let mut rng = Rng::new(21);
        let iq = complex_noise(&mut rng, 3 * MPX_RATE_HZ as usize, 1e-3);
        let mut wfm = WfmDemod::new(WfmConfig::default(), MPX_RATE_HZ).unwrap();
        for chunk in iq.chunks(65_536) {
            wfm.process(chunk);
        }
        let r = wfm.report();
        assert!(!r.pilot.present, "{:?}", r.pilot);
        let rds = r.rds.unwrap();
        assert!(rds.pi.is_none() && rds.frame_log.is_empty(), "{rds:?}");
    }
}
