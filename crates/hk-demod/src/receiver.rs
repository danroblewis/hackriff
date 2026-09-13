//! The analog receiver: one channel request over source IQ → C13 estimate → auto mode →
//! demodulation → an [`AnalogSession`] ready to be written as data-model records
//! ([`crate::record`]).
//!
//! The mode is decided on the first `mode_window_s` of the request; WFM is then demodulated over
//! the whole request through the hk-dsp DDC at [`MPX_RATE_HZ`], centred on the box plus the C13
//! CFO. NBFM/AM/SSB/CW are selected and recorded but not yet demodulated to audio.

use std::fmt;

use hk_core::Discontinuity;
use hk_dsp::{Ddc, DdcError, DdcSpec, DesignError, InputInfo, IqSample};
use hk_estimate::{
    EstimateError, EstimatorConfig, Hints, ParamEstimator, ParameterSet, SnippetConfig,
    SnippetExtractor, SnippetRequest,
};
use hk_model::{EstimatedParams, SampleTime, TimeRange, Timestamp};
use serde::{Deserialize, Serialize};

use crate::DEMOD_VERSION;
use crate::mode::{AnalogMode, ModeConfig, ModeDecision, ModeSelector};
use crate::rds::RdsReport;
use crate::wfm::{MPX_RATE_HZ, WfmConfig, WfmDemod, WfmReport};

/// Receiver settings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReceiverConfig {
    /// Leading part of the request used for estimation and mode selection, s.
    pub mode_window_s: f64,
    /// Snippet extraction (C13).
    pub snippet: SnippetConfig,
    /// Parameter estimation (C13).
    pub estimator: EstimatorConfig,
    /// Mode selection.
    pub mode: ModeConfig,
    /// WFM chain.
    pub wfm: WfmConfig,
    /// Flat DDC bandwidth for a WFM channel, Hz.
    pub wfm_channel_bandwidth_hz: f64,
    /// Largest C13 CFO applied to the DDC centre, Hz.
    pub max_cfo_correction_hz: f64,
    /// Source samples per DDC call.
    pub chunk_samples: usize,
}

impl Default for ReceiverConfig {
    fn default() -> Self {
        Self {
            mode_window_s: 0.5,
            snippet: SnippetConfig::default(),
            estimator: EstimatorConfig::default(),
            mode: ModeConfig::default(),
            wfm: WfmConfig::default(),
            wfm_channel_bandwidth_hz: 200e3,
            max_cfo_correction_hz: 20e3,
            chunk_samples: 1 << 18,
        }
    }
}

/// Why a channel could not be demodulated.
#[derive(Debug)]
pub enum DemodError {
    /// The request lies outside the supplied samples.
    InvalidRequest(String),
    /// Snippet extraction failed.
    Estimate(EstimateError),
    /// The DDC could not be built or run.
    Ddc(DdcError),
    /// A filter could not be designed.
    Design(DesignError),
}

impl fmt::Display for DemodError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DemodError::InvalidRequest(why) => write!(f, "invalid request: {why}"),
            DemodError::Estimate(e) => write!(f, "estimation: {e:?}"),
            DemodError::Ddc(e) => write!(f, "DDC: {e:?}"),
            DemodError::Design(e) => write!(f, "filter design: {e:?}"),
        }
    }
}

impl std::error::Error for DemodError {}

impl From<EstimateError> for DemodError {
    fn from(e: EstimateError) -> Self {
        DemodError::Estimate(e)
    }
}

impl From<DdcError> for DemodError {
    fn from(e: DdcError) -> Self {
        DemodError::Ddc(e)
    }
}

impl From<DesignError> for DemodError {
    fn from(e: DesignError) -> Self {
        DemodError::Design(e)
    }
}

/// MPX position → source sample index.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct MpxTimeMap {
    /// Source index of MPX sample 0.
    pub source_index0: f64,
    /// Source samples per MPX sample.
    pub source_per_output: f64,
}

