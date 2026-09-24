//! T-817 (MAP-17, RESEARCH-003): marker collections at the HTTP boundary — named, toggleable
//! collections of time-frequency markers, the `/api/bookmarks` facade over the reserved collection,
//! server-stamped provenance, paging, the audit log and persistence across servers (docs/25 §3,
//! §10; docs/api.md "Marker collections").

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use hk_api::{ApiState, AuditLog, Server, ServerConfig, Token};
use hk_model::{BOOKMARKS_COLLECTION, MarkerWindow, Repository};
use serde_json::{Value, json};

const TOKEN: &str = "t817-collections-token-0123456789abcdef";

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "hk-api-t817-{tag}-{}-{}",
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

fn start(dir: &Path, audit: bool) -> Server {
    let state = ApiState {
        bookmarks: Some(Arc::new(Mutex::new(
            Repository::open(dir.join("hackriff.db")).unwrap(),
        ))),
        audit: audit.then(|| Arc::new(AuditLog::open(&dir.join("audit.jsonl")).unwrap())),
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
}

struct Reply {
    status: u16,
    body: Value,
}

fn call(addr: SocketAddr, method: &str, path: &str, auth: bool, body: Option<&str>) -> Reply {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    let body = body.unwrap_or("");
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    if auth {
        head.push_str(&format!("Authorization: Bearer {TOKEN}\r\n"));
    }
    if !body.is_empty() {
        head.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        ));
    }
    head.push_str("\r\n");
    s.write_all(head.as_bytes()).unwrap();
    s.write_all(body.as_bytes()).unwrap();
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    let split = out
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("response head");
    let status = String::from_utf8_lossy(&out[..split])
        .lines()
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|c| c.parse().ok())
        .unwrap();
    Reply {
        status,
        body: serde_json::from_slice(&out[split + 4..]).unwrap_or(Value::Null),
    }
}

fn authed(addr: SocketAddr, method: &str, path: &str, body: Option<&str>) -> Reply {
    call(addr, method, path, true, body)
}

