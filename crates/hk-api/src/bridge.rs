//! WebSocket bridge: one hk-stream stream to one browser WebSocket (docs/stream-contract.md §10).
//!
//! # Mapping (1:1)
//! - The stream header frame becomes the **first text message** (the JSON object, verbatim).
//! - Each record frame becomes **one message**: text for `messages` streams (NDJSON record or
//!   drop marker, verbatim including its trailing `\n`), binary for every binary kind (32-byte
//!   record header + payload, or a §5.3 marker).
//! - The `u32` length prefix is dropped: WebSocket already frames messages.
//!
//! # Why this shape (legal guardrail and backpressure)
//! - **One subscription point.** A browser connection is subscribed through
//!   [`PublisherHandle::subscribe`] with its sink wrapped in [`Declared::remote`], **always**, even
//!   from loopback: a browser is remote-capable (the page can forward what it receives). The
//!   contract therefore refuses `own-key-decrypted` streams ([`StreamError::LocalOnly`], counted in
//!   `gate_stats().remote_consumers_refused`) before a byte is queued, and every gate the publisher
//!   applies (class ceiling, gated spectrum rate/size) applies unchanged. The bridge never sees a
//!   record the gate has not already passed.
//! - **Same queue, same drop policy.** The publisher's per-consumer writer thread writes into
//!   [`WsSink`]; a slow browser blocks only that thread on its TCP socket, the consumer's bounded
//!   ring fills, records are dropped for that consumer with markers, and the consumer is
//!   disconnected after `disconnect_after` (the closer shuts the socket down). The producer never
//!   waits on a browser. There is no second queue in the bridge.
//! - **Handshake after admission.** The `101 Switching Protocols` response is written by the sink
//!   on its first write, i.e. only once the subscription was accepted. A refused subscription
//!   (local-only, consumer cap, finished stream) is answered with a plain HTTP error status and no
//!   upgrade ever happens.
//! - **Consumers never write.** Anything the browser sends (including a close frame) or a hang-up
//!   closes the consumer ([`watch_peer`]).
//!
//! # A stream id outlives its publishers (T-417)
//! A retune finishes the spectrum publisher and offers a new one under the **same id**, because a
//! header must describe every row after it (T-057) — and a re-plumb rebuilds every reader around
//! the still-open device (T-399), which does the same. Neither is a reason to drop the browser:
//! *"a settle gap is fine … so connected consumers keep receiving after the retune"* (the user,
//! 2026-09-17). So [`watch_peer`] **carries the connection across**: on
//! [`CloseReason::PublisherFinished`] the socket is left open, the bridge waits (up to
//! [`CARRY_OVER_GRACE`]) for the producer's next offer under that id, and re-subscribes the same
//! socket to it. The new publisher's header goes out as a **text message** on the live connection,
//! and the rows after it belong to the new window.
//!
//! **A stream that is really over says so.** A producer that is done withdraws the id
//! ([`StreamRegistry::unregister`], wired to `PipelineConfig::stream_unsink`) and the connection
//! ends at once: an unregistered stream is over, a registered one whose publisher has finished is
//! only between windows. The grace is the bound on that second case.
//!
//! **The gap stays visible.** Nothing is held, repeated or interpolated across it: the rows simply
//! stop and start again, and the header is the honest seam that says the window moved.
//!
//! **And the gap is only the gap (T-425).** While it waits, [`watch_peer`] blocks on the registry
//! ([`StreamRegistry::wait_for_offer_after`]) rather than re-checking it once per [`WATCH_TICK`],
//! so the socket is re-subscribed within microseconds of the offer instead of up to 50 ms later.
//! The producer offers its new publisher when the new segment's first samples arrive and publishes
//! that window's first row a row period later (~40 ms), so a tick-quantised re-attach lost the
//! first one or two rows of every new window — silently, with no drop marker, since the consumer
//! was not subscribed to be told. It made every retune look like a longer break in the air than it
//! was, which is the same lie as papering the seam over, told in the other direction.
//!
//! **This is not the slow-consumer policy.** A consumer that cannot keep up is still dropped
//! deliberately (§7, T-388) — [`CloseReason::SlowConsumer`], [`CloseReason::DrainTimeout`],
//! [`CloseReason::PeerGone`] and [`CloseReason::Detached`] all shut the socket down exactly as
//! before. Only "the producer replaced this stream's publisher" is carried.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::{Duration, Instant};

