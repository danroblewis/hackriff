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
//! T-082: one entry per physical emitter. The FM station (track + RDS decode) and the FSK sensor
//! (track + blind framer) each appear once with both kinds of evidence; two nearby stations stay
//! two entries; re-detection after deleting a merged entry creates exactly one new candidate.
//!
//! Emitters are matched to the private truth after the run (`matches_truth`); no frequency is
//! looked up.

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hk_api::{ApiState, AuditLog, Server, ServerConfig, Token};
use hk_e2e::blind::{matches_truth, matching};
use hk_e2e::{Fixture, SynthRequest, synth_or_skip};
use hk_model::sigmf::Datatype;
use hk_model::{EmitterId, FreqRange, IdentityScheme, LinkTarget, Region, Repository};
use serde_json::{Value, json};

use crate::blind::{
    BlindSource, blind_replay, center_tol_hz, inventory_rows, private_truth, replay_config, start,
};
use crate::common::*;

const T078: &str = "T-078";
const T082: &str = "T-082";

/// The inventory row's emitter id.
fn row_id(row: &Value) -> EmitterId {
    row["id"].as_str().unwrap().parse().unwrap()
}

/// The entry carries track evidence and decoder evidence (a demodulation or decode linked).
fn assert_track_and_decoder_evidence(r: &Repository, row: &Value) {
    let links = r.emitter_links(row_id(row)).unwrap();
    assert!(
        links
            .iter()
            .any(|l| matches!(l.target, LinkTarget::Track(_))),
        "[{T082}] track evidence on {row}"
    );
    assert!(
        links.iter().any(|l| matches!(
            l.target,
            LinkTarget::Demodulation(_) | LinkTarget::Decode(_)
        )),
        "[{T082}] decoder evidence on {row}"
    );
}

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
        assert_eq!(
            matched.len(),
            1,
            "[{T082}] the station appears once in the inventory"
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
        let r = repo(&run.dir.0);
        assert_track_and_decoder_evidence(&r, confirmed);
        assert_eq!(
            r.identity_decode_evidence(row_id(confirmed))
                .unwrap()
                .map(|(scheme, _)| scheme),
            Some(IdentityScheme::RdsPi),
            "[{T082}] the one entry holds the decoded RDS PI"
        );
    }
}

fn read_json(p: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap()
}

fn cf32(bytes: &[u8]) -> impl Iterator<Item = f32> + '_ {
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
}

/// The real ci8 FM recording (scaled x/128, as the pipeline quantises) with a synthetic cf32
/// station of the same capture added, keeping both truths.
pub fn add_station(real: &Path, synth: &hk_e2e::SynthOutput, dir: &Path) -> PathBuf {
    let mut meta = read_json(real);
    let raw = std::fs::read(real.with_extension("sigmf-data")).unwrap();
    let mut iq: Vec<f32> = raw[..raw.len() / 2 * 2]
        .iter()
        .map(|&b| f32::from(b as i8) / 128.0)
        .collect();
    let m = synth.fixture(0).unwrap().meta_path;
    let extra: Vec<Value> = read_json(&m)["annotations"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|x| !x.to_string().contains("\"kind\":\"capture\""))
        .cloned()
        .collect();
    meta["annotations"].as_array_mut().unwrap().extend(extra);
    let d = std::fs::read(m.with_extension("sigmf-data")).unwrap();
    for (x, v) in iq.iter_mut().zip(cf32(&d)) {
        *x += v;
    }
    meta["global"]["core:datatype"] = json!("cf32_le");
    if let Some(g) = meta["global"].as_object_mut() {
        g.remove("core:sha512");
    }
    let mut out = Vec::with_capacity(4 * iq.len());
    for v in &iq {
        out.extend_from_slice(&v.to_le_bytes());
    }
    let path = dir.join("two_stations.sigmf-meta");
    std::fs::write(path.with_extension("sigmf-data"), out).unwrap();
    std::fs::write(&path, serde_json::to_vec_pretty(&meta).unwrap()).unwrap();
    path
}

#[test]
fn t082_two_nearby_fm_stations_stay_two_entries() {
    let Some((real, _)) = private_truth(crate::signal_062::FM_FIXTURE) else {
        return;
    };
    // A second RDS station 300 kHz above the recording's 101.3 MHz station, on air with it.
    let near = synth_or_skip!(
        SynthRequest::new("fm_broadcast_rds")
            .seed(8201)
            .datatype(Datatype::Cf32Le)
            .param("sample_rate", 2.4e6)
            .param("center_hz", 100.8e6)
            .param("offset_hz", 800e3)
            .param("duration_s", 5.0)
            .param("power_dbfs", -16.0)
            .param("noise_dbfs", -120.0)
            .param("pi_hex", "8A2B")
            .param("ps", "T082")
    );
    let work = TempDir::new("t082-scene");
    let meta = add_station(&real, &near, &work.0);
    let fx = Fixture::load(&meta).unwrap();
    let stations = fx.of_kind("wfm-broadcast");
    assert_eq!(stations.len(), 2, "[{T082}] two truth stations");
    let run = blind_replay(&meta, "t082", BlindSource::default());
    let mut ids = BTreeSet::new();
    for station in stations {
        let matched = matching(
            station,
            0.0,
            &run.api_rows,
            |r| {
                (
                    r["f_center_hz"].as_f64().unwrap_or(f64::NAN),
                    r["bandwidth_hz"].as_f64().unwrap_or(0.0),
                )
            },
            center_tol_hz(station),
        );
        eprintln!(
            "[{T082}] station {:.4}..{:.4} MHz: {:?}",
            station.f_lo_hz / 1e6,
            station.f_hi_hz / 1e6,
            matched
                .iter()
                .map(|r| (
                    r["id"].clone(),
                    r["f_center_hz"].clone(),
                    r["state"].clone()
                ))
                .collect::<Vec<_>>()
        );
        assert_eq!(matched.len(), 1, "[{T082}] each station appears once");
        ids.insert(matched[0]["id"].as_str().unwrap().to_owned());
    }
    assert_eq!(ids.len(), 2, "[{T082}] nearby stations stay two entries");
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
    assert_eq!(sensor.len(), 1, "[{T082}] the FSK sensor appears once");
    assert_track_and_decoder_evidence(&repo(&dir.0), sensor[0]);
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
    assert_eq!(
        fresh.len(),
        1,
        "[{T082}] re-detection creates exactly one new candidate"
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
