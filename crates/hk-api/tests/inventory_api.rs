//! T-022 `/api/inventory`: the T-018 inventory query over HTTP, with identity gating intact.
//! AWARE-042 (what was seen in a region, filtered) and SIGNAL-062 (the RDS emitter row) on the
//! test-only repository of `tests/support/seed_inventory.rs`.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hk_api::{ApiState, Server, ServerConfig, Token};
use hk_model::Repository;
use serde_json::{Value, json};

#[allow(dead_code)]
#[path = "support/seed_inventory.rs"]
mod seed;

const AWARE_042: &str = "AWARE-042";
const SIGNAL_062: &str = "SIGNAL-062";
const TOKEN: &str = "t022-inventory-token-0123456789abcdef";
const T0: i64 = seed::DEFAULT_T0_S;

fn serve_seeded() -> (Server, seed::Seeded) {
    let mut repo = Repository::open_in_memory().unwrap();
    let seeded = seed::seed(&mut repo, T0).unwrap();
    let config = ServerConfig::new(
        "127.0.0.1:0".parse().unwrap(),
        Token::from_config(TOKEN).unwrap(),
    );
    let state = ApiState {
        inventory: Some(Arc::new(Mutex::new(repo))),
        ..ApiState::default()
    };
    (Server::start(config, state).unwrap(), seeded)
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

fn authed(addr: SocketAddr, path: &str) -> (u16, Vec<u8>) {
    get(addr, path, Some(&format!("Bearer {TOKEN}")))
}

fn page(addr: SocketAddr, path: &str) -> Value {
    let (status, body) = authed(addr, path);
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(status, 200, "{path}: {v}");
    v
}

fn ids(v: &Value) -> Vec<String> {
    v["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_str().unwrap().to_owned())
        .collect()
}

fn row(v: &Value, id: impl ToString) -> &Value {
    let id = id.to_string();
    v["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == json!(id))
        .unwrap_or_else(|| panic!("row {id} missing"))
}

#[test]
fn unauthenticated_inventory_calls_are_rejected() {
    let (server, _) = serve_seeded();
    let addr = server.local_addr();
    for path in ["/api/inventory", "/api/inventory?f_lo=1e8&f_hi=1.1e8"] {
        for auth in [
            None,
            Some("Bearer wrong-token-0123456789abcdef"),
            Some("Bearer "),
        ] {
            let (status, body) = get(addr, path, auth);
            assert_eq!(status, 401, "{path} with {auth:?}");
            assert!(!String::from_utf8_lossy(&body).contains("entries"));
        }
    }
    // No inventory configured: 404, not an empty page.
    let config = ServerConfig::new(
        "127.0.0.1:0".parse().unwrap(),
        Token::from_config(TOKEN).unwrap(),
    );
    let bare = Server::start(config, ApiState::default()).unwrap();
    assert_eq!(authed(bare.local_addr(), "/api/inventory").0, 404);
}

#[test]
fn signal_062_rds_emitter_row_shows_the_unrestricted_identity() {
    let (server, s) = serve_seeded();
    let v = page(server.local_addr(), "/api/inventory");
    assert_eq!(v["entries"].as_array().unwrap().len(), 7);
    assert_eq!(v["identity_access"], json!("standard"));
    let r = row(&v, s.rds);
    assert_eq!(r["identity_scheme"], json!("rds-pi"), "{SIGNAL_062}");
    assert_eq!(r["identity_value"], json!(seed::RDS_PI), "{SIGNAL_062}");
    assert_eq!(r["identity_class"], json!("unrestricted"));
    assert_eq!(r["withheld"], json!(false));
    assert_eq!(r["f_center_hz"], json!(100.8e6));
    assert_eq!(r["bandwidth_hz"], json!(180e3));
    assert_eq!(r["first_seen_s"], json!(T0 as f64));
    assert_eq!(r["last_seen_s"], json!((T0 + 5) as f64));
    assert_eq!(r["count"], json!(5));
    assert_eq!(r["family"], json!("wfm"));
    assert_eq!(r["classification"]["family"], json!("wfm"));
    assert_eq!(
        r["known_status"],
        json!("known"),
        "{SIGNAL_062}: band-plan prior"
    );
    assert_eq!(r["status"]["author"], json!("prior"));
    assert_eq!(r["status"]["prior_ref"], json!("bandplan:demo#87.5-108MHz"));
    assert_eq!(r["status"]["reason"], json!("broadcast FM allocation"));
    assert_eq!(r["tags"], json!(["broadcast"]));
    // Other statuses and an anonymous emitter.
    assert_eq!(row(&v, s.fsk)["known_status"], json!("unexpected-here"));
    let carrier = row(&v, s.carrier);
    assert_eq!(carrier["known_status"], json!("unknown"));
    assert_eq!(carrier["identity_scheme"], Value::Null);
    assert_eq!(carrier["withheld"], json!(false));
    // No decode content, fingerprints or links are part of a row.
    for e in v["entries"].as_array().unwrap() {
        for key in ["content", "fingerprint", "links", "decodes", "metadata"] {
            assert!(e.get(key).is_none(), "{key} in {e}");
        }
    }
}

#[test]
fn aware_042_inventory_filters_by_region_time_status_and_tag() {
    let (server, s) = serve_seeded();
    let addr = server.local_addr();
    let set = |path: &str| -> HashSet<String> { ids(&page(addr, path)).into_iter().collect() };
    let one = |id: hk_model::EmitterId| HashSet::from([id.to_string()]);

    // Region: the 433 MHz ISM band holds only the sensor; the FM band only the RDS emitter.
    assert_eq!(
        set("/api/inventory?f_lo=433.05e6&f_hi=434.79e6"),
        one(s.sensor),
        "{AWARE_042}: region filter"
    );
    assert_eq!(set("/api/inventory?f_lo=87.5e6&f_hi=108e6"), one(s.rds));
    // An emitter overlapping the region edge counts (446.1 MHz ± 6.25 kHz).
    assert_eq!(
        set("/api/inventory?f_lo=446.105e6&f_hi=446.2e6"),
        one(s.fsk)
    );
    // Time: the FSK burst was an hour earlier; the last 10 minutes exclude it and the legacy row.
    let recent = set(&format!("/api/inventory?t0={}&t1={}", T0 - 600, T0 + 600));
    assert!(!recent.contains(&s.fsk.to_string()) && !recent.contains(&s.legacy.to_string()));
    assert!(recent.contains(&s.rds.to_string()) && recent.contains(&s.pager.to_string()));
    // Region × time together.
    assert!(
        set(&format!(
            "/api/inventory?f_lo=446e6&f_hi=446.2e6&t0={}&t1={}",
            T0 - 600,
            T0
        ))
        .is_empty()
    );
    // Status.
    assert_eq!(set("/api/inventory?status=unexpected-here"), one(s.fsk));
    let known = set("/api/inventory?status=known");
    assert_eq!(
        known,
        HashSet::from([s.rds.to_string(), s.sensor.to_string()])
    );
    assert_eq!(
        set("/api/inventory?status=known,unexpected-here,unknown").len(),
        7
    );
    // Tag, family, scheme.
    assert_eq!(set("/api/inventory?tag=weather"), one(s.sensor));
    assert_eq!(set("/api/inventory?family=wfm"), one(s.rds));
    assert_eq!(set("/api/inventory?scheme=rds-pi"), one(s.rds));

    // Pagination: pages of 3 cover every row once, most recently seen first.
    let mut seen = Vec::new();
    let mut last_seen = f64::INFINITY;
    let mut cursor: Option<String> = None;
    loop {
        let path = match &cursor {
            Some(c) => format!("/api/inventory?limit=3&cursor={c}"),
            None => "/api/inventory?limit=3".to_owned(),
        };
        let v = page(addr, &path);
        assert!(v["entries"].as_array().unwrap().len() <= 3);
        for e in v["entries"].as_array().unwrap() {
            let t = e["last_seen_s"].as_f64().unwrap();
            assert!(t <= last_seen, "ordered by last seen");
            last_seen = t;
        }
        seen.extend(ids(&v));
        match v["next_cursor"].as_str() {
            Some(c) => cursor = Some(c.to_owned()),
            None => break,
        }
    }
    assert_eq!(seen.len(), 7);
    let all: HashSet<String> = s.all().iter().map(ToString::to_string).collect();
    assert_eq!(seen.into_iter().collect::<HashSet<_>>(), all);

    // Bad parameters are refused without echoing them.
    for bad in [
        "limit=0",
        "limit=100000",
        "cursor=abc",
        "cursor=99999999999",
        "status=bogus",
        "f_lo=1e8",
        "f_lo=2e8&f_hi=1e8",
        "t0=5&t1=1",
        "scheme=not-a-scheme",
        "f_lo=NaN&f_hi=1",
    ] {
        let (status, body) = authed(addr, &format!("/api/inventory?{bad}"));
        assert_eq!(status, 400, "{bad}");
        assert!(!String::from_utf8_lossy(&body).contains("bogus"));
    }
}

/// Code-review guard: hk-api reads the inventory only through the gated `query_inventory`. The raw
/// emitter getters return identities ungated and must never be called from this crate.
#[test]
fn hk_api_never_calls_the_ungated_emitter_getters() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let forbidden = [
        ".emitter(",
        "emitter_by_identity",
        "emitters_in_region",
        "decodes_for_identity",
        "emitter_links",
        "emitter_link_history",
        "emitters_matching_fingerprint",
        "upsert_emitter",
        "record_sighting",
        // T-036: decodes stay off HTTP in M0, and reclassification has no HTTP exposure.
        "decode_with_access",
        "reclassify_identity",
        "add_emitter_tag",
    ];
    let mut scanned = 0;
    for entry in std::fs::read_dir(&src).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "rs") {
            let text = std::fs::read_to_string(&path).unwrap();
            for f in forbidden {
                assert!(!text.contains(f), "{} calls {f}", path.display());
            }
            scanned += 1;
        }
    }
    assert!(scanned >= 5);
    let query = std::fs::read_to_string(src.join("query.rs")).unwrap();
    assert!(query.contains("query_inventory"));
    assert!(query.contains("access: IdentityAccess::Standard"));
    assert!(!query.contains("OwnTrafficAuthorised"));
}

