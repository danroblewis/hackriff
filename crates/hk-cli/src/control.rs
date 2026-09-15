//! Adapters from the pipeline's control plane (`hk_pipeline::PipelineController`) to the control
//! API's traits (`hk_api::RunControl`, `hk_api::WindowRetuner`), T-050. hk-api does not depend on
//! hk-pipeline; the binaries' composition joins them here.

use std::path::PathBuf;
use std::sync::Arc;

use hk_api::live_control::AppliedWindow;
use hk_api::{
    DisplayLimits, DisplayState, DisplayUpdate, LiveControlError, OutputControl, OutputFailure,
    OutputStart, RecordingState, RunControl, RunState, WindowRetuner,
};
use hk_dsp::window::WindowKind;
use hk_model::ContentClass;
use hk_pipeline::config::{
    DISPLAY_AVERAGING_MAX, DISPLAY_FFT_MAX, DISPLAY_FFT_MIN, DISPLAY_ROWS_MAX, DISPLAY_ROWS_MIN,
};
use hk_pipeline::{
    ControlFailure, DisplayPatch, DisplaySettings, OutputError, OutputKind, OutputRecorders,
    OutputRequest, OutputTarget, PipelineController, RecordingStatus,
};
use serde_json::Value;

/// Display, pause and recording control over a running pipeline.
pub struct PipelineRunControl(pub PipelineController);

/// Window changes (centre, rate) through the pipeline: class re-derivation and re-plumbing.
pub struct PipelineRetuner(pub PipelineController);

/// The API error for a pipeline control failure.
pub fn api_error(e: ControlFailure) -> LiveControlError {
    match e {
        ControlFailure::Invalid(m) => LiveControlError::Invalid(m),
        ControlFailure::NotLive(m) => LiveControlError::NotLive(m),
        ControlFailure::Refused(m) => LiveControlError::Refused(m),
        ControlFailure::Conflict(m) => LiveControlError::Conflict(m),
        ControlFailure::Timeout(m) => LiveControlError::Timeout(m),
        ControlFailure::Source(e) => LiveControlError::Source(e),
        ControlFailure::Finished(m) => LiveControlError::Finished(m),
        ControlFailure::Failed(m) => LiveControlError::Failed(m),
    }
}

fn display(d: DisplaySettings) -> DisplayState {
    DisplayState {
        fft_size: d.fft_size,
        averaging: d.averaging,
        rows_per_s: d.rows_per_s,
        paused: d.paused,
        window: d.window.name().to_owned(),
    }
}

/// [`hk_pipeline::config`]'s `DISPLAY_*` bounds, translated for the control API (T-067).
fn pipeline_display_limits() -> DisplayLimits {
    DisplayLimits {
        fft_size_min: DISPLAY_FFT_MIN,
        fft_size_max: DISPLAY_FFT_MAX,
        averaging_max: DISPLAY_AVERAGING_MAX,
        rows_per_s_min: DISPLAY_ROWS_MIN,
        rows_per_s_max: DISPLAY_ROWS_MAX,
        windows: WindowKind::ALL
            .iter()
            .map(|w| w.name().to_owned())
            .collect(),
    }
}

fn recording(r: RecordingStatus) -> RecordingState {
    RecordingState {
        active: r.active,
        id: r.id.map(|id| id.to_string()),
        label: r.label,
        center_hz: r.center_hz,
        sample_rate_hz: r.sample_rate_hz,
        samples: r.samples,
        lost_samples: r.lost_samples,
        max_s: r.max_s,
        stored: r.stored,
        ended: r.ended,
    }
}

impl RunControl for PipelineRunControl {
    fn state(&self) -> RunState {
        let s = self.0.status();
        RunState {
            live: s.live,
            content_class: s.content_class,
            center_hz: s.center_hz,
            sample_rate_hz: s.sample_rate_hz,
            segment: s.segment,
            replumbing: s.replumbing,
            finished: s.finished,
            display: display(s.display),
            recording: recording(s.recording),
        }
    }

    fn display_limits(&self) -> DisplayLimits {
        pipeline_display_limits()
    }

