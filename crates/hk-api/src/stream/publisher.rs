//! Drop-not-block fan-out (ADR-0004 backpressure; docs/stream-contract.md §7).
//!
//! # Shape
//! - One [`Publisher`] per stream, owned by the producer thread (`&mut self` publish calls).
//! - One bounded **byte ring** per consumer, allocated once at subscribe time, holding whole
//!   frames. A frame that does not fit is **dropped for that consumer only**; the producer never
//!   waits for space. Drops are counted per consumer, and the next frame that fits is preceded
//!   by a "dropped N" marker frame naming the missing seq range.
//! - One writer thread per consumer blocks on a condvar while its ring is empty (no polling),
//!   copies at most 16 KiB out under the lock, and writes it to the socket with the lock
//!   released. A slow or stuck consumer therefore only ever blocks its own writer thread.
//! - A consumer that stays full for [`PublisherConfig::disconnect_after`], or drops
//!   [`PublisherConfig::disconnect_after_drops`] consecutive records, is disconnected: its
//!   transport is shut down (which unblocks its writer) and its ring is freed.
//!
//! # Producer cost
//! Per record: build the frame (binary: a 36-byte prefix+header on the stack, payload borrowed;
//! messages: JSON into a reused scratch buffer), then for each consumer one uncontended lock,
//! a free-space check and one or two `memcpy`s into the ring. That is O(consumers) and, once
//! the consumer snapshot is warm, allocates nothing for binary kinds: rings are preallocated,
//! markers are encoded on the stack, and the consumer list is re-snapshotted (into a reused
//! `Vec`) only when membership changes. The writer holds a consumer's lock for at most one
//! 16 KiB `memcpy`, which bounds how long the producer can wait on that lock (microseconds);
//! no I/O ever happens under it. `tests/alloc_free.rs` asserts the zero-allocation claim.
//!
//! # Ungated feed
//! [`DecoderFeed`] reuses this machinery for the plugin data plane (ADR-0003) without the egress
//! gate, and only attaches to a child process's stdin. See [`super::gate`] for why.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::process::ChildStdin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use hk_model::{ContentClass, Timestamp};

use super::frame::{FrameError, LEN_PREFIX, frame_prefix};
use super::gate;
use super::header::{HeaderError, StreamHeader, StreamKind};
use super::record::{
    BINARY_RECORD_HEADER_LEN, BinaryRecord, BinaryRecordHeader, BinaryRecordType, DropMarker,
    MARKER_MAX_LEN, MessageRecord, MessageWire, RecordFlags, encode_marker,
};

/// How much a writer thread copies out of a ring per write.
const WRITE_CHUNK: usize = 16 * 1024;
/// Disconnected consumers whose stats are kept for inspection.
const FINISHED_KEPT: usize = 64;
/// Writer thread stack size.
const WRITER_STACK: usize = 256 * 1024;

/// Per-consumer queue and disconnect policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PublisherConfig {
    /// Ring size per consumer, bytes. Must hold the header frame plus one maximum-size record
    /// frame plus a marker.
    pub queue_bytes: usize,
    /// Disconnect after this many consecutive drops (`u64::MAX`: never by count).
    pub disconnect_after_drops: u64,
    /// Disconnect after the ring has stayed full (every offer dropped) for this long.
    pub disconnect_after: Duration,
}

impl Default for PublisherConfig {
    fn default() -> Self {
        Self {
            queue_bytes: 8 * 1024 * 1024,
            disconnect_after_drops: u64::MAX,
            disconnect_after: Duration::from_secs(5),
        }
    }
}

