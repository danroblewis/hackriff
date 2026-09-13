//! Token-authenticated TCP stream server (T-060, docs/stream-contract.md §13): external programs
//! (netcat, socat, a Python script, a GNU Radio TCP source) connect to any offered stream or
//! on-demand opener over plain TCP, with the same framing as every other transport.
//!
//! # Handshake
//! The client sends **one line** (at most [`MAX_HANDSHAKE_LINE`] bytes, `\n`-terminated, an
//! optional `\r` stripped) within `handshake_timeout`:
//!
//! ```text
//! <stream_id>?token=<token>              an always-on stream, e.g. spectrum/live
//! open/<name>?token=<token>[&k=v...]      an on-demand stream, e.g. open/bits, open/listen?emitter=<id>
//! ```
//!
//! Query values are percent-decoded; a leading `/` is ignored. The server then sends the stream
//! exactly as on a Unix socket (§3 framing: `u32` LE length-prefixed frames, header first).
//! A **refusal** is sent in the same framing instead of the header: one frame whose JSON object is
//! `{"type":"refused","status","code","reason","content_class"}` (status 400 bad handshake, 401
//! token, 403 local-only or legal refusal, 404 unknown stream or opener, 408 handshake timeout,
//! 410 finished, 503 busy, ...), then the connection closes. A reader tells the two apart by the
//! first frame's `schema` (header) or `type` (refusal).
//!
//! # Rules
//! - **Token first:** nothing about streams (not even whether one exists) is revealed before the
//!   token verifies ([`Token::verify`], constant time). The token travels in cleartext: bind to
//!   loopback (the default in `hk serve`) unless the network is trusted.
//! - **Consumers never send.** Bytes after the handshake line, or a hang-up, close the consumer
//!   and drop an on-demand session, stopping its producer. A client whose tool half-closes on
//!   stdin EOF must keep its sending side open (`nc` does by default; `socat` needs
//!   `-t <large>`), or the stream ends at once.
//! - **Remote locality:** the connection subscribes as a `TcpStream` (`Locality::Remote`), so
//!   `own-key-decrypted` streams are refused (403) and every §6 gate applies unchanged.
//! - **Drop, never block:** the publisher's per-consumer queue (§7) is the only queue; a client
//!   that does not read fills its own ring, loses records with markers and is disconnected after
//!   `disconnect_after`. The producer never waits. [`StreamServer::stats`] counts drops.
//! - **Bounded:** at most `max_connections` connection threads; further connections get a 503
//!   refusal frame.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use hk_stream::{ConsumerId, OpenRefusal, OpenRequest, OpenerRegistry, PublisherHandle};

use crate::auth::Token;
use crate::bridge::StreamRegistry;

/// Longest handshake line accepted, bytes.
pub const MAX_HANDSHAKE_LINE: usize = 4096;
/// The conventional stream port (`hk serve` binds `127.0.0.1:8788` unless told otherwise).
pub const DEFAULT_STREAM_TCP_PORT: u16 = 8788;

/// TCP stream server settings.
#[derive(Clone, Debug)]
pub struct StreamServerConfig {
    /// Listen address. Loopback unless the network is trusted.
    pub bind: SocketAddr,
    /// The API token (the same one the HTTP API uses).
    pub token: Token,
    /// Most connection threads at once.
    pub max_connections: usize,
    /// Time allowed for the handshake line.
    pub handshake_timeout: Duration,
}

impl StreamServerConfig {
    /// Defaults: 32 connections, 10 s handshake timeout.
    pub fn new(bind: SocketAddr, token: Token) -> Self {
        Self {
            bind,
            token,
            max_connections: 32,
            handshake_timeout: Duration::from_secs(10),
        }
    }
}

/// Server counters. Metadata only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StreamServerStats {
    /// Connections accepted.
    pub accepted: u64,
    /// Connections answered with a refusal frame (or dropped at the connection cap).
    pub refused: u64,
    /// Connections subscribed to a stream.
    pub served: u64,
    /// Subscribed connections open now.
    pub active: u64,
    /// Records queued for this server's consumers (open and closed).
    pub records_enqueued: u64,
    /// Records dropped for this server's consumers because their queue was full.
    pub records_dropped: u64,
}

