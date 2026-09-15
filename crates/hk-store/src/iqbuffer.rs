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
//! - **Full disk.** A chunk enters the index only after its first write succeeded (the file is
//!   deleted otherwise), a failed write backs further writes off ([`RETRY_BACKOFF_MIN`] doubling
//!   to [`RETRY_BACKOFF_MAX`]), and the writer **pauses** while opening another chunk would leave
//!   less than the free-space floor ([`IqBufferConfig::free_floor`]) on the filesystem, resuming
//!   as soon as it would not. Nothing it skips is ever an index entry or a file.
//! - **Exact times.** Sample `k` of a segment is at `t0_ns + round_half_up(k · 10¹² / fs_mHz)` ns
//!   (the rate in integer millihertz, i128 arithmetic): a time range selects exactly the samples
//!   whose time lies in `[t0_ns, t1_ns)`, and a stream-index range ([`ClipRange::Index`]) selects
//!   exactly its indices.

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use hk_model::{ContentClass, Provenance};
use serde::Serialize;

/// `0`/`off`/`false` disables the buffer, `1`/`on`/`true` enables it for every run.
pub const ENV_ENABLED: &str = "HK_IQ_BUFFER";
/// Retention window (`hk serve --iq-retention`): a duration (`90s`, `2m`, `1h`); `0`/`off`
/// disables the buffer.
pub const ENV_RETENTION: &str = "HK_IQ_RETENTION";
/// Hard size cap (`hk serve --iq-buffer-max`): a size (`512MiB`, `8GiB`).
pub const ENV_MAX: &str = "HK_IQ_BUFFER_MAX";
/// Free-space floor override, a size.
pub const ENV_MIN_FREE: &str = "HK_IQ_BUFFER_MIN_FREE";
/// Largest clip export override, a size.
pub const ENV_CLIP_MAX: &str = "HK_IQ_BUFFER_CLIP_MAX";
/// Default retention window: 2 min.
pub const DEFAULT_RETENTION_S: f64 = 120.0;
/// Rate that sizes the implied quota when the device's highest rate is unknown: 20 Msps.
pub const DEFAULT_QUOTA_RATE_HZ: f64 = 20e6;
/// Default largest clip export: 256 MiB.
pub const DEFAULT_MAX_CLIP_BYTES: u64 = 256 << 20;
/// Smallest default free-space floor: 2 GiB.
pub const FREE_FLOOR_MIN_BYTES: u64 = 2 << 30;
/// Largest default free-space floor: 8 GiB (10 % of a large disk is more than SQLite and
/// recordings need).
pub const FREE_FLOOR_MAX_BYTES: u64 = 8 << 30;
/// Default free-space floor as a fraction of the filesystem.
pub const FREE_FLOOR_FRACTION: f64 = 0.10;
/// First wait after a failed write.
pub const RETRY_BACKOFF_MIN: Duration = Duration::from_millis(100);
/// Longest wait after repeated failed writes.
pub const RETRY_BACKOFF_MAX: Duration = Duration::from_secs(10);
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

/// Retention, size limits and switch of the buffer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IqBufferConfig {
    /// `None`: on for every run that is not a lossless (unpaced) replay, which is already a
    /// recording; `Some(b)` forces it.
    pub enabled: Option<bool>,
    /// Retention window on the sample clock, s (`0` disables the buffer).
    pub retention_s: f64,
    /// Hard size cap, bytes; `None`: the implied quota only ([`Self::quota_bytes`]).
    pub max_bytes: Option<u64>,
    /// Highest sample rate the device can be configured to, Hz: sizes the implied quota
    /// (`None`: [`DEFAULT_QUOTA_RATE_HZ`]).
    pub max_rate_hz: Option<f64>,
    /// Free space the buffer always leaves on its filesystem, bytes; `None`: 10 % of the
    /// filesystem within 2..8 GiB ([`Self::free_floor`]).
    pub min_free_bytes: Option<u64>,
    /// Largest clip export, bytes.
    pub max_clip_bytes: u64,
}

impl Default for IqBufferConfig {
    fn default() -> Self {
        Self {
            enabled: None,
            retention_s: DEFAULT_RETENTION_S,
            max_bytes: None,
            max_rate_hz: None,
            min_free_bytes: None,
            max_clip_bytes: DEFAULT_MAX_CLIP_BYTES,
        }
    }
}

/// Parses a retention duration: `90s`, `2m`, `1.5h`, `1d`, `500ms` or bare seconds; `0` and
/// `off` are 0 (disabled).
pub fn parse_duration_s(text: &str) -> Result<f64, String> {
    let t = text.trim().to_ascii_lowercase();
    if t == "off" {
        return Ok(0.0);
    }
    let split = t
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let scale = match unit.trim() {
        "" | "s" | "sec" | "secs" => 1.0,
        "ms" => 1e-3,
        "m" | "min" | "mins" => 60.0,
        "h" | "hr" | "hrs" => 3600.0,
        "d" => 86_400.0,
        _ => {
            return Err(format!(
                "{text:?} is not a duration (e.g. 90s, 2m, 1h, off)"
            ));
        }
    };
    num.parse::<f64>()
        .ok()
        .map(|v| v * scale)
        .filter(|v| v.is_finite() && *v >= 0.0 && *v <= 365.0 * 86_400.0)
        .ok_or_else(|| format!("{text:?} is not a duration (e.g. 90s, 2m, 1h, off)"))
}

