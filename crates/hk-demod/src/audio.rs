//! Streaming analog audio for Listen (T-043, C19). Where [`crate::AnalogReceiver`] demodulates
//! a finished window, this runs live on successive source chunks:
//!
//! 1. [`probe`]: C13 estimate + [`ModeSelector`] on a short leading snippet. **No manual mode
//!    input**: the mode, channel centre and bandwidth all come from the estimate.
//! 2. [`AudioPlan::from_probe`]: the channel to demodulate (centre from the measured emission,
//!    bandwidth per mode from OBW99), the noise power the squelch compares against
//!    (C13 noise density × channel bandwidth), whether AGC runs.
//! 3. [`AudioDemod`]: DDC → demodulator → 48 kS/s mono audio, with squelch and AGC.
//!
//! | Mode | Channel | Audio |
//! |---|---|---|
//! | WFM | 200 kHz at 240 kS/s | [`WfmDemod`] mono, 75 µs de-emphasis, deviation-scaled (no AGC) |
//! | NBFM | OBW99 × 1.25, 6–25 kHz | discriminator / 5 kHz, 3.5 kHz low-pass, DC removed (no AGC) |
//! | AM | OBW99 × 1.1, 5–20 kHz | envelope, DC removed, 5 kHz low-pass, AGC |
//! | SSB | OBW99, 2.4–4 kHz | sideband (from spectral symmetry) shifted to 0 Hz, real part, AGC |
//! | CW | 500 Hz | carrier shifted to a 700 Hz tone, AGC |
//!
//! **Squelch.** Channel power (DDC output, smoothed over ~50 ms) against the probe's channel
//! noise power; opens at `squelch_open_snr_db`, closes `squelch_hysteresis_db` lower. Without a
//! noise estimate it stays open. The audio is still produced while closed (filter state stays
//! continuous); callers withhold it ([`AudioDemod::squelch_open`]).
//!
//! **AGC.** Peak envelope follower (fast attack, slow decay) to `agc_target_dbfs`, gain capped
//! at `agc_max_gain_db`, output clamped to ±1.

use std::f64::consts::TAU;

use hk_dsp::{Ddc, DdcSpec, InputInfo, IqSample};
use hk_estimate::{
    EstimatorConfig, Hints, ParamEstimator, ParameterSet, SnippetConfig, SnippetExtractor,
    SnippetRequest,
};
use num_complex::Complex32;

use crate::dsp::{Discriminator, FirDecimator, lowpass_taps};
use crate::mode::{AnalogMode, ModeDecision, ModeSelector};
use crate::receiver::DemodError;
use crate::wfm::{MPX_RATE_HZ, WfmConfig, WfmDemod};

/// Audio output rate, Hz.
pub const AUDIO_RATE_HZ: f64 = 48_000.0;
/// Demodulator id and version for listen audio.
pub const LISTEN_DEMOD_VERSION: &str = "hk-demod/listen-audio@0.1.0";

/// Listen audio settings.
#[derive(Clone, Debug, PartialEq)]
pub struct AudioConfig {
    /// Squelch opening SNR, dB.
    pub squelch_open_snr_db: f64,
    /// Squelch hysteresis, dB.
    pub squelch_hysteresis_db: f64,
    /// AGC target, dBFS (peak envelope).
    pub agc_target_dbfs: f64,
    /// Largest AGC gain, dB.
    pub agc_max_gain_db: f64,
    /// AGC attack time constant, s.
    pub agc_attack_s: f64,
    /// AGC decay time constant, s.
    pub agc_decay_s: f64,
    /// NBFM deviation mapped to full scale, Hz.
    pub nbfm_deviation_hz: f64,
    /// NBFM audio low-pass, Hz.
    pub nbfm_audio_hz: f64,
    /// AM audio low-pass, Hz.
    pub am_audio_hz: f64,
    /// CW beat tone, Hz.
    pub cw_tone_hz: f64,
    /// WFM channel bandwidth, Hz.
    pub wfm_channel_bandwidth_hz: f64,
    /// Channel-power smoothing time constant, s.
    pub level_tau_s: f64,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            squelch_open_snr_db: 6.0,
            squelch_hysteresis_db: 3.0,
            agc_target_dbfs: -6.0,
            agc_max_gain_db: 60.0,
            agc_attack_s: 0.002,
            agc_decay_s: 0.5,
            nbfm_deviation_hz: 5_000.0,
            nbfm_audio_hz: 3_500.0,
            am_audio_hz: 5_000.0,
            cw_tone_hz: 700.0,
            wfm_channel_bandwidth_hz: 200e3,
            level_tau_s: 0.05,
        }
    }
}