use hk_model::ContentClass;
use hk_stream::{
    CloseReason, ConsumerId, Declared, FrameDecoder, HEADER_MAX_LEN, PublisherHandle, StreamError,
    StreamHeader, StreamKind,
};
use serde_json::{Value, json};
use tungstenite::protocol::{Role, WebSocket};
use tungstenite::{Bytes, Message, Utf8Bytes};

/// What `/api/streams` reports about a stream: header metadata only, never content.
#[derive(Clone, Debug, PartialEq)]
pub struct StreamInfo {
    /// Header `stream_id`.
    pub stream_id: String,
    /// Header kind.
    pub kind: StreamKind,
    /// Header class.
    pub content_class: ContentClass,
    /// Header datatype.
    pub datatype: Option<String>,
    /// Header sample/row rate.
    pub sample_rate_hz: Option<f64>,
    /// Header RF centre.
    pub center_hz: Option<f64>,
    /// Header bandwidth.
    pub bandwidth_hz: Option<f64>,
    /// Header FFT size.
    pub fft_size: Option<u32>,
    /// Header DC-notch half-width (ADR-0013 §4.9 gap 10).
    pub dc_excluded_hz: Option<f64>,
}

impl StreamInfo {
    /// The listing fields of `header`.
    pub fn from_header(header: &StreamHeader) -> Self {
        Self {
            stream_id: header.stream_id.clone(),
            kind: header.kind,
            content_class: header.content_class,
            datatype: header.datatype.clone(),
            sample_rate_hz: header.sample_rate_hz,
            center_hz: header.center_hz,
            bandwidth_hz: header.bandwidth_hz,
            fft_size: header.fft_size,
            dc_excluded_hz: header.dc_excluded_hz,
        }
    }
}

struct Entry {
    info: StreamInfo,
    header: StreamHeader,
    handle: PublisherHandle,
    generation: u64,
    /// When this offer was registered — what [`StreamRegistry::reap`] ages a finished stream by.
    offered: Instant,
}

impl Entry {
    /// No publisher is going to publish on this stream again and nobody is attached to it: it is
    /// over, and only [`StreamRegistry::linger`] still holds the id open for a carry-over.
    ///
    /// A *live* publisher with no consumers is not spent — the spectrum stream with no browser
    /// open is exactly that — so both halves are required.
    fn spent(&self) -> bool {
        self.handle.finished() && self.handle.open_consumers() == 0
    }
}

/// The publisher currently offered under a `stream_id`, and **which** offer it is.
///
/// A stream id outlives its publishers: a retune or a re-plumb finishes one and offers the next
/// under the same id. `generation` increases on every [`StreamRegistry::register`] anywhere in the
/// registry, so a consumer attached at generation `g` can wait for an offer strictly newer than
/// `g` instead of re-attaching to the finished publisher it already had.
#[derive(Clone)]
pub struct Offer {
    /// The publisher to subscribe to.
    pub handle: PublisherHandle,
    /// Which registration this is.
    pub generation: u64,
}

/// How long a stream whose publisher has finished, with nobody attached, is still offered under
/// its id before the registry withdraws it (T-531).
///
/// It is exactly [`CARRY_OVER_GRACE`] and must not be shorter: the grace is how long
/// [`watch_peer`] will wait in a settle gap for the producer's next offer, and withdrawing the id
/// inside that window would end a connection that was still legitimately waiting. At the grace the
/// waiter gives up of its own accord, so from then on the entry can serve nobody — a new
/// subscriber is refused ([`StreamError::Finished`]) and no consumer is left to carry over.
pub const FINISHED_LINGER: Duration = CARRY_OVER_GRACE;