/// Parses a size: `512MiB`, `8GiB`, `64KiB`, `1TiB`, decimal `500MB`/`2GB`, or bare bytes.
pub fn parse_size_bytes(text: &str) -> Result<u64, String> {
    let t = text.trim();
    let split = t
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let scale: u64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "kib" | "k" => 1 << 10,
        "mib" | "m" => 1 << 20,
        "gib" | "g" => 1 << 30,
        "tib" | "t" => 1 << 40,
        "kb" => 1_000,
        "mb" => 1_000_000,
        "gb" => 1_000_000_000,
        "tb" => 1_000_000_000_000,
        _ => return Err(format!("{text:?} is not a size (e.g. 512MiB, 8GiB)")),
    };
    let err = || format!("{text:?} is not a size (e.g. 512MiB, 8GiB)");
    if let Ok(n) = num.parse::<u64>() {
        return n.checked_mul(scale).ok_or_else(err);
    }
    num.parse::<f64>()
        .ok()
        .map(|v| (v * scale as f64).round())
        .filter(|v| v.is_finite() && *v >= 0.0 && *v < u64::MAX as f64)
        .map(|v| v as u64)
        .ok_or_else(err)
}

impl IqBufferConfig {
    /// The defaults with [`ENV_ENABLED`], [`ENV_RETENTION`], [`ENV_MAX`], [`ENV_MIN_FREE`] and
    /// [`ENV_CLIP_MAX`] applied when set (a malformed value is ignored with a warning).
    pub fn from_env() -> Self {
        let mut c = Self::default();
        let var = |k: &str| std::env::var(k).ok().map(|v| v.trim().to_ascii_lowercase());
        c.enabled = match var(ENV_ENABLED).as_deref() {
            Some("0" | "off" | "false" | "no") => Some(false),
            Some("1" | "on" | "true" | "yes") => Some(true),
            _ => None,
        };
        let warn = |k: &str, e: String| eprintln!("ignoring {k}: {e}");
        if let Some(v) = var(ENV_RETENTION) {
            match parse_duration_s(&v) {
                Ok(s) => c.retention_s = s,
                Err(e) => warn(ENV_RETENTION, e),
            }
        }
        let size = |k: &str| var(k).and_then(|v| parse_size_bytes(&v).map_err(|e| warn(k, e)).ok());
        if let Some(b) = size(ENV_MAX) {
            c.max_bytes = Some(b);
        }
        if let Some(b) = size(ENV_MIN_FREE) {
            c.min_free_bytes = Some(b);
        }
        if let Some(b) = size(ENV_CLIP_MAX) {
            c.max_clip_bytes = b;
        }
        c
    }

    /// Whether a run buffers (see [`Self::enabled`]); a zero retention or cap disables it.
    pub fn active(&self, lossless: bool) -> bool {
        self.enabled.unwrap_or(!lossless)
            && self.retention_s > 0.0
            && self.max_bytes.is_none_or(|b| b > 0)
    }

    /// `retention × highest rate × 2 bytes/sample` (ci8), bytes.
    pub fn implied_bytes(&self) -> u64 {
        let rate = self
            .max_rate_hz
            .filter(|r| r.is_finite() && *r > 0.0)
            .unwrap_or(DEFAULT_QUOTA_RATE_HZ);
        let b = (self.retention_s.max(0.0) * rate).ceil() * BYTES_PER_SAMPLE as f64;
        if b >= u64::MAX as f64 {
            u64::MAX
        } else {
            b as u64
        }
    }

    /// Chunk file size: a sixteenth of the quota within [`MIN_CHUNK_BYTES`]..[`MAX_CHUNK_BYTES`],
    /// whole samples.
    pub fn chunk_bytes(&self) -> u64 {
        (self.raw_quota() / 16).clamp(MIN_CHUNK_BYTES, MAX_CHUNK_BYTES) & !1
    }

    fn raw_quota(&self) -> u64 {
        self.max_bytes
            .map_or(self.implied_bytes(), |m| m.min(self.implied_bytes()))
    }

    /// The disk quota enforced: `min(implied, max_bytes)`, at least two chunks.
    pub fn quota_bytes(&self) -> u64 {
        self.raw_quota().max(2 * self.chunk_bytes())
    }

    /// The free-space floor on a filesystem of `total` bytes.
    pub fn free_floor(&self, total: u64) -> u64 {
        self.min_free_bytes.unwrap_or_else(|| {
            ((total as f64 * FREE_FLOOR_FRACTION) as u64)
                .clamp(FREE_FLOOR_MIN_BYTES, FREE_FLOOR_MAX_BYTES)
        })
    }
}

/// Free and total bytes of a filesystem.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FsSpace {
    /// Bytes available to an unprivileged writer.
    pub free: u64,
    /// Filesystem size.
    pub total: u64,
}

/// The space of the filesystem holding `path` (`statvfs`).
#[allow(clippy::unnecessary_cast, clippy::useless_conversion)]
pub fn fs_space(path: &Path) -> io::Result<FsSpace> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(io::Error::other)?;
    // SAFETY: an all-zero `statvfs` is a valid value of this plain C struct.
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a NUL-terminated path and `s` a valid, writable `statvfs`.
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let block = s.f_frsize as u64;
    Ok(FsSpace {
        free: (s.f_bavail as u64).saturating_mul(block),
        total: (s.f_blocks as u64).saturating_mul(block),
    })
}

