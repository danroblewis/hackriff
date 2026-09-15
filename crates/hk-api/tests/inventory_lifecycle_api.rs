//! T-078 inventory lifecycle at the HTTP boundary: the `state` filter and row fields, promote and
//! delete (bearer auth, audit, `{error, code}` errors), deleted entries leaving the list while
//! their history stays.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use hk_api::{ApiState, AuditLog, Server, ServerConfig, Token};
use hk_model::{
    EmitterId, Fingerprint, Repository, Sighting, TimeRange, Timestamp, TimingFeatures, Track,
    TrackId, TrackState,
};
use serde_json::Value;

const TOKEN: &str = "t078-inventory-lifecycle-token-0123456789";

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "hk-api-t078-{tag}-{}-{}",
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

fn t(sec: f64) -> Timestamp {
    Timestamp::from_unix_nanos(1_789_000_000_000_000_000 + (sec * 1e9) as i64)
}

/// Two emitters: an intermittent sensor and a steady carrier, both candidates.
fn seed(repo: &mut Repository) -> (EmitterId, EmitterId) {
    let mut emitter = |f: f64, bw: f64, duty: f64, count: u64| {
        let track = Track {
            id: TrackId::new(),
            state: TrackState::Closed,
            split_from: None,
            time: TimeRange::new(t(0.0), t(4.0)),
            f_center_hz: f,
            bandwidth_hz: bw,
            detection_count: count,
            timing: TimingFeatures {
                duty_cycle: Some(duty),
                ..TimingFeatures::default()
            },
            updated_at: t(4.0),
        };
        repo.upsert_track(&track).unwrap();
        let mut s = Sighting::track(&track, Fingerprint::new(f, bw));
        s.count = count;
        repo.record_sighting(&s, None).unwrap().emitter_id
    };
    (
        emitter(433.92e6, 36e3, 0.15, 30),
        emitter(162.4e6, 12e3, 1.0, 400),
    )
}

fn start(dir: &Path, audit: bool) -> (Server, EmitterId, EmitterId) {
    let mut repo = Repository::open(dir.join("hackriff.db")).unwrap();
    let (sensor, carrier) = seed(&mut repo);
    let state = ApiState {
        inventory: Some(Arc::new(Mutex::new(repo))),
        audit: audit.then(|| Arc::new(AuditLog::open(&dir.join("audit.jsonl")).unwrap())),
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
    (server, sensor, carrier)
}

struct Reply {
    status: u16,
    body: Value,
}

fn call(addr: SocketAddr, method: &str, path: &str, auth: bool, body: Option<&str>) -> Reply {
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    if auth {
        head.push_str(&format!("Authorization: Bearer {TOKEN}\r\n"));
    }
    let body = body.unwrap_or("");
    if !body.is_empty() {
        head.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        ));
    }
    head.push_str("\r\n");
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    s.write_all(head.as_bytes()).unwrap();
    s.write_all(body.as_bytes()).unwrap();
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    let split = out.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
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

