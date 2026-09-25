//! T-022 `/api/inventory`: the T-018 inventory query over HTTP, with identity gating intact.
//! AWARE-042 (what was seen in a region, filtered) and SIGNAL-062 (the RDS emitter row) on the
//! test-only repository of `tests/support/seed_inventory.rs`.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hk_api::{ApiState, Server, ServerConfig, Token};
use hk_model::{Repository, Timestamp};
use serde_json::{Value, json};

#[allow(dead_code)]
#[path = "support/seed_inventory.rs"]
mod seed;

const AWARE_042: &str = "AWARE-042";
const SIGNAL_062: &str = "SIGNAL-062";
const TOKEN: &str = "t022-inventory-token-0123456789abcdef";
const T0: i64 = seed::DEFAULT_T0_S;

fn serve_seeded() -> (Server, seed::Seeded) {
    let (server, seeded, _) = serve_seeded_repo();
    (server, seeded)
}

/// [`serve_seeded`] keeping the repository handle, for a test that must change the store under the
/// running server (a merge, say).
fn serve_seeded_repo() -> (Server, seed::Seeded, Arc<Mutex<Repository>>) {
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

/// T-972 (docs/api.md, "Inventory entry"): `GET /api/inventory/{id}` resolves an id that has been
/// merged away to its live survivor and serves **the survivor's row** — so the `id` in the answer is
/// not necessarily the one asked for, and that is how a caller learns its id was re-keyed.
///
/// This is not a corner case in a live run: the inventory is self-cleaning and merges near-duplicate
/// rows continuously (CLAUDE.md, ADR-0019), so an id read from `/api/inventory` can be absorbed
/// before the very next call. `hk-cli`'s `api_contract.rs` used to assert the two ids were equal and
/// raced exactly that under load; here the merge is *made* to happen, so the documented answer is
/// pinned by value instead of waited for.
#[test]
fn the_entry_route_resolves_a_merged_id_to_its_survivor() {
    let (server, s, repo) = serve_seeded_repo();
    let addr = server.local_addr();
    // The absorbed row is listed, and answers under its own id, before the merge.
    let before = page(addr, "/api/inventory");
    assert!(ids(&before).contains(&s.carrier.to_string()));
    let entry = page(addr, &format!("/api/inventory/{}", s.carrier));
    assert_eq!(entry["id"], json!(s.carrier.to_string()));

    // Merge the identity-free `carrier` into `rds` (only one side holds an identity, so the merge
    // is allowed), as the pipeline's same-emission link does.
    repo.lock()
        .unwrap()
        .merge_emitters(
            s.carrier,
            s.rds,
            Timestamp::from_unix_nanos((T0 + 10) * 1_000_000_000),
            "same emission (test)",
        )
        .unwrap();

    // The absorbed id still answers `200` — never a 404, and never a shell under the old id — with
    // the survivor's row, field for field the row the survivor's own id serves.
    let resolved = page(addr, &format!("/api/inventory/{}", s.carrier));
    assert_eq!(
        resolved["id"],
        json!(s.rds.to_string()),
        "the answer carries the survivor's id: {resolved}"
    );
    // Field for field the row the survivor's own id serves, bar `presence.silence_s`: that one is
    // measured from the live edge at the moment of the call, so two calls differ by their spacing.
    let settled = |mut v: Value| -> Value {
        v["presence"].as_object_mut().unwrap().remove("silence_s");
        v
    };
    assert_eq!(
        settled(resolved),
        settled(page(addr, &format!("/api/inventory/{}", s.rds))),
        "resolution serves the survivor's row, not a copy under the old id"
    );
    // And the list no longer carries the absorbed id, which is what leaves a client holding one:
    // re-read the id from the row the server returns.
    let after = page(addr, "/api/inventory");
    let listed = ids(&after);
    assert!(!listed.contains(&s.carrier.to_string()), "{after}");
    assert!(listed.contains(&s.rds.to_string()), "{after}");
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

/// Code-review guard: hk-api reads the inventory only through the gated `query_inventory` (or,
/// since T-159, `decodes_for_identity` in `src/decode.rs` alone — see the exception below). The
/// other raw emitter getters return identities ungated and must never be called from this crate.
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
            // T-159 (ADR-0013 API GAP 3): `decode.rs` alone may call the gated
            // `decodes_for_identity` — it only reaches it after `emitter_with_access` (Standard
            // access) already resolved the identity to `InventoryIdentity::Clear`, so a withheld
            // or absent identity never gets there, and the callee re-checks and gates every row's
            // own class regardless. Every other forbidden call, and every other file, is unchanged.
            let is_decode_rs = path.file_name().is_some_and(|n| n == "decode.rs");
            for f in forbidden {
                if is_decode_rs && f == "decodes_for_identity" {
                    continue;
                }
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

/// T-591: **liveness is derived in exactly one place.** An emitter's presence is one interval
/// `[start, end?]` (ADR-0017/0019), so `live`/`ended` is a property of the emitter and not of the
/// route asked — yet `/api/events` and `/api/tiles/events` each hard-coded `IdleGap::conservative()`
/// (60 s) while `/api/inventory` measured the gap off the band's tune history (T-410), and T-254's
/// ISM scene read `open: true` on 3 of 3 events beside rows reading `ended`.
///
/// Two constants agreeing would have drifted apart again. `ObservedCoverage::track` is the single
/// derivation; this refuses any other route into `Repository::presence_intervals`, and refuses the
/// conservative constant anywhere in the crate — `IdleGap::from_coverage` already yields it for a
/// band with no recorded tune history, which is its real meaning.
#[test]
fn hk_api_derives_liveness_in_exactly_one_place() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut callers: Vec<String> = Vec::new();
    let mut conservative: Vec<String> = Vec::new();
    let mut scanned = 0;
    for entry in std::fs::read_dir(&src).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let text = std::fs::read_to_string(&path).unwrap();
        scanned += 1;
        // Code only: both of these are named in prose here and in `coverage.rs`, describing the
        // defect and the rule, and a guard that forbade *writing them down* would forbid the
        // explanation along with the call.
        let code = |needle: &str| {
            text.lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .any(|l| l.contains(needle))
        };
        // `coverage.rs` is the one derivation. Every other file must reach the track through it.
        if name != "coverage.rs" && code(".presence_intervals(") {
            callers.push(name.clone());
        }
        if code("IdleGap::conservative()") {
            conservative.push(name);
        }
    }
    assert!(scanned >= 5, "scanned {scanned} files");
    assert!(
        callers.is_empty(),
        "these derive presence outside ObservedCoverage::track: {callers:?}"
    );
    assert!(
        conservative.is_empty(),
        "these hard-code the 60 s unknown where a measurement exists: {conservative:?}"
    );
    // And the one place really is one place: the derivation, and the projection that reuses its gap.
    let coverage = std::fs::read_to_string(src.join("coverage.rs")).unwrap();
    assert_eq!(
        coverage
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .filter(|l| l.contains(".presence_intervals("))
            .count(),
        1
    );
    assert!(coverage.contains("pub fn track("));
    assert!(coverage.contains("pub fn project("));
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

/// T-284 (ADR-0017 TM-2 and §7.1): the two additive window-scoped projections.
///
/// `presence` is served on every row — Explore's Confirmed list is deliberately unwindowed and
/// still has to render liveness — while `family_in_window` is served only when a window was
/// actually asked about, so that `null` ("the window re-evidenced nothing") stays distinguishable
/// from absent ("no window was asked about"). `family` is all-time throughout.
#[test]
fn tm2_presence_and_family_in_window_are_additive_window_scoped_projections() {
    let (server, s) = serve_seeded();
    let addr = server.local_addr();
    let secs = |t: i64| json!(t as f64);

    // --- Unwindowed: presence yes, family_in_window no. ---
    let all = page(addr, "/api/inventory");
    let rds = row(&all, s.rds);
    assert!(
        rds.get("family_in_window").is_none(),
        "no window was asked about, so the question has no meaning: {rds}"
    );
    assert_eq!(rds["family"], json!("wfm"));
    // The seed was written at T0, days before the wall clock: the station stopped, and the object
    // can now say when — the sentence the product previously could not say.
    assert_eq!(rds["presence"]["liveness"], json!("ended"), "{rds}");
    assert_eq!(rds["presence"]["intervals"], json!(1), "{rds}");
    assert_eq!(rds["presence"]["on_air_s"], json!(5.0), "{rds}");
    assert_eq!(rds["presence"]["ended_t_s"], secs(T0 + 5), "{rds}");
    assert_eq!(rds["presence"]["last_interval"]["t_start_s"], secs(T0));
    assert_eq!(rds["presence"]["last_interval"]["t_end_s"], secs(T0 + 5));
    assert_eq!(rds["presence"]["last_interval"]["open"], json!(false));
    // None of it came from the lifetime count, which is what used to stand in for "still here".
    assert_eq!(rds["count"], json!(5), "{rds}");

    // --- A window ending at the station's own last evidence: the same row reads `live`. ---
    // This is what makes scrubbing back re-derive the liveness a row had at that time, instead of
    // marking every past window `ended` against the wall clock.
    let live = page(
        addr,
        &format!("/api/inventory?t0={}&t1={}", T0 - 600, T0 + 5),
    );
    let rds = row(&live, s.rds);
    assert_eq!(rds["presence"]["liveness"], json!("live"), "{rds}");
    assert_eq!(rds["presence"]["ended_t_s"], Value::Null, "{rds}");
    assert_eq!(
        rds["presence"]["last_interval"]["open"],
        json!(true),
        "{rds}"
    );
    // The window holds the classification the seed wrote at T0+5, so it re-evidences the family.
    assert_eq!(rds["family_in_window"], json!("wfm"), "{rds}");
    assert_eq!(
        rds["family"],
        json!("wfm"),
        "`family` is unchanged by any of it"
    );

    // On-air time is clipped to the window, so what a live list ranks by is honest.
    let clipped = page(
        addr,
        &format!("/api/inventory?t0={}&t1={}", T0 + 2, T0 + 600),
    );
    assert_eq!(row(&clipped, s.rds)["presence"]["on_air_s"], json!(3.0));

    // --- The "(from earlier)" case, which is the whole point of §7.1. ---
    // The sensor was on the air from T0-900, but its classification was written at T0+60. A
    // window over the start of its interval therefore intersects the signal and holds no
    // classification row: `family` still says what it is, and `family_in_window` says — honestly
    // — that nothing in these minutes re-evidenced it. A field never measured reads as
    // not-measured, never as an empty family and never as the all-time answer.
    let early = page(
        addr,
        &format!("/api/inventory?t0={}&t1={}", T0 - 900, T0 - 800),
    );
    let sensor = row(&early, s.sensor);
    assert_eq!(sensor["family"], json!("fsk2"), "{sensor}");
    assert!(
        sensor.get("family_in_window").is_some(),
        "a window was asked about, so it is answered: {sensor}"
    );
    assert_eq!(
        sensor["family_in_window"],
        Value::Null,
        "nothing in this window re-evidenced the family: {sensor}"
    );
    assert_eq!(
        sensor["presence"]["intervals"],
        json!(1),
        "and yet it was on the air in it: {sensor}"
    );
}

/// T-860 (ADR-0015 §5.4): an inventory row carries its latest analysis, summarised — `null`
/// before any (not searched), the job's confirm outcome and confirm key after one — and a
/// withheld row reads `null` whatever storage holds.
#[test]
fn inventory_row_summarises_the_latest_synthesis_and_withholds_it() {
    use hk_api::query::inventory_entry_json;
    use hk_model::repo::synthesis::{
        EmitterSynthesis, SYNTHESIZED_BY_OUTPUT_ANALYSIS, Stage, SynthPipeline, SynthesisJob,
        Verdict,
    };
    use hk_model::{
        Fingerprint, IdentityAccess, InventoryIdentity, LinkTarget, Repository, Sighting,
        TimeRange, Timestamp, TrackId,
    };
    use serde_json::{Value, json};

    let t = |s: i64| Timestamp::from_unix_nanos(1_800_000_000_000_000_000 + s * 1_000_000_000);
    let mut repo = Repository::open_in_memory().unwrap();
    let id = repo
        .record_sighting(
            &Sighting {
                source: LinkTarget::Track(TrackId::new()),
                seen: TimeRange::new(t(0), t(5)),
                count: 3,
                f_center_hz: 915e6,
                bandwidth_hz: 12e3,
                fingerprint: Some(Fingerprint::new(915e6, 12e3)),
                identity: None,
                context: None,
                classification: None,
                tags: Vec::new(),
            },
            None,
        )
        .unwrap()
        .emitter_id;
    let row = |repo: &Repository| {
        let entry = repo
            .emitter_with_access(id, IdentityAccess::Standard)
            .unwrap();
        inventory_entry_json(repo, &entry).unwrap()
    };
    let v = row(&repo);
    assert_eq!(v["synthesis"], Value::Null, "not searched: {v}");
    assert_eq!(v["identity_synthesized"], Value::Null, "no identity: {v}");

    repo.insert_synthesis(&EmitterSynthesis {
        emitter_id: id,
        provenance: SYNTHESIZED_BY_OUTPUT_ANALYSIS.into(),
        engine: "hk-synth@1".into(),
        t: t(4),
        verdict: Verdict::Solved,
        stage_reached: Stage::S5Check,
        pipeline: Some(SynthPipeline {
            demod: "fsk_demod".into(),
            decode: Some("crc".into()),
            params: Vec::new(),
            summary: "2-FSK at 9.6 kBd, CRC-16 framed".into(),
        }),
        evidence: Vec::new(),
        trace: Vec::new(),
        resolution: None,
        receiver: None,
        job: Some(SynthesisJob {
            job_id: "a7".into(),
            profile: "standard".into(),
            evidence_bits: 60.0,
            prior_bits: 1.5,
            analytic_holdout_bits: Some(30.0),
            template: None,
            recipe: json!({ "schema": "hackriff.recipe" }),
            recipe_hash: "sha256:00".into(),
            check: None,
            holdout: None,
            trace_summary: None,
            replay_key: None,
            null_control: None,
            sealed_resolution: None,
            decodes_stored: 12,
            decodes_valid: 12,
            confirm: Some(json!({ "outcome": "confirmed" })),
        }),
    })
    .unwrap();
    let v = row(&repo);
    let s = &v["synthesis"];
    assert_eq!(s["verdict"], json!("solved"), "{v}");
    assert_eq!(s["stage_reached"], json!("s5-check"), "{v}");
    assert_eq!(
        s["summary"],
        json!("2-FSK at 9.6 kBd, CRC-16 framed"),
        "{v}"
    );
    assert_eq!(s["resolution"], Value::Null, "{v}");
    assert_eq!(s["job_id"], json!("a7"), "{v}");
    assert_eq!(s["profile"], json!("standard"), "{v}");
    assert_eq!(s["analytic_holdout_bits"], json!(30.0), "{v}");
    assert_eq!(s["decodes_stored"], json!(12), "{v}");
    assert_eq!(s["confirm"], json!("confirmed"), "{v}");
    assert_eq!(s["t_s"], json!(1_800_000_004.0), "{v}");

    let mut entry = repo
        .emitter_with_access(id, IdentityAccess::Standard)
        .unwrap();
    entry.identity = InventoryIdentity::Withheld {
        scheme: hk_model::IdentityScheme::Other("pocsag-capcode".into()),
        class: None,
    };
    let v = inventory_entry_json(&repo, &entry).unwrap();
    assert_eq!(v["synthesis"], Value::Null, "withheld: {v}");
    assert_eq!(v["identity_synthesized"], Value::Null, "withheld: {v}");
}

/// T-860 review B2: a region-analyze job may not target a user-deleted entry — it would link
/// decodes to it and append analyses under it. Refused at admission with `404`, as every mutating
/// inventory route answers for a deleted entry; a live entry gets past target resolution (and
/// this server, with no job manager, then answers `503`).
#[test]
fn an_analyze_job_may_not_target_a_deleted_entry() {
    use hk_model::{LifecycleAuthor, LifecycleState, Timestamp};

    let mut repo = Repository::open_in_memory().unwrap();
    let seeded = seed::seed(&mut repo, T0).unwrap();
    repo.change_emitter_lifecycle(
        seeded.fsk,
        LifecycleState::Deleted,
        LifecycleAuthor::User,
        "tok-test",
        "not interesting",
        Timestamp::from_unix_nanos(T0 * 1_000_000_000),
    )
    .unwrap();
    let config = ServerConfig::new(
        "127.0.0.1:0".parse().unwrap(),
        Token::from_config(TOKEN).unwrap(),
    );
    let dir = std::env::temp_dir().join(format!("t860-deleted-target-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let state = ApiState {
        inventory: Some(Arc::new(Mutex::new(repo))),
        audit: Some(Arc::new(
            hk_api::AuditLog::open(&dir.join("audit.jsonl")).unwrap(),
        )),
        ..ApiState::default()
    };
    let server = Server::start(config, state).unwrap();
    let addr = server.local_addr();
    let post = |body: Value| {
        let body = body.to_string();
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        write!(
            s,
            "POST /api/analyze HTTP/1.1\r\nHost: t\r\nAuthorization: Bearer {TOKEN}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut raw = String::new();
        s.read_to_string(&mut raw).unwrap();
        let status: u16 = raw[9..12].parse().unwrap();
        let body = raw.split_once("\r\n\r\n").map_or("", |(_, b)| b);
        (
            status,
            serde_json::from_str::<Value>(body).unwrap_or(Value::Null),
        )
    };
    let (st, v) = post(json!({ "emitter_id": seeded.fsk.to_string(), "profile": "quick" }));
    assert_eq!(st, 404, "a deleted target is refused: {v}");
    let (st, v) = post(json!({ "emitter_id": seeded.carrier.to_string(), "profile": "quick" }));
    assert_eq!(
        st, 503,
        "a live target resolves, and only the missing job manager refuses: {v}"
    );
    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}

/// T-566 (ADR-0021 §7A.4, §11.1): every inventory row carries a `resolution`, and `?resolution=`
/// filters on it.
///
/// > *Not-yet-analysed and analysed-and-found-nothing are different states, and neither may be
/// > rendered as the other.*
///
/// So an emitter nothing has analysed reads `kind: "not-searched"` with no time — never `null`,
/// never `unknown` — a finished search that identified nothing reads its own sealed kind and the
/// time and profile it was measured at, a solved row reads `kind: null` (no unresolved finding),
/// and a withheld-identity row reads every field `null` with `withheld: true` rather than
/// borrowing the un-looked-at answer. The filter reads the same stored row the field is served
/// from, so the two cannot disagree, and it never matches a withheld row.
#[test]
fn the_inventory_row_and_filter_keep_not_searched_apart_from_unknown_and_solved() {
    use hk_api::query::inventory_entry_json;
    use hk_model::repo::synthesis::{
        EmitterSynthesis, Resolution, ResolutionKind, ResolutionReason,
        SYNTHESIZED_BY_OUTPUT_ANALYSIS, Stage, SynthPipeline, SynthesisJob, Verdict,
    };
    use hk_model::{
        Fingerprint, IdentityAccess, InventoryIdentity, InventoryQuery, LinkTarget, Repository,
        Sighting, TimeRange, Timestamp, TrackId,
    };
    use serde_json::{Value, json};

    let t = |s: i64| Timestamp::from_unix_nanos(1_800_000_000_000_000_000 + s * 1_000_000_000);
    let mut repo = Repository::open_in_memory().unwrap();
    let sight = |f_hz: f64| Sighting {
        source: LinkTarget::Track(TrackId::new()),
        seen: TimeRange::new(t(0), t(5)),
        count: 3,
        f_center_hz: f_hz,
        bandwidth_hz: 12e3,
        fingerprint: Some(Fingerprint::new(f_hz, 12e3)),
        identity: None,
        context: None,
        classification: None,
        tags: Vec::new(),
    };
    // Two emitters: one will be analysed, the other never is.
    let looked = repo
        .record_sighting(&sight(915e6), None)
        .unwrap()
        .emitter_id;
    let never = repo
        .record_sighting(&sight(433.92e6), None)
        .unwrap()
        .emitter_id;
    let row = |repo: &Repository, id| {
        let entry = repo
            .emitter_with_access(id, IdentityAccess::Standard)
            .unwrap();
        inventory_entry_json(repo, &entry).unwrap()
    };
    let listed = |repo: &Repository, kind: Option<ResolutionKind>| {
        let q = InventoryQuery {
            resolution: kind,
            ..InventoryQuery::default()
        };
        let ids: Vec<String> = repo
            .query_inventory(&q)
            .unwrap()
            .entries
            .iter()
            .map(|e| e.emitter.id.to_string())
            .collect();
        // T-171: the count past the page answers the same filter.
        assert_eq!(
            repo.count_inventory(&q).unwrap(),
            ids.len() as u64,
            "{ids:?}"
        );
        ids
    };

    // ---- un-looked-at is a positive state, not an absent field ----
    for id in [looked, never] {
        let v = row(&repo, id);
        let r = &v["resolution"];
        assert_eq!(r["kind"], json!("not-searched"), "{v}");
        assert_eq!((&r["reason"], &r["t"]), (&Value::Null, &Value::Null), "{v}");
        assert_eq!(r["profile"], Value::Null, "{v}");
        assert_eq!(r["withheld"], json!(false), "{v}");
    }
    assert_eq!(listed(&repo, Some(ResolutionKind::NotSearched)).len(), 2);
    assert!(listed(&repo, Some(ResolutionKind::Unknown)).is_empty());
    assert_eq!(listed(&repo, None).len(), 2, "no filter lists both");

    // ---- a finished search that identified nothing: `unknown`, with when and how deep ----
    let base = EmitterSynthesis {
        emitter_id: looked,
        provenance: SYNTHESIZED_BY_OUTPUT_ANALYSIS.into(),
        engine: "hk-synth@1".into(),
        t: t(4),
        verdict: Verdict::Framed,
        stage_reached: Stage::S4Framing,
        pipeline: None,
        evidence: Vec::new(),
        trace: Vec::new(),
        resolution: Some(Resolution {
            kind: ResolutionKind::Unknown,
            deepest_verdict: Some(Verdict::Framed),
            reason: Some(ResolutionReason::BudgetExhausted),
            suspected: None,
            summary: "9 of 11 skeletons tried; more budget is the missing ingredient".into(),
        }),
        receiver: None,
        job: Some(SynthesisJob {
            job_id: "a7".into(),
            profile: "deep".into(),
            evidence_bits: 41.2,
            prior_bits: 1.5,
            analytic_holdout_bits: None,
            template: None,
            recipe: json!({ "schema": "hackriff.recipe" }),
            recipe_hash: "sha256:00".into(),
            check: None,
            holdout: None,
            trace_summary: None,
            replay_key: None,
            null_control: None,
            sealed_resolution: None,
            decodes_stored: 0,
            decodes_valid: 0,
            confirm: None,
        }),
    };
    repo.insert_synthesis(&base).unwrap();
    let v = row(&repo, looked);
    let r = &v["resolution"];
    assert_eq!(r["kind"], json!("unknown"), "{v}");
    assert_eq!(r["reason"], json!("budget-exhausted"), "{v}");
    assert_eq!(r["t"], json!(1_800_000_004.0), "{v}");
    assert_eq!(r["profile"], json!("deep"), "{v}");
    assert_eq!(
        listed(&repo, Some(ResolutionKind::Unknown)),
        vec![looked.to_string()],
        "the searched-and-found-nothing row, and only it"
    );
    assert_eq!(
        listed(&repo, Some(ResolutionKind::NotSearched)),
        vec![never.to_string()],
        "the un-looked-at row is never listed as `unknown`"
    );
    assert!(listed(&repo, Some(ResolutionKind::StructuredUnidentified)).is_empty());

    // ---- a later analysis that solved: no unresolved finding, and it matches no filter value ----
    repo.insert_synthesis(&EmitterSynthesis {
        t: t(9),
        verdict: Verdict::Solved,
        stage_reached: Stage::S5Check,
        pipeline: Some(SynthPipeline {
            demod: "fsk_demod".into(),
            decode: Some("crc".into()),
            params: Vec::new(),
            summary: "2-FSK at 9.6 kBd, CRC-16 framed".into(),
        }),
        resolution: None,
        ..base.clone()
    })
    .unwrap();
    let v = row(&repo, looked);
    let r = &v["resolution"];
    assert_eq!(r["kind"], Value::Null, "solved: {v}");
    assert_eq!(r["t"], json!(1_800_000_009.0), "the latest row: {v}");
    for kind in [
        ResolutionKind::Unknown,
        ResolutionKind::NotSearched,
        ResolutionKind::StructuredUnidentified,
        ResolutionKind::UnsupportedStructure,
    ] {
        assert!(
            !listed(&repo, Some(kind)).contains(&looked.to_string()),
            "{kind:?}"
        );
    }

    // ---- a withheld-identity row says nothing, and that is not `not-searched` ----
    let mut entry = repo
        .emitter_with_access(looked, IdentityAccess::Standard)
        .unwrap();
    entry.identity = InventoryIdentity::Withheld {
        scheme: hk_model::IdentityScheme::Other("pocsag-capcode".into()),
        class: None,
    };
    let v = inventory_entry_json(&repo, &entry).unwrap();
    let r = &v["resolution"];
    assert_eq!(r["withheld"], json!(true), "{v}");
    for key in ["kind", "reason", "t", "profile"] {
        assert_eq!(r[key], Value::Null, "{key}: {v}");
    }
}
