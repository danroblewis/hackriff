//! Manual IQ recording (T-050 record start/stop): the tuned window from "now" until stop, a
//! maximum duration, or the end of the run segment, streamed to `recordings/<id>.sigmf-data`
//! (ci8), then a `Recording` row (trigger `manual`, retention `pinned`).
//!
//! - **Legal gating.** Raw IQ carries the window's content, so a recording starts only under a
//!   segment class that permits content, and every chunk's own window is re-checked
//!   ([`crate::class::window_class`]): a chunk whose window forbids content ends the recording
//!   before it is written. The repository refuses a gated Recording row as well.
//! - **Bounded.** At most [`RECORDING_MAX_S`] seconds and [`RECORDING_MAX_BYTES`] of data.
//! - **Honest data.** One SigMF capture per provenance run; a lost stretch (ring overrun) starts
//!   a new capture and is counted in `lost_samples` (the data is never spliced silently).
//! - A re-plumb or the end of the run ends the recording (the segment's ring closes).

use std::fs::File;
use std::io::{BufWriter, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::Context as _;
use hk_core::ReadOutcome;
use hk_model::sigmf::{Capture, Datatype, SigmfMeta};
use hk_model::{
    ProvenanceId, Recording, RecordingId, RecordingKind, RecordingTrigger, RetentionClass,
    TimeRange, Timestamp,
};
use num_complex::Complex;
use serde::Serialize;

use crate::chains::record::iso8601;
use crate::class::window_class;
use crate::run::{ControlFailure, Shared};
use crate::stats::inc;

/// Default recording length, s.
pub const RECORDING_DEFAULT_S: f64 = 60.0;
/// Longest recording, s.
pub const RECORDING_MAX_S: f64 = 600.0;
/// Largest recording data file, bytes.
pub const RECORDING_MAX_BYTES: u64 = 4 << 30;
/// Longest recording label, characters.
pub const RECORDING_LABEL_MAX: usize = 120;

/// A manual recording's state.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct RecordingStatus {
    /// Still recording.
    pub active: bool,
    /// Recording id (the `Recording` row when stored).
    pub id: Option<RecordingId>,
    /// User label.
    pub label: Option<String>,
    /// Centre of the first capture, Hz.
    pub center_hz: Option<f64>,
    /// Sample rate, Hz.
    pub sample_rate_hz: Option<f64>,
    /// Samples written.
    pub samples: u64,
    /// Samples lost to ring overruns while recording.
    pub lost_samples: u64,
    /// Requested maximum, s.
    pub max_s: f64,
    /// Time of the first sample.
    pub started: Option<Timestamp>,
    /// The Recording row was stored.
    pub stored: bool,
    /// Why it ended (`None` while active).
    pub ended: Option<String>,
}

/// A running manual recording.
pub(crate) struct ManualRecorder {
    stop: Arc<AtomicBool>,
    status: Arc<Mutex<RecordingStatus>>,
    join: Option<JoinHandle<()>>,
}

