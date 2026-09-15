//! T-079: HTTP/WS API contract tests. `docs/api.md` is the reference; this file exercises it
//! against a real `hk serve` running the mock SDR device over the `fm_100p8M` fixture (T-049), the
//! same composition `hk serve --device mock:<fixture>` uses, so a rewritten UI can rely on the
//! contract without reading the server's source: every documented route's status, JSON shape
//! (required fields and types) and auth refusals. Deterministic within bounded timeouts (the mock
//! runs the fixture in real time and loops it, T-049/T-057, so detection takes real wall-clock
//! seconds, not instant).
//!
//! Coverage: `/api/streams`, `/api/history`, `/api/floor`, `/api/inventory` (including the T-078
//! `state`/`lifecycle`/`recurrence` fields), `/api/inventory/{id}[/promote]` (T-078),
//! `/api/analysis/strongest` (T-079), `/api/status`, `/api/control/*`, `/api/bookmarks[/<id>]`,
//! `/api/selections[/<id>[/links]]`, `/api/outputs[...]`, `/ws/<id>` (spectrum header),
//! `/ws/open/listen` (audio header + PCM data records on the 101.3 MHz station), and auth/CORS
//! refusals. `docs/stream-contract.md` covers stream framing in full; this file only checks the
//! shapes `docs/api.md` promises.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hk_cli::pipeline::{LiveArgs, temp_data_dir};
use hk_cli::serve::{ServeOptions, ServeSource, Serving, start};
use serde_json::{Value, json};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

const TOKEN: &str = "t079-contract-token-0123456789abcdef";
/// The fixture's known station (crates/hk-pipeline/tests/signal_062_pipeline.rs and
/// tests/e2e/tests/mock_device.rs agree on this truth).
const STATION_HZ: f64 = 101.3e6;
const FIXTURE_CENTER_HZ: f64 = 100.8e6;
const FIXTURE_RATE_HZ: f64 = 2.4e6;

type Ws = WebSocket<MaybeTlsStream<TcpStream>>;

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/hackrf/2026-09-13/fm_100p8M_2p4M_l32g30a1_t1p5_5s.sigmf-meta")
}

fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

/// Starts `hk serve` over the mock SDR device on the fixture (the CLI's `mock:<path>` device spec,
/// T-049), a fresh temp data directory and token file, at the fixture's own tuning.
fn start_server() -> (Serving, SocketAddr) {
    let dir = temp_data_dir();
    let serving = start(&ServeOptions {
        source: ServeSource::HackRf {
            spec: format!("mock:{}", fixture_path().display()),
            live: LiveArgs::default(),
        },
        data_dir: Some(dir),
        bind: "127.0.0.1:0".parse().unwrap(),
        ui_dist: None,
        fft_len: 1024,
        rows_per_s: 25.0,
        calibration: None,
        token: Some(TOKEN.into()),
        listen: Default::default(),
        compute: Default::default(),
    })
    .unwrap();
    let addr = serving.server.local_addr();
    (serving, addr)
}

/// Stops the run and waits for it to finish, bounded (the mock loops forever until told to stop).
fn stop_server(serving: Serving) {
    serving.handle.stop();
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = serving.handle;
    std::thread::spawn(move || {
        let _ = tx.send(handle.wait());
    });
    let _ = rx.recv_timeout(Duration::from_secs(30));
    drop(serving.server);
}

/// `METHOD path` with an optional `Authorization` header and JSON body; returns the status and the
/// parsed body (`Value::Null` if the body is empty or not JSON).
fn call(
    addr: SocketAddr,
    method: &str,
    path: &str,
    auth: Option<&str>,
    body: Option<&str>,
) -> (u16, Value) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let body = body.unwrap_or("");
    let ct = if body.is_empty() {
        String::new()
    } else {
        format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        )
    };
    let auth_h = auth.map_or(String::new(), |a| format!("Authorization: {a}\r\n"));
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: t\r\n{auth_h}{ct}Connection: close\r\n\r\n{body}"
    )
    .unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    let status = raw
        .get(9..12)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("malformed response head: {raw:?}"));
    let body = raw.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    (status, serde_json::from_str(body).unwrap_or(Value::Null))
}

fn get(addr: SocketAddr, path: &str) -> (u16, Value) {
    call(addr, "GET", path, Some(&format!("Bearer {TOKEN}")), None)
}

fn post(addr: SocketAddr, path: &str, body: &str) -> (u16, Value) {
    call(
        addr,
        "POST",
        path,
        Some(&format!("Bearer {TOKEN}")),
        Some(body),
    )
}

/// A GET whose body is not JSON: `(status, content type, body)`.
fn get_raw(addr: SocketAddr, path: &str) -> (u16, String, Vec<u8>) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: t\r\nAuthorization: Bearer {TOKEN}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("response head");
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let status = head[9..12].parse().unwrap();
    let content_type = head
        .lines()
        .find_map(|l| l.strip_prefix("Content-Type: "))
        .unwrap_or("")
        .to_owned();
    (status, content_type, raw[split + 4..].to_vec())
}

fn put(addr: SocketAddr, path: &str, body: &str) -> (u16, Value) {
    call(
        addr,
        "PUT",
        path,
        Some(&format!("Bearer {TOKEN}")),
        Some(body),
    )
}

fn delete(addr: SocketAddr, path: &str) -> (u16, Value) {
    call(addr, "DELETE", path, Some(&format!("Bearer {TOKEN}")), None)
}

