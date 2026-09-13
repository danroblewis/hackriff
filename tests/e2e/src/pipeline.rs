//! Stage runner stub: capability stages plug in here as they are built.
//!
//! The output records below are **provisional** stand-ins for the docs/07 objects (Detection §2.9,
//! Decode §2.15, NoiseFloor via SpectrumTile §2.5, Anomaly §2.18, Explanation §2.19). When T-002
//! lands the hk-model types, stages should emit those and these structs should become
//! conversions or be removed; the assertion helpers only need time/frequency boxes and numbers.
//!
//! Stages never see truth: [`Pipeline::run`] hands them a copy of the metadata with the
//! annotations removed.

use std::collections::BTreeMap;

use hk_model::sigmf::SigmfMeta;
use serde_json::Value;

use crate::fixture::Fixture;
use crate::samples::{Cf32, SampleError};

/// Error type stages return.
pub type StageError = Box<dyn std::error::Error + Send + Sync>;

/// What a stage sees.
pub struct StageInput<'a> {
    /// Recording metadata **without annotations** (no truth leakage).
    pub meta: &'a SigmfMeta,
    /// All samples of the recording (T-003 seam: will become a replay source).
    pub samples: &'a [Cf32],
    /// Sample rate, Hz.
    pub sample_rate: f64,
}

/// A detection box (provisional; docs/07 §2.9 Detection).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DetectionBox {
    /// Start, seconds from the first sample.
    pub t_start_s: f64,
    /// End, seconds.
    pub t_end_s: f64,
    /// Lower edge, absolute Hz.
    pub f_lo_hz: f64,
    /// Upper edge, absolute Hz.
    pub f_hi_hz: f64,
    /// Peak SNR, dB.
    pub snr_db: Option<f64>,
    /// Trust flags, e.g. `clipped`, `spur_candidate`.
    pub flags: Vec<String>,
}

impl DetectionBox {
    /// Centre frequency, Hz.
    pub fn center_hz(&self) -> f64 {
        (self.f_lo_hz + self.f_hi_hz) / 2.0
    }

    /// Centre time, seconds.
    pub fn t_center_s(&self) -> f64 {
        (self.t_start_s + self.t_end_s) / 2.0
    }
}

/// A named parameter estimate, optionally tied to a detection (index into `detections`).
#[derive(Clone, Debug, PartialEq)]
pub struct ParameterEstimate {
    /// Index into [`PipelineOutputs::detections`].
    pub detection: Option<usize>,
    /// Name matching the truth field, e.g. `symbol_rate_bd`.
    pub name: String,
    /// Estimated value.
    pub value: f64,
}

/// A decoded message (provisional; docs/07 §2.15 Decode).
#[derive(Clone, Debug, PartialEq)]
pub struct DecodedMessage {
    /// Time of the message start, seconds.
    pub t_s: f64,
    /// Protocol or framing, e.g. `adsb-df17`.
    pub kind: String,
    /// Decoded identity, e.g. an ICAO hex or RDS PI.
    pub identity: Option<String>,
    /// Whether the CRC validated.
    pub crc_ok: bool,
    /// Decoded metadata (mirrors the T-002 `Decode.metadata`; there is no `fields`).
    pub metadata: serde_json::Map<String, Value>,
    /// Decoded content, when the legal guardrails allow keeping it (T-002 `Decode.content`).
    pub content: Option<Value>,
}

/// A noise-floor estimate over a time/frequency region.
#[derive(Clone, Debug, PartialEq)]
pub struct FloorEstimate {
    /// Start, seconds.
    pub t_start_s: f64,
    /// End, seconds.
    pub t_end_s: f64,
    /// Lower edge, Hz.
    pub f_lo_hz: f64,
    /// Upper edge, Hz.
    pub f_hi_hz: f64,
    /// Integrated floor over the region, dBFS.
    pub dbfs: f64,
    /// Calibrated floor, dBm, when calibration is known.
    pub dbm: Option<f64>,
}

/// Everything the stages produced.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PipelineOutputs {
    /// Detections (C09).
    pub detections: Vec<DetectionBox>,
    /// Parameter estimates (C11/C13/C14).
    pub parameters: Vec<ParameterEstimate>,
    /// Decodes (C21/C22).
    pub decodes: Vec<DecodedMessage>,
    /// Noise-floor estimates (C08/C33).
    pub floors: Vec<FloorEstimate>,
    /// Not-yet-typed records by kind: `anomaly`, `emitter`, `explanation`, … (docs/07 JSON form).
    pub records: BTreeMap<String, Vec<Value>>,
    /// Names of the stages that ran, in order.
    pub stages_run: Vec<String>,
}

/// One capability stage.
pub trait Stage {
    /// Stage name for logs and errors.
    fn name(&self) -> &str;
    /// Reads the input and the outputs of earlier stages; appends its own outputs.
    fn run(&mut self, input: &StageInput<'_>, out: &mut PipelineOutputs) -> Result<(), StageError>;
}

/// Errors running a pipeline.
#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    /// Samples could not be read.
    #[error(transparent)]
    Samples(#[from] SampleError),
    /// A stage failed.
    #[error("stage {stage}: {source}")]
    Stage {
        /// Stage name.
        stage: String,
        /// Underlying error.
        #[source]
        source: StageError,
    },
}

/// An ordered list of stages.
#[derive(Default)]
pub struct Pipeline {
    stages: Vec<Box<dyn Stage>>,
}

impl Pipeline {
    /// An empty pipeline.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a stage (builder form).
    pub fn with(mut self, stage: impl Stage + 'static) -> Self {
        self.stages.push(Box::new(stage));
        self
    }

    /// Replays a fixture through every stage in order.
    pub fn run(&mut self, fixture: &Fixture) -> Result<PipelineOutputs, PipelineError> {
        let samples = fixture.samples()?;
        let mut meta = fixture.meta.clone();
        meta.annotations.clear();
        let input = StageInput {
            meta: &meta,
            samples: &samples,
            sample_rate: fixture.sample_rate,
        };
        let mut out = PipelineOutputs::default();
        for stage in &mut self.stages {
            stage
                .run(&input, &mut out)
                .map_err(|source| PipelineError::Stage {
                    stage: stage.name().to_owned(),
                    source,
                })?;
            out.stages_run.push(stage.name().to_owned());
        }
        Ok(out)
    }
}
