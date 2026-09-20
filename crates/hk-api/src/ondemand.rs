//! `/ws/open/<name>`: on-demand streams over WebSocket (T-043 Listen; the transport T-060 reuses
//! for bits, symbols and audio). Thin: the contract — framing, header, sequence numbers, drop
//! markers, egress gating and the per-consumer drop-not-block queue — is hk-stream's
//! ([`hk_stream::ondemand`], [`hk_stream::Publisher`]); this file only maps it onto a socket.
//!
//! # Flow
//! 1. `http.rs` has already checked the token (`401` before anything else).
//! 2. The upgrade request is validated (`426`/`400`) and the opener looked up by `<name>` (`404`).
//! 3. [`StreamOpener::open`](hk_stream::StreamOpener::open) with the query (token removed). A
//!    **refusal** (legal gate, capacity, bad request...) completes the upgrade, sends one text
//!    message `{"type":"refused","status":..,"code":..,"reason":..,"content_class":..}` and closes
//!    with code `4000 + status` (browsers cannot read an HTTP error body on a failed upgrade).
//!    Nothing was attached.
//! 4. Otherwise the connection is subscribed through [`bridge::attach`] as a **remote** consumer,
//!    exactly like `/ws/<stream_id>`: header as the first text message, one binary message per
//!    record. A slow browser only fills its own queue; records are dropped with markers, the
//!    producer never waits.
//! 5. **Liveness and teardown (T-066).** The server pings the peer every
//!    [`ServerConfig::ondemand_ping_interval`] (browsers and WebSocket libraries answer with a pong
//!    on their own) and watches what comes back ([`watch`]). A close frame, any data message, a
//!    hang-up or reset, or **silence for [`ServerConfig::ondemand_peer_timeout`]** (a half-open
//!    connection, a tunnel whose browser vanished) closes the consumer and drops the session
//!    guard, which stops the producer. A write blocked for the peer timeout fails the consumer
//!    too. A producer that ends on its own finishes its publisher, which closes the socket.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::{Arc, Mutex, PoisonError, TryLockError};
use std::time::{Duration, Instant};

use hk_stream::{
    ConsumerId, Declared, OpenRefusal, OpenRequest, OpenerRegistry, PublisherHandle, StreamError,
};
use serde_json::json;
use tungstenite::Message;
use tungstenite::protocol::frame::CloseFrame;
use tungstenite::protocol::frame::coding::CloseCode;
use tungstenite::protocol::{Role, WebSocket};

use crate::bridge;
use crate::http::ServerConfig;

/// Longest close reason (the WebSocket limit is 123 bytes).
const MAX_CLOSE_REASON: usize = 120;

fn http_error(stream: &mut TcpStream, status: u16, message: &str) {
    let body = json!({ "error": message }).to_string();
    let head = format!(
        "HTTP/1.1 {status} Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\nCache-Control: no-store\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body.as_bytes());
    let _ = stream.flush();
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Completes the upgrade, sends the refusal and closes with `4000 + status`.
fn refuse(mut stream: TcpStream, handshake: &[u8], refusal: &OpenRefusal) {
    if stream.write_all(handshake).is_err() {
        return;
    }
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let mut ws = WebSocket::from_raw_socket(stream, Role::Server, None);
    let _ = ws.send(Message::Text(refusal.to_json().to_string().into()));
    let _ = ws.close(Some(CloseFrame {
        code: CloseCode::from(refusal.close_code()),
        reason: truncate(&refusal.reason, MAX_CLOSE_REASON)
            .to_owned()
            .into(),
    }));
    // Wait briefly for the peer's close acknowledgement, then drop the connection.
    while ws.read().is_ok() {}
    let _ = ws.get_mut().shutdown(Shutdown::Both);
}

pub(crate) fn attach_refusal(e: &StreamError) -> OpenRefusal {
    match e {
        StreamError::LocalOnly { class } => OpenRefusal::gated(
            *class,
            "stream is local-only (own-key-decrypted); never served over WebSocket",
        ),
        StreamError::TooManyConsumers { max } => {
            OpenRefusal::new(503, "busy", format!("consumer limit {max} reached"))
        }
        StreamError::Finished => OpenRefusal::new(410, "finished", "stream finished"),
        // T-530: the producer said a successor is coming under this id. The same answer a chain
        // gives for a request that arrives mid-re-plumb (`chains::listen`, `chains::iq`): try
        // again, not "it is gone". Unlike `/ws/{stream_id}` there is no registry offer to wait on
        // here — the caller already holds the handle it resolved — so it is refused, not held.
        StreamError::BetweenWindows => OpenRefusal::new(
            503,
            "replumbing",
            "the stream is moving to a new window; try again",
        ),
        _ => OpenRefusal::new(500, "subscribe", "subscription failed"),
    }
}

/// The consumer's socket writer, shared with [`watch`] so its pings never land inside a message.
struct Conn {
    sink: bridge::WsSink,
    /// The `101` response and the first messages went out: pings may follow.
    started: bool,
}

struct ConnSink(Arc<Mutex<Conn>>);

impl Write for ConnSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut c = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let n = c.sink.write(buf)?;
        c.started = true;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .sink
            .flush()
    }
}