struct Conn {
    handle: PublisherHandle,
    id: ConsumerId,
}

struct Shared {
    config: StreamServerConfig,
    streams: StreamRegistry,
    openers: OpenerRegistry,
    threads: AtomicUsize,
    accepted: AtomicU64,
    refused: AtomicU64,
    served: AtomicU64,
    closed_enqueued: AtomicU64,
    closed_dropped: AtomicU64,
    next: AtomicU64,
    conns: Mutex<BTreeMap<u64, Conn>>,
}

/// A running TCP stream server. Dropping it stops accepting; open connections continue until
/// their consumer closes.
pub struct StreamServer {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    shared: Arc<Shared>,
}

impl StreamServer {
    /// Binds and starts the accept thread. `streams` are served by id, `openers` as
    /// `open/<name>`.
    pub fn start(
        config: StreamServerConfig,
        streams: StreamRegistry,
        openers: OpenerRegistry,
    ) -> io::Result<Self> {
        let listener = TcpListener::bind(config.bind)?;
        let addr = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(Shared {
            config,
            streams,
            openers,
            threads: AtomicUsize::new(0),
            accepted: AtomicU64::new(0),
            refused: AtomicU64::new(0),
            served: AtomicU64::new(0),
            closed_enqueued: AtomicU64::new(0),
            closed_dropped: AtomicU64::new(0),
            next: AtomicU64::new(0),
            conns: Mutex::new(BTreeMap::new()),
        });
        let (sh, flag) = (Arc::clone(&shared), Arc::clone(&stop));
        let thread = thread::Builder::new()
            .name("hk-stream-tcp".into())
            .spawn(move || accept_loop(listener, sh, flag))?;
        Ok(Self {
            addr,
            stop,
            thread: Some(thread),
            shared,
        })
    }

    /// The bound address (useful with port 0).
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Counters, including drops of open consumers.
    pub fn stats(&self) -> StreamServerStats {
        let conns = self
            .shared
            .conns
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let (mut enq, mut dropped) = (
            self.shared.closed_enqueued.load(Ordering::Relaxed),
            self.shared.closed_dropped.load(Ordering::Relaxed),
        );
        for c in conns.values() {
            if let Some(s) = c.handle.stats(c.id) {
                enq += s.records_enqueued;
                dropped += s.records_dropped;
            }
        }
        StreamServerStats {
            accepted: self.shared.accepted.load(Ordering::Relaxed),
            refused: self.shared.refused.load(Ordering::Relaxed),
            served: self.shared.served.load(Ordering::Relaxed),
            active: conns.len() as u64,
            records_enqueued: enq,
            records_dropped: dropped,
        }
    }

    /// Consumer counters of every open connection (label `tcp:<target>:<peer>`). Each is kept by
    /// its own stream's publisher, so drops on one stream never show on another.
    pub fn consumers(&self) -> Vec<hk_stream::ConsumerStats> {
        let conns = self
            .shared
            .conns
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        conns
            .values()
            .filter_map(|c| c.handle.stats(c.id))
            .collect()
    }