    fn set_display(&self, update: &DisplayUpdate) -> Result<DisplayState, LiveControlError> {
        let window = match &update.window {
            None => None,
            Some(s) => Some(WindowKind::from_name(s).ok_or_else(|| {
                LiveControlError::Invalid(format!(
                    "window {s:?} must be one of {}",
                    WindowKind::ALL
                        .iter()
                        .map(|w| w.name())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })?),
        };
        self.0
            .set_display(&DisplayPatch {
                fft_size: update.fft_size,
                averaging: update.averaging,
                rows_per_s: update.rows_per_s,
                window,
            })
            .map(display)
            .map_err(api_error)
    }

    fn set_paused(&self, paused: bool) -> Result<DisplayState, LiveControlError> {
        Ok(display(self.0.set_paused(paused)))
    }

    fn start_recording(
        &self,
        label: Option<&str>,
        max_s: Option<f64>,
    ) -> Result<RecordingState, LiveControlError> {
        self.0
            .start_recording(label, max_s)
            .map(recording)
            .map_err(api_error)
    }

    fn stop_recording(&self) -> Result<RecordingState, LiveControlError> {
        self.0.stop_recording().map(recording).map_err(api_error)
    }
}

impl WindowRetuner for PipelineRetuner {
    fn retune(
        &self,
        center_hz: f64,
        sample_rate_hz: f64,
    ) -> Result<ContentClass, LiveControlError> {
        self.0
            .retune(center_hz, sample_rate_hz)
            .map(|o| o.content_class)
            .map_err(api_error)
    }

    fn applied_window(&self) -> Option<AppliedWindow> {
        let s = self.0.status();
        Some(AppliedWindow {
            center_hz: s.center_hz,
            sample_rate_hz: s.sample_rate_hz,
            settling: s.replumbing,
        })
    }
}

/// The rolling IQ capture buffer (T-157) over the pipeline's service.
pub struct PipelineIqBuffer(pub Arc<hk_pipeline::iqbuffer::IqBufferService>);

impl hk_api::IqBufferControl for PipelineIqBuffer {
    fn status(&self, q: &hk_api::IqBufferQuery) -> Value {
        serde_json::to_value(self.0.status(q.t0, q.t1, q.limit)).unwrap_or_default()
    }

    fn clip(&self, r: &hk_api::ClipStart) -> Result<Value, hk_api::IqBufferFailure> {
        use hk_pipeline::iqbuffer::{ClipFailure, ClipRequest};
        self.0
            .export_clip(&ClipRequest {
                range: r.range,
                band: r.band,
                label: r.label.clone(),
            })
            .map(|c| serde_json::to_value(c).unwrap_or_default())
            .map_err(|e| {
                let (status, code) = match &e {
                    ClipFailure::Unavailable(_) => (503, "unavailable"),
                    ClipFailure::Invalid(_) | ClipFailure::TooLarge(_) => (400, "invalid"),
                    ClipFailure::NotFound(_) => (404, "not_found"),
                    ClipFailure::Conflict(_) => (409, "conflict"),
                    ClipFailure::NoSpace(_) => (507, "insufficient_storage"),
                    ClipFailure::Failed(_) => (500, "failed"),
                };
                hk_api::IqBufferFailure {
                    status,
                    code: code.into(),
                    message: e.to_string(),
                }
            })
    }
}

/// Output recordings (T-061) over the pipeline's recorders.
pub struct PipelineOutputs(pub Arc<OutputRecorders>);

fn output_failure(e: OutputError) -> OutputFailure {
    OutputFailure {
        status: e.http_status(),
        code: e.code().to_owned(),
        message: e.to_string(),
    }
}

fn invalid(message: String) -> OutputFailure {
    output_failure(OutputError::Invalid(message))
}

impl OutputControl for PipelineOutputs {
    fn start(&self, request: &OutputStart) -> Result<Value, OutputFailure> {
        let target = match &request.target {
            hk_api::OutputTarget::Selection(id) => OutputTarget::Selection(
                id.parse()
                    .map_err(|_| invalid(format!("selection_id {id:?} is not an id")))?,
            ),
            hk_api::OutputTarget::Emitter(id) => OutputTarget::Emitter(
                id.parse()
                    .map_err(|_| invalid(format!("emitter_id {id:?} is not an id")))?,
            ),
            hk_api::OutputTarget::Band { f_lo_hz, f_hi_hz } => OutputTarget::Band {
                f_lo_hz: *f_lo_hz,
                f_hi_hz: *f_hi_hz,
            },
        };
        let kinds = request
            .kinds
            .iter()
            .map(|k| OutputKind::parse(k).ok_or_else(|| invalid(format!("unknown kind {k:?}"))))
            .collect::<Result<Vec<_>, _>>()?;
        let status = self
            .0
            .start(&OutputRequest {
                target,
                kinds,
                max_s: request.max_s,
                max_bytes: request.max_bytes,
            })
            .map_err(output_failure)?;
        Ok(serde_json::to_value(status).unwrap_or_default())
    }

    fn stop(&self, id: &str) -> Result<Value, OutputFailure> {
        let status = self.0.stop(id).map_err(output_failure)?;
        Ok(serde_json::to_value(status).unwrap_or_default())
    }

    fn list(&self) -> Vec<Value> {
        self.0
            .list()
            .into_iter()
            .map(|s| serde_json::to_value(s).unwrap_or_default())
            .collect()
    }

    fn file(&self, id: &str, name: &str) -> Result<PathBuf, OutputFailure> {
        self.0.file(id, name).map_err(output_failure)
    }
}