/// Filesystem probes of the buffer, replaceable for tests (a full disk, a slow writer).
pub trait IqBufferHooks: Send + Sync {
    /// The space of the filesystem holding `path`.
    fn fs_space(&self, path: &Path) -> io::Result<FsSpace> {
        fs_space(path)
    }

    /// Called before every chunk write of `bytes`; an error fails that write.
    fn before_write(&self, _bytes: usize) -> io::Result<()> {
        Ok(())
    }
}

/// The real filesystem.
pub struct OsHooks;

impl IqBufferHooks for OsHooks {}

fn sat_i64(v: i128) -> i64 {
    v.clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

/// A rate in integer millihertz (at least 1).
fn fs_mhz(fs: f64) -> i128 {
    ((fs * 1000.0).round() as i128).max(1)
}

/// ns from a segment's first sample to its sample `k`: `round_half_up(k · 10¹² / fs_mHz)`.
fn ns_of(k: u64, mhz: i128) -> i128 {
    (2 * k as i128 * 1_000_000_000_000 + mhz) / (2 * mhz)
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
    fn mhz(&self) -> i128 {
        fs_mhz(self.start.provenance.tune.sample_rate_hz)
    }

    /// Sample-clock time of stream index `index`, ns (exact integer arithmetic).
    fn t_ns(&self, index: u64) -> i64 {
        let k = index.saturating_sub(self.start.global_index);
        sat_i64(self.start.t_ns as i128 + ns_of(k, self.mhz()))
    }

    fn t0_ns(&self) -> i64 {
        self.t_ns(self.global_index)
    }

    fn t1_ns(&self) -> i64 {
        self.t_ns(self.global_index + self.samples)
    }

    /// Offset (samples from the retained start) of the first sample whose time is at or after
    /// `t_ns`: the exact inverse of [`Self::t_ns`].
    fn offset_at(&self, t_ns: i64) -> u64 {
        let d = t_ns as i128 - self.start.t_ns as i128;
        let k = if d <= 0 {
            0
        } else {
            let mhz = self.mhz();
            let mut k = (d * mhz / 1_000_000_000_000).min(u64::MAX as i128 / 2) as u64;
            while k > 0 && ns_of(k - 1, mhz) >= d {
                k -= 1;
            }
            while ns_of(k, mhz) < d {
                k += 1;
            }
            k
        };
        k.saturating_sub(self.global_index - self.start.global_index)
            .min(self.samples)
    }

    /// Offset (samples from the retained start) of stream index `index`.
    fn offset_of(&self, index: u64) -> u64 {
        index.saturating_sub(self.global_index).min(self.samples)
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
    failed_samples: u64,
    pauses: u64,
    paused_samples: u64,
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
    /// Writing is paused below the free-space floor.
    paused: bool,
    counts: Counts,
    error: Option<String>,
}

struct Inner {
    dir: PathBuf,
    cfg: IqBufferConfig,
    chunk_bytes: u64,
    quota_bytes: u64,
    hooks: Arc<dyn IqBufferHooks>,
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
    /// Current wait after failed writes (zero after a success).
    backoff: Duration,
    /// No write is attempted before this.
    retry_at: Option<Instant>,
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
    /// `t0` exactly, Unix ns.
    pub t0_ns: i64,
    /// `t1` exactly, Unix ns.
    pub t1_ns: i64,
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
    /// Retention window on the sample clock, s.
    pub retention_s: f64,
    /// Hard size cap, bytes (`null`: none, the implied quota applies).
    pub max_bytes: Option<u64>,
    /// Disk quota enforced: `min(retention × highest rate × 2, max_bytes)`, at least two chunks.
    pub quota_bytes: u64,
    /// Largest clip export, bytes.
    pub max_clip_bytes: u64,
    /// Chunk file size, bytes.
    pub chunk_bytes: u64,
    /// Chunk files in the index.
    pub chunk_files: usize,
    /// Free bytes of the buffer's filesystem (`null` when unknown or disabled).
    pub fs_free_bytes: Option<u64>,
    /// Size of the buffer's filesystem (`null` when unknown or disabled).
    pub fs_total_bytes: Option<u64>,
    /// Free-space floor enforced on that filesystem (`null` when unknown or disabled).
    pub min_free_bytes: Option<u64>,
    /// Writing is paused because another chunk would go below the free-space floor.
    pub paused: bool,
    /// Pauses so far.
    pub pauses: u64,
    /// Samples not stored while paused.
    pub paused_samples: u64,
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
    /// Failed chunk writes (each backs writing off).
    pub write_errors: u64,
    /// Samples not stored because a write failed or writing was backing off.
    pub failed_samples: u64,
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
            retention_s: cfg.retention_s,
            max_bytes: cfg.max_bytes,
            quota_bytes: cfg.quota_bytes(),
            max_clip_bytes: cfg.max_clip_bytes,
            chunk_bytes: cfg.chunk_bytes(),
            chunk_files: 0,
            fs_free_bytes: None,
            fs_total_bytes: None,
            min_free_bytes: None,
            paused: false,
            pauses: 0,
            paused_samples: 0,
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
            failed_samples: 0,
            error: None,
        }
    }
}

