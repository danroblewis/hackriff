//! Rolling IQ capture buffer of a run (T-157, ADR-0013 §4 API gap 1): always-on raw IQ with
//! tuning/gain provenance segments, a status and clip export into the recordings model.
//! Storage, segments and eviction are [`hk_store::iqbuffer`]; this module feeds it from each
//! segment's ring and turns an exported clip into a SigMF recording plus a `Recording` row.
//!
//! - **Real-time safety.** The feeder is one more ring reader on its own thread (`hk-iqbuffer`),
//!   the only thread that writes the buffer's files. The ring never waits for it: on a live source
//!   a slow disk laps the reader, and the lost samples are counted (`dropped_samples`, a gap before
//!   the next segment). Only a lossless replay that opts in (`HK_IQ_BUFFER=1`) registers a flow-gate
//!   cursor, like every other reader of an unpaced replay.
//! - **Default.** On for every run that is not a lossless (unpaced) replay, which is a recording
//!   already: 2 GiB or 10 min of sample clock, whichever is reached first
//!   ([`hk_store::iqbuffer::IqBufferConfig`], `HK_IQ_BUFFER`, `HK_IQ_BUFFER_BYTES`,
//!   `HK_IQ_BUFFER_S`).
//! - **Content rule.** A block whose tuned window's class forbids content is never stored
//!   (`gated_samples`), exactly as the manual recorder refuses it (only when `HK_CONTENT_GATING=1`).
//! - **Clips.** `[t0, t1)` on the sample clock, optionally only the segments whose tuned window
//!   overlaps a band (the data stays the window as captured; the band is a SigMF annotation). One
//!   SigMF capture per piece, each with its provenance, centre, time and `core:global_index`, so a
//!   retune or a gap is a capture boundary, never spliced silently. Written to
//!   `recordings/<id>.sigmf-{data,meta}` with a `Recording` row (kind `iq-snippet`, trigger
//!   `manual`, retention `pinned`). A range spanning a sample-rate change is refused (one rate per
//!   SigMF file).

use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use hk_core::{ReadOutcome, RingReader};
use hk_model::sigmf::{Annotation, Capture, Datatype, SigmfMeta};
use hk_model::{
    ContentClass, ProvenanceId, Recording, RecordingId, RecordingKind, RecordingTrigger,
    RetentionClass, TimeRange, Timestamp,
};
use hk_store::iqbuffer::{
    ClipError, IqBuffer, IqBufferConfig, IqBufferStatus, IqBufferWriter, SegmentStart,
};
use num_complex::Complex;
use serde::Serialize;

use crate::chains::record::iso8601;
use crate::class::window_class;
use crate::config::PipelineConfig;
use crate::gate::GateCursor;
use crate::recorder::{RECORDING_LABEL_MAX, RECORDING_MAX_BYTES};
use crate::run::Shared;

/// A clip export request (times Unix s on the sample clock, frequencies Hz).
#[derive(Clone, Debug, PartialEq)]
pub struct ClipRequest {
    /// Start (inclusive).
    pub t0_s: f64,
    /// End (exclusive).
    pub t1_s: f64,
    /// Only segments whose tuned window overlaps `(f_lo, f_hi)`.
    pub band: Option<(f64, f64)>,
    /// User label.
    pub label: Option<String>,
}

/// Why a clip was not exported.
#[derive(Debug)]
pub enum ClipFailure {
    /// The run has no buffer.
    Unavailable(String),
    /// A malformed request.
    Invalid(String),
    /// Nothing buffered in the range.
    NotFound(String),
    /// The range spans a sample-rate change.
    Conflict(String),
    /// Too large for one recording.
    TooLarge(String),
    /// Storage failed.
    Failed(String),
}

impl std::fmt::Display for ClipFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(m)
            | Self::Invalid(m)
            | Self::NotFound(m)
            | Self::Conflict(m)
            | Self::TooLarge(m)
            | Self::Failed(m) => f.write_str(m),
        }
    }
}

/// One SigMF capture of an exported clip.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ClipCaptureInfo {
    /// First sample in the clip file.
    pub sample_start: u64,
    /// Samples.
    pub samples: u64,
    /// Stream index of the first sample.
    pub global_index: u64,
    /// Time of the first sample, Unix s.
    pub t0: f64,
    /// Buffer segment id.
    pub segment: u64,
    /// Tuned centre, Hz.
    pub center_hz: f64,
    /// Sample rate, Hz.
    pub sample_rate_hz: f64,
    /// Baseband filter, Hz.
    pub bandwidth_hz: f64,
    /// LNA gain, dB.
    pub lna_db: f64,
    /// VGA gain, dB.
    pub vga_db: f64,
    /// RF amplifier on.
    pub amp_on: bool,
    /// Device id.
    pub device_id: String,
}

