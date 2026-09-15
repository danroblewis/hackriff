//! Drop-not-block fan-out (ADR-0004 backpressure; docs/stream-contract.md §7).
//!
//! # Shape
//! - One [`Publisher`] per stream, owned by the producer thread (`&mut self` publish calls).
//! - One bounded **byte ring** per consumer, allocated once at subscribe time, holding whole
//!   frames. A frame that does not fit is **dropped for that consumer only**; the producer never
//!   waits for space. Drops are counted per consumer, and the next frame that fits (or the end
//!   of the stream) is preceded by a "dropped N" marker frame naming the missing seq range.
//! - One writer thread per consumer blocks on a condvar while its ring is empty (no polling),
//!   copies at most 16 KiB out under the lock, and writes it to the socket with the lock
//!   released. A slow or stuck consumer therefore only ever blocks its own writer thread.
//! - A consumer that stays full for [`PublisherConfig::disconnect_after`], or drops
//!   [`PublisherConfig::disconnect_after_drops`] consecutive records, is disconnected: its
//!   transport is shut down (which unblocks its writer) and its ring is freed.
//! - At most [`PublisherConfig::max_consumers`] consumers are open at once; after the publisher
//!   finishes, a consumer still draining after [`PublisherConfig::drain_timeout`] is closed.
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
//!
//! # Egress enforcement (legal guardrail)
//! - **Locality.** Every consumer is subscribed through [`Shared::subscribe`] with a
//!   [`Locality`] derived from its writer type ([`EgressWriter`]); a `Remote` consumer of an
//!   `own-key-decrypted` stream is refused. Listeners, bridges and direct subscribers all pass
//!   through it.
//! - **Metadata policy.** Messages whose effective class forbids content are reduced to the
//!   publisher's [`MetadataPolicy`] in [`Publisher::publish_message`].
//! - **Gated spectrum.** Rows are rate- and size-enforced in [`Publisher::publish_binary`].

use std::any::Any;
use std::collections::VecDeque;
use std::io::{self, Write};
use std::net::TcpStream;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::process::ChildStdin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use hk_model::sigmf::Datatype;
use hk_model::{ContentClass, CrcStatus, DecodedIdentity, EmitterId, Timestamp};
use serde_json::Value;

use super::frame::{FrameError, LEN_PREFIX, frame_prefix};
use super::gate;
use super::header::{HeaderError, StreamHeader, StreamKind};
use super::inspector::{FRAME_RECORD_TYPE, FrameContent, FrameRecord, InspectorRecordType};

/// The on-wire inspector `frame` record (§14.2), built only by [`Publisher::publish_frame`].
#[derive(serde::Serialize)]
struct FrameWire<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    seq: u64,
    t: i64,
    content_class: ContentClass,
    gated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    crc_status: Option<CrcStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    decoder: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    frame_model: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    emitter_id: Option<EmitterId>,
    metadata: &'a Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<&'a FrameContent>,
}

/// The on-wire inspector `status`/`edit` record (§14.3).
#[derive(serde::Serialize)]
struct MetaWire<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    seq: u64,
    t: Timestamp,
    content_class: ContentClass,
    gated: bool,
    metadata: &'a Value,
}
use super::policy::{self, MetadataPolicy};
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

/// Per-consumer queue, consumer cap and disconnect policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PublisherConfig {
    /// Ring size per consumer, bytes. Must hold the header frame plus one maximum-size record
    /// frame plus a marker.
    pub queue_bytes: usize,
    /// Disconnect after this many consecutive drops (`u64::MAX`: never by count).
    pub disconnect_after_drops: u64,
    /// Disconnect after the ring has stayed full (every offer dropped) for this long.
    pub disconnect_after: Duration,
    /// Most consumers open at once; further subscriptions are refused.
    pub max_consumers: usize,
    /// After the publisher finishes, a consumer that has not drained within this long is closed.
    pub drain_timeout: Duration,
}

