//! The hk-api HTTP server: JSON query endpoints, the authenticated control API (T-050), the
//! WebSocket bridge, and the static web UI (ADR-0002: the UI is the first client of the same API
//! external programs use).
//!
//! # Endpoints
//! [`ROUTES`] is the complete table; nothing else answers 2xx under `/api/` or `/ws/`.
//!
//! | Path | Method | Auth | Returns |
//! |---|---|---|---|
//! | `/api/streams` | GET | token | Discovery (T-060): offered streams (id, kind, class, geometry, format), on-demand openers, the TCP stream address. Never content. |
//! | `/api/history?f_lo&f_hi&t0&t1[&max_cells][&format][&stat]` | GET | token | T-017 region-over-time grid ([`crate::query`]); T-116 `format=csv` (hackrf_sweep) / `format=png` (waterfall) |
//! | `/api/floor?f_lo&f_hi&t0&t1[&max_steps]` | GET | token | T-021 floor vs time ([`crate::query`]) |
//! | `/api/inventory?[f_lo&f_hi][&t0&t1][&state][&status][&tag][&scheme][&family][&cursor][&limit]` | GET | token | T-018 signal inventory, identity-gated ([`crate::query::inventory_json`]); `state` = T-078 lifecycle |
//! | `/api/inventory/<id>[/promote\|/band]` | GET, POST, PUT, DELETE | token (header only for mutating) | T-078 one entry, promote a candidate, delete; T-191 set/clear the user band ([`crate::inventory`]) |
//! | `/api/inventory/<id>/decode` | GET | token | T-159 the emitter's latest decode fields, one row per decoder/frame-model ([`crate::decode`]) |
//! | `/api/analysis/strongest?f_lo&f_hi[&window_s]` | GET | token | T-079 strongest observed signal in a band over a recent window, from spectrum history ([`crate::query::strongest_json`]) |
//! | `/api/observations?f_lo&f_hi&t0&t1[&tier][&cursor][&limit]` | GET | token | T-115 observation log records in a box ([`crate::observations`]) |
//! | `/api/observations/coverage?f_lo&f_hi&t0&t1[&channel_hz][&tau_s][&min_gap_s]` | GET | token | T-115 observation totals, per-channel totals, gaps and POI ([`crate::observations`]) |
//! | `/api/occupancy?f_lo&f_hi&t0&t1[&interval][&site]` | GET | token | T-118 occupancy series (FCO/FBO/SRO per learned channel and band) and the learned plan ([`crate::occupancy`]) |
//! | `/api/sites`, `/api/sites/current`, `/api/sites/<id>` | GET, PUT | token (header only for mutating) | T-119 sites and the current (pinned) site ([`crate::attention`]) |
//! | `/api/baselines[?site]`, `/api/baselines/slots?f_lo&f_hi[&site][&slot][&resolution]`, `/api/baselines/refreeze` | GET, POST | token (header only for mutating) | T-119 baselines, slot pools, re-freeze ([`crate::attention`]) |
//! | `/api/candidates[?f_lo&f_hi][&limit]` | GET | token | T-119 latest `CandidateSet`; T-131 published read-only when the bandit is off ([`crate::attention`]) |
//! | `/api/attention/weights` | GET, PUT | token (header only for mutating) | T-119 versioned score weights ([`crate::attention`]) |
//! | `/api/scheduler[?f_lo&f_hi][&t0&t1][&tau_s]` | GET | token | T-127 tier shares, sweep floor, bandit summary, leases, POI + gaps from the observation log ([`crate::schedule`]) |
//! | `/api/scheduler/arms`, `/api/scheduler/leases[/<id>]` | GET, POST, DELETE | token (header only for mutating) | T-127 bandit arm table; lease list, create, release ([`crate::schedule`]) |
//! | `/api/report?f_lo&f_hi&t0&t1[&site][&format]` | GET | token | T-121 survey report (`SurveyReport` JSON, or CSV/PNG export) with mandatory coverage and POI ([`crate::reports`]) |
//! | `/api/anomalies[?f_lo&f_hi][&t0&t1][&kind][&status][&cursor][&limit]`, `/api/anomalies/<id>[/dismiss\|/reopen]` | GET, POST | token (header only for mutating) | T-122 anomalies and novelty alarms with explanations; dismiss/reopen ([`crate::anomalies`]) |
//! | `/api/status` | GET | token | T-027 pipeline counters. Never content |
//! | `/api/control/*`, `/api/bookmarks[/<id>]` | GET, POST, PUT, DELETE | token (header only for mutating) | T-050 control API ([`crate::control`]) |
//! | `/api/selections[/<id>[/links]]` | GET, POST, PUT, DELETE | token (header only for mutating) | T-052 persisted region selections ([`crate::selections`]) |
//! | `/api/outputs[/record/start\|/record/stop]`, `/api/outputs/<id>/files/<name>` | GET, POST | token (header only for mutating) | T-061 output recordings and downloads ([`crate::outputs`]) |
//! | `/api/analyze` | POST | token | T-190 stub: validates a selection/emitter/band target, answers `501 not_implemented` until MAUTO fills it in ([`crate::analyze`]) |
//! | `/ws/<stream_id>` | GET | token | WebSocket bridge ([`crate::bridge`]) |
//! | `/ws/open/<name>?…` | GET | token | On-demand stream, e.g. `listen` (T-043, [`crate::ondemand`]) |
//! | `/`, `/<file>` | GET | none | Static files from the UI build directory (code, no data) |
//!
//! Frequencies are Hz; times are Unix seconds (floats), so browsers never handle i64 nanoseconds.
//!
//! # Security properties
//! - **Token.** `Authorization: Bearer <token>`, compared in constant time ([`Token::verify`]);
//!   read-only requests may use `?token=` instead (browser WebSockets cannot set headers), control
//!   requests may not. Missing, wrong or expired token: `401`, before anything about streams or
//!   the device is revealed.
//! - **CORS.** No `Access-Control-Allow-*` header is ever sent; `OPTIONS` preflights answer `403`;
//!   a mutating request whose `Origin` host differs from `Host` / `X-Forwarded-Host` answers `403`
//!   ([`crate::control`] has the details, including the cloudflared tunnel).
//! - **Methods.** GET, POST, PUT, DELETE and OPTIONS are parsed; anything else is `405`. A known
//!   path with the wrong method is `405` with `Allow`.
//! - **Bind address.** Loopback by default (the caller's choice; `hk serve` defaults to
//!   `127.0.0.1`). `0.0.0.0` exposes the API on every interface: the token is then the only
//!   protection and travels in cleartext (no TLS in M0; the cloudflared tunnel adds TLS).
//! - **Own-key content is never served**: every bridge consumer is `Locality::Remote`.
//! - **Inventory identities are gated by the model**: `/api/inventory` reads only
//!   `Repository::query_inventory` with the default `IdentityAccess::Standard`.
//! - **Receive only.** No route reaches a transmit path (C37 gated).
//! - **Bounded resources.** At most `max_connections` connection threads; request heads are
//!   limited to 16 KiB, bodies to 64 KiB, and both must arrive within `request_timeout`; query
//!   results are capped.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use hk_model::{Repository, Timestamp};
use hk_store::{FloorProduct, Pyramid};
use hk_stream::StreamError;
use serde_json::{Value, json};

