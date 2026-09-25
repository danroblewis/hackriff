//! T-163 (ADR-0013 gap 7a) `estimated_params` on `GET /api/inventory/{id}`: the emitter's latest
//! blind-estimated parameters (C13/C14), on the shared `tests/support/seed_inventory.rs`
//! repository — SIGNAL-062's RDS emitter (`rds`, identity in clear), the anonymous `carrier`
//! (no identity), and the withheld-identity `pager` (restricted-paging class, T-036).

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hk_api::query::inventory_entry_json;
use hk_api::{ApiState, Server, ServerConfig, Token};
use hk_model::{
    ContentClass, Demodulation, DemodulationId, EmitterId, EstimatedParams, IdentityAccess,
    IdentityScheme, InventoryIdentity, Repository, TimeRange, Timestamp,
};
use serde_json::{Value, json};

#[allow(dead_code)]
#[path = "support/seed_inventory.rs"]
mod seed;

const TOKEN: &str = "t163-estimated-params-token-0123456789ab";
const T0: i64 = seed::DEFAULT_T0_S;

fn ts(s: i64) -> Timestamp {
    Timestamp::from_unix_nanos(s * 1_000_000_000)
}

fn serve_seeded() -> (Server, seed::Seeded, Arc<Mutex<Repository>>) {
    let mut repo = Repository::open_in_memory().unwrap();
    let seeded = seed::seed(&mut repo, T0).unwrap();
    let repo = Arc::new(Mutex::new(repo));
    let config = ServerConfig::new(
        "127.0.0.1:0".parse().unwrap(),
        Token::from_config(TOKEN).unwrap(),
    );
    let state = ApiState {
        inventory: Some(Arc::clone(&repo)),
        ..ApiState::default()
    };
    (Server::start(config, state).unwrap(), seeded, repo)
}

fn get(addr: SocketAddr, path: &str, authorization: Option<&str>) -> (u16, Vec<u8>) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let auth = authorization.map_or(String::new(), |a| format!("Authorization: {a}\r\n"));
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: test\r\n{auth}Connection: close\r\n\r\n"
    )
    .unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&raw[..split]);
    let status = head.split(' ').nth(1).and_then(|c| c.parse().ok()).unwrap();
    (status, raw[split + 4..].to_vec())
}

fn authed(addr: SocketAddr, path: &str) -> (u16, Value) {
    let (status, body) = get(addr, path, Some(&format!("Bearer {TOKEN}")));
    (status, serde_json::from_slice(&body).unwrap())
}

/// A demodulation session on `emitter`, ending at `t_end_s`, with distinctive (never
/// accidentally-default) `params`.
fn session(emitter: EmitterId, mode: &str, t_end_s: i64, params: EstimatedParams) -> Demodulation {
    Demodulation {
        id: DemodulationId::new(),
        emitter_ref: Some(emitter),
        detection_ref: None,
        recording_ref: None,
        mode: mode.to_owned(),
        params,
        lock_quality: Some(0.97),
        evm_db: None,
        time: TimeRange::new(ts(t_end_s - 1), ts(t_end_s)),
        demod_version: "hk-demod/test@0".to_owned(),
    }
}

/// Distinctive, non-round values a route computing a default or fabricating a number would be
/// very unlikely to reproduce by accident. `symbol_rate_hz` and `mod_order` are left unmeasured
/// (as a WFM broadcast station's would be), to prove those read `null`, not `0`/`0.0`.
fn measured_2fsk_params() -> EstimatedParams {
    EstimatedParams {
        symbol_rate_hz: Some(9587.25),
        deviation_hz: Some(2431.75),
        cfo_hz: Some(-317.5),
        mod_order: Some(2),
        roll_off: None,
        bandwidth_hz: Some(19174.5),
        pilot_hz: None,
    }
}

#[test]
fn estimated_params_is_null_then_serves_the_latest_measured_session() {
    let (server, seeded, repo) = serve_seeded();
    let addr = server.local_addr();

    // Before any Demodulation is stored: `null`, not a default or fabricated object.
    let (status, v) = authed(addr, &format!("/api/inventory/{}", seeded.carrier));
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["estimated_params"], Value::Null, "{v}");

    let first = measured_2fsk_params();
    {
        let mut repo = repo.lock().unwrap();
        repo.insert_demodulation(&session(seeded.carrier, "2fsk", T0 + 1, first.clone()))
            .unwrap();
    }

    let (status, v) = authed(addr, &format!("/api/inventory/{}", seeded.carrier));
    assert_eq!(status, 200, "{v}");
    let p = &v["estimated_params"];
    assert_eq!(p["modulation"], json!("2fsk"), "{v}");
    assert_eq!(p["symbol_rate_hz"], json!(9587.25), "{v}");
    assert_eq!(p["deviation_hz"], json!(2431.75), "{v}");
    assert_eq!(p["cfo_hz"], json!(-317.5), "{v}");
    assert_eq!(p["mod_order"], json!(2), "{v}");
    assert_eq!(p["bandwidth_hz"], json!(19174.5), "{v}");
    // Never measured for this signal: `null`, never a fabricated `0`/`0.0`.
    assert_eq!(p["roll_off"], Value::Null, "{v}");
    assert_eq!(p["pilot_hz"], Value::Null, "{v}");
    assert!(p["source_session"].is_string(), "{v}");
    assert!(p["source_recording"].is_null(), "{v}");
    assert_eq!(p["t_s"], json!((T0 + 1) as f64), "{v}");
    // T-953: the session's own extent, from the stored row's `time` — a burst's length where the
    // session is one burst. `session()` spans exactly one second.
    assert_eq!(p["duration_s"], json!(1.0), "{v}");

    // A newer session with different numbers: the route tracks storage, not the first insert.
    let second = EstimatedParams {
        symbol_rate_hz: Some(4813.0),
        ..measured_2fsk_params()
    };
    {
        let mut repo = repo.lock().unwrap();
        repo.insert_demodulation(&session(seeded.carrier, "2fsk", T0 + 8, second))
            .unwrap();
    }
    let (status, v) = authed(addr, &format!("/api/inventory/{}", seeded.carrier));
    assert_eq!(status, 200, "{v}");
    assert_eq!(
        v["estimated_params"]["symbol_rate_hz"],
        json!(4813.0),
        "the newer session wins: {v}"
    );
    assert_ne!(
        v["estimated_params"]["symbol_rate_hz"],
        json!(9587.25),
        "must not still read the first session's value: {v}"
    );
}

