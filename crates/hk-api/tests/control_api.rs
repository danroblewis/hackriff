//! T-050 control API at the HTTP boundary (no pipeline; a recording device and a fake run):
//! authentication bypass attempts (missing, wrong, expired, query-string token), wrong methods,
//! CORS preflights and cross-origin requests, validation errors, the audit log, TX
//! unreachability, replay refusal of device settings, and bookmark persistence.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use hk_api::control::AuditLimits;
use hk_api::{
    ApiState, AuditLog, DisplayLimits, DisplayState, DisplayUpdate, LiveControl, LiveControlError,
    LiveTuning, ROUTES, RecordingState, RunControl, RunState, Server, ServerConfig,
    SourceLiveControl, Token,
};
use hk_core::{Gains, NamedGain, SourceCapabilities, SourceControl, SourceError};
use hk_model::{BookmarkId, ContentClass, Repository};
use serde_json::{Value, json};

const TOKEN: &str = "t050-control-token-0123456789abcdef";

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "hk-api-t050-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A device that records every receive-side command.
struct Device {
    caps: SourceCapabilities,
    calls: Mutex<Vec<String>>,
}

impl Device {
    fn new(bias_tee: bool) -> Arc<Self> {
        let mut caps = SourceCapabilities::hackrf_one();
        caps.bias_tee = bias_tee;
        Arc::new(Self {
            caps,
            calls: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn push(&self, s: String) -> Result<(), SourceError> {
        self.calls.lock().unwrap().push(s);
        Ok(())
    }
}

/// The front end's provenance `device_id` (T-343): every device action is recorded against it.
const DEVICE_ID: &str = "hackrf:0000000000000000fake0000000000ab";

impl SourceControl for Device {
    fn capabilities(&self) -> &SourceCapabilities {
        &self.caps
    }
    fn device_info(&self) -> Option<hk_core::source::DeviceInfo> {
        Some(hk_core::source::DeviceInfo {
            driver: "hackrf-one".into(),
            device_id: DEVICE_ID.into(),
            hw: "HackRF One (test), r4, fw 2026.01.3".into(),
        })
    }
    fn tune(&self, hz: f64) -> Result<(), SourceError> {
        self.push(format!("tune {hz}"))
    }
    fn set_sample_rate(&self, hz: f64) -> Result<(), SourceError> {
        self.push(format!("rate {hz}"))
    }
    fn set_gains(&self, g: &Gains) -> Result<(), SourceError> {
        self.push(format!("gains {g:?}"))
    }
    fn set_gain(&self, stage: &str, db: f64) -> Result<(), SourceError> {
        self.push(format!("gain {stage} {db}"))
    }
    fn set_baseband_filter(&self, hz: f64) -> Result<(), SourceError> {
        self.push(format!("filter {hz}"))
    }
    fn set_bias_tee(&self, on: bool) -> Result<(), SourceError> {
        self.push(format!("bias {on}"))
    }
    fn start(&self) -> Result<(), SourceError> {
        self.push("start".into())
    }
    fn stop(&self) -> Result<(), SourceError> {
        self.push("stop".into())
    }
}

/// Receive-side commands a device may see from the control API.
const RECEIVE_OPS: [&str; 5] = ["tune ", "rate ", "gain ", "bias ", "filter "];

fn live(device: &Arc<Device>) -> Arc<dyn LiveControl> {
    Arc::new(SourceLiveControl::new(
        Arc::clone(device) as Arc<dyn SourceControl>,
        LiveTuning {
            center_hz: 100.8e6,
            sample_rate_hz: 2.4e6,
            gains: vec![NamedGain::new("lna", 16.0), NamedGain::new("vga", 20.0)],
            bias_tee: if device.caps.bias_tee {
                hk_model::BiasTee::Off
            } else {
                hk_model::BiasTee::Unknown
            },
            baseband_filter_hz: None,
        },
    ))
}

/// A run whose display settings and recordings behave like the pipeline's.
struct FakeRun(Mutex<RunState>);

impl FakeRun {
    fn new(class: ContentClass) -> Arc<Self> {
        Arc::new(Self(Mutex::new(RunState {
            live: true,
            content_class: class,
            center_hz: 100.8e6,
            sample_rate_hz: 2.4e6,
            segment: 0,
            replumbing: false,
            finished: false,
            capture: hk_api::CaptureStatus::Running,
            capture_note: None,
            display: DisplayState {
                fft_size: 1024,
                averaging: 1,
                rows_per_s: 25.0,
                window: "hann".to_owned(),
            },
            recording: RecordingState::default(),
        })))
    }
}

impl RunControl for FakeRun {
    fn state(&self) -> RunState {
        self.0.lock().unwrap().clone()
    }
    fn display_limits(&self) -> DisplayLimits {
        DisplayLimits {
            fft_size_min: 64,
            fft_size_max: 65_536,
            averaging_max: 100,
            rows_per_s_min: 0.5,
            rows_per_s_max: 200.0,
            windows: vec!["hann".into(), "blackman-harris".into(), "flat-top".into()],
        }
    }
    fn set_display(&self, u: &DisplayUpdate) -> Result<DisplayState, LiveControlError> {
        let mut s = self.0.lock().unwrap();
        if let Some(n) = u.fft_size {
            if !n.is_power_of_two() {
                return Err(LiveControlError::Invalid(format!(
                    "fft_size {n} must be a power of two"
                )));
            }
            s.display.fft_size = n;
        }
        if let Some(a) = u.averaging {
            s.display.averaging = a;
        }
        if let Some(r) = u.rows_per_s {
            s.display.rows_per_s = r;
        }
        if let Some(w) = &u.window {
            if !["hann", "blackman-harris", "flat-top"].contains(&w.as_str()) {
                return Err(LiveControlError::Invalid(format!(
                    "window {w:?} must be one of hann, blackman-harris, flat-top"
                )));
            }
            s.display.window = w.clone();
        }
        Ok(s.display.clone())
    }
    fn start_recording(
        &self,
        label: Option<&str>,
        _max_s: Option<f64>,
    ) -> Result<RecordingState, LiveControlError> {
        let mut s = self.0.lock().unwrap();
        if !s.content_class.permits_content() {
            return Err(LiveControlError::Refused("class forbids content".into()));
        }
        s.recording = RecordingState {
            active: true,
            label: label.map(str::to_owned),
            ..RecordingState::default()
        };
        Ok(s.recording.clone())
    }
    fn stop_recording(&self) -> Result<RecordingState, LiveControlError> {
        let mut s = self.0.lock().unwrap();
        if !s.recording.active {
            return Err(LiveControlError::Conflict("no recording".into()));
        }
        s.recording.active = false;
        s.recording.stored = true;
        Ok(s.recording.clone())
    }
}

struct Rig {
    server: Server,
    device: Arc<Device>,
    audit: PathBuf,
    audit_log: Arc<AuditLog>,
    _dir: TempDir,
}

fn rig(tag: &str, token: Token, device: Arc<Device>, live_device: bool) -> Rig {
    let dir = TempDir::new(tag);
    let audit = dir.0.join("control-audit.jsonl");
    let repo = Repository::open(dir.0.join("hackriff.db")).unwrap();
    let audit_log = Arc::new(AuditLog::open(&audit).unwrap());
    let state = ApiState {
        live_control: live_device.then(|| live(&device)),
        run_control: Some(FakeRun::new(ContentClass::Unrestricted) as Arc<dyn RunControl>),
        bookmarks: Some(Arc::new(Mutex::new(repo))),
        audit: Some(Arc::clone(&audit_log)),
        ..ApiState::default()
    };
    let server = Server::start(
        ServerConfig::new("127.0.0.1:0".parse().unwrap(), token),
        state,
    )
    .unwrap();
    Rig {
        server,
        device,
        audit,
        audit_log,
        _dir: dir,
    }
}

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Value,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

fn raw(addr: SocketAddr, head: &str, body: &[u8]) -> Reply {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    s.write_all(head.as_bytes()).unwrap();
    let _ = s.write_all(body);
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    let split = out
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("response head");
    let head = String::from_utf8_lossy(&out[..split]).to_string();
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|c| c.parse().ok())
        .unwrap();
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .collect();
    let body = serde_json::from_slice(&out[split + 4..]).unwrap_or(Value::Null);
    Reply {
        status,
        headers,
        body,
    }
}

/// `method path` with extra header lines and an optional JSON body.
fn call(
    addr: SocketAddr,
    method: &str,
    path: &str,
    extra: &[(&str, &str)],
    body: Option<&str>,
) -> Reply {
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    for (k, v) in extra {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    let body = body.unwrap_or("");
    if !body.is_empty() {
        head.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        ));
    }
    head.push_str("\r\n");
    raw(addr, &head, body.as_bytes())
}

fn bearer() -> String {
    format!("Bearer {TOKEN}")
}

fn authed(addr: SocketAddr, method: &str, path: &str, body: Option<&str>) -> Reply {
    call(addr, method, path, &[("Authorization", &bearer())], body)
}

fn audit_entries(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn control_requests_need_a_valid_bearer_token_in_the_header() {
    let r = rig(
        "auth",
        Token::from_config(TOKEN).unwrap(),
        Device::new(true),
        true,
    );
    let addr = r.server.local_addr();
    let body = r#"{"center_hz": 101.1e6}"#;
    for auth in [
        None,
        Some("Bearer wrong-token-0123456789abcdef".to_owned()),
        Some("Basic dXNlcjpwYXNz".to_owned()),
        Some("Bearer ".to_owned()),
        Some(format!("Bearer {TOKEN}x")),
        Some(TOKEN.to_owned()),
    ] {
        let extra: Vec<(&str, &str)> = auth.iter().map(|a| ("Authorization", a.as_str())).collect();
        let rep = call(addr, "POST", "/api/control/center", &extra, Some(body));
        assert_eq!(rep.status, 401, "{auth:?}");
        assert_eq!(rep.header("WWW-Authenticate"), Some("Bearer"));
        assert!(
            !rep.body.to_string().contains("100800000"),
            "401 reveals nothing"
        );
    }
    // A valid token in the query string is not enough for a control request.
    let rep = call(
        addr,
        "POST",
        &format!("/api/control/center?token={TOKEN}"),
        &[],
        Some(body),
    );
    assert_eq!(rep.status, 401);
    assert!(
        rep.body["error"]
            .as_str()
            .unwrap()
            .contains("Authorization header")
    );
    // ...while read-only requests still accept it (unchanged GET behaviour).
    assert_eq!(
        call(
            addr,
            "GET",
            &format!("/api/control/state?token={TOKEN}"),
            &[],
            None
        )
        .status,
        200
    );
    assert_eq!(
        call(addr, "GET", "/api/control/state", &[], None).status,
        401
    );
    assert_eq!(
        call(
            addr,
            "DELETE",
            &format!("/api/bookmarks/{}", BookmarkId::new()),
            &[],
            None
        )
        .status,
        401
    );
    assert!(r.device.calls().is_empty(), "nothing reached the device");
    // Unauthenticated refusals are coalesced per client; every one is counted.
    r.audit_log.flush_refused();
    let refused = audit_entries(&r.audit);
    assert!(!refused.is_empty());
    assert_eq!(
        refused
            .iter()
            .map(|e| e["count"].as_u64().unwrap())
            .sum::<u64>(),
        8,
        "{refused:?}"
    );
    assert!(refused.iter().all(|e| {
        (e["result"] == "refused" && e["status"] == 401) || e["result"] == "refused_summary"
    }));
    assert!(refused.iter().all(|e| e["token_id"].is_null()));

    let rep = authed(addr, "POST", "/api/control/center", Some(body));
    assert_eq!(rep.status, 200, "{}", rep.body);
    assert_eq!(rep.body["tuning"]["center_hz"], json!(101.1e6));
    assert_eq!(r.device.calls(), vec!["tune 101100000".to_string()]);

    // An expired token never authenticates, for control or reads.
    let expired = Token::from_config(TOKEN)
        .unwrap()
        .with_expiry(SystemTime::now() - Duration::from_secs(1));
    let old = rig("expired", expired, Device::new(true), true);
    let addr = old.server.local_addr();
    assert_eq!(
        authed(addr, "POST", "/api/control/center", Some(body)).status,
        401
    );
    assert_eq!(authed(addr, "GET", "/api/control/state", None).status, 401);
    assert_eq!(authed(addr, "GET", "/api/streams", None).status, 401);
    assert!(old.device.calls().is_empty());
}

#[test]
fn wrong_methods_cors_preflights_and_cross_origin_requests_are_refused() {
    let r = rig(
        "cors",
        Token::from_config(TOKEN).unwrap(),
        Device::new(true),
        true,
    );
    let addr = r.server.local_addr();
    let rep = authed(addr, "GET", "/api/control/center", None);
    assert_eq!((rep.status, rep.header("Allow")), (405, Some("POST")));
    let rep = authed(addr, "PUT", "/api/control/state", Some("{}"));
    assert_eq!((rep.status, rep.header("Allow")), (405, Some("GET")));
    assert_eq!(authed(addr, "DELETE", "/api/streams", None).status, 405);
    assert_eq!(
        authed(addr, "POST", "/api/inventory", Some("{}")).status,
        405
    );
    assert_eq!(authed(addr, "POST", "/", Some("{}")).status, 405);
    for m in ["PATCH", "HEAD", "TRACE", "CONNECT"] {
        assert_eq!(
            authed(addr, m, "/api/control/center", None).status,
            405,
            "{m}"
        );
    }

    // Preflight from another origin: refused, and no CORS grant anywhere.
    let rep = call(
        addr,
        "OPTIONS",
        "/api/control/center",
        &[
            ("Origin", "https://evil.example"),
            ("Access-Control-Request-Method", "POST"),
            (
                "Access-Control-Request-Headers",
                "authorization, content-type",
            ),
        ],
        None,
    );
    assert_eq!(rep.status, 403);
    let body = r#"{"center_hz": 99.5e6}"#;
    let cross = call(
        addr,
        "POST",
        "/api/control/center",
        &[
            ("Authorization", &bearer()),
            ("Origin", "https://evil.example"),
        ],
        Some(body),
    );
    assert_eq!(
        cross.status, 403,
        "a cross-origin control request is refused even with the token"
    );
    let null_origin = call(
        addr,
        "POST",
        "/api/control/center",
        &[("Authorization", &bearer()), ("Origin", "null")],
        Some(body),
    );
    assert_eq!(null_origin.status, 403);
    assert!(r.device.calls().is_empty());
    let same = call(
        addr,
        "POST",
        "/api/control/center",
        &[
            ("Authorization", &bearer()),
            ("Origin", &format!("http://{addr}")),
        ],
        Some(body),
    );
    assert_eq!(same.status, 200, "the same-origin UI works: {}", same.body);
    for rep in [&rep, &cross, &null_origin, &same] {
        assert!(
            rep.headers
                .iter()
                .all(|(k, _)| !k.to_ascii_lowercase().starts_with("access-control-")),
            "no CORS headers: {:?}",
            rep.headers
        );
    }
    let entries = audit_entries(&r.audit);
    assert!(
        entries
            .iter()
            .any(|e| e["error"] == "cross-origin" && e["status"] == 403)
    );
}

#[test]
fn control_values_are_validated_with_clear_errors() {
    let r = rig(
        "validate",
        Token::from_config(TOKEN).unwrap(),
        Device::new(false),
        true,
    );
    let addr = r.server.local_addr();
    let cases: [(&str, &str, u16, &str); 18] = [
        (
            "/api/control/center",
            r#"{"center_hz": 101e6, "tx": true}"#,
            400,
            "tx",
        ),
        ("/api/control/center", r#"{}"#, 400, "center_hz is required"),
        (
            "/api/control/center",
            r#"{"center_hz": "101e6"}"#,
            400,
            "finite number",
        ),
        (
            "/api/control/center",
            r#"{"center_hz": 500e3}"#,
            400,
            "out of range",
        ),
        ("/api/control/center", r#"[101e6]"#, 400, "JSON object"),
        (
            "/api/control/center",
            r#"{"center_hz": 1"#,
            400,
            "malformed",
        ),
        (
            "/api/control/rate",
            r#"{"sample_rate_hz": 40e6}"#,
            400,
            "out of range",
        ),
        ("/api/control/gains", r#"{"gains": {}}"#, 400, "non-empty"),
        (
            "/api/control/gains",
            r#"{"gains": {"mixer": 3}}"#,
            400,
            "mixer",
        ),
        (
            "/api/control/gains",
            r#"{"gains": {"lna": 48}}"#,
            400,
            "lna",
        ),
        (
            "/api/control/bias_tee",
            r#"{"enabled": "yes"}"#,
            400,
            "true or false",
        ),
        (
            "/api/control/bias_tee",
            r#"{"enabled": true}"#,
            501,
            "bias tee",
        ),
        (
            "/api/control/display",
            r#"{"fft_size": 1000}"#,
            400,
            "power of two",
        ),
        (
            "/api/control/display",
            r#"{"window": "kaiser"}"#,
            400,
            "must be one of",
        ),
        (
            "/api/control/display",
            r#"{"window": null}"#,
            400,
            "not null",
        ),
        ("/api/control/record/stop", r#"{"now": true}"#, 400, "now"),
        (
            "/api/control/baseband_filter",
            r#"{}"#,
            400,
            "bandwidth_hz is required",
        ),
        (
            "/api/control/baseband_filter",
            r#"{"bandwidth_hz": 9.5e6}"#,
            400,
            "out of range",
        ),
    ];
    for (path, body, status, needle) in cases {
        let rep = authed(addr, "POST", path, Some(body));
        assert_eq!(rep.status, status, "{path} {body}: {}", rep.body);
        let msg = rep.body["error"].as_str().unwrap_or_default();
        assert!(
            msg.contains(needle),
            "{path} {body}: {msg:?} lacks {needle:?}"
        );
        assert!(rep.body["code"].is_string(), "{}", rep.body);
    }
    let rep = authed(addr, "POST", "/api/control/display", Some("{}"));
    assert_eq!(rep.status, 400);
    // Wrong media type, oversized and chunked bodies.
    let plain = br#"{"center_hz":101e6}"#;
    let head = format!(
        "POST /api/control/center HTTP/1.1\r\nHost: {addr}\r\nAuthorization: {}\r\n\
         Content-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        bearer(),
        plain.len()
    );
    assert_eq!(raw(addr, &head, plain).status, 415);
    // Refused from the head alone (no body is sent, so the refusal cannot race a reset).
    let big = format!(
        "POST /api/control/center HTTP/1.1\r\nHost: {addr}\r\nAuthorization: {}\r\n\
         Content-Type: application/json\r\nContent-Length: 70000\r\nConnection: close\r\n\r\n",
        bearer()
    );
    assert_eq!(raw(addr, &big, b"").status, 413);
    let chunked = format!(
        "POST /api/control/center HTTP/1.1\r\nHost: {addr}\r\nAuthorization: {}\r\n\
         Content-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
        bearer()
    );
    assert_eq!(raw(addr, &chunked, b"").status, 411);
    assert!(
        r.device.calls().is_empty(),
        "nothing invalid reached the device"
    );
    // A valid request still works afterwards.
    let rep = authed(
        addr,
        "POST",
        "/api/control/gains",
        Some(r#"{"gains": {"lna": 30, "amp": 11}}"#),
    );
    assert_eq!(rep.status, 200, "{}", rep.body);
    assert_eq!(rep.body["tuning"]["gains"]["lna"], json!(24.0), "quantised");
    assert_eq!(
        r.device.calls(),
        vec!["gain amp 11", "gain lna 24"],
        "stages are applied in name order"
    );
    // Baseband filter (T-067): validated against the device's discrete bandwidths.
    let rep = authed(
        addr,
        "POST",
        "/api/control/baseband_filter",
        Some(r#"{"bandwidth_hz": 7e6}"#),
    );
    assert_eq!(rep.status, 200, "{}", rep.body);
    assert_eq!(rep.body["tuning"]["baseband_filter_hz"], json!(7e6));
    // Display window (T-067).
    let rep = authed(
        addr,
        "POST",
        "/api/control/display",
        Some(r#"{"window": "flat-top"}"#),
    );
    assert_eq!(rep.status, 200, "{}", rep.body);
    assert_eq!(rep.body["display"]["window"], json!("flat-top"));
}

#[test]
fn every_control_action_is_audited_with_old_and_new_values() {
    let token = Token::from_config(TOKEN).unwrap();
    let id = token.id();
    let r = rig("audit", token, Device::new(true), true);
    let addr = r.server.local_addr();
    assert_eq!(
        authed(
            addr,
            "POST",
            "/api/control/center",
            Some(r#"{"center_hz": 99.1e6}"#)
        )
        .status,
        200
    );
    assert_eq!(
        authed(
            addr,
            "POST",
            "/api/control/center",
            Some(r#"{"center_hz": 1}"#)
        )
        .status,
        400
    );
    assert_eq!(
        authed(
            addr,
            "POST",
            "/api/control/display",
            Some(r#"{"averaging": 8}"#)
        )
        .status,
        200
    );
    assert_eq!(
        authed(
            addr,
            "POST",
            "/api/control/display",
            Some(r#"{"rows_per_s": 10.0}"#)
        )
        .status,
        200
    );
    assert_eq!(
        authed(
            addr,
            "POST",
            "/api/control/record/start",
            Some(r#"{"label": "fm"}"#)
        )
        .status,
        200
    );
    assert_eq!(
        authed(addr, "POST", "/api/control/record/stop", Some("{}")).status,
        200
    );
    assert_eq!(
        authed(
            addr,
            "POST",
            "/api/control/bias_tee",
            Some(r#"{"enabled": true}"#)
        )
        .status,
        200
    );
    assert_eq!(authed(addr, "GET", "/api/control/state", None).status, 200);
    let entries = audit_entries(&r.audit);
    assert_eq!(
        entries.len(),
        7,
        "one entry per control request (reads are not audited): {entries:#?}"
    );
    for e in &entries {
        assert_eq!(e["token_id"], json!(id));
        assert!(e["peer"].as_str().unwrap().starts_with("127.0.0.1:"));
        assert!(e["t_s"].as_f64().unwrap() > 1.7e9);
    }
    let center = &entries[0];
    assert_eq!(center["action"], "center");
    assert_eq!(center["result"], "ok");
    assert_eq!(center["request"]["center_hz"], json!(99.1e6));
    assert_eq!(center["old"]["center_hz"], json!(100.8e6));
    assert_eq!(center["new"]["center_hz"], json!(99.1e6));
    let failed = &entries[1];
    assert_eq!(
        (failed["result"].as_str(), failed["status"].as_u64()),
        (Some("error"), Some(400))
    );
    assert!(failed["error"].as_str().unwrap().contains("out of range"));
    assert_eq!(entries[2]["old"]["averaging"], json!(1));
    assert_eq!(entries[2]["new"]["averaging"], json!(8));
    assert_eq!(entries[3]["new"]["rows_per_s"], json!(10.0));
    assert_eq!(entries[5]["new"]["stored"], json!(true));
    assert_eq!(entries[6]["old"]["bias_tee"], json!("off"));
    assert_eq!(entries[6]["new"]["bias_tee"], json!("on"));
    let text = std::fs::read_to_string(&r.audit).unwrap();
    assert!(!text.contains(TOKEN), "the token itself is never logged");
    let mode = std::fs::metadata(&r.audit).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}

/// Sizes of the audit log and its rotated files.
fn audit_files(audit: &Path) -> Vec<(PathBuf, u64)> {
    let name = audit.file_name().unwrap().to_str().unwrap().to_owned();
    let mut files: Vec<(PathBuf, u64)> = std::fs::read_dir(audit.parent().unwrap())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_str().is_some_and(|n| n.starts_with(&name)))
        .map(|e| (e.path(), e.metadata().unwrap().len()))
        .collect();
    files.sort();
    files
}

fn all_audit_entries(audit: &Path) -> Vec<Value> {
    audit_files(audit)
        .iter()
        .flat_map(|(p, _)| audit_entries(p))
        .collect()
}

#[test]
fn unauthenticated_oversized_floods_cannot_fill_the_audit_directory() {
    const REQUESTS: usize = 10_000;
    const THREADS: usize = 16;
    let r = rig(
        "flood",
        Token::from_config(TOKEN).unwrap(),
        Device::new(true),
        true,
    );
    let addr = r.server.local_addr();
    let long_path = "a".repeat(15_000);
    let long_origin = format!("https://{}", "o".repeat(1_000));
    let started = std::time::Instant::now();
    std::thread::scope(|s| {
        for t in 0..THREADS {
            let (long_path, long_origin) = (&long_path, &long_origin);
            s.spawn(move || {
                for i in 0..REQUESTS / THREADS {
                    // Half the threads claim a new client address per request (through a
                    // loopback proxy), the rest are one client.
                    let forwarded = if t % 2 == 0 {
                        format!("X-Forwarded-For: 10.{t}.{}.{}\r\n", i / 256, i % 256)
                    } else {
                        String::new()
                    };
                    let head = format!(
                        "POST /api/{long_path} HTTP/1.1\r\nHost: {addr}\r\nOrigin: {long_origin}\r\n\
                         {forwarded}Connection: close\r\n\r\n"
                    );
                    assert_eq!(raw(addr, &head, b"").status, 401);
                }
            });
        }
    });
    let elapsed = started.elapsed();
    r.audit_log.flush_refused();

    let limits = *r.audit_log.limits();
    let files = audit_files(&r.audit);
    let total: u64 = files.iter().map(|(_, n)| n).sum();
    assert!(files.len() <= limits.files, "{files:?}");
    assert!(
        total <= limits.files as u64 * limits.max_file_bytes,
        "audit dir {total} bytes"
    );
    assert!(
        total < 1024 * 1024,
        "coalesced: {REQUESTS} requests of ~16 KiB logged in {total} bytes"
    );
    let entries = all_audit_entries(&r.audit);
    let lines = entries.iter().filter(|e| e["result"] == "refused").count();
    let windows = elapsed.as_secs() / 60 + 1;
    assert!(
        lines as u64 <= u64::from(limits.refused_lines_per_window) * windows,
        "{lines} refused lines in {elapsed:?}"
    );
    let summaries = entries
        .iter()
        .filter(|e| e["result"] == "refused_summary")
        .count();
    assert!(summaries >= 1, "the suppressed refusals are summarised");
    assert_eq!(
        entries
            .iter()
            .map(|e| e["count"].as_u64().unwrap())
            .sum::<u64>(),
        REQUESTS as u64,
        "every refusal is counted exactly once"
    );
    for e in entries.iter().filter(|e| e["result"] == "refused") {
        let path = e["path"].as_str().unwrap();
        assert!(path.len() < 300 && path.contains("truncated"), "{path}");
        assert!(e["origin"].as_str().unwrap().len() < 300);
        assert!(e["count"].as_u64().unwrap() >= 1);
    }
    // Control still works, and is logged in full.
    let rep = authed(
        addr,
        "POST",
        "/api/control/center",
        Some(r#"{"center_hz": 99.1e6}"#),
    );
    assert_eq!(rep.status, 200, "{}", rep.body);
    let entries = all_audit_entries(&r.audit);
    assert!(
        entries
            .iter()
            .any(|e| e["action"] == "center" && e["new"]["center_hz"] == json!(99.1e6))
    );
}

#[test]
fn the_audit_log_rotates_at_its_cap_and_bounds_fields() {
    let dir = TempDir::new("rotate");
    let path = dir.0.join("control-audit.jsonl");
    let limits = AuditLimits {
        max_file_bytes: 8 * 1024,
        files: 3,
        max_line_bytes: 4 * 1024,
        ..AuditLimits::default()
    };
    let log = AuditLog::open_with_limits(&path, limits).unwrap();
    for i in 0..2_000 {
        log.append(&json!({
            "t_s": i,
            "token_id": "tok-000000000000",
            "path": "p".repeat(5_000),
            "action": "bookmark_create",
            "request": { "note": "n".repeat(600), ("k".repeat(2_000)): 1 },
            "old": null,
            "new": { "i": i },
            "result": "ok",
        }))
        .unwrap();
    }
    let big = json!({
        "action": "gains",
        "request": (0..500).map(|i| (format!("s{i}"), json!("x".repeat(100)))).collect::<serde_json::Map<_, _>>(),
    });
    log.append(&big).unwrap();
    let files = audit_files(&path);
    assert_eq!(files.len(), 3, "{files:?}");
    assert!(
        files.iter().all(|(_, n)| *n <= limits.max_file_bytes),
        "{files:?}"
    );
    let entries = all_audit_entries(&path);
    assert!(!entries.is_empty());
    for e in &entries {
        let line = serde_json::to_string(e).unwrap();
        assert!(line.len() < limits.max_line_bytes, "{} bytes", line.len());
        if let Some(p) = e["path"].as_str() {
            assert!(p.starts_with("ppp") && p.contains("truncated"));
        }
    }
    let last = entries.iter().find(|e| e["action"] == "gains").unwrap();
    assert_eq!(last["request"]["truncated"], json!(true), "{last}");
    assert!(
        entries.iter().any(|e| e["new"]["i"] == json!(1_999)),
        "the newest entries are kept"
    );
    assert!(!std::fs::exists(format!("{}.3", path.display())).unwrap());
}

#[test]
fn refusals_are_coalesced_per_client_under_a_global_budget() {
    let dir = TempDir::new("coalesce");
    let path = dir.0.join("audit.jsonl");
    let log = AuditLog::open_with_limits(
        &path,
        AuditLimits {
            refused_interval: Duration::from_secs(3600),
            refused_window: Duration::from_secs(3600),
            refused_lines_per_window: 2,
            ..AuditLimits::default()
        },
    )
    .unwrap();
    let entry = |client: &str| json!({ "result": "refused", "status": 401, "peer": client });
    for client in ["a", "a", "b", "b", "c", "c", "a"] {
        log.append_refused(client, &entry(client));
    }
    let lines = audit_entries(&path);
    assert_eq!(
        lines
            .iter()
            .map(|e| (e["peer"].as_str().unwrap(), e["count"].as_u64().unwrap()))
            .collect::<Vec<_>>(),
        vec![("a", 1), ("b", 1)],
        "one line per client per interval, two lines per window"
    );
    drop(log); // flushes pending counts
    let lines = audit_entries(&path);
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[2]["result"], "refused_summary");
    assert_eq!(
        lines[2]["count"],
        json!(5),
        "a x2, b x1, c x2 not on a line"
    );
}

#[test]
fn the_audit_log_refuses_symlinks() {
    let dir = TempDir::new("audit-link");
    let target = dir.0.join("target.jsonl");
    std::fs::write(&target, "").unwrap();
    let link = dir.0.join("control-audit.jsonl");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let e = AuditLog::open(&link).unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput, "{e}");
    let dangling = dir.0.join("dangling.jsonl");
    std::os::unix::fs::symlink(dir.0.join("nowhere"), &dangling).unwrap();
    assert!(AuditLog::open(&dangling).is_err());
    assert!(!dir.0.join("nowhere").exists());
}

#[test]
fn without_an_audit_log_control_is_disabled() {
    let device = Device::new(true);
    let state = ApiState {
        live_control: Some(live(&device)),
        ..ApiState::default()
    };
    let server = Server::start(
        ServerConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            Token::from_config(TOKEN).unwrap(),
        ),
        state,
    )
    .unwrap();
    let addr = server.local_addr();
    let rep = authed(
        addr,
        "POST",
        "/api/control/center",
        Some(r#"{"center_hz": 99.1e6}"#),
    );
    assert_eq!(
        (rep.status, rep.body["code"].as_str()),
        (503, Some("unavailable"))
    );
    assert!(device.calls().is_empty());
    let state = authed(addr, "GET", "/api/control/state", None);
    assert_eq!(state.status, 200);
    assert_eq!(state.body["audit"], json!(false));
}

#[test]
fn no_route_or_method_reaches_a_transmit_path() {
    let r = rig(
        "tx",
        Token::from_config(TOKEN).unwrap(),
        Device::new(true),
        true,
    );
    let addr = r.server.local_addr();
    let state = authed(addr, "GET", "/api/control/state", None);
    assert_eq!(state.status, 200);
    assert_eq!(state.body["transmit"]["available"], json!(false));
    assert_eq!(
        state.body["device"]["tx_capable_hardware"],
        json!(true),
        "the HackRF can transmit, and the API still offers no way to"
    );
    let listed: Vec<(String, String)> = state.body["routes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["method"].as_str().unwrap().to_owned(),
                r["path"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(listed.len(), ROUTES.len());
    for (_, path) in ROUTES {
        let segs: Vec<String> = path
            .to_ascii_lowercase()
            .split(['/', '_'])
            .map(str::to_owned)
            .collect();
        for word in [
            "tx",
            "transmit",
            "transmitter",
            "send",
            "replay",
            "jam",
            "duplex",
        ] {
            assert!(!segs.iter().any(|s| s == word), "{path} names {word}");
        }
    }

    let id = BookmarkId::new().to_string();
    let mut paths: Vec<String> = ROUTES
        .iter()
        .map(|(_, p)| {
            p.replace("{id}", &id)
                .replace("{stream_id}", "spectrum/live")
        })
        .collect();
    for p in [
        "/api/control/tx",
        "/api/control/transmit",
        "/api/control/tx/start",
        "/api/control/transmit/start",
        "/api/control/replay",
        "/api/control/mode",
        "/api/control/duplex",
        "/api/control/amp_tx",
        "/api/control/tx_gain",
        "/api/tx",
        "/api/transmit",
        "/api/control",
        "/ws/tx",
        "/ws/transmit",
    ] {
        paths.push(p.to_owned());
    }
    let bodies = [
        None,
        Some(r#"{"tx": true}"#),
        Some(r#"{"transmit": true, "center_hz": 433.92e6, "sample_rate_hz": 2e6}"#),
        Some(r#"{"mode": "tx", "gains": {"tx_vga": 40}, "enabled": true}"#),
    ];
    let methods = [
        "GET", "POST", "PUT", "DELETE", "OPTIONS", "PATCH", "HEAD", "TRACE", "CONNECT",
    ];
    let mut calls = 0;
    for path in &paths {
        for method in methods {
            for body in bodies {
                calls += 1;
                let rep = authed(addr, method, path, body);
                if (200..300).contains(&rep.status) {
                    let concrete = |p: &str| {
                        p.replace("{id}", &id)
                            .replace("{stream_id}", "spectrum/live")
                    };
                    assert!(
                        ROUTES
                            .iter()
                            .any(|(m, p)| *m == method && concrete(p) == *path),
                        "{method} {path} {body:?} answered {} but is not a listed route",
                        rep.status
                    );
                    assert!(
                        method == "GET" || body.is_none(),
                        "{method} {path} {body:?}: a transmit-flavoured body was accepted: {}",
                        rep.body
                    );
                }
            }
        }
    }
    assert!(calls > 500);
    // Whatever reached the device was a receive-side command.
    for c in r.device.calls() {
        assert!(
            RECEIVE_OPS.iter().any(|op| c.starts_with(op)),
            "device saw {c:?}"
        );
    }
    assert!(
        r.device.calls().is_empty(),
        "no body above was valid, so nothing was applied"
    );
}

#[test]
fn replayed_recordings_refuse_device_settings_and_accept_display_settings() {
    let r = rig(
        "replay",
        Token::from_config(TOKEN).unwrap(),
        Device::new(true),
        false,
    );
    let addr = r.server.local_addr();
    for (path, body) in [
        ("/api/control/center", r#"{"center_hz": 99.1e6}"#),
        ("/api/control/rate", r#"{"sample_rate_hz": 10e6}"#),
        ("/api/control/gains", r#"{"gains": {"lna": 24}}"#),
        ("/api/control/bias_tee", r#"{"enabled": true}"#),
        ("/api/control/baseband_filter", r#"{"bandwidth_hz": 7e6}"#),
    ] {
        let rep = authed(addr, "POST", path, Some(body));
        assert_eq!(rep.status, 409, "{path}: {}", rep.body);
        assert_eq!(rep.body["code"], "not_live", "{path}");
    }
    let rep = authed(
        addr,
        "POST",
        "/api/control/display",
        Some(r#"{"fft_size": 2048, "rows_per_s": 10, "window": "blackman-harris"}"#),
    );
    assert_eq!(rep.status, 200, "{}", rep.body);
    assert_eq!(rep.body["display"]["fft_size"], json!(2048));
    assert_eq!(rep.body["display"]["window"], json!("blackman-harris"));
    let state = authed(addr, "GET", "/api/control/state", None);
    assert_eq!(state.body["live"], json!(false));
    assert!(state.body["device"].is_null());
    assert_eq!(state.body["run"]["display"]["fft_size"], json!(2048));
    // Display limits (T-067) are reported even without a live device.
    let limits = &state.body["display_limits"];
    assert!(limits["fft_size_min"].as_u64().unwrap() > 0);
    assert!(limits["fft_size_max"].as_u64().unwrap() >= limits["fft_size_min"].as_u64().unwrap());
    assert!(limits["averaging_max"].as_u64().unwrap() >= 1);
    assert!(limits["rows_per_s_max"].as_f64().unwrap() > 0.0);
    assert!(
        limits["windows"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w == "hann")
    );
    assert!(r.device.calls().is_empty());
}

#[test]
fn bookmarks_are_validated_and_persist_across_servers() {
    let dir = TempDir::new("bookmarks");
    let db = dir.0.join("hackriff.db");
    let start = |dir: &Path| {
        let state = ApiState {
            bookmarks: Some(Arc::new(Mutex::new(
                Repository::open(dir.join("hackriff.db")).unwrap(),
            ))),
            audit: Some(Arc::new(AuditLog::open(&dir.join("audit.jsonl")).unwrap())),
            ..ApiState::default()
        };
        Server::start(
            ServerConfig::new(
                "127.0.0.1:0".parse().unwrap(),
                Token::from_config(TOKEN).unwrap(),
            ),
            state,
        )
        .unwrap()
    };
    let server = start(&dir.0);
    let addr = server.local_addr();
    let created = authed(
        addr,
        "POST",
        "/api/bookmarks",
        Some(
            r#"{"name": "  FM 101.3 ", "f_center_hz": 101.3e6, "bandwidth_hz": 200e3, "note": "RDS"}"#,
        ),
    );
    assert_eq!(created.status, 201, "{}", created.body);
    assert_eq!(created.body["name"], "FM 101.3", "trimmed");
    assert_eq!(created.body["kind"], "bookmark");
    let id = created.body["id"].as_str().unwrap().to_owned();
    for (body, needle) in [
        (r#"{"name": "", "f_center_hz": 1e8}"#, "name"),
        (r#"{"name": "x", "f_center_hz": -1}"#, "f_center_hz"),
        (
            r#"{"name": "x", "f_center_hz": 1e8, "bandwidth_hz": 0}"#,
            "bandwidth",
        ),
        (
            r#"{"name": "x", "f_center_hz": 1e8, "kind": "beacon"}"#,
            "kind",
        ),
        (
            r#"{"name": "x", "f_center_hz": 1e8, "content": "x"}"#,
            "content",
        ),
    ] {
        let rep = authed(addr, "POST", "/api/bookmarks", Some(body));
        assert_eq!(rep.status, 400, "{body}: {}", rep.body);
        assert!(
            rep.body["error"].as_str().unwrap().contains(needle),
            "{body}: {}",
            rep.body
        );
    }
    let marker = authed(
        addr,
        "POST",
        "/api/bookmarks",
        Some(r#"{"name": "pager?", "kind": "marker", "f_center_hz": 930.5e6}"#),
    );
    assert_eq!(marker.status, 201);
    let updated = authed(
        addr,
        "PUT",
        &format!("/api/bookmarks/{id}"),
        Some(r#"{"name": "FM 101.3 (RDS)", "bandwidth_hz": null}"#),
    );
    assert_eq!(updated.status, 200, "{}", updated.body);
    assert!(updated.body["bandwidth_hz"].is_null());
    assert_eq!(updated.body["note"], "RDS", "untouched fields are kept");
    assert_eq!(
        authed(
            addr,
            "PUT",
            &format!("/api/bookmarks/{}", BookmarkId::new()),
            Some(r#"{"name": "x"}"#)
        )
        .status,
        404
    );
    assert_eq!(
        authed(addr, "GET", "/api/bookmarks/not-an-id", None).status,
        404
    );
    drop(server);

    // Persisted: the database has both, and a new server lists them.
    let repo = Repository::open(&db).unwrap();
    let names: Vec<String> = repo
        .bookmarks()
        .unwrap()
        .into_iter()
        .map(|b| b.name)
        .collect();
    assert_eq!(
        names,
        vec!["FM 101.3 (RDS)".to_owned(), "pager?".to_owned()]
    );
    drop(repo);
    let server = start(&dir.0);
    let addr = server.local_addr();
    let list = authed(addr, "GET", "/api/bookmarks", None);
    assert_eq!(list.body["bookmarks"].as_array().unwrap().len(), 2);
    let one = authed(addr, "GET", &format!("/api/bookmarks/{id}"), None);
    assert_eq!(one.body["name"], "FM 101.3 (RDS)");
    let deleted = authed(addr, "DELETE", &format!("/api/bookmarks/{id}"), None);
    assert_eq!(deleted.status, 200);
    assert_eq!(deleted.body["deleted"]["id"], json!(id));
    assert_eq!(
        authed(addr, "GET", &format!("/api/bookmarks/{id}"), None).status,
        404
    );
    let entries = audit_entries(&dir.0.join("audit.jsonl"));
    assert!(
        entries
            .iter()
            .any(|e| e["action"] == "bookmark_delete" && e["old"]["name"] == "FM 101.3 (RDS)")
    );
}

/// T-343 — **a retune is a device action, not a view change.**
///
/// T-339 proved that pause, scrub and zoom never reach the device, and T-347 went further: they
/// reach no route at all, because holding the view is the client's own time cursor. Its sibling
/// property is the other half: exactly one family of requests *does* reach the front end, and it
/// is labelled as such everywhere it can be seen — in the answer, in the audit log, and against a
/// named front end.
///
/// The failure this guards is the one the T-339 audit found: `POST /api/control/center` was
/// indistinguishable from a view change, so a pan let go slightly too far reached
/// `PipelineController::retune`, which stops and re-plumbs the running segment when the window's
/// class or rate changes. Nothing in the code said that route was dangerous.
///
/// The control is the second half of this test: every request that only changes what is shown
/// must reach the device **zero** times and carry **no** `device` field. Without it the property
/// could be satisfied by making the device unreachable altogether.
#[test]
fn t343_device_actions_are_labelled_and_attributed_and_view_changes_reach_nothing() {
    let r = rig(
        "device-actions",
        Token::from_config(TOKEN).unwrap(),
        Device::new(true),
        true,
    );
    let addr = r.server.local_addr();

    // The state names the front end a retune would move, before anything is asked for: the UI can
    // say *which* device it is about to change, rather than discovering it afterwards.
    let state = authed(addr, "GET", "/api/control/state", None);
    assert_eq!(state.body["device"]["device_id"], json!(DEVICE_ID));

    // The five device actions. Each answers with `device: {action, id}` and reaches the driver.
    let device_calls: [(&str, &str, &str); 5] = [
        ("/api/control/center", r#"{"center_hz": 99.5e6}"#, "retune"),
        ("/api/control/rate", r#"{"sample_rate_hz": 10e6}"#, "rate"),
        ("/api/control/gains", r#"{"gains": {"lna": 24}}"#, "gains"),
        ("/api/control/bias_tee", r#"{"enabled": false}"#, "bias_tee"),
        (
            "/api/control/baseband_filter",
            r#"{"bandwidth_hz": 7e6}"#,
            "baseband_filter",
        ),
    ];
    for (path, body, action) in device_calls {
        let before = r.device.calls().len();
        let rep = authed(addr, "POST", path, Some(body));
        assert_eq!(rep.status, 200, "{path}: {:?}", rep.body);
        assert_eq!(
            rep.body["device"],
            json!({ "action": action, "id": DEVICE_ID }),
            "{path} must answer as a device action naming the front end"
        );
        assert!(
            r.device.calls().len() > before,
            "{path} claims to be a device action but reached no driver command"
        );
    }

    // The view side: display and recording. None may reach the device, and none may be labelled a
    // device action — this is the pan-does-not-retune control at the API seam. T-347 removed pause
    // and resume from this list by removing the routes: the view's own Pause is client state and
    // makes no request, which `one_clients_pause_never_freezes_another_clients_stream`
    // (crates/hk-cli/tests/api_contract.rs) asserts across two connected clients.
    let reached_before = r.device.calls().len();
    let view_calls: [(&str, &str); 3] = [
        ("/api/control/display", r#"{"fft_size": 2048}"#),
        ("/api/control/record/start", r#"{"label": "view"}"#),
        ("/api/control/record/stop", "{}"),
    ];
    for (path, body) in view_calls {
        let rep = authed(addr, "POST", path, Some(body));
        assert_eq!(rep.status, 200, "{path}: {:?}", rep.body);
        assert!(
            rep.body.get("device").is_none(),
            "{path} only changes what is shown, so it must not be labelled a device action: {:?}",
            rep.body
        );
    }
    assert_eq!(
        r.device.calls().len(),
        reached_before,
        "a view change reached the front end: {:?}",
        &r.device.calls()[reached_before..]
    );

    // The audit log is where "which device retuned" has to survive. Every device action carries
    // `device.id`; no view change carries a `device` key at all.
    let entries = audit_entries(&r.audit);
    for (path, _, action) in device_calls {
        let e = entries
            .iter()
            .find(|e| e["path"] == path)
            .unwrap_or_else(|| panic!("no audit entry for {path}"));
        assert_eq!(
            e["device"],
            json!({ "action": action, "id": DEVICE_ID }),
            "{path}"
        );
    }
    for (path, _) in view_calls {
        let e = entries.iter().find(|e| e["path"] == path).unwrap();
        assert!(e.get("device").is_none(), "{path}: {e}");
    }
}

/// T-343: a device action that cannot have the front end fails cleanly, naming the device and the
/// holder, instead of racing it to the driver. The HackRF one-agent-at-a-time rule, enforced
/// rather than assumed: the UI is one claimant among others, not a privileged one.
#[test]
fn t343_a_second_device_action_is_refused_while_the_front_end_is_held() {
    use hk_api::{DeviceAction, DeviceGate};

    let r = rig(
        "device-busy",
        Token::from_config(TOKEN).unwrap(),
        Device::new(true),
        true,
    );
    let addr = r.server.local_addr();

    // A long-running device action elsewhere (a HIL run, a scheduler survey, another tab) holds
    // the front end. The gate is the same one every device action passes through.
    let gate = DeviceGate::new(Some(DEVICE_ID.to_owned()));
    let held = gate.enter(DeviceAction::Retune).expect("free");
    let refused = gate.enter(DeviceAction::Gains).unwrap_err();
    assert_eq!(refused.http_status(), 409);
    assert_eq!(refused.code(), "device_busy");
    assert!(refused.to_string().contains(DEVICE_ID), "{refused}");
    drop(held);

    // And the wire form a client sees: `device_busy` is a stable code with a 409, distinct from
    // `not_live` (a replay) and from `conflict` (the run's own state).
    let rep = authed(
        addr,
        "POST",
        "/api/control/center",
        Some(r#"{"center_hz": 99e6}"#),
    );
    assert_eq!(rep.status, 200, "uncontended: {:?}", rep.body);

    // Concurrency: two retunes at once produce one driver tune per accepted request and never two
    // interleaved ones; whichever loses is told the device is busy rather than silently dropped.
    let gate = std::sync::Arc::new(DeviceGate::new(Some(DEVICE_ID.to_owned())));
    let g2 = std::sync::Arc::clone(&gate);
    let holder = gate.enter(DeviceAction::Rate).expect("free");
    let t = std::thread::spawn(move || g2.enter(DeviceAction::Retune).map(|_| ()));
    let e = t.join().unwrap().expect_err("the rate change holds it");
    assert_eq!(e.code(), "device_busy");
    drop(holder);
    assert!(gate.enter(DeviceAction::Retune).is_ok(), "released on drop");
}