/// T-054: `/api/inventory` serves explanations only from the family map's Classifier annotations
/// (`author_ref == EXPLANATIONS_AUTHOR_REF`); a later Classifier annotation by another author
/// carrying an `explanations` key is not served.
#[test]
fn explanations_are_served_only_from_the_family_map_author() {
    use hk_model::{
        Annotation, AnnotationAuthor, AnnotationId, AnnotationKind, AnnotationTarget, ContentClass,
        Fingerprint, LinkTarget, Sighting, TimeRange, Timestamp, TrackId,
    };
    let t = |s: i64| Timestamp::from_unix_nanos(s * 1_000_000_000);
    let mut repo = Repository::open_in_memory().unwrap();
    let r = repo
        .record_sighting(
            &Sighting {
                source: LinkTarget::Track(TrackId::new()),
                seen: TimeRange::new(t(T0), t(T0 + 1)),
                count: 1,
                f_center_hz: 101.3e6,
                bandwidth_hz: 230e3,
                fingerprint: Some(Fingerprint::new(101.3e6, 230e3)),
                identity: None,
                context: None,
                classification: None,
                tags: Vec::new(),
            },
            None,
        )
        .unwrap();
    let id = r.emitter_id;
    let note = |author_ref: &str, service: &str, at: i64| Annotation {
        id: AnnotationId::new(),
        target: AnnotationTarget::Emitter(id),
        author: AnnotationAuthor::Classifier,
        author_ref: author_ref.into(),
        kind: AnnotationKind::Label,
        value: format!("explanations/{service}"),
        metadata: json!({ "explanations": [{ "service": service }] }),
        content: None,
        confidence: 0.5,
        supersedes: None,
        content_class: ContentClass::MetadataOnly,
        t: t(at),
        exported: false,
    };
    repo.insert_annotation(&note(
        hk_api::query::EXPLANATIONS_AUTHOR_REF,
        "fm-broadcast",
        T0 + 2,
    ))
    .unwrap();
    repo.insert_annotation(&note("some-future-classifier@1", "adsb", T0 + 3))
        .unwrap();
    let config = ServerConfig::new(
        "127.0.0.1:0".parse().unwrap(),
        Token::from_config(TOKEN).unwrap(),
    );
    let state = ApiState {
        inventory: Some(Arc::new(Mutex::new(repo))),
        ..ApiState::default()
    };
    let server = Server::start(config, state).unwrap();
    let body = page(server.local_addr(), "/api/inventory");
    let row = body["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == id.to_string())
        .expect("emitter row");
    assert_eq!(row["explanations"][0]["service"], "fm-broadcast", "{row}");
}
