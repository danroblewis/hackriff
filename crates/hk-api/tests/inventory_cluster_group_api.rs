//! T-320 `/api/inventory` → `cluster_group`: near-duplicate rows group **visibly**, and nothing
//! about them is merged.
//!
//! T-309 made near-duplicate detections of one emitter fall into a single C18 signature cluster
//! instead of each taking its own id. That is the affordance which makes duplication *visible* —
//! *these rows all measure alike* — and until this ticket its only consumer was `/api/clusters`,
//! so the Explore lists showed dozens of rows and no grouping at all.
//!
//! **What this is not.** It is not deduplication and must not read as any. Duplicate rows are
//! minted upstream by entity resolution; a cluster is an opinion *about* emitters and sets nothing
//! on them, which `hk_context::signature::cluster` pins in
//! `a_cluster_never_changes_anything_about_the_emitter`. The wire-level echo of that guarantee is
//! [`clustering_changes_nothing_on_the_wire_except_the_cluster_fields`] below: the *entire* served
//! row is byte-identical before and after clustering apart from `cluster_id` and `cluster_group`.
//!
//! The clusterer here is the real one (`hk_context`), not hand-written cluster rows, so "these
//! rows measure alike" is decided by the measurements rather than by the test.

use std::collections::{BTreeSet, HashMap};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hk_api::{ApiState, Server, ServerConfig, Token};
use hk_context::signature::assign_emitter;
use hk_model::signature::field;
use hk_model::{
    ClusterState, EmissionFeatures, EmitterId, Feat, Fingerprint, LinkTarget, Repository, Sighting,
    SignatureCluster, TimeRange, Timestamp, TrackId, cluster_label, is_cluster_id, new_cluster_id,
};
use serde_json::Value;

/// The repository the server reads, kept by the test so it can go on writing to it.
type Shared = Arc<Mutex<Repository>>;

const TOKEN: &str = "t320-cluster-group-token-0123456789abcdef";

fn t(sec: i64) -> Timestamp {
    Timestamp::from_unix_nanos(1_789_000_000_000_000_000 + sec * 1_000_000_000)
}

/// One inventory row, resolved from a sighting like any other.
fn an_emitter(r: &mut Repository, f_hz: f64) -> EmitterId {
    r.record_sighting(
        &Sighting {
            source: LinkTarget::Track(TrackId::new()),
            seen: TimeRange::new(t(0), t(1)),
            count: 3,
            f_center_hz: f_hz,
            bandwidth_hz: 36e3,
            fingerprint: Some(Fingerprint::new(f_hz, 36e3)),
            identity: None,
            context: None,
            classification: None,
            tags: Vec::new(),
        },
        None,
    )
    .unwrap()
    .emitter_id
}

/// The measurement vector the clusterer compares. `source` names the estimator/front end that
/// produced each figure — deliberately varied by the caller, because a shared-air claim must not
/// depend on which receiver made the measurement (T-259/T-305).
fn measurement(rate: f64, deviation: f64, period: f64, source: &str) -> Vec<(&'static str, Feat)> {
    vec![
        (field::FAMILY, Feat::text("fsk", source)),
        (field::SYMBOL_RATE_HZ, Feat::num(rate, rate * 0.002, source)),
        (
            field::DEVIATION_HZ,
            Feat::num(deviation, deviation * 0.02, source),
        ),
        (field::PERIOD_S, Feat::num(period, period * 0.01, source)),
        (field::OBW_HZ, Feat::num(36e3, 500.0, source)),
    ]
}

fn store_measurement(r: &mut Repository, id: EmitterId, tag: &str, fields: &[(&str, Feat)]) {
    let mut f = EmissionFeatures::new(format!("features:{tag}"), id, t(10));
    f.observe(
        fields.iter().map(|(n, v)| ((*n).to_owned(), v.clone())),
        false,
    );
    r.put_emission_features(&f).unwrap();
}

/// Serves `repo`, and hands the handle back so the test can keep writing to the same repository
/// the server reads — that is what lets one test compare a row before and after clustering.
fn serve(repo: Repository) -> (Server, Shared) {
    let shared: Shared = Arc::new(Mutex::new(repo));
    let config = ServerConfig::new(
        "127.0.0.1:0".parse().unwrap(),
        Token::from_config(TOKEN).unwrap(),
    );
    let server = Server::start(
        config,
        ApiState {
            inventory: Some(Arc::clone(&shared)),
            ..ApiState::default()
        },
    )
    .unwrap();
    (server, shared)
}

