//! T-078: inventory candidates vs confirmed, through the mock SDR, blind.
//!
//! - The real FM fixture's broadcast station (the shared SIGNAL-062 run) auto-confirms: nothing
//!   about it is configured, the pipeline's confirmation rule decides from what it measured and
//!   decoded.
//! - The synthetic intermittent FSK sensor (`fsk_burst_train`, ~15 % duty) stays a candidate until
//!   a user promotes it over the authenticated, audited API.
//! - A deleted entry leaves the list and stays in history (listed with `state=deleted`, detections
//!   kept); a later re-detection of the same sensor creates a new candidate.
//!
//! Emitters are matched to the private truth after the run (`matches_truth`); no frequency is
//! looked up.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hk_api::{ApiState, AuditLog, Server, ServerConfig, Token};
use hk_e2e::blind::{matches_truth, matching};
use hk_e2e::{Fixture, SynthRequest, synth_or_skip};
use hk_model::{FreqRange, Region};
use serde_json::{Value, json};

use crate::blind::{center_tol_hz, inventory_rows, replay_config, start};
use crate::common::*;

const T078: &str = "T-078";

fn api(dir: &Path) -> Server {
    let state = ApiState {
        inventory: Some(Arc::new(Mutex::new(repo(dir)))),
        audit: Some(Arc::new(AuditLog::open(&dir.join("audit.jsonl")).unwrap())),
        ..ApiState::default()
    };
    Server::start(
        ServerConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            Token::from_config(API_TOKEN).unwrap(),
        ),
        state,
    )
    .unwrap()
}

fn call(addr: SocketAddr, method: &str, path: &str, auth: bool) -> (u16, Value) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let auth = if auth {
        format!("Authorization: Bearer {API_TOKEN}\r\n")
    } else {
        String::new()
    };
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: test\r\n{auth}Connection: close\r\n\r\n"
    )
    .unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&raw[..split]);
    let status = head.split(' ').nth(1).and_then(|c| c.parse().ok()).unwrap();
    (
        status,
        serde_json::from_slice(&raw[split + 4..]).unwrap_or(Value::Null),
    )
}

fn rows(addr: SocketAddr, query: &str) -> Vec<Value> {
    let (status, body) = call(
        addr,
        "GET",
        &format!("/api/inventory?limit=500{query}"),
        true,
    );
    assert_eq!(status, 200, "{body}");
    body["entries"].as_array().cloned().unwrap_or_default()
}

/// Rows matching any `fsk-burst` truth burst of `fx`.
fn at_sensor<'a>(fx: &Fixture, rows: &'a [Value]) -> Vec<&'a Value> {
    let truths = fx.of_kind("fsk-burst");
    rows.iter()
        .filter(|r| {
            let f = r["f_center_hz"].as_f64().unwrap_or(f64::NAN);
            let bw = r["bandwidth_hz"].as_f64().unwrap_or(0.0);
            truths
                .iter()
                .any(|t| matches_truth(t, 0.0, f, bw, center_tol_hz(t)))
        })
        .collect()
}

fn fsk_run(dir: &Path, fx: &Fixture) {
    let (cfg, replay) = replay_config(dir, &fx.meta_path, json!({}), hk_core::Pacing::Unpaced);
    let s = finish(start(cfg, replay));
    assert!(s.errors.is_empty(), "[{T078}] {:?}", s.errors);
}

#[test]
fn t078_steady_fm_station_auto_confirms() {
    let Some(run) = crate::signal_062::fm_run() else {
        return;
    };
    let all = inventory_rows(&run.dir.0);
    let stations = run.fx.of_kind("wfm-broadcast");
    assert!(!stations.is_empty());
    for station in stations {
        let tol = center_tol_hz(station);
        let matched = matching(
            station,
            0.0,
            &all,
            |r| {
                (
                    r["f_center_hz"].as_f64().unwrap_or(f64::NAN),
                    r["bandwidth_hz"].as_f64().unwrap_or(0.0),
                )
            },
            tol,
        );
        eprintln!(
            "[{T078}] station {:.4} MHz: {:?}",
            station.f_lo_hz / 1e6,
            matched
                .iter()
                .map(|r| (
                    r["f_center_hz"].clone(),
                    r["state"].clone(),
                    r["lifecycle"]["reason"].clone(),
                    r["recurrence"]["duty_cycle"].clone()
                ))
                .collect::<Vec<_>>()
        );
        let confirmed = matched
            .iter()
            .find(|r| r["state"] == "confirmed")
            .unwrap_or_else(|| panic!("[{T078}] the steady FM station was not auto-confirmed"));
        assert_eq!(confirmed["lifecycle"]["author"], "auto");
        assert_eq!(
            confirmed["lifecycle"]["actor"],
            hk_pipeline::inventory::CONFIRM_RULE
        );
        assert_eq!(confirmed["lifecycle"]["previous"], "candidate");
        assert!(confirmed["recurrence"]["appearances"].as_u64().unwrap() >= 1);
    }
}