fn wait_for(what: &str, limit: Duration, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn is_object(v: &Value) -> bool {
    v.is_object()
}

fn is_array(v: &Value) -> bool {
    v.is_array()
}

// --- GET routes: status, required fields, types -------------------------------------------------

#[test]
fn discovery_history_floor_status_and_control_state_have_the_documented_shape() {
    let (serving, addr) = start_server();

    // /api/streams: discovery (T-060), never content.
    let (st, v) = get(addr, "/api/streams");
    assert_eq!(st, 200, "{v}");
    assert!(is_array(&v["streams"]), "{v}");
    assert!(is_array(&v["on_demand"]), "{v}");
    let names: Vec<&str> = v["on_demand"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|o| o["name"].as_str())
        .collect();
    for want in ["listen", "bits", "symbols"] {
        assert!(names.contains(&want), "on_demand openers: {names:?}");
    }
    assert!(v["tcp"]["addr"].is_string(), "{v}");
    wait_for(
        "the spectrum stream to be offered",
        Duration::from_secs(30),
        || {
            get(addr, "/api/streams").1["streams"]
                .as_array()
                .is_some_and(|a| a.iter().any(|s| s["stream_id"] == "spectrum/live"))
        },
    );

    // /api/control/state: device capabilities and tuning for a live source.
    let (st, v) = get(addr, "/api/control/state");
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["live"], json!(true));
    assert!(is_object(&v["device"]), "{v}");
    assert_eq!(v["tuning"]["center_hz"], json!(FIXTURE_CENTER_HZ));
    assert_eq!(v["tuning"]["sample_rate_hz"], json!(FIXTURE_RATE_HZ));
    assert!(is_object(&v["run"]), "{v}");
    assert_eq!(v["transmit"]["available"], json!(false));
    assert!(is_array(&v["routes"]), "{v}");
    assert!(
        v["routes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["path"] == "/api/control/center"),
        "{v}"
    );
    assert!(
        v["routes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["path"] == "/api/control/baseband_filter"),
        "{v}"
    );
    // display_limits (T-067): the UI reads these instead of hard-coding hk-pipeline's DISPLAY_*.
    let limits = &v["display_limits"];
    for field in [
        "fft_size_min",
        "fft_size_max",
        "averaging_max",
        "rows_per_s_min",
        "rows_per_s_max",
        "windows",
    ] {
        assert!(
            limits.get(field).is_some(),
            "display_limits missing {field}: {v}"
        );
    }
    assert!(
        limits["fft_size_max"].as_u64().unwrap() >= limits["fft_size_min"].as_u64().unwrap(),
        "{v}"
    );
    assert!(
        is_array(&limits["windows"])
            && limits["windows"]
                .as_array()
                .unwrap()
                .contains(&json!("hann")),
        "{v}"
    );
    // device.baseband_filter (T-067): the mock device inherits the HackRF's discrete filter list.
    assert!(
        is_array(&v["device"]["baseband_filter"]["values_hz"]),
        "{v}"
    );
    assert!(
        v["tuning"]["baseband_filter_hz"].is_null(),
        "unset until requested: {v}"
    );

    // /api/status: pipeline counters, never content.
    let (st, v) = get(addr, "/api/status");
    assert_eq!(st, 200, "{v}");
    assert!(is_object(&v), "{v}");
    // T-056: compute providers, chosen once per run (docs/api.md `compute`).
    let compute = &v["compute"];
    assert!(is_object(&compute["options"]), "{v}");
    assert!(compute["options"]["provider"].is_string(), "{v}");
    assert!(is_array(&compute["providers"]), "{v}");
    for reader in ["detect", "history", "spectrum"] {
        let sel = &compute["stft"][reader];
        assert!(sel["provider"].is_string(), "{reader}: {v}");
        assert!(sel["requested"].is_string(), "{reader}: {v}");
        assert!(
            sel["fallback"].is_null() || sel["fallback"].is_string(),
            "{reader}: {v}"
        );
    }
    assert_eq!(compute["provider_changes"], json!(0), "{v}");

    // /api/history over the fixture's band: cell grid.
    let t1 = unix_now() + 5.0;
    let (st, v) = get(
        addr,
        &format!(
            "/api/history?f_lo={}&f_hi={}&t0=0&t1={t1}",
            FIXTURE_CENTER_HZ - FIXTURE_RATE_HZ / 2.0,
            FIXTURE_CENTER_HZ + FIXTURE_RATE_HZ / 2.0
        ),
    );
    assert_eq!(st, 200, "{v}");
    for field in [
        "level",
        "f_cell_hz",
        "f_lo_hz",
        "nf",
        "t_cell_s",
        "t0_s",
        "nt",
        "provenance",
    ] {
        assert!(v.get(field).is_some(), "history missing {field}: {v}");
    }
    assert!(is_array(&v["max_db"]) && is_array(&v["occupancy"]), "{v}");
    // T-116: coverage, floor, scheme/format and provenance steps (additive).
    for field in [
        "scheme",
        "tile_format",
        "coverage",
        "floor_db",
        "coverage_summary",
    ] {
        assert!(v.get(field).is_some(), "history missing {field}: {v}");
    }
    assert!(
        is_array(&v["floor_db"]) && is_array(&v["coverage_summary"]["gaps"]),
        "{v}"
    );
    let prov = &v["provenance"];
    for field in [
        "gain_table",
        "filter",
        "spur_mask",
        "cell_shape",
        "steps_dropped",
    ] {
        assert!(prov.get(field).is_some(), "provenance missing {field}: {v}");
    }
    assert!(is_array(&prov["steps"]), "{v}");
    let region = format!(
        "f_lo={}&f_hi={}&t0=0&t1={t1}",
        FIXTURE_CENTER_HZ - FIXTURE_RATE_HZ / 2.0,
        FIXTURE_CENTER_HZ + FIXTURE_RATE_HZ / 2.0
    );
    let (st, ct, body) = get_raw(addr, &format!("/api/history?{region}&format=csv"));
    assert_eq!((st, ct.as_str()), (200, "text/csv; charset=utf-8"));
    let text = String::from_utf8(body).unwrap();
    // Lines may be absent before history has ingested (unobserved lines are omitted); every line
    // present has the hackrf_sweep shape (hk-store tests cover the values).
    for line in text.lines() {
        let f: Vec<&str> = line.split(", ").collect();
        assert!(
            f.len() > 6
                && f[0].len() == 10
                && f[2].parse::<u64>().is_ok()
                && f[3].parse::<u64>().is_ok()
                && f[4].parse::<f64>().is_ok()
                && f[5].parse::<u32>().is_ok()
                && f[6..].iter().all(|x| x.parse::<f64>().is_ok()),
            "hackrf_sweep line: {line}"
        );
    }
    let (st, ct, body) = get_raw(addr, &format!("/api/history?{region}&format=png&stat=max"));
    assert_eq!((st, ct.as_str()), (200, "image/png"));
    assert!(body.starts_with(b"\x89PNG\r\n\x1a\n"));
    let (st, _) = get(addr, &format!("/api/history?{region}&format=xml"));
    assert_eq!(st, 400, "unknown format refused");
    let (st, _) = get(
        addr,
        &format!("/api/history?{region}&format=csv&stat=median"),
    );
    assert_eq!(st, 400, "unknown stat refused");
    let (st, _) = get(addr, "/api/history?f_lo=2&f_hi=1&t0=0&t1=1");
    assert_eq!(st, 400, "inverted region refused");
    let (st, _) = get(addr, "/api/history");
    assert_eq!(st, 400, "missing params refused");

    // /api/floor: same region, floor-vs-time shape (empty steps are fine before calibration).
    let (st, v) = get(
        addr,
        &format!(
            "/api/floor?f_lo={}&f_hi={}&t0=0&t1={t1}",
            FIXTURE_CENTER_HZ - FIXTURE_RATE_HZ / 2.0,
            FIXTURE_CENTER_HZ + FIXTURE_RATE_HZ / 2.0
        ),
    );
    assert_eq!(st, 200, "{v}");
    for field in ["region", "level", "t_cell_s", "shape", "steps"] {
        assert!(v.get(field).is_some(), "floor missing {field}: {v}");
    }
    assert!(is_array(&v["steps"]), "{v}");

    // Method not allowed on a GET-only read endpoint.
    let (st, v) = post(addr, "/api/history", "{}");
    assert_eq!(st, 405, "{v}");

    stop_server(serving);
}

#[test]
fn inventory_and_analysis_strongest_find_the_blind_fm_station() {
    let (serving, addr) = start_server();

    // /api/inventory: the station is found blind, no frequency lookup involved (vision step 4).
    wait_for(
        "the 101.3 MHz station to appear in the inventory",
        Duration::from_secs(60),
        || {
            let (st, v) = get(addr, "/api/inventory");
            st == 200
                && v["entries"].as_array().is_some_and(|a| {
                    a.iter().any(|e| {
                        e["f_center_hz"]
                            .as_f64()
                            .is_some_and(|f| (f - STATION_HZ).abs() < 50e3)
                    })
                })
        },
    );
    let (st, v) = get(addr, "/api/inventory");
    assert_eq!(st, 200, "{v}");
    for field in ["entries", "next_cursor", "limit", "identity_access"] {
        assert!(v.get(field).is_some(), "inventory missing {field}: {v}");
    }
    let row = v["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| {
            e["f_center_hz"]
                .as_f64()
                .is_some_and(|f| (f - STATION_HZ).abs() < 50e3)
        })
        .unwrap();
    for field in [
        "id",
        "f_center_hz",
        "bandwidth_hz",
        "f_lo_hz",
        "f_hi_hz",
        "first_seen_s",
        "last_seen_s",
        "count",
        "known_status",
        "tags",
        "family",
        "explanations",
        "identity_scheme",
        "withheld",
        // T-078 lifecycle fields.
        "state",
        "lifecycle",
        "recurrence",
    ] {
        assert!(
            row.get(field).is_some(),
            "inventory row missing {field}: {row}"
        );
    }
    assert!(row["id"].is_string(), "{row}");
    assert!(
        matches!(row["state"].as_str(), Some("candidate" | "confirmed")),
        "default listing excludes deleted entries: {row}"
    );
    for field in [
        "occurrences",
        "appearances",
        "span_s",
        "on_air_s",
        "duty_cycle",
        "recent",
    ] {
        assert!(
            row["recurrence"].get(field).is_some(),
            "recurrence missing {field}: {row}"
        );
    }

    // Filters and pagination parameters are accepted.
    let (st, v) = get(addr, "/api/inventory?status=known,unknown&limit=5");
    assert_eq!(st, 200, "{v}");
    let (st, v) = get(addr, "/api/inventory?status=bogus");
    assert_eq!(st, 400, "{v}");
    let (st, v) = get(addr, "/api/inventory?state=deleted");
    assert_eq!(st, 200, "{v}");
    let (st, v) = get(addr, "/api/inventory?state=bogus");
    assert_eq!(st, 400, "{v}");

    // /api/analysis/strongest (T-079): the station is the (or a) strongest thing in its own band.
    let (f_lo, f_hi) = (STATION_HZ - 100e3, STATION_HZ + 100e3);
    wait_for(
        "/api/analysis/strongest to find the station",
        Duration::from_secs(60),
        || {
            get(
                addr,
                &format!("/api/analysis/strongest?f_lo={f_lo}&f_hi={f_hi}"),
            )
            .1["found"]
                == json!(true)
        },
    );
    let (st, v) = get(
        addr,
        &format!("/api/analysis/strongest?f_lo={f_lo}&f_hi={f_hi}"),
    );
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["found"], json!(true), "{v}");
    for field in ["f_center_hz", "f_lo_hz", "f_hi_hz", "max_db"] {
        assert!(v[field].is_number(), "strongest missing {field}: {v}");
    }
    assert!(
        v["f_center_hz"].as_f64().unwrap() >= f_lo && v["f_center_hz"].as_f64().unwrap() <= f_hi,
        "{v}"
    );
    // A quiet band well outside the fixture: nothing found.
    let (st, v) = get(addr, "/api/analysis/strongest?f_lo=1e9&f_hi=1.0001e9");
    assert_eq!((st, &v["found"]), (200, &json!(false)));
    // Validation.
    let (st, v) = get(addr, "/api/analysis/strongest?f_lo=2&f_hi=1");
    assert_eq!(st, 400, "{v}");
    let (st, v) = get(
        addr,
        &format!("/api/analysis/strongest?f_lo={f_lo}&f_hi={f_hi}&window_s=0"),
    );
    assert_eq!(st, 400, "{v}");

    stop_server(serving);
}