/// Demodulated audio held in memory (the stream/file output is the caller's; see
/// [`AudioBuffer::to_wav_bytes`]).
#[derive(Clone, Debug, PartialEq)]
pub struct AudioBuffer {
    /// Sample rate, Hz.
    pub sample_rate_hz: f64,
    /// Mono samples, ±1 full scale.
    pub samples: Vec<f32>,
    /// Source index of audio sample 0.
    pub source_index0: f64,
    /// Source samples per audio sample.
    pub source_per_sample: f64,
}

impl AudioBuffer {
    /// A minimal 16-bit PCM mono WAV file.
    pub fn to_wav_bytes(&self) -> Vec<u8> {
        let rate = self.sample_rate_hz.round() as u32;
        let data_len = (self.samples.len() * 2) as u32;
        let mut out = Vec::with_capacity(44 + data_len as usize);
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes()); // PCM
        out.extend_from_slice(&1u16.to_le_bytes()); // mono
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * 2).to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        for &s in &self.samples {
            let v = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
            out.extend_from_slice(&v.to_le_bytes());
        }
        out
    }
}

/// One demodulated channel.
#[derive(Clone, Debug)]
pub struct AnalogSession {
    /// Demodulator id and version.
    pub demod_version: String,
    /// The channel request.
    pub request: SnippetRequest,
    /// Source sample rate, Hz.
    pub source_rate_hz: f64,
    /// Tuned centre of the source, Hz.
    pub tuned_center_hz: f64,
    /// Timing anchor of the source samples.
    pub anchor: SampleTime,
    /// DDC centre relative to the tuned centre, Hz (box centre + CFO).
    pub channel_offset_hz: f64,
    /// RF centre of the emission (receiver frame), Hz.
    pub rf_center_hz: f64,
    /// C13 estimate over the mode window.
    pub params: ParameterSet,
    /// Mode decision.
    pub mode: ModeDecision,
    /// WFM results, when WFM was selected.
    pub wfm: Option<WfmReport>,
    /// Audio, when demodulated.
    pub audio: Option<AudioBuffer>,
    /// MPX time map, when WFM was demodulated.
    pub mpx_time: Option<MpxTimeMap>,
}

impl AnalogSession {
    /// RDS results, if any.
    pub fn rds(&self) -> Option<&RdsReport> {
        self.wfm.as_ref().and_then(|w| w.rds.as_ref())
    }

    /// Host time of a (fractional) source index.
    pub fn timestamp_of_source(&self, index: f64) -> Timestamp {
        let dt = (index - self.anchor.sample_index as f64) / self.source_rate_hz;
        self.anchor
            .host_time
            .saturating_add_nanos((dt * 1e9).round() as i64)
    }

    /// Host time of an MPX position (e.g. an RDS frame's `position`).
    pub fn timestamp_of_mpx(&self, position: f64) -> Option<Timestamp> {
        self.mpx_time
            .map(|m| self.timestamp_of_source(m.source_index0 + position * m.source_per_output))
    }

    /// The request's time span.
    pub fn time_range(&self) -> TimeRange {
        TimeRange::new(
            self.timestamp_of_source(self.request.start_index as f64),
            self.timestamp_of_source(self.request.end_index as f64),
        )
    }

    /// Data-model parameters: CFO and bandwidth from C13, deviation from the demodulator.
    pub fn estimated_params(&self) -> EstimatedParams {
        let mut p = self.params.estimated_params();
        if let Some(w) = &self.wfm {
            p.deviation_hz = w.peak_deviation_hz;
            p.pilot_hz = w.pilot.frequency_hz;
            p.cfo_hz =
                Some(self.channel_offset_hz - self.request.center_offset_hz + w.carrier_offset_hz);
        }
        p
    }
}

/// The analog auto-mode receiver.
pub struct AnalogReceiver {
    config: ReceiverConfig,
    extractor: SnippetExtractor,
    estimator: ParamEstimator,
    selector: ModeSelector,
}

impl Default for AnalogReceiver {
    fn default() -> Self {
        Self::new(ReceiverConfig::default())
    }
}

impl AnalogReceiver {
    /// A receiver.
    pub fn new(config: ReceiverConfig) -> Self {
        Self {
            extractor: SnippetExtractor::new(config.snippet),
            estimator: ParamEstimator::new(config.estimator.clone()),
            selector: ModeSelector::new(config.mode),
            config,
        }
    }

