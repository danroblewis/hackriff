//! Rolling IQ capture buffer (T-157, ADR-0013 §4 API gap 1) on a **pre-allocated, persistent
//! on-disk ring** (T-178, ADR-0014 `docs/adr/0014-iq-capture-ring.md`): the always-on raw-IQ
//! history behind the Capture timeline ("reviewing N ago / LIVE", "export clip from the buffer").
//! It survives restarts.
//!
//! - **Storage** (`<data dir>/iqbuffer/`): three files whose number and sizes are fixed once
//!   opened. `ring.ci8` holds `slot_count` fixed-size **slots** of [`IqBufferConfig::chunk_bytes`]
//!   each (interleaved ci8, 2 bytes/sample), allocated up front ([`preallocate`]: `F_PREALLOCATE`
//!   on macOS, `fallocate` on Linux, sparse elsewhere) **and flushed as it is allocated**, because
//!   the filesystem defers a reservation's real cost to the first fsync of the file and that bill
//!   must not land on a writer later ([`allocate_stepwise`], T-536). `ring.journal` is an
//!   append-only journal of CRC-framed records (header, run, slot open, slot seal, segment start), rewritten as a compact
//!   snapshot on open and whenever it outgrows [`JOURNAL_COMPACT_MIN`]. `ring.lock` carries the
//!   `flock` of the one buffer using the ring.
//! - **Logical log.** Bytes are addressed on a monotonic logical log; logical slot `L` covers
//!   `[L·S, (L+1)·S)` and lives at the ring position its `open` record names. A new slot takes an
//!   unused position if there is one, otherwise the position of the **oldest** slot, which is
//!   evicted first (the floor passes its end) and then **overwritten in place**: disk usage never
//!   changes and every position is rewritten in turn (even flash wear).
//! - **Durability and torn writes.** `open {slot, pos}` is journalled (fsync) before the slot's
//!   first byte. A checkpoint (every [`CHECKPOINT_INTERVAL`], at a full slot and at the end of a
//!   run) fsyncs the ring, journals the new segment starts and `seal {slot, bytes, crc, floor}`
//!   (CRC-32 of the slot's first `bytes`), and fsyncs the journal. Recovery keeps only what seals
//!   cover, stops reading the journal at the first torn or corrupt record, re-verifies the CRC of
//!   the newest sealed slots, and drops a slot that fails together with everything newer.
//! - **Segments.** A segment is a contiguous run of samples under one provenance: the writer starts
//!   a new one on every provenance change (retune, rate, gain, filter, overload), source gap, ring
//!   overrun or discontinuity flag, so retune boundaries are always segment boundaries. Each keeps
//!   its first sample's stream index (`global_index`), **sample-clock** time (ADR-0012 §0) and the
//!   **run** (one per open of the ring) whose stream indices it uses; sample `i` of the segment is
//!   at `t0 + i / fs`.
//! - **Retention: oldest first.** Two limits, whichever is hit first: the ring (a reused slot
//!   evicts its previous contents) and duration (the retained span on the sample clock, trimmed to
//!   the sample by advancing the log floor, which seals persist).
//! - **Never blocks capture.** Only the writer thread (a ring reader) writes the ring and journal;
//!   the index mutex is held for bookkeeping only, never across I/O. A clip export plans under the
//!   mutex and reads outside it, re-checking the floor after every read: data overwritten meanwhile
//!   fails the clip ([`ClipError::Evicted`]) instead of being exported.
//! - **Free space.** Allocation keeps the free-space floor ([`IqBufferConfig::free_floor`]): with
//!   too little room the ring is **shrunk** to the whole slots that fit, or **refused** below two
//!   ([`AllocationRefused`]). A ring the filesystem could not reserve (sparse) also **pauses**
//!   writing while starting another slot would leave less than the floor free.
//! - **Exact times.** Sample `k` of a segment is at `t0_ns + round_half_up(k · 10¹² / fs_mHz)` ns
//!   (the rate in integer millihertz, i128 arithmetic): a time range selects exactly the samples
//!   whose time lies in `[t0_ns, t1_ns)`, and a stream-index range ([`ClipRange::Index`]) selects
//!   exactly its indices within one run.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::FileExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use hk_model::{ContentClass, Provenance};
use serde::{Deserialize, Serialize};

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
/// Largest slot.
pub const MAX_CHUNK_BYTES: u64 = 64 << 20;
/// Smallest slot (and the smallest honoured quota is two of them).
pub const MIN_CHUNK_BYTES: u64 = 64 << 10;
/// Bytes per stored sample (ci8 I, Q).
pub const BYTES_PER_SAMPLE: u64 = 2;
/// Directory name under the data directory.
pub const DIR_NAME: &str = "iqbuffer";
/// The ring of slots.
pub const RING_FILE: &str = "ring.ci8";
/// The journal.
pub const JOURNAL_FILE: &str = "ring.journal";
/// The lock file.
pub const LOCK_FILE: &str = "ring.lock";
/// On-disk format version (journal header).
pub const RING_VERSION: u32 = 1;
/// Longest time between checkpoints while writing.
pub const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(1);
/// The journal is compacted once it is larger than this and than 4× its last snapshot.
pub const JOURNAL_COMPACT_MIN: u64 = 1 << 20;
/// Newest sealed slots whose CRC recovery re-reads before trusting the older ones.
pub const RECOVERY_VERIFY_SLOTS: usize = 4;
/// Default and largest number of segments a status lists.
pub const STATUS_SEGMENTS_DEFAULT: usize = 1000;
/// Largest `limit` of a status.
pub const STATUS_SEGMENTS_MAX: usize = 10_000;
const RING_MAGIC: &str = "hackriff-iq-ring";
const JOURNAL_RECORD_MAX: usize = 16 << 20;

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

    /// Slot size: a sixteenth of the quota within [`MIN_CHUNK_BYTES`]..[`MAX_CHUNK_BYTES`], whole
    /// samples.
    pub fn chunk_bytes(&self) -> u64 {
        (self.raw_quota() / 16).clamp(MIN_CHUNK_BYTES, MAX_CHUNK_BYTES) & !1
    }

    fn raw_quota(&self) -> u64 {
        self.max_bytes
            .map_or(self.implied_bytes(), |m| m.min(self.implied_bytes()))
    }

    /// The disk quota: `min(implied, max_bytes)`, at least two slots.
    pub fn quota_bytes(&self) -> u64 {
        self.raw_quota().max(2 * self.chunk_bytes())
    }

    /// Slots of a ring holding the quota: `⌊quota / slot⌋`, at least 2.
    pub fn slot_count(&self) -> u64 {
        (self.quota_bytes() / self.chunk_bytes()).max(2)
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

/// Sizes `file` to `len` bytes, reserving the blocks where the filesystem can: `fallocate` on
/// Linux, `F_PREALLOCATE` then `ftruncate` on macOS (neither writes the data, so a large ring
/// allocates in milliseconds). `Ok(true)`: reserved; `Ok(false)`: only sized (sparse).
#[allow(clippy::unnecessary_cast, clippy::useless_conversion)]
pub fn preallocate(file: &File, len: u64) -> io::Result<bool> {
    let current = file.metadata()?.len();
    if len <= current {
        file.set_len(len)?;
        return Ok(true);
    }
    #[cfg(target_os = "linux")]
    {
        // SAFETY: a valid open descriptor; mode 0 allocates and extends the size.
        let r = unsafe { libc::fallocate(file.as_raw_fd(), 0, 0, len as libc::off_t) };
        if r == 0 {
            return Ok(true);
        }
        let e = io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::ENOSPC) {
            return Err(e);
        }
        file.set_len(len)?;
        Ok(false)
    }
    #[cfg(target_os = "macos")]
    {
        let mut reserved = false;
        for flags in [libc::F_ALLOCATECONTIG, libc::F_ALLOCATEALL] {
            let mut store = libc::fstore_t {
                fst_flags: flags,
                fst_posmode: libc::F_PEOFPOSMODE,
                fst_offset: 0,
                fst_length: (len - current) as libc::off_t,
                fst_bytesalloc: 0,
            };
            // SAFETY: a valid open descriptor and a valid, writable `fstore_t`.
            if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_PREALLOCATE, &mut store) } != -1 {
                reserved = true;
                break;
            }
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::ENOSPC) && flags == libc::F_ALLOCATEALL {
                return Err(e);
            }
        }
        file.set_len(len)?;
        Ok(reserved)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = file.as_raw_fd();
        file.set_len(len)?;
        Ok(false)
    }
}

/// Commits a completed allocation step to the **filesystem** — not to the drive (T-536).
///
/// [`allocate_stepwise`] explains why the flush has to happen at all. It is a plain `fsync`, and
/// deliberately **not** [`File::sync_data`], which on macOS is `F_FULLFSYNC`: a barrier on the
/// whole device that every other thread's I/O then queues behind. What has to be committed here is
/// the *extent map* the reservation promised, so that a later fsync of this file is not the one
/// that pays for it; there is no data in the ring yet, so there is nothing whose durability needs
/// the drive's cache flushed — that is the checkpoint's job, and it still uses `sync_data`.
/// Measured on APFS for a 4.8 GB ring: per-step `fsync` opens in 1.32 s and leaves later fsyncs at
/// 0–1 ms; per-step `F_FULLFSYNC` opens in 2.02 s, leaves them at 8–11 ms, and stalls the rest of
/// the process while it runs.
pub fn sync_allocation(file: &File) -> io::Result<()> {
    #[cfg(unix)]
    {
        // SAFETY: a valid open descriptor.
        if unsafe { libc::fsync(file.as_raw_fd()) } == 0 {
            return Ok(());
        }
        Err(io::Error::last_os_error())
    }
    #[cfg(not(unix))]
    {
        file.sync_data()
    }
}

/// The ring would not fit above the free-space floor even at two slots (the buffer is refused).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AllocationRefused {
    /// Bytes the smallest ring needs.
    pub need_bytes: u64,
    /// Free bytes of the filesystem.
    pub free_bytes: u64,
    /// The free-space floor.
    pub floor_bytes: u64,
}

impl std::fmt::Display for AllocationRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "allocation refused: the IQ capture ring needs {} bytes above the {}-byte free-space \
             floor and {} bytes are free",
            self.need_bytes, self.floor_bytes, self.free_bytes
        )
    }
}

impl std::error::Error for AllocationRefused {}

/// Whether an [`IqBuffer::open`] error is an [`AllocationRefused`].
pub fn is_allocation_refused(e: &io::Error) -> bool {
    e.get_ref()
        .is_some_and(|x| x.downcast_ref::<AllocationRefused>().is_some())
}

/// Another process (or another buffer in this one) holds the ring's `flock`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RingLocked {
    /// The lock file.
    pub lock_path: PathBuf,
}

impl std::fmt::Display for RingLocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the IQ capture ring is in use by another process ({} is locked)",
            self.lock_path.display()
        )
    }
}

impl std::error::Error for RingLocked {}

/// Whether an [`IqBuffer::open`] error is a [`RingLocked`].
pub fn is_ring_locked(e: &io::Error) -> bool {
    e.get_ref()
        .is_some_and(|x| x.downcast_ref::<RingLocked>().is_some())
}

/// The ring was written by a newer on-disk format than this build reads: it is left untouched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RingIncompatible {
    /// The version in the ring's journal header.
    pub found: u64,
    /// The newest version this build reads ([`RING_VERSION`]).
    pub supported: u32,
}

impl std::fmt::Display for RingIncompatible {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the IQ capture ring was written by a newer format (version {}; this build reads up to \
             {}): it is left untouched and the buffer is disabled (move or delete the iqbuffer \
             directory to start a new ring)",
            self.found, self.supported
        )
    }
}

impl std::error::Error for RingIncompatible {}

