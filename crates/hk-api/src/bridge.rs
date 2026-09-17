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
//! **This is not the slow-consumer policy.** A consumer that cannot keep up is still dropped
//! deliberately (§7, T-388) — [`CloseReason::SlowConsumer`], [`CloseReason::DrainTimeout`],
//! [`CloseReason::PeerGone`] and [`CloseReason::Detached`] all shut the socket down exactly as
//! before. Only "the producer replaced this stream's publisher" is carried.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
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

/// The streams a server offers, by `stream_id`. Registering an id again replaces the entry (e.g.
/// a new publisher per replay pass, or the next window after a retune).
#[derive(Clone, Default)]
pub struct StreamRegistry {
    inner: Arc<RwLock<BTreeMap<String, Entry>>>,
    generation: Arc<AtomicU64>,
}

impl StreamRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Offers `handle` under `header.stream_id`.
    pub fn register(&self, header: &StreamHeader, handle: PublisherHandle) {
        let info = StreamInfo::from_header(header);
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let mut map = self.inner.write().unwrap_or_else(|p| p.into_inner());
        map.insert(
            info.stream_id.clone(),
            Entry {
                info,
                header: header.clone(),
                handle,
                generation,
            },
        );
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

    /// The `/api/streams` JSON: metadata only.
    pub fn listing(&self) -> Value {
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
/// during a carry-over wait, which must not outlive the server — and it paces the wait.
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
        match stream.read(&mut byte) {
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            _ => break, // peer data, EOF, the closer's shutdown, or the server draining
        }
        if waiting.is_none() {
            match attached.reason() {
                None => continue,
                Some(CloseReason::PublisherFinished) => {
                    waiting = Some(Instant::now() + CARRY_OVER_GRACE);
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
                    }
                    // Local-only, the consumer cap, or already finished: end it honestly.
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