#[test]
fn t078_intermittent_fsk_stays_candidate_until_promoted_then_delete_and_redetect() {
    let request = |seed: u64| {
        SynthRequest::new("fsk_burst_train")
            .seed(seed)
            .param("snr_db", 20.0)
            .param("duration_s", 2.4)
    };
    let first = synth_or_skip!(request(78));
    let fx = first.fixture(0).unwrap();
    let dir = TempDir::new("t078");
    fsk_run(&dir.0, &fx);

    let server = api(&dir.0);
    let addr = server.local_addr();
    let all = rows(addr, "");
    let sensor = at_sensor(&fx, &all);
    eprintln!(
        "[{T078}] sensor rows: {:?}",
        sensor
            .iter()
            .map(|r| (
                r["f_center_hz"].clone(),
                r["state"].clone(),
                r["recurrence"].clone()
            ))
            .collect::<Vec<_>>()
    );
    assert!(
        !sensor.is_empty(),
        "[{T078}] the FSK sensor reached the inventory"
    );
    for r in &sensor {
        assert_eq!(
            r["state"], "candidate",
            "[{T078}] intermittent stays a candidate: {r}"
        );
        assert!(r["lifecycle"].is_null());
    }
    assert!(at_sensor(&fx, &rows(addr, "&state=confirmed")).is_empty());
    // The main sensor entry: the one with the most occurrences; its recurrence shows it is
    // intermittent.
    let main = sensor
        .iter()
        .max_by_key(|r| r["recurrence"]["occurrences"].as_u64().unwrap_or(0))
        .unwrap();
    let id = main["id"].as_str().unwrap().to_owned();
    let rec = &main["recurrence"];
    assert!(rec["occurrences"].as_u64().unwrap() >= 10, "{rec}");
    assert!(rec["duty_cycle"].as_f64().unwrap() < 0.5, "{rec}");
    assert!(!rec["recent"].as_array().unwrap().is_empty());

    // Promote: authentication required, then confirmed by the user.
    let promote = format!("/api/inventory/{id}/promote");
    assert_eq!(call(addr, "POST", &promote, false).0, 401);
    assert_eq!(
        rows(addr, "&state=candidate")
            .iter()
            .filter(|r| r["id"] == id.as_str())
            .count(),
        1
    );
    let (status, body) = call(addr, "POST", &promote, true);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["entry"]["state"], "confirmed");
    assert_eq!(body["entry"]["lifecycle"]["author"], "user");
    let confirmed = rows(addr, "&state=confirmed");
    assert!(confirmed.iter().any(|r| r["id"] == id.as_str()));
    assert!(
        !rows(addr, "&state=candidate")
            .iter()
            .any(|r| r["id"] == id.as_str())
    );

    // Delete: authentication required; the entry leaves the list and stays in history.
    let path = format!("/api/inventory/{id}");
    assert_eq!(call(addr, "DELETE", &path, false).0, 401);
    let (status, body) = call(addr, "DELETE", &path, true);
    assert_eq!(status, 200, "{body}");
    assert!(!rows(addr, "").iter().any(|r| r["id"] == id.as_str()));
    assert!(
        rows(addr, "&state=deleted")
            .iter()
            .any(|r| r["id"] == id.as_str())
    );
    let audit = std::fs::read_to_string(dir.0.join("audit.jsonl")).unwrap();
    let actions: Vec<Value> = audit
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .filter(|e| e["result"] == "ok")
        .map(|e| e["action"].clone())
        .collect();
    assert_eq!(
        actions,
        [json!("inventory_promote"), json!("inventory_delete")]
    );
    drop(server);
    let detections_before = repo(&dir.0)
        .detections_in_region(&Region::new(FreqRange::new(0.0, 7.0e9), ever()))
        .unwrap()
        .len();
    assert!(detections_before > 0, "raw detections are kept");

    // Re-detected later (another capture of the same sensor): a new candidate.
    let second = synth_or_skip!(request(79));
    let fx2 = second.fixture(0).unwrap();
    fsk_run(&dir.0, &fx2);
    let server = api(&dir.0);
    let addr = server.local_addr();
    let after = rows(addr, "");
    let fresh = at_sensor(&fx2, &after);
    eprintln!(
        "[{T078}] after re-detection: {:?}",
        fresh
            .iter()
            .map(|r| (r["id"].clone(), r["state"].clone()))
            .collect::<Vec<_>>()
    );
    assert!(
        !fresh.is_empty(),
        "[{T078}] the re-detected sensor is listed"
    );
    assert!(
        fresh
            .iter()
            .all(|r| r["id"] != id.as_str() && r["state"] == "candidate")
    );
    let (status, kept) = call(addr, "GET", &path, true);
    assert_eq!((status, kept["state"].as_str()), (200, Some("deleted")));
    assert!(
        repo(&dir.0)
            .detections_in_region(&Region::new(FreqRange::new(0.0, 7.0e9), ever()))
            .unwrap()
            .len()
            > detections_before
    );
}
