//! Rolling IQ capture buffer (T-157, ADR-0013 §4 API gap 1): the always-on raw-IQ history behind
//! the Capture timeline ("reviewing N ago / LIVE", "export clip from the buffer").
//!
//! - **Storage** (`<data dir>/iqbuffer/`). An append-only byte log of interleaved ci8 samples
//!   (2 bytes/sample, the ring's native format), split into chunk files `<seq>.ci8` of
//!   [`IqBufferConfig::chunk_bytes`] each. The index is in memory: chunks (logical byte range,
//!   read handle) and **segments**. The buffer lives as long as its run: dropping it deletes its
//!   chunk files, and opening clears any a killed process left (exported clips are ordinary
//!   recordings and stay).
//! - **Segments.** A segment is a contiguous run of samples under one provenance: the writer
//!   starts a new one on every provenance change (retune, rate, gain, filter, overload), source
//!   gap, ring overrun or discontinuity flag, so retune boundaries are always segment boundaries.
//!   Each segment keeps its first sample's stream index (`global_index`) and **sample-clock** time
//!   (ADR-0012 §0: the `Timestamp` the captured block carried, never wall time); sample `i` of the
//!   segment is at `t0 + i / fs`.
//! - **Retention: oldest first.** Two limits, whichever is hit first: disk bytes (whole oldest
//!   chunk files are deleted) and duration (the retained span `t_newest − t_oldest` on the sample
//!   clock, trimmed to the sample by advancing the log floor; a chunk file is deleted once it lies
//!   wholly below the floor). Evicted chunks, segments and samples are counted in the status.
//! - **Never blocks capture.** Only the writer thread (a ring reader) touches the log files; the
//!   index mutex is held for bookkeeping only, never across a write. A clip export snapshots the
//!   plan (segment ranges plus cloned chunk read handles) under the mutex and reads outside it; a
//!   chunk evicted meanwhile stays readable through its open handle.

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use hk_model::{ContentClass, Provenance};
use serde::Serialize;

/// `0`/`off`/`false` disables the buffer, `1`/`on`/`true` enables it for every run.
pub const ENV_ENABLED: &str = "HK_IQ_BUFFER";
/// Disk quota override, bytes.
pub const ENV_MAX_BYTES: &str = "HK_IQ_BUFFER_BYTES";
/// Duration quota override, s.
pub const ENV_MAX_S: &str = "HK_IQ_BUFFER_S";
/// Default disk quota: 2 GiB.
pub const DEFAULT_MAX_BYTES: u64 = 2 << 30;
/// Default duration quota: 10 min.
pub const DEFAULT_MAX_S: f64 = 600.0;
/// Largest chunk file.
pub const MAX_CHUNK_BYTES: u64 = 64 << 20;
/// Smallest chunk file (and smallest honoured quota is two of them).
pub const MIN_CHUNK_BYTES: u64 = 64 << 10;
/// Bytes per stored sample (ci8 I, Q).
pub const BYTES_PER_SAMPLE: u64 = 2;
/// Directory name under the data directory.
pub const DIR_NAME: &str = "iqbuffer";
/// Default and largest number of segments a status lists.
pub const STATUS_SEGMENTS_DEFAULT: usize = 1000;
/// Largest `limit` of a status.
pub const STATUS_SEGMENTS_MAX: usize = 10_000;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Size limits and switch of the buffer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IqBufferConfig {
    /// `None`: on for every run that is not a lossless (unpaced) replay, which is already a
    /// recording; `Some(b)` forces it.
    pub enabled: Option<bool>,
    /// Most bytes of chunk files kept on disk.
    pub max_bytes: u64,
    /// Longest retained span on the sample clock, s.
    pub max_s: f64,
}

impl Default for IqBufferConfig {
    fn default() -> Self {
        Self {
            enabled: None,
            max_bytes: DEFAULT_MAX_BYTES,
            max_s: DEFAULT_MAX_S,
        }
    }
}