fn ids(addr: SocketAddr, query: &str) -> Vec<String> {
    let r = call(addr, "GET", &format!("/api/inventory{query}"), true, None);
    assert_eq!(r.status, 200, "{query}: {}", r.body);
    r.body["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_str().unwrap().to_owned())
        .collect()
}

fn audit_entries(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn t078_rows_carry_state_lifecycle_and_recurrence_and_filter_by_state() {
    let dir = TempDir::new("rows");
    let (server, sensor, carrier) = start(&dir.0, true);
    let addr = server.local_addr();
    let r = call(addr, "GET", "/api/inventory", true, None);
    assert_eq!(r.status, 200);
    let rows = r.body["entries"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    for row in rows {
        assert_eq!(row["state"], "candidate");
        assert!(row["lifecycle"].is_null());
        let rec = &row["recurrence"];
        assert_eq!(rec["appearances"], 1);
        assert!(rec["span_s"].as_f64().unwrap() > 3.9);
        assert_eq!(rec["recent"].as_array().unwrap().len(), 1);
    }
    let sensor_row = rows.iter().find(|r| r["id"] == sensor.to_string()).unwrap();
    assert_eq!(sensor_row["recurrence"]["occurrences"], 30);
    assert!((sensor_row["recurrence"]["duty_cycle"].as_f64().unwrap() - 0.15).abs() < 1e-9);
    let mut both = vec![sensor.to_string(), carrier.to_string()];
    both.sort();
    let mut got = ids(addr, "?state=candidate");
    got.sort();
    assert_eq!(got, both);
    assert!(ids(addr, "?state=confirmed").is_empty());
    assert!(ids(addr, "?state=deleted").is_empty());
    let bad = call(addr, "GET", "/api/inventory?state=gone", true, None);
    assert_eq!(bad.status, 400);
    assert!(bad.body["error"].as_str().unwrap().contains("state"));
    let one = call(
        addr,
        "GET",
        &format!("/api/inventory/{carrier}"),
        true,
        None,
    );
    assert_eq!(one.status, 200);
    assert_eq!(one.body["id"], carrier.to_string());
}

#[test]
fn t078_promote_and_delete_are_authenticated_audited_and_validated() {
    let dir = TempDir::new("mutate");
    let (server, sensor, carrier) = start(&dir.0, true);
    let addr = server.local_addr();
    let promote = format!("/api/inventory/{sensor}/promote");

    // Auth: no header, and a query token is not enough for a mutating call.
    assert_eq!(call(addr, "POST", &promote, false, None).status, 401);
    assert_eq!(
        call(
            addr,
            "POST",
            &format!("{promote}?token={TOKEN}"),
            false,
            None
        )
        .status,
        401
    );
    assert_eq!(
        call(
            addr,
            "DELETE",
            &format!("/api/inventory/{carrier}"),
            false,
            None
        )
        .status,
        401
    );
    assert_eq!(
        ids(addr, "?state=candidate").len(),
        2,
        "refused calls change nothing"
    );

    // Validation and errors.
    let unknown = call(addr, "POST", &promote, true, Some(r#"{"bogus": 1}"#));
    assert_eq!(
        (unknown.status, unknown.body["code"].as_str()),
        (400, Some("invalid"))
    );
    let missing = call(
        addr,
        "POST",
        &format!("/api/inventory/{}/promote", EmitterId::new()),
        true,
        None,
    );
    assert_eq!(
        (missing.status, missing.body["code"].as_str()),
        (404, Some("not_found"))
    );
    let bad_id = call(addr, "DELETE", "/api/inventory/not-a-uuid", true, None);
    assert_eq!(bad_id.status, 404);
    let method = call(addr, "PUT", &format!("/api/inventory/{sensor}"), true, None);
    assert_eq!(method.status, 405);

    // Promote: candidate → confirmed, author user, actor the token fingerprint.
    let p = call(
        addr,
        "POST",
        &promote,
        true,
        Some(r#"{"reason": "my weather sensor"}"#),
    );
    assert_eq!(p.status, 200, "{}", p.body);
    assert_eq!(p.body["changed"], true);
    let entry = &p.body["entry"];
    assert_eq!(entry["state"], "confirmed");
    assert_eq!(entry["lifecycle"]["author"], "user");
    assert_eq!(entry["lifecycle"]["previous"], "candidate");
    assert_eq!(entry["lifecycle"]["reason"], "my weather sensor");
    assert!(
        entry["lifecycle"]["actor"]
            .as_str()
            .unwrap()
            .starts_with("tok-")
    );
    assert_eq!(ids(addr, "?state=confirmed"), vec![sensor.to_string()]);
    assert_eq!(ids(addr, "?state=candidate"), vec![carrier.to_string()]);
    let again = call(addr, "POST", &promote, true, None);
    assert_eq!(
        (again.status, again.body["changed"].clone()),
        (200, Value::Bool(false))
    );

    // Delete: leaves the default list, stays listed as deleted, cannot be acted on again.
    let d = call(
        addr,
        "DELETE",
        &format!("/api/inventory/{sensor}"),
        true,
        None,
    );
    assert_eq!(d.status, 200, "{}", d.body);
    assert_eq!(d.body["deleted"]["state"], "deleted");
    assert_eq!(d.body["deleted"]["lifecycle"]["previous"], "confirmed");
    assert_eq!(ids(addr, ""), vec![carrier.to_string()]);
    assert_eq!(ids(addr, "?state=deleted"), vec![sensor.to_string()]);
    assert_eq!(call(addr, "POST", &promote, true, None).status, 404);
    assert_eq!(
        call(
            addr,
            "DELETE",
            &format!("/api/inventory/{sensor}"),
            true,
            None
        )
        .status,
        404
    );
    let kept = call(addr, "GET", &format!("/api/inventory/{sensor}"), true, None);
    assert_eq!(
        (kept.status, kept.body["state"].as_str()),
        (200, Some("deleted"))
    );

    // Audit: one entry per mutating call that passed auth, with old and new state.
    let entries = audit_entries(&dir.0.join("audit.jsonl"));
    let ok: Vec<&Value> = entries.iter().filter(|e| e["result"] == "ok").collect();
    let actions: Vec<&str> = ok.iter().map(|e| e["action"].as_str().unwrap()).collect();
    assert_eq!(
        actions,
        ["inventory_promote", "inventory_promote", "inventory_delete"],
        "{entries:#?}"
    );
    assert_eq!(ok[0]["old"]["state"], "candidate");
    assert_eq!(ok[0]["new"]["state"], "confirmed");
    assert_eq!(ok[0]["request"]["reason"], "my weather sensor");
    assert!(ok[0]["token_id"].as_str().unwrap().starts_with("tok-"));
    assert_eq!(ok[2]["old"]["state"], "confirmed");
    assert_eq!(ok[2]["new"]["state"], "deleted");
    assert!(entries.iter().any(|e| e["result"] == "error"
        && e["action"] == "inventory_promote"
        && e["status"] == 404));
}

/// T-191: set/clear the user band over HTTP, the measured band kept, audited as `inventory_band`,
/// persisted across a restart, refused on deleted entries.
#[test]
fn t191_user_band_is_set_cleared_audited_and_survives_restart() {
    let dir = TempDir::new("band");
    let (server, sensor, _) = start(&dir.0, true);
    let addr = server.local_addr();
    let band = format!("/api/inventory/{sensor}/band");
    // Measured: 433.902e6 ..= 433.938e6.
    let set = call(
        addr,
        "PUT",
        &band,
        true,
        Some(r#"{"f_lo": 433.9e6, "f_hi": 433.94e6, "reason": "wider"}"#),
    );
    assert_eq!(set.status, 200, "{}", set.body);
    let row = &set.body["entry"];
    assert_eq!(row["user_band"]["f_lo"], 433.9e6);
    assert_eq!(row["user_band"]["reason"], "wider");
    assert!(
        row["user_band"]["actor"]
            .as_str()
            .unwrap()
            .starts_with("tok-")
    );
    assert_eq!(
        (row["f_lo_hz"].as_f64(), row["bandwidth_hz"].as_f64()),
        (Some(433.902e6), Some(36e3)),
        "measured band unchanged"
    );
    let bad = call(
        addr,
        "PUT",
        &band,
        true,
        Some(r#"{"f_lo": 1e6, "f_hi": 2e6}"#),
    );
    assert_eq!(
        (bad.status, bad.body["code"].as_str()),
        (400, Some("invalid"))
    );
    let wrong = call(addr, "POST", &band, true, Some("{}"));
    assert_eq!(wrong.status, 405);
    let replaced = call(
        addr,
        "PUT",
        &band,
        true,
        Some(r#"{"f_lo": 433.91e6, "f_hi": 433.93e6}"#),
    );
    assert_eq!(replaced.status, 200, "{}", replaced.body);
    assert_eq!(replaced.body["user_band"]["reason"], Value::Null);
    drop(server);

    // Restart over the same database: still there.
    let repo = Repository::open(dir.0.join("hackriff.db")).unwrap();
    let state = ApiState {
        inventory: Some(Arc::new(Mutex::new(repo))),
        audit: Some(Arc::new(
            AuditLog::open(&dir.0.join("audit.jsonl")).unwrap(),
        )),
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
    let got = call(addr, "GET", &format!("/api/inventory/{sensor}"), true, None);
    assert_eq!(got.body["user_band"]["f_hi"], 433.93e6, "{}", got.body);
    let cleared = call(addr, "DELETE", &band, true, None);
    assert_eq!(cleared.body["cleared"], true, "{}", cleared.body);
    assert_eq!(cleared.body["entry"]["user_band"], Value::Null);
    let del = call(
        addr,
        "DELETE",
        &format!("/api/inventory/{sensor}"),
        true,
        None,
    );
    assert_eq!(del.status, 200);
    let gone = call(
        addr,
        "PUT",
        &band,
        true,
        Some(r#"{"f_lo": 433.91e6, "f_hi": 433.93e6}"#),
    );
    assert_eq!(gone.status, 404);

    let entries = audit_entries(&dir.0.join("audit.jsonl"));
    let ok: Vec<&Value> = entries
        .iter()
        .filter(|e| e["result"] == "ok" && e["action"] == "inventory_band")
        .collect();
    assert_eq!(ok.len(), 3, "{entries:#?}");
    assert_eq!(ok[0]["old"]["user_band"], Value::Null);
    assert_eq!(ok[0]["new"]["user_band"]["f_lo"], 433.9e6);
    assert_eq!(ok[1]["old"]["user_band"]["f_lo"], 433.9e6);
    assert_eq!(ok[1]["new"]["user_band"]["f_lo"], 433.91e6);
    assert_eq!(ok[2]["old"]["user_band"]["f_lo"], 433.91e6);
    assert_eq!(ok[2]["new"]["user_band"], Value::Null);
    assert!(ok[0]["token_id"].as_str().unwrap().starts_with("tok-"));
    assert!(
        entries
            .iter()
            .any(|e| e["action"] == "inventory_band" && e["status"] == 400)
    );
}

#[test]
fn t078_mutating_inventory_calls_need_an_audit_log() {
    let dir = TempDir::new("noaudit");
    let (server, sensor, _) = start(&dir.0, false);
    let addr = server.local_addr();
    let r = call(
        addr,
        "POST",
        &format!("/api/inventory/{sensor}/promote"),
        true,
        None,
    );
    assert_eq!(
        (r.status, r.body["code"].as_str()),
        (503, Some("unavailable"))
    );
    assert!(ids(addr, "?state=confirmed").is_empty());
}
