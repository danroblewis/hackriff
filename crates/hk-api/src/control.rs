//! The control API (T-050): device, display, recording and bookmark endpoints behind the API
//! token, with an audit log. **Receive only**: no endpoint, method or body field reaches a
//! transmit path (C37 stays gated), and the state endpoint says so.
//!
//! # Endpoints
//! Every body is JSON (`Content-Type: application/json`); unknown fields are refused (400). Every
//! error answers `{"error": message, "code": code}` ([`LiveControlError::code`] plus `invalid`,
//! `not_found`, `unavailable`, `unsupported_media_type`, `method_not_allowed`).
//!
//! | Method | Path | Body | Answers |
//! |---|---|---|---|
//! | GET | `/api/control/state` | – | `live`, `device` (capabilities), `tuning`, `run` (content class, segment, display, recording), `transmit.available: false`, `routes` |
//! | POST | `/api/control/center` | `{"center_hz"}` | `tuning`, `run`. A window of another class **re-plumbs** the run with that class |
//! | POST | `/api/control/rate` | `{"sample_rate_hz"}` | `tuning`, `run` (a rate change re-plumbs the run) |
//! | POST | `/api/control/gains` | `{"gains": {"lna": 24, ...}}` | `tuning` (quantised per stage) |
//! | POST | `/api/control/bias_tee` | `{"enabled"}` | `tuning` (501 without a bias tee) |
//! | POST | `/api/control/display` | any of `{"fft_size", "averaging", "rows_per_s"}` | `display` |
//! | POST | `/api/control/pause`, `/api/control/resume` | `{}` or empty | `display` |
//! | POST | `/api/control/record/start` | `{"label"?, "max_s"?}` | `recording` (409 `refused` under a class that forbids content) |
//! | POST | `/api/control/record/stop` | `{}` or empty | `recording` (the stored Recording) |
//! | GET, POST | `/api/bookmarks` | create: `{"name", "f_center_hz", "kind"?, "bandwidth_hz"?, "note"?}` | list / the created bookmark (201) |
//! | GET, PUT, DELETE | `/api/bookmarks/<id>` | update: any create field (`null` clears optional ones) | the bookmark / `{"deleted": ...}` |
//!
//! Device endpoints (centre, rate, gains, bias tee) need a live source
//! ([`crate::ApiState::live_control`]); on a replayed recording they answer 409 `not_live`, while
//! display, pause, recording and bookmarks keep working.
//!
//! # Security properties
//! - **Token in the header only.** Mutating requests (`POST`, `PUT`, `DELETE`) must carry
//!   `Authorization: Bearer <token>`; the `?token=` query form is refused for them (it is for
//!   browser WebSockets and GETs). No cookie is ever read or set, so a cross-site page cannot ride
//!   on a browser session (no CSRF).
//! - **Strict CORS.** No `Access-Control-Allow-*` header is ever sent and `OPTIONS` preflights are
//!   refused (403), so browsers block cross-origin calls that carry the token or a JSON body. A
//!   mutating request whose `Origin` names another host than `Host` / `X-Forwarded-Host` is
//!   refused (403). Same-origin use (the UI served by this server, directly or through the
//!   cloudflared tunnel, whose `Host` is the public hostname) is unaffected.
//! - **Audit log** ([`AuditLog`], JSON lines, mode `0600`): one entry per control request,
//!   including refused and unauthenticated ones: time, token id ([`crate::Token::id`], never the
//!   token), peer, forwarded-for, method, path, action, request body, old and new values, status
//!   and result. Without an audit log every mutating endpoint answers 503.
//! - **Legal gating** lives in the pipeline and the repository, not here: a retune re-derives the
//!   window's class, recordings are refused under content-forbidding classes, and the API never
//!   opens content (bookmarks are user metadata only).
//!
//! # Tokens for the web UI (T-051)
//! `hk serve` keeps the token in a `0600` file ([`crate::auth::default_token_path`]) and prints
//! `http://<addr>/#token=<token>` at start. The UI reads the fragment (never sent to the server),
//! keeps the token in `sessionStorage` for the tab, strips it from the address bar, and otherwise
//! asks the user to paste it (e.g. from the token file when opening the cloudflared tunnel URL,
//! `https://<tunnel-host>/#token=<token>`). It must send `Authorization: Bearer <token>` on every
//! `fetch` (control calls included) and `?token=` only on the `wss://` WebSocket URL.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use hk_core::source::{SampleRates, SourceKind};
use hk_core::{NamedGain, SourceCapabilities};
use hk_model::{
    BOOKMARK_NAME_MAX, Bookmark, BookmarkId, BookmarkKind, ContentClass, RepoError, Timestamp,
};
use serde_json::{Map, Value, json};