impl IqBufferConfig {
    /// The defaults with [`ENV_ENABLED`], [`ENV_MAX_BYTES`] and [`ENV_MAX_S`] applied when set.
    pub fn from_env() -> Self {
        let mut c = Self::default();
        let var = |k| std::env::var(k).ok().map(|v| v.trim().to_ascii_lowercase());
        c.enabled = match var(ENV_ENABLED).as_deref() {
            Some("0" | "off" | "false" | "no") => Some(false),
            Some("1" | "on" | "true" | "yes") => Some(true),
            _ => None,
        };
        if let Some(v) = var(ENV_MAX_BYTES).and_then(|v| v.parse::<u64>().ok()) {
            c.max_bytes = v;
        }
        if let Some(v) = var(ENV_MAX_S)
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| v.is_finite())
        {
            c.max_s = v;
        }
        c
    }

    /// Whether a run buffers (see [`Self::enabled`]); a zero quota disables it.
    pub fn active(&self, lossless: bool) -> bool {
        self.enabled.unwrap_or(!lossless) && self.max_bytes > 0 && self.max_s > 0.0
    }

    /// Chunk file size: a sixteenth of the quota within [`MIN_CHUNK_BYTES`]..[`MAX_CHUNK_BYTES`],
    /// whole samples.
    pub fn chunk_bytes(&self) -> u64 {
        (self.max_bytes / 16).clamp(MIN_CHUNK_BYTES, MAX_CHUNK_BYTES) & !1
    }

    /// The disk quota actually enforced (at least two chunks).
    pub fn effective_max_bytes(&self) -> u64 {
        self.max_bytes.max(2 * self.chunk_bytes())
    }
}

/// What the writer knows about a new segment.
#[derive(Clone, Debug)]
pub struct SegmentStart {
    /// Stream index of the first sample.
    pub global_index: u64,
    /// Sample-clock time of the first sample, Unix ns.
    pub t_ns: i64,
    /// Provenance in force for every sample of the segment.
    pub provenance: Provenance,
    /// Content class of the segment's window.
    pub content_class: ContentClass,
    /// Samples this buffer's reader lost (ring overruns) since the previous segment.
    pub dropped_before: u64,
}

struct Chunk {
    log_start: u64,
    bytes: u64,
    file: Arc<File>,
    path: PathBuf,
}

struct Seg {
    id: u64,
    /// Logical byte of the first retained sample.
    log_start: u64,
    samples: u64,
    /// Stream index of the first retained sample.
    global_index: u64,
    start: SegmentStart,
}

impl Seg {
    fn fs(&self) -> f64 {
        self.start.provenance.tune.sample_rate_hz
    }

    /// Sample-clock time of stream index `index`, ns.
    fn t_ns(&self, index: u64) -> i64 {
        let k = index.saturating_sub(self.start.global_index) as f64;
        self.start.t_ns + (k * 1e9 / self.fs()).round() as i64
    }

    fn t0_ns(&self) -> i64 {
        self.t_ns(self.global_index)
    }

    fn t1_ns(&self) -> i64 {
        self.t_ns(self.global_index + self.samples)
    }

    /// Offset (samples from the retained start) of the first sample at or after `t_ns`.
    fn offset_at(&self, t_ns: i64) -> u64 {
        let from_origin = (t_ns - self.start.t_ns) as f64 * self.fs() / 1e9;
        let k = from_origin.ceil().max(0.0) as u64;
        k.saturating_sub(self.global_index - self.start.global_index)
            .min(self.samples)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
struct Counts {
    evicted_chunks: u64,
    evicted_segments: u64,
    evicted_samples: u64,
    dropped_samples: u64,
    gated_samples: u64,
    write_errors: u64,
}

#[derive(Default)]
struct State {
    chunks: VecDeque<Chunk>,
    segments: VecDeque<Seg>,
    log_floor: u64,
    log_end: u64,
    disk_bytes: u64,
    next_chunk_seq: u64,
    next_seg_id: u64,
    /// The last segment is still being appended to.
    open: bool,
    counts: Counts,
    error: Option<String>,
}

struct Inner {
    dir: PathBuf,
    cfg: IqBufferConfig,
    chunk_bytes: u64,
    max_bytes: u64,
    state: Mutex<State>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        let st = self.state.get_mut().unwrap_or_else(PoisonError::into_inner);
        for c in st.chunks.drain(..) {
            let _ = fs::remove_file(&c.path);
        }
    }
}

/// The buffer's shared index (status and clip export); cheap to clone.
#[derive(Clone)]
pub struct IqBuffer {
    inner: Arc<Inner>,
}

/// The single appender (owned by the writer thread).
pub struct IqBufferWriter {
    buffer: IqBuffer,
    file: Option<File>,
    room: u64,
}

/// A segment as the status lists it (times Unix s, frequencies Hz).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SegmentStatus {
    /// Segment id (increasing for the life of the buffer).
    pub id: u64,
    /// First retained sample's time.
    pub t0: f64,
    /// End of the last sample.
    pub t1: f64,
    /// Samples retained.
    pub samples: u64,
    /// Stream index of the first retained sample.
    pub global_index: u64,
    /// Tuned centre.
    pub center_hz: f64,
    /// Sample rate.
    pub sample_rate_hz: f64,
    /// Baseband filter bandwidth.
    pub bandwidth_hz: f64,
    /// LNA gain, dB.
    pub lna_db: f64,
    /// VGA gain, dB.
    pub vga_db: f64,
    /// RF amplifier on.
    pub amp_on: bool,
    /// Device id of the provenance.
    pub device_id: String,
    /// Antenna port, if known.
    pub antenna_port: Option<String>,
    /// The front end reported overload.
    pub overload: bool,
    /// Content class of the window.
    pub content_class: ContentClass,
    /// Samples the buffer's reader lost (overruns) just before this segment.
    pub dropped_before: u64,
}