/// What a probe measured.
#[derive(Clone, Debug)]
pub struct ProbeResult {
    /// C13 estimate.
    pub params: ParameterSet,
    /// Mode decision.
    pub mode: ModeDecision,
    /// RF centre of the emission, Hz.
    pub rf_center_hz: f64,
}

/// Estimates and selects the mode for `request` over `iq` (first sample
/// `info.time.sample_index`).
pub fn probe<T: IqSample>(
    info: InputInfo<'_>,
    iq: &[T],
    request: &SnippetRequest,
) -> Result<ProbeResult, DemodError> {
    let base = info.time.sample_index;
    if request.start_index < base || request.end_index > base + iq.len() as u64 {
        return Err(DemodError::InvalidRequest(
            "probe request outside the samples".into(),
        ));
    }
    let mut extractor = SnippetExtractor::new(SnippetConfig::default());
    let snip = extractor.extract(info, iq, request)?;
    let params = ParamEstimator::new(EstimatorConfig::default()).estimate(&snip, &Hints::default());
    let mode = ModeSelector::default().select(&snip, &params);
    let rf_center_hz = params
        .rf_center_hz
        .value()
        .unwrap_or_else(|| snip.rf_center_hz());
    Ok(ProbeResult {
        params,
        mode,
        rf_center_hz,
    })
}

/// SSB sideband.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sideband {
    /// Upper sideband (energy above the suppressed carrier).
    Upper,
    /// Lower sideband.
    Lower,
}

/// The channel and processing chosen from a probe.
#[derive(Clone, Debug, PartialEq)]
pub struct AudioPlan {
    /// Mode.
    pub mode: AnalogMode,
    /// SSB sideband.
    pub sideband: Option<Sideband>,
    /// Channel centre, RF Hz.
    pub channel_center_hz: f64,
    /// Channel bandwidth, Hz.
    pub channel_bandwidth_hz: f64,
    /// Channel noise power (±1 full-scale units), if estimated.
    pub noise_power: Option<f64>,
    /// AGC runs.
    pub agc: bool,
    /// De-emphasis, s.
    pub deemphasis_s: Option<f64>,
}

impl AudioPlan {
    /// The plan for a probe; `Err` with the reason when no analog mode was recognised.
    pub fn from_probe(p: &ProbeResult, cfg: &AudioConfig) -> Result<Self, String> {
        let obw = p.params.obw99_hz.value();
        let bw = |factor: f64, lo: f64, hi: f64| (obw.unwrap_or(lo) * factor).clamp(lo, hi);
        let (bandwidth, sideband, agc, deemph) = match p.mode.mode {
            AnalogMode::Wfm => (
                cfg.wfm_channel_bandwidth_hz,
                None,
                false,
                Some(WfmConfig::default().deemphasis_tau_s),
            ),
            AnalogMode::Nbfm => (bw(1.25, 6e3, 25e3), None, false, None),
            AnalogMode::Am => (bw(1.1, 5e3, 20e3), None, true, None),
            AnalogMode::Ssb => {
                let lower = p.params.shape.symmetry.is_some_and(|s| s > 0.0);
                let sb = if lower {
                    Sideband::Lower
                } else {
                    Sideband::Upper
                };
                (bw(1.0, 2.4e3, 4e3), Some(sb), true, None)
            }
            AnalogMode::Cw => (500.0, None, true, None),
            AnalogMode::Unknown => {
                return Err(p
                    .mode
                    .reason
                    .clone()
                    .unwrap_or_else(|| "no analog modulation recognised".into()));
            }
        };
        let noise_power = p
            .params
            .noise_density
            .value()
            .filter(|n| n.is_finite() && *n > 0.0)
            .map(|n| n * bandwidth);
        Ok(Self {
            mode: p.mode.mode,
            sideband,
            channel_center_hz: p.rf_center_hz,
            channel_bandwidth_hz: bandwidth,
            noise_power,
            agc,
            deemphasis_s: deemph,
        })
    }

