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
//! 5. When the browser sends anything or hangs up ([`bridge::watch_peer`]) the session guard is
//!    dropped, which stops the producer. A producer that ends on its own finishes its publisher,
//!    which closes the socket.

use std::io::Write;
use std::net::{Shutdown, TcpStream};
use std::time::Duration;

use hk_stream::{OpenRefusal, OpenRequest, OpenerRegistry, StreamError};
use serde_json::json;
use tungstenite::Message;
use tungstenite::protocol::frame::CloseFrame;
use tungstenite::protocol::frame::coding::CloseCode;
use tungstenite::protocol::{Role, WebSocket};

use crate::bridge;

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
        _ => OpenRefusal::new(500, "subscribe", "subscription failed"),
    }
}

/// Serves one `/ws/open/<name>` request (token already verified).
pub(crate) fn serve(
    mut stream: TcpStream,
    openers: &OpenerRegistry,
    name: &str,
    query: &[(String, String)],
    headers: &[(String, String)],
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
    let _ = stream.set_write_timeout(None);
    match bridge::attach(
        &opened.handle,
        &stream,
        format!("open/{name}:{peer}"),
        handshake.clone(),
    ) {
        Ok(id) => bridge::watch_peer(&opened.handle, stream, id),
        Err(e) => refuse(stream, &handshake, &attach_refusal(&e)),
    }
    // Dropping the session guard stops the producer.
    drop(opened);
}