/// A hole between two retained segments.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GapStatus {
    /// End of the segment before.
    pub t0: f64,
    /// Start of the segment after.
    pub t1: f64,
    /// Segment id after the gap.
    pub before_segment: u64,
    /// Stream indices missing (source gaps, settle blocks, overruns, gated blocks).
    pub samples: u64,
    /// Of which lost to this buffer's reader (ring overruns).
    pub dropped_samples: u64,
}

/// Eviction counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct EvictedStatus {
    /// Chunk files deleted.
    pub chunks: u64,
    /// Whole segments dropped.
    pub segments: u64,
    /// Samples dropped.
    pub samples: u64,
    /// IQ bytes dropped (`2 × samples`).
    pub bytes: u64,
}

/// The buffer's status.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct IqBufferStatus {
    /// The run buffers IQ.
    pub enabled: bool,
    /// Why not (`null` when enabled).
    pub reason: Option<String>,
    /// Buffer directory.
    pub dir: Option<String>,
    /// Disk quota enforced, bytes.
    pub max_bytes: u64,
    /// Duration quota, s.
    pub max_s: f64,
    /// Chunk file size, bytes.
    pub chunk_bytes: u64,
    /// Oldest retained sample (`null` when empty).
    pub t0: Option<f64>,
    /// End of the newest retained sample.
    pub t1: Option<f64>,
    /// `t1 − t0`, s (0 when empty).
    pub span_s: f64,
    /// Retained IQ bytes.
    pub bytes: u64,
    /// Bytes of chunk files on disk (≥ `bytes` by less than one chunk).
    pub disk_bytes: u64,
    /// Retained samples.
    pub samples: u64,
    /// Retained segments in the whole buffer.
    pub segments_total: usize,
    /// Matching segments not listed (the oldest beyond `limit`).
    pub segments_omitted: usize,
    /// Retained segments overlapping the query span, oldest first.
    pub segments: Vec<SegmentStatus>,
    /// Gaps between the listed consecutive segments.
    pub gaps: Vec<GapStatus>,
    /// Oldest-first eviction counts.
    pub evicted: EvictedStatus,
    /// Samples this buffer's reader lost to ring overruns (never held capture).
    pub dropped_samples: u64,
    /// Samples not stored because their window's class forbids content.
    pub gated_samples: u64,
    /// Failed chunk writes (the data of each is dropped and counted).
    pub write_errors: u64,
    /// The last write error.
    pub error: Option<String>,
}

impl IqBufferStatus {
    /// The status of a run without a buffer.
    pub fn disabled(cfg: &IqBufferConfig, reason: impl Into<String>) -> Self {
        Self {
            enabled: false,
            reason: Some(reason.into()),
            dir: None,
            max_bytes: cfg.effective_max_bytes(),
            max_s: cfg.max_s,
            chunk_bytes: cfg.chunk_bytes(),
            t0: None,
            t1: None,
            span_s: 0.0,
            bytes: 0,
            disk_bytes: 0,
            samples: 0,
            segments_total: 0,
            segments_omitted: 0,
            segments: Vec::new(),
            gaps: Vec::new(),
            evicted: EvictedStatus::default(),
            dropped_samples: 0,
            gated_samples: 0,
            write_errors: 0,
            error: None,
        }
    }
}

