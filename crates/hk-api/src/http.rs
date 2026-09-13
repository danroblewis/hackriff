//! The hk-api HTTP server: read-only JSON control/query endpoints, the WebSocket bridge, and the
//! static web UI (ADR-0002: the UI is the first client of the same API external programs use).
//!
//! # Endpoints (all `GET`; M0 is read-only)
//! | Path | Auth | Returns |
//! |---|---|---|
//! | `/api/streams` | token | Offered streams: id, kind, class, geometry. Never content. |
//! | `/api/history?f_lo&f_hi&t0&t1[&max_cells]` | token | T-017 region-over-time grid ([`crate::query`]) |
//! | `/api/floor?f_lo&f_hi&t0&t1[&max_steps]` | token | T-021 floor vs time ([`crate::query`]) |
//! | `/api/inventory?[f_lo&f_hi][&t0&t1][&status][&tag][&scheme][&family][&cursor][&limit]` | token | T-018 signal inventory, identity-gated ([`crate::query::inventory_json`]) |
//! | `/api/status` | token | T-027 pipeline counters (per-stage samples, frames, drops, detections, tracks, chains, plugins). Never content |
//! | `/ws/<stream_id>` | token | WebSocket bridge ([`crate::bridge`]) |
//! | `/`, `/<file>` | none | Static files from the UI build directory (code, no data) |
//!
//! Frequencies are Hz; times are Unix seconds (floats), so browsers never handle i64 nanoseconds.
//!
//! # Security properties
//! - **Token.** `Authorization: Bearer <token>` or `?token=` (browser WebSockets cannot set
//!   headers), compared in constant time ([`Token::verify`]). Missing or wrong token: `401`, before
//!   anything about streams is revealed. There are no CORS headers, so other origins cannot read
//!   responses, and they do not know the token.
//! - **Bind address.** Loopback by default (the caller's choice; `hk serve` defaults to
//!   `127.0.0.1`). `0.0.0.0` exposes the API on every interface, e.g. the LAN or public Wi-Fi: the
//!   token is then the only protection and travels in cleartext (no TLS in M0).
//! - **Own-key content is never served**: every bridge consumer is `Locality::Remote`.
//! - **Inventory identities are gated by the model**: `/api/inventory` reads only
//!   `Repository::query_inventory` with the default `IdentityAccess::Standard` (no own-traffic
//!   authorisation over HTTP), never the ungated emitter getters.
//! - **Bounded resources.** At most `max_connections` connection threads; request heads are
//!   limited to 16 KiB and must arrive within `request_timeout`; query results are capped.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use hk_model::Repository;
use hk_store::{FloorProduct, Pyramid};
use hk_stream::StreamError;
use serde_json::{Value, json};

use crate::auth::Token;
use crate::bridge::{self, StreamRegistry};
use crate::query::{self, ApiError};

/// Largest request head accepted.
const MAX_HEAD: usize = 16 * 1024;
/// Largest static file served.
const MAX_STATIC: u64 = 32 * 1024 * 1024;

/// Server settings.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// Listen address. Use loopback unless the network is trusted (see the module docs).
    pub bind: SocketAddr,
    /// The API token.
    pub token: Token,
    /// Directory of the built UI (`ui/dist`); `None` serves no static files.
    pub ui_dist: Option<PathBuf>,
    /// Most connection threads at once (WebSocket consumers included; each stream also caps its
    /// own consumers through `PublisherConfig::max_consumers`).
    pub max_connections: usize,
    /// Time allowed for a request head to arrive.
    pub request_timeout: Duration,
}

impl ServerConfig {
    /// Defaults: 64 connections, 10 s request timeout, no static files.
    pub fn new(bind: SocketAddr, token: Token) -> Self {
        Self {
            bind,
            token,
            ui_dist: None,
            max_connections: 64,
            request_timeout: Duration::from_secs(10),
        }
    }
}

