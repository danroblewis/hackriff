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
use serde_json::{Value, json};

/// Display and recording control over a running pipeline.
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
            capture: match s.capture {
                hk_pipeline::CaptureState::Running => hk_api::CaptureStatus::Running,
                hk_pipeline::CaptureState::Recovering => hk_api::CaptureStatus::Recovering,
                hk_pipeline::CaptureState::Ended => hk_api::CaptureStatus::Ended,
            },
            capture_note: s.capture_note,
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
            // T-508: capture recoveries are the pipeline moving the window by itself.
            pipeline_moves: s.stats["capture_recoveries"].as_u64().unwrap_or(0),
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
                run: r.run,
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

/// Region-analyze jobs (T-859, MAUTO M-8; ADR-0015 §5) over the pipeline's job manager. The
/// request arrives validated and resolved to a band and window; this only converts names.
pub struct PipelineAnalyze(pub Arc<hk_pipeline::synth::jobs::AnalyzeJobs>);

fn analyze_failure(f: hk_pipeline::synth::jobs::JobFailure) -> hk_api::AnalyzeFailure {
    hk_api::AnalyzeFailure {
        status: f.status,
        code: f.code.into(),
        message: f.message,
    }
}

fn analyze_invalid(message: &str) -> hk_api::AnalyzeFailure {
    hk_api::AnalyzeFailure {
        status: 400,
        code: "invalid".into(),
        message: message.into(),
    }
}

fn analyze_json(job: hk_pipeline::synth::jobs::AnalyzeJob) -> Value {
    serde_json::to_value(job).unwrap_or(Value::Null)
}

fn unix_s(s: f64) -> hk_model::Timestamp {
    hk_model::Timestamp::from_unix_nanos((s * 1e9).round() as i64)
}

impl hk_api::AnalyzeControl for PipelineAnalyze {
    fn start(&self, r: &hk_api::AnalyzeStart) -> Result<Value, hk_api::AnalyzeFailure> {
        use hk_pipeline::synth::jobs::{JobRequest, TemplateFilter, parse_name};
        let window = match (r.t_lo_s, r.t_hi_s) {
            (Some(a), Some(b)) => Some(hk_model::TimeRange::new(unix_s(a), unix_s(b))),
            _ => None,
        };
        let request = JobRequest {
            target: r.target.clone(),
            emitter_id: r.emitter_id,
            band: hk_model::FreqRange::new(r.f_lo_hz, r.f_hi_hz),
            window,
            window_explicit: r.window_explicit,
            profile: parse_name(r.profile).ok_or_else(|| analyze_invalid("unknown profile"))?,
            max_wall_s: r.max_wall_s,
            source: parse_name(r.source).ok_or_else(|| analyze_invalid("unknown source"))?,
            live_s: r.live_s,
            templates: TemplateFilter {
                only: r.templates.only.clone(),
                exclude: r.templates.exclude.clone(),
                off: r.templates.off,
            },
            attach: r.attach,
        };
        self.0
            .start(request)
            .map(analyze_json)
            .map_err(analyze_failure)
    }

    fn list(&self, state: Option<&str>) -> Result<Value, hk_api::AnalyzeFailure> {
        let state = match state {
            None => None,
            Some(s) => Some(
                hk_pipeline::synth::jobs::parse_name(s)
                    .ok_or_else(|| analyze_invalid("state names no job state"))?,
            ),
        };
        Ok(json!({ "jobs": self.0.list(state) }))
    }

    fn get(&self, id: &str) -> Result<Value, hk_api::AnalyzeFailure> {
        self.0.get(id).map(analyze_json).map_err(analyze_failure)
    }

    fn cancel(&self, id: &str) -> Result<(Value, bool), hk_api::AnalyzeFailure> {
        self.0
            .cancel(id)
            .map(|(job, forgotten)| (analyze_json(job), forgotten))
            .map_err(analyze_failure)
    }

    fn trace(
        &self,
        id: &str,
        q: &hk_api::AnalyzeTraceQuery,
    ) -> Result<Value, hk_api::AnalyzeFailure> {
        use hk_pipeline::synth::jobs::{TraceQuery, parse_name};
        let query = TraceQuery {
            stage: match &q.stage {
                None => None,
                Some(s) => Some(parse_name(s).ok_or_else(|| analyze_invalid("unknown stage"))?),
            },
            outcome: match &q.outcome {
                None => None,
                Some(o) => Some(parse_name(o).ok_or_else(|| analyze_invalid("unknown outcome"))?),
            },
            family: q.family.clone(),
            tried: q.tried,
            limit: q.limit,
        };
        self.0.trace(id, &query).map_err(analyze_failure)
    }
}