/// One contiguous piece of an exported clip (one SigMF capture).
#[derive(Clone, Debug)]
pub struct ClipPiece {
    /// First sample within the clip file.
    pub sample_start: u64,
    /// Samples.
    pub samples: u64,
    /// Stream index of the first sample.
    pub global_index: u64,
    /// Sample-clock time of the first sample, Unix ns.
    pub t_ns: i64,
    /// Buffer segment id.
    pub segment: u64,
    /// Provenance.
    pub provenance: Provenance,
    /// Content class.
    pub content_class: ContentClass,
}

/// An exported clip.
#[derive(Clone, Debug)]
pub struct Clip {
    /// Pieces in order.
    pub pieces: Vec<ClipPiece>,
    /// Samples written.
    pub samples: u64,
    /// Sample rate of every piece, Hz.
    pub sample_rate_hz: f64,
    /// First sample's time, Unix ns.
    pub t0_ns: i64,
    /// End of the last sample, Unix ns.
    pub t1_ns: i64,
}

/// Why a clip was not exported.
#[derive(Debug)]
pub enum ClipError {
    /// Nothing buffered in the range (and band).
    Empty,
    /// The range spans a sample-rate change (SigMF has one rate per file).
    MixedRates {
        /// Time of the first sample at the other rate, Unix ns.
        t_ns: i64,
    },
    /// Larger than the caller's limit.
    TooLarge {
        /// Bytes the clip would have.
        bytes: u64,
    },
    /// Reading the buffer or writing the clip failed.
    Io(io::Error),
}

