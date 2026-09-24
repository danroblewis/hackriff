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
//! | `/api/timeline?[f_lo&f_hi][&columns][&rows]` | GET | token | T-338 the capture window (the IQ ring's retention, **not** the history horizon) and the compressed overview waterfall drawn on it ([`crate::timeline`]) |
//! | `/api/tiles?level_f&level_t&f_index&t_index[&scheme][&device][&cells]` | GET | token | T-438 one tile of the unified surface, addressed by **independent** `(level_f, level_t)` ([`crate::tiles`]) |
//! | `/api/tiles/events?…` | GET | token | T-438 the coarse-zoom event aggregate on the same address — a count per cell, never a tile channel (`docs/16` §5.3) |
//! | `/api/floor?f_lo&f_hi&t0&t1[&max_steps]` | GET | token | T-021 floor vs time ([`crate::query`]) |
//! | `/api/inventory?[f_lo&f_hi][&t0&t1][&state][&status][&tag][&scheme][&family][&cursor][&limit]` | GET | token | T-018 signal inventory, identity-gated ([`crate::query::inventory_json`]); `state` = T-078 lifecycle |
//! | `/api/inventory/<id>[/promote\|/band]` | GET, POST, PUT, DELETE | token (header only for mutating) | T-078 one entry, promote a candidate, delete; T-191 set/clear the user band ([`crate::inventory`]) |
//! | `/api/inventory/<id>/decode` | GET | token | T-159 the emitter's latest decode fields, one row per decoder/frame-model ([`crate::decode`]) |
//! | `/api/inventory/<id>/classification` | GET | token | T-247 the emitter's full C15 classification: posterior, likelihood, prior, provenance, reasons ([`crate::classification`]) |
//! | `/api/events?f_lo&f_hi&t0&t1[&state][&limit][&cursor]` | GET | token | T-264 (ADR-0017 TM-8) the durable catalogue of events in a region over a time range, with coverage ([`crate::events`]) |
//! | `/api/inventory/<id>/presence[?t0&t1]` | GET | token | T-264 one emitter's presence track: every interval with its own timespan ([`crate::presence`]) |
//! | `/api/analysis/strongest?f_lo&f_hi[&window_s]` | GET | token | T-079 strongest observed signal in a band over a recent window, from spectrum history ([`crate::query::strongest_json`]) |
//! | `/api/navigation[?center_hz&span_hz[&t_cell_s]]` | GET | token | T-341 the achievable `(centre, span)` grid (ranges, tuning step, spans, history tiers) and, for a requested state, the nearest realizable one plus its live-IQ/overview claim ([`crate::navigation`]) |
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
//! | `/api/collections[/<id>[/markers]]`, `/api/markers[/<id>]` | GET, POST, PUT, DELETE | token (header only for mutating) | T-817 marker collections ([`crate::collections`]) |
//! | `/api/selections[/<id>[/links]]` | GET, POST, PUT, DELETE | token (header only for mutating) | T-052 persisted region selections ([`crate::selections`]) |
//! | `/api/measurements[/<id>]` | GET, POST, PUT, DELETE | token (header only for mutating) | T-818 saved measurements: cursors in, server-computed value+unit+place out, server-stamped provenance ([`crate::measurements`]) |
//! | `/api/annotations[/<id>]` | GET, POST, PUT, DELETE | token (header only for mutating) | T-816 human-authored time–frequency annotations with a server-stamped provenance ([`crate::annotations`]) |
//! | `/api/views[/<id>]` | GET, POST, PUT, DELETE | token (header only for mutating) | T-819 saved views: named, restorable (time × frequency) window extents; view-arithmetic state, never a device command ([`crate::views`]) |
//! | `/api/selections/<id>/watch` | GET | token | T-166 the selection's region-watch alerts and the activity it did not alert on, with reasoning ([`crate::selections`]) |
//! | `/api/outputs[/record/start\|/record/stop]`, `/api/outputs/<id>/files/<name>` | GET, POST | token (header only for mutating) | T-061 output recordings and downloads ([`crate::outputs`]) |
//! | `/api/analyze` | POST | token | T-190 stub: validates a selection/emitter/band target, answers `501 not_implemented` until MAUTO fills it in ([`crate::analyze`]) |
//! | `/api/iqbuffer[?…]`, `/api/iqbuffer/clip` | GET, POST | token (header only for mutating) | T-157 rolling IQ capture buffer and clip export ([`crate::iqbuffer`]) |
//! | `/api/datasets[/<id>]` | GET, POST | token (header only for mutating) | T-205 labelled-capture dataset export (CRC-valid decodes and user labels) ([`crate::datasets`]) |
//! | `/api/taxonomy` | GET | token | T-218 the modulation taxonomy `hk-mod@1` and `thresholds@1`, as data ([`crate::taxonomy`]). Reference data, never a measurement |
//! | `/api/signatures/match` | GET | token | T-201 an emitter's C18 signature match and its history ([`crate::signatures`]). Ranked evidence, never an identity |
//! | `/api/clusters[/<id>[/promote]]` | GET, POST | token (header only for mutating) | T-202 C18 clusters of unknown emissions — "the same thing I saw before" ([`crate::clusters`]). A *type* above emitters; evidence, never an identity |
//! | `/api/ml/models`, `/api/ml/models/<id>/mode`, `/api/ml/shadow[?…]` | GET, PUT | token (header only for mutating) | T-844 C38 model registry and `(model, consumer)` modes (audited; `active` needs §4.6 evidence or `force`), and the durable shadow log's per-SNR agreement ([`crate::ml`]). Shadow never decides |
//! | `/ws/<stream_id>` | GET | token | WebSocket bridge ([`crate::bridge`]) |
//! | `/ws/open/<name>?…` | GET | token | On-demand stream, e.g. `listen` (T-043, [`crate::ondemand`]) |
//! | `/ws/tiles/rows?…` | GET | token | Rows pushed over a tile-lattice address range (T-468, [`crate::rows`]) |
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

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

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
    // T-247 the emitter's full C15 classification (ADR-0016 §2/§9)
    ("GET", "/api/inventory/{id}/classification"),
    // T-264 (ADR-0017 TM-8) the History surface: the durable catalogue of events, and one
    // emitter's presence track
    ("GET", "/api/events"),
    ("GET", "/api/inventory/{id}/presence"),
    ("GET", "/api/analysis/strongest"),
    // T-341: the achievable (centre, span) grid, and which tier answers for a requested state
    ("GET", "/api/navigation"),
    // T-338: the capture window (the IQ ring's retention, not the history horizon) and the
    // compressed overview waterfall drawn on it
    ("GET", "/api/timeline"),
    // T-368: the coverage map - which front end actually sampled which frequency, so a view greys
    // only what was never observed
    ("GET", "/api/coverage"),
    // T-438: one tile of the unified surface, addressed by INDEPENDENT (level_f, level_t), and the
    // coarse-zoom event aggregate that a tile deliberately does not carry (docs/16 §5.3)
    ("GET", "/api/tiles"),
    ("GET", "/api/tiles/events"),
    // T-469: the persisted IQ recordings that extend the audio horizon past the IQ ring
    ("GET", "/api/recordings"),
    // T-463: the one playhead of historical playback (view state over recorded history; audio at
    // it is the `playback` on-demand opener)
    ("GET", "/api/playback"),
    ("POST", "/api/playback"),
    ("GET", "/api/status"),
    ("GET", "/api/control/state"),
    ("POST", "/api/control/center"),
    ("POST", "/api/control/rate"),
    // T-529: centre AND rate as one device action, because a user retune names a whole capture
    // configuration and committing the halves separately commands a window nobody asked for.
    ("POST", "/api/control/window"),
    ("POST", "/api/control/gains"),
    ("POST", "/api/control/bias_tee"),
    ("POST", "/api/control/baseband_filter"),
    ("POST", "/api/control/display"),
    // T-452: the in-app survey sweep. GET reports where it is and prices a proposed one without
    // starting it; POST starts or resumes it; stop is its own path so it can never be mistaken for
    // start, and is never refused.
    ("GET", "/api/control/scan"),
    ("POST", "/api/control/scan"),
    ("POST", "/api/control/scan/stop"),
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
    ("GET", "/api/selections/{id}/watch"),
    // T-818 MAP-18 saved measurements
    ("GET", "/api/measurements"),
    ("POST", "/api/measurements"),
    ("GET", "/api/measurements/{id}"),
    ("PUT", "/api/measurements/{id}"),
    ("DELETE", "/api/measurements/{id}"),
    // T-816 MAP-16 human-authored annotations
    ("GET", "/api/annotations"),
    ("POST", "/api/annotations"),
    ("GET", "/api/annotations/{id}"),
    ("PUT", "/api/annotations/{id}"),
    ("DELETE", "/api/annotations/{id}"),
    // T-819 MAP-19 saved views
    ("GET", "/api/views"),
    ("POST", "/api/views"),
    ("GET", "/api/views/{id}"),
    ("PUT", "/api/views/{id}"),
    ("DELETE", "/api/views/{id}"),
    // T-817 (MAP-17): time-frequency marker collections; /api/bookmarks is a facade over one.
    ("GET", "/api/collections"),
    ("POST", "/api/collections"),
    ("GET", "/api/collections/{id}"),
    ("PUT", "/api/collections/{id}"),
    ("DELETE", "/api/collections/{id}"),
    ("GET", "/api/collections/{id}/markers"),
    ("POST", "/api/collections/{id}/markers"),
    ("GET", "/api/markers"),
    ("GET", "/api/markers/{id}"),
    ("PUT", "/api/markers/{id}"),
    ("DELETE", "/api/markers/{id}"),
    ("GET", "/api/outputs"),
    ("POST", "/api/outputs/record/start"),
    ("POST", "/api/outputs/record/stop"),
    ("GET", "/api/outputs/{id}/files/{name}"),
    // T-190 analyze stub
    ("POST", "/api/analyze"),
    // T-157 rolling IQ capture buffer
    ("GET", "/api/iqbuffer"),
    ("POST", "/api/iqbuffer/clip"),
    // T-205 labelled-capture dataset export
    ("GET", "/api/datasets"),
    ("POST", "/api/datasets"),
    ("GET", "/api/datasets/{id}"),
    // T-218 classification reference data (ADR-0016 §1-§2)
    ("GET", "/api/taxonomy"),
    // T-201 C18 signature matches (ADR-0016 §5)
    ("GET", "/api/signatures/match"),
    // T-202 C18 clusters of unknown emissions (ADR-0016 §5)
    ("GET", "/api/clusters"),
    ("GET", "/api/clusters/{id}"),
    ("POST", "/api/clusters/{id}/promote"),
    // T-844 C38 model registry, modes and the durable shadow log (ADR-0016 §6, §9)
    ("GET", "/api/ml/models"),
    ("PUT", "/api/ml/models/{id}/mode"),
    ("GET", "/api/ml/shadow"),
    ("GET", "/ws/{stream_id}"),
    ("GET", "/ws/open/{name}"),
    // T-468 rows pushed to a subscription over an address range of the tile lattice
    ("GET", "/ws/tiles/rows"),
    // Decoder workbench (ADR-0011 §7): each task appends its rows under its own marker.
    // T-088 recipes and pipelines
    ("GET", "/api/blocks"),
    ("GET", "/api/recipes"),
    ("POST", "/api/recipes"),
    ("POST", "/api/recipes/validate"),
    // T-164 recipes ranked against an emitter's measured parameters
    ("GET", "/api/recipes/match"),
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
    // T-273 trunking load index (metadata only, AWARE-067)
    ("GET", "/api/trunking/load"),
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
    /// T-439: the **view-scheme** pyramid — `docs/16` §8.2's de-welded lattice, written by the
    /// live chain at its finest node. `/api/tiles?scheme=view` reads it; every other route reads
    /// [`Self::history`] / [`Self::floor`] as before.
    ///
    /// It is a *separate* handle, not a second pyramid inside the floor product, for the reason
    /// T-037b gave: `/api/history` and `/api/floor` hold the product's lock for a whole query, and
    /// the growing edge must not be behind that lock. `None` leaves the tile route folding out of
    /// scheme 1's welded ladder exactly as it did before T-439.
    pub view_history: Option<Arc<Mutex<Pyramid>>>,
    /// Calibrated floor product for `/api/floor`.
    pub floor: Option<Arc<Mutex<FloorProduct>>>,
    /// Signal inventory (C27, T-018) for `/api/inventory`. Read through `query_inventory` only.
    pub inventory: Option<Arc<Mutex<Repository>>>,
    /// Pipeline counters for `/api/status` (T-027): a snapshot builder, called per request.
    pub status: Option<StatusFn>,
    /// Live front-end control (T-042, [`crate::live_control`]): **every** front end this run
    /// holds, keyed by `device_id`, empty for replays and scheduler-driven runs (device endpoints
    /// then answer 409 `not_live`).
    ///
    /// T-511 made this a collection rather than one `Option`: the length is a fact about the run,
    /// measured per request, and each handle carries its own [`crate::DeviceGate`], so "one
    /// capture at a time" is per device rather than per server. Device routes resolve a selector
    /// through [`crate::LiveControls::select`], which may be omitted only when there is exactly
    /// one front end.
    pub live_controls: crate::live_control::LiveControls,
    /// T-452: the in-app survey sweep over one of [`Self::live_controls`] ([`crate::scan`]). `None` leaves
    /// `/api/control/scan*` answering 503 — the front end is there but nothing can sweep it.
    ///
    /// It is a *driver over the interactive retune path*, not a scheduler: `hk serve` still does
    /// not drive the scheduler, and every step is the same gated [`crate::DeviceAction::Retune`] a
    /// user's explicit tune is. See [`crate::scan`] for the decision and the arbitration rule.
    pub scan: Option<Arc<crate::scan::ScanRunner>>,
    /// Display and recording control of the running pipeline (T-050).
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
    /// T-273: the trunking store behind `GET /api/trunking/load` ([`crate::trunking`]), the
    /// metadata-only load index over the GrantEvent stream (AWARE-067); `None` answers 503.
    pub trunking: Option<Arc<Mutex<Repository>>>,
    /// T-122: anomalies and novelty alarms for `/api/anomalies*` ([`crate::anomalies`]); `None`
    /// answers 503.
    pub anomalies: Option<Arc<dyn crate::anomalies::AnomalyControl>>,
    /// T-157: the rolling IQ capture buffer for `/api/iqbuffer*` ([`crate::iqbuffer`]); `None`
    /// answers 503.
    pub iq_buffer: Option<Arc<dyn crate::iqbuffer::IqBufferControl>>,
    /// T-205: labelled-capture dataset export for `/api/datasets*` ([`crate::datasets`]); `None`
    /// answers 503.
    pub datasets: Option<Arc<dyn crate::datasets::DatasetControl>>,
    /// T-844: the C38 stage for `/api/ml/*` ([`crate::ml`]); `None` answers 503.
    pub ml: Option<Arc<dyn crate::ml::MlControl>>,
    /// T-469: the persisted IQ recordings behind `GET /api/recordings`
    /// ([`crate::recordings`]) - the half of the audio horizon the IQ ring is not. `None`
    /// answers 503.
    pub recordings: Option<Arc<dyn crate::recordings::RecordingCatalog>>,
    /// T-463: the one playhead behind `GET/POST /api/playback` ([`crate::playback`]); `None`
    /// answers 503.
    pub playback: Option<Arc<dyn crate::playback::PlaybackControl>>,
    /// T-166: the region watch behind `GET /api/selections/{id}/watch`
    /// ([`crate::selections::WatchControl`]); `None` answers 503.
    pub watch: Option<Arc<dyn crate::selections::WatchControl>>,
    /// T-438/T-630: who may have one of the tile route's in-flight slots — the
    /// ingest-backpressure cap of `docs/16` §5.5 (cap 3), plus the per-client fair share that
    /// decides whose request meets it ([`crate::tiles::TileAdmission`]).
    ///
    /// Per **state**, not a `static`: two servers in one process must not share a cap, and a cap
    /// that leaks across tests is a cap nobody can assert. Cloning the state shares the table,
    /// which is what makes it a server-wide cap rather than a per-request one.
    pub tile_admission: Arc<crate::tiles::TileAdmission>,
    /// T-572: the hot-tile LRU in front of `GET /api/tiles`.
    ///
    /// Per **state** and shared by cloning, exactly like [`Self::tile_admission`] and for the
    /// same reason: two servers in one process must not share a cache, and a cache that leaks
    /// across tests is a cache nobody can assert. `None` disables it entirely (the default for a
    /// state built by hand in a test that is not about caching), so every existing assertion
    /// about what a tile read costs still measures a real read.
    pub tile_cache: Option<Arc<crate::tiles::HotTileCache>>,
    /// T-468: `/ws/tiles/rows` subscriptions open now, capped at [`crate::rows::MAX_ROW_FEEDS`].
    /// Per state for the same reason as `tile_admission`.
    pub row_feeds: Arc<std::sync::atomic::AtomicUsize>,
    /// T-579: the per-lattice readable ceiling, memoised — a pure function of the store's
    /// geometry and config, so it is computed once per lattice rather than probed per request.
    /// Shared by cloning, like [`Self::tile_admission`].
    pub ceiling_memo: Arc<crate::tiles::CeilingMemo>,
    /// T-579: the tile coverage raster, memoised against the tune history it is computed from.
    /// Always on: its key is the evidence itself, so it cannot serve a stale grey.
    pub coverage_raster: Arc<crate::coverage::CoverageRasterMemo>,
}