/// Publisher errors.
#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    /// Bad header.
    #[error(transparent)]
    Header(#[from] HeaderError),
    /// Record larger than the stream's `max_frame_len`.
    #[error(transparent)]
    Frame(#[from] FrameError),
    /// Message serialisation failed.
    #[error("message JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// The operation does not match the stream kind.
    #[error("{operation} on a {} stream", kind.as_str())]
    WrongKind {
        /// Stream kind.
        kind: StreamKind,
        /// What was attempted.
        operation: &'static str,
    },
    /// ADR-0004: a content-bearing record was offered on a stream whose class forbids content.
    /// The payload was withheld and a metadata-only (header-only, `GATED`) record was published
    /// in its place; `outcome` says how it was delivered.
    #[error("{} payload refused under content class {class:?} (ADR-0004); metadata-only record sent", kind.as_str())]
    ContentGated {
        /// Stream kind.
        kind: StreamKind,
        /// Header class.
        class: ContentClass,
        /// Delivery of the metadata-only record.
        outcome: PublishOutcome,
    },
    /// Bad configuration.
    #[error("invalid publisher config: {0}")]
    Config(String),
    /// The publisher has finished; no new consumers.
    #[error("stream finished")]
    Finished,
    /// Thread spawn or socket error.
    #[error("io: {0}")]
    Io(#[from] io::Error),
}

/// How one publish call was delivered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PublishOutcome {
    /// Open consumers offered the record.
    pub consumers: u32,
    /// Consumers that queued it.
    pub enqueued: u32,
    /// Consumers that dropped it (queue full).
    pub dropped: u32,
    /// Consumers disconnected by this call (slow-consumer policy).
    pub disconnected: u32,
}

/// Consumer id, unique within a publisher.
pub type ConsumerId = u64;

/// Why a consumer was closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseReason {
    /// Stayed full past the policy threshold.
    SlowConsumer,
    /// The peer closed or a write failed.
    PeerGone,
    /// The publisher finished and the ring was drained.
    PublisherFinished,
    /// Detached by the owner.
    Detached,
}

/// Consumer lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConsumerState {
    /// Receiving.
    Open,
    /// Publisher finished; flushing what is queued.
    Draining,
    /// Done.
    Closed(CloseReason),
}

/// Per-consumer counters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConsumerStats {
    /// Id.
    pub id: ConsumerId,
    /// Label (peer description).
    pub label: String,
    /// State.
    pub state: ConsumerState,
    /// Records queued.
    pub records_enqueued: u64,
    /// Records dropped because the queue was full.
    pub records_dropped: u64,
    /// Drop markers queued.
    pub drop_markers: u64,
    /// Bytes queued (header, records, markers).
    pub bytes_enqueued: u64,
    /// Bytes written to the transport.
    pub bytes_written: u64,
    /// Bytes discarded from the queue at close.
    pub bytes_discarded: u64,
    /// Bytes currently queued.
    pub queued_bytes: u64,
}

/// Fixed-capacity byte ring.
struct Ring {
    buf: Box<[u8]>,
    head: usize,
    len: usize,
}

impl Ring {
    fn new(capacity: usize) -> Self {
        Self {
            buf: vec![0u8; capacity].into_boxed_slice(),
            head: 0,
            len: 0,
        }
    }

    fn free(&self) -> usize {
        self.buf.len() - self.len
    }

    /// Appends `data`; the caller has checked `free()`.
    fn push(&mut self, data: &[u8]) {
        let cap = self.buf.len();
        debug_assert!(data.len() <= cap - self.len);
        if data.is_empty() {
            return;
        }
        let tail = (self.head + self.len) % cap;
        let first = data.len().min(cap - tail);
        self.buf[tail..tail + first].copy_from_slice(&data[..first]);
        self.buf[..data.len() - first].copy_from_slice(&data[first..]);
        self.len += data.len();
    }

    fn pop_into(&mut self, out: &mut [u8]) -> usize {
        let n = self.len.min(out.len());
        if n == 0 {
            return 0;
        }
        let cap = self.buf.len();
        let first = n.min(cap - self.head);
        out[..first].copy_from_slice(&self.buf[self.head..self.head + first]);
        out[first..n].copy_from_slice(&self.buf[..n - first]);
        self.head = (self.head + n) % cap;
        self.len -= n;
        if self.len == 0 {
            self.head = 0;
        }
        n
    }