    /// Wire mode name: `wfm`, `nbfm`, `am`, `usb`, `lsb`, `cw`.
    pub fn mode_name(&self) -> &'static str {
        match (self.mode, self.sideband) {
            (AnalogMode::Ssb, Some(Sideband::Lower)) => "lsb",
            (AnalogMode::Ssb, _) => "usb",
            (m, _) => m.as_str(),
        }
    }

    /// `[lo, hi]` of the demodulated channel, Hz.
    pub fn channel_extent_hz(&self) -> (f64, f64) {
        let h = 0.5 * self.channel_bandwidth_hz;
        (self.channel_center_hz - h, self.channel_center_hz + h)
    }
}

enum Kind {
    Wfm(Box<WfmDemod>),
    Nbfm {
        disc: Discriminator,
        lp: FirDecimator<f32>,
        scale: f32,
        dc: f32,
    },
    Am {
        lp: FirDecimator<f32>,
        dc: f32,
    },
    Shift {
        /// Oscillator step, rad/sample.
        step: f64,
        phase: f64,
    },
}

/// Streaming audio demodulator for one channel.
pub struct AudioDemod {
    plan: AudioPlan,
    cfg: AudioConfig,
    ddc: Ddc,
    kind: Kind,
    out: Vec<f32>,
    level: f64,
    level_init: bool,
    squelch_open: bool,
    env: f32,
    gain: f32,
}

impl AudioDemod {
    /// A demodulator for `plan` on a source at `source_rate_hz` tuned to `tuned_center_hz`.
    pub fn new(
        plan: AudioPlan,
        cfg: AudioConfig,
        source_rate_hz: f64,
        tuned_center_hz: f64,
    ) -> Result<Self, DemodError> {
        let wfm = plan.mode == AnalogMode::Wfm;
        let rate = if wfm { MPX_RATE_HZ } else { AUDIO_RATE_HZ };
        let ddc = Ddc::new(
            DdcSpec::new(
                plan.channel_center_hz - tuned_center_hz,
                plan.channel_bandwidth_hz,
            )
            .with_output_rate(rate),
            source_rate_hz,
        )?;
        let kind = match (plan.mode, plan.sideband) {
            (AnalogMode::Wfm, _) => {
                let wc = WfmConfig {
                    rds: None,
                    ..WfmConfig::default()
                };
                Kind::Wfm(Box::new(WfmDemod::new(wc, MPX_RATE_HZ)?))
            }
            (AnalogMode::Nbfm, _) => Kind::Nbfm {
                disc: Discriminator::new(rate),
                lp: FirDecimator::new(
                    lowpass_taps(rate, cfg.nbfm_audio_hz, cfg.nbfm_audio_hz * 1.4, 50.0)?,
                    1,
                ),
                scale: (1.0 / cfg.nbfm_deviation_hz) as f32,
                dc: 0.0,
            },
            (AnalogMode::Am, _) => Kind::Am {
                lp: FirDecimator::new(
                    lowpass_taps(rate, cfg.am_audio_hz, cfg.am_audio_hz * 1.4, 50.0)?,
                    1,
                ),
                dc: 0.0,
            },
            (AnalogMode::Ssb, sb) => {
                let half = 0.5 * plan.channel_bandwidth_hz;
                let shift = if sb == Some(Sideband::Lower) {
                    -half
                } else {
                    half
                };
                Kind::Shift {
                    step: TAU * shift / rate,
                    phase: 0.0,
                }
            }
            _ => Kind::Shift {
                step: TAU * cfg.cw_tone_hz / rate,
                phase: 0.0,
            },
        };
        Ok(Self {
            squelch_open: plan.noise_power.is_none(),
            plan,
            cfg,
            ddc,
            kind,
            out: Vec::new(),
            level: 0.0,
            level_init: false,
            env: 0.0,
            gain: 1.0,
        })
    }