/// Builds the `/api/status` JSON (counters only: no content, no identities).
pub type StatusFn = Arc<dyn Fn() -> Value + Send + Sync>;

/// How long [`Server::shutdown`] waits for in-flight connection threads after closing their
/// sockets (T-236). Closing the sockets first is what makes the usual wait sub-millisecond: a
/// handler blocked on a peer returns at once, so this budget only has to cover a handler in the
/// middle of *compute* (a query holding a store mutex) on a loaded machine. Two seconds is far
/// above any handler's measured work and still keeps teardown cheap across a suite that starts
/// hundreds of servers; past it the thread is abandoned, counted and reported rather than waited
/// on forever.
const SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// T-530: how long a `/ws/{stream_id}` handshake that lands **between windows** waits for the
/// successor publisher before answering `503 replumbing`.
///
/// It bounds one connection thread, not the client's patience: past it the client is told to retry
/// rather than held. Two seconds is an order of magnitude over the re-plumb gap of a run that is
/// not being swept (0.17 s after T-525) and well under [`hk_stream::BETWEEN_WINDOWS_GRACE`], so a
/// producer that never offers its successor degrades to `503` here and then, once that grace
/// expires, to the honest `410`.
///
/// **T-542: two seconds is not an upper bound on a re-plumb and was never meant to be.** Under a
/// 1 MHz–6 GHz sweep at dwell 1 s on the live HackRF, re-plumbs of 8–19 s are routine, so this
/// wait *ends in `503`* many times a minute — which is the designed outcome ("not now, retry"),
/// and is why [`hk_stream::BETWEEN_WINDOWS_GRACE`], not this, is the constant that had to move.
const REPLUMB_HANDSHAKE_WAIT: Duration = Duration::from_secs(2);