/// Labelled-capture dataset export (T-205) over the pipeline's IQ capture buffer (for snippet
/// export, reusing the same clip writer as `/api/iqbuffer/clip`) and its own repository handle,
/// opened fresh per call like [`hk_pipeline::iqbuffer::IqBufferService`]'s own `Recording` writer
/// does. Manifests are JSON files under `<data dir>/datasets/`, indexing the `Recording` and
/// `Annotation` rows `hk_store::dataset::export_dataset` wrote — no database row of their own.
pub struct PipelineDatasets {
    db_path: PathBuf,
    iq_buffer: Arc<hk_pipeline::iqbuffer::IqBufferService>,
    dir: PathBuf,
}

impl PipelineDatasets {
    /// `data_dir` is the run's data directory; manifests land in its `datasets/` subdirectory.
    pub fn new(
        db_path: PathBuf,
        iq_buffer: Arc<hk_pipeline::iqbuffer::IqBufferService>,
        data_dir: &std::path::Path,
    ) -> Self {
        Self {
            db_path,
            iq_buffer,
            dir: data_dir.join("datasets"),
        }
    }
}

/// [`hk_store::dataset::SnippetExporter`] over the pipeline's IQ capture buffer clip writer.
struct ClipSnippetExporter<'a>(&'a hk_pipeline::iqbuffer::IqBufferService);

impl hk_store::dataset::SnippetExporter for ClipSnippetExporter<'_> {
    fn export(
        &mut self,
        window: hk_model::TimeRange,
        band: Option<(f64, f64)>,
    ) -> Result<hk_store::dataset::ExportedSnippet, hk_store::dataset::DatasetError> {
        use hk_pipeline::iqbuffer::{ClipFailure, ClipRequest};
        use hk_store::iqbuffer::ClipRange;
        self.0
            .export_clip(&ClipRequest {
                range: ClipRange::Time {
                    t0_ns: window.start.as_unix_nanos(),
                    t1_ns: window.end.as_unix_nanos(),
                },
                band,
                label: None,
                run: None,
            })
            .map(|c| hk_store::dataset::ExportedSnippet {
                recording_id: c.id,
                sample_rate_hz: c.sample_rate_hz,
                center_hz: c.center_hz,
            })
            .map_err(|e: ClipFailure| {
                hk_store::dataset::DatasetError::SnippetUnavailable(e.to_string())
            })
    }
}

fn dataset_failed(message: impl Into<String>) -> hk_api::DatasetFailure {
    hk_api::DatasetFailure {
        status: 500,
        code: "failed".into(),
        message: message.into(),
    }
}

fn dataset_error(e: hk_store::dataset::DatasetError) -> hk_api::DatasetFailure {
    match e {
        hk_store::dataset::DatasetError::Invalid(m) => hk_api::DatasetFailure {
            status: 400,
            code: "invalid".into(),
            message: m,
        },
        hk_store::dataset::DatasetError::Repo(e) => dataset_failed(format!("dataset store: {e}")),
        hk_store::dataset::DatasetError::SnippetUnavailable(m) => dataset_failed(m),
    }
}

impl hk_api::DatasetControl for PipelineDatasets {
    fn export(
        &self,
        req: &hk_store::dataset::DatasetRequest,
    ) -> Result<Value, hk_api::DatasetFailure> {
        let mut repo = hk_model::Repository::open(&self.db_path)
            .map_err(|e| dataset_failed(format!("opening the dataset database: {e}")))?;
        let mut exporter = ClipSnippetExporter(&self.iq_buffer);
        let manifest = hk_store::dataset::export_dataset(&mut repo, &mut exporter, req)
            .map_err(dataset_error)?;
        std::fs::create_dir_all(&self.dir)
            .map_err(|e| dataset_failed(format!("creating the dataset directory: {e}")))?;
        let body = serde_json::to_value(&manifest).unwrap_or_default();
        let path = self.dir.join(format!("{}.json", manifest.id));
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&manifest).unwrap_or_default(),
        )
        .map_err(|e| dataset_failed(format!("writing the dataset manifest: {e}")))?;
        Ok(body)
    }

    fn list(&self) -> Result<Value, hk_api::DatasetFailure> {
        let mut rows: Vec<(String, Value)> = Vec::new();
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(json!({ "datasets": [] }));
            }
            Err(e) => return Err(dataset_failed(format!("listing datasets: {e}"))),
        };
        for entry in entries {
            let entry = entry.map_err(|e| dataset_failed(format!("listing datasets: {e}")))?;
            let Ok(text) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            let created = v["created_at"].as_str().unwrap_or_default().to_owned();
            rows.push((created, v));
        }
        rows.sort_by(|a, b| b.0.cmp(&a.0));
        Ok(json!({ "datasets": rows.into_iter().map(|(_, v)| v).collect::<Vec<_>>() }))
    }

    fn get(&self, id: &str) -> Result<Value, hk_api::DatasetFailure> {
        if id.contains(['/', '\\']) || id.is_empty() {
            return Err(hk_api::DatasetFailure {
                status: 404,
                code: "not_found".into(),
                message: "no such dataset".into(),
            });
        }
        let path = self.dir.join(format!("{id}.json"));
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|e| dataset_failed(format!("reading dataset manifest: {e}"))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(hk_api::DatasetFailure {
                status: 404,
                code: "not_found".into(),
                message: "no such dataset".into(),
            }),
            Err(e) => Err(dataset_failed(format!("reading dataset manifest: {e}"))),
        }
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