    /// The plan.
    pub fn plan(&self) -> &AudioPlan {
        &self.plan
    }

    /// Demodulates contiguous source samples; audio accumulates for [`Self::take_audio`].
    pub fn process<T: IqSample>(
        &mut self,
        info: InputInfo<'_>,
        iq: &[T],
    ) -> Result<(), DemodError> {
        let block = self.ddc.process(info, iq)?;
        let rate = block.header.sample_rate_hz;
        let x = block.samples;
        if x.is_empty() {
            return Ok(());
        }
        // Channel power, smoothed.
        let alpha = 1.0 - (-1.0 / (self.cfg.level_tau_s * rate)).exp();
        let mut level = self.level;
        for s in x {
            let p = f64::from(s.norm_sqr());
            if !self.level_init {
                level = p;
                self.level_init = true;
            }
            level += alpha * (p - level);
        }
        self.level = level;
        if let Some(n) = self.plan.noise_power {
            let snr = 10.0 * (level / n).log10();
            let open = self.cfg.squelch_open_snr_db;
            if snr >= open {
                self.squelch_open = true;
            } else if snr < open - self.cfg.squelch_hysteresis_db {
                self.squelch_open = false;
            }
        }
        let start = self.out.len();
        match &mut self.kind {
            Kind::Wfm(w) => {
                w.process(x);
                self.out.extend(w.take_audio());
            }
            Kind::Nbfm {
                disc,
                lp,
                scale,
                dc,
            } => {
                for &s in x {
                    let f = disc.push(s) * *scale;
                    if let Some(y) = lp.push(f) {
                        *dc += 0.001 * (y - *dc);
                        self.out.push(y - *dc);
                    }
                }
            }
            Kind::Am { lp, dc } => {
                for &s in x {
                    let a = s.norm();
                    *dc += 2e-4 * (a - *dc);
                    if let Some(y) = lp.push(a - *dc) {
                        self.out.push(y);
                    }
                }
            }
            Kind::Shift { step, phase } => {
                for &s in x {
                    let r = Complex32::new(phase.cos() as f32, phase.sin() as f32);
                    self.out.push((s * r).re);
                    *phase = (*phase + *step) % TAU;
                }
            }
        }
        if self.plan.agc {
            let fs = AUDIO_RATE_HZ;
            let attack = (1.0 - (-1.0 / (self.cfg.agc_attack_s * fs)).exp()) as f32;
            let decay = (1.0 - (-1.0 / (self.cfg.agc_decay_s * fs)).exp()) as f32;
            let target = 10f32.powf(self.cfg.agc_target_dbfs as f32 / 20.0);
            let max_gain = 10f32.powf(self.cfg.agc_max_gain_db as f32 / 20.0);
            for y in &mut self.out[start..] {
                let a = y.abs();
                let k = if a > self.env { attack } else { decay };
                self.env += k * (a - self.env);
                self.gain = (target / self.env.max(1e-9)).min(max_gain);
                *y = (*y * self.gain).clamp(-1.0, 1.0);
            }
        } else {
            for y in &mut self.out[start..] {
                *y = y.clamp(-1.0, 1.0);
            }
        }
        Ok(())
    }

    /// Also decodes RDS on a WFM channel (T-463: playback re-runs decode, not only demod, from
    /// raw IQ). Other modes are unchanged. Call before the first [`Self::process`]; the audio is
    /// the same either way (RDS only taps the MPX the audio path already computes).
    pub fn with_rds(mut self) -> Result<Self, DemodError> {
        if matches!(self.kind, Kind::Wfm(_)) {
            let wc = WfmConfig {
                rds: Some(crate::rds::RdsConfig::default()),
                ..WfmConfig::default()
            };
            self.kind = Kind::Wfm(Box::new(WfmDemod::new(wc, MPX_RATE_HZ)?));
        }
        Ok(self)
    }

