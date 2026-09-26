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
        provenance: None,
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

// ---- T-384: the window, so a scrubbed output panel can ask ----

/// Seeds one emitter with a decode history spread over a minute, so a window can select a *part*
/// of it. Three `rds-group` rows and two `rds-ps` rows, each at a distinct capture second.
fn seed_decode_history(repo: &Arc<Mutex<Repository>>) {
    let mut repo = repo.lock().unwrap();
    for (t, pty) in [(10, 1), (30, 2), (50, 3)] {
        repo.insert_decode(&rds_decode(
            "rds-group",
            T0 + t,
            json!({"group_type": 0, "pty": pty}),
            None,
        ))
        .unwrap();
    }
    for (t, text) in [(20, "EARLY"), (40, "LATE")] {
        repo.insert_decode(&rds_decode(
            "rds-ps",
            T0 + t,
            json!({}),
            Some(json!({"text": text})),
        ))
        .unwrap();
    }
}

/// Rows as `(frame_model, at−T0)` pairs, for terse assertions.
fn rows_of(v: &Value) -> Vec<(String, i64)> {
    v["decodes"]
        .as_array()
        .unwrap_or_else(|| panic!("{v}"))
        .iter()
        .map(|r| {
            (
                r["frame_model"].as_str().unwrap().to_owned(),
                r["at"].as_f64().unwrap().round() as i64 - T0,
            )
        })
        .collect()
}

/// **THE CONTROL THAT MATTERS.** A window that *does* hold decodes serves them, asserted on the
/// values — not merely on the absence of an error, and not merely on emptiness.
///
/// Without this every other assertion in this section is satisfiable by a route that answers
/// `{"decodes": []}` for every window it is given, which is precisely the
/// *we-have-it-but-didn't-render-it* failure the whole-UI window rule is about. The panel above it
/// would then explain its emptiness beautifully and be wrong every time.
#[test]
fn a_window_that_holds_decodes_serves_them_with_their_values() {
    let (server, seeded, repo) = serve_seeded();
    let addr = server.local_addr();
    seed_decode_history(&repo);

    // [T0+15, T0+45] holds rds-group at 30 and rds-ps at 20 and 40.
    let (status, v) = authed(
        addr,
        &format!(
            "/api/inventory/{}/decode?t0={}&t1={}",
            seeded.rds,
            T0 + 15,
            T0 + 45
        ),
    );
    assert_eq!(status, 200, "{v}");
    assert_eq!(
        rows_of(&v),
        vec![("rds-ps".to_owned(), 40), ("rds-group".to_owned(), 30)],
        "the window's own latest row per frame model, newest first: {v}"
    );
    assert_eq!(
        v["decodes"][0]["fields"]["text"],
        json!("LATE"),
        "the value the panel renders comes from the windowed row: {v}"
    );
    assert_eq!(v["decodes"][1]["fields"]["pty"], json!(2), "{v}");
}

/// **THE PROPERTY.** The window selects what was decoded *in it*, on the capture clock — nothing
/// widened to look full, nothing narrowed to the live edge.
#[test]
fn the_window_selects_what_was_decoded_in_it_and_never_widens() {
    let (server, seeded, repo) = serve_seeded();
    let addr = server.local_addr();
    seed_decode_history(&repo);
    let ask = |t0: i64, t1: i64| {
        authed(
            addr,
            &format!("/api/inventory/{}/decode?t0={t0}&t1={t1}", seeded.rds),
        )
        .1
    };

    // Unwindowed is unchanged: the latest of all time (T-159's behaviour, still the default).
    let (status, all) = authed(addr, &format!("/api/inventory/{}/decode", seeded.rds));
    assert_eq!(status, 200, "{all}");
    assert_eq!(
        rows_of(&all),
        vec![("rds-group".to_owned(), 50), ("rds-ps".to_owned(), 40)]
    );

    // An early window answers about the early rows — NOT about the live edge's. This is the bug
    // the route's missing parameter caused: a panel scrubbed back to T0+10 had no way to ask, so
    // it went on showing pty 3 from T0+50 and called it the window's.
    assert_eq!(
        rows_of(&ask(T0 + 5, T0 + 25)),
        vec![("rds-ps".to_owned(), 20), ("rds-group".to_owned(), 10)],
        "the early window's own rows"
    );
    assert_eq!(
        ask(T0 + 5, T0 + 25)["decodes"][1]["fields"]["pty"],
        json!(1)
    );

    // A window that holds ONLY the older of two rows of a frame model must serve that older row —
    // latest-per-frame-model is computed after the filter, not before. The other order answers
    // "nothing decoded here" for a window that plainly holds a decode.
    assert_eq!(
        rows_of(&ask(T0 + 5, T0 + 15)),
        vec![("rds-group".to_owned(), 10)],
        "the older row is the window's latest"
    );

    // Closed on both ends, like every other window on this API.
    assert_eq!(
        rows_of(&ask(T0 + 10, T0 + 10)),
        vec![("rds-group".to_owned(), 10)]
    );
}

