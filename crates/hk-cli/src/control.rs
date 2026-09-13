//! Adapters from the pipeline's control plane (`hk_pipeline::PipelineController`) to the control
//! API's traits (`hk_api::RunControl`, `hk_api::WindowRetuner`), T-050. hk-api does not depend on
//! hk-pipeline; the binaries' composition joins them here.

use hk_api::{
    DisplayState, DisplayUpdate, LiveControlError, RecordingState, RunControl, RunState,
    WindowRetuner,
};
use hk_model::ContentClass;
use hk_pipeline::{
    ControlFailure, DisplayPatch, DisplaySettings, PipelineController, RecordingStatus,
};

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

    fn set_display(&self, update: &DisplayUpdate) -> Result<DisplayState, LiveControlError> {
        self.0
            .set_display(&DisplayPatch {
                fft_size: update.fft_size,
                averaging: update.averaging,
                rows_per_s: update.rows_per_s,
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
}
