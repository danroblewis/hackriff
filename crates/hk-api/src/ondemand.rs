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
//! 6. **Every ending sends a real close frame (T-954), not a bare TCP hang-up.** [`close_frame_for`]
//!    maps each [`PeerEnd`] to a code and reason before the socket goes down, once the `101`
//!    response and the header actually reached the peer — a session that ended before that has
//!    nothing valid to close on the wire, so it is left as a hang-up as before. Skipping this was
//!    the T-954 bug: every `/ws/open/<name>` session, whatever ended it, read as WebSocket close
//!    code `1006` ("abnormal closure") to the browser, indistinguishable from a real fault.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError};
use std::time::{Duration, Instant};

use hk_stream::{
    ConsumerId, Declared, OpenRefusal, OpenRequest, OpenerRegistry, PublisherHandle, StreamError,
};
use serde_json::json;
use tungstenite::Message;
use tungstenite::protocol::frame::CloseFrame;
use tungstenite::protocol::frame::coding::CloseCode;
use tungstenite::protocol::{Role, WebSocket};

/// How each [`PeerEnd`] closes the WebSocket (T-954): a code and reason, never a bare TCP
/// hang-up. A browser reports an unframed close as `1006` ("abnormal closure"), indistinguishable
/// from a real fault, which is exactly what every `/ws/open/<name>` session did before this: the
/// shared close path (below) only ever shut the raw socket down.
fn close_frame_for(end: PeerEnd) -> (CloseCode, &'static str) {
    match end {
        // A close frame from the peer, or a producer that finished and dropped the session: a
        // normal, expected end.
        PeerEnd::Closed => (CloseCode::Normal, "closed"),
        // Consumers never write (§2): unsolicited data breaks the contract.
        PeerEnd::Message => (CloseCode::Protocol, "unexpected message from a consumer"),
        // No pong within the peer timeout: half-open or vanished, not a graceful close, but still
        // told with a real frame rather than left to read as 1006.
        PeerEnd::Unresponsive => (CloseCode::Normal, "no response within the peer timeout"),
        // The transport itself faulted (reset, broken pipe): sending is best-effort and usually a
        // no-op, but costs nothing to attempt.
        PeerEnd::Reset => (CloseCode::Error, "transport reset"),
    }
}

use crate::bridge;
use crate::http::ServerConfig;

/// Longest close reason (the WebSocket limit is 123 bytes).
const MAX_CLOSE_REASON: usize = 120;

/// Write timeout for every close-frame attempt (T-954, review attempt 3). A close frame is a few
/// bytes: to a peer that is reading it goes out at once, and a peer whose send buffer is full is
/// not reading, so the full peer timeout the socket otherwise carries would only hold the session
/// (its guard, producer chain and budget slot) that much longer. The socket is shut down right
/// after every close attempt, so shortening its timeout here cannot fail a later record write.
const CLOSE_WRITE_TIMEOUT: Duration = Duration::from_millis(200);

/// Longest a closer waits for `conn`'s lock before giving up on the close frame (T-954, review
/// attempt 3). A writer mid-write to a live peer releases it within milliseconds; one that still
/// holds it after this is stuck on a peer that is not reading, which would not read the frame
/// either — and only the raw shutdown that follows can unblock it.
const CLOSE_LOCK_WAIT: Duration = Duration::from_millis(200);

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
    /// A close frame was already written (T-954): the producer's subscribe closer and `serve`'s
    /// own end-of-session code both reach for this, from different threads, on whichever end
    /// happens first — see [`Conn::send_close`].
    close_sent: bool,
}

impl Conn {
    /// Sends the close frame at most once, and only once the handshake actually went out
    /// (`started`): whichever of the subscribe closer (a producer-side close, T-954) or `serve`'s
    /// own `watch`-driven end reaches this first, through the same lock, wins; the other is a
    /// no-op here (the socket still gets shut down either way, by its own caller).
    fn send_close(&mut self, code: CloseCode, reason: &str) {
        if self.started && !self.close_sent {
            self.sink.set_write_timeout(CLOSE_WRITE_TIMEOUT);
            self.sink.close(code, reason);
            self.close_sent = true;
        }
    }
}

/// `conn`'s lock if it can be had within `wait` (`Duration::ZERO`: only if free right now).
/// **Never `conn.lock()` on a close path (T-954, review attempt 3):** [`ConnSink::write`] holds
/// that lock for a whole socket write, whose timeout is the full peer timeout, so a closer that
/// blocks on it waits out a write stuck on a vanished peer — the one the close is meant to cut
/// short.
fn lock_within(conn: &Mutex<Conn>, wait: Duration) -> Option<MutexGuard<'_, Conn>> {
    let deadline = Instant::now() + wait;
    loop {
        match conn.try_lock() {
            Ok(g) => return Some(g),
            Err(TryLockError::Poisoned(p)) => return Some(p.into_inner()),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(TryLockError::WouldBlock) => return None,
        }
    }
}