/// What the endpoints read. History stores are shared with their ingest thread.
#[derive(Clone, Default)]
pub struct ApiState {
    /// Streams offered over the bridge.
    pub streams: StreamRegistry,
    /// Spectrum-history pyramid for `/api/history`. When absent, the floor product's uncalibrated
    /// (dBFS) pyramid answers instead.
    pub history: Option<Arc<Mutex<Pyramid>>>,
    /// Calibrated floor product for `/api/floor`.
    pub floor: Option<Arc<Mutex<FloorProduct>>>,
    /// Signal inventory (C27, T-018) for `/api/inventory`. Read through `query_inventory` only.
    pub inventory: Option<Arc<Mutex<Repository>>>,
    /// Pipeline counters for `/api/status` (T-027): a snapshot builder, called per request.
    pub status: Option<StatusFn>,
}

/// Builds the `/api/status` JSON (counters only: no content, no identities).
pub type StatusFn = Arc<dyn Fn() -> Value + Send + Sync>;

struct Shared {
    config: ServerConfig,
    state: ApiState,
    active: AtomicUsize,
}

/// A running server. Dropping it stops accepting (open connections finish on their own).
pub struct Server {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    /// Binds and starts the accept thread.
    pub fn start(config: ServerConfig, state: ApiState) -> io::Result<Self> {
        let listener = TcpListener::bind(config.bind)?;
        let addr = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(Shared {
            config,
            state,
            active: AtomicUsize::new(0),
        });
        let stop_flag = Arc::clone(&stop);
        let thread = thread::Builder::new()
            .name("hk-api-accept".into())
            .spawn(move || accept_loop(listener, shared, stop_flag))?;
        Ok(Self {
            addr,
            stop,
            thread: Some(thread),
        })
    }

    /// The bound address (useful with port 0).
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
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

impl Drop for Server {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct ActiveGuard(Arc<Shared>);

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}

fn accept_loop(listener: TcpListener, shared: Arc<Shared>, stop: Arc<AtomicBool>) {
    for conn in listener.incoming() {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let Ok(stream) = conn else { continue };
        if shared.active.fetch_add(1, Ordering::SeqCst) >= shared.config.max_connections {
            shared.active.fetch_sub(1, Ordering::SeqCst);
            continue; // dropped: closes the connection
        }
        let guard = ActiveGuard(Arc::clone(&shared));
        let spawned = thread::Builder::new()
            .name("hk-api-conn".into())
            .stack_size(256 * 1024)
            .spawn(move || {
                let guard = guard;
                handle_connection(stream, &guard.0);
            });
        // A failed spawn drops the closure, its guard and the connection.
        let _ = spawned;
    }
}

/// A parsed request head.
struct Request {
    path: String,
    query: Vec<(String, String)>,
    headers: Vec<(String, String)>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn param(&self, name: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    fn authorized(&self, token: &Token) -> bool {
        let bearer = self.header("authorization").and_then(|v| {
            v.strip_prefix("Bearer ")
                .or_else(|| v.strip_prefix("bearer "))
        });
        match (bearer, self.param("token")) {
            (Some(b), _) => token.verify(b.trim()),
            (None, Some(q)) => token.verify(q),
            (None, None) => false,
        }
    }
}

fn read_head(stream: &mut TcpStream) -> Result<Vec<u8>, u16> {
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 2048];
    loop {
        let n = stream.read(&mut chunk).map_err(|_| 408u16)?;
        if n == 0 {
            return Err(400);
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            return Ok(buf);
        }
        if buf.len() > MAX_HEAD {
            return Err(431);
        }
    }
}

fn parse_request(head: &[u8]) -> Result<Request, u16> {
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut req = httparse::Request::new(&mut headers);
    match req.parse(head) {
        Ok(httparse::Status::Complete(_)) => {}
        _ => return Err(400),
    }
    if req.method != Some("GET") {
        return Err(405);
    }
    let target = req.path.ok_or(400u16)?;
    let (path, raw_query) = target.split_once('?').unwrap_or((target, ""));
    let headers = req
        .headers
        .iter()
        .filter_map(|h| {
            std::str::from_utf8(h.value)
                .ok()
                .map(|v| (h.name.to_owned(), v.to_owned()))
        })
        .collect();
    Ok(Request {
        path: percent_decode(path).ok_or(400u16)?,
        query: parse_query(raw_query).ok_or(400u16)?,
        headers,
    })
}

fn parse_query(q: &str) -> Option<Vec<(String, String)>> {
    q.split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            Some((
                percent_decode(&k.replace('+', " "))?,
                percent_decode(&v.replace('+', " "))?,
            ))
        })
        .collect()
}

fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        410 => "Gone",
        426 => "Upgrade Required",
        431 => "Request Header Fields Too Large",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    }
}

fn respond(stream: &mut TcpStream, status: u16, content_type: &str, extra: &str, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         Connection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\
         Referrer-Policy: no-referrer\r\n{extra}\r\n",
        reason(status),
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

fn respond_json(stream: &mut TcpStream, status: u16, body: &Value) {
    let bytes = serde_json::to_vec(body).unwrap_or_else(|_| b"{}".to_vec());
    let extra = if status == 401 {
        "WWW-Authenticate: Bearer\r\n"
    } else {
        ""
    };
    respond(stream, status, "application/json", extra, &bytes);
}

fn respond_error(stream: &mut TcpStream, status: u16, message: &str) {
    respond_json(stream, status, &json!({ "error": message }));
}

fn handle_connection(mut stream: TcpStream, shared: &Shared) {
    let _ = stream.set_read_timeout(Some(shared.config.request_timeout));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
    let _ = stream.set_nodelay(true);
    let req = match read_head(&mut stream).and_then(|h| parse_request(&h)) {
        Ok(r) => r,
        Err(status) => return respond_error(&mut stream, status, reason(status)),
    };
    let token = &shared.config.token;
    let is_api = req.path.starts_with("/api/") || req.path.starts_with("/ws/");
    if is_api && !req.authorized(token) {
        return respond_error(&mut stream, 401, "missing or invalid token");
    }
    if let Some(id) = req.path.strip_prefix("/ws/") {
        return websocket(stream, shared, &req, id);
    }
    let state = &shared.state;
    let result = match req.path.as_str() {
        "/api/streams" => Ok(state.streams.listing()),
        "/api/history" => history(state, &req),
        "/api/floor" => floor(state, &req),
        "/api/inventory" => inventory(state, &req),
        "/api/status" => state
            .status
            .as_ref()
            .map(|f| f())
            .ok_or_else(|| ApiError::new(404, "no pipeline status")),
        p if p.starts_with("/api/") => Err(ApiError::new(404, "no such endpoint")),
        _ => return static_file(&mut stream, shared.config.ui_dist.as_deref(), &req.path),
    };
    match result {
        Ok(v) => respond_json(&mut stream, 200, &v),
        Err(e) => respond_error(&mut stream, e.status, &e.message),
    }
}

fn history(state: &ApiState, req: &Request) -> Result<Value, ApiError> {
    if let Some(p) = &state.history {
        let p = p
            .lock()
            .map_err(|_| ApiError::new(500, "history store poisoned"))?;
        return query::history_json(&p, &req.query);
    }
    if let Some(f) = &state.floor {
        let f = f
            .lock()
            .map_err(|_| ApiError::new(500, "floor store poisoned"))?;
        return query::history_json(f.uncalibrated_pyramid(), &req.query);
    }
    Err(ApiError::new(404, "no spectrum history on this server"))
}

fn floor(state: &ApiState, req: &Request) -> Result<Value, ApiError> {
    let f = state
        .floor
        .as_ref()
        .ok_or_else(|| ApiError::new(404, "no floor product on this server"))?;
    let f = f
        .lock()
        .map_err(|_| ApiError::new(500, "floor store poisoned"))?;
    query::floor_json(&f, &req.query)
}

fn inventory(state: &ApiState, req: &Request) -> Result<Value, ApiError> {
    let repo = state
        .inventory
        .as_ref()
        .ok_or_else(|| ApiError::new(404, "no signal inventory on this server"))?;
    let repo = repo
        .lock()
        .map_err(|_| ApiError::new(500, "inventory store poisoned"))?;
    query::inventory_json(&repo, &req.query)
}

fn websocket(mut stream: TcpStream, shared: &Shared, req: &Request, stream_id: &str) {
    let has_token = |name: &str, want: &str| {
        req.header(name)
            .is_some_and(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case(want)))
    };
    if !has_token("upgrade", "websocket") || !has_token("connection", "upgrade") {
        return respond_error(&mut stream, 426, "WebSocket upgrade required");
    }
    if req.header("sec-websocket-version").map(str::trim) != Some("13") {
        return respond(
            &mut stream,
            426,
            "text/plain",
            "Sec-WebSocket-Version: 13\r\n",
            b"WebSocket version 13 required",
        );
    }
    let Some(key) = req.header("sec-websocket-key").map(str::trim) else {
        return respond_error(&mut stream, 400, "missing Sec-WebSocket-Key");
    };
    let Some(handle) = shared.state.streams.handle(stream_id) else {
        return respond_error(&mut stream, 404, "no such stream");
    };
    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Accept: {}\r\n\r\n",
        tungstenite::handshake::derive_accept_key(key.as_bytes())
    )
    .into_bytes();
    let peer = stream
        .peer_addr()
        .map_or_else(|_| "unknown".into(), |a| a.to_string());
    let _ = stream.set_write_timeout(None);
    match bridge::attach(&handle, &stream, format!("ws:{peer}"), response) {
        Ok(id) => bridge::watch_peer(&handle, stream, id),
        Err(StreamError::LocalOnly { .. }) => respond_error(
            &mut stream,
            403,
            "stream is local-only (own-key-decrypted); it is never served over the bridge",
        ),
        Err(StreamError::TooManyConsumers { max }) => {
            respond_error(&mut stream, 503, &format!("consumer limit {max} reached"))
        }
        Err(StreamError::Finished) => respond_error(&mut stream, 410, "stream finished"),
        Err(_) => respond_error(&mut stream, 500, "subscription failed"),
    }
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") | Some("map") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("wasm") => "application/wasm",
        _ => "application/octet-stream",
    }
}