use crate::http::{ApiState, ROUTES};
use crate::live_control::{LiveControlError, LiveTuning};

/// Display settings of the spectrum stream.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DisplayState {
    /// Bins per row.
    pub fft_size: usize,
    /// Rows in the exponential average (1 = off).
    pub averaging: u32,
    /// Requested rows per second (waterfall speed).
    pub rows_per_s: f64,
    /// Publishing paused.
    pub paused: bool,
}

/// A partial display update.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DisplayUpdate {
    /// New FFT size.
    pub fft_size: Option<usize>,
    /// New averaging.
    pub averaging: Option<u32>,
    /// New row rate.
    pub rows_per_s: Option<f64>,
}

/// A manual recording's state.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RecordingState {
    /// Still recording.
    pub active: bool,
    /// Recording id.
    pub id: Option<String>,
    /// User label.
    pub label: Option<String>,
    /// Centre, Hz.
    pub center_hz: Option<f64>,
    /// Sample rate, Hz.
    pub sample_rate_hz: Option<f64>,
    /// Samples written.
    pub samples: u64,
    /// Samples lost while recording.
    pub lost_samples: u64,
    /// Requested maximum, s.
    pub max_s: f64,
    /// The Recording row was stored.
    pub stored: bool,
    /// Why it ended.
    pub ended: Option<String>,
}

/// The running pipeline as the control API sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct RunState {
    /// Device settings can change (a live source).
    pub live: bool,
    /// Content class in force.
    pub content_class: ContentClass,
    /// Requested centre, Hz.
    pub center_hz: f64,
    /// Requested sample rate, Hz.
    pub sample_rate_hz: f64,
    /// Segment number (increments at every re-plumb).
    pub segment: u64,
    /// A re-plumb is in progress.
    pub replumbing: bool,
    /// The run has finished.
    pub finished: bool,
    /// Display settings.
    pub display: DisplayState,
    /// The current or last manual recording.
    pub recording: RecordingState,
}

/// Display, pause and recording control of the running pipeline (implemented by the composition
/// over `hk_pipeline::PipelineController`). Available for recordings and live runs alike.
pub trait RunControl: Send + Sync {
    /// The run's state.
    fn state(&self) -> RunState;
    /// Applies a display update (all or nothing).
    fn set_display(&self, update: &DisplayUpdate) -> Result<DisplayState, LiveControlError>;
    /// Pauses or resumes spectrum publishing.
    fn set_paused(&self, paused: bool) -> Result<DisplayState, LiveControlError>;
    /// Starts a manual IQ recording of the tuned window.
    fn start_recording(
        &self,
        label: Option<&str>,
        max_s: Option<f64>,
    ) -> Result<RecordingState, LiveControlError>;
    /// Stops the manual recording.
    fn stop_recording(&self) -> Result<RecordingState, LiveControlError>;
}

/// Append-only JSON-lines audit log of control requests (mode `0600`).
#[derive(Debug)]
pub struct AuditLog {
    path: PathBuf,
    file: Mutex<File>,
}

impl AuditLog {
    /// Opens (creating) `path` for appending, with mode `0600`.
    pub fn open(path: &Path) -> io::Result<Self> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            fs::create_dir_all(dir)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        Ok(Self {
            path: path.to_owned(),
            file: Mutex::new(file),
        })
    }

    /// The log file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends one entry as a line.
    pub fn append(&self, entry: &Value) -> io::Result<()> {
        let mut line = serde_json::to_vec(entry).map_err(io::Error::other)?;
        line.push(b'\n');
        let mut f = self.file.lock().unwrap_or_else(PoisonError::into_inner);
        f.write_all(&line)?;
        f.flush()
    }
}

