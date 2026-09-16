//! T-079: HTTP/WS API contract tests. `docs/api.md` is the reference; this file exercises it
//! against a real `hk serve` running the mock SDR device over the `fm_100p8M` fixture (T-049), the
//! same composition `hk serve --device mock:<fixture>` uses, so a rewritten UI can rely on the
//! contract without reading the server's source: every documented route's status, JSON shape
//! (required fields and types) and auth refusals. Deterministic within bounded timeouts (the mock
//! runs the fixture in real time and loops it, T-049/T-057, so detection takes real wall-clock
//! seconds, not instant).
//!
//! Coverage: `/api/streams`, `/api/history`, `/api/floor`, `/api/inventory` (including the T-078
//! `state`/`lifecycle`/`recurrence` and T-163 `estimated_params` fields),
//! `/api/inventory/{id}[/promote\|/decode]` (T-078, T-159, T-163),
//! `/api/analysis/strongest` (T-079), `/api/status`, `/api/control/*`, `/api/bookmarks[/<id>]`,
//! `/api/selections[/<id>[/links]]`, `/api/outputs[...]`, `/ws/<id>` (spectrum header),
//! `/ws/open/listen` (audio header + PCM data records on the 101.3 MHz station), and auth/CORS
//! refusals. `docs/stream-contract.md` covers stream framing in full; this file only checks the
//! shapes `docs/api.md` promises.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hk_cli::pipeline::{LiveArgs, TempDataDirGuard, temp_data_dir};
use hk_cli::serve::{ServeOptions, ServeSource, Serving, start};
use hk_model::{EmitterId, SelectionId};
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
/// The guard comes **first** on purpose (T-236). Bindings of one `let` pattern drop in reverse
/// order, so a guard bound last would be dropped *first*: it would remove the data directory while
/// the server and pipeline were still alive, and their teardown (the observation log sealing its
/// open hour) would recreate `observations/<date>/` underneath it — a removal that succeeds and
/// still leaves an orphan, which is what T-232 measured as its one residual per run and why no
/// amount of retrying in the guard could fix it. First in the tuple means dropped last.
fn start_server() -> (TempDataDirGuard, Serving, SocketAddr) {
    let dir = temp_data_dir();
    let guard = TempDataDirGuard::new(dir.clone());
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
        // T-178: the ring is allocated up front, so tests keep it small.
        iq_buffer: hk_cli::pipeline::IqBufferArgs {
            retention_s: None,
            max_bytes: Some(64 << 20),
        },
        iq_buffer_hooks: None,
    })
    .unwrap();
    let addr = serving.server.local_addr();
    (guard, serving, addr)
}

