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

// T-089 inspector

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