/// Who sent a request (audit log fields; `forwarded_for` and `origin` are as claimed).
#[derive(Clone, Debug, Default)]
pub(crate) struct Caller {
    pub token_id: Option<String>,
    pub peer: Option<String>,
    pub forwarded_for: Option<String>,
    pub origin: Option<String>,
}

/// A control request as the HTTP layer parsed it.
pub(crate) struct CtlRequest<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub body: &'a [u8],
    pub content_type: Option<&'a str>,
    pub caller: Caller,
}

/// A control response.
pub(crate) struct CtlResponse {
    pub status: u16,
    pub body: Value,
    pub allow: Option<&'static str>,
}

struct Fail {
    status: u16,
    code: &'static str,
    message: String,
}

impl Fail {
    fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    fn invalid(message: impl Into<String>) -> Self {
        Self::new(400, "invalid", message)
    }

    fn response(&self) -> CtlResponse {
        CtlResponse {
            status: self.status,
            body: json!({ "error": self.message, "code": self.code }),
            allow: None,
        }
    }
}

impl From<LiveControlError> for Fail {
    fn from(e: LiveControlError) -> Self {
        Self::new(e.http_status(), e.code(), e.to_string())
    }
}

fn repo_fail(e: RepoError) -> Fail {
    match e {
        RepoError::Invalid(m) => Fail::invalid(m),
        RepoError::NotFound { .. } => Fail::new(404, "not_found", "no such bookmark"),
        other => Fail::new(500, "failed", format!("bookmark store: {other}")),
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Action {
    State,
    Center,
    Rate,
    Gains,
    BiasTee,
    Display,
    Pause,
    Resume,
    RecordStart,
    RecordStop,
    ListBookmarks,
    CreateBookmark,
    GetBookmark(BookmarkId),
    UpdateBookmark(BookmarkId),
    DeleteBookmark(BookmarkId),
}

impl Action {
    fn name(self) -> &'static str {
        match self {
            Self::State => "state",
            Self::Center => "center",
            Self::Rate => "rate",
            Self::Gains => "gains",
            Self::BiasTee => "bias_tee",
            Self::Display => "display",
            Self::Pause => "pause",
            Self::Resume => "resume",
            Self::RecordStart => "record_start",
            Self::RecordStop => "record_stop",
            Self::ListBookmarks => "bookmarks_list",
            Self::CreateBookmark => "bookmark_create",
            Self::GetBookmark(_) => "bookmark_get",
            Self::UpdateBookmark(_) => "bookmark_update",
            Self::DeleteBookmark(_) => "bookmark_delete",
        }
    }

    fn mutating(self) -> bool {
        !matches!(
            self,
            Self::State | Self::ListBookmarks | Self::GetBookmark(_)
        )
    }
}

/// Resolves `(method, path)`: `Ok(action)`, `Err(Some(allow))` for a known path with another
/// method, `Err(None)` for an unknown control path. `None` when `path` is not a control path.
fn resolve(method: &str, path: &str) -> Option<Result<Action, Option<&'static str>>> {
    let pick = |allow: &'static str, action: Action| {
        if allow.split(", ").any(|m| m == method) {
            Ok(action)
        } else {
            Err(Some(allow))
        }
    };
    if let Some(rest) = path.strip_prefix("/api/control/") {
        return Some(match rest {
            "state" => pick("GET", Action::State),
            "center" => pick("POST", Action::Center),
            "rate" => pick("POST", Action::Rate),
            "gains" => pick("POST", Action::Gains),
            "bias_tee" => pick("POST", Action::BiasTee),
            "display" => pick("POST", Action::Display),
            "pause" => pick("POST", Action::Pause),
            "resume" => pick("POST", Action::Resume),
            "record/start" => pick("POST", Action::RecordStart),
            "record/stop" => pick("POST", Action::RecordStop),
            _ => Err(None),
        });
    }
    if path == "/api/bookmarks" {
        return Some(match method {
            "GET" => Ok(Action::ListBookmarks),
            "POST" => Ok(Action::CreateBookmark),
            _ => Err(Some("GET, POST")),
        });
    }
    if let Some(id) = path.strip_prefix("/api/bookmarks/") {
        let Ok(id) = id.parse::<BookmarkId>() else {
            return Some(Err(None));
        };
        return Some(match method {
            "GET" => Ok(Action::GetBookmark(id)),
            "PUT" => Ok(Action::UpdateBookmark(id)),
            "DELETE" => Ok(Action::DeleteBookmark(id)),
            _ => Err(Some("GET, PUT, DELETE")),
        });
    }
    if path == "/api/control" {
        return Some(Err(None));
    }
    None
}

