//! T-052 persisted selections at the HTTP boundary: CRUD of several concurrent selections,
//! persistence across a server restart, validation, links, client-chosen ids (409 on a retried
//! create), bearer auth on mutating calls, and the audit log.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use hk_api::{ApiState, AuditLog, Server, ServerConfig, Token};
use hk_model::{Repository, SelectionId};
use serde_json::{Value, json};

const TOKEN: &str = "t052-selections-token-0123456789abcdef";

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "hk-api-t052-{tag}-{}-{}",
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

fn authed(addr: SocketAddr, method: &str, path: &str, body: Option<&str>) -> Reply {
    call(
        addr,
        method,
        path,
        &[("Authorization", &format!("Bearer {TOKEN}"))],
        body,
    )
}

fn audit_entries(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn several_selections_crud_and_persist_across_a_restart() {
    let dir = TempDir::new("crud");
    let server = start(&dir.0, true);
    let addr = server.local_addr();
    assert_eq!(
        authed(addr, "GET", "/api/selections", None).body,
        json!({ "selections": [] })
    );

    let a = authed(
        addr,
        "POST",
        "/api/selections",
        Some(
            r#"{"name": "  FM 101.3 ", "f_lo": 101.2e6, "f_hi": 101.4e6, "tags": ["fm", " fm", "broadcast"], "notes": "RDS"}"#,
        ),
    );
    assert_eq!(a.status, 201, "{}", a.body);
    assert_eq!(a.body["name"], "FM 101.3", "trimmed");
    assert_eq!(a.body["tags"], json!(["fm", "broadcast"]), "deduplicated");
    assert!(a.body["t_lo"].is_null() && a.body["links"] == json!([]));
    let a_id = a.body["id"].as_str().unwrap().to_owned();

    // A client-chosen id (optimistic/offline create) is kept; retrying it conflicts.
    let chosen = SelectionId::new().to_string();
    let b_body = json!({
        "id": chosen, "name": "pager burst", "f_lo": 930.4e6, "f_hi": 930.6e6,
        "t_lo": 1_789_297_800.25, "t_hi": 1_789_297_802.5,
    })
    .to_string();
    let b = authed(addr, "POST", "/api/selections", Some(&b_body));
    assert_eq!(b.status, 201, "{}", b.body);
    assert_eq!(b.body["id"], json!(chosen));
    assert!((b.body["t_lo"].as_f64().unwrap() - 1_789_297_800.25).abs() < 1e-6);
    let again = authed(addr, "POST", "/api/selections", Some(&b_body));
    assert_eq!(again.status, 409, "{}", again.body);
    assert_eq!(again.body["code"], "conflict");

    let c = authed(
        addr,
        "POST",
        "/api/selections",
        Some(r#"{"name": "ISM", "f_lo": 433.0e6, "f_hi": 434.8e6}"#),
    );
    assert_eq!(c.status, 201);
    let list = authed(addr, "GET", "/api/selections", None);
    let names: Vec<&str> = list.body["selections"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["FM 101.3", "pager burst", "ISM"], "creation order");

    let renamed = authed(
        addr,
        "PUT",
        &format!("/api/selections/{a_id}"),
        Some(r#"{"name": "FM 101.3 (RDS)", "notes": null}"#),
    );
    assert_eq!(renamed.status, 200, "{}", renamed.body);
    assert_eq!(renamed.body["name"], "FM 101.3 (RDS)");
    assert!(renamed.body["notes"].is_null());
    assert_eq!(renamed.body["tags"], json!(["fm", "broadcast"]), "kept");
    assert!(renamed.body["updated"].as_f64() >= renamed.body["created"].as_f64());
    let untimed = authed(
        addr,
        "PUT",
        &format!("/api/selections/{chosen}"),
        Some(r#"{"t_lo": null, "t_hi": null}"#),
    );
    assert!(untimed.body["t_lo"].is_null() && untimed.body["t_hi"].is_null());

    let linked = authed(
        addr,
        "POST",
        &format!("/api/selections/{a_id}/links"),
        Some(r#"{"kind": "recording", "target": "rec-1", "note": "manual IQ"}"#),
    );
    assert_eq!(linked.status, 201, "{}", linked.body);
    assert_eq!(linked.body["links"][0]["kind"], "recording");
    assert_eq!(linked.body["links"][0]["target"], "rec-1");

    let deleted = authed(addr, "DELETE", &format!("/api/selections/{chosen}"), None);
    assert_eq!(deleted.status, 200);
    assert_eq!(deleted.body["deleted"]["name"], "pager burst");
    assert_eq!(
        authed(addr, "GET", &format!("/api/selections/{chosen}"), None).status,
        404
    );
    drop(server);

    // Restart: a new server over the same database lists what survived, links included.
    let server = start(&dir.0, true);
    let addr = server.local_addr();
    let list = authed(addr, "GET", "/api/selections", None);
    let all = list.body["selections"].as_array().unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0]["name"], "FM 101.3 (RDS)");
    assert_eq!(all[0]["links"][0]["note"], "manual IQ");
    assert_eq!(all[1]["name"], "ISM");

    let entries = audit_entries(&dir.0.join("audit.jsonl"));
    for action in [
        "selection_create",
        "selection_update",
        "selection_link",
        "selection_delete",
    ] {
        assert!(
            entries
                .iter()
                .any(|e| e["action"] == action && e["result"] == "ok"),
            "{action} audited"
        );
    }
    assert!(entries.iter().any(|e| e["action"] == "selection_delete"
        && e["old"]["name"] == "pager burst"
        && e["new"].is_null()));
    assert!(
        entries
            .iter()
            .all(|e| e["token_id"].as_str().is_none_or(|t| !t.contains(TOKEN))),
        "never the token"
    );
}

#[test]
fn selections_are_validated_with_clear_errors() {
    let dir = TempDir::new("invalid");
    let server = start(&dir.0, true);
    let addr = server.local_addr();
    for (body, needle) in [
        (r#"{"f_lo": 1e6, "f_hi": 2e6}"#, "name"),
        (r#"{"name": " ", "f_lo": 1e6, "f_hi": 2e6}"#, "name"),
        (r#"{"name": "x", "f_hi": 2e6}"#, "f_lo"),
        (r#"{"name": "x", "f_lo": 2e6, "f_hi": 2e6}"#, "f_lo < f_hi"),
        (r#"{"name": "x", "f_lo": -5, "f_hi": 2e6}"#, "f_lo < f_hi"),
        (r#"{"name": "x", "f_lo": "1", "f_hi": 2e6}"#, "f_lo"),
        (
            r#"{"name": "x", "f_lo": 1e6, "f_hi": 2e6, "t_lo": 5}"#,
            "t_hi",
        ),
        (
            r#"{"name": "x", "f_lo": 1e6, "f_hi": 2e6, "t_lo": 5, "t_hi": 4}"#,
            "t_lo <= t_hi",
        ),
        (
            r#"{"name": "x", "f_lo": 1e6, "f_hi": 2e6, "t_lo": 1e30, "t_hi": 1e30}"#,
            "Unix time",
        ),
        (
            r#"{"name": "x", "f_lo": 1e6, "f_hi": 2e6, "tags": "a"}"#,
            "tags",
        ),
        (
            r#"{"name": "x", "f_lo": 1e6, "f_hi": 2e6, "id": "not-a-uuid"}"#,
            "UUID",
        ),
        (
            r#"{"name": "x", "f_lo": 1e6, "f_hi": 2e6, "content": "iq"}"#,
            "content",
        ),
    ] {
        let rep = authed(addr, "POST", "/api/selections", Some(body));
        assert_eq!(rep.status, 400, "{body}: {}", rep.body);
        assert_eq!(rep.body["code"], "invalid");
        assert!(
            rep.body["error"].as_str().unwrap().contains(needle),
            "{body}: {}",
            rep.body
        );
    }
    let long = json!({ "name": "x".repeat(121), "f_lo": 1.0, "f_hi": 2.0 }).to_string();
    assert_eq!(
        authed(addr, "POST", "/api/selections", Some(&long)).status,
        400
    );
    assert!(
        authed(addr, "GET", "/api/selections", None).body["selections"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    let made = authed(
        addr,
        "POST",
        "/api/selections",
        Some(r#"{"name": "ok", "f_lo": 1e6, "f_hi": 2e6}"#),
    );
    let id = made.body["id"].as_str().unwrap();
    let path = format!("/api/selections/{id}");
    for body in [
        r#"{"f_lo": 3e6}"#,
        r#"{"id": "x"}"#,
        r#"{"name": ""}"#,
        r#"{"t_hi": 4}"#,
    ] {
        assert_eq!(authed(addr, "PUT", &path, Some(body)).status, 400, "{body}");
    }
    assert_eq!(authed(addr, "GET", &path, None).body["f_lo"], json!(1e6));
    for body in [
        r#"{"kind": "transmit", "target": "x"}"#,
        r#"{"kind": "recording", "target": " "}"#,
        r#"{"kind": "recording"}"#,
    ] {
        let rep = authed(addr, "POST", &format!("{path}/links"), Some(body));
        assert_eq!(rep.status, 400, "{body}: {}", rep.body);
    }
    assert_eq!(
        authed(
            addr,
            "PUT",
            &format!("/api/selections/{}", SelectionId::new()),
            Some(r#"{"name": "x"}"#)
        )
        .status,
        404
    );
    assert_eq!(
        authed(addr, "GET", "/api/selections/not-an-id", None).status,
        404
    );
    let wrong = authed(addr, "PATCH", "/api/selections", None);
    assert_eq!(wrong.status, 405);
    let wrong = authed(addr, "POST", &path, Some("{}"));
    assert_eq!(wrong.status, 405);
    assert_eq!(wrong.body["code"], "method_not_allowed");
}

#[test]
fn mutating_selection_calls_need_the_bearer_header_and_an_audit_log() {
    let dir = TempDir::new("auth");
    let server = start(&dir.0, true);
    let addr = server.local_addr();
    let body = Some(r#"{"name": "x", "f_lo": 1e6, "f_hi": 2e6}"#);
    assert_eq!(call(addr, "POST", "/api/selections", &[], body).status, 401);
    assert_eq!(
        call(
            addr,
            "POST",
            "/api/selections",
            &[("Authorization", "Bearer wrong-token-0123456789")],
            body
        )
        .status,
        401
    );
    assert_eq!(
        call(
            addr,
            "POST",
            &format!("/api/selections?token={TOKEN}"),
            &[],
            body
        )
        .status,
        401,
        "the query token is for reads only"
    );
    assert_eq!(
        call(addr, "GET", "/api/selections", &[], None).status,
        401,
        "reads need a token too"
    );
    assert_eq!(
        call(
            addr,
            "GET",
            &format!("/api/selections?token={TOKEN}"),
            &[],
            None
        )
        .status,
        200
    );
    assert_eq!(
        call(
            addr,
            "POST",
            "/api/selections",
            &[
                ("Authorization", &format!("Bearer {TOKEN}")),
                ("Origin", "https://evil.example")
            ],
            body
        )
        .status,
        403,
        "cross-origin"
    );
    assert!(
        Repository::open(dir.0.join("hackriff.db"))
            .unwrap()
            .selections()
            .unwrap()
            .is_empty(),
        "nothing was written"
    );
    drop(server);
    let refused = audit_entries(&dir.0.join("audit.jsonl"));
    assert!(
        refused
            .iter()
            .any(|e| e["path"] == "/api/selections" && e["result"] == "refused"),
        "{refused:?}"
    );

    // Without an audit log, mutating selection calls are disabled; reads still work.
    let dir = TempDir::new("noaudit");
    let server = start(&dir.0, false);
    let addr = server.local_addr();
    let rep = authed(addr, "POST", "/api/selections", body);
    assert_eq!(rep.status, 503, "{}", rep.body);
    assert_eq!(authed(addr, "GET", "/api/selections", None).status, 200);
}