/// Subscribes the connection as a remote consumer, like [`bridge::attach`], through a writer
/// [`watch`] can ping between messages.
fn attach(
    handle: &PublisherHandle,
    stream: &TcpStream,
    label: String,
    handshake: Vec<u8>,
) -> Result<(ConsumerId, Arc<Mutex<Conn>>), StreamError> {
    let conn = Arc::new(Mutex::new(Conn {
        sink: bridge::WsSink::new(stream.try_clone()?, handle.kind(), handshake),
        started: false,
    }));
    let closer = stream.try_clone()?;
    let id = handle.subscribe(
        label,
        Declared::remote(ConnSink(Arc::clone(&conn))),
        Box::new(move |_reason| {
            let _ = closer.shutdown(Shutdown::Both);
        }),
    )?;
    Ok((id, conn))
}

/// How the peer went away.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PeerEnd {
    /// A close frame or a hang-up (also our own shutdown once the producer finished).
    Closed,
    /// A data message: consumers never write (§2), so it ends the session.
    Message,
    /// A reset or read/write error.
    Reset,
    /// Nothing, not even a pong, for the peer timeout (half-open or vanished peer).
    Unresponsive,
}

/// Server ping, no payload, unmasked (RFC 6455 §5.5.2).
const PING: [u8; 2] = [0x89, 0x00];

/// Consumes whole ping/pong frames at the start of `buf`. A close frame or any other frame ends
/// the session.
fn control_frames(buf: &mut Vec<u8>) -> Result<(), PeerEnd> {
    while let Some(&b0) = buf.first() {
        match b0 & 0x0f {
            0x9 | 0xa => {}
            0x8 => return Err(PeerEnd::Closed),
            _ => return Err(PeerEnd::Message),
        }
        let Some(&b1) = buf.get(1) else {
            return Ok(());
        };
        let len = usize::from(b1 & 0x7f);
        if len > 125 {
            // Control frames carry at most 125 bytes (RFC 6455 §5.5).
            return Err(PeerEnd::Message);
        }
        let total = 2 + if b1 & 0x80 != 0 { 4 } else { 0 } + len;
        if buf.len() < total {
            return Ok(());
        }
        buf.drain(..total);
    }
    Ok(())
}