/// Connections accepted and not yet finished: one entry per handler thread, holding a cloned
/// socket handle that [`Server::shutdown`] closes to unblock it (T-236).
#[derive(Default)]
struct Conns {
    next: u64,
    open: BTreeMap<u64, TcpStream>,
}

struct Shared {
    config: ServerConfig,
    state: ApiState,
    /// Open connections. Locked only to register or retire one (never while a request is handled,
    /// and never by the capture or audio path) and by [`Server::shutdown`] while it drains.
    conns: Mutex<Conns>,
    /// Signalled when the last open connection retires.
    idle: Condvar,
    /// Connection threads still running when the bounded shutdown wait expired.
    abandoned: AtomicUsize,
}

fn lock_conns(shared: &Shared) -> std::sync::MutexGuard<'_, Conns> {
    shared.conns.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A running server. Dropping it (or calling [`Server::shutdown`]) stops accepting, closes every
/// open connection and waits — bounded by [`SHUTDOWN_DRAIN_TIMEOUT`] — for the connection threads
/// to finish, so nothing is still reading or writing the run's files once it returns (T-236).
pub struct Server {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    shared: Arc<Shared>,
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
            conns: Mutex::new(Conns::default()),
            idle: Condvar::new(),
            abandoned: AtomicUsize::new(0),
        });
        let stop_flag = Arc::clone(&stop);
        let accepting = Arc::clone(&shared);
        let thread = thread::Builder::new()
            .name("hk-api-accept".into())
            .spawn(move || accept_loop(listener, accepting, stop_flag))?;
        Ok(Self {
            addr,
            stop,
            thread: Some(thread),
            shared,
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

    /// Stops accepting, joins the accept thread, then closes every open connection and waits for
    /// its handler thread to finish, bounded by [`SHUTDOWN_DRAIN_TIMEOUT`] (T-236). After this
    /// returns, no connection thread is still touching the run's data directory (the audit log,
    /// the database and its WAL, the observation log, the IQ ring) — a caller may delete it.
    /// Idempotent, and called by `Drop`.
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
        self.drain_connections();
    }

    /// Connection threads this server abandoned at the shutdown deadline (cumulative). Zero unless
    /// a handler was stuck in compute: a stalled *peer* never counts here, because shutdown closes
    /// its socket instead of waiting for it.
    pub fn abandoned_connections(&self) -> usize {
        self.shared.abandoned.load(Ordering::SeqCst)
    }

    /// Closes every open connection's socket, then waits for its thread to retire itself.
    ///
    /// Only the shutdown path runs this; the capture and audio paths never touch `conns`, and no
    /// request handler holds this lock (it is taken around the spawn and around the guard's drop,
    /// never across `handle_connection`).
    fn drain_connections(&self) {
        let deadline = Instant::now() + SHUTDOWN_DRAIN_TIMEOUT;
        let mut conns = lock_conns(&self.shared);
        // Closing the sockets first is what bounds this in practice: a handler blocked reading a
        // stalled client's request head, or parked in `bridge::watch_peer` on a WebSocket peer
        // that never closes, fails its read immediately instead of holding shutdown open for its
        // own (much longer) socket timeout. Queued response bytes are still flushed: this is a
        // shutdown, not an abort.
        for sock in conns.open.values() {
            let _ = sock.shutdown(Shutdown::Both);
        }
        while !conns.open.is_empty() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            // A retiring guard removes its entry under this same mutex before signalling, so a
            // wakeup can never be lost between the check above and this wait.
            let (guard, _) = self
                .shared
                .idle
                .wait_timeout(conns, left)
                .unwrap_or_else(PoisonError::into_inner);
            conns = guard;
        }
        let abandoned = conns.open.len();
        drop(conns);
        if abandoned > 0 {
            self.shared.abandoned.fetch_add(abandoned, Ordering::SeqCst);
            eprintln!(
                "hk-api: shutdown abandoned {abandoned} connection thread(s) still running after \
                 {SHUTDOWN_DRAIN_TIMEOUT:?} (their sockets are closed)"
            );
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Retires a connection when its handler thread ends: drops its socket from the registry and wakes
/// a [`Server::drain_connections`] waiting for the last one.
struct ActiveGuard(Arc<Shared>, u64);

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        let mut conns = lock_conns(&self.0);
        conns.open.remove(&self.1);
        let empty = conns.open.is_empty();
        drop(conns);
        if empty {
            self.0.idle.notify_all();
        }
    }
}