/// Stops the run and waits for it to finish, bounded (the mock loops forever until told to stop).
fn stop_server(serving: Serving) {
    serving.handle.stop();
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = serving.handle;
    let waiter = std::thread::spawn(move || {
        let _ = tx.send(handle.wait());
        // `handle` drops *after* the send: the pipeline's stores tear down on this thread (the
        // observation log seals its open hour, recreating its dated directory), so the run's data
        // directory is not free until this thread ends. T-236: join it.
    });
    let _ = rx.recv_timeout(Duration::from_secs(30));
    let _ = waiter.join();
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

/// T-218 (ADR-0016 §1–§2): `GET /api/taxonomy` serves the modulation taxonomy and `thresholds@1`
/// as `docs/api.md` documents them, so the thin client never keeps its own family tree or gates.
/// It is reference data: no emitter, detection or identity is reachable through it.
#[test]
fn taxonomy_route_answers_as_documented() {
    let (_dir_guard, serving, addr) = start_server();

    let (st, v) = get(addr, "/api/taxonomy");
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["current"], "hk-mod@1");
    assert_eq!(v["unknown"], "unknown");
    assert!(is_array(&v["taxonomies"]), "{v}");

    let tax = &v["taxonomies"][0];
    assert_eq!(tax["ref"], "hk-mod@1");
    assert_eq!(tax["name"], "hk-mod");
    assert_eq!(tax["version"], 1);
    let families: Vec<&str> = tax["families"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["family"].as_str().unwrap())
        .collect();
    for want in [
        "analog",
        "fsk",
        "psk-qam",
        "ook-ask",
        "pulsed",
        "noise-like",
    ] {
        assert!(families.contains(&want), "families: {families:?}");
    }
    let fsk = tax["families"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["family"] == "fsk")
        .unwrap();
    assert_eq!(fsk["coarse"], "digital");
    let classes: Vec<&str> = fsk["classes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap())
        .collect();
    assert!(classes.contains(&"2fsk"), "{classes:?}");
    assert!(is_array(&tax["legacy"]), "{tax}");

    // thresholds@1: the gates, the confidence cap and the prior's minimum uniform weight.
    let th = &v["thresholds"];
    assert_eq!(th["version"], "thresholds@1");
    assert_eq!(th["max_confidence"], 0.999);
    assert_eq!(th["lambda0_min"], 0.1);
    let rows = th["families"].as_array().unwrap();
    assert_eq!(rows.len(), families.len(), "one threshold row per family");
    let fsk_gate = rows.iter().find(|t| t["family"] == "fsk").unwrap();
    assert_eq!(fsk_gate["snr_gate_db"], 20.0, "the S5 floor, unchanged");
    assert!(fsk_gate["min_confidence"].is_number());
    let noise = rows.iter().find(|t| t["family"] == "noise-like").unwrap();
    assert!(noise["snr_gate_db"].is_null(), "noise-like has no SNR gate");

    // Reference data only: nothing measured, and no service label, is reachable here.
    let text = v.to_string();
    for forbidden in ["emitter", "identity", "f_center_hz", "adsb", "fm-broadcast"] {
        assert!(
            !text.contains(forbidden),
            "{forbidden} leaked into taxonomy"
        );
    }

    // GET only.
    let (st, _) = put(addr, "/api/taxonomy", "{}");
    assert_eq!(st, 405);
    let (st, _) = post(addr, "/api/taxonomy", "{}");
    assert_eq!(st, 405);

    stop_server(serving);
}

/// T-201 (ADR-0016 §5): `GET /api/signatures/match?emitter=<id>` answers as `docs/api.md`
/// documents it. The match is ranked evidence about an emitter, never an identity, so the shape
/// carries the arithmetic (per-field `z`, what is missing, what conflicts) and nothing that could
/// name the emitter.
#[test]
fn signature_match_route_answers_as_documented() {
    let (_dir_guard, serving, addr) = start_server();

    // `emitter` is required, and an unparsable or unknown id is a 404 — never a 200 that could be
    // probed for which ids exist.
    let (st, v) = get(addr, "/api/signatures/match");
    assert_eq!(st, 400, "{v}");
    assert_eq!(v["code"], "invalid", "{v}");
    let (st, v) = get(addr, "/api/signatures/match?emitter=not-an-id");
    assert_eq!(st, 404, "{v}");
    assert_eq!(v["code"], "not_found", "{v}");
    let unknown = EmitterId::new();
    let (st, v) = get(addr, &format!("/api/signatures/match?emitter={unknown}"));
    assert_eq!(st, 404, "{v}");

    // A real emitter from the fixture: the documented shape, whether or not the catalogue has
    // anything to say about it (an empty catalogue is the normal case on a fresh server).
    let (st, inv) = get(addr, "/api/inventory");
    assert_eq!(st, 200, "{inv}");
    if let Some(row) = inv["emitters"].as_array().and_then(|r| r.first()) {
        let id = row["id"].as_str().expect("an inventory row has an id");
        let (st, v) = get(addr, &format!("/api/signatures/match?emitter={id}"));
        assert_eq!(st, 200, "{v}");
        assert_eq!(v["emitter"], id, "{v}");
        assert!(is_array(&v["history"]), "{v}");
        assert!(
            v["match"].is_null() || v["match"].is_object(),
            "match is a SignatureMatch or null: {v}"
        );
        if let Some(m) = v["match"].as_object() {
            let outcome = m["outcome"].as_str().unwrap_or_default();
            assert!(
                ["full", "partial", "none"].contains(&outcome),
                "outcome: {outcome}"
            );
            assert!(is_array(&m["candidates"]), "{v}");
            assert!(is_array(&m["reasons"]), "{v}");
            // `none` carries no candidates; anything else carries at least one.
            assert_eq!(
                m["candidates"].as_array().unwrap().is_empty(),
                outcome == "none",
                "{v}"
            );
            for c in m["candidates"].as_array().unwrap() {
                assert!(c["signature"]["id"].is_string(), "{c}");
                assert!(c["score"].is_number(), "{c}");
                assert!(is_array(&c["agreement"]), "{c}");
                assert!(is_array(&c["missing"]), "{c}");
                assert!(is_array(&c["conflicting"]), "{c}");
            }
            // A match never carries an identity or a status: it explains, it does not name.
            for forbidden in ["identity", "known_status", "lifecycle"] {
                assert!(m.get(forbidden).is_none(), "{forbidden} leaked: {v}");
            }
        }
    }

    // GET only.
    let (st, _) = post(addr, "/api/signatures/match", "{}");
    assert_eq!(st, 405);
    let (st, _) = put(addr, "/api/signatures/match", "{}");
    assert_eq!(st, 405);

    stop_server(serving);
}

/// T-202 (ADR-0016 §5): `/api/clusters` answers as `docs/api.md` documents it. A cluster is a
/// *type* above emitters and is evidence, never identity, so the shape carries what the group
/// measured like (with its uncertainty) and nothing that could name it.
#[test]
fn cluster_routes_answer_as_documented() {
    let (_dir_guard, serving, addr) = start_server();
    const UNKNOWN: &str = "cluster:01999999-0000-7000-8000-000000000000";

    // The list is always answerable, and empty on a fresh server (clusters are built from
    // measurement, never seeded).
    let (st, v) = get(addr, "/api/clusters");
    assert_eq!(st, 200, "{v}");
    assert!(is_array(&v["clusters"]), "{v}");
    for c in v["clusters"].as_array().unwrap() {
        for field in [
            "id",
            "state",
            "members",
            "member_ids",
            "created_at_s",
            "updated_at_s",
            "observations",
            "suspect_fraction",
            "centroid",
            "events",
        ] {
            assert!(c.get(field).is_some(), "cluster missing {field}: {c}");
        }
        // Only visible clusters are served: a pending group is still a guess.
        let state = c["state"].as_str().unwrap_or_default();
        assert!(["active", "promoted"].contains(&state), "state: {state}");
        assert!(is_array(&c["member_ids"]), "{c}");
        assert!(is_array(&c["centroid"]), "{c}");
        for f in c["centroid"].as_array().unwrap() {
            assert!(f["field"].is_string(), "{f}");
            assert!(f["sigma"].is_number(), "{f}");
            assert!(f["n"].is_number(), "{f}");
        }
        // A cluster never carries an identity, a status or a lifecycle.
        for forbidden in ["identity", "known_status", "lifecycle", "family"] {
            assert!(c.get(forbidden).is_none(), "{forbidden} leaked: {c}");
        }
    }

    // An unknown or unparsable id is a 404 — never a 200 that could be probed.
    let (st, v) = get(addr, &format!("/api/clusters/{UNKNOWN}"));
    assert_eq!(st, 404, "{v}");
    assert_eq!(v["code"], "not_found", "{v}");
    let (st, v) = get(addr, "/api/clusters/not%20a%20cluster");
    assert_eq!(st, 404, "{v}");

    // Promote is POST-only and refuses an unknown cluster (503 when the server has no audit log).
    let (st, v) = post(addr, &format!("/api/clusters/{UNKNOWN}/promote"), "{}");
    assert!(st == 404 || st == 503, "{st}: {v}");
    let (st, _) = get(addr, &format!("/api/clusters/{UNKNOWN}/promote"));
    assert_eq!(st, 405);
    let (st, _) = post(addr, "/api/clusters", "{}");
    assert_eq!(st, 405);
    let (st, _) = put(addr, "/api/clusters", "{}");
    assert_eq!(st, 405);

    stop_server(serving);
}

#[test]
fn discovery_history_floor_status_and_control_state_have_the_documented_shape() {
    let (_dir_guard, serving, addr) = start_server();

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

    // dc_excluded_hz (T-167, ADR-0013 gap 10): the spectrum stream's DC-notch half-width, taken
    // from the detector's own DC rule, not hardcoded on the wire.
    let (_, v) = get(addr, "/api/streams");
    let spectrum = v["streams"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["stream_id"] == "spectrum/live")
        .unwrap();
    assert_eq!(
        spectrum["dc_excluded_hz"],
        json!(hk_pipeline::observe::DC_NOTCH_HALF_HZ),
        "{spectrum}"
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
    // T-132: the baseline memory bound (docs/api.md `attention`).
    for field in [
        "memory_bytes",
        "unloaded_engines",
        "refused_folds",
        "gain_overflow_folds",
    ] {
        assert!(v["attention"][field].is_u64(), "attention.{field}: {v}");
    }
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
        "cell_shapes",
        "other_shape_values",
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
    // T-133: provenance origins and the source/site filter.
    assert!(is_array(&prov["origins"]), "{v}");
    assert!(prov["other_origin_frames"].is_u64(), "{v}");
    for o in prov["origins"].as_array().unwrap() {
        assert!(
            o["source"].is_null() || o["source"].as_str().unwrap().len() == 16,
            "{o}"
        );
        assert!(o["site"].is_null() || o["site"].is_string(), "{o}");
        assert!(o["frames"].is_u64(), "{o}");
    }
    let (st, unfiltered) = get(addr, &format!("/api/history?{region}"));
    assert_eq!(st, 200);
    assert!(unfiltered["filter"].is_null(), "{unfiltered}");
    let (st, f) = get(
        addr,
        &format!("/api/history?{region}&site=unknown&source=00000000000000ff"),
    );
    assert_eq!(st, 200, "{f}");
    assert_eq!(f["filter"]["site"], "unknown", "{f}");
    assert_eq!(f["filter"]["source"], "00000000000000ff", "{f}");
    for field in [
        "tiles_matched",
        "tiles_mixed",
        "tiles_other",
        "cells_excluded",
        "cells_from_children",
    ] {
        assert!(f["filter"][field].is_u64(), "filter.{field}: {f}");
    }
    let (st, f) = get(addr, &format!("/api/history?{region}&site=mobile"));
    assert_eq!(st, 200, "{f}");
    assert!(f["filter"]["source"].is_null(), "{f}");
    let (st, ct, _) = get_raw(
        addr,
        &format!("/api/history?{region}&site=mobile&format=csv"),
    );
    assert_eq!((st, ct.as_str()), (200, "text/csv; charset=utf-8"));
    for bad in ["site=nowhere", "source=12", "source=zzzzzzzzzzzzzzzz"] {
        let (st, _) = get(addr, &format!("/api/history?{region}&{bad}"));
        assert_eq!(st, 400, "{bad} refused");
    }
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
    let (_dir_guard, serving, addr) = start_server();

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
    for field in [
        "entries",
        "next_cursor",
        "limit",
        "total",
        "identity_access",
    ] {
        assert!(v.get(field).is_some(), "inventory missing {field}: {v}");
    }
    // T-171: `total` ignores pagination, so it is at least the entries on this page.
    assert!(
        v["total"]
            .as_u64()
            .is_some_and(|t| t >= v["entries"].as_array().unwrap().len() as u64),
        "total should be >= this page's entry count: {v}"
    );
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
        // T-158: measurement fields (present, possibly null).
        "snr_db",
        "peak_dbfs",
        // T-219: why this row defers to another, when it does (present, possibly null).
        "relation",
        // T-211: arbitrated classification and a differing latest row (present, possibly null).
        "classification",
        "latest_classification",
        // T-202: the C18 cluster of unknowns this row belongs to (present, possibly null).
        "cluster_id",
    ] {
        assert!(
            row.get(field).is_some(),
            "inventory row missing {field}: {row}"
        );
    }
    assert!(row["id"].is_string(), "{row}");
    // T-211 (ADR-0016 §2): classification objects carry the M3 fields, nullable except the
    // (stored or derived) stage and arbitration rank; the classification agrees with `family`.
    for key in ["classification", "latest_classification"] {
        let c = &row[key];
        if c.is_null() {
            continue;
        }
        for field in [
            "family",
            "confidence",
            "open_set_score",
            "model_version",
            "t_s",
            "taxonomy",
            "stage",
            "arb_rank",
            "coarse",
            "class",
            "top",
            "entropy_norm",
            "flags",
        ] {
            assert!(c.get(field).is_some(), "{key} missing {field}: {row}");
        }
        assert!(c["stage"].is_string(), "{key}.stage: {row}");
        assert!(
            c["arb_rank"].as_u64().is_some_and(|r| r <= 4),
            "{key}.arb_rank: {row}"
        );
        assert!(
            c["taxonomy"].is_null() || c["taxonomy"].is_string(),
            "{key}.taxonomy: {row}"
        );
    }
    if !row["classification"].is_null() {
        assert_eq!(row["classification"]["family"], row["family"], "{row}");
    }
    // T-158: both are backend-derived numbers once a detection is linked, and null together
    // until then (never computed client-side, so the contract only pins their shape and pairing).
    assert!(
        row["snr_db"].is_null() || row["snr_db"].is_number(),
        "snr_db should be a number or null: {row}"
    );
    assert!(
        row["peak_dbfs"].is_null() || row["peak_dbfs"].is_number(),
        "peak_dbfs should be a number or null: {row}"
    );
    assert_eq!(
        row["snr_db"].is_null(),
        row["peak_dbfs"].is_null(),
        "snr_db and peak_dbfs come from the same detection, so they are null together: {row}"
    );
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
    // T-219: rows that defer to another are hidden by default and listed with `relations=all`.
    let (st, all) = get(addr, "/api/inventory?relations=all");
    assert_eq!(st, 200, "{all}");
    assert!(
        all["total"].as_u64() >= v["total"].as_u64(),
        "relations=all never lists fewer rows: {all}"
    );
    let (st, v) = get(addr, "/api/inventory?relations=bogus");
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
    let (_dir_guard, serving, addr) = start_server();

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
    for field in [
        "state",
        "lifecycle",
        "recurrence",
        "classification",
        "latest_classification",
        "estimated_params",
    ] {
        assert!(
            row.get(field).is_some(),
            "inventory entry missing {field}: {row}"
        );
    }
    // T-163: the emitter's latest blind-estimated parameters, or `null` before any demodulation
    // session has run. Shape only, like the decode fields below: whether the composed pipeline's
    // analog chain has demodulated this station yet by now is not deterministic on a fixture.
    // The `null`-vs-populated distinction and its measured-values-only rule are covered against a
    // controlled seeded repository in `crates/hk-api/tests/estimated_params_api.rs`.
    let p = &row["estimated_params"];
    assert!(p.is_null() || p.is_object(), "{row}");
    if p.is_object() {
        assert!(p["modulation"].is_string(), "{row}");
        assert!(p["t_s"].is_f64(), "{row}");
        assert!(p["source_session"].is_string(), "{row}");
        for field in [
            "symbol_rate_hz",
            "mod_order",
            "deviation_hz",
            "cfo_hz",
            "bandwidth_hz",
            "roll_off",
            "pilot_hz",
            "source_recording",
        ] {
            assert!(
                p.get(field).is_some(),
                "estimated_params missing {field}: {row}"
            );
        }
    }
    let (st, v) = get(addr, "/api/inventory/not-a-uuid");
    assert_eq!(st, 404, "{v}");

    // T-159: the emitter's latest decode fields. `hk serve`'s composed pipeline runs its
    // built-in RDS decoder on every WFM station automatically (no recipe started here), so this
    // station's row is often already populated by the time it is confirmed above; shape only
    // (`decoder`/`frame_model`/`at`/`fields`/`crc`/`source_session`), since RDS lock timing on a
    // fixture is not deterministic. Emptiness and per-frame-model grouping are covered against a
    // controlled seeded repository in `crates/hk-api/tests/decode_api.rs`.
    let (st, v) = get(addr, &format!("/api/inventory/{id}/decode"));
    assert_eq!(st, 200, "{v}");
    let decodes = v["decodes"].as_array().unwrap_or_else(|| panic!("{v}"));
    for row in decodes {
        assert!(row["decoder"].is_string(), "{row}");
        assert!(row["frame_model"].is_string(), "{row}");
        assert!(row["at"].is_f64(), "{row}");
        assert!(row["fields"].is_object(), "{row}");
        assert!(row["crc"]["valid"].is_boolean(), "{row}");
        assert!(
            row["recipe_id"].is_null() || row["recipe_id"].is_string(),
            "{row}"
        );
        assert!(
            row["source_session"].is_null() || row["source_session"].is_string(),
            "{row}"
        );
    }
    let (st, v) = get(addr, &format!("/api/inventory/{}/decode", EmitterId::new()));
    assert_eq!((st, v["code"].as_str()), (404, Some("not_found")), "{v}");

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

    // User band (T-191): set, read back beside the unchanged measured band, validate, clear.
    let band = format!("/api/inventory/{id}/band");
    assert_eq!(row["user_band"], Value::Null, "{row}");
    let (m_lo, m_hi) = (
        row["f_lo_hz"].as_f64().unwrap(),
        row["f_hi_hz"].as_f64().unwrap(),
    );
    let (lo, hi) = (m_lo + 5e3, m_hi - 5e3);
    let (st, v) = put(
        addr,
        &band,
        &format!(r#"{{"f_lo": {lo}, "f_hi": {hi}, "reason": "tighter edges"}}"#),
    );
    assert_eq!(st, 200, "{v}");
    let ub = &v["user_band"];
    assert_eq!(
        (ub["f_lo"].as_f64(), ub["f_hi"].as_f64()),
        (Some(lo), Some(hi)),
        "{v}"
    );
    assert!(ub["set_at"].is_number() && ub["actor"].is_string(), "{v}");
    assert_eq!(v["entry"]["user_band"], *ub, "{v}");
    let (st, got) = get(addr, &format!("/api/inventory/{id}"));
    assert_eq!(st, 200);
    assert_eq!(got["user_band"]["f_lo"].as_f64(), Some(lo), "{got}");
    assert_eq!(got["user_band"]["reason"], json!("tighter edges"), "{got}");
    // The measured band is untouched by the override (the live pipeline may move it on its own,
    // but never to the user's edges).
    assert_ne!(got["f_lo_hz"].as_f64(), Some(lo), "{got}");
    let (st, v) = get(addr, "/api/inventory");
    assert_eq!(st, 200);
    assert!(
        v["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["id"] == id && e["user_band"]["f_hi"].as_f64() == Some(hi)),
        "list rows carry user_band: {v}"
    );
    for bad in [
        format!(r#"{{"f_lo": {hi}, "f_hi": {lo}}}"#),
        format!(r#"{{"f_lo": 0, "f_hi": {hi}}}"#),
        format!(r#"{{"f_lo": {lo}}}"#),
        format!(r#"{{"f_lo": "x", "f_hi": {hi}}}"#),
        format!(r#"{{"f_lo": {}, "f_hi": {}}}"#, m_lo - 30e6, m_hi + 30e6),
        format!(r#"{{"f_lo": {}, "f_hi": {}}}"#, m_hi + 2e6, m_hi + 3e6),
        format!(r#"{{"f_lo": {lo}, "f_hi": {hi}, "bogus": 1}}"#),
    ] {
        let (st, v) = put(addr, &band, &bad);
        assert_eq!(
            (st, v["code"].as_str()),
            (400, Some("invalid")),
            "{bad}: {v}"
        );
    }
    let (st, v) = put(
        addr,
        &format!(
            "/api/inventory/{}/band",
            "0199aaaa-0000-7000-8000-000000000000"
        ),
        &format!(r#"{{"f_lo": {lo}, "f_hi": {hi}}}"#),
    );
    assert_eq!((st, v["code"].as_str()), (404, Some("not_found")), "{v}");
    let (st, v) = call(
        addr,
        "PUT",
        &band,
        None,
        Some(&format!(r#"{{"f_lo": {lo}, "f_hi": {hi}}}"#)),
    );
    assert_eq!(st, 401, "{v}");
    let (st, v) = call(addr, "DELETE", &band, None, None);
    assert_eq!(st, 401, "{v}");
    let (st, v) = delete(addr, &band);
    assert_eq!((st, &v["cleared"]), (200, &json!(true)), "{v}");
    assert_eq!(v["entry"]["user_band"], Value::Null, "{v}");
    let (st, v) = delete(addr, &band);
    assert_eq!((st, &v["cleared"]), (200, &json!(false)), "{v}");

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
    let (st, v) = put(addr, &band, &format!(r#"{{"f_lo": {lo}, "f_hi": {hi}}}"#));
    assert_eq!(
        (st, v["code"].as_str()),
        (404, Some("not_found")),
        "a deleted entry takes no user band: {v}"
    );

    stop_server(serving);
}

// --- Control API: display/pause (device-independent), bookmarks, selections, outputs ------------

#[test]
fn control_display_pause_and_bookmarks_answer_as_documented() {
    let (_dir_guard, serving, addr) = start_server();

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
    let (_dir_guard, serving, addr) = start_server();

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
    let (_dir_guard, serving, addr) = start_server();

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

/// T-190: `POST /api/analyze` (the MUI "Analyze / synthesize decoder" stub, docs/15 §7)
/// validates its target — a selection, an inventory emitter (merged ids resolve to the live
/// entity like `/api/inventory/{id}`), or an ad-hoc band — and answers `501 not_implemented`
/// once the target is known to exist; the engine itself is MAUTO's (docs/15 §8), not built yet.
#[test]
fn analyze_stub_validates_targets_and_answers_not_implemented() {
    let (_dir_guard, serving, addr) = start_server();

    // A valid band target: 501 not_implemented once it validates. No engine runs.
    let (st, v) = post(
        addr,
        "/api/analyze",
        r#"{"band": {"f_lo": 101.2e6, "f_hi": 101.4e6}}"#,
    );
    assert_eq!(
        (st, v["code"].as_str()),
        (501, Some("not_implemented")),
        "{v}"
    );
    assert_eq!(v["error"], json!("analyze is not implemented yet"), "{v}");

    // A band target with a history time window: still just a stub answer.
    let (st, v) = post(
        addr,
        "/api/analyze",
        r#"{"band": {"f_lo": 101.2e6, "f_hi": 101.4e6, "t_lo": 0.0, "t_hi": 10.0}}"#,
    );
    assert_eq!((st, v["code"].as_str()), (501, Some("not_implemented")));

    // A valid selection target: also 501, not a validation error.
    let (st, s) = post(
        addr,
        "/api/selections",
        r#"{"name": "analyze target", "f_lo": 101.2e6, "f_hi": 101.4e6}"#,
    );
    assert_eq!(st, 201, "{s}");
    let selection_id = s["id"].as_str().unwrap().to_owned();
    let (st, v) = post(
        addr,
        "/api/analyze",
        &json!({ "selection_id": selection_id }).to_string(),
    );
    assert_eq!((st, v["code"].as_str()), (501, Some("not_implemented")));

    // A valid emitter target (the blind-detected station, no frequency lookup): also 501.
    let emitter_id = {
        let mut found = None;
        wait_for(
            "the 101.3 MHz station to appear so its emitter id is known",
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
    let (st, v) = post(
        addr,
        "/api/analyze",
        &json!({ "emitter_id": emitter_id }).to_string(),
    );
    assert_eq!((st, v["code"].as_str()), (501, Some("not_implemented")));

    // 400 invalid: zero target forms, several target forms, an unknown field, a malformed band,
    // f_lo >= f_hi.
    for bad in [
        r#"{}"#,
        r#"{"band": {"f_lo": 1e6, "f_hi": 2e6}, "emitter_id": "x"}"#,
        r#"{"selection_id": "x", "extra": 1}"#,
        r#"{"band": {"f_lo": 2e6, "f_hi": 1e6}}"#,
        r#"{"band": {"f_lo": 1e6, "f_hi": 1e6}}"#,
        r#"{"band": {"f_hi": 1e6}}"#,
        r#"{"band": {"f_lo": 1e6, "f_hi": 2e6, "extra": 1}}"#,
        r#"{"band": "nope"}"#,
    ] {
        let (st, v) = post(addr, "/api/analyze", bad);
        assert_eq!(
            (st, v["code"].as_str()),
            (400, Some("invalid")),
            "{bad}: {v}"
        );
    }

    // 404 not_found: an unknown (but well-formed) selection/emitter id, and a malformed one (the
    // same shape a malformed `/api/inventory/{id}` path answers with).
    for bad in [
        json!({ "selection_id": SelectionId::new().to_string() }),
        json!({ "emitter_id": EmitterId::new().to_string() }),
        json!({ "selection_id": "not-a-uuid" }),
        json!({ "emitter_id": "not-a-uuid" }),
    ] {
        let (st, v) = post(addr, "/api/analyze", &bad.to_string());
        assert_eq!(
            (st, v["code"].as_str()),
            (404, Some("not_found")),
            "{bad}: {v}"
        );
    }

    // 401: no token, and the query-string token (mutating requests need the header).
    let (st, v) = call(
        addr,
        "POST",
        "/api/analyze",
        None,
        Some(r#"{"band": {"f_lo": 1e6, "f_hi": 2e6}}"#),
    );
    assert_eq!(st, 401, "{v}");

    // Wrong method: 405 with Allow.
    let (st, v) = get(addr, "/api/analyze");
    assert_eq!(st, 405, "{v}");

    stop_server(serving);
}

/// T-157: the rolling IQ capture buffer of `hk serve` over the mock SDR device fills with no
/// request; `GET /api/iqbuffer` reports its span, quota, segments (tuning and gain) and counts, and
/// `POST /api/iqbuffer/clip` exports a span as a SigMF recording, as `docs/api.md` "IQ capture
/// buffer" documents; bad queries and bodies answer 400, an empty band 404, other methods 405,
/// and the clip needs the header token.
#[test]
fn iq_buffer_status_and_clip_export_answer_as_documented() {
    let (_dir_guard, serving, addr) = start_server();
    const CLIP_SAMPLES: u64 = 240_000; // 0.1 s at 2.4 Msps
    let mut status = Value::Null;
    wait_for(
        "a buffered segment of 0.1 s",
        Duration::from_secs(60),
        || {
            let (st, v) = get(addr, "/api/iqbuffer");
            assert_eq!(st, 200, "{v}");
            status = v;
            status["segments"].as_array().is_some_and(|s| {
                s.iter()
                    .any(|g| g["samples"].as_u64().unwrap_or(0) >= CLIP_SAMPLES)
            })
        },
    );
    let v = &status;
    assert_eq!(v["enabled"], json!(true), "{v}");
    assert!(v["reason"].is_null(), "{v}");
    assert!(v["dir"].is_string(), "{v}");
    for k in [
        "quota_bytes",
        "max_clip_bytes",
        "chunk_bytes",
        "chunk_files",
        "fs_free_bytes",
        "fs_total_bytes",
        "min_free_bytes",
        "pauses",
        "paused_samples",
        "bytes",
        "disk_bytes",
        "samples",
        "segments_total",
        "segments_omitted",
        "dropped_samples",
        "gated_samples",
        "write_errors",
        "failed_samples",
    ] {
        assert!(v[k].is_u64(), "{k}: {v}");
    }
    for k in ["retention_s", "t0", "t1", "span_s"] {
        assert!(v[k].is_f64(), "{k}: {v}");
    }
    assert_eq!(v["paused"], json!(false), "{v}");
    assert!(
        v["fs_free_bytes"].as_u64() <= v["fs_total_bytes"].as_u64(),
        "{v}"
    );
    // T-178: the pre-allocated persistent ring.
    for k in [
        "slot_count",
        "allocated_bytes",
        "recovered_segments",
        "discarded_slots",
        "wrap_count",
        "run",
        "head_slot",
        "head_offset_bytes",
        "sync_errors",
        "poisoned_samples",
        // T-217: samples the feeder saw before the ring finished opening, never buffered.
        "allocation_skipped_samples",
    ] {
        assert!(v[k].is_u64(), "{k}: {v}");
    }
    // The ring here opens well inside `ALLOCATION_WAIT`, so nothing was skipped (T-217's own
    // allocation-window behaviour is covered by tests/iq_buffer_allocation_http.rs).
    assert_eq!(v["allocation_skipped_samples"], json!(0), "{v}");
    assert_eq!(
        (
            &v["persisted"],
            &v["allocation"],
            v["allocation_progress"].as_f64()
        ),
        (&json!(true), &json!("full"), Some(1.0)),
        "{v}"
    );
    assert!(v["preallocated"].is_boolean(), "{v}");
    assert_eq!(
        v["allocated_bytes"].as_u64(),
        Some(v["slot_count"].as_u64().unwrap() * v["chunk_bytes"].as_u64().unwrap()),
        "{v}"
    );
    assert!(
        v["allocated_bytes"].as_u64() <= v["quota_bytes"].as_u64()
            && v["disk_bytes"].as_u64() > v["allocated_bytes"].as_u64(),
        "{v}"
    );
    // The quota: min(retention × the mock's highest rate (20 Msps) × 2 bytes, max_bytes).
    let implied = (v["retention_s"].as_f64().unwrap() * 20e6).ceil() as u64 * 2;
    let quota = v["max_bytes"].as_u64().map_or(implied, |m| m.min(implied));
    assert!(v["max_bytes"].is_null() || v["max_bytes"].is_u64(), "{v}");
    assert_eq!(v["quota_bytes"].as_u64(), Some(quota), "{v}");
    assert_eq!(v["bytes"].as_u64(), v["samples"].as_u64().map(|n| 2 * n));
    assert!(v["disk_bytes"].as_u64() >= v["bytes"].as_u64(), "{v}");
    assert!(v["error"].is_null(), "{v}");
    assert!(is_object(&v["evicted"]), "{v}");
    for k in ["chunks", "segments", "samples", "bytes"] {
        assert!(v["evicted"][k].is_u64(), "evicted.{k}: {v}");
    }
    assert!(is_array(&v["gaps"]), "{v}");
    let segs = v["segments"].as_array().expect("segments");
    let seg = segs
        .iter()
        .find(|g| g["samples"].as_u64().unwrap_or(0) >= CLIP_SAMPLES)
        .expect("a segment of 0.1 s");
    for k in ["id", "run", "samples", "global_index", "dropped_before"] {
        assert!(seg[k].is_u64(), "segment {k}: {seg}");
    }
    assert_eq!(seg["run"], v["run"], "this run's segment: {seg}");
    for k in ["t0_ns", "t1_ns"] {
        assert!(seg[k].is_i64(), "segment {k}: {seg}");
    }
    for k in [
        "t0",
        "t1",
        "center_hz",
        "sample_rate_hz",
        "bandwidth_hz",
        "lna_db",
        "vga_db",
    ] {
        assert!(seg[k].is_number(), "segment {k}: {seg}");
    }
    assert_eq!(seg["center_hz"], json!(100.8e6), "{seg}");
    assert_eq!(seg["sample_rate_hz"], json!(2.4e6), "{seg}");
    assert!(
        seg["amp_on"].is_boolean() && seg["overload"].is_boolean(),
        "{seg}"
    );
    assert!(
        seg["device_id"].is_string() && seg["content_class"].is_string(),
        "{seg}"
    );

    let (st, v) = get(addr, "/api/iqbuffer?limit=1");
    assert_eq!(st, 200, "{v}");
    assert!(v["segments"].as_array().unwrap().len() <= 1, "{v}");
    for q in [
        "bogus=1",
        "limit=0",
        "limit=x",
        "t0=5&t1=4",
        "t0=abc",
        "t0=-5",
    ] {
        let (st, v) = get(addr, &format!("/api/iqbuffer?{q}"));
        assert_eq!((st, v["code"].as_str()), (400, Some("invalid")), "{q}: {v}");
    }

    // Export the first 0.1 s of a segment by stream index: exactly 240 000 samples.
    let g0 = seg["global_index"].as_u64().unwrap();
    let body =
        json!({ "global_index": g0, "samples": CLIP_SAMPLES, "label": "contract" }).to_string();
    let (st, _) = call(addr, "POST", "/api/iqbuffer/clip", None, Some(&body));
    assert_eq!(st, 401, "the clip needs the token");
    let (st, v) = post(addr, "/api/iqbuffer/clip", &body);
    assert_eq!(st, 200, "{v}");
    let r = &v["recording"];
    assert!(r["id"].is_string(), "{r}");
    assert_eq!(r["label"], json!("contract"), "{r}");
    let n = r["samples"].as_u64().unwrap();
    assert_eq!(n, CLIP_SAMPLES, "{r}");
    assert_eq!(r["captures"][0]["global_index"].as_u64(), Some(g0), "{r}");
    assert_eq!(r["captures"][0]["run"], status["run"], "{r}");
    assert_eq!(r["bytes"].as_u64(), Some(2 * n), "{r}");
    // T-178: stream indices are per run; the same indices of another run select nothing.
    let other = json!({
        "global_index": g0,
        "samples": CLIP_SAMPLES,
        "run": status["run"].as_u64().unwrap() + 1
    })
    .to_string();
    let (st, v3) = post(addr, "/api/iqbuffer/clip", &other);
    assert_eq!((st, v3["code"].as_str()), (404, Some("not_found")), "{v3}");
    // The same span in exact ns selects exactly the same samples.
    let t0_ns = seg["t0_ns"].as_i64().unwrap();
    assert_eq!(r["t0_ns"].as_i64(), Some(t0_ns), "{r}");
    assert_eq!(r["t1_ns"].as_i64(), Some(t0_ns + 100_000_000), "{r}");
    let by_ns = json!({ "t0_ns": t0_ns, "t1_ns": t0_ns + 100_000_000 }).to_string();
    let (st, v2) = post(addr, "/api/iqbuffer/clip", &by_ns);
    assert_eq!(st, 200, "{v2}");
    assert_eq!(
        v2["recording"]["samples"].as_u64(),
        Some(CLIP_SAMPLES),
        "{v2}"
    );
    assert_eq!(
        v2["recording"]["captures"][0]["global_index"].as_u64(),
        Some(g0),
        "{v2}"
    );
    let (t0, t1) = (
        seg["t0"].as_f64().unwrap(),
        seg["t0"].as_f64().unwrap() + 0.1,
    );
    assert_eq!(r["sample_rate_hz"], json!(2.4e6), "{r}");
    assert_eq!(r["center_hz"], json!(100.8e6), "{r}");
    assert!(r["band"].is_null() && r["content_class"].is_string(), "{r}");
    for k in ["t0", "t1"] {
        assert!(r[k].is_f64(), "{k}: {r}");
    }
    let id = r["id"].as_str().unwrap();
    assert_eq!(
        r["meta_uri"],
        json!(format!("recordings/{id}.sigmf-meta")),
        "{r}"
    );
    assert_eq!(
        r["data_uri"],
        json!(format!("recordings/{id}.sigmf-data")),
        "{r}"
    );
    let caps = r["captures"].as_array().expect("captures");
    assert_eq!(caps.len(), 1, "{r}");
    assert_eq!(caps[0]["sample_start"], json!(0), "{r}");
    assert_eq!(caps[0]["samples"].as_u64(), Some(n), "{r}");
    assert_eq!(caps[0]["segment"], seg["id"], "{r}");
    for k in [
        "global_index",
        "t0",
        "t0_ns",
        "center_hz",
        "sample_rate_hz",
        "bandwidth_hz",
        "lna_db",
        "vga_db",
        "amp_on",
        "device_id",
    ] {
        assert!(!caps[0][k].is_null(), "capture {k}: {r}");
    }
    let data = std::fs::metadata(r["data_path"].as_str().unwrap()).unwrap();
    assert_eq!(data.len(), 2 * n);
    let meta: Value =
        serde_json::from_slice(&std::fs::read(r["meta_path"].as_str().unwrap()).unwrap()).unwrap();
    assert_eq!(meta["global"]["core:datatype"], json!("ci8"), "{meta}");
    assert_eq!(
        meta["captures"][0]["core:global_index"], caps[0]["global_index"],
        "{meta}"
    );
    assert_eq!(
        meta["captures"][0]["core:frequency"],
        json!(100.8e6),
        "{meta}"
    );

    let band = json!({ "t0": t0, "t1": t1, "band": { "f_lo": 5.0e9, "f_hi": 5.1e9 } }).to_string();
    let (st, v) = post(addr, "/api/iqbuffer/clip", &band);
    assert_eq!((st, v["code"].as_str()), (404, Some("not_found")), "{v}");
    for bad in [
        json!({ "t0": t1, "t1": t0 }),
        json!({ "t0": t0 }),
        json!({ "t0": -8.9e9, "t1": t1 }),
        json!({ "t0_ns": 5, "t1_ns": 4 }),
        json!({ "t0_ns": -5, "t1_ns": 4 }),
        json!({ "global_index": 0, "samples": 0 }),
        json!({ "global_index": u64::MAX, "samples": 2 }),
        json!({ "t0": t0, "t1": t1, "global_index": 0, "samples": 5 }),
        json!({ "label": "no range" }),
        json!({ "t0": t0, "t1": t1, "extra": 1 }),
        json!({ "t0": t0, "t1": t1, "band": { "f_lo": 2.0, "f_hi": 1.0 } }),
        json!({ "t0": t0, "t1": t1, "label": 5 }),
        json!({ "t0": t0, "t1": t1, "run": -1 }),
        json!({ "t0": t0, "t1": t1, "run": "one" }),
    ] {
        let (st, v) = post(addr, "/api/iqbuffer/clip", &bad.to_string());
        assert_eq!(
            (st, v["code"].as_str()),
            (400, Some("invalid")),
            "{bad}: {v}"
        );
    }
    let (st, _) = get(addr, "/api/iqbuffer/clip");
    assert_eq!(st, 405);
    let (st, _) = post(addr, "/api/iqbuffer", "{}");
    assert_eq!(st, 405);

    stop_server(serving);
}

/// T-205: `POST /api/datasets` (a filter matching nothing, so the export is deterministic and
/// fast: no live decode needs to happen), `GET /api/datasets` and `GET /api/datasets/{id}`, and
/// the documented `400`/`404`/`405` refusals.
#[test]
fn dataset_export_and_manifest_lookup_answer_as_documented() {
    let (_dir_guard, serving, addr) = start_server();

    let (st, v) = get(addr, "/api/datasets");
    assert_eq!(st, 200, "{v}");
    assert!(is_array(&v["datasets"]), "{v}");

    // A family no fixture ever produces: the export runs and finds nothing, deterministically.
    let (st, v) = post(
        addr,
        "/api/datasets",
        &json!({ "filter": { "family": "css" }, "split": "dev" }).to_string(),
    );
    assert_eq!(st, 201, "{v}");
    let dataset = &v["dataset"];
    assert!(dataset["id"].is_string(), "{dataset}");
    assert_eq!(dataset["split"], json!("dev"), "{dataset}");
    assert_eq!(dataset["filter"]["family"], json!("css"), "{dataset}");
    assert!(is_array(&dataset["samples"]), "{dataset}");
    assert_eq!(dataset["samples"], json!([]), "{dataset}");
    assert_eq!(dataset["skipped"], json!(0), "{dataset}");
    let id = dataset["id"].as_str().unwrap().to_owned();

    // The manifest is readable back by id, and listed.
    let (st, v) = get(addr, &format!("/api/datasets/{id}"));
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["dataset"]["id"], json!(id), "{v}");
    let (st, v) = get(addr, "/api/datasets");
    assert_eq!(st, 200, "{v}");
    assert!(
        v["datasets"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["id"] == json!(id)),
        "{v}"
    );

    let (st, v) = get(addr, "/api/datasets/no-such-id");
    assert_eq!((st, v["code"].as_str()), (404, Some("not_found")), "{v}");

    for bad in [
        json!({ "filter": {} }), // split is required
        json!({ "filter": {}, "split": "sideways" }),
        json!({ "filter": {}, "split": "dev", "max_samples": 0 }),
        json!({ "filter": {}, "split": "dev", "pad_pre_s": -1.0 }),
        json!({ "filter": { "emitter": "not-an-id" }, "split": "dev" }),
        json!({ "filter": {}, "split": "dev", "extra": 1 }),
    ] {
        let (st, v) = post(addr, "/api/datasets", &bad.to_string());
        assert_eq!(
            (st, v["code"].as_str()),
            (400, Some("invalid")),
            "{bad}: {v}"
        );
    }

    let (st, _) = put(addr, "/api/datasets", "{}");
    assert_eq!(st, 405);
    let (st, _) = post(addr, "/api/datasets/x", "{}");
    assert_eq!(st, 405);

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
    let (_dir_guard, serving, addr) = start_server();
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
    // dc_excluded_hz (T-167, ADR-0013 gap 10): additive on the header itself, not just discovery.
    assert_eq!(
        header["dc_excluded_hz"],
        json!(hk_pipeline::observe::DC_NOTCH_HALF_HZ),
        "{header}"
    );
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
    let (_dir_guard, serving, addr) = start_server();
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
    let (_dir_guard, serving, addr) = start_server();

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
    let (_dir_guard, serving, addr) = start_server();
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

/// T-160: stage tap `view=spectrum` (stream contract §14.4). `view=raw`/omitted is unchanged; on
/// an `iq`/`real` port `view=spectrum` serves a bounded PSD (`kind: spectrum`, `rf32_le` rows,
/// `fft_size` declared, at most 25 rows/s) instead of raw samples; an unsupported port type is
/// refused 409, an unknown `view` value 400 (§14.8 opener refusals).
#[test]
fn stage_tap_spectrum_view_answers_as_documented() {
    let (_dir_guard, serving, addr) = start_server();
    let doc = json!({
        "schema": "hackriff.recipe", "schema_version": 2, "id": "t160-contract", "version": 1,
        "name": "T-160 contract",
        "input": {"port": "iq", "sample_rate_hz": 48000.0, "bandwidth_hz": 40000.0},
        "nodes": [
            {"id": "fm", "block": "fm_demod", "params": {"deviation_hz": 5000}},
            {"id": "clock", "block": "clock_recovery", "params": {"symbol_rate_bd": 1000.0}},
            {"id": "bits", "block": "slicer"}
        ],
        "outputs": [{"id": "fm", "kind": "stage", "from": "fm"}],
        "output_policy": {"content_class": "unrestricted"}
    });
    let target = json!({"band": {"f_lo": STATION_HZ - 20e3, "f_hi": STATION_HZ + 20e3}});
    let (st, v) = post(
        addr,
        "/api/pipelines",
        &json!({"recipe": doc, "target": target}).to_string(),
    );
    assert_eq!(st, 201, "{v}");
    let pid = v["id"].as_str().unwrap().to_owned();

    // Default (no view / view=raw) is unchanged: the fm node's real port serves raw samples,
    // with no spectrum geometry in the header.
    let mut ws = connect_ws(
        addr,
        &format!("/ws/open/stage?token={TOKEN}&pipeline={pid}&node=fm"),
    )
    .unwrap();
    let Message::Text(t) = ws.read().unwrap() else {
        panic!("header first")
    };
    let header: Value = serde_json::from_str(t.as_str()).unwrap();
    assert_eq!(
        (&header["kind"], &header["datatype"]),
        (&json!("audio"), &json!("rf32_le"))
    );
    assert!(header.get("fft_size").is_none(), "raw view: {header}");
    let _ = ws.close(None);

    // view=spectrum on the same real port: a bounded PSD, not raw samples.
    let mut ws = connect_ws(
        addr,
        &format!("/ws/open/stage?token={TOKEN}&pipeline={pid}&node=fm&view=spectrum"),
    )
    .unwrap();
    let Message::Text(t) = ws.read().unwrap() else {
        panic!("header first")
    };
    let header: Value = serde_json::from_str(t.as_str()).unwrap();
    assert_eq!(header["kind"], json!("spectrum"), "{header}");
    assert_eq!(header["datatype"], json!("rf32_le"), "{header}");
    assert_eq!(header["fft_size"], json!(4096), "{header}");
    assert!(
        header["sample_rate_hz"].as_f64().unwrap() <= 25.0 + 1e-9,
        "{header}"
    );
    assert_eq!(
        header["center_hz"],
        json!(0.0),
        "a real port is demodulated baseband, not RF-referenced: {header}"
    );
    assert!(header["bandwidth_hz"].as_f64().unwrap() > 0.0, "{header}");
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        assert!(Instant::now() < deadline, "a spectrum data record");
        if let Message::Binary(b) = ws.read().unwrap() {
            assert_eq!(
                b.len(),
                32 + 4 * 4096,
                "one PSD row of fft_size f32s: {}",
                b.len()
            );
            break;
        }
    }
    let _ = ws.close(None);

    // Unsupported port type (soft, bits): 409 (§14.8 "a view the port type doesn't support").
    for node in ["clock", "bits"] {
        let mut ws = connect_ws(
            addr,
            &format!("/ws/open/stage?token={TOKEN}&pipeline={pid}&node={node}&view=spectrum"),
        )
        .unwrap();
        let Message::Text(t) = ws.read().unwrap() else {
            panic!("refusal first ({node})")
        };
        let v: Value = serde_json::from_str(t.as_str()).unwrap();
        assert_eq!(
            (&v["type"], &v["status"]),
            (&json!("refused"), &json!(409)),
            "{v}"
        );
    }

    // An unrecognised view value: 400.
    let mut ws = connect_ws(
        addr,
        &format!("/ws/open/stage?token={TOKEN}&pipeline={pid}&node=fm&view=bogus"),
    )
    .unwrap();
    let Message::Text(t) = ws.read().unwrap() else {
        panic!("refusal first")
    };
    let v: Value = serde_json::from_str(t.as_str()).unwrap();
    assert_eq!(
        (&v["type"], &v["status"]),
        (&json!("refused"), &json!(400)),
        "{v}"
    );

    stop_server(serving);
}

/// T-162: stage tap `view=sync_search` (stream contract §14.4, ADR-0013 §4.9 gap 6). `view=raw`
/// is unchanged; on a `bits` port, `view=sync_search&sync_word=0x…&sync_bits=<n>` serves a match
/// score per candidate bit position (`kind: sync-search`, `rf32_le` rows, row length declared as
/// `fft_size`, at most 25 rows/s) instead of raw bits; an unsupported port type is refused 409, a
/// missing/invalid `sync_word`/`sync_bits` is refused 400, and an unknown `view` value stays 400
/// (§14.8 opener refusals). The score itself resolving a real sync word to a clear peak is
/// asserted at the block level (`hk_pipeline::recipes::tap_sync_search`
/// `known_sync_word_shows_a_clear_peak_and_nothing_comparable_elsewhere`); this test only checks
/// the wiring and shape the UI relies on.
#[test]
fn stage_tap_sync_search_view_answers_as_documented() {
    let (_dir_guard, serving, addr) = start_server();
    let doc = json!({
        "schema": "hackriff.recipe", "schema_version": 2, "id": "t162-contract", "version": 1,
        "name": "T-162 contract",
        "input": {"port": "iq", "sample_rate_hz": 48000.0, "bandwidth_hz": 40000.0},
        "nodes": [
            {"id": "fm", "block": "fm_demod", "params": {"deviation_hz": 5000}},
            {"id": "clock", "block": "clock_recovery", "params": {"symbol_rate_bd": 1000.0}},
            {"id": "bits", "block": "slicer"}
        ],
        "outputs": [{"id": "fm", "kind": "stage", "from": "fm"}],
        "output_policy": {"content_class": "unrestricted"}
    });
    let target = json!({"band": {"f_lo": STATION_HZ - 20e3, "f_hi": STATION_HZ + 20e3}});
    let (st, v) = post(
        addr,
        "/api/pipelines",
        &json!({"recipe": doc, "target": target}).to_string(),
    );
    assert_eq!(st, 201, "{v}");
    let pid = v["id"].as_str().unwrap().to_owned();

    // Default (no view / view=raw) is unchanged: the slicer's bits port serves raw samples, with
    // no sync-search geometry in the header.
    let mut ws = connect_ws(
        addr,
        &format!("/ws/open/stage?token={TOKEN}&pipeline={pid}&node=bits"),
    )
    .unwrap();
    let Message::Text(t) = ws.read().unwrap() else {
        panic!("header first")
    };
    let header: Value = serde_json::from_str(t.as_str()).unwrap();
    assert_eq!(
        (&header["kind"], &header["datatype"]),
        (&json!("bits"), &json!("ru8"))
    );
    assert!(header.get("fft_size").is_none(), "raw view: {header}");
    let _ = ws.close(None);

    // view=sync_search on the same bits port: a match-score row, not raw bits.
    let mut ws = connect_ws(
        addr,
        &format!(
            "/ws/open/stage?token={TOKEN}&pipeline={pid}&node=bits&view=sync_search&sync_word=0x2DD4&sync_bits=16"
        ),
    )
    .unwrap();
    let Message::Text(t) = ws.read().unwrap() else {
        panic!("header first")
    };
    let header: Value = serde_json::from_str(t.as_str()).unwrap();
    assert_eq!(header["kind"], json!("sync-search"), "{header}");
    assert_eq!(header["datatype"], json!("rf32_le"), "{header}");
    let row_len = header["fft_size"].as_u64().expect("row length declared");
    assert!(row_len >= 256, "{header}");
    assert!(
        header["sample_rate_hz"].as_f64().unwrap() <= 25.0 + 1e-9,
        "{header}"
    );
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        assert!(Instant::now() < deadline, "a sync-search data record");
        if let Message::Binary(b) = ws.read().unwrap() {
            assert_eq!(
                b.len(),
                32 + 4 * row_len as usize,
                "one match-score row of the declared length: {}",
                b.len()
            );
            break;
        }
    }
    let _ = ws.close(None);

    // Unsupported port type (real, soft): 409 (§14.8 "a view the port type doesn't support").
    for node in ["fm", "clock"] {
        let mut ws = connect_ws(
            addr,
            &format!(
                "/ws/open/stage?token={TOKEN}&pipeline={pid}&node={node}&view=sync_search&sync_word=0x2DD4&sync_bits=16"
            ),
        )
        .unwrap();
        let Message::Text(t) = ws.read().unwrap() else {
            panic!("refusal first ({node})")
        };
        let v: Value = serde_json::from_str(t.as_str()).unwrap();
        assert_eq!(
            (&v["type"], &v["status"]),
            (&json!("refused"), &json!(409)),
            "{v}"
        );
    }

    // Missing sync_word / sync_bits, and an invalid one of each: 400.
    for qs in [
        "view=sync_search",
        "view=sync_search&sync_word=0x2DD4",
        "view=sync_search&sync_bits=16",
        "view=sync_search&sync_word=not-hex&sync_bits=16",
        "view=sync_search&sync_word=0x2DD4&sync_bits=not-a-number",
        // sync_word wider than sync_bits: refused by the same rule the block itself applies.
        "view=sync_search&sync_word=0xFFFF&sync_bits=8",
    ] {
        let mut ws = connect_ws(
            addr,
            &format!("/ws/open/stage?token={TOKEN}&pipeline={pid}&node=bits&{qs}"),
        )
        .unwrap();
        let Message::Text(t) = ws.read().unwrap() else {
            panic!("refusal first ({qs})")
        };
        let v: Value = serde_json::from_str(t.as_str()).unwrap();
        assert_eq!(
            (&v["type"], &v["status"]),
            (&json!("refused"), &json!(400)),
            "{qs}: {v}"
        );
    }

    // An unrecognised view value: still 400.
    let mut ws = connect_ws(
        addr,
        &format!("/ws/open/stage?token={TOKEN}&pipeline={pid}&node=bits&view=bogus"),
    )
    .unwrap();
    let Message::Text(t) = ws.read().unwrap() else {
        panic!("refusal first")
    };
    let v: Value = serde_json::from_str(t.as_str()).unwrap();
    assert_eq!(
        (&v["type"], &v["status"]),
        (&json!("refused"), &json!(400)),
        "{v}"
    );

    stop_server(serving);
}

/// Retries the `/ws/open/iq` handshake (the run may be mid-replumb, 503 `replumbing`, right after
/// start): returns the connection *after* its header message, plus the parsed header.
fn wait_for_iq(addr: SocketAddr, query: &str) -> (Ws, Value) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let mut ws = connect_ws(addr, &format!("/ws/open/iq?{query}&token={TOKEN}")).unwrap();
        match ws.read().unwrap() {
            Message::Text(t) if t.contains("\"type\":\"refused\"") => {
                let v: Value = serde_json::from_str(t.as_str()).unwrap();
                assert!(
                    Instant::now() < deadline,
                    "open/iq kept refusing on {query}: {v}"
                );
                assert!(
                    matches!(v["status"].as_u64(), Some(503 | 409)),
                    "unexpected refusal while waiting for {query}: {v}"
                );
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

/// One binary data record (type 1: 32-byte header + `cf32_le` payload) within a bounded read
/// budget. `re,im` `f32` LE pairs, so the payload is always a multiple of 8 bytes.
fn read_one_iq_record(ws: &mut Ws) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(Instant::now() < deadline, "no iq data record arrived");
        if let Message::Binary(b) = ws.read().unwrap() {
            assert!(
                b.len() >= 32,
                "shorter than the 32-byte record header: {}",
                b.len()
            );
            assert_eq!(b[0], 1, "expected a data record (type 1): {}", b[0]);
            assert_eq!(
                (b.len() - 32) % 8,
                0,
                "cf32_le payload must be a whole number of re,im f32 pairs: {}",
                b.len()
            );
            return;
        }
    }
}

/// T-165 (ADR-0013 §4.9 gap 8): `open/iq?emitter=<id>` and `open/iq?f_lo=&f_hi=` serve the
/// requested band's raw channelised samples (`kind: iq`, `cf32_le`), both bound forms, and are
/// refused as documented: an unknown emitter (404), a band outside the tuned window (409), a
/// span over the streaming ceiling or an unrecognised parameter (400), and no token (401 before
/// the upgrade). The served IQ actually carrying the requested channel's own tone (not silence,
/// not some other channel) is proved at the DSP/wiring level by
/// `hk_pipeline::chains::iq::tests::the_ddc_carries_the_in_band_tone_and_rejects_the_out_of_band_one`
/// (the same DDC construction this opener uses); this test only checks the wiring and shapes the
/// UI's "Stream out" action relies on, the T-160/T-162 convention.
#[test]
fn open_iq_answers_as_documented() {
    let (_dir_guard, serving, addr) = start_server();

    // Discovery (`GET /api/streams`): the opener is listed with its shape.
    let (st, v) = get(addr, "/api/streams");
    assert_eq!(st, 200, "{v}");
    let entry = v["on_demand"]
        .as_array()
        .and_then(|a| a.iter().find(|o| o["name"] == json!("iq")))
        .unwrap_or_else(|| panic!("no iq opener in discovery: {v}"))
        .clone();
    assert_eq!(entry["ws_path"], json!("/ws/open/iq"), "{entry}");
    assert_eq!(entry["tcp_target"], json!("open/iq"), "{entry}");
    assert_eq!(entry["kind"], json!("iq"), "{entry}");
    assert_eq!(entry["datatype"], json!("cf32_le"), "{entry}");
    let params = entry["params"].as_array().unwrap();
    for p in ["emitter", "f_lo", "f_hi"] {
        assert!(
            params.iter().any(|x| x == p),
            "iq opener params missing {p}: {entry}"
        );
    }
    assert!(
        !params.iter().any(|x| x == "detection" || x == "mode"),
        "raw IQ takes no mode/detection: {entry}"
    );

    // f_lo/f_hi form, centred on the known station.
    let (f_lo, f_hi) = (STATION_HZ - 100e3, STATION_HZ + 100e3);
    let (mut ws, header) = wait_for_iq(addr, &format!("f_lo={f_lo}&f_hi={f_hi}"));
    assert_eq!(header["kind"], json!("iq"), "{header}");
    assert_eq!(header["datatype"], json!("cf32_le"), "{header}");
    assert!(
        (header["center_hz"].as_f64().unwrap() - STATION_HZ).abs() < 1.0,
        "{header}"
    );
    assert!(
        (header["bandwidth_hz"].as_f64().unwrap() - (f_hi - f_lo)).abs() < 1.0,
        "{header}"
    );
    assert!(header["sample_rate_hz"].as_f64().unwrap() > 0.0, "{header}");
    assert!(
        header.get("emitter_id").is_none_or(Value::is_null),
        "{header}"
    );
    read_one_iq_record(&mut ws);
    let _ = ws.close(None);

    // emitter form: the same station, found blind (vision step 4), targeted by its inventory id.
    let emitter_id = {
        let mut found = None;
        wait_for(
            "the station to appear so its emitter id is known",
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
    let (mut ws, header) = wait_for_iq(addr, &format!("emitter={emitter_id}"));
    assert_eq!(header["kind"], json!("iq"), "{header}");
    assert_eq!(header["emitter_id"], json!(emitter_id), "{header}");
    read_one_iq_record(&mut ws);
    let _ = ws.close(None);

    // Refusals, each completing the upgrade with a `{"type":"refused",...}` text message first.
    let refusal = |query: &str| -> Value {
        let mut ws = connect_ws(addr, &format!("/ws/open/iq?{query}&token={TOKEN}")).unwrap();
        let Message::Text(t) = ws.read().unwrap() else {
            panic!("refusal first ({query})")
        };
        let v: Value = serde_json::from_str(t.as_str()).unwrap();
        assert_eq!(v["type"], json!("refused"), "{query}: {v}");
        v
    };
    // No target.
    assert_eq!(refusal("")["status"], json!(400));
    // An unrecognised parameter: no mode, there is nothing to demodulate.
    assert_eq!(refusal("f_lo=1e6&f_hi=2e6&mode=am")["status"], json!(400));
    // Unknown emitter.
    assert_eq!(
        refusal(&format!("emitter={}", EmitterId::new()))["status"],
        json!(404)
    );
    // Outside the tuned window (the fixture is tuned near 100.8 MHz).
    assert_eq!(
        refusal("f_lo=200000000&f_hi=200100000")["status"],
        json!(409)
    );
    // Wider than the streaming ceiling (2 MHz), even though it would fit inside the tuned window.
    let (wide_lo, wide_hi) = (FIXTURE_CENTER_HZ - 1.1e6, FIXTURE_CENTER_HZ + 1.1e6);
    assert_eq!(
        refusal(&format!("f_lo={wide_lo}&f_hi={wide_hi}"))["status"],
        json!(400)
    );

    // No token: refused before the upgrade (unlike an opener refusal, a plain HTTP status).
    let err = connect_ws(addr, &format!("/ws/open/iq?f_lo={f_lo}&f_hi={f_hi}")).unwrap_err();
    match err {
        tungstenite::Error::Http(resp) => assert_eq!(resp.status().as_u16(), 401),
        e => panic!("expected an HTTP 401 refusal, got {e}"),
    }

    stop_server(serving);
}

// T-089 inspector

/// T-089: `POST /api/inspector/parse` evaluates a draft field map (the RDS worked recipe's) over
/// submitted frames and answers layer trees with absolute bit/byte ranges and the per-byte leaf
/// index; `POST /api/captures/{id}/parse` answers 503 on a server without a capture store; auth,
/// method and validation refusals as documented.
#[test]
fn inspector_parse_answers_as_documented() {
    let (_dir_guard, serving, addr) = start_server();
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
    let (_dir_guard, serving, addr) = start_server();
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
    let (_dir_guard, serving, addr) = start_server();
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
    let (_dir_guard, serving, addr) = start_server();
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
    let (_dir_guard, serving, addr) = start_server();
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
    let waiter = std::thread::spawn(move || {
        let _ = tx.send(handle.wait());
    });
    rx.recv_timeout(Duration::from_secs(30))
        .expect("the run stopped")
        .expect("the run finished cleanly");
    // The waiter drops the pipeline handle after sending, and that teardown writes (the
    // observation log seals its open hour and recreates its dated directory). Joining it is what
    // stops this test — the one T-232 measured as its residual orphan — from racing the temp-dir
    // guard once `hk-api`'s own connection threads are no longer the racer (T-236).
    let _ = waiter.join();
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
    let (_dir_guard, serving, addr) = start_server();
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

/// T-118: `/api/channels` and `/api/occupancy` (series and span) answer the documented shapes;
/// bad queries are 400 and other methods 405.
#[test]
fn occupancy_and_channel_routes_answer_as_documented() {
    let (_dir_guard, serving, addr) = start_server();
    let (t0, t1) = (unix_now() - 600.0, unix_now() + 5.0);
    let q = format!("f_lo=100000000&f_hi=101600000&t0={t0}&t1={t1}");

    let (status, v) = get(addr, "/api/channels?f_lo=100000000&f_hi=101600000");
    assert_eq!(status, 200, "{v}");
    assert!(v["plan_version"].is_u64() && v["scheme"].is_u64(), "{v}");
    assert!(v["f_cell_hz"].as_f64().is_some_and(|f| f > 0.0), "{v}");
    assert_eq!(v["source"], "learned-from-detections");
    for c in v["channels"].as_array().expect("channels array") {
        assert!(
            is_object(&c["key"]) && c["f_lo_hz"].is_f64() && c["obw_hz"].is_f64(),
            "{c}"
        );
    }

    for interval in ["15m", "1h", "span"] {
        let (status, v) = get(addr, &format!("/api/occupancy?{q}&interval={interval}"));
        assert_eq!(status, 200, "{interval}: {v}");
        assert_eq!(v["interval"], interval);
        assert!(is_array(&v["rows"]), "{v}");
        assert_eq!(v["truncated"], false);
        assert!(v["plan_version"].is_u64() && v["f_cell_hz"].is_f64(), "{v}");
        assert_eq!(v["coverage"]["unobserved_is_not_quiet"], true);
        assert!(v["coverage"]["rows"].is_u64() && v["coverage"]["rows_with_fco"].is_u64());
        for r in v["rows"].as_array().unwrap() {
            assert!(is_object(&r["subject"]) && !r["interval"].is_null(), "{r}");
            assert!(
                r["subject_extent"]["f_lo_hz"].is_f64() && r["n_revisits"].is_u64(),
                "{r}"
            );
            assert_eq!(r["revisit_biased"], false);
        }
    }
    let (status, v) = get(
        addr,
        &format!("/api/occupancy?{q}&subject=band&interval=span"),
    );
    assert_eq!(status, 200, "{v}");
    for r in v["rows"].as_array().unwrap() {
        assert_eq!(r["subject"]["kind"], "band", "{r}");
    }

    for bad in [
        "/api/channels?f_lo=2&f_hi=1".to_string(),
        format!("/api/occupancy?f_lo=1&t0={t0}&t1={t1}"),
        format!("/api/occupancy?{q}&interval=2h"),
        format!("/api/occupancy?{q}&subject=cell"),
        format!("/api/occupancy?f_lo=1&f_hi=2&t0={t1}&t1={t0}"),
        format!("/api/occupancy?f_lo=0&f_hi=1000000000&t0={t0}&t1={t1}&interval=span"),
    ] {
        let (status, v) = get(addr, &bad);
        assert_eq!(status, 400, "{bad}: {v}");
        assert_eq!(v["code"], "invalid", "{bad}: {v}");
    }
    let (status, _) = post(addr, &format!("/api/occupancy?{q}"), "{}");
    assert_eq!(status, 405);
    let (status, _) = post(addr, "/api/channels?f_lo=1&f_hi=2", "{}");
    assert_eq!(status, 405);
    stop_server(serving);
}
// T-119 sites, baselines, candidates, weights

/// T-119: `/api/sites[...]`, `/api/baselines[...]`, `/api/candidates` and
/// `/api/attention/weights` answer with the documented shapes: unassigned start (baselines 409),
/// a user site pinned by name, rename, empty baselines/slots/refreeze on a fresh site, an empty
/// version-0 candidate set, versioned weights (v1 defaults → v2), and 400/404/405/401 refusals.
#[test]
fn attention_sites_baselines_candidates_and_weights_answer_as_documented() {
    let (_dir_guard, serving, addr) = start_server();

    let (st, v) = get(addr, "/api/sites");
    assert_eq!(st, 200, "{v}");
    assert!(is_array(&v["sites"]), "{v}");
    assert_eq!(v["current"]["kind"], "unassigned", "{v}");
    let (st, v) = get(addr, "/api/baselines");
    assert_eq!((st, &v["code"]), (409, &json!("conflict")), "{v}");

    let (st, v) = put(
        addr,
        "/api/sites/current",
        &json!({"name": "contract-home", "utc_offset_min": 60}).to_string(),
    );
    assert_eq!(st, 200, "{v}");
    assert_eq!(
        (&v["site"]["kind"], &v["set_by"], &v["pinned"]),
        (&json!("site"), &json!("user"), &json!(true)),
        "{v}"
    );
    let id = v["record"]["id"].as_str().unwrap().to_owned();
    for key in [
        "name",
        "radius_m",
        "utc_offset_min",
        "source",
        "first_seen",
        "last_seen",
    ] {
        assert!(!v["record"][key].is_null(), "{key}: {v}");
    }
    let (st, v) = get(addr, "/api/sites/current");
    assert_eq!(
        (st, v["site"]["id"].as_str()),
        (200, Some(id.as_str())),
        "{v}"
    );
    let (st, v) = put(addr, "/api/sites/current", "{}");
    assert_eq!((st, &v["code"]), (400, &json!("invalid")), "{v}");

    let (st, v) = put(
        addr,
        &format!("/api/sites/{id}"),
        &json!({"name": "contract-home-2"}).to_string(),
    );
    assert_eq!((st, &v["name"]), (200, &json!("contract-home-2")), "{v}");
    let (st, v) = put(
        addr,
        &format!("/api/sites/{id}"),
        &json!({"x": 1}).to_string(),
    );
    assert_eq!(st, 400, "{v}");
    let (st, v) = put(
        addr,
        "/api/sites/00000000-0000-7000-8000-000000000000",
        &json!({"name": "nobody"}).to_string(),
    );
    assert_eq!((st, &v["code"]), (404, &json!("not_found")), "{v}");

    let (st, v) = get(addr, "/api/baselines");
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["site"].as_str(), Some(id.as_str()), "{v}");
    assert!(is_array(&v["baselines"]) && v["slot"].is_u64(), "{v}");
    let (st, v) = get(
        addr,
        "/api/baselines/slots?f_lo=100000000&f_hi=101000000&resolution=all-hours",
    );
    assert_eq!(st, 200, "{v}");
    assert!(
        is_array(&v["subjects"]) && v["truncated"] == json!(false),
        "{v}"
    );
    let (st, _) = get(addr, "/api/baselines/slots?f_lo=100000000");
    assert_eq!(st, 400);
    let (st, v) = post(addr, "/api/baselines/refreeze", "{}");
    assert_eq!((st, &v["refrozen"]), (200, &json!(0)), "{v}");

    let (st, v) = get(addr, "/api/candidates?limit=10");
    assert_eq!(st, 200, "{v}");
    assert!(v["version"].is_u64() && is_array(&v["candidates"]), "{v}");
    assert_eq!(v["truncated"], json!(false), "{v}");
    assert!(is_object(&v["weights"]) && !v["site"].is_null(), "{v}");
    let (st, _) = get(addr, "/api/candidates?limit=0");
    assert_eq!(st, 400);

    let (st, v) = get(addr, "/api/attention/weights");
    assert_eq!(st, 200, "{v}");
    assert_eq!(
        (&v["weights"]["version"], &v["weights"]["novelty"]),
        (&json!(1), &json!(2.0))
    );
    assert!(is_array(&v["history"]) && is_object(&v["defaults"]), "{v}");
    let w = json!({"snr": 1, "novelty": 3, "class_entropy": 1, "decoder": 0.5,
        "periodicity": 0.5, "boring": 1});
    let (st, v) = put(addr, "/api/attention/weights", &w.to_string());
    assert_eq!((st, &v["weights"]["version"]), (200, &json!(2)), "{v}");
    let (_, v) = get(addr, "/api/attention/weights");
    assert_eq!(
        (&v["weights"]["version"], &v["history"][0]["version"]),
        (&json!(2), &json!(2))
    );
    let mut bad = w.clone();
    bad["snr"] = json!(11);
    let (st, v) = put(addr, "/api/attention/weights", &bad.to_string());
    assert_eq!((st, &v["code"]), (400, &json!("invalid")), "{v}");
    let (st, _) = call(
        addr,
        "PUT",
        "/api/attention/weights",
        None,
        Some(&w.to_string()),
    );
    assert_eq!(st, 401);
    let (st, _) = delete(addr, "/api/attention/weights");
    assert_eq!(st, 405);
    stop_server(serving);
}
// T-120 scheduler
// T-127 scheduler routes

/// T-127: `/api/scheduler*` answer the documented shapes on a run without the scheduler
/// (`hk serve`): reads say `"scheduler": null`, POI rows come from the observation log when a box
/// is given (unobserved is a gap, never quiet; a bare read computes no POI), lease changes are
/// refused with 409 (bad bodies and ids with 400), other methods with 405, and no token with 401.
/// A full lease table (409 `table_full`) and a control-thread timeout (503 `busy`, cancelled)
/// need a running scheduler: hk-api's `scheduler_failures_map_to_documented_statuses` and
/// hk-pipeline's `a_lease_command_that_timed_out_never_applies` cover them.
#[test]
fn scheduler_routes_answer_as_documented_without_a_scheduler() {
    let (_dir_guard, serving, addr) = start_server();
    let bearer = format!("Bearer {TOKEN}");
    let auth = Some(bearer.as_str());

    let (st, v) = call(addr, "GET", "/api/scheduler", auth, None);
    assert_eq!(st, 200, "{v}");
    assert!(v["scheduler"].is_null(), "{v}");
    assert_eq!(v["leases"], json!([]));
    assert_eq!(v["poi"], json!([]));
    assert_eq!(v["observation_log"], json!(true));

    let (t0, t1) = (unix_now() - 60.0, unix_now());
    let path = format!("/api/scheduler?f_lo=100000000&f_hi=102000000&t0={t0}&t1={t1}&tau_s=0.1,1");
    let (st, v) = call(addr, "GET", &path, auth, None);
    assert_eq!(st, 200, "{v}");
    let rows = v["poi"].as_array().expect("poi rows");
    assert_eq!(rows.len(), 1, "{v}");
    let row = &rows[0];
    assert_eq!(row["f_lo"], 100_000_000.0);
    assert_eq!(row["cells"], 2);
    for key in [
        "observed_cells",
        "observed_fraction",
        "gap_threshold_s",
        "gaps_truncated",
    ] {
        assert!(!row[key].is_null(), "{key} in {row}");
    }
    let taus: Vec<f64> = row["poi"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            assert!(e["p_poi"].is_number() && e["p_poi_min"].is_number(), "{e}");
            e["tau_s"].as_f64().unwrap()
        })
        .collect();
    assert_eq!(taus, vec![0.1, 1.0]);
    assert!(row["gaps"].is_array());
    assert_eq!(v["poi_truncated"], json!(false));

    let (st, v) = call(addr, "GET", "/api/scheduler?f_lo=5", auth, None);
    assert_eq!(st, 400, "unpaired region refused: {v}");

    let (st, v) = call(addr, "GET", "/api/scheduler/arms", auth, None);
    assert_eq!(st, 200, "{v}");
    assert_eq!(
        (v["scheduler"].clone(), v["arms"].clone()),
        (json!(false), json!([]))
    );
    let (st, v) = call(addr, "GET", "/api/scheduler/leases", auth, None);
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["leases"], json!([]));

    let (st, v) = call(addr, "POST", "/api/scheduler/leases", auth, Some("{}"));
    assert_eq!(st, 400, "a lease needs center_hz: {v}");
    let body = json!({ "center_hz": STATION_HZ, "kind": "user-pin", "duration_s": 5 }).to_string();
    let (st, v) = call(addr, "POST", "/api/scheduler/leases", auth, Some(&body));
    assert_eq!(st, 409, "no scheduler on this run: {v}");
    let (st, v) = call(addr, "DELETE", "/api/scheduler/leases/abc", auth, None);
    assert_eq!(st, 400, "{v}");
    let (st, v) = call(addr, "DELETE", "/api/scheduler/leases/5", auth, None);
    assert_eq!(st, 409, "{v}");

    let (st, v) = call(addr, "POST", "/api/scheduler", auth, Some("{}"));
    assert_eq!(st, 405, "{v}");
    let (st, _) = call(addr, "GET", "/api/scheduler", None, None);
    assert_eq!(st, 401);
    stop_server(serving);
}
// T-121 reports

/// T-121: `/api/report` answers a `SurveyReport` (coverage and POI always disclosed, baseline
/// comparison explicitly unavailable) and backend-rendered CSV/PNG exports.
#[test]
fn report_route_serves_document_and_exports() {
    let (_dir_guard, serving, addr) = start_server();
    let now = unix_now();
    let region = format!(
        "f_lo={}&f_hi={}&t0={}&t1={}",
        FIXTURE_CENTER_HZ - FIXTURE_RATE_HZ / 2.0,
        FIXTURE_CENTER_HZ + FIXTURE_RATE_HZ / 2.0,
        now - 7.0 * 86_400.0,
        now + 60.0
    );
    let (st, v) = get(addr, &format!("/api/report?{region}"));
    assert_eq!(st, 200, "{v}");
    for field in [
        "schema",
        "generated_at",
        "region",
        "span",
        "site",
        "occupancy",
        "top_emitters",
        "change_vs_baseline",
        "coverage",
        "provenance_steps",
        "anomalies",
        "warnings",
    ] {
        assert!(v.get(field).is_some(), "report missing {field}: {v}");
    }
    let c = &v["coverage"];
    for field in [
        "observed_fraction",
        "observed_s",
        "gaps",
        "gaps_truncated",
        "never_observed",
        "poi",
        "statement",
    ] {
        assert!(c.get(field).is_some(), "coverage missing {field}: {v}");
    }
    assert_eq!(c["poi"].as_array().map(Vec::len), Some(4), "{v}");
    assert!(
        c["statement"].as_str().unwrap().contains("not quiet"),
        "{v}"
    );
    // T-128: the run attaches its baselines; a replay has no site, so there is no baseline.
    assert_eq!(v["change_vs_baseline"]["status"], "no-baseline", "{v}");
    assert!(is_array(&v["occupancy"]["channels"]) && is_array(&v["top_emitters"]));

    let (st, ct, body) = get_raw(addr, &format!("/api/report?{region}&format=csv"));
    assert_eq!((st, ct.as_str()), (200, "text/csv; charset=utf-8"));
    let text = String::from_utf8(body).unwrap();
    assert!(
        text.lines()
            .any(|l| l.starts_with("# coverage:") && l.contains("not quiet")),
        "{text}"
    );
    assert!(text.lines().any(|l| l.starts_with("row,f_lo_hz,f_hi_hz")));
    let (st, ct, body) = get_raw(addr, &format!("/api/report?{region}&format=png"));
    assert_eq!((st, ct.as_str()), (200, "image/png"));
    assert!(body.starts_with(b"\x89PNG\r\n\x1a\n"));

    // T-133: an explicit site or a source filters the report's history, and says so.
    let (st, v) = get(addr, &format!("/api/report?{region}&site=mobile"));
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["site"]["kind"], "mobile", "{v}");
    let warnings = v["warnings"].as_array().unwrap();
    assert!(
        warnings.iter().any(|w| w
            .as_str()
            .unwrap()
            .contains("history tiles filtered to source any and site mobile")),
        "{v}"
    );
    let (st, v) = get(
        addr,
        &format!("/api/report?{region}&source=unknown&format=csv"),
    );
    assert_eq!(st, 200, "{v}");
    let (st, v) = get(addr, &format!("/api/report?{region}"));
    assert_eq!(st, 200, "{v}");
    assert!(
        !v["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("history tiles filtered")),
        "no filter without site/source: {v}"
    );
    for bad in [
        format!("/api/report?{region}&format=xml"),
        format!("/api/report?{region}&site=nowhere"),
        format!("/api/report?{region}&source=nothex"),
        "/api/report?f_lo=2&f_hi=1&t0=0&t1=1".to_owned(),
        // Over the report grid budget even at the coarsest history level.
        "/api/report?f_lo=1&f_hi=1000000000000&t0=0&t1=172800".to_owned(),
    ] {
        let (st, v) = get(addr, &bad);
        assert_eq!(st, 400, "{bad}: {v}");
    }
    let (st, _) = call(
        addr,
        "POST",
        &format!("/api/report?{region}"),
        Some(&format!("Bearer {TOKEN}")),
        Some("{}"),
    );
    assert_eq!(st, 405);
    stop_server(serving);
}
// T-122 anomalies

/// T-122: `/api/anomalies[...]` answer the documented shapes (an empty list with suppression
/// counts on a fresh run), refuse bad ids, kinds and methods, and a dismiss of an unknown anomaly
/// is 404; the `anomalies` stream is offered.
#[test]
fn anomalies_routes_answer_documented_shapes() {
    let (_dir_guard, serving, addr) = start_server();
    let (st, v) = get(
        addr,
        "/api/anomalies?f_lo=1e8&f_hi=2e9&t0=0&t1=4e9&status=open&limit=5",
    );
    assert_eq!(st, 200, "{v}");
    for field in ["anomalies", "next_cursor", "truncated", "suppressions"] {
        assert!(v.get(field).is_some(), "missing {field}: {v}");
    }
    assert!(
        is_array(&v["anomalies"]) && v["suppressions"].is_object(),
        "{v}"
    );
    assert_eq!(v["truncated"], false);

    for bad in [
        "/api/anomalies?kind=loud",
        "/api/anomalies?status=maybe",
        "/api/anomalies?f_lo=2&f_hi=1",
        "/api/anomalies?f_lo=1",
        "/api/anomalies/not-a-uuid",
    ] {
        let (st, v) = get(addr, bad);
        assert_eq!(st, 400, "{bad}: {v}");
    }
    let unknown = "01890000-0000-7000-8000-000000000000";
    let (st, _) = get(addr, &format!("/api/anomalies/{unknown}"));
    assert_eq!(st, 404);
    let auth = format!("Bearer {TOKEN}");
    let (st, _) = call(
        addr,
        "POST",
        &format!("/api/anomalies/{unknown}/dismiss"),
        Some(&auth),
        Some("{}"),
    );
    assert_eq!(st, 404);
    let (st, _) = call(
        addr,
        "POST",
        &format!("/api/anomalies/{unknown}/reopen"),
        Some(&auth),
        Some(r#"{"bogus":1}"#),
    );
    assert_eq!(st, 400);
    let (st, _) = call(addr, "POST", "/api/anomalies", Some(&auth), Some("{}"));
    assert_eq!(st, 405);
    let (st, v) = get(addr, "/api/streams");
    assert_eq!(st, 200);
    assert!(
        v.to_string().contains("\"anomalies\""),
        "stream offered: {v}"
    );
    stop_server(serving);
}