use crate::auth::Token;
use crate::bridge::{self, StreamRegistry};
use crate::control::{self, AuditLog, Caller, CtlRequest, RunControl};
use crate::query::{self, ApiError};

/// Largest request head accepted.
const MAX_HEAD: usize = 16 * 1024;
/// Largest request body accepted.
const MAX_BODY: usize = 64 * 1024;
/// Largest static file served.
const MAX_STATIC: u64 = 32 * 1024 * 1024;

/// Every route the server answers (method, path; `{…}` is a path parameter). Receive only:
/// there is no transmit route.
pub const ROUTES: &[(&str, &str)] = &[
    ("GET", "/api/streams"),
    ("GET", "/api/history"),
    ("GET", "/api/floor"),
    ("GET", "/api/inventory"),
    ("GET", "/api/inventory/{id}"),
    ("POST", "/api/inventory/{id}/promote"),
    ("DELETE", "/api/inventory/{id}"),
    ("PUT", "/api/inventory/{id}/band"),
    ("DELETE", "/api/inventory/{id}/band"),
    // T-159 latest decode fields
    ("GET", "/api/inventory/{id}/decode"),
    ("GET", "/api/analysis/strongest"),
    ("GET", "/api/status"),
    ("GET", "/api/control/state"),
    ("POST", "/api/control/center"),
    ("POST", "/api/control/rate"),
    ("POST", "/api/control/gains"),
    ("POST", "/api/control/bias_tee"),
    ("POST", "/api/control/baseband_filter"),
    ("POST", "/api/control/display"),
    ("POST", "/api/control/pause"),
    ("POST", "/api/control/resume"),
    ("POST", "/api/control/record/start"),
    ("POST", "/api/control/record/stop"),
    ("GET", "/api/bookmarks"),
    ("POST", "/api/bookmarks"),
    ("GET", "/api/bookmarks/{id}"),
    ("PUT", "/api/bookmarks/{id}"),
    ("DELETE", "/api/bookmarks/{id}"),
    ("GET", "/api/selections"),
    ("POST", "/api/selections"),
    ("GET", "/api/selections/{id}"),
    ("PUT", "/api/selections/{id}"),
    ("DELETE", "/api/selections/{id}"),
    ("POST", "/api/selections/{id}/links"),
    ("GET", "/api/outputs"),
    ("POST", "/api/outputs/record/start"),
    ("POST", "/api/outputs/record/stop"),
    ("GET", "/api/outputs/{id}/files/{name}"),
    // T-190 analyze stub
    ("POST", "/api/analyze"),
    // T-157 rolling IQ capture buffer
    ("GET", "/api/iqbuffer"),
    ("POST", "/api/iqbuffer/clip"),
    ("GET", "/ws/{stream_id}"),
    ("GET", "/ws/open/{name}"),
    // Decoder workbench (ADR-0011 §7): each task appends its rows under its own marker.
    // T-088 recipes and pipelines
    ("GET", "/api/blocks"),
    ("GET", "/api/recipes"),
    ("POST", "/api/recipes"),
    ("POST", "/api/recipes/validate"),
    ("GET", "/api/recipes/{id}"),
    ("DELETE", "/api/recipes/{id}"),
    ("GET", "/api/recipes/{id}/versions/{version}"),
    ("GET", "/api/pipelines"),
    ("POST", "/api/pipelines"),
    ("GET", "/api/pipelines/{id}"),
    ("DELETE", "/api/pipelines/{id}"),
    ("PUT", "/api/pipelines/{id}/recipe"),
    ("POST", "/api/pipelines/{id}/save"),
    ("PUT", "/api/pipelines/{id}/channels"),
    ("POST", "/api/pipelines/{id}/channels/refresh"),
    // T-089 inspector
    ("POST", "/api/inspector/parse"),
    ("POST", "/api/captures/{id}/parse"),
    // T-091 assist
    ("POST", "/api/assist/sync"),
    ("POST", "/api/assist/fields"),
    ("POST", "/api/assist/crc"),
    // T-092 captures
    ("GET", "/api/captures"),
    ("GET", "/api/captures/{id}"),
    ("DELETE", "/api/captures/{id}"),
    ("GET", "/api/captures/{id}/frames"),
    // Attention + memory (ADR-0012 §11): each M2 task appends its rows under its own marker.
    // T-115 observations
    ("GET", "/api/observations"),
    ("GET", "/api/observations/coverage"),
    // T-118 occupancy
    ("GET", "/api/occupancy"),
    ("GET", "/api/channels"),
    // T-119 sites, baselines, candidates, weights
    ("GET", "/api/sites"),
    ("GET", "/api/sites/current"),
    ("PUT", "/api/sites/current"),
    ("PUT", "/api/sites/{id}"),
    ("GET", "/api/baselines"),
    ("GET", "/api/baselines/slots"),
    ("POST", "/api/baselines/refreeze"),
    ("GET", "/api/candidates"),
    ("GET", "/api/attention/weights"),
    ("PUT", "/api/attention/weights"),
    // T-120 scheduler
    // T-127 scheduler routes
    ("GET", "/api/scheduler"),
    ("GET", "/api/scheduler/arms"),
    ("GET", "/api/scheduler/leases"),
    ("POST", "/api/scheduler/leases"),
    ("DELETE", "/api/scheduler/leases/{id}"),
    // T-121 reports
    ("GET", "/api/report"),
    // T-122 anomalies
    ("GET", "/api/anomalies"),
    ("GET", "/api/anomalies/{id}"),
    ("POST", "/api/anomalies/{id}/dismiss"),
    ("POST", "/api/anomalies/{id}/reopen"),
];

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
    /// Time allowed for a request head (and body) to arrive.
    pub request_timeout: Duration,
    /// On-demand streams (`/ws/open/<name>`, T-066): how often the server pings the peer.
    pub ondemand_ping_interval: Duration,
    /// On-demand streams: a peer that sends nothing (no pong) for this long is dropped with its
    /// session (half-open connections, vanished tunnel clients).
    pub ondemand_peer_timeout: Duration,
    /// Address of the TCP stream server ([`crate::tcp`], T-060), reported by `/api/streams`.
    pub stream_tcp: Option<SocketAddr>,
}