fn audit_entries(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// A pane on a 902–928 MHz ISM window, one capture-clock second wide.
const VIEW: &str = r#"{"center_hz": 915e6, "span_hz": 20e6, "t_capture": [1726480000.0, 1726480001.0], "tier": "live-iq"}"#;

/// RESEARCH-003: a researcher builds a named collection of time-frequency marks — a one-off
/// burst (a point in time), a chirp (a box) and a band (a frequency-only pin) — hides it as a
/// layer, finds the marks by window, and edits and deletes them; every write is audited and none
/// names a device.
#[test]
fn a_collection_of_time_frequency_markers_round_trips_and_is_audited() {
    let dir = TempDir::new("crud");
    let server = start(&dir.0, true);
    let addr = server.local_addr();

    // A fresh store already holds the reserved bookmarks collection.
    let list = authed(addr, "GET", "/api/collections", None);
    assert_eq!(list.status, 200, "{}", list.body);
    assert_eq!(list.body["matched"], 1, "{}", list.body);
    let reserved = &list.body["collections"][0];
    assert_eq!(reserved["id"], json!(BOOKMARKS_COLLECTION.to_string()));
    assert_eq!(reserved["reserved"], true);

    let c = authed(
        addr,
        "POST",
        "/api/collections",
        Some(r##"{"name": " ISM bursts ", "color": "#33aaff", "note": "902-928"}"##),
    );
    assert_eq!(c.status, 201, "{}", c.body);
    assert_eq!(c.body["name"], "ISM bursts", "trimmed");
    assert_eq!(
        (&c.body["visible"], &c.body["member_count"]),
        (&json!(true), &json!(0))
    );
    let cid = c.body["id"].as_str().unwrap().to_owned();

    let burst = authed(
        addr,
        "POST",
        &format!("/api/collections/{cid}/markers"),
        Some(&format!(
            r#"{{"name": "one-off burst", "f_center_hz": 915.2e6, "bandwidth_hz": 200e3,
                "t_center_s": 1726480000.5, "view": {VIEW}}}"#
        )),
    );
    assert_eq!(burst.status, 201, "{}", burst.body);
    let b = &burst.body;
    assert_eq!(b["collection_id"], json!(cid));
    assert_eq!(
        (&b["f_lo_hz"], &b["f_hi_hz"]),
        (&json!(915.1e6), &json!(915.3e6))
    );
    assert_eq!(b["t_center_s"], json!(1726480000.5));
    assert!(b["duration_s"].is_null(), "a point: {b}");
    assert_eq!(
        (&b["t_start_s"], &b["t_end_s"]),
        (&json!(1726480000.5), &json!(1726480000.5))
    );
    // Provenance: the view the client reported plus what only the server stamps. The capture
    // clock (`t_capture`) and the wall clock (`authored_s`) are kept apart.
    let p = &b["provenance"];
    assert_eq!(p["tier"], "live-iq");
    assert_eq!(p["center_hz"], json!(915e6));
    assert_eq!(p["t_capture"], json!([1726480000.0, 1726480001.0]));
    assert_eq!(p["authored"], true);
    assert!(p["actor"].is_string(), "the token id, never the token: {p}");
    assert_ne!(p["actor"], json!(TOKEN));
    assert!(
        p["device_id"].is_null() && p["sample_rate_hz"].is_null(),
        "{p}"
    );
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    assert!(
        (p["authored_s"].as_f64().unwrap() - now).abs() < 60.0,
        "authored_s is the wall clock: {p}"
    );
    assert!(b.get("device").is_none(), "authoring reaches no radio: {b}");
    let burst_id = b["id"].as_str().unwrap().to_owned();

    let chirp = authed(
        addr,
        "POST",
        &format!("/api/collections/{cid}/markers"),
        Some(&format!(
            r#"{{"name": "chirp", "f_center_hz": 903e6, "t_center_s": 1726480100, "duration_s": 10, "view": {VIEW}}}"#
        )),
    );
    assert_eq!(chirp.status, 201, "{}", chirp.body);
    assert_eq!(
        (&chirp.body["t_start_s"], &chirp.body["t_end_s"]),
        (&json!(1726480095.0), &json!(1726480105.0))
    );
    let band = authed(
        addr,
        "POST",
        &format!("/api/collections/{cid}/markers"),
        Some(&format!(
            r#"{{"name": "LoRa uplink", "f_center_hz": 904.6e6, "bandwidth_hz": 1.6e6, "view": {VIEW}}}"#
        )),
    );
    assert_eq!(band.status, 201, "{}", band.body);
    assert!(band.body["t_start_s"].is_null(), "a frequency-only pin");

    // Window box: the chirp's time finds the chirp and the always-on pin, never the burst.
    let w = authed(
        addr,
        "GET",
        &format!("/api/collections/{cid}/markers?t0=1726480099&t1=1726480101"),
        None,
    );
    assert_eq!(w.status, 200, "{}", w.body);
    let names: Vec<&str> = w.body["markers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["chirp", "LoRa uplink"]);
    // Frequency box over every collection.
    let w = authed(addr, "GET", "/api/markers?f_lo=915e6&f_hi=916e6", None);
    assert_eq!(w.body["matched"], 1, "{}", w.body);
    assert_eq!(w.body["markers"][0]["id"], json!(burst_id));
    // Paging: one per page, cursor to the next.
    let p1 = authed(
        addr,
        "GET",
        &format!("/api/collections/{cid}/markers?limit=1"),
        None,
    );
    assert_eq!(
        (
            &p1.body["count"],
            &p1.body["matched"],
            &p1.body["next_cursor"]
        ),
        (&json!(1), &json!(3), &json!("1")),
        "{}",
        p1.body
    );
    let p3 = authed(
        addr,
        "GET",
        &format!("/api/collections/{cid}/markers?limit=1&cursor=2"),
        None,
    );
    assert!(p3.body["next_cursor"].is_null(), "{}", p3.body);
    for q in ["limit=0", "limit=2001", "cursor=x", "f_lo=1", "t0=5&t1=1"] {
        let r = authed(addr, "GET", &format!("/api/markers?{q}"), None);
        assert_eq!(r.status, 400, "{q}: {}", r.body);
    }

    // Toggle-as-a-layer: the stored `visible` flag.
    let hidden = authed(
        addr,
        "PUT",
        &format!("/api/collections/{cid}"),
        Some(r#"{"visible": false}"#),
    );
    assert_eq!(hidden.status, 200, "{}", hidden.body);
    assert_eq!(
        (&hidden.body["visible"], &hidden.body["member_count"]),
        (&json!(false), &json!(3))
    );

    // Update: clearing the time makes a frequency-only pin (duration goes with it).
    let edited = authed(
        addr,
        "PUT",
        &format!("/api/markers/{}", chirp.body["id"].as_str().unwrap()),
        Some(r#"{"t_center_s": null, "note": "seen twice"}"#),
    );
    assert_eq!(edited.status, 200, "{}", edited.body);
    assert!(edited.body["t_center_s"].is_null() && edited.body["duration_s"].is_null());
    assert_eq!(edited.body["note"], "seen twice");
    assert_eq!(
        edited.body["provenance"], chirp.body["provenance"],
        "provenance kept without a new view"
    );

    let del = authed(addr, "DELETE", &format!("/api/markers/{burst_id}"), None);
    assert_eq!(del.status, 200, "{}", del.body);
    assert_eq!(del.body["deleted"]["id"], json!(burst_id));
    assert_eq!(
        authed(addr, "GET", &format!("/api/markers/{burst_id}"), None).status,
        404
    );
    let gone = authed(addr, "DELETE", &format!("/api/collections/{cid}"), None);
    assert_eq!(gone.status, 200, "{}", gone.body);
    assert_eq!(gone.body["members_deleted"], 2);
    assert_eq!(
        authed(
            addr,
            "GET",
            &format!("/api/collections/{cid}/markers"),
            None
        )
        .status,
        404
    );

    // Audited, with the action names docs/api.md lists, and no `device` key on any of them.
    let entries = audit_entries(&dir.0.join("audit.jsonl"));
    for action in [
        "collection_create",
        "collection_update",
        "collection_delete",
        "marker_create",
        "marker_update",
        "marker_delete",
    ] {
        assert!(
            entries.iter().any(|e| e["action"] == action),
            "{action} not audited"
        );
    }
    assert!(entries.iter().all(|e| e.get("device").is_none()));
    assert!(
        !entries.iter().any(|e| e["action"] == "markers_list"),
        "GETs are never audited"
    );
}

#[test]
fn bodies_are_validated_and_provenance_is_never_accepted_from_the_client() {
    let dir = TempDir::new("invalid");
    let server = start(&dir.0, true);
    let addr = server.local_addr();
    let c = authed(addr, "POST", "/api/collections", Some(r#"{"name": "c"}"#));
    let cid = c.body["id"].as_str().unwrap().to_owned();
    let markers = format!("/api/collections/{cid}/markers");
    let with_view = |extra: &str, view: &str| {
        format!(r#"{{"name": "m", "f_center_hz": 100e6{extra}, "view": {view}}}"#)
    };
    for (body, needle) in [
        (r#"{"name": "m", "f_center_hz": 100e6}"#.to_owned(), "view"),
        (
            with_view(
                "",
                r#"{"center_hz": 1e8, "span_hz": 2e6, "t_capture": 5, "tier": "live-iq", "actor": "me"}"#,
            ),
            "stamped by the server",
        ),
        (
            with_view(
                "",
                r#"{"center_hz": 1e8, "span_hz": 2e6, "t_capture": 5, "tier": "live-iq", "authored_s": 1}"#,
            ),
            "stamped by the server",
        ),
        (
            with_view(
                "",
                r#"{"center_hz": 1e8, "span_hz": 2e6, "t_capture": 5, "tier": "upscaled"}"#,
            ),
            "tier",
        ),
        (
            with_view(
                "",
                r#"{"center_hz": 1e8, "span_hz": 2e6, "tier": "live-iq"}"#,
            ),
            "t_capture",
        ),
        (
            with_view(
                "",
                r#"{"center_hz": 1e8, "span_hz": 2e6, "t_capture": [5, 1], "tier": "live-iq"}"#,
            ),
            "t_capture",
        ),
        (with_view(r#", "provenance": {}"#, VIEW), "provenance"),
        (with_view(r#", "duration_s": 3"#, VIEW), "t_center_s"),
        (
            with_view(r#", "t_center_s": 1, "duration_s": -3"#, VIEW),
            "duration_s",
        ),
        (with_view(r#", "bandwidth_hz": 0"#, VIEW), "bandwidth_hz"),
        (
            with_view(r#", "collection_id": "x""#, VIEW),
            "collection_id",
        ),
    ] {
        let r = authed(addr, "POST", &markers, Some(&body));
        assert_eq!(r.status, 400, "{body}: {}", r.body);
        assert_eq!(r.body["code"], "invalid");
        assert!(
            r.body["error"].as_str().unwrap().contains(needle),
            "{body}: {}",
            r.body
        );
    }
    for body in [
        r#"{"name": ""}"#,
        r#"{"name": "x", "color": "blue"}"#,
        r#"{"name": "x", "visible": "yes"}"#,
        r#"{"name": "x", "reserved": true}"#,
    ] {
        let r = authed(addr, "POST", "/api/collections", Some(body));
        assert_eq!(r.status, 400, "{body}: {}", r.body);
    }
    // A supplied id that exists is a conflict, not a silent overwrite.
    let dup = authed(
        addr,
        "POST",
        "/api/collections",
        Some(&format!(r#"{{"id": "{cid}", "name": "again"}}"#)),
    );
    assert_eq!(
        (dup.status, dup.body["code"].as_str()),
        (409, Some("conflict"))
    );
    // A marker into a collection that does not exist.
    let r = authed(
        addr,
        "POST",
        &format!("/api/collections/{}/markers", hk_model::CollectionId::new()),
        Some(&with_view("", VIEW)),
    );
    assert_eq!(r.status, 404, "{}", r.body);
    // Wrong method: 405 with Allow.
    assert_eq!(authed(addr, "POST", "/api/markers", Some("{}")).status, 405);
    // Unauthenticated: 401 on reads and writes alike.
    assert_eq!(
        call(addr, "GET", "/api/collections", false, None).status,
        401
    );
    assert_eq!(
        call(
            addr,
            "POST",
            "/api/collections",
            false,
            Some(r#"{"name": "x"}"#)
        )
        .status,
        401
    );
}

/// docs/25 §10.6: `/api/bookmarks` is a facade over the reserved collection — the same rows — and
/// that collection is un-deletable and holds no timed marker.
#[test]
fn bookmarks_are_the_reserved_collections_frequency_only_markers() {
    let dir = TempDir::new("facade");
    let server = start(&dir.0, true);
    let addr = server.local_addr();
    let bm = authed(
        addr,
        "POST",
        "/api/bookmarks",
        Some(r#"{"name": "FM 101.3", "kind": "marker", "f_center_hz": 101.3e6}"#),
    );
    assert_eq!(bm.status, 201, "{}", bm.body);
    let id = bm.body["id"].as_str().unwrap().to_owned();
    let reserved = BOOKMARKS_COLLECTION.to_string();

    // The bookmark is the reserved collection's marker, same id.
    let m = authed(addr, "GET", &format!("/api/markers/{id}"), None);
    assert_eq!(m.status, 200, "{}", m.body);
    assert_eq!(m.body["collection_id"], json!(reserved));
    assert!(m.body["t_center_s"].is_null());
    assert!(m.body["provenance"]["actor"].is_string(), "{}", m.body);

    // A marker added through the collection route is a bookmark too.
    let added = authed(
        addr,
        "POST",
        &format!("/api/collections/{reserved}/markers"),
        Some(&format!(
            r#"{{"name": "ADS-B", "f_center_hz": 1090e6, "view": {VIEW}}}"#
        )),
    );
    assert_eq!(added.status, 201, "{}", added.body);
    let list = authed(addr, "GET", "/api/bookmarks", None);
    let names: Vec<&str> = list.body["bookmarks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["FM 101.3", "ADS-B"]);
    assert_eq!(list.body["bookmarks"][1]["kind"], "bookmark");

    // A rename through either route is seen by the other.
    let renamed = authed(
        addr,
        "PUT",
        &format!("/api/markers/{id}"),
        Some(r#"{"name": "FM 101.3 (RDS)"}"#),
    );
    assert_eq!(renamed.status, 200, "{}", renamed.body);
    let b = authed(addr, "GET", &format!("/api/bookmarks/{id}"), None);
    assert_eq!(
        (&b.body["name"], &b.body["kind"]),
        (&json!("FM 101.3 (RDS)"), &json!("marker"))
    );

    // A bookmark has no time; the reserved collection cannot be deleted.
    let timed = authed(
        addr,
        "PUT",
        &format!("/api/markers/{id}"),
        Some(r#"{"t_center_s": 1726480000}"#),
    );
    assert_eq!(timed.status, 400, "{}", timed.body);
    let del = authed(
        addr,
        "DELETE",
        &format!("/api/collections/{reserved}"),
        None,
    );
    assert_eq!(del.status, 400, "{}", del.body);
    assert!(del.body["error"].as_str().unwrap().contains("reserved"));

    // A marker in another collection is not a bookmark.
    let c = authed(addr, "POST", "/api/collections", Some(r#"{"name": "c"}"#));
    let other = authed(
        addr,
        "POST",
        &format!(
            "/api/collections/{}/markers",
            c.body["id"].as_str().unwrap()
        ),
        Some(&format!(
            r#"{{"name": "x", "f_center_hz": 1e8, "view": {VIEW}}}"#
        )),
    );
    let oid = other.body["id"].as_str().unwrap();
    assert_eq!(
        authed(addr, "GET", &format!("/api/bookmarks/{oid}"), None).status,
        404
    );

    // Deleting a bookmark deletes the marker.
    assert_eq!(
        authed(addr, "DELETE", &format!("/api/bookmarks/{id}"), None).status,
        200
    );
    assert_eq!(
        authed(addr, "GET", &format!("/api/markers/{id}"), None).status,
        404
    );
}

#[test]
fn collections_persist_across_servers_and_need_the_audit_log_to_change() {
    let dir = TempDir::new("persist");
    let server = start(&dir.0, true);
    let addr = server.local_addr();
    let c = authed(
        addr,
        "POST",
        "/api/collections",
        Some(r#"{"name": "kept"}"#),
    );
    let cid = c.body["id"].as_str().unwrap().to_owned();
    let m = authed(
        addr,
        "POST",
        &format!("/api/collections/{cid}/markers"),
        Some(&format!(
            r#"{{"name": "burst", "f_center_hz": 433.92e6, "t_center_s": 1726480000, "duration_s": 0.2, "view": {VIEW}}}"#
        )),
    );
    assert_eq!(m.status, 201, "{}", m.body);
    drop(server);

    let repo = Repository::open(dir.0.join("hackriff.db")).unwrap();
    let page = repo
        .markers(Some(cid.parse().unwrap()), MarkerWindow::default(), 0, 10)
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].name, "burst");
    drop(repo);

    // Without an audit log reads still answer and every write is 503 unavailable.
    let server = start(&dir.0, false);
    let addr = server.local_addr();
    let got = authed(addr, "GET", &format!("/api/collections/{cid}"), None);
    assert_eq!(
        (got.status, &got.body["member_count"]),
        (200, &json!(1)),
        "{}",
        got.body
    );
    for (method, path, body) in [
        (
            "POST",
            "/api/collections".to_owned(),
            Some(r#"{"name": "x"}"#),
        ),
        (
            "PUT",
            format!("/api/collections/{cid}"),
            Some(r#"{"visible": false}"#),
        ),
        ("DELETE", format!("/api/collections/{cid}"), None),
    ] {
        let r = authed(addr, method, &path, body);
        assert_eq!(
            (r.status, r.body["code"].as_str()),
            (503, Some("unavailable")),
            "{method} {path}: {}",
            r.body
        );
    }
}