/// A control path.
#[cfg(test)]
fn is_control_path(path: &str) -> bool {
    path.starts_with("/api/control/")
        || path == "/api/bookmarks"
        || path.starts_with("/api/bookmarks/")
}

fn now_s() -> f64 {
    Timestamp::now().as_unix_nanos() as f64 / 1e9
}

/// Records a control request refused before routing (no or wrong token, cross-origin).
pub(crate) fn audit_refused(
    state: &ApiState,
    method: &str,
    path: &str,
    caller: &Caller,
    status: u16,
    reason: &str,
) {
    if let Some(audit) = &state.audit {
        let entry = json!({
            "t_s": now_s(),
            "token_id": caller.token_id,
            "peer": caller.peer,
            "forwarded_for": caller.forwarded_for,
            "origin": caller.origin,
            "method": method,
            "path": path,
            "action": null,
            "result": "refused",
            "status": status,
            "error": reason,
        });
        if let Err(e) = audit.append(&entry) {
            eprintln!("hk-api: audit log {}: {e}", audit.path().display());
        }
    }
}

/// Routes a control or bookmark request; `None` when `path` is not one.
pub(crate) fn route(state: &ApiState, req: &CtlRequest<'_>) -> Option<CtlResponse> {
    let action = match resolve(req.method, req.path)? {
        Ok(a) => a,
        Err(Some(allow)) => {
            if req.method != "GET" {
                audit_refused(
                    state,
                    req.method,
                    req.path,
                    &req.caller,
                    405,
                    "method not allowed",
                );
            }
            return Some(CtlResponse {
                status: 405,
                body: json!({ "error": format!("use {allow}"), "code": "method_not_allowed" }),
                allow: Some(allow),
            });
        }
        Err(None) => {
            if req.method != "GET" {
                audit_refused(
                    state,
                    req.method,
                    req.path,
                    &req.caller,
                    404,
                    "no such endpoint",
                );
            }
            return Some(Fail::new(404, "not_found", "no such endpoint").response());
        }
    };
    if !action.mutating() {
        return Some(match read(state, action) {
            Ok(v) => CtlResponse {
                status: 200,
                body: v,
                allow: None,
            },
            Err(f) => f.response(),
        });
    }
    let Some(audit) = &state.audit else {
        return Some(
            Fail::new(
                503,
                "unavailable",
                "control is disabled: this server has no audit log",
            )
            .response(),
        );
    };
    let parsed = parse_body(req);
    let request = parsed
        .as_ref()
        .map_or(Value::Null, |m| Value::Object(m.clone()));
    let result = parsed.and_then(|body| apply(state, action, &body));
    let (status, response, old, new, error) = match result {
        Ok(a) => (a.status, a.body, a.old, a.new, None),
        Err(f) => {
            let r = f.response();
            (f.status, r.body, Value::Null, Value::Null, Some(f.message))
        }
    };
    let entry = json!({
        "t_s": now_s(),
        "token_id": req.caller.token_id,
        "peer": req.caller.peer,
        "forwarded_for": req.caller.forwarded_for,
        "origin": req.caller.origin,
        "method": req.method,
        "path": req.path,
        "action": action.name(),
        "request": request,
        "old": old,
        "new": new,
        "result": if error.is_none() { "ok" } else { "error" },
        "status": status,
        "error": error,
    });
    if let Err(e) = audit.append(&entry) {
        eprintln!("hk-api: audit log {}: {e}", audit.path().display());
    }
    Some(CtlResponse {
        status,
        body: response,
        allow: None,
    })
}