impl std::fmt::Display for ClipError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "no buffered IQ in that range"),
            Self::MixedRates { t_ns } => write!(
                f,
                "the range spans a sample-rate change at {:.6} s; export each side separately",
                *t_ns as f64 / 1e9
            ),
            Self::TooLarge { bytes } => write!(f, "the clip would be {bytes} bytes"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ClipError {}

impl IqBuffer {
    /// Opens (creating) `dir`, deleting chunk files a previous process left.
    pub fn open(dir: &Path, cfg: IqBufferConfig) -> io::Result<(Self, IqBufferWriter)> {
        fs::create_dir_all(dir)?;
        for e in fs::read_dir(dir)?.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "ci8") {
                fs::remove_file(&p)?;
            }
        }
        let buffer = Self {
            inner: Arc::new(Inner {
                dir: dir.to_path_buf(),
                cfg,
                chunk_bytes: cfg.chunk_bytes(),
                max_bytes: cfg.effective_max_bytes(),
                state: Mutex::new(State::default()),
            }),
        };
        let writer = IqBufferWriter {
            buffer: buffer.clone(),
            file: None,
            room: 0,
        };
        Ok((buffer, writer))
    }

    /// The configuration.
    pub fn config(&self) -> IqBufferConfig {
        self.inner.cfg
    }

    /// The status: segments overlapping `[t0_ns, t1_ns)` (whole buffer when `None`), the newest
    /// `limit` of them.
    pub fn status(&self, t0_ns: Option<i64>, t1_ns: Option<i64>, limit: usize) -> IqBufferStatus {
        let st = lock(&self.inner.state);
        let live: Vec<&Seg> = st.segments.iter().filter(|s| s.samples > 0).collect();
        let (t0, t1) = match (live.first(), live.last()) {
            (Some(a), Some(b)) => (Some(a.t0_ns()), Some(b.t1_ns())),
            _ => (None, None),
        };
        let matching: Vec<&Seg> = live
            .iter()
            .copied()
            .filter(|s| t0_ns.is_none_or(|t| s.t1_ns() > t) && t1_ns.is_none_or(|t| s.t0_ns() < t))
            .collect();
        let omitted = matching.len().saturating_sub(limit);
        let listed = &matching[omitted..];
        let s = |ns: i64| ns as f64 / 1e9;
        let segments = listed
            .iter()
            .map(|g| {
                let p = &g.start.provenance;
                SegmentStatus {
                    id: g.id,
                    t0: s(g.t0_ns()),
                    t1: s(g.t1_ns()),
                    samples: g.samples,
                    global_index: g.global_index,
                    center_hz: p.tune.center_hz,
                    sample_rate_hz: p.tune.sample_rate_hz,
                    bandwidth_hz: p.tune.bandwidth_hz,
                    lna_db: p.tune.lna_db,
                    vga_db: p.tune.vga_db,
                    amp_on: p.tune.amp_on,
                    device_id: p.device_id.clone(),
                    antenna_port: p.antenna_port.clone(),
                    overload: p.overload,
                    content_class: g.start.content_class,
                    dropped_before: g.start.dropped_before,
                }
            })
            .collect();
        let gaps = listed
            .windows(2)
            .filter_map(|w| {
                let (a, b) = (w[0], w[1]);
                let missing = b.global_index.saturating_sub(a.global_index + a.samples);
                (missing > 0 || b.start.dropped_before > 0).then(|| GapStatus {
                    t0: s(a.t1_ns()),
                    t1: s(b.t0_ns()),
                    before_segment: b.id,
                    samples: missing,
                    dropped_samples: b.start.dropped_before,
                })
            })
            .collect();
        let c = st.counts;
        let bytes = st.log_end - st.log_floor;
        IqBufferStatus {
            enabled: true,
            reason: None,
            dir: Some(self.inner.dir.display().to_string()),
            max_bytes: self.inner.max_bytes,
            max_s: self.inner.cfg.max_s,
            chunk_bytes: self.inner.chunk_bytes,
            t0: t0.map(s),
            t1: t1.map(s),
            span_s: match (t0, t1) {
                (Some(a), Some(b)) => s(b - a),
                _ => 0.0,
            },
            bytes,
            disk_bytes: st.disk_bytes,
            samples: bytes / BYTES_PER_SAMPLE,
            segments_total: live.len(),
            segments_omitted: omitted,
            segments,
            gaps,
            evicted: EvictedStatus {
                chunks: c.evicted_chunks,
                segments: c.evicted_segments,
                samples: c.evicted_samples,
                bytes: c.evicted_samples * BYTES_PER_SAMPLE,
            },
            dropped_samples: c.dropped_samples,
            gated_samples: c.gated_samples,
            write_errors: c.write_errors,
            error: st.error.clone(),
        }
    }

    /// Writes the buffered samples in `[t0_ns, t1_ns)` (segments whose tuned window overlaps
    /// `band` when given) to `out` as ci8 and describes them. At most `max_bytes` of data.
    pub fn export_clip(
        &self,
        t0_ns: i64,
        t1_ns: i64,
        band: Option<(f64, f64)>,
        max_bytes: u64,
        out: &mut dyn Write,
    ) -> Result<Clip, ClipError> {
        type Span = (Arc<File>, u64, u64);
        let mut plan: Vec<(ClipPiece, Vec<Span>)> = Vec::new();
        {
            let st = lock(&self.inner.state);
            let mut sample_start = 0u64;
            for seg in st.segments.iter().filter(|s| s.samples > 0) {
                let tune = &seg.start.provenance.tune;
                if let Some((lo, hi)) = band {
                    let half = tune.sample_rate_hz / 2.0;
                    if tune.center_hz + half <= lo || tune.center_hz - half >= hi {
                        continue;
                    }
                }
                let (j0, j1) = (seg.offset_at(t0_ns), seg.offset_at(t1_ns));
                if j1 <= j0 {
                    continue;
                }
                if let Some((first, _)) = plan.first() {
                    if first.provenance.tune.sample_rate_hz != tune.sample_rate_hz {
                        return Err(ClipError::MixedRates {
                            t_ns: seg.t_ns(seg.global_index + j0),
                        });
                    }
                }
                let (a, b) = (
                    seg.log_start + j0 * BYTES_PER_SAMPLE,
                    seg.log_start + j1 * BYTES_PER_SAMPLE,
                );
                let reads = st
                    .chunks
                    .iter()
                    .filter(|c| c.log_start < b && c.log_start + c.bytes > a)
                    .map(|c| {
                        let lo = a.max(c.log_start);
                        let hi = b.min(c.log_start + c.bytes);
                        (Arc::clone(&c.file), lo - c.log_start, hi - lo)
                    })
                    .collect();
                let n = j1 - j0;
                plan.push((
                    ClipPiece {
                        sample_start,
                        samples: n,
                        global_index: seg.global_index + j0,
                        t_ns: seg.t_ns(seg.global_index + j0),
                        segment: seg.id,
                        provenance: seg.start.provenance.clone(),
                        content_class: seg.start.content_class,
                    },
                    reads,
                ));
                sample_start += n;
            }
            let bytes = sample_start * BYTES_PER_SAMPLE;
            if bytes > max_bytes {
                return Err(ClipError::TooLarge { bytes });
            }
        }
        let Some((first, _)) = plan.first() else {
            return Err(ClipError::Empty);
        };
        let fs_hz = first.provenance.tune.sample_rate_hz;
        let t0 = first.t_ns;
        let mut buf = vec![0u8; 1 << 20];
        for (_, reads) in &plan {
            for (file, offset, len) in reads {
                let mut done = 0;
                while done < *len {
                    let n = (*len - done).min(buf.len() as u64) as usize;
                    file.read_exact_at(&mut buf[..n], offset + done)
                        .map_err(ClipError::Io)?;
                    out.write_all(&buf[..n]).map_err(ClipError::Io)?;
                    done += n as u64;
                }
            }
        }
        out.flush().map_err(ClipError::Io)?;
        let pieces: Vec<ClipPiece> = plan.into_iter().map(|(p, _)| p).collect();
        let last = pieces.last().expect("a piece");
        let t1 = last.t_ns + (last.samples as f64 * 1e9 / fs_hz).round() as i64;
        Ok(Clip {
            samples: last.sample_start + last.samples,
            pieces,
            sample_rate_hz: fs_hz,
            t0_ns: t0,
            t1_ns: t1,
        })
    }

    /// Advances the floor and deletes chunks for both quotas; returns files to delete.
    fn evict(&self, st: &mut State) -> Vec<PathBuf> {
        let mut dead = Vec::new();
        // Disk bytes: whole oldest chunks (never the one being written).
        while st.disk_bytes > self.inner.max_bytes && st.chunks.len() > 1 {
            let c = st.chunks.pop_front().expect("a chunk");
            st.disk_bytes -= c.bytes;
            st.counts.evicted_chunks += 1;
            st.log_floor = st.log_floor.max(c.log_start + c.bytes);
            dead.push(c.path);
        }
        // Duration on the sample clock, to the sample.
        let max_ns = (self.inner.cfg.max_s * 1e9) as i64;
        if let Some(t_end) = st.segments.back().map(Seg::t1_ns) {
            let limit = t_end.saturating_sub(max_ns);
            for seg in &st.segments {
                if seg.samples == 0 || seg.t1_ns() <= limit {
                    st.log_floor = st
                        .log_floor
                        .max(seg.log_start + seg.samples * BYTES_PER_SAMPLE);
                    continue;
                }
                let k = seg.offset_at(limit);
                st.log_floor = st.log_floor.max(seg.log_start + k * BYTES_PER_SAMPLE);
                break;
            }
        }
        while st.chunks.len() > 1
            && st
                .chunks
                .front()
                .is_some_and(|c| c.log_start + c.bytes <= st.log_floor)
        {
            let c = st.chunks.pop_front().expect("a chunk");
            st.disk_bytes -= c.bytes;
            st.counts.evicted_chunks += 1;
            dead.push(c.path);
        }
        // Segments below the floor.
        let floor = st.log_floor;
        loop {
            let keep_open = st.segments.len() == 1 && st.open;
            let Some(seg) = st.segments.front_mut() else {
                break;
            };
            let end = seg.log_start + seg.samples * BYTES_PER_SAMPLE;
            if seg.log_start >= floor {
                break;
            }
            let k = ((floor.min(end) - seg.log_start) / BYTES_PER_SAMPLE).min(seg.samples);
            seg.log_start += k * BYTES_PER_SAMPLE;
            seg.global_index += k;
            seg.samples -= k;
            st.counts.evicted_samples += k;
            if seg.samples > 0 {
                break;
            }
            if keep_open {
                break;
            }
            st.segments.pop_front();
            st.counts.evicted_segments += 1;
        }
        dead
    }
}