/// The samples a clip selects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipRange {
    /// The samples whose sample-clock time lies in `[t0_ns, t1_ns)`, Unix ns.
    Time {
        /// Start (inclusive).
        t0_ns: i64,
        /// End (exclusive).
        t1_ns: i64,
    },
    /// Stream indices `[start, end)` (exact whatever the rate).
    Index {
        /// First index.
        start: u64,
        /// One past the last index.
        end: u64,
    },
}

impl ClipRange {
    /// `[t0_s, t1_s)` Unix seconds, each converted once to integer ns by `round(s · 10⁹)` (half
    /// away from zero). `None` unless `0 ≤ t0_s < t1_s < 9.2 × 10⁹` and the ns differ. An f64 near
    /// the present epoch resolves only ≈ 240 ns, so exact ranges use `Time` ns or `Index`.
    pub fn from_unix_s(t0_s: f64, t1_s: f64) -> Option<Self> {
        let ok = |s: f64| s.is_finite() && (0.0..9.2e9).contains(&s);
        if !(ok(t0_s) && ok(t1_s)) {
            return None;
        }
        let (t0_ns, t1_ns) = ((t0_s * 1e9).round() as i64, (t1_s * 1e9).round() as i64);
        (t0_ns < t1_ns).then_some(Self::Time { t0_ns, t1_ns })
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
    /// End of the last sample, Unix ns.
    pub t1_ns: i64,
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

type Span = (Arc<File>, u64, u64);

/// A clip selected under the index lock, written without it ([`IqBuffer::plan_clip`]). It holds
/// read handles of its chunks, so evicted chunks stay readable (and on disk) until it is written
/// or dropped.
pub struct ClipPlan {
    plan: Vec<(ClipPiece, Vec<Span>)>,
    samples: u64,
}

impl ClipPlan {
    /// Samples the clip holds.
    pub fn samples(&self) -> u64 {
        self.samples
    }

    /// Data bytes the clip holds (ci8).
    pub fn bytes(&self) -> u64 {
        self.samples * BYTES_PER_SAMPLE
    }

    /// Writes the clip's ci8 data to `out` and describes it.
    pub fn write(self, out: &mut dyn Write) -> Result<Clip, ClipError> {
        let mut buf = vec![0u8; 1 << 20];
        for (_, reads) in &self.plan {
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
        let pieces: Vec<ClipPiece> = self.plan.into_iter().map(|(p, _)| p).collect();
        let (first, last) = (&pieces[0], &pieces[pieces.len() - 1]);
        Ok(Clip {
            samples: self.samples,
            sample_rate_hz: first.provenance.tune.sample_rate_hz,
            t0_ns: first.t_ns,
            t1_ns: last.t1_ns,
            pieces,
        })
    }
}

impl IqBuffer {
    /// Opens (creating) `dir`, deleting chunk files a previous process left.
    pub fn open(dir: &Path, cfg: IqBufferConfig) -> io::Result<(Self, IqBufferWriter)> {
        Self::open_with(dir, cfg, Arc::new(OsHooks))
    }

    /// [`Self::open`] with filesystem probes `hooks`.
    pub fn open_with(
        dir: &Path,
        cfg: IqBufferConfig,
        hooks: Arc<dyn IqBufferHooks>,
    ) -> io::Result<(Self, IqBufferWriter)> {
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
                quota_bytes: cfg.quota_bytes(),
                hooks,
                state: Mutex::new(State::default()),
            }),
        };
        let writer = IqBufferWriter {
            buffer: buffer.clone(),
            file: None,
            room: 0,
            backoff: Duration::ZERO,
            retry_at: None,
        };
        Ok((buffer, writer))
    }

    /// The configuration.
    pub fn config(&self) -> IqBufferConfig {
        self.inner.cfg
    }

    /// The space of the filesystem holding `path` (through the buffer's hooks).
    pub fn space_at(&self, path: &Path) -> io::Result<FsSpace> {
        self.inner.hooks.fs_space(path)
    }