impl Default for PublisherConfig {
    fn default() -> Self {
        Self {
            queue_bytes: 8 * 1024 * 1024,
            disconnect_after_drops: u64::MAX,
            disconnect_after: Duration::from_secs(5),
            max_consumers: 16,
            drain_timeout: Duration::from_secs(5),
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
    /// ADR-0004: a spectrum stream under a content-forbidding class must declare a row rate of at
    /// most [`gate::GATED_SPECTRUM_MAX_ROW_RATE_HZ`] in `sample_rate_hz`.
    #[error(
        "spectrum row rate {row_rate_hz:?} Hz refused under content class {class:?} (max {} rows/s)",
        gate::GATED_SPECTRUM_MAX_ROW_RATE_HZ
    )]
    SpectrumRowRate {
        /// Header class.
        class: ContentClass,
        /// Declared row rate.
        row_rate_hz: Option<f64>,
    },
    /// ADR-0004: a spectrum stream under a content-forbidding class must declare `fft_size` and a
    /// known `datatype`, which bound each row's payload.
    #[error("spectrum stream under content class {class:?} must declare {missing}")]
    SpectrumGeometry {
        /// Header class.
        class: ContentClass,
        /// The missing header field.
        missing: &'static str,
    },
    /// ADR-0004: a row on a gated spectrum stream exceeded the declared row rate or the
    /// `fft_size` payload cap. It was withheld (it still consumed a seq) and is reported to
    /// consumers by a counted `GATED` drop marker before the next delivered row.
    #[error("spectrum row withheld under content class {class:?}: {reason:?}")]
    SpectrumGated {
        /// Header class.
        class: ContentClass,
        /// Which cap.
        reason: SpectrumGateReason,
    },
    /// ADR-0004: a messages stream whose header class forbids content needs a
    /// [`MetadataPolicy`] (fail closed); use [`Publisher::with_metadata_policy`].
    #[error("messages stream under content class {class:?} needs a metadata policy")]
    MetadataPolicyRequired {
        /// Header class.
        class: ContentClass,
    },
    /// `own-key-decrypted` streams are local-only: a [`Locality::Remote`] consumer was refused.
    #[error("{class:?} streams are local-only: remote consumer refused (serve on a Unix socket)")]
    LocalOnly {
        /// Header class.
        class: ContentClass,
    },
    /// The consumer cap is reached.
    #[error("consumer limit {max} reached")]
    TooManyConsumers {
        /// The cap.
        max: usize,
    },
    /// Bad configuration.
    #[error("invalid publisher config: {0}")]
    Config(String),
    /// The publisher has finished; no new consumers.
    #[error("stream finished")]
    Finished,
    /// A status record was not a flat metadata object (contract 1.1); nothing was published.
    #[error("status record is not a flat metadata object")]
    NotMetadata,
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

/// Which gated-spectrum cap withheld a row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpectrumGateReason {
    /// Faster than the declared row rate (wall-clock arrival or `t` spacing), or `t` went
    /// backwards.
    RowRate,
    /// Payload longer than `fft_size` x element size.
    PayloadLen,
}

/// Egress-gate counters of one publisher.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GateStats {
    /// Message fields (metadata keys, frame models/labels, identities, decoders) removed or
    /// replaced by the metadata policy.
    pub metadata_fields_sanitized: u64,
    /// Spectrum rows withheld by the gated-spectrum rate or payload cap.
    pub spectrum_rows_gated: u64,
    /// Remote consumers refused because the stream is local-only.
    pub remote_consumers_refused: u64,
}

/// Where a consumer's bytes go (docs/stream-contract.md §2, locality rule).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Locality {
    /// Stays on this host: a Unix-domain socket, a child's stdin, an in-process sink.
    Local,
    /// Leaves (or may leave) this host: TCP, a WebSocket bridge, anything network-backed.
    Remote,
}

/// A consumer transport that knows its [`Locality`]. Implemented for [`UnixStream`] (`Local`)
/// and [`TcpStream`] (`Remote`); wrap anything else in [`Declared`]. A bridge (WebSocket, relay)
/// must subscribe as `Remote`.
pub trait EgressWriter: Write + Send + 'static {
    /// The transport's locality.
    fn locality(&self) -> Locality;
}

impl EgressWriter for UnixStream {
    fn locality(&self) -> Locality {
        Locality::Local
    }
}

impl EgressWriter for TcpStream {
    fn locality(&self) -> Locality {
        Locality::Remote
    }
}

/// A writer with a declared [`Locality`], for transports the type system cannot classify
/// (in-process buffers, bridges). `Declared::local` is an assertion that the bytes never leave the
/// host: it is reviewed code. A `TcpStream` declared local is still `Remote`.
pub struct Declared<W> {
    writer: W,
    locality: Locality,
}

impl<W: Write + Send + 'static> Declared<W> {
    /// Declares `writer` local (a `TcpStream` stays `Remote`).
    pub fn local(writer: W) -> Self {
        let any: &dyn Any = &writer;
        let locality = if any.is::<TcpStream>() {
            Locality::Remote
        } else {
            Locality::Local
        };
        Self { writer, locality }
    }

    /// Declares `writer` remote (bridges, relays, anything network-backed).
    pub fn remote(writer: W) -> Self {
        Self {
            writer,
            locality: Locality::Remote,
        }
    }
}

