//! Recorded decoded streams (T-092, ADR-0011 §4, `docs/stream-contract.md` §14.7): always-on
//! capture of pipeline inspector streams, a sidecar frame index for scrubbing, a quota with
//! oldest-first eviction, and the [`CaptureSource`] reader the inspector API opens.
//!
//! **Files** per capture, in the store directory:
//! - `<id>.hks`: the §3 byte stream itself: the header frame, then the records as published
//!   (frame records without `content.layers`, which are derived and re-computable; `status`,
//!   `edit` and drop-marker records verbatim). `StreamReader`/`RecordedFrames` read it as a socket.
//! - `<id>.idx`: the frame index, one [`INDEX_ENTRY_LEN`]-byte little-endian entry per frame
//!   record in stored order: `u64` byte offset of the record's length prefix in `.hks`, then
//!   `i64` `t` (Unix ns). Frame `n` is entry `n`, so paging seeks (`open_at`) and a time scrub is
//!   a binary search (`frame_at_time`).
//! - `<id>.json`: the catalogue entry ([`CaptureInfo`]) plus `header_len`, rewritten at start, at
//!   least every [`META_EVERY`] while recording, and at the end.
//!
//! **Recording never blocks the pipeline.** [`DecodedCaptures::tee`] subscribes a *local*
//! consumer to the stream's publisher: the publisher's bounded per-consumer ring
//! ([`CaptureQuota::queue_bytes`]) is the queue and its per-consumer writer thread does the disk
//! I/O. A slow disk fills the ring and the publisher drops records (counted, then marked with a
//! drop marker that this writer stores and sums into `dropped_records`); the publishing thread
//! never waits. The egress gate has already run, so a frame whose class forbids content arrives
//! (and is stored) metadata-only. The recorder is exempt from the publisher's slow-consumer
//! disconnect, so a disk stall of any length loses (and counts) records but never the recording.
//!
//! **Failures.** A write error never writes a byte twice (a retry resumes after the bytes that
//! landed) and a segment that ends on an error is truncated to its last complete commit. On open,
//! a capture left recording by a crashed process is cut back to its last complete, indexed record
//! (torn tail records and partial index entries are removed) and closed as `interrupted`.
//!
//! **Quota** ([`CaptureQuota`], defaults 1 GiB total, 64 MiB per capture): a capture that reaches
//! the per-capture size rolls to a new segment (a new capture id, `segment + 1`, same header);
//! whenever the store's total exceeds the quota, the oldest finished captures are deleted. If only
//! recording captures remain over the quota, their current segments roll (becoming evictable)
//! and records are dropped and counted until the store is under quota again.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Cursor, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hk_stream::frame::{MAX_FRAME_LEN, encode_frame};
use hk_stream::inspector::{
    CaptureCursor, CaptureDelete, CaptureInfo, CaptureSource, INSPECTOR_MESSAGE_SCHEMA,
};
use hk_stream::{
    CloseReason, ConsumerId, Declared, FrameDecoder, HEADER_MAX_LEN, PublisherHandle, StreamError,
    StreamHeader, StreamKind,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Bytes per frame-index entry: `u64` offset + `i64` `t` (Unix ns), little-endian.
pub const INDEX_ENTRY_LEN: u64 = 16;
/// How often a recording capture's `.json` is rewritten (crash recovery keeps its counts).
pub const META_EVERY: Duration = Duration::from_secs(5);
/// Smallest per-capture size honoured (a segment always holds at least one frame record).
pub const MIN_CAPTURE_BYTES: u64 = 2 * 1024;
/// Smallest per-consumer queue: a header, one default-size (1 MiB) record and a marker.
const MIN_QUEUE_BYTES: usize = 2 * 1024 * 1024 + 64 * 1024;
const META_SCHEMA: &str = "hackriff-decoded-capture/1";
/// Bytes in a §3 record's little-endian `u32` length prefix.
const LEN_PREFIX: u64 = 4;

/// Environment override of [`CaptureQuota::total_bytes`].
pub const ENV_TOTAL_BYTES: &str = "HK_DECODED_CAPTURE_TOTAL_BYTES";
/// Environment override of [`CaptureQuota::capture_bytes`].
pub const ENV_CAPTURE_BYTES: &str = "HK_DECODED_CAPTURE_BYTES";

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn now_s() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

fn ns_to_s(ns: i64) -> f64 {
    ns as f64 / 1e9
}

/// Size limits of the capture store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaptureQuota {
    /// Most bytes (stream + index) kept across every capture; oldest finished captures are evicted
    /// past it.
    pub total_bytes: u64,
    /// Stream bytes per capture before it rolls to a new segment (clamped to at most a quarter of
    /// the total and at least [`MIN_CAPTURE_BYTES`]).
    pub capture_bytes: u64,
    /// Publisher queue per recorded stream (clamped to at least a header plus one maximum-size
    /// record); past it records are dropped and counted, never waited for.
    pub queue_bytes: usize,
}

impl Default for CaptureQuota {
    fn default() -> Self {
        Self {
            total_bytes: 1024 * 1024 * 1024,
            capture_bytes: 64 * 1024 * 1024,
            queue_bytes: 4 * 1024 * 1024,
        }
    }
}

impl CaptureQuota {
    /// The defaults with [`ENV_TOTAL_BYTES`] / [`ENV_CAPTURE_BYTES`] applied when set.
    pub fn from_env() -> Self {
        let mut q = Self::default();
        let var = |k| {
            std::env::var(k)
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
        };
        if let Some(v) = var(ENV_TOTAL_BYTES) {
            q.total_bytes = v;
        }
        if let Some(v) = var(ENV_CAPTURE_BYTES) {
            q.capture_bytes = v;
        }
        q
    }

    fn segment_bytes(&self) -> u64 {
        self.capture_bytes
            .min(self.total_bytes / 4)
            .max(MIN_CAPTURE_BYTES)
    }
}

/// Opens the stream (`.hks`) file of a capture for writing. The default creates a file; tests
/// and alternative media substitute their own.
pub type DataWriterFactory = Arc<dyn Fn(&Path) -> io::Result<Box<dyn Write + Send>> + Send + Sync>;

