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

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::{Arc, RwLock};

use hk_model::ContentClass;
use hk_stream::{
    ConsumerId, Declared, FrameDecoder, HEADER_MAX_LEN, PublisherHandle, StreamError, StreamHeader,
    StreamKind,
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
        }
    }
}

struct Entry {
    info: StreamInfo,
    handle: PublisherHandle,
}

/// The streams a server offers, by `stream_id`. Registering an id again replaces the entry (e.g.
/// a new publisher per replay pass).
#[derive(Clone, Default)]
pub struct StreamRegistry {
    inner: Arc<RwLock<BTreeMap<String, Entry>>>,
}

impl StreamRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Offers `handle` under `header.stream_id`.
    pub fn register(&self, header: &StreamHeader, handle: PublisherHandle) {
        let info = StreamInfo::from_header(header);
        let mut map = self.inner.write().unwrap_or_else(|p| p.into_inner());
        map.insert(info.stream_id.clone(), Entry { info, handle });
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
                    "open_consumers": e.handle.open_consumers(),
                    "ws_path": format!("/ws/{}", i.stream_id),
                })
            })
            .collect();
        json!({ "streams": streams })
    }
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

/// Subscribes one browser connection to `handle` as a **remote** consumer.
///
/// `stream` is the accepted connection after its upgrade request was validated;
/// `handshake_response` is the `101` response to send once admitted. On success the returned id is
/// the consumer; call [`watch_peer`] on the same thread to close it when the browser goes away. On
/// refusal nothing has been written to `stream`; the caller answers with an HTTP status.
pub fn attach(
    handle: &PublisherHandle,
    stream: &TcpStream,
    label: String,
    handshake_response: Vec<u8>,
) -> Result<ConsumerId, StreamError> {
    let sink = WsSink::new(stream.try_clone()?, handle.kind(), handshake_response);
    let closer_stream = stream.try_clone()?;
    handle.subscribe(
        label,
        Declared::remote(sink),
        Box::new(move |_reason| {
            let _ = closer_stream.shutdown(Shutdown::Both);
        }),
    )
}

/// Blocks until the browser sends anything or hangs up (or the consumer is closed, which shuts
/// the socket down), then closes consumer `id`. Consumers never write back (§2).
pub fn watch_peer(handle: &PublisherHandle, mut stream: TcpStream, id: ConsumerId) {
    let _ = stream.set_read_timeout(None);
    let mut byte = [0u8; 1];
    let _ = stream.read(&mut byte);
    handle.close(id);
    let _ = stream.shutdown(Shutdown::Both);
}