impl IqBufferWriter {
    /// The shared index.
    pub fn buffer(&self) -> &IqBuffer {
        &self.buffer
    }

    /// Starts a segment; the next [`Self::append`] belongs to it.
    pub fn begin_segment(&mut self, start: SegmentStart) {
        let mut st = lock(&self.buffer.inner.state);
        if st.segments.back().is_some_and(|s| s.samples == 0) {
            st.segments.pop_back();
        }
        let id = st.next_seg_id;
        st.next_seg_id += 1;
        let log_start = st.log_end;
        st.segments.push_back(Seg {
            id,
            log_start,
            samples: 0,
            global_index: start.global_index,
            start,
        });
        st.open = true;
    }

    /// Ends the open segment (the next append needs a new one).
    pub fn end_segment(&mut self) {
        lock(&self.buffer.inner.state).open = false;
    }

    /// Whether a segment is open.
    pub fn is_open(&self) -> bool {
        lock(&self.buffer.inner.state).open
    }

    /// Counts samples lost by the feeding reader (ring overruns).
    pub fn count_dropped(&mut self, samples: u64) {
        lock(&self.buffer.inner.state).counts.dropped_samples += samples;
    }

    /// Counts samples not stored because their class forbids content.
    pub fn count_gated(&mut self, samples: u64) {
        lock(&self.buffer.inner.state).counts.gated_samples += samples;
    }