    /// Estimates, selects the mode and demodulates `request` over `iq` (whose first sample is
    /// `info.time.sample_index`).
    pub fn run<T: IqSample>(
        &mut self,
        info: InputInfo<'_>,
        iq: &[T],
        request: &SnippetRequest,
    ) -> Result<AnalogSession, DemodError> {
        let fs = info.provenance.tune.sample_rate_hz;
        let base = info.time.sample_index;
        let end = base + iq.len() as u64;
        if request.start_index < base
            || request.end_index > end
            || request.end_index <= request.start_index
        {
            return Err(DemodError::InvalidRequest(format!(
                "samples {}..{} not inside {base}..{end}",
                request.start_index, request.end_index
            )));
        }
        let window = (self.config.mode_window_s * fs).round() as u64;
        let mode_req = SnippetRequest {
            end_index: request.end_index.min(request.start_index + window.max(1)),
            ..*request
        };
        let snip = self.extractor.extract(info, iq, &mode_req)?;
        let params = self.estimator.estimate(&snip, &Hints::default());
        let mode = self.selector.select(&snip, &params);
        let cfo = params
            .cfo_hz
            .value()
            .filter(|c| c.abs() <= self.config.max_cfo_correction_hz)
            .unwrap_or(0.0);
        let channel_offset_hz = request.center_offset_hz + cfo;
        let rf_center_hz = params
            .rf_center_hz
            .value()
            .unwrap_or_else(|| snip.rf_center_hz());

        let mut session = AnalogSession {
            demod_version: DEMOD_VERSION.into(),
            request: *request,
            source_rate_hz: fs,
            tuned_center_hz: info.provenance.tune.center_hz,
            anchor: info.time,
            channel_offset_hz,
            rf_center_hz,
            params,
            mode,
            wfm: None,
            audio: None,
            mpx_time: None,
        };
        if session.mode.mode == AnalogMode::Wfm {
            self.run_wfm(info, iq, &mut session)?;
        }
        Ok(session)
    }

    fn run_wfm<T: IqSample>(
        &self,
        info: InputInfo<'_>,
        iq: &[T],
        session: &mut AnalogSession,
    ) -> Result<(), DemodError> {
        let fs = session.source_rate_hz;
        let base = info.time.sample_index;
        let mut ddc = Ddc::new(
            DdcSpec::new(
                session.channel_offset_hz,
                self.config.wfm_channel_bandwidth_hz,
            )
            .with_output_rate(MPX_RATE_HZ),
            fs,
        )?;
        let mut wfm = WfmDemod::new(self.config.wfm, MPX_RATE_HZ)?;
        let (a, b) = (
            (session.request.start_index - base) as usize,
            (session.request.end_index - base) as usize,
        );
        let mut time_map = None;
        let mut audio = Vec::new();
        let mut idx = session.request.start_index;
        for (k, chunk) in iq[a..b]
            .chunks(self.config.chunk_samples.max(1))
            .enumerate()
        {
            let ci = InputInfo {
                time: SampleTime {
                    sample_index: idx,
                    host_time: info.time.time_of(idx, fs),
                },
                discontinuity: if k == 0 {
                    Discontinuity::STREAM_START
                } else {
                    Discontinuity::NONE
                },
                dropped_before: 0,
                provenance: info.provenance,
            };
            let block = ddc.process(ci, chunk)?;
            if time_map.is_none() && !block.samples.is_empty() {
                time_map = Some(MpxTimeMap {
                    source_index0: block.header.time.source_index,
                    source_per_output: block.header.time.source_per_output,
                });
            }
            wfm.process(block.samples);
            audio.extend(wfm.take_audio());
            idx += chunk.len() as u64;
        }
        if let Some(m) = time_map {
            session.audio = Some(AudioBuffer {
                sample_rate_hz: wfm.audio_rate_hz(),
                samples: audio,
                source_index0: m.source_index0 + wfm.audio_mpx_offset() * m.source_per_output,
                source_per_sample: m.source_per_output * MPX_RATE_HZ / wfm.audio_rate_hz(),
            });
        }
        session.mpx_time = time_map;
        session.wfm = Some(wfm.report());
        Ok(())
    }
}
