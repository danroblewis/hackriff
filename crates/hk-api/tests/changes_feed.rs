//! T-1065: `GET /ws/changes` — the versioned change feed, so a client stops polling.
//!
//! What these pin, in the ticket's own words (USER, 2026-09-26: *"We don't need hundreds of requests
//! every time I slightly zoom in"*):
//!
//! - **a write to a route's state produces exactly one `changed` for that route** — and for no other
//!   route, so a client re-reads what moved and nothing else;
//! - **a burst coalesces**: twenty writes inside one sampling tick are one message carrying the
//!   newest version, never twenty;
//! - **no write means no messages at all** — asserted over the full 30 s the acceptance names, which
//!   is the whole promise: a feed that chattered while idle would have replaced a poll with a push
//!   of the same volume. An idle socket is kept open by a WebSocket **ping**, not by JSON traffic;
//! - and the feed is **capped and authenticated** like every other `/ws/` route.
//!
//! The bumps here are made through [`hk_api::ChangeFeed::bump`], the producer-side API, on a server
//! with no audit log and so no writable control plane: what is under test is the feed itself. The
//! HTTP **write** path that calls it — a real `POST` bumping a real route — is pinned end to end in
//! `hk-cli/tests/api_contract.rs`, against a server driving the mock SDR.
//!
//! Every assertion is a count, a version, a message type or a status: no assertion reads a clock to
//! decide what to assert on.

use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hk_api::{ApiState, Change, ChangeFeed, Server, ServerConfig, Token};
use serde_json::{Value, json};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

const TOKEN: &str = "t1065-changes-feed-token-0123456789ab";

type Ws = WebSocket<MaybeTlsStream<TcpStream>>;

/// A server whose only interesting state is its change feed.
fn serve() -> (Server, Arc<ChangeFeed>) {
    let changes: Arc<ChangeFeed> = Arc::default();
    let state = ApiState {
        changes: Arc::clone(&changes),
        ..ApiState::default()
    };
    let config = ServerConfig::new(
        "127.0.0.1:0".parse().unwrap(),
        Token::from_config(TOKEN).unwrap(),
    );
    (Server::start(config, state).unwrap(), changes)
}

fn connect(addr: SocketAddr) -> Ws {
    let (mut ws, _) =
        tungstenite::connect(format!("ws://{addr}/ws/changes?token={TOKEN}")).expect("upgrade");
    if let MaybeTlsStream::Plain(s) = ws.get_mut() {
        s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    }
    ws
}

/// The next text message as JSON, or `None` on close / timeout / error. Pings are housekeeping.
fn next(ws: &mut Ws) -> Option<Value> {
    loop {
        match ws.read() {
            Ok(Message::Text(t)) => return Some(serde_json::from_str(&t).unwrap()),
            Ok(Message::Close(_)) | Err(_) => return None,
            Ok(_) => {}
        }
    }
}

/// Connects and consumes the opening `versions` snapshot, returning it with the socket.
fn subscribed(addr: SocketAddr) -> (Ws, Value) {
    let mut ws = connect(addr);
    let snap = next(&mut ws).expect("the first message is the versions snapshot");
    assert_eq!(snap["type"], json!("versions"), "{snap}");
    (ws, snap)
}

/// Every text message that arrives within `limit`, stopping early once `want` have.
fn drain(ws: &mut Ws, limit: Duration, want: usize) -> Vec<Value> {
    let deadline = Instant::now() + limit;
    let mut out = Vec::new();
    while out.len() < want {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        if let MaybeTlsStream::Plain(s) = ws.get_mut() {
            s.set_read_timeout(Some(left)).unwrap();
        }
        match next(ws) {
            Some(v) => out.push(v),
            None => break,
        }
    }
    out
}

#[test]
fn the_snapshot_carries_every_route_the_feed_covers() {
    let (server, feed) = serve();
    feed.bump(Change::Coverage);
    feed.bump(Change::Coverage);
    let (mut ws, snap) = subscribed(server.local_addr());
    for c in Change::ALL {
        assert!(
            snap["routes"][c.route()].is_u64(),
            "{} is missing from the snapshot: {snap}",
            c.route()
        );
    }
    // The snapshot is the versions AS OF the connect, so a client can file the bodies it already
    // has against them; it is not a list of what recently changed.
    assert_eq!(snap["routes"]["/api/coverage"], json!(2), "{snap}");
    assert_eq!(snap["routes"]["/api/inventory"], json!(0), "{snap}");
    assert_eq!(snap["tick_ms"], json!(250), "{snap}");
    let _ = ws.close(None);
    drop(server);
}

#[test]
fn one_write_is_one_changed_for_that_route_and_no_other() {
    let (server, feed) = serve();
    let (mut ws, _) = subscribed(server.local_addr());
    feed.bump(Change::Inventory);
    let msgs = drain(&mut ws, Duration::from_secs(5), 1);
    assert_eq!(msgs.len(), 1, "one write, one message: {msgs:?}");
    assert_eq!(
        (
            msgs[0]["type"].clone(),
            msgs[0]["route"].clone(),
            msgs[0]["version"].clone()
        ),
        (json!("changed"), json!("/api/inventory"), json!(1)),
        "{msgs:?}"
    );
    // And nothing for any other route: a client re-reads what moved, not everything.
    let more = drain(&mut ws, Duration::from_secs(2), 1);
    assert!(
        more.is_empty(),
        "only the written route is reported: {more:?}"
    );
    let _ = ws.close(None);
    drop(server);
}