impl ServerConfig {
    /// Defaults: 64 connections, 10 s request timeout, no static files, no TCP stream server,
    /// on-demand pings every 5 s with a 20 s peer timeout.
    pub fn new(bind: SocketAddr, token: Token) -> Self {
        Self {
            bind,
            token,
            ui_dist: None,
            max_connections: 64,
            request_timeout: Duration::from_secs(10),
            ondemand_ping_interval: Duration::from_secs(5),
            ondemand_peer_timeout: Duration::from_secs(20),
            stream_tcp: None,
        }
    }
}

/// What the endpoints read and control. History stores are shared with their ingest thread.
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
    /// Live front-end control (T-042, [`crate::live_control`]); `None` for replays and
    /// scheduler-driven runs (device endpoints then answer 409 `not_live`).
    pub live_control: Option<Arc<dyn crate::live_control::LiveControl>>,
    /// Display, pause and recording control of the running pipeline (T-050).
    pub run_control: Option<Arc<dyn RunControl>>,
    /// Bookmark store (T-050), usually the run's database.
    pub bookmarks: Option<Arc<Mutex<Repository>>>,
    /// Control audit log (T-050). Without one, every mutating endpoint answers 503.
    pub audit: Option<Arc<AuditLog>>,
    /// On-demand streams served at `/ws/open/<name>` ([`crate::ondemand`]), e.g. `listen` (T-043).
    pub on_demand: hk_stream::OpenerRegistry,
    /// Output recordings (T-061, [`crate::outputs`]); `None` answers 503.
    pub outputs: Option<Arc<dyn crate::outputs::OutputControl>>,
    /// Recorded decoded streams (T-089 reader interface, T-092 store) for
    /// `POST /api/captures/{id}/parse` ([`crate::inspector`]); `None` answers 503.
    pub captures: Option<Arc<dyn hk_stream::inspector::CaptureSource>>,
    /// Decoder-workbench recipe runtime (T-088, [`crate::recipes`]); `None` answers 503.
    pub recipes: Option<Arc<dyn crate::recipes::RecipeControl>>,
    /// Occupancy engine (T-118, [`crate::occupancy`]); `None` answers 503.
    pub occupancy: Option<Arc<dyn crate::occupancy::OccupancyControl>>,
    /// T-115: the observation log for `/api/observations` ([`crate::observations`]); `None`
    /// answers 503.
    pub observations: Option<hk_store::observation::ObservationStore>,
    /// T-119: sites, baselines, candidates and score weights ([`crate::attention`]); `None`
    /// answers 503.
    pub attention: Option<Arc<dyn crate::attention::AttentionControl>>,
    /// T-127: the run's scheduler for `/api/scheduler*` ([`crate::schedule`]); `None` answers
    /// reads with `"scheduler": null` and refuses lease changes.
    pub scheduler: Option<Arc<dyn crate::schedule::SchedulerControl>>,
    /// T-121: survey reports for `/api/report` ([`crate::reports`]); `None` answers 503.
    pub reports: Option<Arc<dyn crate::reports::ReportControl>>,
    /// T-122: anomalies and novelty alarms for `/api/anomalies*` ([`crate::anomalies`]); `None`
    /// answers 503.
    pub anomalies: Option<Arc<dyn crate::anomalies::AnomalyControl>>,
    /// T-157: the rolling IQ capture buffer for `/api/iqbuffer*` ([`crate::iqbuffer`]); `None`
    /// answers 503.
    pub iq_buffer: Option<Arc<dyn crate::iqbuffer::IqBufferControl>>,
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
    stream_server: Option<crate::tcp::StreamServer>,
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
            stream_server: None,
        })
    }

    /// The bound address (useful with port 0).
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Keeps a TCP stream server ([`crate::tcp`]) alive for as long as this server.
    pub fn attach_stream_server(&mut self, server: crate::tcp::StreamServer) {
        self.stream_server = Some(server);
    }

    /// The attached TCP stream server.
    pub fn stream_server(&self) -> Option<&crate::tcp::StreamServer> {
        self.stream_server.as_ref()
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

/// A parsed request.
struct Request {
    method: String,
    path: String,
    query: Vec<(String, String)>,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
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

    fn bearer(&self) -> Option<&str> {
        self.header("authorization").and_then(|v| {
            v.strip_prefix("Bearer ")
                .or_else(|| v.strip_prefix("bearer "))
                .map(str::trim)
        })
    }

    /// The header token (only) verifies.
    fn authorized_by_header(&self, token: &Token) -> bool {
        self.bearer().is_some_and(|b| token.verify(b))
    }

    /// The header token, or else the query token, verifies.
    fn authorized(&self, token: &Token) -> bool {
        match (self.bearer(), self.param("token")) {
            (Some(b), _) => token.verify(b),
            (None, Some(q)) => token.verify(q),
            (None, None) => false,
        }
    }

    /// Only GET reads; everything else changes state.
    fn mutating(&self) -> bool {
        self.method != "GET"
    }

    /// A browser `Origin` that names another host than the one addressed. `X-Forwarded-Host`
    /// counts only from a loopback peer (the local cloudflared tunnel or proxy).
    fn cross_origin(&self, loopback_peer: bool) -> bool {
        let Some(origin) = self.header("origin") else {
            return false;
        };
        let Some((_, authority)) = origin.trim().split_once("://") else {
            return true; // `null` or malformed
        };
        let authority = authority.trim_end_matches('/');
        let forwarded = self.header("x-forwarded-host").filter(|_| loopback_peer);
        ![forwarded, self.header("host")]
            .into_iter()
            .flatten()
            .filter_map(|h| h.split(',').next())
            .any(|h| h.trim().eq_ignore_ascii_case(authority))
    }
}

/// Reads the head and body. The error is the status to answer.
fn read_request(stream: &mut TcpStream) -> Result<Request, u16> {
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 4096];
    let head_len = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > MAX_HEAD {
            return Err(431);
        }
        let n = stream.read(&mut chunk).map_err(|_| 408u16)?;
        if n == 0 {
            return Err(400);
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    if head_len > MAX_HEAD {
        return Err(431);
    }
    let mut req = parse_request(&buf[..head_len])?;
    if req.header("transfer-encoding").is_some() {
        return Err(411);
    }
    let len = match req.header("content-length") {
        None => 0,
        Some(v) => v.trim().parse::<usize>().map_err(|_| 400u16)?,
    };
    if len > MAX_BODY {
        return Err(413);
    }
    let mut body = buf.split_off(head_len);
    while body.len() < len {
        let n = stream.read(&mut chunk).map_err(|_| 408u16)?;
        if n == 0 {
            return Err(400);
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(len);
    req.body = body;
    Ok(req)
}

fn parse_request(head: &[u8]) -> Result<Request, u16> {
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut req = httparse::Request::new(&mut headers);
    match req.parse(head) {
        Ok(httparse::Status::Complete(_)) => {}
        _ => return Err(400),
    }
    let method = match req.method {
        Some(m @ ("GET" | "POST" | "PUT" | "DELETE" | "OPTIONS")) => m.to_owned(),
        _ => return Err(405),
    };
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
        method,
        path: percent_decode(path).ok_or(400u16)?,
        query: parse_query(raw_query).ok_or(400u16)?,
        headers,
        body: Vec::new(),
    })
}

pub(crate) fn parse_query(q: &str) -> Option<Vec<(String, String)>> {
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

pub(crate) fn percent_decode(s: &str) -> Option<String> {
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
        201 => "Created",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        409 => "Conflict",
        410 => "Gone",
        411 => "Length Required",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        426 => "Upgrade Required",
        431 => "Request Header Fields Too Large",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        507 => "Insufficient Storage",
        _ => "Internal Server Error",
    }
}

fn respond(stream: &mut TcpStream, status: u16, content_type: &str, extra: &str, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         Connection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\
         Referrer-Policy: no-referrer\r\nVary: Origin\r\n{extra}\r\n",
        reason(status),
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

fn respond_json_with(stream: &mut TcpStream, status: u16, body: &Value, extra: &str) {
    let bytes = serde_json::to_vec(body).unwrap_or_else(|_| b"{}".to_vec());
    let auth = if status == 401 {
        "WWW-Authenticate: Bearer\r\n"
    } else {
        ""
    };
    respond(
        stream,
        status,
        "application/json",
        &format!("{auth}{extra}"),
        &bytes,
    );
}

fn respond_json(stream: &mut TcpStream, status: u16, body: &Value) {
    respond_json_with(stream, status, body, "");
}

fn respond_error(stream: &mut TcpStream, status: u16, message: &str) {
    respond_json(stream, status, &json!({ "error": message }));
}

fn caller(stream: &TcpStream, req: &Request, token: &Token) -> Caller {
    caller_from(stream.peer_addr().ok(), req, token)
}

/// Forwarding headers (`CF-Connecting-IP`, `X-Forwarded-For`) are trusted only from a loopback
/// peer (cloudflared connects locally); from anyone else they are ignored.
fn caller_from(peer: Option<SocketAddr>, req: &Request, token: &Token) -> Caller {
    let loopback = peer.is_some_and(|a| a.ip().is_loopback());
    Caller {
        token_id: req.authorized_by_header(token).then(|| token.id()),
        peer: peer.map(|a| a.to_string()),
        forwarded_for: req
            .header("cf-connecting-ip")
            .or_else(|| req.header("x-forwarded-for"))
            .filter(|_| loopback)
            .map(|v| v.chars().take(128).collect()),
        origin: req.header("origin").map(|v| v.chars().take(256).collect()),
    }
}

fn loopback_peer(stream: &TcpStream) -> bool {
    stream.peer_addr().is_ok_and(|a| a.ip().is_loopback())
}

fn handle_connection(mut stream: TcpStream, shared: &Shared) {
    let _ = stream.set_read_timeout(Some(shared.config.request_timeout));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
    let _ = stream.set_nodelay(true);
    let req = match read_request(&mut stream) {
        Ok(r) => r,
        Err(status) => return respond_error(&mut stream, status, reason(status)),
    };
    let token = &shared.config.token;
    let state = &shared.state;
    if req.method == "OPTIONS" {
        // No preflight is ever granted: browsers then refuse cross-origin requests that carry
        // the token or a JSON body.
        return respond_error(
            &mut stream,
            403,
            "cross-origin requests are not allowed (no CORS)",
        );
    }
    let is_api = req.path.starts_with("/api/") || req.path.starts_with("/ws/");
    if is_api {
        let mutating = req.mutating();
        let authorized = if mutating {
            req.authorized_by_header(token)
        } else {
            req.authorized(token)
        };
        if !authorized {
            if mutating {
                let who = caller(&stream, &req, token);
                control::audit_refused(state, &req.method, &req.path, &who, 401, "unauthorized");
            }
            let message = if mutating && req.param("token").is_some() {
                "control requests need the token in the Authorization header"
            } else {
                "missing or invalid token"
            };
            return respond_error(&mut stream, 401, message);
        }
        if mutating && req.cross_origin(loopback_peer(&stream)) {
            let who = caller(&stream, &req, token);
            control::audit_refused(state, &req.method, &req.path, &who, 403, "cross-origin");
            return respond_error(&mut stream, 403, "cross-origin control request refused");
        }
    }
    if let Some(name) = req.path.strip_prefix("/ws/open/")
        && req.method == "GET"
    {
        return crate::ondemand::serve(
            stream,
            &shared.state.on_demand,
            name,
            &req.query,
            &req.headers,
            &shared.config,
        );
    }
    if let Some(id) = req.path.strip_prefix("/ws/") {
        if req.method != "GET" {
            return respond_json_with(
                &mut stream,
                405,
                &json!({ "error": "use GET" }),
                "Allow: GET\r\n",
            );
        }
        return websocket(stream, shared, &req, id);
    }
    if let Some((id, name)) = req
        .path
        .strip_prefix("/api/outputs/")
        .and_then(|rest| rest.split_once("/files/"))
    {
        if req.method != "GET" {
            return respond_json_with(
                &mut stream,
                405,
                &json!({ "error": "use GET" }),
                "Allow: GET\r\n",
            );
        }
        return output_file(&mut stream, state, id, name);
    }
    let ctl = CtlRequest {
        method: &req.method,
        path: &req.path,
        body: &req.body,
        content_type: req.header("content-type"),
        caller: caller(&stream, &req, token),
        query: &req.query,
    };
    if let Some(r) = control::route(state, &ctl)
        .or_else(|| crate::selections::route(state, &ctl))
        .or_else(|| crate::decode::route(state, &ctl)) // T-159; before inventory::route (see its docs)
        .or_else(|| crate::inventory::route(state, &ctl))
        .or_else(|| crate::outputs::route(state, &ctl))
        .or_else(|| crate::analyze::route(state, &ctl)) // T-190
        .or_else(|| crate::iqbuffer::route(state, &ctl)) // T-157
        // Decoder workbench (ADR-0011 §7): one line per owning task, pre-added by T-085.
        .or_else(|| crate::recipes::route(state, &ctl)) // T-088
        .or_else(|| crate::inspector::route(state, &ctl)) // T-089
        .or_else(|| crate::assist::route(state, &ctl)) // T-091
        .or_else(|| crate::captures::route(state, &ctl)) // T-092
        // Attention + memory (ADR-0012 §11): one line per owning task, pre-added by T-113.
        .or_else(|| crate::observations::route(state, &ctl)) // T-115
        .or_else(|| crate::occupancy::route(state, &ctl)) // T-118
        .or_else(|| crate::attention::route(state, &ctl)) // T-119
        .or_else(|| crate::schedule::route(state, &ctl)) // T-120
        .or_else(|| crate::reports::route(state, &ctl)) // T-121
        .or_else(|| crate::anomalies::route(state, &ctl))
    // T-122
    {
        let allow = r
            .allow
            .map(|a| format!("Allow: {a}\r\n"))
            .unwrap_or_default();
        return respond_json_with(&mut stream, r.status, &r.body, &allow);
    }
    let get = req.method == "GET";
    let result = match req.path.as_str() {
        "/api/streams"
        | "/api/history"
        | "/api/floor"
        | "/api/inventory"
        | "/api/analysis/strongest"
        | "/api/report"
        | "/api/status"
            if !get =>
        {
            return respond_json_with(
                &mut stream,
                405,
                &json!({ "error": "use GET" }),
                "Allow: GET\r\n",
            );
        }
        "/api/streams" => Ok(bridge::discovery_json(
            &state.streams,
            &state.on_demand,
            shared.config.stream_tcp,
        )),
        "/api/history" => match with_history(state, |p| query::history_export(p, &req.query)) {
            Ok(Some((content_type, body))) => {
                return respond(&mut stream, 200, content_type, "", &body);
            }
            Ok(None) => history(state, &req),
            Err(e) => Err(e),
        },
        // T-121: the report document, or its CSV/PNG export.
        "/api/report" => match crate::reports::serve(state, &req.query) {
            Ok(crate::reports::Served::Export(content_type, body)) => {
                return respond(&mut stream, 200, content_type, "", &body);
            }
            Ok(crate::reports::Served::Json(v)) => Ok(v),
            Err(e) => Err(e),
        },
        "/api/floor" => floor(state, &req),
        "/api/inventory" => inventory(state, &req),
        "/api/analysis/strongest" => strongest(state, &req),
        "/api/status" => state
            .status
            .as_ref()
            .map(|f| f())
            .ok_or_else(|| ApiError::new(404, "no pipeline status")),
        p if p.starts_with("/api/") => Err(ApiError::new(404, "no such endpoint")),
        _ if !get => {
            return respond_json_with(
                &mut stream,
                405,
                &json!({ "error": "use GET" }),
                "Allow: GET\r\n",
            );
        }
        _ => return static_file(&mut stream, shared.config.ui_dist.as_deref(), &req.path),
    };
    match result {
        Ok(v) => respond_json(&mut stream, 200, &v),
        Err(e) => respond_error(&mut stream, e.status, &e.message),
    }
}

/// Streams an output file (T-061) without loading it into memory.
fn output_file(stream: &mut TcpStream, state: &ApiState, id: &str, name: &str) {
    let Some(outputs) = state.outputs.as_ref() else {
        return respond_error(stream, 503, "no output recorder on this server");
    };
    let path = match outputs.file(id, name) {
        Ok(p) => p,
        Err(f) => return respond_error(stream, f.status, &f.message),
    };
    let Ok(file) = std::fs::File::open(&path) else {
        return respond_error(stream, 404, "no such file");
    };
    let len = file.metadata().map_or(0, |m| m.len());
    let content_type = match path.extension().and_then(|e| e.to_str()) {
        Some("wav") => "audio/wav",
        Some("json" | "sigmf-meta") => "application/json",
        Some("jsonl") => "application/x-ndjson",
        _ => "application/octet-stream",
    };
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {len}\r\n\
         Content-Disposition: attachment; filename=\"{name}\"\r\nConnection: close\r\n\
         Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\r\n"
    );
    if stream.write_all(head.as_bytes()).is_ok() {
        let _ = io::copy(&mut file.take(len), stream);
    }
}

/// Runs `f` on the server's spectrum history: the history store, else the floor product's
/// uncalibrated pyramid.
fn with_history<T>(
    state: &ApiState,
    f: impl FnOnce(&hk_store::Pyramid) -> Result<T, ApiError>,
) -> Result<T, ApiError> {
    if let Some(p) = &state.history {
        let p = p
            .lock()
            .map_err(|_| ApiError::new(500, "history store poisoned"))?;
        return f(&p);
    }
    if let Some(fl) = &state.floor {
        let fl = fl
            .lock()
            .map_err(|_| ApiError::new(500, "floor store poisoned"))?;
        return f(fl.uncalibrated_pyramid());
    }
    Err(ApiError::new(404, "no spectrum history on this server"))
}

fn history(state: &ApiState, req: &Request) -> Result<Value, ApiError> {
    with_history(state, |p| query::history_json(p, &req.query))
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

/// `/api/analysis/strongest` (T-079): the same spectrum-history source as `/api/history`. The
/// window ends at the stream time the history has reached (a replay or a time-compressed scene
/// runs on its own clock, T-125); the wall clock only before any frame.
fn strongest(state: &ApiState, req: &Request) -> Result<Value, ApiError> {
    let now = |p: &Pyramid| p.latest_frame_end().unwrap_or_else(Timestamp::now);
    if let Some(p) = &state.history {
        let p = p
            .lock()
            .map_err(|_| ApiError::new(500, "history store poisoned"))?;
        return query::strongest_json(&p, &req.query, now(&p));
    }
    if let Some(f) = &state.floor {
        let f = f
            .lock()
            .map_err(|_| ApiError::new(500, "floor store poisoned"))?;
        let p = f.uncalibrated_pyramid();
        return query::strongest_json(p, &req.query, now(p));
    }
    Err(ApiError::new(404, "no spectrum history on this server"))
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
    fn only_known_methods_are_parsed() {
        for m in ["PATCH", "HEAD", "TRACE", "CONNECT", "TX", "get"] {
            assert_eq!(
                parse_request(format!("{m} /api/streams HTTP/1.1\r\n\r\n").as_bytes()).err(),
                Some(405),
                "{m}"
            );
        }
        for m in ["GET", "POST", "PUT", "DELETE", "OPTIONS"] {
            let r =
                parse_request(format!("{m} /api/streams HTTP/1.1\r\nHost: a\r\n\r\n").as_bytes())
                    .unwrap();
            assert_eq!(r.method, m);
        }
        assert!(parse_request(b"GET /api/streams?token=x HTTP/1.1\r\nHost: a\r\n\r\n").is_ok());
    }

    fn with_headers(headers: &[(&str, &str)]) -> Request {
        Request {
            method: "POST".into(),
            path: "/api/control/center".into(),
            query: Vec::new(),
            headers: headers
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            body: Vec::new(),
        }
    }

    #[test]
    fn origin_must_match_the_addressed_host() {
        assert!(
            !with_headers(&[("Host", "127.0.0.1:8787")]).cross_origin(true),
            "no Origin"
        );
        assert!(
            !with_headers(&[
                ("Host", "127.0.0.1:8787"),
                ("Origin", "http://127.0.0.1:8787")
            ])
            .cross_origin(false)
        );
        assert!(
            !with_headers(&[
                ("Host", "abc.trycloudflare.com"),
                ("Origin", "https://abc.trycloudflare.com")
            ])
            .cross_origin(true),
            "through the tunnel"
        );
        let proxied = with_headers(&[
            ("Host", "localhost:8787"),
            ("X-Forwarded-Host", "abc.example.org"),
            ("Origin", "https://abc.example.org"),
        ]);
        assert!(
            !proxied.cross_origin(true),
            "a loopback proxy that rewrites Host but forwards it"
        );
        assert!(
            proxied.cross_origin(false),
            "X-Forwarded-Host from a non-loopback peer is ignored"
        );
        assert!(
            with_headers(&[
                ("Host", "127.0.0.1:8787"),
                ("Origin", "https://evil.example")
            ])
            .cross_origin(true)
        );
        assert!(with_headers(&[("Host", "127.0.0.1:8787"), ("Origin", "null")]).cross_origin(true));
    }

    #[test]
    fn forwarding_headers_are_trusted_only_from_loopback_peers() {
        let token = Token::from_config("0123456789abcdef").unwrap();
        let req = with_headers(&[
            ("Host", "127.0.0.1:8787"),
            ("X-Forwarded-For", "203.0.113.9"),
            ("CF-Connecting-IP", "198.51.100.7"),
        ]);
        let local = caller_from(Some("127.0.0.1:50000".parse().unwrap()), &req, &token);
        assert_eq!(local.forwarded_for.as_deref(), Some("198.51.100.7"));
        assert_eq!(local.client_key(), "198.51.100.7");
        let v6 = caller_from(Some("[::1]:50000".parse().unwrap()), &req, &token);
        assert_eq!(v6.forwarded_for.as_deref(), Some("198.51.100.7"));
        let remote = caller_from(Some("192.0.2.44:50000".parse().unwrap()), &req, &token);
        assert_eq!(
            remote.forwarded_for, None,
            "a remote peer cannot claim a client"
        );
        assert_eq!(remote.client_key(), "192.0.2.44");
        assert_eq!(remote.token_id, None);
    }
}