    /// The status: segments overlapping `[t0_ns, t1_ns)` (whole buffer when `None`), the newest
    /// `limit` of them.
    pub fn status(&self, t0_ns: Option<i64>, t1_ns: Option<i64>, limit: usize) -> IqBufferStatus {
        let space = self.inner.hooks.fs_space(&self.inner.dir).ok();
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
                    t0_ns: g.t0_ns(),
                    t1_ns: g.t1_ns(),
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
            retention_s: self.inner.cfg.retention_s,
            max_bytes: self.inner.cfg.max_bytes,
            quota_bytes: self.inner.quota_bytes,
            max_clip_bytes: self.inner.cfg.max_clip_bytes,
            chunk_bytes: self.inner.chunk_bytes,
            chunk_files: st.chunks.len(),
            fs_free_bytes: space.map(|s| s.free),
            fs_total_bytes: space.map(|s| s.total),
            min_free_bytes: space.map(|s| self.inner.cfg.free_floor(s.total)),
            paused: st.paused,
            pauses: c.pauses,
            paused_samples: c.paused_samples,
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
            failed_samples: c.failed_samples,
            error: st.error.clone(),
        }
    }

    /// Writes the buffered samples in `range` (segments whose tuned window overlaps `band` when
    /// given) to `out` as ci8 and describes them. At most `max_bytes` of data.
    pub fn export_clip(
        &self,
        range: ClipRange,
        band: Option<(f64, f64)>,
        max_bytes: u64,
        out: &mut dyn Write,
    ) -> Result<Clip, ClipError> {
        let plan = self.plan_clip(range, band)?;
        if plan.bytes() > max_bytes {
            return Err(ClipError::TooLarge {
                bytes: plan.bytes(),
            });
        }
        plan.write(out)
    }

    /// Selects the buffered samples in `range` (and `band`) without reading them.
    pub fn plan_clip(
        &self,
        range: ClipRange,
        band: Option<(f64, f64)>,
    ) -> Result<ClipPlan, ClipError> {
        let mut plan: Vec<(ClipPiece, Vec<Span>)> = Vec::new();
        let mut sample_start = 0u64;
        {
            let st = lock(&self.inner.state);
            for seg in st.segments.iter().filter(|s| s.samples > 0) {
                let tune = &seg.start.provenance.tune;
                if let Some((lo, hi)) = band {
                    let half = tune.sample_rate_hz / 2.0;
                    if tune.center_hz + half <= lo || tune.center_hz - half >= hi {
                        continue;
                    }
                }
                let (j0, j1) = match range {
                    ClipRange::Time { t0_ns, t1_ns } => {
                        (seg.offset_at(t0_ns), seg.offset_at(t1_ns))
                    }
                    ClipRange::Index { start, end } => (seg.offset_of(start), seg.offset_of(end)),
                };
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
                        t1_ns: seg.t_ns(seg.global_index + j1),
                        segment: seg.id,
                        provenance: seg.start.provenance.clone(),
                        content_class: seg.start.content_class,
                    },
                    reads,
                ));
                sample_start += n;
            }
        }
        if plan.is_empty() {
            return Err(ClipError::Empty);
        }
        Ok(ClipPlan {
            plan,
            samples: sample_start,
        })
    }

    /// Advances the floor and deletes chunks for both quotas; returns files to delete.
    fn evict(&self, st: &mut State) -> Vec<PathBuf> {
        let mut dead = Vec::new();
        // Disk bytes: whole oldest chunks (never the one being written).
        while st.disk_bytes > self.inner.quota_bytes && st.chunks.len() > 1 {
            let c = st.chunks.pop_front().expect("a chunk");
            st.disk_bytes -= c.bytes;
            st.counts.evicted_chunks += 1;
            st.log_floor = st.log_floor.max(c.log_start + c.bytes);
            dead.push(c.path);
        }
        // Duration on the sample clock, to the sample.
        let max_ns = (self.inner.cfg.retention_s * 1e9) as i64;
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

    /// Whether writing is paused below the free-space floor.
    pub fn is_paused(&self) -> bool {
        lock(&self.buffer.inner.state).paused
    }

    /// Appends interleaved ci8 `bytes` (whole samples) to the open segment.
    ///
    /// - **Paused** (another chunk would go below the free-space floor): the rest is not stored
    ///   (`paused_samples`), the segment ends, and `Ok` is returned.
    /// - **Write error**: the unwritten samples are counted (`failed_samples`), the segment ends,
    ///   the error is kept and returned, and writes back off; an append during the back-off
    ///   stores nothing, touches no file and returns an error.
    pub fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        debug_assert!(bytes.len() % 2 == 0);
        let bytes = &bytes[..bytes.len() & !1];
        if bytes.is_empty() {
            return Ok(());
        }
        let inner = Arc::clone(&self.buffer.inner);
        if !lock(&inner.state).open {
            return Err(io::Error::other("append without an open segment"));
        }
        if let Some(at) = self.retry_at {
            if Instant::now() < at {
                let mut st = lock(&inner.state);
                st.counts.failed_samples += bytes.len() as u64 / BYTES_PER_SAMPLE;
                st.open = false;
                return Err(io::Error::other("backing off after a failed write"));
            }
            self.retry_at = None;
        }
        let mut rest = bytes;
        while !rest.is_empty() {
            match self.write_some(&mut rest) {
                Ok(true) => {}
                Ok(false) => {
                    let mut st = lock(&inner.state);
                    st.counts.paused_samples += rest.len() as u64 / BYTES_PER_SAMPLE;
                    st.open = false;
                    return Ok(());
                }
                Err(e) => {
                    // A chunk keeps only what its index entry counts.
                    let accounted = lock(&inner.state).chunks.back().map_or(0, |c| c.bytes);
                    if let Some(f) = self.file.take() {
                        let _ = f.set_len(accounted);
                    }
                    self.room = 0;
                    self.backoff = if self.backoff.is_zero() {
                        RETRY_BACKOFF_MIN
                    } else {
                        (self.backoff * 2).min(RETRY_BACKOFF_MAX)
                    };
                    self.retry_at = Some(Instant::now() + self.backoff);
                    let mut st = lock(&inner.state);
                    st.counts.write_errors += 1;
                    st.counts.failed_samples += rest.len() as u64 / BYTES_PER_SAMPLE;
                    st.error = Some(e.to_string());
                    st.open = false;
                    return Err(e);
                }
            }
        }
        self.backoff = Duration::ZERO;
        Ok(())
    }

    /// Writes some of `rest`; `Ok(false)`: paused (nothing written).
    fn write_some(&mut self, rest: &mut &[u8]) -> io::Result<bool> {
        let inner = Arc::clone(&self.buffer.inner);
        let hooks = Arc::clone(&inner.hooks);
        let n;
        let need_chunk = self.file.is_none() || self.room == 0;
        if need_chunk {
            self.file = None;
            // The free-space floor: an unknown space carries on (a full disk then fails a write).
            if let Ok(space) = hooks.fs_space(&inner.dir) {
                let need = inner
                    .cfg
                    .free_floor(space.total)
                    .saturating_add(inner.chunk_bytes);
                let mut st = lock(&inner.state);
                if space.free < need {
                    if !st.paused {
                        st.paused = true;
                        st.counts.pauses += 1;
                    }
                    return Ok(false);
                }
                st.paused = false;
            }
            n = (inner.chunk_bytes as usize).min(rest.len());
            let seq = {
                let mut st = lock(&inner.state);
                st.next_chunk_seq += 1;
                st.next_chunk_seq - 1
            };
            let path = inner.dir.join(format!("{seq:016}.ci8"));
            // A chunk enters the index only once its first write succeeded.
            let created = (|| {
                let mut file = OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(&path)?;
                hooks.before_write(n)?;
                file.write_all(&rest[..n])?;
                let read = File::open(&path)?;
                Ok::<_, io::Error>((file, read))
            })();
            let (file, read) = match created {
                Ok(f) => f,
                Err(e) => {
                    let _ = fs::remove_file(&path);
                    return Err(e);
                }
            };
            let mut st = lock(&inner.state);
            let log_start = st.log_end;
            st.chunks.push_back(Chunk {
                log_start,
                bytes: 0,
                file: Arc::new(read),
                path,
            });
            self.file = Some(file);
            self.room = inner.chunk_bytes;
        } else {
            n = (self.room as usize).min(rest.len());
            hooks.before_write(n)?;
            self.file
                .as_mut()
                .expect("a chunk file")
                .write_all(&rest[..n])?;
        }
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
        Ok(true)
    }
}