/// Hard ceiling on registered streams, a fail-closed backstop under [`FINISHED_LINGER`] (T-531).
///
/// The linger bounds the registry at *offer rate × linger*, which is the real bound; this only
/// stops a pathological rate from making that number large. Over the cap the **oldest spent**
/// entries go first, and a live publisher is never evicted: live streams are bounded by the
/// things that make them (one spectrum, one presence, one per plugin, one per recipe output, one
/// per chain slot), so an over-cap registry is always over-cap in spent entries.
pub const MAX_STREAMS: usize = 256;

/// The streams a server offers, by `stream_id`. Registering an id again replaces the entry (e.g.
/// a new publisher per replay pass, or the next window after a retune).
///
/// # A registration has a lifetime (T-531)
/// A stream is offered from the moment a publisher is registered under its id until that
/// publisher has **finished**, has **no open consumer**, and has not been re-offered for
/// [`FINISHED_LINGER`]. Then the registry withdraws it, exactly as a producer's
/// `PipelineConfig::stream_unsink` would.
///
/// Without that, one producer shape leaked the whole run: `hk-pipeline`'s FSK chains publish each
/// emitter's bursts on `bits/fsk-bursts/<emitter>` by creating a publisher, registering it,
/// writing the bursts and finishing it — all inside one call. The entry then sat in the map for
/// ever, listed by `/api/streams`, attachable by nobody (subscribing to a finished publisher is
/// [`StreamError::Finished`]). A 40-minute sweep across 6 GHz met ~1300 emitters and the discovery
/// document grew to 659 KB of them (T-525). That is the accumulator the inventory invariants
/// reject elsewhere — "no unbounded seen-count accumulators; candidates decay and expire when a
/// region goes quiet" — and the same rule applies here: the listing is **time-scoped**, it is what
/// this server is offering *now*, not everything it ever offered.
///
/// Nothing that can still serve a consumer is touched: a publisher between windows (the retune
/// carry-over, T-417/T-425) has not finished its id, only its publisher, and is re-offered long
/// inside the linger; a live publisher with no consumers is not spent at all.
#[derive(Clone)]
pub struct StreamRegistry {
    inner: Arc<RwLock<BTreeMap<String, Entry>>>,
    /// Generation counter and the signal that it moved (T-425): a consumer in a settle gap waits
    /// on this instead of polling, so it is re-subscribed before the next window's first record.
    offers: Arc<(Mutex<u64>, Condvar)>,
    /// [`FINISHED_LINGER`], overridable by [`StreamRegistry::with_linger`] so a test can age a
    /// spent entry without sleeping a minute.
    linger: Duration,
}

impl Default for StreamRegistry {
    fn default() -> Self {
        Self {
            inner: Arc::default(),
            offers: Arc::default(),
            linger: FINISHED_LINGER,
        }
    }
}