fn parse_body(req: &CtlRequest<'_>) -> Result<Map<String, Value>, Fail> {
    if req.body.iter().all(u8::is_ascii_whitespace) {
        return Ok(Map::new());
    }
    let json_type = req.content_type.is_some_and(|t| {
        t.split(';')
            .next()
            .is_some_and(|m| m.trim().eq_ignore_ascii_case("application/json"))
    });
    if !json_type {
        return Err(Fail::new(
            415,
            "unsupported_media_type",
            "control bodies must be Content-Type: application/json",
        ));
    }
    match serde_json::from_slice::<Value>(req.body) {
        Ok(Value::Object(m)) => Ok(m),
        Ok(_) => Err(Fail::invalid("the body must be a JSON object")),
        Err(e) => Err(Fail::invalid(format!("malformed JSON body: {e}"))),
    }
}

fn only(body: &Map<String, Value>, allowed: &[&str]) -> Result<(), Fail> {
    match body.keys().find(|k| !allowed.contains(&k.as_str())) {
        Some(k) => Err(Fail::invalid(format!(
            "unknown field {k:?} (allowed: {})",
            if allowed.is_empty() {
                "none".to_owned()
            } else {
                allowed.join(", ")
            }
        ))),
        None => Ok(()),
    }
}

fn number(body: &Map<String, Value>, key: &str) -> Result<Option<f64>, Fail> {
    match body.get(key) {
        None => Ok(None),
        Some(v) => v
            .as_f64()
            .filter(|x| x.is_finite())
            .map(Some)
            .ok_or_else(|| Fail::invalid(format!("{key} must be a finite number"))),
    }
}

fn required(body: &Map<String, Value>, key: &str) -> Result<f64, Fail> {
    number(body, key)?.ok_or_else(|| Fail::invalid(format!("{key} is required")))
}

fn integer(body: &Map<String, Value>, key: &str, max: u64) -> Result<Option<u64>, Fail> {
    match body.get(key) {
        None => Ok(None),
        Some(v) => v
            .as_u64()
            .filter(|n| *n <= max)
            .map(Some)
            .ok_or_else(|| Fail::invalid(format!("{key} must be an integer in 0..={max}"))),
    }
}

fn text<'a>(body: &'a Map<String, Value>, key: &str) -> Result<Option<Option<&'a str>>, Fail> {
    match body.get(key) {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(Value::String(s)) => Ok(Some(Some(s.as_str()))),
        Some(_) => Err(Fail::invalid(format!("{key} must be a string or null"))),
    }
}

fn nullable_number(body: &Map<String, Value>, key: &str) -> Result<Option<Option<f64>>, Fail> {
    match body.get(key) {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(_) => number(body, key).map(Some),
    }
}

fn no_fields(body: &Map<String, Value>) -> Result<(), Fail> {
    only(body, &[])
}

fn live(state: &ApiState) -> Result<&dyn crate::LiveControl, Fail> {
    state.live_control.as_deref().ok_or_else(|| {
        Fail::new(
            409,
            "not_live",
            "device settings apply to a live source; this server is not running one (a replayed \
             recording accepts display, pause, recording and bookmark requests only)",
        )
    })
}

fn run(state: &ApiState) -> Result<&dyn RunControl, Fail> {
    state
        .run_control
        .as_deref()
        .ok_or_else(|| Fail::new(503, "unavailable", "no running pipeline on this server"))
}

fn bookmarks(state: &ApiState) -> Result<std::sync::MutexGuard<'_, hk_model::Repository>, Fail> {
    state
        .bookmarks
        .as_ref()
        .map(|r| r.lock().unwrap_or_else(PoisonError::into_inner))
        .ok_or_else(|| Fail::new(503, "unavailable", "no bookmark store on this server"))
}

fn class_json(c: ContentClass) -> Value {
    serde_json::to_value(c).unwrap_or(Value::Null)
}

fn display_json(d: &DisplayState) -> Value {
    json!({
        "fft_size": d.fft_size,
        "averaging": d.averaging,
        "rows_per_s": d.rows_per_s,
        "paused": d.paused,
    })
}

fn recording_json(r: &RecordingState) -> Value {
    json!({
        "active": r.active,
        "id": r.id,
        "label": r.label,
        "center_hz": r.center_hz,
        "sample_rate_hz": r.sample_rate_hz,
        "samples": r.samples,
        "lost_samples": r.lost_samples,
        "max_s": r.max_s,
        "stored": r.stored,
        "ended": r.ended,
    })
}