impl Drop for IqBufferWriter {
    fn drop(&mut self) {
        self.end_segment();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    use hk_model::{ClockSource, TimestampMethod, Tune};

    use super::*;

    fn cfg(retention_s: f64, max_bytes: Option<u64>) -> IqBufferConfig {
        IqBufferConfig {
            enabled: Some(true),
            retention_s,
            max_bytes,
            min_free_bytes: Some(0),
            ..IqBufferConfig::default()
        }
    }

    /// A filesystem whose free space and write failures the test sets.
    struct Faults {
        fail: AtomicBool,
        free: AtomicU64,
        writes: AtomicU64,
    }

    const TOTAL: u64 = 1 << 40;

    impl IqBufferHooks for Faults {
        fn fs_space(&self, _: &Path) -> io::Result<FsSpace> {
            Ok(FsSpace {
                free: self.free.load(Ordering::SeqCst),
                total: TOTAL,
            })
        }

        fn before_write(&self, _: usize) -> io::Result<()> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            if self.fail.load(Ordering::SeqCst) {
                Err(io::Error::from_raw_os_error(libc::ENOSPC))
            } else {
                Ok(())
            }
        }
    }

    fn faults() -> Arc<Faults> {
        Arc::new(Faults {
            fail: AtomicBool::new(false),
            free: AtomicU64::new(100 << 30),
            writes: AtomicU64::new(0),
        })
    }

    #[test]
    fn capture_buffer_retention_and_sizes_parse_and_set_the_quota() {
        for (t, s) in [
            ("90s", 90.0),
            ("2m", 120.0),
            ("1h", 3600.0),
            ("1.5h", 5400.0),
            ("45", 45.0),
            ("0", 0.0),
            ("off", 0.0),
            ("OFF", 0.0),
        ] {
            assert_eq!(parse_duration_s(t), Ok(s), "{t}");
        }
        for bad in ["", "abc", "5x", "-1s", "2 fortnights"] {
            assert!(parse_duration_s(bad).is_err(), "{bad}");
        }
        for (t, b) in [
            ("512MiB", 512u64 << 20),
            ("8GiB", 8 << 30),
            ("64KiB", 64 << 10),
            ("1048576", 1 << 20),
            ("1.5GiB", 3 << 29),
            ("500MB", 500_000_000),
        ] {
            assert_eq!(parse_size_bytes(t), Ok(b), "{t}");
        }
        for bad in ["", "MiB", "-5", "12 parsecs", "99999999999TiB"] {
            assert!(parse_size_bytes(bad).is_err(), "{bad}");
        }
        // Implied: retention × highest rate × 2 bytes/sample; the cap wins when smaller.
        let mut c = IqBufferConfig::default();
        assert_eq!(c.retention_s, 120.0);
        assert_eq!(c.quota_bytes(), 120 * 20_000_000 * 2);
        c.max_rate_hz = Some(2.4e6);
        assert_eq!(c.quota_bytes(), 576_000_000);
        c.max_bytes = Some(512 << 20);
        assert_eq!(c.quota_bytes(), 512 << 20);
        c.max_bytes = Some(1 << 30);
        assert_eq!(c.quota_bytes(), 576_000_000);
        c.max_bytes = Some(1024);
        assert_eq!(c.quota_bytes(), 2 * MIN_CHUNK_BYTES, "at least two chunks");
        assert!(c.active(false));
        c.max_bytes = Some(0);
        assert!(!c.active(false));
        c.max_bytes = None;
        c.retention_s = 0.0;
        assert!(!c.active(false));
        // The default free-space floor: 10 % within 2..8 GiB.
        let d = IqBufferConfig::default();
        assert_eq!(d.free_floor(1 << 40), 8 << 30);
        assert_eq!(d.free_floor(64 << 30), (64u64 << 30) / 10);
        assert_eq!(d.free_floor(8 << 30), 2 << 30);
    }