impl StreamRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// [`StreamRegistry::new`] with a different [`FINISHED_LINGER`] (tests).
    pub fn with_linger(linger: Duration) -> Self {
        Self {
            linger,
            ..Self::default()
        }
    }

    /// Withdraws every spent entry older than the linger, then, if the map is still over
    /// [`MAX_STREAMS`], the oldest spent entries until it is not (see the [type docs](Self)).
    ///
    /// Called where the map can grow ([`Self::register`]) and where it is read out
    /// ([`Self::listing`]), so no timer thread is needed and a quiet run still answers a listing
    /// with what it is offering now. The scan is over a map the same rule keeps small.
    fn reap(&self) {
        let mut map = self.inner.write().unwrap_or_else(|p| p.into_inner());
        reap_locked(&mut map, self.linger);
    }

    /// Offers `handle` under `header.stream_id`.
    pub fn register(&self, header: &StreamHeader, handle: PublisherHandle) {
        let info = StreamInfo::from_header(header);
        let (lock, cv) = &*self.offers;
        let mut generation = lock.lock().unwrap_or_else(|p| p.into_inner());
        *generation += 1;
        let mut map = self.inner.write().unwrap_or_else(|p| p.into_inner());
        map.insert(
            info.stream_id.clone(),
            Entry {
                info,
                header: header.clone(),
                handle,
                generation: *generation,
                offered: Instant::now(),
            },
        );
        // After the insert and under the same lock: the id being registered is never transiently
        // absent, so a consumer carrying over cannot read the map between a withdrawal and the
        // offer that replaces it.
        reap_locked(&mut map, self.linger);
        drop(map);
        drop(generation);
        cv.notify_all();
    }

    /// Streams currently offered (after a reap).
    pub fn len(&self) -> usize {
        self.reap();
        self.inner.read().unwrap_or_else(|p| p.into_inner()).len()
    }

    /// No stream is offered.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Stops offering **every** stream: the run behind them is over (T-531).
    ///
    /// A bridged consumer whose publisher finishes waits [`CARRY_OVER_GRACE`] for the next offer,
    /// because between windows is the normal case (T-417). On a shutdown there is no next offer
    /// and nothing said so, so every attached browser sat out the full grace while the process
    /// tried to exit under it. This is the contract's own answer — *"a producer that is done
    /// withdraws the id and the connection ends at once"* — applied to the producer being done
    /// for good.
    pub fn clear(&self) {
        let mut map = self.inner.write().unwrap_or_else(|p| p.into_inner());
        map.clear();
    }

    /// Stops offering `stream_id`.
    pub fn unregister(&self, stream_id: &str) -> bool {
        let mut map = self.inner.write().unwrap_or_else(|p| p.into_inner());
        map.remove(stream_id).is_some()
    }

    /// The handle of `stream_id`.
    pub fn handle(&self, stream_id: &str) -> Option<PublisherHandle> {
        let map = self.inner.read().unwrap_or_else(|p| p.into_inner());
        map.get(stream_id).map(|e| e.handle.clone())
    }

    /// The offer under `stream_id`.
    pub fn offer(&self, stream_id: &str) -> Option<Offer> {
        let map = self.inner.read().unwrap_or_else(|p| p.into_inner());
        map.get(stream_id).map(|e| Offer {
            handle: e.handle.clone(),
            generation: e.generation,
        })
    }

    /// The offer under `stream_id`, only if it is newer than `generation` (T-417: the next
    /// publisher of a stream whose previous one has finished, never the finished one again).
    pub fn offer_after(&self, stream_id: &str, generation: u64) -> Option<Offer> {
        self.offer(stream_id).filter(|o| o.generation > generation)
    }

    /// [`StreamRegistry::offer_after`], waiting up to `timeout` for one to appear (T-425).
    ///
    /// A consumer whose publisher has finished is in the settle gap and has nothing else to do, so
    /// it blocks here rather than re-checking on a timer: the producer offers the next publisher
    /// one row period *before* it publishes that window's first record, and a polled re-attach
    /// spends that margin and loses records the consumer is never told about. Returns `None` on
    /// timeout (the caller re-checks the socket and its grace, then waits again).
    pub fn wait_for_offer_after(
        &self,
        stream_id: &str,
        generation: u64,
        timeout: Duration,
    ) -> Option<Offer> {
        let deadline = Instant::now() + timeout;
        let (lock, cv) = &*self.offers;
        // Held across the check so an offer registered between the check and the wait still wakes
        // us: `register` takes this lock before it inserts, and notifies after releasing it.
        let mut seen = lock.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            if let Some(offer) = self.offer_after(stream_id, generation) {
                return Some(offer);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            seen = cv
                .wait_timeout(seen, left)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
    }

    /// The `/api/streams` JSON: metadata only, and only what is offered now (see the
    /// [type docs](Self) for the lifetime a registration has).
    pub fn listing(&self) -> Value {
        self.reap();
        let map = self.inner.read().unwrap_or_else(|p| p.into_inner());
        let streams: Vec<Value> = map
            .values()
            .map(|e| {
                let i = &e.info;
                let class = serde_json::to_value(i.content_class).unwrap_or(Value::Null);
                json!({
                    "stream_id": i.stream_id,
                    "kind": i.kind.as_str(),
                    "content_class": class,
                    "content_permitted": i.content_class.permits_content(),
                    "remote_permitted": hk_stream::gate::remote_transport_permitted(i.content_class),
                    "datatype": i.datatype,
                    "sample_rate_hz": i.sample_rate_hz,
                    "center_hz": i.center_hz,
                    "bandwidth_hz": i.bandwidth_hz,
                    "fft_size": i.fft_size,
                    "dc_excluded_hz": i.dc_excluded_hz,
                    "open_consumers": e.handle.open_consumers(),
                    "ws_path": format!("/ws/{}", i.stream_id),
                    "tcp_target": i.stream_id,
                    "format": format_json(&e.header),
                })
            })
            .collect();
        json!({ "streams": streams })
    }
}

