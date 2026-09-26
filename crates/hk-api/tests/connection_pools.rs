//! T-1063 connection caps: the server **answers** at a cap, and WebSockets have their own pool.
//!
//! The defect these pin: at `max_connections` the accept loop did `continue` — no response, no
//! log, no counter — so a client behind the cloudflared tunnel saw only a closed socket and the
//! tunnel logged "Unable to reach the origin service: EOF" (8 622 in 15 minutes, 2026-09-26). The
//! same 64 slots were shared with a tab's 17–19 long-lived WebSockets, so the stream sockets could
//! exhaust the slots the HTTP requests drawn beside them needed.
//!
//! Three facts are asserted here, all through the real socket rather than through internals:
//! a connection past the HTTP cap gets `503` with `Retry-After` (not EOF) and is counted and
//! named; a WebSocket upgrade succeeds past the HTTP cap while its own pool has room; and the
//! WebSocket pool's own cap answers the same way rather than dropping.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use hk_api::stream::{Publisher, PublisherConfig, StreamHeader, StreamKind};
use hk_api::{ApiState, Server, ServerConfig, StreamRegistry, Token};
use hk_model::ContentClass;
use serde_json::Value;
use tungstenite::WebSocket;
use tungstenite::stream::MaybeTlsStream;

const TOKEN: &str = "t1063-connection-pool-token-0123";
type Ws = WebSocket<MaybeTlsStream<TcpStream>>;

fn serve(http_cap: usize, ws_cap: usize, state: ApiState) -> Server {
    let mut config = ServerConfig::new(
        "127.0.0.1:0".parse().unwrap(),
        Token::from_config(TOKEN).unwrap(),
    );
    config.max_connections = http_cap;
    config.max_ws_connections = ws_cap;
    // Long enough that a holder which sends a partial head keeps its slot for the whole test,
    // short enough that a failing test still ends.
    config.request_timeout = Duration::from_secs(10);
    Server::start(config, state).unwrap()
}

/// One raw HTTP response, read to EOF (the server sends `Connection: close`): `(status, head,
/// body)`. Reading to EOF is what makes this reliable under load — the head and the body may
/// arrive in any number of TCP segments.
fn raw_get(addr: SocketAddr, path: &str) -> (u16, String, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: test\r\nAuthorization: Bearer {TOKEN}\r\n\
         Connection: close\r\n\r\n"
    )
    .unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text
        .split_once("\r\n\r\n")
        .unwrap_or_else(|| panic!("no complete HTTP response: {text:?}"));
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or_else(|| panic!("no status in {head:?}"));
    (status, head.to_owned(), body.to_owned())
}

fn health(addr: SocketAddr) -> Value {
    let (status, _, body) = raw_get(addr, "/api/health");
    assert_eq!(status, 200, "/api/health: {body}");
    serde_json::from_str(&body).unwrap()
}

/// A connection that is accepted and then says nothing more: it holds one HTTP slot until the
/// request timeout. The partial head is what keeps its handler thread parked in `read`.
fn hold_a_slot(addr: SocketAddr) -> TcpStream {
    let mut s = TcpStream::connect(addr).unwrap();
    write!(s, "GET /api/health HTTP/1.1\r\nHost: test\r\n").unwrap();
    s.flush().unwrap();
    s
}

fn wait_for(what: &str, f: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(5));
    }
}

fn spectrum_header(id: &str) -> StreamHeader {
    let mut h = StreamHeader::new(
        id,
        StreamKind::Spectrum,
        ContentClass::Unrestricted,
        "t1063",
    );
    h.datatype = Some("rf32_le".into());
    h.fft_size = Some(16);
    h.sample_rate_hz = Some(10.0);
    h.center_hz = Some(100e6);
    h.bandwidth_hz = Some(2.4e6);
    h.max_frame_len = 32 + 4 * 16;
    h
}

/// Opens a consumer of `id`, waiting for the handshake to complete.
fn consumer(addr: SocketAddr, id: &str) -> Result<Ws, tungstenite::Error> {
    let (mut ws, _) = tungstenite::connect(format!("ws://{addr}/ws/{id}?token={TOKEN}"))?;
    if let MaybeTlsStream::Plain(s) = ws.get_mut() {
        s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    }
    Ok(ws)
}

fn refusal_status(r: Result<Ws, tungstenite::Error>) -> u16 {
    match r {
        Err(tungstenite::Error::Http(resp)) => resp.status().as_u16(),
        Err(e) => panic!("expected an HTTP refusal, got {e}"),
        Ok(_) => panic!("expected an HTTP refusal, got an upgrade"),
    }
}