    #[test]
    fn capture_buffer_failed_writes_leave_no_chunks_and_back_off() {
        let dir = tmp("enospc");
        let hooks = faults();
        let (buf, mut w) =
            IqBuffer::open_with(&dir, cfg(1e9, Some(2 * MIN_CHUNK_BYTES)), hooks.clone()).unwrap();
        hooks.fail.store(true, Ordering::SeqCst);
        let blocks = 2000u64;
        for i in 0..blocks {
            w.begin_segment(start(i * 1024, i as i64 * 1_000_000, 100e6, 1e6));
            assert!(w.append(&ramp(i * 1024, 1024)).is_err());
        }
        let s = buf.status(None, None, 10);
        assert_eq!((s.chunk_files, s.disk_bytes, s.samples), (0, 0, 0), "{s:?}");
        assert_eq!(
            fs::read_dir(&dir).unwrap().count(),
            0,
            "no chunk file stays"
        );
        let attempts = hooks.writes.load(Ordering::SeqCst);
        assert!(
            (1..=3).contains(&s.write_errors) && attempts == s.write_errors,
            "backed off: {attempts} attempts for {blocks} blocks, {s:?}"
        );
        assert_eq!(s.failed_samples, blocks * 1024);
        assert_eq!(s.segments_total, 0);
        assert!(s.error.is_some());
        // Writable again after the back-off: one chunk, exactly the data.
        hooks.fail.store(false, Ordering::SeqCst);
        std::thread::sleep(RETRY_BACKOFF_MIN * (1 << s.write_errors) + Duration::from_millis(50));
        w.begin_segment(start(1 << 30, 5_000_000_000, 100e6, 1e6));
        w.append(&ramp(0, 1024)).unwrap();
        let s = buf.status(None, None, 10);
        assert_eq!((s.chunk_files, s.samples), (1, 1024), "{s:?}");
        // A failure in a chunk already indexed keeps the chunk at its indexed size.
        hooks.fail.store(true, Ordering::SeqCst);
        assert!(w.append(&ramp(1024, 1024)).is_err());
        let s = buf.status(None, None, 10);
        assert_eq!((s.chunk_files, s.samples, s.disk_bytes), (1, 1024, 2048));
        let files: Vec<_> = fs::read_dir(&dir).unwrap().flatten().collect();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].metadata().unwrap().len(), 2048);
        drop(w);
        drop(buf);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn capture_buffer_pauses_below_the_free_space_floor_and_resumes() {
        let dir = tmp("floor");
        let hooks = faults();
        let mut c = cfg(1e9, Some(4 * MIN_CHUNK_BYTES));
        c.min_free_bytes = Some(1 << 30);
        let (buf, mut w) = IqBuffer::open_with(&dir, c, hooks.clone()).unwrap();
        let per_chunk = MIN_CHUNK_BYTES / 2;
        w.begin_segment(start(0, 0, 100e6, 1e6));
        w.append(&ramp(0, per_chunk)).unwrap();
        // Another chunk would leave less than the floor free.
        hooks.free.store(1 << 30, Ordering::SeqCst);
        w.append(&ramp(per_chunk, 100)).unwrap();
        assert!(!w.is_open() && w.is_paused());
        for i in 0..50 {
            w.begin_segment(start(per_chunk + 100 + i * 100, 0, 100e6, 1e6));
            w.append(&ramp(0, 100)).unwrap();
        }
        let s = buf.status(None, None, 10);
        assert!(s.paused, "{s:?}");
        assert_eq!((s.pauses, s.paused_samples), (1, 5100));
        assert_eq!((s.chunk_files, s.samples), (1, per_chunk));
        assert_eq!(
            (s.fs_free_bytes, s.fs_total_bytes, s.min_free_bytes),
            (Some(1 << 30), Some(TOTAL), Some(1 << 30))
        );
        assert_eq!((s.write_errors, s.failed_samples), (0, 0));
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        // Room again: resumes into a new segment after the gap.
        hooks.free.store(100 << 30, Ordering::SeqCst);
        w.begin_segment(start(per_chunk + 10_000, 0, 100e6, 1e6));
        w.append(&ramp(0, 100)).unwrap();
        let s = buf.status(None, None, 10);
        assert!(!s.paused, "{s:?}");
        assert_eq!((s.pauses, s.chunk_files, s.segments_total), (1, 2, 2));
        assert_eq!(s.gaps[0].samples, 10_000);
        drop(w);
        drop(buf);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn capture_buffer_index_and_ns_ranges_are_exact_at_a_fractional_ns_rate() {
        let dir = tmp("exact");
        let fs_hz = 2.4e6; // 416.67 ns per sample
        let (buf, mut w) = IqBuffer::open(&dir, cfg(600.0, Some(1 << 30))).unwrap();
        let t0 = 1_757_000_000_123_456_789i64;
        w.begin_segment(start(0, t0, 100e6, fs_hz));
        w.append(&ramp(0, 60_000)).unwrap();
        let s = buf.status(None, None, 10);
        assert_eq!(s.segments[0].t0_ns, t0);
        assert_eq!(
            s.segments[0].t1_ns,
            t0 + 25_000_000,
            "60 000 samples = 25 ms"
        );
        let export = |range| {
            let mut out = Vec::new();
            buf.export_clip(range, None, u64::MAX, &mut out)
                .map(|c| (c, out))
        };
        let (clip, data) = export(ClipRange::Index {
            start: 1234,
            end: 51_234,
        })
        .unwrap();
        assert_eq!((clip.samples, clip.pieces[0].global_index), (50_000, 1234));
        assert_eq!(data, ramp(1234, 50_000));
        // The clip's own ns bounds select exactly the same samples.
        let (again, data2) = export(ClipRange::Time {
            t0_ns: clip.t0_ns,
            t1_ns: clip.t1_ns,
        })
        .unwrap();
        assert_eq!(
            (again.samples, again.pieces[0].global_index),
            (50_000, 1234)
        );
        assert_eq!(data2, data);
        // Each sample's time (round half up of k · 10¹² / 2.4 × 10⁹ mHz) selects exactly it.
        for k in [1u64, 2, 3, 5, 6, 7, 11, 12, 13, 59_998] {
            let t =
                t0 + ((2 * k as i128 * 1_000_000_000_000 + 2_400_000_000) / 4_800_000_000) as i64;
            let (c, d) = export(ClipRange::Time {
                t0_ns: t,
                t1_ns: t + 1,
            })
            .unwrap();
            assert_eq!(
                (c.samples, c.pieces[0].global_index, c.t0_ns),
                (1, k, t),
                "{k}"
            );
            assert_eq!(d, ramp(k, 1));
            let (c, _) = export(ClipRange::Time {
                t0_ns: t + 1,
                t1_ns: i64::MAX,
            })
            .unwrap();
            assert_eq!(c.pieces[0].global_index, k + 1, "{k}");
        }
        // Extreme bounds neither overflow nor panic.
        let (all, _) = export(ClipRange::Time {
            t0_ns: i64::MIN,
            t1_ns: i64::MAX,
        })
        .unwrap();
        assert_eq!(all.samples, 60_000);
        assert!(matches!(
            export(ClipRange::Index {
                start: u64::MAX - 1,
                end: u64::MAX
            }),
            Err(ClipError::Empty)
        ));
        // Seconds convert once to whole ns; out-of-range or empty ranges are refused.
        assert_eq!(
            ClipRange::from_unix_s(1.5, 2.25),
            Some(ClipRange::Time {
                t0_ns: 1_500_000_000,
                t1_ns: 2_250_000_000
            })
        );
        for (a, b) in [
            (-8.9e9, 1.0),
            (-1e-9, 1.0),
            (2.0, 1.0),
            (1.0, 1.0),
            (f64::NAN, 1.0),
            (1.0, 9.3e9),
        ] {
            assert_eq!(ClipRange::from_unix_s(a, b), None, "{a} {b}");
        }
        drop(w);
        let _ = fs::remove_dir_all(&dir);
    }

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
        let (buf, mut w) = IqBuffer::open(&dir, cfg(2.0, Some(1 << 30))).unwrap();
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
            .export_clip(
                ClipRange::Time {
                    t0_ns: 1_200_000_000,
                    t1_ns: 2_300_000_000,
                },
                None,
                u64::MAX,
                &mut out,
            )
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
                ClipRange::Time {
                    t0_ns: 0,
                    t1_ns: i64::MAX / 2,
                },
                Some((100.9e6, 101.1e6)),
                u64::MAX,
                &mut out,
            )
            .unwrap();
        assert_eq!((clip.pieces.len(), clip.samples), (1, 1000));
        assert!(matches!(
            buf.export_clip(
                ClipRange::Time {
                    t0_ns: 10_000_000_000,
                    t1_ns: 11_000_000_000,
                },
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
        let (buf, mut w) = IqBuffer::open(&dir, cfg(1e9, Some(2 * MIN_CHUNK_BYTES))).unwrap();
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
        let all = ClipRange::Time {
            t0_ns: 0,
            t1_ns: i64::MAX / 2,
        };
        assert!(matches!(
            buf.export_clip(all, None, u64::MAX, &mut Vec::new()),
            Err(ClipError::MixedRates { .. })
        ));
        let first = ClipRange::Time {
            t0_ns: 0,
            t1_ns: 5_000_000_000,
        };
        assert!(matches!(
            buf.export_clip(first, None, 0, &mut Vec::new()),
            Err(ClipError::TooLarge { .. })
        ));
        // The run's end deletes the chunk files.
        drop(w);
        drop(buf);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        let _ = fs::remove_dir_all(&dir);
    }
}