/// An exported clip: the stored recording.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ClipExported {
    /// `Recording` row id.
    pub id: RecordingId,
    /// User label.
    pub label: Option<String>,
    /// `recordings/<id>.sigmf-meta` (relative to the data directory).
    pub meta_uri: String,
    /// `recordings/<id>.sigmf-data`.
    pub data_uri: String,
    /// Absolute meta path.
    pub meta_path: String,
    /// Absolute data path.
    pub data_path: String,
    /// First sample, Unix s.
    pub t0: f64,
    /// End of the last sample, Unix s.
    pub t1: f64,
    /// Samples.
    pub samples: u64,
    /// Data bytes (ci8).
    pub bytes: u64,
    /// Sample rate, Hz.
    pub sample_rate_hz: f64,
    /// Centre of the first capture, Hz.
    pub center_hz: f64,
    /// Requested band `[f_lo, f_hi]`, Hz.
    pub band: Option<[f64; 2]>,
    /// Content class of the recording.
    pub content_class: ContentClass,
    /// One entry per SigMF capture.
    pub captures: Vec<ClipCaptureInfo>,
}

/// The run's buffer: status, clip export and the per-segment feeder.
pub struct IqBufferService {
    buffer: Option<IqBuffer>,
    writer: Mutex<Option<IqBufferWriter>>,
    cfg: IqBufferConfig,
    reason: Option<String>,
    data_dir: PathBuf,
    db_path: PathBuf,
    device_hw: Option<String>,
    exports: Mutex<()>,
}

impl IqBufferService {
    /// Opens the buffer for a run under `cfg` (never fails the run: a buffer that cannot open is
    /// reported as disabled with its reason).
    pub(crate) fn open(cfg: &PipelineConfig, db_path: PathBuf) -> Self {
        let bc = cfg.iq_buffer;
        let (buffer, writer, reason) = if bc.active(cfg.lossless) {
            let dir = cfg.data_dir.join(hk_store::iqbuffer::DIR_NAME);
            match IqBuffer::open(&dir, bc) {
                Ok((b, w)) => (Some(b), Some(w), None),
                Err(e) => {
                    eprintln!("IQ capture buffer disabled: opening {}: {e}", dir.display());
                    (None, None, Some(format!("the buffer could not open: {e}")))
                }
            }
        } else {
            let why = if bc.enabled == Some(false) || bc.max_bytes == 0 || bc.max_s <= 0.0 {
                "disabled by configuration (HK_IQ_BUFFER / quota)"
            } else {
                "off for a lossless replay (the recording is the history); HK_IQ_BUFFER=1 forces it"
            };
            (None, None, Some(why.to_owned()))
        };
        Self {
            buffer,
            writer: Mutex::new(writer),
            cfg: bc,
            reason,
            data_dir: cfg.data_dir.clone(),
            db_path,
            device_hw: cfg.device_hw.clone(),
            exports: Mutex::new(()),
        }
    }

    /// Whether the run buffers IQ.
    pub fn enabled(&self) -> bool {
        self.buffer.is_some()
    }

    /// The status: segments overlapping `[t0_s, t1_s)` (all when `None`), the newest `limit`.
    pub fn status(&self, t0_s: Option<f64>, t1_s: Option<f64>, limit: usize) -> IqBufferStatus {
        let ns = |s: f64| (s * 1e9).round() as i64;
        match &self.buffer {
            Some(b) => b.status(t0_s.map(ns), t1_s.map(ns), limit),
            None => IqBufferStatus::disabled(&self.cfg, self.reason.clone().unwrap_or_default()),
        }
    }