    /// Appends interleaved ci8 `bytes` (whole samples) to the open segment. On a write error the
    /// unwritten samples are counted as dropped, the segment ends and the error is kept.
    pub fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        debug_assert!(bytes.len() % 2 == 0);
        let mut rest = &bytes[..bytes.len() & !1];
        while !rest.is_empty() {
            if let Err(e) = self.write_some(&mut rest) {
                let mut st = lock(&self.buffer.inner.state);
                st.counts.write_errors += 1;
                st.counts.dropped_samples += rest.len() as u64 / BYTES_PER_SAMPLE;
                st.error = Some(e.to_string());
                st.open = false;
                self.file = None;
                self.room = 0;
                return Err(e);
            }
        }
        Ok(())
    }

    fn write_some(&mut self, rest: &mut &[u8]) -> io::Result<()> {
        let inner = Arc::clone(&self.buffer.inner);
        if !lock(&inner.state).open {
            return Err(io::Error::other("append without an open segment"));
        }
        if self.file.is_none() || self.room == 0 {
            let seq = lock(&inner.state).next_chunk_seq;
            let path = inner.dir.join(format!("{seq:016}.ci8"));
            let file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&path)?;
            let read = File::open(&path)?;
            let mut st = lock(&inner.state);
            st.next_chunk_seq += 1;
            let log_start = st.log_end;
            st.chunks.push_back(Chunk {
                log_start,
                bytes: 0,
                file: Arc::new(read),
                path,
            });
            self.file = Some(file);
            self.room = inner.chunk_bytes;
        }
        let n = (self.room as usize).min(rest.len());
        self.file
            .as_mut()
            .expect("a chunk file")
            .write_all(&rest[..n])?;
        self.room -= n as u64;
        *rest = &rest[n..];
        let dead = {
            let mut st = lock(&inner.state);
            st.chunks.back_mut().expect("the chunk").bytes += n as u64;
            st.log_end += n as u64;
            st.disk_bytes += n as u64;
            st.segments.back_mut().expect("the open segment").samples +=
                n as u64 / BYTES_PER_SAMPLE;
            self.buffer.evict(&mut st)
        };
        for p in dead {
            let _ = fs::remove_file(p);
        }
        Ok(())
    }
}

impl Drop for IqBufferWriter {
    fn drop(&mut self) {
        self.end_segment();
    }
}

#[cfg(test)]
mod tests {
    use hk_model::{ClockSource, TimestampMethod, Tune};

    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("hk-store-iqbuffer-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    fn prov(center_hz: f64, fs: f64) -> Provenance {
        Provenance {
            device_id: "test".into(),
            tune: Tune {
                center_hz,
                sample_rate_hz: fs,
                lna_db: 16.0,
                vga_db: 20.0,
                amp_on: false,
                bandwidth_hz: 0.75 * fs,
            },
            overload: false,
            quantisation_limited: false,
            temperature_c: None,
            antenna_port: None,
            clock_source: ClockSource::Internal,
            clock_locked: true,
            calibration_state_ref: None,
            spur_mask_ref: None,
            timestamp_method: TimestampMethod::Synthetic,
            timestamp_error_budget_ns: None,
        }
    }

    fn start(index: u64, t_ns: i64, center_hz: f64, fs: f64) -> SegmentStart {
        SegmentStart {
            global_index: index,
            t_ns,
            provenance: prov(center_hz, fs),
            content_class: ContentClass::Unrestricted,
            dropped_before: 0,
        }
    }

    fn ramp(from: u64, n: u64) -> Vec<u8> {
        (from..from + n)
            .flat_map(|i| [i as u8, (i >> 8) as u8])
            .collect()
    }