#[test]
fn every_route_the_feed_carries_can_be_written_and_is_reported_once() {
    let (server, feed) = serve();
    let (mut ws, _) = subscribed(server.local_addr());
    // One write per route, all before the first tick can sample: each route is reported exactly
    // once, and the twelve messages name the twelve routes — no route is missing a producer path.
    for c in Change::ALL {
        feed.bump(c);
    }
    let msgs = drain(&mut ws, Duration::from_secs(10), Change::ALL.len());
    let mut seen: Vec<&str> = msgs
        .iter()
        .map(|m| {
            assert_eq!(m["type"], json!("changed"), "{m}");
            assert_eq!(m["version"], json!(1), "{m}");
            m["route"].as_str().expect("route")
        })
        .collect();
    seen.sort_unstable();
    let mut want: Vec<&str> = Change::ALL.iter().map(|c| c.route()).collect();
    want.sort_unstable();
    assert_eq!(seen, want, "{msgs:?}");
    let _ = ws.close(None);
    drop(server);
}

#[test]
fn a_burst_of_writes_coalesces_into_one_message_at_the_newest_version() {
    let (server, feed) = serve();
    let (mut ws, _) = subscribed(server.local_addr());
    for _ in 0..20 {
        feed.bump(Change::Annotations);
    }
    assert_eq!(feed.version(Change::Annotations), 20);
    // Twenty atomic increments complete far inside one 250 ms sampling tick, so this is normally
    // exactly one message. The bound is 2 rather than 1 only because this thread can be preempted
    // mid-burst on a loaded box — which would split the burst across two samples and is still
    // coalescing. What is asserted either way: FAR fewer messages than writes, and the last message
    // carries the newest version, so nothing is lost by the coalescing.
    let msgs = drain(&mut ws, Duration::from_secs(5), 3);
    assert!(
        (1..=2).contains(&msgs.len()),
        "20 writes must coalesce, not amplify: {msgs:?}"
    );
    let last = msgs.last().expect("at least one message");
    assert_eq!(
        (last["route"].clone(), last["version"].clone()),
        (json!("/api/annotations"), json!(20)),
        "{msgs:?}"
    );
    let _ = ws.close(None);
    drop(server);
}

/// The acceptance's own words: *no write for 30 s → no messages.* An idle feed is silent, and stays
/// open on a **ping** rather than on JSON — a heartbeat message would be the traffic the feed exists
/// to remove.
#[test]
fn nothing_is_written_for_thirty_seconds_and_nothing_is_sent() {
    let (server, feed) = serve();
    let (mut ws, _) = subscribed(server.local_addr());
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut pings = 0usize;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        if let MaybeTlsStream::Plain(s) = ws.get_mut() {
            s.set_read_timeout(Some(left)).unwrap();
        }
        match ws.read() {
            Ok(Message::Ping(_)) => pings += 1,
            Ok(Message::Pong(_)) => {}
            Ok(m) => panic!("an idle feed must send nothing; got {m:?}"),
            Err(_) => break, // the read timeout: silence, which is the point
        }
    }
    // Silent, and alive: the keep-alive is a ping (PING_EVERY = 20 s), so 30 s of idleness shows at
    // least one and the socket is still writable afterwards.
    assert!(pings >= 1, "an idle feed pings to stay open, {pings} seen");
    assert_eq!(feed.version(Change::ControlState), 0);
    // Still connected: one write now is still reported.
    feed.bump(Change::ControlState);
    let msgs = drain(&mut ws, Duration::from_secs(5), 1);
    assert_eq!(msgs.len(), 1, "{msgs:?}");
    assert_eq!(msgs[0]["route"], json!("/api/control/state"), "{msgs:?}");
    let _ = ws.close(None);
    drop(server);
}

#[test]
fn the_feed_is_capped_per_server_and_the_refusal_completes_the_upgrade() {
    let (server, _feed) = serve();
    let addr = server.local_addr();
    let mut open: Vec<Ws> = (0..hk_api::changes::MAX_CHANGE_FEEDS)
        .map(|_| subscribed(addr).0)
        .collect();
    let mut over = connect(addr);
    let refused = next(&mut over).expect("a refusal is a message, not a failed upgrade");
    assert_eq!(refused["type"], json!("refused"), "{refused}");
    assert_eq!(refused["status"], json!(503), "{refused}");
    // Closing one frees a slot: the cap is a live count, not a high-water mark.
    let mut freed = open.pop().expect("a feed to close");
    let _ = freed.close(None);
    while freed.read().is_ok() {}
    drop(freed);
    // The upgrade itself always completes (the refusal is a message, above), so "a slot is free" is
    // read from the FIRST MESSAGE, never from the connect.
    let deadline = Instant::now() + Duration::from_secs(5);
    let ok = loop {
        let mut ws = connect(addr);
        let first = next(&mut ws).expect("a first message");
        let admitted = first["type"] == json!("versions");
        let _ = ws.close(None);
        if admitted {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(ok, "a closed feed must free its slot");
    for mut ws in open {
        let _ = ws.close(None);
    }
    drop(server);
}

#[test]
fn the_feed_needs_the_token_and_is_refused_before_the_upgrade() {
    let (server, _feed) = serve();
    let addr = server.local_addr();
    let err = tungstenite::connect(format!("ws://{addr}/ws/changes")).unwrap_err();
    match err {
        tungstenite::Error::Http(r) => assert_eq!(r.status().as_u16(), 401),
        e => panic!("expected a 401 before the upgrade, got {e}"),
    }
    let err = tungstenite::connect(format!("ws://{addr}/ws/changes?token=wrong")).unwrap_err();
    match err {
        tungstenite::Error::Http(r) => assert_eq!(r.status().as_u16(), 401),
        e => panic!("expected a 401 before the upgrade, got {e}"),
    }
    drop(server);
}
