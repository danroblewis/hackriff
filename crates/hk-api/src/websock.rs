//! The server side of a **self-served** WebSocket route: the upgrade handshake and the small
//! send/close/liveness helpers every one of them needs.
//!
//! Most `/ws/*` routes are bridged streams ([`crate::bridge`]) or on-demand openers
//! ([`crate::ondemand`]), which share one implementation each. Two routes are neither — they are
//! answered by this crate directly, from server state, on the connection's own thread:
//! [`crate::rows`] (`/ws/tiles/rows`, T-468) and [`crate::changes`] (`/ws/changes`, T-1065). This
//! module exists so the handshake is written **once**: a second hand-rolled `101 Switching
//! Protocols` is exactly the two-implementations drift the project keeps paying for, and this one
//! carries the refusal codes a browser depends on.
//!
//! # Conventions these helpers encode
//!
//! - **A refusal before the upgrade is plain HTTP** ([`http_error`]): `426` when the request is not
//!   a version-13 WebSocket upgrade, `400` without a key. Those are conditions a *client library*
//!   reports, so the body is readable.
//! - **A refusal after the upgrade is a message plus `4000 + status`** ([`refuse`]) — the
//!   `/ws/open/{name}` convention, because a browser cannot read an HTTP error body on a failed
//!   upgrade.
//! - **A consumer never sends data.** [`peer_alive`] reads only to answer pings and to notice a
//!   close; anything else is housekeeping and is ignored.

use std::io::{self, Write};
use std::net::{Shutdown, TcpStream};
use std::time::Duration;

use serde_json::{Value, json};
use tungstenite::Message;
use tungstenite::protocol::frame::CloseFrame;
use tungstenite::protocol::frame::coding::CloseCode;
use tungstenite::protocol::{Role, WebSocket};

use crate::query::ApiError;

/// How long a self-served socket waits on one write before giving up on the peer.
pub(crate) const WRITE_TIMEOUT: Duration = Duration::from_secs(20);

/// A JSON error answered **without** upgrading (the request was not a usable WebSocket upgrade).
pub(crate) fn http_error(stream: &mut TcpStream, status: u16, message: &str) {
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

/// Completes the handshake and hands back the server-role socket, or answers the plain-HTTP
/// refusal itself and returns `None`. The token is verified by the caller, before this.
pub(crate) fn upgrade(
    mut stream: TcpStream,
    headers: &[(String, String)],
) -> Option<WebSocket<TcpStream>> {
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
        http_error(&mut stream, 426, "WebSocket upgrade required");
        return None;
    }
    if header("sec-websocket-version") != Some("13") {
        http_error(&mut stream, 426, "WebSocket version 13 required");
        return None;
    }
    let Some(key) = header("sec-websocket-key") else {
        http_error(&mut stream, 400, "missing Sec-WebSocket-Key");
        return None;
    };
    let handshake = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Accept: {}\r\n\r\n",
        tungstenite::handshake::derive_accept_key(key.as_bytes())
    );
    if stream.write_all(handshake.as_bytes()).is_err() {
        return None;
    }
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    Some(WebSocket::from_raw_socket(stream, Role::Server, None))
}

/// Sends a close frame with `code` and `reason`, drains what the peer still says, and shuts down.
pub(crate) fn close(ws: &mut WebSocket<TcpStream>, code: u16, reason: &str) {
    let mut end = reason.len().min(120);
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    let _ = ws.close(Some(CloseFrame {
        code: CloseCode::from(code),
        reason: reason[..end].to_owned().into(),
    }));
    let _ = ws.get_mut().set_read_timeout(Some(Duration::from_secs(2)));
    while ws.read().is_ok() {}
    let _ = ws.get_mut().shutdown(Shutdown::Both);
}

/// Sends `{"type":"refused",…}` and closes with `4000 + status` — the `/ws/open/{name}`
/// convention, because a browser cannot read an HTTP error body on a failed upgrade.
pub(crate) fn refuse(mut ws: WebSocket<TcpStream>, e: &ApiError) {
    let _ = ws.send(Message::Text(
        json!({ "type": "refused", "status": e.status, "reason": e.message })
            .to_string()
            .into(),
    ));
    close(&mut ws, 4000 + e.status, &e.message);
}

/// One JSON text message; `false` once the socket will take no more.
pub(crate) fn send(ws: &mut WebSocket<TcpStream>, v: &Value) -> bool {
    ws.send(Message::Text(v.to_string().into())).is_ok()
}

/// `true` while the peer is still there. Reads (and so answers pings) for at most `wait`.
pub(crate) fn peer_alive(ws: &mut WebSocket<TcpStream>, wait: Duration) -> bool {
    let _ = ws.get_mut().set_read_timeout(Some(wait));
    match ws.read() {
        Ok(Message::Close(_)) => false,
        // A subscriber never sends data; anything else (a ping, a pong) is housekeeping.
        Ok(_) => true,
        Err(tungstenite::Error::Io(e))
            if matches!(
                e.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
            ) =>
        {
            true
        }
        Err(_) => false,
    }
}

/// An unsolicited ping, so an **idle** feed keeps a proxy's connection open without sending a
/// message. A feed whose whole promise is "nothing while nothing changed" cannot use a JSON
/// heartbeat for this: the heartbeat would *be* the traffic the feed exists to remove.
pub(crate) fn ping(ws: &mut WebSocket<TcpStream>) -> bool {
    ws.send(Message::Ping(Vec::new().into())).is_ok()
}

/// Shuts the socket down without a close frame (the peer is already gone).
pub(crate) fn drop_socket(ws: &mut WebSocket<TcpStream>) {
    let _ = ws.get_mut().shutdown(Shutdown::Both);
}