/// Whether an [`IqBuffer::open`] error is a [`RingIncompatible`].
pub fn is_ring_incompatible(e: &io::Error) -> bool {
    e.get_ref()
        .is_some_and(|x| x.downcast_ref::<RingIncompatible>().is_some())
}

/// Allocation grows the ring file in steps of about this many bytes (whole slots), reporting
/// progress and checking for cancellation between steps.
pub const ALLOCATION_STEP_BYTES: u64 = 1 << 30;

/// Filesystem probes of the buffer, replaceable for tests (a full disk, a slow writer, a
/// filesystem without preallocation).
pub trait IqBufferHooks: Send + Sync {
    /// The space of the filesystem holding `path`.
    fn fs_space(&self, path: &Path) -> io::Result<FsSpace> {
        fs_space(path)
    }

    /// Called before every ring write of `bytes`; an error fails that write.
    fn before_write(&self, _bytes: usize) -> io::Result<()> {
        Ok(())
    }

    /// Sizes the ring file ([`preallocate`]).
    fn preallocate(&self, file: &File, len: u64) -> io::Result<bool> {
        preallocate(file, len)
    }

    /// Commits one completed allocation step ([`allocate_stepwise`]). **This is where the cost of
    /// a large ring is paid** — see that function — so it runs on the thread doing the open and
    /// nowhere else.
    fn sync_allocation(&self, file: &File) -> io::Result<()> {
        sync_allocation(file)
    }

    /// Called before every ring fsync; an error fails that fsync (the unsealed bytes are then
    /// poisoned, [`IqBufferWriter::checkpoint`]).
    fn before_sync(&self) -> io::Result<()> {
        Ok(())
    }
}

/// The real filesystem.
pub struct OsHooks;

impl IqBufferHooks for OsHooks {}

/// CRC-32 (IEEE 802.3, reflected) of `data` continuing `crc` (`crc32_update(0, x)` is the CRC of
/// `x`, and `crc32_update(crc32_update(0, a), b)` that of `a ++ b`). Slicing-by-8.
pub fn crc32_update(crc: u32, data: &[u8]) -> u32 {
    static TABLES: OnceLock<[[u32; 256]; 8]> = OnceLock::new();
    let t = TABLES.get_or_init(|| {
        let mut t = [[0u32; 256]; 8];
        for (i, e) in t[0].iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
            *e = c;
        }
        for i in 0..256 {
            for k in 1..8 {
                let p = t[k - 1][i];
                t[k][i] = (p >> 8) ^ t[0][(p & 0xff) as usize];
            }
        }
        t
    });
    let mut c = !crc;
    let mut chunks = data.chunks_exact(8);
    for b in &mut chunks {
        let v = u32::from_le_bytes([b[0], b[1], b[2], b[3]]) ^ c;
        c = t[7][(v & 0xff) as usize]
            ^ t[6][((v >> 8) & 0xff) as usize]
            ^ t[5][((v >> 16) & 0xff) as usize]
            ^ t[4][(v >> 24) as usize]
            ^ t[3][b[4] as usize]
            ^ t[2][b[5] as usize]
            ^ t[1][b[6] as usize]
            ^ t[0][b[7] as usize];
    }
    for &b in chunks.remainder() {
        c = (c >> 8) ^ t[0][((c ^ b as u32) & 0xff) as usize];
    }
    !c
}

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

/// A journal record (framed `[len u32 LE][crc32 u32 LE][JSON]`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "k", rename_all = "snake_case")]
enum Rec {
    /// First record: the geometry.
    Header {
        magic: String,
        version: u32,
        slot_bytes: u64,
        slots: u64,
    },
    /// A buffer opened the ring (its segments' stream indices are this run's).
    Run { run: u64 },
    /// Logical slot `slot` is written at ring position `pos` from here on: any older slot there is
    /// dead.
    Open { slot: u64, pos: u64 },
    /// The first `bytes` of `slot` are durable with CRC-32 `crc`; the log floor is `floor`.
    Seal {
        slot: u64,
        bytes: u64,
        crc: u32,
        floor: u64,
    },
    /// A segment starts at logical byte `log_start`.
    Seg {
        id: u64,
        run: u64,
        log_start: u64,
        global_index: u64,
        t_ns: i64,
        content_class: ContentClass,
        dropped_before: u64,
        provenance: Box<Provenance>,
    },
}

fn encode(recs: &[Rec]) -> Vec<u8> {
    let mut out = Vec::new();
    for r in recs {
        let p = serde_json::to_vec(r).expect("a journal record serialises");
        out.extend_from_slice(&(p.len() as u32).to_le_bytes());
        out.extend_from_slice(&crc32_update(0, &p).to_le_bytes());
        out.extend_from_slice(&p);
    }
    out
}

/// The records of a journal up to its first torn or corrupt one, and that record's offset.
fn decode(data: &[u8]) -> (Vec<Rec>, usize) {
    let mut recs = Vec::new();
    let mut off = 0;
    while off + 8 <= data.len() {
        let n = u32::from_le_bytes(data[off..off + 4].try_into().expect("4 bytes")) as usize;
        let crc = u32::from_le_bytes(data[off + 4..off + 8].try_into().expect("4 bytes"));
        if n > JOURNAL_RECORD_MAX || off + 8 + n > data.len() {
            break;
        }
        let p = &data[off + 8..off + 8 + n];
        if crc32_update(0, p) != crc {
            break;
        }
        match serde_json::from_slice::<Rec>(p) {
            Ok(r) => recs.push(r),
            Err(_) => break,
        }
        off += 8 + n;
    }
    (recs, off)
}

/// A logical slot holding a ring position.
#[derive(Clone, Copy, Debug)]
struct Slot {
    l: u64,
    pos: u64,
    /// Bytes written.
    bytes: u64,
    /// Bytes a seal covers.
    sealed: u64,
    /// CRC-32 of the sealed bytes.
    crc: u32,
}

struct Seg {
    id: u64,
    run: u64,
    /// Logical byte of the segment's first sample (before any eviction).
    start_log: u64,
    /// Logical byte of the first retained sample.
    log_start: u64,
    samples: u64,
    /// Stream index of the first retained sample.
    global_index: u64,
    start: SegmentStart,
    /// Its start is in the journal.
    persisted: bool,
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

    fn rec(&self) -> Rec {
        Rec::Seg {
            id: self.id,
            run: self.run,
            log_start: self.start_log,
            global_index: self.start.global_index,
            t_ns: self.start.t_ns,
            content_class: self.start.content_class,
            dropped_before: self.start.dropped_before,
            provenance: Box::new(self.start.provenance.clone()),
        }
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
    sync_errors: u64,
    poisoned_samples: u64,
}

#[derive(Default)]
struct State {
    /// Slots holding positions, oldest first.
    slots: VecDeque<Slot>,
    /// Unused ring positions.
    free: BTreeSet<u64>,
    segments: VecDeque<Seg>,
    log_floor: u64,
    log_end: u64,
    next_seg_id: u64,
    /// The last segment is still being appended to.
    open: bool,
    /// Writing is paused below the free-space floor (sparse ring only).
    paused: bool,
    counts: Counts,
    error: Option<String>,
}

/// How much of the quota the ring got.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Allocation {
    /// The whole quota.
    Full,
    /// Fewer slots: the quota would not fit above the free-space floor.
    Shrunk,
    /// Not even two slots fit: the run has no buffer.
    Refused,
    /// The ring is being opened and allocated in the background (`allocation_progress`); nothing
    /// is buffered until it completes.
    Allocating,
    /// Another process holds the ring's lock: the run has no buffer.
    Locked,
    /// The ring was written by a newer on-disk format: left untouched, the run has no buffer.
    Incompatible,
}

/// What recovery found when the ring opened.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Recovery {
    segments: usize,
    discarded_slots: u64,
    note: Option<String>,
}

struct Inner {
    dir: PathBuf,
    cfg: IqBufferConfig,
    slot_bytes: u64,
    slot_count: u64,
    quota_bytes: u64,
    allocation: Allocation,
    preallocated: bool,
    run: u64,
    recovery: Recovery,
    hooks: Arc<dyn IqBufferHooks>,
    read: File,
    journal_len: AtomicU64,
    state: Mutex<State>,
    /// Holds the `flock` for the buffer's life.
    _lock: File,
}

/// The buffer's shared index (status and clip export); cheap to clone.
#[derive(Clone)]
pub struct IqBuffer {
    inner: Arc<Inner>,
}

/// The slot being written.
#[derive(Clone, Copy, Debug)]
struct Cursor {
    l: u64,
    pos: u64,
    offset: u64,
    crc: u32,
    sealed: u64,
}

/// The single appender (owned by the writer thread).
pub struct IqBufferWriter {
    buffer: IqBuffer,
    data: File,
    journal: File,
    cur: Option<Cursor>,
    /// Current wait after failed writes (zero after a success).
    backoff: Duration,
    /// No write is attempted before this.
    retry_at: Option<Instant>,
    last_checkpoint: Instant,
    compacted_len: u64,
    /// Test only: drop without the final checkpoint (a crash).
    skip_final_checkpoint: bool,
}

/// A segment as the status lists it (times Unix s, frequencies Hz).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SegmentStatus {
    /// Segment id (increasing for the life of the ring).
    pub id: u64,
    /// Run (open of the ring) whose stream indices `global_index` uses.
    pub run: u64,
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
    /// Antenna-port bias-tee state under this segment's provenance (T-325): `unknown`, `off` or
    /// `on`. `unknown` means the source could not report it — a replayed recording, say — and
    /// must never be read as `off`: the DC may have been on the port for these samples, which is
    /// both a hardware hazard and a reason the noise floor is not comparable with other segments.
    pub bias_tee: hk_model::BiasTee,
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
    /// Slots overwritten (their previous contents evicted).
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
    /// Disk quota: `min(retention × highest rate × 2, max_bytes)`, at least two slots.
    pub quota_bytes: u64,
    /// Largest clip export, bytes.
    pub max_clip_bytes: u64,
    /// Slot size, bytes.
    pub chunk_bytes: u64,
    /// Slots holding retained samples.
    pub chunk_files: usize,
    /// Slots of the ring.
    pub slot_count: u64,
    /// Size of the ring file (`slot_count × chunk_bytes`), fixed while the ring is open.
    pub allocated_bytes: u64,
    /// `full`, `shrunk` (fewer slots than the quota: not enough free space) or `refused`
    /// (disabled for lack of space); `null` when disabled otherwise.
    pub allocation: Option<Allocation>,
    /// Fraction of the ring allocated, 0..1, while `allocation` is `allocating`; 1 once the ring
    /// is open; `null` when there is no ring.
    pub allocation_progress: Option<f64>,
    /// The filesystem reserved the ring's blocks (false: a sparse file).
    pub preallocated: bool,
    /// The buffer survives restarts (always true when enabled).
    pub persisted: bool,
    /// This run's number (increases on every open of the ring).
    pub run: Option<u64>,
    /// Segments recovered from the ring when this run opened it.
    pub recovered_segments: usize,
    /// Slots recovery discarded (unsealed, torn, failed CRC, or no longer fitting).
    pub discarded_slots: u64,
    /// Ring position (slot) being written (`null` before the first write).
    pub head_slot: Option<u64>,
    /// Byte offset of the write head in the ring file.
    pub head_offset_bytes: Option<u64>,
    /// Complete passes of the write head over the ring.
    pub wrap_count: u64,
    /// Free bytes of the buffer's filesystem (`null` when unknown or disabled).
    pub fs_free_bytes: Option<u64>,
    /// Size of the buffer's filesystem (`null` when unknown or disabled).
    pub fs_total_bytes: Option<u64>,
    /// Free-space floor enforced on that filesystem (`null` when unknown or disabled).
    pub min_free_bytes: Option<u64>,
    /// Writing is paused because another slot of a sparse ring would go below the floor.
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
    /// Bytes of the buffer's files on disk (ring + journal).
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
    /// Oldest-first eviction counts (this run).
    pub evicted: EvictedStatus,
    /// Samples this buffer's reader lost to ring overruns (never held capture).
    pub dropped_samples: u64,
    /// Samples not stored because their window's class forbids content.
    pub gated_samples: u64,
    /// Failed ring or journal writes (each backs writing off).
    pub write_errors: u64,
    /// Samples not stored because a write failed or writing was backing off.
    pub failed_samples: u64,
    /// Failed ring fsyncs.
    pub sync_errors: u64,
    /// Samples written but discarded because the fsync meant to make them durable failed: they are
    /// never indexed, sealed, exported or recovered, and the ring rewrites their span.
    pub poisoned_samples: u64,
    /// Samples captured while the ring was still opening in the background (T-217): capture is
    /// never buffered until allocation finishes, so a large quota's first minutes hold no IQ. Set
    /// by the feeder ([`hk_pipeline::iqbuffer`]), which sees these blocks before the ring exists;
    /// this crate always reports 0 here and the pipeline overlays the real count. Persists once the
    /// ring opens (`allocation` moves past `"allocating"`), so the gap stays explained.
    pub allocation_skipped_samples: u64,
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
            slot_count: 0,
            allocated_bytes: 0,
            allocation: None,
            allocation_progress: None,
            preallocated: false,
            persisted: false,
            run: None,
            recovered_segments: 0,
            discarded_slots: 0,
            head_slot: None,
            head_offset_bytes: None,
            wrap_count: 0,
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
            sync_errors: 0,
            poisoned_samples: 0,
            allocation_skipped_samples: 0,
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
    /// Stream indices `[start, end)` of one run (exact whatever the rate).
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
    /// Run of the segment.
    pub run: u64,
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
    /// A time range matches segments of more than one run (a restart), which are never spliced.
    MixedRuns {
        /// Time of the first sample of the other run, Unix ns.
        t_ns: i64,
    },
    /// Larger than the caller's limit.
    TooLarge {
        /// Bytes the clip would have.
        bytes: u64,
    },
    /// The ring overwrote part of the range while it was being exported.
    Evicted,
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
            Self::MixedRuns { t_ns } => write!(
                f,
                "the range spans a restart of the buffer at {:.6} s; give run, or export each \
                 side separately",
                *t_ns as f64 / 1e9
            ),
            Self::TooLarge { bytes } => write!(f, "the clip would be {bytes} bytes"),
            Self::Evicted => write!(
                f,
                "the buffer overwrote part of the range during the export (evicted)"
            ),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ClipError {}