fn get(addr: SocketAddr, path: &str) -> Value {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: test\r\nAuthorization: Bearer {TOKEN}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&raw[..split]);
    let status: u16 = head.split(' ').nth(1).and_then(|c| c.parse().ok()).unwrap();
    let v: Value = serde_json::from_slice(&raw[split + 4..]).unwrap();
    assert_eq!(status, 200, "{path}: {v}");
    v
}

/// Rows by emitter id, from one `/api/inventory` page.
///
/// `at` (T-263) pins the live edge to the scene's own clock, so `presence.silence_s` is a fixed
/// number rather than a wall-clock reading that ticks between two requests — otherwise
/// [`clustering_changes_nothing_on_the_wire_except_the_cluster_fields`] would fail on the passage
/// of time rather than on anything clustering did.
fn rows(addr: SocketAddr) -> HashMap<String, Value> {
    get(addr, "/api/inventory?at=1789000030")["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["id"].as_str().unwrap().to_owned(), r.clone()))
        .collect()
}

/// A repository holding two groups of three like emitters plus one emitter nothing was measured
/// on, all run through the real clusterer. Returns the ids in that order.
///
/// Group A's three measurements are made by **three different front ends** and group B's by one;
/// clustering must be blind to that either way.
fn two_groups_and_a_loner() -> (Repository, Vec<EmitterId>) {
    let mut r = Repository::open_in_memory().unwrap();
    let mut ids = Vec::new();
    // Group B repeats group A's *relative* geometry at a different scale, so the two groups are
    // internally as close as each other and the only thing separating them is that they measure
    // nothing alike (half the symbol rate, a quarter the deviation, 7.5× the period).
    let scene: [(f64, f64, f64, &str); 6] = [
        // Group A — one kind of emitter, measured by three different receivers.
        (4800.0, 9600.0, 0.12, "hackrf:A"),
        (4810.0, 9500.0, 0.121, "hackrf:B"),
        (4795.0, 9700.0, 0.119, "rtlsdr:C"),
        // Group B — a different emitter type entirely, all three seen by one receiver.
        (2400.0, 2400.0, 0.9, "hackrf:A"),
        (2405.0, 2375.0, 0.9075, "hackrf:A"),
        (2397.5, 2425.0, 0.8925, "hackrf:A"),
    ];
    for (i, (rate, dev, period, source)) in scene.into_iter().enumerate() {
        let e = an_emitter(&mut r, 433.0e6 + 1e6 * i as f64);
        store_measurement(
            &mut r,
            e,
            &i.to_string(),
            &measurement(rate, dev, period, source),
        );
        ids.push(e);
    }
    // A seventh row with nothing measured on it: it must read `null`, not an empty group.
    ids.push(an_emitter(&mut r, 446.0e6));
    for e in &ids {
        assign_emitter(&mut r, *e, t(20)).unwrap();
    }
    (r, ids)
}

fn group(row: &Value) -> &Value {
    &row["cluster_group"]
}

/// **The property, on values.** Rows that share a signature cluster carry the same cluster id and
/// the same label *on the wire*, and `rows_in_view` says how many of the rows in front of the
/// reader measure alike. Asserting the field merely *exists* would pass on a `null` (T-315).
///
/// **The control that matters.** The second group of three does not measure like the first, and
/// reads a **different** id and a **different** label. Without it every assertion above is
/// satisfiable by emitting one constant for every row.
#[test]
fn like_rows_carry_one_group_on_the_wire_and_unlike_rows_carry_another() {
    let (repo, ids) = two_groups_and_a_loner();
    let (server, _repo) = serve(repo);
    let rows = rows(server.local_addr());
    let row = |i: usize| rows[&ids[i].to_string()].clone();

    // Group A: one id, one label, three rows in view — asserted on the values themselves.
    let a = group(&row(0)).clone();
    assert!(a.is_object(), "group A has no cluster_group: {:?}", row(0));
    let a_id = a["cluster_id"].as_str().unwrap().to_owned();
    assert!(is_cluster_id(&a_id), "{a_id}");
    for i in 0..3 {
        assert_eq!(group(&row(i)), &a, "row {i} is in group A: {:?}", row(i));
    }
    assert_eq!(a["label"], Value::from(cluster_label(&a_id)));
    assert_eq!(a["rows_in_view"], Value::from(3), "{a}");

    // Group B: a different cluster, and — the control — a different label.
    let b = group(&row(3)).clone();
    let b_id = b["cluster_id"].as_str().unwrap().to_owned();
    assert_ne!(b_id, a_id, "unlike measurements must not share a cluster");
    assert_ne!(
        b["label"], a["label"],
        "two clusters reading one label is a constant, not a grouping"
    );
    for i in 3..6 {
        assert_eq!(group(&row(i)), &b, "row {i} is in group B: {:?}", row(i));
    }
    assert_eq!(b["rows_in_view"], Value::from(3), "{b}");

    // Nothing measured, so nothing to group: `null`, never an empty or singleton group object.
    assert_eq!(group(&row(6)), &Value::Null, "{:?}", row(6));
    assert_eq!(row(6)["cluster_id"], Value::Null);

    // `cluster_group` is null exactly when `cluster_id` is, on every row served.
    for r in rows.values() {
        assert_eq!(
            group(r).is_null(),
            r["cluster_id"].is_null(),
            "cluster_group and cluster_id disagree: {r}"
        );
    }
}

/// **No device leakage.** A signature cluster is a shared-air claim, so it must not acquire a
/// device on its way to the wire (T-259/T-305: dedup, clustering and identity never read the
/// device; images, harmonics, IMD, floor and gain state must).
///
/// Group A's three rows were measured by three *different* front ends and still read one group;
/// group B's three were measured by one front end and still read a different group from A. So the
/// grouping tracks the measurements and not the receiver, in both directions — the direction that
/// would hide a shared emitter behind two front ends, and the direction that would group two
/// unrelated signals because one receiver saw both.
///
/// The object itself names no device either: its keys are exactly the three below, and the served
/// `label` is `cluster_label` of the id and nothing else, so no device id can reach it.
#[test]
fn a_cluster_group_names_no_device_and_does_not_vary_with_the_front_end() {
    let (repo, ids) = two_groups_and_a_loner();
    let (server, _repo) = serve(repo);
    let rows = rows(server.local_addr());

    // Three front ends, one group — and one front end did not fuse the two groups.
    let by_row: Vec<Value> = ids.iter().map(|i| rows[&i.to_string()].clone()).collect();
    let a: BTreeSet<String> = (0..3)
        .map(|i| by_row[i]["cluster_id"].as_str().unwrap().to_owned())
        .collect();
    let b: BTreeSet<String> = (3..6)
        .map(|i| by_row[i]["cluster_id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(a.len(), 1, "three receivers split one cluster: {a:?}");
    assert_eq!(b.len(), 1, "{b:?}");
    assert!(a.is_disjoint(&b), "one receiver fused two clusters");

    for r in rows.values() {
        let g = group(r);
        if g.is_null() {
            continue;
        }
        let keys: BTreeSet<&str> = g.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            BTreeSet::from(["cluster_id", "label", "rows_in_view"]),
            "cluster_group grew a key: {g}"
        );
        let id = g["cluster_id"].as_str().unwrap();
        assert_eq!(
            g["label"].as_str(),
            Some(cluster_label(id).as_str()),
            "the label is derived from the cluster id alone: {g}"
        );
    }
}

/// **The honesty test, at the wire.** `a_cluster_never_changes_anything_about_the_emitter`
/// (`hk_context::signature::cluster_tests`) pins that clustering writes nothing on an emitter.
/// This is the same guarantee where the user actually sees it: serve the inventory before
/// clustering and after, and the *whole* row is identical except `cluster_id` and `cluster_group`.
///
/// So a shared group can never have moved a row's identity, family, known status, lifecycle, tags,
/// classification, counts or presence — and surfacing the grouping cannot quietly become
/// deduplication, because a merged row would differ in far more than these two keys.
#[test]
fn clustering_changes_nothing_on_the_wire_except_the_cluster_fields() {
    let mut repo = Repository::open_in_memory().unwrap();
    let mut ids = Vec::new();
    for (i, (rate, dev, period)) in [
        (4800.0, 9600.0, 0.12),
        (4810.0, 9500.0, 0.121),
        (4795.0, 9700.0, 0.119),
    ]
    .into_iter()
    .enumerate()
    {
        let e = an_emitter(&mut repo, 433.0e6 + 1e6 * i as f64);
        store_measurement(
            &mut repo,
            e,
            &i.to_string(),
            &measurement(rate, dev, period, "hackrf:A"),
        );
        ids.push(e);
    }

    // Before: no cluster has been assigned, so every row reads `null` for both fields.
    let (server, shared) = serve(repo);
    let before = rows(server.local_addr());
    for id in &ids {
        let r = &before[&id.to_string()];
        assert_eq!(r["cluster_id"], Value::Null, "{r}");
        assert_eq!(r["cluster_group"], Value::Null, "{r}");
    }

    // The same repository the server is reading, so nothing but clustering has happened between
    // the two pages.
    {
        let mut repo = shared.lock().unwrap();
        for e in &ids {
            assign_emitter(&mut repo, *e, t(20)).unwrap().unwrap();
        }
    }
    let after = rows(server.local_addr());

    for id in &ids {
        let id = id.to_string();
        let (mut b, mut a) = (before[&id].clone(), after[&id].clone());
        // The cluster fields (T-320, and T-593 cluster_status) are the *only* ones allowed to move.
        assert!(a["cluster_group"].is_object(), "{a}");
        assert!(a["cluster_id"].is_string(), "{a}");
        for key in ["cluster_id", "cluster_group", "cluster_status"] {
            b.as_object_mut().unwrap().remove(key);
            a.as_object_mut().unwrap().remove(key);
        }
        assert_eq!(
            b, a,
            "clustering changed something else about the emitter on the wire"
        );
    }
    // And the rows are still three rows: grouping shows duplication, it does not collapse it.
    assert_eq!(after.len(), before.len(), "a row disappeared");
}

/// **T-593: an explained absence is served, not silent.** Four rows all read `cluster_id: null`
/// and `cluster_group: null`, for four different reasons the clusterer recorded — and on the wire
/// they must be four different answers:
///
/// - a row with **two** comparable fields, below the three-field evidence floor → `abstained`,
///   `too_few_fields`;
/// - a row with a full vector sitting between two mutually incompatible clusters → `abstained`,
///   `ambiguous` — declined for a *different* cause, and distinguishable from the floor;
/// - a lone row with a full vector, which seeded a group still below the visibility floor →
///   `pending` (no id: a guess with an id reads as a finding);
/// - a row nothing was measured on, which the clusterer never decided about → `unassessed`.
///
/// Asserted by counting states and reasons over the served page, never on a clock. Without the
/// field every one of these rows reads identically, which is the failure the ticket names.
#[test]
fn a_null_cluster_id_carries_why_and_the_floor_reads_differently_from_other_abstentions() {
    let mut r = Repository::open_in_memory().unwrap();

    // Two active clusters that genuinely disagree with each other (deviation 9600 vs 2400).
    for deviation in [9600.0_f64, 2400.0] {
        let mut c = SignatureCluster::new(new_cluster_id(), t(0));
        c.centroid.fold_member(
            &[
                (field::SYMBOL_RATE_HZ, Feat::num(4800.0, 5.0, "c14")),
                (field::OBW_HZ, Feat::num(36e3, 200.0, "c14")),
                (
                    field::DEVIATION_HZ,
                    Feat::num(deviation, deviation * 0.001, "c14"),
                ),
            ]
            .into_iter()
            .map(|(n, f)| (n.to_owned(), f))
            .collect(),
            0.0,
        );
        c.state = ClusterState::Active;
        r.put_cluster(&c).unwrap();
    }

    // Below the floor: two comparable fields, nothing else.
    let thin = an_emitter(&mut r, 433.0e6);
    store_measurement(
        &mut r,
        thin,
        "thin",
        &[
            (field::SYMBOL_RATE_HZ, Feat::num(1200.0, 2.0, "c14")),
            (field::OBW_HZ, Feat::num(12e3, 200.0, "c14")),
        ],
    );
    // Between the two clusters, with a deviation sigma wide enough to reach both.
    let between = an_emitter(&mut r, 434.0e6);
    store_measurement(
        &mut r,
        between,
        "between",
        &[
            (field::SYMBOL_RATE_HZ, Feat::num(4800.0, 5.0, "c14")),
            (field::OBW_HZ, Feat::num(36e3, 200.0, "c14")),
            (field::DEVIATION_HZ, Feat::num(6000.0, 4000.0, "c14")),
        ],
    );
    // A full vector like nothing else: it seeds its own group, which is not yet visible.
    let lone = an_emitter(&mut r, 435.0e6);
    store_measurement(
        &mut r,
        lone,
        "lone",
        &measurement(300.0, 150_000.0, 30.0, "hackrf:A"),
    );
    // Nothing measured: the clusterer has nothing to decide.
    let blank = an_emitter(&mut r, 436.0e6);

    let decided: Vec<_> = [thin, between, lone, blank]
        .into_iter()
        .map(|e| assign_emitter(&mut r, e, t(20)).unwrap().map(|a| a.reason))
        .collect();
    assert_eq!(
        decided,
        vec![
            Some("too_few_fields"),
            Some("ambiguous"),
            Some("seeded"),
            None
        ],
        "the scene must set up the four cases it claims to"
    );

    let (server, _repo) = serve(r);
    let rows = rows(server.local_addr());
    let status = |e: EmitterId| rows[&e.to_string()]["cluster_status"].clone();

    // Every one of the four is a bare null to `cluster_id` and `cluster_group`…
    for e in [thin, between, lone, blank] {
        let row = &rows[&e.to_string()];
        assert_eq!(row["cluster_id"], Value::Null, "{row}");
        assert_eq!(row["cluster_group"], Value::Null, "{row}");
    }

    // …and `cluster_status` tells them apart.
    let s = status(thin);
    assert_eq!(s["state"], "abstained", "{s}");
    assert_eq!(
        s["reason"], "too_few_fields",
        "the floor is named on the wire: {s}"
    );
    assert_eq!(s["t_s"].as_f64(), Some(1_789_000_020.0), "{s}");

    let s = status(between);
    assert_eq!(s["state"], "abstained", "{s}");
    assert_eq!(
        s["reason"], "ambiguous",
        "a different cause reads that cause: {s}"
    );

    let s = status(lone);
    assert_eq!(s["state"], "pending", "{s}");
    assert_eq!(s["reason"], "seeded", "{s}");

    let s = status(blank);
    assert_eq!(s["state"], "unassessed", "{s}");
    assert_eq!(s["reason"], Value::Null, "{s}");
    assert_eq!(s["t_s"], Value::Null, "{s}");

    // Counted over the page: four rows that `cluster_id` cannot tell apart are four distinct
    // (state, reason) pairs here, and exactly one of them names the evidence floor.
    let pairs: BTreeSet<(String, String)> = [thin, between, lone, blank]
        .into_iter()
        .map(|e| {
            let s = status(e);
            (s["state"].to_string(), s["reason"].to_string())
        })
        .collect();
    assert_eq!(pairs.len(), 4, "explained absences collapsed: {pairs:?}");
    let floor = rows
        .values()
        .filter(|r| r["cluster_status"]["reason"] == "too_few_fields")
        .count();
    assert_eq!(floor, 1, "exactly the sub-floor row names the floor");
    let null_ids = rows.values().filter(|r| r["cluster_id"].is_null()).count();
    assert_eq!(null_ids, 4);
}

/// A visibly clustered row says so, and how it got there — `cluster_status` never contradicts
/// `cluster_id`: `clustered` exactly when the id is served.
#[test]
fn cluster_status_agrees_with_cluster_id_on_every_row() {
    let (repo, _ids) = two_groups_and_a_loner();
    let (server, _repo) = serve(repo);
    let rows = rows(server.local_addr());
    let mut clustered = 0;
    for r in rows.values() {
        let s = &r["cluster_status"];
        assert!(
            s.is_object(),
            "an unwithheld row always explains itself: {r}"
        );
        assert_eq!(
            s["state"] == "clustered",
            r["cluster_id"].is_string(),
            "cluster_status and cluster_id disagree: {r}"
        );
        if s["state"] == "clustered" {
            clustered += 1;
            assert!(
                matches!(s["reason"].as_str(), Some("joined" | "seeded")),
                "{s}"
            );
        }
    }
    assert_eq!(clustered, 6, "both groups of three read clustered");
}
