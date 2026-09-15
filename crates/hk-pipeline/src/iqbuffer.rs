//! Rolling IQ capture buffer of a run (T-157, ADR-0013 §4 API gap 1): always-on raw IQ with
//! tuning/gain provenance segments, a status and clip export into the recordings model.
//! Storage (a pre-allocated on-disk ring that survives restarts, T-178 / ADR-0014), segments and
//! eviction are [`hk_store::iqbuffer`]; this module feeds it from each
//! segment's ring and turns an exported clip into a SigMF recording plus a `Recording` row.
//!
//! - **Real-time safety.** The feeder is one more ring reader on its own thread (`hk-iqbuffer`),
//!   the only thread that writes the buffer's files. The ring never waits for it: on a live source
//!   a slow disk laps the reader, and the lost samples are counted (`dropped_samples`, a gap before
//!   the next segment). Only a lossless replay that opts in (`HK_IQ_BUFFER=1`) registers a flow-gate
//!   cursor, like every other reader of an unpaced replay.
//! - **Default.** On for every run that is not a lossless (unpaced) replay, which is a recording
//!   already: a 2 min retention window on the sample clock, the disk quota `min(retention ×
//!   the device's highest rate × 2 bytes, --iq-buffer-max)`, and never below the free-space floor
//!   ([`hk_store::iqbuffer::IqBufferConfig`]; `hk serve --iq-retention/--iq-buffer-max`,
//!   `HK_IQ_RETENTION`, `HK_IQ_BUFFER_MAX`, `HK_IQ_BUFFER`).
//! - **Clip guard.** A clip is sized before anything is written: over `max_clip_bytes` (256 MiB
//!   by default) it is refused, and so is one the recordings filesystem cannot hold above the
//!   free-space floor.
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
pub use hk_store::iqbuffer::ClipRange;
use hk_store::iqbuffer::{
    Allocation, ClipError, IqBuffer, IqBufferConfig, IqBufferStatus, IqBufferWriter, OsHooks,
    SegmentStart, is_allocation_refused,
};
use num_complex::Complex;
use serde::Serialize;

use crate::chains::record::iso8601;
use crate::class::window_class;
use crate::config::PipelineConfig;
use crate::gate::GateCursor;
use crate::recorder::{RECORDING_LABEL_MAX, RECORDING_MAX_BYTES};
use crate::run::Shared;