impl<W: Write> Write for Declared<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.writer.write(buf)
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.writer.write_all(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

impl<W: Write + Send + 'static> EgressWriter for Declared<W> {
    fn locality(&self) -> Locality {
        self.locality
    }
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
    /// The publisher finished and the consumer did not drain within `drain_timeout`.
    DrainTimeout,
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

fn writer_loop(c: Arc<Consumer>, mut w: Box<dyn Write + Send>, binary: bool, markers: bool) {
    let mut chunk = vec![0u8; WRITE_CHUNK];
    let mut marker = [0u8; MARKER_MAX_LEN];
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
                        // Drops just before the end still get their marker (the ring is empty,
                        // so a marker always fits).
                        if markers && let Some(m) = g.pending_drop.take() {
                            let len = encode_marker(binary, &m, &mut marker);
                            g.ring.push(&marker[..len]);
                            g.counters.drop_markers += 1;
                            g.counters.bytes_enqueued += len as u64;
                            continue;
                        }
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
    class: ContentClass,
    header_frame: Vec<u8>,
    config: PublisherConfig,
    min_queue_bytes: usize,
    list: Mutex<List>,
    generation: AtomicU64,
    next_id: AtomicU64,
    metadata_sanitized: AtomicU64,
    spectrum_rows_gated: AtomicU64,
    remote_refused: AtomicU64,
}

impl Shared {
    /// The single subscription path for every consumer (listeners, bridges, direct subscribers,
    /// plugin stdin). Refuses a `Remote` consumer of a local-only stream before anything is
    /// allocated or queued.
    fn subscribe(
        self: &Arc<Self>,
        label: String,
        writer: Box<dyn Write + Send>,
        locality: Locality,
        closer: Closer,
        queue_bytes: usize,
    ) -> Result<ConsumerId, StreamError> {
        if locality == Locality::Remote && !gate::remote_transport_permitted(self.class) {
            self.remote_refused.fetch_add(1, Ordering::Relaxed);
            return Err(StreamError::LocalOnly { class: self.class });
        }
        if queue_bytes < self.min_queue_bytes {
            return Err(StreamError::Config(format!(
                "queue_bytes {queue_bytes} < {} (header + one max_frame_len record + marker)",
                self.min_queue_bytes
            )));
        }
        {
            let list = lock(&self.list);
            if list.closed_for_new {
                return Err(StreamError::Finished);
            }
            if Self::open_count(&list) >= self.config.max_consumers {
                return Err(StreamError::TooManyConsumers {
                    max: self.config.max_consumers,
                });
            }
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
        let (binary, markers) = (self.kind.is_binary(), self.mode != Mode::FeedRaw);
        thread::Builder::new()
            .name(format!("hk-stream-tx-{id}"))
            .stack_size(WRITER_STACK)
            .spawn(move || writer_loop(for_thread, writer, binary, markers))?;
        let mut list = lock(&self.list);
        let refusal = if list.closed_for_new {
            Some(StreamError::Finished)
        } else if Self::open_count(&list) >= self.config.max_consumers {
            Some(StreamError::TooManyConsumers {
                max: self.config.max_consumers,
            })
        } else {
            None
        };
        if let Some(e) = refusal {
            drop(list);
            consumer.close(CloseReason::Detached);
            return Err(e);
        }
        list.active.push(consumer);
        self.generation.fetch_add(1, Ordering::Release);
        Ok(id)
    }

    fn open_count(list: &List) -> usize {
        list.active
            .iter()
            .filter(|c| !c.closed.load(Ordering::Acquire))
            .count()
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

    fn close(&self, id: ConsumerId, reason: CloseReason) -> bool {
        let closed = self.find(id).is_some_and(|c| c.close(reason));
        self.prune();
        closed
    }

    /// Stops new subscriptions, lets open consumers drain, and closes any consumer still
    /// draining after `drain_timeout`.
    fn finish(self: &Arc<Self>) {
        let consumers = {
            let mut list = lock(&self.list);
            list.closed_for_new = true;
            list.active.clone()
        };
        let mut draining = Vec::new();
        for c in consumers {
            let mut g = lock(&c.inner);
            if g.state == ConsumerState::Open {
                g.state = ConsumerState::Draining;
                draining.push(Arc::clone(&c));
            }
            drop(g);
            c.cv.notify_all();
        }
        if draining.is_empty() {
            return;
        }
        let timeout = self.config.drain_timeout;
        let shared = Arc::clone(self);
        // One short-lived watchdog per finished stream; it exits as soon as all have drained.
        let _ = thread::Builder::new()
            .name("hk-stream-drain".into())
            .stack_size(WRITER_STACK)
            .spawn(move || {
                let deadline = Instant::now() + timeout;
                while Instant::now() < deadline {
                    if draining.iter().all(|c| c.closed.load(Ordering::Acquire)) {
                        break;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                for c in &draining {
                    c.close(CloseReason::DrainTimeout);
                }
                shared.prune();
            });
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
    /// consumer closes for any reason. Refused beyond `max_consumers`, and refused
    /// ([`StreamError::LocalOnly`]) when `writer` is [`Locality::Remote`] and the stream is
    /// `own-key-decrypted`. Pass sockets as their concrete type (`UnixStream`, `TcpStream`);
    /// anything else through [`Declared`].
    pub fn subscribe<W: EgressWriter>(
        &self,
        label: impl Into<String>,
        writer: W,
        closer: Box<dyn FnOnce(CloseReason) + Send>,
    ) -> Result<ConsumerId, StreamError> {
        let queue_bytes = self.shared.config.queue_bytes;
        self.subscribe_with_queue(label, writer, closer, queue_bytes)
    }

    /// [`PublisherHandle::subscribe`] with a per-consumer queue size (e.g. a recording-like local
    /// consumer that wants more slack than the stream default).
    pub fn subscribe_with_queue<W: EgressWriter>(
        &self,
        label: impl Into<String>,
        writer: W,
        closer: Box<dyn FnOnce(CloseReason) + Send>,
        queue_bytes: usize,
    ) -> Result<ConsumerId, StreamError> {
        if self.shared.mode != Mode::Egress {
            return Err(StreamError::Config(
                "decoder feeds attach through FeedAttacher".into(),
            ));
        }
        let locality = writer.locality();
        self.shared.subscribe(
            label.into(),
            Box::new(writer),
            locality,
            closer,
            queue_bytes,
        )
    }

    /// Egress-gate counters.
    pub fn gate_stats(&self) -> GateStats {
        GateStats {
            metadata_fields_sanitized: self.shared.metadata_sanitized.load(Ordering::Relaxed),
            spectrum_rows_gated: self.shared.spectrum_rows_gated.load(Ordering::Relaxed),
            remote_consumers_refused: self.shared.remote_refused.load(Ordering::Relaxed),
        }
    }

    /// The stream header's content class (transports use it: own-key streams are local-only).
    pub fn content_class(&self) -> ContentClass {
        self.shared.class
    }

    /// The stream kind.
    pub fn kind(&self) -> StreamKind {
        self.shared.kind
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
        Shared::open_count(&lock(&self.shared.list))
    }

    /// Force-closes one consumer (queued bytes are discarded).
    pub fn close(&self, id: ConsumerId) -> bool {
        self.shared.close(id, CloseReason::Detached)
    }

    /// Closes a consumer whose peer hung up (used by listeners).
    pub(crate) fn peer_gone(&self, id: ConsumerId) -> bool {
        self.shared.close(id, CloseReason::PeerGone)
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
    policy: Option<MetadataPolicy>,
    spectrum_gate: Option<SpectrumGate>,
    snapshot: Vec<Arc<Consumer>>,
    seen_generation: u64,
    next_seq: u64,
    scratch: Vec<u8>,
}

/// Per-row enforcement of a gated spectrum stream's declared row rate and payload size.
struct SpectrumGate {
    rate_hz: f64,
    max_payload: usize,
    wall_tokens: f64,
    wall_last: Option<Instant>,
    time_tokens: f64,
    time_last_ns: Option<i64>,
    /// Withheld run not yet reported: (first seq, count).
    pending: Option<(u64, u64)>,
    /// `t` and sample index of the last delivered row.
    last_delivered: (Timestamp, u64),
}

impl SpectrumGate {
    /// Token buckets over wall-clock arrival and over `t` spacing; a row needs a token from both.
    fn admit(&mut self, t: Timestamp) -> bool {
        let burst = gate::GATED_SPECTRUM_BURST_ROWS;
        let now = Instant::now();
        if let Some(last) = self.wall_last {
            self.wall_tokens = (self.wall_tokens
                + now.duration_since(last).as_secs_f64() * self.rate_hz)
                .min(burst);
        }
        self.wall_last = Some(now);
        let t_ns = t.as_unix_nanos();
        match self.time_last_ns {
            // `t` going backwards is refused and does not move the bucket.
            Some(last) if t_ns < last => return false,
            Some(last) => {
                self.time_tokens = (self.time_tokens
                    + t_ns.saturating_sub(last) as f64 / 1e9 * self.rate_hz)
                    .min(burst);
            }
            None => {}
        }
        self.time_last_ns = Some(t_ns);
        if self.wall_tokens >= 1.0 && self.time_tokens >= 1.0 {
            self.wall_tokens -= 1.0;
            self.time_tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// A message's non-content fields reduced to the publisher's policy.
struct Reduced {
    metadata: Value,
    frame_model: Option<String>,
    identity: Option<DecodedIdentity>,
    decoder: Option<String>,
    removed: u64,
}

impl Publisher {
    /// A gated egress publisher for `header`. A messages stream whose header class forbids
    /// content is refused ([`StreamError::MetadataPolicyRequired`]); use
    /// [`Publisher::with_metadata_policy`]. Without a policy, restricted records on a permitting
    /// stream are reduced to the empty allowlist.
    pub fn new(header: StreamHeader, config: PublisherConfig) -> Result<Self, StreamError> {
        Self::with_mode(header, config, Mode::Egress, None)
    }

    /// A gated egress publisher whose messages are reduced to `policy` whenever their effective
    /// class forbids content (docs/stream-contract.md §6). For a plugin republisher this is the
    /// manifest's `output` policy, with `header.message_schema` set to its `schema_id`.
    pub fn with_metadata_policy(
        header: StreamHeader,
        config: PublisherConfig,
        policy: MetadataPolicy,
    ) -> Result<Self, StreamError> {
        Self::with_mode(header, config, Mode::Egress, Some(policy))
    }

    fn with_mode(
        header: StreamHeader,
        config: PublisherConfig,
        mode: Mode,
        policy: Option<MetadataPolicy>,
    ) -> Result<Self, StreamError> {
        let json = header.to_json_bytes()?;
        let class = header.content_class;
        if header.kind == StreamKind::Spectrum
            && !gate::spectrum_stream_permitted(class, header.sample_rate_hz)
        {
            return Err(StreamError::SpectrumRowRate {
                class,
                row_rate_hz: header.sample_rate_hz,
            });
        }
        if header.kind == StreamKind::Messages && !class.permits_content() && policy.is_none() {
            return Err(StreamError::MetadataPolicyRequired { class });
        }
        let spectrum_gate = if mode == Mode::Egress
            && header.kind == StreamKind::Spectrum
            && !class.permits_content()
        {
            let fft_size =
                header
                    .fft_size
                    .filter(|n| *n > 0)
                    .ok_or(StreamError::SpectrumGeometry {
                        class,
                        missing: "fft_size",
                    })?;
            let datatype: Datatype = header
                .datatype
                .as_deref()
                .and_then(|d| serde_json::from_value(Value::String(d.to_owned())).ok())
                .ok_or(StreamError::SpectrumGeometry {
                    class,
                    missing: "datatype",
                })?;
            let burst = gate::GATED_SPECTRUM_BURST_ROWS;
            Some(SpectrumGate {
                rate_hz: header.sample_rate_hz.unwrap_or(0.0),
                max_payload: fft_size as usize * datatype.bytes_per_sample(),
                wall_tokens: burst,
                wall_last: None,
                time_tokens: burst,
                time_last_ns: None,
                pending: None,
                last_delivered: (Timestamp::from_unix_nanos(0), 0),
            })
        } else {
            None
        };
        let header_frame = if mode == Mode::FeedRaw {
            Vec::new()
        } else {
            let mut f = Vec::with_capacity(LEN_PREFIX + json.len());
            super::frame::encode_frame(&mut f, &json, super::frame::HEADER_MAX_LEN)?;
            f
        };
        // A gated spectrum row may be preceded by both a queue-drop and a gated marker.
        let markers = if spectrum_gate.is_some() { 2 } else { 1 };
        let needed = header_frame.len()
            + LEN_PREFIX
            + header.max_frame_len as usize
            + markers * MARKER_MAX_LEN;
        if config.queue_bytes < needed {
            return Err(StreamError::Config(format!(
                "queue_bytes {} < {needed} (header + one max_frame_len record + marker)",
                config.queue_bytes
            )));
        }
        if config.disconnect_after_drops == 0 || config.max_consumers == 0 {
            return Err(StreamError::Config(
                "disconnect_after_drops and max_consumers must be > 0".into(),
            ));
        }
        Ok(Self {
            shared: Arc::new(Shared {
                mode,
                kind: header.kind,
                class: header.content_class,
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
                metadata_sanitized: AtomicU64::new(0),
                spectrum_rows_gated: AtomicU64::new(0),
                remote_refused: AtomicU64::new(0),
            }),
            header,
            policy,
            spectrum_gate,
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

    /// The metadata policy applied to restricted messages, if any.
    pub fn metadata_policy(&self) -> Option<&MetadataPolicy> {
        self.policy.as_ref()
    }

    /// Reduces a message's non-content fields to the policy (the empty allowlist without one):
    /// metadata keys, frame model (decodes: `frame_models`; annotations: `labels`; otherwise the
    /// header's `message_schema` or omitted), identity shape, and a token-shaped decoder id.
    fn reduce(&self, rec: &MessageRecord) -> Reduced {
        let policy = self.policy.as_ref();
        let (metadata, mut removed) = policy::sanitize_metadata_ref(policy, &rec.metadata);
        let fallback = self
            .header
            .message_schema
            .as_deref()
            .filter(|s| policy::is_token(s));
        let frame_model = match rec.frame_model.as_deref() {
            None => None,
            Some(fm) => {
                let allowed = policy.map_or(&[][..], |p| {
                    if rec.annotation_id.is_some() {
                        p.labels.as_slice()
                    } else {
                        p.frame_models.as_slice()
                    }
                });
                if fallback == Some(fm) || allowed.iter().any(|a| a == fm) {
                    Some(fm.to_owned())
                } else {
                    removed += 1;
                    fallback.map(str::to_owned)
                }
            }
        };
        let (identity, n) = policy::sanitize_identity(policy, rec.identity.clone());
        removed += n;
        let decoder = match rec.decoder.as_deref() {
            Some(d) if policy::is_producer_token(d) => Some(d.to_owned()),
            Some(_) => {
                removed += 1;
                None
            }
            None => None,
        };
        Reduced {
            metadata,
            frame_model,
            identity,
            decoder,
            removed,
        }
    }

    /// Publishes a message record (messages streams). The record's class is clamped to the
    /// header class; content is only serialised when [`gate::message_content_permitted`]; and
    /// when the effective class forbids content, every other field is reduced to the metadata
    /// policy ([`Publisher::with_metadata_policy`]), whoever produced the record.
    pub fn publish_message(&mut self, rec: &MessageRecord) -> Result<PublishOutcome, StreamError> {
        if self.header.kind != StreamKind::Messages {
            return Err(StreamError::WrongKind {
                kind: self.header.kind,
                operation: "publish_message",
            });
        }
        let class = gate::clamp(self.header.content_class, rec.content_class);
        let permitted = gate::message_content_permitted(self.header.content_class, class);
        let reduced = (!class.permits_content()).then(|| self.reduce(rec));
        if let Some(r) = &reduced
            && r.removed > 0
        {
            self.shared
                .metadata_sanitized
                .fetch_add(r.removed, Ordering::Relaxed);
        }
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
            decoder: match &reduced {
                Some(r) => r.decoder.as_deref(),
                None => rec.decoder.as_deref(),
            },
            frame_model: match &reduced {
                Some(r) => r.frame_model.as_deref(),
                None => rec.frame_model.as_deref(),
            },
            crc_status: rec.crc_status,
            identity: match &reduced {
                Some(r) => r.identity.as_ref(),
                None => rec.identity.as_ref(),
            },
            metadata: reduced.as_ref().map_or(&rec.metadata, |r| &r.metadata),
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
            self.offer(&[&scratch], seq, rec.t, 0, 1)
        });
        self.scratch = scratch;
        result
    }

    /// Publishes an inspector `frame` record (docs/stream-contract.md §14.2) on a messages stream.
    /// The publisher assigns `seq` and `gated` (the record's own values are ignored). As for
    /// [`Publisher::publish_message`] (§14.5): the class is clamped to the header class;
    /// `content` (bytes and layers) is serialised only when
    /// [`gate::message_content_permitted`]; and when the effective class forbids content,
    /// `metadata`, `frame_model` and `decoder` are reduced to the metadata policy.
    pub fn publish_frame(&mut self, rec: &FrameRecord) -> Result<PublishOutcome, StreamError> {
        if self.header.kind != StreamKind::Messages {
            return Err(StreamError::WrongKind {
                kind: self.header.kind,
                operation: "publish_frame",
            });
        }
        let class = gate::clamp(self.header.content_class, rec.content_class);
        let permitted = gate::message_content_permitted(self.header.content_class, class);
        let t = Timestamp::from_unix_nanos(rec.t);
        let (metadata, reduced) = if class.permits_content() {
            (serde_json::to_value(&rec.metadata)?, None)
        } else {
            let as_message = MessageRecord {
                t,
                emitter_id: None,
                provenance_ref: None,
                content_class: class,
                decode_id: None,
                annotation_id: None,
                decoder: rec.decoder.clone(),
                frame_model: rec.frame_model.clone(),
                crc_status: None,
                identity: None,
                metadata: serde_json::to_value(&rec.metadata)?,
                content: None,
            };
            let r = self.reduce(&as_message);
            if r.removed > 0 {
                self.shared
                    .metadata_sanitized
                    .fetch_add(r.removed, Ordering::Relaxed);
            }
            (Value::Null, Some(r))
        };
        let wire = FrameWire {
            kind: FRAME_RECORD_TYPE,
            seq: self.next_seq,
            t: rec.t,
            content_class: class,
            gated: !permitted,
            crc_status: rec.crc_status,
            decoder: match &reduced {
                Some(r) => r.decoder.as_deref(),
                None => rec.decoder.as_deref(),
            },
            frame_model: match &reduced {
                Some(r) => r.frame_model.as_deref(),
                None => rec.frame_model.as_deref(),
            },
            emitter_id: rec.emitter_id,
            metadata: reduced.as_ref().map_or(&metadata, |r| &r.metadata),
            content: if permitted {
                rec.content.as_ref()
            } else {
                None
            },
        };
        self.publish_json(&wire, t)
    }

    /// Publishes an inspector `status` or `edit` record (§14.3). They are metadata only:
    /// `metadata` must be a flat object of numbers, booleans and short tokens
    /// ([`policy::metadata_is_allowlist_shaped`]), otherwise nothing is published and
    /// [`StreamError::NotMetadata`] is returned. The class is clamped to the header class.
    pub fn publish_record(
        &mut self,
        record_type: InspectorRecordType,
        t: Timestamp,
        content_class: ContentClass,
        metadata: &Value,
    ) -> Result<PublishOutcome, StreamError> {
        if self.header.kind != StreamKind::Messages {
            return Err(StreamError::WrongKind {
                kind: self.header.kind,
                operation: "publish_record",
            });
        }
        if !policy::metadata_is_allowlist_shaped(metadata) {
            return Err(StreamError::NotMetadata);
        }
        let wire = MetaWire {
            kind: record_type.as_str(),
            seq: self.next_seq,
            t,
            content_class: gate::clamp(self.header.content_class, content_class),
            gated: false,
            metadata,
        };
        self.publish_json(&wire, t)
    }

    /// Serialises one NDJSON record (whose `seq` is [`Self::next_seq`]) into a frame and offers
    /// it to every consumer.
    fn publish_json(
        &mut self,
        wire: &impl serde::Serialize,
        t: Timestamp,
    ) -> Result<PublishOutcome, StreamError> {
        let mut scratch = std::mem::take(&mut self.scratch);
        scratch.clear();
        scratch.extend_from_slice(&[0; LEN_PREFIX]);
        let encoded = serde_json::to_writer(&mut scratch, wire)
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
            self.offer(&[&scratch], seq, t, 0, 1)
        });
        self.scratch = scratch;
        result
    }

    /// Publishes a binary record (bits, symbols, iq, audio, spectrum). On a stream whose class
    /// forbids content for this kind, the payload is withheld, a header-only `GATED` record is
    /// published instead, and [`StreamError::ContentGated`] is returned.
    ///
    /// On a spectrum stream whose class forbids content, a row faster than the declared row rate
    /// (wall-clock arrival or `t` spacing) or longer than `fft_size` x element size is withheld
    /// entirely and [`StreamError::SpectrumGated`] is returned; consumers get one counted `GATED`
    /// drop marker for the withheld run before the next delivered row.
    pub fn publish_binary(&mut self, rec: BinaryRecord<'_>) -> Result<PublishOutcome, StreamError> {
        if !self.header.kind.is_binary() {
            return Err(StreamError::WrongKind {
                kind: self.header.kind,
                operation: "publish_binary",
            });
        }
        frame_prefix(
            BINARY_RECORD_HEADER_LEN + rec.payload.len(),
            self.header.max_frame_len,
        )?;
        let mut lead_buf = [0u8; MARKER_MAX_LEN];
        let mut lead = None;
        if let Some(g) = &mut self.spectrum_gate {
            let reason = if rec.payload.len() > g.max_payload {
                Some(SpectrumGateReason::PayloadLen)
            } else if !g.admit(rec.t) {
                Some(SpectrumGateReason::RowRate)
            } else {
                None
            };
            if let Some(reason) = reason {
                let seq = self.next_seq;
                self.next_seq += 1;
                match &mut g.pending {
                    Some((_, count)) => *count += 1,
                    None => g.pending = Some((seq, 1)),
                }
                self.shared
                    .spectrum_rows_gated
                    .fetch_add(1, Ordering::Relaxed);
                return Err(StreamError::SpectrumGated {
                    class: self.header.content_class,
                    reason,
                });
            }
            g.last_delivered = (rec.t, rec.sample_index);
            if let Some((first_seq, count)) = g.pending.take() {
                let marker = DropMarker {
                    first_seq,
                    count,
                    t: rec.t,
                    sample_index: rec.sample_index,
                    gated: true,
                };
                let len = encode_marker(true, &marker, &mut lead_buf);
                lead = Some((len, first_seq, count));
            }
        }
        let permitted = gate::binary_payload_permitted(self.header.kind, self.header.content_class);
        let lead = lead.map(|(len, first_seq, count)| (&lead_buf[..len], first_seq, count));
        let outcome = self.binary(rec, permitted, lead)?;
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

    /// Publishes a status record (contract 1.1, record type 3) on a binary egress stream, e.g. an
    /// audio stream's level and squelch state. The payload is metadata by construction: `status`
    /// must be a flat object of numbers, booleans and short tokens
    /// ([`policy::metadata_is_allowlist_shaped`]), otherwise nothing is published and
    /// [`StreamError::NotMetadata`] is returned, so no free text can ride on it under any class.
    pub fn publish_status(
        &mut self,
        t: Timestamp,
        sample_index: u64,
        status: &serde_json::Value,
    ) -> Result<PublishOutcome, StreamError> {
        if !self.header.kind.is_binary() || self.shared.mode != Mode::Egress {
            return Err(StreamError::WrongKind {
                kind: self.header.kind,
                operation: "publish_status",
            });
        }
        if !policy::metadata_is_allowlist_shaped(status) {
            return Err(StreamError::NotMetadata);
        }
        let payload = serde_json::to_vec(status)?;
        let max = self.header.max_frame_len;
        let full_len = BINARY_RECORD_HEADER_LEN + payload.len();
        let prefix = frame_prefix(full_len, max)?;
        let seq = self.next_seq;
        self.next_seq += 1;
        let header = BinaryRecordHeader {
            record_type: BinaryRecordType::Status as u8,
            flags: RecordFlags::empty(),
            payload_len: payload.len() as u32,
            seq,
            t,
            sample_index,
        };
        let mut head = [0u8; LEN_PREFIX + BINARY_RECORD_HEADER_LEN];
        head[..LEN_PREFIX].copy_from_slice(&prefix);
        head[LEN_PREFIX..].copy_from_slice(&header.encode());
        Ok(self.offer(&[&head, &payload], seq, t, sample_index, 1))
    }

    /// Frames and offers one binary record. `lead` is an encoded gated marker (with the run's
    /// first seq and count) to deliver atomically in front of the record.
    fn binary(
        &mut self,
        rec: BinaryRecord<'_>,
        payload_permitted: bool,
        lead: Option<(&[u8], u64, u64)>,
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
            return Ok(self.offer(&[rec.payload], seq, rec.t, rec.sample_index, 1));
        }
        let (lead_bytes, first_seq, covers) = match lead {
            Some((bytes, first, count)) => (bytes, first, count + 1),
            None => (&[][..], seq, 1),
        };
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
            self.offer(
                &[lead_bytes, &head, rec.payload],
                first_seq,
                rec.t,
                rec.sample_index,
                covers,
            )
        } else {
            self.offer(
                &[lead_bytes, &head],
                first_seq,
                rec.t,
                rec.sample_index,
                covers,
            )
        })
    }

    /// Reports a withheld gated-spectrum run that no delivered row followed (end of stream).
    fn flush_gated_run(&mut self) {
        let Some(g) = &mut self.spectrum_gate else {
            return;
        };
        let Some((first_seq, count)) = g.pending.take() else {
            return;
        };
        let (t, sample_index) = g.last_delivered;
        let marker = DropMarker {
            first_seq,
            count,
            t,
            sample_index,
            gated: true,
        };
        let mut buf = [0u8; MARKER_MAX_LEN];
        let len = encode_marker(true, &marker, &mut buf);
        self.offer(&[&buf[..len]], first_seq, t, sample_index, count);
    }

    /// Offers one frame (given as parts) to every open consumer. The frame accounts for `covers`
    /// seqs starting at `seq` (more than one when a gated marker leads the record), so a
    /// consumer that drops it gets a marker naming all of them.
    fn offer(
        &mut self,
        parts: &[&[u8]],
        seq: u64,
        t: Timestamp,
        sample_index: u64,
        covers: u64,
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
                    Some(m) => m.count += covers,
                    None => {
                        g.pending_drop = Some(DropMarker {
                            first_seq: seq,
                            count: covers,
                            t,
                            sample_index,
                            gated: false,
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
        self.flush_gated_run();
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
            publisher: Publisher::with_mode(header, config, mode, None)?,
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
        self.publisher.binary(rec, true, None)
    }

    /// Free bytes in the fullest open plugin queue, `None` with no consumer attached. A producer
    /// that can pause (a lossless replay) waits on it instead of letting `push` drop; `push`
    /// itself still never waits.
    pub fn min_free_bytes(&self) -> Option<usize> {
        let active: Vec<Arc<Consumer>> = lock(&self.publisher.shared.list).active.clone();
        active
            .iter()
            .filter(|c| !c.closed.load(Ordering::Acquire))
            .filter_map(|c| {
                let g = lock(&c.inner);
                (g.state == ConsumerState::Open).then(|| g.ring.free())
            })
            .min()
    }
}

/// A child's stdin pipe made non-blocking, whose writes can be abandoned from another thread:
/// closing the wake socket's peer makes a blocked write return `BrokenPipe`. A plain blocking
/// write to a pipe held open by a descendant that never reads could otherwise never be woken.
struct WakeablePipe {
    pipe: ChildStdin,
    wake: UnixStream,
}

fn set_nonblocking(fd: std::os::fd::RawFd) -> io::Result<()> {
    // SAFETY: plain fcntl calls on an fd we own for the duration of the call.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

impl Write for WakeablePipe {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        loop {
            match self.pipe.write(buf) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    let mut fds = [
                        libc::pollfd {
                            fd: self.pipe.as_raw_fd(),
                            events: libc::POLLOUT,
                            revents: 0,
                        },
                        libc::pollfd {
                            fd: self.wake.as_raw_fd(),
                            events: libc::POLLIN,
                            revents: 0,
                        },
                    ];
                    // SAFETY: `fds` is a valid array of two initialised pollfd structs.
                    let rc = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
                    if rc < 0 {
                        let e = io::Error::last_os_error();
                        if e.kind() != io::ErrorKind::Interrupted {
                            return Err(e);
                        }
                    } else if fds[1].revents != 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "plugin input detached",
                        ));
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                other => return other,
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Attaches plugin stdin pipes to a [`DecoderFeed`].
#[derive(Clone)]
pub struct FeedAttacher {
    shared: Arc<Shared>,
}

impl FeedAttacher {
    /// Attaches a child's stdin. `on_stall` runs if the plugin stays full past the policy (it
    /// should kill the child's process group). Any close (stall, detach, finish) also wakes a
    /// write blocked on the pipe.
    pub fn attach_child_stdin(
        &self,
        label: impl Into<String>,
        stdin: ChildStdin,
        on_stall: Box<dyn FnOnce() + Send>,
    ) -> Result<ConsumerId, StreamError> {
        set_nonblocking(stdin.as_raw_fd())?;
        let (wake_tx, wake_rx) = UnixStream::pair()?;
        self.shared.subscribe(
            label.into(),
            Box::new(WakeablePipe {
                pipe: stdin,
                wake: wake_rx,
            }),
            // A child's stdin pipe stays on this host.
            Locality::Local,
            Box::new(move |reason| {
                if reason == CloseReason::SlowConsumer {
                    on_stall();
                }
                let _ = wake_tx.shutdown(std::net::Shutdown::Both);
            }),
            self.shared.config.queue_bytes,
        )
    }

    /// Detaches a consumer (queued bytes are discarded, a blocked write is abandoned) and
    /// returns its final stats.
    pub fn detach(&self, id: ConsumerId) -> Option<ConsumerStats> {
        let c = self.shared.find(id)?;
        c.close(CloseReason::Detached);
        self.shared.prune();
        Some(c.stats())
    }

    /// Ends one plugin's input without discarding it: bytes already queued are still written,
    /// then the consumer closes and its stdin pipe is dropped, so the plugin reads EOF (the
    /// contract's "no more input": a decoder flushes and exits). Records pushed afterwards do not
    /// reach it. `false` for an unknown consumer or one that is not open.
    pub fn finish_input(&self, id: ConsumerId) -> bool {
        let Some(c) = self.shared.find(id) else {
            return false;
        };
        let mut g = lock(&c.inner);
        if g.state != ConsumerState::Open {
            return false;
        }
        g.state = ConsumerState::Draining;
        drop(g);
        c.cv.notify_all();
        true
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
        let no_consumers = PublisherConfig {
            max_consumers: 0,
            ..PublisherConfig::default()
        };
        assert!(matches!(
            Publisher::new(h.clone(), no_consumers),
            Err(StreamError::Config(_))
        ));
        assert!(Publisher::new(h.clone(), PublisherConfig::default()).is_ok());
        assert!(DecoderFeed::new(h, FeedFraming::Raw, PublisherConfig::default()).is_err());
    }
}