/// [`StreamRegistry::reap`]'s body, with the map already locked.
fn reap_locked(map: &mut BTreeMap<String, Entry>, linger: Duration) {
    let now = Instant::now();
    map.retain(|_, e| !(e.spent() && now.duration_since(e.offered) >= linger));
    if map.len() <= MAX_STREAMS {
        return;
    }
    let mut spent: Vec<(Instant, String)> = map
        .iter()
        .filter(|(_, e)| e.spent())
        .map(|(id, e)| (e.offered, id.clone()))
        .collect();
    spent.sort_unstable();
    let over = map.len().saturating_sub(MAX_STREAMS);
    for (_, id) in spent.into_iter().take(over) {
        map.remove(&id);
    }
}

/// How a stream's records are shaped (T-060 discovery): header fields a client needs to parse
/// payloads. Metadata only.
fn format_json(h: &StreamHeader) -> Value {
    json!({
        "framing": "u32-le length-prefixed frames; first frame is the JSON header",
        "records": if h.kind.is_binary() {
            "32-byte binary record header + payload (type 1 data, 2 dropped, 3 status)"
        } else {
            "NDJSON message records"
        },
        "record_header_len": h.record_header_len,
        "max_frame_len": h.max_frame_len,
        "emitter_id": h.emitter_id,
        "bitstream_id": h.bitstream_id,
        "bit_framing": h.framing,
        "audio": h.audio,
        "message_schema": h.message_schema,
    })
}

/// The `/api/streams` discovery document (T-060): every offered stream with its format, every
/// on-demand opener with what it produces, and how to reach both over WebSocket and TCP.
pub fn discovery_json(
    streams: &StreamRegistry,
    openers: &hk_stream::OpenerRegistry,
    tcp: Option<std::net::SocketAddr>,
) -> Value {
    let mut doc = streams.listing();
    let on_demand: Vec<Value> = openers
        .names()
        .into_iter()
        .filter_map(|name| {
            let opener = openers.get(&name)?;
            let mut v = json!({
                "name": name,
                "ws_path": format!("/ws/open/{name}"),
                "tcp_target": format!("open/{name}"),
            });
            if let (Some(o), Value::Object(d)) = (v.as_object_mut(), opener.describe()) {
                for (k, x) in d {
                    o.entry(k).or_insert(x);
                }
            }
            Some(v)
        })
        .collect();
    doc["on_demand"] = Value::Array(on_demand);
    doc["tcp"] = match tcp {
        Some(addr) => json!({
            "addr": addr.to_string(),
            "handshake": "<tcp_target>?token=<token>[&param=value...]\\n",
            "refusal": "one frame {\"type\":\"refused\",\"status\",\"code\",\"reason\"} instead of the header",
        }),
        None => Value::Null,
    };
    doc
}

/// The consumer writer for one browser: re-frames the publisher's length-prefixed byte stream
/// into WebSocket messages (see the [module docs](self)).
pub struct WsSink {
    ws: WebSocket<TcpStream>,
    handshake: Option<Vec<u8>>,
    decoder: FrameDecoder,
    header_sent: bool,
    text_records: bool,
}

impl WsSink {
    /// A sink over an accepted TCP connection whose upgrade request has been read and validated.
    /// `handshake_response` (the `101` response) is written before the first message.
    pub fn new(stream: TcpStream, kind: StreamKind, handshake_response: Vec<u8>) -> Self {
        Self {
            ws: WebSocket::from_raw_socket(stream, Role::Server, None),
            handshake: Some(handshake_response),
            decoder: FrameDecoder::new(HEADER_MAX_LEN),
            header_sent: false,
            text_records: !kind.is_binary(),
        }
    }
}