/// The connection past the HTTP cap is **answered**, not dropped: `503`, `Retry-After: 1`,
/// `Connection: close`, the stable `overloaded` code — and it is counted on `/api/health`.
///
/// Before T-1063 this connection received nothing at all and the server kept no trace of it.
#[test]
fn a_connection_past_the_http_cap_is_answered_503_not_dropped() {
    let server = serve(3, 8, ApiState::default());
    let addr = server.local_addr();

    let holders: Vec<TcpStream> = (0..3).map(|_| hold_a_slot(addr)).collect();

    // The accept thread registers a connection before spawning its handler, so the cap takes hold
    // as the accepts are processed; retry until it does rather than sleeping a guessed interval.
    let mut refused = None;
    let deadline = Instant::now() + Duration::from_secs(10);
    while refused.is_none() {
        assert!(Instant::now() < deadline, "the HTTP cap never took hold");
        let got = raw_get(addr, "/api/health");
        if got.0 == 503 {
            refused = Some(got);
        } else {
            thread::sleep(Duration::from_millis(5));
        }
    }
    let (status, head, body) = refused.unwrap();
    assert_eq!(status, 503);
    let lower = head.to_ascii_lowercase();
    assert!(
        lower.contains("retry-after: 1"),
        "no Retry-After in {head:?}"
    );
    assert!(lower.contains("connection: close"), "head: {head:?}");
    let v: Value = serde_json::from_str(&body).unwrap_or_else(|e| panic!("{e}: {body:?}"));
    assert_eq!(v["code"], "overloaded", "body: {body}");

    // Freeing the slots makes the server answer again, and the refusal is on the record.
    drop(holders);
    wait_for("a free HTTP slot", || raw_get(addr, "/api/health").0 == 200);
    let h = health(addr);
    assert_eq!(h["connections"]["http"]["max"], 3);
    assert!(
        h["connections"]["http"]["refused"].as_u64().unwrap() >= 1,
        "the refusal is counted: {h}"
    );
    assert!(
        h["connections"]["refused_last_t"].as_f64().is_some(),
        "the refusal is dated: {h}"
    );
    assert_eq!(h["connections"]["websocket"]["refused"], 0);
}

/// A WebSocket upgrade succeeds past the HTTP cap, because it is counted in its own pool — the
/// point of the split: a tab's 17–19 stream sockets must not consume the HTTP slots its own
/// requests need, and an HTTP request must still be served while they are open.
#[test]
fn websocket_upgrades_live_in_their_own_pool_past_the_http_cap() {
    let registry = StreamRegistry::new();
    let publisher = Publisher::new(
        spectrum_header("spectrum/pool"),
        PublisherConfig {
            max_consumers: 16,
            ..PublisherConfig::default()
        },
    )
    .unwrap();
    let handle = publisher.handle();
    registry.register(publisher.header(), handle.clone());
    let state = ApiState {
        streams: registry.clone(),
        ..ApiState::default()
    };
    let server = serve(2, 8, state);
    let addr = server.local_addr();

    // Four sockets, twice the HTTP cap, all open at once.
    let sockets: Vec<Ws> = (0..4)
        .map(|i| consumer(addr, "spectrum/pool").unwrap_or_else(|e| panic!("socket {i}: {e}")))
        .collect();
    wait_for("four consumers attached", || handle.open_consumers() == 4);

    // And the HTTP pool is free the whole time: a plain request is served, not refused.
    let h = health(addr);
    assert_eq!(h["connections"]["http"]["max"], 2);
    assert_eq!(h["connections"]["websocket"]["open"], 4);
    assert_eq!(h["connections"]["http"]["refused"], 0);
    assert_eq!(h["connections"]["websocket"]["refused"], 0);

    // A closed socket gives its slot back to the pool that held it.
    drop(sockets);
    wait_for("the WebSocket pool drains", || {
        health(addr)["connections"]["websocket"]["open"] == 0
    });
    // The HTTP pool holds this very request, and never more than its cap.
    let open = health(addr)["connections"]["http"]["open"]
        .as_u64()
        .unwrap();
    assert!(
        (1..=2).contains(&open),
        "http open after the sockets went: {open}"
    );
}

/// The WebSocket pool's own cap answers the same way the HTTP one does: `503` with `Retry-After`,
/// counted — never a silent drop.
#[test]
fn a_websocket_past_its_own_cap_is_refused_503_and_counted() {
    let registry = StreamRegistry::new();
    let publisher = Publisher::new(
        spectrum_header("spectrum/wscap"),
        PublisherConfig {
            max_consumers: 16,
            ..PublisherConfig::default()
        },
    )
    .unwrap();
    let handle = publisher.handle();
    registry.register(publisher.header(), handle.clone());
    let state = ApiState {
        streams: registry.clone(),
        ..ApiState::default()
    };
    let server = serve(8, 1, state);
    let addr = server.local_addr();

    let first = consumer(addr, "spectrum/wscap").unwrap();
    wait_for("one consumer attached", || handle.open_consumers() == 1);
    assert_eq!(refusal_status(consumer(addr, "spectrum/wscap")), 503);

    let h = health(addr);
    assert_eq!(h["connections"]["websocket"]["max"], 1);
    assert_eq!(h["connections"]["websocket"]["refused"], 1);
    assert_eq!(h["connections"]["http"]["refused"], 0);

    // The slot is reusable once the first socket goes.
    drop(first);
    wait_for("the refused socket's slot frees", || {
        health(addr)["connections"]["websocket"]["open"] == 0
    });
    let _second = consumer(addr, "spectrum/wscap").expect("slot reusable");
}