fn accept_loop(listener: TcpListener, shared: Arc<Shared>, stop: Arc<AtomicBool>) {
    for conn in listener.incoming() {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let Ok(stream) = conn else { continue };
        let guard = {
            let mut conns = lock_conns(&shared);
            if conns.open.len() >= shared.config.max_connections {
                continue; // dropped: closes the connection
            }
            // The clone is only ever used to close the socket at shutdown. A clone that fails (fd
            // exhaustion) drops the connection rather than leaving a handler shutdown can't reach.
            let Ok(sock) = stream.try_clone() else {
                continue;
            };
            let id = conns.next;
            conns.next += 1;
            conns.open.insert(id, sock);
            ActiveGuard(Arc::clone(&shared), id)
        };
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
    respond_cached(stream, status, content_type, "no-store", extra, body);
}

/// [`respond`] with the `Cache-Control` value the caller chooses, rather than the hard-coded
/// `no-store` every other route wants (T-574: only a sealed tile response earns anything else).
///
/// `Vary` names **both** axes in ONE header (T-700). `Origin` has always been here; T-533 added
/// content negotiation, and a second `Vary:` line beside the first is a cache-correctness bug
/// waiting to happen — a shared cache that reads one of them stores the gzipped body under a key
/// that a client refusing gzip can hit. It is stated on EVERY answer, not only the compressed one,
/// for the same reason: the response a cache is storing for a sealed tile (`immutable`,
/// `max-age=1y`) may legitimately be either form, so the key has to say so whichever arrived first.
fn respond_cached(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    cache_control: &str,
    extra: &str,
    body: &[u8],
) {
    let head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         Connection: close\r\nCache-Control: {cache_control}\r\nX-Content-Type-Options: nosniff\r\n\
         Referrer-Policy: no-referrer\r\nVary: Origin, Accept-Encoding\r\n{extra}\r\n",
        reason(status),
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

/// `GET /api/tiles` alone (T-574): sealed tiles are immutable by design (docs/16 §5.2/§5.3), so a
/// sealed response carries a content-derived ETag and `public, max-age=…, immutable`, and a
/// matching `If-None-Match` gets a bodyless 304. `sealed` is read from the tile's own JSON body —
/// `tiles.rs` derives it from the pyramid's watermark against the tile's own time extent, never
/// re-guessed here from age or from a timer — so a LIVE tile (the growing edge, `sealed: false`)
/// always keeps the existing `no-store` and is never given an ETag at all, which is what stops a
/// cache from ever answering it with a stale 304.
///
/// T-533: this route answers itself, so the generic tail's gzip never reaches it — and it is the
/// body the compression exists for. The coding is applied HERE, over the same bytes, and the ETag
/// is deliberately computed over the UNCOMPRESSED JSON: gzip is a transfer coding, so the
/// representation a cache is validating is the same tag whether or not this hop compressed it.
fn respond_tile(stream: &mut TcpStream, req: &Request, body: Value) {
    let sealed = body.get("sealed").and_then(Value::as_bool).unwrap_or(false);
    let bytes = serde_json::to_vec(&body).unwrap_or_else(|_| b"{}".to_vec());
    let gzip_ok = accepts_gzip(req);
    if !sealed {
        let (extra, out) = maybe_gzip(gzip_ok, "", &bytes);
        respond(stream, 200, "application/json", &extra, &out);
        return;
    }
    // Content-derived, not a timestamp — but `build_ms` and `in_flight` (both `cost` and
    // `shadow.search`, T-574) are THIS READ's own diagnostics, not the tile's content: wall clock
    // and concurrent in-flight count, which vary request to request even when a sealed tile's real
    // content is byte-identical. They are stripped out of what the ETag is computed over (and
    // still served verbatim in the body) so the tag reflects only the measurement, never the
    // measuring.
    let mut canonical = body.clone();
    strip_read_diagnostics(&mut canonical);
    let canonical_bytes = serde_json::to_vec(&canonical).unwrap_or_default();
    let etag = format!("\"{:08x}\"", crc32(&canonical_bytes));
    let cache_control = "public, max-age=31536000, immutable";
    let extra = format!("ETag: {etag}\r\n");
    if req
        .header("if-none-match")
        .is_some_and(|v| v.split(',').any(|part| part.trim() == etag))
    {
        respond_cached(stream, 304, "application/json", cache_control, &extra, &[]);
        return;
    }
    let (extra, out) = maybe_gzip(gzip_ok, &extra, &bytes);
    respond_cached(stream, 200, "application/json", cache_control, &extra, &out);
}

/// Gzip `bytes` when the caller accepts it and the body is big enough, returning the extra headers
/// to send with them (T-533). Borrowed body back unchanged when it is not worth it.
fn maybe_gzip<'a>(gzip_ok: bool, extra: &str, bytes: &'a [u8]) -> (String, Cow<'a, [u8]>) {
    if gzip_ok
        && bytes.len() >= GZIP_MIN_BYTES
        && let Some(z) = gzip(bytes)
    {
        // `Vary` is already stated, once, by `respond_cached` — see there.
        return (format!("{extra}Content-Encoding: gzip\r\n"), Cow::Owned(z));
    }
    (extra.to_string(), Cow::Borrowed(bytes))
}

/// `cost` fields T-630 added that describe the admission of THIS read, not the tile.
const T630_READ_FIELDS: [&str; 6] = [
    "in_flight_share",
    "in_flight_held",
    "clients",
    "client",
    "reserved",
    "fair_share",
];

/// Recursively nulls `build_ms`, `in_flight` and `served_from` wherever they appear (`cost` and
/// `shadow.search`, T-574/T-572) — this read's own timing, concurrency and provenance, never the
/// tile's content.
///
/// `served_from` matters as much as the other two: T-572's hot-tile cache adds it on a HIT and not
/// on a miss, so leaving it in what the ETag is hashed over would make a sealed tile's first
/// re-read a 200 instead of the 304 T-574 exists for — the cache would have broken the cache.
///
/// And, inside `cost`, T-630's admission fields ([`T630_READ_FIELDS`]): a second client reading
/// the same sealed tile under a different share must still get its 304.
fn strip_read_diagnostics(v: &mut Value) {
    match v {
        Value::Object(obj) => {
            // REMOVED, not nulled: `served_from` is present only on a cache hit, and a key that
            // is absent on one read and null on the next is still a different byte string — which
            // is a different ETag, which is a 200 where T-574 promises a 304.
            obj.remove("served_from");
            for (k, val) in obj.iter_mut() {
                if k == "build_ms" || k == "in_flight" {
                    *val = Value::Null;
                } else if k == "cost" {
                    // T-630's admission facts are this read's too — whose share it was admitted
                    // under and how busy the route was — and differ between two clients reading
                    // the same sealed tile. Nulled only inside `cost`, where they are defined.
                    if let Value::Object(cost) = val {
                        for f in T630_READ_FIELDS {
                            if let Some(x) = cost.get_mut(f) {
                                *x = Value::Null;
                            }
                        }
                    }
                    strip_read_diagnostics(val);
                } else {
                    strip_read_diagnostics(val);
                }
            }
        }
        Value::Array(arr) => {
            for item in arr {
                strip_read_diagnostics(item);
            }
        }
        _ => {}
    }
}

/// IEEE CRC-32 (the same polynomial `hk-store`'s tile codec uses), over the exact bytes the wire
/// sends — an ETag from the response's own content, not from its address or its age.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// Bodies at or above this are worth compressing (T-533).
///
/// Below it the gzip member's own header and trailer are a large share of what is sent and the
/// round trip is dominated by the request anyway; the bodies this exists for — a `/api/tiles` grid,
/// an `/api/history` overview — are two orders of magnitude above it.
const GZIP_MIN_BYTES: usize = 4096;

/// Does this request say it can read gzip?
///
/// `Accept-Encoding: gzip;q=0` is a client saying it **cannot**, and is honoured: a coding offered
/// at zero quality is explicitly refused (RFC 9110 §12.5.3), and sending it anyway would hand that
/// caller bytes it will not decode.
fn accepts_gzip(req: &Request) -> bool {
    req.header("accept-encoding").is_some_and(|v| {
        v.split(',').any(|part| {
            let mut it = part.split(';').map(str::trim);
            let coding = it.next().unwrap_or("");
            coding.eq_ignore_ascii_case("gzip")
                && !it.any(|p| {
                    p.strip_prefix("q=")
                        .is_some_and(|q| q.parse::<f32>().is_ok_and(|q| q <= 0.0))
                })
        })
    })
}

/// Gzip, or `None` when it did not help.
///
/// **Level 1, measured rather than chosen by taste.** A live 856 178 B `?planes=f16` tile body is
/// served at 117 382 B here; level 6 takes the same bytes to ~53 kB (measured offline) for several
/// times the CPU, on the thread the caller is waiting on. The point of the ticket is a body small
/// enough for the live edge to track, and that is already a 16x cut — spending milliseconds on the
/// last part of it is the wrong trade.
fn gzip(bytes: &[u8]) -> Option<Vec<u8>> {
    use std::io::Write as _;
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    e.write_all(bytes).ok()?;
    let out = e.finish().ok()?;
    (out.len() < bytes.len()).then_some(out)
}

fn respond_json_with(stream: &mut TcpStream, status: u16, body: &Value, extra: &str) {
    respond_json_encoded(stream, status, body, extra, false);
}

/// [`respond_json_with`], with the caller's `Accept-Encoding` honoured (T-533).
///
/// **The body is the same JSON either way** — this chooses a transfer coding, never a
/// representation. What a client decodes is byte-identical to what it would have received without
/// the header, which is why the contract tests assert the *decompressed* body against the plain one
/// rather than treating the two as separate shapes.
fn respond_json_encoded(
    stream: &mut TcpStream,
    status: u16,
    body: &Value,
    extra: &str,
    gzip_ok: bool,
) {
    let bytes = serde_json::to_vec(body).unwrap_or_else(|_| b"{}".to_vec());
    let auth = if status == 401 {
        "WWW-Authenticate: Bearer\r\n"
    } else {
        ""
    };
    if gzip_ok
        && bytes.len() >= GZIP_MIN_BYTES
        && let Some(z) = gzip(&bytes)
    {
        return respond(
            stream,
            status,
            "application/json",
            // `Vary` is already stated, once, by `respond_cached` — see there.
            &format!("{auth}{extra}Content-Encoding: gzip\r\n"),
            &z,
        );
    }
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
    // T-468: before the `/ws/{stream_id}` bridge, which would otherwise read this as a stream id.
    if req.path == "/ws/tiles/rows" && req.method == "GET" {
        return crate::rows::serve(stream, &shared.state, &req.query, &req.headers);
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
        .or_else(|| crate::measurements::route(state, &ctl)) // T-818
        .or_else(|| crate::annotations::route(state, &ctl)) // T-816
        .or_else(|| crate::views::route(state, &ctl)) // T-819
        .or_else(|| crate::collections::route(state, &ctl)) // T-817 (MAP-17)
        .or_else(|| crate::decode::route(state, &ctl)) // T-159; before inventory::route (see its docs)
        .or_else(|| crate::classification::route(state, &ctl)) // T-247; before inventory::route
        .or_else(|| crate::presence::route(state, &ctl)) // T-264; before inventory::route
        .or_else(|| crate::signatures::route(state, &ctl)) // T-201 C18 signature matches
        .or_else(|| crate::clusters::route(state, &ctl)) // T-202 C18 clusters of unknowns
        .or_else(|| crate::inventory::route(state, &ctl))
        .or_else(|| crate::outputs::route(state, &ctl))
        .or_else(|| crate::analyze::route(state, &ctl)) // T-190
        .or_else(|| crate::iqbuffer::route(state, &ctl)) // T-157
        .or_else(|| crate::datasets::route(state, &ctl)) // T-205
        .or_else(|| crate::ml::route(state, &ctl)) // T-844
        .or_else(|| crate::recordings::route(state, &ctl)) // T-469
        .or_else(|| crate::playback::route(state, &ctl)) // T-463
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
        .or_else(|| crate::trunking::route(state, &ctl)) // T-273
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
        | "/api/events"
        | "/api/analysis/strongest"
        | "/api/navigation"
        | "/api/timeline"
        | "/api/coverage"
        | "/api/tiles"
        | "/api/tiles/events"
        | "/api/report"
        | "/api/status"
        | "/api/taxonomy"
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
        // T-264 (ADR-0017 TM-8): the durable all-time catalogue, where Explore is window-scoped.
        "/api/events" => events(state, &req),
        "/api/analysis/strongest" => strongest(state, &req),
        // T-341: the backend owns which capture states are realizable; the client snaps against
        // this grid rather than deciding for itself what the front end can do.
        "/api/navigation" => crate::navigation::navigation_json(state, &req.query),
        // T-338: the scrubber's span is the IQ ring's retention — the capture window — and the
        // band it draws is a measurement made here, not a reduction made in the client.
        "/api/timeline" => crate::timeline::timeline_json(state, &req.query),
        // T-368: observed-versus-unobserved is computed from the tune history - what the front end
        // actually sampled - so grey means genuinely unobserved and never "quiet".
        "/api/coverage" => crate::coverage::coverage_json(state, &req.query),
        // T-438: the panes, the minimap and the live edge are projections of ONE pyramid, so they
        // read one route and cannot disagree on one screen. `(level_f, level_t)` are independent.
        // T-574: this route alone gets ETag/Cache-Control treatment (a sealed tile is immutable; a
        // live one must never be), so it answers itself rather than falling into the generic
        // `respond_json` tail below.
        "/api/tiles" => {
            return match crate::tiles::tiles_json(state, &req.query) {
                Ok(v) => respond_tile(&mut stream, &req, v),
                Err(e) => respond_error(&mut stream, e.status, &e.message),
            };
        }
        // T-573: one request per viewport, not one per tile. Each entry's `tile` is exactly what
        // the route above answers for that address alone, so a partial viewport — some data, one
        // genuinely unobserved, one refused — is expressible in one response. Answered through
        // the generic tail: a batch is never a single sealed representation, so it gets no ETag
        // and no immutable cache, but it does get `Accept-Encoding` (T-700) where it matters most.
        "/api/tiles/batch" => crate::tiles::tiles_batch_json(state, &req.query),
        // docs/16 §5.3: a tile never carries emitters (identity gating is per-caller and a sealed
        // tile is immutable), so the coarse-zoom highlight layer is a count per cell, computed on
        // demand on the same address.
        "/api/tiles/events" => crate::tiles::tile_events_json(state, &req.query),
        // T-351: `t` is the server's own wall clock at the instant this response was built — added
        // here, not inside the pipeline's opaque counter object, since every consumer of
        // `StatusFn` already answers without one and a caller needs it to tell a fresh read from a
        // cached one, or to measure its own clock skew against this device. Bare name, Unix
        // seconds, per the units convention; wall clock (`Timestamp::now`), not the run's sample
        // clock, because skew-against-the-device is exactly what a sample clock cannot answer.
        "/api/status" => state
            .status
            .as_ref()
            .map(|f| {
                let mut v = f();
                if let Some(o) = v.as_object_mut() {
                    o.insert(
                        "t".into(),
                        json!(Timestamp::now().as_unix_nanos() as f64 / 1e9),
                    );
                    // T-572: the hot-tile cache's bound and its eviction, reported HERE and not in
                    // a tile body — `cost.cache` would change on every read and so would a sealed
                    // tile's ETag, which is the one thing T-574's 304 depends on not doing.
                    if let Some(c) = state.tile_cache.as_ref() {
                        o.insert("tile_cache".into(), c.stats_json());
                    }
                }
                v
            })
            .ok_or_else(|| ApiError::new(404, "no pipeline status")),
        // T-218: reference data (the taxonomy and its thresholds), so the thin client never keeps
        // its own copy of the family tree or the gates. No server state is involved.
        "/api/taxonomy" => Ok(crate::taxonomy::taxonomy_json()),
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
        // T-533: the one place every routed JSON answer is written, so the transfer coding is
        // decided once rather than per route. A `/api/tiles` grid is measurement text and
        // compresses about ninefold; an error body is a sentence and is below the threshold.
        Ok(v) => respond_json_encoded(&mut stream, 200, &v, "", accepts_gzip(&req)),
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
pub(crate) fn with_history<T>(
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

/// The widest instantaneous bandwidth this run can produce, Hz (T-341), for the live-vs-overview
/// claim in `resolution.source`. `None` when nothing here can say — and then the **weaker** claim
/// is made, never the stronger: not knowing the window is not evidence that a span fits inside it.
///
/// Two answers, in order of how much they know:
///
/// 1. a live front end's capabilities — its *widest* sample rate, because the span it can deliver
///    is what the user could retune to, not only what it is set to now. With several front ends
///    (T-511) it is the widest any of them can deliver: a span is live-detail if **some** window
///    could have captured it in one piece;
/// 2. failing that, the running segment's own sample rate. A replay has no live control, but it
///    still has exactly one instantaneous bandwidth, and it is this. Nothing wider than it ever
///    came from one window.
pub(crate) fn max_live_span_hz(state: &ApiState) -> Option<f64> {
    state
        .live_controls
        .iter()
        .filter_map(|l| l.capabilities().max_live_span_hz())
        .fold(None, |acc: Option<f64>, hz| {
            Some(acc.map_or(hz, |a| a.max(hz)))
        })
        .or_else(|| {
            state
                .run_control
                .as_deref()
                .map(|r| r.state().sample_rate_hz)
                .filter(|hz| hz.is_finite() && *hz > 0.0)
        })
}

fn history(state: &ApiState, req: &Request) -> Result<Value, ApiError> {
    with_history(state, |p| {
        query::history_json(p, &req.query, max_live_span_hz(state))
    })
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
    query::inventory_json(state, &repo, &req.query)
}

/// `/api/events` (T-264, ADR-0017 TM-8): the durable catalogue of events in a region over a time
/// range, plus what the receiver actually observed there.
///
/// Reads the inventory's observation ledger, and the spectrum history when there is one — a server
/// without history still serves the catalogue, with coverage reported as **unknown** rather than
/// letting an empty answer read as a quiet band.
fn events(state: &ApiState, req: &Request) -> Result<Value, ApiError> {
    let repo = state
        .inventory
        .as_ref()
        .ok_or_else(|| ApiError::new(404, "no signal inventory on this server"))?;
    let repo = repo
        .lock()
        .map_err(|_| ApiError::new(500, "inventory store poisoned"))?;
    match &state.history {
        Some(h) => {
            let h = h
                .lock()
                .map_err(|_| ApiError::new(500, "history store poisoned"))?;
            crate::events::events_json(state, &repo, Some(&h), &req.query)
        }
        None => crate::events::events_json(state, &repo, None, &req.query),
    }
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
    let Some(mut offer) = shared.state.streams.offer(stream_id) else {
        return respond_error(&mut stream, 404, "no such stream");
    };
    let accept = tungstenite::handshake::derive_accept_key(key.as_bytes());
    let peer = stream
        .peer_addr()
        .map_or_else(|_| "unknown".into(), |a| a.to_string());
    let _ = stream.set_write_timeout(None);
    let label = format!("ws:{peer}");
    let deadline = Instant::now() + REPLUMB_HANDSHAKE_WAIT;
    loop {
        let response = format!(
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Accept: {accept}\r\n\r\n"
        )
        .into_bytes();
        return match bridge::attach(&offer.handle, &stream, label.clone(), response) {
            // T-417: the connection is watched against the *stream id*, not one publisher of it,
            // so a retune (which offers a new publisher under the same id) does not drop the
            // browser.
            Ok(attached) => bridge::watch_peer(
                &shared.state.streams,
                stream_id,
                offer,
                attached,
                stream,
                label,
            ),
            // T-530: the publisher finished but its producer said a successor is coming under this
            // id — the stream is between windows, not over. An arriving consumer gets the same
            // treatment `watch_peer` gives one that was already attached: wait for the next offer
            // and subscribe to that. The gap a re-plumb leaves was measured at ~0.17 s (T-525), so
            // this normally ends in a `101` and the client never learns anything happened.
            Err(StreamError::BetweenWindows) => {
                let left = deadline.saturating_duration_since(Instant::now());
                match shared
                    .state
                    .streams
                    .wait_for_offer_after(stream_id, offer.generation, left)
                {
                    Some(next) => {
                        offer = next;
                        continue;
                    }
                    // Still between windows when the wait ran out. `503` is "not now": a client
                    // that retries is right to, and `410` — the resource is permanently gone —
                    // would be a lie about a run that is still capturing.
                    None => respond_json_with(
                        &mut stream,
                        503,
                        &json!({
                            "error": "the stream is moving to a new window; try again",
                            "code": "replumbing",
                        }),
                        "Retry-After: 1\r\n",
                    ),
                }
            }
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
        };
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