fn run_json(r: &RunState) -> Value {
    json!({
        "live": r.live,
        "content_class": class_json(r.content_class),
        "content_permitted": r.content_class.permits_content(),
        "center_hz": r.center_hz,
        "sample_rate_hz": r.sample_rate_hz,
        "segment": r.segment,
        "replumbing": r.replumbing,
        "finished": r.finished,
        "display": display_json(&r.display),
        "recording": recording_json(&r.recording),
    })
}

fn tuning_json(t: &LiveTuning) -> Value {
    let gains: Map<String, Value> = t
        .gains
        .iter()
        .map(|g| (g.stage.clone(), json!(g.db)))
        .collect();
    json!({
        "center_hz": t.center_hz,
        "sample_rate_hz": t.sample_rate_hz,
        "gains": gains,
        "bias_tee": t.bias_tee,
    })
}

fn caps_json(c: &SourceCapabilities) -> Value {
    json!({
        "driver": c.driver,
        "kind": match c.kind {
            SourceKind::Hardware => "hardware",
            SourceKind::Replay => "replay",
        },
        "controllable": c.controllable,
        "frequency_ranges_hz": c.frequency_ranges.iter().map(|r| [r.min_hz, r.max_hz]).collect::<Vec<_>>(),
        "sample_rates_hz": match &c.sample_rates {
            SampleRates::Continuous { min_hz, max_hz } => json!({ "min": min_hz, "max": max_hz }),
            SampleRates::Discrete(v) => json!({ "values": v }),
        },
        "gain_stages": c.gain_stages.iter().map(|s| json!({
            "name": s.name, "min_db": s.min_db, "max_db": s.max_db, "step_db": s.step_db,
        })).collect::<Vec<_>>(),
        "bias_tee": c.bias_tee,
        "adc_bits": c.adc_bits,
        // A hardware descriptor only: the API has no transmit operation (see `transmit`).
        "tx_capable_hardware": c.tx_capable,
    })
}

fn bookmark_json(b: &Bookmark) -> Value {
    let secs = |t: Timestamp| t.as_unix_nanos() as f64 / 1e9;
    json!({
        "id": b.id.to_string(),
        "kind": serde_json::to_value(b.kind).unwrap_or(Value::Null),
        "name": b.name,
        "f_center_hz": b.f_center_hz,
        "bandwidth_hz": b.bandwidth_hz,
        "note": b.note,
        "created_s": secs(b.created_at),
        "updated_s": secs(b.updated_at),
    })
}

/// The `/api/control/state` body.
pub(crate) fn state_json(state: &ApiState) -> Value {
    let live = state.live_control.as_deref();
    json!({
        "live": live.is_some(),
        "device": live.map(|l| caps_json(l.capabilities())),
        "tuning": live.map(|l| tuning_json(&l.tuning())),
        "run": state.run_control.as_deref().map(|r| run_json(&r.state())),
        "transmit": {
            "available": false,
            "reason": "receive only: the control API has no transmit operation (C37 gated)",
        },
        "audit": state.audit.is_some(),
        "routes": ROUTES.iter().map(|(m, p)| json!({ "method": m, "path": p })).collect::<Vec<_>>(),
    })
}

fn read(state: &ApiState, action: Action) -> Result<Value, Fail> {
    match action {
        Action::State => Ok(state_json(state)),
        Action::ListBookmarks => {
            let repo = bookmarks(state)?;
            let all = repo.bookmarks().map_err(repo_fail)?;
            Ok(json!({ "bookmarks": all.iter().map(bookmark_json).collect::<Vec<_>>() }))
        }
        Action::GetBookmark(id) => {
            let repo = bookmarks(state)?;
            repo.bookmark(id)
                .map(|b| bookmark_json(&b))
                .map_err(repo_fail)
        }
        _ => Err(Fail::new(500, "failed", "not a read")),
    }
}

struct Applied {
    status: u16,
    body: Value,
    old: Value,
    new: Value,
}

fn ok(body: Value, old: Value, new: Value) -> Applied {
    Applied {
        status: 200,
        body,
        old,
        new,
    }
}