#[derive(Serialize, Deserialize)]
struct MetaFile {
    schema: String,
    header_len: u64,
    #[serde(flatten)]
    info: CaptureInfo,
}

struct Entry {
    info: CaptureInfo,
    header_len: u64,
}

impl Entry {
    fn stored_bytes(&self) -> u64 {
        self.info.bytes + self.info.frames * INDEX_ENTRY_LEN
    }
}

struct Inner {
    dir: PathBuf,
    quota: Mutex<CaptureQuota>,
    factory: Mutex<Option<DataWriterFactory>>,
    entries: Mutex<BTreeMap<String, Entry>>,
    seq: AtomicU64,
    over_quota: AtomicBool,
}

/// The always-on decoded-stream capture store (see the module docs). Cheap to clone.
#[derive(Clone)]
pub struct DecodedCaptures(Arc<Inner>);

/// Whether `id` is a capture id (`[A-Za-z0-9_.:-]{1,128}`, not starting with `.`), so it can
/// never name a path outside the store.
pub fn is_capture_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && !id.starts_with('.')
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.:-".contains(&b))
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(40)
        .collect()
}

impl Inner {
    fn path(&self, id: &str, ext: &str) -> PathBuf {
        self.dir.join(format!("{id}.{ext}"))
    }

    fn remove_files(&self, id: &str) {
        for ext in ["hks", "idx", "json"] {
            let _ = fs::remove_file(self.path(id, ext));
        }
    }

    fn write_meta(&self, e: &Entry) {
        let meta = MetaFile {
            schema: META_SCHEMA.into(),
            header_len: e.header_len,
            info: e.info.clone(),
        };
        let tmp = self.path(&e.info.id, "json.tmp");
        let ok = serde_json::to_vec_pretty(&meta)
            .map_err(io::Error::other)
            .and_then(|b| fs::write(&tmp, b));
        if ok.is_ok() {
            let _ = fs::rename(&tmp, self.path(&e.info.id, "json"));
        }
    }

    /// Evicts the oldest finished captures until the store is under its total quota.
    fn enforce_quota(&self) {
        let total = lock(&self.quota).total_bytes;
        let mut entries = lock(&self.entries);
        let mut used: u64 = entries.values().map(Entry::stored_bytes).sum();
        if used > total {
            // Ids start with the zero-padded start time: id order is start order.
            let victims: Vec<String> = entries
                .values()
                .filter(|e| !e.info.recording)
                .map(|e| e.info.id.clone())
                .collect();
            for id in victims {
                if used <= total {
                    break;
                }
                if let Some(e) = entries.remove(&id) {
                    used = used.saturating_sub(e.stored_bytes());
                    self.remove_files(&id);
                }
            }
        }
        self.over_quota.store(used > total, Ordering::Relaxed);
    }

    fn new_id(&self, pipeline: &str, output: &str) -> String {
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        let n = self.seq.fetch_add(1, Ordering::Relaxed);
        // Zero-padded so ids of captures started in the same millisecond sort in start order.
        let mut id = format!(
            "dc{ms:013}-{n:08}-{}-{}",
            sanitize(pipeline),
            sanitize(output)
        );
        id.truncate(128);
        id
    }

    /// Repairs a capture a previous process left recording. A power loss can leave a torn final
    /// record, a partial index entry, or an entry for a record that never landed: keep the longest
    /// run of complete records whose frame records are all indexed, truncate both files to it, and
    /// take `frames`, `bytes`, `t_first`/`t_last` from what is kept (`dropped_records` from the
    /// drop markers kept, if more than the catalogue last saw). Returns the frames kept.
    fn recover(&self, e: &mut Entry) -> u64 {
        let (data_path, idx_path) = (self.path(&e.info.id, "hks"), self.path(&e.info.id, "idx"));
        let data_len = fs::metadata(&data_path).map_or(0, |m| m.len());
        let index: Vec<(u64, i64)> = fs::read(&idx_path)
            .unwrap_or_default()
            .chunks_exact(INDEX_ENTRY_LEN as usize)
            .map(|b| {
                let (off, t) = b.split_at(8);
                (
                    u64::from_le_bytes(off.try_into().unwrap_or_default()),
                    i64::from_le_bytes(t.try_into().unwrap_or_default()),
                )
            })
            .collect();
        let (frames, end, dropped) = match File::open(&data_path) {
            Ok(f) if data_len >= e.header_len => {
                walk_records(&mut BufReader::new(f), e.header_len, data_len, &index)
            }
            _ => (0, 0, 0),
        };
        truncate(&data_path, end);
        truncate(&idx_path, frames * INDEX_ENTRY_LEN);
        let kept = &index[..usize::try_from(frames).unwrap_or(0).min(index.len())];
        e.info.frames = frames;
        e.info.bytes = end;
        e.info.t_first = kept.first().map(|&(_, t)| ns_to_s(t));
        e.info.t_last = kept.last().map(|&(_, t)| ns_to_s(t));
        e.info.dropped_records = e.info.dropped_records.max(dropped);
        frames
    }

    fn index_entry(&self, id: &str, k: u64) -> io::Result<(u64, i64)> {
        let mut f = File::open(self.path(id, "idx"))?;
        read_index_entry(&mut f, k)
    }
}

fn read_index_entry(f: &mut File, k: u64) -> io::Result<(u64, i64)> {
    let mut b = [0u8; INDEX_ENTRY_LEN as usize];
    f.seek(SeekFrom::Start(k * INDEX_ENTRY_LEN))?;
    f.read_exact(&mut b)?;
    let mut off = [0u8; 8];
    let mut t = [0u8; 8];
    off.copy_from_slice(&b[..8]);
    t.copy_from_slice(&b[8..]);
    Ok((u64::from_le_bytes(off), i64::from_le_bytes(t)))
}