    /// RDS groups decoded since the last call (empty unless [`Self::with_rds`] on a WFM channel).
    pub fn take_rds_groups(&mut self) -> Vec<crate::rds::RdsGroup> {
        match &mut self.kind {
            Kind::Wfm(w) => w.take_rds_groups(),
            _ => Vec::new(),
        }
    }

    /// Audio produced since the last call (48 kS/s mono, ±1).
    pub fn take_audio(&mut self) -> Vec<f32> {
        std::mem::take(&mut self.out)
    }

    /// Moves the audio produced so far into `out`.
    pub fn drain_audio_into(&mut self, out: &mut Vec<f32>) {
        out.append(&mut self.out);
    }

    /// Squelch state.
    pub fn squelch_open(&self) -> bool {
        self.squelch_open
    }

    /// Smoothed channel power, dBFS.
    pub fn level_dbfs(&self) -> f64 {
        10.0 * self.level.max(1e-20).log10()
    }

    /// Channel SNR, dB, with a noise estimate.
    pub fn snr_db(&self) -> Option<f64> {
        self.plan
            .noise_power
            .map(|n| 10.0 * (self.level.max(1e-20) / n).log10())
    }

    /// AGC gain, dB (0 without AGC).
    pub fn agc_gain_db(&self) -> f64 {
        if self.plan.agc {
            20.0 * f64::from(self.gain.max(1e-9)).log10()
        } else {
            0.0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hk_core::{Discontinuity, ProvenanceHandle};
    use hk_model::{SampleTime, Timestamp};

    const FS: f64 = 1_000_000.0;
    const FC: f64 = 150e6;

    fn provenance() -> ProvenanceHandle {
        let p: hk_model::Provenance = serde_json::from_value(serde_json::json!({
            "device_id": "synthetic:hk-demod-audio-test",
            "tune": {
                "center_hz": FC, "sample_rate_hz": FS, "lna_db": 16.0, "vga_db": 20.0,
                "amp_on": false, "bandwidth_hz": FS * 0.75,
            },
            "overload": false, "quantisation_limited": false, "clock_source": "internal",
            "clock_locked": true, "timestamp_method": "synthetic", "timestamp_error_budget_ns": 0,
        }))
        .unwrap();
        ProvenanceHandle::new(p)
    }

    fn info(p: &ProvenanceHandle, index: u64) -> InputInfo<'_> {
        InputInfo {
            time: SampleTime {
                sample_index: index,
                host_time: Timestamp::from_unix_nanos(1_700_000_000_000_000_000),
            },
            discontinuity: if index == 0 {
                Discontinuity::STREAM_START
            } else {
                Discontinuity::NONE
            },
            dropped_before: 0,
            provenance: p,
        }
    }

    /// Tone power at `f` relative to total power of `x`, dB (single-bin DFT over whole cycles).
    fn tone_db(x: &[f32], fs: f64, f: f64) -> f64 {
        let (mut re, mut im, mut total) = (0.0f64, 0.0f64, 0.0f64);
        for (n, &v) in x.iter().enumerate() {
            let ph = TAU * f * n as f64 / fs;
            re += f64::from(v) * ph.cos();
            im += f64::from(v) * ph.sin();
            total += f64::from(v).powi(2);
        }
        let tone = 2.0 * (re * re + im * im) / x.len() as f64;
        10.0 * (tone / total.max(1e-30)).log10()
    }

    fn noise(n: usize, seed: u64, sigma: f32) -> Vec<Complex32> {
        let mut rng = hk_dsp::synth::Rng::new(seed);
        hk_dsp::synth::complex_noise(&mut rng, n, f64::from(sigma).powi(2))
    }

    fn run(iq: &[Complex32], offset_hz: f64, bw: f64) -> (AudioPlan, Vec<f32>, bool) {
        let p = provenance();
        let probe_len = (0.5 * FS) as usize;
        let req = SnippetRequest {
            start_index: 0,
            end_index: probe_len as u64,
            center_offset_hz: offset_hz,
            bandwidth_hz: bw,
        };
        let pr = probe(info(&p, 0), &iq[..probe_len], &req).unwrap();
        let plan = AudioPlan::from_probe(&pr, &AudioConfig::default())
            .unwrap_or_else(|why| panic!("no plan: {why} ({:?})", pr.mode));
        let mut d = AudioDemod::new(plan.clone(), AudioConfig::default(), FS, FC).unwrap();
        let mut audio = Vec::new();
        let mut idx = probe_len;
        for chunk in iq[probe_len..].chunks(8192) {
            d.process(info(&p, idx as u64), chunk).unwrap();
            d.drain_audio_into(&mut audio);
            idx += chunk.len();
        }
        (plan, audio, d.squelch_open())
    }

    #[test]
    fn nbfm_tone_is_auto_selected_and_demodulated() {
        let n = (1.5 * FS) as usize;
        let off = 120e3;
        let mut iq = noise(n, 7, 0.002);
        let (dev, tone) = (3_000.0, 1_000.0);
        for (k, s) in iq.iter_mut().enumerate() {
            let t = k as f64 / FS;
            let ph = TAU * off * t + dev / tone * (TAU * tone * t).sin();
            *s += Complex32::new(0.2 * ph.cos() as f32, 0.2 * ph.sin() as f32);
        }
        let (plan, audio, open) = run(&iq, off, 20e3);
        assert_eq!(plan.mode_name(), "nbfm");
        assert!(
            (plan.channel_center_hz - (FC + off)).abs() < 2e3,
            "{plan:?}"
        );
        assert!(open, "strong carrier opens the squelch");
        let tail = &audio[audio.len() / 4..];
        assert!(
            tone_db(tail, AUDIO_RATE_HZ, tone) > -3.0,
            "1 kHz tone dominates the audio"
        );
    }

    #[test]
    fn am_tone_gets_agc_and_noise_keeps_the_squelch_closed() {
        let n = (1.5 * FS) as usize;
        let off = -80e3;
        let mut iq = noise(n, 9, 0.002);
        let tone = 700.0;
        for (k, s) in iq.iter_mut().enumerate() {
            let t = k as f64 / FS;
            let a = 0.1 * (1.0 + 0.6 * (TAU * tone * t).cos());
            let ph = TAU * off * t;
            *s += Complex32::new((a * ph.cos()) as f32, (a * ph.sin()) as f32);
        }
        // Mode selection is T-012's (its own tests); this checks the AM demodulator and AGC.
        let plan = AudioPlan {
            mode: AnalogMode::Am,
            sideband: None,
            channel_center_hz: FC + off,
            channel_bandwidth_hz: 10e3,
            noise_power: None,
            agc: true,
            deemphasis_s: None,
        };
        assert_eq!(plan.mode_name(), "am");
        let p = provenance();
        let mut d = AudioDemod::new(plan.clone(), AudioConfig::default(), FS, FC).unwrap();
        let mut audio = Vec::new();
        let mut idx = 0;
        for chunk in iq.chunks(8192) {
            d.process(info(&p, idx as u64), chunk).unwrap();
            d.drain_audio_into(&mut audio);
            idx += chunk.len();
        }
        assert!(d.agc_gain_db() > 0.0);
        let tail = &audio[audio.len() / 4..];
        let peak = tail.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(peak > 0.2, "AGC brings the audio up (peak {peak})");
        assert!(tone_db(tail, AUDIO_RATE_HZ, tone) > -3.0);

        // A plan whose noise estimate sits far above the channel power keeps the squelch shut.
        let mut quiet = plan.clone();
        quiet.noise_power = Some(1.0);
        let p = provenance();
        let mut d = AudioDemod::new(quiet, AudioConfig::default(), FS, FC).unwrap();
        d.process(info(&p, 0), &iq[..65_536]).unwrap();
        assert!(!d.squelch_open());
    }

    #[test]
    fn noise_only_is_refused_as_unknown() {
        let iq = noise((0.5 * FS) as usize, 11, 0.01);
        let p = provenance();
        let req = SnippetRequest {
            start_index: 0,
            end_index: iq.len() as u64,
            center_offset_hz: 50e3,
            bandwidth_hz: 20e3,
        };
        let pr = probe(info(&p, 0), &iq, &req).unwrap();
        assert!(AudioPlan::from_probe(&pr, &AudioConfig::default()).is_err());
    }
}