fn ws_io(e: tungstenite::Error) -> io::Error {
    match e {
        tungstenite::Error::Io(e) => e,
        other => io::Error::other(other),
    }
}

impl Write for WsSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Some(resp) = self.handshake.take() {
            self.ws.get_mut().write_all(&resp)?;
        }
        self.decoder.push(buf);
        let Self {
            ws,
            decoder,
            header_sent,
            text_records,
            ..
        } = self;
        loop {
            let frame = match decoder.next_frame() {
                Ok(Some(frame)) => frame,
                Ok(None) => break,
                Err(e) => return Err(io::Error::new(io::ErrorKind::InvalidData, e)),
            };
            let msg = if !*header_sent || *text_records {
                let text = std::str::from_utf8(frame)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                Message::Text(Utf8Bytes::from(text))
            } else {
                Message::Binary(Bytes::copy_from_slice(frame))
            };
            if !*header_sent {
                // The header bounds every later frame.
                let max = StreamHeader::from_json_bytes(frame)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
                    .max_frame_len;
                *header_sent = true;
                ws.write(msg).map_err(ws_io)?;
                decoder.set_max_frame_len(max);
            } else {
                ws.write(msg).map_err(ws_io)?;
            }
        }
        self.ws.flush().map_err(ws_io)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.ws.flush().map_err(ws_io)
    }
}

/// How long the bridge holds a connection whose publisher finished, waiting for the producer to
/// offer the next one under the same id. A re-plumb tears every reader down and rebuilds them
/// around the still-open device (T-399), and the spectrum reader offers its new header at the
/// first row of the new segment; the control API tells the UI a re-plumb "can take up to 30 s", so
/// this is that with room to spare. After it, the connection ends as it always did.
pub const CARRY_OVER_GRACE: Duration = Duration::from_secs(60);

/// How often the watcher wakes to see whether its consumer was closed. It bounds how much this
/// adds to a settle gap that is already there, and costs one timed-out `read` per consumer per
/// tick — nothing at the single-digit consumer counts a handheld has (§2).
const WATCH_TICK: Duration = Duration::from_millis(50);

/// How long the peer check may block during a carry-over wait (T-425). The wait itself is spent in
/// the registry, so this read is only the peer/drain check and must not add to the re-attach
/// latency; it runs at most once per [`WATCH_TICK`] that passes without an offer.
const GAP_PEER_POLL: Duration = Duration::from_millis(1);

/// The consumer of one bridged connection, and how it ended.
///
/// Held by [`watch_peer`], which needs the close **reason** to tell "the producer replaced this
/// stream's publisher" (carry the connection over) from every other reason (end it).
pub struct Attached {
    id: ConsumerId,
    end: Arc<Mutex<Option<CloseReason>>>,
}

impl Attached {
    /// The consumer id.
    pub fn id(&self) -> ConsumerId {
        self.id
    }

