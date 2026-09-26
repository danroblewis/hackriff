//! Streaming analog audio for Listen (T-043, C19). Where [`crate::AnalogReceiver`] demodulates
//! a finished window, this runs live on successive source chunks:
//!
//! 1. [`probe`]: C13 estimate + [`ModeSelector`] on a short leading snippet. **No manual mode
//!    input**: the mode, channel centre and bandwidth all come from the estimate.
//! 2. [`AudioPlan::from_probe`]: the channel to demodulate (centre from the measured emission,
//!    bandwidth per mode from OBW99), the noise power the squelch compares against
//!    (C13 noise density × channel bandwidth), whether AGC runs.
//! 3. [`AudioDemod`]: DDC → demodulator → 48 kS/s mono audio, with squelch and AGC — or, on a
//!    WFM channel whose consumer asked for it ([`AudioDemod::with_stereo`], T-874), interleaved
//!    `L, R` audio from the pilot-locked L−R channel (mono content in both while unlocked).
//!
//! | Mode | Channel | Audio |
//! |---|---|---|
//! | WFM | 200 kHz at 240 kS/s | [`WfmDemod`] mono, 75 µs de-emphasis, deviation-scaled (no AGC) |
//! | NBFM | OBW99 × 1.25, 6–25 kHz | discriminator / 5 kHz, 3.5 kHz low-pass, DC removed (no AGC); CTCSS/DCS identified blind on the discriminator ([`AudioDemod::subaudible`], T-988) |
//! | AM | OBW99 × 1.1, 5–20 kHz | envelope, DC removed, 5 kHz low-pass, AGC |
//! | SSB | OBW99, 2.4–4 kHz | sideband (from spectral symmetry) shifted to 0 Hz, real part, AGC |
//! | CW | 500 Hz | carrier shifted to a 700 Hz tone, AGC |
//!
//! **Squelch.** Channel power (DDC output, smoothed over ~50 ms) against the probe's channel
//! noise power; opens at `squelch_open_snr_db`, closes `squelch_hysteresis_db` lower. Without a
//! noise estimate it stays open. The audio is still produced while closed (filter state stays
//! continuous); callers withhold it ([`AudioDemod::squelch_open`]). [`AudioDemod::level_dbfs`]
//! reports what is actually delivered: it tracks only the samples produced while squelch is
//! open, reads [`SILENCE_FLOOR_DBFS`] for the whole closed period (never the discarded demod
//! output — loud discriminator noise on a closed NBFM channel), and restarts fresh the next time
//! squelch opens (T-1015).
//!
//! **AGC.** Peak envelope follower (fast attack, slow decay) to `agc_target_dbfs`, gain capped
//! at `agc_max_gain_db`, output clamped to ±1.

use std::f64::consts::TAU;

use hk_dsp::{Ddc, DdcSpec, InputInfo, IqSample};
use hk_estimate::{
    EstimatorConfig, Hints, ParamEstimator, ParameterSet, SnippetConfig, SnippetExtractor,
    SnippetRequest,
};
use hk_model::Subaudible;
use num_complex::Complex32;

use crate::dsp::{Discriminator, FirDecimator, lowpass_taps};
use crate::mode::{AnalogMode, ModeDecision, ModeSelector};
use crate::receiver::DemodError;
use crate::subaudible::{SubaudibleConfig, SubaudibleDetector};
use crate::wfm::{MPX_RATE_HZ, WfmConfig, WfmDemod};

