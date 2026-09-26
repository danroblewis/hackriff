//! T-164 (ADR-0013 gap 7b) `GET /api/recipes/match?emitter=`: the recipes the store holds, ranked
//! against one emitter's **measured** parameters, on the shared `tests/support/seed_inventory.rs`
//! repository and the recipes the repository actually ships.
//!
//! The ranking arithmetic is unit-tested in `hk-recipe` (`tests/recipe_match.rs`); what these
//! tests pin is the route: that it reads real measurements out of the repository, that a
//! measurement appearing (T-163's `estimated_params`) changes the answer, that an emitter nothing
//! has characterised gets no forced match, and the error and auth behaviour.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hk_api::recipes::{RecipeCall, RecipeControl, RecipeFail};
use hk_api::{ApiState, Server, ServerConfig, Token};
use hk_model::{
    Demodulation, DemodulationId, EmitterId, EstimatedParams, Repository, TimeRange, Timestamp,
};
use serde_json::{Value, json};

#[allow(dead_code)]
#[path = "support/seed_inventory.rs"]
mod seed;

const TOKEN: &str = "t164-recipes-match-token-0123456789abcd0";
const T0: i64 = seed::DEFAULT_T0_S;

/// The recipes the repository ships, served exactly as `GET /api/recipes` lists them (id, name,
/// version and the `match` block). Using the real documents means a change to a shipped recipe's
/// expectations is caught here.
struct Builtins(Vec<Value>);

impl Builtins {
    fn load() -> Self {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../recipes");
        let rows = ["rds", "acars", "adsb", "pocsag"]
            .iter()
            .map(|id| {
                let path = dir.join(format!("{id}.recipe.json"));
                let text = std::fs::read_to_string(&path)
                    .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
                let doc: Value = serde_json::from_str(&text).unwrap();
                json!({
                    "id": doc["id"],
                    "name": doc["name"],
                    "version": doc["version"],
                    "match": doc["match"],
                })
            })
            .collect();
        Self(rows)
    }
}

impl RecipeControl for Builtins {
    fn call(&self, call: RecipeCall) -> Result<Value, RecipeFail> {
        match call {
            RecipeCall::ListRecipes => Ok(json!({ "recipes": self.0 })),
            _ => Err(RecipeFail {
                status: 500,
                code: "failed",
                message: "this test only lists recipes".into(),
                detail: Value::Null,
            }),
        }
    }
}

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
        recipes: Some(Arc::new(Builtins::load())),
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
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

fn matches_for(addr: SocketAddr, emitter: EmitterId) -> Value {
    let (status, v) = authed(addr, &format!("/api/recipes/match?emitter={emitter}"));
    assert_eq!(status, 200, "{v}");
    v
}

/// The candidate with this id, or `None` when it was not offered.
fn candidate<'a>(v: &'a Value, id: &str) -> Option<&'a Value> {
    v["recipes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == json!(id))
}

fn reason<'a>(candidate: &'a Value, field: &str) -> &'a Value {
    candidate["reasons"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["field"] == json!(field))
        .unwrap_or_else(|| panic!("no reason for {field}: {candidate}"))
}