/// One read of a clip: ring file offset, length, logical byte of its start.
type Span = (u64, u64, u64);

/// A clip selected under the index lock, read without it ([`IqBuffer::plan_clip`]). Every read is
/// checked against the floor afterwards, so data the ring overwrote meanwhile fails the write.
pub struct ClipPlan {
    buffer: IqBuffer,
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
        let inner = &self.buffer.inner;
        let mut buf = vec![0u8; 1 << 20];
        for (_, reads) in &self.plan {
            for &(offset, len, log) in reads {
                let mut done = 0;
                while done < len {
                    let n = (len - done).min(buf.len() as u64) as usize;
                    inner
                        .read
                        .read_exact_at(&mut buf[..n], offset + done)
                        .map_err(ClipError::Io)?;
                    if lock(&inner.state).log_floor > log + done {
                        return Err(ClipError::Evicted);
                    }
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

/// Deletes the chunk files of the T-157 grow-and-delete log (`<16 digits>.ci8`).
fn remove_legacy_chunks(dir: &Path) -> io::Result<()> {
    for e in fs::read_dir(dir)?.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if let Some(stem) = name.strip_suffix(".ci8") {
            if stem.len() == 16 && stem.bytes().all(|b| b.is_ascii_digit()) {
                fs::remove_file(e.path())?;
            }
        }
    }
    Ok(())
}

fn sync_dir(dir: &Path) {
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
}

/// Writes `recs` as the whole journal (a temporary file renamed over it) and opens it to append.
fn write_snapshot(dir: &Path, recs: &[Rec]) -> io::Result<(File, u64)> {
    let tmp = dir.join(format!("{JOURNAL_FILE}.tmp"));
    let bytes = encode(recs);
    {
        let mut f = File::create(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, dir.join(JOURNAL_FILE))?;
    sync_dir(dir);
    let f = OpenOptions::new()
        .append(true)
        .open(dir.join(JOURNAL_FILE))?;
    Ok((f, bytes.len() as u64))
}

/// The `version` of the journal's header record, if the first record is a ring header (read
/// loosely, so a newer format's header is still recognised).
fn journal_version(dir: &Path) -> Option<u64> {
    let data = fs::read(dir.join(JOURNAL_FILE)).ok()?;
    let n = u32::from_le_bytes(data.get(0..4)?.try_into().ok()?) as usize;
    let crc = u32::from_le_bytes(data.get(4..8)?.try_into().ok()?);
    let p = data.get(8..8usize.checked_add(n)?)?;
    if crc32_update(0, p) != crc {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(p).ok()?;
    (v["k"] == "header" && v["magic"] == RING_MAGIC)
        .then(|| v["version"].as_u64())
        .flatten()
}

/// Sizes the ring from `from` to `len` bytes in steps of whole slots near
/// [`ALLOCATION_STEP_BYTES`], reporting the fraction done and stopping when `cancel` is set.
/// `Ok(true)`: every step reserved its blocks.
///
/// # Each step is flushed here, on purpose (T-536)
///
/// Reserving blocks is cheap and *deferred*: `F_PREALLOCATE` (and `fallocate`) return long before
/// the filesystem has committed the extents they promised, and the bill lands on the **first fsync
/// of that file** — whoever, whenever, on whatever thread. It used to land on
/// [`IqBufferWriter::checkpoint`], i.e. on the feeder thread, and the feeder's last act before its
/// segment ends is a checkpoint, so **a re-plumb's `join_workers` paid for the whole ring while
/// capture was off**.
///
/// Measured on this Mac (APFS), the segment-end checkpoint inside a re-plumb, before → after:
/// 244 ms → 38 ms at a 1.2 GB quota, 740 ms → under 20 ms at 4.8 GB (the default 2 min ×
/// 20 Msps), **1.9 s → under 20 ms at 12 GB**. The syscall on its own says the same thing: first
/// fsync after preallocation 30 ms / 194 ms / 2.0 s / 7.6 s at 0.2 / 1.2 / 4.8 / 12 GB, against
/// 0–11 ms for every fsync after it. End to end, a re-plumb whose always-on readers stalled 55 ms
/// with the buffer off stalled **3.0 s** with a 12 GB ring, and that is what T-531's straggler
/// report caught in the act.
///
/// So the debt is paid where the allocation is: on `hk-iqbuffer-alloc`, the thread that exists
/// precisely so a slow open never holds up the run (the status reports `allocating` with a
/// progress fraction until it is done, and nothing is buffered meanwhile). Paying it **per step**
/// rather than once at the end keeps `cancel`'s granularity at one step, which is what
/// `hk_pipeline::iqbuffer::IqBufferService`'s drop waits for: worst measured step, 0.6 s at 12 GB.
/// The open itself is correspondingly slower — 2.5 s → 6.8 s for a 12 GB ring — and that is the
/// trade: it is the one window the design already declares as `allocating`, on a thread nothing
/// waits for, with nothing buffered yet.
///
/// A failed flush is **not** fatal. The extents are reserved either way; all that is lost is the
/// head start, and refusing to open the buffer over it would trade a slow re-plumb for no IQ
/// history at all. The write path's own fsync failures are still handled where they matter
/// ([`IqBufferWriter::checkpoint`] poisons the unsealed bytes).
fn allocate_stepwise(
    hooks: &dyn IqBufferHooks,
    ring: &File,
    from: u64,
    len: u64,
    slot_bytes: u64,
    progress: &dyn Fn(f64),
    cancel: &AtomicBool,
) -> io::Result<bool> {
    let flush = |at: u64| {
        if let Err(e) = hooks.sync_allocation(ring) {
            eprintln!(
                "IQ capture ring: flushing the allocation at {at} bytes failed ({e}); the first \
                 checkpoint will pay for it instead"
            );
        }
    };
    if len <= from {
        let r = hooks.preallocate(ring, len);
        flush(len);
        progress(1.0);
        return r;
    }
    let step = ALLOCATION_STEP_BYTES.div_ceil(slot_bytes).max(1) * slot_bytes;
    let mut reserved = true;
    let mut at = from;
    progress(0.0);
    while at < len {
        if cancel.load(Ordering::Relaxed) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "IQ capture ring allocation cancelled",
            ));
        }
        at = (at + step).min(len);
        reserved &= hooks.preallocate(ring, at)?;
        flush(at);
        progress((at - from) as f64 / (len - from) as f64);
    }
    Ok(reserved)
}

/// The ring's state as the journal and ring file describe it, before this run.
struct Recovered {
    slots: VecDeque<Slot>,
    segments: VecDeque<Seg>,
    floor: u64,
    end: u64,
    last_run: u64,
    next_seg_id: u64,
    recovery: Recovery,
}

/// Rebuilds the index from the journal and verifies the newest slots against the ring file
/// (`file_len`: its size before this open resized it). Never fails: what cannot be trusted is
/// discarded.
fn recover(dir: &Path, ring: &File, file_len: u64, slot_bytes: u64, slot_count: u64) -> Recovered {
    let mut out = Recovered {
        slots: VecDeque::new(),
        segments: VecDeque::new(),
        floor: 0,
        end: 0,
        last_run: 0,
        next_seg_id: 0,
        recovery: Recovery::default(),
    };
    let Ok(data) = fs::read(dir.join(JOURNAL_FILE)) else {
        return out;
    };
    let (recs, valid) = decode(&data);
    if valid < data.len() {
        out.recovery.note = Some(format!(
            "ignored {} bytes of torn or corrupt journal tail",
            data.len() - valid
        ));
    }
    match recs.first() {
        Some(Rec::Header {
            magic,
            version,
            slot_bytes: s,
            ..
        }) if magic == RING_MAGIC && *version == RING_VERSION && *s == slot_bytes => {}
        Some(Rec::Header { slot_bytes: s, .. }) => {
            out.recovery.note = Some(format!(
                "reset: the slot size changed from {s} to {slot_bytes} bytes (or the format did)"
            ));
            return out;
        }
        _ => {
            if !data.is_empty() {
                out.recovery.note = Some("reset: the journal has no valid header".into());
            }
            return out;
        }
    }
    // Replay: the latest slot at each position, its longest seal.
    let mut slots: BTreeMap<u64, Slot> = BTreeMap::new();
    let mut owner: BTreeMap<u64, u64> = BTreeMap::new();
    let mut segs = Vec::new();
    for r in recs.into_iter().skip(1) {
        match r {
            Rec::Header { .. } => {}
            Rec::Run { run } => out.last_run = out.last_run.max(run),
            Rec::Open { slot, pos } => {
                if let Some(old) = owner.insert(pos, slot) {
                    if old != slot {
                        slots.remove(&old);
                    }
                }
                if let Some(prev) = slots.get(&slot) {
                    if prev.pos != pos {
                        owner.remove(&prev.pos);
                    }
                }
                slots.insert(
                    slot,
                    Slot {
                        l: slot,
                        pos,
                        bytes: 0,
                        sealed: 0,
                        crc: 0,
                    },
                );
            }
            Rec::Seal {
                slot,
                bytes,
                crc,
                floor,
            } => {
                if let Some(s) = slots.get_mut(&slot) {
                    if bytes >= s.sealed && bytes <= slot_bytes {
                        (s.sealed, s.bytes, s.crc) = (bytes, bytes, crc);
                    }
                }
                out.floor = out.floor.max(floor);
            }
            Rec::Seg {
                id,
                run,
                log_start,
                global_index,
                t_ns,
                content_class,
                dropped_before,
                provenance,
            } => {
                out.next_seg_id = out.next_seg_id.max(id + 1);
                out.last_run = out.last_run.max(run);
                segs.push((
                    id,
                    run,
                    log_start,
                    SegmentStart {
                        global_index,
                        t_ns,
                        provenance: *provenance,
                        content_class,
                        dropped_before,
                    },
                ));
            }
        }
    }
    let total = slots.len() as u64;
    // Sealed, inside the ring file as it was, and inside the (possibly shrunk) ring.
    let mut live: Vec<Slot> = slots
        .into_values()
        .filter(|s| s.sealed > 0 && s.pos < slot_count && s.pos * slot_bytes + s.sealed <= file_len)
        .collect();
    // Newest first: contiguous logical slots, every older one full, at most `slot_count`.
    live.sort_by_key(|s| std::cmp::Reverse(s.l));
    let mut keep: Vec<Slot> = Vec::new();
    for s in live {
        let fits = keep.len() < slot_count as usize;
        let contiguous = keep
            .last()
            .is_none_or(|n| n.l == s.l + 1 && s.sealed == slot_bytes);
        if !(fits && contiguous) {
            break;
        }
        keep.push(s);
    }
    // Re-verify the newest slots: a failure drops that slot and everything newer.
    let mut verified = false;
    let mut buf = Vec::new();
    for _ in 0..RECOVERY_VERIFY_SLOTS {
        let Some(s) = keep.first().copied() else {
            break;
        };
        buf.resize(s.sealed as usize, 0);
        let ok = ring.read_exact_at(&mut buf, s.pos * slot_bytes).is_ok()
            && crc32_update(0, &buf) == s.crc;
        if ok {
            verified = true;
            break;
        }
        keep.remove(0);
        out.recovery.note = Some(format!(
            "slot {} failed its CRC check (torn write) and was discarded with anything newer",
            s.l
        ));
    }
    if !verified {
        keep.clear();
    }
    keep.reverse();
    out.recovery.discarded_slots = total - keep.len() as u64;
    let (Some(oldest), Some(newest)) = (keep.first().copied(), keep.last().copied()) else {
        return out;
    };
    out.floor = out.floor.max(oldest.l * slot_bytes);
    out.end = newest.l * slot_bytes + newest.sealed;
    out.floor = out.floor.min(out.end);
    out.slots = keep.into();
    segs.sort_by_key(|s| (s.2, s.0));
    for (i, (id, run, start_log, start)) in segs.iter().enumerate() {
        let next = segs.get(i + 1).map_or(out.end, |n| n.2);
        let end = next.min(out.end);
        let from = (*start_log).max(out.floor);
        if from >= end {
            continue;
        }
        out.segments.push_back(Seg {
            id: *id,
            run: *run,
            start_log: *start_log,
            log_start: from,
            samples: (end - from) / BYTES_PER_SAMPLE,
            global_index: start.global_index + (from - start_log) / BYTES_PER_SAMPLE,
            start: start.clone(),
            persisted: true,
        });
    }
    out.recovery.segments = out.segments.len();
    out
}

impl IqBuffer {
    /// Opens (creating and allocating) the ring in `dir`, recovering what a previous run left.
    pub fn open(dir: &Path, cfg: IqBufferConfig) -> io::Result<(Self, IqBufferWriter)> {
        Self::open_with(dir, cfg, Arc::new(OsHooks))
    }

    /// [`Self::open`] with filesystem probes `hooks`.
    ///
    /// - The ring is locked (`flock`): a second open of the same directory fails.
    /// - A ring of another slot size (a quota change across the 64 KiB..64 MiB range, or another
    ///   format) is reset. A ring of another slot count keeps the slots that still fit: growing
    ///   keeps everything, shrinking keeps the newest contiguous slots stored below the new size.
    /// - Space: the ring (less what its existing file already holds) must fit above the free-space
    ///   floor; otherwise it is shrunk to the whole slots that fit, and refused below two
    ///   ([`AllocationRefused`]).
    pub fn open_with(
        dir: &Path,
        cfg: IqBufferConfig,
        hooks: Arc<dyn IqBufferHooks>,
    ) -> io::Result<(Self, IqBufferWriter)> {
        Self::open_with_progress(dir, cfg, hooks, &|_| {}, &AtomicBool::new(false))
    }

    /// [`Self::open_with`] reporting the allocated fraction (0..1) to `progress` and giving up
    /// (an [`io::ErrorKind::Interrupted`] error, the ring file back at its old size) when `cancel`
    /// is set between allocation steps ([`ALLOCATION_STEP_BYTES`]).
    ///
    /// - Held lock: [`RingLocked`] ([`is_ring_locked`]).
    /// - A ring of a newer on-disk version: [`RingIncompatible`] ([`is_ring_incompatible`]),
    ///   before anything in the directory is changed.
    pub fn open_with_progress(
        dir: &Path,
        cfg: IqBufferConfig,
        hooks: Arc<dyn IqBufferHooks>,
        progress: &dyn Fn(f64),
        cancel: &AtomicBool,
    ) -> io::Result<(Self, IqBufferWriter)> {
        fs::create_dir_all(dir)?;
        let lock_path = dir.join(LOCK_FILE);
        let lock_file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)?;
        // SAFETY: a valid open descriptor.
        if unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    RingLocked { lock_path },
                ));
            }
            return Err(io::Error::new(
                e.kind(),
                format!("locking the IQ capture ring {}: {e}", lock_path.display()),
            ));
        }
        if let Some(found) = journal_version(dir).filter(|v| *v > u64::from(RING_VERSION)) {
            return Err(io::Error::other(RingIncompatible {
                found,
                supported: RING_VERSION,
            }));
        }
        remove_legacy_chunks(dir)?;
        let slot_bytes = cfg.chunk_bytes();
        let ring_path = dir.join(RING_FILE);
        let file_len = fs::metadata(&ring_path).map_or(0, |m| m.len());
        let want = cfg.slot_count();
        let (mut slot_count, mut allocation) = (want, Allocation::Full);
        if let Ok(space) = hooks.fs_space(dir) {
            let floor = cfg.free_floor(space.total);
            let usable = space.free.saturating_sub(floor).saturating_add(file_len);
            if want.saturating_mul(slot_bytes) > usable {
                slot_count = usable / slot_bytes;
                allocation = Allocation::Shrunk;
                if slot_count < 2 {
                    return Err(io::Error::other(AllocationRefused {
                        need_bytes: (2 * slot_bytes).saturating_sub(file_len),
                        free_bytes: space.free,
                        floor_bytes: floor,
                    }));
                }
            }
        }
        let ring = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&ring_path)?;
        let rec = recover(dir, &ring, file_len, slot_bytes, slot_count);
        let preallocated = match allocate_stepwise(
            &*hooks,
            &ring,
            file_len,
            slot_count * slot_bytes,
            slot_bytes,
            progress,
            cancel,
        ) {
            Ok(p) => p,
            Err(e) => {
                // Leave the file as it was (a later open sees the ring it recovered from).
                if ring.metadata().is_ok_and(|m| m.len() > file_len) {
                    let _ = ring.set_len(file_len);
                }
                return Err(e);
            }
        };
        let run = rec.last_run + 1;
        let mut st = State {
            slots: rec.slots,
            free: (0..slot_count).collect(),
            segments: rec.segments,
            log_floor: rec.floor,
            log_end: rec.end,
            next_seg_id: rec.next_seg_id,
            ..State::default()
        };
        for s in &st.slots {
            st.free.remove(&s.pos);
        }
        if let Some(note) = &rec.recovery.note {
            eprintln!("IQ capture ring {}: {note}", dir.display());
        }
        let inner = Inner {
            dir: dir.to_path_buf(),
            cfg,
            slot_bytes,
            slot_count,
            quota_bytes: cfg.quota_bytes(),
            allocation,
            preallocated,
            run,
            recovery: rec.recovery,
            hooks,
            read: ring.try_clone()?,
            journal_len: AtomicU64::new(0),
            state: Mutex::new(State::default()),
            _lock: lock_file,
        };
        let (journal, len) = write_snapshot(dir, &inner.snapshot(&st))?;
        inner.journal_len.store(len, Ordering::Relaxed);
        let cur = st
            .slots
            .back()
            .filter(|s| s.sealed < slot_bytes)
            .map(|s| Cursor {
                l: s.l,
                pos: s.pos,
                offset: s.sealed,
                crc: s.crc,
                sealed: s.sealed,
            });
        *lock(&inner.state) = st;
        let buffer = Self {
            inner: Arc::new(inner),
        };
        let writer = IqBufferWriter {
            buffer: buffer.clone(),
            data: ring,
            journal,
            cur,
            backoff: Duration::ZERO,
            retry_at: None,
            last_checkpoint: Instant::now(),
            compacted_len: len,
            skip_final_checkpoint: false,
        };
        Ok((buffer, writer))
    }

    /// The configuration.
    pub fn config(&self) -> IqBufferConfig {
        self.inner.cfg
    }

    /// This run's number.
    pub fn run(&self) -> u64 {
        self.inner.run
    }

    /// The space of the filesystem holding `path` (through the buffer's hooks).
    pub fn space_at(&self, path: &Path) -> io::Result<FsSpace> {
        self.inner.hooks.fs_space(path)
    }

    /// The status: segments overlapping `[t0_ns, t1_ns)` (whole buffer when `None`), the newest
    /// `limit` of them.
    pub fn status(&self, t0_ns: Option<i64>, t1_ns: Option<i64>, limit: usize) -> IqBufferStatus {
        let inner = &self.inner;
        let space = inner.hooks.fs_space(&inner.dir).ok();
        let st = lock(&inner.state);
        let live: Vec<&Seg> = st.segments.iter().filter(|s| s.samples > 0).collect();
        let t0 = live.iter().map(|s| s.t0_ns()).min();
        let t1 = live.iter().map(|s| s.t1_ns()).max();
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
                    run: g.run,
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
                    bias_tee: p.bias_tee,
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
                if a.run != b.run {
                    return None;
                }
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
        let sb = inner.slot_bytes;
        let head = st.slots.back();
        let allocated = inner.slot_count * sb;
        IqBufferStatus {
            enabled: true,
            reason: None,
            dir: Some(inner.dir.display().to_string()),
            retention_s: inner.cfg.retention_s,
            max_bytes: inner.cfg.max_bytes,
            quota_bytes: inner.quota_bytes,
            max_clip_bytes: inner.cfg.max_clip_bytes,
            chunk_bytes: sb,
            chunk_files: st
                .slots
                .iter()
                .filter(|x| x.bytes > 0 && x.l * sb + x.bytes > st.log_floor)
                .count(),
            slot_count: inner.slot_count,
            allocated_bytes: allocated,
            allocation: Some(inner.allocation),
            allocation_progress: Some(1.0),
            preallocated: inner.preallocated,
            persisted: true,
            run: Some(inner.run),
            recovered_segments: inner.recovery.segments,
            discarded_slots: inner.recovery.discarded_slots,
            head_slot: head.map(|h| h.pos),
            head_offset_bytes: head.map(|h| h.pos * sb + h.bytes),
            wrap_count: head.map_or(0, |h| h.l / inner.slot_count),
            fs_free_bytes: space.map(|s| s.free),
            fs_total_bytes: space.map(|s| s.total),
            min_free_bytes: space.map(|s| inner.cfg.free_floor(s.total)),
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
            disk_bytes: allocated + inner.journal_len.load(Ordering::Relaxed),
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
            sync_errors: c.sync_errors,
            poisoned_samples: c.poisoned_samples,
            // Overlaid by the pipeline's feeder (this crate never sees allocation-window blocks).
            allocation_skipped_samples: 0,
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
        self.export_clip_run(range, band, None, max_bytes, out)
    }

    /// [`Self::export_clip`] within run `run` ([`Self::plan_clip_run`]).
    pub fn export_clip_run(
        &self,
        range: ClipRange,
        band: Option<(f64, f64)>,
        run: Option<u64>,
        max_bytes: u64,
        out: &mut dyn Write,
    ) -> Result<Clip, ClipError> {
        let plan = self.plan_clip_run(range, band, run)?;
        if plan.bytes() > max_bytes {
            return Err(ClipError::TooLarge {
                bytes: plan.bytes(),
            });
        }
        plan.write(out)
    }

    /// Selects the buffered samples in `range` (and `band`) without reading them
    /// ([`Self::plan_clip_run`] with no run).
    pub fn plan_clip(
        &self,
        range: ClipRange,
        band: Option<(f64, f64)>,
    ) -> Result<ClipPlan, ClipError> {
        self.plan_clip_run(range, band, None)
    }

    /// Selects the buffered samples in `range` (and `band`) of run `run` without reading them.
    /// Without a run, an index range selects in this run (stream indices restart with every
    /// run), and a time range in every run but is refused ([`ClipError::MixedRuns`]) when it
    /// matches more than one.
    pub fn plan_clip_run(
        &self,
        range: ClipRange,
        band: Option<(f64, f64)>,
        run: Option<u64>,
    ) -> Result<ClipPlan, ClipError> {
        let inner = &self.inner;
        let sb = inner.slot_bytes;
        let run = match (run, range) {
            (Some(r), _) => Some(r),
            (None, ClipRange::Index { .. }) => Some(inner.run),
            (None, ClipRange::Time { .. }) => None,
        };
        let mut plan: Vec<(ClipPiece, Vec<Span>)> = Vec::new();
        let mut sample_start = 0u64;
        {
            let st = lock(&inner.state);
            for seg in st.segments.iter().filter(|s| s.samples > 0) {
                if run.is_some_and(|r| r != seg.run) {
                    continue;
                }
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
                    if first.run != seg.run {
                        return Err(ClipError::MixedRuns {
                            t_ns: seg.t_ns(seg.global_index + j0),
                        });
                    }
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
                    .slots
                    .iter()
                    .filter(|x| x.l * sb < b && x.l * sb + sb > a)
                    .map(|x| {
                        let lo = a.max(x.l * sb);
                        let hi = b.min(x.l * sb + sb);
                        (x.pos * sb + (lo - x.l * sb), hi - lo, lo)
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
                        run: seg.run,
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
            buffer: self.clone(),
            plan,
            samples: sample_start,
        })
    }
}

impl Inner {
    /// The whole journal describing `st`: header, run, live slots and their seals, the persisted
    /// retained segments.
    fn snapshot(&self, st: &State) -> Vec<Rec> {
        let mut v = vec![
            Rec::Header {
                magic: RING_MAGIC.into(),
                version: RING_VERSION,
                slot_bytes: self.slot_bytes,
                slots: self.slot_count,
            },
            Rec::Run { run: self.run },
        ];
        for s in &st.slots {
            v.push(Rec::Open {
                slot: s.l,
                pos: s.pos,
            });
            if s.sealed > 0 {
                v.push(Rec::Seal {
                    slot: s.l,
                    bytes: s.sealed,
                    crc: s.crc,
                    floor: st.log_floor,
                });
            }
        }
        v.extend(
            st.segments
                .iter()
                .filter(|g| g.persisted && g.samples > 0)
                .map(Seg::rec),
        );
        v
    }

    /// Duration eviction (advances the floor) and trims segments below the floor.
    fn evict(&self, st: &mut State) {
        let max_ns = (self.cfg.retention_s * 1e9) as i64;
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
        Self::trim(st);
    }

    /// Drops the samples of segments below the floor.
    fn trim(st: &mut State) {
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
    }
}

impl IqBufferWriter {
    /// The shared index.
    pub fn buffer(&self) -> &IqBuffer {
        &self.buffer
    }

    /// Starts a segment; the next [`Self::append`] belongs to it.
    pub fn begin_segment(&mut self, start: SegmentStart) {
        let run = self.buffer.inner.run;
        let mut st = lock(&self.buffer.inner.state);
        if st.segments.back().is_some_and(|s| s.samples == 0) {
            st.segments.pop_back();
        }
        let id = st.next_seg_id;
        st.next_seg_id += 1;
        let log_start = st.log_end;
        st.segments.push_back(Seg {
            id,
            run,
            start_log: log_start,
            log_start,
            samples: 0,
            global_index: start.global_index,
            start,
            persisted: false,
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
    /// - **Paused** (a sparse ring's next slot would go below the free-space floor): the rest is
    ///   not stored (`paused_samples`), the segment ends, and `Ok` is returned.
    /// - **Write error**: the unwritten samples are counted (`failed_samples`), the segment ends,
    ///   the error is kept and returned, and writes back off; an append during the back-off
    ///   stores nothing, touches no file and returns an error.
    /// - A checkpoint follows when [`CHECKPOINT_INTERVAL`] has passed since the last one.
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
                    self.fail(&e, rest.len() as u64 / BYTES_PER_SAMPLE);
                    return Err(e);
                }
            }
        }
        self.backoff = Duration::ZERO;
        if self.last_checkpoint.elapsed() >= CHECKPOINT_INTERVAL {
            if let Err(e) = self.checkpoint() {
                self.fail(&e, 0);
            }
        }
        Ok(())
    }

    fn fail(&mut self, e: &io::Error, lost_samples: u64) {
        self.backoff = if self.backoff.is_zero() {
            RETRY_BACKOFF_MIN
        } else {
            (self.backoff * 2).min(RETRY_BACKOFF_MAX)
        };
        self.retry_at = Some(Instant::now() + self.backoff);
        let mut st = lock(&self.buffer.inner.state);
        st.counts.write_errors += 1;
        st.counts.failed_samples += lost_samples;
        st.error = Some(e.to_string());
        st.open = false;
    }

    /// Writes some of `rest`; `Ok(false)`: paused (nothing written).
    fn write_some(&mut self, rest: &mut &[u8]) -> io::Result<bool> {
        let inner = Arc::clone(&self.buffer.inner);
        let sb = inner.slot_bytes;
        if self.cur.is_none_or(|c| c.offset == sb) {
            if !inner.preallocated {
                // A sparse ring consumes space as it fills: keep the floor (an unknown space
                // carries on, and a full disk then fails a write).
                if let Ok(space) = inner.hooks.fs_space(&inner.dir) {
                    let need = inner.cfg.free_floor(space.total).saturating_add(sb);
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
            }
            self.advance()?;
        }
        let c = self.cur.as_mut().expect("a slot");
        let n = ((sb - c.offset) as usize).min(rest.len());
        inner.hooks.before_write(n)?;
        self.data.write_all_at(&rest[..n], c.pos * sb + c.offset)?;
        c.crc = crc32_update(c.crc, &rest[..n]);
        c.offset += n as u64;
        *rest = &rest[n..];
        let mut st = lock(&inner.state);
        st.slots.back_mut().expect("the slot").bytes += n as u64;
        st.log_end += n as u64;
        st.segments.back_mut().expect("the open segment").samples += n as u64 / BYTES_PER_SAMPLE;
        inner.evict(&mut st);
        Ok(true)
    }

    /// Seals the full slot, takes the next position (evicting the oldest slot when none is free)
    /// and journals it before any of its bytes is written.
    fn advance(&mut self) -> io::Result<()> {
        let inner = Arc::clone(&self.buffer.inner);
        let sb = inner.slot_bytes;
        if self.cur.is_some_and(|c| c.offset > c.sealed) {
            self.checkpoint()?;
        }
        let (l, pos) = {
            let mut st = lock(&inner.state);
            let l = st.slots.back().map_or(st.log_end / sb, |s| s.l + 1);
            let pos = match st.free.pop_first() {
                Some(p) => p,
                None => {
                    let old = st.slots.pop_front().expect("a full ring has slots");
                    st.log_floor = st.log_floor.max((old.l + 1) * sb);
                    st.counts.evicted_chunks += 1;
                    Inner::trim(&mut st);
                    old.pos
                }
            };
            st.log_end = st.log_end.max(l * sb);
            st.log_floor = st
                .log_floor
                .max(st.slots.front().map_or(l * sb, |s| s.l * sb));
            st.slots.push_back(Slot {
                l,
                pos,
                bytes: 0,
                sealed: 0,
                crc: 0,
            });
            (l, pos)
        };
        if let Err(e) = self.journal_append(&[Rec::Open { slot: l, pos }]) {
            let mut st = lock(&inner.state);
            st.slots.pop_back();
            st.free.insert(pos);
            return Err(e);
        }
        self.cur = Some(Cursor {
            l,
            pos,
            offset: 0,
            crc: 0,
            sealed: 0,
        });
        Ok(())
    }

    fn journal_append(&mut self, recs: &[Rec]) -> io::Result<()> {
        let bytes = encode(recs);
        let inner = &self.buffer.inner;
        let r = self
            .journal
            .write_all(&bytes)
            .and_then(|()| self.journal.sync_data());
        match r {
            Ok(()) => {
                inner
                    .journal_len
                    .fetch_add(bytes.len() as u64, Ordering::Relaxed);
                Ok(())
            }
            Err(e) => {
                // A torn record would hide every later one: cut it off.
                let _ = self
                    .journal
                    .set_len(inner.journal_len.load(Ordering::Relaxed));
                Err(e)
            }
        }
    }

    /// Makes everything written so far durable: fsyncs the ring, journals the new segment starts
    /// and a seal of the current slot, fsyncs the journal; compacts an outgrown journal.
    pub fn checkpoint(&mut self) -> io::Result<()> {
        self.last_checkpoint = Instant::now();
        let Some(c) = self.cur else {
            return Ok(());
        };
        let inner = Arc::clone(&self.buffer.inner);
        let (mut recs, ids, floor) = {
            let st = lock(&inner.state);
            let pending: Vec<&Seg> = st
                .segments
                .iter()
                .filter(|g| !g.persisted && g.samples > 0)
                .collect();
            (
                pending.iter().map(|g| g.rec()).collect::<Vec<_>>(),
                pending.iter().map(|g| g.id).collect::<Vec<_>>(),
                st.log_floor,
            )
        };
        if recs.is_empty() && c.offset == c.sealed {
            return Ok(());
        }
        if let Err(e) = inner
            .hooks
            .before_sync()
            .and_then(|()| self.data.sync_data())
        {
            // The unsealed bytes may never reach the disk, and a later fsync can succeed without
            // them: never seal over them.
            self.poison();
            return Err(e);
        }
        recs.push(Rec::Seal {
            slot: c.l,
            bytes: c.offset,
            crc: c.crc,
            floor,
        });
        self.journal_append(&recs)?;
        {
            let mut st = lock(&inner.state);
            for g in st.segments.iter_mut().filter(|g| ids.contains(&g.id)) {
                g.persisted = true;
            }
            if let Some(s) = st.slots.iter_mut().rev().find(|s| s.l == c.l) {
                (s.sealed, s.crc) = (c.offset, c.crc);
            }
        }
        if let Some(cur) = self.cur.as_mut() {
            cur.sealed = c.offset;
        }
        let len = inner.journal_len.load(Ordering::Relaxed);
        if len > JOURNAL_COMPACT_MIN.max(4 * self.compacted_len) {
            let recs = inner.snapshot(&lock(&inner.state));
            let (journal, len) = write_snapshot(&inner.dir, &recs)?;
            self.journal = journal;
            inner.journal_len.store(len, Ordering::Relaxed);
            self.compacted_len = len;
        }
        Ok(())
    }

    /// A ring fsync failed: rolls the current slot back to its last seal. The unsealed span leaves
    /// the index (its samples are counted in `poisoned_samples`, the segments covering it are cut
    /// there and the open one ends), so no clip exports it and no seal ever covers it; the next
    /// writes overwrite it and are made durable by a later, successful fsync of their own.
    fn poison(&mut self) {
        let inner = Arc::clone(&self.buffer.inner);
        let sb = inner.slot_bytes;
        let Some(c) = self.cur.as_mut() else {
            return;
        };
        let mut st = lock(&inner.state);
        st.counts.sync_errors += 1;
        st.open = false;
        let lost = c.offset - c.sealed;
        if lost == 0 {
            return;
        }
        let sealed_crc = match st.slots.iter_mut().rev().find(|s| s.l == c.l) {
            Some(s) => {
                s.bytes = c.sealed;
                s.crc
            }
            None => 0,
        };
        (c.offset, c.crc) = (c.sealed, sealed_crc);
        let cut = c.l * sb + c.sealed;
        st.counts.poisoned_samples += lost / BYTES_PER_SAMPLE;
        st.log_end = st.log_end.min(cut);
        st.log_floor = st.log_floor.min(cut);
        while let Some(seg) = st.segments.back_mut() {
            let end = seg.log_start + seg.samples * BYTES_PER_SAMPLE;
            if end <= cut {
                break;
            }
            if seg.log_start >= cut {
                // Only ever unpersisted: a persisted segment start lies below a seal.
                st.segments.pop_back();
                continue;
            }
            seg.samples = (cut - seg.log_start) / BYTES_PER_SAMPLE;
            break;
        }
        st.error = Some(format!(
            "ring fsync failed: {} unsynced samples discarded",
            lost / BYTES_PER_SAMPLE
        ));
    }

    /// Test support: drops the writer like a crash (no final checkpoint).
    #[doc(hidden)]
    pub fn simulate_crash(mut self) {
        self.skip_final_checkpoint = true;
    }
}

impl Drop for IqBufferWriter {
    fn drop(&mut self) {
        self.end_segment();
        if !self.skip_final_checkpoint {
            if let Err(e) = self.checkpoint() {
                eprintln!("IQ capture ring: final checkpoint failed: {e}");
            }
        }
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

    /// A filesystem whose free space, write failures and preallocation support the test sets.
    struct Faults {
        fail: AtomicBool,
        sync_fail: AtomicBool,
        sparse: AtomicBool,
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

        fn preallocate(&self, file: &File, len: u64) -> io::Result<bool> {
            if self.sparse.load(Ordering::SeqCst) {
                file.set_len(len)?;
                Ok(false)
            } else {
                preallocate(file, len)
            }
        }

        fn before_sync(&self) -> io::Result<()> {
            if self.sync_fail.load(Ordering::SeqCst) {
                Err(io::Error::from_raw_os_error(libc::EIO))
            } else {
                Ok(())
            }
        }
    }

    fn faults() -> Arc<Faults> {
        Arc::new(Faults {
            fail: AtomicBool::new(false),
            sync_fail: AtomicBool::new(false),
            sparse: AtomicBool::new(false),
            free: AtomicU64::new(100 << 30),
            writes: AtomicU64::new(0),
        })
    }

    fn tmp(tag: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("hk-store-iqbuffer-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    /// The buffer's files: names and sizes.
    fn files(dir: &Path) -> Vec<(String, u64)> {
        let mut v: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| {
                (
                    e.file_name().to_string_lossy().into_owned(),
                    e.metadata().unwrap().len(),
                )
            })
            .collect();
        v.sort();
        v
    }

    fn ring_len(dir: &Path) -> u64 {
        fs::metadata(dir.join(RING_FILE)).unwrap().len()
    }

    fn names(dir: &Path) -> Vec<String> {
        files(dir).into_iter().map(|(n, _)| n).collect()
    }

    const RING_NAMES: [&str; 3] = [RING_FILE, JOURNAL_FILE, LOCK_FILE];

    /// (id, run, global_index, samples, t0_ns, t1_ns, centre) of every listed segment.
    fn seg_keys(s: &IqBufferStatus) -> Vec<(u64, u64, u64, u64, i64, i64, f64)> {
        s.segments
            .iter()
            .map(|g| {
                (
                    g.id,
                    g.run,
                    g.global_index,
                    g.samples,
                    g.t0_ns,
                    g.t1_ns,
                    g.center_hz,
                )
            })
            .collect()
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
            noise_sigma_lsb: None,
            temperature_c: None,
            antenna_port: None,
            bias_tee: hk_model::BiasTee::Unknown,
            clock_source: ClockSource::Internal,
            clock_locked: true,
            calibration_state_ref: None,
            spur_mask_ref: None,
            timestamp_method: TimestampMethod::Synthetic,
            timestamp_error_budget_ns: None,
            capture_artefacts: Vec::new(),
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

    fn export(buf: &IqBuffer, range: ClipRange, run: Option<u64>) -> Result<Vec<u8>, ClipError> {
        let mut out = Vec::new();
        buf.export_clip_run(range, None, run, u64::MAX, &mut out)
            .map(|_| out)
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
        assert_eq!((c.chunk_bytes(), c.slot_count()), (32 << 20, 16));
        c.max_bytes = Some(1 << 30);
        assert_eq!(c.quota_bytes(), 576_000_000);
        c.max_bytes = Some(1024);
        assert_eq!(c.quota_bytes(), 2 * MIN_CHUNK_BYTES, "at least two slots");
        assert_eq!(c.slot_count(), 2);
        assert!(c.active(false));
        c.max_bytes = Some(0);
        assert!(!c.active(false));
        c.max_bytes = None;
        c.retention_s = 0.0;
        assert!(!c.active(false));
        // 1 h at 20 Msps is 144 GiB-class (144 GB) in 64 MiB slots.
        let hour = IqBufferConfig {
            retention_s: 3600.0,
            ..IqBufferConfig::default()
        };
        assert_eq!(hour.quota_bytes(), 144_000_000_000);
        assert_eq!(hour.chunk_bytes(), MAX_CHUNK_BYTES);
        // The default free-space floor: 10 % within 2..8 GiB.
        let d = IqBufferConfig::default();
        assert_eq!(d.free_floor(1 << 40), 8 << 30);
        assert_eq!(d.free_floor(64 << 30), (64u64 << 30) / 10);
        assert_eq!(d.free_floor(8 << 30), 2 << 30);
        // CRC-32 check value, and it continues across pieces.
        assert_eq!(crc32_update(0, b"123456789"), 0xCBF4_3926);
        let data = ramp(7, 1000);
        assert_eq!(
            crc32_update(crc32_update(0, &data[..333]), &data[333..]),
            crc32_update(0, &data)
        );
    }

    #[test]
    fn capture_buffer_failed_writes_store_nothing_and_back_off() {
        let dir = tmp("enospc");
        let hooks = faults();
        let c = cfg(1e9, Some(2 * MIN_CHUNK_BYTES));
        let (buf, mut w) = IqBuffer::open_with(&dir, c, hooks.clone()).unwrap();
        assert_eq!(
            names(&dir),
            RING_NAMES.map(String::from).to_vec().tap_sort()
        );
        assert_eq!(ring_len(&dir), 2 * MIN_CHUNK_BYTES, "allocated up front");
        hooks.fail.store(true, Ordering::SeqCst);
        let blocks = 2000u64;
        for i in 0..blocks {
            w.begin_segment(start(i * 1024, i as i64 * 1_000_000, 100e6, 1e6));
            assert!(w.append(&ramp(i * 1024, 1024)).is_err());
        }
        let s = buf.status(None, None, 10);
        assert_eq!((s.chunk_files, s.samples), (0, 0), "{s:?}");
        assert_eq!(s.allocated_bytes, 2 * MIN_CHUNK_BYTES);
        assert!(s.disk_bytes > s.allocated_bytes, "ring + journal: {s:?}");
        assert_eq!(ring_len(&dir), 2 * MIN_CHUNK_BYTES, "the ring never grows");
        let attempts = hooks.writes.load(Ordering::SeqCst);
        assert!(
            (1..=3).contains(&s.write_errors) && attempts == s.write_errors,
            "backed off: {attempts} attempts for {blocks} blocks, {s:?}"
        );
        assert_eq!(s.failed_samples, blocks * 1024);
        assert_eq!(s.segments_total, 0);
        assert!(s.error.is_some());
        // Writable again after the back-off: exactly the data.
        hooks.fail.store(false, Ordering::SeqCst);
        std::thread::sleep(RETRY_BACKOFF_MIN * (1 << s.write_errors) + Duration::from_millis(50));
        w.begin_segment(start(1 << 30, 5_000_000_000, 100e6, 1e6));
        w.append(&ramp(0, 1024)).unwrap();
        let s = buf.status(None, None, 10);
        assert_eq!((s.chunk_files, s.samples), (1, 1024), "{s:?}");
        // A failure after that stores nothing more; the ring keeps its size.
        hooks.fail.store(true, Ordering::SeqCst);
        assert!(w.append(&ramp(1024, 1024)).is_err());
        let s = buf.status(None, None, 10);
        assert_eq!((s.chunk_files, s.samples), (1, 1024));
        assert_eq!(ring_len(&dir), 2 * MIN_CHUNK_BYTES);
        // Only what was stored comes back after a restart.
        drop(w);
        drop(buf);
        hooks.fail.store(false, Ordering::SeqCst);
        let (buf, w2) = IqBuffer::open_with(&dir, c, hooks).unwrap();
        let s = buf.status(None, None, 10);
        assert_eq!((s.samples, s.recovered_segments), (1024, 1), "{s:?}");
        assert_eq!(
            export(
                &buf,
                ClipRange::Index {
                    start: 0,
                    end: 1 << 40
                },
                Some(1)
            )
            .unwrap(),
            ramp(0, 1024)
        );
        drop(w2);
        drop(buf);
        let _ = fs::remove_dir_all(&dir);
    }

    trait TapSort {
        fn tap_sort(self) -> Self;
    }

    impl TapSort for Vec<String> {
        fn tap_sort(mut self) -> Self {
            self.sort();
            self
        }
    }

    #[test]
    fn capture_buffer_sparse_ring_pauses_below_the_free_space_floor_and_resumes() {
        let dir = tmp("floor");
        let hooks = faults();
        hooks.sparse.store(true, Ordering::SeqCst);
        let mut c = cfg(1e9, Some(4 * MIN_CHUNK_BYTES));
        c.min_free_bytes = Some(1 << 30);
        let (buf, mut w) = IqBuffer::open_with(&dir, c, hooks.clone()).unwrap();
        let per_chunk = MIN_CHUNK_BYTES / 2;
        w.begin_segment(start(0, 0, 100e6, 1e6));
        w.append(&ramp(0, per_chunk)).unwrap();
        // Another slot of the sparse ring would leave less than the floor free.
        hooks.free.store(1 << 30, Ordering::SeqCst);
        w.append(&ramp(per_chunk, 100)).unwrap();
        assert!(!w.is_open() && w.is_paused());
        for i in 0..50 {
            w.begin_segment(start(per_chunk + 100 + i * 100, 0, 100e6, 1e6));
            w.append(&ramp(0, 100)).unwrap();
        }
        let s = buf.status(None, None, 10);
        assert!(s.paused && !s.preallocated, "{s:?}");
        assert_eq!((s.pauses, s.paused_samples), (1, 5100));
        assert_eq!((s.chunk_files, s.samples), (1, per_chunk));
        assert_eq!(
            (s.fs_free_bytes, s.fs_total_bytes, s.min_free_bytes),
            (Some(1 << 30), Some(TOTAL), Some(1 << 30))
        );
        assert_eq!((s.write_errors, s.failed_samples), (0, 0));
        assert_eq!(files(&dir).len(), 3);
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
        let (buf, mut w) = IqBuffer::open(&dir, cfg(600.0, Some(1 << 20))).unwrap();
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
        drop(buf);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn capture_buffer_segments_clip_and_duration_eviction() {
        let dir = tmp("dur");
        let fs_hz = 1000.0;
        let (buf, mut w) = IqBuffer::open(&dir, cfg(2.0, Some(1 << 20))).unwrap();
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
        // The duration floor is persisted: a restart retains the same span.
        drop(w);
        drop(buf);
        let (buf, w) = IqBuffer::open(&dir, cfg(2.0, Some(1 << 20))).unwrap();
        let r = buf.status(None, None, 10);
        assert_eq!(
            (r.t0, r.t1, r.samples),
            (Some(1.0), Some(3.0), 1500),
            "{r:?}"
        );
        drop(w);
        drop(buf);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn capture_buffer_byte_quota_evicts_whole_oldest_slots() {
        let dir = tmp("bytes");
        let c = cfg(1e9, Some(2 * MIN_CHUNK_BYTES));
        let (buf, mut w) = IqBuffer::open(&dir, c).unwrap();
        let fs_hz = 1e6;
        w.begin_segment(start(0, 0, 100e6, fs_hz));
        let per_chunk = MIN_CHUNK_BYTES / 2;
        // Five slots' worth: two stay.
        for k in 0..5 {
            w.append(&ramp(k * per_chunk, per_chunk)).unwrap();
        }
        let s = buf.status(None, None, 10);
        assert_eq!(s.allocated_bytes, 2 * MIN_CHUNK_BYTES, "{s:?}");
        assert_eq!(s.evicted.chunks, 3);
        assert_eq!(s.samples, 2 * per_chunk);
        assert_eq!(s.segments[0].global_index, 3 * per_chunk);
        assert_eq!(files(&dir).len(), 3);
        assert_eq!(ring_len(&dir), 2 * MIN_CHUNK_BYTES);
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
        // The run's end keeps the ring (it persists); the next open recovers it.
        let before = buf.status(None, None, 10);
        drop(w);
        drop(buf);
        assert_eq!(files(&dir).len(), 3);
        let (buf, w) = IqBuffer::open(&dir, c).unwrap();
        let after = buf.status(None, None, 10);
        assert_eq!(seg_keys(&after), seg_keys(&before));
        drop(w);
        drop(buf);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ring_wrap_keeps_files_constant_and_evicts_the_oldest_segments_in_order() {
        let dir = tmp("wrap");
        let slot = MIN_CHUNK_BYTES;
        let (buf, mut w) = IqBuffer::open(&dir, cfg(1e9, Some(4 * slot))).unwrap();
        let s = buf.status(None, None, 10);
        assert_eq!((s.slot_count, s.allocated_bytes), (4, 4 * slot));
        assert_eq!(
            (s.allocation, s.persisted, s.run),
            (Some(Allocation::Full), true, Some(1))
        );
        let names0 = names(&dir);
        let per_seg = slot / 4; // half a slot of samples
        let mut g = 0u64;
        let mut first_id = 0u64;
        for k in 0..24u64 {
            w.begin_segment(start(g, g as i64 * 1000, 100e6 + k as f64, 1e6));
            w.append(&ramp(g, per_seg)).unwrap();
            g += per_seg;
            assert_eq!(names(&dir), names0, "the same files after segment {k}");
            assert_eq!(
                ring_len(&dir),
                4 * slot,
                "the same ring size after segment {k}"
            );
            let journal = fs::metadata(dir.join(JOURNAL_FILE)).unwrap().len();
            assert!(journal < JOURNAL_COMPACT_MIN + (64 << 10), "{journal}");
            let s = buf.status(None, None, 100);
            let ids: Vec<u64> = s.segments.iter().map(|x| x.id).collect();
            assert_eq!(*ids.last().unwrap(), k);
            assert!(ids.windows(2).all(|p| p[1] == p[0] + 1), "{ids:?}");
            assert!(ids[0] >= first_id, "evicted oldest first: {ids:?}");
            first_id = ids[0];
            assert!(s.bytes <= 4 * slot);
        }
        let s = buf.status(None, None, 100);
        assert_eq!(s.evicted.chunks, 8, "12 slots written into 4");
        assert_eq!(s.evicted.segments, 16);
        assert_eq!(first_id, 16);
        assert_eq!(
            (s.wrap_count, s.head_slot, s.head_offset_bytes),
            (2, Some(3), Some(4 * slot))
        );
        assert_eq!(s.chunk_files, 4);
        // The retained data is intact after overwriting in place.
        let data = export(
            &buf,
            ClipRange::Index {
                start: 0,
                end: u64::MAX,
            },
            None,
        )
        .unwrap();
        assert_eq!(data, ramp(16 * per_seg, 8 * per_seg));
        drop(w);
        drop(buf);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ring_restart_recovers_segments_and_byte_identical_clips_and_resizes() {
        let dir = tmp("restart");
        let slot = MIN_CHUNK_BYTES;
        let per = slot / 2; // samples per slot
        let c8 = cfg(1e9, Some(8 * slot));
        let t0 = 1_757_000_000_000_000_000i64;
        let (buf, mut w) = IqBuffer::open(&dir, c8).unwrap();
        assert!(IqBuffer::open(&dir, c8).is_err(), "locked while open");
        w.begin_segment(start(0, t0, 100e6, 1e6));
        for k in 0..5 {
            w.append(&ramp(k * per / 2, per / 2)).unwrap();
        }
        w.begin_segment(start(3 * per, t0 + 3 * per as i64 * 1000, 101e6, 1e6));
        w.append(&ramp(3 * per, per)).unwrap();
        let before = buf.status(None, None, 100);
        let range = ClipRange::Index {
            start: per / 2,
            end: per / 2 + per,
        };
        let clip_before = export(&buf, range, None).unwrap();
        assert_eq!(clip_before, ramp(per / 2, per));
        drop(w);
        drop(buf);

        // Restart: the same span and segments, run 2, byte-identical clips of run 1.
        let (buf, mut w) = IqBuffer::open(&dir, c8).unwrap();
        let s = buf.status(None, None, 100);
        assert_eq!(
            (s.run, s.recovered_segments, s.discarded_slots),
            (Some(2), 2, 0)
        );
        assert_eq!(seg_keys(&s), seg_keys(&before));
        assert_eq!(
            (s.t0, s.t1, s.samples),
            (before.t0, before.t1, before.samples)
        );
        assert!(
            matches!(export(&buf, range, None), Err(ClipError::Empty)),
            "indices are per run"
        );
        assert_eq!(export(&buf, range, Some(1)).unwrap(), clip_before);
        let t_range = ClipRange::Time {
            t0_ns: t0 + (per / 2) as i64 * 1000,
            t1_ns: t0 + (per / 2 + per) as i64 * 1000,
        };
        assert_eq!(export(&buf, t_range, None).unwrap(), clip_before);
        // A replayed recording restarts at the same times: run 2 overlaps run 1.
        w.begin_segment(start(0, t0, 100e6, 1e6));
        w.append(&ramp(0, per)).unwrap();
        let s = buf.status(None, None, 100);
        assert_eq!((s.segments[2].id, s.segments[2].run), (2, 2));
        assert!(matches!(
            export(&buf, t_range, None),
            Err(ClipError::MixedRuns { .. })
        ));
        assert_eq!(export(&buf, t_range, Some(1)).unwrap(), clip_before);
        assert_eq!(
            export(&buf, ClipRange::Index { start: 0, end: per }, None).unwrap(),
            ramp(0, per)
        );
        let before = buf.status(None, None, 100);
        drop(w);
        drop(buf);

        // A larger quota keeps everything.
        let c16 = cfg(1e9, Some(16 * slot));
        let (buf, w) = IqBuffer::open(&dir, c16).unwrap();
        let s = buf.status(None, None, 100);
        assert_eq!((s.slot_count, s.allocation), (16, Some(Allocation::Full)));
        assert_eq!(ring_len(&dir), 16 * slot);
        assert_eq!(seg_keys(&s), seg_keys(&before));
        drop(w);
        drop(buf);

        // A smaller one keeps the newest contiguous slots stored below the new size (run 1
        // wrote positions 0..3, run 2 filled 3 and 4).
        let c3 = cfg(1e9, Some(3 * slot));
        let (buf, w) = IqBuffer::open(&dir, c3).unwrap();
        let s = buf.status(None, None, 100);
        assert_eq!(
            (s.slot_count, s.samples, s.discarded_slots),
            (3, 3 * per, 2),
            "{s:?}"
        );
        assert_eq!(ring_len(&dir), 3 * slot);
        assert_eq!(export(&buf, range, Some(1)).unwrap(), clip_before);
        drop(w);
        drop(buf);

        // Another slot size resets the ring.
        let big = cfg(1e9, Some(64 << 20));
        let (buf, w) = IqBuffer::open(&dir, big).unwrap();
        let s = buf.status(None, None, 100);
        assert_eq!(
            (s.samples, s.recovered_segments, s.chunk_bytes),
            (0, 0, 4 << 20)
        );
        drop(w);
        drop(buf);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ring_crash_with_a_torn_slot_or_stale_journal_recovers_a_consistent_index() {
        let dir = tmp("crash");
        let slot = MIN_CHUNK_BYTES;
        let per = slot / 2;
        let c = cfg(1e9, Some(8 * slot));
        let reopen = || {
            let (buf, w) = IqBuffer::open(&dir, c).unwrap();
            let s = buf.status(None, None, 100);
            (buf, w, s)
        };
        let all = ClipRange::Index {
            start: 0,
            end: u64::MAX,
        };

        // 1. A crash with an unsealed tail and a torn journal record.
        let (buf, mut w) = IqBuffer::open(&dir, c).unwrap();
        w.begin_segment(start(0, 0, 100e6, 1e6));
        w.append(&ramp(0, 2 * per)).unwrap();
        w.checkpoint().unwrap();
        w.append(&ramp(2 * per, per / 2)).unwrap();
        w.simulate_crash();
        drop(buf);
        let mut j = OpenOptions::new()
            .append(true)
            .open(dir.join(JOURNAL_FILE))
            .unwrap();
        j.write_all(&[0xff, 0xff, 0, 0, 7]).unwrap();
        drop(j);
        let (buf, w, s) = reopen();
        assert_eq!(
            (s.samples, s.recovered_segments, s.discarded_slots),
            (2 * per, 1, 1),
            "{s:?}"
        );
        assert_eq!(export(&buf, all, Some(1)).unwrap(), ramp(0, 2 * per));
        let journal = fs::read(dir.join(JOURNAL_FILE)).unwrap();
        assert_eq!(
            decode(&journal).1,
            journal.len(),
            "the journal was rewritten whole"
        );
        drop(w);
        drop(buf);

        // 2. A torn newest slot under a stale seal: that slot fails its CRC and is dropped.
        let ring = OpenOptions::new()
            .write(true)
            .open(dir.join(RING_FILE))
            .unwrap();
        ring.write_all_at(&[0x55; 3], slot + 100).unwrap();
        drop(ring);
        let (buf, w, s) = reopen();
        assert_eq!((s.samples, s.discarded_slots), (per, 1), "{s:?}");
        assert_eq!(export(&buf, all, Some(1)).unwrap(), ramp(0, per));
        drop(w);
        drop(buf);

        // 3. A truncated ring file: nothing the index names lies inside it any more.
        let ring = OpenOptions::new()
            .write(true)
            .open(dir.join(RING_FILE))
            .unwrap();
        ring.set_len(slot / 2).unwrap();
        drop(ring);
        let (buf, mut w, s) = reopen();
        assert_eq!(
            (s.samples, s.segments_total, s.recovered_segments),
            (0, 0, 0),
            "{s:?}"
        );
        assert_eq!(ring_len(&dir), 8 * slot, "re-allocated");
        w.begin_segment(start(0, 0, 100e6, 1e6));
        w.append(&ramp(0, 1000)).unwrap();
        assert_eq!(export(&buf, all, None).unwrap(), ramp(0, 1000));
        drop(w);
        drop(buf);

        // 4. A garbage journal: a clean reset.
        fs::write(dir.join(JOURNAL_FILE), [0u8; 64]).unwrap();
        let (buf, w, s) = reopen();
        assert_eq!((s.samples, s.segments_total), (0, 0), "{s:?}");
        drop(w);
        drop(buf);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ring_allocation_shrinks_or_refuses_when_free_space_is_low() {
        let dir = tmp("alloc");
        let hooks = faults();
        let slot = MIN_CHUNK_BYTES;
        let mut c = cfg(1e9, Some(8 * slot));
        c.min_free_bytes = Some(1 << 30);
        hooks
            .free
            .store((1 << 30) + 3 * slot + 100, Ordering::SeqCst);
        let (buf, mut w) = IqBuffer::open_with(&dir, c, hooks.clone()).unwrap();
        let s = buf.status(None, None, 10);
        assert_eq!(
            (s.quota_bytes, s.slot_count, s.allocated_bytes, s.allocation),
            (8 * slot, 3, 3 * slot, Some(Allocation::Shrunk)),
            "{s:?}"
        );
        assert_eq!(ring_len(&dir), 3 * slot);
        w.begin_segment(start(0, 0, 100e6, 1e6));
        w.append(&ramp(0, 5 * slot / 2)).unwrap();
        assert_eq!(
            buf.status(None, None, 10).samples,
            3 * slot / 2,
            "a 3-slot ring"
        );
        drop(w);
        drop(buf);
        let _ = fs::remove_dir_all(&dir);

        // Not even two slots above the floor: refused, nothing allocated.
        hooks.free.store((1 << 30) + slot, Ordering::SeqCst);
        let e = IqBuffer::open_with(&dir, c, hooks).err().expect("refused");
        assert!(is_allocation_refused(&e), "{e}");
        assert!(e.to_string().contains("allocation refused"), "{e}");
        assert!(!dir.join(RING_FILE).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    /// A failed ring fsync poisons the unsealed span: it leaves the index at once (no clip exports
    /// it), is counted, and a later successful fsync never seals over it; recovery keeps exactly
    /// what was durable plus what was rewritten and synced after the failure.
    #[test]
    fn ring_fsync_failure_poisons_the_unsealed_span_and_never_seals_it() {
        let dir = tmp("fsync-poison");
        let hooks = faults();
        let c = cfg(1e9, Some(4 * MIN_CHUNK_BYTES));
        let (buf, mut w) = IqBuffer::open_with(&dir, c, hooks.clone()).unwrap();
        let all = ClipRange::Index {
            start: 0,
            end: 1 << 40,
        };
        // A: durable.
        w.begin_segment(start(0, 0, 100e6, 1e6));
        w.append(&ramp(0, 4096)).unwrap();
        w.checkpoint().unwrap();
        // B: written, then its fsync fails.
        w.append(&ramp(4096, 4096)).unwrap();
        hooks.sync_fail.store(true, Ordering::SeqCst);
        assert!(w.checkpoint().is_err());
        hooks.sync_fail.store(false, Ordering::SeqCst);
        // What the disk may hold for B after the failure: not B.
        let raw = OpenOptions::new()
            .write(true)
            .open(dir.join(RING_FILE))
            .unwrap();
        raw.write_all_at(&[0xEE; 8192], 8192).unwrap();
        let s = buf.status(None, None, 10);
        assert_eq!(
            (s.samples, s.sync_errors, s.poisoned_samples),
            (4096, 1, 4096),
            "{s:?}"
        );
        assert!(!w.is_open(), "the poisoned segment ended");
        assert!(s.error.is_some());
        assert!(matches!(
            export(
                &buf,
                ClipRange::Index {
                    start: 4096,
                    end: 8192
                },
                None
            ),
            Err(ClipError::Empty)
        ));
        assert_eq!(export(&buf, all, None).unwrap(), ramp(0, 4096));
        // C: rewritten over B's span and synced; the seal covers A and C only.
        w.begin_segment(start(8192, 8_192_000_000, 100e6, 1e6));
        w.append(&ramp(8192, 1024)).unwrap();
        w.checkpoint().unwrap();
        let mut want = ramp(0, 4096);
        want.extend(ramp(8192, 1024));
        assert_eq!(export(&buf, all, None).unwrap(), want);
        w.simulate_crash();
        drop(buf);
        let (buf, w) = IqBuffer::open_with(&dir, c, hooks).unwrap();
        let s = buf.status(None, None, 10);
        assert_eq!(
            (s.samples, s.discarded_slots, s.recovered_segments),
            (5120, 0, 2),
            "the seal's CRC matches the disk: {s:?}"
        );
        assert_eq!(export(&buf, all, Some(1)).unwrap(), want);
        drop(w);
        drop(buf);
        let _ = fs::remove_dir_all(&dir);
    }

    /// T-536: **every allocation step is flushed before the open returns.** A reservation's real
    /// cost falls on the first fsync of the file, so an open that reserves blocks and does not
    /// flush them has not finished the work — it has handed it to whichever writer fsyncs next,
    /// which for this ring is the segment feeder's checkpoint, inside a re-plumb.
    #[test]
    fn every_allocation_step_is_flushed_before_the_open_returns() {
        /// Counts the two halves of an allocation and the order they happened in.
        #[derive(Default)]
        struct Counting {
            steps: AtomicU64,
            flushes: AtomicU64,
            /// Reservations made since the last flush: zero when the open returns, or the open
            /// has left blocks reserved and uncommitted for someone else to pay for.
            unflushed_at_end: AtomicU64,
        }
        impl IqBufferHooks for Counting {
            fn fs_space(&self, _: &Path) -> io::Result<FsSpace> {
                Ok(FsSpace {
                    free: 1 << 42,
                    total: 1 << 43,
                })
            }
            fn preallocate(&self, file: &File, len: u64) -> io::Result<bool> {
                self.steps.fetch_add(1, Ordering::SeqCst);
                self.unflushed_at_end.fetch_add(1, Ordering::SeqCst);
                file.set_len(len)?;
                Ok(true)
            }
            fn sync_allocation(&self, _: &File) -> io::Result<()> {
                self.flushes.fetch_add(1, Ordering::SeqCst);
                self.unflushed_at_end.store(0, Ordering::SeqCst);
                Ok(())
            }
        }

        let dir = tmp("alloc-flush");
        // Four 1 GiB steps, so "flushed once at the very end" is not enough to pass either.
        let hooks = Arc::new(Counting::default());
        let c = cfg(1e9, Some(4 << 30));
        let (buf, w) = IqBuffer::open_with(&dir, c, Arc::clone(&hooks) as Arc<dyn IqBufferHooks>)
            .expect("open");
        let steps = hooks.steps.load(Ordering::SeqCst);
        assert!(
            steps >= 4,
            "the ring was allocated in {steps} steps, not several"
        );
        assert_eq!(
            hooks.flushes.load(Ordering::SeqCst),
            steps,
            "{} of {steps} allocation steps were flushed; an unflushed reservation is a bill for \
             the next thread that fsyncs the ring",
            hooks.flushes.load(Ordering::SeqCst)
        );
        assert_eq!(
            hooks.unflushed_at_end.load(Ordering::SeqCst),
            0,
            "the open returned with blocks reserved and not committed"
        );
        drop(w);
        drop(buf);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ring_locked_by_another_holder_is_reported_as_locked() {
        let dir = tmp("locked");
        let c = cfg(1e9, Some(2 * MIN_CHUNK_BYTES));
        let (buf, w) = IqBuffer::open_with(&dir, c, faults()).unwrap();
        let e = IqBuffer::open_with(&dir, c, faults())
            .err()
            .expect("locked");
        assert!(is_ring_locked(&e) && !is_allocation_refused(&e), "{e}");
        assert!(e.to_string().contains("in use by another process"), "{e}");
        drop(w);
        drop(buf);
        let (buf, w) = IqBuffer::open_with(&dir, c, faults()).expect("free again");
        drop(w);
        drop(buf);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A ring whose journal header carries a newer version is refused and left byte-identical.
    #[test]
    fn ring_of_a_newer_version_disables_the_buffer_and_is_not_wiped() {
        let dir = tmp("newer");
        let c = cfg(1e9, Some(2 * MIN_CHUNK_BYTES));
        let (buf, mut w) = IqBuffer::open_with(&dir, c, faults()).unwrap();
        w.begin_segment(start(0, 0, 100e6, 1e6));
        w.append(&ramp(0, 1000)).unwrap();
        drop(w);
        drop(buf);
        let jpath = dir.join(JOURNAL_FILE);
        let old = fs::read(&jpath).unwrap();
        let first = 8 + u32::from_le_bytes(old[0..4].try_into().unwrap()) as usize;
        let header = serde_json::to_vec(&serde_json::json!({
            "k": "header", "magic": RING_MAGIC, "version": RING_VERSION + 1,
            "slot_bytes": MIN_CHUNK_BYTES, "slots": 2, "future_field": true
        }))
        .unwrap();
        let mut j = (header.len() as u32).to_le_bytes().to_vec();
        j.extend(crc32_update(0, &header).to_le_bytes());
        j.extend(&header);
        j.extend(&old[first..]);
        fs::write(&jpath, &j).unwrap();
        let ring = fs::read(dir.join(RING_FILE)).unwrap();
        for _ in 0..2 {
            let e = IqBuffer::open_with(&dir, c, faults())
                .err()
                .expect("incompatible");
            assert!(is_ring_incompatible(&e), "{e}");
            assert!(e.to_string().contains("newer format"), "{e}");
        }
        assert_eq!(fs::read(&jpath).unwrap(), j, "the journal is untouched");
        assert_eq!(fs::read(dir.join(RING_FILE)).unwrap(), ring, "the ring too");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Allocation grows the ring in steps with progress, and a cancel leaves the file as it was.
    #[test]
    fn ring_allocation_reports_progress_in_steps_and_cancels() {
        let dir = tmp("steps");
        let hooks = faults();
        hooks.sparse.store(true, Ordering::SeqCst);
        let c = cfg(1e9, Some(3 << 30));
        assert_eq!((c.chunk_bytes(), c.slot_count()), (MAX_CHUNK_BYTES, 48));
        let seen = Mutex::new(Vec::new());
        let never = AtomicBool::new(false);
        let (buf, w) = IqBuffer::open_with_progress(
            &dir,
            c,
            hooks.clone(),
            &|f| seen.lock().unwrap().push(f),
            &never,
        )
        .unwrap();
        assert_eq!(*seen.lock().unwrap(), vec![0.0, 1.0 / 3.0, 2.0 / 3.0, 1.0]);
        assert_eq!(buf.status(None, None, 0).allocation_progress, Some(1.0));
        drop(w);
        drop(buf);
        let _ = fs::remove_dir_all(&dir);

        let cancel = AtomicBool::new(false);
        let e = IqBuffer::open_with_progress(
            &dir,
            c,
            hooks,
            &|f| {
                if f > 0.0 {
                    cancel.store(true, Ordering::SeqCst);
                }
            },
            &cancel,
        )
        .err()
        .expect("cancelled");
        assert_eq!(e.kind(), io::ErrorKind::Interrupted, "{e}");
        assert_eq!(ring_len(&dir), 0, "back to its size before the open");
        let _ = fs::remove_dir_all(&dir);
    }
}