impl DecodedCaptures {
    /// Opens (creating) the store in `dir`. Captures left recording by a previous process are
    /// closed as `interrupted`, their counts taken from the files; the quota is then enforced.
    pub fn open(dir: impl Into<PathBuf>, quota: CaptureQuota) -> io::Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        let inner = Inner {
            dir,
            quota: Mutex::new(quota),
            factory: Mutex::new(None),
            entries: Mutex::new(BTreeMap::new()),
            seq: AtomicU64::new(1),
            over_quota: AtomicBool::new(false),
        };
        let mut entries = BTreeMap::new();
        for dent in fs::read_dir(&inner.dir)?.flatten() {
            let path = dent.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Some(meta) = fs::read(&path)
                .ok()
                .and_then(|b| serde_json::from_slice::<MetaFile>(&b).ok())
                .filter(|m| m.schema == META_SCHEMA && is_capture_id(&m.info.id))
            else {
                continue;
            };
            let mut e = Entry {
                info: meta.info,
                header_len: meta.header_len,
            };
            if e.info.recording {
                if inner.recover(&mut e) == 0 {
                    inner.remove_files(&e.info.id);
                    continue;
                }
                e.info.recording = false;
                e.info.ended = Some(e.info.ended.unwrap_or(now_s()));
                e.info.end_reason = Some("interrupted".into());
                inner.write_meta(&e);
            }
            entries.insert(e.info.id.clone(), e);
        }
        *lock(&inner.entries) = entries;
        let this = Self(Arc::new(inner));
        this.0.enforce_quota();
        Ok(this)
    }

    /// The store directory.
    pub fn dir(&self) -> &Path {
        &self.0.dir
    }

    /// The quota in force.
    pub fn quota(&self) -> CaptureQuota {
        *lock(&self.0.quota)
    }

    /// Replaces the quota (new segments and the next eviction pass use it) and enforces it.
    pub fn set_quota(&self, quota: CaptureQuota) {
        *lock(&self.0.quota) = quota;
        self.0.enforce_quota();
    }

    /// Replaces how later captures open their stream files (see [`DataWriterFactory`]).
    pub fn set_data_writer(&self, factory: DataWriterFactory) {
        *lock(&self.0.factory) = Some(factory);
    }

    /// Bytes stored across every capture (stream + index).
    pub fn used_bytes(&self) -> u64 {
        lock(&self.0.entries)
            .values()
            .map(Entry::stored_bytes)
            .sum()
    }

    /// Records `header`'s stream through `handle` (see the module docs). Only inspector streams
    /// (`message_schema: hackriff.inspector/1` messages) are recorded: `Ok(None)` otherwise.
    pub fn tee(
        &self,
        header: &StreamHeader,
        handle: &PublisherHandle,
    ) -> Result<Option<ConsumerId>, StreamError> {
        if header.kind != StreamKind::Messages
            || header.message_schema.as_deref() != Some(INSPECTOR_MESSAGE_SCHEMA)
        {
            return Ok(None);
        }
        let reason = Arc::new(Mutex::new(None));
        let writer = CaptureWriter {
            store: Arc::clone(&self.0),
            dec: FrameDecoder::new(HEADER_MAX_LEN),
            header: None,
            header_frame: Vec::new(),
            seg: None,
            next_segment: 0,
            reason: Arc::clone(&reason),
            failed: false,
        };
        let queue = self.quota().queue_bytes.max(MIN_QUEUE_BYTES);
        handle
            .subscribe_recorder(
                "decoded-capture",
                Declared::local(writer),
                Box::new(move |r| *lock(&reason) = Some(r)),
                queue,
            )
            .map(Some)
    }
}

struct Segment {
    id: String,
    data: Box<dyn Write + Send>,
    idx: File,
    data_buf: Vec<u8>,
    idx_buf: Vec<u8>,
    /// Stream bytes written or buffered: the next record's offset.
    bytes: u64,
    frames: u64,
    t_first: Option<i64>,
    t_last: Option<i64>,
    dropped: u64,
    meta_at: Instant,
}

/// The per-stream consumer writer: runs on the publisher's writer thread for this consumer.
struct CaptureWriter {
    store: Arc<Inner>,
    dec: FrameDecoder,
    header: Option<StreamHeader>,
    header_frame: Vec<u8>,
    seg: Option<Segment>,
    next_segment: u32,
    reason: Arc<Mutex<Option<CloseReason>>>,
    failed: bool,
}

/// Writes `buf` to `w`, removing what was written: after an error (e.g. a partial write on a full
/// disk) `buf` holds exactly the unwritten remainder, so a retry never writes a byte twice.
fn write_pending<W: Write + ?Sized>(w: &mut W, buf: &mut Vec<u8>) -> io::Result<()> {
    let mut done = 0;
    let result = loop {
        if done == buf.len() {
            break Ok(());
        }
        match w.write(&buf[done..]) {
            Ok(0) => break Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => done += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => break Err(e),
        }
    };
    buf.drain(..done);
    result
}

/// Shortens the file at `path` to `len` bytes if it is longer (best effort).
fn truncate(path: &Path, len: u64) {
    if fs::metadata(path).is_ok_and(|m| m.len() > len) {
        let _ = OpenOptions::new()
            .write(true)
            .open(path)
            .and_then(|f| f.set_len(len));
    }
}

/// Walks `.hks` records from `start` (just past the header) while each is complete and every
/// frame record among them is the next `index` entry: `(frames, end offset, dropped-marker
/// counts)`. A torn record, or a frame record whose index entry never landed, ends the walk.
fn walk_records(
    r: &mut BufReader<File>,
    start: u64,
    data_len: u64,
    index: &[(u64, i64)],
) -> (u64, u64, u64) {
    let (mut frames, mut pos, mut dropped) = (0u64, start, 0u64);
    if r.seek(SeekFrom::Start(start)).is_err() {
        return (0, start, 0);
    }
    let mut prefix = [0u8; LEN_PREFIX as usize];
    while r.read_exact(&mut prefix).is_ok() {
        let len = u64::from(u32::from_le_bytes(prefix));
        let next = pos + LEN_PREFIX + len;
        if len > u64::from(MAX_FRAME_LEN) || next > data_len {
            break;
        }
        let indexed = usize::try_from(frames)
            .ok()
            .and_then(|k| index.get(k))
            .is_some_and(|&(off, _)| off == pos);
        if indexed {
            let Ok(skip) = i64::try_from(len) else { break };
            if r.seek_relative(skip).is_err() {
                break;
            }
            frames += 1;
        } else {
            let mut payload = vec![0u8; len as usize];
            if r.read_exact(&mut payload).is_err() {
                break;
            }
            let v: Option<Value> = serde_json::from_slice(&payload).ok();
            match v
                .as_ref()
                .and_then(|v| v.get("type"))
                .and_then(Value::as_str)
            {
                Some("frame") => break,
                Some("dropped") => {
                    dropped += v
                        .as_ref()
                        .and_then(|v| v.get("count"))
                        .and_then(Value::as_u64)
                        .unwrap_or(1);
                }
                _ => {}
            }
        }
        pos = next;
    }
    (frames, pos, dropped)
}