/// Sends the close frame if `conn` can be had within `wait` (see [`lock_within`]); otherwise skips
/// it — a writer that has held the lock that long is stuck on a peer that is not reading.
fn send_close_within(conn: &Mutex<Conn>, wait: Duration, code: CloseCode, reason: &str) {
    if let Some(mut c) = lock_within(conn, wait) {
        c.send_close(code, reason);
    }
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

/// How each producer-side [`hk_stream::CloseReason`] closes the WebSocket (T-954 follow-up): the
/// producer can drop this consumer from its own thread — finished, too slow, not drained in
/// time — with `serve`'s `watch` loop not yet aware anything happened, so the subscribe closer in
/// [`attach`] is the only place that can reliably send the frame before the raw shutdown that
/// same closer performs. Without this, that shutdown always won the race against `serve`'s own
/// close-frame write once `watch` woke up on the resulting EOF, and every producer-initiated end
/// still read as `1006`.
fn close_frame_for_reason(reason: hk_stream::CloseReason) -> (CloseCode, &'static str) {
    use hk_stream::CloseReason;
    match reason {
        CloseReason::PublisherFinished => (CloseCode::Normal, "producer finished"),
        CloseReason::SlowConsumer => (CloseCode::Policy, "too slow to keep up"),
        CloseReason::DrainTimeout => (CloseCode::Policy, "did not drain in time"),
        CloseReason::PeerGone | CloseReason::Detached => (CloseCode::Normal, "closed"),
    }
}

/// Subscribes the connection as a remote consumer, like [`bridge::attach`], through a writer
/// [`watch`] can ping between messages.
///
/// **The subscribe closer must never block on a stuck write (T-954, review attempts 2 and 3).**
/// hk-stream's own contract for it (`publisher.rs`) is to unblock a writer thread that may be
/// stuck inside [`ConnSink::write`] holding `conn`'s lock, by shutting the raw socket down — so no
/// reason may wait on that lock unboundedly, and what each one may afford differs:
/// - `SlowConsumer` fires straight out of `Publisher::publish_binary`/`publish_status`, on the
///   *producer's own real-time thread* ("the producer never waits on a browser", [`bridge`]'s
///   module docs), and `DrainTimeout` from a watchdog thread shared by every draining consumer:
///   they never touch the lock at all, only shut down.
/// - `PeerGone` fires after the writer's own write already failed (timed out on a peer that is
///   not reading, or cut short by a shutdown), and `Detached` is `serve`'s own close, after it has
///   already made its own close-frame attempt: both try the lock once, without waiting.
/// - `PublisherFinished` fires from the writer thread once its last write returned, so the lock is
///   normally free; it may briefly wait ([`CLOSE_LOCK_WAIT`]) behind a ping or `serve`'s own
///   close attempt, so a normal end is not left to read as `1006` over that race.
///
/// Every close-frame write runs with the short [`CLOSE_WRITE_TIMEOUT`], never the peer timeout.
fn attach(
    handle: &PublisherHandle,
    stream: &TcpStream,
    label: String,
    handshake: Vec<u8>,
) -> Result<(ConsumerId, Arc<Mutex<Conn>>), StreamError> {
    let conn = Arc::new(Mutex::new(Conn {
        sink: bridge::WsSink::new(stream.try_clone()?, handle.kind(), handshake),
        started: false,
        close_sent: false,
    }));
    let closer = stream.try_clone()?;
    let conn_for_closer = Arc::clone(&conn);
    let id = handle.subscribe(
        label,
        Declared::remote(ConnSink(Arc::clone(&conn))),
        Box::new(move |reason| on_close(reason, &conn_for_closer, &closer)),
    )?;
    Ok((id, conn))
}

/// The subscribe closer's actual work, factored out of [`attach`] so review attempt 2's exact
/// concern — `SlowConsumer`/`DrainTimeout` must never wait on `conn`'s lock — has a direct,
/// deterministic test (below) instead of one that hopes to stall a real TCP write.
fn on_close(reason: hk_stream::CloseReason, conn: &Mutex<Conn>, closer: &TcpStream) {
    use hk_stream::CloseReason;
    let wait = match reason {
        CloseReason::SlowConsumer | CloseReason::DrainTimeout => None,
        CloseReason::PeerGone | CloseReason::Detached => Some(Duration::ZERO),
        CloseReason::PublisherFinished => Some(CLOSE_LOCK_WAIT),
    };
    if let Some(wait) = wait {
        let (code, msg) = close_frame_for_reason(reason);
        send_close_within(conn, wait, code, msg);
    }
    let _ = closer.shutdown(Shutdown::Both);
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
            // T-633: tell the producer how the session ended BEFORE its guard is dropped, so a
            // peer this server reaped for silence, or a connection that faulted, is not counted
            // as the client going away.
            opened.end.set(match end {
                PeerEnd::Closed | PeerEnd::Message => hk_stream::SessionEnd::Client,
                PeerEnd::Unresponsive => hk_stream::SessionEnd::Unresponsive,
                PeerEnd::Reset => hk_stream::SessionEnd::Transport,
            });
            // T-954: an honest close frame, not a bare TCP hang-up — see [`close_frame_for`] —
            // for the reason `watch` actually observed, unless the producer already closed this
            // consumer with its own (see [`Conn::send_close`]). `watch` returning does NOT mean
            // `conn` is free (review attempt 3): the writer thread may be stuck in a socket write
            // to a vanished peer, holding it for up to the full peer timeout. So wait for it only
            // briefly and skip the frame if it stays busy (that peer is not reading); the closer
            // that `handle.close` runs then shuts the socket down, which is what cuts that write
            // short, instead of this thread waiting it out.
            let (code, reason) = close_frame_for(end);
            send_close_within(&conn, CLOSE_LOCK_WAIT, code, reason);
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

    fn loopback_conn() -> (Arc<Mutex<Conn>>, TcpStream, TcpStream) {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        let closer = server.try_clone().unwrap();
        let conn = Arc::new(Mutex::new(Conn {
            sink: bridge::WsSink::new(
                server.try_clone().unwrap(),
                hk_stream::StreamKind::Audio,
                Vec::new(),
            ),
            started: true,
            close_sent: false,
        }));
        (conn, closer, client)
    }

    /// T-954, review attempts 2 and 3: the regression itself, made deterministic instead of
    /// depending on actually stalling a TCP write. `conn`'s lock stands in for a writer thread
    /// stuck mid-write holding it (for 1 s here, a peer timeout in miniature). No closer may wait
    /// that out: `SlowConsumer`/`DrainTimeout` (the producer's own real-time thread, the drain
    /// watchdog) and `PeerGone`/`Detached` never wait at all, and `PublisherFinished` waits at
    /// most [`CLOSE_LOCK_WAIT`]. Each still shuts the socket down, which is what frees the writer.
    #[test]
    fn no_closer_waits_out_a_writer_stuck_holding_conns_lock() {
        use hk_stream::CloseReason;
        let no_wait = Duration::from_millis(100);
        for (reason, bound) in [
            (CloseReason::SlowConsumer, no_wait),
            (CloseReason::DrainTimeout, no_wait),
            (CloseReason::PeerGone, no_wait),
            (CloseReason::Detached, no_wait),
            (CloseReason::PublisherFinished, CLOSE_LOCK_WAIT + no_wait),
        ] {
            let (conn, closer, client) = loopback_conn();
            let held = Arc::clone(&conn);
            let (tx, rx) = std::sync::mpsc::channel();
            let holder = std::thread::spawn(move || {
                let _g = held.lock().unwrap_or_else(PoisonError::into_inner);
                tx.send(()).unwrap();
                std::thread::sleep(Duration::from_secs(1));
            });
            rx.recv().unwrap(); // the lock is now held, simulating a stuck writer_loop write.

            let t0 = Instant::now();
            on_close(reason, &conn, &closer);
            let elapsed = t0.elapsed();
            assert!(
                elapsed < bound,
                "{reason:?} waited {elapsed:?} (bound {bound:?}) on a lock a stuck writer holds"
            );
            let mut probe = [0u8; 1];
            let _ = client.set_read_timeout(Some(Duration::from_secs(1)));
            assert!(
                matches!((&client).read(&mut probe), Ok(0) | Err(_)),
                "{reason:?} must still shut the socket down"
            );

            holder.join().unwrap();
        }
    }

    /// With `conn`'s lock free — the normal case for these three (see [`attach`]'s doc comment) —
    /// they close with a real frame, not just a hang-up.
    #[test]
    fn the_other_reasons_still_send_a_close_frame() {
        for reason in [
            hk_stream::CloseReason::PublisherFinished,
            hk_stream::CloseReason::PeerGone,
            hk_stream::CloseReason::Detached,
        ] {
            let (conn, closer, client) = loopback_conn();
            on_close(reason, &conn, &closer);
            let mut ws = WebSocket::from_raw_socket(client, Role::Client, None);
            let code = loop {
                match ws.read() {
                    Ok(Message::Close(f)) => break f.map(|f| u16::from(f.code)),
                    Ok(_) => {}
                    Err(_) => break None,
                }
            };
            assert!(code.is_some(), "{reason:?} must still send a close frame");
        }
    }
}