    /// Exports `request` as a SigMF recording and stores its `Recording` row.
    pub fn export_clip(&self, request: &ClipRequest) -> Result<ClipExported, ClipFailure> {
        let buffer = self.buffer.as_ref().ok_or_else(|| {
            ClipFailure::Unavailable(format!(
                "this run has no IQ capture buffer: {}",
                self.reason.clone().unwrap_or_default()
            ))
        })?;
        let ClipRequest {
            t0_s,
            t1_s,
            band,
            label,
        } = request;
        if !(t0_s.is_finite() && t1_s.is_finite() && t0_s < t1_s && t0_s.abs() < 9.0e9) {
            return Err(ClipFailure::Invalid(
                "t0 and t1 are Unix seconds with t0 < t1".into(),
            ));
        }
        if let Some((lo, hi)) = band {
            if !(lo.is_finite() && hi.is_finite() && lo < hi) {
                return Err(ClipFailure::Invalid("band needs f_lo < f_hi (Hz)".into()));
            }
        }
        let label = match label.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(l)
                if l.chars().count() > RECORDING_LABEL_MAX || l.chars().any(char::is_control) =>
            {
                return Err(ClipFailure::Invalid(format!(
                    "label must be at most {RECORDING_LABEL_MAX} printable characters"
                )));
            }
            Some(l) => Some(l.to_owned()),
        };
        let _one = self.exports.lock().unwrap_or_else(PoisonError::into_inner);
        let id = RecordingId::new();
        let dir = self.data_dir.join("recordings");
        std::fs::create_dir_all(&dir).map_err(|e| ClipFailure::Failed(e.to_string()))?;
        let stem = id.to_string();
        let data_path = dir.join(format!("{stem}.sigmf-data"));
        let meta_path = dir.join(format!("{stem}.sigmf-meta"));
        let cleanup = || {
            let _ = std::fs::remove_file(&data_path);
            let _ = std::fs::remove_file(&meta_path);
        };
        let clip = File::create(&data_path)
            .map_err(ClipError::Io)
            .and_then(|f| {
                let mut out = BufWriter::new(f);
                buffer.export_clip(
                    (t0_s * 1e9).round() as i64,
                    (t1_s * 1e9).round() as i64,
                    *band,
                    RECORDING_MAX_BYTES,
                    &mut out,
                )
            });
        let clip = match clip {
            Ok(c) => c,
            Err(e) => {
                cleanup();
                return Err(match e {
                    ClipError::Empty => ClipFailure::NotFound(e.to_string()),
                    ClipError::MixedRates { .. } => ClipFailure::Conflict(e.to_string()),
                    ClipError::TooLarge { .. } => ClipFailure::TooLarge(format!(
                        "{e}; one recording holds at most {RECORDING_MAX_BYTES} bytes"
                    )),
                    ClipError::Io(_) => ClipFailure::Failed(format!("exporting the clip: {e}")),
                });
            }
        };
        let r = self.store(id, label.as_deref(), *band, &clip, &meta_path, &data_path);
        if r.is_err() {
            cleanup();
        }
        r
    }

    fn store(
        &self,
        id: RecordingId,
        label: Option<&str>,
        band: Option<(f64, f64)>,
        clip: &hk_store::iqbuffer::Clip,
        meta_path: &std::path::Path,
        data_path: &std::path::Path,
    ) -> Result<ClipExported, ClipFailure> {
        let failed = |e: String| ClipFailure::Failed(e);
        let first = &clip.pieces[0];
        let mut meta = SigmfMeta::new(Datatype::Ci8);
        meta.global.sample_rate = Some(clip.sample_rate_hz);
        meta.global.description = Some(match label {
            Some(l) => format!("hk-pipeline IQ capture buffer clip ({l})"),
            None => "hk-pipeline IQ capture buffer clip".into(),
        });
        meta.global.recorder = Some("hk-pipeline:iqbuffer".into());
        meta.global.hw = self.device_hw.clone();
        meta.global.provenance = Some(first.provenance.clone());
        meta.captures = clip
            .pieces
            .iter()
            .map(|p| {
                let mut extra = serde_json::Map::new();
                extra.insert("core:global_index".into(), p.global_index.into());
                extra.insert("hackriff:buffer_segment".into(), p.segment.into());
                Capture {
                    sample_start: p.sample_start,
                    frequency: Some(p.provenance.tune.center_hz),
                    datetime: Some(iso8601(Timestamp::from_unix_nanos(p.t_ns))),
                    provenance: Some(p.provenance.clone()),
                    clip_count: None,
                    extra,
                }
            })
            .collect();
        if let Some((lo, hi)) = band {
            meta.annotations.push(Annotation {
                sample_start: 0,
                sample_count: Some(clip.samples),
                freq_lower_edge: Some(lo),
                freq_upper_edge: Some(hi),
                label: Some("requested band".into()),
                comment: Some(
                    "clip exported for this band: segments whose tuned window overlaps it, as \
                     captured (not channelised)"
                        .into(),
                ),
                truth: None,
                extra: serde_json::Map::new(),
            });
        }
        meta.write(meta_path)
            .map_err(|e| failed(format!("writing the clip metadata: {e}")))?;
        let duration_s = clip.samples as f64 / clip.sample_rate_hz;
        let content_class = first.content_class;
        let mut repo = hk_model::Repository::open(&self.db_path)
            .map_err(|e| failed(format!("opening the recordings database: {e}")))?;
        let provenance_ref: ProvenanceId = repo
            .intern_provenance(&first.provenance)
            .map_err(|e| failed(format!("storing the clip provenance: {e}")))?;
        let stem = id.to_string();
        let (meta_uri, data_uri) = (
            format!("recordings/{stem}.sigmf-meta"),
            format!("recordings/{stem}.sigmf-data"),
        );
        let t0 = Timestamp::from_unix_nanos(clip.t0_ns);
        repo.insert_recording(&Recording {
            id,
            meta_uri: meta_uri.clone(),
            data_uri: data_uri.clone(),
            kind: RecordingKind::IqSnippet,
            time: TimeRange::new(t0, Timestamp::from_unix_nanos(clip.t1_ns)),
            f_center_hz: first.provenance.tune.center_hz,
            sample_rate_hz: clip.sample_rate_hz,
            trigger: RecordingTrigger::Manual,
            pre_trigger_s: 0.0,
            post_trigger_s: duration_s,
            size_bytes: 2 * clip.samples,
            retention_class: RetentionClass::Pinned,
            content_class,
            provenance_ref,
        })
        .map_err(|e| failed(format!("storing the Recording row: {e}")))?;
        let s = |ns: i64| ns as f64 / 1e9;
        Ok(ClipExported {
            id,
            label: label.map(str::to_owned),
            meta_uri,
            data_uri,
            meta_path: meta_path.display().to_string(),
            data_path: data_path.display().to_string(),
            t0: s(clip.t0_ns),
            t1: s(clip.t1_ns),
            samples: clip.samples,
            bytes: 2 * clip.samples,
            sample_rate_hz: clip.sample_rate_hz,
            center_hz: first.provenance.tune.center_hz,
            band: band.map(|(lo, hi)| [lo, hi]),
            content_class,
            captures: clip
                .pieces
                .iter()
                .map(|p| ClipCaptureInfo {
                    sample_start: p.sample_start,
                    samples: p.samples,
                    global_index: p.global_index,
                    t0: s(p.t_ns),
                    segment: p.segment,
                    center_hz: p.provenance.tune.center_hz,
                    sample_rate_hz: p.provenance.tune.sample_rate_hz,
                    bandwidth_hz: p.provenance.tune.bandwidth_hz,
                    lna_db: p.provenance.tune.lna_db,
                    vga_db: p.provenance.tune.vga_db,
                    amp_on: p.provenance.tune.amp_on,
                    device_id: p.provenance.device_id.clone(),
                })
                .collect(),
        })
    }

    /// Feeds one run segment's ring into the buffer until the ring closes (`hk-iqbuffer`). The
    /// reader and cursor are created before the capture thread starts, so the segment's first
    /// block is buffered.
    pub(crate) fn feed(
        &self,
        _shared: Arc<Shared>,
        mut reader: RingReader<Complex<i8>>,
        cursor: GateCursor,
    ) -> anyhow::Result<()> {
        let mut guard = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(w) = guard.as_mut() else {
            return Ok(());
        };
        let mut buf = vec![Complex::<i8>::default(); 1 << 16];
        let mut bytes = Vec::with_capacity(2 << 16);
        let mut last_prov: Option<ProvenanceId> = None;
        let mut next_index: Option<u64> = None;
        let mut dropped_before = 0u64;
        let mut reported = false;
        loop {
            match reader.read_timeout(&mut buf, Duration::from_millis(50)) {
                ReadOutcome::Data(c) => {
                    let t = &c.provenance.tune;
                    let class = window_class(t.center_hz, t.sample_rate_hz);
                    if !class.permits_content() {
                        w.count_gated(c.len as u64);
                        w.end_segment();
                        next_index = None;
                        cursor.set(c.end_sample());
                        continue;
                    }
                    let pid = c.provenance.id();
                    let flagged = c.block_start && !c.discontinuity.is_empty();
                    if !w.is_open()
                        || last_prov != Some(pid)
                        || next_index != Some(c.first_sample())
                        || flagged
                    {
                        w.begin_segment(SegmentStart {
                            global_index: c.first_sample(),
                            t_ns: c.time.host_time.as_unix_nanos(),
                            provenance: c.provenance.get().clone(),
                            content_class: class,
                            dropped_before: std::mem::take(&mut dropped_before),
                        });
                        last_prov = Some(pid);
                    }
                    bytes.clear();
                    for s in &buf[..c.len] {
                        bytes.push(s.re as u8);
                        bytes.push(s.im as u8);
                    }
                    match w.append(&bytes) {
                        Ok(()) => next_index = Some(c.end_sample()),
                        Err(e) => {
                            next_index = None;
                            if !std::mem::replace(&mut reported, true) {
                                eprintln!(
                                    "IQ capture buffer: write failed (dropping, counted): {e}"
                                );
                            }
                        }
                    }
                    cursor.set(c.end_sample());
                }
                ReadOutcome::Overrun {
                    lost_samples,
                    resume_at,
                    ..
                } => {
                    w.count_dropped(lost_samples);
                    dropped_before += lost_samples;
                    next_index = None;
                    cursor.set(resume_at);
                }
                ReadOutcome::Empty => {}
                // Everything written has been read (the segment ended: stop or re-plumb).
                ReadOutcome::Closed => break,
            }
        }
        w.end_segment();
        Ok(())
    }
}