/// Audio output rate, Hz.
pub const AUDIO_RATE_HZ: f64 = 48_000.0;
/// Demodulator id and version for listen audio.
pub const LISTEN_DEMOD_VERSION: &str = "hk-demod/listen-audio@0.1.0";
/// [`AudioDemod::level_dbfs`] while squelch is closed (T-1015): no audio is delivered, so the
/// level is documented silence, not the discarded demod output. Below the smallest level a real
/// signal chain reports, so it reads unambiguously as "nothing delivered" rather than a measured
/// quiet passage.
pub const SILENCE_FLOOR_DBFS: f64 = -120.0;

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
        if p.mode.mode == AnalogMode::Unknown {
            return Err(p
                .mode
                .reason
                .clone()
                .unwrap_or_else(|| "no analog modulation recognised".into()));
        }
        let sideband = (p.mode.mode == AnalogMode::Ssb).then(|| {
            if p.params.shape.symmetry.is_some_and(|s| s > 0.0) {
                Sideband::Lower
            } else {
                Sideband::Upper
            }
        });
        let mut plan = Self::for_mode(
            p.mode.mode,
            sideband,
            p.rf_center_hz,
            p.params.obw99_hz.value(),
            cfg,
        )
        .ok_or_else(|| "no analog modulation recognised".to_owned())?;
        plan.noise_power = p
            .params
            .noise_density
            .value()
            .filter(|n| n.is_finite() && *n > 0.0)
            .map(|n| n * plan.channel_bandwidth_hz);
        Ok(plan)
    }

    /// The plan for a mode that was **already decided** — by a probe ([`Self::from_probe`]), or
    /// by the per-burst evidence an emitter accumulated while it was keyed (T-987: Listen opened
    /// in the silence between bursts). The channel is `center_hz`, as wide as the mode's rule
    /// makes `obw_hz` (the table in the [module docs](self)); `None` for [`AnalogMode::Unknown`].
    ///
    /// No noise power: that is a measurement of the channel *now*, which the caller makes
    /// ([`channel_noise_power`]) or takes from its probe. Without one the squelch stays open.
    pub fn for_mode(
        mode: AnalogMode,
        sideband: Option<Sideband>,
        center_hz: f64,
        obw_hz: Option<f64>,
        cfg: &AudioConfig,
    ) -> Option<Self> {
        let bw = |factor: f64, lo: f64, hi: f64| (obw_hz.unwrap_or(lo) * factor).clamp(lo, hi);
        let (bandwidth, sideband, agc, deemph) = match mode {
            AnalogMode::Wfm => (
                cfg.wfm_channel_bandwidth_hz,
                None,
                false,
                Some(WfmConfig::default().deemphasis_tau_s),
            ),
            AnalogMode::Nbfm => (bw(1.25, 6e3, 25e3), None, false, None),
            AnalogMode::Am => (bw(1.1, 5e3, 20e3), None, true, None),
            AnalogMode::Ssb => (
                bw(1.0, 2.4e3, 4e3),
                Some(sideband.unwrap_or(Sideband::Upper)),
                true,
                None,
            ),
            AnalogMode::Cw => (500.0, None, true, None),
            AnalogMode::Unknown => return None,
        };
        Some(Self {
            mode,
            sideband,
            channel_center_hz: center_hz,
            channel_bandwidth_hz: bandwidth,
            noise_power: None,
            agc,
            deemphasis_s: deemph,
        })
    }

    /// The mode and sideband a stored mode label names: a [`Demodulation::mode`] or a
    /// Classification `family` (`wfm`, `nbfm`/`nfm`, `am`, `usb`, `lsb`, `ssb`, `cw`). `None` for
    /// anything that is not an analog audio mode (`2fsk`, `unknown`, a service family, …).
    ///
    /// A bare `ssb` names no sideband; [`Self::for_mode`] then demodulates the upper one.
    ///
    /// [`Demodulation::mode`]: hk_model::Demodulation::mode
    pub fn mode_from_label(label: &str) -> Option<(AnalogMode, Option<Sideband>)> {
        Some(match label.trim().to_ascii_lowercase().as_str() {
            "wfm" => (AnalogMode::Wfm, None),
            "nbfm" | "nfm" => (AnalogMode::Nbfm, None),
            "am" => (AnalogMode::Am, None),
            "usb" => (AnalogMode::Ssb, Some(Sideband::Upper)),
            "lsb" => (AnalogMode::Ssb, Some(Sideband::Lower)),
            "ssb" => (AnalogMode::Ssb, None),
            "cw" => (AnalogMode::Cw, None),
            _ => return None,
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

/// The channel power of `iq` through `plan`'s channel filter (±1 full-scale units, the unit
/// [`AudioPlan::noise_power`] is in), for arming the squelch on a channel that is **quiet now**
/// (T-987). `None` when the samples yield too little channel output to measure.
///
/// The channel is the one [`AudioDemod`] will demodulate, through the same DDC, so the squelch
/// compares like with like. The answer is the **median** of ~10 ms block powers rather than the
/// mean: a probe window that caught the tail of a burst is still read as the floor it mostly
/// was, not lifted by the burst.
pub fn channel_noise_power<T: IqSample>(
    plan: &AudioPlan,
    source_rate_hz: f64,
    tuned_center_hz: f64,
    info: InputInfo<'_>,
    iq: &[T],
) -> Result<Option<f64>, DemodError> {
    let wfm = plan.mode == AnalogMode::Wfm;
    let rate = if wfm { MPX_RATE_HZ } else { AUDIO_RATE_HZ };
    let mut ddc = Ddc::new(
        DdcSpec::new(
            plan.channel_center_hz - tuned_center_hz,
            plan.channel_bandwidth_hz,
        )
        .with_output_rate(rate),
        source_rate_hz,
    )?;
    let block = ddc.process(info, iq)?;
    let out_rate = block.header.sample_rate_hz;
    let x = block.samples;
    let per = ((0.01 * out_rate) as usize).max(16);
    // The filter's start-up transient is not the channel: skip the first block.
    let mut powers: Vec<f64> = x
        .chunks_exact(per)
        .skip(1)
        .map(|c| c.iter().map(|s| f64::from(s.norm_sqr())).sum::<f64>() / c.len() as f64)
        .filter(|p| p.is_finite())
        .collect();
    if powers.len() < 3 {
        return Ok(None);
    }
    powers.sort_by(f64::total_cmp);
    let median = powers[powers.len() / 2];
    Ok((median > 0.0).then_some(median))
}

enum Kind {
    Wfm(Box<WfmDemod>),
    Nbfm {
        disc: Discriminator,
        lp: FirDecimator<f32>,
        scale: f32,
        dc: f32,
        /// Blind CTCSS/DCS identification on the discriminator (T-988).
        sub: Box<SubaudibleDetector>,
        /// Discriminator output of the current block, Hz (reused; no steady-state allocation).
        hz: Vec<f32>,
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
    audio_level: f64,
    audio_level_init: bool,
    /// The delivered-audio level was last updated while squelch was open (T-1015): tells
    /// [`Self::process`] to restart the smoother on the next open, rather than resume from a
    /// stale value the closed period never updated.
    audio_level_was_open: bool,
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
                sub: Box::new(SubaudibleDetector::new(SubaudibleConfig::default(), rate)?),
                hz: Vec::new(),
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
            audio_level: 0.0,
            audio_level_init: false,
            audio_level_was_open: false,
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
                let m = w.take_audio();
                if w.is_stereo() {
                    // One S per M, aligned (the same filter on both paths): L = M + S, R = M − S.
                    let side = w.take_side();
                    self.out.reserve(2 * m.len());
                    for (&mm, &ss) in m.iter().zip(&side) {
                        self.out.push(mm + ss);
                        self.out.push(mm - ss);
                    }
                } else {
                    self.out.extend(m);
                }
            }
            Kind::Nbfm {
                disc,
                lp,
                scale,
                dc,
                sub,
                hz,
            } => {
                hz.clear();
                for &s in x {
                    let f_hz = disc.push(s);
                    hz.push(f_hz);
                    if let Some(y) = lp.push(f_hz * *scale) {
                        *dc += 0.001 * (y - *dc);
                        self.out.push(y - *dc);
                    }
                }
                // Only on-air audio is analysed: squelch-closed noise would dilute a tone.
                sub.push(hz, self.squelch_open);
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
        // Delivered-audio power, smoothed the same way as the channel power above but over the
        // samples that actually leave [`Self::take_audio`] / [`Self::drain_audio_into`] — after
        // demod, AGC and the ±1 clamp — so [`Self::level_dbfs`] reports what a listener (or the
        // stream's audio meter) gets, not the DDC's pre-demod channel power (T-966).
        //
        // While squelch is closed, [`listen`]/[`playback`] drop these very frames instead of
        // delivering them, so nothing is updated here (T-1015): tracking them would report the
        // discarded demod output (loud discriminator noise on NBFM) as if it had gone out.
        // `level_dbfs` reads [`SILENCE_FLOOR_DBFS`] for the whole closed period. On reopening,
        // the smoother restarts from the first delivered sample rather than resuming a value the
        // closed period left stale.
        if self.squelch_open {
            if !self.audio_level_was_open {
                self.audio_level_init = false;
            }
            self.audio_level_was_open = true;
            // One audio *frame* (mono: 1 sample; stereo: interleaved L, R) per unit of audio
            // time, so the smoothing time constant is computed per frame, not per interleaved
            // sample — halving it on stereo otherwise (T-1015). The level is the mean power over
            // both channels of a frame.
            let channels = self.channels() as usize;
            let new = &self.out[start..];
            if !new.is_empty() && channels > 0 {
                let alpha_audio = 1.0 - (-1.0 / (self.cfg.level_tau_s * AUDIO_RATE_HZ)).exp();
                let mut audio_level = self.audio_level;
                for frame in new.chunks_exact(channels) {
                    let p = frame
                        .iter()
                        .map(|&y| f64::from(y) * f64::from(y))
                        .sum::<f64>()
                        / channels as f64;
                    if !self.audio_level_init {
                        audio_level = p;
                        self.audio_level_init = true;
                    }
                    audio_level += alpha_audio * (p - audio_level);
                }
                self.audio_level = audio_level;
            }
        } else {
            self.audio_level_was_open = false;
        }
        Ok(())
    }

    /// Also decodes RDS on a WFM channel (T-463: playback re-runs decode, not only demod, from
    /// raw IQ). Other modes are unchanged. Call before the first [`Self::process`]; the audio is
    /// the same either way (RDS only taps the MPX the audio path already computes).
    pub fn with_rds(mut self) -> Result<Self, DemodError> {
        if let Kind::Wfm(old) = &self.kind {
            let stereo = old.is_stereo();
            let wc = WfmConfig {
                rds: Some(crate::rds::RdsConfig::default()),
                ..WfmConfig::default()
            };
            let mut w = WfmDemod::new(wc, MPX_RATE_HZ)?;
            if stereo {
                w.enable_stereo();
            }
            self.kind = Kind::Wfm(Box::new(w));
        }
        Ok(self)
    }

    /// Asks for two-channel audio (T-874, ADR-0015 §12.13). Only broadcast FM carries a second
    /// channel, so this enables the L−R path on a WFM channel and leaves every other mode mono;
    /// [`Self::channels`] says which it became — the stream header's `channels` comes from there,
    /// never from the request. Call before the first [`Self::process`].
    pub fn with_stereo(mut self) -> Self {
        if let Kind::Wfm(w) = &mut self.kind {
            w.enable_stereo();
        }
        self
    }

    /// Channels of the audio [`Self::take_audio`] yields: 2 (interleaved `L, R`) after
    /// [`Self::with_stereo`] on a WFM channel, otherwise 1. Fixed for the demodulator's life.
    pub fn channels(&self) -> u32 {
        match &self.kind {
            Kind::Wfm(w) if w.is_stereo() => 2,
            _ => 1,
        }
    }

    /// Two-channel audio only: L−R is being decoded right now (the stereo pilot is locked).
    /// `false` on a mono demodulator, and while the two channels carry the same mono audio.
    pub fn stereo_locked(&self) -> bool {
        matches!(&self.kind, Kind::Wfm(w) if w.stereo_locked())
    }

    /// Two-channel audio only: locked → unlocked transitions of the pilot so far.
    pub fn stereo_lock_losses(&self) -> u64 {
        match &self.kind {
            Kind::Wfm(w) => w.stereo_lock_losses(),
            _ => 0,
        }
    }

    /// RDS groups decoded since the last call (empty unless [`Self::with_rds`] on a WFM channel).
    pub fn take_rds_groups(&mut self) -> Vec<crate::rds::RdsGroup> {
        match &mut self.kind {
            Kind::Wfm(w) => w.take_rds_groups(),
            _ => Vec::new(),
        }
    }

    /// Audio produced since the last call (48 kS/s, ±1; interleaved `L, R` when
    /// [`Self::channels`] is 2).
    pub fn take_audio(&mut self) -> Vec<f32> {
        std::mem::take(&mut self.out)
    }

    /// Moves the audio produced so far into `out`.
    pub fn drain_audio_into(&mut self, out: &mut Vec<f32>) {
        out.append(&mut self.out);
    }

    /// Blind sub-audible squelch identification (T-988): `Some` on an NBFM channel — a CTCSS
    /// tone, a DCS code, `none` once enough on-air audio found neither, or `measuring` before —
    /// and `None` on every other mode (nobody looked).
    pub fn subaudible(&mut self) -> Option<Subaudible> {
        match &mut self.kind {
            Kind::Nbfm { sub, .. } => Some(sub.report()),
            _ => None,
        }
    }

    /// Carries the sub-audible analysis over from the demodulator this one replaces (a refined
    /// retune of the same NBFM channel), so a rebuild does not restart the measurement.
    pub fn inherit_subaudible(&mut self, old: &mut AudioDemod) {
        if let (Kind::Nbfm { sub, .. }, Kind::Nbfm { sub: prev, .. }) =
            (&mut self.kind, &mut old.kind)
        {
            std::mem::swap(sub, prev);
        }
    }

    /// Squelch state.
    pub fn squelch_open(&self) -> bool {
        self.squelch_open
    }

    /// Smoothed level of the **delivered audio**, dBFS — the same samples [`Self::take_audio`] /
    /// [`Self::drain_audio_into`] yield, after demod, AGC and the ±1 clamp, so it never reads
    /// above 0 dBFS and matches the RMS a consumer measures on the stream (T-966). The mean power
    /// over both channels when [`Self::channels`] is 2 (T-1015), with the smoothing time constant
    /// computed per audio frame so stereo keeps `level_tau_s`, not half of it.
    ///
    /// While squelch is closed nothing is delivered, so this reads [`SILENCE_FLOOR_DBFS`] — the
    /// documented floor — rather than the discarded demod output (T-1015): callers drop the
    /// frames while closed, and this must agree with what actually went out. Distinct from the
    /// pre-demod DDC channel power squelch and [`Self::snr_db`] compare against, which keep
    /// updating while closed.
    pub fn level_dbfs(&self) -> f64 {
        if !self.squelch_open || !self.audio_level_init {
            return SILENCE_FLOOR_DBFS;
        }
        10.0 * self.audio_level.max(1e-20).log10()
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

    /// A broadcast-FM station `off` Hz from the tuned centre carrying left = 1 kHz and right =
    /// 2.5 kHz (BS.450 multiplex, 75 kHz peak deviation). `pilot(t)` says whether the 19 kHz
    /// pilot is on the air at `t` (the L−R subcarrier always is).
    fn stereo_station(n: usize, off: f64, pilot: impl Fn(f64) -> bool) -> Vec<Complex32> {
        let mut iq = noise(n, 13, 0.002);
        let mut ph = 0.0f64;
        for (k, s) in iq.iter_mut().enumerate() {
            let t = k as f64 / FS;
            let l = 0.4 * (TAU * 1_000.0 * t).sin();
            let r = 0.4 * (TAU * 2_500.0 * t + 0.3).sin();
            let wt = TAU * 19_000.0 * t + 0.7;
            let p = if pilot(t) { 0.1 * wt.sin() } else { 0.0 };
            let mpx = 0.9 * (0.5 * (l + r) + 0.5 * (l - r) * (2.0 * wt).sin()) + p;
            ph = (ph + TAU * (off + 75e3 * mpx) / FS) % TAU;
            *s += Complex32::new(0.3 * ph.cos() as f32, 0.3 * ph.sin() as f32);
        }
        iq
    }

    fn wfm_plan(off: f64) -> AudioPlan {
        AudioPlan {
            mode: AnalogMode::Wfm,
            sideband: None,
            channel_center_hz: FC + off,
            channel_bandwidth_hz: 200e3,
            noise_power: None,
            agc: false,
            deemphasis_s: Some(75e-6),
        }
    }

    /// Runs `d` over `iq`; returns the audio and whether the pilot was locked after each chunk.
    fn demod_all(mut d: AudioDemod, iq: &[Complex32]) -> (AudioDemod, Vec<f32>) {
        let p = provenance();
        let mut audio = Vec::new();
        let mut idx = 0;
        for chunk in iq.chunks(8192) {
            d.process(info(&p, idx as u64), chunk).unwrap();
            d.drain_audio_into(&mut audio);
            idx += chunk.len();
        }
        (d, audio)
    }

    /// T-874 (ADR-0015 §12.13): asked for stereo, a WFM channel yields interleaved L/R with the
    /// two programmes separated; its mid channel is the mono stream's audio; mono is unchanged.
    #[test]
    fn stereo_wfm_separates_left_and_right_and_mono_is_its_mid() {
        let off = 200e3;
        let iq = stereo_station((1.5 * FS) as usize, off, |_| true);
        let mk = || AudioDemod::new(wfm_plan(off), AudioConfig::default(), FS, FC).unwrap();
        assert_eq!(mk().channels(), 1, "mono unless asked");
        let (mono_d, mono) = demod_all(mk(), &iq);
        assert!(!mono_d.stereo_locked() && mono_d.stereo_lock_losses() == 0);
        let (st_d, st) = demod_all(mk().with_stereo(), &iq);
        assert_eq!(st_d.channels(), 2);
        assert!(st_d.stereo_locked(), "the pilot locked");
        assert_eq!(st_d.stereo_lock_losses(), 0);
        assert_eq!(st.len(), 2 * mono.len(), "one L/R pair per mono sample");

        let tail = st.len() / 2 / 3 * 2..st.len() - st.len() % 2;
        let lr = &st[tail.clone()];
        let left: Vec<f32> = lr.iter().step_by(2).copied().collect();
        let right: Vec<f32> = lr.iter().skip(1).step_by(2).copied().collect();
        let (l1, l2) = (
            tone_db(&left, AUDIO_RATE_HZ, 1_000.0),
            tone_db(&left, AUDIO_RATE_HZ, 2_500.0),
        );
        let (r1, r2) = (
            tone_db(&right, AUDIO_RATE_HZ, 1_000.0),
            tone_db(&right, AUDIO_RATE_HZ, 2_500.0),
        );
        eprintln!("left 1k {l1:.1} dB 2.5k {l2:.1} dB; right 1k {r1:.1} dB 2.5k {r2:.1} dB");
        assert!(
            l1 > -1.0 && r2 > -1.0,
            "each channel carries its own programme"
        );
        assert!(l2 < -20.0 && r1 < -20.0, "≥ 20 dB separation");

        // The mid channel (L+R)/2 is exactly the mono path's audio (same filter, same samples).
        let worst = mono
            .iter()
            .zip(st.chunks_exact(2))
            .map(|(&m, lr)| (m - 0.5 * (lr[0] + lr[1])).abs())
            .fold(0.0f32, f32::max);
        assert!(worst < 1e-5, "mid differs from mono by {worst}");
        // Mono tone content is the mix, as it always was.
        let m_tail = &mono[mono.len() / 3..];
        assert!(tone_db(m_tail, AUDIO_RATE_HZ, 1_000.0) > -5.0);
        assert!(tone_db(m_tail, AUDIO_RATE_HZ, 2_500.0) > -5.0);

        // Only broadcast FM has a second channel: other modes stay mono when asked.
        let nbfm = AudioPlan {
            mode: AnalogMode::Nbfm,
            channel_bandwidth_hz: 12.5e3,
            deemphasis_s: None,
            ..wfm_plan(off)
        };
        let d = AudioDemod::new(nbfm, AudioConfig::default(), FS, FC).unwrap();
        assert_eq!(d.with_stereo().channels(), 1);
    }

    /// T-874 honesty: no pilot, no L−R — the two channels are identical, bit for bit; and a pilot
    /// that goes away is a counted lock loss, after which the channels are identical again.
    #[test]
    fn stereo_without_a_pilot_is_honest_mono_and_a_lost_pilot_is_counted() {
        let off = -150e3;
        let mk = || {
            AudioDemod::new(wfm_plan(off), AudioConfig::default(), FS, FC)
                .unwrap()
                .with_stereo()
        };
        let iq = stereo_station(FS as usize, off, |_| false);
        let (d, st) = demod_all(mk(), &iq);
        assert_eq!(
            d.channels(),
            2,
            "the stream shape is fixed; its content is not stereo"
        );
        assert!(!d.stereo_locked());
        assert!(
            st.chunks_exact(2).all(|lr| lr[0] == lr[1]),
            "unlocked: L = R exactly, never an L−R guessed from an unlocked carrier"
        );

        let iq = stereo_station((2.0 * FS) as usize, off, |t| t < 1.0);
        let (d, st) = demod_all(mk(), &iq);
        assert!(!d.stereo_locked(), "the pilot went away");
        assert_eq!(
            d.stereo_lock_losses(),
            1,
            "the loss is reported, not hidden"
        );
        let last = &st[st.len() - (0.2 * AUDIO_RATE_HZ) as usize * 2..];
        assert!(
            last.chunks_exact(2).all(|lr| lr[0] == lr[1]),
            "mono again after the loss"
        );
        let mid = &st[(0.8 * AUDIO_RATE_HZ) as usize * 2..(0.9 * AUDIO_RATE_HZ) as usize * 2];
        assert!(
            mid.chunks_exact(2).any(|lr| lr[0] != lr[1]),
            "stereo while locked"
        );
    }

    /// T-966: a live capture reported `level_dbfs` of +1.3..+1.6 dBFS (above full scale) while
    /// the delivered audio measured -9.4 dBFS RMS — `level_dbfs` was reading the pre-demod DDC
    /// channel power, which is unrelated to the audio a listener or the stream's meter gets and
    /// isn't bounded by full scale at all. Feed a carrier well above unity IQ amplitude (as an
    /// unnormalized front-end gain would deliver) modulating an NBFM tone: the pre-demod channel
    /// power alone would read `10*log10(2.0^2) ≈ +6 dBFS`, but the delivered audio (discriminator
    /// output, always clamped to ±1) cannot exceed 0 dBFS. `level_dbfs` must track the delivered
    /// audio's own RMS, not the channel power.
    ///
    /// Red on the pre-fix code (`level_dbfs` returning the channel-power `level`): a strong
    /// carrier reads `level_dbfs` far above 0 dBFS and far from the delivered audio's RMS.
    #[test]
    fn level_dbfs_tracks_the_delivered_audio_not_the_pre_demod_channel_power() {
        let n = (1.5 * FS) as usize;
        let off = 90e3;
        let mut iq = noise(n, 41, 0.002);
        let (dev, tone) = (3_000.0, 700.0);
        for (k, s) in iq.iter_mut().enumerate() {
            let t = k as f64 / FS;
            let ph = TAU * off * t + dev / tone * (TAU * tone * t).sin();
            // Amplitude 2.0: well above unity, as raw unnormalized front-end IQ can be.
            *s += Complex32::new(2.0 * ph.cos() as f32, 2.0 * ph.sin() as f32);
        }
        let plan = AudioPlan {
            mode: AnalogMode::Nbfm,
            sideband: None,
            channel_center_hz: FC + off,
            channel_bandwidth_hz: 20e3,
            noise_power: None,
            agc: false,
            deemphasis_s: None,
        };
        let p = provenance();
        let mut d = AudioDemod::new(plan, AudioConfig::default(), FS, FC).unwrap();
        let mut audio = Vec::new();
        let mut idx = 0;
        for chunk in iq.chunks(8192) {
            d.process(info(&p, idx as u64), chunk).unwrap();
            d.drain_audio_into(&mut audio);
            idx += chunk.len();
        }
        let reported = d.level_dbfs();
        assert!(
            reported <= 1e-6,
            "level_dbfs must never read above full scale (clamped audio): got {reported}"
        );
        // The delivered audio's own RMS, measured independently over the settled tail (skip the
        // smoothing filter's ~50 ms transient).
        let tail = &audio[audio.len() / 4..];
        let ms = tail
            .iter()
            .map(|&y| f64::from(y) * f64::from(y))
            .sum::<f64>()
            / tail.len() as f64;
        let measured_dbfs = 10.0 * ms.max(1e-20).log10();
        assert!(
            (reported - measured_dbfs).abs() < 0.5,
            "level_dbfs {reported} should be within 0.5 dB of the delivered audio's measured RMS \
             {measured_dbfs}"
        );
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

    /// T-987: a plan decided **before** the carrier is on the air (from an emitter's per-burst
    /// evidence) whose squelch is armed from the silence the probe measured stays shut through
    /// the silence, opens on the next burst, and demodulates that burst's tone.
    #[test]
    fn a_squelch_armed_on_the_silence_opens_on_the_next_burst() {
        let off = 120e3;
        let (dev, tone) = (3_000.0, 1_000.0);
        let quiet = (1.0 * FS) as usize;
        let n = quiet + (1.0 * FS) as usize;
        let mut iq = noise(n, 17, 0.002);
        for (k, s) in iq.iter_mut().enumerate().skip(quiet) {
            let t = k as f64 / FS;
            let ph = TAU * off * t + dev / tone * (TAU * tone * t).sin();
            *s += Complex32::new(0.2 * ph.cos() as f32, 0.2 * ph.sin() as f32);
        }
        // The silence alone is refused by the probe (nothing recognised) ...
        let p = provenance();
        let req = SnippetRequest {
            start_index: 0,
            end_index: (0.5 * FS) as u64,
            center_offset_hz: off,
            bandwidth_hz: 25e3,
        };
        let pr = probe(info(&p, 0), &iq[..(0.5 * FS) as usize], &req).unwrap();
        assert!(AudioPlan::from_probe(&pr, &AudioConfig::default()).is_err());
        // ... so the mode comes from elsewhere, and the squelch is armed from what was measured.
        let (mode, sb) = AudioPlan::mode_from_label("nbfm").unwrap();
        let mut plan =
            AudioPlan::for_mode(mode, sb, FC + off, Some(10e3), &AudioConfig::default()).unwrap();
        assert_eq!(plan.mode_name(), "nbfm");
        assert!((plan.channel_bandwidth_hz - 12.5e3).abs() < 1.0, "{plan:?}");
        let noise_power = channel_noise_power(&plan, FS, FC, info(&p, 0), &iq[..quiet])
            .unwrap()
            .expect("the silence is measurable");
        // The floor of a 12.5 kHz channel of complex noise of variance 0.002^2 per sample at 1 MS/s.
        let expect = 0.002f64.powi(2) * 12.5e3 / FS;
        assert!(
            noise_power > 0.2 * expect && noise_power < 5.0 * expect,
            "measured {noise_power:e}, expected about {expect:e}"
        );
        plan.noise_power = Some(noise_power);
        let mut d = AudioDemod::new(plan, AudioConfig::default(), FS, FC).unwrap();
        let mut idx = 0;
        let mut opened_in_silence = false;
        for chunk in iq[..quiet].chunks(8192) {
            d.process(info(&p, idx as u64), chunk).unwrap();
            opened_in_silence |= d.squelch_open();
            d.take_audio();
            idx += chunk.len();
        }
        assert!(
            !opened_in_silence,
            "the squelch stays shut while nothing is keyed"
        );
        let mut audio = Vec::new();
        for chunk in iq[quiet..].chunks(8192) {
            d.process(info(&p, idx as u64), chunk).unwrap();
            d.drain_audio_into(&mut audio);
            idx += chunk.len();
        }
        assert!(d.squelch_open(), "the returning carrier opens the squelch");
        let tail = &audio[audio.len() / 4..];
        assert!(
            tone_db(tail, AUDIO_RATE_HZ, tone) > -3.0,
            "the burst's tone"
        );
        assert_eq!(AudioPlan::mode_from_label("2fsk"), None);
        assert_eq!(AudioPlan::mode_from_label("unknown"), None);
        assert_eq!(
            AudioPlan::mode_from_label("LSB"),
            Some((AnalogMode::Ssb, Some(Sideband::Lower)))
        );
    }

    /// T-1015: while squelch is closed the audio is still produced internally (filter state stays
    /// continuous) but withheld by every caller (`listen`/`playback` drop the frame), so
    /// `level_dbfs` must report the documented silence floor, not the discarded demod output — for
    /// NBFM that output is loud discriminator noise, which the pre-fix code reported straight
    /// through as a level.
    ///
    /// Red on the pre-fix code: with the squelch held shut throughout by a noise estimate far
    /// above the channel power, `level_dbfs` tracked the (withheld) discriminator noise instead of
    /// [`SILENCE_FLOOR_DBFS`].
    #[test]
    fn level_dbfs_reads_the_silence_floor_while_squelch_is_closed() {
        let n = (1.5 * FS) as usize;
        let off = 90e3;
        let mut iq = noise(n, 41, 0.002);
        let (dev, tone) = (3_000.0, 700.0);
        for (k, s) in iq.iter_mut().enumerate() {
            let t = k as f64 / FS;
            let ph = TAU * off * t + dev / tone * (TAU * tone * t).sin();
            *s += Complex32::new(0.2 * ph.cos() as f32, 0.2 * ph.sin() as f32);
        }
        let plan = AudioPlan {
            mode: AnalogMode::Nbfm,
            sideband: None,
            channel_center_hz: FC + off,
            channel_bandwidth_hz: 20e3,
            // Far above the channel power: the squelch never opens.
            noise_power: Some(1.0),
            agc: false,
            deemphasis_s: None,
        };
        let p = provenance();
        let mut d = AudioDemod::new(plan, AudioConfig::default(), FS, FC).unwrap();
        let mut idx = 0;
        for chunk in iq.chunks(8192) {
            d.process(info(&p, idx as u64), chunk).unwrap();
            assert!(!d.squelch_open(), "the noise estimate keeps this shut");
            d.take_audio();
            idx += chunk.len();
        }
        assert_eq!(
            d.level_dbfs(),
            SILENCE_FLOOR_DBFS,
            "closed squelch reports the documented floor, not the withheld demod output"
        );
    }

    /// T-1015: on reopening, the level restarts from the delivered samples rather than resuming a
    /// value the closed period never updated — so right after reopening it already tracks the new
    /// burst, not a stale reading dragged from before the close.
    #[test]
    fn level_dbfs_restarts_from_delivered_audio_when_squelch_reopens() {
        let off = 120e3;
        let (dev, tone) = (3_000.0, 1_000.0);
        let quiet = (0.5 * FS) as usize;
        let n = quiet + (0.5 * FS) as usize;
        let mut iq = noise(n, 17, 0.002);
        for (k, s) in iq.iter_mut().enumerate().skip(quiet) {
            let t = k as f64 / FS;
            let ph = TAU * off * t + dev / tone * (TAU * tone * t).sin();
            *s += Complex32::new(0.2 * ph.cos() as f32, 0.2 * ph.sin() as f32);
        }
        let plan = AudioPlan {
            mode: AnalogMode::Nbfm,
            sideband: None,
            channel_center_hz: FC + off,
            channel_bandwidth_hz: 20e3,
            noise_power: Some(1.0),
            agc: false,
            deemphasis_s: None,
        };
        let p = provenance();
        let mut d = AudioDemod::new(plan.clone(), AudioConfig::default(), FS, FC).unwrap();
        let mut idx = 0;
        for chunk in iq[..quiet].chunks(8192) {
            d.process(info(&p, idx as u64), chunk).unwrap();
            d.take_audio();
            idx += chunk.len();
        }
        assert_eq!(d.level_dbfs(), SILENCE_FLOOR_DBFS);
        // Reopen it directly (a fresh burst) instead of waiting on a real SNR crossing.
        let mut open_plan = plan;
        open_plan.noise_power = None;
        let mut d = AudioDemod::new(open_plan, AudioConfig::default(), FS, FC).unwrap();
        let mut idx = 0;
        let mut audio = Vec::new();
        for chunk in iq[quiet..].chunks(8192) {
            d.process(info(&p, idx as u64), chunk).unwrap();
            d.drain_audio_into(&mut audio);
            idx += chunk.len();
        }
        // The very first delivered chunk already settles near the tone's own level, not a floor
        // dragged in from a closed period elsewhere.
        assert!(
            d.level_dbfs() > SILENCE_FLOOR_DBFS + 40.0,
            "level_dbfs {} should already reflect the delivered burst",
            d.level_dbfs()
        );
    }

    /// T-1015: `level_dbfs`'s smoothing time constant must be `level_tau_s` regardless of channel
    /// count. The output array is interleaved `L, R` for stereo, so smoothing per **sample**
    /// instead of per audio **frame** halves the effective time constant on stereo. Feed the same
    /// mono WFM content (pilot off, so stereo output is L = R = M, T-874) through a mono and a
    /// stereo demodulator with an amplitude step partway through: with the fix both converge on
    /// the post-step level at the same rate; on the pre-fix per-sample smoothing the stereo one
    /// would already sit measurably closer to the final level shortly after the step.
    ///
    /// Red on the pre-fix code: shortly after the step, `level_dbfs` differs between the mono and
    /// stereo demodulators fed identical content, because the stereo smoother runs at twice the
    /// intended rate.
    #[test]
    fn stereo_level_dbfs_time_constant_matches_mono_not_half_of_it() {
        let off = -150e3;
        // A long quiet lead-in settles both demodulators' smoothers on a near-floor level, then a
        // short slice at full amplitude is fed in one go: enough audio to be well inside the
        // ~50 ms time constant's transient, not enough to let either fully settle, so the halved
        // stereo tau (a 2x faster approach) is visible in the reading and a matched tau is not.
        let quiet = (2.0 * FS) as usize;
        let step = (25e-3 * FS) as usize;
        let n = quiet + step;
        let mut iq = noise(n, 23, 0.002);
        let mut ph = 0.0f64;
        for (k, s) in iq.iter_mut().enumerate() {
            let t = k as f64 / FS;
            let amp = if k < quiet { 0.01 } else { 0.4 };
            let m = amp * (TAU * 1_000.0 * t).sin();
            ph = (ph + TAU * (off + 75e3 * m) / FS) % TAU;
            *s += Complex32::new(0.3 * ph.cos() as f32, 0.3 * ph.sin() as f32);
        }
        let mut mono = AudioDemod::new(wfm_plan(off), AudioConfig::default(), FS, FC).unwrap();
        let mut stereo = AudioDemod::new(wfm_plan(off), AudioConfig::default(), FS, FC)
            .unwrap()
            .with_stereo();
        assert_eq!(stereo.channels(), 2);
        let p = provenance();
        // Settle the lead-in identically for both, discarding audio.
        let mut idx = 0;
        for chunk in iq[..quiet].chunks(8192) {
            mono.process(info(&p, idx as u64), chunk).unwrap();
            stereo.process(info(&p, idx as u64), chunk).unwrap();
            mono.take_audio();
            stereo.take_audio();
            idx += chunk.len();
        }
        // Feed the stepped-up slice in one call to each so both see the same transient window.
        mono.process(info(&p, idx as u64), &iq[quiet..]).unwrap();
        stereo.process(info(&p, idx as u64), &iq[quiet..]).unwrap();
        assert!(
            (mono.level_dbfs() - stereo.level_dbfs()).abs() < 1.0,
            "identical mono content (pilot off) fed through an amplitude step must read the same \
             level_dbfs regardless of channel count, since level_tau_s is the same for both: mono \
             {} vs stereo {}",
            mono.level_dbfs(),
            stereo.level_dbfs()
        );
    }

    /// T-1015 (d): a known-level WFM **stereo** tone with AGC **on** — the acceptance's other gap
    /// (until now only NBFM with AGC off was covered by [`level_dbfs_tracks_the_delivered_audio_not_the_pre_demod_channel_power`]).
    /// `level_dbfs` must be the mean power over both interleaved channels of what
    /// [`AudioDemod::drain_audio_into`] actually yields.
    #[test]
    fn wfm_stereo_agc_level_dbfs_matches_the_mean_power_of_delivered_audio() {
        let off = -150e3;
        let mut plan = wfm_plan(off);
        plan.agc = true;
        let iq = stereo_station((1.5 * FS) as usize, off, |_| true);
        let d = AudioDemod::new(plan, AudioConfig::default(), FS, FC)
            .unwrap()
            .with_stereo();
        let (d, audio) = demod_all(d, &iq);
        assert_eq!(d.channels(), 2);
        assert!(d.agc_gain_db() >= 0.0);
        let tail = &audio[audio.len() / 2..];
        let ms = tail
            .iter()
            .map(|&y| f64::from(y) * f64::from(y))
            .sum::<f64>()
            / tail.len() as f64;
        let measured_dbfs = 10.0 * ms.max(1e-20).log10();
        let reported = d.level_dbfs();
        assert!(
            (reported - measured_dbfs).abs() < 0.5,
            "level_dbfs {reported} should be within 0.5 dB of the delivered stereo audio's own \
             mean power {measured_dbfs}"
        );
    }
}