const CSP: &str = "Content-Security-Policy: default-src 'self'; connect-src 'self' ws: wss:; \
                   img-src 'self' data:; style-src 'self' 'unsafe-inline'; base-uri 'none'; \
                   frame-ancestors 'none'\r\n";

fn static_file(stream: &mut TcpStream, dist: Option<&Path>, path: &str) {
    let rel = if path == "/" {
        "index.html"
    } else {
        &path[1..]
    };
    let safe = rel.split('/').all(|seg| {
        !seg.is_empty()
            && !seg.starts_with('.')
            && seg
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    });
    let not_built = || {
        b"<!doctype html><title>hackriff</title><p>UI not built: run <code>just ui-build</code>, \
          then reload.</p>"
            .to_vec()
    };
    let Some(dist) = dist.filter(|_| safe) else {
        return if path == "/" {
            respond(stream, 200, "text/html; charset=utf-8", CSP, &not_built())
        } else {
            respond_error(stream, 404, "not found")
        };
    };
    let resolved = dist
        .canonicalize()
        .ok()
        .and_then(|root| Some((dist.join(rel).canonicalize().ok()?, root)))
        .filter(|(file, root)| file.starts_with(root) && file.is_file());
    let body = resolved.and_then(|(file, _)| {
        let len = file.metadata().ok()?.len();
        (len <= MAX_STATIC)
            .then(|| std::fs::read(&file).ok().map(|b| (file, b)))
            .flatten()
    });
    match body {
        Some((file, bytes)) => respond(stream, 200, content_type(&file), CSP, &bytes),
        None if path == "/" => respond(stream, 200, "text/html; charset=utf-8", CSP, &not_built()),
        None => respond_error(stream, 404, "not found"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_and_path_decoding() {
        assert_eq!(
            parse_query("a=1&b=x%2By&c=a+b&token=").unwrap(),
            vec![
                ("a".into(), "1".into()),
                ("b".into(), "x+y".into()),
                ("c".into(), "a b".into()),
                ("token".into(), "".into())
            ]
        );
        assert!(parse_query("a=%zz").is_none());
        assert_eq!(
            percent_decode("/ws/spectrum%2Flive").unwrap(),
            "/ws/spectrum/live"
        );
    }

    #[test]
    fn non_get_is_refused() {
        assert_eq!(
            parse_request(b"POST /api/streams HTTP/1.1\r\n\r\n").err(),
            Some(405)
        );
        assert!(parse_request(b"GET /api/streams?token=x HTTP/1.1\r\nHost: a\r\n\r\n").is_ok());
    }
}