fn ruled_reason(v: &Value, id: &str) -> String {
    v["ruled_out"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == json!(id))
        .unwrap_or_else(|| panic!("{id} was neither offered nor ruled out: {v}"))["reason"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// The seeded broadcast-FM emitter (`wfm`, 180 kHz wide) ranks the RDS recipe first, and the
/// answer says which measurements put it there. The other shipped recipes decode other
/// modulations, so they are ruled out on the measured family rather than merely ranked lower.
#[test]
fn the_fm_emitter_ranks_the_rds_recipe_first_and_cites_the_measurements() {
    let (server, seeded, _repo) = serve_seeded();
    let v = matches_for(server.local_addr(), seeded.rds);

    let best = &v["recipes"][0];
    assert_eq!(best["id"], json!("rds"), "{v}");
    assert!(
        best["score"].as_f64().unwrap() >= 0.6,
        "the measurements should back it strongly: {best}"
    );
    assert_eq!(reason(best, "family")["verdict"], json!("agree"), "{best}");
    assert_eq!(
        reason(best, "bandwidth_hz")["verdict"],
        json!("agree"),
        "{best}"
    );

    // The echo shows what the ranking actually read.
    assert_eq!(v["measured"]["family"], json!("wfm"), "{v}");
    assert_eq!(v["measured"]["bandwidth_hz"], json!(180e3), "{v}");
    assert_eq!(
        v["measured"]["family_source"],
        json!("classification"),
        "{v}"
    );

    for id in ["acars", "adsb", "pocsag"] {
        assert_eq!(ruled_reason(&v, id), "family_conflict", "{id}: {v}");
    }
    drop(server);
}

/// Nothing has demodulated the station yet, so the RDS recipe's `pilot-19k` expectation reads as
/// unmeasured and earns nothing. Store a demodulation session that actually measured the pilot
/// (T-163's `estimated_params`) and the same expectation becomes agreement, and the score rises.
/// This is the wiring from measurement to ranking, and the reason absence must never be scored as
/// agreement: otherwise these two answers would be identical.
#[test]
fn a_measured_pilot_turns_an_unmeasured_expectation_into_agreement() {
    let (server, seeded, repo) = serve_seeded();
    let addr = server.local_addr();

    let before = matches_for(addr, seeded.rds);
    let rds_before = candidate(&before, "rds").expect("rds offered");
    assert_eq!(
        reason(rds_before, "feature:pilot-19k")["verdict"],
        json!("unmeasured"),
        "{rds_before}"
    );
    assert_eq!(
        reason(rds_before, "feature:pilot-19k")["earned"],
        json!(0.0)
    );

    repo.lock()
        .unwrap()
        .insert_demodulation(&Demodulation {
            id: DemodulationId::new(),
            emitter_ref: Some(seeded.rds),
            detection_ref: None,
            recording_ref: None,
            mode: "wfm".to_owned(),
            params: EstimatedParams {
                symbol_rate_hz: None,
                deviation_hz: Some(74_812.5),
                cfo_hz: Some(-118.25),
                mod_order: None,
                roll_off: None,
                bandwidth_hz: Some(178_500.0),
                // Measured on the receiver clock, so never exactly 19 kHz.
                pilot_hz: Some(19_000.4),
                subaudible: None,
            },
            lock_quality: Some(0.98),
            evm_db: None,
            time: TimeRange::new(ts(T0 + 1), ts(T0 + 4)),
            demod_version: "hk-demod/test@0".to_owned(),
        })
        .unwrap();

    let after = matches_for(addr, seeded.rds);
    let rds_after = candidate(&after, "rds").expect("rds still offered");
    assert_eq!(
        reason(rds_after, "feature:pilot-19k")["verdict"],
        json!("agree"),
        "{rds_after}"
    );
    assert!(
        rds_after["score"].as_f64().unwrap() > rds_before["score"].as_f64().unwrap(),
        "a measured pilot must raise the score: {} -> {}",
        rds_before["score"],
        rds_after["score"]
    );
    // The bandwidth now comes from the session rather than the detected extent.
    assert_eq!(after["measured"]["bandwidth_source"], json!("demodulation"));
    assert_eq!(after["measured"]["bandwidth_hz"], json!(178_500.0));
    drop(server);
}

/// A bare 145.8 MHz carrier, 3 kHz wide, with no family classified and nothing demodulated: the
/// honest answer is that there is nothing to offer. The ACARS recipe's declared 2–8 kHz bandwidth
/// contains this carrier's one measurement, and it is still not offered.
#[test]
fn an_uncharacterised_carrier_gets_no_forced_match() {
    let (server, seeded, _repo) = serve_seeded();
    let v = matches_for(server.local_addr(), seeded.carrier);

    assert_eq!(v["recipes"].as_array().unwrap().len(), 0, "{v}");
    assert_eq!(v["outcome"], json!("none"), "{v}");
    let reasons: Vec<&str> = v["reasons"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(reasons.contains(&"no_candidate"), "{v}");
    assert_eq!(ruled_reason(&v, "acars"), "too_few_compared", "{v}");
    assert!(v["measured"]["family"].is_null(), "{v}");
    drop(server);
}

/// `emitter` is required; an unparsable or unknown id is a 404 rather than a 200 that could be
/// probed for which ids exist; and the token is required like everywhere else.
#[test]
fn the_route_refuses_a_missing_unknown_or_unauthenticated_request() {
    let (server, seeded, _repo) = serve_seeded();
    let addr = server.local_addr();

    let (status, v) = authed(addr, "/api/recipes/match");
    assert_eq!(status, 400, "{v}");
    assert_eq!(v["code"], json!("invalid"), "{v}");

    let (status, v) = authed(addr, "/api/recipes/match?emitter=not-an-id");
    assert_eq!(status, 404, "{v}");
    assert_eq!(v["code"], json!("not_found"), "{v}");

    let unknown = EmitterId::new();
    let (status, v) = authed(addr, &format!("/api/recipes/match?emitter={unknown}"));
    assert_eq!(status, 404, "{v}");

    let (status, _) = get(
        addr,
        &format!("/api/recipes/match?emitter={}", seeded.rds),
        None,
    );
    assert_eq!(status, 401);
    drop(server);
}