impl ManualRecorder {
    /// Starts recording the segment's window now. The caller has checked the segment class.
    pub fn start(
        shared: Arc<Shared>,
        label: Option<&str>,
        max_s: Option<f64>,
    ) -> Result<Self, ControlFailure> {
        let label = match label.map(str::trim) {
            None | Some("") => None,
            Some(l)
                if l.chars().count() > RECORDING_LABEL_MAX || l.chars().any(char::is_control) =>
            {
                return Err(ControlFailure::Invalid(format!(
                    "label must be at most {RECORDING_LABEL_MAX} printable characters"
                )));
            }
            Some(l) => Some(l.to_owned()),
        };
        let max_s = max_s.unwrap_or(RECORDING_DEFAULT_S);
        if !(max_s.is_finite() && max_s > 0.0 && max_s <= RECORDING_MAX_S) {
            return Err(ControlFailure::Invalid(format!(
                "max_s {max_s} must be in (0, {RECORDING_MAX_S}]"
            )));
        }
        if !shared.cfg.source_class.permits_content() {
            return Err(ControlFailure::Refused(format!(
                "recording refused: content class {} forbids storing IQ of this window",
                crate::class::class_name(shared.cfg.source_class)
            )));
        }
        let id = RecordingId::new();
        let status = Arc::new(Mutex::new(RecordingStatus {
            active: true,
            id: Some(id),
            label: label.clone(),
            max_s,
            ..RecordingStatus::default()
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let (st, sp) = (Arc::clone(&status), Arc::clone(&stop));
        let join = thread::Builder::new()
            .name("hk-manual-rec".into())
            .spawn(move || {
                let r = record(&shared, id, label.as_deref(), max_s, &sp, &st);
                let mut s = st.lock().unwrap_or_else(PoisonError::into_inner);
                s.active = false;
                if let Err(e) = r {
                    inc(&shared.counters.chains.errors);
                    s.ended = Some(format!("error: {e:#}"));
                }
            })
            .map_err(|e| ControlFailure::Failed(format!("starting the recorder: {e}")))?;
        Ok(Self {
            stop,
            status,
            join: Some(join),
        })
    }

    /// The state now.
    pub fn status(&self) -> RecordingStatus {
        self.status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The recording thread has ended.
    pub fn is_finished(&self) -> bool {
        self.join.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Stops (if still running), waits for the row to be stored, and returns the final state.
    pub fn finish(mut self) -> RecordingStatus {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
        self.status()
    }
}

impl Drop for ManualRecorder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn record(
    shared: &Arc<Shared>,
    id: RecordingId,
    label: Option<&str>,
    max_s: f64,
    stop: &AtomicBool,
    status: &Mutex<RecordingStatus>,
) -> anyhow::Result<()> {
    let start = shared.ring.next_sample().unwrap_or(0);
    let cursor = shared.gate.register(start);
    let mut reader = shared.ring.reader_at(start);
    let dir = shared.cfg.data_dir.join("recordings");
    std::fs::create_dir_all(&dir)?;
    let stem = id.to_string();
    let data_path = dir.join(format!("{stem}.sigmf-data"));
    let meta_path = dir.join(format!("{stem}.sigmf-meta"));
    let mut out = BufWriter::new(
        File::create(&data_path).with_context(|| format!("creating {}", data_path.display()))?,
    );
    let mut buf = vec![Complex::<i8>::default(); 1 << 16];
    let mut bytes = Vec::new();
    let mut captures: Vec<Capture> = Vec::new();
    let mut first: Option<(Timestamp, hk_core::ProvenanceHandle)> = None;
    let mut last_prov: Option<ProvenanceId> = None;
    let mut next_index: Option<u64> = None;
    let (mut delivered, mut lost, mut max_samples) = (0u64, 0u64, u64::MAX);
    let ended = loop {
        if stop.load(Ordering::SeqCst) {
            break "stopped";
        }
        match reader.read_timeout(&mut buf, Duration::from_millis(50)) {
            ReadOutcome::Data(c) => {
                let t = &c.provenance.tune;
                if !window_class(t.center_hz, t.sample_rate_hz).permits_content() {
                    break "the tuned window's content class forbids storing IQ";
                }
                let fs = t.sample_rate_hz;
                if first.is_none() {
                    max_samples = ((max_s * fs) as u64).min(RECORDING_MAX_BYTES / 2);
                }
                let pid = c.provenance.id();
                if let Some(expected) = next_index {
                    lost += c.first_sample().saturating_sub(expected);
                }
                if last_prov != Some(pid) || next_index != Some(c.first_sample()) {
                    last_prov = Some(pid);
                    captures.push(Capture {
                        sample_start: delivered,
                        frequency: Some(t.center_hz),
                        datetime: Some(iso8601(c.time.host_time)),
                        provenance: Some(c.provenance.get().clone()),
                        clip_count: None,
                        extra: serde_json::Map::new(),
                    });
                }
                first.get_or_insert((c.time.host_time, c.provenance.clone()));
                let n = (c.len as u64).min(max_samples - delivered) as usize;
                bytes.clear();
                for s in &buf[..n] {
                    bytes.push(s.re as u8);
                    bytes.push(s.im as u8);
                }
                out.write_all(&bytes)?;
                delivered += n as u64;
                next_index = Some(c.end_sample());
                cursor.set(c.end_sample());
                {
                    let mut s = status.lock().unwrap_or_else(PoisonError::into_inner);
                    s.samples = delivered;
                    s.lost_samples = lost;
                    s.center_hz.get_or_insert(t.center_hz);
                    s.sample_rate_hz.get_or_insert(fs);
                    s.started.get_or_insert(c.time.host_time);
                }
                if delivered >= max_samples {
                    break "maximum duration reached";
                }
            }
            ReadOutcome::Overrun { lost_samples, .. } => {
                lost += lost_samples;
                next_index = None;
            }
            ReadOutcome::Empty => {}
            ReadOutcome::Closed => break "the run segment ended (stop or re-plumb)",
        }
    };
    out.flush()?;
    drop(out);
    drop(cursor);
    {
        let mut s = status.lock().unwrap_or_else(PoisonError::into_inner);
        s.samples = delivered;
        s.lost_samples = lost;
        s.ended = Some(ended.to_owned());
    }
    let Some((t0, prov)) = first.filter(|_| delivered > 0) else {
        let _ = std::fs::remove_file(&data_path);
        return Ok(());
    };
    let fs = prov.tune.sample_rate_hz;
    let mut meta = SigmfMeta::new(Datatype::Ci8);
    meta.global.sample_rate = Some(fs);
    meta.global.description = Some(match label {
        Some(l) => format!("hk-pipeline manual recording ({l})"),
        None => "hk-pipeline manual recording".into(),
    });
    meta.global.recorder = Some("hk-pipeline".into());
    meta.global.hw = shared.cfg.device_hw.clone();
    meta.global.provenance = Some(prov.get().clone());
    meta.captures = captures;
    meta.write(&meta_path)
        .map_err(|e| anyhow::anyhow!("writing {}: {e}", meta_path.display()))?;
    let duration_s = delivered as f64 / fs;
    let mut repo = shared.repo();
    let provenance_ref = repo.intern_provenance(prov.get())?;
    let rec = Recording {
        id,
        meta_uri: format!("recordings/{stem}.sigmf-meta"),
        data_uri: format!("recordings/{stem}.sigmf-data"),
        kind: RecordingKind::IqSnippet,
        time: TimeRange::new(t0, t0.saturating_add_nanos((duration_s * 1e9) as i64)),
        f_center_hz: prov.tune.center_hz,
        sample_rate_hz: fs,
        trigger: RecordingTrigger::Manual,
        pre_trigger_s: 0.0,
        post_trigger_s: duration_s,
        size_bytes: 2 * delivered,
        retention_class: RetentionClass::Pinned,
        content_class: shared.cfg.source_class,
        provenance_ref,
    };
    repo.insert_recording(&rec)?;
    inc(&shared.counters.chains.recordings);
    status.lock().unwrap_or_else(PoisonError::into_inner).stored = true;
    Ok(())
}