/// A clip export request (frequencies Hz).
#[derive(Clone, Debug, PartialEq)]
pub struct ClipRequest {
    /// The samples: sample-clock ns or stream indices (exact).
    pub range: ClipRange,
    /// Only segments whose tuned window overlaps `(f_lo, f_hi)`.
    pub band: Option<(f64, f64)>,
    /// User label.
    pub label: Option<String>,
    /// Only segments of this run (T-178; `None`: an index range selects in the current run, a
    /// time range in any single run).
    pub run: Option<u64>,
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
    /// Larger than the clip cap.
    TooLarge(String),
    /// The recordings filesystem cannot hold it above the free-space floor.
    NoSpace(String),
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
            | Self::NoSpace(m)
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
    /// `t0` exactly, Unix ns.
    pub t0_ns: i64,
    /// Buffer segment id.
    pub segment: u64,
    /// Run of the segment (its stream indices are that run's).
    pub run: u64,
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
    /// `t0` exactly, Unix ns.
    pub t0_ns: i64,
    /// `t1` exactly, Unix ns.
    pub t1_ns: i64,
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
    /// The ring was refused for lack of free space.
    refused: bool,
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
        let mut refused = false;
        let (buffer, writer, reason) = if bc.active(cfg.lossless) {
            let dir = cfg.data_dir.join(hk_store::iqbuffer::DIR_NAME);
            let hooks = cfg
                .iq_buffer_hooks
                .clone()
                .unwrap_or_else(|| Arc::new(OsHooks));
            let started = std::time::Instant::now();
            match IqBuffer::open_with(&dir, bc, hooks) {
                Ok((b, w)) => {
                    let s = b.status(None, None, 0);
                    eprintln!(
                        "IQ capture ring {}: {} slots of {} bytes ({:?}, preallocated {}), run {}, \
                         {} segments recovered, opened in {:.3} s",
                        dir.display(),
                        s.slot_count,
                        s.chunk_bytes,
                        s.allocation,
                        s.preallocated,
                        b.run(),
                        s.recovered_segments,
                        started.elapsed().as_secs_f64()
                    );
                    (Some(b), Some(w), None)
                }
                Err(e) if is_allocation_refused(&e) => {
                    eprintln!("IQ capture buffer disabled: {e}");
                    refused = true;
                    (None, None, Some(e.to_string()))
                }
                Err(e) => {
                    eprintln!("IQ capture buffer disabled: opening {}: {e}", dir.display());
                    (None, None, Some(format!("the buffer could not open: {e}")))
                }
            }
        } else {
            let why = if bc.enabled == Some(false)
                || bc.max_bytes == Some(0)
                || bc.retention_s <= 0.0
            {
                "disabled by configuration (--iq-retention off, HK_IQ_RETENTION, HK_IQ_BUFFER=0 or a \
                 zero --iq-buffer-max)"
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
            refused,
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
            None => {
                let mut s =
                    IqBufferStatus::disabled(&self.cfg, self.reason.clone().unwrap_or_default());
                if self.refused {
                    s.allocation = Some(Allocation::Refused);
                }
                s
            }
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
            range,
            band,
            label,
            run,
        } = request;
        match *range {
            ClipRange::Time { t0_ns, t1_ns } if !(0 <= t0_ns && t0_ns < t1_ns) => {
                return Err(ClipFailure::Invalid(
                    "the range needs 0 ≤ t0 < t1 on the sample clock".into(),
                ));
            }
            ClipRange::Index { start, end } if start >= end => {
                return Err(ClipFailure::Invalid(
                    "the range needs at least one sample".into(),
                ));
            }
            _ => {}
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
        let failure = |e: ClipError| match e {
            ClipError::Empty | ClipError::Evicted => ClipFailure::NotFound(e.to_string()),
            ClipError::MixedRates { .. } | ClipError::MixedRuns { .. } => {
                ClipFailure::Conflict(e.to_string())
            }
            ClipError::TooLarge { .. } => ClipFailure::TooLarge(e.to_string()),
            ClipError::Io(_) => ClipFailure::Failed(format!("exporting the clip: {e}")),
        };
        // Sized before anything is written.
        let plan = buffer.plan_clip_run(*range, *band, *run).map_err(failure)?;
        let cap = self.cfg.max_clip_bytes.min(RECORDING_MAX_BYTES);
        if plan.bytes() > cap {
            return Err(ClipFailure::TooLarge(format!(
                "the clip would be {} bytes; a clip holds at most {cap} bytes \
                 (HK_IQ_BUFFER_CLIP_MAX): export a shorter range",
                plan.bytes()
            )));
        }
        let dir = self.data_dir.join("recordings");
        std::fs::create_dir_all(&dir).map_err(|e| ClipFailure::Failed(e.to_string()))?;
        if let Ok(space) = buffer.space_at(&dir) {
            let floor = self.cfg.free_floor(space.total);
            if space.free < plan.bytes().saturating_add(floor) {
                return Err(ClipFailure::NoSpace(format!(
                    "not enough free space for the clip: it needs {} bytes above the {floor}-byte \
                     free-space floor and {} bytes are free",
                    plan.bytes(),
                    space.free
                )));
            }
        }
        let id = RecordingId::new();
        let stem = id.to_string();
        let data_path = dir.join(format!("{stem}.sigmf-data"));
        let meta_path = dir.join(format!("{stem}.sigmf-meta"));
        let cleanup = || {
            let _ = std::fs::remove_file(&data_path);
            let _ = std::fs::remove_file(&meta_path);
        };
        let clip = File::create(&data_path)
            .map_err(ClipError::Io)
            .and_then(|f| plan.write(&mut BufWriter::new(f)));
        let clip = match clip {
            Ok(c) => c,
            Err(e) => {
                cleanup();
                return Err(failure(e));
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
                extra.insert("hackriff:buffer_run".into(), p.run.into());
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
            t0_ns: clip.t0_ns,
            t1_ns: clip.t1_ns,
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
                    t0_ns: p.t_ns,
                    segment: p.segment,
                    run: p.run,
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
        // The segment's end (stop or re-plumb) makes everything stored durable for a restart.
        if let Err(e) = w.checkpoint() {
            eprintln!("IQ capture buffer: checkpoint failed: {e}");
        }
        Ok(())
    }
}
