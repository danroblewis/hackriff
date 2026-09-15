//! T-159 `GET /api/inventory/{id}/decode`: the emitter's latest decode fields, on the shared
//! `tests/support/seed_inventory.rs` repository (SIGNAL-062's RDS emitter, PI `C0DE`, in clear).

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hk_api::{ApiState, Server, ServerConfig, Token};
use hk_model::{ContentClass, CrcStatus, Decode, DecodeId, EmitterId, Repository, Timestamp};
use serde_json::{Value, json};

#[allow(dead_code)]
#[path = "support/seed_inventory.rs"]
mod seed;

const TOKEN: &str = "t159-decode-token-0123456789abcdef";
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

/// A recipe-style RDS decode row (`rds.recipe.json`'s `group-info`/`station` outputs), naming
/// `identity` in the RDS PI scheme.
fn rds_decode(frame_model: &str, t_s: i64, metadata: Value, content: Option<Value>) -> Decode {
    Decode {
        id: DecodeId::new(),
        demodulation_ref: None,
        recording_ref: None,
        decoder_id: "recipe:rds".to_owned(),
        decoder_version: "1".to_owned(),
        frame_model: frame_model.to_owned(),
        metadata,
        content,
        crc_status: CrcStatus::Valid,
        identity: Some(hk_model::DecodedIdentity {
            scheme: hk_model::IdentityScheme::RdsPi,
            value: seed::RDS_PI.to_owned(),
        }),
        content_class: ContentClass::Unrestricted,
        t: ts(t_s),
    }
}

#[test]
fn decode_route_is_empty_then_serves_the_latest_row_per_frame_model() {
    let (server, seeded, repo) = serve_seeded();
    let addr = server.local_addr();

    // Before any Decode row is stored, the route is a documented empty list, not a 404.
    let (status, v) = authed(addr, &format!("/api/inventory/{}/decode", seeded.rds));
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["decodes"], json!([]), "{v}");

    {
        let mut repo = repo.lock().unwrap();
        // An older and a newer `rds-group` row (metadata: group fields); the newer one wins.
        repo.insert_decode(&rds_decode(
            "rds-group",
            T0 + 1,
            json!({"group_type": 0, "tp": false, "pty": 10}),
            None,
        ))
        .unwrap();
        repo.insert_decode(&rds_decode(
            "rds-group",
            T0 + 4,
            json!({"group_type": 0, "tp": false, "pty": 11}),
            None,
        ))
        .unwrap();
        // A `rds-ps` row: PS is carried as content (station name), not metadata.
        repo.insert_decode(&rds_decode(
            "rds-ps",
            T0 + 3,
            json!({}),
            Some(json!({"text": "KROQ FM"})),
        ))
        .unwrap();
    }

    let (status, v) = authed(addr, &format!("/api/inventory/{}/decode", seeded.rds));
    assert_eq!(status, 200, "{v}");
    let decodes = v["decodes"].as_array().unwrap();
    assert_eq!(decodes.len(), 2, "one row per frame_model: {v}");

    // Newest first: rds-group (t = T0+4) before rds-ps (t = T0+3).
    assert_eq!(decodes[0]["frame_model"], json!("rds-group"), "{v}");
    assert_eq!(decodes[0]["decoder"], json!("recipe:rds"), "{v}");
    assert_eq!(decodes[0]["recipe_id"], json!("rds"), "{v}");
    assert_eq!(decodes[0]["at"], json!((T0 + 4) as f64), "{v}");
    assert_eq!(
        decodes[0]["fields"]["pty"],
        json!(11),
        "the newer group row wins: {v}"
    );
    assert_eq!(decodes[0]["crc"], json!({"valid": true}), "{v}");
    assert!(decodes[0]["source_session"].is_null(), "{v}");

    assert_eq!(decodes[1]["frame_model"], json!("rds-ps"), "{v}");
    assert_eq!(decodes[1]["at"], json!((T0 + 3) as f64), "{v}");
    assert_eq!(
        decodes[1]["fields"]["text"],
        json!("KROQ FM"),
        "content merges into fields: {v}"
    );
}

#[test]
fn decode_route_404s_for_an_unknown_id_and_is_empty_without_a_decoded_identity() {
    let (server, seeded, _repo) = serve_seeded();
    let addr = server.local_addr();

    let unknown = EmitterId::new();
    let (status, v) = authed(addr, &format!("/api/inventory/{unknown}/decode"));
    assert_eq!(status, 404, "{v}");
    assert_eq!(v["code"], json!("not_found"), "{v}");

    // `carrier` has no decoded identity at all (an anonymous, unidentified emitter): the route
    // has nothing to look decodes up by, so it answers the documented empty list, not an error.
    // (Content gating — which would also withhold a *known* but restricted identity, T-036 — is
    // off by default and "deliberately untested" (`hk_model::content`); this route's own code
    // only ever calls `decodes_for_identity` from the `InventoryIdentity::Clear` match arm, so a
    // `None`/`Withheld` identity structurally never reaches it, gated or not.)
    let (status, v) = authed(addr, &format!("/api/inventory/{}/decode", seeded.carrier));
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["decodes"], json!([]), "{v}");
}

#[test]
fn decode_route_resolves_a_merged_id_to_its_survivor() {
    let (server, seeded, repo) = serve_seeded();
    let addr = server.local_addr();
    {
        let mut repo = repo.lock().unwrap();
        repo.insert_decode(&rds_decode(
            "rds-group",
            T0 + 1,
            json!({"group_type": 0}),
            None,
        ))
        .unwrap();
    }
    // Merge the identity-free `carrier` row into `rds` (allowed: only one side holds an
    // identity); `carrier`'s id must then answer with the survivor's decodes.
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
    let (status, v) = authed(addr, &format!("/api/inventory/{absorbed}/decode"));
    assert_eq!(status, 200, "{v}");
    let decodes = v["decodes"].as_array().unwrap();
    assert_eq!(decodes.len(), 1, "{v}");
    assert_eq!(decodes[0]["frame_model"], json!("rds-group"), "{v}");
}