fn kind(v: Option<Option<&str>>) -> Result<Option<BookmarkKind>, Fail> {
    match v {
        None | Some(None) => Ok(None),
        Some(Some("marker")) => Ok(Some(BookmarkKind::Marker)),
        Some(Some("bookmark")) => Ok(Some(BookmarkKind::Bookmark)),
        Some(Some(other)) => Err(Fail::invalid(format!(
            "kind {other:?} must be \"marker\" or \"bookmark\""
        ))),
    }
}

fn name(v: Option<&str>) -> Result<String, Fail> {
    let n = v.unwrap_or("").trim();
    if n.is_empty() || n.chars().count() > BOOKMARK_NAME_MAX {
        return Err(Fail::invalid(format!(
            "name must be 1..={BOOKMARK_NAME_MAX} characters"
        )));
    }
    Ok(n.to_owned())
}

const BOOKMARK_FIELDS: &[&str] = &["kind", "name", "f_center_hz", "bandwidth_hz", "note"];

fn apply(state: &ApiState, action: Action, body: &Map<String, Value>) -> Result<Applied, Fail> {
    let run_body = |state: &ApiState| state.run_control.as_deref().map(|r| run_json(&r.state()));
    match action {
        Action::Center | Action::Rate => {
            let key = if action == Action::Center {
                "center_hz"
            } else {
                "sample_rate_hz"
            };
            only(body, &[key])?;
            let hz = required(body, key)?;
            let lc = live(state)?;
            let old = tuning_json(&lc.tuning());
            let t = if action == Action::Center {
                lc.set_center(hz)?
            } else {
                lc.set_rate(hz)?
            };
            let new = tuning_json(&t);
            Ok(ok(
                json!({ "tuning": new, "run": run_body(state) }),
                old,
                new,
            ))
        }
        Action::Gains => {
            only(body, &["gains"])?;
            let obj = body
                .get("gains")
                .and_then(Value::as_object)
                .filter(|o| !o.is_empty())
                .ok_or_else(|| {
                    Fail::invalid("gains must be a non-empty object of stage name to dB")
                })?;
            let gains = obj
                .iter()
                .map(|(stage, v)| {
                    let db = v.as_f64().filter(|x| x.is_finite());
                    match db {
                        Some(db) if !stage.is_empty() && stage.len() <= 32 => {
                            Ok(NamedGain::new(stage.clone(), db))
                        }
                        _ => Err(Fail::invalid(format!(
                            "gain {stage:?} must be a finite number of dB for a stage name of \
                             1..=32 characters"
                        ))),
                    }
                })
                .collect::<Result<Vec<_>, _>>()?;
            let lc = live(state)?;
            let old = tuning_json(&lc.tuning());
            let new = tuning_json(&lc.set_gains(&gains)?);
            Ok(ok(json!({ "tuning": new }), old, new))
        }
        Action::BiasTee => {
            only(body, &["enabled"])?;
            let enabled = body
                .get("enabled")
                .and_then(Value::as_bool)
                .ok_or_else(|| Fail::invalid("enabled must be true or false"))?;
            let lc = live(state)?;
            let old = tuning_json(&lc.tuning());
            let new = tuning_json(&lc.set_bias_tee(enabled)?);
            Ok(ok(json!({ "tuning": new }), old, new))
        }
        Action::Display => {
            only(body, &["fft_size", "averaging", "rows_per_s"])?;
            let update = DisplayUpdate {
                fft_size: integer(body, "fft_size", 1 << 20)?.map(|n| n as usize),
                averaging: integer(body, "averaging", u64::from(u32::MAX))?.map(|n| n as u32),
                rows_per_s: number(body, "rows_per_s")?,
            };
            if update == DisplayUpdate::default() {
                return Err(Fail::invalid(
                    "give at least one of fft_size, averaging, rows_per_s",
                ));
            }
            let rc = run(state)?;
            let old = display_json(&rc.state().display);
            let new = display_json(&rc.set_display(&update)?);
            Ok(ok(json!({ "display": new }), old, new))
        }
        Action::Pause | Action::Resume => {
            no_fields(body)?;
            let rc = run(state)?;
            let old = display_json(&rc.state().display);
            let new = display_json(&rc.set_paused(action == Action::Pause)?);
            Ok(ok(json!({ "display": new }), old, new))
        }
        Action::RecordStart => {
            only(body, &["label", "max_s"])?;
            let label = text(body, "label")?.flatten();
            let max_s = number(body, "max_s")?;
            let rc = run(state)?;
            let old = recording_json(&rc.state().recording);
            let new = recording_json(&rc.start_recording(label, max_s)?);
            Ok(ok(json!({ "recording": new }), old, new))
        }
        Action::RecordStop => {
            no_fields(body)?;
            let rc = run(state)?;
            let old = recording_json(&rc.state().recording);
            let new = recording_json(&rc.stop_recording()?);
            Ok(ok(json!({ "recording": new }), old, new))
        }
        Action::CreateBookmark => {
            only(body, BOOKMARK_FIELDS)?;
            let f = required(body, "f_center_hz")?;
            let mut b = Bookmark::new(
                kind(text(body, "kind")?)?.unwrap_or_default(),
                name(text(body, "name")?.flatten())?,
                f,
            );
            b.bandwidth_hz = nullable_number(body, "bandwidth_hz")?.flatten();
            b.note = text(body, "note")?.flatten().map(str::to_owned);
            bookmarks(state)?.insert_bookmark(&b).map_err(repo_fail)?;
            let new = bookmark_json(&b);
            Ok(Applied {
                status: 201,
                body: new.clone(),
                old: Value::Null,
                new,
            })
        }
        Action::UpdateBookmark(id) => {
            only(body, BOOKMARK_FIELDS)?;
            let mut repo = bookmarks(state)?;
            let stored = repo.bookmark(id).map_err(repo_fail)?;
            let mut next = stored.clone();
            if let Some(k) = kind(text(body, "kind")?)? {
                next.kind = k;
            }
            if let Some(n) = text(body, "name")? {
                next.name = name(n)?;
            }
            if let Some(f) = number(body, "f_center_hz")? {
                next.f_center_hz = f;
            }
            if let Some(bw) = nullable_number(body, "bandwidth_hz")? {
                next.bandwidth_hz = bw;
            }
            if let Some(note) = text(body, "note")? {
                next.note = note.map(str::to_owned);
            }
            next.updated_at = Timestamp::now().max(stored.created_at);
            let saved = repo.update_bookmark(&next).map_err(repo_fail)?;
            let new = bookmark_json(&saved);
            Ok(ok(new.clone(), bookmark_json(&stored), new))
        }
        Action::DeleteBookmark(id) => {
            no_fields(body)?;
            let deleted = bookmarks(state)?.delete_bookmark(id).map_err(repo_fail)?;
            let old = bookmark_json(&deleted);
            Ok(ok(json!({ "deleted": old }), old, Value::Null))
        }
        Action::State | Action::ListBookmarks | Action::GetBookmark(_) => {
            Err(Fail::new(500, "failed", "not a control action"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_resolve_with_methods() {
        assert_eq!(
            resolve("POST", "/api/control/center"),
            Some(Ok(Action::Center))
        );
        assert_eq!(
            resolve("GET", "/api/control/center"),
            Some(Err(Some("POST")))
        );
        assert_eq!(resolve("POST", "/api/control/tx"), Some(Err(None)));
        assert_eq!(resolve("GET", "/api/streams"), None);
        let id = BookmarkId::new();
        assert_eq!(
            resolve("DELETE", &format!("/api/bookmarks/{id}")),
            Some(Ok(Action::DeleteBookmark(id)))
        );
        assert_eq!(resolve("PUT", "/api/bookmarks/not-a-uuid"), Some(Err(None)));
        assert_eq!(
            resolve("PATCH", "/api/bookmarks"),
            Some(Err(Some("GET, POST")))
        );
    }

    #[test]
    fn every_control_route_is_in_the_route_table_and_none_transmits() {
        for (method, path) in ROUTES {
            let lower = path.to_ascii_lowercase();
            for word in ["tx", "transmit", "replay_tx", "send"] {
                assert!(
                    !lower.split(['/', '_', '{', '}']).any(|seg| seg == word),
                    "{method} {path}"
                );
            }
            let concrete = path
                .replace("{id}", &BookmarkId::new().to_string())
                .replace("{stream_id}", "spectrum");
            if is_control_path(&concrete) {
                assert!(
                    matches!(resolve(method, &concrete), Some(Ok(_))),
                    "{method} {path}"
                );
            }
        }
    }
}