/// Turns TCP keepalive on (first probe after `idle`, then every `idle / 4`, 4 misses) so a peer
/// that vanished without a FIN or RST errors a blocked read instead of holding an on-demand
/// session forever. For transports without an application-level ping (the TCP stream server);
/// the WebSocket front end also pings. Best effort.
pub(crate) fn keepalive(stream: &TcpStream, idle: Duration) {
    use std::os::fd::AsRawFd;
    let fd = stream.as_raw_fd();
    let secs = libc::c_int::try_from(idle.as_secs().clamp(1, 7200)).unwrap_or(7200);
    let set = |level: libc::c_int, name: libc::c_int, value: libc::c_int| {
        // SAFETY: `fd` is an open socket owned by `stream` for this call; `value` outlives it
        // and the length matches its type.
        let _ = unsafe {
            libc::setsockopt(
                fd,
                level,
                name,
                (&raw const value).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
    };
    set(libc::SOL_SOCKET, libc::SO_KEEPALIVE, 1);
    #[cfg(target_os = "macos")]
    set(libc::IPPROTO_TCP, libc::TCP_KEEPALIVE, secs);
    #[cfg(target_os = "linux")]
    set(libc::IPPROTO_TCP, libc::TCP_KEEPIDLE, secs);
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        set(libc::IPPROTO_TCP, libc::TCP_KEEPINTVL, (secs / 4).max(1));
        set(libc::IPPROTO_TCP, libc::TCP_KEEPCNT, 4);
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let _ = secs;
}

/// Pings the peer and blocks until it goes away (see the module docs).
fn watch(stream: &mut TcpStream, conn: &Mutex<Conn>, config: &ServerConfig) -> PeerEnd {
    let interval = config.ondemand_ping_interval.max(Duration::from_millis(1));
    let timeout = config.ondemand_peer_timeout.max(Duration::from_millis(1));
    let tick = interval.min(timeout / 2).max(Duration::from_millis(5));
    let _ = stream.set_read_timeout(Some(tick));
    let mut buf = [0u8; 512];
    let mut pending = Vec::new();
    let mut heard = Instant::now();
    let mut pinged = Instant::now();
    loop {
        match stream.read(&mut buf) {
            Ok(0) => return PeerEnd::Closed,
            Ok(n) => {
                heard = Instant::now();
                pending.extend_from_slice(&buf[..n]);
                if let Err(end) = control_frames(&mut pending) {
                    return end;
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(_) => return PeerEnd::Reset,
        }
        if heard.elapsed() > timeout {
            return PeerEnd::Unresponsive;
        }
        if pinged.elapsed() >= interval {
            let guard = match conn.try_lock() {
                Ok(g) => Some(g),
                Err(TryLockError::Poisoned(p)) => Some(p.into_inner()),
                // A message is being written: ping at the next tick.
                Err(TryLockError::WouldBlock) => None,
            };
            if let Some(g) = guard
                && g.started
            {
                pinged = Instant::now();
                if stream.write_all(&PING).is_err() {
                    return PeerEnd::Reset;
                }
            }
        }
    }
}

/// Serves one `/ws/open/<name>` request (token already verified).
pub(crate) fn serve(
    mut stream: TcpStream,
    openers: &OpenerRegistry,
    name: &str,
    query: &[(String, String)],
    headers: &[(String, String)],
    config: &ServerConfig,
) {
    let header = |n: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(n))
            .map(|(_, v)| v.trim())
    };
    let has = |n: &str, want: &str| {
        header(n).is_some_and(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case(want)))
    };
    if !has("upgrade", "websocket") || !has("connection", "upgrade") {
        return http_error(&mut stream, 426, "WebSocket upgrade required");
    }
    if header("sec-websocket-version") != Some("13") {
        return http_error(&mut stream, 426, "WebSocket version 13 required");
    }
    let Some(key) = header("sec-websocket-key") else {
        return http_error(&mut stream, 400, "missing Sec-WebSocket-Key");
    };
    let Some(opener) = openers.get(name) else {
        return http_error(&mut stream, 404, "no such on-demand stream");
    };
    let handshake = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Accept: {}\r\n\r\n",
        tungstenite::handshake::derive_accept_key(key.as_bytes())
    )
    .into_bytes();
    let peer = stream
        .peer_addr()
        .map_or_else(|_| "unknown".into(), |a| a.to_string());
    let request = OpenRequest::from_query(query, format!("ws:{peer}"));
    let opened = match opener.open(&request) {
        Ok(o) => o,
        Err(refusal) => return refuse(stream, &handshake, &refusal),
    };
    keepalive(&stream, config.ondemand_peer_timeout);
    // A write blocked this long means the peer is gone: the consumer fails and closes.
    let _ = stream.set_write_timeout(Some(
        config.ondemand_peer_timeout.max(Duration::from_millis(1)),
    ));
    match attach(
        &opened.handle,
        &stream,
        format!("open/{name}:{peer}"),
        handshake.clone(),
    ) {
        Ok((id, conn)) => {
            let end = watch(&mut stream, &conn, config);
            opened.handle.close(id);
            let _ = stream.shutdown(Shutdown::Both);
            if std::env::var_os("HK_PIPELINE_DEBUG").is_some() {
                eprintln!("hk-api: /ws/open/{name} from {peer} ended: {end:?}");
            }
        }
        Err(e) => refuse(stream, &handshake, &attach_refusal(&e)),
    }
    // Dropping the session guard stops the producer.
    drop(opened);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pings_and_pongs_are_consumed_close_and_data_end_the_session() {
        // Masked client pong with a 2-byte payload, then half of a ping header.
        let mut buf = vec![0x8a, 0x82, 1, 2, 3, 4, 9, 9, 0x89];
        assert_eq!(control_frames(&mut buf), Ok(()));
        assert_eq!(buf, vec![0x89], "a partial frame waits for more bytes");
        buf.extend_from_slice(&[0x80, 0, 0, 0, 0]);
        assert_eq!(control_frames(&mut buf), Ok(()));
        assert!(buf.is_empty());
        assert_eq!(
            control_frames(&mut vec![0x88, 0x80, 0, 0, 0, 0]),
            Err(PeerEnd::Closed)
        );
        assert_eq!(control_frames(&mut vec![0x81]), Err(PeerEnd::Message));
        assert_eq!(control_frames(&mut vec![0x8a, 0x7e]), Err(PeerEnd::Message));
    }
}