#[test]
fn inventory_entry_promote_and_delete_answer_as_documented() {
    let (serving, addr) = start_server();

    let id = {
        let mut found = None;
        wait_for(
            "the station to appear so its entry id is known",
            Duration::from_secs(60),
            || {
                let (st, v) = get(addr, "/api/inventory");
                if st != 200 {
                    return false;
                }
                found = v["entries"]
                    .as_array()
                    .and_then(|a| {
                        a.iter().find(|e| {
                            e["f_center_hz"]
                                .as_f64()
                                .is_some_and(|f| (f - STATION_HZ).abs() < 50e3)
                        })
                    })
                    .and_then(|e| e["id"].as_str())
                    .map(str::to_owned);
                found.is_some()
            },
        );
        found.unwrap()
    };

    // GET one entry: same shape as a list row.
    let (st, row) = get(addr, &format!("/api/inventory/{id}"));
    assert_eq!(st, 200, "{row}");
    assert_eq!(row["id"], json!(id));
    for field in ["state", "lifecycle", "recurrence"] {
        assert!(
            row.get(field).is_some(),
            "inventory entry missing {field}: {row}"
        );
    }
    let (st, v) = get(addr, "/api/inventory/not-a-uuid");
    assert_eq!(st, 404, "{v}");

    // Promote: candidate -> confirmed (idempotent: a second promote reports changed: false).
    let (st, v) = post(addr, &format!("/api/inventory/{id}/promote"), "{}");
    assert_eq!(st, 200, "{v}");
    assert!(v.get("entry").is_some(), "{v}");
    assert_eq!(v["entry"]["state"], json!("confirmed"), "{v}");
    let (st, v) = post(
        addr,
        &format!("/api/inventory/{id}/promote"),
        r#"{"reason": "manual re-check"}"#,
    );
    assert_eq!((st, &v["changed"]), (200, &json!(false)), "{v}");
    let (st, v) = post(
        addr,
        &format!("/api/inventory/{id}/promote"),
        r#"{"bogus": 1}"#,
    );
    assert_eq!((st, v["code"].as_str()), (400, Some("invalid")), "{v}");

    // Delete: leaves the default (candidate/confirmed) list but is still readable with state=deleted.
    let (st, v) = delete(addr, &format!("/api/inventory/{id}"));
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["deleted"]["id"], json!(id), "{v}");
    let (st, v) = get(addr, "/api/inventory");
    assert_eq!(st, 200, "{v}");
    assert!(
        !v["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["id"] == id),
        "a deleted entry must not appear in the default list: {v}"
    );
    let (st, v) = get(addr, "/api/inventory?state=deleted");
    assert_eq!(st, 200, "{v}");
    assert!(
        v["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["id"] == id),
        "state=deleted still lists it: {v}"
    );
    let (st, v) = delete(addr, &format!("/api/inventory/{id}"));
    assert_eq!(
        (st, v["code"].as_str()),
        (404, Some("not_found")),
        "deleting twice: {v}"
    );

    stop_server(serving);
}

// --- Control API: display/pause (device-independent), bookmarks, selections, outputs ------------

#[test]
fn control_display_pause_and_bookmarks_answer_as_documented() {
    let (serving, addr) = start_server();

    let (st, v) = post(
        addr,
        "/api/control/display",
        r#"{"fft_size": 512, "averaging": 2, "rows_per_s": 10}"#,
    );
    assert_eq!(st, 200, "{v}");
    assert_eq!(
        v["display"],
        json!({"fft_size": 512, "averaging": 2, "rows_per_s": 10.0, "paused": false, "window": "hann"})
    );
    let (st, v) = post(addr, "/api/control/display", "{}");
    assert_eq!((st, v["code"].as_str()), (400, Some("invalid")), "{v}");
    let (st, v) = post(addr, "/api/control/pause", "{}");
    assert_eq!((st, &v["display"]["paused"]), (200, &json!(true)));
    let (st, v) = post(addr, "/api/control/resume", "{}");
    assert_eq!((st, &v["display"]["paused"]), (200, &json!(false)));

    // Display window (T-067).
    let (st, v) = post(
        addr,
        "/api/control/display",
        r#"{"window": "blackman-harris"}"#,
    );
    assert_eq!(
        (st, v["display"]["window"].as_str()),
        (200, Some("blackman-harris")),
        "{v}"
    );
    let (st, v) = post(addr, "/api/control/display", r#"{"window": "kaiser"}"#);
    assert_eq!((st, v["code"].as_str()), (400, Some("invalid")), "{v}");

    // Baseband filter (T-067): validated against device.baseband_filter's discrete list.
    let (st, v) = post(
        addr,
        "/api/control/baseband_filter",
        r#"{"bandwidth_hz": 7e6}"#,
    );
    assert_eq!(
        (st, v["tuning"]["baseband_filter_hz"].as_f64()),
        (200, Some(7e6)),
        "{v}"
    );
    let (st, v) = post(
        addr,
        "/api/control/baseband_filter",
        r#"{"bandwidth_hz": 9.5e6}"#,
    );
    assert_eq!((st, v["code"].as_str()), (400, Some("out_of_range")), "{v}");

    // Bookmarks: create (201), list, get, update (including rename), delete.
    let (st, bm) = post(
        addr,
        "/api/bookmarks",
        r#"{"name": "test mark", "f_center_hz": 101.3e6}"#,
    );
    assert_eq!(st, 201, "{bm}");
    for field in [
        "id",
        "kind",
        "name",
        "f_center_hz",
        "created_s",
        "updated_s",
    ] {
        assert!(bm.get(field).is_some(), "bookmark missing {field}: {bm}");
    }
    let id = bm["id"].as_str().unwrap();
    let (st, list) = get(addr, "/api/bookmarks");
    assert_eq!(st, 200, "{list}");
    assert!(
        list["bookmarks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["id"] == id)
    );
    let (st, got) = get(addr, &format!("/api/bookmarks/{id}"));
    assert_eq!((st, got["name"].as_str()), (200, Some("test mark")));
    let (st, updated) = put(
        addr,
        &format!("/api/bookmarks/{id}"),
        r#"{"name": "renamed"}"#,
    );
    assert_eq!((st, updated["name"].as_str()), (200, Some("renamed")));
    let (st, deleted) = delete(addr, &format!("/api/bookmarks/{id}"));
    assert_eq!((st, deleted["deleted"]["id"].as_str()), (200, Some(id)));
    let (st, missing) = get(addr, &format!("/api/bookmarks/{id}"));
    assert_eq!((st, missing["code"].as_str()), (404, Some("not_found")));

    stop_server(serving);
}

#[test]
fn selections_crud_and_links_answer_as_documented() {
    let (serving, addr) = start_server();

    let (st, s) = post(
        addr,
        "/api/selections",
        r#"{"name": "band", "f_lo": 101.2e6, "f_hi": 101.4e6}"#,
    );
    assert_eq!(st, 201, "{s}");
    for field in [
        "id", "name", "f_lo", "f_hi", "t_lo", "t_hi", "notes", "tags", "links", "created",
        "updated",
    ] {
        assert!(s.get(field).is_some(), "selection missing {field}: {s}");
    }
    let id = s["id"].as_str().unwrap();
    let (st, list) = get(addr, "/api/selections");
    assert_eq!(st, 200, "{list}");
    assert!(
        list["selections"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["id"] == id)
    );
    let (st, got) = get(addr, &format!("/api/selections/{id}"));
    assert_eq!((st, got["name"].as_str()), (200, Some("band")));
    let (st, updated) = put(
        addr,
        &format!("/api/selections/{id}"),
        r#"{"name": "renamed band"}"#,
    );
    assert_eq!((st, updated["name"].as_str()), (200, Some("renamed band")));
    let (st, linked) = post(
        addr,
        &format!("/api/selections/{id}/links"),
        r#"{"kind": "inspection", "target": "manual note"}"#,
    );
    assert_eq!(st, 201, "{linked}");
    assert_eq!(linked["links"].as_array().unwrap().len(), 1, "{linked}");
    let (st, deleted) = delete(addr, &format!("/api/selections/{id}"));
    assert_eq!((st, deleted["deleted"]["id"].as_str()), (200, Some(id)));
    let (st, missing) = get(addr, &format!("/api/selections/{id}"));
    assert_eq!((st, missing["code"].as_str()), (404, Some("not_found")));
    let (st, bad) = post(
        addr,
        "/api/selections",
        r#"{"name": "x", "f_lo": 2, "f_hi": 1}"#,
    );
    assert_eq!(st, 400, "{bad}");

    stop_server(serving);
}

#[test]
fn outputs_list_and_unknown_file_answer_as_documented() {
    let (serving, addr) = start_server();

    let (st, v) = get(addr, "/api/outputs");
    assert_eq!(st, 200, "{v}");
    assert!(is_array(&v["recordings"]), "{v}");
    let (st, v) = get(addr, "/api/outputs/no-such-id/files/no-such-file");
    assert_eq!(st, 404, "{v}");
    let (st, v) = post(addr, "/api/outputs/record/stop", r#"{"id": "no-such-id"}"#);
    assert_eq!(st, 404, "{v}");
    let (st, v) = post(addr, "/api/outputs/record/start", r#"{"kinds": ["bits"]}"#);
    assert_eq!((st, v["code"].as_str()), (400, Some("invalid")), "{v}");

    stop_server(serving);
}

// --- WebSocket routes -----------------------------------------------------------------------------

fn connect_ws(addr: SocketAddr, path: &str) -> Result<Ws, tungstenite::Error> {
    let (mut ws, _) = tungstenite::connect(format!("ws://{addr}{path}"))?;
    if let MaybeTlsStream::Plain(s) = ws.get_mut() {
        s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    }
    Ok(ws)
}

#[test]
fn ws_stream_header_matches_the_stream_contract() {
    let (serving, addr) = start_server();
    wait_for(
        "the spectrum stream to be offered",
        Duration::from_secs(30),
        || {
            get(addr, "/api/streams").1["streams"]
                .as_array()
                .is_some_and(|a| a.iter().any(|s| s["stream_id"] == "spectrum/live"))
        },
    );

    let mut ws = connect_ws(addr, &format!("/ws/spectrum/live?token={TOKEN}")).unwrap();
    let Message::Text(text) = ws.read().unwrap() else {
        panic!("first message must be the header (text)")
    };
    let header: Value = serde_json::from_str(text.as_str()).unwrap();
    assert_eq!(header["schema"], json!("hackriff.stream"));
    assert_eq!(header["stream_id"], json!("spectrum/live"));
    assert_eq!(header["kind"], json!("spectrum"));
    assert!(header["content_class"].is_string(), "{header}");
    let _ = ws.close(None);

    // No token: refused before the upgrade.
    let err = connect_ws(addr, "/ws/spectrum/live").unwrap_err();
    match err {
        tungstenite::Error::Http(resp) => assert_eq!(resp.status().as_u16(), 401),
        e => panic!("expected an HTTP 401 refusal, got {e}"),
    }

    stop_server(serving);
}

#[test]
fn ws_open_listen_streams_pcm_data_records_of_the_station() {
    let (serving, addr) = start_server();
    let (f_lo, f_hi) = (STATION_HZ - 100e3, STATION_HZ + 100e3);

    let (mut ws, header) = wait_for_listen(addr, f_lo, f_hi);
    assert_eq!(header["schema"], json!("hackriff.stream"));
    assert_eq!(header["kind"], json!("audio"));
    assert_eq!(header["datatype"], json!("ri16_le"));
    assert!(header["audio"]["mode"].is_string(), "{header}");

    // At least one binary data record (type 1: 32-byte header + i16 LE PCM payload) within a
    // bounded number of messages (status records, type 3, interleave).
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut saw_data = false;
    while Instant::now() < deadline && !saw_data {
        match ws.read() {
            Ok(Message::Binary(b)) => {
                assert!(
                    b.len() >= 32,
                    "binary record shorter than the 32-byte header: {}",
                    b.len()
                );
                let record_type = b[0];
                assert!(
                    matches!(record_type, 1..=3),
                    "unknown record type {record_type}"
                );
                if record_type == 1 {
                    assert_eq!(
                        (b.len() - 32) % 2,
                        0,
                        "ri16_le PCM payload must be a whole number of samples"
                    );
                    saw_data = true;
                }
            }
            Ok(Message::Text(_) | Message::Ping(_) | Message::Pong(_)) => {}
            Ok(other) => panic!("unexpected message: {other:?}"),
            Err(e) => panic!("listen stream ended early: {e}"),
        }
    }
    assert!(
        saw_data,
        "no PCM data record arrived on the station within 30 s"
    );
    let _ = ws.close(None);

    // Refused: no target parameters.
    let mut refused = connect_ws(addr, &format!("/ws/open/listen?token={TOKEN}")).unwrap();
    let msg = loop {
        match refused.read().unwrap() {
            Message::Text(t) => break t,
            _ => continue,
        }
    };
    let v: Value = serde_json::from_str(msg.as_str()).unwrap();
    assert_eq!(v["type"], json!("refused"));
    assert!(v["status"].is_number() && v["code"].is_string(), "{v}");

    stop_server(serving);
}

/// Retries the `/ws/open/listen` handshake: the run may be mid-replumb (503 `replumbing`) right
/// after start, before the mock's power-on window settles. Returns the connection *after* its
/// header message, plus the parsed header (the caller's next read is the first data/status record).
fn wait_for_listen(addr: SocketAddr, f_lo: f64, f_hi: f64) -> (Ws, Value) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let mut ws = connect_ws(
            addr,
            &format!("/ws/open/listen?f_lo={f_lo}&f_hi={f_hi}&token={TOKEN}"),
        )
        .unwrap();
        match ws.read().unwrap() {
            Message::Text(t) if t.contains("\"type\":\"refused\"") => {
                assert!(Instant::now() < deadline, "listen kept refusing: {t}");
                std::thread::sleep(Duration::from_millis(200));
            }
            Message::Text(t) => {
                let header: Value = serde_json::from_str(t.as_str()).unwrap();
                assert_eq!(header["schema"], json!("hackriff.stream"), "{header}");
                return (ws, header);
            }
            other => panic!("unexpected first message: {other:?}"),
        }
    }
}

// --- Auth and CORS ---------------------------------------------------------------------------------

#[test]
fn unauthenticated_wrong_token_and_cross_origin_requests_are_refused() {
    let (serving, addr) = start_server();

    let (st, v) = call(addr, "GET", "/api/streams", None, None);
    assert_eq!(st, 401, "{v}");
    let (st, v) = call(addr, "GET", "/api/streams", Some("Bearer nope"), None);
    assert_eq!(st, 401, "{v}");

    // A read endpoint accepts the query-string token (browser WebSockets cannot set headers).
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(
        s,
        "GET /api/streams?token={TOKEN} HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    assert!(
        raw.starts_with("HTTP/1.1 200"),
        "?token= works for a GET: {raw}"
    );

    // Mutating requests refuse the query-string token (must be the Authorization header).
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let body = r#"{"fft_size": 512}"#;
    write!(
        s,
        "POST /api/control/display?token={TOKEN} HTTP/1.1\r\nHost: t\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    assert!(
        raw.starts_with("HTTP/1.1 401"),
        "?token= refused for a mutation: {raw}"
    );

    // Cross-origin mutating request (Origin naming a different host than Host).
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(
        s,
        "POST /api/control/display HTTP/1.1\r\nHost: {addr}\r\nOrigin: https://evil.example\r\n\
         Authorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    assert!(
        raw.starts_with("HTTP/1.1 403"),
        "cross-origin mutation refused: {raw}"
    );

    // OPTIONS preflight: always refused (no CORS).
    let (st, _) = call(addr, "OPTIONS", "/api/control/display", None, None);
    assert_eq!(st, 403);

    // Unknown route: 404. Known path, wrong method: 405 with Allow.
    let (st, v) = get(addr, "/api/no-such-route");
    assert_eq!(st, 404, "{v}");
    let (st, v) = call(
        addr,
        "PATCH",
        "/api/bookmarks",
        Some(&format!("Bearer {TOKEN}")),
        None,
    );
    assert_eq!(st, 405, "{v}");

    stop_server(serving);
}

// --- Decoder workbench (ADR-0011 §7): each task appends its test fns under its own marker ---
// T-088 recipes and pipelines

/// T-088: `/api/blocks`, `/api/recipes[...]`, `/api/pipelines[...]` and the `stage` opener, as
/// `docs/api.md` "Recipes and pipelines" documents them, on the mock device's FM window.
#[test]
fn recipe_and_pipeline_routes_match_the_documented_shapes() {
    let (serving, addr) = start_server();
    let bearer = format!("Bearer {TOKEN}");
    let rid = "t088-contract";

    let (st, v) = get(addr, "/api/blocks");
    assert_eq!(st, 200, "{v}");
    let identity = v["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["name"] == "identity")
        .cloned()
        .expect("the identity block is in the catalogue");
    for k in ["version", "group", "doc", "inputs", "outputs", "params"] {
        assert!(!identity[k].is_null(), "{k}: {identity}");
    }

    let doc = json!({
        "schema": "hackriff.recipe", "schema_version": 2, "id": rid, "version": 1,
        "name": "T-088 contract",
        "input": {"port": "iq", "sample_rate_hz": 240000.0, "bandwidth_hz": 200000.0},
        "nodes": [{"id": "a", "block": "identity"}],
        "outputs": [{"id": "base", "kind": "stage", "from": "a"}],
        "output_policy": {"content_class": "unrestricted"}
    });
    let mut bad = doc.clone();
    bad["nodes"][0]["block"] = json!("no_such_block");

    // Validate: 200 either way, with paths.
    let (st, v) = post(addr, "/api/recipes/validate", &doc.to_string());
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["valid"], json!(true));
    assert_eq!(
        v["edges"][0],
        json!({"node": "a", "port": "in", "from": "input", "type": "iq"})
    );
    let (st, v) = post(addr, "/api/recipes/validate", &bad.to_string());
    assert_eq!((st, &v["valid"]), (200, &json!(false)), "{v}");
    assert_eq!(v["errors"][0]["path"], json!("nodes[0].block"));

    // Save: invalid 400 with paths; valid 201 as latest + 1.
    let (st, v) = post(addr, "/api/recipes", &bad.to_string());
    assert_eq!((st, &v["code"]), (400, &json!("invalid")), "{v}");
    assert!(v["errors"].is_array() && v["warnings"].is_array(), "{v}");
    for want in [1, 2] {
        let (st, v) = post(addr, "/api/recipes", &doc.to_string());
        assert_eq!(st, 201, "{v}");
        assert_eq!((&v["id"], &v["version"]), (&json!(rid), &json!(want)));
    }
    let (st, v) = get(addr, "/api/recipes");
    assert_eq!(st, 200);
    let mine = v["recipes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == rid)
        .cloned()
        .unwrap();
    assert_eq!(mine["versions"], json!([1, 2]));
    assert_eq!(mine["builtin"], json!(false));
    assert!(
        v["recipes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == "rds" && r["builtin"] == json!(true)),
        "built-in recipes are listed: {v}"
    );
    let (st, v) = get(addr, &format!("/api/recipes/{rid}"));
    assert_eq!((st, &v["version"]), (200, &json!(2)), "{v}");
    let (st, v) = get(addr, &format!("/api/recipes/{rid}/versions/1"));
    assert_eq!((st, &v["version"]), (200, &json!(1)), "{v}");
    let (st, v) = get(addr, "/api/recipes/no-such-recipe");
    assert_eq!((st, &v["code"]), (404, &json!("not_found")), "{v}");

    // Start on the station's band: 201 with the documented pipeline shape.
    let start = json!({"recipe_id": rid,
        "target": {"band": {"f_lo": STATION_HZ - 100e3, "f_hi": STATION_HZ + 100e3}}});
    let (st, p) = post(addr, "/api/pipelines", &start.to_string());
    assert_eq!(st, 201, "{p}");
    let pid = p["id"].as_str().unwrap().to_owned();
    assert_eq!(p["state"], json!("running"));
    assert_eq!(
        (&p["recipe_version"], &p["edit_rev"]),
        (&json!(2), &json!(0))
    );
    assert!(
        (p["channel"]["sample_rate_hz"].as_f64().unwrap() - 240e3).abs() < 1.0,
        "{p}"
    );
    assert_eq!(
        p["outputs"][0]["stream_id"],
        json!(format!("stage/{pid}/base"))
    );
    assert_eq!(p["nodes"][0]["id"], json!("a"));
    assert!(p["stats"].is_object() && p["status"].is_object() && p["target"]["band"].is_object());
    // T-111: the `messages` outputs' decode counters.
    assert!(
        p["stats"]["decodes"].is_u64() && p["stats"]["decodes_dropped"].is_u64(),
        "{p}"
    );
    let (st, v) = get(addr, "/api/pipelines");
    assert_eq!(st, 200);
    assert!(
        v["pipelines"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["id"] == json!(pid))
    );
    let (st, _) = get(addr, &format!("/api/pipelines/{pid}"));
    assert_eq!(st, 200);
    let (st, v) = post(
        addr,
        "/api/pipelines",
        &json!({"recipe_id": rid, "target": {"band": {"f_lo": 90e6, "f_hi": 90.1e6}}}).to_string(),
    );
    assert_eq!((st, &v["code"]), (409, &json!("outside_window")), "{v}");

    // A stage tap over WebSocket: iq header, then data records.
    let mut ws = connect_ws(
        addr,
        &format!("/ws/open/stage?token={TOKEN}&pipeline={pid}&node=a"),
    )
    .unwrap();
    let Message::Text(t) = ws.read().unwrap() else {
        panic!("header first")
    };
    let header: Value = serde_json::from_str(t.as_str()).unwrap();
    assert_eq!(
        (&header["kind"], &header["datatype"]),
        (&json!("iq"), &json!("cf32_le"))
    );
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        assert!(Instant::now() < deadline, "a stage data record");
        if let Message::Binary(_) = ws.read().unwrap() {
            break;
        }
    }
    let _ = ws.close(None);

    // Hot edit: a node added; an invalid draft leaves the revision untouched.
    let mut edited = doc.clone();
    edited["nodes"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id": "b", "block": "identity"}));
    let (st, v) = put(
        addr,
        &format!("/api/pipelines/{pid}/recipe"),
        &edited.to_string(),
    );
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["edit_rev"], json!(1));
    assert!(v["applied_at_sample"].is_u64(), "{v}");
    assert!(
        v["plan"]["nodes"]
            .as_array()
            .unwrap()
            .contains(&json!({"id": "b", "change": "added"})),
        "{v}"
    );
    assert!(v["swap"]["rebuilt"].is_u64());
    let (st, v) = put(
        addr,
        &format!("/api/pipelines/{pid}/recipe"),
        &bad.to_string(),
    );
    assert_eq!((st, &v["code"]), (400, &json!("invalid")), "{v}");
    assert_eq!(
        get(addr, &format!("/api/pipelines/{pid}")).1["edit_rev"],
        json!(1)
    );

    // Save the running revision; stop; delete.
    let (st, v) = post(addr, &format!("/api/pipelines/{pid}/save"), "");
    assert_eq!((st, &v["version"]), (201, &json!(3)), "{v}");
    let (st, v) = call(
        addr,
        "DELETE",
        &format!("/api/pipelines/{pid}"),
        Some(&bearer),
        None,
    );
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["stopped"]["state"], json!("ended"));
    let (st, _) = get(addr, &format!("/api/pipelines/{pid}"));
    assert_eq!(st, 404);
    let (st, v) = call(
        addr,
        "DELETE",
        &format!("/api/recipes/{rid}"),
        Some(&bearer),
        None,
    );
    assert_eq!(
        (st, &v["deleted_versions"]),
        (200, &json!([1, 2, 3])),
        "{v}"
    );

    // Mutations need the bearer header.
    let (st, _) = call(
        addr,
        "POST",
        "/api/pipelines",
        None,
        Some(&start.to_string()),
    );
    assert_eq!(st, 401);
    stop_server(serving);
}

// T-089 inspector

/// T-089: `POST /api/inspector/parse` evaluates a draft field map (the RDS worked recipe's) over
/// submitted frames and answers layer trees with absolute bit/byte ranges and the per-byte leaf
/// index; `POST /api/captures/{id}/parse` answers 503 on a server without a capture store; auth,
/// method and validation refusals as documented.
#[test]
fn inspector_parse_answers_as_documented() {
    let (serving, addr) = start_server();
    let recipe: Value = serde_json::from_str(
        &std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../recipes/rds.recipe.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let map = &recipe["field_maps"]["rds_group"];
    // Group 0A: PI 0x54A8, TP, PTY 10, PS segment 1 "CK".
    let body = json!({"field_map": map, "frames": [{"hex": "54a8054de0cd434b", "bit_len": 64}]});
    let (st, v) = post(addr, "/api/inspector/parse", &body.to_string());
    assert_eq!(st, 200, "{v}");
    let frame = &v["frames"][0];
    assert_eq!(frame["bit_len"], 64);
    assert_eq!(frame["hex"], "54a8054de0cd434b");
    let layers = &frame["layers"];
    assert_eq!(layers["fit"], "ok");
    let nodes = layers["nodes"].as_array().unwrap();
    for n in nodes {
        for key in ["id", "name", "path", "type", "bits", "bytes"] {
            assert!(n.get(key).is_some(), "node lacks {key}: {n}");
        }
    }
    let node = |path: &str| nodes.iter().find(|n| n["path"] == path).unwrap();
    assert_eq!(node("pi")["text"], "0x54A8");
    assert_eq!(node("pi")["bytes"], json!([0, 2]));
    assert_eq!(node("ps.segment")["value"], 1);
    assert_eq!(node("ps.chars")["value"], "CK");
    assert_eq!(node("ps.flags.music")["type"], "flag");
    assert_eq!(layers["byte_index"].as_array().unwrap().len(), 8);
    assert_eq!(layers["byte_index"][7], json!([node("ps.chars")["id"]]));
    assert_eq!(v["fit"]["ok"], 1);

    let (st, v) = post(
        addr,
        "/api/inspector/parse",
        &json!({"field_map": {"fields": [{"name": "Bad", "type": "uint", "length": 1}]},
                "frames": [{"hex": "00"}]})
        .to_string(),
    );
    assert_eq!((st, v["code"].as_str()), (400, Some("invalid")), "{v}");
    assert!(v["errors"].as_array().is_some_and(|e| !e.is_empty()), "{v}");

    // T-092: `hk serve` has a capture store, so an unknown capture is 404 (503 = no store).
    let (st, v) = post(addr, "/api/captures/any/parse", "{}");
    assert_eq!((st, v["code"].as_str()), (404, Some("not_found")), "{v}");
    let (st, _) = call(addr, "POST", "/api/inspector/parse", None, Some("{}"));
    assert_eq!(st, 401);
    let (st, _) = get(addr, "/api/inspector/parse");
    assert_eq!(st, 405);

    stop_server(serving);
}

// T-091 assist

/// Bits of `v` (`n` bits, MSB first) as a `"0101…"` string.
fn t091_bits(v: u64, n: usize) -> String {
    (0..n)
        .rev()
        .map(|i| if (v >> i) & 1 == 1 { '1' } else { '0' })
        .collect()
}

/// Bit-serial CRC-16 (poly 0x1021, init 0) over a bit string.
fn t091_crc16(bits: &str) -> u64 {
    let mut reg = 0u64;
    for b in bits.bytes() {
        let top = ((reg >> 15) & 1) ^ u64::from(b == b'1');
        reg = (reg << 1) & 0xFFFF;
        if top == 1 {
            reg ^= 0x1021;
        }
    }
    reg
}

/// T-091: `/api/assist/{sync,fields,crc}` answer scored suggestions in the documented shapes,
/// refuse bad input with 400 and other methods with 405. Blind: the posted data carry a CRC-16
/// and a sync word the request never names.
#[test]
fn assist_routes_answer_suggestions_as_documented() {
    let (serving, addr) = start_server();
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    // 24 frames: 8-bit constant, 16-bit counter, 32 random bits, CRC-16 over all of it.
    let frames: Vec<String> = (0..24u64)
        .map(|i| {
            let mut f = t091_bits(0xA7, 8) + &t091_bits(100 + i, 16) + &t091_bits(next() >> 32, 32);
            let c = t091_crc16(&f);
            f.push_str(&t091_bits(c, 16));
            f
        })
        .collect();
    let frames_json: Vec<Value> = frames.iter().map(|f| json!({ "bits": f })).collect();

    let (st, v) = post(
        addr,
        "/api/assist/crc",
        &json!({ "frames": frames_json }).to_string(),
    );
    assert_eq!(st, 200, "{v}");
    assert!(is_array(&v["codes"]) && is_array(&v["parity"]), "{v}");
    assert!(
        v["work"]["partial"].is_boolean() && v["work"]["ops"].is_u64(),
        "{v}"
    );
    let code = &v["codes"][0];
    assert_eq!(code["generator"].as_u64(), Some(0x1_1021), "{v}");
    assert_eq!(code["fragment"]["block"].as_str(), Some("crc"), "{v}");
    assert!(code["score"].is_f64() && is_array(&code["reasons"]), "{v}");
    assert!(code["differences"].as_u64().is_some_and(|d| d >= 20), "{v}");
    assert!(code["score"].as_f64().is_some_and(|s| s > 0.9), "{v}");
    // T-105: 23 differences leave no competing generator, so no ambiguous group.
    assert!(code.get("ambiguous_with").is_none(), "{v}");

    let (st, v) = post(
        addr,
        "/api/assist/fields",
        &json!({ "frames": frames_json }).to_string(),
    );
    assert_eq!(st, 200, "{v}");
    assert!(
        is_array(&v["suggestions"]) && is_array(&v["field_map"]["fields"]),
        "{v}"
    );
    assert!(
        is_array(&v["field_map_errors"]) && is_array(&v["per_bit"]),
        "{v}"
    );
    assert_eq!(v["frames_aligned"].as_u64(), Some(24), "{v}");

    // A stream: 20 packets of alternating preamble + 0x1ACF sync + 40 random bits.
    let mut stream = String::new();
    for _ in 0..20 {
        stream.push_str(&"10".repeat(16));
        stream.push_str(&t091_bits(0x1ACF, 16));
        stream.push_str(&t091_bits(next() >> 24, 40));
    }
    let (st, v) = post(
        addr,
        "/api/assist/sync",
        &json!({ "bits": stream }).to_string(),
    );
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["input"].as_str(), Some("bits"), "{v}");
    assert!(
        is_array(&v["syncs"]) && is_array(&v["periods"]) && is_array(&v["block_codes"]),
        "{v}"
    );
    assert_eq!(v["syncs"][0]["hex"].as_str(), Some("0x1ACF"), "{v}");
    let sync = &v["syncs"][0];
    assert!(
        sync["significance_bits"].is_f64()
            && sync["relative_score"].is_f64()
            && sync["score"].as_f64().is_some_and(|s| s > 0.5),
        "{v}"
    );
    assert_eq!(
        v["syncs"][0]["fragment"]["block"].as_str(),
        Some("sync_search"),
        "{v}"
    );
    let (st, v) = post(
        addr,
        "/api/assist/sync",
        &json!({ "frames": frames_json, "max_ops": 1000 }).to_string(),
    );
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["input"].as_str(), Some("frames"), "{v}");
    assert!(v["work"]["partial"].is_boolean(), "{v}");

    let (st, v) = post(addr, "/api/assist/crc", r#"{"frames": [{"bits": "01x"}]}"#);
    assert_eq!((st, v["code"].as_str()), (400, Some("invalid")), "{v}");
    let (st, v) = post(
        addr,
        "/api/assist/sync",
        r#"{"bits": "0101", "frames": []}"#,
    );
    assert_eq!((st, v["code"].as_str()), (400, Some("invalid")), "{v}");
    let (st, v) = get(addr, "/api/assist/crc");
    assert_eq!(st, 405, "{v}");

    stop_server(serving);
}

// T-092 captures

/// T-092: a running pipeline's inspector output is recorded with no request. `GET /api/captures`
/// lists it; `/frames` scrubs by frame and by time through the index; `POST .../parse` re-parses
/// it with a draft field map; `DELETE` refuses a recording capture and deletes a finished one;
/// `/ws/open/inspector?capture=` replays it. All as `docs/api.md` "Decoded captures" documents,
/// on the mock device's FM window. The recipe (FM discriminator, clock recovery, slicer, 32-bit
/// deframe) yields frames from any signal, so the test does not depend on a decoder's lock.
/// The documented replay cap (503 `busy` beyond 4 at once) is exercised in hk-pipeline's
/// `decoded_capture` test, which can hold replays open without a WebSocket client.
#[test]
fn decoded_captures_are_recorded_listed_scrubbed_reparsed_and_replayed_as_documented() {
    let (serving, addr) = start_server();
    let (st, v) = get(addr, "/api/captures");
    assert_eq!(st, 200, "{v}");
    assert!(is_array(&v["captures"]), "{v}");

    let doc = json!({
        "schema": "hackriff.recipe", "schema_version": 2, "id": "t092-contract", "version": 1,
        "name": "T-092 contract",
        "input": {"port": "iq", "sample_rate_hz": 48000.0, "bandwidth_hz": 40000.0},
        "nodes": [
            {"id": "fm", "block": "fm_demod", "params": {"deviation_hz": 5000}},
            {"id": "clock", "block": "clock_recovery", "params": {"symbol_rate_bd": 1000.0}},
            {"id": "bits", "block": "slicer"},
            {"id": "words", "block": "deframe", "params": {"frame_bits": 32}}
        ],
        "outputs": [{"id": "words", "kind": "inspector", "from": "words"}],
        "output_policy": {"content_class": "unrestricted"}
    });
    let (st, v) = post(addr, "/api/recipes/validate", &doc.to_string());
    assert_eq!((st, &v["valid"]), (200, &json!(true)), "{v}");
    let target = json!({"band": {"f_lo": STATION_HZ - 20e3, "f_hi": STATION_HZ + 20e3}});
    let (st, v) = post(
        addr,
        "/api/pipelines",
        &json!({"recipe": doc, "target": target}).to_string(),
    );
    assert_eq!(st, 201, "{v}");
    let pid = v["id"].as_str().unwrap().to_owned();

    // Recorded automatically, listed while recording.
    let mut cap = Value::Null;
    wait_for(
        "a recorded capture with frames",
        Duration::from_secs(90),
        || {
            let (_, v) = get(addr, "/api/captures");
            cap = v["captures"]
                .as_array()
                .and_then(|a| a.iter().find(|c| c["pipeline_id"] == json!(pid)).cloned())
                .unwrap_or(Value::Null);
            cap["frames"].as_u64().is_some_and(|n| n >= 20)
        },
    );
    for k in [
        "id",
        "pipeline_id",
        "recipe_id",
        "recipe_version",
        "output_id",
        "stream_id",
        "content_class",
        "segment",
        "started",
        "ended",
        "t_first",
        "t_last",
        "frames",
        "bytes",
        "dropped_records",
        "recording",
        "end_reason",
    ] {
        assert!(cap.get(k).is_some(), "Capture lacks {k}: {cap}");
    }
    assert_eq!(cap["recording"], json!(true), "{cap}");
    assert_eq!(
        (&cap["recipe_id"], &cap["output_id"], &cap["stream_id"]),
        (
            &json!("t092-contract"),
            &json!("words"),
            &json!(format!("inspector/{pid}/words"))
        )
    );
    let cid = cap["id"].as_str().unwrap().to_owned();
    let (st, v) = get(addr, &format!("/api/captures/{cid}"));
    assert_eq!((st, &v["id"]), (200, &json!(cid)), "{v}");
    let (st, v) = delete(addr, &format!("/api/captures/{cid}"));
    assert_eq!((st, v["code"].as_str()), (409, Some("conflict")), "{v}");

    // Stop the pipeline: the capture finishes.
    let (st, v) = delete(addr, &format!("/api/pipelines/{pid}"));
    assert_eq!(st, 200, "{v}");
    wait_for("the capture to finish", Duration::from_secs(30), || {
        get(addr, &format!("/api/captures/{cid}")).1["recording"] == json!(false)
    });
    let (_, cap) = get(addr, &format!("/api/captures/{cid}"));
    let total = cap["frames"].as_u64().unwrap();
    assert_eq!(cap["end_reason"], json!("finished"), "{cap}");

    // Frame scrub.
    let (st, all) = get(addr, &format!("/api/captures/{cid}/frames?limit=500"));
    assert_eq!(st, 200, "{all}");
    assert_eq!(all["total_frames"], json!(total));
    let frames = all["frames"].as_array().unwrap().clone();
    assert!(frames.len() >= 20, "{all}");
    let (st, v) = get(
        addr,
        &format!("/api/captures/{cid}/frames?from_frame=3&limit=2"),
    );
    assert_eq!(st, 200, "{v}");
    assert_eq!((&v["from_frame"], &v["limit"]), (&json!(3), &json!(2)));
    assert_eq!(v["frames"].as_array().unwrap().as_slice(), &frames[3..5]);
    assert_eq!(v["next_from_frame"], json!(5));
    assert_eq!(v["capture"]["id"], json!(cid));
    assert_eq!(
        v["stream"]["inspector"]["source"],
        json!({"kind": "capture", "capture_id": cid, "reparse": false})
    );
    assert_eq!(frames[3]["type"], json!("frame"));
    assert_eq!(frames[3]["content"]["hex"].as_str().unwrap().len(), 8);

    // Time scrub: from_t resolves to a frame; to_t ends the page.
    let t = |k: usize| frames[k]["t"].as_i64().unwrap();
    let secs = |ns: i64| format!("{:.6}", ns as f64 / 1e9);
    let (st, v) = get(
        addr,
        &format!(
            "/api/captures/{cid}/frames?from_t={}&limit=1",
            secs(t(5) - 1_000_000)
        ),
    );
    assert_eq!((st, &v["from_frame"]), (200, &json!(5)), "{v}");
    assert_eq!(v["frames"][0], frames[5]);
    let (st, v) = get(
        addr,
        &format!(
            "/api/captures/{cid}/frames?from_frame=5&to_t={}",
            secs(t(7) + 1_000_000)
        ),
    );
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["frames"].as_array().unwrap().as_slice(), &frames[5..8]);
    assert!(v["next_from_frame"].is_null(), "{v}");

    // Re-parse over the whole recording with a draft field map.
    let map = json!({"unit": "bits", "fields": [
        {"name": "hi", "type": "uint", "length": 16},
        {"name": "lo", "type": "uint", "length": 16}
    ]});
    let (st, v) = post(
        addr,
        &format!("/api/captures/{cid}/parse"),
        &json!({"field_map": map, "from_frame": 3, "limit": 2}).to_string(),
    );
    assert_eq!(st, 200, "{v}");
    assert_eq!(
        (&v["total_frames"], &v["fit"]["frames"], &v["fit"]["ok"]),
        (&json!(total), &json!(total), &json!(total)),
        "{v}"
    );
    assert_eq!(
        v["frames"][0]["content"]["hex"],
        frames[3]["content"]["hex"]
    );
    assert_eq!(v["frames"][0]["content"]["layers"]["fit"], json!("ok"));

    // Replay stream from frame 3.
    let mut ws = connect_ws(
        addr,
        &format!("/ws/open/inspector?capture={cid}&from_frame=3&token={TOKEN}"),
    )
    .unwrap();
    let Message::Text(text) = ws.read().unwrap() else {
        panic!("first message must be the header (text)")
    };
    let h: Value = serde_json::from_str(text.as_str()).unwrap();
    assert_eq!(h["stream_id"], json!(format!("capture/{cid}")));
    assert_eq!(
        h["inspector"]["source"],
        json!({"kind": "capture", "capture_id": cid, "reparse": false})
    );
    let Message::Text(text) = ws.read().unwrap() else {
        panic!("frame records are text messages")
    };
    let rec: Value = serde_json::from_str(text.as_str().trim()).unwrap();
    assert_eq!(
        // `seq` is the replay stream's own; `t`, metadata and content are the recording's.
        (
            &rec["type"],
            &rec["t"],
            &rec["metadata"],
            &rec["content"]["hex"]
        ),
        (
            &frames[3]["type"],
            &frames[3]["t"],
            &frames[3]["metadata"],
            &frames[3]["content"]["hex"]
        )
    );
    let _ = ws.close(None);

    // Refusals.
    let (st, v) = get(addr, &format!("/api/captures/{cid}/frames?bogus=1"));
    assert_eq!((st, v["code"].as_str()), (400, Some("invalid")), "{v}");
    let (st, v) = get(
        addr,
        &format!("/api/captures/{cid}/frames?from_frame=1&from_t=2"),
    );
    assert_eq!((st, v["code"].as_str()), (400, Some("invalid")), "{v}");
    let (st, _) = put(addr, &format!("/api/captures/{cid}"), "{}");
    assert_eq!(st, 405);
    let (st, v) = get(addr, "/api/captures/no-such-capture");
    assert_eq!((st, v["code"].as_str()), (404, Some("not_found")), "{v}");

    // Delete a finished capture.
    let (st, v) = delete(addr, &format!("/api/captures/{cid}"));
    assert_eq!((st, &v["deleted"]["id"]), (200, &json!(cid)), "{v}");
    let (st, _) = get(addr, &format!("/api/captures/{cid}"));
    assert_eq!(st, 404);

    stop_server(serving);
}

// --- Route-table / docs consistency ---------------------------------------------------------------

/// T-079: every route in [`hk_api::ROUTES`] must appear (method and path on the same line) in
/// `docs/api.md`, so the reference can never silently fall behind the server.
#[test]
fn every_route_in_the_route_table_is_documented() {
    let doc_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/api.md");
    let doc = std::fs::read_to_string(&doc_path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", doc_path.display()));
    let mut missing = Vec::new();
    for (method, path) in hk_api::ROUTES {
        let documented = doc
            .lines()
            .any(|line| line.contains(path) && line.contains(method));
        if !documented {
            missing.push(format!("{method} {path}"));
        }
    }
    assert!(
        missing.is_empty(),
        "routes missing from docs/api.md: {missing:#?}"
    );
}

/// T-107: `PUT /api/pipelines/{id}/channels` and `POST /api/pipelines/{id}/channels/refresh` on a
/// follow-hops pipeline over the mock device's FM window, as `docs/api.md` documents them: the
/// answer shape, a no-op change, `400 invalid` bodies, the refresh back to the recipe's
/// `list_hz`, and `503 busy` (nothing changed) when the added channels exceed the chain budget.
#[test]
fn follow_hops_channel_routes_match_the_documented_shapes() {
    let (serving, addr) = start_server();
    let bearer = format!("Bearer {TOKEN}");
    let (a, b) = (STATION_HZ - 200e3, STATION_HZ + 200e3);
    let draft = json!({
        "schema": "hackriff.recipe", "schema_version": 2, "id": "t107-channels", "version": 1,
        "name": "T-107 channel routes",
        "input": {"port": "iq", "sample_rate_hz": 24000.0, "bandwidth_hz": 16000.0,
                  "channels": {"mode": "follow-hops", "channel_bandwidth_hz": 12500.0,
                               "max_channels": 64, "list_hz": [a]}},
        "nodes": [
            {"id": "fsk", "block": "fsk_demod"},
            {"id": "clock", "block": "clock_recovery",
             "params": {"symbol_rate_bd": 1200, "pulse": "nrz", "algorithm": "gardner"}},
            {"id": "slice", "block": "slicer", "params": {"threshold": 0.0}},
            {"id": "sync", "block": "sync_search",
             "params": {"mode": "sync-word", "sync_word": "0x7CD215D8", "sync_bits": 32,
                        "max_errors": 2, "frame_bits": 512, "include_sync": false}},
            {"id": "hops", "block": "follow_hops"}
        ],
        "outputs": [{"id": "frames", "kind": "inspector", "from": "hops"}],
        "output_policy": {"content_class": "unrestricted"}
    });
    let start = json!({"recipe": draft,
        "target": {"band": {"f_lo": a - 10e3, "f_hi": a + 10e3}}});
    let (st, p) = post(addr, "/api/pipelines", &start.to_string());
    assert_eq!(st, 201, "{p}");
    let pid = p["id"].as_str().unwrap().to_owned();
    assert_eq!(p["follow_hops"]["channel_source"], json!("list"), "{p}");
    let path = format!("/api/pipelines/{pid}/channels");
    let centers = |v: &Value| -> Vec<f64> {
        v["channels"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["center_hz"].as_f64().unwrap())
            .collect()
    };

    // A channel added: the documented answer.
    let (st, v) = put(addr, &path, &json!({"channels_hz": [a, b]}).to_string());
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["id"], json!(pid));
    assert_eq!(centers(&v), [a, b], "{v}");
    for k in ["index", "center_hz", "bandwidth_hz"] {
        assert!(!v["channels"][1][k].is_null(), "{k}: {v}");
    }
    assert_eq!(v["added"][0]["index"], json!(1), "{v}");
    assert_eq!(v["removed"], json!([]));
    assert!(v["applied_at_sample"].is_u64(), "{v}");
    let (_, p) = get(addr, &format!("/api/pipelines/{pid}"));
    assert_eq!(centers(&p["follow_hops"]), [a, b], "{p}");

    // The same set again: nothing changes.
    let (st, v) = put(addr, &path, &json!({"channels_hz": [b, a]}).to_string());
    assert_eq!(
        (st, &v["added"], &v["applied_at_sample"]),
        (200, &json!([]), &Value::Null),
        "{v}"
    );

    // Invalid bodies.
    for bad in [
        json!({"channels_hz": "101.1e6"}),
        json!({"channels_hz": [a, -1.0]}),
        json!({"channels_hz": [a], "extra": 1}),
        json!({}),
    ] {
        let (st, v) = put(addr, &path, &bad.to_string());
        assert_eq!((st, &v["code"]), (400, &json!("invalid")), "{bad}: {v}");
    }
    let (st, v) = post(addr, &path, "{}");
    assert_eq!(st, 405, "{v}");

    // Refresh re-resolves the recipe's list: back to one channel.
    let (st, v) = post(addr, &format!("{path}/refresh"), "");
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["removed"], json!([1]), "{v}");
    assert_eq!(centers(&v), [a]);

    // 64 channels at 25 kHz: each added one claims a chain; beyond the budget, 503 busy and the
    // running channel set is untouched.
    let many: Vec<f64> = (0..64).map(|k| 100.0e6 + f64::from(k) * 25e3).collect();
    let (st, v) = put(addr, &path, &json!({"channels_hz": many}).to_string());
    assert_eq!((st, &v["code"]), (503, &json!("busy")), "{v}");
    let (_, p) = get(addr, &format!("/api/pipelines/{pid}"));
    assert_eq!(centers(&p["follow_hops"]), [a], "{p}");
    let (st, v) = put(
        addr,
        "/api/pipelines/p999/channels",
        &json!({"channels_hz": [a]}).to_string(),
    );
    assert_eq!(st, 404, "{v}");

    let (st, v) = call(
        addr,
        "DELETE",
        &format!("/api/pipelines/{pid}"),
        Some(&bearer),
        None,
    );
    assert_eq!(st, 200, "{v}");
    stop_server(serving);
}

// Attention + memory (ADR-0012 §11): each M2 task appends its contract tests under its marker.
// T-115 observations

/// T-115 review: a live `hk serve` without `--schedule` logs its tuning as interactive dwell
/// records, and `/api/observations/coverage` reports them: observed seconds at the interactive
/// tier, never activity-independent visits.
#[test]
fn observation_coverage_reports_interactive_tuning_without_a_scheduler() {
    use std::sync::atomic::Ordering::Relaxed;
    let (serving, addr) = start_server();
    let counters = serving.handle.counters();
    let deadline = Instant::now() + Duration::from_secs(60);
    let wait = |f: &dyn Fn() -> bool| {
        while !f() {
            assert!(Instant::now() < deadline, "the mock never streamed");
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    wait(&|| counters.stream_time_ns.load(Relaxed) > 0);
    let first = counters.stream_time_ns.load(Relaxed);
    wait(&|| counters.stream_time_ns.load(Relaxed) >= first + 500_000_000);
    let center = f64::from_bits(counters.tune_center_bits.load(Relaxed));
    // The run ends: its open interactive record closes; the API keeps serving the log.
    let Serving { server, handle, .. } = serving;
    handle.stop();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(handle.wait());
    });
    rx.recv_timeout(Duration::from_secs(30))
        .expect("the run stopped")
        .expect("the run finished cleanly");
    let end = counters.stream_time_ns.load(Relaxed);
    let q = format!(
        "f_lo={}&f_hi={}&t0={}&t1={}",
        center + 100e3,
        center + 112.5e3,
        first as f64 * 1e-9 - 1.0,
        end as f64 * 1e-9 + 1.0
    );
    let (status, v) = get(
        addr,
        &format!("/api/observations/coverage?{q}&channel_hz=12500&tau_s=0.1&min_gap_s=1"),
    );
    assert_eq!(status, 200, "{v}");
    let totals = &v["totals"];
    assert!(totals["n_visits"].as_u64().unwrap() >= 1, "{v}");
    assert_eq!(totals["n_visits_activity_independent"], 0, "{v}");
    assert!(
        totals["observed_s"]["interactive"].as_f64().unwrap() >= 0.4,
        "{v}"
    );
    let (status, v) = get(addr, &format!("/api/observations?{q}&tier=interactive"));
    assert_eq!(status, 200, "{v}");
    assert!(!v["records"].as_array().unwrap().is_empty(), "{v}");
    drop(server);
}

/// T-115: `GET /api/observations` and `/api/observations/coverage` answer the documented shapes
/// (the contract server runs without the scheduler, so the log is empty: unobserved is reported
/// as a gap over the whole span, never as quiet), refuse bad boxes and other methods, and the
/// `observations` stream is offered.
#[test]
fn observation_log_routes_answer_as_documented() {
    let (serving, addr) = start_server();
    let (t0, t1) = (unix_now() - 60.0, unix_now());
    let q = format!("f_lo=100000000&f_hi=101000000&t0={t0}&t1={t1}");

    let (status, v) = get(addr, &format!("/api/observations?{q}&tier=bandit&limit=5"));
    assert_eq!(status, 200, "{v}");
    assert!(is_array(&v["records"]) && is_array(&v["geometries"]), "{v}");
    assert!(v["next_cursor"].is_null());
    assert_eq!(v["truncated"], false);
    assert_eq!(v["f_lo"], 100_000_000.0);
    for key in [
        "offered",
        "dropped",
        "written",
        "flushes",
        "sealed",
        "write_errors",
        "segments_deleted",
        "bytes",
    ] {
        assert!(v["log"][key].is_u64(), "log.{key}: {v}");
    }

    let (status, v) = get(
        addr,
        &format!("/api/observations/coverage?{q}&channel_hz=250000&tau_s=0.01,0.1&min_gap_s=1"),
    );
    assert_eq!(status, 200, "{v}");
    let totals = &v["totals"];
    assert_eq!(totals["n_visits"], 0);
    assert_eq!(totals["n_visits_activity_independent"], 0);
    assert!(is_object(&totals["observed_s"]) && is_object(&totals["freq"]));
    assert!(
        (totals["max_gap_s"].as_f64().unwrap() - 60.0).abs() < 1e-3,
        "{v}"
    );
    assert!(totals.get("mean_revisit_s").is_none());
    assert_eq!(v["channels"].as_array().map(Vec::len), Some(4));
    assert_eq!(v["gaps"].as_array().map(Vec::len), Some(1), "{v}");
    assert_eq!(v["gaps_truncated"], false);
    let poi = v["poi"].as_array().unwrap();
    assert_eq!(poi.len(), 2);
    assert_eq!(poi[0]["tau_s"], 0.01);
    assert_eq!(poi[0]["p_poi"], 0.0);

    for bad in [
        "/api/observations?f_lo=1&t0=0&t1=1".to_string(),
        format!("/api/observations?{q}&tier=loud"),
        format!("/api/observations/coverage?f_lo=2&f_hi=1&t0={t0}&t1={t1}"),
        format!("/api/observations/coverage?{q}&tau_s=x"),
    ] {
        let (status, v) = get(addr, &bad);
        assert_eq!(status, 400, "{bad}: {v}");
        assert_eq!(v["code"], "invalid", "{bad}: {v}");
    }
    let (status, _) = post(addr, &format!("/api/observations?{q}"), "{}");
    assert_eq!(status, 405);

    let (status, streams) = get(addr, "/api/streams");
    assert_eq!(status, 200);
    assert!(
        streams.to_string().contains("\"observations\""),
        "observations stream offered: {streams}"
    );
    stop_server(serving);
}
// T-118 occupancy
// T-119 sites, baselines, candidates, weights
// T-120 scheduler
// T-121 reports
// T-122 anomalies