    #[test]
    fn capture_buffer_segments_clip_and_duration_eviction() {
        let dir = tmp("dur");
        let fs_hz = 1000.0;
        let cfg = IqBufferConfig {
            enabled: Some(true),
            max_bytes: 1 << 30,
            max_s: 2.0,
        };
        let (buf, mut w) = IqBuffer::open(&dir, cfg).unwrap();
        w.begin_segment(start(0, 0, 100e6, fs_hz));
        w.append(&ramp(0, 1500)).unwrap();
        // A retune with a 500-sample gap.
        w.begin_segment(start(2000, 2_000_000_000, 101e6, fs_hz));
        w.append(&ramp(2000, 1000)).unwrap();
        let s = buf.status(None, None, 10);
        // Span 3 s > 2 s: trimmed to [1.0, 3.0).
        assert_eq!(s.t0, Some(1.0));
        assert_eq!(s.t1, Some(3.0));
        assert_eq!(s.samples, 1500);
        assert_eq!(s.evicted.samples, 1000);
        assert_eq!(s.segments.len(), 2);
        assert_eq!(s.segments[0].global_index, 1000);
        assert_eq!(s.segments[1].center_hz, 101e6);
        assert_eq!(s.gaps.len(), 1);
        assert_eq!(s.gaps[0].samples, 500);
        // A clip across the retune.
        let mut out = Vec::new();
        let clip = buf
            .export_clip(1_200_000_000, 2_300_000_000, None, u64::MAX, &mut out)
            .unwrap();
        assert_eq!(clip.pieces.len(), 2);
        assert_eq!(clip.pieces[0].global_index, 1200);
        assert_eq!(clip.pieces[0].samples, 300);
        assert_eq!(clip.pieces[1].global_index, 2000);
        assert_eq!(clip.pieces[1].sample_start, 300);
        assert_eq!(clip.pieces[1].samples, 300);
        let mut want = ramp(1200, 300);
        want.extend(ramp(2000, 300));
        assert_eq!(out, want);
        // Band selects the second window only.
        let mut out = Vec::new();
        let clip = buf
            .export_clip(
                0,
                i64::MAX / 2,
                Some((100.9e6, 101.1e6)),
                u64::MAX,
                &mut out,
            )
            .unwrap();
        assert_eq!((clip.pieces.len(), clip.samples), (1, 1000));
        assert!(matches!(
            buf.export_clip(
                10_000_000_000,
                11_000_000_000,
                None,
                u64::MAX,
                &mut Vec::new()
            ),
            Err(ClipError::Empty)
        ));
        drop(w);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn capture_buffer_byte_quota_evicts_whole_oldest_chunks() {
        let dir = tmp("bytes");
        let cfg = IqBufferConfig {
            enabled: Some(true),
            max_bytes: 2 * MIN_CHUNK_BYTES,
            max_s: 1e9,
        };
        let (buf, mut w) = IqBuffer::open(&dir, cfg).unwrap();
        let fs_hz = 1e6;
        w.begin_segment(start(0, 0, 100e6, fs_hz));
        let per_chunk = MIN_CHUNK_BYTES / 2;
        // Five chunks' worth: at most two stay on disk.
        for k in 0..5 {
            w.append(&ramp(k * per_chunk, per_chunk)).unwrap();
        }
        let s = buf.status(None, None, 10);
        assert!(s.disk_bytes <= 2 * MIN_CHUNK_BYTES, "{s:?}");
        assert_eq!(s.evicted.chunks, 3);
        assert_eq!(s.samples, 2 * per_chunk);
        assert_eq!(s.segments[0].global_index, 3 * per_chunk);
        let files = fs::read_dir(&dir).unwrap().count();
        assert_eq!(files, 2);
        // A mixed-rate range is refused.
        w.begin_segment(start(5 * per_chunk, 10_000_000_000, 100e6, 2e6));
        w.append(&ramp(0, 10)).unwrap();
        assert!(matches!(
            buf.export_clip(0, i64::MAX / 2, None, u64::MAX, &mut Vec::new()),
            Err(ClipError::MixedRates { .. })
        ));
        assert!(matches!(
            buf.export_clip(0, 5_000_000_000, None, 0, &mut Vec::new()),
            Err(ClipError::TooLarge { .. })
        ));
        // The run's end deletes the chunk files.
        drop(w);
        drop(buf);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        let _ = fs::remove_dir_all(&dir);
    }
}