    /// Empties the ring and releases its memory; returns the bytes discarded.
    fn release(&mut self) -> usize {
        let n = self.len;
        *self = Ring {
            buf: Box::new([]),
            head: 0,
            len: 0,
        };
        n
    }
}

struct Counters {
    records_enqueued: u64,
    records_dropped: u64,
    drop_markers: u64,
    bytes_enqueued: u64,
    bytes_written: u64,
    bytes_discarded: u64,
}

struct Inner {
    ring: Ring,
    state: ConsumerState,
    counters: Counters,
    pending_drop: Option<DropMarker>,
    full_since: Option<Instant>,
    consecutive_drops: u64,
}

type Closer = Box<dyn FnOnce(CloseReason) + Send>;

struct Consumer {
    id: ConsumerId,
    label: String,
    inner: Mutex<Inner>,
    cv: Condvar,
    closed: AtomicBool,
    closer: Mutex<Option<Closer>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Consumer {
    /// Closes the consumer (idempotent): frees the ring, wakes the writer, runs the closer.
    fn close(&self, reason: CloseReason) -> bool {
        {
            let mut g = lock(&self.inner);
            if matches!(g.state, ConsumerState::Closed(_)) {
                return false;
            }
            g.state = ConsumerState::Closed(reason);
            g.counters.bytes_discarded += g.ring.release() as u64;
        }
        self.closed.store(true, Ordering::Release);
        self.cv.notify_all();
        if let Some(closer) = lock(&self.closer).take() {
            closer(reason);
        }
        true
    }

    fn stats(&self) -> ConsumerStats {
        let g = lock(&self.inner);
        ConsumerStats {
            id: self.id,
            label: self.label.clone(),
            state: g.state,
            records_enqueued: g.counters.records_enqueued,
            records_dropped: g.counters.records_dropped,
            drop_markers: g.counters.drop_markers,
            bytes_enqueued: g.counters.bytes_enqueued,
            bytes_written: g.counters.bytes_written,
            bytes_discarded: g.counters.bytes_discarded,
            queued_bytes: g.ring.len as u64,
        }
    }
}

fn writer_loop(c: Arc<Consumer>, mut w: Box<dyn Write + Send>) {
    let mut chunk = vec![0u8; WRITE_CHUNK];
    let mut just_written = 0u64;
    loop {
        let n = {
            let mut g = lock(&c.inner);
            g.counters.bytes_written += just_written;
            loop {
                let state = g.state;
                match state {
                    ConsumerState::Closed(_) => return,
                    _ if g.ring.len > 0 => break,
                    ConsumerState::Open => {
                        g = c.cv.wait(g).unwrap_or_else(PoisonError::into_inner);
                    }
                    ConsumerState::Draining => {
                        drop(g);
                        let _ = w.flush();
                        c.close(CloseReason::PublisherFinished);
                        return;
                    }
                }
            }
            g.ring.pop_into(&mut chunk)
        };
        if w.write_all(&chunk[..n]).is_err() {
            c.close(CloseReason::PeerGone);
            return;
        }
        just_written = n as u64;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// External egress: gated, framed, with markers.
    Egress,
    /// Plugin data plane, contract framing with markers, not gated.
    FeedFramed,
    /// Plugin data plane, bare payload bytes, not gated, no header or markers.
    FeedRaw,
}

struct List {
    active: Vec<Arc<Consumer>>,
    finished: VecDeque<Arc<Consumer>>,
    closed_for_new: bool,
}

struct Shared {
    mode: Mode,
    kind: StreamKind,
    header_frame: Vec<u8>,
    config: PublisherConfig,
    min_queue_bytes: usize,
    list: Mutex<List>,
    generation: AtomicU64,
    next_id: AtomicU64,
}

impl Shared {
    fn subscribe(
        self: &Arc<Self>,
        label: String,
        writer: Box<dyn Write + Send>,
        closer: Closer,
        queue_bytes: usize,
    ) -> Result<ConsumerId, StreamError> {
        if queue_bytes < self.min_queue_bytes {
            return Err(StreamError::Config(format!(
                "queue_bytes {queue_bytes} < {} (header + one max_frame_len record + marker)",
                self.min_queue_bytes
            )));
        }
        if lock(&self.list).closed_for_new {
            return Err(StreamError::Finished);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut ring = Ring::new(queue_bytes);
        ring.push(&self.header_frame);
        let consumer = Arc::new(Consumer {
            id,
            label,
            inner: Mutex::new(Inner {
                ring,
                state: ConsumerState::Open,
                counters: Counters {
                    records_enqueued: 0,
                    records_dropped: 0,
                    drop_markers: 0,
                    bytes_enqueued: self.header_frame.len() as u64,
                    bytes_written: 0,
                    bytes_discarded: 0,
                },
                pending_drop: None,
                full_since: None,
                consecutive_drops: 0,
            }),
            cv: Condvar::new(),
            closed: AtomicBool::new(false),
            closer: Mutex::new(Some(closer)),
        });
        let for_thread = Arc::clone(&consumer);
        thread::Builder::new()
            .name(format!("hk-stream-tx-{id}"))
            .stack_size(WRITER_STACK)
            .spawn(move || writer_loop(for_thread, writer))?;
        let mut list = lock(&self.list);
        if list.closed_for_new {
            drop(list);
            consumer.close(CloseReason::PublisherFinished);
            return Err(StreamError::Finished);
        }
        list.active.push(consumer);
        self.generation.fetch_add(1, Ordering::Release);
        Ok(id)
    }

    fn all(&self) -> Vec<Arc<Consumer>> {
        let list = lock(&self.list);
        list.active
            .iter()
            .chain(list.finished.iter())
            .cloned()
            .collect()
    }

    fn find(&self, id: ConsumerId) -> Option<Arc<Consumer>> {
        self.all().into_iter().find(|c| c.id == id)
    }

    /// Moves closed consumers out of the active list.
    fn prune(&self) {
        let mut list = lock(&self.list);
        let before = list.active.len();
        let (closed, open): (Vec<_>, Vec<_>) = list
            .active
            .drain(..)
            .partition(|c| c.closed.load(Ordering::Acquire));
        list.active = open;
        for c in closed {
            if list.finished.len() == FINISHED_KEPT {
                list.finished.pop_front();
            }
            list.finished.push_back(c);
        }
        if list.active.len() != before {
            self.generation.fetch_add(1, Ordering::Release);
        }
    }

    fn finish(&self) {
        let consumers = {
            let mut list = lock(&self.list);
            list.closed_for_new = true;
            list.active.clone()
        };
        for c in consumers {
            let mut g = lock(&c.inner);
            if g.state == ConsumerState::Open {
                g.state = ConsumerState::Draining;
            }
            drop(g);
            c.cv.notify_all();
        }
    }
}

/// A clonable, thread-safe handle for subscribing and inspecting consumers.
#[derive(Clone)]
pub struct PublisherHandle {
    shared: Arc<Shared>,
}

impl PublisherHandle {
    /// Adds a consumer. The header frame is queued first. `closer` must unblock a write in
    /// progress on `writer` (for sockets: `shutdown(Both)` on a clone); it runs once, when the
    /// consumer closes for any reason.
    pub fn subscribe(
        &self,
        label: impl Into<String>,
        writer: Box<dyn Write + Send>,
        closer: Box<dyn FnOnce(CloseReason) + Send>,
    ) -> Result<ConsumerId, StreamError> {
        let queue_bytes = self.shared.config.queue_bytes;
        self.subscribe_with_queue(label, writer, closer, queue_bytes)
    }

    /// [`PublisherHandle::subscribe`] with a per-consumer queue size (e.g. a recording-like local
    /// consumer that wants more slack than the stream default).
    pub fn subscribe_with_queue(
        &self,
        label: impl Into<String>,
        writer: Box<dyn Write + Send>,
        closer: Box<dyn FnOnce(CloseReason) + Send>,
        queue_bytes: usize,
    ) -> Result<ConsumerId, StreamError> {
        if self.shared.mode != Mode::Egress {
            return Err(StreamError::Config(
                "decoder feeds attach through FeedAttacher".into(),
            ));
        }
        self.shared
            .subscribe(label.into(), writer, closer, queue_bytes)
    }

    /// Stats for open consumers and up to 64 recently closed ones.
    pub fn consumer_stats(&self) -> Vec<ConsumerStats> {
        self.shared.all().iter().map(|c| c.stats()).collect()
    }

    /// Stats for one consumer.
    pub fn stats(&self, id: ConsumerId) -> Option<ConsumerStats> {
        self.shared.find(id).map(|c| c.stats())
    }

    /// Open (not closed) consumers.
    pub fn open_consumers(&self) -> usize {
        lock(&self.shared.list)
            .active
            .iter()
            .filter(|c| !c.closed.load(Ordering::Acquire))
            .count()
    }

    /// Force-closes one consumer (queued bytes are discarded).
    pub fn close(&self, id: ConsumerId) -> bool {
        let closed = self
            .shared
            .find(id)
            .is_some_and(|c| c.close(CloseReason::Detached));
        self.shared.prune();
        closed
    }

    /// Waits until every consumer has closed (e.g. drained after the publisher finished).
    /// Returns `false` on timeout. For shutdown paths and tests; it polls every millisecond.
    pub fn wait_closed(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self
                .shared
                .all()
                .iter()
                .all(|c| c.closed.load(Ordering::Acquire))
            {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(1));
        }
    }
}

/// The producer side of one stream. Dropping it (or [`Publisher::finish`]) lets every consumer
/// drain what is queued and then closes it.
pub struct Publisher {
    shared: Arc<Shared>,
    header: StreamHeader,
    snapshot: Vec<Arc<Consumer>>,
    seen_generation: u64,
    next_seq: u64,
    scratch: Vec<u8>,
}

impl Publisher {
    /// A gated egress publisher for `header`.
    pub fn new(header: StreamHeader, config: PublisherConfig) -> Result<Self, StreamError> {
        Self::with_mode(header, config, Mode::Egress)
    }

    fn with_mode(
        header: StreamHeader,
        config: PublisherConfig,
        mode: Mode,
    ) -> Result<Self, StreamError> {
        let json = header.to_json_bytes()?;
        let header_frame = if mode == Mode::FeedRaw {
            Vec::new()
        } else {
            let mut f = Vec::with_capacity(LEN_PREFIX + json.len());
            super::frame::encode_frame(&mut f, &json, super::frame::HEADER_MAX_LEN)?;
            f
        };
        let needed =
            header_frame.len() + LEN_PREFIX + header.max_frame_len as usize + MARKER_MAX_LEN;
        if config.queue_bytes < needed {
            return Err(StreamError::Config(format!(
                "queue_bytes {} < {needed} (header + one max_frame_len record + marker)",
                config.queue_bytes
            )));
        }
        if config.disconnect_after_drops == 0 {
            return Err(StreamError::Config(
                "disconnect_after_drops must be > 0".into(),
            ));
        }
        Ok(Self {
            shared: Arc::new(Shared {
                mode,
                kind: header.kind,
                header_frame,
                config,
                min_queue_bytes: needed,
                list: Mutex::new(List {
                    active: Vec::new(),
                    finished: VecDeque::new(),
                    closed_for_new: false,
                }),
                generation: AtomicU64::new(0),
                next_id: AtomicU64::new(0),
            }),
            header,
            snapshot: Vec::new(),
            seen_generation: u64::MAX,
            next_seq: 0,
            scratch: Vec::new(),
        })
    }

    /// Handle for listeners and stats.
    pub fn handle(&self) -> PublisherHandle {
        PublisherHandle {
            shared: Arc::clone(&self.shared),
        }
    }

    /// The stream header.
    pub fn header(&self) -> &StreamHeader {
        &self.header
    }

    /// The seq the next record will get.
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// Publishes a message record (messages streams). The record's class is clamped to the
    /// header class; content is only serialised when the effective class permits it.
    pub fn publish_message(&mut self, rec: &MessageRecord) -> Result<PublishOutcome, StreamError> {
        if self.header.kind != StreamKind::Messages {
            return Err(StreamError::WrongKind {
                kind: self.header.kind,
                operation: "publish_message",
            });
        }
        let class = gate::clamp(self.header.content_class, rec.content_class);
        let permitted = class.permits_content();
        let wire = MessageWire {
            kind: "message",
            seq: self.next_seq,
            t: rec.t,
            emitter_id: rec.emitter_id,
            provenance_ref: rec.provenance_ref,
            content_class: class,
            gated: !permitted,
            decode_id: rec.decode_id,
            annotation_id: rec.annotation_id,
            decoder: rec.decoder.as_deref(),
            frame_model: rec.frame_model.as_deref(),
            crc_status: rec.crc_status,
            identity: rec.identity.as_ref(),
            metadata: &rec.metadata,
            content: if permitted {
                rec.content.as_ref()
            } else {
                None
            },
        };
        let mut scratch = std::mem::take(&mut self.scratch);
        scratch.clear();
        scratch.extend_from_slice(&[0; LEN_PREFIX]);
        let encoded = serde_json::to_writer(&mut scratch, &wire)
            .map_err(StreamError::from)
            .and_then(|()| {
                scratch.push(b'\n');
                Ok(frame_prefix(
                    scratch.len() - LEN_PREFIX,
                    self.header.max_frame_len,
                )?)
            });
        let result = encoded.map(|prefix| {
            scratch[..LEN_PREFIX].copy_from_slice(&prefix);
            let seq = self.next_seq;
            self.next_seq += 1;
            self.offer(&[&scratch], seq, rec.t, 0)
        });
        self.scratch = scratch;
        result
    }

    /// Publishes a binary record (bits, symbols, iq, audio, spectrum). On a stream whose class
    /// forbids content for this kind, the payload is withheld, a header-only `GATED` record is
    /// published instead, and [`StreamError::ContentGated`] is returned.
    pub fn publish_binary(&mut self, rec: BinaryRecord<'_>) -> Result<PublishOutcome, StreamError> {
        let permitted = gate::binary_payload_permitted(self.header.kind, self.header.content_class);
        let outcome = self.binary(rec, permitted)?;
        if permitted {
            Ok(outcome)
        } else {
            Err(StreamError::ContentGated {
                kind: self.header.kind,
                class: self.header.content_class,
                outcome,
            })
        }
    }

    fn binary(
        &mut self,
        rec: BinaryRecord<'_>,
        payload_permitted: bool,
    ) -> Result<PublishOutcome, StreamError> {
        if !self.header.kind.is_binary() {
            return Err(StreamError::WrongKind {
                kind: self.header.kind,
                operation: "publish_binary",
            });
        }
        let max = self.header.max_frame_len;
        let full_len = BINARY_RECORD_HEADER_LEN + rec.payload.len();
        frame_prefix(full_len, max)?;
        let mut flags = RecordFlags(rec.flags.0 & !RecordFlags::GATED.0);
        if !payload_permitted {
            flags = flags.with(RecordFlags::GATED);
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        if self.shared.mode == Mode::FeedRaw {
            return Ok(self.offer(&[rec.payload], seq, rec.t, rec.sample_index));
        }
        let header = BinaryRecordHeader {
            record_type: BinaryRecordType::Data as u8,
            flags,
            payload_len: rec.payload.len() as u32,
            seq,
            t: rec.t,
            sample_index: rec.sample_index,
        };
        let frame_len = if payload_permitted {
            full_len
        } else {
            BINARY_RECORD_HEADER_LEN
        };
        let mut head = [0u8; LEN_PREFIX + BINARY_RECORD_HEADER_LEN];
        head[..LEN_PREFIX].copy_from_slice(&frame_prefix(frame_len, max)?);
        head[LEN_PREFIX..].copy_from_slice(&header.encode());
        Ok(if payload_permitted {
            self.offer(&[&head, rec.payload], seq, rec.t, rec.sample_index)
        } else {
            self.offer(&[&head], seq, rec.t, rec.sample_index)
        })
    }

    /// Offers one frame (given as parts) to every open consumer.
    fn offer(
        &mut self,
        parts: &[&[u8]],
        seq: u64,
        t: Timestamp,
        sample_index: u64,
    ) -> PublishOutcome {
        let generation = self.shared.generation.load(Ordering::Acquire);
        if generation != self.seen_generation {
            let list = lock(&self.shared.list);
            self.snapshot.clear();
            self.snapshot.extend(list.active.iter().cloned());
            self.seen_generation = generation;
        }
        let total: usize = parts.iter().map(|p| p.len()).sum();
        let markers = self.shared.mode != Mode::FeedRaw;
        let binary = self.shared.kind.is_binary();
        let config = self.shared.config;
        let mut out = PublishOutcome::default();
        let mut prune = false;
        let mut marker = [0u8; MARKER_MAX_LEN];
        for c in &self.snapshot {
            if c.closed.load(Ordering::Acquire) {
                prune = true;
                continue;
            }
            let mut g = lock(&c.inner);
            if g.state != ConsumerState::Open {
                prune |= matches!(g.state, ConsumerState::Closed(_));
                continue;
            }
            out.consumers += 1;
            let marker_len = match (g.pending_drop, markers) {
                (Some(m), true) => encode_marker(binary, &m, &mut marker),
                _ => 0,
            };
            if g.ring.free() >= total + marker_len {
                let was_empty = g.ring.len == 0;
                if g.pending_drop.take().is_some() && markers {
                    g.ring.push(&marker[..marker_len]);
                    g.counters.drop_markers += 1;
                }
                for p in parts {
                    g.ring.push(p);
                }
                g.counters.records_enqueued += 1;
                g.counters.bytes_enqueued += (total + marker_len) as u64;
                g.consecutive_drops = 0;
                g.full_since = None;
                drop(g);
                if was_empty {
                    c.cv.notify_one();
                }
                out.enqueued += 1;
            } else {
                out.dropped += 1;
                g.counters.records_dropped += 1;
                g.consecutive_drops += 1;
                match &mut g.pending_drop {
                    Some(m) => m.count += 1,
                    None => {
                        g.pending_drop = Some(DropMarker {
                            first_seq: seq,
                            count: 1,
                            t,
                            sample_index,
                        })
                    }
                }
                let since = *g.full_since.get_or_insert_with(Instant::now);
                let stale = g.consecutive_drops >= config.disconnect_after_drops
                    || since.elapsed() >= config.disconnect_after;
                drop(g);
                if stale && c.close(CloseReason::SlowConsumer) {
                    out.disconnected += 1;
                    prune = true;
                }
            }
        }
        if prune {
            self.shared.prune();
        }
        out
    }

    /// Finishes the stream: consumers drain their queues and close. Same as dropping.
    pub fn finish(self) {}
}

impl Drop for Publisher {
    fn drop(&mut self) {
        self.shared.finish();
    }
}

/// Framing of the plugin data plane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeedFraming {
    /// Contract framing: header frame, binary records, drop markers.
    HackriffV1,
    /// Bare payload bytes only (for existing tools that read raw samples on stdin, e.g.
    /// readsb). Drops are counted but invisible to the plugin.
    Raw,
}

/// The internal data plane from capture to one decoder plugin (ADR-0003). Same queues,
/// drop-not-block policy and framing as a [`Publisher`], **without the egress gate**: decoders
/// must see samples to decode; the plugin host clamps and gates what comes back. It can only
/// attach to a child process's stdin, never to a listener.
pub struct DecoderFeed {
    publisher: Publisher,
}

impl DecoderFeed {
    /// A feed for a binary stream `header`.
    pub fn new(
        header: StreamHeader,
        framing: FeedFraming,
        config: PublisherConfig,
    ) -> Result<Self, StreamError> {
        if !header.kind.is_binary() {
            return Err(StreamError::WrongKind {
                kind: header.kind,
                operation: "DecoderFeed::new",
            });
        }
        let mode = match framing {
            FeedFraming::HackriffV1 => Mode::FeedFramed,
            FeedFraming::Raw => Mode::FeedRaw,
        };
        Ok(Self {
            publisher: Publisher::with_mode(header, config, mode)?,
        })
    }

    /// Handle for attaching plugin processes.
    pub fn attacher(&self) -> FeedAttacher {
        FeedAttacher {
            shared: Arc::clone(&self.publisher.shared),
        }
    }

    /// The input stream header sent to plugins.
    pub fn header(&self) -> &StreamHeader {
        &self.publisher.header
    }

    /// Offers one record; never waits. Errors only for an oversize record.
    pub fn push(&mut self, rec: BinaryRecord<'_>) -> Result<PublishOutcome, StreamError> {
        self.publisher.binary(rec, true)
    }
}

/// Attaches plugin stdin pipes to a [`DecoderFeed`].
#[derive(Clone)]
pub struct FeedAttacher {
    shared: Arc<Shared>,
}

impl FeedAttacher {
    /// Attaches a child's stdin. `on_stall` runs if the plugin stays full past the policy (it
    /// should kill the child, which unblocks the writer).
    pub fn attach_child_stdin(
        &self,
        label: impl Into<String>,
        stdin: ChildStdin,
        on_stall: Box<dyn FnOnce() + Send>,
    ) -> Result<ConsumerId, StreamError> {
        self.shared.subscribe(
            label.into(),
            Box::new(stdin),
            Box::new(move |reason| {
                if reason == CloseReason::SlowConsumer {
                    on_stall();
                }
            }),
            self.shared.config.queue_bytes,
        )
    }

    /// Detaches a consumer (queued bytes are discarded) and returns its final stats.
    pub fn detach(&self, id: ConsumerId) -> Option<ConsumerStats> {
        let c = self.shared.find(id)?;
        c.close(CloseReason::Detached);
        self.shared.prune();
        Some(c.stats())
    }

    /// Stats for one consumer.
    pub fn stats(&self, id: ConsumerId) -> Option<ConsumerStats> {
        self.shared.find(id).map(|c| c.stats())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_wraps_and_releases() {
        let mut r = Ring::new(8);
        r.push(&[1, 2, 3, 4, 5]);
        let mut out = [0u8; 3];
        assert_eq!(r.pop_into(&mut out), 3);
        assert_eq!(out, [1, 2, 3]);
        r.push(&[6, 7, 8, 9, 10, 11]);
        assert_eq!(r.free(), 0);
        let mut all = [0u8; 16];
        assert_eq!(r.pop_into(&mut all), 8);
        assert_eq!(&all[..8], &[4, 5, 6, 7, 8, 9, 10, 11]);
        r.push(&[1]);
        assert_eq!(r.release(), 1);
        assert_eq!(r.free(), 0);
        assert_eq!(r.pop_into(&mut all), 0);
    }

    #[test]
    fn config_must_hold_a_max_record() {
        let h = StreamHeader::new("m", StreamKind::Messages, ContentClass::Unrestricted, "t");
        let small = PublisherConfig {
            queue_bytes: 1024,
            ..PublisherConfig::default()
        };
        assert!(matches!(
            Publisher::new(h.clone(), small),
            Err(StreamError::Config(_))
        ));
        assert!(Publisher::new(h.clone(), PublisherConfig::default()).is_ok());
        assert!(DecoderFeed::new(h, FeedFraming::Raw, PublisherConfig::default()).is_err());
    }
}