/// **THE GENUINELY-EMPTY CONTROL.** A window nothing was decoded in is empty, and that emptiness
/// is a fact about the window rather than a failure to ask: the same emitter, the same route, a
/// different window, and the rows are there.
#[test]
fn a_window_with_no_decodes_is_empty_while_the_neighbouring_one_is_not() {
    let (server, seeded, repo) = serve_seeded();
    let addr = server.local_addr();
    seed_decode_history(&repo);

    let (status, quiet) = authed(
        addr,
        &format!(
            "/api/inventory/{}/decode?t0={}&t1={}",
            seeded.rds,
            T0 + 60,
            T0 + 120
        ),
    );
    assert_eq!(status, 200, "{quiet}");
    assert_eq!(quiet["decodes"], json!([]), "nothing was decoded then");

    let (_, busy) = authed(
        addr,
        &format!(
            "/api/inventory/{}/decode?t0={}&t1={}",
            seeded.rds,
            T0,
            T0 + 60
        ),
    );
    assert_eq!(rows_of(&busy).len(), 2, "and the very next window is full");
}

/// A half-given window is refused rather than completed. An invented end is exactly the
/// *plausible query* the whole-UI window rule forbids: it would succeed, return an honest zero
/// rows, and read on screen as a finding.
#[test]
fn a_half_given_or_backwards_window_is_refused_and_nanoseconds_are_not_mistaken_for_seconds() {
    let (server, seeded, _repo) = serve_seeded();
    let addr = server.local_addr();
    let path = |q: &str| format!("/api/inventory/{}/decode?{q}", seeded.rds);

    for q in [
        format!("t0={}", T0),
        format!("t1={}", T0),
        format!("t0={}&t1={}", T0 + 10, T0),
        // A nanosecond value in a seconds parameter: refused, never silently misread as a
        // year-56000 window that selects everything.
        format!("t0=0&t1={}", T0 * 1_000_000_000),
    ] {
        let (status, v) = authed(addr, &path(&q));
        assert_eq!(
            (status, v["code"].as_str()),
            (400, Some("invalid")),
            "{q} must be refused: {v}"
        );
    }
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

/// T-962: an identity-less emitter serves the **provisional** identity rows linked to it — a PI
/// recorded below its vote bar, with no identity column — so the panel can show "PI 1704
/// (3 groups, provisional)". A linked row that is not a provisional reading is not served, and a
/// withheld identity is untouched by this (the route's `Withheld` arm).
#[test]
fn an_identity_less_emitter_serves_its_linked_provisional_identity_rows() {
    let (server, seeded, repo) = serve_seeded();
    let addr = server.local_addr();
    let provisional = |frame_model: &str, t_s: i64, votes: u32| Decode {
        identity: None,
        ..rds_decode(
            frame_model,
            t_s,
            json!({"group_type": 0, "identity_provisional": true,
                   "identity_scheme": "rds-pi", "identity_value": "1704",
                   "identity_votes": votes, "identity_votes_needed": 10}),
            None,
        )
    };
    {
        let mut repo = repo.lock().unwrap();
        for d in [
            provisional("rds-group", T0 + 1, 2),
            provisional("rds-group", T0 + 2, 3),
            // Linked, no identity, but not a provisional reading: not this route's to serve.
            Decode {
                identity: None,
                ..rds_decode("rds-other", T0 + 3, json!({"x": 1}), None)
            },
        ] {
            repo.insert_decode(&d).unwrap();
            repo.link_emitter(&hk_model::EmitterLink {
                emitter_id: seeded.carrier,
                target: hk_model::LinkTarget::Decode(d.id),
                linked_at: d.t,
            })
            .unwrap();
        }
    }
    let (status, v) = authed(addr, &format!("/api/inventory/{}/decode", seeded.carrier));
    assert_eq!(status, 200, "{v}");
    let decodes = v["decodes"].as_array().unwrap();
    assert_eq!(decodes.len(), 1, "[T-962] one provisional frame model: {v}");
    let f = &decodes[0]["fields"];
    assert_eq!(decodes[0]["at"], json!((T0 + 2) as f64), "latest wins: {v}");
    assert_eq!(f["identity_provisional"], json!(true), "{v}");
    assert_eq!(f["identity_value"], json!("1704"), "{v}");
    assert_eq!(
        (
            f["identity_votes"].clone(),
            f["identity_votes_needed"].clone()
        ),
        (json!(3), json!(10)),
        "[T-962] 'PI 1704 (3 groups, provisional)' is readable: {v}"
    );
    // The window still applies.
    let (status, v) = authed(
        addr,
        &format!(
            "/api/inventory/{}/decode?t0={}&t1={}",
            seeded.carrier,
            T0,
            T0 + 1
        ),
    );
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["decodes"][0]["fields"]["identity_votes"], json!(2), "{v}");
}