    fn reason(&self) -> Option<CloseReason> {
        *self.end.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// Subscribes one browser connection to `handle` as a **remote** consumer.
///
/// `stream` is the accepted connection after its upgrade request was validated;
/// `handshake_response` is the `101` response to send once admitted (empty on a re-attach, which
/// continues an already-upgraded connection). On success call [`watch_peer`] on the same thread to
/// close the consumer when the browser goes away. On refusal nothing has been written to `stream`;
/// the caller answers with an HTTP status.
pub fn attach(
    handle: &PublisherHandle,
    stream: &TcpStream,
    label: String,
    handshake_response: Vec<u8>,
) -> Result<Attached, StreamError> {
    let sink = WsSink::new(stream.try_clone()?, handle.kind(), handshake_response);
    let closer_stream = stream.try_clone()?;
    let end = Arc::new(Mutex::new(None));
    let closed = Arc::clone(&end);
    let id = handle.subscribe(
        label,
        Declared::remote(sink),
        Box::new(move |reason| {
            *closed.lock().unwrap_or_else(|p| p.into_inner()) = Some(reason);
            // T-417: a finished publisher is not the end of the *connection* — the stream id
            // outlives its publishers and the watcher re-attaches to the next offer on this same
            // socket. Every other reason, the slow-consumer drop included (§7, T-388), shuts it
            // down here exactly as before.
            if reason != CloseReason::PublisherFinished {
                let _ = closer_stream.shutdown(Shutdown::Both);
            }
        }),
    )?;
    Ok(Attached { id, end })
}

/// Blocks until the browser goes away, carrying the connection across publisher changes under the
/// same `stream_id` (see the [module docs](self)), then closes the consumer and the socket.
///
/// `offer` is the offer `attached` was subscribed to; `streams` is where the next one appears.
///
/// The loop reads the socket on a short timeout rather than blocking on it forever. That one read
/// does three jobs: it notices the peer (consumers never write, so anything readable means gone),
/// it notices the server's shutdown drain, which closes every connection's socket — including
/// during a carry-over wait, which must not outlive the server — and it paces the wait. During a
/// carry-over wait the pacing job moves to the registry ([`StreamRegistry::wait_for_offer_after`],
/// T-425) so the re-attach is not quantised to [`WATCH_TICK`]; the read still runs once per tick
/// that passes without an offer, on a [`GAP_PEER_POLL`] timeout, so the other two jobs are
/// unchanged.
pub fn watch_peer(
    streams: &StreamRegistry,
    stream_id: &str,
    mut offer: Offer,
    mut attached: Attached,
    mut stream: TcpStream,
    label: String,
) {
    let _ = stream.set_read_timeout(Some(WATCH_TICK));
    let mut byte = [0u8; 1];
    // Set once this connection's publisher has finished: from then on it is in the settle gap,
    // waiting for the producer's next offer under this id. Nothing is sent in the meantime — the
    // gap is the truth about the front end moving.
    let mut waiting: Option<Instant> = None;
    loop {
        if waiting.is_some() {
            // In the settle gap: nothing arrives on the socket, so wait on the *registry* for the
            // producer's next offer (T-425). Waking on the offer rather than on the next tick is
            // what keeps the new window's first rows — the producer publishes them a row period
            // after it offers, and a consumer that is not subscribed yet is not even told it
            // missed them. Only when that times out is the peer polled (briefly: the socket's read
            // timeout is dropped to `GAP_PEER_POLL` for the gap), so a hang-up mid-gap still ends
            // the connection as it did before.
            if streams
                .wait_for_offer_after(stream_id, offer.generation, WATCH_TICK)
                .is_none()
            {
                match stream.read(&mut byte) {
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                        ) => {}
                    _ => break,
                }
            }
        } else {
            match stream.read(&mut byte) {
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) => {}
                _ => break, // peer data, EOF, the closer's shutdown, or the server draining
            }
            match attached.reason() {
                None => continue,
                Some(CloseReason::PublisherFinished) => {
                    waiting = Some(Instant::now() + CARRY_OVER_GRACE);
                    let _ = stream.set_read_timeout(Some(GAP_PEER_POLL));
                }
                Some(_) => break, // SlowConsumer, PeerGone, DrainTimeout, Detached: it ends here
            }
        }
        match streams.offer(stream_id) {
            // The new publisher's header goes out on the live connection as a text message, and
            // describes every row after it.
            Some(next) if next.generation > offer.generation => {
                match attach(&next.handle, &stream, label.clone(), Vec::new()) {
                    Ok(a) => {
                        offer = next;
                        attached = a;
                        waiting = None;
                        let _ = stream.set_read_timeout(Some(WATCH_TICK));
                    }
                    // T-530: a second re-plumb finished the successor before we reached it. It
                    // says so, so keep waiting on the id from *its* generation instead of
                    // dropping the browser on a gap that is still a gap.
                    Err(StreamError::BetweenWindows) if waiting.is_some() => offer = next,
                    // Local-only, the consumer cap, or really finished: end it honestly.
                    Err(_) => break,
                }
            }
            // The producer withdrew the id (`PipelineConfig::stream_unsink`): this stream is over,
            // not between publishers, so there is nothing to wait for.
            None => break,
            Some(_) if waiting.is_some_and(|d| Instant::now() >= d) => break,
            Some(_) => {}
        }
    }
    offer.handle.close(attached.id);
    let _ = stream.shutdown(Shutdown::Both);
}