/// The persisted IQ recordings catalogue (T-469) over the run's database and data directory —
/// the other half of the audio horizon the IQ ring answers for.
///
/// The repository is opened **fresh per call**, like [`PipelineDatasets`] and
/// `hk_pipeline::iqbuffer::IqBufferService`'s own `Recording` writer: the listing is an
/// occasional read, and borrowing the run's handle would put a listing on the ingest path.
pub struct PipelineRecordings {
    db_path: PathBuf,
    data_dir: PathBuf,
}

impl PipelineRecordings {
    /// `data_dir` is the run's data directory: what every `Recording`'s `meta_uri`/`data_uri` is
    /// relative to, and where `hackriff.db` lives.
    pub fn new(db_path: PathBuf, data_dir: PathBuf) -> Self {
        Self { db_path, data_dir }
    }
}

impl hk_api::RecordingCatalog for PipelineRecordings {
    fn list(
        &self,
        query: &hk_store::recordings::RecordingsQuery,
    ) -> Result<hk_store::recordings::RecordingsCatalogue, hk_api::RecordingsFailure> {
        let fail = |status: u16, code: &str, message: String| hk_api::RecordingsFailure {
            status,
            code: code.into(),
            message,
        };
        let repo = hk_model::Repository::open(&self.db_path).map_err(|e| {
            fail(
                500,
                "failed",
                format!("opening the recordings database: {e}"),
            )
        })?;
        hk_store::recordings::catalogue(&repo, &self.data_dir, query)
            .map_err(|e| fail(500, "failed", format!("listing recordings: {e}")))
    }
}

/// The one playhead of historical playback (T-463) behind `GET/POST /api/playback`. The same
/// [`hk_pipeline::playback::PlaybackService`] is the `playback` on-demand opener, so the audio a
/// client opens follows exactly the playhead this route moves.
pub struct PipelinePlayback(pub Arc<hk_pipeline::playback::PlaybackService>);

impl hk_api::PlaybackControl for PipelinePlayback {
    fn state(&self) -> Value {
        self.0.state_json()
    }

    fn apply(&self, change: &hk_api::PlaybackChange) -> Result<Value, hk_api::PlaybackFailure> {
        let fail = |status: u16, code: &str, e: hk_pipeline::playback::PlayheadError| {
            hk_api::PlaybackFailure {
                status,
                code: code.into(),
                message: e.0,
            }
        };
        let head = self.0.playhead();
        if let Some(speed) = change.speed {
            head.set_speed(speed).map_err(|e| fail(400, "invalid", e))?;
        }
        if let Some(t) = change.t_ns {
            head.seek(t).map_err(|e| fail(400, "invalid", e))?;
        }
        match change.playing {
            Some(true) => {
                head.play().map_err(|e| fail(409, "no_position", e))?;
            }
            Some(false) => {
                head.pause();
            }
            None => {}
        }
        Ok(self.0.state_json())
    }
}

/// The C38 stage (T-844) for `/api/ml/*`: the model registry, the `(model, consumer)` modes and
/// the durable shadow log, all owned by [`hk_pipeline::ml::MlStage`]. This adapter only maps
/// shapes; it adds no path by which a model's output reaches a decision.
pub struct PipelineMl(pub Arc<hk_pipeline::ml::MlStage>);

fn ml_failure(f: hk_pipeline::ml::MlStageFailure) -> hk_api::MlFailure {
    hk_api::MlFailure {
        status: f.status,
        code: f.code.to_owned(),
        message: f.message,
    }
}

impl hk_api::MlControl for PipelineMl {
    fn models(&self) -> Result<Value, hk_api::MlFailure> {
        Ok(self.0.models_json())
    }

    fn set_mode(&self, change: &hk_api::MlModeChange) -> Result<Value, hk_api::MlFailure> {
        use hk_pipeline::ml::MlMode;
        let mode = match change.mode {
            hk_api::MlModeWanted::Off => MlMode::Off,
            hk_api::MlModeWanted::Shadow => MlMode::Shadow,
            hk_api::MlModeWanted::Active => MlMode::Active,
        };
        self.0
            .set_mode(&hk_pipeline::ml::ModeRequest {
                id: change.id.clone(),
                version: change.version.clone(),
                consumer: change.consumer.clone(),
                mode,
                force: change.force,
            })
            .map_err(ml_failure)
    }

    fn shadow(&self, q: &hk_store::ml::ShadowQuery) -> Result<Value, hk_api::MlFailure> {
        self.0.shadow_json(q).map_err(ml_failure)
    }
}