    /// Stops accepting and joins the accept thread.
    pub fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let mut wake = self.addr;
        if wake.ip().is_unspecified() {
            wake.set_ip(std::net::Ipv4Addr::LOCALHOST.into());
        }
        let _ = TcpStream::connect_timeout(&wake, Duration::from_millis(200));
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for StreamServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// A refusal as it goes on the wire: one §3 frame holding the refusal JSON.
pub fn refusal_frame(refusal: &OpenRefusal) -> Vec<u8> {
    let body = refusal.to_json().to_string().into_bytes();
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

/// Splits a handshake line into its target and query (percent-decoded, `token` included).
pub fn parse_handshake(line: &str) -> Result<(String, Vec<(String, String)>), OpenRefusal> {
    let bad = |why: &str| OpenRefusal::new(400, "bad-handshake", why);
    let line = line.trim();
    let (raw_target, raw_query) = line.split_once('?').unwrap_or((line, ""));
    let target = crate::http::percent_decode(raw_target.trim_start_matches('/'))
        .ok_or_else(|| bad("the target is not valid percent-encoding"))?;
    if target.is_empty() || target.len() > 256 || target.chars().any(char::is_whitespace) {
        return Err(bad(
            "expected `<stream_id>?token=<token>` or `open/<name>?token=<token>[&k=v]`",
        ));
    }
    let query = crate::http::parse_query(raw_query)
        .ok_or_else(|| bad("the query is not valid percent-encoding"))?;
    Ok((target, query))
}

struct ThreadGuard(Arc<Shared>);

impl Drop for ThreadGuard {
    fn drop(&mut self) {
        self.0.threads.fetch_sub(1, Ordering::SeqCst);
    }
}

fn accept_loop(listener: TcpListener, shared: Arc<Shared>, stop: Arc<AtomicBool>) {
    for conn in listener.incoming() {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let Ok(stream) = conn else { continue };
        shared.accepted.fetch_add(1, Ordering::Relaxed);
        if shared.threads.fetch_add(1, Ordering::SeqCst) >= shared.config.max_connections {
            shared.threads.fetch_sub(1, Ordering::SeqCst);
            shared.refused.fetch_add(1, Ordering::Relaxed);
            // Best effort without blocking the accept thread.
            let busy = OpenRefusal::new(503, "busy", "connection limit reached");
            let _ = stream.set_nonblocking(true);
            let _ = (&stream).write_all(&refusal_frame(&busy));
            continue;
        }
        let guard = ThreadGuard(Arc::clone(&shared));
        let spawned = thread::Builder::new()
            .name("hk-stream-tcp-conn".into())
            .stack_size(256 * 1024)
            .spawn(move || {
                let guard = guard;
                serve_connection(stream, &guard.0);
            });
        let _ = spawned;
    }
}

/// Reads the handshake line; anything after it is refused (consumers never send).
fn read_line(s: &mut TcpStream) -> Result<String, OpenRefusal> {
    let mut buf: Vec<u8> = Vec::with_capacity(256);
    let mut chunk = [0u8; 512];
    loop {
        let n = s
            .read(&mut chunk)
            .map_err(|_| OpenRefusal::new(408, "handshake-timeout", "no handshake line in time"))?;
        if n == 0 {
            return Err(OpenRefusal::new(
                400,
                "bad-handshake",
                "connection closed before the handshake line",
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.iter().position(|&b| b == b'\n') {
            if buf[i + 1..].iter().any(|b| !b.is_ascii_whitespace()) {
                return Err(OpenRefusal::new(
                    400,
                    "bad-handshake",
                    "data after the handshake line (consumers never send)",
                ));
            }
            buf.truncate(i);
            if i > MAX_HANDSHAKE_LINE {
                break;
            }
            return String::from_utf8(buf)
                .map_err(|_| OpenRefusal::new(400, "bad-handshake", "handshake is not UTF-8"));
        }
        if buf.len() > MAX_HANDSHAKE_LINE {
            break;
        }
    }
    Err(OpenRefusal::new(
        431,
        "bad-handshake",
        "handshake line too long",
    ))
}

fn refuse(mut s: TcpStream, refusal: &OpenRefusal) {
    let _ = s.set_write_timeout(Some(Duration::from_secs(2)));
    let _ = s.write_all(&refusal_frame(refusal));
    let _ = s.flush();
    let _ = s.shutdown(Shutdown::Write);
    // Let the client read the frame before the socket closes (closing with unread input resets).
    let _ = s.set_read_timeout(Some(Duration::from_millis(200)));
    let mut sink = [0u8; 256];
    while matches!(s.read(&mut sink), Ok(n) if n > 0) {}
}

type Resolved = (
    PublisherHandle,
    Option<Box<dyn std::any::Any + Send>>,
    String,
);

fn resolve(s: &mut TcpStream, sh: &Shared, peer: &str) -> Result<Resolved, OpenRefusal> {
    let line = read_line(s)?;
    let (target, query) = parse_handshake(&line)?;
    let token = query
        .iter()
        .find(|(k, _)| k == "token")
        .map(|(_, v)| v.as_str());
    if !token.is_some_and(|t| sh.config.token.verify(t)) {
        return Err(OpenRefusal::new(
            401,
            "unauthorized",
            "missing or invalid token",
        ));
    }
    if let Some(name) = target.strip_prefix("open/") {
        let opener = sh
            .openers
            .get(name)
            .ok_or_else(|| OpenRefusal::new(404, "not-found", "no such on-demand stream"))?;
        let opened = opener.open(&OpenRequest::from_query(&query, format!("tcp:{peer}")))?;
        return Ok((
            opened.handle,
            Some(opened.session),
            format!("tcp:open/{name}:{peer}"),
        ));
    }
    if query.iter().any(|(k, _)| k != "token") {
        return Err(OpenRefusal::new(
            400,
            "bad-request",
            "always-on streams take no parameters besides token",
        ));
    }
    let handle = sh
        .streams
        .handle(&target)
        .ok_or_else(|| OpenRefusal::new(404, "not-found", "no such stream"))?;
    Ok((handle, None, format!("tcp:{target}:{peer}")))
}

fn serve_connection(mut s: TcpStream, sh: &Shared) {
    let peer = s
        .peer_addr()
        .map_or_else(|_| "unknown".into(), |a| a.to_string());
    let _ = s.set_nodelay(true);
    let _ = s.set_read_timeout(Some(sh.config.handshake_timeout));
    let (handle, session, label) = match resolve(&mut s, sh, &peer) {
        Ok(r) => r,
        Err(refusal) => {
            sh.refused.fetch_add(1, Ordering::Relaxed);
            return refuse(s, &refusal);
        }
    };
    let _ = s.set_read_timeout(None);
    let _ = s.set_write_timeout(None);
    let clones = s.try_clone().and_then(|w| Ok((w, s.try_clone()?)));
    let Ok((writer, closer)) = clones else {
        sh.refused.fetch_add(1, Ordering::Relaxed);
        return;
    };
    // A `TcpStream` writer is `Locality::Remote`: own-key streams are refused here.
    let id = match handle.subscribe(
        label,
        writer,
        Box::new(move |_| {
            let _ = closer.shutdown(Shutdown::Both);
        }),
    ) {
        Ok(id) => id,
        Err(e) => {
            sh.refused.fetch_add(1, Ordering::Relaxed);
            drop(session);
            return refuse(s, &crate::ondemand::attach_refusal(&e));
        }
    };
    sh.served.fetch_add(1, Ordering::Relaxed);
    let key = sh.next.fetch_add(1, Ordering::Relaxed);
    sh.conns
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(
            key,
            Conn {
                handle: handle.clone(),
                id,
            },
        );
    // Blocks until the client sends anything or hangs up, or the consumer closes (its closer
    // shuts the socket down).
    let mut byte = [0u8; 1];
    let _ = s.read(&mut byte);
    handle.close(id);
    let _ = s.shutdown(Shutdown::Both);
    {
        let mut conns = sh.conns.lock().unwrap_or_else(PoisonError::into_inner);
        conns.remove(&key);
        if let Some(st) = handle.stats(id) {
            sh.closed_enqueued
                .fetch_add(st.records_enqueued, Ordering::Relaxed);
            sh.closed_dropped
                .fetch_add(st.records_dropped, Ordering::Relaxed);
        }
    }
    // Dropping the session stops an on-demand producer.
    drop(session);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshake_lines_parse() {
        let (t, q) = parse_handshake("open/bits?token=abc%2Bd&f_lo=1&f_hi=2\r").unwrap();
        assert_eq!(t, "open/bits");
        assert_eq!(q[0], ("token".into(), "abc+d".into()));
        assert_eq!(q.len(), 3);
        let (t, q) = parse_handshake("/bits%2Ffsk-bursts%2Fx?token=z").unwrap();
        assert_eq!(t, "bits/fsk-bursts/x");
        assert_eq!(q.len(), 1);
        for bad in ["", "?token=x", "a b?token=x", "x?token=%zz"] {
            assert_eq!(parse_handshake(bad).unwrap_err().status, 400, "{bad:?}");
        }
        let frame = refusal_frame(&OpenRefusal::new(401, "unauthorized", "no"));
        let len = u32::from_le_bytes(frame[..4].try_into().unwrap()) as usize;
        assert_eq!(len, frame.len() - 4);
        let v: serde_json::Value = serde_json::from_slice(&frame[4..]).unwrap();
        assert_eq!(v["type"], "refused");
        assert_eq!(v["status"], 401);
    }
}