fn invalid(e: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

impl CaptureWriter {
    fn on_payload(&mut self, payload: &[u8]) -> io::Result<()> {
        let Some(header) = &self.header else {
            let h = StreamHeader::from_json_bytes(payload).map_err(invalid)?;
            encode_frame(&mut self.header_frame, payload, HEADER_MAX_LEN).map_err(invalid)?;
            self.dec.set_max_frame_len(h.max_frame_len);
            self.header = Some(h);
            return self.open_segment();
        };
        let max = header.max_frame_len;
        let mut value: Option<Value> = serde_json::from_slice(payload).ok();
        let kind = value
            .as_ref()
            .and_then(|v| v.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let mut rec = Vec::with_capacity(payload.len() + 4);
        match (kind.as_str(), value.as_mut()) {
            ("frame", Some(v)) => {
                let t = v.get("t").and_then(Value::as_i64).unwrap_or(0);
                let stripped = v
                    .get_mut("content")
                    .and_then(Value::as_object_mut)
                    .and_then(|c| c.remove("layers"))
                    .is_some();
                let reencoded =
                    stripped
                        .then(|| serde_json::to_vec(v).ok())
                        .flatten()
                        .map(|mut p| {
                            if payload.ends_with(b"\n") {
                                p.push(b'\n');
                            }
                            p
                        });
                let body = reencoded.as_deref().unwrap_or(payload);
                if encode_frame(&mut rec, body, max).is_err() {
                    rec.clear();
                    encode_frame(&mut rec, payload, max).map_err(invalid)?;
                }
                self.store_frame(&rec, t)
            }
            (kind, v) => {
                if kind == "dropped"
                    && let Some(seg) = self.seg.as_mut()
                {
                    seg.dropped += v
                        .and_then(|v| v.get("count"))
                        .and_then(Value::as_u64)
                        .unwrap_or(1);
                }
                encode_frame(&mut rec, payload, max).map_err(invalid)?;
                if let Some(seg) = self.seg.as_mut() {
                    seg.data_buf.extend_from_slice(&rec);
                    seg.bytes += rec.len() as u64;
                }
                Ok(())
            }
        }
    }

    fn store_frame(&mut self, rec: &[u8], t: i64) -> io::Result<()> {
        let limit = lock(&self.store.quota).segment_bytes();
        let over = self.store.over_quota.load(Ordering::Relaxed);
        let full = self
            .seg
            .as_ref()
            .is_some_and(|s| s.frames > 0 && (over || s.bytes + rec.len() as u64 > limit));
        if full {
            self.roll()?;
        }
        let over = self.store.over_quota.load(Ordering::Relaxed);
        let Some(seg) = self.seg.as_mut() else {
            return Ok(());
        };
        if over {
            seg.dropped += 1;
            return Ok(());
        }
        seg.idx_buf.extend_from_slice(&seg.bytes.to_le_bytes());
        seg.idx_buf.extend_from_slice(&t.to_le_bytes());
        seg.data_buf.extend_from_slice(rec);
        seg.bytes += rec.len() as u64;
        seg.frames += 1;
        seg.t_first.get_or_insert(t);
        seg.t_last = Some(t);
        Ok(())
    }

    fn open_segment(&mut self) -> io::Result<()> {
        let Some(h) = &self.header else {
            return Ok(());
        };
        let p = h.inspector.as_ref();
        let pipeline = p.map_or("", |p| p.pipeline_id.as_str());
        let output = p.map_or("", |p| p.output_id.as_str());
        let id = self.store.new_id(pipeline, output);
        let factory = lock(&self.store.factory).clone();
        let data_path = self.store.path(&id, "hks");
        let data = match factory {
            Some(f) => f(&data_path)?,
            None => Box::new(File::create(&data_path)?),
        };
        let idx = File::create(self.store.path(&id, "idx"))?;
        let entry = Entry {
            info: CaptureInfo {
                id: id.clone(),
                pipeline_id: pipeline.to_owned(),
                recipe_id: p.map_or(String::new(), |p| p.recipe_id.clone()),
                recipe_version: p.map_or(0, |p| p.recipe_version),
                output_id: output.to_owned(),
                stream_id: h.stream_id.clone(),
                content_class: h.content_class,
                segment: self.next_segment,
                started: now_s(),
                ended: None,
                t_first: None,
                t_last: None,
                frames: 0,
                bytes: 0,
                dropped_records: 0,
                recording: true,
                end_reason: None,
            },
            header_len: self.header_frame.len() as u64,
        };
        self.store.write_meta(&entry);
        lock(&self.store.entries).insert(id.clone(), entry);
        self.next_segment += 1;
        self.seg = Some(Segment {
            id,
            data,
            idx,
            data_buf: self.header_frame.clone(),
            idx_buf: Vec::new(),
            bytes: self.header_frame.len() as u64,
            frames: 0,
            t_first: None,
            t_last: None,
            dropped: 0,
            meta_at: Instant::now(),
        });
        self.commit()
    }

    /// Writes what is buffered (stream first, then index, then the catalogue entry, so a reader
    /// only ever sees complete records) and enforces the quota.
    fn commit(&mut self) -> io::Result<()> {
        let Some(seg) = self.seg.as_mut() else {
            return Ok(());
        };
        if !seg.data_buf.is_empty() {
            write_pending(&mut seg.data, &mut seg.data_buf)?;
            seg.data.flush()?;
        }
        if !seg.idx_buf.is_empty() {
            write_pending(&mut seg.idx, &mut seg.idx_buf)?;
        }
        let meta_due = seg.meta_at.elapsed() >= META_EVERY;
        {
            let mut entries = lock(&self.store.entries);
            if let Some(e) = entries.get_mut(&seg.id) {
                e.info.frames = seg.frames;
                e.info.bytes = seg.bytes;
                e.info.t_first = seg.t_first.map(ns_to_s);
                e.info.t_last = seg.t_last.map(ns_to_s);
                e.info.dropped_records = seg.dropped;
                if meta_due {
                    self.store.write_meta(e);
                    seg.meta_at = Instant::now();
                }
            }
        }
        self.store.enforce_quota();
        Ok(())
    }

    /// Ends the current segment with `reason` (a capture without frame records is deleted).
    fn finish_segment(&mut self, reason: &str) {
        let committed = self.commit().is_ok();
        if !committed {
            self.failed = true;
        }
        let Some(seg) = self.seg.take() else {
            return;
        };
        let id = seg.id.clone();
        drop(seg);
        {
            let mut entries = lock(&self.store.entries);
            let empty = entries.get(&id).is_some_and(|e| e.info.frames == 0);
            if empty {
                entries.remove(&id);
                self.store.remove_files(&id);
            } else if let Some(e) = entries.get_mut(&id) {
                if !committed {
                    // A failed write may have left part of a record or index entry: end both
                    // files at the last commit, whose counts the entry holds.
                    truncate(&self.store.path(&id, "hks"), e.info.bytes);
                    truncate(
                        &self.store.path(&id, "idx"),
                        e.info.frames * INDEX_ENTRY_LEN,
                    );
                }
                e.info.recording = false;
                e.info.ended = Some(now_s());
                e.info.end_reason = Some(reason.to_owned());
                self.store.write_meta(e);
            }
        }
        self.store.enforce_quota();
    }

    /// Opens the next segment, then ends the current one, so a recording stream always lists a
    /// recording capture.
    fn roll(&mut self) -> io::Result<()> {
        self.commit()?;
        let old = self.seg.take();
        let opened = self.open_segment();
        let new = self.seg.take();
        self.seg = old;
        self.finish_segment("rolled");
        self.seg = new;
        opened
    }
}

impl Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.dec.push(buf);
        let result = (|| {
            loop {
                let payload = match self.dec.next_frame() {
                    Ok(Some(f)) => f.to_vec(),
                    Ok(None) => break,
                    Err(e) => return Err(invalid(e)),
                };
                self.on_payload(&payload)?;
            }
            self.commit()
        })();
        if result.is_err() {
            self.failed = true;
        }
        result.map(|()| buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.commit()
    }
}

impl Drop for CaptureWriter {
    fn drop(&mut self) {
        let reason = match *lock(&self.reason) {
            _ if self.failed => "write-failed",
            Some(CloseReason::SlowConsumer) => "slow-consumer",
            Some(CloseReason::PeerGone) => "write-failed",
            Some(CloseReason::DrainTimeout) => "drain-timeout",
            Some(CloseReason::Detached) => "detached",
            Some(CloseReason::PublisherFinished) | None => "finished",
        };
        self.finish_segment(reason);
    }
}

impl CaptureSource for DecodedCaptures {
    fn open(&self, id: &str) -> io::Result<Option<Box<dyn Read + Send>>> {
        if !is_capture_id(id) {
            return Ok(None);
        }
        let Some(bytes) = lock(&self.0.entries).get(id).map(|e| e.info.bytes) else {
            return Ok(None);
        };
        let f = File::open(self.0.path(id, "hks"))?;
        Ok(Some(Box::new(f.take(bytes))))
    }

    fn open_at(&self, id: &str, from_frame: u64) -> io::Result<Option<CaptureCursor>> {
        if !is_capture_id(id) {
            return Ok(None);
        }
        let Some((bytes, frames, header_len)) = lock(&self.0.entries)
            .get(id)
            .map(|e| (e.info.bytes, e.info.frames, e.header_len))
        else {
            return Ok(None);
        };
        let mut data = File::open(self.0.path(id, "hks"))?;
        let first = from_frame.min(frames);
        if first == 0 {
            return Ok(Some(CaptureCursor {
                reader: Box::new(data.take(bytes)),
                first_frame: 0,
                total_frames: Some(frames),
            }));
        }
        let offset = if first == frames {
            bytes
        } else {
            self.0.index_entry(id, first)?.0.min(bytes)
        };
        let mut header = vec![0u8; header_len.min(bytes) as usize];
        data.read_exact(&mut header)?;
        data.seek(SeekFrom::Start(offset))?;
        Ok(Some(CaptureCursor {
            reader: Box::new(Cursor::new(header).chain(data.take(bytes - offset))),
            first_frame: first,
            total_frames: Some(frames),
        }))
    }

    fn list(&self) -> io::Result<Vec<CaptureInfo>> {
        let mut v: Vec<CaptureInfo> = lock(&self.0.entries)
            .values()
            .map(|e| e.info.clone())
            .collect();
        v.sort_by(|a, b| b.id.cmp(&a.id));
        Ok(v)
    }

    fn info(&self, id: &str) -> io::Result<Option<CaptureInfo>> {
        Ok(lock(&self.0.entries).get(id).map(|e| e.info.clone()))
    }

    fn frame_at_time(&self, id: &str, t_unix_nanos: i64) -> io::Result<Option<u64>> {
        let Some(frames) = lock(&self.0.entries).get(id).map(|e| e.info.frames) else {
            return Ok(None);
        };
        if frames == 0 {
            return Ok(Some(0));
        }
        let mut idx = File::open(self.0.path(id, "idx"))?;
        let (mut lo, mut hi) = (0u64, frames);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if read_index_entry(&mut idx, mid)?.1 < t_unix_nanos {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        Ok(Some(lo))
    }

    fn delete(&self, id: &str) -> io::Result<CaptureDelete> {
        let mut entries = lock(&self.0.entries);
        match entries.get(id) {
            None => Ok(CaptureDelete::NotFound),
            Some(e) if e.info.recording => Ok(CaptureDelete::Recording),
            Some(_) => {
                entries.remove(id);
                self.0.remove_files(id);
                drop(entries);
                self.0.enforce_quota();
                Ok(CaptureDelete::Deleted)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hk_model::ContentClass;
    use hk_stream::inspector::{
        FrameContent, FrameMetadata, FrameRecord, InspectorProfile, InspectorSource, LayerTree,
        RecordedFrames,
    };
    use hk_stream::{Publisher, PublisherConfig};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "hk-decoded-{tag}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn publisher(class: ContentClass) -> Publisher {
        let mut h = StreamHeader::new("inspector/p1/frames", StreamKind::Messages, class, "test");
        h.message_schema = Some(INSPECTOR_MESSAGE_SCHEMA.into());
        h.inspector = Some(InspectorProfile {
            pipeline_id: "p1".into(),
            recipe_id: "tone".into(),
            recipe_version: 3,
            output_id: "frames".into(),
            source: InspectorSource::Live,
            channels: Vec::new(),
        });
        Publisher::new(h, PublisherConfig::default()).unwrap()
    }

    fn frame(i: u64) -> FrameRecord {
        FrameRecord {
            record_type: "frame".into(),
            seq: i,
            t: 1_000_000_000 * (100 + i as i64),
            content_class: ContentClass::Unrestricted,
            gated: false,
            crc_status: None,
            decoder: Some("recipe:tone@3".into()),
            frame_model: None,
            emitter_id: None,
            metadata: FrameMetadata {
                frame: Some(i),
                bit_len: Some(16),
                ..FrameMetadata::default()
            },
            content: Some(FrameContent {
                hex: format!("01{:02x}", i & 0xff),
                layers: Some(LayerTree::default()),
            }),
        }
    }

    fn wait_ended(store: &DecodedCaptures) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let list = store.list().unwrap();
            if !list.is_empty() && !list.iter().any(|c| c.recording) {
                break;
            }
            assert!(Instant::now() < deadline, "captures did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn records_frames_without_layers_indexes_and_seeks() {
        let dir = TempDir::new("seek");
        let store = DecodedCaptures::open(&dir.0, CaptureQuota::default()).unwrap();
        let mut p = publisher(ContentClass::Unrestricted);
        assert!(store.tee(p.header(), &p.handle()).unwrap().is_some());
        for i in 0..50 {
            p.publish_frame(&frame(i)).unwrap();
        }
        drop(p);
        wait_ended(&store);
        let list = store.list().unwrap();
        assert_eq!(list.len(), 1);
        let c = &list[0];
        assert_eq!(
            (c.frames, c.recipe_id.as_str(), c.recipe_version, c.segment),
            (50, "tone", 3, 0)
        );
        assert_eq!(c.end_reason.as_deref(), Some("finished"));
        assert_eq!((c.t_first, c.t_last), (Some(100.0), Some(149.0)));

        // Full read: every frame, layers stripped.
        let mut r = RecordedFrames::open(store.open(&c.id).unwrap().unwrap()).unwrap();
        let mut n = 0;
        while let Some(f) = r.next_frame().unwrap() {
            assert_eq!(f.metadata.frame, Some(n));
            assert!(f.content.unwrap().layers.is_none());
            n += 1;
        }
        assert_eq!(n, 50);

        // Seek: frame 37 first.
        let cur = store.open_at(&c.id, 37).unwrap().unwrap();
        assert_eq!((cur.first_frame, cur.total_frames), (37, Some(50)));
        let mut r = RecordedFrames::open(cur.reader).unwrap();
        assert_eq!(r.header().stream_id, "inspector/p1/frames");
        assert_eq!(r.next_frame().unwrap().unwrap().metadata.frame, Some(37));
        let past = store.open_at(&c.id, 99).unwrap().unwrap();
        assert_eq!(past.first_frame, 50);
        assert!(
            RecordedFrames::open(past.reader)
                .unwrap()
                .next_frame()
                .unwrap()
                .is_none()
        );

        // Time scrub.
        let at = |s: i64| store.frame_at_time(&c.id, s * 1_000_000_000).unwrap();
        assert_eq!(at(0), Some(0));
        assert_eq!(at(120), Some(20));
        assert_eq!(at(1000), Some(50));
        assert_eq!(store.frame_at_time("nope", 0).unwrap(), None);

        // Reopen: the catalogue survives.
        let again = DecodedCaptures::open(&dir.0, CaptureQuota::default()).unwrap();
        assert_eq!(again.info(&c.id).unwrap().unwrap().frames, 50);
        assert_eq!(again.delete(&c.id).unwrap(), CaptureDelete::Deleted);
        assert_eq!(again.delete(&c.id).unwrap(), CaptureDelete::NotFound);
        assert!(again.open("../etc").unwrap().is_none());
    }

    #[test]
    fn rolls_segments_and_evicts_oldest_first_within_quota() {
        let dir = TempDir::new("quota");
        let quota = CaptureQuota {
            total_bytes: 40 * 1024,
            capture_bytes: 8 * 1024,
            queue_bytes: 0,
        };
        let store = DecodedCaptures::open(&dir.0, quota).unwrap();
        let mut p = publisher(ContentClass::Unrestricted);
        store.tee(p.header(), &p.handle()).unwrap();
        for i in 0..2000 {
            p.publish_frame(&frame(i)).unwrap();
            if i % 50 == 0 {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        drop(p);
        wait_ended(&store);
        let list = store.list().unwrap();
        assert!(
            store.used_bytes() <= quota.total_bytes,
            "{}",
            store.used_bytes()
        );
        assert!(list.len() >= 2, "{list:?}");
        // Oldest evicted: the newest segment survives, the first segments are gone.
        let segs: Vec<u32> = list.iter().map(|c| c.segment).collect();
        assert!(!segs.contains(&0), "{segs:?}");
        let newest = &list[0];
        assert_eq!(newest.segment, *segs.iter().max().unwrap());
        for c in &list {
            assert!(c.bytes <= 8 * 1024 + 512, "{c:?}");
        }
        // Segments together hold the most recent frames, contiguous.
        let last = RecordedFrames::open(store.open(&newest.id).unwrap().unwrap())
            .unwrap()
            .last()
            .unwrap()
            .unwrap();
        assert_eq!(last.metadata.frame, Some(1999));
        assert!(fs::read_dir(&dir.0).unwrap().count() <= list.len() * 3);
    }

    /// A stream file that takes 20 ms per write.
    struct SlowFile(File);

    impl Write for SlowFile {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            std::thread::sleep(Duration::from_millis(20));
            self.0.write(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.0.flush()
        }
    }

    #[test]
    fn a_slow_disk_drops_and_counts_without_blocking_the_publisher() {
        let dir = TempDir::new("slow");
        let store = DecodedCaptures::open(&dir.0, CaptureQuota::default()).unwrap();
        store.set_data_writer(Arc::new(|p: &Path| {
            Ok(Box::new(SlowFile(File::create(p)?)) as Box<dyn Write + Send>)
        }));
        let mut p = publisher(ContentClass::Unrestricted);
        store.tee(p.header(), &p.handle()).unwrap();
        let big = "ab".repeat(100_000);
        let started = Instant::now();
        let mut slowest = Duration::ZERO;
        let n = 400u64;
        for i in 0..n {
            let mut f = frame(i);
            f.content.as_mut().unwrap().hex = big.clone();
            let t = Instant::now();
            p.publish_frame(&f).unwrap();
            slowest = slowest.max(t.elapsed());
        }
        let publishing = started.elapsed();
        // A publisher waiting on the 20 ms/write disk would take >= 8 s for 400 frames.
        assert!(slowest < Duration::from_millis(500), "{slowest:?}");
        assert!(publishing < Duration::from_secs(4), "{publishing:?}");
        drop(p);
        wait_ended(&store);
        let c = &store.list().unwrap()[0];
        assert!(c.frames < n, "{c:?}");
        assert_eq!(c.frames + c.dropped_records, n, "{c:?}");
        let r = RecordedFrames::open(store.open(&c.id).unwrap().unwrap()).unwrap();
        assert_eq!(r.count() as u64, c.frames);
    }

    #[test]
    fn interrupted_captures_are_closed_on_reopen() {
        let dir = TempDir::new("crash");
        let store = DecodedCaptures::open(&dir.0, CaptureQuota::default()).unwrap();
        let mut p = publisher(ContentClass::Unrestricted);
        store.tee(p.header(), &p.handle()).unwrap();
        for i in 0..10 {
            p.publish_frame(&frame(i)).unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while store.list().unwrap().first().is_none_or(|c| c.frames < 10) {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        // Another process opens the directory while this one still records.
        let other = DecodedCaptures::open(&dir.0, CaptureQuota::default()).unwrap();
        let c = &other.list().unwrap()[0];
        assert!(!c.recording);
        assert_eq!(c.end_reason.as_deref(), Some("interrupted"));
        assert_eq!(c.frames, 10);
        drop(p);
        wait_ended(&store);
    }

    fn publisher_with(class: ContentClass, config: PublisherConfig) -> Publisher {
        Publisher::new(publisher(class).header().clone(), config).unwrap()
    }

    fn wait_frames(store: &DecodedCaptures, n: u64) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while store.list().unwrap().first().is_none_or(|c| c.frames < n) {
            assert!(Instant::now() < deadline, "{n} frames not recorded");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Reads every stored frame and checks frames are `0..frames` in order and that each index
    /// entry seeks to its frame.
    fn assert_fully_readable(store: &DecodedCaptures, c: &CaptureInfo, what: &str) {
        let mut r = RecordedFrames::open(store.open(&c.id).unwrap().unwrap()).unwrap();
        let mut n = 0;
        while let Some(f) = r.next_frame().unwrap() {
            assert_eq!(f.metadata.frame, Some(n), "{what}");
            n += 1;
        }
        assert_eq!(n, c.frames, "{what}");
        for k in 0..c.frames {
            let cur = store.open_at(&c.id, k).unwrap().unwrap();
            let mut r = RecordedFrames::open(cur.reader).unwrap();
            assert_eq!(
                r.next_frame().unwrap().unwrap().metadata.frame,
                Some(k),
                "{what}"
            );
        }
        let len = |ext: &str| {
            fs::metadata(store.dir().join(format!("{}.{ext}", c.id)))
                .unwrap()
                .len()
        };
        assert_eq!(len("hks"), c.bytes, "{what}");
        assert_eq!(len("idx"), c.frames * INDEX_ENTRY_LEN, "{what}");
    }

    /// A stream file whose writes block while the flag is set.
    struct StalledFile(File, Arc<AtomicBool>);

    impl Write for StalledFile {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            while self.1.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(2));
            }
            self.0.write(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.0.flush()
        }
    }

    #[test]
    fn a_disk_stall_longer_than_the_disconnect_policy_keeps_recording_and_counts_drops() {
        let dir = TempDir::new("stall");
        let store = DecodedCaptures::open(&dir.0, CaptureQuota::default()).unwrap();
        let stalled = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stalled);
        store.set_data_writer(Arc::new(move |p: &Path| {
            Ok(Box::new(StalledFile(File::create(p)?, Arc::clone(&flag))) as Box<dyn Write + Send>)
        }));
        let config = PublisherConfig {
            disconnect_after: Duration::from_millis(100),
            ..PublisherConfig::default()
        };
        let mut p = publisher_with(ContentClass::Unrestricted, config);
        store.tee(p.header(), &p.handle()).unwrap();
        let mut i = 0u64;
        for _ in 0..5 {
            p.publish_frame(&frame(i)).unwrap();
            i += 1;
        }
        wait_frames(&store, 5);
        // The disk hangs for 6× the disconnect policy while large frames fill the queue.
        stalled.store(true, Ordering::SeqCst);
        let big = "ab".repeat(100_000);
        let until = Instant::now() + Duration::from_millis(600);
        while Instant::now() < until {
            let mut f = frame(i);
            f.content.as_mut().unwrap().hex = big.clone();
            p.publish_frame(&f).unwrap();
            i += 1;
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            p.handle().open_consumers(),
            1,
            "the recorder was disconnected"
        );
        stalled.store(false, Ordering::SeqCst);
        // Once the recorder has drained what it queued, later frames are accepted, and the first
        // one carries the drop marker.
        let deadline = Instant::now() + Duration::from_secs(10);
        while p
            .handle()
            .consumer_stats()
            .iter()
            .any(|s| s.queued_bytes > 0)
        {
            assert!(Instant::now() < deadline, "the recorder did not drain");
            std::thread::sleep(Duration::from_millis(2));
        }
        for _ in 0..20 {
            p.publish_frame(&frame(i)).unwrap();
            i += 1;
            std::thread::sleep(Duration::from_millis(1));
        }
        drop(p);
        wait_ended(&store);
        let list = store.list().unwrap();
        assert_eq!(list.len(), 1, "{list:?}");
        let c = &list[0];
        assert_eq!(c.end_reason.as_deref(), Some("finished"), "{c:?}");
        assert!(c.dropped_records > 0, "{c:?}");
        assert_eq!(c.frames + c.dropped_records, i, "{c:?}");
        // Frames after the stall were recorded: the last one published is the last stored.
        let mut r = RecordedFrames::open(store.open(&c.id).unwrap().unwrap()).unwrap();
        let mut last = None;
        while let Some(f) = r.next_frame().unwrap() {
            last = f.metadata.frame;
        }
        assert_eq!(last, Some(i - 1));
    }

    #[test]
    fn a_torn_tail_record_or_index_entry_recovers_to_a_fully_readable_capture() {
        for case in ["torn-record", "torn-index"] {
            let dir = TempDir::new(case);
            let store = DecodedCaptures::open(&dir.0, CaptureQuota::default()).unwrap();
            let mut p = publisher(ContentClass::Unrestricted);
            store.tee(p.header(), &p.handle()).unwrap();
            for i in 0..20 {
                p.publish_frame(&frame(i)).unwrap();
            }
            drop(p);
            wait_ended(&store);
            let id = store.list().unwrap()[0].id.clone();
            drop(store);
            let path = |ext: &str| dir.0.join(format!("{id}.{ext}"));
            // A power loss mid-write: the catalogue still says recording, with stale counts.
            let mut meta: Value = serde_json::from_slice(&fs::read(path("json")).unwrap()).unwrap();
            meta["recording"] = Value::Bool(true);
            meta["ended"] = Value::Null;
            meta["end_reason"] = Value::Null;
            fs::write(path("json"), serde_json::to_vec(&meta).unwrap()).unwrap();
            let idx = fs::read(path("idx")).unwrap();
            let data_len = fs::metadata(path("hks")).unwrap().len();
            let expect = if case == "torn-record" {
                // Half of the last frame record reached the disk, and its index entry did.
                let last =
                    u64::from_le_bytes(idx[idx.len() - 16..idx.len() - 8].try_into().unwrap());
                File::options()
                    .write(true)
                    .open(path("hks"))
                    .unwrap()
                    .set_len(last + (data_len - last) / 2)
                    .unwrap();
                19
            } else {
                // A torn length prefix, one whole index entry for it and a partial one.
                let mut hks = File::options().append(true).open(path("hks")).unwrap();
                hks.write_all(&[0x40, 0, 0]).unwrap();
                let mut ix = File::options().append(true).open(path("idx")).unwrap();
                ix.write_all(&data_len.to_le_bytes()).unwrap();
                ix.write_all(&[7u8; 8]).unwrap();
                ix.write_all(&[1, 2, 3, 4, 5]).unwrap();
                20
            };
            let store = DecodedCaptures::open(&dir.0, CaptureQuota::default()).unwrap();
            let c = store.info(&id).unwrap().unwrap();
            assert!(!c.recording, "{case}");
            assert_eq!(c.end_reason.as_deref(), Some("interrupted"), "{case}");
            assert_eq!(c.frames, expect, "{case}");
            assert_eq!(c.t_first, Some(ns_to_s(frame(0).t)), "{case}");
            assert_eq!(c.t_last, Some(ns_to_s(frame(expect - 1).t)), "{case}");
            assert_fully_readable(&store, &c, case);
        }
    }

    /// A stream file on a disk that fills at `budget` bytes: the write reaching it lands partly
    /// and later writes fail, until (with `transient`) the disk has room again.
    struct FullDisk {
        file: File,
        budget: Arc<AtomicU64>,
        written: u64,
        failed: bool,
        transient: bool,
    }

    impl Write for FullDisk {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let n = if self.failed && self.transient {
                buf.len()
            } else {
                let room = self
                    .budget
                    .load(Ordering::SeqCst)
                    .saturating_sub(self.written);
                if room == 0 {
                    self.failed = true;
                    return Err(io::Error::other("disk full"));
                }
                buf.len().min(usize::try_from(room).unwrap_or(usize::MAX))
            };
            let n = self.file.write(&buf[..n])?;
            self.written += n as u64;
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.file.flush()
        }
    }

    #[test]
    fn a_partial_write_never_duplicates_bytes_and_ends_at_a_complete_record() {
        for transient in [true, false] {
            let what = if transient { "transient" } else { "full" };
            let dir = TempDir::new(what);
            let store = DecodedCaptures::open(&dir.0, CaptureQuota::default()).unwrap();
            let budget = Arc::new(AtomicU64::new(u64::MAX));
            let b = Arc::clone(&budget);
            store.set_data_writer(Arc::new(move |p: &Path| {
                Ok(Box::new(FullDisk {
                    file: File::create(p)?,
                    budget: Arc::clone(&b),
                    written: 0,
                    failed: false,
                    transient,
                }) as Box<dyn Write + Send>)
            }));
            let mut p = publisher(ContentClass::Unrestricted);
            store.tee(p.header(), &p.handle()).unwrap();
            for i in 0..5 {
                p.publish_frame(&frame(i)).unwrap();
            }
            wait_frames(&store, 5);
            // The disk fills part-way through a later record.
            let committed = store.list().unwrap()[0].bytes;
            budget.store(committed + 1001, Ordering::SeqCst);
            for i in 5..100 {
                p.publish_frame(&frame(i)).unwrap();
            }
            drop(p);
            wait_ended(&store);
            let c = store.list().unwrap()[0].clone();
            assert_eq!(
                c.end_reason.as_deref(),
                Some("write-failed"),
                "{what}: {c:?}"
            );
            if transient {
                // The retry wrote the rest of the failed commit, once.
                assert!(c.frames > 5, "{what}: {c:?}");
            } else {
                assert!(c.frames >= 5, "{what}: {c:?}");
            }
            assert_fully_readable(&store, &c, what);
        }
    }
}