/// T-036 identity gating (content-class access control) is off by default and, per
/// `hk_model::content`'s own docs, "deliberately untested": flipping the process-global switch on
/// is outside this codebase's test conventions (`crates/hk-api/tests/decode_api.rs` makes the same
/// choice). So this test exercises the gating decision `inventory_entry_json` actually makes
/// directly: a real [`hk_model::InventoryEntry`] for `pager` (read at `IdentityAccess::Standard`,
/// exactly like the HTTP route does), with only its `identity` field swapped to
/// [`InventoryIdentity::Withheld`] — the shape any row gets when content gating *is* enabled and
/// finds a restricted class. A real, distinctive `Demodulation` sits in the repository for it
/// throughout, so a non-null answer here could only mean the field ignored the withheld identity.
#[test]
fn estimated_params_is_withheld_on_a_withheld_identity_row() {
    let (server, seeded, repo) = serve_seeded();
    let addr = server.local_addr();

    {
        let mut repo = repo.lock().unwrap();
        repo.insert_demodulation(&session(
            seeded.pager,
            "2fsk",
            T0 - 595,
            measured_2fsk_params(),
        ))
        .unwrap();
    }

    let (real_entry, stored) = {
        let repo = repo.lock().unwrap();
        let real_entry = repo
            .emitter_with_access(seeded.pager, IdentityAccess::Standard)
            .unwrap();
        let stored = repo.latest_demodulation_for_emitter(seeded.pager).unwrap();
        (real_entry, stored)
    };
    // Prove the data really is there before gating it: the assertion below is about the gate,
    // not an absent measurement.
    assert_eq!(
        stored.unwrap().params.symbol_rate_hz,
        Some(9587.25),
        "the session was in fact stored, and it is the one just inserted"
    );

    let withheld_entry = hk_model::InventoryEntry {
        identity: InventoryIdentity::Withheld {
            scheme: IdentityScheme::Other("pocsag-capcode".into()),
            class: Some(ContentClass::RestrictedPaging),
        },
        ..real_entry
    };
    let row = {
        let repo = repo.lock().unwrap();
        inventory_entry_json(&repo, &withheld_entry).unwrap()
    };
    assert!(row["identity_value"].is_null(), "sanity: {row}");
    assert_eq!(
        row["estimated_params"],
        Value::Null,
        "a withheld identity must not be confirmed indirectly by this field appearing: {row}"
    );

    // An emitter with no decoded identity at all (`carrier`) is the common case — unknown
    // signals are the priority — and is served normally over HTTP, not withheld.
    {
        let mut repo = repo.lock().unwrap();
        repo.insert_demodulation(&session(
            seeded.carrier,
            "2fsk",
            T0 + 1,
            measured_2fsk_params(),
        ))
        .unwrap();
    }
    let (status, v) = authed(addr, &format!("/api/inventory/{}", seeded.carrier));
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["withheld"], json!(false), "{v}");
    assert!(!v["estimated_params"].is_null(), "{v}");
}

#[test]
fn estimated_params_resolves_a_merged_id_to_its_survivor() {
    let (server, seeded, repo) = serve_seeded();
    let addr = server.local_addr();
    {
        let mut repo = repo.lock().unwrap();
        repo.insert_demodulation(&session(
            seeded.carrier,
            "2fsk",
            T0 + 1,
            measured_2fsk_params(),
        ))
        .unwrap();
    }
    // Merge the identity-free `carrier` row into `rds`; `carrier`'s id must then answer with the
    // survivor's estimated params (the `latest_demodulation_for_emitter` absorbed-emitters CTE).
    let absorbed = {
        let mut repo = repo.lock().unwrap();
        repo.merge_emitters(
            seeded.carrier,
            seeded.rds,
            ts(T0 + 10),
            "same emission (test)",
        )
        .unwrap();
        seeded.carrier
    };
    let (status, v) = authed(addr, &format!("/api/inventory/{absorbed}"));
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["id"], json!(seeded.rds.to_string()), "{v}");
    assert_eq!(
        v["estimated_params"]["symbol_rate_hz"],
        json!(9587.25),
        "{v}"
    );
}
