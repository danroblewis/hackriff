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
//! `/api/selections[/<id>[/links]]`, `/api/annotations[/<id>]` (T-816), `/api/outputs[...]`, `/ws/<id>` (spectrum header),
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
    start_server_retaining(None)
}

/// [`start_server`] with an explicit IQ-ring retention (T-338), so a test can reconfigure the
/// capture window and watch the timeline follow it.
fn start_server_retaining(retention_s: Option<f64>) -> (TempDataDirGuard, Serving, SocketAddr) {
    start_server_in(temp_data_dir(), retention_s)
}

/// [`start_server_retaining`] over a data directory the test has already seeded.
fn start_server_in(
    dir: PathBuf,
    retention_s: Option<f64>,
) -> (TempDataDirGuard, Serving, SocketAddr) {
    start_server_fft(dir, retention_s, 1024)
}

/// [`start_server_in`] at an explicit display FFT length — which sets the view lattice's finest
/// frequency cell, and with it how much spectrum one store block spans (T-1034: `hk serve`'s own
/// default is 4096).
fn start_server_fft(
    dir: PathBuf,
    retention_s: Option<f64>,
    fft_len: usize,
) -> (TempDataDirGuard, Serving, SocketAddr) {
    let guard = TempDataDirGuard::new(dir.clone());
    let serving = start(&ServeOptions {
        source: ServeSource::HackRf {
            spec: format!("mock:{}", fixture_path().display()),
            extra: Vec::new(),
            live: LiveArgs::default(),
        },
        data_dir: Some(dir),
        bind: "127.0.0.1:0".parse().unwrap(),
        ui_dist: None,
        fft_len,
        rows_per_s: 25.0,
        calibration: None,
        token: Some(TOKEN.into()),
        listen: Default::default(),
        compute: Default::default(),
        // T-178: the ring is allocated up front, so tests keep it small.
        iq_buffer: hk_cli::pipeline::IqBufferArgs {
            retention_s,
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

/// T-247 (ADR-0016 §2/§9): `GET /api/inventory/{id}/classification` answers as `docs/api.md`
/// documents it. The route serves the parts of a classification too large for an inventory row —
/// both distributions, the prior, the provenance and the reason codes — and, like every other
/// per-emitter read, it explains without ever naming.
#[test]
fn inventory_classification_route_answers_as_documented() {
    let (_dir_guard, serving, addr) = start_server();

    // An unparsable or unknown id is a 404 — never a 200 that could be probed for which ids exist.
    let (st, v) = get(addr, "/api/inventory/not-a-uuid/classification");
    assert_eq!((st, v["code"].as_str()), (404, Some("not_found")), "{v}");
    let unknown = EmitterId::new();
    let (st, v) = get(addr, &format!("/api/inventory/{unknown}/classification"));
    assert_eq!(st, 404, "{v}");

    // A real emitter from the fixture. Both fields may be `null` on a fresh server — nothing has
    // classified it yet, which is not an error and is not a classification of `unknown`.
    let (st, inv) = get(addr, "/api/inventory");
    assert_eq!(st, 200, "{inv}");
    if let Some(row) = inv["emitters"].as_array().and_then(|r| r.first()) {
        let id = row["id"].as_str().expect("an inventory row has an id");
        let (st, v) = get(addr, &format!("/api/inventory/{id}/classification"));
        assert_eq!(st, 200, "{v}");
        assert_eq!(v["emitter"], id, "{v}");
        for field in ["classification", "latest"] {
            assert!(
                v[field].is_null() || v[field].is_object(),
                "{field} is a Classification or null: {v}"
            );
            let Some(c) = v[field].as_object() else {
                continue;
            };
            // The full ADR-0016 §2 contract, not the inventory row's summary.
            for required in [
                "posterior",
                "likelihood",
                "family",
                "confidence",
                "open_set_score",
                "entropy_norm",
                "coarse",
                "taxonomy",
                "stage",
                "provenance",
                "flags",
                "reasons",
            ] {
                assert!(c.contains_key(required), "{field} missing {required}: {v}");
            }
            // Both distributions are served, and `unknown` is a label in them like any other.
            for dist in ["posterior", "likelihood"] {
                let labels = c[dist].as_array().unwrap_or_else(|| panic!("{dist}: {v}"));
                assert!(!labels.is_empty(), "{dist} is empty: {v}");
                for l in labels {
                    assert!(l["label"].is_string(), "{l}");
                    assert!(l["p"].is_number(), "{l}");
                }
            }
            assert!(c["provenance"]["rules"].is_string(), "{v}");
            // T-290: provenance names a feature set that exists. `1` is the pre-T-290
            // indeterminate marker (`hk_model::classify::FEATURES_VERSION_INDETERMINATE`), which
            // says only "some vector, unrecorded". This server runs on a fresh data directory, so
            // every row it serves was written by this build and must name its own vector.
            let fv = c["provenance"]["features_version"]
                .as_u64()
                .unwrap_or_else(|| panic!("features_version is an integer: {v}"));
            assert!(fv >= 1, "features_version {fv} names no feature set: {v}");
            if c["stage"] == json!("feature-tree") {
                assert!(
                    fv > 1,
                    "a feature-tree row this build wrote must not claim the indeterminate \
                     features_version 1: {v}"
                );
            }
            assert!(is_array(&c["reasons"]), "{v}");
            // A classification explains; it never names.
            for forbidden in ["identity", "known_status", "lifecycle"] {
                assert!(c.get(forbidden).is_none(), "{forbidden} leaked: {v}");
            }
        }
    }

    // GET only.
    let (st, _) = post(
        addr,
        &format!("/api/inventory/{unknown}/classification"),
        "{}",
    );
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

/// T-164 (ADR-0013 gap 7b): `GET /api/recipes/match?emitter=<id>` answers as `docs/api.md`
/// documents it. The answer is ranked *evidence about a measurement*, so it carries the
/// measurements it read, a per-field reason for every candidate, and nothing it cannot justify:
/// no candidate below the confidence floor, and no identity anywhere.
#[test]
fn recipe_match_route_answers_as_documented() {
    let (_dir_guard, serving, addr) = start_server();

    // `emitter` is required, and an unparsable or unknown id is a 404 — never a 200 that could be
    // probed for which ids exist.
    let (st, v) = get(addr, "/api/recipes/match");
    assert_eq!(st, 400, "{v}");
    assert_eq!(v["code"], "invalid", "{v}");
    let (st, v) = get(addr, "/api/recipes/match?emitter=not-an-id");
    assert_eq!(st, 404, "{v}");
    assert_eq!(v["code"], "not_found", "{v}");
    let unknown = EmitterId::new();
    let (st, v) = get(addr, &format!("/api/recipes/match?emitter={unknown}"));
    assert_eq!(st, 404, "{v}");

    // A real emitter the run detected blind.
    let (st, inv) = get(addr, "/api/inventory");
    assert_eq!(st, 200, "{inv}");
    if let Some(row) = inv["entries"].as_array().and_then(|r| r.first()) {
        let id = row["id"].as_str().expect("an inventory row has an id");
        let (st, v) = get(addr, &format!("/api/recipes/match?emitter={id}"));
        assert_eq!(st, 200, "{v}");
        assert_eq!(v["emitter"], id, "{v}");
        assert!(is_array(&v["recipes"]), "{v}");
        assert!(is_array(&v["ruled_out"]), "{v}");
        assert!(is_array(&v["reasons"]), "{v}");
        let outcome = v["outcome"].as_str().unwrap_or_default();
        assert!(["fit", "partial", "none"].contains(&outcome), "{v}");

        // The measurements the ranking read, with every documented key present. An unmeasured
        // one is null, never a fabricated default (T-163).
        let measured = v["measured"].as_object().expect("measured object");
        for k in [
            "family",
            "family_source",
            "f_center_hz",
            "bandwidth_hz",
            "bandwidth_source",
            "symbol_rate_bd",
            "bursty",
            "duty_cycle",
            "features",
        ] {
            assert!(measured.contains_key(k), "measured.{k} missing: {v}");
        }
        assert!(is_array(&v["measured"]["features"]), "{v}");

        for c in v["recipes"].as_array().unwrap() {
            assert!(c["id"].is_string(), "{c}");
            assert!(c["name"].is_string(), "{c}");
            assert!(c["score"].is_number(), "{c}");
            assert!(
                ["fit", "partial", "none"].contains(&c["outcome"].as_str().unwrap_or_default()),
                "{c}"
            );
            // Nothing below the confidence floor is ever offered.
            assert!(
                c["score"].as_f64().unwrap() >= 0.2,
                "offered below the floor: {c}"
            );
            assert!(c["band_hint"].is_boolean(), "{c}");
            assert!(is_array(&c["reasons"]), "{c}");
            for r in c["reasons"].as_array().unwrap() {
                assert!(r["field"].is_string(), "{r}");
                assert!(r["detail"].is_string(), "{r}");
                assert!(
                    ["agree", "near", "conflict", "unmeasured"]
                        .contains(&r["verdict"].as_str().unwrap_or_default()),
                    "{r}"
                );
            }
            // A recipe suggestion explains; it never names anything.
            for forbidden in ["identity", "identity_value", "known_status", "lifecycle"] {
                assert!(c.get(forbidden).is_none(), "{forbidden} leaked: {c}");
            }
        }
        for r in v["ruled_out"].as_array().unwrap() {
            assert!(r["id"].is_string(), "{r}");
            assert!(r["reason"].is_string(), "{r}");
            assert!(r["detail"].is_string(), "{r}");
        }
    }

    // GET only.
    let (st, _) = post(addr, "/api/recipes/match", "{}");
    assert_eq!(st, 405);
    let (st, _) = put(addr, "/api/recipes/match", "{}");
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
    for want in ["listen", "bits", "symbols", "playback"] {
        assert!(names.contains(&want), "on_demand openers: {names:?}");
    }
    // T-874: Listen advertises its one opt-in parameter besides the target; still never a mode.
    let listen = v["on_demand"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["name"] == "listen")
        .unwrap();
    assert_eq!(
        listen["params"],
        json!(["emitter", "detection", "f_lo", "f_hi", "channels"]),
        "{listen}"
    );
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

    // T-388: the `presence` stream is offered beside the spectrum stream — the push that lets a live
    // signal's box top track the live edge instead of waiting for the 5 s `/api/inventory` poll
    // (docs/stream-contract.md §15, ADR-0004). Timing metadata, so `messages` and unrestricted, and
    // remote-permitted or the browser could not subscribe at all.
    wait_for(
        "the presence stream to be offered",
        Duration::from_secs(30),
        || {
            get(addr, "/api/streams").1["streams"]
                .as_array()
                .is_some_and(|a| a.iter().any(|s| s["stream_id"] == "presence"))
        },
    );
    let (_, v) = get(addr, "/api/streams");
    let presence = v["streams"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["stream_id"] == "presence")
        .unwrap();
    assert_eq!(presence["kind"], "messages", "{presence}");
    assert_eq!(presence["content_class"], "unrestricted", "{presence}");
    assert_eq!(presence["remote_permitted"], true, "{presence}");
    assert_eq!(presence["ws_path"], "/ws/presence", "{presence}");
    // A timing stream has no geometry: an extension is new time, not new frequency (T-362).
    for absent in ["center_hz", "bandwidth_hz", "fft_size", "datatype"] {
        assert!(
            presence[absent].is_null(),
            "presence.{absent} should be null: {presence}"
        );
    }

    // T-531: the listing is what this server is **offering now**, and it is bounded. A stream is
    // offered until its publisher has finished, nothing is attached to it and no new publisher has
    // been offered under that id for `hk_api::FINISHED_LINGER`; then the id is withdrawn. Before
    // that rule a sweep's per-emitter `bits/fsk-bursts/<emitter>` streams accumulated for the life
    // of the run (1299 entries / 659 KB in 40 minutes). The bound is asserted as a value, and it
    // is a bound on the registry, not a cap applied to the response.
    assert_eq!(hk_api::MAX_STREAMS, 256);
    assert_eq!(hk_api::FINISHED_LINGER, hk_api::bridge::CARRY_OVER_GRACE);
    let (_, v) = get(addr, "/api/streams");
    let offered = v["streams"].as_array().unwrap();
    assert!(
        offered.len() <= hk_api::MAX_STREAMS,
        "the discovery document is unbounded: {} streams",
        offered.len()
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
    // T-325: bias-tee state is a three-state field, not a bool, and it reports what the source
    // actually said — the mock has a bias tee and was opened with it off. Asserting the value,
    // not just that some field is present.
    assert_eq!(
        v["device"]["bias_tee"],
        json!(true),
        "the mock device has a bias tee: {v}"
    );
    assert_eq!(v["tuning"]["bias_tee"], json!("off"), "{v}");
    // T-343: the state names the front end a retune would move, from the source's own identity —
    // the mock SDR reports `mock:<recorded device_id>`. A client can say which radio it is about to
    // change before it asks, rather than discovering it from the audit log afterwards.
    let device_id = v["device"]["device_id"]
        .as_str()
        .unwrap_or_else(|| panic!("the live source must report its device_id: {v}"));
    assert!(device_id.starts_with("mock:"), "{v}");
    assert!(is_object(&v["run"]), "{v}");
    // T-508: whether the front end is delivering is STATED, never left to be inferred from
    // `finished` (which could not tell a device failure from a recording's end, nor a run
    // restarting capture from one that is running).
    assert_eq!(v["run"]["capture"], json!("running"), "{v}");
    assert_eq!(v["run"]["capture_note"], Value::Null, "{v}");
    assert_eq!(v["run"]["finished"], json!(false), "{v}");
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
    let before = unix_now();
    let (st, v) = get(addr, "/api/status");
    let after = unix_now();
    assert_eq!(st, 200, "{v}");
    assert!(is_object(&v), "{v}");
    // T-351: `t` is the server's own wall clock at the instant this response was built, bare-named
    // Unix seconds — asserted by VALUE against the test's own clock, bracketing the request, not
    // merely that the field is present (T-315's point: a shape check would not catch a stale or
    // frozen clock).
    let t = v["t"].as_f64().expect("t (server clock, s): {v}");
    assert!((before - 1.0..=after + 1.0).contains(&t), "t={t}: {v}");
    // T-572: the hot-tile cache's bound and its eviction, reported HERE — not in a tile body,
    // where a per-read counter would change a sealed tile's ETag and turn T-574's 304 back into a
    // 200. The BOUND is the assertion, not a rate: entries and bytes both inside the cap the same
    // object states.
    let tc = &v["tile_cache"];
    assert!(is_object(tc), "tile_cache: {v}");
    for field in [
        "entries",
        "bytes",
        "max_entries",
        "max_bytes",
        "hits",
        "misses",
        "evictions",
        "invalidations",
    ] {
        assert!(tc[field].is_u64(), "tile_cache.{field}: {tc}");
    }
    // T-1020: sized in viewports (256 MiB / 600 entries), past the 135-290 tiles/screen the
    // tile-latency review measured against the old 32 MiB / ~35-tile bound.
    assert_eq!(tc["max_entries"], json!(600), "{tc}");
    assert_eq!(tc["max_bytes"], json!(256 * 1024 * 1024), "{tc}");
    assert!(tc["entries"].as_u64().unwrap() <= 600, "{tc}");
    assert!(tc["bytes"].as_u64().unwrap() <= 256 * 1024 * 1024, "{tc}");
    // T-1020: per-client hit/miss, so a pan-back over one pane's own viewport is measurable.
    assert!(is_object(&tc["by_client"]), "tile_cache.by_client: {tc}");

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

    // T-904: `storage` — the detection store's size and the retention policy in force, refreshed
    // by the run's retention thread (its first refresh is at start-up, so wait for it rather than
    // race it). The composed daemon prunes by default: 1 hour, rolled up, the full 256-row tail.
    wait_for(
        "the storage figures in /api/status",
        Duration::from_secs(30),
        || get(addr, "/api/status").1["storage"]["measured_s"].is_f64(),
    );
    let (_, v) = get(addr, "/api/status");
    let storage = &v["storage"];
    for field in [
        "db_bytes",
        "free_bytes",
        "detection_rows",
        "rollup_rows",
        "passes",
    ] {
        assert!(storage[field].is_u64(), "storage.{field}: {storage}");
    }
    assert!(storage["db_bytes"].as_u64().unwrap() > 0, "{storage}");
    assert!(
        storage["wal_bytes"].is_u64(),
        "a file database in WAL mode has a -wal file: {storage}"
    );
    // T-913: the row counts carry the time they were taken; the sizes beside them are current.
    assert!(
        storage["detection_rows_counted_s"].is_f64(),
        "storage.detection_rows_counted_s: {storage}"
    );
    for field in ["oldest_detection_s", "newest_detection_s"] {
        assert!(
            storage[field].is_null() || storage[field].is_f64(),
            "storage.{field}: {storage}"
        );
    }
    let measured = storage["measured_s"].as_f64().unwrap();
    assert!(
        (before - 60.0..=unix_now() + 1.0).contains(&measured),
        "{storage}"
    );
    // Pruning is on, so a next pass is always scheduled, never in the past of the snapshot.
    // How far ahead depends on whether the first pass (one minute in) has run yet, which is the
    // wall clock's business, not this test's: the server's own `last_prune` says which.
    let next = storage["next_prune_s"]
        .as_f64()
        .expect("next_prune_s: {storage}");
    assert!(next >= measured - 1.0, "{storage}");
    assert_eq!(
        storage["retention"],
        json!({
            "enabled": true,
            "max_age_s": 3_600.0,
            "min_age_s": 600.0,
            "clamped_from_s": null,
            "rollup": true,
            "keep_per_emitter": 256,
            "batch": 100,
            "interval_s": 600.0,
            "count_rows_s": 600.0,
            "rollup_gap_s": 10.0,
            "rollup_span_s": 60.0,
        }),
        "{storage}"
    );
    // Either no pass has run yet (`null`, and the first is due within its one-minute delay of
    // the snapshot), or one has and reports itself whole; which, is read off the server's report.
    let last = &storage["last_prune"];
    if last.is_null() {
        assert_eq!(storage["passes"], json!(0), "{storage}");
        assert!(next <= measured + 61.0, "the first pass is due: {storage}");
    } else {
        assert!(
            storage["passes"].as_u64().is_some_and(|n| n >= 1),
            "{storage}"
        );
        assert!(last["error"].is_null(), "{storage}");
        for field in ["t_s", "duration_s", "lock_ms_max", "wait_ms_max"] {
            assert!(last[field].is_f64(), "last_prune.{field}: {storage}");
        }
        for field in ["examined", "deleted", "batches"] {
            assert!(last[field].is_u64(), "last_prune.{field}: {storage}");
        }
        assert!(last["complete"].is_boolean(), "{storage}");
        // The next pass is one interval (600 s) after the last.
        assert!(next <= measured + 601.0, "{storage}");
    }

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

    // T-334: span-matched resolution. These assert the `resolution` field's VALUES against the
    // grid actually served — a field that could be renamed or dropped without failing here would
    // be documentation, not a contract (T-315).
    let band = format!(
        "f_lo={}&f_hi={}",
        FIXTURE_CENTER_HZ - FIXTURE_RATE_HZ / 2.0,
        FIXTURE_CENTER_HZ + FIXTURE_RATE_HZ / 2.0
    );
    let now = unix_now();
    // (a) A budget the ladder can meet: the block agrees with the grid, and reports `matched`.
    let (st, v) = get(
        addr,
        &format!(
            "/api/history?{band}&t0={}&t1={now}&max_t=600&max_f=1024",
            now - 60.0
        ),
    );
    assert_eq!(st, 200, "{v}");
    let res = &v["resolution"];
    // T-341: `source` is a detail claim, not a constant. This span fits one capture window, so the
    // grid is spectrum history — reduced from real frames, never live IQ (this route reads the
    // pyramid and only the pyramid), and never interpolated.
    assert_eq!(res["source"], json!("spectrum-history"), "{v}");
    assert_eq!(res["live"], json!(false), "{v}");
    assert_eq!(res["max_live_span_hz"], json!(20e6), "{v}");
    assert_eq!(
        res["served_span_hz"].as_f64(),
        Some(v["nf"].as_f64().unwrap() * v["f_cell_hz"].as_f64().unwrap()),
        "{v}"
    );
    assert_eq!(res["requested"]["max_t"], json!(600), "{v}");
    assert_eq!(res["requested"]["max_f"], json!(1024), "{v}");
    assert_eq!(res["requested"]["max_cells"], json!(100_000), "{v}");
    assert_eq!(res["served"]["nt"], v["nt"], "{v}");
    assert_eq!(res["served"]["nf"], v["nf"], "{v}");
    assert_eq!(
        res["served"]["cells"].as_u64(),
        Some(v["nt"].as_u64().unwrap() * v["nf"].as_u64().unwrap()),
        "{v}"
    );
    assert_eq!(res["level"], v["level"], "{v}");
    assert_eq!(res["t_cell_s"], v["t_cell_s"], "{v}");
    assert_eq!(res["f_cell_hz"], v["f_cell_hz"], "{v}");
    assert!(res["levels"].as_u64().is_some_and(|n| n >= 1), "{v}");
    assert_eq!(res["matched"], json!(true), "{v}");
    assert_eq!(res["over_resolved"], json!([]), "{v}");
    let (nt_zoomed, t_cell_zoomed) = (v["nt"].as_u64().unwrap(), v["t_cell_s"].as_f64().unwrap());
    assert!(nt_zoomed <= 600, "served more rows than max_t: {v}");
    // The whole requested span is covered — zooming re-scales, it never truncates (invariant 4).
    let t0_s = v["t0_s"].as_f64().unwrap();
    assert!(t0_s <= now - 60.0 + 1e-6, "span start clipped: {v}");
    assert!(
        t0_s + nt_zoomed as f64 * t_cell_zoomed >= now - 1e-6,
        "span end clipped: {v}"
    );

    // (b) The same view budget over a 24 h span: a coarser level, so the span re-scales into the
    // same number of rows instead of being cut short.
    let (st, wide) = get(
        addr,
        &format!(
            "/api/history?{band}&t0={}&t1={now}&max_t=600",
            now - 86_400.0
        ),
    );
    assert_eq!(st, 200, "{wide}");
    assert_eq!(wide["resolution"]["matched"], json!(true), "{wide}");
    assert!(wide["nt"].as_u64().unwrap() <= 600, "{wide}");
    let t_cell_wide = wide["t_cell_s"].as_f64().unwrap();
    assert!(
        t_cell_wide > t_cell_zoomed,
        "a 24 h span must be served coarser than a 60 s one ({t_cell_wide} vs {t_cell_zoomed})"
    );
    let t0_wide = wide["t0_s"].as_f64().unwrap();
    assert!(
        t0_wide <= now - 86_400.0 + 1e-6,
        "wide span start clipped: {wide}"
    );
    assert!(
        t0_wide + wide["nt"].as_u64().unwrap() as f64 * t_cell_wide >= now - 1e-6,
        "wide span end clipped: {wide}"
    );

    // (c) A budget no level can meet (the coarsest cell is 100 kHz, so 100 MHz cannot become one
    // column): the shortfall is named, never hidden by reducing in the client.
    let (st, over) = get(
        addr,
        &format!(
            "/api/history?f_lo=100000000&f_hi=200000000&t0={}&t1={now}&max_f=1",
            now - 3600.0
        ),
    );
    assert_eq!(st, 200, "{over}");
    // T-341: 100 MHz never fitted one 20 MHz window, so this picture was stitched from separate
    // dwells. It says so rather than looking like a 100 MHz observation.
    assert_eq!(
        over["resolution"]["source"],
        json!("survey-overview"),
        "{over}"
    );
    assert_eq!(over["resolution"]["live"], json!(false), "{over}");
    assert!(
        over["resolution"]["served_span_hz"].as_f64().unwrap()
            > over["resolution"]["max_live_span_hz"].as_f64().unwrap(),
        "{over}"
    );
    assert_eq!(over["resolution"]["matched"], json!(false), "{over}");
    assert_eq!(
        over["resolution"]["over_resolved"],
        json!(["max_f"]),
        "{over}"
    );
    assert!(over["nf"].as_u64().unwrap() > 1, "{over}");
    assert_eq!(
        over["resolution"]["requested"]["max_t"],
        Value::Null,
        "{over}"
    );

    // (d) Bad per-axis budgets are refused, not clamped.
    for bad in ["max_t=0", "max_f=-1", "max_t=abc", "max_f=500001"] {
        let (st, _) = get(addr, &format!("/api/history?{band}&t0=0&t1={t1}&{bad}"));
        assert_eq!(st, 400, "expected 400 for {bad}");
    }

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
        // T-350: the same measurement with the time it was measured over (present, possibly null).
        "measured",
        // T-219: why this row defers to another, when it does (present, possibly null).
        "relation",
        // T-211: arbitrated classification and a differing latest row (present, possibly null).
        "classification",
        "latest_classification",
        // T-202: the C18 cluster of unknowns this row belongs to (present, possibly null).
        "cluster_id",
        // T-320: that membership as grouping data (present, possibly null).
        "cluster_group",
        // T-593: why `cluster_id` reads what it does (present; null only on a withheld row).
        "cluster_status",
        // T-284 (ADR-0017 TM-2): when this row was on the air, through the request's window.
        "presence",
        // T-860 (ADR-0015 §5.4): the latest analysis, summarised (present; null = not searched).
        "synthesis",
        // T-860 (ADR-0015 §5.5): the identity rests only on synthesized decodes (present, possibly
        // null).
        "identity_synthesized",
        // T-566 (ADR-0021 §7A.4): the decode-side resolution — never absent, and `not-searched`
        // rather than `null` on a row nothing has analysed.
        "resolution",
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
    // T-350 (ADR-0017): `snr_db`/`peak_dbfs` are a *detection's* numbers, and a detection is a
    // time-frequency region rather than a persistent carrier - so the row has to say WHEN they
    // were measured, or an SNR from two days ago is indistinguishable from one from two seconds
    // ago on a list whose whole question is what is on the air now. Asserted by VALUE (T-315): a
    // shape check passes on a `measured` block filled with the request's own window, which is
    // precisely the guess-dressed-as-a-measurement this exists to prevent.
    {
        let m = &row["measured"];
        // Null exactly with the levels: all five values come from one detection, so there is no
        // state in which a time exists without its levels, or levels without their time.
        assert_eq!(
            m.is_null(),
            row["snr_db"].is_null(),
            "`measured` is the dated form of snr_db/peak_dbfs, so they are null together: {row}"
        );
        if !m.is_null() {
            // The levels are the SAME measurement, not a second one taken elsewhere.
            assert_eq!(m["snr_db"], row["snr_db"], "measured.snr_db differs: {row}");
            assert_eq!(
                m["peak_dbfs"], row["peak_dbfs"],
                "measured.peak_dbfs differs: {row}"
            );
            let (t0, t1) = (
                m["t_start_s"].as_f64().expect("t_start_s"),
                m["t_end_s"].as_f64().expect("t_end_s"),
            );
            let dur = m["duration_s"].as_f64().expect("duration_s");
            // Unix SECONDS, per this API's units law (T-349): the band the sweep in
            // `every_serialized_time_declares_its_unit` holds every `_s` field to, asserted here
            // against this row's own clock rather than only against the magnitude.
            let (first, last) = (
                row["first_seen_s"].as_f64().expect("first_seen_s"),
                row["last_seen_s"].as_f64().expect("last_seen_s"),
            );
            assert!(t1 >= t0, "measured extent runs backwards: {m}");
            assert!(
                (dur - (t1 - t0)).abs() < 1e-6,
                "duration_s must be t_end_s - t_start_s: {m}"
            );
            // It is the DETECTION's extent, so it lies inside the emitter's first/last-seen hull
            // and is never the hull itself: serving the hull (or the request window) would report
            // a span over which nothing was measured. A tolerance of 1 s absorbs the detection's
            // own frame quantisation against the hull's endpoints.
            assert!(
                t0 >= first - 1.0 && t1 <= last + 1.0,
                "measured extent {t0}..{t1} is outside the row's seen hull {first}..{last}: {row}"
            );
            // A real detection is short - seconds, not the run - and never longer than the hull
            // it sits in. This is the honesty claim the block exists for: the levels are dated to
            // when they were taken, so a client can tell a live number from a stale one.
            assert!(
                dur <= (last - first) + 1.0,
                "measured extent {dur}s is wider than the row's whole seen hull: {row}"
            );
            assert!(
                (0.0..300.0).contains(&dur),
                "a detection's extent should be seconds, not a whole survey: {m}"
            );
        }
    }
    assert!(
        matches!(row["state"].as_str(), Some("candidate" | "confirmed")),
        "default listing excludes deleted entries: {row}"
    );
    // T-320: `cluster_group` is grouping *data*, so the contract is about its values, not its
    // keys (T-315: a shape-only assertion satisfies the letter of T-079 and not its point). Every
    // claim below is re-derived from the served page here, independently of the server:
    //   - it is null exactly when `cluster_id` is, so it can never reveal a withheld membership;
    //   - rows in the same cluster carry the *same* id and the *same* label on the wire;
    //   - rows in different clusters carry *different* labels — without which the property is
    //     satisfiable by emitting one constant;
    //   - `rows_in_view` is the count of same-cluster rows on this page, not a cluster's total.
    {
        use std::collections::HashMap;
        let rows = v["entries"].as_array().unwrap();
        let mut counted: HashMap<&str, usize> = HashMap::new();
        for r in rows {
            if let Some(id) = r["cluster_id"].as_str() {
                *counted.entry(id).or_default() += 1;
            }
        }
        let mut label_of: HashMap<&str, &str> = HashMap::new();
        for r in rows {
            let g = &r["cluster_group"];
            assert_eq!(
                g.is_null(),
                r["cluster_id"].is_null(),
                "cluster_group is null exactly when cluster_id is: {r}"
            );
            let Some(id) = r["cluster_id"].as_str() else {
                continue;
            };
            assert_eq!(g["cluster_id"].as_str(), Some(id), "{r}");
            let label = g["label"].as_str().expect("label is a string");
            assert!(!label.is_empty() && label.len() <= 6, "label {label}: {r}");
            assert_eq!(
                g["rows_in_view"].as_u64(),
                Some(counted[id] as u64),
                "rows_in_view counts same-cluster rows on this page: {r}"
            );
            // The property: one cluster reads one label, on every row that shares it.
            let seen = label_of.entry(id).or_insert(label);
            assert_eq!(*seen, label, "one cluster, two labels: {r}");
        }
        // The control: two rows that are not in the same cluster do not read as one group.
        // Without it the property above is satisfiable by emitting a single constant.
        let mut labels: Vec<&str> = label_of.values().copied().collect();
        labels.sort_unstable();
        let distinct = labels.len();
        labels.dedup();
        assert_eq!(
            labels.len(),
            distinct,
            "distinct clusters must read distinct labels: {label_of:?}"
        );
        // T-593: `cluster_status` explains a null `cluster_id` instead of leaving it silent. Null
        // only on a withheld row (as `cluster_id` is); otherwise one of four states, `clustered`
        // exactly when an id is served, and a reason exactly when the clusterer decided something.
        for r in rows {
            let s = &r["cluster_status"];
            if r["withheld"] == true {
                assert!(s.is_null(), "a withheld row explains nothing: {r}");
                continue;
            }
            let state = s["state"]
                .as_str()
                .unwrap_or_else(|| panic!("cluster_status.state: {r}"));
            assert!(
                matches!(state, "clustered" | "pending" | "abstained" | "unassessed"),
                "{r}"
            );
            assert_eq!(state == "clustered", r["cluster_id"].is_string(), "{r}");
            assert_eq!(state == "unassessed", s["reason"].is_null(), "{r}");
            assert_eq!(state == "unassessed", s["t_s"].is_null(), "{r}");
            for key in ["reason", "distance", "t_s"] {
                assert!(s.get(key).is_some(), "cluster_status missing {key}: {r}");
            }
        }
    }
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
    // T-284 (ADR-0017 TM-2/§2.3): the presence object says *when* this emitter was on the air.
    // Every field is derived from presence-interval boundaries and none from the lifetime
    // `count`, so the contract pins the shape and the two invariants that make liveness readable:
    // `ended_t_s` is set exactly when the row reads `ended`, and the latest in-window interval is
    // open exactly when it reads `live`.
    for field in [
        "intervals",
        "on_air_s",
        "last_interval",
        "liveness",
        "ended_t_s",
        "silence_s",
        "confidence",
    ] {
        assert!(
            row["presence"].get(field).is_some(),
            "presence missing {field}: {row}"
        );
    }
    let liveness = row["presence"]["liveness"].as_str();
    assert!(
        matches!(liveness, Some("live" | "ended" | "absent")),
        "liveness is live/ended/absent: {row}"
    );
    assert_eq!(
        liveness == Some("ended"),
        row["presence"]["ended_t_s"].is_number(),
        "ended_t_s is set exactly when the row reads ended: {row}"
    );
    assert!(
        row["presence"]["on_air_s"]
            .as_f64()
            .is_some_and(|s| s >= 0.0),
        "on_air_s is in-window time on air: {row}"
    );
    assert!(
        row["presence"]["intervals"].as_u64().is_some(),
        "intervals counts the intervals intersecting the window: {row}"
    );
    // T-251 (ADR-0017 TM-6): `confidence` is what ranks a candidate that stopped *inside* the
    // window below one transmitting now — the case window-scoping cannot answer, because
    // `on_air_s` is blind to *when* inside the window the signal was on. It is a rank, never a
    // lifetime: it removes no row, and a live row is fully confident by definition.
    let confidence = row["presence"]["confidence"].as_f64();
    assert!(
        confidence.is_some_and(|c| (0.0..=1.0).contains(&c)),
        "confidence is a 0-1 rank: {row}"
    );
    assert_eq!(
        liveness == Some("live"),
        confidence == Some(1.0),
        "a live row is fully confident, and only a live row is: {row}"
    );
    if liveness == Some("absent") {
        assert_eq!(
            confidence,
            Some(0.0),
            "an absent row has no in-window hypothesis to rank: {row}"
        );
    }
    assert_eq!(
        row["presence"]["silence_s"].is_null(),
        liveness == Some("absent"),
        "silence_s is the time since the latest in-window interval ended: {row}"
    );
    if liveness == Some("absent") {
        assert_eq!(row["presence"]["intervals"], json!(0), "{row}");
        assert!(row["presence"]["last_interval"].is_null(), "{row}");
    } else {
        // `revoked_s` (T-413, ADR-0019 §6.2): measured silence inside the interval whose detected
        // end a resumption revoked. Always served, so a client can never confuse "no revoked gap"
        // with "this backend does not say".
        for field in ["t_start_s", "t_end_s", "revoked_s"] {
            assert!(
                row["presence"]["last_interval"][field].is_number(),
                "last_interval missing {field}: {row}"
            );
        }
        assert!(
            row["presence"]["last_interval"]["revoked_s"]
                .as_f64()
                .unwrap()
                >= 0.0,
            "revoked silence is a duration: {row}"
        );
        assert_eq!(
            row["presence"]["last_interval"]["open"].as_bool(),
            Some(liveness == Some("live")),
            "the latest in-window interval is open exactly when the row reads live: {row}"
        );
    }
    // §7.1: this listing sent no window, so the window question is not answered at all — which is
    // what keeps a `null` answer distinguishable from an absent one.
    assert!(
        row.get("family_in_window").is_none(),
        "an unwindowed query never answers a window question: {row}"
    );

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
    // T-566 (ADR-0021 §7A.4): every row carries its decode-side `resolution`, and nothing on this
    // run has been analysed, so every row reads `not-searched` — the un-looked-at state, with no
    // time and no profile, which is never `null` and never `unknown`. The filter takes exactly the
    // four kinds, and an unknown value is `400 invalid` rather than a filter that quietly matches
    // everything.
    let (st, none_searched) = get(addr, "/api/inventory?resolution=not-searched&limit=500");
    assert_eq!(st, 200, "{none_searched}");
    let (st, listed) = get(addr, "/api/inventory?limit=500");
    assert_eq!(st, 200, "{listed}");
    assert_eq!(
        none_searched["total"], listed["total"],
        "nothing here has been analysed: {none_searched}"
    );
    for row in listed["entries"].as_array().expect("entries") {
        let r = &row["resolution"];
        assert_eq!(r["kind"], json!("not-searched"), "{row}");
        assert_eq!(r["reason"], Value::Null, "{row}");
        assert_eq!(r["t"], Value::Null, "{row}");
        assert_eq!(r["profile"], Value::Null, "{row}");
        assert_eq!(r["withheld"], json!(false), "{row}");
    }
    for kind in [
        "unknown",
        "structured-unidentified",
        "unsupported-structure",
    ] {
        let (st, v) = get(addr, &format!("/api/inventory?resolution={kind}"));
        assert_eq!(st, 200, "{v}");
        assert_eq!(v["total"], json!(0), "{kind}: {v}");
    }
    for bad in ["solved", "energy", "notsearched", "not_searched"] {
        let (st, v) = get(addr, &format!("/api/inventory?resolution={bad}"));
        assert_eq!(st, 400, "{bad} must be refused, never ignored: {v}");
    }
    // T-369: and what is left is never two boxes drawn on top of each other. Overlap in time
    // *and* frequency is an error signal, not a display choice: the served list is what the
    // waterfall lays its boxes out from (`f_lo_hz`, `f_hi_hz`, `presence.last_interval`), so two
    // stacked rows here are two stacked boxes on screen. The region re-analysis either collapses
    // them or records why it could not; either way this list does not serve the pair.
    let (st, shown) = get(addr, "/api/inventory?limit=500");
    assert_eq!(st, 200, "{shown}");
    let rows = shown["entries"].as_array().expect("entries");
    let extent = |r: &Value| {
        let p = &r["presence"]["last_interval"];
        let (t0, t1) = match (p["t_start_s"].as_f64(), p["t_end_s"].as_f64()) {
            (Some(a), Some(b)) => (a, b),
            _ => (
                r["first_seen_s"].as_f64().unwrap_or(f64::NAN),
                r["last_seen_s"].as_f64().unwrap_or(f64::NAN),
            ),
        };
        (
            r["f_lo_hz"].as_f64().unwrap_or(f64::NAN),
            r["f_hi_hz"].as_f64().unwrap_or(f64::NAN),
            t0,
            t1,
        )
    };
    for (i, a) in rows.iter().enumerate() {
        for b in rows.iter().skip(i + 1) {
            let ((alo, ahi, at0, at1), (blo, bhi, bt0, bt1)) = (extent(a), extent(b));
            assert!(
                !(alo.max(blo) < ahi.min(bhi) && at0.max(bt0) <= at1.min(bt1)),
                "two boxes served stacked in time and frequency: {a} vs {b}"
            );
        }
    }
    // T-250 (ADR-0017 §2.1): `t0`/`t1` are accepted together and select on presence-interval
    // overlap, so a window in which nothing was ever on the air lists nothing — however wide the
    // rows' first-seen/last-seen hulls are.
    let (st, v) = get(addr, "/api/inventory?t0=0&t1=1");
    assert_eq!(st, 200, "{v}");
    assert_eq!(
        v["total"].as_u64(),
        Some(0),
        "nothing was on the air in 1970: {v}"
    );
    let (st, v) = get(addr, "/api/inventory?t0=5&t1=1");
    assert_eq!(st, 400, "t1 must not precede t0: {v}");
    // T-260 (ADR-0017 §2.2) — the backend half of Explore's safety valve. The window is a *caller*
    // choice: a query with no `t0`/`t1` is not time-filtered at all. Explore sends the window on
    // the Candidate list only, and this is precisely what keeps a quiet Confirmed station listed
    // while it is off the air, instead of vanishing the moment it stops transmitting.
    let (st, windowed) = get(addr, "/api/inventory?state=confirmed&t0=0&t1=1");
    assert_eq!(st, 200, "{windowed}");
    assert_eq!(
        windowed["total"].as_u64(),
        Some(0),
        "no confirmed row was on the air in 1970: {windowed}"
    );
    let (st, unwindowed) = get(addr, "/api/inventory?state=confirmed");
    assert_eq!(st, 200, "{unwindowed}");
    assert!(
        unwindowed["total"].as_u64() >= windowed["total"].as_u64(),
        "an unwindowed Confirmed query never lists fewer rows than a windowed one: {unwindowed}"
    );
    // T-284 (ADR-0017 §7.1) — the additive family projection. `family` stays the all-time
    // arbitration, because identity evidence is time-invariant: a CRC-valid decode from yesterday
    // still says what the thing is. `family_in_window` re-runs the *same* ladder over only the
    // classification rows inside the window, and is `null` when the window re-evidenced nothing —
    // which is what lets a view-scoped client show `family` marked "(from earlier)" instead of
    // silently asserting a stale classification. The key appears only when a window was asked
    // about, so `null` and absent stay different answers.
    let t1 = unix_now();
    let (st, recent) = get(addr, &format!("/api/inventory?t0={}&t1={t1}", t1 - 3600.0));
    assert_eq!(st, 200, "{recent}");
    let rows = recent["entries"].as_array().expect("entries");
    assert!(!rows.is_empty(), "the station is on the air now: {recent}");
    for r in rows {
        assert!(
            r.get("family_in_window").is_some(),
            "a windowed query always answers the window question: {r}"
        );
        assert!(
            r["family_in_window"].is_null() || r["family_in_window"].is_string(),
            "family_in_window is a family or null: {r}"
        );
        assert!(
            r["family"].is_null() || r["family"].is_string(),
            "`family` is untouched and still all-time: {r}"
        );
    }
    assert!(
        rows.iter()
            .any(|r| r["presence"]["liveness"] != json!("absent")),
        "something was on the air in the last hour: {recent}"
    );
    // T-263 (ADR-0017 TM-7) — scrubbing back must not empty the Confirmed catalogue, and must not
    // let it read the liveness it has *now*. `t0`/`t1` do two things at once (select rows, scope
    // the projections); `at` supplies only the second, so the same rows are listed and their
    // presence is re-derived against the caller's own instant. Windowing Confirmed instead is
    // precisely the §2.2 regression: the quiet stations would vanish.
    let (st, scrubbed) = get(addr, "/api/inventory?state=confirmed&at=1");
    assert_eq!(st, 200, "{scrubbed}");
    assert_eq!(
        scrubbed["total"], unwindowed["total"],
        "`at` scopes the projection and selects nothing: the same rows stay listed: {scrubbed}"
    );
    for r in scrubbed["entries"].as_array().expect("entries") {
        assert_eq!(
            r["presence"]["liveness"],
            json!("absent"),
            "nothing had been on the air by 1970, however live the row is now: {r}"
        );
        assert_eq!(r["presence"]["intervals"], json!(0), "{r}");
        assert!(
            r["presence"]["last_interval"].is_null(),
            "no interval is fabricated for a window that precedes all of them: {r}"
        );
        assert!(
            r.get("family_in_window").is_none(),
            "naming a live edge is not asking about a window: {r}"
        );
    }
    // Refused, not silently ignored: a window's `t1` is already the caller's live edge, so a
    // request giving both asks two questions at once.
    let (st, v) = get(addr, &format!("/api/inventory?t0=0&t1={t1}&at={t1}"));
    assert_eq!(st, 400, "at beside t0/t1 is refused: {v}");
    let (st, v) = get(addr, "/api/inventory?at=nonsense");
    assert_eq!(st, 400, "at must be a finite Unix second: {v}");

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
    // T-342: `max_db` says what statistic it is and what it is relative to, in the response rather
    // than only in the docs. "Strongest in a band" is a max-hold over a window — the same fold the
    // band-collapsed series on `/api/timeline` carries — and a consumer must not have to infer
    // either that or the scale.
    assert_eq!(v["semantics"]["statistic"], json!("max-hold"), "{v}");
    assert_eq!(v["semantics"]["scale"], json!("dbfs-per-hz"), "{v}");
    assert!(
        v["semantics"]["rule"]
            .as_str()
            .is_some_and(|s| s.contains("the max of nothing is unobserved, not zero")),
        "{v}"
    );
    // T-337: the box carries absolute capture time, checked by value — a client places it from the
    // response, never from its own request or from when the reply arrived. The answer's own time
    // extent is the cell the peak was measured in, and it must lie inside the window searched.
    let num = |v: &Value, k: &str| {
        v[k].as_f64()
            .unwrap_or_else(|| panic!("strongest missing a numeric {k}: {v}"))
    };
    let (w0, w1) = (num(&v["window"], "t0_s"), num(&v["window"], "t1_s"));
    let (ts, te, dur, cell) = (
        num(&v, "t_start_s"),
        num(&v, "t_end_s"),
        num(&v, "duration_s"),
        num(&v, "t_cell_s"),
    );
    assert!(
        w1 - w0 > 4.9 && w1 - w0 < 5.1,
        "the default 5 s window, reported as searched: {v}"
    );
    assert!(
        w0 > 1.7e9 && w1 > w0,
        "absolute Unix seconds, not an offset or a counter: {v}"
    );
    assert!(
        te > ts && (te - ts - dur).abs() < 1e-9 && (dur - cell).abs() < 1e-9,
        "the box's own extent is one pyramid cell, stated three ways that must agree: {v}"
    );
    assert!(
        ts >= w0 - cell && te <= w1 + cell,
        "the cell the peak was measured in lies in the window searched: {v}"
    );
    // A quiet band well outside the fixture: nothing found — but the window is still reported, so
    // "nothing in the last 5 s" and "nothing in the last 300 s" are distinguishable (T-337).
    let (st, v) = get(addr, "/api/analysis/strongest?f_lo=1e9&f_hi=1.0001e9");
    assert_eq!((st, &v["found"]), (200, &json!(false)));
    let (n0, n1) = (num(&v["window"], "t0_s"), num(&v["window"], "t1_s"));
    assert!(n0 > 1.7e9 && n1 - n0 > 4.9 && n1 - n0 < 5.1, "{v}");
    let (st, wide) = get(
        addr,
        "/api/analysis/strongest?f_lo=1e9&f_hi=1.0001e9&window_s=300",
    );
    assert_eq!((st, &wide["found"]), (200, &json!(false)));
    let span = num(&wide["window"], "t1_s") - num(&wide["window"], "t0_s");
    assert!(
        span > 299.0 && span < 301.0,
        "a longer search reports the longer window it actually searched: {wide}"
    );
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

/// T-904: a window whose per-frame detection rows have been pruned still answers
/// `/api/inventory`, `/api/events` and each listed emitter's `/api/inventory/{id}/presence` (the
/// durable catalogue: presence intervals, `measured`, relations) **exactly** as before the prune. A finished, unpaced replay makes the store stable,
/// so the two answers can be compared whole. The prune here is harsher than the product's: every
/// row older than one second of the recording, keeping only each emitter's newest **one** — the
/// one `/api/inventory`'s `measured` reads (`Repository::prune_detections` keeps 256 by default,
/// every per-emitter query's cap; `hk-model`'s retention tests hold that bound).
#[test]
fn a_pruned_window_still_answers_inventory_and_events_as_before() {
    let dir = temp_data_dir();
    let _guard = TempDataDirGuard::new(dir.clone());
    let Serving { server, handle, .. } = start(&ServeOptions {
        source: ServeSource::Replay {
            path: fixture_path(),
            loop_replay: false,
            realtime: false,
        },
        data_dir: Some(dir.clone()),
        bind: "127.0.0.1:0".parse().unwrap(),
        ui_dist: None,
        fft_len: 1024,
        rows_per_s: 25.0,
        calibration: None,
        token: Some(TOKEN.into()),
        listen: Default::default(),
        compute: Default::default(),
        iq_buffer: Default::default(),
        iq_buffer_hooks: None,
    })
    .unwrap();
    let addr = server.local_addr();
    handle.wait().expect("the replay runs to its end");

    let db = dir.join("hackriff.db");
    let mut repo = hk_model::Repository::open(&db).unwrap();
    let rows = repo.detection_storage().unwrap();
    let newest = rows
        .newest_detection_end
        .expect("the replay detected something");
    let t1 = newest.as_unix_nanos() as f64 / 1e9 + 1.0;
    let t0 = t1 - 3600.0;
    let (f_lo, f_hi) = (STATION_HZ - 1.2e6, STATION_HZ + 1.2e6);
    let window = format!("f_lo={f_lo}&f_hi={f_hi}&t0={t0}&t1={t1}");
    let read = || {
        let (st, inv) = get(addr, &format!("/api/inventory?{window}&limit=500"));
        assert_eq!(st, 200, "{inv}");
        let (st, ev) = get(addr, &format!("/api/events?{window}"));
        assert_eq!(st, 200, "{ev}");
        // Each listed emitter's own presence track (`/api/inventory/{id}/presence`).
        let presence: Vec<Value> = inv["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                let id = e["id"].as_str().expect("an inventory entry has an id");
                // Windowed, so liveness derives against `t1` rather than the wall clock.
                let (st, track) = get(
                    addr,
                    &format!("/api/inventory/{id}/presence?t0={t0}&t1={t1}"),
                );
                assert_eq!(st, 200, "{track}");
                track
            })
            .collect();
        (inv, ev, presence)
    };
    let (inv_before, ev_before, presence_before) = read();
    assert!(
        presence_before
            .iter()
            .any(|p| p["intervals"].as_array().is_some_and(|i| !i.is_empty())),
        "some listed emitter has a presence track: {presence_before:?}"
    );
    assert!(
        !inv_before["entries"].as_array().unwrap().is_empty(),
        "the blind FM station is in the inventory: {inv_before}"
    );
    assert!(
        ev_before["total"].as_u64().is_some_and(|n| n > 0),
        "{ev_before}"
    );

    let report = repo
        .prune_detections(
            &hk_model::DetectionRetention {
                max_age_ns: 1_000_000_000,
                keep_per_emitter: 1,
                batch: 64,
                ..hk_model::DetectionRetention::default()
            },
            || true,
        )
        .unwrap();
    assert!(
        report.deleted > rows.detection_rows / 2,
        "most per-frame rows went: {report:?} of {rows:?}"
    );
    let after = repo.detection_storage().unwrap();
    assert_eq!(after.detection_rows, rows.detection_rows - report.deleted);
    assert!(after.rollup_rows > 0, "{after:?}");

    let (inv_after, ev_after, presence_after) = read();
    assert_eq!(inv_after, inv_before, "the inventory answer did not move");
    assert_eq!(ev_after, ev_before, "the event catalogue did not move");
    assert_eq!(
        presence_after, presence_before,
        "every listed emitter's presence track did not move"
    );
    drop(repo);
    drop(server);
}

/// T-264 (ADR-0017 stage TM-8): the History surface's two routes — the durable catalogue of
/// events in a region over a time range, and one emitter's presence track.
///
/// The invariant under test is the one that makes the whole time model safe to live with:
/// **nothing is deleted to make the live list correct**. Explore is window-scoped (T-260), so a
/// signal that stopped hours ago is not listed there; every one of its events is still catalogued
/// here, one row per presence interval, one-offs included. And the answer never lets an empty
/// catalogue read as a quiet band: `coverage.statement` says which of "unknown", "no data for this
/// period" and "nothing was on the air" it is.
#[test]
fn events_and_presence_serve_the_durable_catalogue() {
    let (_guard, serving, addr) = start_server();
    let (f_lo, f_hi) = (STATION_HZ - 400e3, STATION_HZ + 400e3);
    // The catalogue only has events once the pipeline has seen the station.
    wait_for(
        "an event in the catalogue for the blind FM station",
        Duration::from_secs(90),
        || {
            let t1 = unix_now();
            get(
                addr,
                &format!(
                    "/api/events?f_lo={f_lo}&f_hi={f_hi}&t0={}&t1={t1}",
                    t1 - 3600.0
                ),
            )
            .1["total"]
                .as_u64()
                .is_some_and(|n| n > 0)
        },
    );
    let t1 = unix_now();
    let t0 = t1 - 3600.0;
    let (st, v) = get(
        addr,
        &format!("/api/events?f_lo={f_lo}&f_hi={f_hi}&t0={t0}&t1={t1}"),
    );
    assert_eq!(st, 200, "{v}");
    for field in [
        "window",
        "events",
        "emitters",
        "total",
        "limit",
        "next_cursor",
        "emitters_truncated",
        "emitters_no_interval",
        "coverage",
        "identity_access",
    ] {
        assert!(v.get(field).is_some(), "events answer missing {field}: {v}");
    }
    let events = v["events"].as_array().expect("events");
    assert!(!events.is_empty(), "the station was on the air: {v}");
    let emitters = v["emitters"].as_array().expect("emitters");
    for e in events {
        for field in [
            "emitter_id",
            "t_start_s",
            "t_end_s",
            "duration_s",
            "in_window_s",
            "open",
            "count",
            "sources",
            "f_center_hz",
        ] {
            assert!(e.get(field).is_some(), "event missing {field}: {e}");
        }
        let (start, end) = (
            e["t_start_s"].as_f64().unwrap(),
            e["t_end_s"].as_f64().unwrap(),
        );
        // An event IS a timespan: it has a start and a stop, and the backend states its length —
        // a client never derives a duration from two fields it was handed.
        assert!(end >= start, "an event never ends before it starts: {e}");
        let d = e["duration_s"].as_f64().unwrap();
        assert!(
            (d - (end - start)).abs() < 1e-6,
            "duration_s is the span: {e}"
        );
        assert!(
            e["in_window_s"].as_f64().unwrap() <= d + 1e-6,
            "time inside the window never exceeds the event: {e}"
        );
        assert!(e["open"].is_boolean(), "{e}");
        // Every event names an emitter that is listed once, with its ranked explanations.
        let id = e["emitter_id"].as_str().expect("emitter_id");
        assert!(
            emitters.iter().filter(|m| m["id"] == json!(id)).count() == 1,
            "each emitter with events is listed exactly once: {v}"
        );
    }
    for m in emitters {
        for field in [
            "id",
            "state",
            "f_center_hz",
            "bandwidth_hz",
            "f_lo_hz",
            "f_hi_hz",
            "known_status",
            "family",
            "explanations",
            "identity_scheme",
            "withheld",
            "events",
            "on_air_s",
            "liveness",
            "count",
        ] {
            assert!(
                m.get(field).is_some(),
                "catalogue emitter missing {field}: {m}"
            );
        }
        assert!(is_array(&m["explanations"]), "{m}");
        assert!(
            m["events"].as_u64().is_some_and(|n| n > 0),
            "an emitter is listed only when it has events in the window: {m}"
        );
        assert!(
            matches!(m["liveness"].as_str(), Some("live" | "ended" | "absent")),
            "liveness is live/ended/absent: {m}"
        );
    }
    // T-591: **the two surfaces cannot disagree about one emitter's liveness.** An emitter's
    // presence is one interval `[start, end?]` (ADR-0017/0019), so liveness is a property of the
    // emitter and not of the route asked. `/api/events` used to derive it under
    // `IdleGap::conservative()` (60 s) while `/api/inventory` measured the gap off the band's tune
    // history (T-410), and they disagreed. Asserted over EVERY emitter the catalogue listed, with
    // the count reported — a comparison of zero emitters would be vacuous.
    let (st, inv) = get(
        addr,
        &format!("/api/inventory?f_lo={f_lo}&f_hi={f_hi}&t0={t0}&t1={t1}&limit=500"),
    );
    assert_eq!(st, 200, "{inv}");
    let rows = inv["entries"].as_array().expect("inventory entries");
    let mut compared = 0usize;
    for m in emitters {
        let id = m["id"].as_str().unwrap();
        let Some(row) = rows.iter().find(|r| r["id"] == json!(id)) else {
            continue;
        };
        assert_eq!(
            m["liveness"], row["presence"]["liveness"],
            "/api/events and /api/inventory disagree about emitter {id} in the same window: \
             events {m}, inventory row presence {}",
            row["presence"]
        );
        compared += 1;
    }
    assert!(
        compared > 0,
        "liveness was compared for {compared} emitters — a vacuous comparison: events {v}, \
         inventory {inv}"
    );
    eprintln!("T-591: liveness compared across both surfaces for {compared} emitters");
    // Coverage always answers, and always in words a client can show beside an empty list: an
    // unobserved stretch is never reported as a quiet band (C26).
    let statement = v["coverage"]["statement"]
        .as_str()
        .expect("coverage carries a statement");
    assert!(!statement.is_empty(), "{v}");

    // A period before anything was recorded: the catalogue is empty, and the coverage statement
    // says *no data for this period*, never "nothing was on the air".
    let (st, past) = get(
        addr,
        &format!("/api/events?f_lo={f_lo}&f_hi={f_hi}&t0=0&t1=1"),
    );
    assert_eq!(st, 200, "{past}");
    assert_eq!(
        past["total"],
        json!(0),
        "nothing was on the air in 1970: {past}"
    );
    assert!(past["events"].as_array().unwrap().is_empty(), "{past}");
    let past_statement = past["coverage"]["statement"].as_str().unwrap();
    assert!(
        !past_statement.contains("nothing was on the air"),
        "an unobserved period must never read as a quiet band: {past_statement}"
    );

    // Paging and validation.
    let (st, one) = get(
        addr,
        &format!("/api/events?f_lo={f_lo}&f_hi={f_hi}&t0={t0}&t1={t1}&limit=1"),
    );
    assert_eq!(st, 200, "{one}");
    assert!(one["events"].as_array().unwrap().len() <= 1, "{one}");
    assert_eq!(
        one["total"], v["total"],
        "a page never changes the total: {one}"
    );
    // `limit`'s documented range is 1..=2000 (T-592: `events_json` used to re-validate the same
    // `limit` parameter against `/api/inventory`'s tighter 500-row cap via `parse_inventory_query`,
    // so every value from 501 to 2000 was documented as valid and answered with a 400). Assert the
    // boundary, not a value in the middle: the last accepted value and the first rejected one.
    let (st, at_max) = get(
        addr,
        &format!("/api/events?f_lo={f_lo}&f_hi={f_hi}&t0={t0}&t1={t1}&limit=2000"),
    );
    assert_eq!(
        st, 200,
        "limit=2000 is the documented max and must be accepted: {at_max}"
    );
    let (st, over_max) = get(
        addr,
        &format!("/api/events?f_lo={f_lo}&f_hi={f_hi}&t0={t0}&t1={t1}&limit=2001"),
    );
    assert_eq!(
        st, 400,
        "limit=2001 is one past the documented max and must be refused: {over_max}"
    );
    for bad in [
        format!("/api/events?f_lo={f_lo}&f_hi={f_hi}&t0={t0}"),
        format!("/api/events?f_lo={f_lo}&f_hi={f_hi}&t0={t1}&t1={t0}"),
        format!("/api/events?f_lo={f_hi}&f_hi={f_lo}&t0={t0}&t1={t1}"),
        format!("/api/events?f_lo={f_lo}&f_hi={f_hi}&t0={t0}&t1={t1}&cursor=nope"),
        format!("/api/events?f_lo={f_lo}&f_hi={f_hi}&t0={t0}&t1={t1}&state=bogus"),
    ] {
        let (st, e) = get(addr, &bad);
        assert_eq!(st, 400, "{bad} should be refused: {e}");
    }
    let (st, e) = post(addr, "/api/events", "{}");
    assert_eq!(st, 405, "read-only route: {e}");

    // `GET /api/inventory/{id}/presence` — the track itself, every interval with its own timespan.
    let id = emitters[0]["id"].as_str().unwrap().to_owned();
    let (st, track) = get(addr, &format!("/api/inventory/{id}/presence"));
    assert_eq!(st, 200, "{track}");
    for field in [
        "emitter",
        "window",
        "intervals",
        "total",
        "truncated",
        "presence",
    ] {
        assert!(
            track.get(field).is_some(),
            "presence missing {field}: {track}"
        );
    }
    assert!(
        track["window"].is_null(),
        "no window was asked about: {track}"
    );
    let intervals = track["intervals"].as_array().expect("intervals");
    assert!(!intervals.is_empty(), "this emitter has events: {track}");
    for i in intervals {
        for field in [
            "t_start_s",
            "t_end_s",
            "duration_s",
            "open",
            "count",
            "sources",
            "f_center_hz",
        ] {
            assert!(i.get(field).is_some(), "interval missing {field}: {i}");
        }
        assert!(
            i["t_end_s"].as_f64().unwrap() >= i["t_start_s"].as_f64().unwrap(),
            "{i}"
        );
    }
    // The projection agrees with the row's own, so the two surfaces never disagree on liveness —
    // or, since T-251, on the decayed confidence that ranks a stopped candidate.
    for field in [
        "intervals",
        "on_air_s",
        "last_interval",
        "liveness",
        "ended_t_s",
        "silence_s",
        "confidence",
    ] {
        assert!(
            track["presence"].get(field).is_some(),
            "presence projection missing {field}: {track}"
        );
    }
    // Same emitter: this route and the inventory row must agree field for field — except
    // `silence_s` and the `confidence` derived from it, both of which on an unwindowed query are a
    // reading of the live clock and so advance between two HTTP calls. Asserting bit equality there
    // would assert the clock cannot tick; the invariant that matters is that the two surfaces agree
    // on liveness and on the decayed rank.
    //
    // **`confidence` joined that list at T-410, and it is the fix working.** It used to be bit-equal
    // by accident: the idle gap was `IdleGap::conservative()` — 60 s — on every surface, so any
    // silence under a minute sat on the flat part of the decay law and both calls read exactly 1.0.
    // Now the gap is *measured* off the run's tune history (ADR-0019 §3), so on a continuously
    // dwelt band it is 1 s, a few-second silence is genuinely decayed, and a few milliseconds of
    // clock between two calls moves it. Equality here would now be asserting the 60 s default back.
    let (st, one) = get(addr, &format!("/api/inventory/{id}"));
    assert_eq!(st, 200, "{one}");
    let clock_read = |p: &serde_json::Value| {
        let mut p = p.clone();
        let o = p.as_object_mut().expect("presence is an object");
        o.remove("silence_s");
        o.remove("confidence");
        p
    };
    assert_eq!(
        clock_read(&track["presence"]),
        clock_read(&one["presence"]),
        "the track route and the row serve the same projection: {track} vs {one}"
    );
    // And the decayed rank still agrees between them, to the width of the clock tick between the
    // two calls — which is the property the bit-equality was standing in for.
    let (track_conf, row_conf) = (
        track["presence"]["confidence"].as_f64(),
        one["presence"]["confidence"].as_f64(),
    );
    assert_eq!(
        track_conf.is_none(),
        row_conf.is_none(),
        "both surfaces rank the row: {track} vs {one}"
    );
    if let (Some(a), Some(b)) = (track_conf, row_conf) {
        assert!(
            (a - b).abs() < 0.05,
            "the same decayed rank up to the clock ticking between two calls: {a} vs {b}"
        );
    }
    let (track_silence, row_silence) = (
        track["presence"]["silence_s"].as_f64(),
        one["presence"]["silence_s"].as_f64(),
    );
    assert_eq!(
        track_silence.is_none(),
        row_silence.is_none(),
        "both surfaces say whether the row has been silent at all: {track} vs {one}"
    );
    if let (Some(a), Some(b)) = (track_silence, row_silence) {
        assert!(
            (a - b).abs() < 5.0,
            "the same silence up to the clock ticking between two calls: {a} vs {b}"
        );
    }
    // A window scopes the track and carries its own live edge: a window before every interval
    // holds none of them, and nothing is fabricated for it.
    let (st, empty) = get(addr, &format!("/api/inventory/{id}/presence?t0=0&t1=1"));
    assert_eq!(st, 200, "{empty}");
    assert_eq!(empty["total"], json!(0), "{empty}");
    assert_eq!(empty["presence"]["liveness"], json!("absent"), "{empty}");
    assert!(empty["presence"]["last_interval"].is_null(), "{empty}");
    let (st, e) = get(addr, &format!("/api/inventory/{id}/presence?t0=5&t1=1"));
    assert_eq!(st, 400, "{e}");
    let (st, e) = get(addr, &format!("/api/inventory/{id}/presence?t0=1"));
    assert_eq!(st, 400, "a window is both bounds or neither: {e}");
    let (st, e) = get(addr, "/api/inventory/not-a-uuid/presence");
    assert_eq!(st, 404, "{e}");
    let unknown = EmitterId::new();
    let (st, e) = get(addr, &format!("/api/inventory/{unknown}/presence"));
    assert_eq!(st, 404, "{e}");
    let (st, e) = post(addr, &format!("/api/inventory/{id}/presence"), "{}");
    assert_eq!(st, 405, "read-only route: {e}");

    stop_server(serving);
}

/// T-812 (MAP-12, docs/24 §7): `GET /api/priors` serves the band plan over a viewport as **ranked
/// suggestions, never truth**. Asserted by value:
///
/// - over the blind FM station's window, the `fm-broadcast` allocation is served, backed by the
///   measured emission (`in-band`/`cited`, never `context`), ranked above every context-only row,
///   and — the station sitting on its 200 kHz raster — not flagged off-raster;
/// - over a band nothing was detected in, the allocation is **context only**, and asking for priors
///   there adds **no inventory row** (the database never pre-populates the inventory);
/// - the box is required and validated like `/api/events`, the route is read-only, and it is
///   token-gated.
#[test]
fn priors_serve_ranked_band_plan_suggestions_never_truth() {
    let (_guard, serving, addr) = start_server();
    let (f_lo, f_hi) = (87e6, 109e6);
    // Detection is blind and comes first: wait until the station is in the catalogue.
    wait_for(
        "an event in the catalogue for the blind FM station",
        Duration::from_secs(90),
        || {
            let t1 = unix_now();
            get(
                addr,
                &format!(
                    "/api/events?f_lo={}&f_hi={}&t0={}&t1={t1}",
                    STATION_HZ - 400e3,
                    STATION_HZ + 400e3,
                    t1 - 3600.0
                ),
            )
            .1["total"]
                .as_u64()
                .is_some_and(|n| n > 0)
        },
    );
    let t1 = unix_now();
    let t0 = t1 - 3600.0;
    let (st, v) = get(
        addr,
        &format!("/api/priors?f_lo={f_lo}&f_hi={f_hi}&t0={t0}&t1={t1}"),
    );
    assert_eq!(st, 200, "{v}");
    for field in [
        "window",
        "kind",
        "source",
        "region",
        "priors",
        "total",
        "truncated",
        "emitters_considered",
        "emitters_truncated",
        "statement",
    ] {
        assert!(v.get(field).is_some(), "priors answer missing {field}: {v}");
    }
    assert_eq!(v["kind"], json!("suggestion"), "{v}");
    assert!(
        v["statement"].as_str().unwrap().contains("never truth"),
        "{v}"
    );
    let priors = v["priors"].as_array().expect("priors");
    assert_eq!(v["total"].as_u64(), Some(priors.len() as u64), "{v}");
    for (i, p) in priors.iter().enumerate() {
        assert_eq!(
            p["rank"].as_u64(),
            Some(i as u64 + 1),
            "ranks run 1..n: {v}"
        );
        for field in [
            "f_lo_hz",
            "f_hi_hz",
            "service",
            "allocation",
            "source",
            "reason",
            "support",
            "off_raster_hz",
        ] {
            assert!(p.get(field).is_some(), "prior missing {field}: {p}");
        }
        let hz = |k: &str| {
            p[k].as_f64()
                .unwrap_or_else(|| panic!("{k} is a number: {p}"))
        };
        assert!(
            hz("f_hi_hz") >= f_lo && hz("f_lo_hz") <= f_hi,
            "every prior intersects the box: {p}"
        );
        assert!(
            p["reason"].as_str().unwrap().contains("never truth"),
            "every reason is worded as a suggestion: {p}"
        );
    }
    let fm = priors
        .iter()
        .position(|p| p["id"] == json!("fm-broadcast"))
        .unwrap_or_else(|| panic!("the FM broadcast allocation intersects 87–109 MHz: {v}"));
    let fm_row = &priors[fm];
    assert!(
        matches!(fm_row["support"].as_str(), Some("cited" | "in-band")),
        "the blind station lies in the FM allocation, so it is measured support, not context: {fm_row}"
    );
    assert!(fm_row["emitters_in_band"].as_u64() >= Some(1), "{fm_row}");
    assert!(
        priors[..fm]
            .iter()
            .all(|p| p["support"] != json!("context")),
        "a measured-backed allocation outranks every context-only one: {v}"
    );
    // The off-raster flag, read from the emitters' own stored raster fits: the station on its
    // 200 kHz odd-tenth channel is never flagged; anything flagged lies in the allocation, off its
    // nearest channel by what the entry says, and is FLAGGED rather than snapped (its centre is
    // served as measured, not as the channel).
    let off = fm_row["off_raster"].as_array().expect("off_raster");
    let mut worst: Option<f64> = None;
    for o in off {
        let (fc, near, dx, step) = (
            o["f_center_hz"].as_f64().unwrap(),
            o["nearest_channel_hz"].as_f64().unwrap(),
            o["offset_hz"].as_f64().unwrap(),
            o["raster_hz"].as_f64().unwrap(),
        );
        assert!(
            (fc - STATION_HZ).abs() > 50e3,
            "{STATION_HZ} Hz sits on the 200 kHz odd-tenth raster and must not be flagged: {o}"
        );
        assert!((88e6..=108e6).contains(&fc), "{o}");
        assert!(
            ((fc - near) - dx).abs() < 1.0 && dx != 0.0 && dx.abs() <= step / 2.0 + 1.0,
            "an off-raster entry states its own offset from its nearest channel: {o}"
        );
        if worst.is_none_or(|w| dx.abs() > w.abs()) {
            worst = Some(dx);
        }
    }
    assert_eq!(
        fm_row["off_raster_hz"].as_f64(),
        worst,
        "off_raster_hz is the furthest flagged offset, or null when nothing is flagged: {fm_row}"
    );
    if !off.is_empty() {
        assert!(
            fm_row["reason"]
                .as_str()
                .unwrap()
                .contains("flagged, not snapped"),
            "{fm_row}"
        );
    }

    // A band nothing was detected in: context only — and asking adds no inventory row.
    let quiet = format!("f_lo=1e9&f_hi=1.0001e9&t0={t0}&t1={t1}");
    let inventory_total = || {
        let (st, inv) = get(addr, &format!("/api/inventory?{quiet}&relations=all"));
        assert_eq!(st, 200, "{inv}");
        inv["total"].as_u64().expect("total")
    };
    let before = inventory_total();
    let (st, q) = get(addr, &format!("/api/priors?{quiet}"));
    assert_eq!(st, 200, "{q}");
    let qp = q["priors"].as_array().expect("priors");
    assert!(
        qp.iter()
            .any(|p| p["id"] == json!("aero-radionav-960-1215")),
        "1 GHz lies in the 960–1215 MHz aeronautical allocation: {q}"
    );
    for p in qp {
        assert_eq!(p["support"], json!("context"), "nothing measured here: {p}");
        assert!(
            p["reason"].as_str().unwrap().contains("context only"),
            "{p}"
        );
    }
    assert_eq!(
        inventory_total(),
        before,
        "the band plan never pre-populates the inventory"
    );

    // Validation, read-only, gated.
    let (st, e) = get(
        addr,
        &format!("/api/priors?f_lo={f_lo}&f_hi={f_hi}&t0={t0}"),
    );
    assert_eq!(st, 400, "the box is required, like /api/events: {e}");
    let (st, e) = get(
        addr,
        &format!("/api/priors?f_lo={f_hi}&f_hi={f_lo}&t0={t0}&t1={t1}"),
    );
    assert_eq!(st, 400, "{e}");
    let (st, e) = post(
        addr,
        &format!("/api/priors?f_lo={f_lo}&f_hi={f_hi}&t0={t0}&t1={t1}"),
        "{}",
    );
    assert_eq!(st, 405, "read-only route: {e}");
    let (st, e) = call(
        addr,
        "GET",
        &format!("/api/priors?f_lo={f_lo}&f_hi={f_hi}&t0={t0}&t1={t1}"),
        None,
        None,
    );
    assert_eq!(st, 401, "{e}");

    stop_server(serving);
}

/// T-897 (docs/23 §10.6 rule 2): `GET /api/paths` answers as `docs/api.md` documents it, on a live
/// `hk serve` over the mock device's FM window. By value, not only shape: the fixture's one
/// emitter is a **steady** broadcast station, so once the run has stored detections of it the
/// route must have read them (`detections_read > 0`) and traced **no** path through them — a
/// carrier cut into segments is not a chirp. The chirp/sweep/hop producers are asserted blind, with
/// hidden truth, through the mock device in `hk-pipeline/tests/paths_blind.rs`.
#[test]
fn paths_route_answers_as_documented() {
    let (_guard, serving, addr) = start_server();
    let (f_lo, f_hi) = (STATION_HZ - 400e3, STATION_HZ + 400e3);
    let url = |t0: f64, t1: f64, extra: &str| {
        format!("/api/paths?f_lo={f_lo}&f_hi={f_hi}&t0={t0}&t1={t1}{extra}")
    };
    wait_for(
        "the station's detections to reach the paths route",
        Duration::from_secs(90),
        || {
            let t1 = unix_now();
            get(addr, &url(t1 - 3600.0, t1, "")).1["detections_read"]
                .as_u64()
                .is_some_and(|n| n > 0)
        },
    );
    let t1 = unix_now();
    let t0 = t1 - 3600.0;
    let (st, v) = get(addr, &url(t0, t1, ""));
    assert_eq!(st, 200, "{v}");
    for field in [
        "window",
        "context",
        "paths",
        "total",
        "limit",
        "truncated",
        "detections_read",
        "detections_truncated",
        "method",
    ] {
        assert!(v.get(field).is_some(), "paths answer missing {field}: {v}");
    }
    assert_eq!(v["method"], json!("hk-model/path@1"), "{v}");
    assert_eq!(v["limit"], json!(200), "{v}");
    assert_eq!(v["window"]["f_lo_hz"].as_f64(), Some(f_lo), "{v}");
    assert_eq!(v["window"]["f_hi_hz"].as_f64(), Some(f_hi), "{v}");
    // The context is the window widened by its own span, the time margin capped at 600 s.
    assert_eq!(v["context"]["f_lo_hz"].as_f64(), Some(f_lo - 800e3), "{v}");
    assert_eq!(v["context"]["f_hi_hz"].as_f64(), Some(f_hi + 800e3), "{v}");
    let ct0 = v["context"]["t0_s"].as_f64().unwrap();
    assert!((ct0 - (t0 - 600.0)).abs() < 1e-3, "{v}");
    assert_eq!(v["paths"], json!([]), "a steady station draws no path: {v}");
    assert_eq!(v["total"], json!(0), "{v}");
    assert_eq!(v["truncated"], json!(false), "{v}");
    // `kind` and `limit` narrow; malformed ones refuse.
    let (st, k) = get(addr, &url(t0, t1, "&kind=hop&limit=5"));
    assert_eq!(st, 200, "{k}");
    assert_eq!(k["limit"], json!(5), "{k}");
    for bad in ["&kind=radar", "&limit=0", "&limit=5000"] {
        let (st, e) = get(addr, &url(t0, t1, bad));
        assert_eq!(st, 400, "{bad}: {e}");
    }
    // A viewport route needs the whole viewport.
    for q in [
        format!("/api/paths?f_lo={f_lo}&f_hi={f_hi}&t0={t0}"),
        format!("/api/paths?f_lo={f_hi}&f_hi={f_lo}&t0={t0}&t1={t1}"),
        format!("/api/paths?f_lo={f_lo}&f_hi={f_hi}&t0={t1}&t1={t0}"),
        "/api/paths".to_owned(),
    ] {
        let (st, e) = get(addr, &q);
        assert_eq!(st, 400, "{q}: {e}");
    }
    let (st, e) = post(addr, "/api/paths", "{}");
    assert_eq!(st, 405, "read-only route: {e}");
    let (st, _) = call(addr, "GET", &url(t0, t1, ""), None, None);
    assert_eq!(st, 401, "token-gated like every other route");

    stop_server(serving);
}

/// T-898 (docs/23 §10.6 rule 2): `GET /api/tune-history` answers as `docs/api.md` documents it, on
/// a live `hk serve` over the mock device. By value: the run is tuned to the fixture's window, so
/// the served route must be that front end's, its vertices must sit at the tuned centre, and a
/// window the radio was never in (nor crossed) must draw nothing. The blind end-to-end assertion —
/// vertices landing on **scripted retune instants** — is `hk-pipeline/tests/tune_history_path.rs`.
#[test]
fn tune_history_route_answers_as_documented() {
    let (_guard, serving, addr) = start_server();
    // The route is drawn at the TUNED CENTRE, so the viewport is the fixture's own window.
    let (f_lo, f_hi) = (FIXTURE_CENTER_HZ - 400e3, FIXTURE_CENTER_HZ + 400e3);
    let url = |t0: f64, t1: f64, extra: &str| {
        format!("/api/tune-history?f_lo={f_lo}&f_hi={f_hi}&t0={t0}&t1={t1}{extra}")
    };
    wait_for(
        "a tune record to reach the tune-history route",
        Duration::from_secs(90),
        || {
            let t1 = unix_now();
            get(addr, &url(t1 - 3600.0, t1, "")).1["total"]
                .as_u64()
                .is_some_and(|n| n > 0)
        },
    );
    let t1 = unix_now();
    let t0 = t1 - 3600.0;
    let (st, v) = get(addr, &url(t0, t1, ""));
    assert_eq!(st, 200, "{v}");
    for field in [
        "window",
        "context",
        "paths",
        "devices",
        "total",
        "limit",
        "truncated",
        "horizon",
        "method",
    ] {
        assert!(
            v.get(field).is_some(),
            "tune-history answer missing {field}: {v}"
        );
    }
    assert_eq!(v["method"], json!("hk-store/tune-path@1"), "{v}");
    assert_eq!(v["limit"], json!(64), "{v}");
    assert_eq!(v["window"]["f_lo_hz"].as_f64(), Some(f_lo), "{v}");
    // The context widens the time window by its own duration, capped at 600 s, and never narrows
    // the frequency axis: the band bounds are null, not numbers.
    assert_eq!(v["context"]["f_lo_hz"], json!(null), "{v}");
    assert_eq!(v["context"]["f_hi_hz"], json!(null), "{v}");
    let ct0 = v["context"]["t0_s"].as_f64().unwrap();
    assert!((ct0 - (t0 - 600.0)).abs() < 1e-3, "{v}");
    // The one front end's route, over the window it is tuned to.
    let p = &v["paths"][0];
    assert!(p["device"].is_string(), "{v}");
    assert!(
        p["vertices"].as_array().is_some_and(|a| a.len() >= 2),
        "{v}"
    );
    for vx in p["vertices"].as_array().unwrap() {
        assert!(vx["t_s"].as_f64().is_some_and(f64::is_finite), "{vx}");
        let f = vx["f_hz"].as_f64().expect("a vertex names a frequency");
        assert!(
            (f - FIXTURE_CENTER_HZ).abs() < 1.0,
            "the route sits at the tuned centre: {vx}"
        );
        assert!(
            ["start", "end"].contains(&vx["at"].as_str().unwrap_or("")),
            "{vx}"
        );
    }
    assert!(
        p["legs"].as_u64().unwrap_or(0).max(1) > p["retunes"].as_u64().unwrap_or(0),
        "retunes are the changes of centre between legs: {p}"
    );
    assert!(v["horizon"]["as_of_s"].as_f64().is_some(), "{v}");
    // A band the radio was never in and never crossed draws nothing.
    let (st, far) = get(
        addr,
        &format!("/api/tune-history?f_lo=2.0e9&f_hi=2.1e9&t0={t0}&t1={t1}"),
    );
    assert_eq!(st, 200, "{far}");
    assert_eq!(far["total"], json!(0), "{far}");
    // `device` and `limit` narrow; malformed ones refuse.
    let dev = p["device"].as_str().unwrap().to_owned();
    let (st, d) = get(addr, &url(t0, t1, &format!("&device={dev}&limit=5")));
    assert_eq!(st, 200, "{d}");
    assert_eq!(d["limit"], json!(5), "{d}");
    assert_eq!(d["total"], v["total"], "{d}");
    let (st, none) = get(addr, &url(t0, t1, "&device=no-such-radio"));
    assert_eq!(st, 200, "{none}");
    assert_eq!(none["total"], json!(0), "{none}");
    for bad in ["&device=any", "&limit=0", "&limit=5000"] {
        let (st, e) = get(addr, &url(t0, t1, bad));
        assert_eq!(st, 400, "{bad}: {e}");
    }
    // A viewport route needs the whole viewport.
    for q in [
        format!("/api/tune-history?f_lo={f_lo}&f_hi={f_hi}&t0={t0}"),
        format!("/api/tune-history?f_lo={f_hi}&f_hi={f_lo}&t0={t0}&t1={t1}"),
        format!("/api/tune-history?f_lo={f_lo}&f_hi={f_hi}&t0={t1}&t1={t0}"),
        "/api/tune-history".to_owned(),
    ] {
        let (st, e) = get(addr, &q);
        assert_eq!(st, 400, "{q}: {e}");
    }
    let (st, e) = post(addr, "/api/tune-history", "{}");
    assert_eq!(st, 405, "read-only route: {e}");
    let (st, _) = call(addr, "GET", &url(t0, t1, ""), None, None);
    assert_eq!(st, 401, "token-gated like every other route");

    stop_server(serving);
}

/// T-981: the front end's clip state — `/api/status`'s `frontend` block and
/// `GET /api/frontend/events` — answers as `docs/api.md` documents it, on a live `hk serve` over
/// the mock device. The blind end-to-end assertion (a saturating burst through the mock SDR is
/// flagged on its row, counted, logged as one event and never stored as a detection) is
/// `hk-pipeline/tests/frontend_overload.rs`; this pins the wire shape and the rule by value.
#[test]
fn frontend_status_block_and_events_route_answer_as_documented() {
    let (_guard, serving, addr) = start_server();
    wait_for(
        "spectrum rows measured by the front-end judgement",
        Duration::from_secs(60),
        || {
            get(addr, "/api/status").1["frontend"]["rows"]
                .as_u64()
                .is_some_and(|n| n > 0)
        },
    );
    let (st, v) = get(addr, "/api/status");
    assert_eq!(st, 200, "{v}");
    let fe = &v["frontend"];
    for field in [
        "rows",
        "clipped_rows",
        "event_rows",
        "events",
        "clipped_samples",
        "samples",
        "suppressed_detections",
        "evicted_events",
    ] {
        assert!(fe[field].is_u64(), "frontend.{field}: {fe}");
    }
    assert!(fe["adc_peak_max"].is_number(), "{fe}");
    assert!(
        fe["clipped_rows"].as_u64() <= fe["rows"].as_u64(),
        "a clipped row is a measured row: {fe}"
    );
    assert!(
        fe["event_rows"].as_u64() <= fe["clipped_rows"].as_u64(),
        "an event row is a clipped row: {fe}"
    );
    let last = &fe["last_row"];
    for field in ["t", "clip_fraction", "adc_peak"] {
        assert!(last[field].is_number(), "frontend.last_row.{field}: {fe}");
    }
    for field in ["clipped", "event"] {
        assert!(last[field].is_boolean(), "frontend.last_row.{field}: {fe}");
    }
    assert!(
        fe["last_event"].is_null() || fe["last_event"]["t0"].is_number(),
        "{fe}"
    );
    assert_eq!(fe["log"]["capacity"], json!(1024), "{fe}");
    assert_eq!(fe["rule"]["clip_fraction"], json!(1e-4), "{fe}");
    assert_eq!(fe["rule"]["step_db"], json!(6.0), "{fe}");
    assert_eq!(fe["rule"]["saturation_fraction"], json!(0.01), "{fe}");

    // Every capture time there could be: a replay's clock is its recording's, not the wall's.
    let (t0, t1) = (0.0_f64, 4.0e9_f64);
    let url = |extra: &str| format!("/api/frontend/events?t0={t0}&t1={t1}{extra}");
    let (st, e) = get(addr, &url(""));
    assert_eq!(st, 200, "{e}");
    for field in ["window", "events", "total", "limit", "truncated", "log"] {
        assert!(
            e.get(field).is_some(),
            "frontend/events missing {field}: {e}"
        );
    }
    assert_eq!(e["limit"], json!(256), "{e}");
    assert_eq!(e["window"]["t0"].as_f64(), Some(t0), "{e}");
    assert_eq!(e["log"]["capacity"], json!(1024), "{e}");
    let events = e["events"].as_array().expect("events is an array");
    assert_eq!(e["total"].as_u64(), Some(events.len() as u64), "{e}");
    for ev in events {
        assert_eq!(ev["kind"], json!("clip"), "{ev}");
        let (c, r) = (
            ev["center_hz"].as_f64().unwrap(),
            ev["sample_rate_hz"].as_f64().unwrap(),
        );
        assert_eq!(ev["f_lo_hz"].as_f64(), Some(c - r / 2.0), "{ev}");
        assert_eq!(ev["f_hi_hz"].as_f64(), Some(c + r / 2.0), "{ev}");
        assert!(ev["t1"].as_f64() > ev["t0"].as_f64(), "{ev}");
    }
    let (st, d) = get(addr, &url("&device=no-such-radio&limit=5"));
    assert_eq!(st, 200, "{d}");
    assert_eq!(
        (d["total"].clone(), d["limit"].clone()),
        (json!(0), json!(5)),
        "{d}"
    );
    for bad in [
        format!("/api/frontend/events?t0={t0}"),
        format!("/api/frontend/events?t0={t1}&t1={t0}"),
        url("&limit=0"),
        url("&limit=5000"),
        url("&f_lo=1"),
    ] {
        let (st, e) = get(addr, &bad);
        assert_eq!(st, 400, "{bad}: {e}");
    }
    let (st, e) = post(addr, "/api/frontend/events", "{}");
    assert_eq!(st, 405, "read-only route: {e}");
    let (st, _) = call(addr, "GET", &url(""), None, None);
    assert_eq!(st, 401, "token-gated like every other route");

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
    //
    // T-972: the row that comes back is the **live** entry the id resolves to, and its `id` is the
    // survivor's, not necessarily the one asked for (docs/api.md, "Inventory entry"). Under a live
    // inventory that is not pedantry: detection is "fast, continuous and self-cleaning" (CLAUDE.md,
    // ADR-0019), so the near-duplicate merge can re-key this station between the list call above and
    // this one, and asserting the two ids are equal was a race in the *test's* choice of row — it
    // failed once at ~load 23 with a 200 and a different id, which is reachable only through that
    // resolution. So read the live id back off the server and use it from here on, exactly as a
    // client holding a list id must. The resolution itself is pinned by value against a seeded
    // repository in `crates/hk-api/tests/inventory_api.rs`
    // (`the_entry_route_resolves_a_merged_id_to_its_survivor`), where the merge is made to happen
    // rather than waited for.
    let (st, row) = get(addr, &format!("/api/inventory/{id}"));
    assert_eq!(st, 200, "{row}");
    let id = row["id"]
        .as_str()
        .unwrap_or_else(|| panic!("an entry row carries its id: {row}"))
        .to_owned();
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
            "subaudible",
            "source_recording",
        ] {
            assert!(
                p.get(field).is_some(),
                "estimated_params missing {field}: {row}"
            );
        }
        // T-988: `subaudible` is `null` (nobody looked) or the CTCSS/DCS/none object.
        let sub = &p["subaudible"];
        assert!(sub.is_null() || sub["kind"].is_string(), "{row}");
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
        // T-962: every vote-gated identity row states its vote, the bar and its window, and
        // `provisional` is exactly "the bar has not yet fallen within the window" — a rate, not
        // a lifetime count (docs/api.md, "Provisional identity").
        let f = &row["fields"];
        if row["frame_model"] == json!("rds-pi") {
            assert!(f["pi_provisional"].is_boolean(), "{row}");
            assert_eq!(f["pi_provisional"], f["identity_provisional"], "{row}");
        }
        if let Some(provisional) = f.get("identity_provisional") {
            let votes = f["identity_votes"]
                .as_u64()
                .unwrap_or_else(|| panic!("{row}"));
            let needed = f["identity_votes_needed"]
                .as_u64()
                .unwrap_or_else(|| panic!("{row}"));
            let in_window = f["identity_votes_in_window"]
                .as_u64()
                .unwrap_or_else(|| panic!("{row}"));
            let window_s = f["identity_votes_window_s"]
                .as_f64()
                .unwrap_or_else(|| panic!("{row}"));
            assert!(in_window <= votes.min(needed), "{row}");
            assert!(window_s > 0.0, "{row}");
            assert_eq!(provisional, &json!(in_window < needed), "{row}");
        }
    }
    let (st, v) = get(addr, &format!("/api/inventory/{}/decode", EmitterId::new()));
    assert_eq!((st, v["code"].as_str()), (404, Some("not_found")), "{v}");

    // T-384: the window. Values, not shape — a window that covers every row's `at` must serve the
    // same rows the unwindowed call did, and one that covers none of them must serve none. RDS lock
    // timing on a fixture is not deterministic, so this asserts the *relationship* between the two
    // answers rather than a row count; the seeded-repository suite
    // (`crates/hk-api/tests/decode_api.rs`) pins partial windows, the after-the-filter collapse and
    // the refusals by value.
    let all: Vec<f64> = decodes.iter().filter_map(|r| r["at"].as_f64()).collect();
    let (lo, hi) = (
        all.iter().cloned().fold(f64::INFINITY, f64::min),
        all.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
    );
    let windowed = |t0: f64, t1: f64| {
        let (st, v) = get(addr, &format!("/api/inventory/{id}/decode?t0={t0}&t1={t1}"));
        assert_eq!(st, 200, "{v}");
        v["decodes"].as_array().cloned().unwrap_or_default()
    };
    if !all.is_empty() {
        assert_eq!(
            windowed(lo - 1.0, hi + 1.0),
            *decodes,
            "a window covering every row's `at` serves exactly the unwindowed rows"
        );
        assert!(
            windowed(hi + 60.0, hi + 120.0).is_empty(),
            "a window after the last decode holds none of them"
        );
    }
    // A half-given, backwards, or nanosecond-valued window is refused rather than completed: an
    // invented window would succeed and return a plausible-looking zero rows (ADR-0013 §3.3.1).
    for q in [
        "t0=1789300820",
        "t1=1789300820",
        "t0=2&t1=1",
        "t0=0&t1=1789300820000000000",
    ] {
        let (st, v) = get(addr, &format!("/api/inventory/{id}/decode?{q}"));
        assert_eq!(
            (st, v["code"].as_str()),
            (400, Some("invalid")),
            "{q} must be refused: {v}"
        );
    }

    // Promote: candidate -> confirmed (idempotent: a second promote reports changed: false).
    let (st, v) = post(addr, &format!("/api/inventory/{id}/promote"), "{}");
    assert_eq!(st, 200, "{v}");
    assert!(v.get("entry").is_some(), "{v}");
    assert_eq!(v["entry"]["state"], json!("confirmed"), "{v}");
    // T-972, again from the server's own answer: `entry` is the live row, so this both re-anchors
    // the id for the band/delete calls below and settles it — a same-emission merge keeps the
    // confirmed entry's id over a candidate's (`merge_same_emission_rows`), so from here the id a
    // client holds is the one the user accepted.
    let id = v["entry"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("a promoted entry carries its id: {v}"))
        .to_owned();
    // ...and the band block below must validate against *that* row's measured band, not the one
    // read before the promote: the user band is checked against the live entry's `[f_lo_hz,
    // f_hi_hz]`, which a merge moves.
    let row = v["entry"].clone();
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

// --- Control API: display (device-independent), bookmarks, selections, outputs ------------------

#[test]
fn control_display_and_bookmarks_answer_as_documented() {
    let (_dir_guard, serving, addr) = start_server();

    let (st, v) = post(
        addr,
        "/api/control/display",
        r#"{"fft_size": 512, "averaging": 2, "rows_per_s": 10}"#,
    );
    assert_eq!(st, 200, "{v}");
    assert_eq!(
        v["display"],
        json!({"fft_size": 512, "averaging": 2, "rows_per_s": 10.0, "window": "hann"})
    );
    let (st, v) = post(addr, "/api/control/display", "{}");
    assert_eq!((st, v["code"].as_str()), (400, Some("invalid")), "{v}");
    // T-347: `display` no longer carries `paused`, and pause/resume are not routes. The view's
    // Pause is the client's own time cursor (docs/api.md, "Pause is client view state"); a
    // run-wide flag meant one browser froze every other browser's waterfall.
    for path in ["/api/control/pause", "/api/control/resume"] {
        let (st, v) = post(addr, path, "{}");
        assert_eq!(
            (st, v["code"].as_str()),
            (404, Some("not_found")),
            "{path} must not exist: {v}"
        );
    }

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
    // T-343: a device action's answer says so, and against which front end. A display answer
    // carries no `device` key at all — that difference is the contract a client reads to tell
    // "this changed the world" from "this changed the view".
    assert_eq!(v["device"]["action"], json!("baseband_filter"), "{v}");
    assert!(
        v["device"]["id"]
            .as_str()
            .is_some_and(|d| d.starts_with("mock:")),
        "{v}"
    );
    let (_, shown) = post(addr, "/api/control/display", r#"{"averaging": 4}"#);
    assert!(
        shown.get("device").is_none(),
        "a display change is not a device action: {shown}"
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

/// T-817 (MAP-17, RESEARCH-003): `/api/collections`, `/api/collections/{id}/markers` and
/// `/api/markers[/{id}]` as `docs/api.md` "Marker collections" documents them, on a live `hk serve`
/// over the mock SDR device. A marker is a time-frequency place; its provenance is stamped by the
/// server (including the named front end's sample rate) and never accepted from the client;
/// authoring reaches no radio; and `/api/bookmarks` is a facade over the reserved collection.
#[test]
fn marker_collections_answer_as_documented() {
    let (_dir_guard, serving, addr) = start_server();
    let (_, state) = get(addr, "/api/control/state");
    let device_id = state["device"]["device_id"].as_str().unwrap().to_owned();
    let tuned = state["tuning"].clone();

    // The reserved `Bookmarks` collection exists on a fresh server, and a bookmark is its marker.
    let (st, bm) = post(
        addr,
        "/api/bookmarks",
        r#"{"name": "FM", "f_center_hz": 100.8e6}"#,
    );
    assert_eq!(st, 201, "{bm}");
    let (st, list) = get(addr, "/api/collections");
    assert_eq!(st, 200, "{list}");
    for key in ["collections", "count", "matched", "limit", "next_cursor"] {
        assert!(
            list.get(key).is_some(),
            "collections list missing {key}: {list}"
        );
    }
    assert_eq!(list["limit"], json!(500));
    let reserved = list["collections"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["reserved"] == json!(true))
        .unwrap_or_else(|| panic!("no reserved bookmarks collection: {list}"))
        .clone();
    assert_eq!(reserved["name"], json!("Bookmarks"));
    assert_eq!(reserved["member_count"], json!(1), "{reserved}");
    let (st, m) = get(
        addr,
        &format!("/api/markers/{}", bm["id"].as_str().unwrap()),
    );
    assert_eq!((st, &m["collection_id"]), (200, &reserved["id"]), "{m}");

    // A collection and a timed marker on it, authored from a live-IQ pane on this device.
    let (st, c) = post(addr, "/api/collections", r#"{"name": "bursts"}"#);
    assert_eq!(st, 201, "{c}");
    for field in [
        "id",
        "name",
        "note",
        "color",
        "visible",
        "reserved",
        "member_count",
        "created_s",
        "updated_s",
    ] {
        assert!(c.get(field).is_some(), "collection missing {field}: {c}");
    }
    let cid = c["id"].as_str().unwrap().to_owned();
    let view = json!({
        "center_hz": FIXTURE_CENTER_HZ,
        "span_hz": FIXTURE_RATE_HZ,
        "t_capture": 1_726_480_000.0,
        "tier": "live-iq",
        "device_id": device_id,
    });
    let body = json!({
        "name": "burst", "f_center_hz": 100.9e6, "bandwidth_hz": 50e3,
        "t_center_s": 1_726_480_000.0, "duration_s": 2.0, "view": view,
    });
    let (st, mk) = post(
        addr,
        &format!("/api/collections/{cid}/markers"),
        &body.to_string(),
    );
    assert_eq!(st, 201, "{mk}");
    for field in [
        "id",
        "collection_id",
        "name",
        "note",
        "f_center_hz",
        "bandwidth_hz",
        "f_lo_hz",
        "f_hi_hz",
        "t_center_s",
        "duration_s",
        "t_start_s",
        "t_end_s",
        "provenance",
        "created_s",
        "updated_s",
    ] {
        assert!(mk.get(field).is_some(), "marker missing {field}: {mk}");
    }
    assert_eq!(
        (&mk["t_start_s"], &mk["t_end_s"]),
        (&json!(1_726_479_999.0), &json!(1_726_480_001.0)),
        "{mk}"
    );
    let p = &mk["provenance"];
    assert_eq!(p["device_id"], json!(device_id), "{p}");
    assert_eq!(
        p["sample_rate_hz"],
        json!(FIXTURE_RATE_HZ),
        "stamped by the server: {p}"
    );
    assert_eq!(
        p["t_capture"],
        json!([1_726_480_000.0, 1_726_480_000.0]),
        "{p}"
    );
    assert_eq!(p["authored"], json!(true));
    assert!(p["actor"].is_string() && p["actor"] != json!(TOKEN), "{p}");
    assert!(
        mk.get("device").is_none(),
        "authoring is not a device action: {mk}"
    );

    // A client cannot supply provenance.
    let mut forged = body.clone();
    forged["view"]["actor"] = json!("someone else");
    let (st, v) = post(
        addr,
        &format!("/api/collections/{cid}/markers"),
        &forged.to_string(),
    );
    assert_eq!((st, v["code"].as_str()), (400, Some("invalid")), "{v}");

    // Windowed list: the marker's time finds it; a far window does not.
    let (st, v) = get(
        addr,
        &format!("/api/collections/{cid}/markers?t0=1726480000&t1=1726480000.5"),
    );
    assert_eq!((st, &v["matched"]), (200, &json!(1)), "{v}");
    let (_, v) = get(addr, "/api/markers?t0=1&t1=2&f_lo=100e6&f_hi=101e6");
    assert!(
        v["markers"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["t_center_s"].is_null()),
        "only frequency-only pins match a far window: {v}"
    );

    // Toggle, edit, delete.
    let (st, v) = put(
        addr,
        &format!("/api/collections/{cid}"),
        r#"{"visible": false}"#,
    );
    assert_eq!((st, &v["visible"]), (200, &json!(false)), "{v}");
    let mid = mk["id"].as_str().unwrap();
    let (st, v) = put(
        addr,
        &format!("/api/markers/{mid}"),
        r#"{"note": "again at 12:00"}"#,
    );
    assert_eq!(
        (st, v["note"].as_str()),
        (200, Some("again at 12:00")),
        "{v}"
    );
    let (st, v) = delete(addr, &format!("/api/collections/{cid}"));
    assert_eq!((st, &v["members_deleted"]), (200, &json!(1)), "{v}");
    let (st, v) = get(addr, &format!("/api/markers/{mid}"));
    assert_eq!((st, v["code"].as_str()), (404, Some("not_found")), "{v}");
    let (st, v) = delete(
        addr,
        &format!("/api/collections/{}", reserved["id"].as_str().unwrap()),
    );
    assert_eq!((st, v["code"].as_str()), (400, Some("invalid")), "{v}");

    // None of it moved the radio.
    let (_, after) = get(addr, "/api/control/state");
    assert_eq!(
        (
            &after["tuning"]["center_hz"],
            &after["tuning"]["sample_rate_hz"]
        ),
        (&tuned["center_hz"], &tuned["sample_rate_hz"]),
        "authoring markers never reaches the device"
    );

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
        "id", "name", "f_lo", "f_hi", "t_lo", "t_hi", "notes", "tags", "watch", "links", "created",
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

    // T-166 (ADR-0013 §4.9 gap 9): the region watch is armed and disarmed on the selection, and
    // its report discloses both the alerts it raised and the activity it did not alert on.
    let (st, unwatched) = get(addr, &format!("/api/selections/{id}/watch"));
    assert_eq!(st, 200, "{unwatched}");
    for field in [
        "selection_id",
        "watch",
        "armed",
        "alerts",
        "suppressed",
        "alerted_total",
        "suppressed_total",
    ] {
        assert!(
            unwatched.get(field).is_some(),
            "watch report missing {field}: {unwatched}"
        );
    }
    assert!(
        unwatched["watch"].is_null(),
        "no watch until armed: {unwatched}"
    );
    assert_eq!(unwatched["armed"], false, "{unwatched}");
    assert!(
        is_array(&unwatched["alerts"]) && is_array(&unwatched["suppressed"]),
        "{unwatched}"
    );

    let (st, armed) = put(
        addr,
        &format!("/api/selections/{id}"),
        r#"{"watch": {"enabled": true}}"#,
    );
    assert_eq!(
        (st, armed["watch"]["enabled"].as_bool()),
        (200, Some(true)),
        "{armed}"
    );
    let (st, report) = get(addr, &format!("/api/selections/{id}/watch"));
    assert_eq!(
        (st, report["armed"].as_bool()),
        (200, Some(true)),
        "{report}"
    );

    // Reversible, and never an automatic action: disarming clears the watch and keeps everything
    // else about the selection, including any alert already raised.
    let (st, off) = put(addr, &format!("/api/selections/{id}"), r#"{"watch": null}"#);
    assert_eq!(st, 200, "{off}");
    assert!(off["watch"].is_null(), "disarming clears the watch: {off}");
    assert_eq!(off["name"].as_str(), Some("renamed band"), "{off}");
    let (st, report) = get(addr, &format!("/api/selections/{id}/watch"));
    assert_eq!(
        (st, report["armed"].as_bool()),
        (200, Some(false)),
        "{report}"
    );

    // There is no threshold to set: the watch takes an `enabled` flag and nothing else.
    for bad in [
        r#"{"watch": {"enabled": "yes"}}"#,
        r#"{"watch": {"bogus": true}}"#,
        r#"{"watch": {"enabled": true, "min_snr_db": 12}}"#,
        r#"{"watch": true}"#,
    ] {
        let (st, v) = put(addr, &format!("/api/selections/{id}"), bad);
        assert_eq!(st, 400, "{bad}: {v}");
    }
    let unknown = SelectionId::new();
    let (st, v) = get(addr, &format!("/api/selections/{unknown}/watch"));
    assert_eq!((st, v["code"].as_str()), (404, Some("not_found")), "{v}");
    let (st, _) = call(
        addr,
        "POST",
        &format!("/api/selections/{id}/watch"),
        Some(&format!("Bearer {TOKEN}")),
        Some("{}"),
    );
    assert_eq!(st, 405);

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

/// A POST that keeps the response head: `(status, head, body)`.
fn post_with_head(addr: SocketAddr, path: &str, body: &str) -> (u16, String, Value) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    write!(
        s,
        "POST {path} HTTP/1.1\r\nHost: t\r\nAuthorization: Bearer {TOKEN}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw.as_str(), ""));
    let status = head[9..12].parse().unwrap();
    (
        status,
        head.to_owned(),
        serde_json::from_str(body).unwrap_or(Value::Null),
    )
}

/// Polls `GET /api/analyze/{id}` until the job has finished; the finished job.
fn wait_job(addr: SocketAddr, id: &str) -> Value {
    let mut last = Value::Null;
    wait_for(
        "the analyze job to finish",
        Duration::from_secs(120),
        || {
            let (st, v) = get(addr, &format!("/api/analyze/{id}"));
            assert_eq!(st, 200, "{v}");
            let done = v["ended"].is_number()
                && matches!(v["state"].as_str(), Some("done" | "failed" | "cancelled"));
            last = v;
            done
        },
    );
    last
}

/// T-190/T-546/T-859 (MAUTO M-8, ADR-0015 §5.1–§5.2, ADR-0021 §4, §7A.4): `/api/analyze` jobs over
/// the served run's IQ ring, and an emitter's persisted analysis.
///
/// - A **band** or **selection** target (or an emitter plus a job field) answers `202 {"job"}`
///   with `Location: /api/analyze/{id}`; the job acquires from the ring — its `window` is what was
///   read — and, because stage evaluation over IQ (MAUTO M-2) is not built, ends `failed` with
///   `error.code: "no_evaluator"` and `resolution.kind: "not-searched"`, never `unknown`.
/// - A bare **`{"emitter_id"}`** still answers `200` with the persisted analysis (T-546), and an
///   emitter nothing has analysed says `not-searched`.
/// - `GET /api/analyze` lists newest first; `GET /api/analyze/{id}/trace` is the trace fetch;
///   `DELETE` cancels or forgets, audited `analyze_cancel`; a forgotten id is `410 gone`, an
///   unissued one `404 not_found`; a window older than the ring is `410 evicted`.
/// - `/ws/analyze/{id}` streams `hackriff.analyze/1` and ends with the `done` record.
#[test]
fn analyze_jobs_run_over_the_ring_and_the_emitter_read_distinguishes_not_searched() {
    let (_dir_guard, serving, addr) = start_server();
    let (lo, hi) = (STATION_HZ - 100e3, STATION_HZ + 100e3);

    // ---- a band job: 202, Location, then acquire → no_evaluator ----
    let (st, head, v) = post_with_head(
        addr,
        "/api/analyze",
        &json!({ "band": { "f_lo": lo, "f_hi": hi }, "profile": "quick", "live_s": 1.0 })
            .to_string(),
    );
    assert_eq!(st, 202, "{v}");
    let id = v["job"]["id"].as_str().unwrap().to_owned();
    assert!(
        head.lines()
            .any(|l| l == format!("Location: /api/analyze/{id}")),
        "{head}"
    );
    assert_eq!(v["job"]["profile"], json!("quick"));
    assert_eq!(
        v["job"]["target"],
        json!({ "band": { "f_lo": lo, "f_hi": hi } })
    );
    // The stream, opened while the job is young, ends with `done`.
    let mut ws = connect_ws(addr, &format!("/ws/analyze/{id}?token={TOKEN}")).unwrap();
    let header: Value = match ws.read().unwrap() {
        Message::Text(t) => serde_json::from_str(t.as_str()).unwrap(),
        other => panic!("expected the header, got {other:?}"),
    };
    assert_eq!(header["kind"], json!("messages"));
    assert_eq!(header["message_schema"], json!("hackriff.analyze/1"));
    let job = wait_job(addr, &id);
    assert_eq!(job["state"], json!("failed"), "{job}");
    assert_eq!(job["error"]["code"], json!("no_evaluator"), "{job}");
    assert_eq!(job["window"]["source"], json!("live"), "{job}");
    assert!(
        job["window"]["samples"].as_u64().is_some_and(|n| n > 0),
        "coverage is what was read: {job}"
    );
    assert_eq!(job["resolution"]["kind"], json!("not-searched"), "{job}");
    // T-860 (MAUTO M-9): a job that searched nothing attached nothing — `decodes` and `confirm`
    // are present and null; only a `done` job states a confirm outcome.
    for key in ["decodes", "confirm"] {
        assert!(job.get(key).is_some_and(Value::is_null), "{key}: {job}");
    }
    assert!(
        job["channel"]["center_hz"]
            .as_f64()
            .is_some_and(|c| (c - STATION_HZ).abs() < 1.0),
        "{job}"
    );
    let mut kinds = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline && kinds.last() != Some(&"done".to_owned()) {
        match ws.read() {
            Ok(Message::Text(t)) => {
                let r: Value = serde_json::from_str(t.as_str()).unwrap();
                if let Some(k) = r["metadata"]["type"].as_str() {
                    assert_eq!(r["metadata"]["job_id"], json!(id), "{r}");
                    kinds.push(k.to_owned());
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    assert_eq!(kinds.last().map(String::as_str), Some("done"), "{kinds:?}");
    let _ = ws.close(None);

    // The trace fetch answers for a job that searched nothing: no nodes, and it says so.
    let (st, t) = get(addr, &format!("/api/analyze/{id}/trace?limit=10"));
    assert_eq!(st, 200, "{t}");
    assert_eq!(t["nodes"], json!([]), "{t}");
    assert_eq!(t["job_id"], json!(id), "{t}");
    // T-930: this job ENDED (failed) without ever producing a trace, so the fetch is `final` —
    // otherwise a watching client polls it every 2 s for as long as it is on screen.
    assert_eq!(
        t["final"],
        json!(true),
        "a job that ended without a trace is final: {t}"
    );
    // Every filter is parsed, and an unknown VALUE is `400 invalid` — never silently ignored,
    // which would answer a different question from the one asked (ADR-0021 §4.2).
    for good in [
        "stage=S1",
        "outcome=pruned_floor",
        "family=fsk",
        "tried=false",
        "limit=512",
        "stage=S3&outcome=deferred_budget&family=psk&tried=false&limit=8",
    ] {
        let (st, t) = get(addr, &format!("/api/analyze/{id}/trace?{good}"));
        assert_eq!(st, 200, "{good}: {t}");
        assert_eq!(t["nodes"], json!([]), "{good}: {t}");
    }
    for bad in [
        "stage=S9",
        "outcome=pruned-floor",
        "outcome=nope",
        "tried=maybe",
        "limit=513",
        "limit=0",
        "limit=lots",
        "bogus=1",
    ] {
        let (st, t) = get(addr, &format!("/api/analyze/{id}/trace?{bad}"));
        assert_eq!(
            (st, t["code"].as_str()),
            (400, Some("invalid")),
            "{bad}: {t}"
        );
    }

    // ---- an explicit window older than the ring: 410 evicted, and no job ----
    let (st, v) = post(
        addr,
        "/api/analyze",
        r#"{"band": {"f_lo": 101.2e6, "f_hi": 101.4e6, "t_lo": 0.0, "t_hi": 10.0}}"#,
    );
    assert_eq!((st, v["code"].as_str()), (410, Some("evicted")), "{v}");

    // ---- a selection job ----
    let (st, s) = post(
        addr,
        "/api/selections",
        &json!({ "name": "analyze target", "f_lo": lo, "f_hi": hi }).to_string(),
    );
    assert_eq!(st, 201, "{s}");
    let selection_id = s["id"].as_str().unwrap().to_owned();
    let (st, v) = post(
        addr,
        "/api/analyze",
        &json!({ "selection_id": selection_id, "live_s": 1.0 }).to_string(),
    );
    assert_eq!(st, 202, "{v}");
    let sel_job = v["job"]["id"].as_str().unwrap().to_owned();
    assert_eq!(
        v["job"]["profile"],
        json!("standard"),
        "the default profile"
    );

    // ---- the emitter: the bare read, and a job ----
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
    assert_eq!(st, 200, "{v}");
    assert_eq!(
        v["resolution"]["kind"],
        json!("not-searched"),
        "nothing has analysed this emitter, and saying so is not the same as saying `unknown`: \
         {v}",
    );
    assert!(v["pipeline"].is_null(), "{v}");
    assert_eq!(
        v["resolution"]["reason"],
        json!(null),
        "an aborted-or-absent look rules nothing out: {v}"
    );
    // T-567 (ADR-0021 §7A.6): `suspected` belongs to `unsupported-structure` and to no other
    // kind. A look that never happened suspects nothing, and must not appear to.
    assert!(
        v["resolution"].get("suspected").is_none(),
        "not-searched names no missing block: {v}"
    );
    // T-884 item 6 (docs/api.md): this is also the shape a **withheld-identity** emitter gets,
    // whatever storage holds for it — the same withholding `/api/inventory` applies to
    // `synthesis` (T-159/T-163), and with no marker that anything was held back, since such a
    // marker is itself an oracle. The withheld case is asserted where an identity can be gated
    // without switching process-wide content gating under every other test in this binary
    // (`hk_api::analyze`'s `t884_the_analyze_read_withholds_the_analysis_of_a_withheld_identity`);
    // here the contract records that the two answers are **the same four fields and no others**.
    assert_eq!(
        v.as_object().map(|o| o.len()),
        Some(5),
        "the not-searched answer is exactly emitter_id, pipeline, evidence, trace, resolution: {v}"
    );
    for absent in ["job", "verdict", "stage_reached", "engine", "receiver"] {
        assert!(
            v.get(absent).is_none(),
            "{absent} is not part of the not-searched answer: {v}"
        );
    }
    let (st, v) = post(
        addr,
        "/api/analyze",
        &json!({ "emitter_id": emitter_id, "profile": "quick", "source": "live", "live_s": 1.0 })
            .to_string(),
    );
    assert_eq!(st, 202, "{v}");
    let em_job = v["job"]["id"].as_str().unwrap().to_owned();
    assert_eq!(v["job"]["emitter_id"], json!(emitter_id), "{v}");

    // ---- list, newest first; a state filter; a bad state ----
    let (st, l) = get(addr, "/api/analyze");
    assert_eq!(st, 200, "{l}");
    let ids: Vec<&str> = l["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|j| j["id"].as_str())
        .collect();
    assert_eq!(ids, [em_job.as_str(), sel_job.as_str(), id.as_str()], "{l}");
    let (st, l) = get(addr, "/api/analyze?state=failed");
    assert_eq!(st, 200, "{l}");
    assert!(
        l["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .all(|j| j["state"] == json!("failed")),
        "{l}"
    );
    let (st, v) = get(addr, "/api/analyze?state=sleeping");
    assert_eq!((st, v["code"].as_str()), (400, Some("invalid")), "{v}");

    // ---- cancel: a queued or running job becomes `cancelled` at once ----
    let (st, v) = delete(addr, &format!("/api/analyze/{em_job}"));
    assert_eq!(st, 200, "{v}");
    if v["forgotten"] == json!(false) {
        // Queued or running when the DELETE landed: cancelled, finally, at once.
        assert_eq!(v["job"]["state"], json!("cancelled"), "{v}");
        assert_eq!(wait_job(addr, &em_job)["state"], json!("cancelled"));
    }
    wait_job(addr, &sel_job);
    // A finished job is forgotten by DELETE: then it is `410 gone`, never `404`.
    let (st, v) = delete(addr, &format!("/api/analyze/{id}"));
    assert_eq!((st, v["forgotten"].as_bool()), (200, Some(true)), "{v}");
    let (st, v) = get(addr, &format!("/api/analyze/{id}"));
    assert_eq!((st, v["code"].as_str()), (410, Some("gone")), "{v}");
    let (st, v) = get(addr, &format!("/api/analyze/{id}/trace"));
    assert_eq!((st, v["code"].as_str()), (410, Some("gone")), "{v}");
    for unissued in ["a999", "x1", "a0"] {
        let (st, v) = get(addr, &format!("/api/analyze/{unissued}"));
        assert_eq!((st, v["code"].as_str()), (404, Some("not_found")), "{v}");
    }

    // ---- the audit log names the actions ----
    let audit = std::fs::read_to_string(serving.handle.data_dir().join("control-audit.jsonl"))
        .unwrap_or_default();
    let actions: Vec<String> = audit
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter_map(|e| e["action"].as_str().map(str::to_owned))
        .collect();
    for want in ["analyze_start", "analyze_cancel", "analyze"] {
        assert!(actions.iter().any(|a| a == want), "{want}: {actions:?}");
    }

    // ---- 400 invalid ----
    for bad in [
        r#"{}"#,
        r#"{"band": {"f_lo": 1e6, "f_hi": 2e6}, "emitter_id": "x"}"#,
        r#"{"selection_id": "x", "extra": 1}"#,
        r#"{"band": {"f_lo": 2e6, "f_hi": 1e6}}"#,
        r#"{"band": {"f_lo": 1e6, "f_hi": 1e6}}"#,
        r#"{"band": {"f_hi": 1e6}}"#,
        r#"{"band": {"f_lo": 1e6, "f_hi": 2e6, "extra": 1}}"#,
        r#"{"band": {"f_lo": 1e6, "f_hi": 2e6, "t_lo": 5.0}}"#,
        r#"{"band": "nope"}"#,
        r#"{"band": {"f_lo": 1e6, "f_hi": 2e6}, "profile": "turbo"}"#,
        r#"{"band": {"f_lo": 1e6, "f_hi": 2e6}, "source": "tape"}"#,
        r#"{"band": {"f_lo": 1e6, "f_hi": 2e6}, "live_s": 60}"#,
        r#"{"band": {"f_lo": 1e6, "f_hi": 2e6}, "templates": {"only": "x"}}"#,
    ] {
        let (st, v) = post(addr, "/api/analyze", bad);
        assert_eq!(
            (st, v["code"].as_str()),
            (400, Some("invalid")),
            "{bad}: {v}"
        );
    }

    // ---- 404 not_found: an unknown (but well-formed) selection/emitter id, and a malformed one
    // (the same shape a malformed `/api/inventory/{id}` path answers with) ----
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

    // ---- 409 outside_window: a live job whose band the tuned window cannot contain ----
    let (st, v) = post(
        addr,
        "/api/analyze",
        r#"{"band": {"f_lo": 2.0e9, "f_hi": 2.001e9}, "source": "live"}"#,
    );
    assert_eq!(
        (st, v["code"].as_str()),
        (409, Some("outside_window")),
        "{v}"
    );

    // ---- 401: no token; mutating requests need the header ----
    let (st, v) = call(
        addr,
        "POST",
        "/api/analyze",
        None,
        Some(r#"{"band": {"f_lo": 1e6, "f_hi": 2e6}}"#),
    );
    assert_eq!(st, 401, "{v}");
    let (st, v) = call(
        addr,
        "DELETE",
        &format!("/api/analyze/{sel_job}"),
        None,
        None,
    );
    assert_eq!(st, 401, "{v}");

    // ---- wrong method: 405 with Allow ----
    let (st, v) = put(addr, "/api/analyze", "{}");
    assert_eq!(st, 405, "{v}");
    let (st, v) = post(addr, &format!("/api/analyze/{sel_job}"), "{}");
    assert_eq!(st, 405, "{v}");

    stop_server(serving);
}

/// T-157: the rolling IQ capture buffer of `hk serve` over the mock SDR device fills with no
/// request; `GET /api/iqbuffer` reports its span, quota, segments (tuning and gain) and counts, and
/// `POST /api/iqbuffer/clip` exports a span as a SigMF recording, as `docs/api.md` "IQ capture
/// buffer" documents; bad queries and bodies answer 400, an empty band 404, other methods 405,
/// and the clip needs the header token.
///
/// T-469 rides on the same exported clip, because a clip *is* a persisted IQ recording: the tail
/// of this test asserts `GET /api/recordings` enumerates it with its extent, tuning, device and
/// on-disk state, and that truncating and then removing its data file moves it from `complete` to
/// `partial` to `missing` — out of the audio horizon each time.
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
    // T-325: bias-tee state rides on the segment beside antenna_port and overload as device-local
    // trust context. The mock reports it, so assert the value rather than merely the shape: a
    // segment that said "unknown" here would mean the state never reached the API.
    assert_eq!(seg["bias_tee"], json!("off"), "{seg}");

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

    // ---- T-469: `GET /api/recordings` enumerates what that clip persisted -------------------
    //
    // The ring answers "where is IQ still buffered"; this answers "where is IQ still on disk",
    // and only the two together are the audio horizon. `docs/api.md` "Persisted IQ recordings".
    let (st, list) = get(addr, "/api/recordings");
    assert_eq!(st, 200, "{list}");
    // Two clips were exported above (by index and by ns), so at least two rows exist.
    assert!(list["matched"].as_u64().unwrap() >= 2, "{list}");
    assert_eq!(
        list["count"].as_u64(),
        list["recordings"].as_array().map(|a| a.len() as u64),
        "{list}"
    );
    let find = |v: &Value, want: &str| -> Value {
        v["recordings"]
            .as_array()
            .expect("recordings")
            .iter()
            .find(|e| e["id"].as_str() == Some(want))
            .unwrap_or_else(|| panic!("{want} not listed: {v}"))
            .clone()
    };
    let e = find(&list, id);
    // The same extent, tuning and URIs the clip export reported: one recording, one truth.
    assert_eq!(e["kind"], json!("iq-snippet"), "{e}");
    assert_eq!(e["iq"], json!(true), "{e}");
    assert_eq!(e["t0_ns"], r["t0_ns"], "{e}");
    assert_eq!(e["t1_ns"], r["t1_ns"], "{e}");
    assert_eq!(e["t0"], r["t0"], "{e}");
    assert_eq!(e["duration_s"], json!(0.1), "{e}");
    assert_eq!(e["center_hz"], json!(FIXTURE_CENTER_HZ), "{e}");
    assert_eq!(e["sample_rate_hz"], json!(2.4e6), "{e}");
    assert_eq!(e["f_lo"], json!(FIXTURE_CENTER_HZ - 1.2e6), "{e}");
    assert_eq!(e["f_hi"], json!(FIXTURE_CENTER_HZ + 1.2e6), "{e}");
    assert_eq!(e["meta_uri"], r["meta_uri"], "{e}");
    assert_eq!(e["data_uri"], r["data_uri"], "{e}");
    assert_eq!(e["trigger"], json!({"kind": "manual"}), "{e}");
    assert_eq!(e["retention_class"], json!("pinned"), "{e}");
    assert_eq!(e["content_class"], r["content_class"], "{e}");
    // Provenance: which front end captured it, and under what state. The mock reports its bias
    // tee, so assert the value - "unknown" here would mean it never reached the route.
    assert_eq!(e["device_id"], seg["device_id"], "{e}");
    assert_eq!(e["bias_tee"], json!("off"), "{e}");
    assert_eq!(e["bandwidth_hz"], seg["bandwidth_hz"], "{e}");
    assert_eq!(e["lna_db"], seg["lna_db"], "{e}");
    assert_eq!(e["vga_db"], seg["vga_db"], "{e}");
    assert_eq!(e["overload"], json!(false), "{e}");
    // What is on disk now: exactly the bytes the row records (2 per ci8 sample).
    assert_eq!(e["size_bytes"].as_u64(), Some(2 * n), "{e}");
    assert_eq!(e["bytes_on_disk"].as_u64(), Some(2 * n), "{e}");
    assert_eq!(e["state"], json!("complete"), "{e}");
    assert_eq!(e["available"], json!(true), "{e}");
    assert_eq!(e["meta_present"], json!(true), "{e}");
    assert!(e["detail"].is_null(), "{e}");

    // The horizon is the ring PLUS these recordings, as separate labelled spans - never merged
    // into one envelope over a gap that has no IQ in it.
    let iq = &list["iq_available"];
    assert_eq!(iq["horizon"], json!("iq-ring + recordings"), "{iq}");
    assert_eq!(iq["ring"]["enabled"], json!(true), "{iq}");
    assert!(
        iq["ring"]["t0"].is_f64() && iq["ring"]["t1"].is_f64(),
        "{iq}"
    );
    assert!(iq.get("t0").is_none(), "no envelope over the gaps: {iq}");
    let spans = iq["spans"].as_array().expect("spans");
    let mine = spans
        .iter()
        .find(|s| s["recording"].as_str() == Some(id))
        .unwrap_or_else(|| panic!("{id} has no span: {iq}"));
    assert_eq!(mine["source"], json!("recording"), "{mine}");
    assert_eq!(mine["t0_ns"], r["t0_ns"], "{mine}");
    assert_eq!(mine["span_s"], json!(0.1), "{mine}");
    assert_eq!(
        spans
            .iter()
            .filter(|s| s["source"] == json!("ring"))
            .count(),
        1,
        "{iq}"
    );
    // Spans are oldest first, on the one shared time axis, whatever their source.
    let mut ordered: Vec<i64> = spans.iter().map(|s| s["t0_ns"].as_i64().unwrap()).collect();
    let given = ordered.clone();
    ordered.sort_unstable();
    assert_eq!(given, ordered, "{iq}");

    // A kind filter narrows the query, not just the page: no audio has been recorded here.
    let (st, none) = get(addr, "/api/recordings?kind=audio");
    assert_eq!(st, 200, "{none}");
    assert_eq!(none["matched"], json!(0), "{none}");
    assert!(none["recordings"].as_array().unwrap().is_empty(), "{none}");
    assert!(
        none["iq_available"]["spans"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["source"] == json!("ring")),
        "{none}"
    );
    // The window is an overlap test: a window ending a second before the clip starts excludes
    // it. (A second, not zero: `t0`/`t1` are Unix SECONDS, and an f64 near today's epoch resolves
    // only ~240 ns, so a boundary given in seconds is not exact to the sample - as the clip
    // route's own `{t0, t1}` form documents.)
    let before = format!("/api/recordings?t1={}", r["t0"].as_f64().unwrap() - 1.0);
    let (st, v) = get(addr, &before);
    assert_eq!(st, 200, "{v}");
    assert!(
        !v["recordings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["id"].as_str() == Some(id)),
        "{v}"
    );
    // A page smaller than the catalogue says what it left out.
    let (st, page) = get(addr, "/api/recordings?limit=1");
    assert_eq!(st, 200, "{page}");
    assert_eq!(page["count"], json!(1), "{page}");
    assert_eq!(
        page["omitted"].as_u64(),
        Some(page["matched"].as_u64().unwrap() - 1),
        "{page}"
    );

    // HONESTY: a partially written file is listed, and is NOT available. Truncating the data
    // file is exactly what an interrupted recording leaves behind.
    let data_path = r["data_path"].as_str().unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(data_path)
        .unwrap()
        .set_len(n) // half the bytes: 1 of the 2 per sample
        .unwrap();
    let (st, after) = get(addr, "/api/recordings");
    assert_eq!(st, 200, "{after}");
    let e = find(&after, id);
    assert_eq!(e["state"], json!("partial"), "{e}");
    assert_eq!(e["available"], json!(false), "{e}");
    assert_eq!(e["bytes_on_disk"].as_u64(), Some(n), "{e}");
    assert_eq!(e["size_bytes"].as_u64(), Some(2 * n), "{e}");
    assert!(
        e["detail"].as_str().unwrap().contains("partially written"),
        "{e}"
    );
    assert!(
        !after["iq_available"]["spans"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["recording"].as_str() == Some(id)),
        "a truncated recording must not extend the audio horizon: {after}"
    );

    // ...and a file that is gone is `missing`, never a silently available row.
    std::fs::remove_file(data_path).unwrap();
    let (st, gone) = get(addr, "/api/recordings");
    assert_eq!(st, 200, "{gone}");
    let e = find(&gone, id);
    assert_eq!(e["state"], json!("missing"), "{e}");
    assert_eq!(e["available"], json!(false), "{e}");
    assert!(e["bytes_on_disk"].is_null(), "{e}");
    assert_eq!(e["meta_present"], json!(true), "{e}");
    assert!(e["detail"].as_str().unwrap().contains("not on disk"), "{e}");

    for q in [
        "bogus=1",
        "limit=0",
        "limit=1001",
        "limit=x",
        "t0=abc",
        "t0=-5",
        "t0=5&t1=4",
        "kind=video",
        "kind=iq",
    ] {
        let (st, v) = get(addr, &format!("/api/recordings?{q}"));
        assert_eq!((st, v["code"].as_str()), (400, Some("invalid")), "{q}: {v}");
    }
    let (st, _) = post(addr, "/api/recordings", "{}");
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

/// T-844: `GET /api/ml/models`, `PUT /api/ml/models/{id}/mode` and `GET /api/ml/shadow`, over a
/// data directory seeded with an installed probe model and one record already in the durable
/// shadow log — so the served shapes are asserted on real content, not on empty arrays — and the
/// documented refusals: `active` without §4.6 evidence is 409 `needs_evidence`, a forced one is
/// recorded and audited, an unauthenticated change is 401, bad queries are 400.
#[test]
fn ml_models_modes_and_the_shadow_log_answer_as_documented() {
    let dir = temp_data_dir();
    // Guarded from creation (T-232), not only from `start_server_in` below: the probe model and the
    // seeded shadow record are written into it first.
    let _setup_guard = TempDataDirGuard::new(dir.clone());
    std::fs::create_dir_all(&dir).unwrap();
    let probe = hk_pipeline::ml::install_probe_model(&dir, "fsk", &["2fsk", "gfsk", "msk", "4fsk"])
        .unwrap();
    let t_ns = 1_789_300_820_000_000_000_i64;
    {
        use hk_store::ml::*;
        let store = ShadowStore::open(hk_pipeline::ml::shadow_dir(&dir)).unwrap();
        store
            .append(&ShadowRecord {
                schema: SHADOW_SCHEMA,
                t: hk_model::Timestamp::from_unix_nanos(t_ns),
                model: probe.to_string(),
                consumer: hk_pipeline::ml::DL_CONSUMER.into(),
                subject: ShadowSubject {
                    kind: "detection".into(),
                    detection: "det-contract".into(),
                },
                snr_db: Some(22.0),
                snr_bin_db: snr_bin_db(Some(22.0)),
                prediction: ShadowPrediction {
                    label: "gfsk".into(),
                    p: 0.6,
                    energy: -3.0,
                    unknown_score: 0.2,
                    provider: "cpu-mlp".into(),
                    precision: "fp32".into(),
                    latency_ms: 0.1,
                    batch_size: 1,
                    mode: "shadow".into(),
                },
                classical: ClassicalDecision {
                    family: "fsk".into(),
                    class: Some("2fsk".into()),
                    class_p: Some(0.7),
                    confidence: 0.8,
                    open_set_score: 0.1,
                    stage: "feature-tree".into(),
                },
            })
            .unwrap();
    }
    let (_dir_guard, serving, addr) = start_server_in(dir.clone(), None);

    let (st, v) = get(addr, "/api/ml/models");
    assert_eq!(st, 200, "{v}");
    let models = v["models"].as_array().unwrap();
    assert_eq!(models.len(), 1, "{v}");
    let m = &models[0];
    assert_eq!(m["id"], json!("probe-fsk"), "{m}");
    assert_eq!(m["family"], json!("fsk"), "{m}");
    assert_eq!(m["consumer"], json!("hk-classify/dl"), "{m}");
    assert_eq!(m["format"], json!("hk-mlp@1"), "{m}");
    assert_eq!(m["enable_evidence"], Value::Null, "{m}");
    assert_eq!(
        (m["loaded"].as_bool(), m["modes"].clone()),
        (Some(false), json!([]))
    );
    assert_eq!(v["hosts"].as_array().unwrap().len(), 2, "{v}");
    assert!(v["hosts"][0]["stats"]["shadow_records"].is_u64(), "{v}");
    assert_eq!(v["producer"]["consumer"], json!("hk-classify/dl"), "{v}");
    assert!(v["producer"]["offered"].is_u64(), "{v}");
    assert!(is_array(&v["restore_errors"]), "{v}");

    // Installed is not on: a change of mode is the operator's act, and it is authenticated.
    let path = "/api/ml/models/probe-fsk/mode";
    let (st, _) = call(addr, "PUT", path, None, Some(r#"{"mode":"shadow"}"#));
    assert_eq!(st, 401);
    let (st, v) = put(addr, path, r#"{"mode":"shadow"}"#);
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["mode"]["mode"], json!("shadow"), "{v}");
    assert_eq!(v["mode"]["previous"], json!("off"), "{v}");
    assert_eq!(v["mode"]["forced"], json!(false), "{v}");
    assert_eq!(v["mode"]["provider"], json!("cpu-mlp"), "{v}");
    let (_, v) = get(addr, "/api/ml/models");
    assert_eq!(v["models"][0]["loaded"], json!(true), "{v}");
    assert_eq!(
        v["models"][0]["modes"],
        json!([{"consumer": "hk-classify/dl", "mode": "shadow", "forced": false}])
    );

    // `active` needs the §4.6 evidence; `force` gets past it and says so.
    let (st, v) = put(addr, path, r#"{"mode":"active"}"#);
    assert_eq!(
        (st, v["code"].as_str()),
        (409, Some("needs_evidence")),
        "{v}"
    );
    let (st, v) = put(addr, path, r#"{"mode":"active","force":true}"#);
    assert_eq!(st, 200, "{v}");
    assert_eq!(
        (v["mode"]["mode"].clone(), v["mode"]["forced"].clone()),
        (json!("active"), json!(true))
    );
    let (st, v) = put(addr, path, r#"{"mode":"off"}"#);
    assert_eq!(st, 200, "{v}");
    let audit = std::fs::read_to_string(dir.join("control-audit.jsonl")).unwrap();
    let ml: Vec<Value> = audit
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|e| e["action"] == json!("ml_mode"))
        .collect();
    assert_eq!(
        ml.len(),
        4,
        "every change, refused or not, is audited: {audit}"
    );
    assert!(
        ml.iter()
            .any(|e| e["new"]["forced"] == json!(true) && e["result"] == json!("ok")),
        "the forced change is in the audit log: {ml:?}"
    );

    for (body, status, code) in [
        (r#"{}"#, 400, "invalid"),
        (r#"{"mode":"on"}"#, 400, "invalid"),
        (r#"{"mode":"shadow","evidence":"trust me"}"#, 400, "invalid"),
        (
            r#"{"mode":"shadow","consumer":"someone-else"}"#,
            400,
            "invalid",
        ),
    ] {
        let (st, v) = put(addr, path, body);
        assert_eq!(
            (st, v["code"].as_str()),
            (status, Some(code)),
            "{body}: {v}"
        );
    }
    let (st, v) = put(
        addr,
        "/api/ml/models/no-such-model/mode",
        r#"{"mode":"shadow"}"#,
    );
    assert_eq!((st, v["code"].as_str()), (404, Some("not_found")), "{v}");

    // The durable log, served with its per-SNR agreement.
    let (st, v) = get(addr, "/api/ml/shadow");
    assert_eq!(st, 200, "{v}");
    let r = &v["records"][0];
    assert_eq!(r["model"], json!(probe.to_string()), "{r}");
    assert_eq!(r["subject"]["detection"], json!("det-contract"), "{r}");
    assert_eq!(r["prediction"]["mode"], json!("shadow"), "{r}");
    assert_eq!(r["classical"]["class"], json!("2fsk"), "{r}");
    assert_eq!(r["agrees"], json!(false), "{r}");
    assert_eq!(r["t_s"].as_f64(), Some(t_ns as f64 / 1e9), "{r}");
    assert_eq!(r["t_ns"].as_i64(), Some(t_ns), "{r}");
    assert!(
        r.get("t").is_none(),
        "nanoseconds are served under `_ns` only: {r}"
    );
    let a = &v["aggregates"][0];
    assert_eq!(
        (
            a["family"].clone(),
            a["snr_bin_db"].clone(),
            a["n"].clone(),
            a["compared"].clone(),
            a["agree"].clone()
        ),
        (json!("fsk"), json!(20), json!(1), json!(1), json!(0)),
        "{a}"
    );
    assert_eq!(v["store"]["records"], json!(1), "{v}");
    assert_eq!(v["store"]["max_bytes"], json!(256_u64 << 20), "{v}");
    let (_, v) = get(addr, "/api/ml/shadow?family=analog");
    assert_eq!(
        (v["records"].clone(), v["aggregates"].clone()),
        (json!([]), json!([])),
        "{v}"
    );
    let (_, v) = get(
        addr,
        "/api/ml/shadow?model=probe-fsk&t0=1789300800&t1=1789300900&limit=5",
    );
    assert_eq!(v["records"].as_array().unwrap().len(), 1, "{v}");
    for q in ["t0=abc", "t0=20&t1=10", "limit=0", "limit=5000", "model="] {
        let (st, v) = get(addr, &format!("/api/ml/shadow?{q}"));
        assert_eq!((st, v["code"].as_str()), (400, Some("invalid")), "{q}: {v}");
    }

    let (st, _) = post(addr, "/api/ml/models", "{}");
    assert_eq!(st, 405);
    let (st, _) = put(addr, "/api/ml/shadow", "{}");
    assert_eq!(st, 405);
    let (st, _) = get(addr, "/api/ml/models/probe-fsk/mode");
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

    // T-337 (the user's "one shared time axis" invariant): every time-varying record the backend
    // serves carries absolute capture time, so the client can place it instead of inferring it from
    // arrival order, a sequence number, or a row count. For the waterfall's rows that is the binary
    // record header — and it is asserted here by VALUE, not by shape (T-315), because the values are
    // the whole contract: `t` must be a real Unix-epoch instant on the capture clock, and it must
    // agree with `sample_index` over the declared bandwidth, which is what makes it *capture* time
    // rather than a timestamp taken when the row happened to be encoded or to arrive.
    let fs = header["bandwidth_hz"].as_f64().expect("bandwidth_hz");
    let declared_hz = header["sample_rate_hz"].as_f64().expect("sample_rate_hz");
    let mut rows: Vec<(i64, u64)> = Vec::new();
    while rows.len() < 6 {
        let Ok(Message::Binary(b)) = ws.read() else {
            continue;
        };
        assert!(b.len() >= 32, "a binary record carries the 32-byte header");
        if b[0] != 1 {
            continue; // not a data record (a drop marker); its timestamp is checked by kind above
        }
        let t = i64::from_le_bytes(b[16..24].try_into().unwrap());
        let sample_index = u64::from_le_bytes(b[24..32].try_into().unwrap());
        rows.push((t, sample_index));
    }
    for (t, _) in &rows {
        assert!(
            *t > 1_700_000_000_000_000_000,
            "row timestamps are absolute Unix nanoseconds, not an offset or a counter: {t}"
        );
    }
    for w in rows.windows(2) {
        let (t0, i0) = w[0];
        let (t1, i1) = w[1];
        assert!(t1 > t0 && i1 > i0, "rows advance in time and in samples");
        // The row's time IS its sample index on the capture clock: t advances by exactly the
        // samples between the rows over the sample rate. This is what a client anchors a box to.
        let from_samples = (i1 - i0) as f64 / fs * 1e9;
        let measured = (t1 - t0) as f64;
        assert!(
            (measured - from_samples).abs() <= 1.0 + from_samples * 1e-9,
            "t must be sample_index on the capture clock: {measured} ns vs {from_samples} ns"
        );
    }
    // And the declared row rate is NOT that clock: it is a rate the producer declares (on a gated
    // class, deliberately above the actual row rate — `hk_pipeline::class::RowPlan::declared_hz`),
    // so it may never be used to place a row or an overlay in time. Asserted as the documented
    // direction: the rows never arrive faster than declared, and the gap may be real.
    let observed_hz = (rows.len() - 1) as f64 * 1e9 / (rows[rows.len() - 1].0 - rows[0].0) as f64;
    assert!(
        observed_hz <= declared_hz * 1.02,
        "declared {declared_hz} rows/s must bound the observed {observed_hz} rows/s"
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

/// One thing a stream consumer saw, in arrival order.
#[derive(Debug)]
enum Saw {
    /// A data record: its capture time (ns), its stream sample index, and whether it carries
    /// `DISCONTINUITY` (the rows before it are not contiguous with it).
    Row(i64, u64, bool),
    /// A text message that parsed as JSON — on a binary stream, a stream header.
    Header(Value),
}

/// What a consumer saw on its socket over one window of time.
#[derive(Debug, Default)]
struct Seen {
    saw: Vec<Saw>,
    closed: Option<String>,
}

impl Seen {
    fn rows(&self) -> usize {
        self.saw
            .iter()
            .filter(|s| matches!(s, Saw::Row(..)))
            .count()
    }

    /// Everything seen after the first header satisfying `pick` (none if no such header arrived).
    fn after_header(&self, pick: impl Fn(&Value) -> bool) -> Option<(&Value, &[Saw])> {
        let i = self
            .saw
            .iter()
            .position(|s| matches!(s, Saw::Header(h) if pick(h)))?;
        let Saw::Header(h) = &self.saw[i] else {
            unreachable!()
        };
        Some((h, &self.saw[i + 1..]))
    }

    /// The capture time of the last row seen.
    fn last_row(&self) -> Option<i64> {
        self.saw.iter().rev().find_map(|s| match s {
            Saw::Row(t, ..) => Some(*t),
            _ => None,
        })
    }
}

/// Reads `ws` until `done` is satisfied, `deadline` passes, or it closes, recording data records
/// (with their capture timestamps) and text messages in order. The socket's read timeout bounds
/// each blocking read.
fn drain_ws(ws: &mut Ws, deadline: Instant, done: impl Fn(&Seen) -> bool) -> Seen {
    let mut seen = Seen::default();
    if let MaybeTlsStream::Plain(s) = ws.get_mut() {
        s.set_read_timeout(Some(Duration::from_millis(250)))
            .unwrap();
    }
    while Instant::now() < deadline && !done(&seen) {
        match ws.read() {
            Ok(Message::Binary(b)) => {
                if b.len() >= 32 && b[0] == 1 {
                    seen.saw.push(Saw::Row(
                        i64::from_le_bytes(b[16..24].try_into().unwrap()),
                        u64::from_le_bytes(b[24..32].try_into().unwrap()),
                        b[1] & hk_api::stream::RecordFlags::DISCONTINUITY.0 != 0,
                    ));
                }
            }
            Ok(Message::Text(t)) => {
                if let Ok(v) = serde_json::from_str::<Value>(t.as_str()) {
                    seen.saw.push(Saw::Header(v));
                }
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(e) => {
                seen.closed = Some(e.to_string());
                break;
            }
        }
    }
    seen
}

/// T-417 (the user, 2026-09-17): *"a settle gap is fine … so connected consumers keep receiving
/// after the retune"*. A retune moves the front end, so the rows after it describe a different
/// window and the stream must re-offer its header — but the **connection** is not the thing that
/// moved, and tearing it down is what made every retune gesture (T-343, T-392, T-409, the
/// frequency navigator's offer) unpleasant: the picture died on every one.
///
/// Both retune shapes are asserted, because they are different code paths:
/// - **tuned in place** (same class and rate): no re-plumb, but the spectrum reader still finishes
///   its publisher and offers a new one under the same id, because a header must describe every
///   row after it (T-057);
/// - **a re-plumb** (another sample rate): the whole segment is rebuilt around the still-open
///   device (T-399), so every reader and every publisher is new.
///
/// What the consumer sees in both: its rows stop, a **new header** arrives on the same socket, and
/// rows of the new window follow. The gap is real and stays visible — nothing is held, repeated or
/// interpolated across it; the header is the honest seam.
#[test]
fn a_retune_keeps_connected_stream_consumers_connected() {
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
    let first: Value = serde_json::from_str(text.as_str()).unwrap();
    assert_eq!(first["center_hz"], json!(FIXTURE_CENTER_HZ), "{first}");

    let before = drain_ws(&mut ws, Instant::now() + Duration::from_secs(10), |s| {
        s.rows() >= 10
    });
    assert!(before.rows() > 0, "rows before the retune");
    assert!(before.closed.is_none(), "{:?}", before.closed);

    // ---- (1) tuned in place: same class, same rate, a different centre ----
    let step = hk_core::source::HACKRF_ONE_TUNING_STEP_HZ;
    let moved = step * (((FIXTURE_CENTER_HZ + 1e6) / step).round());
    let (st, r) = post(
        addr,
        "/api/control/center",
        &format!("{{\"center_hz\":{moved:?}}}"),
    );
    assert_eq!(st, 200, "{r}");
    assert_eq!(r["run"]["segment"], json!(0), "tuned in place: {r}");

    let moved_to = r["tuning"]["center_hz"].as_f64().expect("center_hz");
    let new_centre = |h: &Value| {
        h["center_hz"]
            .as_f64()
            .is_some_and(|c| (c - moved_to).abs() < 1.0)
    };
    let after = drain_ws(&mut ws, Instant::now() + Duration::from_secs(30), |s| {
        s.after_header(new_centre).is_some_and(|(_, rest)| {
            rest.iter().filter(|x| matches!(x, Saw::Row(..))).count() >= 10
        })
    });
    assert!(
        after.closed.is_none(),
        "the retune closed the consumer's socket: {:?}",
        after.closed
    );
    let (hd, rest) = after
        .after_header(new_centre)
        .unwrap_or_else(|| panic!("a header for the new centre {moved_to}: {after:?}"));
    assert_eq!(hd["stream_id"], json!("spectrum/live"), "{hd}");
    assert_eq!(hd["schema"], json!("hackriff.stream"), "{hd}");
    assert!(
        rest.iter().any(|s| matches!(s, Saw::Row(..))),
        "rows of the new window after its header: {after:?}"
    );

    // ---- (2) a re-plumb: another sample rate rebuilds the whole segment ----
    let (st, r) = post(addr, "/api/control/rate", r#"{"sample_rate_hz": 4.8e6}"#);
    assert_eq!(st, 200, "{r}");
    assert_eq!(r["run"]["segment"], json!(1), "the run re-plumbed: {r}");

    let new_rate = |h: &Value| h["bandwidth_hz"].as_f64() == Some(4.8e6);
    let across = drain_ws(&mut ws, Instant::now() + Duration::from_secs(60), |s| {
        s.after_header(new_rate).is_some_and(|(_, rest)| {
            rest.iter().filter(|x| matches!(x, Saw::Row(..))).count() >= 10
        })
    });
    assert!(
        across.closed.is_none(),
        "the re-plumb closed the consumer's socket: {:?}",
        across.closed
    );
    let (hd, rest) = across
        .after_header(new_rate)
        .unwrap_or_else(|| panic!("a header for the new rate: {across:?}"));
    assert_eq!(hd["stream_id"], json!("spectrum/live"), "{hd}");
    let rows_after: Vec<i64> = rest
        .iter()
        .filter_map(|s| match s {
            Saw::Row(t, ..) => Some(*t),
            _ => None,
        })
        .collect();
    assert!(
        !rows_after.is_empty(),
        "rows after the re-plumb: {across:?}"
    );

    // ---- the gap is real, and it is not papered over ----
    // The honesty constraint (T-410's open cap, T-413's hatched revoked gap, the coverage rule
    // applied to time): a break in the data while the front end moves is true and must stay
    // visible. The connection persists; the data honestly gaps. So across the seam the capture
    // clock **skips** — nothing is held, repeated or interpolated to cover it.
    //
    // T-425 measured what that skip actually is, because the bound here used to be `> 1.5 ×
    // period` and failed on a bit-identical value — 14% of runs on an idle machine, 40% under a
    // 12-way CPU load, and 70% on the coordinator's box, all at the same commit. That spread is
    // the finding, not noise: the rate is a property of the machine, never of the tree, so no
    // merge ever moved it (T-416 believed it had; T-383 edited this file but 5600 lines away).
    // The seam decomposes exactly:
    //
    //     gap = hole + period_old + n × period_new
    //
    // - `hole` is the capture time genuinely missing while the front end moves (the settle
    //   discard plus the samples the re-plumb consumed): **12.8 ms**, about **a third of a row**,
    //   never a whole one. So "at least one row's worth of time must be MISSING" was a claim about
    //   this system that was never true, and 1.5 × period (60.2 ms) sat *above* the real seam of
    //   52.9 ms.
    // - `period_old` = 40.107 ms (96 256 samples at 2.4 MHz), `period_new` = 40.0 ms (192 000 at
    //   4.8 MHz).
    // - `n` was the number of new-window rows this consumer **never received**, because the bridge
    //   looked for the replacement publisher once per `WATCH_TICK` (50 ms) while the new segment
    //   filled its first row in ~40 ms: a phase race with n ∈ {0, 1, 2}. Mutating WATCH_TICK to
    //   1 ms made the old assertion fail 6/6 at exactly 52.906667 ms; 200 ms made it pass 4/4. The
    //   old bound only ever passed *because* rows were dropped in delivery — and a mutation that
    //   papered the seam over completely still passed, because the dropped rows forged a gap that
    //   was not there. T-425 fixed that in `bridge::watch_peer` (it now waits on the registry, not
    //   on a tick), which is what makes the assertion below mean anything: with the fix the same
    //   mutation fails 4/4.
    //
    // So bound the quantity that is invariant. A seam that was held, repeated, interpolated or
    // back-filled puts the next row exactly `period_old` after the last one; an honest one puts it
    // strictly later. `period_old` is therefore not a fitted threshold but the exact boundary
    // between the two, and it stays a *lower* bound because a stalled consumer can still lose rows
    // (n > 0), which only pushes the measurement further above it, never below — so nothing here
    // is load-sensitive. Measured: 52.906667 ms in 7 of 8 runs after the bridge fix (ratio 1.32),
    // one run at 616 ms (n = 14, a stall). No upper bound: `n` has no measured ceiling.
    let last_before = after.last_row().expect("rows before the re-plumb");
    let mut periods: Vec<i64> = after
        .saw
        .iter()
        .filter_map(|s| match s {
            Saw::Row(t, ..) => Some(*t),
            _ => None,
        })
        .collect::<Vec<_>>()
        .windows(2)
        .map(|w| w[1] - w[0])
        .collect();
    periods.sort_unstable();
    let period_ns = periods[periods.len() / 2];
    let period_s = period_ns as f64 / 1e9;
    let gap_s = (rows_after[0] - last_before) as f64 / 1e9;
    assert!(
        gap_s > period_s,
        "capture time must be MISSING at the seam, not filled: the first row of the new window \
         starts {gap_s} s after the last row of the old one, which is no later than the \
         {period_s} s row period a held, repeated or interpolated seam would produce"
    );
    // And the hole is at the seam and nowhere else: no row is a held or repeated one (capture time
    // strictly advances everywhere, seam included — a held last frame would show as a repeated or
    // non-advancing timestamp), and no row after the seam is moved to fill the hole (below).
    for (a, b) in std::iter::once(&last_before)
        .chain(rows_after.iter())
        .zip(rows_after.iter())
    {
        assert!(b > a, "row timestamps strictly advance: {a} then {b}");
    }
    // T-974: the window's own row period comes from its header, not from the median step. The
    // steps after a re-plumb are not all whole rows: a front end that is losing blocks (the mock's
    // real-time pacing drops them whenever the capture thread falls behind 4.8 Msps, which on a
    // loaded box is every few blocks — `source_dropped` 4.2 M samples in one failing run) resets
    // the STFT at each gap, and since T-915 a reset in a freshly retuned stream emits the averaging
    // in progress as a partial row with its true `n_avg` (T-139). Those rows are real measurements
    // of real samples and shorter than a row; the median of a window of them is not the row
    // period, and "no step shorter than the median" failed 5 runs of 5 on a busy worker while
    // holding everywhere the source kept up. What a back-filled or squeezed seam breaks is not
    // step length, it is **where a row sits**: a row's time is its sample index on the capture
    // clock, and a row pulled earlier to close a hole no longer is. So each step is checked on
    // exactly that, and a step shorter than one row is legal only where the stream itself
    // declares the break (`DISCONTINUITY` on the later row) — between rows it calls contiguous,
    // a step is a whole number of row periods (more than one only when rows were lost in delivery,
    // which lengthens a step and never shortens one).
    let fs_new = hd["bandwidth_hz"].as_f64().expect("bandwidth_hz");
    assert_eq!(
        hd["content_class"],
        json!("unrestricted"),
        "an ungated class declares its actual row rate, so the header gives the row period: {hd}"
    );
    let new_period_ns = 1e9
        / hd["sample_rate_hz"]
            .as_f64()
            .expect("the declared row rate");
    let recs: Vec<(i64, u64, bool)> = rest
        .iter()
        .filter_map(|s| match s {
            Saw::Row(t, i, d) => Some((*t, *i, *d)),
            _ => None,
        })
        .collect();
    let (mut contiguous, mut declared_breaks) = (0, 0);
    for w in recs.windows(2) {
        let ((t0, i0, _), (t1, i1, cut)) = (w[0], w[1]);
        assert!(i1 > i0, "sample indices strictly advance: {recs:?}");
        let step = (t1 - t0) as f64;
        let from_samples = (i1 - i0) as f64 / fs_new * 1e9;
        // Each time is rounded to whole nanoseconds from one clock, so two differ by < 1 ns.
        assert!(
            (step - from_samples).abs() <= 1.0,
            "a row after the seam is placed {step} ns after the one before it, but its samples \
             start {from_samples} ns after them — a row's time is its sample index on the capture \
             clock, and a row moved to fill the hole breaks exactly that: {recs:?}"
        );
        if cut {
            declared_breaks += 1;
        } else {
            contiguous += 1;
            let periods = (step / new_period_ns).round();
            assert!(
                periods >= 1.0 && (step - periods * new_period_ns).abs() <= 2.0,
                "rows the stream calls contiguous are whole row periods apart: {step} ns against \
                 the new window's {new_period_ns} ns row period — the hole belongs at the seam, \
                 not squeezed back into the window: {recs:?}"
            );
        }
    }
    assert!(
        contiguous + declared_breaks >= 9,
        "{contiguous} contiguous + {declared_breaks} declared-break steps checked: {recs:?}"
    );

    let _ = ws.close(None);
    stop_server(serving);
}

/// T-529 — **`POST /api/control/window`: one user retune is ONE device action.**
///
/// A retune to a region names a capture *configuration*: a centre and a span. The client used to
/// commit it as `POST /api/control/rate` then `POST /api/control/center`, ~1.5 ms apart, and each
/// route completed the half the caller had not named from the tuning **in force**. One press
/// therefore commanded two windows, and the first of them — the *old* centre at the *new* rate —
/// is a window nobody asked for: reported by `/api/control/state`, captured into a whole segment
/// of its own, and recorded in the coverage map as spectrum this device chose to observe.
///
/// This asserts the route by its **values** (docs/api.md, T-079): both halves land, both are
/// required, the answer names the device action, and the whole press costs **one** re-plumb —
/// against two for the split, which is the observable difference between one window and two.
#[test]
fn a_window_commits_centre_and_rate_as_one_device_action() {
    let (_dir_guard, serving, addr) = start_server();
    let before = get(addr, "/api/control/state").1;
    assert_eq!(
        before["tuning"]["sample_rate_hz"].as_f64(),
        Some(FIXTURE_RATE_HZ),
        "{before}"
    );
    let seg_before = before["run"]["segment"].as_u64().expect("a segment number");

    // A centre the front end can really sit on, and a different rate: both halves move.
    let step = hk_core::source::HACKRF_ONE_TUNING_STEP_HZ;
    let want_center = step * (((FIXTURE_CENTER_HZ + 1e6) / step).round());
    let want_rate = 4.8e6;
    let (st, r) = post(
        addr,
        "/api/control/window",
        &format!("{{\"center_hz\":{want_center:?},\"sample_rate_hz\":{want_rate:?}}}"),
    );
    assert_eq!(st, 200, "{r}");
    // The centre is compared to under a hertz, not bit-for-bit: what the run reports back is the
    // centre the front end **landed on**, re-read from the pipeline's applied window, and a device
    // that quantises to its synthesiser grid (T-341) can answer a neighbouring double. A whole
    // hertz is far inside one HackRF tuning step (~28.6 Hz), so this is still the value and not a
    // relaxation — and every read after it is compared to `got_center` exactly.
    let got_center = r["tuning"]["center_hz"].as_f64().expect("center_hz");
    assert!(
        (got_center - want_center).abs() < 1.0,
        "asked for {want_center} Hz and the window in force is {got_center} Hz: {r}"
    );
    assert_eq!(
        r["tuning"]["sample_rate_hz"].as_f64(),
        Some(want_rate),
        "{r}"
    );
    // T-343's contract, on the new route too: the answer says a device action happened, which one,
    // and which front end it moved.
    assert_eq!(r["device"]["action"], json!("window"), "{r}");
    assert_eq!(
        r["device"]["id"],
        get(addr, "/api/control/state").1["device"]["device_id"],
        "the action must be recorded against the device the state names: {r}"
    );
    // ONE re-plumb for one press. Two posts cost two: the rate change re-plumbs at the old centre,
    // then the centre change re-plumbs (or tunes in place) again.
    assert_eq!(
        r["run"]["segment"].as_f64(),
        Some(seg_before as f64 + 1.0),
        "a whole-window commit is one re-plumb, not one per half: {r}"
    );

    // And the window in force afterwards is the one that was asked for, whole.
    let after = get(addr, "/api/control/state").1;
    assert_eq!(
        after["tuning"]["center_hz"].as_f64(),
        Some(got_center),
        "{after}"
    );
    assert_eq!(
        after["tuning"]["sample_rate_hz"].as_f64(),
        Some(want_rate),
        "{after}"
    );

    // ---- both halves are REQUIRED: a half-stated window is the defect, not a shorthand ----
    for body in [
        format!("{{\"center_hz\":{want_center:?}}}"),
        format!("{{\"sample_rate_hz\":{want_rate:?}}}"),
        "{}".into(),
    ] {
        let (st, v) = post(addr, "/api/control/window", &body);
        assert_eq!(
            st, 400,
            "a half-stated window must be refused: {body} -> {v}"
        );
        assert_eq!(v["code"], json!("invalid"), "{v}");
    }
    // Unknown fields are refused here as everywhere, so a typo is never silently a no-op.
    let (st, v) = post(
        addr,
        "/api/control/window",
        &format!(
            "{{\"center_hz\":{want_center:?},\"sample_rate_hz\":{want_rate:?},\"span_hz\":1}}"
        ),
    );
    assert_eq!(st, 400, "{v}");

    // ---- an unreachable half refuses the WHOLE window, and the front end does not move ----
    for body in [
        r#"{"center_hz": 9.9e12, "sample_rate_hz": 4.8e6}"#,
        r#"{"center_hz": 100800000.0, "sample_rate_hz": 4.0e7}"#,
    ] {
        let (st, v) = post(addr, "/api/control/window", body);
        assert_eq!(st, 400, "{body} -> {v}");
        assert_eq!(v["code"], json!("out_of_range"), "{v}");
    }
    let held = get(addr, "/api/control/state").1;
    assert_eq!(
        held["tuning"]["center_hz"].as_f64(),
        Some(got_center),
        "{held}"
    );
    assert_eq!(
        held["tuning"]["sample_rate_hz"].as_f64(),
        Some(want_rate),
        "a refused window leaves BOTH halves where they were: {held}"
    );

    // ---- re-committing the window in force is accepted and moves nothing ----
    let (st, r) = post(
        addr,
        "/api/control/window",
        &format!("{{\"center_hz\":{want_center:?},\"sample_rate_hz\":{want_rate:?}}}"),
    );
    assert_eq!(st, 200, "{r}");
    assert_eq!(
        r["run"]["segment"].as_f64(),
        Some(seg_before as f64 + 1.0),
        "a window already in force must not re-plumb the run: {r}"
    );

    // ---- GET is not a device action's method ----
    let (st, _) = get(addr, "/api/control/window");
    assert_eq!(st, 405, "only POST commits a window");
    stop_server(serving);
}

/// T-347 — **one client's Pause never freezes another client's stream.**
///
/// The defect: `POST /api/control/pause` set `DisplaySettings.paused` on the **run**, and every
/// connected client shares the run. One browser pressing Pause stopped the spectrum publisher for
/// all of them. T-339 had already shown the flag never reached the device, so the invariant it
/// guarded held — but the user's other half did not: *"the UI's time window is **independent view
/// state**"* (CLAUDE.md), and a run-wide boolean cannot be per-viewer state.
///
/// The design (docs/api.md, "Pause is client view state"): Pause is the client's own time cursor,
/// the same mechanism as scrubbing — one mechanism for what the user calls one thing — and the
/// two routes are gone. This test asserts that where the bug lived: **two real connections**, not
/// one. A single-client test cannot see a cross-client defect, which is presumably why it shipped.
///
/// Three legs, each asserting a value rather than a status code (T-315):
///
/// 1. **The lever is gone.** `POST /api/control/pause` and `/resume` answer `404 not_found`, and
///    `display` carries no `paused`. Without this the rest would pass on a server that still has a
///    run-wide pause nobody happened to press.
/// 2. **A held view is invisible to the other viewer.** Client A does exactly what the UI now does
///    to pause — a store write, i.e. *nothing on the wire* — and B's rows keep arriving with
///    **strictly advancing capture timestamps**, past the newest row B had seen when A paused.
///    A's own rows keep arriving too: holding a view does not stop the client's own stream either,
///    it stops the client *advancing over* it.
/// 3. **And a client that really stops consuming still cannot stall the other one.** A stops
///    draining its socket altogether — the strongest thing a paused viewer can do to the server
///    short of hanging up, and the shape T-348's per-connection saving would take — and B's rows
///    still advance. This is the drop-not-block policy (docs/stream-contract.md §7) holding across
///    clients: the producer never waits on a consumer, so one frozen browser cannot freeze another.
#[test]
fn one_clients_pause_never_freezes_another_clients_stream() {
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

    /// Rows, newest last, from what a consumer saw.
    fn row_times(seen: &Seen) -> Vec<i64> {
        seen.saw
            .iter()
            .filter_map(|s| match s {
                Saw::Row(t, ..) => Some(*t),
                _ => None,
            })
            .collect()
    }

    /// Reads `ws` until it has `n` rows or `limit` passes, and returns their capture times.
    fn rows_within(ws: &mut Ws, n: usize, limit: Duration) -> (Vec<i64>, Option<String>) {
        let seen = drain_ws(ws, Instant::now() + limit, |s| s.rows() >= n);
        (row_times(&seen), seen.closed)
    }

    let open = |who: &str| {
        let mut ws = connect_ws(addr, &format!("/ws/spectrum/live?token={TOKEN}")).unwrap();
        let Message::Text(text) = ws.read().unwrap() else {
            panic!("{who}: the first message must be the header (text)")
        };
        let header: Value = serde_json::from_str(text.as_str()).unwrap();
        assert_eq!(
            header["stream_id"],
            json!("spectrum/live"),
            "{who}: {header}"
        );
        ws
    };
    // Two browsers on one server, which is the whole point: the bug is invisible with one.
    let mut a = open("A");
    let mut b = open("B");

    let (a0, a_closed) = rows_within(&mut a, 5, Duration::from_secs(30));
    let (b0, b_closed) = rows_within(&mut b, 5, Duration::from_secs(30));
    assert!(
        a_closed.is_none() && b_closed.is_none(),
        "{a_closed:?} {b_closed:?}"
    );
    assert!(
        a0.len() >= 5 && b0.len() >= 5,
        "both clients must be receiving before either pauses: A {} rows, B {} rows",
        a0.len(),
        b0.len()
    );
    let b_before = *b0.last().expect("B saw a row");

    // ---- (1) the lever is gone ----
    for path in ["/api/control/pause", "/api/control/resume"] {
        let (st, v) = post(addr, path, "{}");
        assert_eq!(
            (st, v["code"].as_str()),
            (404, Some("not_found")),
            "{path} still exists: a run-wide pause is one viewer freezing every other viewer"
        );
    }
    let (st, state) = get(addr, "/api/control/state");
    assert_eq!(st, 200, "{state}");
    assert!(
        state["run"]["display"].get("paused").is_none(),
        "the run's display still carries a shared `paused`: {}",
        state["run"]["display"]
    );

    // ---- (2) A pauses, which is a store write and nothing else; B is untouched ----
    //
    // There is deliberately no request here. That *is* the design: the UI's Pause writes its own
    // time cursor (ui/src/app/centre/navigators.ts), and `ui/test/navigators.test.ts` asserts it
    // issues no request — the "assert the request the client builds" guard. What this test can see
    // from the server's side is the consequence, and it is the one the user reported.
    let (b1, b_closed) = rows_within(&mut b, 10, Duration::from_secs(30));
    assert!(b_closed.is_none(), "B's socket closed: {b_closed:?}");
    assert!(
        b1.len() >= 10,
        "B's waterfall stopped while A was paused: {} rows in 30 s",
        b1.len()
    );
    for w in b1.windows(2) {
        assert!(
            w[1] > w[0],
            "B's rows must advance in capture time, not repeat: {b1:?}"
        );
    }
    assert!(
        *b1.last().unwrap() > b_before,
        "B's newest row is no newer than before A paused: {} vs {b_before}",
        b1.last().unwrap()
    );
    // A's own stream did not stop either: pausing holds the *view* over the rows, not the rows.
    let (a1, a_closed) = rows_within(&mut a, 5, Duration::from_secs(30));
    assert!(a_closed.is_none(), "A's socket closed: {a_closed:?}");
    assert!(a1.len() >= 5, "A's own stream stopped: {} rows", a1.len());

    // ---- (3) A stops consuming entirely; B still advances ----
    //
    // A is not read again from here. 50 rows is about 2 s of B's stream at the run's 25 rows/s, so
    // A sits unread long enough for its own queue to back up while B is measured (the deadline is
    // 30x the measured time, per the rule about bounding a quantity you have measured). Whether A
    // is then dropped as a slow consumer is the documented §7 policy and not this test's subject;
    // what matters is that nothing about A reaches B.
    let b_mark = *b1.last().unwrap();
    let (b2, b_closed) = rows_within(&mut b, 50, Duration::from_secs(60));
    assert!(
        b_closed.is_none(),
        "B was disconnected because A stopped reading: {b_closed:?}"
    );
    assert!(
        b2.len() >= 50 && *b2.last().unwrap() > b_mark,
        "B stalled behind a client that stopped consuming: {} rows, newest {:?} vs {b_mark}",
        b2.len(),
        b2.last()
    );

    let _ = a.close(None);
    let _ = b.close(None);
    stop_server(serving);
}

#[test]
fn ws_open_listen_streams_pcm_data_records_of_the_station() {
    let (_dir_guard, serving, addr) = start_server();
    let (f_lo, f_hi) = (STATION_HZ - 100e3, STATION_HZ + 100e3);

    let (mut ws, header) = wait_for_listen(addr, f_lo, f_hi, "");
    assert_eq!(header["schema"], json!("hackriff.stream"));
    assert_eq!(header["kind"], json!("audio"));
    assert_eq!(header["datatype"], json!("ri16_le"));
    assert!(header["audio"]["mode"].is_string(), "{header}");
    // T-874: a client that does not ask gets mono.
    assert_eq!(header["audio"]["channels"], json!(1), "{header}");
    // T-987: `audio.wait` is only for a stream that opened waiting for its carrier; a station
    // the probe recognised carries none.
    assert!(header["audio"].get("wait").is_none(), "{header}");

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
                        b.len() - 32,
                        2 * 960,
                        "one 960-sample mono ri16_le frame per data record"
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

    // T-874 (ADR-0015 §12.13): `channels=2` opts in to stereo. The station is broadcast FM, so the
    // stream carries two channels — 960 interleaved L/R frames per record — and its status records
    // say whether L−R is decoded right now (`stereo`), whatever this recording's pilot does.
    let (mut ws, header) = wait_for_listen(addr, f_lo, f_hi, "&channels=2");
    assert_eq!(header["audio"]["mode"], json!("wfm"), "{header}");
    assert_eq!(header["audio"]["channels"], json!(2), "{header}");
    assert_eq!(header["max_frame_len"], json!(32 + 8 * 960), "{header}");
    let deadline = Instant::now() + Duration::from_secs(30);
    let (mut saw_data, mut saw_status) = (false, false);
    while Instant::now() < deadline && !(saw_data && saw_status) {
        match ws.read() {
            Ok(Message::Binary(b)) if b[0] == 1 => {
                assert_eq!(b.len() - 32, 4 * 960, "960 interleaved L/R frames");
                saw_data = true;
            }
            Ok(Message::Binary(b)) if b[0] == 3 => {
                let st: Value = serde_json::from_slice(&b[32..]).unwrap();
                assert!(st["stereo"].is_boolean(), "{st}");
                assert!(st["stereo_lock_losses"].is_u64(), "{st}");
                saw_status = true;
            }
            Ok(_) => {}
            Err(e) => panic!("stereo listen stream ended early: {e}"),
        }
    }
    assert!(
        saw_data && saw_status,
        "no stereo data and status within 30 s"
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
fn wait_for_listen(addr: SocketAddr, f_lo: f64, f_hi: f64, extra: &str) -> (Ws, Value) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let mut ws = connect_ws(
            addr,
            &format!("/ws/open/listen?f_lo={f_lo}&f_hi={f_hi}{extra}&token={TOKEN}"),
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
        p["refinement"],
        Value::Null,
        "only a recipe declaring refine.objective.builtin has a refinement (T-870): {p}"
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

/// T-161: stage tap `view=eye` (stream contract §14.4, ADR-0013 §4.9 gap 5). `view=raw` is
/// unchanged; on an `iq`/`real` port, `view=eye&symbol_rate_bd=<f>` serves the clock-recovery
/// eye/timing diagram (`kind: eye`, `rf32_le` rows of 64 traces × 64 points, row length declared
/// as `fft_size`, at most 25 rows/s, no RF geometry) instead of raw samples, with the record's
/// `sample_index` reporting the row's first symbol instant; an unsupported port type is refused
/// 409, a missing/invalid `symbol_rate_bd` 400, and an unknown `view` value stays 400 (§14.8
/// opener refusals). That the eye actually **opens at the true symbol instants and closes between
/// them** is asserted at the block level (`hk_pipeline::recipes::tap_eye`
/// `eye_opens_at_the_true_symbol_instants_and_closes_between_them`, on a known 2-PAM signal with
/// a hidden timing offset); this test only checks the wiring and shape the UI relies on.
#[test]
fn stage_tap_eye_view_answers_as_documented() {
    let (_dir_guard, serving, addr) = start_server();
    let doc = json!({
        "schema": "hackriff.recipe", "schema_version": 2, "id": "t161-contract", "version": 1,
        "name": "T-161 contract",
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

    // Default (no view / view=raw) is unchanged: the fm node's real port serves raw samples, with
    // no eye geometry in the header.
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

    // view=eye on the same real port: folded traces, not raw samples. 48 kHz / 1000 Bd = 48
    // samples per symbol, well inside the servable 2..=1024.
    let mut ws = connect_ws(
        addr,
        &format!(
            "/ws/open/stage?token={TOKEN}&pipeline={pid}&node=fm&view=eye&symbol_rate_bd=1000"
        ),
    )
    .unwrap();
    let Message::Text(t) = ws.read().unwrap() else {
        panic!("header first")
    };
    let header: Value = serde_json::from_str(t.as_str()).unwrap();
    assert_eq!(header["kind"], json!("eye"), "{header}");
    assert_eq!(header["datatype"], json!("rf32_le"), "{header}");
    // 64 traces of 64 points, both fixed by the contract.
    assert_eq!(header["fft_size"], json!(4096), "{header}");
    assert!(
        header["sample_rate_hz"].as_f64().unwrap() <= 25.0 + 1e-9,
        "{header}"
    );
    assert!(
        header.get("center_hz").is_none() && header.get("bandwidth_hz").is_none(),
        "an eye row's axes are symbol time and amplitude, not frequency: {header}"
    );
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        assert!(Instant::now() < deadline, "an eye data record");
        if let Message::Binary(b) = ws.read().unwrap() {
            assert_eq!(
                b.len(),
                32 + 4 * 4096,
                "one row of fft_size f32s: {}",
                b.len()
            );
            break;
        }
    }
    let _ = ws.close(None);

    // Unsupported port type (soft, bits): 409 (§14.8 "a view the port type doesn't support"). A
    // soft port is already one value per symbol, so there is nothing between the instants to draw.
    for node in ["clock", "bits"] {
        let mut ws = connect_ws(
            addr,
            &format!(
                "/ws/open/stage?token={TOKEN}&pipeline={pid}&node={node}&view=eye&symbol_rate_bd=1000"
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

    // Missing or invalid symbol_rate_bd: 400.
    for qs in [
        "view=eye",
        "view=eye&symbol_rate_bd=not-a-number",
        "view=eye&symbol_rate_bd=0",
        // 1 sample per symbol: nothing between the instants to draw.
        "view=eye&symbol_rate_bd=48000",
        // 48000 samples per symbol: past the window bound (decimate the port first).
        "view=eye&symbol_rate_bd=1",
    ] {
        let mut ws = connect_ws(
            addr,
            &format!("/ws/open/stage?token={TOKEN}&pipeline={pid}&node=fm&{qs}"),
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
    // T-465: the books close on every capture. This one drained cleanly, so nothing was thrown
    // away and the counted drops are zero — the value, not just the field's presence (T-315).
    assert_eq!(cap["dropped_records"], json!(0), "{cap}");

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

    // T-354: **two units in one body, and only the names distinguish them.** A frame record's time
    // is nanoseconds and is named `t_ns` (stream contract 1.2); the `capture` object wrapping it
    // reports `t_first`/`t_last` in seconds under bare names, per this API's units law (T-349).
    // Asserted by value, not shape: a body where `t_ns` were seconds, or `t_first` nanoseconds,
    // passes every "is a number" and key-presence check and fails only here.
    for (k, f) in frames.iter().enumerate().take(3) {
        assert!(
            f.get("t").is_none(),
            "frame {k} still ships a bare `t` that means nanoseconds: {f}"
        );
        let ns = f["t_ns"]
            .as_i64()
            .unwrap_or_else(|| panic!("frame {k} has no `t_ns`: {f}"));
        assert!(
            (1e18..1e19).contains(&(ns as f64)),
            "frames[{k}].t_ns = {ns} is not a nanosecond-magnitude Unix time (read as seconds it \
             is 31 billion years out, and every shape assertion still passes)"
        );
    }
    let cap_first = all["capture"]["t_first"]
        .as_f64()
        .unwrap_or_else(|| panic!("capture.t_first: {}", all["capture"]));
    let cap_last = all["capture"]["t_last"].as_f64().expect("capture.t_last");
    assert!(
        (1e9..1e10).contains(&cap_first) && (1e9..1e10).contains(&cap_last),
        "capture.t_first/t_last are Unix SECONDS beside the frames' nanoseconds: {}",
        all["capture"]
    );
    // The same instant, once each declared unit is applied: the capture's first frame time is the
    // first frame record's own `t_ns`, and t_last is not before it.
    let first_ns = frames[0]["t_ns"].as_i64().unwrap();
    assert!(
        (cap_first - first_ns as f64 / 1e9).abs() < 1.0,
        "capture.t_first ({cap_first} s) must be the same instant as frames[0].t_ns \
         ({first_ns} ns) once the units are applied"
    );
    assert!(
        cap_last >= cap_first,
        "capture.t_last {cap_last} precedes t_first {cap_first}"
    );

    // Time scrub: from_t resolves to a frame; to_t ends the page. `from_t`/`to_t` are seconds
    // (bare names, T-349's law) while the record is `t_ns`, so the scrub divides by 1e9 — and the
    // API refuses a nanosecond value here rather than reading it as a year-56-billion second.
    let t = |k: usize| frames[k]["t_ns"].as_i64().unwrap();
    let secs = |ns: i64| format!("{:.6}", ns as f64 / 1e9);
    let (st, v) = get(addr, &format!("/api/captures/{cid}/frames?from_t={}", t(5)));
    assert_eq!(
        (st, v["code"].as_str()),
        (400, Some("invalid")),
        "a raw t_ns passed to the seconds parameter must be refused, not misread: {v}"
    );
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

    // T-387: **the query the packet inspector actually sends**, and the reason that task needed no
    // stream-contract change. `/ws/open/inspector` is an on-demand opener with no history form, so
    // a scrubbed packet inspector cannot ask the socket for a past window — but this route already
    // carries the window, and `docs/api.md` already names it "the right route for the packet
    // inspector's own scrubbing". Two halves are asserted because the panel depends on both:
    //
    // 1. `pipeline_id` is the key that follows a pipeline to its recorded frames. The inspector is
    //    keyed by pipeline; without this field on the listing there is no way to reach the capture,
    //    and the surface would have had to grow a contract instead.
    // 2. `from_t` and `to_t` **together** select exactly the window's frames. The existing
    //    assertions above cover `from_t` alone and `from_frame` + `to_t`; the pair is what a scrub
    //    sends, and a window that holds frames must return them rather than an empty page.
    assert_eq!(
        cap["pipeline_id"],
        json!(pid),
        "the pipeline->capture key the packet inspector follows: {cap}"
    );
    let (st, v) = get(
        addr,
        &format!(
            "/api/captures/{cid}/frames?from_t={}&to_t={}&limit=500",
            secs(t(5) - 1_000_000),
            secs(t(7) + 1_000_000)
        ),
    );
    assert_eq!(st, 200, "{v}");
    assert_eq!(
        v["frames"].as_array().unwrap().as_slice(),
        &frames[5..8],
        "a from_t+to_t window that holds frames must serve exactly those frames"
    );
    // And the window is closed at both ends: a window strictly before every frame is empty, which
    // is the "genuinely nothing here" the client renders as an empty state rather than widening.
    let (st, v) = get(
        addr,
        &format!(
            "/api/captures/{cid}/frames?from_t={}&to_t={}",
            secs(t(0) - 3_600_000_000_000),
            secs(t(0) - 3_500_000_000_000)
        ),
    );
    assert_eq!(st, 200, "{v}");
    assert!(
        v["frames"].as_array().is_some_and(|a| a.is_empty()),
        "a window before every frame is empty, never widened to the nearest frames: {v}"
    );

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
        // `seq` is the replay stream's own; `t_ns`, metadata and content are the recording's.
        (
            &rec["type"],
            &rec["t_ns"],
            &rec["metadata"],
            &rec["content"]["hex"]
        ),
        (
            &frames[3]["type"],
            &frames[3]["t_ns"],
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
/// T-341: **the view cannot claim detail that was never captured.**
///
/// The user's navigation invariant, asserted by value on the wire. Three things have to hold at
/// once, and the third is what stops "everything is overview" passing the first two:
///
/// 1. the backend reports the achievable `(centre, span)` grid, including the **tuning step** —
///    the axis `SourceCapabilities` did not have before this task;
/// 2. a span wider than the instantaneous bandwidth answers **survey-overview**, not live;
/// 3. a request **inside** the achievable set is still served at full fidelity and marked
///    live-IQ-backed, with nothing snapped.
///
/// Values, not shape (T-315): a block that could be renamed or emptied without failing here would
/// be documentation, not a contract.
#[test]
fn navigation_snaps_to_achievable_states_and_never_claims_uncaptured_detail() {
    let (_dir_guard, serving, addr) = start_server();
    let step = hk_core::source::HACKRF_ONE_TUNING_STEP_HZ;

    // ---- (1) the grid ----
    let (st, v) = get(addr, "/api/navigation");
    assert_eq!(st, 200, "{v}");
    let f = &v["frequency"];
    // The third axis. The mock keeps the HackRF's synthesiser grid, because a test that snaps to
    // the mock's grid must be snapping to the grid the real device has.
    assert_eq!(f["center_step"], json!("uniform"), "{v}");
    assert_eq!(f["center_step_hz"], json!(step), "{v}");
    assert_eq!(f["ranges_hz"], json!([[1e6, 6e9]]), "{v}");
    assert_eq!(f["spans_hz"], json!({ "min": 2e6, "max": 20e6 }), "{v}");
    assert_eq!(f["max_live_span_hz"], json!(20e6), "{v}");
    assert_eq!(f["current"]["center_hz"], json!(FIXTURE_CENTER_HZ), "{v}");
    assert_eq!(f["current"]["span_hz"], json!(FIXTURE_RATE_HZ), "{v}");
    assert!(
        f["device_id"]
            .as_str()
            .is_some_and(|d| d.starts_with("mock:")),
        "{v}"
    );

    // The time axis is a ladder of discrete tiers, strictly coarsening, and the finest one is
    // named — a view cannot ask for a cell below it.
    let tiers = v["time"]["tiers"].as_array().expect("tiers").clone();
    assert!(tiers.len() >= 2, "{v}");
    let cell = |t: &Value| t["t_cell_s"].as_f64().expect("t_cell_s");
    for pair in tiers.windows(2) {
        assert!(cell(&pair[1]) > cell(&pair[0]), "tiers must coarsen: {v}");
    }
    let finest = cell(&tiers[0]);
    assert_eq!(v["time"]["min_t_cell_s"], json!(finest), "{v}");
    assert_eq!(
        v["time"]["max_t_cell_s"],
        json!(cell(tiers.last().unwrap())),
        "{v}"
    );
    assert_eq!(tiers[0]["level"], json!(0), "{v}");

    // ---- (2) the honesty test: wider than the window is overview, not live ----
    let (st, wide) = get(
        addr,
        &format!("/api/navigation?center_hz={FIXTURE_CENTER_HZ}&span_hz=40000000"),
    );
    assert_eq!(st, 200, "{wide}");
    let r = &wide["resolved"];
    assert_eq!(r["source"], json!("survey-overview"), "{wide}");
    assert_eq!(r["live"], json!(false), "{wide}");
    // The span axis clamps down to something the device could actually open...
    assert_eq!(r["span_hz"], json!(20e6), "{wide}");
    // ...and 100.8 MHz is not on the synthesiser grid, so the centre moves too — by less than half
    // a step, which is all a step ever promises.
    let snapped_center = r["center_hz"].as_f64().expect("a snapped centre");
    assert!(
        (snapped_center - FIXTURE_CENTER_HZ).abs() <= step / 2.0,
        "snapped {snapped_center} is more than half a step from {FIXTURE_CENTER_HZ}: {wide}"
    );
    assert_eq!(
        (snapped_center / step).round() * step,
        snapped_center,
        "the snapped centre must be on the grid: {wide}"
    );
    assert_eq!(r["matched"], json!(false), "{wide}");
    assert_eq!(r["snapped"], json!(["center_hz", "span_hz"]), "{wide}");

    // Exactly one window wide is still live: the boundary counts as inside, and one hertz past it
    // does not. Without this pair, "wider is overview" would be satisfied by calling everything
    // overview.
    let on_grid = (FIXTURE_CENTER_HZ / step).round() * step;
    let at_edge = |span: &str| {
        let (st, v) = get(
            addr,
            &format!("/api/navigation?center_hz={on_grid:?}&span_hz={span}"),
        );
        assert_eq!(st, 200, "{v}");
        v["resolved"]["source"].clone()
    };
    assert_eq!(at_edge("20000000"), json!("live-iq"));
    assert_eq!(at_edge("20000001"), json!("survey-overview"));

    // ---- (3) the control: inside the achievable set is live, at full fidelity, nothing snapped ----
    let (st, inside) = get(
        addr,
        &format!("/api/navigation?center_hz={on_grid:?}&span_hz={FIXTURE_RATE_HZ}"),
    );
    assert_eq!(st, 200, "{inside}");
    let r = &inside["resolved"];
    assert_eq!(r["source"], json!("live-iq"), "{inside}");
    assert_eq!(r["live"], json!(true), "{inside}");
    assert_eq!(r["center_hz"], json!(on_grid), "{inside}");
    assert_eq!(r["span_hz"], json!(FIXTURE_RATE_HZ), "{inside}");
    assert_eq!(r["matched"], json!(true), "{inside}");
    assert_eq!(r["snapped"], json!([]), "{inside}");

    // ---- the time half of the honesty test: finer than the finest tier is refused, not faked ----
    let (st, deep) = get(
        addr,
        &format!("/api/navigation?center_hz={on_grid:?}&span_hz={FIXTURE_RATE_HZ}&t_cell_s=0.001"),
    );
    assert_eq!(st, 200, "{deep}");
    let r = &deep["resolved"];
    assert_eq!(r["t_cell_s"], json!(finest), "{deep}");
    assert!(
        r["t_cell_s"].as_f64().unwrap() > 0.001,
        "answered coarser than asked, never finer: {deep}"
    );
    assert_eq!(r["level"], json!(0), "{deep}");
    assert_eq!(r["snapped"], json!(["t_cell_s"]), "{deep}");
    // Asking for a history tier is asking the pyramid, so the claim drops from live-IQ to
    // spectrum-history even though the span itself fits one window.
    assert_eq!(r["source"], json!("spectrum-history"), "{deep}");

    // A tier the ladder does have is served exactly, and nothing is reported as snapped.
    let (_, exact) = get(
        addr,
        &format!(
            "/api/navigation?center_hz={on_grid:?}&span_hz={FIXTURE_RATE_HZ}&t_cell_s={finest:?}"
        ),
    );
    assert_eq!(exact["resolved"]["t_cell_s"], json!(finest), "{exact}");
    assert_eq!(exact["resolved"]["snapped"], json!([]), "{exact}");

    // ---- refusals: a half-given state is an error, never a guess ----
    for bad in [
        "center_hz=100000000",
        "span_hz=2400000",
        "center_hz=100000000&span_hz=0",
        "center_hz=abc&span_hz=2400000",
        "center_hz=100000000&span_hz=2400000&t_cell_s=nan",
    ] {
        let (st, v) = get(addr, &format!("/api/navigation?{bad}"));
        assert_eq!(st, 400, "expected 400 for {bad}: {v}");
    }

    // The same tuning step is reported beside the rest of the device's capabilities.
    let (_, state) = get(addr, "/api/control/state");
    assert_eq!(state["device"]["tuning_step"], json!("uniform"), "{state}");
    assert_eq!(state["device"]["tuning_step_hz"], json!(step), "{state}");

    // ---- (4) T-340: the active capture windows, as a LIST ----
    //
    // The frequency navigator lights one segment per entry, so this is the field that decides
    // whether the navigator is N-capable by construction. The assertion is that it is an **array**
    // whose length is what this run holds — one live front end — with the tuned window's own
    // numbers in it. A client written against `frequency.current` would draw a window on a server
    // that never enumerated its front ends; a client written against this list draws exactly as
    // many as were reported, which is the whole point.
    let windows = v["windows"]
        .as_array()
        .expect("windows must be an array")
        .clone();
    assert_eq!(windows.len(), 1, "one live front end runs here: {v}");
    let w0 = &windows[0];
    assert_eq!(w0["center_hz"], json!(FIXTURE_CENTER_HZ), "{v}");
    assert_eq!(w0["span_hz"], json!(FIXTURE_RATE_HZ), "{v}");
    // A live window's span *is* its sample rate, and its edges are derived from that here rather
    // than in the client.
    assert_eq!(
        w0["f_lo_hz"],
        json!(FIXTURE_CENTER_HZ - FIXTURE_RATE_HZ / 2.0),
        "{v}"
    );
    assert_eq!(
        w0["f_hi_hz"],
        json!(FIXTURE_CENTER_HZ + FIXTURE_RATE_HZ / 2.0),
        "{v}"
    );
    // T-343's identity, so a lit segment can name the radio it belongs to — and it is the same
    // front end the grid above describes, not a second, unrelated name.
    assert_eq!(w0["device_id"], f["device_id"], "{v}");
    assert_eq!(w0["driver"], f["driver"], "{v}");

    // THE CONTROL: the list is **measured, not constant**. Move the front end and the window moves
    // with it, so a hard-coded entry (or one copied from the run's configuration) fails here.
    let moved = hk_core::source::HACKRF_ONE_TUNING_STEP_HZ
        * (((FIXTURE_CENTER_HZ + 5e6) / hk_core::source::HACKRF_ONE_TUNING_STEP_HZ).round());
    let (st, r) = post(
        addr,
        "/api/control/center",
        &format!("{{\"center_hz\":{moved:?}}}"),
    );
    assert_eq!(st, 200, "{r}");
    let (_, after) = get(addr, "/api/navigation");
    let w1 = &after["windows"].as_array().expect("windows")[0];
    // The window reports the front end's **actual** tuned state, not the number that was asked for
    // and not the run's configured centre: it agrees with `/api/control/state` exactly.
    let (_, state2) = get(addr, "/api/control/state");
    assert_eq!(w1["center_hz"], state2["tuning"]["center_hz"], "{after}");
    let c1 = w1["center_hz"].as_f64().expect("center_hz");
    assert!(
        (c1 - moved).abs() <= step,
        "the front end landed within one tuning step of the request: {after}"
    );
    assert!(
        (c1 - FIXTURE_CENTER_HZ).abs() > 1e6,
        "the window must have moved with the device: {after}"
    );
    assert_eq!(w1["f_lo_hz"], json!(c1 - FIXTURE_RATE_HZ / 2.0), "{after}");
    assert_eq!(w1["f_hi_hz"], json!(c1 + FIXTURE_RATE_HZ / 2.0), "{after}");
    assert_eq!(w1["device_id"], w0["device_id"], "{after}");

    stop_server(serving);
}

/// T-338: **the timeline is the capture window, and it is a visualization.**
///
/// The property is that the scrubber's span *is* the IQ ring's configured retention — which is
/// only demonstrated by **reconfiguring it and watching the span follow**. A constant that happens
/// to equal one retention would pass a single-value assertion.
///
/// The control is the other horizon. The spectrum-history pyramid outlives the ring by a long way,
/// and a scrubber sized from it offers times the ring has already overwritten: the band looks right
/// and lies. So the same server is asked for a history window far longer than the ring's, gets one,
/// and the timeline still spans the ring's — the two horizons are demonstrably different lengths
/// here, and the timeline took the capture one.
///
/// Values, not shape (T-315).
#[test]
fn the_timeline_spans_the_capture_window_and_draws_it() {
    let band = format!(
        "f_lo={}&f_hi={}",
        FIXTURE_CENTER_HZ - FIXTURE_RATE_HZ / 2.0,
        FIXTURE_CENTER_HZ + FIXTURE_RATE_HZ / 2.0
    );

    // ---- the property: the span is the configured retention, at two different retentions ----
    let mut spans = Vec::new();
    for retention_s in [90.0f64, 300.0] {
        let (_dir_guard, serving, addr) = start_server_retaining(Some(retention_s));
        let mut v = Value::Null;
        wait_for(
            "a capture window with retained capture drawn on it",
            Duration::from_secs(60),
            || {
                let (st, got) = get(addr, &format!("/api/timeline?{band}&columns=64&rows=4"));
                assert_eq!(st, 200, "{got}");
                v = got;
                v["window"]["span_s"].is_f64()
                    && v["window"]["buffered"]["t0_s"].is_f64()
                    && v["grid"]["observed_cells"].as_u64().unwrap_or(0) > 0
            },
        );

        let w = &v["window"];
        assert_eq!(w["enabled"], json!(true), "{w}");
        assert_eq!(w["retention_s"], json!(retention_s), "{w}");
        assert_eq!(w["span_s"], json!(retention_s), "{w}");
        // The band ends at the live edge and starts exactly one retention earlier — no rounding,
        // no fixed constant, no history horizon.
        let (t0, t1) = (
            w["t0_s"].as_f64().expect("t0_s"),
            w["t1_s"].as_f64().expect("t1_s"),
        );
        assert_eq!(t1 - t0, retention_s, "{w}");
        // Which retention this is. The response says so rather than leaving it to be assumed.
        assert_eq!(w["horizon"], json!("iq-ring"), "{w}");
        // What the ring actually holds sits *inside* the band, never resizes it: a ring 10 s into
        // a 90 s retention is a mostly-empty 90 s capture window, not a 10 s one.
        let b = &w["buffered"];
        let (b0, b1) = (
            b["t0_s"].as_f64().expect("buffered.t0_s"),
            b["t1_s"].as_f64().expect("buffered.t1_s"),
        );
        assert!(b0 >= t0 && b1 <= t1 && b1 - b0 < retention_s, "{w}");
        // T-845: the ring's next whole-slot evictions, so a client polling this can move the IQ
        // horizon past a drop it has not re-polled. Each lies ahead of the write head and moves
        // the oldest sample forward to data already held.
        let drops = b["drops"].as_array().expect("buffered.drops");
        let mut prev = (b1, b0);
        for d in drops {
            let (at, d0) = (
                d["at_s"].as_f64().expect("at_s"),
                d["t0_s"].as_f64().expect("t0_s"),
            );
            assert!(at >= prev.0 && d0 > prev.1 && d0 <= b1, "{w}");
            prev = (at, d0);
        }

        // ---- never an empty box: the band is a compressed overview waterfall ----
        let g = &v["grid"];
        assert_eq!(g["nt"], json!(64), "{g}");
        assert_eq!(g["nf"], json!(4), "{g}");
        assert_eq!(g["cells"], json!(256), "{g}");
        // The grid *is* the window: cell 0 starts at the band's start and cells divide it exactly.
        assert_eq!(g["t0_s"].as_f64(), Some(t0), "{g}");
        assert_eq!(g["t_cell_s"].as_f64(), Some(retention_s / 64.0), "{g}");
        assert_eq!(g["f_cell_hz"].as_f64(), Some(FIXTURE_RATE_HZ / 4.0), "{g}");
        for k in ["max_db", "occupancy_max", "coverage", "frames"] {
            assert_eq!(g[k].as_array().map(Vec::len), Some(256), "{k}: {g}");
        }
        let observed = g["observed_cells"].as_u64().expect("observed_cells");
        assert!(
            observed > 0,
            "the timeline must draw the retained capture: {g}"
        );
        // The dynamic range is measured here, not decided by the client from the values it holds.
        let r = &g["range_db"];
        assert!(
            r["lo"].as_f64().unwrap() <= r["hi"].as_f64().unwrap(),
            "{r}"
        );
        // Unobserved cells are null, never a floor value that would read as a measured quiet band.
        let max_db = g["max_db"].as_array().unwrap();
        assert!(
            max_db.iter().any(|x| x.is_f64()),
            "no observed cell in the band: {g}"
        );

        // ---- the detail claim: the horizon is the ring's, the pixels are the pyramid's ----
        let res = &v["resolution"];
        assert_eq!(res["source"], json!("spectrum-history"), "{res}");
        assert_eq!(res["live"], json!(false), "{res}");
        assert_eq!(res["horizon"], json!("iq-ring"), "{res}");
        assert_eq!(res["served_span_hz"], json!(FIXTURE_RATE_HZ), "{res}");
        assert_eq!(res["requested"], json!({"columns": 64, "rows": 4}), "{res}");
        assert_eq!(
            res["served"],
            json!({"nt": 64, "nf": 4, "cells": 256}),
            "{res}"
        );
        // The fold happens in the backend, so the client is never handed cells it cannot draw.
        assert_eq!(res["matched"], json!(true), "{res}");
        assert_eq!(res["over_resolved"], json!([]), "{res}");
        assert!(res["reduced_from"]["nf"].as_u64().unwrap() >= 4, "{res}");

        // ---- T-367: the overview is scoped to the frequency range asked for ----
        // The time navigator draws "what has been happening **here**", so its picture is the
        // capture window folded over *the range the main view is on*, not over the whole spectrum.
        // Two disjoint halves of the same band are two different pictures, laid on the same
        // window: the grid's own frequency origin and cell width follow the request, while
        // `window` — the bar's *extent* — is byte-identical across all three. That pair is the
        // invariant: the time axis is not a function of the band, the picture is.
        let halves = [
            (FIXTURE_CENTER_HZ - FIXTURE_RATE_HZ / 2.0, FIXTURE_CENTER_HZ),
            (FIXTURE_CENTER_HZ, FIXTURE_CENTER_HZ + FIXTURE_RATE_HZ / 2.0),
        ];
        for (lo, hi) in halves {
            let (st, h) = get(
                addr,
                &format!("/api/timeline?f_lo={lo}&f_hi={hi}&columns=64&rows=4"),
            );
            assert_eq!(st, 200, "{h}");
            assert_eq!(h["region"], json!({"lo_hz": lo, "hi_hz": hi}), "{h}");
            assert_eq!(h["grid"]["f_lo_hz"].as_f64(), Some(lo), "{h}");
            assert_eq!(
                h["grid"]["f_cell_hz"].as_f64(),
                Some((hi - lo) / 4.0),
                "{h}"
            );
            assert_eq!(
                h["resolution"]["served_span_hz"].as_f64(),
                Some(hi - lo),
                "{h}"
            );
            // The extent does not move with the band: the same span, a different picture. (Only
            // the span, not the whole window: `t1_s` is the live edge and advances between calls.)
            assert_eq!(h["window"]["span_s"], json!(retention_s), "{h}");
            assert_eq!(h["window"]["horizon"], json!("iq-ring"), "{h}");
            // And it is a picture, not an empty box, over half the band as over all of it.
            assert_eq!(h["grid"]["cells"], json!(256), "{h}");
        }
        // The control: ask for no band and there is **no grid at all** — which is why the time
        // navigator must send one. An unscoped request draws nothing, not the whole spectrum.
        let (st, unscoped) = get(addr, "/api/timeline?columns=64&rows=4");
        assert_eq!(st, 200, "{unscoped}");
        assert!(
            unscoped["region"].is_null() && unscoped["grid"].is_null(),
            "{unscoped}"
        );
        assert_eq!(
            unscoped["window"]["span_s"],
            json!(retention_s),
            "{unscoped}"
        );

        // ---- the control: the spectrum history reaches much further back, and is not used ----
        // A 48 h history request is answered (the pyramid's horizon is nothing like 90 s), and the
        // timeline still spans the ring's retention. Without this the property could pass on a
        // server where the two horizons happened to coincide.
        let (st, hist) = get(
            addr,
            &format!(
                "/api/history?{band}&t0={}&t1={t1}&max_t=8",
                t1 - 48.0 * 3600.0
            ),
        );
        assert_eq!(st, 200, "{hist}");
        let hist_span_s = hist["nt"].as_f64().unwrap() * hist["t_cell_s"].as_f64().unwrap();
        assert!(
            hist_span_s > retention_s * 10.0,
            "the history horizon must differ from the ring's for this control to mean anything: \
             history {hist_span_s} s vs ring {retention_s} s"
        );
        assert_eq!(v["window"]["span_s"], json!(retention_s));
        // The specific regression: the band used to be a hard-coded 48 h UI constant.
        assert_ne!(w["span_s"].as_f64(), Some(48.0 * 3600.0), "{w}");
        // And `/api/navigation`'s time block is the *history* edge, deliberately not this span.
        let (_, nav) = get(addr, "/api/navigation");
        assert!(nav["time"]["latest_s"].is_f64(), "{nav}");

        spans.push(w["span_s"].as_f64().unwrap());
        stop_server(serving);
    }
    // Reconfiguring the retention moved the span. This is what makes it the capture window rather
    // than a constant that happens to match one.
    assert_eq!(spans, vec![90.0, 300.0]);

    // ---- with no frequency region there is still a window, and no invented picture ----
    let (_dir_guard, serving, addr) = start_server_retaining(Some(45.0));
    let (st, v) = get(addr, "/api/timeline");
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["window"]["retention_s"], json!(45.0), "{v}");
    assert!(v["region"].is_null() && v["grid"].is_null(), "{v}");
    for bad in ["f_lo=1000", "columns=0", "rows=99999", "t0=1&t1=2"] {
        let (st, v) = get(addr, &format!("/api/timeline?{bad}"));
        assert_eq!(st, 400, "expected 400 for {bad}: {v}");
    }
    let (st, _) = call(
        addr,
        "POST",
        "/api/timeline",
        Some(&format!("Bearer {TOKEN}")),
        Some("{}"),
    );
    assert_eq!(st, 405);
    stop_server(serving);
}

/// T-342: **the band-collapsed activity-vs-time series is the backend's, and it says what it is.**
///
/// The client used to take a max over every frequency cell of a history grid and normalise it
/// against the response's own range. Deciding which value represents a band is a *measurement* — it
/// belongs where the levels, the floor and the coverage are known — and T-338 moved the fold here.
/// What this test pins is the part a moved measurement still gets wrong: a served number whose
/// **statistic and scale are unstated** is one a consumer will re-derive, or misread.
///
/// Three properties, all on values:
///
///  1. **The series is the measurement.** The band-collapsed column (`rows=1`) equals, step by
///     step, the max over the rows of the same window at `rows=4`. A mean, a first-row pick or a
///     mid-band sample all fail it.
///  2. **Unobserved is not quiet.** A step nothing was folded into is `null` with zero frames —
///     structurally different from a step that was observed and read low. Emit a floor value for
///     the empty steps and the matched pair below stops being a pair.
///  3. **The budget is honoured and stated.** `columns` is a time-axis budget like `/api/floor`'s
///     `max_steps`; asking for more columns than the window has source cells never truncates the
///     window, it replicates a measured value, and the response says which happened.
#[test]
fn the_band_collapsed_activity_series_is_measured_and_states_its_fold() {
    let (lo, hi) = (
        FIXTURE_CENTER_HZ - FIXTURE_RATE_HZ / 2.0,
        FIXTURE_CENTER_HZ + FIXTURE_RATE_HZ / 2.0,
    );
    let band = format!("f_lo={lo}&f_hi={hi}");
    let (_dir_guard, serving, addr) = start_server_retaining(Some(90.0));

    wait_for(
        "a capture window with retained capture drawn on it",
        Duration::from_secs(60),
        || {
            let (st, got) = get(addr, &format!("/api/timeline?{band}&columns=32&rows=4"));
            st == 200 && got["grid"]["observed_cells"].as_u64().unwrap_or(0) > 0
        },
    );
    // The same window, asked for as one row: the band-collapsed series itself. The two answers must
    // describe the **same** window for the comparison to mean anything — the live edge advances
    // between requests — so the pair is re-taken until both report the same capture window, and the
    // test refuses to compare otherwise rather than comparing across a moved window.
    //
    // T-974: and the live edge is **stopped** first. `window.t1_s` is the ring's newest sample and
    // moves every 20-100 ms (measured: 40 distinct values in 1.8 s of back-to-back requests), while
    // one pair of these requests took 30-40 ms on a quiet box and longer on a busy one — so whether
    // any of 40 re-takes landed inside one step was a race against the machine's load, and a
    // worker under load lost it (T-920: "two timeline answers over the same capture window").
    // Nothing below is about liveness: the fold, the unobserved steps and the budget are properties
    // of whatever the ring holds. Stopping the run freezes that — the capture window is still the
    // ring's retention, the retained capture is still drawn on it, and the answer no longer depends
    // on how fast this machine serves two requests.
    serving.handle.stop();
    let pair = || {
        let (sa, a) = get(addr, &format!("/api/timeline?{band}&columns=32&rows=4"));
        let (sb, b) = get(addr, &format!("/api/timeline?{band}&columns=32&rows=1"));
        assert_eq!((sa, sb), (200, 200), "{a} {b}");
        (a, b)
    };
    // The stop lands at the capture thread's next block, so the edge may still take a last step;
    // after that every pair agrees. Bounded by the run's end, not by a count of attempts.
    let mut same = None;
    let deadline = Instant::now() + Duration::from_secs(60);
    while same.is_none() && Instant::now() < deadline {
        let (a, b) = pair();
        if a["window"]["t1_s"] == b["window"]["t1_s"] && a["window"]["t0_s"] == b["window"]["t0_s"]
        {
            same = Some((a, b));
        } else {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    let (rows4, rows1) = same.expect("two timeline answers over the same (stopped) capture window");

    let g1 = &rows1["grid"];
    let g4 = &rows4["grid"];
    assert_eq!(g1["nt"], json!(32), "{g1}");
    assert_eq!(g1["nf"], json!(1), "{g1}");
    let col = |g: &Value, k: &str| g[k].as_array().expect(k).clone();
    let (c1, c4) = (col(g1, "max_db"), col(g4, "max_db"));
    let f1 = col(g1, "frames");

    // ---- (1) the property: the series IS the max over the band, cell by cell ----
    let mut compared = 0;
    for t in 0..32 {
        let want = (0..4)
            .filter_map(|f| c4[t * 4 + f].as_f64())
            .fold(f64::NEG_INFINITY, f64::max);
        match c1[t].as_f64() {
            Some(got) => {
                assert!(want.is_finite(), "step {t} collapsed from nothing: {g4}");
                assert_eq!(got, want, "step {t}: the band is the max over its rows");
                compared += 1;
            }
            // The converse, and it is the honesty half: a step with no observed row must not
            // acquire a value from the collapse.
            None => assert!(
                !want.is_finite(),
                "step {t} was observed at rows=4 but null at rows=1: {g1}"
            ),
        }
    }
    assert!(compared > 0, "no observed step to compare: {g1} {g4}");

    // ---- the semantics: which statistic, and what it is relative to ----
    let sem = &g1["semantics"];
    assert_eq!(sem["fold"], json!("max-hold"), "{sem}");
    assert_eq!(sem["series"]["max_db"]["statistic"], json!("max-hold"));
    assert_eq!(sem["series"]["max_db"]["scale"], json!("dbfs-per-hz"));
    assert_eq!(sem["series"]["max_db"]["unobserved"], json!("null"));
    // Not every series is a max, and the block says so per series rather than labelling the grid.
    assert_eq!(sem["series"]["occupancy_max"]["statistic"], json!("max"));
    // T-419: coverage is weighted by how much of the output cell each source cell overlaps, not
    // averaged over however many source cells happened to touch it. The wire must say which,
    // because the two differ exactly where the grid is fractional — and the optimistic one paints
    // unobserved spectrum as scanned.
    assert_eq!(
        sem["series"]["coverage"]["statistic"],
        json!("extent-weighted-mean")
    );
    assert_eq!(sem["series"]["frames"]["statistic"], json!("sum"));
    assert_eq!(sem["series"]["coverage"]["unobserved"], json!("0"));
    // The scale is the source grid's unit, carried — not a constant in the route.
    assert_eq!(g1["unit"], json!("dbfs"), "{g1}");
    for (key, phrase) in [
        ("rule", "the max of nothing is unobserved, not zero"),
        ("unobserved_rule", "never observed, never quiet"),
    ] {
        assert!(
            sem[key].as_str().is_some_and(|s| s.contains(phrase)),
            "{key} must state \"{phrase}\": {sem}"
        );
    }

    // ---- (2) the control: observed-and-quiet is a different answer from never-observed ----
    // The band spans 90 s of retention but this server has been up for a fraction of that, so the
    // early steps are genuinely unobserved while the recent ones are measured. Both must be in
    // hand for the pair to mean anything.
    let unobserved: Vec<usize> = (0..32).filter(|&t| c1[t].is_null()).collect();
    let observed: Vec<usize> = (0..32).filter(|&t| c1[t].is_f64()).collect();
    assert!(
        !unobserved.is_empty() && !observed.is_empty(),
        "need both an unobserved and an observed step: {g1}"
    );
    let cov1 = col(g1, "coverage");
    let occ1 = col(g1, "occupancy_max");
    for &t in &unobserved {
        // No field a client can read as a measured zero, and no frames to suggest one was taken.
        assert!(occ1[t].is_null(), "step {t}: {g1}");
        assert_eq!(cov1[t], json!(0.0), "step {t}: {g1}");
        assert_eq!(f1[t], json!(0), "step {t}: {g1}");
    }
    // THE MUTATION CONTROL: null is exactly "nothing was folded in". Fill an empty step with the
    // range's floor — the plausible-looking bug — and this equivalence breaks, because that step
    // still has zero frames behind it.
    for t in 0..32 {
        assert_eq!(
            c1[t].is_null(),
            f1[t].as_u64() == Some(0),
            "step {t}: a value and a frame count that disagree: {g1}"
        );
    }
    // The unobserved steps are not sitting at the bottom of the scale: the scale is the observed
    // range, measured here, and every observed value lies inside it.
    let (rlo, rhi) = (
        g1["range_db"]["lo"].as_f64().expect("range lo"),
        g1["range_db"]["hi"].as_f64().expect("range hi"),
    );
    for &t in &observed {
        let v = c1[t].as_f64().unwrap();
        assert!(v >= rlo - 1e-6 && v <= rhi + 1e-6, "step {t}: {g1}");
    }

    // ---- (3) the budget: honoured exactly, and what happened to it is stated ----
    let b = &rows1["resolution"]["budget"];
    assert_eq!(b["time"]["requested"], json!(32), "{b}");
    assert_eq!(b["time"]["served"], json!(32), "{b}");
    assert_eq!(b["frequency"]["requested"], json!(1), "{b}");
    assert_eq!(b["frequency"]["served"], json!(1), "{b}");
    assert!(
        b["statement"]
            .as_str()
            .is_some_and(|s| s.contains("never truncated") && s.contains("never invented")),
        "{b}"
    );
    // More columns than the window has source cells: the window is NOT truncated to what exists,
    // the grid is still exactly the budget, and `replicated` says the extra columns are repeats of
    // a measured value rather than measurements of their own.
    let (st, many) = get(addr, &format!("/api/timeline?{band}&columns=4000&rows=1"));
    assert_eq!(st, 200, "{many}");
    let bm = &many["resolution"]["budget"]["time"];
    assert_eq!(bm["requested"], json!(4000), "{bm}");
    assert_eq!(bm["served"], json!(4000), "{bm}");
    assert_eq!(many["grid"]["nt"], json!(4000), "{bm}");
    let src = bm["source_cells"].as_u64().expect("source_cells");
    assert!(src < 4000, "{bm}");
    assert_eq!(bm["replicated"], json!(true), "{bm}");
    // The window is unchanged by the budget: the same span, drawn in more columns.
    assert_eq!(many["window"]["span_s"], json!(90.0), "{many}");
    let cell_s = many["grid"]["t_cell_s"].as_f64().expect("t_cell_s");
    assert!((cell_s - 90.0 / 4000.0).abs() < 1e-9, "{many}");
    // And replication is visible in the values: with fewer source cells than columns, adjacent
    // observed columns must repeat.
    let cm = col(&many["grid"], "max_db");
    let repeats = (1..cm.len())
        .filter(|&i| cm[i].is_f64() && cm[i] == cm[i - 1])
        .count();
    assert!(repeats > 0, "a replicated axis must show repeats: {bm}");
    // The counter-case on the same server: `replicated` is measured from that request's own source
    // grid, not a constant — a 32-column budget the source can fill reports false.
    let src32 = b["time"]["source_cells"].as_u64().expect("source_cells");
    assert_eq!(b["time"]["replicated"], json!(src32 < 32), "{b}");
    assert!(src32 >= 32, "the 32-column budget should be fillable: {b}");

    stop_server(serving);
}

/// T-621: why this server has **no IQ capture ring**, in the server's own words, or `None` while
/// it may still get one.
///
/// `hk serve` refuses the ring when it will not fit above the free-space floor (`docs/api.md`
/// "IQ capture buffer": `enabled: false`, `allocation: "refused"`, and a `reason` naming the bytes
/// needed, the floor and the free space). The coverage tests below need one — their evidence is
/// the ring's tune journal — and without it they used to spend 60-90 s inside a `wait_for` and
/// then fail on an assertion about *coverage*, for a reason that has nothing to do with coverage.
/// Three merge gates were lost to that hunt on a day when this machine's free space ran from
/// 43 GiB down to 1.1 GiB.
///
/// So they state the precondition, and a missing one fails **immediately, naming the refusal**.
/// Not a skip: a gate that skips is a gate that passes for the wrong reason (T-346/T-353). The
/// refused ring is not merely fenced off either — it is a real product state (a portable device
/// fills its disk) and is under test in
/// [`coverage_survives_a_refused_iq_ring_and_never_calls_the_lost_evidence_grey`], which forces
/// the refusal deliberately through the allocator's own `fs_space`. Here a refusal can only be an
/// accident of the machine, and accidents get named rather than tolerated.
///
/// Read as a **state, never a clock**: every `enabled: false` allocation state except
/// `"allocating"` is settled, so this answers the first time it is asked.
fn iq_ring_unavailable(addr: SocketAddr) -> Option<String> {
    let (st, v) = get(addr, "/api/iqbuffer");
    if st != 200 || v["enabled"] == json!(true) {
        return None;
    }
    match v["allocation"].as_str() {
        // Still opening in the background: not yet an answer either way.
        Some("allocating") => None,
        alloc => Some(format!(
            "this test needs an IQ capture ring and this server has none, so there is no tune \
             journal for the coverage map to read. allocation={}, the server's reason: {}. That is \
             this machine, not the code under test — `allocation: \"refused\"` means free space \
             below the ring's floor (`docs/api.md`, IQ capture buffer). Free some disk, or lower \
             the floor with HK_IQ_BUFFER_MIN_FREE, and run it again.",
            alloc.map_or_else(|| "null".to_owned(), |a| format!("{a:?}")),
            v["reason"]
        )),
    }
}

/// Fails the calling test at once, in the server's own words, when it has no IQ capture ring
/// ([`iq_ring_unavailable`]).
fn require_iq_ring(addr: SocketAddr) {
    if let Some(why) = iq_ring_unavailable(addr) {
        panic!("{why}");
    }
}

/// The `sources[]` row a coverage answer serves for `kind`.
fn coverage_source<'a>(coverage: &'a Value, kind: &str) -> &'a Value {
    coverage
        .get("sources")
        .and_then(Value::as_array)
        .and_then(|rows| rows.iter().find(|s| s["kind"] == json!(kind)))
        .unwrap_or_else(|| panic!("the {kind} source row is always reported: {coverage}"))
}

/// **T-920: waits for the coverage answer whose evidence is the IQ RING's tune journal.**
///
/// `observed_cells > 0` alone is *not* that answer. Since T-596 the **open dwell** — the dwell in
/// flight, before the observation log has sealed a record for it — rasterises into the same planes
/// and can carry the tuned band on its own. The IQ ring, meanwhile, opens on a background thread
/// (T-178/T-217), so for the first fraction of a second to several seconds of a run, depending on
/// the quota and how loaded the box is, `/api/coverage` answers `observed_cells: 8` with
/// `sources[iq-ring]` reporting `available: false, state: "allocating"`.
///
/// A wait that stops at the observed count therefore steps straight into an assertion about the
/// ring while the ring is still being laid down. That is exactly how this file's coverage test ran
/// red 4/4 on a loaded Linux host and green on a quiet Mac: not a platform defect, a wait that did
/// not wait for its own evidence. [`require_iq_ring`] does not catch it either — `"allocating"` is
/// deliberately "not an answer yet" there, because it is the one unavailable state that resolves
/// itself.
///
/// So the wait is on **both**: the ring able to contribute, and the band observed. A settled
/// refusal still fails immediately in the server's words, and a ring that never finishes opening
/// fails naming the `state` and `reason` it was last serving (T-920 put them on the row for
/// exactly this).
fn wait_for_ring_backed_coverage(addr: SocketAddr, query: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut last = Value::Null;
    loop {
        // T-621: this loop's evidence is the ring's tune journal, so a server that was refused
        // one can never satisfy it. Fail here, in the server's words, instead of 60 s later on
        // an assertion about *coverage*.
        require_iq_ring(addr);
        // 404 until the ring has a live edge to hang a capture window on: a server with no
        // capture window says so rather than inventing a span.
        let (st, got) = get(addr, query);
        if st == 200 {
            let ring_ready = coverage_source(&got, "iq-ring")["available"] == json!(true);
            let observed = got["any"]["observed_cells"].as_u64().unwrap_or(0) > 0;
            last = got;
            if ring_ready && observed {
                return last;
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the IQ ring to back the coverage answer for {query}. The ring              row's own words: {}. (`available: false` with `state: \"allocating\"` means it is              still opening; anything else settled is this machine, not the code under test.) Last              answer: {last}",
            if last.is_null() {
                Value::Null
            } else {
                coverage_source(&last, "iq-ring").clone()
            },
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// T-368: **grey means genuinely unobserved.**
///
/// The user's invariant: *"the view renders whatever samples are actually available for the current
/// time-and-frequency selection, and greys only cells that were truly never observed"*, from *"a
/// coverage map derived from the SDR configuration/tune history — for each interval, which
/// centre/span/rate (and which device) was active"*.
///
/// The property is that **three states stay three**: observed-with-energy, observed-and-quiet, and
/// never-observed are not two states with a null in the middle. The one that must not be servable
/// as the others is the third, so the assertions below are a matched pair on one running server:
///
///  - the band the mock front end is actually tuned to reads **observed**, with the sampling that
///    proves it (a real `duty` over a real interval, and a `device` naming the radio);
///  - a band 2.4 GHz away, which this run demonstrably never tuned, reads **unobserved** — and
///    carries **no measurement key at all**, so there is nothing a client could read as a zero
///    level and draw as a quiet band.
///
/// The second half is the control for the first: without it a route that answered "unobserved"
/// everywhere would pass, and without the first a route that answered "observed" everywhere would.
/// Neither can pass both. (The mutation control — assume coverage everywhere and watch the property
/// break — is the unit test `hk_store::coverage::assuming_coverage_everywhere_breaks_the_property`,
/// where the map can actually be mutated.)
///
/// Values, not shape (T-315).
#[test]
fn coverage_greys_only_what_was_never_observed_and_names_the_device_that_looked() {
    // Measured before the server exists, so it is <= the timestamp of the first frame this run can
    // possibly have written. T-426 reads the phase off it below; see that block for the formula.
    let launch_s = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("a clock after 1970")
        .as_secs_f64();
    let (_dir_guard, serving, addr) = start_server_retaining(Some(120.0));
    let (lo, hi) = (
        FIXTURE_CENTER_HZ - FIXTURE_RATE_HZ / 2.0,
        FIXTURE_CENTER_HZ + FIXTURE_RATE_HZ / 2.0,
    );
    // Somewhere this run has never been and cannot have been: 2.4 GHz, a thousand capture windows
    // away from the fixture's 100.8 MHz.
    let (flo, fhi) = (2.400e9, 2.410e9);

    // T-920: waits for the ring to be able to contribute, not merely for a cell to read
    // `observed` — the open dwell can supply the latter while the ring is still allocating, and
    // every assertion below is about the RING's journal. See [`wait_for_ring_backed_coverage`].
    let tuned =
        wait_for_ring_backed_coverage(addr, &format!("/api/coverage?f_lo={lo}&f_hi={hi}&cells=8"));

    // ---- state 1/2: the tuned band was sampled, and the answer says how ----
    let cells = tuned["any"]["cells"].as_array().expect("cells").clone();
    assert_eq!(cells.len(), 8, "{tuned}");
    let observed: Vec<&Value> = cells
        .iter()
        .filter(|c| c["state"] == json!("observed"))
        .collect();
    assert!(!observed.is_empty(), "{tuned}");
    for c in &observed {
        // A real sampling, not a placeholder: a positive duty over a positive interval, at the
        // rate the front end was actually running.
        assert!(c["duty"].as_f64().unwrap_or(0.0) > 0.0, "{c}");
        assert!(c["observed_s"].as_f64().unwrap_or(0.0) > 0.0, "{c}");
        assert!(c["spans"].as_u64().unwrap_or(0) >= 1, "{c}");
        assert_eq!(c["sample_rate_hz"].as_f64(), Some(FIXTURE_RATE_HZ), "{c}");
        assert_eq!(c["center_hz"].as_f64(), Some(FIXTURE_CENTER_HZ), "{c}");
    }
    // The window is the capture window, and the map was built from a tune history that names the
    // radio: the IQ ring journal contributed, and its spans are device-known.
    assert_eq!(
        tuned["window"]["source"],
        json!("capture-window"),
        "{tuned}"
    );
    let ring = tuned["sources"]
        .as_array()
        .expect("sources")
        .iter()
        .find(|s| s["kind"] == json!("iq-ring"))
        .expect("the iq-ring source row")
        .clone();
    assert_eq!(ring["available"], json!(true), "{tuned}");
    assert_eq!(ring["device_known"], json!(true), "{tuned}");
    assert!(ring["spans"].as_u64().unwrap_or(0) > 0, "{tuned}");
    // T-378: `device_known` is measured, not declared — every span this record contributed named
    // a front end. Both tune histories report the count, so a log still holding records written
    // before devices were logged discloses them instead of claiming a device-local horizon.
    assert_eq!(ring["named_spans"], ring["spans"], "{tuned}");
    for src in tuned["sources"].as_array().expect("sources") {
        let (spans, named) = (
            src["spans"].as_u64().expect("spans"),
            src["named_spans"].as_u64().expect("named_spans"),
        );
        assert!(named <= spans, "{src}");
        assert_eq!(src["device_known"], json!(named == spans), "{src}");
    }

    // ---- device-local: the grid is one radio's, and it is named ----
    let devices = tuned["devices"].as_array().expect("devices").clone();
    let named: Vec<&Value> = devices
        .iter()
        .filter(|d| d["named"] == json!(true))
        .collect();
    assert!(
        !named.is_empty(),
        "the tuned band must name the radio: {tuned}"
    );
    for d in &named {
        let id = d["device"].as_str().unwrap_or_default();
        assert!(!id.is_empty() && id != "unknown" && id != "any", "{d}");
        assert!(d["observed_cells"].as_u64().unwrap_or(0) > 0, "{d}");
    }
    // The union is labelled as a union and is never mistaken for a radio.
    assert_eq!(tuned["any"]["device"], json!("any"), "{tuned}");
    assert_eq!(tuned["any"]["named"], json!(false), "{tuned}");

    // ---- state 3: a band never tuned, and it is a DIFFERENT value, not a null measurement ----
    let (st, fresh) = get(
        addr,
        &format!("/api/coverage?f_lo={flo}&f_hi={fhi}&cells=8"),
    );
    assert_eq!(st, 200, "{fresh}");
    assert_eq!(fresh["any"]["observed_cells"], json!(0), "{fresh}");
    assert_eq!(fresh["any"]["unobserved_cells"], json!(8), "{fresh}");
    assert_eq!(fresh["any"]["observed_fraction"], json!(0.0), "{fresh}");
    for c in fresh["any"]["cells"].as_array().expect("cells") {
        assert_eq!(*c, json!({ "state": "unobserved" }), "{fresh}");
        // The whole point: no field here can be read as a measured zero.
        assert!(c.get("shade").is_none(), "{c}");
        assert!(c.get("duty").is_none(), "{c}");
        assert!(c.get("observed_s").is_none(), "{c}");
    }
    // Two devices' coverage is never unioned into one device's claim: no named front end asserts
    // it sampled a band it never went to.
    for d in fresh["devices"].as_array().expect("devices") {
        assert_eq!(d["observed_cells"], json!(0), "{d}");
    }
    // And the two answers are genuinely different values on the wire, not the same null.
    assert_ne!(tuned["any"]["cells"], fresh["any"]["cells"]);
    assert_ne!(observed[0]["state"], json!("unobserved"));
    // The rule is stated in the response rather than left to the client to invent.
    assert_eq!(
        tuned["resolution"]["grey_rule"],
        json!(
            "grey a cell if and only if its state is \"unobserved\"; \"unknown\" is not grey and \
             not a level — draw it as a fourth thing (hatching, per T-413); \"excluded\" (T-595) \
             is spectrum the radio DID sample and the analysis skipped — draw the measurement, \
             mark it distinctly, never grey"
        ),
        "{tuned}"
    );
    // T-595: every OBSERVED cell states how much of its extent the analysis actually ran on, so
    // "excluded" is a number a client can check and not a word it must take. An unobserved cell
    // carries no such key — that absence is the structural rule above, unchanged.
    for c in tuned["any"]["cells"].as_array().expect("cells") {
        if c["state"] == json!("observed") {
            let a = c["analysed_s"].as_f64().unwrap_or_else(|| panic!("{c}"));
            let o = c["observed_s"].as_f64().unwrap();
            assert!(a > 0.0 && a <= o, "an observed cell was analysed: {c}");
        }
    }
    for c in fresh["any"]["cells"].as_array().expect("cells") {
        assert!(c.get("analysed_s").is_none(), "{c}");
    }
    assert_eq!(fresh["any"]["excluded_cells"], json!(0), "{fresh}");
    // T-342: and so is the SHADE's rule. A 0–1 number normalised against a range the response never
    // named is a measurement the consumer cannot check or match: the strip must be able to share
    // the waterfall's scaling, which needs the range and the scale on the wire, not just the ratio.
    //
    // The rule text is unconditional: it describes the fold, so it is served whether or not the
    // pyramid held a level to fold. Asserted here on the default-window answer, which is the one a
    // client gets before any history exists.
    assert_eq!(tuned["shade"]["fold"], json!("max-hold"), "{tuned}");
    assert!(
        tuned["shade"]["rule"]
            .as_str()
            .is_some_and(|s| s.contains("max-hold")),
        "the fold must name itself: {tuned}"
    );
    assert!(
        tuned["shade"]["unobserved"]
            .as_str()
            .is_some_and(|s| s.contains("the max of nothing is")),
        "{tuned}"
    );

    // The VALUES are asked over an explicit window, and that is not a convenience: T-383 measured
    // why. `shades()` folds with `nt = 1`, so `overview_level` sizes the tier by the WHOLE window.
    // Over the default capture window (`retention_s` = 120 s here) the coarsest adequate tier is
    // level 1, whose cells are 60 s wide — and a level-1 cell exists only once a level-0 block
    // (60 s, aligned to the Unix epoch) has sealed, `seal_lag` = 2 s later. So the time for this
    // route's `range_db` to appear on the DEFAULT window is
    //
    //     (time to the next epoch-aligned 60 s boundary) + 2 s   ∈ (2 s, 62 s]
    //
    // — measured six times running, each within 0.1 s of that formula (2026-09-17): to_next 26.86 s
    // → 29.07 s, 55.67 → 57.82, 56.03 → 58.20, 55.78 → 57.90, 56.02 → 58.11, 55.45 → 57.65. The old
    // 60 s budget therefore expired whenever the run began in the first ~2.2 s of a wall-clock
    // minute, on an idle machine, for a reason that has nothing to do with the code under test; it
    // was recorded twice as a "load flake" (T-320, T-383) and the isolated times people quoted
    // (53.2 s, 21.1 s, 5.3 s, 58.2 s) are just that phase. A 20 s window resolves at level 0, whose
    // 1 s cells the run has actually written, so the wait becomes a real bound on real work
    // (measured 0.00–0.11 s over nine runs) instead of a race with the clock. The budget is 30 s
    // rather than 60 s because it now bounds a quantity whose range we have measured: it is two
    // orders of magnitude above it, and the point of the number is to be nowhere near the edge.
    //
    // T-426 FIXED the defect this comment used to record as deliberately unfixed: on the default
    // window the strip carried no shade for the first minute of a server's life even though the
    // finer levels held the data, because the coarsest adequate tier was the only tier consulted.
    // The value assertion for that fix is the block immediately below, on the DEFAULT window; the
    // explicit 20 s window here stays because it is a different property — a bound on real work
    // rather than on the clock — and because it is the control that keeps the default-window
    // answer honest about which tier it read.
    let t1_s = tuned["window"]["t1_s"].as_f64().expect("a live edge");
    let (wt0, wt1) = (t1_s - 20.0, t1_s);
    let mut shaded = Value::Null;
    wait_for(
        "the spectrum history to give the strip a shade to explain",
        Duration::from_secs(30),
        || {
            let (st, got) = get(
                addr,
                &format!("/api/coverage?f_lo={lo}&f_hi={hi}&cells=8&t0={wt0}&t1={wt1}"),
            );
            shaded = got;
            st == 200 && shaded["shade"]["range_db"].is_object()
        },
    );
    let sh = &shaded["shade"];
    assert_eq!(sh["fold"], json!("max-hold"), "{shaded}");
    assert_eq!(sh["scale"], json!("dbfs-per-hz"), "{shaded}");
    assert_eq!(
        sh["normalisation"],
        json!("0 at `range_db.lo`, 1 at `range_db.hi`, linear in dB and clamped"),
        "{shaded}"
    );
    let (rlo, rhi) = (
        sh["range_db"]["lo"].as_f64().expect("shade range lo"),
        sh["range_db"]["hi"].as_f64().expect("shade range hi"),
    );
    assert!(rhi >= rlo, "{shaded}");
    // The values are the stated normalisation of the stated range, not an arbitrary ratio: every
    // shade served lies in [0, 1].
    let shades: Vec<f64> = shaded["any"]["cells"]
        .as_array()
        .expect("cells")
        .iter()
        .filter_map(|c| c["shade"].as_f64())
        .collect();
    assert!(!shades.is_empty(), "{shaded}");
    assert!(shades.iter().all(|s| (0.0..=1.0).contains(s)), "{shaded}");
    assert!(
        sh["unobserved"]
            .as_str()
            .is_some_and(|s| s.contains("the max of nothing is")),
        "{shaded}"
    );
    // The tier that answered, on the wire (T-426). Here it is the PREFERRED one and no fallback is
    // involved: 20 s drawn in one row is a 20 s cell, and the coarsest tier whose cells are no
    // larger than that is level 0 (1 s) — level 1's are 60 s. So `level` is not hard-wired to the
    // fallback's answer; it reports whichever tier the read actually used.
    assert_eq!(sh["level"], json!(0), "{shaded}");
    assert_eq!(sh["src_t_cell_s"], json!(1.0), "{shaded}");
    assert_eq!(sh["src_f_cell_hz"], json!(6250.0), "{shaded}");

    // ---- T-426: the DEFAULT window is shaded in the first minute of a server's life ------------
    //
    // THE DEFECT, in the user's words (CLAUDE.md, 2026-09-16): *"whenever data exists for that
    // window it must be shown; a surface may render grey/empty only where data genuinely does not
    // exist. 'We have it but didn't render it' is a bug."* On the default capture window this
    // route used to serve NO shade at all until the first epoch-aligned minute had sealed, while
    // level 0 had held 1 s cells the whole time. `shades()` folds with `nt = 1`, so the tier is
    // sized by the whole 120 s window and the coarsest ADEQUATE tier is level 1 (60 s cells) — and
    // a level-1 cell exists only once a level-0 block seals, `seal_lag` = 2 s after an
    // epoch-aligned boundary. T-383 measured that wait at (2 s, 62 s], six runs within 0.1 s of
    // the formula. The old code consulted that tier and stopped.
    //
    // THE VALUE ASSERTED: over the default window, the strip has a real shade range and real
    // shades — a value, not a present field (T-315). On the old code this call answers
    // `"range_db": null` with no `shade` key on any cell.
    //
    // AND IT IS ASSERTED WITHOUT WAITING FOR ANYTHING, which is the half that makes it a guard at
    // all. The 20 s window above has just proved that level 0 holds queryable cells for the last
    // 20 s of this run; the default window is a 120 s superset of it over the same band, so the
    // same cells must answer on the FIRST call. A `wait_for` here would be worse than useless —
    // MEASURED, by mutation: with the fallback disabled the test PASSED in 19.5 s under a 30 s
    // budget, because the old code does eventually shade, once the epoch-aligned minute seals.
    // Any budget over 2 s lets the defect wait its way to green, which is how the same clock
    // dependency spent a month being recorded as a load flake (T-320/T-383). No budget, no clock:
    // one request, one answer, measured at 8.5–10.3 ms over four runs before the wait was removed.
    let (st, dflt) = get(addr, &format!("/api/coverage?f_lo={lo}&f_hi={hi}&cells=8"));
    assert_eq!(st, 200, "{dflt}");
    assert!(
        dflt["shade"]["range_db"].is_object(),
        "the default window must show the data the finer tier is already holding: {dflt}"
    );
    let ds = &dflt["shade"];
    let (dlo, dhi) = (
        ds["range_db"]["lo"].as_f64().expect("shade range lo"),
        ds["range_db"]["hi"].as_f64().expect("shade range hi"),
    );
    assert!(dhi >= dlo, "{dflt}");
    assert_eq!(ds["scale"], json!("dbfs-per-hz"), "{dflt}");
    let dshades: Vec<f64> = dflt["any"]["cells"]
        .as_array()
        .expect("cells")
        .iter()
        .filter_map(|c| c["shade"].as_f64())
        .collect();
    assert!(
        !dshades.is_empty(),
        "the default window must show the data the finer tier holds: {dflt}"
    );
    assert!(dshades.iter().all(|s| (0.0..=1.0).contains(s)), "{dflt}");
    // AND THE LEVEL TELLS THE TRUTH ABOUT WHICH TIER ANSWERED. Falling back silently would trade
    // one lie for another, so `level` and `src_t_cell_s` are the fallback's own disclosure: the
    // pair must agree with the default ladder (level 0 = 1 s cells, level 1 = 60 s), and
    // `src_t_cell_s` must never exceed the drawn cell — a finer source is MORE resolution than the
    // picture asked for, folded down, which is the direction that invents nothing.
    let dlevel = ds["level"].as_u64().expect("the tier that answered");
    let dsrc = ds["src_t_cell_s"].as_f64().expect("that tier's time cell");
    assert_eq!(
        dsrc,
        match dlevel {
            0 => 1.0,
            1 => 60.0,
            l => panic!("neither tier can answer a 120 s window drawn in one row: {l}: {dflt}"),
        },
        "{dflt}"
    );
    let drawn_cell_s = dflt["grid"]["t_cell_s"].as_f64().expect("the drawn cell");
    assert!(dsrc <= drawn_cell_s, "{dflt}");
    // THE PHASE, and it is the branch that makes the level assertion exact rather than permissive.
    // Every frame this run wrote is at or after `launch_s`, so the earliest level-0 block that can
    // hold any of them ends at the next epoch-aligned 60 s boundary after `launch_s`, and seals
    // `seal_lag` = 2 s later. Until then NO level-1 cell can exist for this server and level 0 is
    // the only tier that can answer — which is exactly the regime the defect lived in. Measuring
    // the phase rather than assuming it is T-383's rule applied to the test's own clock. With no
    // wait between launch and here (the whole test measures 0.61–0.69 s) the branch is taken on
    // every run: `now − launch_s` is under a second against a first seal that is 2–62 s away. The
    // else-branch exists only so that a machine slow enough to cross that boundary mid-test
    // reports the honest weaker claim instead of failing for a reason that is not the code's.
    // The strict level-1 control — the same window once a level-1 cell exists — is not here, where
    // it could only be had by waiting out a wall-clock minute: it is
    // `hk_api::query::tests::a_populated_finer_tier_answers_when_the_preferred_one_is_empty_and_says_so`,
    // where the seal is explicit and the answer flips back to level 1 with no clock involved.
    const SEAL_LAG_S: f64 = 2.0;
    let first_seal_s = (launch_s / 60.0).floor() * 60.0 + 60.0 + SEAL_LAG_S;
    let now_s = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("a clock after 1970")
        .as_secs_f64();
    if now_s < first_seal_s {
        assert_eq!(
            dlevel,
            0,
            "no level-1 cell can exist {:.1} s before the first seal, so the finer tier must have \
             answered: {dflt}",
            first_seal_s - now_s
        );
    }

    // ---- the backfill: going Live for a range that has history starts POPULATED, not black ----
    // The same band the coverage map says was sampled has spectrum history behind it, so the first
    // picture drawn for it carries content rather than an empty box.
    let (st, drawn) = get(
        addr,
        &format!("/api/timeline?f_lo={lo}&f_hi={hi}&columns=32&rows=4"),
    );
    assert_eq!(st, 200, "{drawn}");
    assert!(
        drawn["grid"]["observed_cells"].as_u64().unwrap_or(0) > 0,
        "a range with history must backfill, not start black: {drawn}"
    );
    assert!(drawn["grid"]["range_db"].is_object(), "{drawn}");
    // The control: a genuinely fresh range has nothing to backfill, and says so with unobserved
    // cells rather than a drawn-but-quiet band. Backfill is not a constant.
    let (st, blank) = get(
        addr,
        &format!("/api/timeline?f_lo={flo}&f_hi={fhi}&columns=32&rows=4"),
    );
    assert_eq!(st, 200, "{blank}");
    assert_eq!(blank["grid"]["observed_cells"], json!(0), "{blank}");
    assert!(blank["grid"]["range_db"].is_null(), "{blank}");
    for v in blank["grid"]["max_db"].as_array().expect("max_db") {
        assert!(v.is_null(), "not observed is not quiet: {blank}");
    }

    // ---- refusals ----
    for bad in [
        "cells=8",                     // no band: the region is required here
        "f_lo=1000&cells=8",           // half a band
        "f_lo=1000&f_hi=2000&cells=0", // cell budget out of range
        "f_lo=1000&f_hi=2000&cells=99999",
        "f_lo=1000&f_hi=2000&t0=1",   // half a window
        "f_lo=1000&f_hi=2000&nope=1", // unknown parameter
    ] {
        let (st, v) = get(addr, &format!("/api/coverage?{bad}"));
        assert_eq!(st, 400, "expected 400 for {bad}: {v}");
    }
    // An explicit window is honoured and says so.
    let (st, v) = get(
        addr,
        &format!("/api/coverage?f_lo={lo}&f_hi={hi}&cells=4&t0=1&t1=2"),
    );
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["window"]["source"], json!("requested"), "{v}");
    assert_eq!(v["window"]["span_s"], json!(1.0), "{v}");
    // A window in 1970 saw nothing, whatever the band: the map is time-scoped, not a band property.
    assert_eq!(v["any"]["observed_cells"], json!(0), "{v}");
    // T-507: and 1970 is before this server recorded anything, with nothing discarded that could
    // say otherwise — so it is honestly grey, `unobserved`, not the fourth state. (T-423 served
    // `"unknown"` here: every server's memory was taken to be lost before its first sample, which
    // painted a fresh server's past purple.)
    assert_eq!(v["horizon"]["forgotten"], Value::Null, "{v}");
    assert_eq!(v["any"]["unknown_cells"], json!(0), "{v}");
    assert_eq!(v["any"]["unobserved_cells"], json!(4), "{v}");
    for c in v["any"]["cells"].as_array().expect("cells") {
        assert_eq!(*c, json!({ "state": "unobserved" }), "{v}");
    }

    let (st, _) = call(
        addr,
        "POST",
        "/api/coverage",
        Some(&format!("Bearer {TOKEN}")),
        Some("{}"),
    );
    assert_eq!(st, 405);
    stop_server(serving);
}

/// T-621 (with T-596): **a refused IQ capture ring is a product state, and the coverage answer
/// stays honest across it.**
///
/// A portable device fills its disk, and when the ring will not fit above the free-space floor
/// `hk serve` refuses it and carries on: `docs/api.md` "IQ capture buffer" —
/// `enabled: false`, `allocation: "refused"`, and a `reason` naming the bytes needed, the floor
/// and the free space. Until now nothing asserted what the *coverage record* then says, and the
/// gap was not academic: on a machine near the floor the two tests around this one failed on
/// coverage assertions because of it.
///
/// Coverage reads **two** tune histories (`sources`: the ring's journal and the observation log).
/// Losing one is a loss of evidence, and this project's rule is that a lost or unsure state is
/// never served as the cheap one — `Coverage::Unobserved` is not "we have no record". So the
/// honest answer, asserted here on values:
///
///  1. **The refusal is disclosed, in full.** `/api/iqbuffer` says `enabled: false`,
///     `allocation: "refused"`, with a `reason` carrying the three numbers.
///  2. **The source row says it did not answer.** `sources[iq-ring]` is `available: false` with
///     **zero** spans — and the `observation-log` row is `available: true` and does answer. A
///     client can tell "this record had nothing here" from "this record was not consulted".
///  3. **The surviving evidence still colours the map.** The band the radio is demonstrably tuned
///     to is `observed`, at the fixture's own centre and rate. A lost *source* is not evidence
///     that nothing looked: greying the tuned band here would be the mirror of T-368's bug.
///  4. **And it colours only that.** The control, as in T-368: a band 2.4 GHz away this run never
///     visited is still `unobserved`, with no key readable as a measured zero — and **not**
///     `unknown`, because one tune history is still here and it can say. (`unknown` is for a
///     server that recorded and lost, T-507.)
///
/// The refusal is forced through the allocator's own seam — a mocked `fs_space` reporting a
/// nearly-full filesystem — so no disk is filled and nothing here depends on the machine's state.
#[test]
fn coverage_survives_a_refused_iq_ring_and_never_calls_the_lost_evidence_grey() {
    /// A filesystem with 1 MiB free of 8 GiB. The default floor is 10 % of the filesystem within
    /// 2..8 GiB, so 819 MiB here; not even the two slots of the smallest ring fit above it, which
    /// is exactly the documented `"refused"` case. Nothing is allocated or written: the ring never
    /// opens.
    struct FullDisk;
    impl hk_store::iqbuffer::IqBufferHooks for FullDisk {
        fn fs_space(&self, _: &std::path::Path) -> std::io::Result<hk_store::iqbuffer::FsSpace> {
            Ok(hk_store::iqbuffer::FsSpace {
                free: 1 << 20,
                total: 8 << 30,
            })
        }
    }

    let dir = temp_data_dir();
    let guard = TempDataDirGuard::new(dir.clone());
    let serving = start(&ServeOptions {
        source: ServeSource::HackRf {
            spec: format!("mock:{}", fixture_path().display()),
            extra: Vec::new(),
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
        iq_buffer: hk_cli::pipeline::IqBufferArgs {
            retention_s: Some(120.0),
            max_bytes: Some(64 << 20),
        },
        iq_buffer_hooks: Some(hk_cli::pipeline::IqBufferHooksOverride(
            std::sync::Arc::new(FullDisk),
        )),
    })
    .unwrap();
    let addr = serving.server.local_addr();
    let _guard = guard;

    // ---- 1. the refusal is disclosed, with its three numbers ----
    let mut iq = Value::Null;
    wait_for(
        "the ring allocation to settle (it cannot open on this filesystem)",
        Duration::from_secs(30),
        || {
            let (st, got) = get(addr, "/api/iqbuffer");
            iq = got;
            st == 200 && iq["allocation"] != json!("allocating")
        },
    );
    assert_eq!(iq["enabled"], json!(false), "{iq}");
    assert_eq!(iq["allocation"], json!("refused"), "{iq}");
    let reason = iq["reason"]
        .as_str()
        .expect("a refused ring says why")
        .to_owned();
    // The documented reason, and it is arithmetic rather than a shrug: the three numbers it names
    // (bytes needed, the floor, the free space) must be the ones that actually refuse the ring.
    for want in ["needs", "free-space floor", "are free"] {
        assert!(
            reason.contains(want),
            "{reason:?} does not name {want:?}: {iq}"
        );
    }
    let nums: Vec<u64> = reason
        .split(|c: char| !c.is_ascii_digit())
        .filter(|w| !w.is_empty())
        .map(|w| w.parse().expect("a decimal byte count"))
        .collect();
    let [need, floor, free] = nums[..] else {
        panic!("the reason names exactly three byte counts: {reason:?}");
    };
    assert!(
        free < need + floor,
        "the three numbers must be the ones that refuse the ring: {need} + {floor} <= {free} \
         would have fit. {reason:?}"
    );
    // And the status's own space fields are `null` while disabled, as documented: nothing here
    // can be read as a measured filesystem this run is using.
    for k in ["fs_free_bytes", "fs_total_bytes", "min_free_bytes"] {
        assert_eq!(iq[k], Value::Null, "{k} on a disabled ring: {iq}");
    }
    // This is the precondition helper's own subject: on this server it fires, and says so.
    let named = iq_ring_unavailable(addr).expect("the helper sees a settled refusal");
    assert!(
        named.contains(&reason),
        "the helper quotes the server: {named:?}"
    );

    // ---- 2/3. the surviving tune history still answers, and the ring row says it did not ----
    let (lo, hi) = (
        FIXTURE_CENTER_HZ - FIXTURE_RATE_HZ / 2.0,
        FIXTURE_CENTER_HZ + FIXTURE_RATE_HZ / 2.0,
    );
    let mut tuned = Value::Null;
    wait_for(
        "the observation log alone to report the tuned band as sampled",
        Duration::from_secs(90),
        || {
            let (st, got) = get(addr, &format!("/api/coverage?f_lo={lo}&f_hi={hi}&cells=8"));
            if st != 200 {
                return false;
            }
            tuned = got;
            tuned["any"]["observed_cells"].as_u64().unwrap_or(0) > 0
        },
    );
    let src = |kind: &str| -> Value {
        tuned["sources"]
            .as_array()
            .expect("sources")
            .iter()
            .find(|s| s["kind"] == json!(kind))
            .unwrap_or_else(|| panic!("the {kind} source row: {tuned}"))
            .clone()
    };
    let ring = src("iq-ring");
    // A refused ring has no journal, so it contributes nothing and can name nothing.
    assert_eq!(
        ring["spans"],
        json!(0),
        "a refused ring contributes no spans: {tuned}"
    );
    assert_eq!(ring["named_spans"], json!(0), "{tuned}");
    // T-640 closed what T-621 could only report here: `available` is measured (can this source
    // contribute evidence?), not declared (is a handle wired?), so a refused ring reports
    // `false` — and `Evidence::unknown_rows` reads `no_tune_history` from it rather than
    // believing in a journal that was never allocated.
    assert_eq!(
        ring["available"],
        json!(false),
        "a refused ring holds no journal and must not claim one: {tuned}"
    );
    // T-920: and it says WHICH negative it is, end to end, in the words `/api/iqbuffer` served
    // above. A client that cannot tell `"refused"` from `"allocating"` cannot tell *this device
    // has no ring today* from *wait a moment*, and that silence is what sent a Linux worker
    // hunting a portability defect that was not there.
    assert_eq!(ring["state"], json!("refused"), "{tuned}");
    assert_eq!(
        ring["reason"].as_str(),
        Some(reason.as_str()),
        "the coverage row quotes the ring's own refusal verbatim: {tuned}"
    );
    let log = src("observation-log");
    assert_eq!(log["available"], json!(true), "{tuned}");
    // T-680: the surviving tune history is the observation log's sealed records AND the dwell in
    // flight it holds. Before T-680 the open dwell was clipped to a sealed-only record horizon,
    // which is `null` on this server until the first seal, so the wait above sat out the whole
    // first dwell (~60 s) with the tuned band unobserved. Now the dwell in flight answers from
    // the first poll, and the log's sealed spans may still be zero.
    let open = src("open-dwell");
    assert_eq!(open["available"], json!(true), "{tuned}");
    // T-920: an available source states that too, rather than only the unavailable ones, so
    // neither of the rows above can pass by carrying a complaint unconditionally.
    for r in [&log, &open] {
        assert_eq!(r["state"], json!("open"), "{tuned}");
        assert_eq!(r["reason"], Value::Null, "{tuned}");
    }
    assert!(
        log["spans"].as_u64().unwrap_or(0) + open["spans"].as_u64().unwrap_or(0) > 0,
        "the surviving tune history is what answered: {tuned}"
    );

    // The tuned band is observed, measured, and at the fixture's own tuning — the same values
    // T-368 asserts with a ring, reached here through one source instead of two.
    let cells = tuned["any"]["cells"].as_array().expect("cells").clone();
    assert_eq!(cells.len(), 8, "{tuned}");
    let observed: Vec<&Value> = cells
        .iter()
        .filter(|c| c["state"] == json!("observed"))
        .collect();
    assert!(!observed.is_empty(), "{tuned}");
    for c in &observed {
        assert!(c["duty"].as_f64().unwrap_or(0.0) > 0.0, "{c}");
        assert!(c["observed_s"].as_f64().unwrap_or(0.0) > 0.0, "{c}");
        assert!(c["spans"].as_u64().unwrap_or(0) >= 1, "{c}");
        assert_eq!(c["center_hz"].as_f64(), Some(FIXTURE_CENTER_HZ), "{c}");
        assert_eq!(c["sample_rate_hz"].as_f64(), Some(FIXTURE_RATE_HZ), "{c}");
    }

    // ---- 4. the control: never tuned is still `unobserved`, and never `unknown` ----
    let (st, fresh) = get(addr, "/api/coverage?f_lo=2.400e9&f_hi=2.410e9&cells=8");
    assert_eq!(st, 200, "{fresh}");
    assert_eq!(fresh["any"]["observed_cells"], json!(0), "{fresh}");
    assert_eq!(fresh["any"]["unobserved_cells"], json!(8), "{fresh}");
    assert_eq!(fresh["any"]["unknown_cells"], json!(0), "{fresh}");
    for c in fresh["any"]["cells"].as_array().expect("cells") {
        assert_eq!(*c, json!({ "state": "unobserved" }), "{fresh}");
    }

    stop_server(serving);
}

/// T-423, `docs/16` §7 step 2: **the coverage answer gains a time axis, and the wire can say the
/// fourth state.**
///
/// T-368 served a *column* — one row over the whole window — so a band the radio watched for ten
/// seconds of a minute came back `observed` for the whole minute. T-405's survey bar and T-411's
/// time navigator both read that as *"sampled, level not retained"* for a cell the radio was
/// demonstrably tuned away from at that instant. T-421 built the per-(t, f) computation; this is it
/// reaching the wire.
///
/// Four properties, each asserted on a **value** and each with its control (T-315):
///
///  1. **Per cell, not per column.** Over a window whose first row is inside capture and whose
///     later rows are not, row 0 is observed *against its own row* and the later rows are
///     `unobserved` with no measurement key at all. The **control** is the same window at
///     `rows=1`: it still says `observed`, which is exactly the answer that was wrong.
///  2. **The fold is grounded against the rows, not against a parent.** The column's `observed_s`
///     equals the **sum** of the rows' and its `duty` is re-derived against the column's own
///     extent — so a column at duty ≈ 1/6 and a row at duty ≈ 1 describe the same seconds. A
///     parent-vs-child comparison would pass on a rounded-up fold; this cannot (T-419/T-421).
///     The identity is asserted **exactly** on a window wholly behind the live edge (2b), where
///     both answers see a settled log; over the live window it is asserted as a direction (the
///     later answer cannot hold fewer seconds) plus the shape the bug would break. T-430: the
///     live window cannot carry an exact identity, because the radio captures more of it between
///     the two round-trips — one 16384-sample block, 6.8267 ms, at a time.
///  3. **A young server has forgotten nothing (T-507).** A window before this server recorded
///     anything answers `"unobserved"` — nothing looked, and nothing was discarded that could say
///     otherwise — never `"unknown"`. The fourth state is asserted on a server that has actually
///     recorded and lost something, in
///     `unknown_is_only_what_a_server_recorded_and_lost_and_the_plane_says_so`.
///  4. **Device stays in the key.** Every per-device grid carries the full `nt × nf` plane and the
///     union wears `"any"` with `"named": false` (T-259/T-305).
///
/// And the same coverage plane on `/api/timeline`, aligned cell-for-cell with the grid it drew.
#[test]
fn coverage_answers_per_cell_in_time_and_says_when_it_no_longer_knows_whether_it_looked() {
    // One row's duration. The window below is six of them: one inside capture, five after the live
    // edge, so rows 2..6 stay un-sampled for at least `ROW_S` seconds after the edge is read.
    const ROW_S: f64 = 3.0;
    const ROWS: usize = 6;
    const CELLS: usize = 4;

    let (_dir_guard, serving, addr) = start_server_retaining(Some(120.0));
    let (lo, hi) = (
        FIXTURE_CENTER_HZ - FIXTURE_RATE_HZ / 2.0,
        FIXTURE_CENTER_HZ + FIXTURE_RATE_HZ / 2.0,
    );

    // The window is built from the capture the server reports, not from wall clock: a replay runs
    // on its own clock (T-125), and a window anchored to `now` would ask about the future.
    let mut edge = 0.0f64;
    let mut buffered_t0 = 0.0f64;
    wait_for(
        "the ring to buffer more than one row of capture",
        Duration::from_secs(90),
        || {
            // T-621: a refused ring buffers nothing, ever. Name the refusal now rather than
            // timing out in 90 s on a window that cannot arrive.
            require_iq_ring(addr);
            let (st, got) = get(addr, "/api/timeline");
            let b = &got["window"]["buffered"];
            match (st, b["t0_s"].as_f64(), b["t1_s"].as_f64()) {
                (200, Some(a), Some(z)) if z - a > ROW_S + 0.5 => {
                    buffered_t0 = a;
                    edge = z;
                    true
                }
                _ => false,
            }
        },
    );
    let (t0, t1) = (edge - ROW_S, edge - ROW_S + ROW_S * ROWS as f64);
    let band = format!("f_lo={lo}&f_hi={hi}&cells={CELLS}&t0={t0}&t1={t1}");

    // ---- 1. per cell, not per column ----
    let (st, g) = get(addr, &format!("/api/coverage?{band}&rows={ROWS}"));
    assert_eq!(st, 200, "{g}");
    assert_eq!(g["grid"]["rows"], json!(ROWS), "{g}");
    assert_eq!(g["grid"]["cells"], json!(CELLS), "{g}");
    assert!(
        (g["grid"]["t_cell_s"].as_f64().expect("t_cell_s") - ROW_S).abs() < 1e-6,
        "{g}"
    );
    let cells = g["any"]["cells"].as_array().expect("cells").clone();
    assert_eq!(cells.len(), ROWS * CELLS, "{g}");

    // Row 0 is inside capture: observed against ITS OWN row, not against the whole window.
    let mut row0_observed_s = 0.0f64;
    for c in &cells[..CELLS] {
        assert_eq!(
            c["state"],
            json!("observed"),
            "row 0 is inside capture: {g}"
        );
        let duty = c["duty"].as_f64().expect("duty");
        assert!(
            duty > 0.9 && duty <= 1.0,
            "row 0's duty is against the row, so it is ~1, not ~1/{ROWS}: {c} in {g}"
        );
        let obs = c["observed_s"].as_f64().expect("observed_s");
        assert!((obs - ROW_S * duty).abs() < 1e-6, "{c} in {g}");
        assert_eq!(c["sample_rate_hz"].as_f64(), Some(FIXTURE_RATE_HZ), "{c}");
        assert_eq!(c["center_hz"].as_f64(), Some(FIXTURE_CENTER_HZ), "{c}");
        row0_observed_s = obs;
    }
    // Rows 2.. begin at least ROW_S after the live edge this window was built from, so the radio
    // cannot have reached them: genuinely grey, and carrying nothing readable as a zero.
    for (i, c) in cells.iter().enumerate().skip(2 * CELLS) {
        assert_eq!(
            *c,
            json!({ "state": "unobserved" }),
            "cell {i} is past the live edge and inside the record horizon: {g}"
        );
    }
    // And the split runs one way. Capture reaches a row only after every earlier row, so the
    // states down the window are `observed`* then `unobserved`* — coverage never comes back after
    // the live edge. The boundary is row 1 or row 2 and the test does not care which: row 1 STARTS
    // at the edge this window was built from, so whether it is grey depends on how much capture
    // arrived while the request was in flight (see property 2).
    let states: Vec<&str> = (0..ROWS)
        .map(|r| cells[r * CELLS]["state"].as_str().expect("state"))
        .collect();
    let edge_row = states
        .iter()
        .position(|s| *s != "observed")
        .expect("this window runs past the live edge, so some row is not observed");
    assert!(
        states[edge_row..].iter().all(|s| *s == "unobserved"),
        "coverage does not resume after the live edge: {states:?} in {g}"
    );
    assert!(
        (1..=2).contains(&edge_row),
        "row 0 is inside capture and row 2 is a full row past the edge: {states:?} in {g}"
    );

    // ---- 1b. the answer states WHERE ITS EVIDENCE STOPS at the young end (T-532) ----
    //
    // The grey above is honest **at the instant it is served** and false a moment later: the rows
    // past the edge are being recorded while any copy of this answer ages, and a tile cache keeps
    // copies. So the answer names the horizon the grey is relative to, exactly as `oldest_record_s`
    // names the one at the other end, and a client that holds the answer may not draw grey past it.
    //
    // Asserted against the grid's own axes rather than against a clock: `as_of_s` must land in the
    // row where coverage stops — after the last `observed` row starts, and no later than the end of
    // the first `unobserved` one. Anything else and the field is not describing this plane.
    let as_of = g["horizon"]["as_of_s"]
        .as_f64()
        .expect("the horizon states how far forward this answer reaches");
    let row_t0 = |r: usize| t0 + r as f64 * ROW_S;
    assert!(
        as_of >= row_t0(edge_row - 1) && as_of <= row_t0(edge_row + 1),
        "`as_of_s` {as_of} does not land in the row where coverage stops (row {edge_row}, \
         [{}, {})): the young-end horizon and the plane it describes disagree, so a client \
         obeying it would draw grey over rows this answer never reached. {g}",
        row_t0(edge_row),
        row_t0(edge_row + 1)
    );
    // And a band this radio has never been near states the SAME horizon, read over any band
    // (T-881): the radio's record reaches `as_of_s` somewhere else, which is exactly the evidence
    // that 2.4 GHz was unobserved up to there — and past it a held copy cannot speak, here as
    // anywhere. Before T-881 this was `null` ("no record touches this band"), so a kept answer was
    // drawn grey over rows recorded after it was built; for a band the radio had LEFT that grey was
    // the newest rows of its fog-of-war shadow. Read after the tuned band's answer, so at or past
    // its horizon, and never past the window.
    let (st, far) = get(
        addr,
        &format!("/api/coverage?f_lo=2400000000&f_hi=2450000000&cells=8&rows=2&t0={t0}&t1={t1}"),
    );
    assert_eq!(st, 200, "{far}");
    let far_as_of = far["horizon"]["as_of_s"].as_f64().expect(
        "a band the radio is not on is still bounded by how far the radio's record reaches",
    );
    assert!(
        far_as_of >= as_of && far_as_of <= t1,
        "the untouched band's horizon {far_as_of} is not the radio's record reach (tuned band's \
         {as_of}, window end {t1}): {far}"
    );

    // ---- 2. the column is the SUM of the rows, and its duty is re-derived ----
    // The control for property 1, and the answer T-405/T-411 were reading: at rows=1 the very same
    // window still says "observed" for all of it.
    let (st, col) = get(addr, &format!("/api/coverage?{band}&rows=1"));
    assert_eq!(st, 200, "{col}");
    assert_eq!(col["grid"]["rows"], json!(1), "{col}");
    let c0 = &col["any"]["cells"].as_array().expect("cells")[0];
    assert_eq!(
        c0["state"],
        json!("observed"),
        "the column answer calls the whole window sampled — this is the bug the time axis fixes: \
         {col}"
    );
    let col_obs = c0["observed_s"].as_f64().expect("observed_s");
    let col_duty = c0["duty"].as_f64().expect("duty");
    // Grounded against the rows themselves (level 0), never against a coarser answer: the summed
    // seconds match, and the duty is those seconds over the COLUMN's own extent.
    let summed: f64 = (0..ROWS)
        .filter_map(|r| cells[r * CELLS]["observed_s"].as_f64())
        .sum();
    // T-430. This used to read `summed == row0_observed_s` to 1e-6 — "only row 0 was sampled" —
    // and row 1 begins AT the live edge a PREVIOUS round-trip reported, so that asserted the radio
    // captured nothing at all between two HTTP calls. MEASURED, ten idle runs: the quantity is not
    // a latency spread but a QUANTUM. The coverage log grows one capture block at a time, so the
    // seconds row 1 picks up are 0 or an exact multiple of 16384 / 2.4e6 s = 6.8267 ms — the
    // 6.8 ms "inter-request latency" T-425 and T-436 both reported is that constant, not a
    // measurement of the network. Measured range of the quantity, with ~1.5 ms of wall clock
    // between the calls: idle, one block on 1 of 10 runs here and one on the rows-vs-column pair;
    // under 32 CPU burners (load ~47), 1 of 20 here — the old assertion's failure — and 12 of 20
    // on the rows-vs-column pair, which only survived because its tolerance was 0.05 rather than
    // 1e-6. The two bounds differed in nothing but which happened to be generous.
    // Nothing invariant says zero blocks arrive, so nothing here asserts it. What IS invariant:
    // rows past the straddler are unobserved, so they contribute no seconds at all.
    let row1_obs = cells[CELLS]["observed_s"].as_f64().unwrap_or(0.0);
    assert!(
        (summed - (row0_observed_s + row1_obs)).abs() < 1e-6,
        "rows {}.. are unobserved, so the rows sum to rows 0 and 1: {g}",
        edge_row.max(2)
    );
    // The column answer is asked AFTER the rows answer and the coverage log only appends, so the
    // column can hold what the rows did not, never less. A direction, not a tolerance.
    assert!(
        col_obs >= summed - 1e-9,
        "the later answer cannot have seen less capture: {col} vs {g}"
    );
    assert!(
        (col_duty - col_obs / (t1 - t0)).abs() < 1e-6,
        "the column's duty is its seconds over its own extent: {col}"
    );
    // The bug this window exists to catch makes the column claim the WHOLE window (duty 1.0). The
    // bound is on the shape — one row of {ROWS} is ~0.167 — not on the request latency: reaching
    // 0.25 would need 1.5 s of capture, 220 blocks, to arrive between the two calls.
    assert!(
        col_duty < 0.25,
        "a column covering one row of {ROWS} is ~1/{ROWS} sampled, not 1: {col}"
    );

    // ---- 2b. the same identity, asked where it can be exact ----
    // Property 2 can only ever compare two answers taken from a moving edge. Behind the edge the
    // log is settled — it appends at the live edge and never rewrites — so a window wholly in the
    // past gives the rows answer and the column answer byte-identical evidence however far apart
    // the two requests fall. `sum(rows) == column` then holds to 1e-6 instead of to a tolerance
    // sized around a capture block, which is the claim property 2 was reaching for.
    let (pt0, pt1) = (edge - ROW_S, edge);
    let past = format!("f_lo={lo}&f_hi={hi}&cells={CELLS}&t0={pt0}&t1={pt1}");
    let (st, pr) = get(addr, &format!("/api/coverage?{past}&rows={ROWS}"));
    assert_eq!(st, 200, "{pr}");
    let pcells = pr["any"]["cells"].as_array().expect("cells").clone();
    assert_eq!(pcells.len(), ROWS * CELLS, "{pr}");
    for (i, c) in pcells.iter().enumerate() {
        assert_eq!(
            c["state"],
            json!("observed"),
            "cell {i} is wholly behind the live edge: {pr}"
        );
    }
    let psummed: f64 = (0..ROWS)
        .filter_map(|r| pcells[r * CELLS]["observed_s"].as_f64())
        .sum();
    let (st, pcol) = get(addr, &format!("/api/coverage?{past}&rows=1"));
    assert_eq!(st, 200, "{pcol}");
    let p0 = &pcol["any"]["cells"].as_array().expect("cells")[0];
    let (pcol_obs, pcol_duty) = (
        p0["observed_s"].as_f64().expect("observed_s"),
        p0["duty"].as_f64().expect("duty"),
    );
    assert!(
        (psummed - pcol_obs).abs() < 1e-6,
        "settled window: the column IS the sum of its rows: {pcol} vs {pr}"
    );
    assert!(
        (pcol_duty - pcol_obs / (pt1 - pt0)).abs() < 1e-6,
        "settled window: duty is seconds over the column's own extent: {pcol}"
    );
    assert!(
        pcol_duty > 0.9,
        "a window wholly inside capture is sampled throughout: {pcol}"
    );

    // ---- 3. a young server has forgotten nothing (T-507) ----
    // Three hours before the live edge: before this server recorded anything at all. Nothing
    // looked, and this server knows it — it has discarded nothing — so the answer is the true one,
    // `unobserved`. Before T-507 it was `"unknown"` for every such row: the purple wall a freshly
    // started server painted under a live page. The fourth state is asserted where it belongs, on a
    // server that has actually recorded and lost something:
    // `unknown_is_only_what_a_server_recorded_and_lost_and_the_plane_says_so`.
    let (ot0, ot1) = (edge - 3.0 * 3600.0 - 600.0, edge - 3.0 * 3600.0);
    let (st, old) = get(
        addr,
        &format!("/api/coverage?f_lo={lo}&f_hi={hi}&cells={CELLS}&rows=4&t0={ot0}&t1={ot1}"),
    );
    assert_eq!(st, 200, "{old}");
    let began = old["horizon"]["recording_began_s"]
        .as_f64()
        .expect("a recording server names when it began");
    let oldest = old["horizon"]["oldest_record_s"]
        .as_f64()
        .expect("the horizon names the oldest surviving record");
    assert!(
        began > ot1 && oldest > ot1,
        "this window is wholly before the server recorded anything: {old}"
    );
    assert!(
        began <= buffered_t0 + 1e-6,
        "recording began no later than the ring's oldest sample: {old}"
    );
    assert_eq!(old["horizon"]["forgotten"], Value::Null, "{old}");
    assert_eq!(old["horizon"]["unknown_rows"], json!(0), "{old}");
    assert_eq!(old["horizon"]["rows"], json!(4), "{old}");
    assert_eq!(old["any"]["unknown_cells"], json!(0), "{old}");
    assert_eq!(old["any"]["unobserved_cells"], json!(4 * CELLS), "{old}");
    for c in old["any"]["cells"].as_array().expect("cells") {
        assert_eq!(*c, json!({ "state": "unobserved" }), "{old}");
    }
    // And the horizon block inside the live window says the same.
    assert_eq!(g["horizon"]["unknown_rows"], json!(0), "{g}");

    // ---- 4. device stays in the key, with the time axis intact per device ----
    assert_eq!(g["any"]["device"], json!("any"), "{g}");
    assert_eq!(g["any"]["named"], json!(false), "{g}");
    let named: Vec<&Value> = g["devices"]
        .as_array()
        .expect("devices")
        .iter()
        .filter(|d| d["named"] == json!(true))
        .collect();
    assert!(!named.is_empty(), "the ring journal names the radio: {g}");
    for d in &named {
        let dc = d["cells"].as_array().expect("device cells");
        assert_eq!(
            dc.len(),
            ROWS * CELLS,
            "one plane per device, not a column: {d}"
        );
        assert_eq!(dc[0]["state"], json!("observed"), "{d}");
        assert_eq!(dc[5 * CELLS]["state"], json!("unobserved"), "{d}");
    }

    // ---- the same plane on /api/timeline, aligned with the grid it drew ----
    let (columns, rows) = (32usize, 4usize);
    let (st, tl) = get(
        addr,
        &format!("/api/timeline?f_lo={lo}&f_hi={hi}&columns={columns}&rows={rows}"),
    );
    assert_eq!(st, 200, "{tl}");
    let cov = &tl["coverage"];
    assert_eq!(cov["grid"]["nt"], json!(columns), "{tl}");
    assert_eq!(cov["grid"]["nf"], json!(rows), "{tl}");
    assert_eq!(cov["grid"]["aligned"], json!(true), "{tl}");
    let tcells = cov["any"]["cells"].as_array().expect("coverage cells");
    assert_eq!(tcells.len(), columns * rows, "{tl}");
    // The timeline spans the ring's configured retention, which reaches back before this run began.
    // Column 0 is therefore before capture, and the last column is at the live edge: the matched
    // pair, on the timeline's own axis.
    let t_cell = cov["grid"]["t_cell_s"].as_f64().expect("t_cell_s");
    let g0 = cov["grid"]["t0_s"].as_f64().expect("t0_s");
    assert!(
        buffered_t0 > g0 + t_cell,
        "this assertion needs a ring that has not filled its retention: {tl}"
    );
    for c in &tcells[..rows] {
        assert_ne!(
            c["state"],
            json!("observed"),
            "column 0 is before this run's capture began: {tl}"
        );
        // Whichever of the two non-observed states it is, it carries no measurement.
        assert!(
            c["state"] == json!("unobserved") || c["state"] == json!("unknown"),
            "{c} in {tl}"
        );
        assert_eq!(c.as_object().expect("cell").len(), 1, "{c} in {tl}");
    }
    for c in &tcells[(columns - 1) * rows..] {
        assert_eq!(
            c["state"],
            json!("observed"),
            "the live edge is sampled: {tl}"
        );
        assert!(c["duty"].as_f64().unwrap_or(0.0) > 0.0, "{c}");
        assert_eq!(c["sample_rate_hz"].as_f64(), Some(FIXTURE_RATE_HZ), "{c}");
        // No `shade` here: the timeline carries its own levels in `grid.max_db`, and a null would
        // read as "sampled, level not retained" — a claim this plane is not making.
        assert!(c.get("shade").is_none(), "{c}");
    }

    // ---- refusals: the new budget is bounded like every other one ----
    for bad in [
        format!("f_lo={lo}&f_hi={hi}&rows=0"),
        format!("f_lo={lo}&f_hi={hi}&rows=99999"),
        format!("f_lo={lo}&f_hi={hi}&rows=x"),
    ] {
        let (st, v) = get(addr, &format!("/api/coverage?{bad}"));
        assert_eq!(st, 400, "expected 400 for {bad}: {v}");
    }
    stop_server(serving);
}

/// **T-507: `"unknown"` is only what a server recorded and lost, and the plane says exactly that
/// — and since T-680, a ring evicting its first seconds is not a loss while the dwell that
/// recorded them is still in flight.**
///
/// A server whose IQ ring keeps two seconds has, a few seconds in, evicted the ring's journal of
/// its first seconds — and, until the first dwell seals (≤ 60 s), the observation log holds no
/// sealed record either. T-507 asserted those seconds as the fourth state, `"unknown"`: *recorded,
/// record since lost*. **T-680 changed that contract deliberately** (`docs/api.md`, "The live edge
/// has a third source"): the dwell in flight is a tune record this server still holds, in memory,
/// so nothing was lost — the band was the seal's bookkeeping lag, and it flipped to `observed`
/// the instant the seal landed, for samples that never changed. So on this server:
///
/// - `oldest_record_s` reaches back past the ring's floor, to the open dwell's own start, and at
///   least four seconds of rows the ring has discarded lie between the two (the wait's condition —
///   under T-596's clip `oldest_record_s` WAS the ring's floor, so the wait never succeeds: RED);
/// - those rows are `"observed"` over the tuned band, `unknown_rows` 0 — counted per cell;
/// - rows wholly **before `recording_began_s`** are still `"unobserved"`: nothing looked;
/// - `forgotten` is null.
///
/// And on `/api/tiles` the plane agrees with its **own** `horizon`, row for row, over a band never
/// tuned — `"unknown"` only between `recording_began_s` and `oldest_record_s` (now at most the
/// instant between the history's first frame and the observer's first poll), `"unobserved"`
/// elsewhere — so the plane cannot be right by accident. T-461's fail-closed half (a plane holding
/// any `"unknown"` never short-circuits) is asserted here whenever that band is non-empty; its
/// unconditional home is `tiles::tests::the_short_circuit_refuses_an_observed_plane_and_refuses_
/// unknown_over_data_the_store_holds`.
#[test]
fn unknown_is_only_what_a_server_recorded_and_lost_and_the_plane_says_so() {
    const CELLS: usize = 4;
    let (_dir_guard, serving, addr) = start_server_retaining(Some(2.0));
    let (lo, hi) = (
        FIXTURE_CENTER_HZ - FIXTURE_RATE_HZ / 2.0,
        FIXTURE_CENTER_HZ + FIXTURE_RATE_HZ / 2.0,
    );
    let (mut began, mut oldest, mut ring_t0) = (0.0f64, 0.0f64, 0.0f64);
    wait_for(
        "the ring to discard four seconds the dwell in flight still records",
        Duration::from_secs(60),
        || {
            let (st, v) = get(addr, &format!("/api/coverage?f_lo={lo}&f_hi={hi}&cells=1"));
            let h = &v["horizon"];
            let ring = get(addr, "/api/iqbuffer?limit=1").1["t0"].as_f64();
            match (
                st,
                h["recording_began_s"].as_f64(),
                h["oldest_record_s"].as_f64(),
                ring,
            ) {
                (200, Some(b), Some(o), Some(r)) if r - o >= 4.0 => {
                    (began, oldest, ring_t0) = (b, o, r);
                    true
                }
                _ => false,
            }
        },
    );
    assert!(
        began <= oldest,
        "recording began no later than the oldest record"
    );

    // ---- /api/coverage: four 1 s rows the ring has discarded, from the open dwell's start ----
    let (t0, t1) = (oldest, oldest + 4.0);
    assert!(t1 <= ring_t0, "every row here is before the ring's floor");
    let (st, v) = get(
        addr,
        &format!("/api/coverage?f_lo={lo}&f_hi={hi}&cells={CELLS}&rows=4&t0={t0}&t1={t1}"),
    );
    assert_eq!(st, 200, "{v}");
    let h = &v["horizon"];
    assert_eq!(h["forgotten"], Value::Null, "{h}");
    assert_eq!(
        h["unknown_rows"],
        json!(0),
        "the ring's discarded seconds are held by the dwell in flight - nothing was lost: {h}"
    );
    assert_eq!(v["any"]["unknown_cells"], json!(0), "{v}");
    let cells = v["any"]["cells"].as_array().expect("cells");
    assert_eq!(cells.len(), 4 * CELLS, "{v}");
    for (i, c) in cells.iter().enumerate() {
        assert_ne!(c["state"], json!("unknown"), "cell {i}: {v}");
        assert_ne!(c["state"], json!("unobserved"), "cell {i}: {v}");
    }
    let observed = cells
        .iter()
        .filter(|c| c["state"] == json!("observed"))
        .count();
    assert!(
        observed >= 4 * (CELLS / 2),
        "the tuned band is observed over the discarded seconds ({observed} cells): {v}"
    );

    // ---- and before recording began: four 1 s rows, unobserved - nothing looked ----
    // Half a row short of `began`, so no row edge sits on it (a row ending on it to the
    // nanosecond, after float formatting, may straddle it and count as unknown).
    let (st, v) = get(
        addr,
        &format!(
            "/api/coverage?f_lo={lo}&f_hi={hi}&cells={CELLS}&rows=4&t0={}&t1={}",
            began - 4.5,
            began - 0.5
        ),
    );
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["horizon"]["unknown_rows"], json!(0), "{v}");
    assert_eq!(v["any"]["unobserved_cells"], json!(4 * CELLS), "{v}");
    for (i, c) in v["any"]["cells"]
        .as_array()
        .expect("cells")
        .iter()
        .enumerate()
    {
        // No measurement keys: not a level.
        assert_eq!(*c, json!({ "state": "unobserved" }), "cell {i}: {v}");
    }

    // ---- /api/tiles: the same band, in the block `began` falls in, over a band never tuned ----
    const N: u64 = 32;
    let probe = get(
        addr,
        &format!("/api/tiles?level_f=0&level_t=0&f_index=0&t_index=0&cells={N}"),
    )
    .1;
    let (f_cell, t_cell) = (
        probe["extent"]["f_cell_hz"].as_f64().unwrap(),
        probe["extent"]["t_cell_s"].as_f64().unwrap(),
    );
    let far = (STATION_HZ / (f_cell * N as f64)).floor() as u64 + 2_000;
    let block_s = t_cell * N as f64;
    let t_index = (began / block_s).floor() as u64;
    let (st, tile) = get(
        addr,
        &format!("/api/tiles?level_f=0&level_t=0&f_index={far}&t_index={t_index}&cells={N}"),
    );
    assert_eq!(st, 200, "{tile}");
    let cov = &tile["coverage"];
    let th = &cov["horizon"];
    let tb = th["recording_began_s"].as_f64().expect("began");
    let to = th["oldest_record_s"].as_f64().expect("oldest");
    assert_eq!(tb, began, "{th}");
    // The expected plane, row by row, from the tile's own horizon.
    let tile_t0 = t_index as f64 * block_s;
    let want: Vec<&str> = (0..N)
        .map(|r| {
            let z = tile_t0 + (r + 1) as f64 * t_cell;
            if z <= tb || z > to {
                "unobserved"
            } else {
                "unknown"
            }
        })
        .collect();
    let states: Vec<String> = cov["states"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().to_string())
        .collect();
    let sel = cov["selected"]["plane"].as_u64().unwrap() as usize;
    let runs: Vec<u64> = cov["planes"][sel]["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n.as_u64().unwrap())
        .collect();
    let got: Vec<String> = runs
        .chunks(2)
        .flat_map(|p| std::iter::repeat_n(states[p[0] as usize].clone(), p[1] as usize))
        .collect();
    assert_eq!(got.len() as u64, N * N, "{cov}");
    for (r, w) in want.iter().enumerate() {
        for f in 0..N as usize {
            assert_eq!(got[r * N as usize + f], *w, "row {r} cell {f}: {th}");
        }
    }
    let unknown = want.iter().filter(|w| **w == "unknown").count();
    assert_eq!(th["unknown_rows"], json!(unknown), "{th}");
    // T-461, fail-closed, on a real server: any `"unknown"` means the full read. Since T-680 the
    // band is only the instant between the history's first frame and the observer's first poll,
    // so it may be empty here; `tiles::tests` pins the fail-closed half unconditionally.
    if unknown > 0 {
        assert_eq!(
            tile["resolution"]["short_circuit"]["applied"],
            json!(false),
            "{tile}"
        );
    }
    if unknown > 0 && unknown < N as usize {
        assert_eq!(
            tile["resolution"]["short_circuit"]["selected_plane_uniform"],
            Value::Null,
            "a plane that is part unknown and part unobserved is not uniform: {cov}"
        );
    }
    stop_server(serving);
}

/// T-438: `GET /api/tiles` and `GET /api/tiles/events`, as `docs/api.md` documents them.
///
/// The assertions are on **values**, not shapes (T-315), and the two that matter most are the ones
/// T-437 found broken elsewhere:
///
/// - **F2** — on `/api/history`, tightening the *frequency* budget 1.5× cost 34× of *time*
///   resolution and greyed a third of the window, because a per-axis budget there selects a
///   **level**. Here the address *is* the budget, so coarsening `level_f` moves the frequency cell
///   and leaves the time cell bit-identical, and the band the finer tile observed is still observed
///   at the coarser address.
/// - **T-434** — a welded ladder is the **diagonal** of its own lattice, so `scheme=1` off the
///   diagonal is a `404` that says so, never a snap to a level whose time cell is a day.
#[test]
fn tile_route_addresses_independent_axis_levels_and_a_budget_never_greys_a_cell() {
    let (_dir_guard, serving, addr) = start_server();
    const N: u64 = 32;
    let tile = |lf: u64, lt: u64, fi: u64, ti: u64| {
        format!("/api/tiles?level_f={lf}&level_t={lt}&f_index={fi}&t_index={ti}&cells={N}")
    };

    // The address is resolved against the open pyramid's own geometry, so the cell sizes come back
    // from the server rather than being assumed here.
    //
    // **T-484: the floor is not a constant any more.** The view lattice's node (0, 0) is the display
    // plan's own bin and row (`fs / spectrum_fft_len` x the display row period,
    // `hk_pipeline::history::view_geometry`), so it moves with the run's sample rate; the 6250 Hz x
    // 1 s that used to be asserted here was T-439's fixed floor. Pinning a number would restate that
    // function in a second place, which is what this file exists to avoid. What the assertions below
    // need is only that the floor is real and that the route reports one cell, not two.
    let (st, probe) = get(addr, &tile(0, 0, 0, 0));
    assert_eq!(st, 200, "{probe}");
    let f_cell = probe["extent"]["f_cell_hz"].as_f64().unwrap();
    let t_cell = probe["extent"]["t_cell_s"].as_f64().unwrap();
    assert!(
        f_cell.is_finite() && f_cell > 0.0 && t_cell.is_finite() && t_cell > 0.0,
        "{probe}"
    );
    assert_eq!(
        (
            probe["axes"]["frequency"]["cell_hz"].as_f64().unwrap(),
            probe["axes"]["time"]["cell_s"].as_f64().unwrap()
        ),
        (f_cell, t_cell),
        "`extent` and `axes` must name the same node (0, 0): {probe}"
    );
    assert_eq!(probe["key"]["scheme"], json!("view"), "{probe}");
    assert_eq!(probe["key"]["device"], json!("any"), "{probe}");
    // `any` is the union and can never wear one radio's identity (T-259/T-305, docs/16 §6.3).
    assert_eq!(probe["key"]["device_named"], json!(false), "{probe}");
    assert_eq!(probe["key"]["cells"], json!(N), "{probe}");
    assert_eq!(probe["extent"]["nt"], json!(N), "{probe}");
    assert_eq!(probe["extent"]["nf"], json!(N), "{probe}");
    assert_eq!(probe["grid"]["cells"], json!(N * N), "{probe}");
    assert_eq!(probe["cost"]["in_flight_limit"], json!(4), "{probe}");
    // T-630: the cap is server-wide, the SHARE is this client's, and an undeclared caller shares
    // the anonymous bucket — so `curl` and the CLI meet exactly the route they met before.
    assert_eq!(probe["cost"]["in_flight_share"], json!(4), "{probe}");
    // T-959: and what THIS client holds — this read's own slot, and nothing else, for a serial
    // caller. It is the number a client corrects its abandoned-read accounting against (an aborted
    // read holds its slot until the route finishes producing it), and the number that tells a `503`
    // over its own reads (`held >= share`, named in the body) from one over another client's.
    assert_eq!(probe["cost"]["in_flight_held"], json!(1), "{probe}");
    assert_eq!(probe["cost"]["clients"], json!(1), "{probe}");
    assert_eq!(probe["cost"]["client"], json!("-"), "{probe}");
    assert_eq!(probe["cost"]["reserved"], json!(0), "{probe}");
    assert_eq!(probe["cost"]["fair_share"], json!(true), "{probe}");
    // T-1021: every history-lock hold this request took, timed — at least the address lookup's,
    // and no single hold longer than all of them together.
    let lock = &probe["cost"]["lock"];
    assert!(lock["holds"].as_u64().is_some_and(|n| n >= 1), "{probe}");
    let (total, max, cpu) = (
        lock["hold_ms_total"].as_f64().unwrap(),
        lock["hold_ms_max"].as_f64().unwrap(),
        lock["hold_cpu_ms_total"].as_f64().unwrap(),
    );
    assert!(max >= 0.0 && max <= total && cpu >= 0.0, "{probe}");
    assert!(
        lock["yielded_ms"].as_f64().is_some_and(|y| y >= 0.0),
        "{probe}"
    );
    // A client that names itself is a client of its own, and two of them halve the share. The
    // second client here has never been served, so it is also what arms the bootstrap reserve.
    let (st, mine) = get(addr, &format!("{}&client=tab-one", tile(0, 0, 0, 0)));
    assert_eq!(st, 200, "{mine}");
    assert_eq!(mine["cost"]["client"], json!("tab-one"), "{mine}");
    assert_eq!(mine["cost"]["clients"], json!(2), "{mine}");
    assert_eq!(mine["cost"]["in_flight_share"], json!(2), "{mine}");
    assert_eq!(mine["cost"]["in_flight_limit"], json!(4), "{mine}");
    // Held is per client, not server-wide: this named client holds only its own read.
    assert_eq!(mine["cost"]["in_flight_held"], json!(1), "{mine}");
    // An id that is not one is not an error: it shares the anonymous bucket.
    let (st, odd) = get(addr, &format!("{}&client=not%20an%20id", tile(0, 0, 0, 0)));
    assert_eq!(st, 200, "{odd}");
    assert_eq!(odd["cost"]["client"], json!("-"), "{odd}");
    // Tile (0, 0, 0, 0) is 0 Hz in 1970: genuinely unobserved, and that is a coverage answer.
    assert_eq!(probe["grid"]["observed_cells"], json!(0), "{probe}");
    assert_eq!(probe["grid"]["range_db"], Value::Null, "{probe}");
    // **T-507.** 1970 is before this server recorded anything, and it has discarded nothing, so
    // the plane is uniformly `"unobserved"` — nothing looked — and the coverage map answers the tile
    // on its own (T-461). Before T-507 this was `"unknown"`: every server's memory was taken to be
    // lost before its first sample. T-461's fail-closed half — `unknown` never short-circuits — is
    // asserted on a server that has actually lost records, in
    // `unknown_is_only_what_a_server_recorded_and_lost_and_the_plane_says_so`.
    assert_eq!(
        probe["resolution"]["short_circuit"]["selected_plane_uniform"],
        json!("unobserved"),
        "{probe}"
    );
    assert_eq!(
        probe["resolution"]["short_circuit"]["applied"],
        json!(true),
        "{probe}"
    );
    assert_eq!(
        probe["coverage"]["horizon"]["unknown_rows"],
        json!(0),
        "{probe}"
    );

    let f_index = (STATION_HZ / (f_cell * N as f64)).floor() as u64;
    let t_index_of = |lt: u32| (unix_now() / (t_cell * (1u64 << lt) as f64 * N as f64)) as u64;

    // Wait for the pyramid to hold frames under the station, and **PIN the tile that proved it.**
    //
    // A level-0 tile here is `N` display rows — about 1.3 s — so re-reading the wall clock for the
    // next request names the NEXT tile whenever a boundary passes between the two calls, and that
    // tile is empty whenever capture's data edge lags the wall clock by more than the gap. Under a
    // gate's load it does (measured, task-t299's gate: the tile asked for began at +4.78 s after
    // `recording_began`, one full second past the store's newest frame at +3.77 s), so the
    // coverage map answered it on its own — uniformly `unobserved`, `answered: null` — and every
    // assertion below about the tile the wait had just seen observed was made about a different
    // one. A tile is a fixed region of capture time: once observed it stays observed, so every
    // read below that needs data addresses THIS index, never `unix_now()` again. (T-509's cause
    // was the same wall-clock re-derivation, one tile further down this test.)
    //
    // **And the tile is found from the DATA EDGE, not the wall clock** — the store's newest frame,
    // `shadow.edge_s`, which every tile answer carries. Polling the wall-clock tile instead waits
    // for capture to catch up with a clock it trails: under enough load it never does within the
    // budget (deflake-0922 measured the sibling shadow test timing out exactly that way).
    //
    // **And a tune record must exist** (`horizon.oldest_record_s` non-null) before the young-server
    // claims below. For the first moments of a server the history has a frame (so
    // `recording_began_s` is set) but the IQ ring's journal has no span yet, and T-507's
    // `oldest_record == None` branch serves every row after `recording_began_s` as `"unknown"`.
    // T-507 measured that window at ~30 ms; under a gate's load deflake-0922 caught `now_tile` in
    // it (0.37 s into a run: 9 rows `"unknown"`, `oldest_record_s: null`).
    let mut t_pin = 0u64;
    wait_for(
        "the station's tile at the data edge to be observed, with a tune record behind it",
        Duration::from_secs(60),
        || {
            let Some(edge) =
                get(addr, &tile(0, 0, f_index, t_index_of(0))).1["shadow"]["edge_s"].as_f64()
            else {
                return false;
            };
            let ti = ((edge - 0.5 * t_cell) / (t_cell * N as f64)) as u64;
            let v = get(addr, &tile(0, 0, f_index, ti)).1;
            let seen = v["grid"]["observed_cells"].as_u64().is_some_and(|n| n > 0)
                && v["coverage"]["horizon"]["oldest_record_s"].is_f64();
            if seen {
                t_pin = ti;
            }
            seen
        },
    );
    let (st, fine) = get(addr, &tile(0, 0, f_index, t_pin));
    eprintln!(
        "tile route: pinned t_index {t_pin} ({} tile(s) behind the wall clock's now), {} of {} cells observed",
        t_index_of(0).saturating_sub(t_pin),
        fine["grid"]["observed_cells"],
        N * N
    );
    assert_eq!(st, 200, "{fine}");
    // The level that ANSWERED, not the one the address implies (T-426). At the store's own floor
    // the tile sits on scheme 1's diagonal, so the node is exact and nothing was folded.
    assert_eq!(fine["resolution"]["answered"]["level"], json!(0), "{fine}");
    assert_eq!(
        fine["resolution"]["answered"]["exact_node"],
        json!(true),
        "{fine}"
    );
    assert_eq!(fine["axes"]["store_node"], json!(0), "{fine}");
    assert_eq!(fine["resolution"]["tried"], json!([0]), "{fine}");
    // T-439: the growing edge. `scheme=view` reads the view-scheme pyramid the live chain writes,
    // and its node (0, 0) is the same 6.25 kHz x 1 s cell scheme 1's level 0 is — so nothing about
    // the finest address moved, only what stands behind the coarser ones.
    assert_eq!(
        fine["resolution"]["answered"]["store"],
        json!("view-lattice"),
        "{fine}"
    );
    assert_eq!(
        fine["resolution"]["answered"]["levels"],
        json!(16),
        "the view lattice is 4 x 4 nodes, not a 5-rung ladder: {fine}"
    );
    for axis in ["frequency", "time"] {
        assert_eq!(
            fine["resolution"]["fold"][axis]["direction"],
            json!("exact"),
            "{axis}: {fine}"
        );
        assert_eq!(
            fine["resolution"]["fold"][axis]["replicated"],
            json!(false),
            "{axis}: {fine}"
        );
        assert_eq!(
            fine["resolution"]["fold"][axis]["served"],
            json!(N),
            "{axis}: {fine}"
        );
    }
    // This route reads the pyramid and only the pyramid, exactly as /api/history does. T-439 adds
    // the growing edge; claiming live first would be the stronger claim with no evidence.
    assert_eq!(
        fine["resolution"]["source"],
        json!("spectrum-history"),
        "{fine}"
    );
    assert_eq!(fine["resolution"]["live"], json!(false), "{fine}");
    // Grey is decided by the record-derived plane (T-423), and the key says whose.
    assert_eq!(
        fine["coverage"]["selected"]["device"],
        json!("any"),
        "{fine}"
    );
    assert_eq!(
        fine["coverage"]["selected"]["named"],
        json!(false),
        "{fine}"
    );
    assert_eq!(
        fine["coverage"]["selected"]["present"],
        json!(true),
        "{fine}"
    );
    // **T-467.** The plane is a run-length-encoded table of DISTINCT planes, not one JSON object
    // per cell: 99 % of a 19.34 MB tile body was this block, duplicated between `any` and
    // `devices[0]`, to carry the one field a renderer reads.
    assert_eq!(
        fine["coverage"]["encoding"],
        json!("plane-table-rle"),
        "{fine}"
    );
    // FOUR states, in the answer's own alphabet: a code is never read against one the client
    // assumed, `unknown` (T-423) is never spelled as `unobserved`, and `excluded` (T-595 — sampled,
    // deliberately left out of analysis: the DC notch) is neither. It is APPENDED, so every code an
    // older client cached keeps its meaning.
    assert_eq!(
        fine["coverage"]["states"],
        json!(["unobserved", "observed", "unknown", "excluded"]),
        "{fine}"
    );
    let planes = fine["coverage"]["planes"].as_array().unwrap();
    assert!(!planes.is_empty(), "{fine}");
    // Every plane decodes exactly: the runs are [code, count] pairs, every code is in the served
    // alphabet, and the counts sum to the plane's own cell count — which is the tile's grid.
    for p in planes {
        let runs: Vec<u64> = p["runs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n.as_u64().unwrap())
            .collect();
        assert_eq!(runs.len() % 2, 0, "{p}");
        let mut total = 0u64;
        let mut counts = [0u64; 4];
        for pair in runs.chunks(2) {
            assert!(pair[0] < 4, "code outside the served alphabet: {p}");
            counts[pair[0] as usize] += pair[1];
            total += pair[1];
        }
        assert_eq!(total, json!(N * N).as_u64().unwrap(), "{p}");
        assert_eq!(p["cells"], json!(N * N), "{p}");
        // The counts beside the runs are derived from the runs, so they cannot disagree with them.
        assert_eq!(p["observed_cells"], json!(counts[1]), "{p}");
        assert_eq!(p["unobserved_cells"], json!(counts[0]), "{p}");
        assert_eq!(p["unknown_cells"], json!(counts[2]), "{p}");
        assert_eq!(p["excluded_cells"], json!(counts[3]), "{p}");
        // And no cell on this plane carries a measurement key of any kind — there is nothing here
        // that could be read as a level of zero. The measurement plane is `grid`, separately.
        for k in [
            "duty",
            "observed_s",
            "last_s",
            "center_hz",
            "sample_rate_hz",
            "shade",
        ] {
            assert!(p.get(k).is_none(), "plane carries {k}: {p}");
        }
    }
    // An identical plane is carried ONCE: on this single-device server the union and the device's
    // plane are the same answer, so they are the same index rather than two copies.
    let n_planes = planes.len();
    let distinct: std::collections::BTreeSet<String> =
        planes.iter().map(|p| p["runs"].to_string()).collect();
    assert_eq!(distinct.len(), n_planes, "a plane is repeated: {fine}");
    // The selection the answer states is an index into that table, and `any` is the union.
    assert_eq!(fine["coverage"]["any"]["plane"], json!(0), "{fine}");
    assert_eq!(fine["coverage"]["selected"]["plane"], json!(0), "{fine}");
    // The whole tile body is now dominated by the measurement grid, not by the coverage plane —
    // the inversion this ticket bought, asserted rather than assumed.
    let cov_bytes = serde_json::to_string(&fine["coverage"]).unwrap().len();
    let grid_bytes = serde_json::to_string(&fine["grid"]).unwrap().len();
    assert!(
        cov_bytes < grid_bytes,
        "coverage {cov_bytes} B still outweighs the measurement grid {grid_bytes} B: {fine}"
    );
    // De-welding costs the percentiles (T-434): unknown on the wire, never approximated.
    assert!(
        fine["grid"]["percentiles"]
            .as_str()
            .is_some_and(|s| s.starts_with("unknown")),
        "{fine}"
    );
    assert!(fine["grid"]["p_low_db"].is_null(), "{fine}");
    let observed_fine = fine["grid"]["observed_cells"].as_u64().unwrap();
    // The observed tile is the other half of T-461's fail-closed rule: its plane is observed, so
    // the full read runs and the per-cell arrays are there.
    assert_eq!(
        fine["resolution"]["short_circuit"]["applied"],
        json!(false),
        "{fine}"
    );
    assert!(fine["grid"]["max_db"].is_array(), "{fine}");
    assert!(fine["grid"].get("uniform").is_none(), "{fine}");

    // ---- T-461: the short-circuit, and the two ways it fails closed ----
    //
    // 2 000 tiles up the frequency axis: a band this server has never tuned to, on either axis of
    // evidence. What decides the two cases below is *time*.
    let far = f_index + 2_000;

    // **The tile this server started in (T-507, and T-509's flake).** Before T-507 this tile was
    // asserted MIXED — `"unknown"` before the server's first sample, `"unobserved"` after — which
    // was the purple wall, and which also depended on the wall-clock 32 s block still containing
    // the server's start when the request landed: under load the block boundary could pass first,
    // and the tile came back uniformly `"unobserved"` in 0.167 s (T-509). Now a young server has
    // forgotten nothing, so the current tile over a band never tuned is `"unobserved"` whenever the
    // request lands. The mixed plane is built deterministically, from the route's own
    // `recording_began_s`, in `unknown_is_only_what_a_server_recorded_and_lost_and_the_plane_says_so`.
    let (st, now_tile) = get(addr, &tile(0, 0, far, t_index_of(0)));
    assert_eq!(st, 200, "{now_tile}");
    assert_eq!(
        now_tile["resolution"]["short_circuit"]["selected_plane_uniform"],
        json!("unobserved"),
        "a server that has lost nothing has no unknown rows: {}",
        now_tile["coverage"]["horizon"]
    );
    assert_eq!(
        now_tile["coverage"]["horizon"]["unknown_rows"],
        json!(0),
        "{}",
        now_tile["coverage"]["horizon"]
    );

    // **Fires on a uniformly unobserved plane.** One time tile on: wholly after the record
    // horizon, and still a band nothing ever tuned to. Every cell `"unobserved"`, so the coverage
    // map answers the tile on its own.
    let (st, empty) = get(addr, &tile(0, 0, far, t_index_of(0) + 1));
    assert_eq!(st, 200, "{empty}");
    assert_eq!(
        empty["resolution"]["short_circuit"]["selected_plane_uniform"],
        json!("unobserved"),
        "{empty}"
    );
    assert_eq!(
        empty["resolution"]["short_circuit"]["applied"],
        json!(true),
        "{empty}"
    );
    // Nothing was read, and the answer says so rather than naming a level that never ran.
    assert_eq!(empty["cost"]["source_cells"], json!(0), "{empty}");
    assert_eq!(empty["cost"]["chunks"], json!(0), "{empty}");
    assert_eq!(empty["resolution"]["answered"], Value::Null, "{empty}");
    assert_eq!(empty["resolution"]["tried"], json!([]), "{empty}");
    // **The answer is UNOBSERVED, not quiet and not zero.** One stated cell with no level, the
    // per-cell arrays absent rather than empty or zeroed, and the grid still the tile's own.
    assert_eq!(empty["grid"]["uniform"]["max_db"], Value::Null, "{empty}");
    assert_eq!(empty["grid"]["uniform"]["frames"], json!(0), "{empty}");
    assert_eq!(
        empty["grid"]["uniform"]["observed"],
        json!(false),
        "{empty}"
    );
    assert_eq!(empty["grid"]["observed_cells"], json!(0), "{empty}");
    assert_eq!(empty["grid"]["range_db"], Value::Null, "{empty}");
    assert_eq!(empty["grid"]["cells"], json!(N * N), "{empty}");
    for k in ["max_db", "occupancy_max", "coverage", "frames"] {
        assert!(
            empty["grid"].get(k).is_none(),
            "grid enumerates {k}: {empty}"
        );
    }
    // …and the plane the client greys from agrees, in its own vocabulary.
    let sel = empty["coverage"]["selected"]["plane"].as_u64().unwrap() as usize;
    assert_eq!(
        empty["coverage"]["planes"][sel]["uniform"],
        json!("unobserved"),
        "{empty}"
    );
    // The body is a CONSTANT, not a function of the tile's cell count: sixty-four times the cells
    // is the same answer, to within the axis numbers.
    let small = serde_json::to_string(&empty).unwrap().len();
    let (_, empty_big) = get(
        addr,
        &format!(
            "/api/tiles?level_f=0&level_t=0&f_index={}&t_index={}&cells=256",
            far * N / 256,
            t_index_of(0) / 8 + 1
        ),
    );
    assert_eq!(
        empty_big["resolution"]["short_circuit"]["applied"],
        json!(true),
        "{empty_big}"
    );
    assert_eq!(empty_big["grid"]["cells"], json!(256 * 256), "{empty_big}");
    let big = serde_json::to_string(&empty_big).unwrap().len();
    assert!(
        big < small * 2,
        "an unobserved tile's body must not scale with cells: {small} B at {N}x{N}, {big} B at 256x256"
    );
    // Against the full read beside it, the saving is the whole ticket — measured on the `grid`
    // member, which is the part that scales with cells (the rest of a tile answer is fixed prose
    // and axis numbers, and at 32 x 32 that prose is most of both bodies).
    let empty_grid = serde_json::to_string(&empty_big["grid"]).unwrap().len();
    let observed_grid = serde_json::to_string(&fine["grid"]).unwrap().len();
    assert!(
        empty_grid < observed_grid,
        "a 256 x 256 unobserved grid ({empty_grid} B) must be smaller than a {N} x {N} OBSERVED \
         one ({observed_grid} B) — sixty-four times the cells, stated once"
    );

    // ---- the de-welding, on the wire: one axis's level moves only its own axis's cell ----
    let (st, coarse_f) = get(addr, &tile(2, 0, f_index / 4, t_pin));
    assert_eq!(st, 200, "{coarse_f}");
    assert_eq!(
        coarse_f["extent"]["f_cell_hz"],
        json!(f_cell * 4.0),
        "{coarse_f}"
    );
    // F2's exact failure mode: the TIME cell moved when only the FREQUENCY address was coarsened.
    assert_eq!(coarse_f["extent"]["t_cell_s"], json!(t_cell), "{coarse_f}");
    assert_eq!(coarse_f["grid"]["t_cell_s"], json!(t_cell), "{coarse_f}");
    // F2's other half: a third of the window went grey. The coarser address covers the finer one's
    // whole band, so it cannot observe less.
    let observed_coarse = coarse_f["grid"]["observed_cells"].as_u64().unwrap();
    assert!(
        observed_coarse > 0,
        "the coarser frequency address greyed a band the finer one holds: {coarse_f}"
    );

    let (st, coarse_t) = get(addr, &tile(0, 2, f_index, t_pin / 4));
    assert_eq!(st, 200, "{coarse_t}");
    assert_eq!(
        coarse_t["extent"]["t_cell_s"],
        json!(t_cell * 4.0),
        "{coarse_t}"
    );
    assert_eq!(coarse_t["extent"]["f_cell_hz"], json!(f_cell), "{coarse_t}");
    // **T-439: fine frequency + coarse time is a REAL NODE now.** This is the address scheme 1
    // cannot express at all — a welded ladder is the diagonal of its own lattice — and before the
    // pipeline opened a view-scheme pyramid it had no store behind it, so it was folded out of
    // scheme 1's ladder and `store_node` was null. It is the load-bearing gap T-438 named, and it
    // is closed by the LIVE CHAIN writing this lattice's finest node: the same frames, one write,
    // no live-versus-history path (docs/16 §8.1).
    assert_eq!(
        coarse_t["resolution"]["answered"]["store"],
        json!("view-lattice"),
        "the view lattice must be the store that answers a view address: {coarse_t}"
    );
    assert_eq!(
        coarse_t["axes"]["store_node"],
        json!(2),
        "(level_f 0, level_t 2) is node 0*8+2 of the 8x8 view lattice: {coarse_t}"
    );
    // **T-1018: the node answers once it holds every row level 0 does.** The view lattice's
    // coarse nodes are maintained live, so node (0, 2) is read (`exact_node: true`, cells² source
    // cells) for any tile whose rows have all folded up. But a closed level-0 row folds up only
    // `seal_lag` after the clock leaves it, and this tile was pinned at the DATA EDGE of a server
    // that is still capturing — so whether it still reaches into those held-back rows depends on
    // how far capture has moved on since, and while it does the read keeps finest-first (node
    // (0, 0), `exact_node: false`) rather than drop the newest rows. Either is right; nothing else
    // is, and neither replicates.
    let answered = coarse_t["resolution"]["answered"]["level"].as_u64();
    let exact = coarse_t["resolution"]["answered"]["exact_node"].as_bool();
    assert!(
        matches!(
            (answered, exact),
            (Some(2), Some(true)) | (Some(0), Some(false))
        ),
        "node (0, 2) once folded, else finest-first at the live edge: {coarse_t}"
    );
    assert_ne!(
        coarse_t["resolution"]["fold"]["time"]["direction"],
        json!("replicated"),
        "{coarse_t}"
    );
    assert!(
        coarse_t["grid"]["observed_cells"].as_u64().unwrap() > 0 && observed_fine > 0,
        "{coarse_t}"
    );
    // The same node addressed through scheme 1 is a 404 that says why — the two schemes answer the
    // same question differently and neither snaps.
    let (st, v) = get(
        addr,
        &format!("/api/tiles?scheme=1&level_f=0&level_t=2&f_index=0&t_index=0&cells={N}"),
    );
    assert_eq!(st, 404, "{v}");

    // ---- "no such node": a welded ladder is the DIAGONAL of its own lattice (T-434) ----
    let (st, v) = get(
        addr,
        &format!("/api/tiles?scheme=1&level_f=0&level_t=3&f_index=0&t_index=0&cells={N}"),
    );
    assert_eq!(st, 404, "{v}");
    assert!(
        v["error"].as_str().is_some_and(|s| s.contains("diagonal")),
        "the refusal must say WHY: {v}"
    );
    // On the diagonal the same scheme answers — **addressed in SCHEME 1's own cells**, which since
    // T-484 are no longer the view lattice's. Scheme 1's floor is still its fixed 6.25 kHz x 1 s;
    // the view lattice's is now the display plan's own bin and row, so an index computed from one
    // lands somewhere else in the other, and for time it lands outside the addressable range
    // entirely. `scheme` names one store, and so does an index expressed in its cells.
    let s1_f = 6250.0;
    let s1_t = 1.0;
    let (st, v) = get(
        addr,
        &format!(
            "/api/tiles?scheme=1&level_f=0&level_t=0&f_index={}&t_index={}&cells={N}",
            (STATION_HZ / (s1_f * N as f64)).floor() as u64,
            (unix_now() / (s1_t * N as f64)) as u64,
        ),
    );
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["key"]["scheme"], json!("1"), "{v}");
    assert_eq!(v["axes"]["store_node"], json!(0), "{v}");
    // …and it reads the spectrum-history pyramid, not the view lattice. `scheme` names ONE store
    // (T-439): a numeric scheme is never answered out of another scheme's tiles.
    assert_eq!(
        v["resolution"]["answered"]["store"],
        json!("spectrum-history"),
        "{v}"
    );
    assert_eq!(v["resolution"]["answered"]["levels"], json!(5), "{v}");
    // ---- T-505: the OVERVIEW tier is its own lattice, answered by the spectrum-history store ----
    //
    // One lattice cannot be both the display stream's own bin at its floor and device-wide over the
    // record horizon at its ceiling: `max_level` bounds level INDICES, so a finer floor shrinks the
    // coarsest ADDRESSABLE tile by the same factor. The client draws wide-and-long viewports from
    // this tier instead, and `docs/api.md` states its ceiling — so the contract is that the route
    // answers the address, names the scheme back, and reads the store whose cells do not move when
    // the view pyramid's floor does.
    //
    // **Addressed in the OVERVIEW lattice's own cells (T-501).** It is anchored on scheme 1, whose
    // floor is the fixed 6.25 kHz x 1 s — that is the whole point of the tier — while the view
    // lattice's floor is the display plan's own bin and row and moves with it. An index computed in
    // one lattice's cells names a different tile in the other, and on the time axis it lands
    // outside the addressable range entirely, exactly as the scheme-1 address above.
    let (st, v) = get(
        addr,
        &format!(
            "/api/tiles?scheme=overview&level_f=0&level_t=0&f_index={}&t_index={}&cells={N}",
            (STATION_HZ / (s1_f * N as f64)).floor() as u64,
            (unix_now() / (s1_t * N as f64)) as u64,
        ),
    );
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["key"]["scheme"], json!("overview"), "{v}");
    assert_eq!(
        v["resolution"]["answered"]["store"],
        json!("spectrum-history"),
        "the overview tier must never be answered by the view pyramid: {v}"
    );
    // Its axes are the de-welded ladder, NOT scheme 1's welded diagonal — so an off-diagonal node
    // that scheme 1 refuses above is a real address here.
    let (st, off) = get(
        addr,
        &format!("/api/tiles?scheme=overview&level_f=0&level_t=3&f_index=0&t_index=0&cells={N}"),
    );
    assert_eq!(
        st, 200,
        "a de-welded lattice has no diagonal to fall off: {off}"
    );
    assert_eq!(off["key"]["scheme"], json!("overview"), "{off}");
    // And its ceiling reaches past the whole surface, which is the property the tier exists for:
    // the view lattice's own is (9, 1) at the shipped floor and shrinks with any finer one.
    let max_f = v["axes"]["frequency"]["max_level"].as_u64().unwrap();
    let max_t = v["axes"]["time"]["max_level"].as_u64().unwrap();
    assert!(
        max_f >= 11 && max_t >= 14,
        "overview ceiling ({max_f}, {max_t}): {v}"
    );

    // Past the end of an axis is the other "no such node", and names both extents.
    let (st, v) = get(addr, &tile(99, 0, 0, 0));
    assert_eq!(st, 404, "{v}");

    // ---- refusals, never clamps ----
    for bad in [
        "/api/tiles",
        "/api/tiles?level_f=0&level_t=0&f_index=0",
        "/api/tiles?level_f=-1&level_t=0&f_index=0&t_index=0",
        "/api/tiles?level_f=0&level_t=0&f_index=-1&t_index=0",
        "/api/tiles?level_f=0&level_t=0&f_index=0&t_index=0&cells=999",
        "/api/tiles?level_f=0&level_t=0&f_index=0&t_index=0&cells=2",
        "/api/tiles?level_f=0&level_t=0&f_index=0&t_index=0&scheme=nope",
        "/api/tiles?level_f=0&level_t=0&f_index=0&t_index=0&zoom=3",
    ] {
        let (st, v) = get(addr, bad);
        assert!(
            (400..500).contains(&st),
            "expected a refusal for {bad}: {v}"
        );
    }
    let (st, v) = post(addr, "/api/tiles", "{}");
    assert_eq!(st, 405, "{v}");

    // ---- the coarse-zoom aggregate: counts, never emitters (docs/16 §5.3) ----
    let (st, ev) = get(
        addr,
        &format!(
            "/api/tiles/events?level_f=0&level_t=0&f_index={f_index}&t_index={t_pin}&cells={N}"
        ),
    );
    assert_eq!(st, 200, "{ev}");
    assert_eq!(
        ev["counts"].as_array().unwrap().len(),
        (N * N) as usize,
        "{ev}"
    );
    assert_eq!(ev["key"]["cells"], json!(N), "{ev}");
    assert_eq!(ev["extent"]["t_cell_s"], json!(t_cell), "{ev}");
    assert!(ev["total"].is_u64() && ev["placed"].is_u64(), "{ev}");
    assert!(
        ev["placed"].as_u64().unwrap() <= ev["total"].as_u64().unwrap(),
        "an event off the tile is counted, not clamped into it: {ev}"
    );
    // A tile carries no emitters, and neither does the aggregate: counts only.
    assert!(ev["emitters"].is_null() && ev["events"].is_null(), "{ev}");
    assert!(
        ev["rule"].as_str().is_some_and(|s| s.contains("START")),
        "{ev}"
    );

    stop_server(serving);
}

/// T-519: the `shadow` block on `GET /api/tiles` — the **last-known / stale** tier (fog-of-war,
/// `docs/adr/0020`), asserted on values (T-315) against a real server whose radio is retuned away
/// from the station through the real control route.
///
/// - **Swept then departed = shadow.** After the retune, the station's tile carries runs over the
///   station's columns in the rows the grid no longer measures, and each run's value is a value the
///   pyramid measured there — for a value this tile holds, exactly the grid's own value in the row
///   above the run.
/// - **Observed now = its own value.** No run ever covers a cell the grid measured.
/// - **Never observed = nothing.** A tile over spectrum the radio never tuned carries no run and the
///   search reports no column found — grey's meaning is unchanged.
/// - **Every gap in an observed column is filled, and the wire says which way it was read** (T-527).
///   A column the grid measures anywhere has *every* other row of it covered: below its last sample
///   by a `forward` run (the nearest past sample), above its first by a `forward` run from an older
///   value or — where there is none — by the `backward` fill, which is the column's first-ever
///   sample and the only value here read backward in time.
#[test]
fn tile_shadow_carries_a_departed_band_and_nothing_where_never_observed() {
    let (_dir_guard, serving, addr) = start_server();
    const N: u64 = 32;
    let n = N as usize;
    let tile = |fi: u64, ti: u64| {
        format!("/api/tiles?level_f=0&level_t=0&f_index={fi}&t_index={ti}&cells={N}")
    };
    // **Node (0, 0) is read off the route, never written down here.** The view lattice is anchored
    // on the open pyramid's own level-0 cell, so the floor is the server's to state and a literal
    // here would be a second copy of `hk_pipeline::history::view_geometry` — and a stale one would
    // address a tile the front end never tuned, which is correctly unobserved and would make every
    // assertion below vacuous rather than red.
    let (st, probe) = get(addr, &tile(0, 0));
    assert_eq!(st, 200, "{probe}");
    let f_cell = probe["axes"]["frequency"]["cell_hz"].as_f64().unwrap();
    let t_cell = probe["axes"]["time"]["cell_s"].as_f64().unwrap();
    let f_index = (STATION_HZ / (f_cell * N as f64)).floor() as u64;
    // A tile spans `N` rows of `t_cell`, and `t_index` names which of those blocks an instant is in.
    let tile_s = t_cell * N as f64;
    let t_index = |t: f64| (t / tile_s) as u64;
    let t_now = || t_index(unix_now());
    // Readiness, found from the DATA EDGE (the store's newest frame, which every tile answer
    // carries), never from the wall clock: under load capture trails the clock, and a wait on the
    // wall-clock tile then waits for data that is always one tile in the future (deflake-0922: it
    // timed out that way once in ten under a concurrent hk-store suite).
    wait_for(
        "the station's tile at the data edge to be observed",
        Duration::from_secs(60),
        || {
            let Some(edge) = get(addr, &tile(f_index, t_now())).1["shadow"]["edge_s"].as_f64()
            else {
                return false;
            };
            get(addr, &tile(f_index, t_index(edge - 0.5 * t_cell))).1["grid"]["observed_cells"]
                .as_u64()
                .is_some_and(|c| c > 0)
        },
    );

    // The station's own column within its tile: the one that carries the carrier.
    let station_col = ((STATION_HZ - f_index as f64 * f_cell * N as f64) / f_cell) as usize;

    // Never observed: 3 GHz, which this 2.4 MHz front end has never been tuned near.
    let (st, never) = get(addr, &tile((3.0e9 / (f_cell * N as f64)) as u64, t_now()));
    assert_eq!(st, 200, "{never}");
    assert_eq!(never["shadow"]["encoding"], json!("column-runs"), "{never}");
    assert_eq!(never["shadow"]["runs"], json!(0), "{}", never["shadow"]);
    assert_eq!(never["shadow"]["search"]["columns_found"], json!(0));
    assert_eq!(never["shadow"]["f"], json!([]), "{}", never["shadow"]);

    // **Depart from the station, and PIN the tile the departure happened in — found from the
    // DATA, never from a clock.**
    //
    // The subject of this test is one tile that carries both halves of the story: rows the grid
    // measured while the station was swept, and rows it does not measure after the radio left.
    // A tile that *begins* after the departure has neither half — it is honestly unobserved end to
    // end, so the coverage map short-circuits it and its grid carries `uniform` in place of the
    // per-cell arrays (`tiles::unobserved_grid_json`) while still carrying a shadow. That answer is
    // right; asking for it here is not.
    //
    // Which tile the departure landed in used to be computed from `unix_now()` taken after the
    // retune POST returned (T-505 and T-519's rewrite of this block): the wall clock, three times
    // over — the phase in the tile to retune at, the instant of the retune, and the tile to pin.
    // But rows are stamped with CAPTURE time, and under a gate's load capture's data edge trails
    // the wall clock by as much as the POST takes. Measured, task-t800's gate: the pinned tile
    // spanned ..11.35–..12.63 s and the store's newest frame was at ..11.70 s, inside it, yet the
    // tile held not one station row — the departure had reached the data before the tile began.
    // The wall clock named the tile after the departure, and the test then read a tile that could
    // never hold the half it asserts on.
    //
    // So every instant here is read off the wire, in capture time:
    //
    //  - `armed` — the newest measured row of the station's column, seen at the data edge BEFORE
    //    the retune is sent. The departure is after it.
    //  - `arrived` — the first measured row of the band the radio moved TO, at or after `armed`.
    //    The departure is before it, and it is measured data in the same store as the grid, so it
    //    is there only once every older row is — the station rows before it are final.
    //  - the last station row before `arrived`: the tile holding it is the pinned tile, and the
    //    row itself is `last_row`. Every row after it in that tile is after the radio left.
    //
    // The one case with no such tile is a departure exactly at a tile boundary (the last station
    // row is the tile's last row). That is a measured fact, not a timeout, and the test answers it
    // by going back to the station and departing again — a bounded number of times, each named in
    // the panic if they run out.
    let step = hk_core::source::HACKRF_ONE_TUNING_STEP_HZ;
    let moved = step * (((FIXTURE_CENTER_HZ + 3.0e6) / step).round());
    // A column of the band moved to, half a MHz off its centre so the DC notch is not the probe.
    let arrived_f_index = ((moved + 0.5e6) / (f_cell * N as f64)).floor() as u64;
    let edge_now = || {
        get(addr, &tile(f_index, t_now())).1["shadow"]["edge_s"]
            .as_f64()
            .expect("a server with frames has an edge")
    };
    // `(t0_s, max_db)` of a tile, or `None` for a tile short-circuited from the coverage map.
    let grid_of = |ti: u64, fi: u64| {
        let v = get(addr, &tile(fi, ti)).1;
        let t0 = v["extent"]["t0_s"].as_f64().unwrap();
        v["grid"]["max_db"].as_array().cloned().map(|g| (t0, g))
    };
    const ATTEMPTS: usize = 4;
    let mut not_before = 0.0f64;
    let mut boundary_departures = Vec::new();
    let (t_pinned, last_row) = 'depart: {
        for _attempt in 0..ATTEMPTS {
            // Armed: the station's column is being measured at the data edge, after `not_before`
            // (after a previous attempt's departure, that means the station is back).
            let mut armed = 0.0f64;
            wait_for(
                "the station's column to be measured at the data edge",
                Duration::from_secs(60),
                || {
                    let e = edge_now();
                    let Some((t0, g)) = grid_of(t_index(e), f_index) else {
                        return false;
                    };
                    let last = (0..n).rev().find(|&r| !g[r * n + station_col].is_null());
                    match last.map(|r| t0 + r as f64 * t_cell) {
                        Some(at) if at > not_before => {
                            armed = at;
                            true
                        }
                        _ => false,
                    }
                },
            );
            let (st, r) = post(
                addr,
                "/api/control/center",
                &format!("{{\"center_hz\":{moved:?}}}"),
            );
            assert_eq!(st, 200, "{r}");
            // Arrived: the first measured row of the new band at or after `armed`.
            let mut arrived = 0.0f64;
            wait_for(
                "the band the radio moved to, to reach the grid",
                Duration::from_secs(60),
                || {
                    let e = edge_now();
                    (t_index(armed)..=t_index(e)).any(|ti| {
                        let Some((t0, g)) = grid_of(ti, arrived_f_index) else {
                            return false;
                        };
                        // Strictly after the armed row: a band measured before it is a previous
                        // attempt's, not this departure's.
                        let first = (0..n).find(|&r| {
                            t0 + r as f64 * t_cell > armed + 0.5 * t_cell
                                && (0..n).any(|c| !g[r * n + c].is_null())
                        });
                        first.is_some_and(|r| {
                            arrived = t0 + r as f64 * t_cell;
                            true
                        })
                    })
                },
            );
            // The last station row that begins before `arrived`, searched back from its tile.
            let mut found = None;
            for ti in (t_index(armed)..=t_index(arrived)).rev() {
                // A tile with no station row at all (the departure preceded it) is short-circuited
                // from the coverage map; the row sought is in an older one.
                let Some((t0, g)) = grid_of(ti, f_index) else {
                    continue;
                };
                if let Some(r) = (0..n).rev().find(|&r| {
                    t0 + r as f64 * t_cell <= arrived && !g[r * n + station_col].is_null()
                }) {
                    found = Some((ti, r, t0));
                    break;
                }
            }
            let (ti, row, t0) = found.unwrap_or_else(|| {
                panic!("no station row between armed {armed} and arrived {arrived}")
            });
            if row + 1 < n {
                break 'depart (ti, row as i64);
            }
            // The departure fell on the tile's own last row: no tile holds both halves. Go back.
            boundary_departures.push(t0 + row as f64 * t_cell);
            not_before = arrived;
            let (st, r) = post(
                addr,
                "/api/control/center",
                &format!("{{\"center_hz\":{FIXTURE_CENTER_HZ:?}}}"),
            );
            assert_eq!(st, 200, "{r}");
        }
        panic!(
            "{ATTEMPTS} departures in a row landed on a tile's last row ({boundary_departures:?}), \
             so no tile held both halves of the story"
        );
    };

    eprintln!(
        "tile shadow: departed on attempt {} (earlier ones on a tile's last row: {boundary_departures:?}); \
         pinned t_index {t_pinned}, last station row {last_row} of {n}",
        boundary_departures.len() + 1
    );

    // Wait for a station-tile row after the departure that the grid does not measure and a shadow
    // covers — in the PINNED tile. Rows after `last_row` are after the radio left by construction,
    // so this asks only that the shadow has reached one, never how long anything took.
    let mut v = Value::Null;
    wait_for(
        "the departed station to carry a shadow",
        Duration::from_secs(60),
        || {
            v = get(addr, &tile(f_index, t_pinned)).1;
            let sh = &v["shadow"];
            let (f, row, rows) = (&sh["f"], &sh["row"], &sh["rows"]);
            f.as_array().is_some_and(|fs| {
                fs.iter().enumerate().any(|(i, c)| {
                    let (r0, k) = (row[i].as_i64().unwrap_or(0), rows[i].as_i64().unwrap_or(0));
                    c.as_u64() == Some(station_col as u64) && r0 + k > last_row + 1
                })
            })
        },
    );
    let grid = v["grid"]["max_db"].as_array().unwrap_or_else(|| {
        panic!(
            "the pinned tile must hold the rows the grid measured BEFORE the retune — a tile with \
             none is short-circuited from the coverage map and serves `grid.uniform` instead of \
             the per-cell arrays, and there is then nothing for a shadow to have carried: {v}"
        )
    });
    let sh = &v["shadow"];
    let arr = |k: &str| sh[k].as_array().unwrap().clone();
    let (f, row, rows, db, t, src, fill) = (
        arr("f"),
        arr("row"),
        arr("rows"),
        arr("last_db"),
        arr("last_t_s"),
        arr("src"),
        arr("fill"),
    );
    // T-527: the direction alphabet is on the wire, and `fill` indexes it — never a bare bool, and
    // never inferred from the source table.
    assert_eq!(
        sh["fills"],
        json!(["forward", "backward"]),
        "the fill alphabet: {sh}"
    );
    assert_eq!(sh["runs"].as_u64().unwrap() as usize, f.len(), "{sh}");
    for a in [&row, &rows, &db, &t, &src, &fill] {
        assert_eq!(a.len(), f.len(), "parallel arrays: {sh}");
    }
    let (t0, t_cell) = (
        v["extent"]["t0_s"].as_f64().unwrap(),
        v["extent"]["t_cell_s"].as_f64().unwrap(),
    );
    let edge = sh["edge_s"]
        .as_f64()
        .expect("a server with frames has an edge");
    let mut covered = vec![false; n * n];
    let mut backward = 0usize;
    for i in 0..f.len() {
        let (c, r0, k) = (
            f[i].as_u64().unwrap() as usize,
            row[i].as_u64().unwrap() as usize,
            rows[i].as_u64().unwrap() as usize,
        );
        let last_db = db[i].as_f64().expect("a run always carries a value");
        let last_t = t[i].as_f64().unwrap();
        let is_back = fill[i].as_u64().expect("a fill code") == 1;
        // Never past the data edge.
        assert!(
            t0 + r0 as f64 * t_cell < edge,
            "run {i} starts at or past the edge {edge}"
        );
        let source = &sh["sources"][src[i].as_u64().unwrap() as usize];
        if is_back {
            // **The one backward read** (T-527): the column's FIRST-EVER sample, carried up into
            // the stretch before it. So it starts at row 0, stops exactly where that sample begins,
            // carries that sample's value, and its instant lies AFTER its own rows — the mirror of
            // every other run here, which is precisely why it is labelled.
            backward += 1;
            assert_eq!(source["from"], json!("this-tile"), "run {i}: {sh}");
            assert_eq!(r0, 0, "a backward run below the head: {sh}");
            assert_eq!(
                grid[k * n + c].as_f64(),
                Some(last_db),
                "run {i} carries the first sample at ({k}, {c})"
            );
            assert!(
                (0..k).all(|r| grid[r * n + c].is_null()),
                "run {i} covers a measurement in column {c}: {sh}"
            );
            assert!(
                last_t >= t0 + k as f64 * t_cell - 1e-6,
                "run {i}: first seen {last_t} is not after its rows (row {k} starts at {})",
                t0 + k as f64 * t_cell
            );
        } else if source["from"] == "this-tile" {
            // Forward: last seen at or before the run's first row, never a value from its future.
            assert!(
                last_t <= t0 + r0 as f64 * t_cell + 1e-6,
                "run {i}: {last_t} after row {r0}"
            );
            // T-315: the value IS the grid's own, in the last measured row above the run.
            assert!(r0 > 0, "a this-tile run below row 0: {sh}");
            assert_eq!(
                grid[(r0 - 1) * n + c].as_f64(),
                Some(last_db),
                "run {i} at ({r0}, {c})"
            );
        } else {
            assert!(
                last_t <= t0 + r0 as f64 * t_cell + 1e-6,
                "run {i}: {last_t} after row {r0}"
            );
            assert_eq!(source["from"], json!("before-tile"), "{source}");
            assert!(
                source["f_cell_hz"].as_f64().is_some_and(|x| x > 0.0),
                "{source}"
            );
        }
        for r in r0..r0 + k {
            assert!(!covered[r * n + c], "runs overlap at ({r}, {c})");
            covered[r * n + c] = true;
            // A shadow never replaces a measurement.
            assert!(
                grid[r * n + c].is_null(),
                "a shadow over the measured cell ({r}, {c}): {}",
                grid[r * n + c]
            );
        }
    }
    assert!(
        (0..n).any(|r| covered[r * n + station_col]),
        "the departed station carries a shadow: {sh}"
    );
    assert_eq!(
        sh["backward_runs"].as_u64().unwrap() as usize,
        backward,
        "the count on the wire must be the runs on the wire: {sh}"
    );
    // **T-527's invariant, over every column this tile measures at all**: no gap is left grey. A
    // column with any sample has every other row of it covered exactly once — above its first
    // sample and below its last — because a column that was observed was observed, and grey is
    // reserved for the columns that were not.
    let mut filled_heads = 0;
    for c in 0..n {
        let first = (0..n).find(|&r| !grid[r * n + c].is_null());
        let Some(first) = first else { continue };
        for r in 0..n {
            // Rows at or past the data edge are the future and carry nothing, by the same rule
            // that stops a forward carry there. Everything before it is this invariant's.
            if t0 + r as f64 * t_cell >= edge - 1e-6 {
                continue;
            }
            let measured = !grid[r * n + c].is_null();
            assert_eq!(
                !measured,
                covered[r * n + c],
                "column {c} row {r}: measured {measured}, shadowed {} — an observed column may \
                 have no grey gap (T-527) and no shadow over a measurement: {sh}",
                covered[r * n + c]
            );
        }
        if first > 0 {
            filled_heads += 1;
        }
    }
    eprintln!(
        "T-519/T-527 contract: {} runs ({backward} backward), {filled_heads} columns whose first \
         sample is below row 0, sources {}, station rows shadowed {}, search {}",
        f.len(),
        sh["sources"],
        (0..n).filter(|&r| covered[r * n + station_col]).count(),
        sh["search"]
    );
    // T-911: which search answered each before-tile value, and what the own-level one read.
    for s in sh["sources"].as_array().unwrap() {
        if s["from"] == json!("before-tile") {
            assert!(
                s["search"] == json!("own-level") || s["search"] == json!("ladder"),
                "a before-tile source names no search: {s}"
            );
            assert!(s["store"].is_string(), "{s}");
        }
    }
    let own = &sh["search"]["own_level"];
    assert!(
        own.is_null() || own.is_object(),
        "search.own_level is an object, or null when no level is affordable: {own}"
    );
    if own.is_object() {
        assert!(own["store"].is_string(), "{own}");
        assert!(own["level"].is_u64(), "{own}");
        assert!(own["stages"].is_array(), "{own}");
        // T-1034: the tune record's bound on the search, or null when the record made no claim.
        assert!(
            own["record_bound_s"].is_null() || own["record_bound_s"].is_f64(),
            "{own}"
        );
        let (found, used) = (
            own["columns_found"].as_u64().unwrap(),
            own["columns_used"].as_u64().unwrap(),
        );
        assert!(used <= found, "{own}");
        assert!(
            own["source_cells"].as_u64().unwrap() <= sh["search"]["source_cells"].as_u64().unwrap(),
            "{}",
            sh["search"]
        );
    }
    // T-523/T-911: the budget binds what was READ, over both searches.
    assert!(
        sh["search"]["source_cells"].as_u64().unwrap()
            <= sh["search"]["max_source_cells"].as_u64().unwrap(),
        "{}",
        sh["search"]
    );
    assert!(
        sh["rule"]
            .as_str()
            .is_some_and(|s| s.contains("GREY IS UNCHANGED"))
    );
    stop_server(serving);
}

/// T-1034, **the user's bug, through the mock SDR: after a nudge of half a span, the departed
/// band's newest half is fog — never blank — at the finest level, and at every level above it.**
///
/// The user: "If I have the waterfall going for a while, then I nudge it +1/2 to the right, the
/// fog-of-war applies correctly for the 99.5 to 100.2 tiles, but the 100.2 to 100.8 tiles do not
/// load. [...] The issue goes away if I zoom out." Staging answered those finest tiles
/// `shadow.runs = 0` with one store time block `unsearched`: the tile's own-level search skips a
/// time block that holds no tile over its frequency, but a store block spans 600 kHz, the new
/// window's edge bin lands in the departed half's block in every block after the retune, and each
/// was read in full until the budget ran out ~20 s short of the band's last live row.
///
/// Driven through the generic device route (`POST /api/control/center`), with every instant read
/// off the wire in capture time: the band is measured at the data edge (`armed`), the radio moves
/// half a span up, and the data edge is let run more than ten store blocks past it. Then every
/// live-edge tile over the departed band, at (0, 0), (1, 0) and (2, 1): no window unsearched,
/// every column of every row before the data edge either measured or carried by a shadow run, and
/// every carried value last seen at the band's last live row — at or after `armed`, and no later
/// than one row past the new band's first measured row.
#[test]
fn a_nudged_away_band_is_fog_at_the_finest_level_up_to_its_last_live_row() {
    // `hk serve`'s default display FFT, as staging ran: 586 Hz cells, so one store block spans
    // 600 kHz and the new window's edge bin lands in the departed half's block in every time
    // block after the nudge. At this file's usual 1024 a block spans the whole 2.4 MHz window and
    // the edge bin lands in the NEXT block — the case never arises there.
    let (_dir_guard, serving, addr) = start_server_fft(temp_data_dir(), None, 4096);
    const N: u64 = 32;
    let n = N as usize;
    let tile = |lf: u32, lt: u32, fi: u64, ti: u64| {
        format!("/api/tiles?level_f={lf}&level_t={lt}&f_index={fi}&t_index={ti}&cells={N}")
    };
    let (st, probe) = get(addr, &tile(0, 0, 0, 0));
    assert_eq!(st, 200, "{probe}");
    let f_cell = probe["axes"]["frequency"]["cell_hz"].as_f64().unwrap();
    let t_cell = probe["axes"]["time"]["cell_s"].as_f64().unwrap();
    let geom = |lf: u32, lt: u32| {
        (
            f_cell * f64::from(1u32 << lf) * N as f64,
            t_cell * f64::from(1u32 << lt) * N as f64,
        )
    };
    let (w0, h0) = geom(0, 0);
    // The departed band: the fixture window's lower half, which the nudge leaves. The half nearest
    // the new window is where staging went blank.
    let (band_lo, band_hi) = (FIXTURE_CENTER_HZ - FIXTURE_RATE_HZ / 2.0, FIXTURE_CENTER_HZ);
    let probe_fi = ((band_lo + band_hi) / 2.0 / w0) as u64;
    // The store's newest frame, or `None` before the first one is folded.
    let edge_now = || {
        get(addr, &tile(0, 0, probe_fi, (unix_now() / h0) as u64)).1["shadow"]["edge_s"].as_f64()
    };
    let grid_of = |fi: u64, ti: u64| {
        let v = get(addr, &tile(0, 0, fi, ti)).1;
        let t0 = v["extent"]["t0_s"].as_f64().unwrap();
        v["grid"]["max_db"].as_array().cloned().map(|g| (t0, g))
    };
    // Armed: the departed band measured at the data edge, read off the wire.
    let mut armed = 0.0f64;
    wait_for(
        "the band to be measured at the data edge",
        Duration::from_secs(60),
        || {
            let Some(e) = edge_now() else {
                return false;
            };
            let Some((t0, g)) = grid_of(probe_fi, (e / h0) as u64) else {
                return false;
            };
            match (0..n).rev().find(|&r| !g[r * n + n / 2].is_null()) {
                Some(r) => {
                    armed = t0 + r as f64 * t_cell;
                    true
                }
                None => false,
            }
        },
    );
    // The nudge: half a span up, through the device route.
    let step = hk_core::source::HACKRF_ONE_TUNING_STEP_HZ;
    let moved = step * ((FIXTURE_CENTER_HZ + FIXTURE_RATE_HZ / 2.0) / step).round();
    let (st, r) = post(
        addr,
        "/api/control/center",
        &format!("{{\"center_hz\":{moved:?}}}"),
    );
    assert_eq!(st, 200, "{r}");
    // Let capture run well past ten blocks of the view lattice's finest node (64 rows each), so
    // the live edge sits past what the budget could read block by block. Waited on the DATA edge,
    // never the wall clock.
    let blocks_s = 10.5 * 64.0 * t_cell;
    wait_for(
        "the data edge to run ten store blocks past the nudge",
        Duration::from_secs(180),
        || edge_now().is_some_and(|e| e > armed + blocks_s),
    );
    // The new band's first measured row after `armed`: the departure is before it.
    let new_fi = ((moved + FIXTURE_RATE_HZ / 4.0) / w0) as u64;
    let edge = edge_now().expect("the edge ran past the nudge");
    let arrived = ((armed / h0) as u64..=(edge / h0) as u64)
        .find_map(|ti| {
            let (t0, g) = grid_of(new_fi, ti)?;
            (0..n)
                .map(|r| (r, t0 + r as f64 * t_cell))
                .find(|&(r, t)| t > armed && (0..n).any(|c| !g[r * n + c].is_null()))
                .map(|(_, t)| t)
        })
        .expect("the band moved to is measured after the nudge");

    let new_lo = moved - FIXTURE_RATE_HZ / 2.0;
    let mut checked = 0usize;
    for (lf, lt) in [(0u32, 0u32), (1, 0), (2, 1)] {
        let (w, h) = geom(lf, lt);
        let dt = h / N as f64;
        // The live-edge tile, whole tiles over the departed band only.
        let ti = ((edge - dt) / h) as u64;
        let fis = (band_lo / w).ceil() as u64..(band_hi / w).floor() as u64;
        assert!(!fis.is_empty(), "({lf}, {lt}): {band_lo}..{band_hi} / {w}");
        for fi in fis {
            let (st, v) = get(addr, &tile(lf, lt, fi, ti));
            assert_eq!(st, 200, "{v}");
            let sh = &v["shadow"];
            let at = format!(
                "({lf}, {lt}) tile {fi} ({:.3}-{:.3} MHz)",
                fi as f64 * w / 1e6,
                (fi + 1) as f64 * w / 1e6
            );
            assert_eq!(
                sh["search"]["unsearched"],
                json!([]),
                "{at}: the departed band's past is searched, never left: {}",
                sh["search"]
            );
            let t0 = v["extent"]["t0_s"].as_f64().unwrap();
            let tile_edge = sh["edge_s"].as_f64().unwrap();
            let reach = sh["reach_s"].as_f64().unwrap_or(tile_edge).max(tile_edge);
            let grid = v["grid"]["max_db"].as_array();
            let mut covered = vec![false; n * n];
            let arr = |k: &str| sh[k].as_array().unwrap().clone();
            let (f, row, rows, t) = (arr("f"), arr("row"), arr("rows"), arr("last_t_s"));
            for i in 0..f.len() {
                let c = f[i].as_u64().unwrap() as usize;
                let r0 = row[i].as_u64().unwrap() as usize;
                let last_t = t[i].as_f64().unwrap();
                let measured_now = grid.is_some_and(|g| (0..n).any(|r| g[r * n + c].is_number()));
                // The column right below the new window's tuned edge takes its edge bin, which
                // reaches half a bin past the edge: live data from the new window, not the
                // departed band's (staging's tile 671 showed the same one column).
                let cell = w / N as f64;
                let edge_bin = fi as f64 * w + (c + 1) as f64 * cell > new_lo - cell / 2.0;
                if !measured_now && !edge_bin {
                    // A column the new window never reaches: its value is the band's last live
                    // row, which is after `armed` and before the new band's first row.
                    assert!(
                        last_t >= armed - dt && last_t <= arrived + dt,
                        "{at} column {c}: last seen {last_t}, not the band's last live row \
                         (armed {armed}, new band from {arrived}): {sh}"
                    );
                }
                for r in r0..r0 + rows[i].as_u64().unwrap() as usize {
                    covered[r * n + c] = true;
                }
            }
            // What a client following docs/api.md draws as THE grey: a row the answer's record
            // reaches (`coverage.horizon.as_of_s`), a cell the selected plane calls `unobserved`,
            // and no shadow run over it. Over a departed band there must be none: the radio
            // looked here, then left, and that is fog. (A cell the plane calls `observed` past the
            // fold edge is the pending mark, never grey.)
            let cov = &v["coverage"];
            let states: Vec<&str> = cov["states"]
                .as_array()
                .unwrap()
                .iter()
                .map(|s| s.as_str().unwrap())
                .collect();
            let sel = cov["selected"]["plane"].as_u64().unwrap() as usize;
            let plane: Vec<&str> = cov["planes"][sel]["runs"]
                .as_array()
                .unwrap()
                .chunks(2)
                .flat_map(|p| {
                    std::iter::repeat_n(
                        states[p[0].as_u64().unwrap() as usize],
                        p[1].as_u64().unwrap() as usize,
                    )
                })
                .collect();
            assert_eq!(plane.len(), n * n, "{cov}");
            let as_of = cov["horizon"]["as_of_s"].as_f64();
            for c in 0..n {
                for r in 0..n {
                    let start = t0 + r as f64 * dt;
                    if as_of.is_some_and(|a| start >= a) || start >= reach {
                        continue;
                    }
                    let measured = grid.is_some_and(|g| g[r * n + c].is_number());
                    assert!(
                        measured || covered[r * n + c] || plane[r * n + c] != "unobserved",
                        "{at} column {c} row {r} is BLANK: unobserved, not measured, no fog \
                         (armed {armed}, edge {tile_edge}): {}",
                        sh["search"]
                    );
                    // Before the fold edge nothing is pending: every departed cell is fog.
                    if start < tile_edge {
                        assert!(
                            measured || covered[r * n + c],
                            "{at} column {c} row {r}, before the fold edge {tile_edge}, carries \
                             no fog: {}",
                            sh["search"]
                        );
                    }
                }
            }
            checked += 1;
        }
    }
    eprintln!(
        "T-1034: {checked} departed tiles fogged edge to edge; armed {armed:.2}, new band from \
         {arrived:.2}, edge {edge:.2}"
    );
    stop_server(serving);
}

/// deflake-0922: **`horizon.recording_began_s` is when THIS server began sampling, and a retune
/// does not move it into the past.**
///
/// A retune seals the dwell in flight into the observation log, and the log files a record under
/// the hour it falls in. Until this test, `/api/coverage` took the log's reach for
/// `recording_began` from that HOUR (`hours()[0] * HOUR_NS`) — so the first retune on any server
/// moved its stated start back to the top of the hour: measured, 923 s before a 15 s old server
/// existed. Cell states survived it (time before the server is unobserved either way), but every
/// reader that waits on the horizon — the browser tier's `waitForRecordToCover` among them — was
/// told the record reached back a quarter of an hour further than it did, and returned at once.
#[test]
fn a_retune_does_not_move_recording_began_back_to_the_hour_the_log_files_it_under() {
    let started = unix_now();
    let (_dir_guard, serving, addr) = start_server();
    // Both windows: the one the mock opens on and the one it is retuned to. An explicit window
    // from a minute before the server to now, so the answer does not wait on a capture window.
    let horizon = || {
        let (st, v) = get(
            addr,
            &format!(
                "/api/coverage?f_lo=99000000&f_hi=105200000&cells=4&t0={}&t1={}",
                (started - 60.0).floor(),
                (unix_now() + 1.0).ceil()
            ),
        );
        assert_eq!(st, 200, "{v}");
        v
    };
    wait_for("the server to record", Duration::from_secs(60), || {
        horizon()["horizon"]["recording_began_s"].is_f64()
    });
    let before = horizon()["horizon"]["recording_began_s"].as_f64().unwrap();
    assert!(
        before >= started - 1.0,
        "a server cannot have begun sampling {:.1} s before the test started it",
        started - before
    );

    let step = hk_core::source::HACKRF_ONE_TUNING_STEP_HZ;
    let moved = step * (((FIXTURE_CENTER_HZ + 3.0e6) / step).round());
    let (st, r) = post(
        addr,
        "/api/control/center",
        &format!("{{\"center_hz\":{moved:?}}}"),
    );
    assert_eq!(st, 200, "{r}");
    // Non-vacuity: the subject is a log that HOLDS the sealed dwell. Before it does, the log has no
    // hour to misreport and this test would pass about nothing.
    let mut v = Value::Null;
    wait_for(
        "the observation log to hold the sealed dwell",
        Duration::from_secs(60),
        || {
            v = horizon();
            v["sources"].as_array().is_some_and(|ss| {
                ss.iter().any(|s| {
                    s["kind"] == "observation-log" && s["spans"].as_u64().is_some_and(|n| n > 0)
                })
            })
        },
    );
    let after = v["horizon"]["recording_began_s"].as_f64().unwrap();
    eprintln!(
        "recording_began: {before:.3} before the retune, {after:.3} after it, test started at {started:.3}; \
         sources {}",
        v["sources"]
    );
    assert!(
        after >= started - 1.0 && after > before - 1.0,
        "the retune moved recording_began_s {:.1} s into the past ({before} -> {after}), {:.1} s \
         before this server was started — the observation log's filing hour, not a sample: {}",
        before - after,
        started - after,
        v["horizon"]
    );
    stop_server(serving);
}

/// T-482: **`axes.*.max_level` is a ceiling that is TRUE**, on the wire, against a real server.
///
/// # What this test is a property of
///
/// It is a property of the **served answers**, not of the computation behind them: it walks the box
/// the route declares, address by address, over HTTP, and asserts none of them is refused. That is
/// the distinction this ticket exists for — the route used to declare `levels` and then `400` a
/// large part of the grid it had declared, so a declaration checked only against itself is exactly
/// the evidence that was already there.
///
/// It also asserts what a per-axis pair **cannot** say. The binding bound is the tile's *area*, so
/// it is an anti-diagonal in `(level_f, level_t)`, and the wire shows it in the one way an area
/// constraint can be told from a per-axis one: the same address that is refused at `cells = 256` is
/// **served** at `cells = 32`. The declared pair itself does **not** move with `cells` — it is
/// stated for this route's own 256-cell unit, because a client bootstraps its lattice from a cheap
/// `cells = 8` probe and renders at 256, so a per-answer ceiling would be cached against tiles 32×
/// wider on each axis (measured: a probe read back `(11, 9)` and 69 addresses inside that box were
/// refused at 256).
#[test]
fn the_tile_routes_declared_readable_ceiling_is_true_and_is_stated_for_the_routes_tile_unit() {
    let (_dir_guard, serving, addr) = start_server();
    // **Where the walk reads (T-507).** Every address below is placed over the band the mock is
    // tuned to, at a moment it had captured, so its coverage plane holds an observed cell and the
    // FULL read runs — which is where the ceiling is enforced. Until T-507 the walk read 1970 at
    // index 0, which a young server wrongly called `"unknown"` and so also read in full; 1970 is
    // now honestly `unobserved`, and a uniformly unobserved tile is answered by the coverage map
    // (T-461) before any level is consulted, at any address.
    let mut edge = 0.0f64;
    wait_for(
        "the ring to buffer capture",
        Duration::from_secs(60),
        || {
            let (st, got) = get(addr, "/api/timeline");
            let b = &got["window"]["buffered"];
            match (st, b["t0_s"].as_f64(), b["t1_s"].as_f64()) {
                (200, Some(a), Some(z)) if z - a > 1.0 => {
                    edge = z - 0.5;
                    true
                }
                _ => false,
            }
        },
    );
    let probe = |cells: u64| {
        let (st, v) = get(
            addr,
            &format!("/api/tiles?level_f=0&level_t=0&f_index=0&t_index=0&cells={cells}"),
        );
        assert_eq!(st, 200, "{v}");
        v
    };

    let v = probe(256);
    // Scheme `view`, each level doubling its own axis's cell — **and node (0, 0) READ OFF THE
    // ROUTE, never written down here (T-501).** Since T-484 the floor is the display plan's own bin
    // and row (`fs / spectrum_fft_len` x the row period, `hk_pipeline::history::view_geometry`),
    // so it moves with the run's sample rate and display settings. The 6250 Hz x 1 s this closure
    // used to assume was T-439's fixed floor, and assuming it silently addressed a DIFFERENT tile
    // — one over a band the mock never tuned — where the coverage map short-circuits and the
    // ceiling is never exercised at all.
    let f0 = v["axes"]["frequency"]["cell_hz"].as_f64().unwrap();
    let t0 = v["axes"]["time"]["cell_s"].as_f64().unwrap();
    let at = |lf: u64, lt: u64, cells: u64| {
        let f_tile = f0 * (1u64 << lf) as f64 * cells as f64;
        let t_tile = t0 * (1u64 << lt) as f64 * cells as f64;
        format!(
            "/api/tiles?level_f={lf}&level_t={lt}&f_index={}&t_index={}&cells={cells}",
            (FIXTURE_CENTER_HZ / f_tile).floor() as u64,
            (edge / t_tile).floor() as u64
        )
    };
    let levels_f = v["axes"]["frequency"]["levels"].as_u64().unwrap();
    let levels_t = v["axes"]["time"]["levels"].as_u64().unwrap();
    let max_f = v["axes"]["frequency"]["max_level"]
        .as_u64()
        .unwrap_or_else(|| panic!("axes.frequency.max_level must be stated: {v}"));
    let max_t = v["axes"]["time"]["max_level"]
        .as_u64()
        .unwrap_or_else(|| panic!("axes.time.max_level must be stated: {v}"));
    // The two numbers answer different questions, and on the shipped geometry they DIFFER — a
    // 12 x 15 view lattice over a 4 x 4 store. If they were equal there would be nothing to state.
    assert!(max_f < levels_f && max_t < levels_t, "{v}");
    assert!(
        v["axes"]["readable"]
            .as_str()
            .is_some_and(|s| s.contains("BOX")),
        "the wire must say what kind of bound this is: {v}"
    );

    // **The walk.** Every address inside the declared box, at the tile size the box was declared
    // for. A ceiling that still refuses is the same defect one notch down.
    let mut refused = Vec::new();
    for lf in 0..=max_f {
        for lt in 0..=max_t {
            let (st, body) = get(addr, &at(lf, lt, 256));
            assert_ne!(
                body["resolution"]["short_circuit"]["applied"],
                json!(true),
                "({lf},{lt}) must take the full read: {body}"
            );
            if st != 200 {
                refused.push(format!("({lf},{lt}) -> {st} {}", body["error"]));
            }
        }
    }
    assert!(
        refused.is_empty(),
        "the declared ceiling ({max_f}, {max_t}) is a LIE: {refused:?}"
    );

    // And it is not conservative for the sake of it: one level past it, on either axis, refuses.
    for (lf, lt) in [(max_f + 1, max_t), (max_f, max_t + 1)] {
        let (st, body) = get(addr, &at(lf, lt, 256));
        assert_eq!(
            st, 400,
            "({lf},{lt}) is servable, so the ceiling is leaving reach unused: {body}"
        );
    }
    // **T-515 (folded into T-507): the same over a band nothing ever sampled.** 1970 at index 0
    // is uniformly `"unobserved"`, which the coverage map can answer without reading (T-461) — and
    // that shortcut must not answer where the read would refuse, or servability would depend on
    // what the radio sampled rather than on the geometry the ceiling declares. Inside the box it
    // is served by the shortcut; one past it, refused, exactly as over the tuned band.
    let origin = |lf: u64, lt: u64| {
        format!("/api/tiles?level_f={lf}&level_t={lt}&f_index=0&t_index=0&cells=256")
    };
    let (st, inside) = get(addr, &origin(max_f, max_t));
    assert_eq!(st, 200, "{inside}");
    assert_eq!(
        inside["resolution"]["short_circuit"]["applied"],
        json!(true),
        "{inside}"
    );
    for (lf, lt) in [(max_f + 1, max_t), (max_f, max_t + 1)] {
        let (st, body) = get(addr, &origin(lf, lt));
        assert_eq!(
            st, 400,
            "({lf},{lt}) over an unobserved band answered past the declared ceiling: {body}"
        );
    }

    // **One pair, whatever the probe cost.** A client fetches its lattice with a deliberately cheap
    // `cells = 8` tile and renders at 256, so the ceiling it caches must not depend on the size of
    // the probe that fetched it.
    for cells in [8u64, 32, 256] {
        let v = probe(cells);
        assert_eq!(
            (
                v["axes"]["frequency"]["max_level"].as_u64().unwrap(),
                v["axes"]["time"]["max_level"].as_u64().unwrap()
            ),
            (max_f, max_t),
            "the ceiling is a property of the SURFACE, not of this answer's cells={cells}: {v}"
        );
    }

    // **The area constraint, on the wire.** The bound itself does move with tile size, even though
    // the declared pair does not: the same address refused at 256 is served at 32. That is the
    // reach a cacheable pair gives up, and it is what a per-axis bound could never express.
    let (st_big, big) = get(addr, &at(max_f + 1, max_t, 256));
    let (st_small, small) = get(addr, &at(max_f + 1, max_t, 32));
    assert_eq!(st_big, 400, "{big}");
    assert_eq!(
        st_small,
        200,
        "level_f {} must be servable at cells=32 — the bound is on AREA: {small}",
        max_f + 1
    );

    stop_server(serving);
}

/// T-468: `GET /ws/tiles/rows`, as `docs/api.md` documents it, on a real `hk serve`.
///
/// - **An address range, never "now"**: without `t_from` the upgrade completes with a `refused`
///   message and close code `4400`; there is no default anchor for a range to fall back on.
/// - **Rows pushed as recorded**: an open range starting behind the data edge delivers what exists,
///   then keeps delivering rows past the edge it started at — contiguous, each at its own address.
/// - **Sealed history as readily as the growing edge**: a closed range behind the edge is served
///   exactly and ends, through the same route, and its rows are the ones the open range carried for
///   the same addresses wherever both copies say `final`.
#[test]
fn row_push_route_serves_an_address_range_growing_or_sealed() {
    let (_dir_guard, serving, addr) = start_server();
    const N: i64 = 32;
    let (st, probe) = get(
        addr,
        &format!("/api/tiles?level_f=0&level_t=0&f_index=0&t_index=0&cells={N}"),
    );
    assert_eq!(st, 200, "{probe}");
    let f_cell = probe["extent"]["f_cell_hz"].as_f64().unwrap();
    let t_cell = probe["extent"]["t_cell_s"].as_f64().unwrap();
    let f_index = (STATION_HZ / (f_cell * N as f64)).floor() as i64;
    let rows_path = |range: &str| {
        format!(
            "/ws/tiles/rows?token={TOKEN}&level_f=0&level_t=0&f_index={f_index}&cells={N}{range}"
        )
    };
    let text = |ws: &mut Ws| -> Option<Value> {
        loop {
            match ws.read() {
                Ok(Message::Text(t)) => return Some(serde_json::from_str(&t).unwrap()),
                Ok(Message::Close(_)) | Err(_) => return None,
                Ok(_) => {}
            }
        }
    };

    // ---- no range start, no subscription ----
    let mut ws = connect_ws(addr, &rows_path("")).expect("refusals still upgrade");
    let v = text(&mut ws).expect("refusal");
    assert_eq!(v["type"], "refused", "{v}");
    assert_eq!(v["status"], 400, "{v}");
    let mut code = None;
    while let Ok(m) = ws.read() {
        if let Message::Close(f) = m {
            code = f.map(|f| u16::from(f.code));
        }
    }
    assert_eq!(code, Some(4400));

    // ---- the data edge, from a one-row sealed range at the epoch (itself a range, not "now") ----
    let mut edge_s = 0.0;
    wait_for("the store to hold rows", Duration::from_secs(60), || {
        let mut ws = connect_ws(addr, &rows_path("&t_from=0&t_to=1")).unwrap();
        let s = text(&mut ws).expect("subscribed");
        assert_eq!(s["type"], "subscribed", "{s}");
        match s["data_edge_s"].as_f64() {
            Some(e) if e > 10.0 * t_cell * N as f64 => {
                edge_s = e;
                true
            }
            _ => false,
        }
    });
    let edge_row = (edge_s / t_cell).floor() as i64;
    let from = edge_row - 3 * N / 2;

    // ---- an open range from behind the edge: what exists, then what is recorded after ----
    let mut live = connect_ws(addr, &rows_path(&format!("&t_from={from}"))).unwrap();
    let s = text(&mut live).expect("subscribed");
    assert_eq!(s["type"], "subscribed", "{s}");
    assert_eq!(s["range"]["t_from"], json!(from), "{s}");
    assert_eq!(s["range"]["open"], json!(true), "{s}");
    assert_eq!(s["extent"]["t_cell_s"].as_f64(), Some(t_cell), "{s}");
    let mut next = from;
    let mut observed = 0u64;
    let mut kept: std::collections::BTreeMap<i64, (bool, Vec<Value>)> = Default::default();
    while next < edge_row + 10 {
        let v = text(&mut live).expect("rows keep arriving past the edge the range started at");
        assert_eq!(
            v["row0"],
            json!(next),
            "contiguous, never skipped or repeated: {v}"
        );
        let n = v["rows"].as_i64().unwrap();
        if v["type"] == "rows" {
            assert_eq!(v["nf"], json!(N), "{v}");
            let db = v["max_db"].as_array().unwrap();
            assert_eq!(db.len() as i64, n * N, "{v}");
            assert_eq!(v["tile"]["t_index"], json!(next.div_euclid(N)), "{v}");
            assert_eq!(v["coverage"]["plane"]["cells"], json!(n * N), "{v}");
            observed += v["observed_cells"].as_u64().unwrap();
            for r in 0..n {
                kept.insert(
                    next + r,
                    (
                        v["final"] == json!(true),
                        db[(r * N) as usize..((r + 1) * N) as usize].to_vec(),
                    ),
                );
            }
        } else {
            assert_eq!(v["type"], "unobserved", "{v}");
        }
        next += n;
    }
    assert!(
        observed > 0,
        "the tuned station's column holds measurements"
    );
    drop(live);

    // ---- the same addresses as a CLOSED range: served exactly, then `end` ----
    let (a, b) = (from, from + N);
    let mut past = connect_ws(addr, &rows_path(&format!("&t_from={a}&t_to={b}"))).unwrap();
    assert_eq!(
        text(&mut past).expect("subscribed")["range"]["open"],
        json!(false)
    );
    let mut row = a;
    loop {
        let v = text(&mut past).expect("a message");
        if v["type"] == "end" {
            assert_eq!(v["row"], json!(b), "{v}");
            break;
        }
        assert_eq!(v["row0"], json!(row), "{v}");
        let n = v["rows"].as_i64().unwrap();
        if v["type"] == "rows" {
            // T-902: every block states the honesty tier its rows were measured at, and it is the
            // tier `/api/tiles` states for the same address when the same level answered — one
            // rule, so a client never has to infer a pushed-row tile's tier.
            let res = &v["resolution"];
            let src = res["source"]
                .as_str()
                .unwrap_or_else(|| panic!("no tier: {v}"));
            assert!(
                ["live-iq", "spectrum-history", "survey-overview"].contains(&src),
                "{v}"
            );
            assert_eq!(res["live"], json!(src == "live-iq"), "{v}");
            assert_eq!(res["fold"]["time"]["served"], json!(n), "{v}");
            assert_eq!(res["fold"]["frequency"]["served"], json!(N), "{v}");
            let t_index = v["tile"]["t_index"].as_i64().unwrap();
            let (st, tile) = get(
                addr,
                &format!(
                    "/api/tiles?level_f=0&level_t=0&f_index={f_index}&t_index={t_index}&cells={N}"
                ),
            );
            assert_eq!(st, 200, "{tile}");
            if tile["resolution"]["answered"]["level"] == v["answered"]["level"] {
                assert_eq!(
                    tile["resolution"]["source"], res["source"],
                    "the row feed and the tile route disagree on a tier: {v} vs {tile}"
                );
            }
        }
        if v["type"] == "rows" && v["final"] == json!(true) {
            let db = v["max_db"].as_array().unwrap();
            for r in 0..n {
                if let Some((true, earlier)) = kept.get(&(row + r)) {
                    assert_eq!(
                        &db[(r * N) as usize..((r + 1) * N) as usize],
                        earlier.as_slice(),
                        "row {} read two ways",
                        row + r
                    );
                }
            }
        }
        row += n;
    }
    assert_eq!(row, b, "every row of the range, once");
    stop_server(serving);
}

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

/// T-800 (MAP-00, ADR-0023): the MMAP research routes are **reserved, not yet served**, and they
/// stay token-gated the day they are.
///
/// `docs/api.md` "Reserved: the map-UI research routes" fixes the four durable stores' shapes
/// (annotations, collections/markers, measurements, views) and the band-plan-priors query before
/// any of them has code, so MAP-12 and MAP-16..MAP-19 are five instances of one contract rather
/// than five designs. This test is the half of that pairing (T-079) which can be asserted today,
/// and it is written so it does **not** have to be deleted as the routes land:
///
/// - **Without a token, every reserved path answers `401`** — auth runs *before* dispatch
///   (`http.rs`), so this holds whether or not the route exists, and it is exactly the property a
///   reserved name must keep once it does exist. A store of a researcher's notes that answered an
///   unauthenticated caller would be a real defect, and this is what would catch it.
/// - **With a token, a reserved path either 404s (not landed yet) or answers its own contract.**
///   It may never answer `401` with a valid token, and it may never 404 *without* one, which is
///   what pins the ordering.
/// - **Every reserved path is documented**, so the table cannot quietly drift out of `docs/api.md`
///   while the tickets are open.
#[test]
fn mmap_research_routes_are_reserved_and_gated() {
    // (method, path) exactly as docs/api.md reserves them. Paths with an `{id}` are probed with a
    // concrete id: a served route answers 404 `not_found` for it, which is indistinguishable from
    // "not served" on purpose — the shape is the owning ticket's test to assert, not this one's.
    const RESERVED: &[(&str, &str)] = &[
        ("GET", "/api/annotations"),
        ("POST", "/api/annotations"),
        ("GET", "/api/collections"),
        ("POST", "/api/collections"),
        ("GET", "/api/markers"),
        ("GET", "/api/measurements"),
        ("POST", "/api/measurements"),
        ("GET", "/api/views"),
        ("POST", "/api/views"),
        ("GET", "/api/priors"),
    ];

    let doc_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/api.md");
    let doc = std::fs::read_to_string(&doc_path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", doc_path.display()));
    assert!(
        doc.contains("## Reserved: the map-UI research routes"),
        "docs/api.md must carry the reserved MMAP route table (T-800)"
    );
    for (_, path) in RESERVED {
        assert!(
            doc.lines().any(|l| l.contains(path)),
            "{path} is reserved in the contract test but not in docs/api.md"
        );
    }

    let (_dir_guard, serving, addr) = start_server();
    let bearer = format!("Bearer {TOKEN}");
    for (method, path) in RESERVED {
        let body = (*method == "POST").then_some("{}");

        // Unauthenticated: 401, before anything about whether the endpoint exists.
        let (st, v) = call(addr, method, path, None, body);
        assert_eq!(st, 401, "{method} {path} unauthenticated: {v}");
        let (st, v) = call(addr, method, path, Some("Bearer nope"), body);
        assert_eq!(st, 401, "{method} {path} with a wrong token: {v}");

        // Authenticated: not yet served (404 `no such endpoint`), or the route's own answer —
        // never a 401, which would mean the gate and the dispatch had swapped order.
        let (st, v) = call(addr, method, path, Some(&bearer), body);
        assert_ne!(st, 401, "{method} {path} refused a valid token: {v}");
        if st == 404 {
            assert_eq!(
                v.get("error").and_then(|e| e.as_str()),
                Some("no such endpoint"),
                "{method} {path} is not served yet, so it must 404 as an unknown endpoint: {v}"
            );
        }
    }
    stop_server(serving);
}

/// T-816 (MAP-16): `/api/annotations` as `docs/api.md` documents it — the `Annotation` shape with
/// its server-stamped provenance, the required window and paging fields, update/delete, and that
/// an authored note never becomes an inventory row (it is user metadata, never detection input).
#[test]
fn annotations_crud_and_paging_answer_as_documented() {
    let (_dir_guard, serving, addr) = start_server();
    let f = FIXTURE_CENTER_HZ;
    let body = json!({
        "kind": "box",
        "f_lo_hz": f - 1.0e5,
        "f_hi_hz": f + 1.0e5,
        "t0_s": 1000.0,
        "t1_s": 1002.0,
        "label": "t816-contract-note",
        "view": {"center_hz": f, "span_hz": 2.4e6, "t_capture": [990.0, 1010.0], "tier": "spectrum-history"},
    });
    let (st, a) = post(addr, "/api/annotations", &body.to_string());
    assert_eq!(st, 201, "{a}");
    for field in [
        "id",
        "collection_id",
        "kind",
        "f_lo_hz",
        "f_hi_hz",
        "t0_s",
        "t1_s",
        "label",
        "body",
        "author",
        "provenance",
        "created_s",
        "updated_s",
    ] {
        assert!(a.get(field).is_some(), "annotation missing {field}: {a}");
    }
    for field in [
        "device_id",
        "center_hz",
        "span_hz",
        "sample_rate_hz",
        "t_capture",
        "tier",
        "authored_s",
        "actor",
        "authored",
    ] {
        assert!(
            a["provenance"].get(field).is_some(),
            "provenance missing {field}: {a}"
        );
    }
    assert_eq!(a["provenance"]["authored"], true);
    assert!(
        a["author"].as_str().is_some_and(|s| s.starts_with("tok-")),
        "{a}"
    );
    assert!(
        !a.to_string().contains(TOKEN),
        "the token itself is never stored"
    );
    let id = a["id"].as_str().unwrap().to_owned();

    // Client-supplied provenance is refused.
    let mut forged = body.clone();
    forged["provenance"] = json!({"authored_s": 0});
    let (st, v) = post(addr, "/api/annotations", &forged.to_string());
    assert_eq!((st, v["code"].as_str()), (400, Some("invalid")), "{v}");

    let window = format!(
        "/api/annotations?f_lo={}&f_hi={}&t0=0&t1=5000",
        f - 1e6,
        f + 1e6
    );
    let (st, list) = get(addr, &window);
    assert_eq!(st, 200, "{list}");
    for field in [
        "window",
        "annotations",
        "count",
        "matched",
        "limit",
        "next_cursor",
    ] {
        assert!(list.get(field).is_some(), "list missing {field}: {list}");
    }
    assert_eq!(
        (list["count"].as_u64(), list["matched"].as_u64()),
        (Some(1), Some(1))
    );
    assert_eq!(list["limit"], 200, "documented default page");
    assert!(list["next_cursor"].is_null());
    let (st, v) = get(addr, "/api/annotations");
    assert_eq!(
        (st, v["code"].as_str()),
        (400, Some("invalid")),
        "window required: {v}"
    );

    let path = format!("/api/annotations/{id}");
    let (st, got) = get(addr, &path);
    assert_eq!(
        (st, got["label"].as_str()),
        (200, Some("t816-contract-note"))
    );
    let (st, upd) = put(addr, &path, r#"{"body": "carrier edge"}"#);
    assert_eq!(
        (st, upd["body"].as_str()),
        (200, Some("carrier edge")),
        "{upd}"
    );

    // User metadata only: no inventory row names it.
    let (st, inv) = get(addr, "/api/inventory");
    assert_eq!(st, 200, "{inv}");
    assert!(
        !inv.to_string().contains("t816-contract-note"),
        "an annotation never reaches the inventory: {inv}"
    );

    let (st, del) = delete(addr, &path);
    assert_eq!(
        (st, del["deleted"]["id"].as_str()),
        (200, Some(id.as_str()))
    );
    let (st, _) = get(addr, &path);
    assert_eq!(st, 404);
    stop_server(serving);
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

/// T-168 (ADR-0013 §4.9 gap 11): `GET /api/pipelines`/`GET /api/pipelines/{id}` serve `follow_hops`
/// as `docs/api.md` documents it: `null` for a single-channel pipeline, and for a follow-hops
/// pipeline `{channels: [{index, center_hz, bandwidth_hz}], channel_source, channel_bandwidth_hz,
/// max_channels}`.
#[test]
fn pipeline_json_documents_the_follow_hops_field() {
    let (_dir_guard, serving, addr) = start_server();
    let bearer = format!("Bearer {TOKEN}");
    let target = json!({"band": {"f_lo": STATION_HZ - 20e3, "f_hi": STATION_HZ + 20e3}});

    // A single-channel pipeline: follow_hops is null, both freshly started and re-fetched.
    let single = json!({
        "schema": "hackriff.recipe", "schema_version": 2, "id": "t168-single", "version": 1,
        "name": "T-168 single-channel",
        "input": {"port": "iq", "sample_rate_hz": 48000.0, "bandwidth_hz": 40000.0},
        "nodes": [{"id": "fm", "block": "fm_demod", "params": {"deviation_hz": 5000}}],
        "outputs": [{"id": "audio", "kind": "stage", "from": "fm"}],
        "output_policy": {"content_class": "unrestricted"}
    });
    let (st, v) = post(
        addr,
        "/api/pipelines",
        &json!({"recipe": single, "target": target.clone()}).to_string(),
    );
    assert_eq!(st, 201, "{v}");
    assert_eq!(v["follow_hops"], Value::Null, "{v}");
    let pid = v["id"].as_str().unwrap().to_owned();
    let (_, v) = get(addr, &format!("/api/pipelines/{pid}"));
    assert_eq!(v["follow_hops"], Value::Null, "{v}");
    let (st, v) = call(
        addr,
        "DELETE",
        &format!("/api/pipelines/{pid}"),
        Some(&bearer),
        None,
    );
    assert_eq!(st, 200, "{v}");

    // A follow-hops pipeline: the documented shape, resolved from the recipe's list_hz.
    let a = STATION_HZ - 200e3;
    let draft = json!({
        "schema": "hackriff.recipe", "schema_version": 2, "id": "t168-hops", "version": 1,
        "name": "T-168 follow-hops",
        "input": {"port": "iq", "sample_rate_hz": 24000.0, "bandwidth_hz": 16000.0,
                  "channels": {"mode": "follow-hops", "channel_bandwidth_hz": 12500.0,
                               "max_channels": 8, "list_hz": [a]}},
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
    let (st, v) = post(addr, "/api/pipelines", &start.to_string());
    assert_eq!(st, 201, "{v}");
    let pid = v["id"].as_str().unwrap().to_owned();
    let fh = &v["follow_hops"];
    assert_eq!(fh["channel_source"], json!("list"), "{v}");
    assert_eq!(fh["channel_bandwidth_hz"], json!(12500.0), "{v}");
    assert_eq!(fh["max_channels"], json!(8), "{v}");
    let channels = fh["channels"].as_array().unwrap();
    assert_eq!(channels.len(), 1, "{v}");
    for k in ["index", "center_hz", "bandwidth_hz"] {
        assert!(!channels[0][k].is_null(), "{k}: {v}");
    }
    assert_eq!(channels[0]["center_hz"], json!(a), "{v}");

    // GET /api/pipelines lists the same shape inline.
    let (_, v) = get(addr, "/api/pipelines");
    let listed = v["pipelines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == json!(pid))
        .unwrap();
    assert_eq!(
        listed["follow_hops"]["channel_source"],
        json!("list"),
        "{v}"
    );

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
    // T-355: the echoed box is `f_lo_hz`/`f_hi_hz`, matching every other envelope-level frequency
    // field on this API (inventory rows, tiles, `/api/analysis/strongest`, …) instead of the bare
    // `f_lo`/`f_hi` this route used to answer with alone. Asserted by VALUE, not shape, so the
    // field cannot be renamed or dropped again without this test failing (T-315's point applied
    // here too).
    assert_eq!(v["f_lo_hz"], 100_000_000.0, "{v}");
    assert_eq!(v["f_hi_hz"], 101_000_000.0, "{v}");
    assert!(v.get("f_lo").is_none() && v.get("f_hi").is_none(), "{v}");
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
    // T-355: same rename as `/api/observations` above, same route family.
    assert_eq!(v["f_lo_hz"], 100_000_000.0, "{v}");
    assert_eq!(v["f_hi_hz"], 101_000_000.0, "{v}");
    assert!(v.get("f_lo").is_none() && v.get("f_hi").is_none(), "{v}");
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
            // T-359: three-valued and omitted while unknown — never a null, never a bool.
            assert!(
                r["bias_tee"].is_null() || matches!(r["bias_tee"].as_str(), Some("off" | "on")),
                "{r}"
            );
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
    // T-333: every listed baseline discloses the bias-tee cohort its levels belong to, always as
    // one of the three states — never omitted, so "unknown" reads as a cohort, not as a gap.
    // T-315: and the receive-chain cohort (T-303/T-314) it belongs to, the same tagged `ChainKey`
    // shape `/api/baselines/slots` asserts below — never absent, never a bare id. Before this, the
    // key was documented (docs/api.md) but only the response SHAPE was contract-tested
    // (`is_array(v["baselines"])`), so the field could be renamed or dropped without a test
    // failing: documentation, not a contract. (This server is a fresh site with no baselines, so
    // this loop is vacuous, exactly as the slots one below; the non-vacuous assertion against a
    // real fold is `hk_pipeline::attention::tests::*` where `AttentionService::in_memory` folds
    // under `ChainKey::Unknown`.)
    for b in v["baselines"].as_array().into_iter().flatten() {
        assert!(
            matches!(b["bias_tee"].as_str(), Some("unknown" | "off" | "on")),
            "{b}"
        );
        assert!(
            matches!(b["chain"]["kind"].as_str(), Some("unknown" | "device")),
            "{b}"
        );
    }
    let (st, v) = get(
        addr,
        "/api/baselines/slots?f_lo=100000000&f_hi=101000000&resolution=all-hours",
    );
    assert_eq!(st, 200, "{v}");
    assert!(
        is_array(&v["subjects"]) && v["truncated"] == json!(false),
        "{v}"
    );
    // T-371: a slots row names the bias-tee cohort its numbers belong to, so two rows for one
    // subject and slot that differ only in the tee state are told apart on the wire. The served
    // value is always one of the three states: never omitted, never a null and never a bool, so
    // nothing on this route lets a reader coerce `unknown` — the commonest cohort, and the one
    // every pre-T-359 baseline carries — into `off`. (A fresh site has no baselines, so this
    // server serves no rows; the values are asserted against real folds in
    // `hk_pipeline::occupancy::tests::slots_rows_name_their_bias_tee_cohort_and_legacy_reads_unknown`.)
    for r in v["subjects"].as_array().into_iter().flatten() {
        assert!(
            matches!(r["bias_tee"].as_str(), Some("unknown" | "off" | "on")),
            "{r}"
        );
    }
    // T-381: a slots row also names the receive-chain cohort its numbers belong to, the same gap
    // T-371 closed for the bias tee. The served value is always the tagged `ChainKey` shape —
    // `{"kind": "unknown"}` or `{"kind": "device", "id": number}` — never absent and never a bare
    // id that could be misread as device 0. (This server is a fresh site with no baselines, so
    // this loop is vacuous, exactly as T-371's was; the non-vacuous assertions against real folds
    // are in
    // `hk_pipeline::occupancy::tests::slots_rows_name_their_receive_chain_cohort_and_legacy_reads_unknown`.)
    for r in v["subjects"].as_array().into_iter().flatten() {
        assert!(
            matches!(r["chain"]["kind"].as_str(), Some("unknown" | "device")),
            "{r}"
        );
    }
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
        // T-349: the unit is in the name. `nanosecond_and_second_fields_in_one_response_agree…`
        // asserts its value; this only pins the field's presence under its declared name.
        "generated_at_ns",
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

// ---------------------------------------------------------------------------
// T-273 trunking load index

/// T-273, AWARE-067: `/api/trunking/load` answers the documented shape on a fresh run (a trunking
/// store exists — the mock-device server wires it to the same run database as the inventory — but
/// no trunk system has been seen, so `systems` is empty rather than 503), refuses a bad window and
/// other methods, and **never carries audio or call content** — the route's actual boundary.
#[test]
fn trunking_load_index_answers_documented_shape_and_carries_no_content() {
    let (_dir_guard, serving, addr) = start_server();

    let (st, v) = get(addr, "/api/trunking/load?t0=0&t1=3600");
    assert_eq!(st, 200, "{v}");
    for field in ["window", "systems"] {
        assert!(v.get(field).is_some(), "missing {field}: {v}");
    }
    assert!(is_array(&v["systems"]), "{v}");
    assert_eq!(v["window"]["t0"], 0.0);
    assert_eq!(v["window"]["t1"], 3600.0);

    // A system nobody has ever seen answers a zero-grant row, not 404 or 503: the index is a
    // derived view over the stream, not a lookup that fails on an unknown key.
    let unknown = "01890000-0000-7000-8000-000000000000";
    let (st, v) = get(
        addr,
        &format!("/api/trunking/load?t0=0&t1=3600&system={unknown}"),
    );
    assert_eq!(st, 200, "{v}");
    let row = &v["systems"][0];
    assert_eq!(row["system"], unknown);
    assert_eq!(row["grants"], 0);
    assert_eq!(row["distinct_talkgroups"], 0);
    assert_eq!(row["grants_per_min"], 0.0);

    // The hard boundary (the ticket's actual constraint): metadata only, never audio or content,
    // whatever systems or grants exist.
    let body = v.to_string().to_lowercase();
    for banned in ["audio", "content", "vocoder", "payload", "pcm"] {
        assert!(!body.contains(banned), "must never carry {banned:?}: {v}");
    }

    for bad in [
        "/api/trunking/load",
        "/api/trunking/load?t0=10&t1=10",
        "/api/trunking/load?t0=abc&t1=10",
        "/api/trunking/load?t0=0&t1=10&system=not-a-uuid",
    ] {
        let (st, v) = get(addr, bad);
        assert_eq!(st, 400, "{bad}: {v}");
    }

    let auth = format!("Bearer {TOKEN}");
    let (st, _) = call(
        addr,
        "POST",
        "/api/trunking/load?t0=0&t1=3600",
        Some(&auth),
        Some("{}"),
    );
    assert_eq!(st, 405);

    stop_server(serving);
}

// ---------------------------------------------------------------------------
// T-977 control-channel candidates and their verdicts

/// T-977, SIGNAL-085: `/api/trunking/cc-candidates` answers the documented shape on a fresh run.
///
/// The interesting assertion is the **empty state**: a run whose hunt has not completed a pass
/// answers `pass: null`, not `{"channels": []}`. Un-looked-at and looked-at-and-chose-nothing are
/// different facts (ADR-0021 §7A.4), and this route exists precisely to keep them apart — the same
/// rule the canvas's grey obeys. A shape-only assertion would pass either way, so this one asserts
/// the value.
#[test]
fn cc_candidates_answers_pass_null_before_a_hunt_has_completed_one() {
    let (_dir_guard, serving, addr) = start_server();

    let (st, v) = get(addr, "/api/trunking/cc-candidates");
    assert_eq!(st, 200, "{v}");
    assert!(v.get("pass").is_some(), "missing pass: {v}");
    assert!(
        v["pass"].is_null() || v["pass"].is_object(),
        "pass is either null (no pass completed) or the pass object: {v}"
    );
    if let Some(pass) = v["pass"].as_object() {
        for field in [
            "pass",
            "t_start",
            "t_end",
            "device_id",
            "tune_center_hz",
            "raster_hz",
            "grid_offset_hz",
            "channels_swept",
            "channels",
        ] {
            assert!(pass.contains_key(field), "missing {field}: {v}");
        }
        assert!(is_array(&v["pass"]["channels"]), "{v}");
        for ch in v["pass"]["channels"].as_array().unwrap() {
            for field in ["k", "center_hz", "fco", "candidacy", "outcome", "reason"] {
                assert!(ch.get(field).is_some(), "channel missing {field}: {ch}");
            }
            let outcome = ch["outcome"].as_str().unwrap_or_default();
            assert!(
                [
                    "confirmed",
                    "sync-without-check",
                    "no-sync",
                    "not-demodulated",
                    "admission-refused",
                ]
                .contains(&outcome),
                "outcome outside the documented enum: {ch}"
            );
        }
    }

    // Metadata only, exactly like `/api/trunking/load`: a verdict about a channel is not its
    // traffic.
    let body = v.to_string().to_lowercase();
    for banned in ["audio", "vocoder", "pcm"] {
        assert!(!body.contains(banned), "must never carry {banned:?}: {v}");
    }

    let auth = format!("Bearer {TOKEN}");
    let (st, _) = call(
        addr,
        "POST",
        "/api/trunking/cc-candidates",
        Some(&auth),
        Some("{}"),
    );
    assert_eq!(st, 405);

    stop_server(serving);
}

// ---------------------------------------------------------------------------
// T-349: every absolute time on the wire declares its unit in its own name.
//
// `docs/api.md` "Conventions": times are Unix seconds by default, and a field that departs from
// that default says so with an `_ns` suffix. `hk_model::Timestamp` is `#[serde(transparent)]` over
// `i64` nanoseconds, so any hk-model struct serialized straight into a response used to ship raw
// nanoseconds under a name that looked exactly like the seconds beside it in the same body —
// values 10^9 apart, and nothing in the type system or the field name to tell them apart. These
// tests assert the VALUES, not the shape: a shape assertion passes either way, which is precisely
// why the defect survived this long.
// ---------------------------------------------------------------------------

/// Plausible absolute capture times as Unix **seconds**: 2001-09-09 … 2286-11-20.
const BAND_S: (f64, f64) = (1e9, 1e10);
/// The same instants as Unix **nanoseconds**.
const BAND_NS: (f64, f64) = (1e18, 1e19);

/// Field names whose epoch-magnitude number provably is not a time. Each one is here because it is
/// a count or an identifier that can reach 10^9, not because it is inconvenient.
const NOT_A_TIME: &[&str] = &[
    // Frequencies (a 1–10 GHz tune lands squarely in the seconds band).
    "f_lo",
    "f_hi",
    "center",
    // Byte budgets and counters.
    "min_free_bytes",
    "bytes",
    "capacity_bytes",
    // Opaque numeric identifiers.
    "geometry",
    "id",
    "sample_index",
    "seq",
    // **Elapsed-time counters: ns SINCE THE RUN STARTED, never an instant** (T-510, T-939).
    //
    // These are the same kind of entry as `bytes` above — a count that reaches 10^9 — and a
    // nanosecond duration reaches it after **one second** of the thing it measures. This sweep's
    // only evidence is magnitude, and magnitude cannot tell a duration from an instant: both are
    // ns under an `_ns` name, and the seconds band (10^9 … 10^10) is 1 s … 10 s of accumulated
    // time. `wait_ns` crosses it within a second of any run, and `cpu_ns` — here since T-510 —
    // would cross it in any run long enough for one reader to spend a CPU-second, which the
    // 5 s replay this sweep sets up happens not to reach. So the exemption is by name and is
    // exhaustive: these ten are the `ReaderCounters` / `SourceCounters` time accumulators
    // (`crates/hk-pipeline/src/stats.rs`), nothing serves an instant under any of them, and a new
    // *instant* must still not be given one of these names.
    "cpu_ns",
    "wait_ns",
    "clip_ns",
    "burst_ns",
    "stft_ns",
    "frame_ns",
    "floor_ns",
    "detector_ns",
    "track_ns",
];

fn looks_like_a_time(key: &str) -> bool {
    !(key.ends_with("_hz") || key.contains("hz") || NOT_A_TIME.contains(&key))
}

/// Collects every numeric leaf that looks like an absolute Unix time but whose name does not agree
/// with the magnitude it carries.
fn scan_times(v: &Value, path: &str, key: &str, out: &mut Vec<String>) {
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                scan_times(x, &format!("{path}.{k}"), k, out);
            }
        }
        Value::Array(a) => {
            // Three entries is enough to catch a systematic unit error, and keeps a 10 000-row
            // page from dominating the run.
            for (i, x) in a.iter().enumerate().take(3) {
                scan_times(x, &format!("{path}[{i}]"), key, out);
            }
        }
        Value::Number(n) => {
            let Some(x) = n.as_f64() else { return };
            if !looks_like_a_time(key) {
                return;
            }
            let (looks_s, looks_ns) = (
                (BAND_S.0..BAND_S.1).contains(&x),
                (BAND_NS.0..BAND_NS.1).contains(&x),
            );
            let named_ns = key.ends_with("_ns");
            match (looks_s, looks_ns, named_ns) {
                // Seconds under a bare or `_s` name, and nanoseconds under an `_ns` name: the law.
                (true, _, false) | (_, true, true) => {}
                (true, _, true) => out.push(format!(
                    "{path} = {x} is named `_ns` but carries Unix SECONDS (out by 10^9)"
                )),
                (_, true, false) => out.push(format!(
                    "{path} = {x} carries Unix NANOSECONDS under a name that reads as seconds \
                     (a client reading it as seconds is out by ~31 years); name it `…_ns`"
                )),
                _ => {}
            }
        }
        _ => {}
    }
}

/// The routes this sweep can reach on a fresh server, with parameters that actually return data.
///
/// T-370: `selection` extends the sweep to selection-scoped routes exactly as `emitter` already
/// does for the inventory-scoped ones — `GET /api/selections/{id}` and
/// `GET /api/selections/{id}/watch` are otherwise unreachable on a fresh server (no selection
/// exists to address), so a field only that route serves is not swept at all. This is the second
/// field the sweep missed for a reachability reason rather than a listing error, the same class
/// T-354 named for the stream-contract field.
fn time_law_routes(now: f64, emitter: Option<&str>, selection: Option<&str>) -> Vec<String> {
    let (t0, t1) = (now - 3600.0, now + 3600.0);
    // Wide box for the paged/listing routes; the fixture's own band for the ones with
    // band \u00d7 span budgets (occupancy `span`, coverage channel tiling).
    let bx = format!("f_lo=0&f_hi=6000000000&t0={t0}&t1={t1}");
    let narrow = format!(
        "f_lo={}&f_hi={}&t0={t0}&t1={t1}",
        FIXTURE_CENTER_HZ - 1.2e6,
        FIXTURE_CENTER_HZ + 1.2e6
    );
    let mut r: Vec<String> = [
        "/api/status",
        "/api/streams",
        "/api/inventory",
        "/api/selections",
        "/api/bookmarks",
        // T-817: marker collections (the reserved bookmarks collection exists on a fresh server)
        "/api/collections",
        "/api/markers",
        "/api/blocks",
        "/api/iqbuffer",
        "/api/clusters",
        "/api/candidates",
        "/api/sites",
        "/api/sites/current",
        "/api/scheduler/arms",
        "/api/scheduler/leases",
        "/api/datasets",
        // T-844: the C38 models, modes and the durable shadow log
        "/api/ml/models",
        "/api/ml/shadow",
        // T-469: the persisted IQ recordings that extend the audio horizon past the ring
        "/api/recordings",
        "/api/captures",
        "/api/recipes",
        "/api/pipelines",
        "/api/outputs",
        "/api/control/state",
        "/api/attention/weights",
        "/api/taxonomy",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();
    r.extend([
        format!("/api/observations?{bx}"),
        format!("/api/observations/coverage?{narrow}&channel_hz=250000&tau_s=0.01&min_gap_s=0.001"),
        format!("/api/occupancy?{narrow}&interval=span"),
        format!("/api/occupancy?{bx}"),
        format!("/api/report?{narrow}"),
        format!("/api/report?{bx}"),
        format!("/api/history?{bx}&nf=16&nt=8"),
        format!("/api/floor?{bx}"),
        format!("/api/events?{bx}"),
        format!("/api/anomalies?{bx}"),
        format!("/api/scheduler?{bx}"),
        format!("/api/analysis/strongest?f_lo=99e6&f_hi=102e6&window_s=60&now={now}"),
        "/api/channels?f_lo=0&f_hi=6000000000".to_string(),
    ]);
    if let Some(id) = emitter {
        r.extend([
            format!("/api/inventory/{id}"),
            format!("/api/inventory/{id}/classification"),
            format!("/api/inventory/{id}/presence"),
            format!("/api/signatures/match?emitter={id}"),
        ]);
    }
    if let Some(id) = selection {
        r.extend([
            format!("/api/selections/{id}"),
            format!("/api/selections/{id}/watch"),
        ]);
    }
    r
}

/// T-349: sweep every reachable route and hold its JSON to the units convention by value. This is
/// the net that catches the *next* field someone adds, not just the ones T-349 renamed.
#[test]
fn every_serialized_time_declares_its_unit() {
    let (_g, serving, addr) = start_server();
    // Let the run detect something, so the inventory/occupancy/report bodies are not all empty:
    // an empty array satisfies any assertion about its contents.
    let mut emitter: Option<String> = None;
    wait_for("an inventory row", Duration::from_secs(90), || {
        let (st, v) = get(addr, "/api/inventory");
        if st != 200 {
            return false;
        }
        emitter = v["entries"]
            .as_array()
            .and_then(|a| a.first())
            .and_then(|e| e["id"].as_str())
            .map(str::to_string);
        emitter.is_some()
    });
    // T-370: a selection also has to exist for `/api/selections/{id}[/watch]` to be reachable —
    // the same reachability gap `emitter` above closes for the inventory-scoped routes, and the
    // one the sweep missed the stream-contract field for (T-354).
    let (st, created) = post(
        addr,
        "/api/selections",
        &json!({
            "name": "time-law-sweep",
            "f_lo": FIXTURE_CENTER_HZ - 1.0e5,
            "f_hi": FIXTURE_CENTER_HZ + 1.0e5,
        })
        .to_string(),
    );
    assert_eq!(st, 201, "{created}");
    let selection = created["id"].as_str().map(str::to_string);
    assert!(selection.is_some(), "{created}");
    let mut bad: Vec<String> = Vec::new();
    let mut checked = 0usize;
    for r in time_law_routes(unix_now(), emitter.as_deref(), selection.as_deref()) {
        let (st, v) = get(addr, &r);
        assert_eq!(st, 200, "{r} answered {st}: {v}");
        checked += 1;
        scan_times(&v, &r, "", &mut bad);
    }
    stop_server(serving);
    assert!(checked >= 30, "swept only {checked} routes");
    assert!(
        bad.is_empty(),
        "{} field(s) carry an absolute time their name misdeclares:\n  {}",
        bad.len(),
        bad.join("\n  ")
    );
}

/// T-349: the routes the defect was found on, asserted by value against the run's own clock.
///
/// The naming sweep above proves each field's magnitude matches its suffix. This proves the
/// stronger thing: that the `_ns` field and the seconds field **in the same response** are the
/// same instant once the declared units are applied. A response where `span.start_ns` were
/// silently seconds would pass a shape check, pass an "is a number" check, and fail here.
///
/// Times here are on the **sample clock** the captured blocks carry (a replay reports the
/// recording's time), not the wall clock, so the window comes from the run's counters.
#[test]
fn nanosecond_and_second_fields_in_one_response_agree_once_their_units_are_applied() {
    use std::sync::atomic::Ordering::Relaxed;
    let (_g, serving, addr) = start_server();
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
    wait(&|| counters.stream_time_ns.load(Relaxed) >= first + 1_500_000_000);
    let center = f64::from_bits(counters.tune_center_bits.load(Relaxed));
    let (f_lo, f_hi) = (center - 1.2e6, center + 1.2e6);

    // ---- /api/report: `generated_at_ns` and `span` are nanoseconds; `observed_s` stays seconds.
    let (t0, t1) = (first as f64 * 1e-9 - 1.0, first as f64 * 1e-9 + 600.0);
    let bx = format!("f_lo={f_lo}&f_hi={f_hi}&t0={t0}&t1={t1}");
    let (st, v) = get(addr, &format!("/api/report?{bx}"));
    assert_eq!(st, 200, "{v}");
    assert!(
        v.get("generated_at").is_none(),
        "report still ships unitless `generated_at`: {v}"
    );
    let gen_ns = v["generated_at_ns"].as_i64().expect("generated_at_ns");
    assert!(
        (BAND_NS.0..BAND_NS.1).contains(&(gen_ns as f64)),
        "generated_at_ns = {gen_ns} is not a nanosecond-magnitude Unix time"
    );
    // The same instant the run's own counter reports, read as nanoseconds. Read as seconds it
    // would be some 56 billion years out, and every shape assertion would still pass.
    assert!(
        (gen_ns - first).abs() < 60_000_000_000,
        "generated_at_ns = {gen_ns} is not the stream clock ({first}) in nanoseconds"
    );
    let sp = &v["span"];
    assert!(sp.get("start").is_none(), "report span unitless: {sp}");
    assert_eq!(
        (sp["start_ns"].as_i64(), sp["end_ns"].as_i64()),
        (Some((t0 * 1e9) as i64), Some((t1 * 1e9) as i64)),
        "report span must echo the requested window in nanoseconds: {sp}"
    );
    let observed_s = v["coverage"]["observed_s"].as_f64().unwrap_or(-1.0);
    assert!(
        (0.0..1e6).contains(&observed_s),
        "coverage.observed_s must stay a duration in seconds beside the ns span: {v}"
    );
    for row in v["occupancy"]["bands"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .chain(
            v["occupancy"]["channels"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter(),
        )
        .take(4)
    {
        let Some(iv) = row.get("interval").filter(|x| !x.is_null()) else {
            continue;
        };
        assert!(
            iv.get("start").is_none(),
            "occupancy interval unitless: {iv}"
        );
        let a = iv["start_ns"].as_i64().unwrap_or_else(|| panic!("{row}"));
        assert!(
            (BAND_NS.0..BAND_NS.1).contains(&(a as f64)),
            "interval.start_ns = {a} is not nanoseconds: {row}"
        );
        assert!(
            row["observed_s"].as_f64().unwrap_or(0.0) < 1e6,
            "observed_s must be a duration in seconds, not an epoch: {row}"
        );
    }

    // ---- /api/signatures/match: `t_ns` when the catalogue has anything to say (empty on a fresh
    // server is the normal case, so this is conditional by design, not by convenience).
    let (st, inv) = get(addr, "/api/inventory");
    assert_eq!(st, 200, "{inv}");
    if let Some(id) = inv["entries"]
        .as_array()
        .and_then(|a| a.first())
        .and_then(|e| e["id"].as_str())
    {
        let (st, v) = get(addr, &format!("/api/signatures/match?emitter={id}"));
        assert_eq!(st, 200, "{v}");
        for m in std::iter::once(&v["match"]).chain(v["history"].as_array().into_iter().flatten()) {
            if m.is_null() {
                continue;
            }
            assert!(m.get("t").is_none(), "SignatureMatch still ships `t`: {m}");
            let t = m["t_ns"].as_i64().expect("t_ns");
            assert!(
                (BAND_NS.0..BAND_NS.1).contains(&(t as f64)),
                "SignatureMatch.t_ns = {t} is not nanoseconds"
            );
        }
    }

    // ---- The observation log. Its open interactive record closes when the run ends; the API
    // keeps serving the log afterwards (the same sequence as
    // `observation_coverage_reports_interactive_tuning_without_a_scheduler`).
    let Serving { server, handle, .. } = serving;
    handle.stop();
    let (tx, rx) = std::sync::mpsc::channel();
    let waiter = std::thread::spawn(move || {
        let _ = tx.send(handle.wait());
    });
    rx.recv_timeout(Duration::from_secs(30))
        .expect("the run stopped")
        .expect("the run finished cleanly");
    let _ = waiter.join();
    let end = counters.stream_time_ns.load(Relaxed);
    let (t0, t1) = (first as f64 * 1e-9 - 1.0, end as f64 * 1e-9 + 1.0);
    let q = format!(
        "f_lo={}&f_hi={}&t0={t0}&t1={t1}",
        center + 100e3,
        center + 112.5e3
    );

    // The envelope echoes the window in seconds; the records inside carry `TimeRange`s in
    // nanoseconds. Both describe the same window, and the response now says which is which.
    let (st, v) = get(addr, &format!("/api/observations?{q}&tier=interactive"));
    assert_eq!(st, 200, "{v}");
    assert!(
        (v["t0"].as_f64().unwrap() - t0).abs() < 1.0
            && (v["t1"].as_f64().unwrap() - t1).abs() < 1.0,
        "envelope must echo the requested seconds window: {v}"
    );
    let recs = v["records"].as_array().cloned().unwrap_or_default();
    assert!(!recs.is_empty(), "no observation records to check: {v}");
    let mut checked = 0usize;
    for rec in recs.iter().take(5) {
        for field in ["span", "planned", "observed"] {
            let Some(tr) = rec.get(field).filter(|x| !x.is_null()) else {
                continue;
            };
            assert!(
                tr.get("start").is_none() && tr.get("end").is_none(),
                "{field} still serializes unitless `start`/`end`: {tr}"
            );
            let (a, b) = (
                tr["start_ns"].as_i64().unwrap_or_else(|| panic!("{tr}")),
                tr["end_ns"].as_i64().unwrap_or_else(|| panic!("{tr}")),
            );
            assert!(b >= a, "{field} ends before it starts: {tr}");
            // The load-bearing assertion: read as nanoseconds it lands inside the window this same
            // response reports in seconds. Read as seconds it would be ~31 years out per 10^9.
            let start_s = a as f64 / 1e9;
            assert!(
                (t0 - 120.0..t1 + 120.0).contains(&start_s),
                "{field}.start_ns = {a}; as seconds that is {start_s}, outside [{t0}, {t1}]: {rec}"
            );
            checked += 1;
        }
    }
    assert!(checked >= 2, "no record time ranges were checked");

    // `totals.span` (ns) against the same response's envelope (s).
    let (st, v) = get(
        addr,
        &format!("/api/observations/coverage?{q}&min_gap_s=0.001"),
    );
    assert_eq!(st, 200, "{v}");
    let span = &v["totals"]["span"];
    assert!(span.get("start").is_none(), "totals.span unitless: {span}");
    let (a, b) = (
        span["start_ns"].as_i64().unwrap(),
        span["end_ns"].as_i64().unwrap(),
    );
    assert!(
        ((a as f64 / 1e9) - t0).abs() < 1.0 && ((b as f64 / 1e9) - t1).abs() < 1.0,
        "totals.span_ns must be the window the envelope reports in seconds: {v}"
    );
    drop(server);
}

/// T-452: **the in-app survey sweep, and who wins when it and the user both want the radio.**
///
/// T-406 built the iterative scan as a dwell policy over the scheduler — and `hk serve`, the mode
/// the UI talks to, deliberately never drives the scheduler. This asserts the resolution: the
/// sweep is a driver over the *interactive* retune path, reachable as `/api/control/scan`, and its
/// arbitration with interactive tuning is one rule with an honest answer on the wire.
///
/// Four claims, each asserting a **value** and not a shape (T-315):
///
/// 1. **The arithmetic comes before the button.** `GET /api/control/scan` with a proposed range and
///    dwell prices the pass — steps, pass length, duty — **without starting anything**. A user
///    about to commit to an 80-minute sweep is told so first.
/// 2. **Starting commissions retunes and names the radio.** The answer carries
///    `device.commissions = "retune"` and the front end's own `device_id`: the request performed no
///    device action, and the log must not read as if it had.
/// 3. **The user wins, and the sweep says so.** An explicit `POST /api/control/center` while a
///    sweep is running is **not refused** — it succeeds, the user's centre stands, and the *same
///    response* carries the `scan.yielded` object their action caused, naming what it took the
///    radio from and which step the sweep kept its place at.
/// 4. **A user action that never reached the device un-yields it.** A refused retune took no radio,
///    so the sweep is still running afterwards — the honesty rule in the other direction.
#[test]
fn a_survey_sweep_can_be_started_from_the_app_and_yields_to_the_user() {
    let (_dir_guard, serving, addr) = start_server();
    wait_for("a live front end", Duration::from_secs(30), || {
        get(addr, "/api/control/state").1["tuning"]["center_hz"].as_f64() == Some(FIXTURE_CENTER_HZ)
    });

    // ---- (0) idle, and `/api/control/state` says so beside the tuning the sweep would move ----
    let (st, v) = get(addr, "/api/control/scan");
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["scan"]["state"], json!("idle"), "{v}");
    assert_eq!(v["scan"]["available"], json!(true), "{v}");
    assert_eq!(
        v["scan"]["plan"],
        Value::Null,
        "an idle scan has no plan: {v}"
    );
    assert_eq!(v["proposed"], Value::Null, "nothing was proposed: {v}");
    let (_, state) = get(addr, "/api/control/state");
    assert_eq!(state["scan"]["state"], json!("idle"), "{state}");

    // ---- (1) the price of a pass, before committing to it ----
    // 20 MHz at the fixture's 2.4 Msps is a 1.8 MHz usable span: 12 steps (20 / 1.8 = 11.1 -> 12),
    // and at 12 s a step that is a 144 s pass in which any one band is heard 12 s — a duty of
    // 1/12. Every one of those is a VALUE, computed by T-406's own pricing, not a shape.
    let (st, v) = get(
        addr,
        "/api/control/scan?f_lo_hz=88000000&f_hi_hz=108000000&dwell_s=12",
    );
    assert_eq!(st, 200, "{v}");
    let p = &v["proposed"];
    assert_eq!(p["plan"]["steps"], json!(12), "{p}");
    assert_eq!(p["plan"]["dwell_s"], json!(12.0), "{p}");
    assert_eq!(p["plan"]["recommended_dwell"], json!(true), "{p}");
    assert_eq!(
        p["plan"]["sample_rate_hz"],
        json!(2.4e6),
        "tiled at the span in force: {p}"
    );
    assert_eq!(p["budget"]["steps"], json!(12), "{p}");
    assert_eq!(p["budget"]["pass_s"], json!(144.0), "{p}");
    assert_eq!(p["budget"]["step_span_hz"], json!(1.8e6), "{p}");
    let duty = p["budget"]["duty"].as_f64().unwrap_or_default();
    assert!((duty - 1.0 / 12.0).abs() < 1e-9, "duty is dwell/pass: {p}");
    // T-965: nothing has retuned this front end under a scan yet, so there is no measurement of
    // what a step pays beyond its dwell — and `null` is not zero. The pass length is the dwells,
    // and the statement says in words that it is a floor rather than implying it is the answer.
    assert_eq!(p["budget"]["dwell_total_s"], json!(144.0), "{p}");
    assert_eq!(p["budget"]["step_overhead_s"], Value::Null, "{p}");
    assert_eq!(p["budget"]["overhead_measured"], json!(false), "{p}");
    let statement = p["budget"]["statement"].as_str().unwrap_or_default();
    assert!(
        statement.contains("a floor, not the answer"),
        "an unmeasured per-step cost must be admitted, not priced at zero: {statement:?}"
    );
    assert!(
        statement.contains("12 steps") && statement.contains("144.0 s pass"),
        "the statement must carry this plan's own numbers: {statement:?}"
    );
    assert!(
        statement.contains("catches nothing during the other"),
        "the statement must state the limit in the same breath as the capability: {statement:?}"
    );
    // Pricing is not starting.
    assert_eq!(v["scan"]["state"], json!("idle"), "{v}");
    assert_eq!(
        get(addr, "/api/control/state").1["tuning"]["center_hz"].as_f64(),
        Some(FIXTURE_CENTER_HZ),
        "pricing a sweep moved the radio"
    );
    // T-1008: the plan's steps, for a client to DRAW rather than re-derive — only when asked for
    // (`windows=1`), and they tile the priced range: twelve slices, contiguous, in visit order.
    assert!(
        p["plan"].get("windows").is_none(),
        "windows are opt-in on GET: {p}"
    );
    assert!(
        v["scan"]["device_id"]
            .as_str()
            .is_some_and(|d| d.starts_with("mock:")),
        "the scan names the front end it would sweep, before the button: {v}"
    );
    let (st, v) = get(
        addr,
        "/api/control/scan?f_lo_hz=88000000&f_hi_hz=108000000&dwell_s=12&windows=1",
    );
    assert_eq!(st, 200, "{v}");
    let windows = v["proposed"]["plan"]["windows"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(windows.len(), 12, "one slice per step: {v}");
    let wf = |w: &Value, k: &str| w[k].as_f64().unwrap_or(f64::NAN);
    assert!((wf(&windows[0], "lo_hz") - 88e6).abs() < 1.0, "{windows:?}");
    assert!(
        (wf(&windows[11], "hi_hz") - 108e6).abs() < 1.0,
        "{windows:?}"
    );
    for (i, w) in windows.iter().enumerate() {
        assert_eq!(w["step"], json!(i), "{w}");
        if i > 0 {
            assert!(
                (wf(w, "lo_hz") - wf(&windows[i - 1], "hi_hz")).abs() < 1.0,
                "the slices tile the range without gap or overlap: {windows:?}"
            );
        }
    }
    let (st, v) = get(addr, "/api/control/scan?windows=2");
    assert_eq!(st, 400, "windows is 1 or 0, nothing else: {v}");

    // A dwell outside the 10-30 s the survey is sized for still runs, and says it is unusual
    // rather than being clamped to one band's taste (T-406).
    let (st, v) = get(addr, "/api/control/scan?dwell_s=2");
    assert_eq!(st, 200, "{v}");
    assert_eq!(
        v["proposed"]["plan"]["recommended_dwell"],
        json!(false),
        "{v}"
    );
    assert_eq!(v["proposed"]["plan"]["dwell_s"], json!(2.0), "{v}");

    // ---- (2) start: it commissions retunes on a named front end, and performs none itself ----
    let (st, v) = post(
        addr,
        "/api/control/scan",
        r#"{"f_lo_hz": 88000000, "f_hi_hz": 108000000, "dwell_s": 2}"#,
    );
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["device"]["commissions"], json!("retune"), "{v}");
    assert!(
        v["device"]["action"].is_null(),
        "no device action was performed in the call: {v}"
    );
    assert!(
        v["device"]["id"]
            .as_str()
            .is_some_and(|d| d.starts_with("mock:")),
        "the sweep must name the front end it commits: {v}"
    );
    assert_eq!(v["scan"]["state"], json!("running"), "{v}");
    assert_eq!(v["scan"]["plan"]["steps"], json!(12), "{v}");
    // T-1008: the start answer carries the steps it committed to — the ones priced above.
    assert_eq!(
        v["scan"]["plan"]["windows"],
        json!(windows),
        "the started plan is the priced plan: {v}"
    );
    assert!(
        get(addr, "/api/control/state").1["scan"]["plan"]
            .get("windows")
            .is_none(),
        "the polled state stays compact"
    );

    // It steps: the tune moves off the fixture's own centre, through the device path.
    wait_for(
        "the sweep to step the tune",
        Duration::from_secs(30),
        || {
            get(addr, "/api/control/scan").1["scan"]["progress"]["steps_done"]
                .as_u64()
                .unwrap_or(0)
                >= 1
        },
    );
    let (_, v) = get(addr, "/api/control/scan");
    let swept = v["scan"]["progress"]["center_hz"]
        .as_f64()
        .expect("a stepped centre");
    assert!(
        (88e6..=108e6).contains(&swept),
        "a step must land inside the range asked for: {v}"
    );
    // T-1008: the step being dwelt on is named, and it is the drawn window whose centre is tuned.
    let (_, v) = get(addr, "/api/control/scan");
    if let (Some(d), Some(c)) = (
        v["scan"]["progress"]["dwell_step"].as_u64(),
        v["scan"]["progress"]["center_hz"].as_f64(),
    ) {
        let tol = hk_core::source::HACKRF_ONE_TUNING_STEP_HZ;
        assert!(
            (wf(&windows[d as usize], "center_hz") - c).abs() <= tol,
            "dwell_step {d} is not the step tuned to {c}: {v}"
        );
    } else {
        panic!("a running sweep that has stepped names the step it dwells on: {v}");
    }

    // T-965: **a step has now been timed, so the stated pass length includes what it cost.**
    // The live defect this closes: `Scan everything (fast)` priced 418 steps x 0.3 s as 125.4 s
    // and took 237 s, because the price counted the listening and not the retuning. The served
    // budget is repriced from this scan's own retunes, and `pass_s` is the listening plus the
    // measured per-step cost, once per step.
    let b = &v["scan"]["budget"];
    assert_eq!(b["overhead_measured"], json!(true), "{b}");
    let overhead_s = b["step_overhead_s"]
        .as_f64()
        .expect("a measured per-step cost once a step has been timed");
    assert!(
        (0.0..30.0).contains(&overhead_s),
        "a measured retune cost, not a guess: {b}"
    );
    let dwell_total_s = b["dwell_total_s"].as_f64().expect("dwell total");
    let steps = b["steps"].as_f64().expect("steps");
    let pass_s = b["pass_s"].as_f64().expect("pass");
    assert!(
        (pass_s - (dwell_total_s + steps * overhead_s)).abs() < 1e-6,
        "pass_s must be the listening plus the measured per-step cost: {b}"
    );
    // And the progress block states the same measurement, so the stated pass and the measured one
    // can be compared without recomputing either.
    let pr = &v["scan"]["progress"];
    assert_eq!(
        pr["measured_step_overhead_s"].as_f64(),
        Some(overhead_s),
        "{pr}"
    );
    assert!(
        (pr["measured_pass_s"].as_f64().expect("measured pass") - pass_s).abs() < 1e-6,
        "{pr}"
    );
    assert!(
        pr["elapsed_s"].as_f64().unwrap_or(-1.0) >= 0.0,
        "a running scan states how long it has been going: {pr}"
    );

    // **Coverage honesty, which is the whole point of the feature.** T-406's finding was that a
    // dwell step must write ONE RECORD PER STEP with its true band and interval, because a coarse
    // record spanning a pass rasterises as "the whole band, the whole time". The sweep gets that
    // from the plane that already exists — the interactive observer closes one dwell record per
    // steady tune — so what lights the canvas is the same coverage the rest of the UI reads, with
    // no second accumulator. Assert the records, and that they are per-step and not one wide one.
    wait_for(
        "the sweep to reach a second step",
        Duration::from_secs(60),
        || {
            get(addr, "/api/control/scan").1["scan"]["progress"]["steps_done"]
                .as_u64()
                .unwrap_or(0)
                >= 3
        },
    );
    let mut stepped: Vec<f64> = Vec::new();
    wait_for("per-step coverage records", Duration::from_secs(60), || {
        let q = format!(
            "f_lo=88000000&f_hi=108000000&t0={}&t1={}",
            unix_now() - 300.0,
            unix_now()
        );
        let (_, v) = get(addr, &format!("/api/observations?{q}"));
        stepped = v["records"]
            .as_array()
            .map(|rs| {
                rs.iter()
                    .filter_map(|r| r["window"]["center_hz"].as_f64())
                    .filter(|c| (88e6..=108e6).contains(c) && (c - FIXTURE_CENTER_HZ).abs() > 1.0)
                    .collect()
            })
            .unwrap_or_default();
        stepped.len() >= 2
    });
    stepped.sort_by(f64::total_cmp);
    stepped.dedup_by(|a, b| (*a - *b).abs() < 1.0);
    assert!(
        stepped.len() >= 2,
        "the sweep must write one record per step, each with its own centre: {stepped:?}"
    );
    // T-1008: THE PLAN DRAWN IS THE PLAN EXECUTED — every per-step record the sweep wrote is
    // centred on one of the windows the scan served for drawing (to the tuning step).
    let tol = hk_core::source::HACKRF_ONE_TUNING_STEP_HZ;
    for c in &stepped {
        assert!(
            windows
                .iter()
                .any(|w| (wf(w, "center_hz") - c).abs() <= tol),
            "the sweep tuned {c} Hz, which is no window the plan served: {windows:?}"
        );
    }
    let widest = stepped.last().unwrap() - stepped.first().unwrap();
    assert!(
        widest > 1.0,
        "the records all share one centre, so a pass is being recorded as one coarse window \
         instead of per-step: {stepped:?}"
    );

    // ---- (3) THE ARBITRATION: the user wins, is not refused, and the sweep says it yielded ----
    let step = hk_core::source::HACKRF_ONE_TUNING_STEP_HZ;
    let wanted = step * ((FIXTURE_CENTER_HZ / step).round());
    let (st, r) = post(
        addr,
        "/api/control/center",
        &format!("{{\"center_hz\":{wanted:?}}}"),
    );
    assert_eq!(
        st, 200,
        "an explicit user tune is never refused by a sweep: {r}"
    );
    assert_eq!(
        r["scan"]["yielded"]["to"],
        json!("retune"),
        "the response that stopped the sweep must say it stopped the sweep: {r}"
    );
    let detail = r["scan"]["yielded"]["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains("kept its place"),
        "a yield is resumable and says so: {detail:?}"
    );
    let (_, v) = get(addr, "/api/control/scan");
    assert_eq!(v["scan"]["state"], json!("yielded"), "{v}");
    assert_eq!(v["scan"]["yielded"]["to"], json!("retune"), "{v}");
    assert_eq!(
        v["scan"]["plan"]["steps"],
        json!(12),
        "it kept its plan: {v}"
    );
    // And the user's tune stands: nothing takes it back.
    std::thread::sleep(Duration::from_secs(3));
    let held = get(addr, "/api/control/state").1["tuning"]["center_hz"].as_f64();
    assert_eq!(
        held,
        Some(wanted),
        "a yielded sweep must not take the tune back from the user"
    );

    // ---- (4) resume, then a refused user action un-yields rather than stopping the sweep ----
    let (st, v) = post(addr, "/api/control/scan", r#"{"resume": true}"#);
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["scan"]["state"], json!("running"), "{v}");
    assert_eq!(v["scan"]["yielded"], Value::Null, "{v}");
    // A centre no front end can reach is refused before it reaches the device, so nothing took the
    // radio — and the sweep is still running.
    let (st, r) = post(addr, "/api/control/center", r#"{"center_hz": 9.9e12}"#);
    assert!(st >= 400, "an unreachable centre must be refused: {r}");
    assert!(r["scan"].is_null(), "a refused action yielded nothing: {r}");
    assert_eq!(
        get(addr, "/api/control/scan").1["scan"]["state"],
        json!("running"),
        "a user action that never reached the device must not stop the sweep"
    );

    // ---- stopping surrenders the radio and is never refused ----
    let (st, v) = post(addr, "/api/control/scan/stop", "{}");
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["scan"]["state"], json!("idle"), "{v}");
    assert_eq!(v["scan"]["plan"], Value::Null, "{v}");
    let (st, v) = post(addr, "/api/control/scan/stop", "{}");
    assert_eq!(st, 200, "stopping an idle sweep is not an error: {v}");

    // A second sweep while one runs is two policies fighting for one tune.
    let (st, _) = post(addr, "/api/control/scan", r#"{"dwell_s": 30}"#);
    assert_eq!(st, 200);
    let (st, v) = post(addr, "/api/control/scan", r#"{"dwell_s": 30}"#);
    assert_eq!(st, 409, "{v}");
    assert_eq!(v["code"], json!("refused"), "{v}");
    let (st, v) = post(addr, "/api/control/scan/stop", "{}");
    assert_eq!(st, 200, "{v}");

    // Half a range is a half-stated request, and is refused rather than guessed at.
    let (st, v) = post(addr, "/api/control/scan", r#"{"f_lo_hz": 88000000}"#);
    assert_eq!(st, 400, "{v}");
    assert!(
        v["error"]
            .as_str()
            .unwrap_or_default()
            .contains("go together"),
        "{v}"
    );

    stop_server(serving);
}

/// T-517: the sweep's step width. A fine step tiles at the span in force (the fixture's 2.4 Msps:
/// 1.8 MHz per step); a coarse step tiles at the widest power-of-two multiple of it the device
/// supports at which the run's bins keep their width (19.2 Msps: 14.4 MHz per step). Values, not
/// shapes (T-315) — and the bin width is asserted IDENTICAL, against the pipeline's own function,
/// so "coarse" can never later be read as permission to degrade the frequency resolution.
#[test]
fn a_coarse_sweep_step_is_fewer_windows_at_the_same_bin_width() {
    let (_dir_guard, serving, addr) = start_server();
    wait_for("a live front end", Duration::from_secs(30), || {
        get(addr, "/api/control/state").1["tuning"]["center_hz"].as_f64() == Some(FIXTURE_CENTER_HZ)
    });
    let price = |q: &str| {
        let (st, v) = get(addr, &format!("/api/control/scan?{q}"));
        assert_eq!(st, 200, "{v}");
        v["proposed"].clone()
    };
    // The whole tunable range (no f_lo/f_hi), at a fast dwell.
    let omitted = price("dwell_s=0.5");
    let fine = price("dwell_s=0.5&step=fine");
    let coarse = price("dwell_s=0.5&step=coarse");
    assert_eq!(
        omitted["plan"], fine["plan"],
        "omitting step is today's fine step"
    );
    assert_eq!(fine["plan"]["step"], json!("fine"), "{fine}");
    assert_eq!(coarse["plan"]["step"], json!("coarse"), "{coarse}");
    assert_eq!(fine["plan"]["steps"], json!(FINE_STEPS), "{fine}");
    assert_eq!(coarse["plan"]["steps"], json!(COARSE_STEPS), "{coarse}");
    assert_eq!(fine["budget"]["step_span_hz"], json!(1.8e6), "{fine}");
    assert_eq!(coarse["budget"]["step_span_hz"], json!(14.4e6), "{coarse}");
    assert_eq!(
        fine["budget"]["pass_s"],
        json!(FINE_STEPS as f64 * 0.5),
        "{fine}"
    );
    assert_eq!(
        coarse["budget"]["pass_s"],
        json!(COARSE_STEPS as f64 * 0.5),
        "{coarse}"
    );
    assert_eq!(fine["plan"]["sample_rate_hz"], json!(2.4e6), "{fine}");
    assert_eq!(fine["plan"]["changes_rate"], json!(false), "{fine}");
    assert_eq!(coarse["plan"]["sample_rate_hz"], json!(19.2e6), "{coarse}");
    assert_eq!(coarse["plan"]["rate_in_force_hz"], json!(2.4e6), "{coarse}");
    assert_eq!(coarse["plan"]["changes_rate"], json!(true), "{coarse}");
    // FREQUENCY RESOLUTION IS UNCHANGED, coarse vs fine, and it is the pipeline's own bin width.
    let bin = hk_pipeline::detection_bin_hz(2.4e6, None);
    assert_eq!(bin, 4_687.5);
    assert_eq!(fine["plan"]["bin_hz"], json!(bin), "{fine}");
    assert_eq!(coarse["plan"]["bin_hz"], json!(bin), "{coarse}");
    // Pricing moved nothing.
    let (_, state) = get(addr, "/api/control/state");
    assert_eq!(state["tuning"]["sample_rate_hz"], json!(2.4e6), "{state}");
    assert_eq!(
        state["tuning"]["center_hz"].as_f64(),
        Some(FIXTURE_CENTER_HZ)
    );

    // Anything but fine/coarse is refused, on either route.
    let (st, v) = get(addr, "/api/control/scan?step=medium");
    assert_eq!((st, v["code"].clone()), (400, json!("invalid")), "{v}");
    let (st, v) = post(addr, "/api/control/scan", r#"{"step": 3}"#);
    assert_eq!((st, v["code"].clone()), (400, json!("invalid")), "{v}");
    let (st, v) = post(
        addr,
        "/api/control/scan",
        r#"{"resume": true, "step": "coarse"}"#,
    );
    assert_eq!(st, 400, "resume takes no step: {v}");

    // A coarse start names the rate change it commits the front end to, beside the retunes.
    let (st, v) = post(
        addr,
        "/api/control/scan",
        r#"{"f_lo_hz": 88000000, "f_hi_hz": 108000000, "dwell_s": 2, "step": "coarse"}"#,
    );
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["device"]["commissions"], json!("retune"), "{v}");
    assert_eq!(v["device"]["commissions_rate_hz"], json!(19.2e6), "{v}");
    assert_eq!(
        v["scan"]["plan"]["steps"],
        json!(2),
        "20 MHz at 14.4 MHz per step: {v}"
    );
    let (st, _) = post(addr, "/api/control/scan/stop", "{}");
    assert_eq!(st, 200);
    // A fine start commits no rate change.
    let (st, v) = post(
        addr,
        "/api/control/scan",
        r#"{"f_lo_hz": 88000000, "f_hi_hz": 108000000, "dwell_s": 2}"#,
    );
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["device"]["commissions_rate_hz"], Value::Null, "{v}");
    let (st, _) = post(addr, "/api/control/scan/stop", "{}");
    assert_eq!(st, 200);
    stop_server(serving);
}

/// Full-range steps at the fixture's 2.4 Msps: fine 1.8 MHz, coarse 14.4 MHz.
const FINE_STEPS: u64 = 3334;
const COARSE_STEPS: u64 = 418;

// --- Historical playback (T-463) -------------------------------------------------------------------

/// T-463 (AWARE-011): `GET/POST /api/playback` move the one playhead, and `/ws/open/playback`
/// re-runs demod from the IQ ring at it — or refuses `409 no-iq` beyond the IQ horizon, with the
/// reason, rather than going silent.
#[test]
fn playback_route_and_opener_answer_as_documented() {
    let (_dir_guard, serving, addr) = start_server();

    // Before any seek: no position, paused, one playhead, analysis is the recorded one.
    let (st, v) = get(addr, "/api/playback");
    assert_eq!(st, 200, "{v}");
    assert!(v["playhead"]["position_ns"].is_null(), "{v}");
    assert_eq!(v["playhead"]["playing"], json!(false), "{v}");
    assert_eq!(v["playhead"]["speed"], json!(1.0), "{v}");
    assert_eq!(v["playheads"], json!(1), "{v}");
    assert_eq!(v["analysis"], json!("recorded"), "{v}");
    assert_eq!(v["rerun"], json!(["demod", "decode"]), "{v}");
    assert_eq!(v["iq_horizon"], json!("iq-ring + recordings"), "{v}");
    assert!(v["max_speed"].is_f64() && v["streams"].is_object(), "{v}");
    let (st, v) = get(addr, "/api/playback?x=1");
    assert_eq!(st, 400, "{v}");

    // Refusals: nothing to play from, a bad speed, an unknown field.
    let (st, v) = post(addr, "/api/playback", r#"{"playing": true}"#);
    assert_eq!((st, v["code"].as_str()), (409, Some("no_position")), "{v}");
    for body in [
        r#"{"speed": 0}"#,
        r#"{"speed": 99}"#,
        r#"{"mode": "wfm"}"#,
        "{}",
    ] {
        let (st, v) = post(addr, "/api/playback", body);
        assert_eq!(
            (st, v["code"].as_str()),
            (400, Some("invalid")),
            "{body}: {v}"
        );
    }

    // Wait for 1.5 s of raw IQ in the ring, then play from 0.2 s into it.
    let mut t0 = 0.0;
    wait_for("1.5 s of IQ in the ring", Duration::from_secs(60), || {
        let (_, s) = get(addr, "/api/iqbuffer");
        t0 = s["t0"].as_f64().unwrap_or(0.0);
        s["span_s"].as_f64().unwrap_or(0.0) >= 1.5
    });
    let start = t0 + 0.2;
    let (st, v) = post(
        addr,
        "/api/playback",
        &format!(r#"{{"t": {start}, "playing": true}}"#),
    );
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["playhead"]["playing"], json!(true), "{v}");
    assert!(
        (v["playhead"]["position_s"].as_f64().unwrap() - start).abs() < 0.05,
        "{v}"
    );

    // Audio at the playhead: the header is an audio stream whose mode was estimated.
    let (f_lo, f_hi) = (STATION_HZ - 100e3, STATION_HZ + 100e3);
    let mut ws = connect_ws(
        addr,
        &format!("/ws/open/playback?f_lo={f_lo}&f_hi={f_hi}&token={TOKEN}"),
    )
    .unwrap();
    let header: Value = match ws.read().unwrap() {
        Message::Text(t) => serde_json::from_str(t.as_str()).unwrap(),
        other => panic!("unexpected first message: {other:?}"),
    };
    assert_eq!(header["kind"], json!("audio"), "{header}");
    assert_eq!(header["datatype"], json!("ri16_le"), "{header}");
    assert!(header["audio"]["mode"].is_string(), "{header}");
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut saw_data = false;
    while Instant::now() < deadline && !saw_data {
        if let Ok(Message::Binary(b)) = ws.read() {
            saw_data = b[0] == 1 && (b.len() - 32) % 2 == 0;
        }
    }
    assert!(saw_data, "no PCM record at the playhead within 30 s");
    let _ = ws.close(None);

    // Pause: the view freezes, never the capture.
    let (st, v) = post(addr, "/api/playback", r#"{"playing": false}"#);
    assert_eq!(
        (st, v["playhead"]["playing"].as_bool()),
        (200, Some(false)),
        "{v}"
    );

    // Beyond the IQ horizon: refused up front, with the reason.
    let mut refused = connect_ws(
        addr,
        &format!(
            "/ws/open/playback?f_lo={f_lo}&f_hi={f_hi}&t={}&token={TOKEN}",
            t0 - 3600.0
        ),
    )
    .unwrap();
    let msg = loop {
        match refused.read().unwrap() {
            Message::Text(t) => break t,
            _ => continue,
        }
    };
    let v: Value = serde_json::from_str(msg.as_str()).unwrap();
    assert_eq!(v["type"], json!("refused"), "{v}");
    assert_eq!(
        (v["status"].as_u64(), v["code"].as_str()),
        (Some(409), Some("no-iq")),
        "{v}"
    );
    assert!(
        v["reason"].as_str().unwrap().contains("waterfall-only"),
        "{v}"
    );

    stop_server(serving);
}

/// T-573 — **a viewport's worth of tile addresses in ONE request.**
///
/// The assertions are COUNTS and the per-address marks, never a wall clock: eight addresses cost
/// one HTTP request instead of eight; each entry carries its own address, level pair, coverage
/// plane and `cost`, and equals what `GET /api/tiles` answers for that address alone; a tile
/// nothing ever sampled is still answered from the coverage map without reaching the generation
/// path; and both caps are enforced with the answer documented in `docs/api.md`.
#[test]
fn tiles_batch_answers_a_viewport_in_one_request_without_flattening_its_marks() {
    let (_dir_guard, serving, addr) = start_server();
    const N: u64 = 32;
    let one = |fi: u64, ti: u64| {
        format!("/api/tiles?level_f=0&level_t=0&f_index={fi}&t_index={ti}&cells={N}")
    };
    let batch = |spelling: &str| format!("/api/tiles/batch?cells={N}&addresses={spelling}");

    let (st, probe) = get(addr, &one(0, 0));
    assert_eq!(st, 200, "{probe}");
    let f_cell = probe["axes"]["frequency"]["cell_hz"].as_f64().unwrap();
    let t_cell = probe["axes"]["time"]["cell_s"].as_f64().unwrap();
    let f_index = (STATION_HZ / (f_cell * N as f64)).floor() as u64;
    let t_now = || (unix_now() / (t_cell * N as f64)) as u64;
    wait_for(
        "the station's tile to be observed",
        Duration::from_secs(60),
        || {
            get(addr, &one(f_index, t_now())).1["grid"]["observed_cells"]
                .as_u64()
                .is_some_and(|c| c > 0)
        },
    );

    // Eight addresses across the station's band — a pane row — at an address **two tile-heights
    // behind the live edge**, whose extent is entirely in the past. A tile at the edge grows
    // between two HTTP requests, and the comparison below is between a batch read and eight
    // single reads: across a growing tile that compares two different grids. Waited for rather
    // than assumed, because "it has data by now" is the premise of everything after it.
    let deadline = Instant::now() + Duration::from_secs(120);
    let ti = loop {
        let ti = t_now().saturating_sub(2);
        if get(addr, &one(f_index, ti)).1["grid"]["observed_cells"]
            .as_u64()
            .is_some_and(|c| c > 0)
        {
            break ti;
        }
        assert!(
            Instant::now() < deadline,
            "no settled observed tile behind the live edge"
        );
        std::thread::sleep(Duration::from_millis(250));
    };
    let bases: Vec<u64> = (0..8).map(|i| f_index + i).collect();
    let spelling = bases
        .iter()
        .map(|f| format!("0.0.{f}.{ti}"))
        .collect::<Vec<_>>()
        .join(",");

    // ONE request. Eight tiles.
    let (st, v) = get(addr, &batch(&spelling));
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["requested"], json!(8), "{v}");
    assert_eq!(v["returned"], json!(8), "{v}");
    assert_eq!(v["truncated"], json!(false));
    assert_eq!(v["remaining"], json!([]));
    assert_eq!(v["tiles"].as_array().map(Vec::len), Some(8), "{v}");

    let mut observed = 0;
    let mut unobserved = 0;
    for (i, f) in bases.iter().enumerate() {
        let e = &v["tiles"][i];
        assert_eq!(e["status"], json!(200), "{e}");
        assert_eq!(e["address"]["f_index"], json!(*f), "{}", e["address"]);
        assert_eq!(e["address"]["t_index"], json!(ti), "{}", e["address"]);
        assert_eq!(e["address"]["spelling"], json!(format!("0.0.{f}.{ti}")));
        // Byte-identical to the single-tile answer, bar this read's own diagnostics.
        let (st, alone) = get(addr, &one(*f, ti));
        assert_eq!(st, 200, "{alone}");
        for field in ["key", "extent", "axes", "grid", "coverage", "sealed"] {
            assert_eq!(
                e["tile"][field], alone[field],
                "{field} differs between the batch and the single-tile route at {f}"
            );
        }
        // The three marks, per address, where they already were.
        if e["tile"]["grid"]["observed_cells"].as_u64().unwrap_or(0) > 0 {
            observed += 1;
        }
        if e["tile"]["resolution"]["short_circuit"]["applied"] == json!(true) {
            unobserved += 1;
            // The standing invariant: a tile with no data is answered from the coverage map and
            // never runs the generation path — batching must not make empty tiles expensive again.
            assert_eq!(e["tile"]["cost"]["source_cells"], json!(0), "{e}");
            assert_eq!(e["tile"]["cost"]["chunks"], json!(0), "{e}");
        }
    }
    assert!(observed > 0, "the station's own band must carry data: {v}");

    // A tile nothing ever sampled, asked for in the same request as one that did. If the fixture's
    // own row did not already contain one, reach for a band 2000 tiles away.
    if unobserved == 0 {
        let far = f_index + 2_000;
        let mixed = format!("0.0.{f_index}.{ti},0.0.{far}.{ti}");
        let (st, m) = get(addr, &batch(&mixed));
        assert_eq!(st, 200, "{m}");
        assert_eq!(m["returned"], json!(2), "{m}");
        let e = &m["tiles"][1];
        assert_eq!(e["status"], json!(200), "{e}");
        assert_eq!(e["tile"]["cost"]["source_cells"], json!(0), "{e}");
        assert_ne!(
            m["tiles"][0]["tile"]["coverage"], e["tile"]["coverage"],
            "two different marks must stay two different marks inside one response"
        );
    }

    // **Caps.** Over `max_addresses` is refused, naming the cap — nothing produced, and the
    // caller knows exactly what still needs asking for.
    let cap = v["limits"]["max_addresses"].as_u64().unwrap();
    assert_eq!(cap, 64, "docs/api.md documents 64");
    assert_eq!(v["limits"]["max_response_bytes"], json!(8 * 1024 * 1024));
    let too_many = (0..=cap)
        .map(|i| format!("0.0.{}.{ti}", f_index + i))
        .collect::<Vec<_>>()
        .join(",");
    let (st, e) = get(addr, &batch(&too_many));
    assert_eq!(st, 400, "{e}");
    assert!(
        e["error"].as_str().unwrap_or_default().contains("64"),
        "{e}"
    );

    // A malformed address refuses the WHOLE request rather than being dropped silently.
    for bad in ["0.0.1", "0.0.1.2.3", "x.0.1.2"] {
        let (st, e) = get(addr, &batch(bad));
        assert_eq!(st, 400, "{bad}: {e}");
    }
    // …and no addresses at all is not a cheaper spelling of anything.
    let (st, e) = get(addr, &format!("/api/tiles/batch?cells={N}"));
    assert_eq!(st, 400, "{e}");

    // The per-address parameters do not belong beside the batch.
    let (st, e) = get(
        addr,
        &format!("{}&f_index=3", batch(&format!("0.0.{f_index}.{ti}"))),
    );
    assert_eq!(st, 400, "{e}");

    eprintln!(
        "T-573: 8 tiles in 1 request ({} B) against 8 requests; batch cap {} addresses / {} B",
        serde_json::to_vec(&v).unwrap().len(),
        cap,
        v["limits"]["max_response_bytes"],
    );
    stop_server(serving);
}

/// T-533 — **the measurement plane on the wire, and the transfer coding under it.**
///
/// A live tile's body was 1 878 289 B, of which `grid.max_db` was 1 197 118 B: JSON decimal text,
/// seventeen significant digits a cell, for values whose destination is an R16F texture. Two
/// levers, asserted here because `docs/api.md` now promises both:
///
///  1. **`?planes=f16`** spells that one plane as base64 little-endian binary16. This checks the
///     two spellings are the SAME grid — cell for cell, absence for absence — because that is what
///     makes it a representation and not a second answer; that the wire STATES its type, byte order,
///     transfer and absent-value rather than leaving them to be inferred; and that an encoding this
///     server does not serve is a `400` naming it, never a quiet fall back to the other one.
///  2. **`Accept-Encoding: gzip`** compresses the response. The decompressed bytes must be
///     byte-identical to the plain answer: a transfer coding may never change what was said.
#[test]
fn tile_planes_are_typed_on_request_and_the_route_honours_accept_encoding() {
    let (_dir_guard, serving, addr) = start_server();
    const N: u64 = 32;
    let tile = |fi: u64, ti: u64, extra: &str| {
        format!("/api/tiles?level_f=0&level_t=0&f_index={fi}&t_index={ti}&cells={N}{extra}")
    };
    let (st, probe) = get(addr, &tile(0, 0, ""));
    assert_eq!(st, 200, "{probe}");
    // Even the tile the coverage map answers on its own states its encoding: a reader that has to
    // look at which fields are present to learn the spelling reads the wrong one when a field is
    // legitimately missing.
    assert_eq!(
        probe["grid"]["encoding"]["planes"],
        json!("json"),
        "{probe}"
    );
    let f_cell = probe["axes"]["frequency"]["cell_hz"].as_f64().unwrap();
    let t_cell = probe["axes"]["time"]["cell_s"].as_f64().unwrap();
    let f_index = (STATION_HZ / (f_cell * N as f64)).floor() as u64;
    let t_now = || (unix_now() / (t_cell * N as f64)) as u64;
    wait_for(
        "the station's tile to be observed",
        Duration::from_secs(60),
        || {
            get(addr, &tile(f_index, t_now(), "")).1["grid"]["observed_cells"]
                .as_u64()
                .is_some_and(|c| c > 0)
        },
    );

    // **Two reads of one grid, and it has to BE one grid.** A tile at the live edge grows between
    // two HTTP requests — measured: a cell that was `null` in the first read carried a level in
    // the second, milliseconds later — and a comparison across that is a comparison of two
    // different tiles. So the address used here is **two tile-heights behind the edge**, whose
    // extent is entirely in the past and whose rows are folded, and the pair is re-read until both
    // spellings report the same `observed_cells`. That equality is the premise of everything below
    // it, so it is checked rather than assumed.
    let deadline = Instant::now() + Duration::from_secs(60);
    let (settled, plain, packed) = loop {
        let ti = t_now().saturating_sub(2);
        let (st_a, a) = get(addr, &tile(f_index, ti, "&planes=json"));
        let (st_b, b) = get(addr, &tile(f_index, ti, "&planes=f16"));
        let cells = |v: &Value| v["grid"]["observed_cells"].as_u64().unwrap_or(0);
        if st_a == 200 && st_b == 200 && cells(&a) > 0 && cells(&a) == cells(&b) {
            break (ti, a, b);
        }
        assert!(
            Instant::now() < deadline,
            "never got one settled grid in both spellings: {st_a}/{st_b}, \
             observed {}/{}",
            cells(&a),
            cells(&b)
        );
        std::thread::sleep(Duration::from_millis(200));
    };

    assert_eq!(plain["grid"]["encoding"]["planes"], json!("json"));
    assert_eq!(packed["grid"]["encoding"]["planes"], json!("f16"));
    // Absent, not empty, in each direction: an empty array would read as a grid of no cells.
    assert!(plain["grid"]["planes"].is_null(), "{}", plain["grid"]);
    assert!(packed["grid"]["max_db"].is_null(), "{}", packed["grid"]);
    // The three planes JSON spells more cheaply than binary16 does are untouched by `f16` — the
    // mode packs the plane that wins and no other (measured: `frames` is 131 073 B as text against
    // 349 528 B as base64 `u32`).
    for k in ["occupancy_max", "coverage", "frames"] {
        assert!(packed["grid"][k].is_array(), "{k}: {}", packed["grid"]);
    }

    let plane = &packed["grid"]["planes"]["max_db"];
    assert_eq!(plane["type"], json!("f16"), "{plane}");
    assert_eq!(plane["byte_order"], json!("little-endian"), "{plane}");
    assert_eq!(plane["transfer"], json!("base64"), "{plane}");
    assert_eq!(plane["absent"], json!("nan"), "{plane}");
    assert_eq!(plane["cells"], json!(N * N), "{plane}");
    assert_eq!(plane["bytes"], json!(N * N * 2), "{plane}");
    assert_eq!(
        plane["scale"], plain["grid"]["semantics"]["series"]["max_db"]["scale"],
        "the packed plane must name the SAME scale the JSON one is served in"
    );

    // The values, cell for cell. Equal to within binary16's own precision — which is the precision
    // the R16F texture keeps either way — and `null` matched by a non-finite, never by a zero.
    let cells = plain["grid"]["max_db"].as_array().unwrap();
    let bytes = b64_decode(plane["data"].as_str().unwrap());
    assert_eq!(bytes.len(), cells.len() * 2, "{plane}");
    let mut observed = 0usize;
    for (i, cell) in cells.iter().enumerate() {
        let bits = u16::from_le_bytes([bytes[i * 2], bytes[i * 2 + 1]]);
        let v = f16_to_f32(bits);
        match cell.as_f64() {
            None => assert!(!v.is_finite(), "cell {i}: null in JSON, {v} packed"),
            Some(want) => {
                assert!(
                    (v as f64 - want).abs() < 0.07,
                    "cell {i}: {want} dB in JSON, {v} dB packed"
                );
                observed += 1;
            }
        }
    }
    assert!(
        observed > 0,
        "no cell carried a level, so the comparison above would pass on two empty grids"
    );
    // Smaller, stated as a value: the plane is two bytes a cell however loud the band is, which is
    // the property decimal text does not have.
    let as_text = plain["grid"]["max_db"].to_string().len();
    let as_plane = plane["data"].as_str().unwrap().len();
    assert_eq!(as_plane, (cells.len() * 2).div_ceil(3) * 4, "{plane}");
    assert!(as_plane * 2 < as_text, "{as_plane} B packed vs {as_text} B");

    // An encoding this server does not serve: refused, and the refusal names it and the choices.
    let (st, e) = get(addr, &tile(f_index, settled, "&planes=f8"));
    assert_eq!(st, 400, "{e}");
    let msg = e["error"].as_str().unwrap_or_default();
    assert!(
        msg.contains("f8") && msg.contains("json, f16, compact"),
        "{msg}"
    );

    // ---- T-1019: `compact`, the other three planes -------------------------------------------
    //
    // The same grid again: `max_db` byte-identical to `f16`'s plane, `frames` exact at whatever
    // width this tile's counts need, and `grid.coverage` NOT SENT — `coverage.planes[].runs`
    // beside the grid already carries per-cell coverage, which is the duplication this spelling
    // exists to stop.
    let (st, compact) = get(addr, &tile(f_index, settled, "&planes=compact"));
    assert_eq!(st, 200, "{compact}");
    assert_eq!(compact["grid"]["encoding"]["planes"], json!("compact"));
    for k in ["max_db", "occupancy_max", "coverage", "frames"] {
        assert!(
            compact["grid"][k].is_null(),
            "grid.{k} is still spelled per cell under `compact`: {}",
            compact["grid"][k]
        );
    }
    assert!(
        !compact["coverage"]["planes"]
            .as_array()
            .map(|p| p.is_empty())
            .unwrap_or(true),
        "the grey authority must still be served: {}",
        compact["coverage"]
    );
    assert_eq!(
        compact["grid"]["planes"]["max_db"], packed["grid"]["planes"]["max_db"],
        "one spelling of the measurement plane, not two"
    );
    let occ = &compact["grid"]["planes"]["occupancy_max"];
    assert_eq!(occ["type"], json!("f16"), "{occ}");
    assert_eq!(occ["scale"], json!("fraction"), "{occ}");
    assert_eq!(occ["absent"], json!("nan"), "{occ}");
    let want_occ = plain["grid"]["occupancy_max"].as_array().unwrap();
    let occ_bytes = b64_decode(occ["data"].as_str().unwrap());
    assert_eq!(occ_bytes.len(), want_occ.len() * 2, "{occ}");
    for (i, cell) in want_occ.iter().enumerate() {
        let v = f16_to_f32(u16::from_le_bytes([occ_bytes[i * 2], occ_bytes[i * 2 + 1]]));
        match cell.as_f64() {
            None => assert!(
                !v.is_finite(),
                "occupancy cell {i}: null in JSON, {v} packed"
            ),
            Some(w) => assert!(
                (v as f64 - w).abs() <= 1e-3 + w.abs() * 1e-3,
                "occupancy cell {i}: {w} in JSON, {v} packed"
            ),
        }
    }
    let fr = &compact["grid"]["planes"]["frames"];
    let want_frames = plain["grid"]["frames"].as_array().unwrap();
    assert!(
        ["u8", "u16", "u32", "u64"].contains(&fr["type"].as_str().unwrap_or_default()),
        "{fr}"
    );
    let width = fr["bytes"].as_u64().unwrap() as usize / want_frames.len();
    let fr_bytes = b64_decode(fr["data"].as_str().unwrap());
    assert_eq!(fr_bytes.len(), want_frames.len() * width, "{fr}");
    let mut counted = 0usize;
    for (i, cell) in want_frames.iter().enumerate() {
        let mut got = [0u8; 8];
        got[..width].copy_from_slice(&fr_bytes[i * width..(i + 1) * width]);
        let want = cell.as_u64().expect("frames is a count, never null");
        assert_eq!(u64::from_le_bytes(got), want, "frames cell {i}");
        counted += usize::from(want > 0);
    }
    assert!(
        counted > 0,
        "no cell carried a frame count, so the comparison above would pass on an empty grid"
    );

    // ---- the transfer coding ------------------------------------------------------------------
    let path = tile(f_index, settled, "&planes=f16");
    let (st, _, plainb) = get_encoded(addr, &path, None);
    assert_eq!(st, 200);
    let (st, enc, gz) = get_encoded(addr, &path, Some("gzip"));
    assert_eq!(st, 200);
    assert_eq!(
        enc.as_deref(),
        Some("gzip"),
        "the route ignored accept-encoding"
    );
    assert!(
        gz.len() * 2 < plainb.len(),
        "gzip returned {} B against {} B — measured on a live tile it is about ninefold",
        gz.len(),
        plainb.len()
    );
    // **A transfer coding may not change what was said.** Same JSON, byte for byte after inflating.
    let mut inflated = Vec::new();
    flate2::read::GzDecoder::new(&gz[..])
        .read_to_end(&mut inflated)
        .expect("the body must be a gzip member");
    let (a, b): (Value, Value) = (
        serde_json::from_slice(&inflated).unwrap(),
        serde_json::from_slice(&plainb).unwrap(),
    );
    assert_eq!(a["grid"]["planes"], b["grid"]["planes"]);
    assert_eq!(a["key"], b["key"]);
    // A caller that says it cannot read gzip is not sent gzip.
    let (_, enc, _) = get_encoded(addr, &path, Some("gzip;q=0"));
    assert_eq!(enc, None, "`gzip;q=0` is a refusal and must be honoured");
    // And a short body is not worth a gzip member's own header and trailer. The premise — that
    // this route's answer IS short — is read from the uncompressed answer rather than assumed, so
    // the day it grows past the threshold this reads as the premise changing and not as the rule
    // breaking.
    let (_, _, uncompressed) = get_encoded(addr, "/api/status", None);
    let (_, enc, _) = get_encoded(addr, "/api/status", Some("gzip"));
    assert_eq!(
        enc,
        (uncompressed.len() >= 4096).then(|| "gzip".to_owned()),
        "a {} B body was {}compressed",
        uncompressed.len(),
        if enc.is_some() { "" } else { "not " }
    );

    // ---- the four spellings, RE-MEASURED on the tile size the canvas actually renders ----------
    //
    // T-700 relands this onto a `tiles.rs` that T-571 (live incremental maintenance) and T-595
    // (`excluded`) rewrote, so the 2026-09-20 figures are quoted nowhere: the four bodies are read
    // back to back HERE, from ONE address, and the ordering between them is asserted. Byte counts,
    // never wall clock (the ticket's own rule) — `cost.build_ms` is printed because the honest
    // half of this result is that it does NOT move, and a future read of this log should see that.
    let big = format!(
        "/api/tiles?level_f=0&level_t=0&f_index={}&t_index={}&cells=256",
        (STATION_HZ / (f_cell * N as f64 * 8.0)).floor() as u64,
        (settled as f64 / 8.0) as u64,
    );
    let read = |extra: &str, ae: Option<&str>| {
        let (st, enc, body) = get_encoded(addr, &format!("{big}{extra}"), ae);
        assert_eq!(st, 200, "{}{extra}", big);
        (enc, body.len())
    };
    let (_, json_b) = read("&planes=json", None);
    let (gz_enc, json_gz_b) = read("&planes=json", Some("gzip"));
    let (_, f16_b) = read("&planes=f16", None);
    let (f16_gz_enc, f16_gz_b) = read("&planes=f16", Some("gzip"));
    let (_, compact_b) = read("&planes=compact", None);
    let (_, compact_gz_b) = read("&planes=compact", Some("gzip"));
    let build_ms = |extra: &str| {
        get(addr, &format!("{big}{extra}")).1["cost"]["build_ms"]
            .as_f64()
            .unwrap_or(f64::NAN)
    };
    eprintln!(
        "T-700 / T-533 / T-1019 re-measured (256x256 tile, one address, six spellings):\n           json              {json_b} B   build_ms {:.1}\n           json + gzip       {json_gz_b} B\n           f16               {f16_b} B   build_ms {:.1}\n           f16 + gzip        {f16_gz_b} B\n           compact           {compact_b} B   build_ms {:.1}\n           compact + gzip    {compact_gz_b} B   -> {:.1}x off json",
        build_ms("&planes=json"),
        build_ms("&planes=f16"),
        build_ms("&planes=compact"),
        json_b as f64 / compact_gz_b as f64,
    );
    assert_eq!(gz_enc.as_deref(), Some("gzip"));
    assert_eq!(f16_gz_enc.as_deref(), Some("gzip"));
    // **Both levers pay and neither subsumes the other.** JSON decimal text is high-entropy by
    // construction, so compressing it is not the same as not sending it: the packed body must beat
    // the JSON one, the gzipped packed body must beat the gzipped JSON one, and the two together
    // must beat either alone. Ordering, not magic numbers — the magnitudes move with the fixture.
    assert!(f16_b < json_b, "{f16_b} B packed vs {json_b} B as JSON");
    assert!(
        json_gz_b < json_b && f16_gz_b < f16_b,
        "gzip did not shrink"
    );
    assert!(
        f16_gz_b < json_gz_b && f16_gz_b < f16_b,
        "packed+gzipped {f16_gz_b} B must beat gzip alone ({json_gz_b} B) and f16 alone ({f16_b} B)"
    );
    // T-1019: the spelling that packs the other three planes and stops sending `grid.coverage`
    // twice must beat the one that packs only `max_db`, uncompressed and gzipped alike.
    assert!(
        compact_b < f16_b,
        "compact {compact_b} B against f16 {f16_b} B"
    );
    assert!(
        compact_gz_b < f16_gz_b,
        "compact+gzip {compact_gz_b} B against f16+gzip {f16_gz_b} B"
    );
    // The headline claim, asserted rather than quoted: an order of magnitude off the wire.
    assert!(
        f16_gz_b * 8 < json_b,
        "the two levers together were {}x, not the order of magnitude this ticket exists for \
         ({json_b} B -> {f16_gz_b} B)",
        json_b / f16_gz_b.max(1)
    );

    stop_server(serving);
}

/// A GET with an explicit `Accept-Encoding`: `(status, Content-Encoding, raw body bytes)`.
///
/// Raw, because the point is what came off the socket — a helper that transparently inflated would
/// make the assertion about itself.
fn get_encoded(
    addr: SocketAddr,
    path: &str,
    accept: Option<&str>,
) -> (u16, Option<String>, Vec<u8>) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let ae = accept.map_or(String::new(), |a| format!("Accept-Encoding: {a}\r\n"));
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: t\r\nAuthorization: Bearer {TOKEN}\r\n{ae}Connection: close\r\n\r\n"
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
    let enc = head
        .lines()
        .find_map(|l| l.strip_prefix("Content-Encoding: "))
        .map(str::to_owned);
    (status, enc, raw[split + 4..].to_vec())
}

/// Standard base64 decode, for reading a packed plane back (T-533).
fn b64_decode(s: &str) -> Vec<u8> {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let val = |c: u8| A.iter().position(|&a| a == c).expect("base64 alphabet") as u32;
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len() / 4 * 3);
    for c in b.chunks(4) {
        let pad = c.iter().filter(|&&x| x == b'=').count();
        let n = (val(c[0]) << 18)
            | (val(c[1]) << 12)
            | (if pad < 2 { val(c[2]) } else { 0 } << 6)
            | (if pad < 1 { val(c[3]) } else { 0 });
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    out
}

/// One IEEE 754 binary16, as the sixteen bits the wire sent (T-533).
fn f16_to_f32(b: u16) -> f32 {
    let s = if b >> 15 == 1 { -1.0f32 } else { 1.0 };
    let (e, m) = ((b >> 10) & 0x1f, (b & 0x3ff) as f32);
    match e {
        0 => s * m * 2f32.powi(-24),
        31 => f32::NAN,
        _ => s * (m + 1024.0) * 2f32.powi(e as i32 - 25),
    }
}

/// **T-1009 — a sweep and a clip are addressed to a NAMED front end.**
///
/// The map's Measure box offers "Scan this region with &lt;device&gt;" and "Record IQ of this region
/// with &lt;device&gt;", so the choice of radio has to survive the trip to the engine. A run holds
/// one scan runner per front end ([`hk_api::scan::ScanRunners`]) and each front end keeps its own
/// IQ ring, and both routes take the same `device_id` selector the six device routes take.
///
/// This run holds exactly one front end (the mock SDR) — the case the invariant protects: **with
/// one device the selector may be omitted and behaviour is unchanged**, and naming that one device
/// is accepted. Asserted on the wire, by value:
///
/// 1. `GET /api/control/state` enumerates a sweep per front end in `scans`, each naming its own
///    `device_id`, and with one front end it agrees with the singular `scan`;
/// 2. `GET /api/control/scan?device_id=…` prices on the named radio and answers for it;
/// 3. a selector naming a radio this run does not hold is `404 unknown_device` — on the price, on
///    the start and on a clip — and never falls back to the default radio;
/// 4. a start naming this run's own radio is accepted and its `device.commissions`/`device.id`
///    name that radio; stopping it, named or not, is still never refused.
#[test]
fn t1009_a_scan_and_a_clip_are_addressed_to_a_named_front_end() {
    let (_dir_guard, serving, addr) = start_server();
    wait_for("a live front end", Duration::from_secs(30), || {
        get(addr, "/api/control/state").1["tuning"]["center_hz"].as_f64() == Some(FIXTURE_CENTER_HZ)
    });

    // (1) one sweep per front end, each naming its radio.
    let (st, v) = get(addr, "/api/control/state");
    assert_eq!(st, 200, "{v}");
    let device_id = v["device"]["device_id"]
        .as_str()
        .unwrap_or_else(|| panic!("the live source must report its device_id: {v}"))
        .to_owned();
    let scans = v["scans"]
        .as_array()
        .unwrap_or_else(|| panic!("control/state must enumerate a sweep per front end: {v}"));
    assert_eq!(scans.len(), 1, "this run holds one front end: {v}");
    assert_eq!(scans[0]["device_id"], json!(device_id), "{v}");
    assert_eq!(
        scans[0]["state"], v["scan"]["state"],
        "with one front end the enumeration and the singular default are the same sweep: {v}"
    );

    // (2) pricing on the named radio.
    // The id is a bare `mock:<name>`; nothing in it needs escaping in a query string.
    let named = format!(
        "/api/control/scan?f_lo_hz=88000000&f_hi_hz=90000000&dwell_s=1&device_id={device_id}"
    );
    let (st, v) = get(addr, &named);
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["scan"]["device_id"], json!(device_id), "{v}");
    let steps = v["proposed"]["plan"]["steps"]
        .as_u64()
        .unwrap_or_else(|| panic!("a named radio prices a pass: {v}"));
    assert!(steps >= 1, "{v}");

    // (3) a radio this run does not hold: refused everywhere, never the default one instead.
    let (st, v) = get(addr, "/api/control/scan?device_id=mock:not-this-radio");
    assert_eq!(st, 404, "{v}");
    assert_eq!(v["code"], json!("unknown_device"), "{v}");
    let (st, v) = post(
        addr,
        "/api/control/scan",
        "{\"f_lo_hz\":88000000,\"f_hi_hz\":90000000,\"dwell_s\":1,\"device_id\":\"mock:not-this-radio\"}",
    );
    assert_eq!(st, 404, "{v}");
    assert_eq!(v["code"], json!("unknown_device"), "{v}");
    assert_eq!(
        get(addr, "/api/control/scan").1["scan"]["state"],
        json!("idle"),
        "a refused start commissioned nothing"
    );
    let (st, v) = post(
        addr,
        "/api/iqbuffer/clip",
        "{\"t0\":1.0,\"t1\":2.0,\"device_id\":\"mock:not-this-radio\"}",
    );
    assert_eq!(st, 404, "{v}");
    assert_eq!(
        v["code"],
        json!("unknown_device"),
        "a clip names whose ring it comes from: {v}"
    );
    assert!(
        v["error"].as_str().unwrap_or_default().contains(&device_id),
        "the refusal names the ring this run does hold: {v}"
    );

    // (4) the start this run's own radio accepts, and the stop that is never refused.
    let (st, v) = post(
        addr,
        "/api/control/scan",
        &format!(
            "{{\"f_lo_hz\":88000000,\"f_hi_hz\":90000000,\"dwell_s\":1,\"device_id\":{}}}",
            json!(device_id)
        ),
    );
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["device"]["commissions"], json!("retune"), "{v}");
    assert_eq!(v["device"]["id"], json!(device_id), "{v}");
    assert_eq!(v["scan"]["device_id"], json!(device_id), "{v}");
    assert_eq!(v["scan"]["state"], json!("running"), "{v}");
    let (st, v) = post(
        addr,
        "/api/control/scan/stop",
        &format!("{{\"device_id\":{}}}", json!(device_id)),
    );
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["scan"]["state"], json!("idle"), "{v}");
    drop(serving);
}

/// **T-511 — the device selector on the wire, against a real server.**
///
/// The serving layer holds N live controls keyed by `device_id`, and the routes that reach a radio
/// take a `device_id` selector. This run holds exactly one front end (the mock SDR), which is the
/// case the invariant protects: **with one device the selector may be omitted and behaviour is
/// unchanged.** Asserted here rather than in a unit test because "the client's existing bodies
/// still work" is a statement about the wire.
///
/// Four things, all by value:
///
/// 1. `GET /api/control/state` enumerates the front ends in `devices`, and with one it agrees with
///    the singular `device`/`tuning` — so a client can discover the selector it would pass;
/// 2. the ids in `devices` are the same ids `GET /api/navigation`'s `windows` lights segments
///    from: one enumeration of the run's radios, not two;
/// 3. a device route with **no** selector still retunes, and answers `device.id` naming the radio
///    it moved;
/// 4. a selector naming another radio is refused `404 unknown_device` and **the front end does not
///    move** — a wrong name is not "the only device, so they must have meant it".
#[test]
fn t511_a_device_route_takes_a_device_selector_and_one_device_may_omit_it() {
    let (_dir_guard, serving, addr) = start_server();

    // (1) the enumeration, and its agreement with the singular default.
    let (st, v) = get(addr, "/api/control/state");
    assert_eq!(st, 200, "{v}");
    let devices = v["devices"]
        .as_array()
        .unwrap_or_else(|| panic!("control/state must enumerate its front ends: {v}"));
    assert_eq!(devices.len(), 1, "this run holds one front end: {v}");
    let device_id = devices[0]["device_id"]
        .as_str()
        .unwrap_or_else(|| panic!("the live source must report its device_id: {v}"))
        .to_owned();
    assert!(device_id.starts_with("mock:"), "{v}");
    assert_eq!(devices[0]["device_id"], v["device"]["device_id"], "{v}");
    assert_eq!(devices[0]["tuning"], v["tuning"], "{v}");

    // (2) the same radio, seen as a capture window.
    let (st, nav) = get(addr, "/api/navigation");
    assert_eq!(st, 200, "{nav}");
    let windows = nav["windows"].as_array().expect("windows");
    assert_eq!(windows.len(), 1, "{nav}");
    assert_eq!(windows[0]["device_id"], json!(device_id), "{nav}");

    // (3) no selector: unchanged, and the answer names the radio that moved.
    let step = hk_core::source::HACKRF_ONE_TUNING_STEP_HZ;
    let moved = step * (((FIXTURE_CENTER_HZ + 2e6) / step).round());
    let (st, r) = post(
        addr,
        "/api/control/center",
        &format!("{{\"center_hz\":{moved:?}}}"),
    );
    assert_eq!(
        st, 200,
        "an omitted selector is correct with one device: {r}"
    );
    assert_eq!(r["device"]["id"], json!(device_id), "{r}");
    let after_default = r["tuning"]["center_hz"].as_f64().expect("center_hz");

    // The selector given explicitly names the same radio and is accepted.
    let moved2 = step * (((FIXTURE_CENTER_HZ + 3e6) / step).round());
    let (st, r) = post(
        addr,
        "/api/control/center",
        &format!(
            "{{\"center_hz\":{moved2:?},\"device_id\":{}}}",
            json!(device_id)
        ),
    );
    assert_eq!(st, 200, "{r}");
    assert_eq!(r["device"]["id"], json!(device_id), "{r}");
    let after_named = r["tuning"]["center_hz"].as_f64().expect("center_hz");
    assert!(
        (after_named - after_default).abs() > 1.0,
        "the named selector moved the radio: {r}"
    );

    // (4) a selector naming a radio this run does not hold: refused, and nothing moved.
    let (st, r) = post(
        addr,
        "/api/control/center",
        &format!(
            "{{\"center_hz\":{:?},\"device_id\":\"mock:not-this-radio\"}}",
            FIXTURE_CENTER_HZ
        ),
    );
    assert_eq!(st, 404, "{r}");
    assert_eq!(r["code"], json!("unknown_device"), "{r}");
    assert!(
        r["error"].as_str().unwrap_or_default().contains(&device_id),
        "the refusal names what this run does hold: {r}"
    );
    let (_, v) = get(addr, "/api/control/state");
    assert_eq!(
        v["tuning"]["center_hz"].as_f64(),
        Some(after_named),
        "a refused selector must not move the front end: {v}"
    );

    stop_server(serving);
}

/// T-823 (MAP-23, RESEARCH-003): `/api/research/export` as `docs/api.md` documents it — one bundle
/// holding the collections, markers, annotations and measurements as their own routes serve them,
/// a SigMF-adjacent annotation block with the authored marker, a per-collection filter, and the
/// usual errors.
#[test]
fn research_export_bundles_the_durable_objects_as_documented() {
    let (_dir_guard, _serving, addr) = start_server();
    let f = FIXTURE_CENTER_HZ;
    let view = json!({"center_hz": f, "span_hz": 2.4e6, "t_capture": [990.0, 1010.0], "tier": "spectrum-history"});
    let (st, c) = post(
        addr,
        "/api/collections",
        &json!({"name": "t823-export"}).to_string(),
    );
    assert_eq!(st, 201, "{c}");
    let cid = c["id"].as_str().unwrap().to_owned();
    let (st, a) = post(
        addr,
        "/api/annotations",
        &json!({
            "kind": "box", "f_lo_hz": f - 1e5, "f_hi_hz": f + 1e5, "t0_s": 1000.0, "t1_s": 1002.0,
            "label": "t823-note", "body": "off raster", "collection_id": cid, "view": view,
        })
        .to_string(),
    );
    assert_eq!(st, 201, "{a}");
    let (st, _) = post(
        addr,
        "/api/annotations",
        &json!({
            "kind": "text", "f_lo_hz": f, "f_hi_hz": f, "t0_s": 1005.0, "t1_s": 1005.0,
            "label": "t823-unfiled", "view": view,
        })
        .to_string(),
    );
    assert_eq!(st, 201);
    let (st, m) = post(addr, "/api/measurements", &json!({
        "kind": "bandwidth", "collection_id": cid,
        "cursors": [{"f_hz": f - 1e5, "t_s": 1000.0}, {"f_hz": f + 1e5, "t_s": 1001.0}], "view": view,
    }).to_string());
    assert_eq!(st, 201, "{m}");
    let (st, mk) = post(
        addr,
        &format!("/api/collections/{cid}/markers"),
        &json!({"name": "t823-marker", "f_center_hz": f, "view": view}).to_string(),
    );
    assert_eq!(st, 201, "{mk}");

    let (st, all) = get(addr, "/api/research/export");
    assert_eq!(st, 200, "{all}");
    assert_eq!(all["format"], "hackriff-research-export@1");
    assert_eq!(all["truncated"], false);
    assert_eq!(all["counts"]["annotations"], 2);
    assert_eq!(all["counts"]["measurements"], 1);
    assert_eq!(all["counts"]["markers"], 1);
    assert!(
        all["collections"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["id"] == cid.as_str())
    );
    // The objects are the routes' own JSON, carrying their server-stamped provenance.
    assert_eq!(all["measurements"][0]["id"], m["id"]);
    assert_eq!(all["measurements"][0]["value"], m["value"]);
    assert_eq!(all["annotations"].as_array().unwrap().len(), 2);
    // SigMF-adjacent: a standard annotation with the authored block, anchored at the earliest start.
    let sg = &all["sigmf"];
    assert_eq!(sg["global"]["core:sample_rate"].as_f64(), Some(1e6));
    assert_eq!(sg["recording_start_s"].as_f64(), Some(1000.0));
    let ann = sg["annotations"].as_array().unwrap();
    assert_eq!(ann.len(), 2);
    let boxed = ann
        .iter()
        .find(|x| x["core:label"] == "t823-note")
        .expect("the box note");
    assert_eq!(boxed["core:sample_start"], 0);
    assert_eq!(boxed["core:sample_count"], 2_000_000);
    assert_eq!(boxed["core:comment"], "off raster");
    assert_eq!(boxed["hackriff:annotation"]["authored"], true);
    assert!(boxed.get("hackriff:truth").is_none());

    // One collection: only its own members.
    let (st, one) = get(
        addr,
        &format!("/api/research/export?collection={cid}&rate=2e6"),
    );
    assert_eq!(st, 200, "{one}");
    assert_eq!(one["counts"]["annotations"], 1);
    assert_eq!(one["collections"].as_array().unwrap().len(), 1);
    assert_eq!(
        one["sigmf"]["annotations"][0]["core:sample_count"],
        4_000_000
    );

    for bad in ["collection=nope", "rate=0", "rate=x"] {
        let (st, e) = get(addr, &format!("/api/research/export?{bad}"));
        assert_eq!(
            (st, e["code"].as_str()),
            (400, Some("invalid")),
            "{bad}: {e}"
        );
    }
    let (st, e) = get(
        addr,
        "/api/research/export?collection=00000000-0000-7000-8000-00000000dead",
    );
    assert_eq!((st, e["code"].as_str()), (404, Some("not_found")), "{e}");
}

/// T-818 (MAP-18, RESEARCH-003): `/api/measurements` as `docs/api.md` documents it — the
/// `Measurement` shape with its server-computed value/unit/place and server-stamped provenance,
/// cursors in and never a value, the durable-but-paged list, re-measure on PUT, delete, and that a
/// saved measurement never becomes an inventory row (user metadata, never detection input).
#[test]
fn measurements_crud_and_paging_answer_as_documented() {
    let (_dir_guard, serving, addr) = start_server();
    let f = FIXTURE_CENTER_HZ;
    let view = json!({"center_hz": f, "span_hz": 2.4e6, "t_capture": [990.0, 1010.0], "tier": "spectrum-history"});
    let body = json!({
        "kind": "bandwidth",
        "cursors": [{"f_hz": f - 1.0e5, "t_s": 1000.0}, {"f_hz": f + 1.0e5, "t_s": 1001.0}],
        "note": "t818-contract-measurement",
        "view": view,
    });
    let (st, m) = post(addr, "/api/measurements", &body.to_string());
    assert_eq!(st, 201, "{m}");
    for field in [
        "id",
        "collection_id",
        "kind",
        "value",
        "unit",
        "basis",
        "f_lo_hz",
        "f_hi_hz",
        "t0_s",
        "t1_s",
        "cursors",
        "n",
        "note",
        "provenance",
        "created_s",
        "updated_s",
    ] {
        assert!(m.get(field).is_some(), "measurement missing {field}: {m}");
    }
    for field in [
        "device_id",
        "center_hz",
        "span_hz",
        "sample_rate_hz",
        "t_capture",
        "tier",
        "authored_s",
        "actor",
        "authored",
    ] {
        assert!(
            m["provenance"].get(field).is_some(),
            "provenance missing {field}: {m}"
        );
    }
    // The value is the server's: 200 kHz between the cursors, in Hz, on a cursor basis.
    assert!((m["value"].as_f64().unwrap() - 2.0e5).abs() < 1e-3, "{m}");
    assert_eq!(
        (m["unit"].as_str(), m["basis"].as_str()),
        (Some("Hz"), Some("cursors"))
    );
    assert_eq!(
        (m["t0_s"].as_f64(), m["t1_s"].as_f64()),
        (Some(1000.0), Some(1001.0))
    );
    assert_eq!(m["provenance"]["authored"], true);
    assert!(
        m["provenance"]["actor"]
            .as_str()
            .is_some_and(|s| s.starts_with("tok-")),
        "{m}"
    );
    assert!(
        !m.to_string().contains(TOKEN),
        "the token itself is never stored"
    );
    let id = m["id"].as_str().unwrap().to_owned();

    // Cursors in, never a value.
    let mut forged = body.clone();
    forged["value"] = json!(12_500.0);
    let (st, v) = post(addr, "/api/measurements", &forged.to_string());
    assert_eq!((st, v["code"].as_str()), (400, Some("invalid")), "{v}");
    let mut forged = body.clone();
    forged["provenance"] = json!({"authored_s": 0});
    let (st, v) = post(addr, "/api/measurements", &forged.to_string());
    assert_eq!((st, v["code"].as_str()), (400, Some("invalid")), "{v}");

    let (st, list) = get(addr, "/api/measurements");
    assert_eq!(st, 200, "{list}");
    for field in [
        "window",
        "collection",
        "measurements",
        "count",
        "matched",
        "limit",
        "next_cursor",
    ] {
        assert!(list.get(field).is_some(), "list missing {field}: {list}");
    }
    assert_eq!(
        (list["count"].as_u64(), list["matched"].as_u64()),
        (Some(1), Some(1))
    );
    assert_eq!(list["limit"], 500, "documented default page");
    assert!(list["next_cursor"].is_null() && list["window"].is_null());
    let (st, v) = get(addr, "/api/measurements?f_lo=1");
    assert_eq!(
        (st, v["code"].as_str()),
        (400, Some("invalid")),
        "a partial window: {v}"
    );

    let path = format!("/api/measurements/{id}");
    let (st, got) = get(addr, &path);
    assert_eq!(
        (st, got["note"].as_str()),
        (200, Some("t818-contract-measurement"))
    );
    let moved = json!({"cursors": [{"f_hz": f - 1.0e5, "t_s": 1000.0}, {"f_hz": f + 1.5e5, "t_s": 1001.0}]});
    let (st, upd) = put(addr, &path, &moved.to_string());
    assert_eq!(st, 200, "{upd}");
    assert!(
        (upd["value"].as_f64().unwrap() - 2.5e5).abs() < 1e-3,
        "re-measured: {upd}"
    );

    // User metadata only: no inventory row names it.
    let (st, inv) = get(addr, "/api/inventory");
    assert_eq!(st, 200, "{inv}");
    assert!(
        !inv.to_string().contains("t818-contract-measurement"),
        "a measurement never reaches the inventory: {inv}"
    );

    let (st, del) = delete(addr, &path);
    assert_eq!(
        (st, del["deleted"]["id"].as_str()),
        (200, Some(id.as_str()))
    );
    let (st, _) = get(addr, &path);
    assert_eq!(st, 404);
    stop_server(serving);
}

/// T-819 (MAP-19, AWARE-042): `/api/views` as `docs/api.md` documents it — the `SavedView` shape
/// (a named point in view-arithmetic state) with its server-stamped provenance and `share`
/// re-creating body, the two time-extent shapes, the durable-but-paged list, edit, delete, the
/// share round trip, and that a saved view reaches no device and never becomes an inventory row.
#[test]
fn saved_views_crud_share_and_paging_answer_as_documented() {
    let (_dir_guard, serving, addr) = start_server();
    let f = FIXTURE_CENTER_HZ;
    let view = json!({"center_hz": f, "span_hz": 2.4e6, "t_capture": [990.0, 1010.0], "tier": "spectrum-history"});
    let (_, before) = get(addr, "/api/control/state");
    let body = json!({
        "name": "t819-contract-view",
        "center_f_hz": f,
        "span_f_hz": 1.2e6,
        "center_t_s": 1000.0,
        "span_t_s": 20.0,
        "follow_live": false,
        "pane_layout": {"panes": [{"center_f_hz": f, "span_f_hz": 1.2e6}]},
        "view": view,
    });
    let (st, v) = post(addr, "/api/views", &body.to_string());
    assert_eq!(st, 201, "{v}");
    for field in [
        "id",
        "name",
        "note",
        "center_f_hz",
        "span_f_hz",
        "center_t_s",
        "span_t_s",
        "follow_live",
        "pane_layout",
        "provenance",
        "created_s",
        "updated_s",
        "share",
    ] {
        assert!(v.get(field).is_some(), "saved view missing {field}: {v}");
    }
    for field in [
        "device_id",
        "center_hz",
        "span_hz",
        "sample_rate_hz",
        "t_capture",
        "tier",
        "authored_s",
        "actor",
        "authored",
    ] {
        assert!(
            v["provenance"].get(field).is_some(),
            "provenance missing {field}: {v}"
        );
    }
    assert_eq!(
        (v["center_t_s"].as_f64(), v["span_t_s"].as_f64()),
        (Some(1000.0), Some(20.0))
    );
    assert_eq!(v["provenance"]["authored"], true);
    assert!(
        !v.to_string().contains(TOKEN),
        "the token itself is never stored"
    );
    let id = v["id"].as_str().unwrap().to_owned();
    // `share` is exactly the create body minus `view`.
    let mut expect = body.clone();
    expect.as_object_mut().unwrap().remove("view");
    expect["id"] = json!(id);
    expect["note"] = Value::Null;
    assert_eq!(v["share"], expect, "{v}");

    // A frozen view without its window, and a supplied provenance, are refused.
    let mut bad = body.clone();
    bad.as_object_mut().unwrap().remove("center_t_s");
    let (st, e) = post(addr, "/api/views", &bad.to_string());
    assert_eq!((st, e["code"].as_str()), (400, Some("invalid")), "{e}");
    let mut forged = body.clone();
    forged["provenance"] = json!({"authored_s": 0});
    let (st, e) = post(addr, "/api/views", &forged.to_string());
    assert_eq!((st, e["code"].as_str()), (400, Some("invalid")), "{e}");

    let (st, list) = get(addr, "/api/views");
    assert_eq!(st, 200, "{list}");
    for field in [
        "window",
        "views",
        "count",
        "matched",
        "limit",
        "next_cursor",
    ] {
        assert!(list.get(field).is_some(), "list missing {field}: {list}");
    }
    assert_eq!(
        (list["count"].as_u64(), list["matched"].as_u64()),
        (Some(1), Some(1))
    );
    assert_eq!(list["limit"], 500, "documented default page");
    let (st, e) = get(addr, "/api/views?f_lo=1");
    assert_eq!((st, e["code"].as_str()), (400, Some("invalid")), "{e}");

    let path = format!("/api/views/{id}");
    let (st, upd) = put(
        addr,
        &path,
        &json!({"follow_live": true, "center_t_s": null, "note": "now live"}).to_string(),
    );
    assert_eq!(st, 200, "{upd}");
    assert_eq!(
        (upd["follow_live"].as_bool(), upd["note"].as_str()),
        (Some(true), Some("now live"))
    );
    assert!(upd["center_t_s"].is_null());

    // User metadata only: no inventory row names it, and the device was not moved.
    let (_, inv) = get(addr, "/api/inventory");
    assert!(!inv.to_string().contains("t819-contract-view"), "{inv}");
    let (_, after) = get(addr, "/api/control/state");
    assert_eq!(
        before["tuning"]["center_hz"], after["tuning"]["center_hz"],
        "saving a view never retunes"
    );
    assert!(before["tuning"]["center_hz"].is_number(), "{before}");

    // Share round trip: delete, re-create from `share` + a view, same state and id.
    let (_, got) = get(addr, &path);
    let share = got["share"].clone();
    let (st, del) = delete(addr, &path);
    assert_eq!(
        (st, del["deleted"]["id"].as_str()),
        (200, Some(id.as_str()))
    );
    let (st, _) = get(addr, &path);
    assert_eq!(st, 404);
    let mut again = share.clone();
    again["view"] = view.clone();
    let (st, back) = post(addr, "/api/views", &again.to_string());
    assert_eq!(st, 201, "{back}");
    assert_eq!(back["share"], share);
    stop_server(serving);
}

// T-891 VLF accessory

/// T-891 (SPACE-001, SPACE-041, PROP-019): `hk serve --device mock:<radio> --device
/// vlf-mock:<recording>` attaches the mock VLF receiver **beside** the radio, and `/api/vlf`
/// answers the documented shape for it — the accessory's provenance on every report — while the
/// radio's routes are untouched. The science itself is asserted blind in
/// `crates/hk-pipeline/tests/vlf_accessory.rs`; this pins the wire.
#[test]
fn vlf_accessory_route_answers_documented_shape_through_serve() {
    let fs = 48_000.0;
    let rec_dir = temp_data_dir();
    let _rec_guard = TempDataDirGuard::new(rec_dir.clone());
    std::fs::create_dir_all(&rec_dir).unwrap();
    let meta = rec_dir.join("vlf.sigmf-meta");
    let x: Vec<f32> = (0..(fs as usize * 3))
        .map(|i| 0.01 * (2.0 * std::f64::consts::PI * 19_800.0 * i as f64 / fs).cos() as f32)
        .collect();
    hk_core::source::write_real_sigmf(&meta, &x, fs, "2026-09-25T12:00:00Z", "t891", None).unwrap();

    let dir = temp_data_dir();
    let guard = TempDataDirGuard::new(dir.clone());
    let serving = start(&ServeOptions {
        source: ServeSource::HackRf {
            spec: format!("mock:{}", fixture_path().display()),
            extra: vec![(format!("vlf-mock:{}", meta.display()), LiveArgs::default())],
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
        iq_buffer: hk_cli::pipeline::IqBufferArgs {
            retention_s: None,
            max_bytes: Some(64 << 20),
        },
        iq_buffer_hooks: None,
    })
    .unwrap();
    let addr = serving.server.local_addr();

    // The accessory runs in real time; wait (bounded) for its first block.
    let deadline = Instant::now() + Duration::from_secs(30);
    let v = loop {
        let (st, v) = get(addr, "/api/vlf");
        assert_eq!(st, 200, "{v}");
        if v["accessories"][0]["state"] != "waiting" || Instant::now() > deadline {
            break v;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(is_array(&v["accessories"]), "{v}");
    assert_eq!(v["accessories"].as_array().unwrap().len(), 1, "{v}");
    let r = &v["accessories"][0];
    for field in [
        "state",
        "device_id",
        "accessory",
        "provenance_ref",
        "provenance",
        "sample_rate_hz",
        "phase_disciplined",
        "window",
        "samples",
        "gaps",
        "dropped_samples",
        "carriers",
        "sferics",
        "sferic_total",
    ] {
        assert!(r.get(field).is_some(), "missing {field}: {r}");
    }
    assert!(
        ["discovering", "tracking", "finished"].contains(&r["state"].as_str().unwrap()),
        "{r}"
    );
    assert_eq!(r["accessory"], "vlf-receiver");
    let device = r["device_id"].as_str().unwrap().to_string();
    assert!(device.starts_with("vlf-receiver:"), "{r}");
    assert_eq!(r["provenance"]["device_id"], device.as_str());
    assert_eq!(r["provenance"]["antenna_port"], "accessory:vlf-receiver");
    assert_eq!(r["provenance"]["tune"]["center_hz"], 0.0);
    assert_eq!(r["sample_rate_hz"], fs);
    assert!(is_array(&r["carriers"]) && is_array(&r["sferics"]), "{r}");
    assert!(r["window"]["start_ns"].is_i64(), "{r}");

    let (st, v) = get(addr, &format!("/api/vlf?device={device}&points=1"));
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["accessories"][0]["device_id"], device.as_str());
    let (st, _) = get(addr, "/api/vlf?device=hackrf:nope");
    assert_eq!(st, 404);
    let (st, _) = get(addr, "/api/vlf?points=2");
    assert_eq!(st, 400);
    let auth = format!("Bearer {TOKEN}");
    let (st, _) = call(addr, "POST", "/api/vlf", Some(&auth), Some("{}"));
    assert_eq!(st, 405);
    let (st, _) = call(addr, "GET", "/api/vlf", None, None);
    assert_eq!(st, 401);

    stop_server(serving);
    drop(guard);
}
