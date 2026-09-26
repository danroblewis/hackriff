//! T-962 (SIGNAL-062), the path the false commit actually took: the **`rds` recipe's**
//! `messages` outputs may not make an RDS PI an identity on fewer agreeing groups than the
//! always-on `hk-rds` decoder may.
//!
//! **The defect.** The explorer started `POST /api/pipelines {recipe_id: "rds"}` on every station
//! (journal 2026-09-25), and 98.088 MHz read "PI committed 1704 (Confirmed, from about 3 groups
//! earlier)" while an independent oracle found no RDS on the clip. `rds.recipe.json`'s consensus
//! node passes a PI after two agreeing groups; every `messages` output declares
//! `identity: {scheme: rds-pi}`, so each group became a Decode row **carrying the identity**, and
//! `ConfirmPolicy`'s route A confirms on one such row.
//!
//! **The bar.** One for every producer: [`hk_model::RDS_PI_COMMIT_VOTES`] (10) agreeing CRC-valid
//! frames per identity, applied by the recipe writer (`IdentityTally`) exactly as `hk-demod`'s
//! record writer applies it to the chain's PI vote. Below it the row is written with no identity
//! and `identity_provisional: true` plus its vote, linked to the pipeline's target emitter.
//!
//! Driven through the real `recipes/rds.recipe.json` and its `group-info` output's writer (the
//! same `MessagesSink` a running pipeline uses), with the layer trees the consensus node emits.

mod common;

use std::sync::Arc;

use common::TempDir;
use hk_blocks::{FrameInfo, Output, PortInfo, PortVec};
use hk_model::{
    ContentClass, CrcStatus, DecodedIdentity, EmitterId, Fingerprint, IdentityScheme,
    LifecycleState, LinkTarget, MeasurementKey, RDS_PI_COMMIT_VOTES, Repository, Sighting,
    TimeRange, Timestamp,
};
use hk_pipeline::inventory::{ConfirmEvidence, ConfirmPolicy, ConfirmRoute};
use hk_pipeline::recipes::messages::MessagesSink;
use hk_pipeline::recipes::runtime::{PipelineStats, parse_recipe};
use hk_pipeline::recipes::taps::FrameCtx;
use hk_recipe::PortType;
use hk_stream::inspector::{FitStatus, LayerNode, LayerTree, NodeType};
use serde_json::{Value, json};

const T962: &str = "T-962";
const PI: u16 = 0x1704;
const CENTER: f64 = 98.085e6;
const T0_NS: i64 = 1_789_000_000_000_000_000;

fn rds_recipe() -> hk_recipe::Recipe {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../recipes/rds.recipe.json");
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    parse_recipe(doc).unwrap()
}

/// One CRC-valid 0A group as the consensus node (`agree`) passes it: PI committed, group fields.
fn group_tree(pi: u16) -> Arc<LayerTree> {
    let node = |id: u32, path: &str, bits: u32, value: u64| LayerNode {
        id,
        parent: None,
        name: path.into(),
        path: path.into(),
        ty: NodeType::Uint,
        bits: [0, bits],
        bytes: [0, bits.div_ceil(8)],
        value: Some(json!(value)),
        text: None,
        label: None,
        error: false,
    };
    Arc::new(LayerTree {
        nodes: vec![
            node(0, "pi", 16, u64::from(pi)),
            node(1, "group_type", 4, 0),
            node(2, "version", 1, 0),
            node(3, "tp", 1, 0),
            node(4, "pty", 5, 10),
        ],
        byte_index: Vec::new(),
        fit: FitStatus::Ok,
        errors: Vec::new(),
    })
}

/// The station's inventory entry, placed by occupancy with no identity: the `{emitter_id}` the
/// explorer started the recipe on.
fn seed_station(repo: &mut Repository) -> EmitterId {
    let t = Timestamp::from_unix_nanos(T0_NS);
    let sighting = Sighting {
        source: LinkTarget::Detection(hk_model::DetectionId::new()),
        seen: TimeRange::new(t, t),
        count: 1,
        f_center_hz: CENTER,
        bandwidth_hz: 180e3,
        fingerprint: Some(Fingerprint::new(CENTER, 180e3)),
        identity: None,
        context: None,
        classification: None,
        tags: Vec::new(),
    };
    repo.record_sighting_measured(&sighting, &MeasurementKey::new("t962-recipe"), None)
        .unwrap()
        .emitter_id
}

/// Runs the `group-info` writer targeting `station` over `groups` agreeing groups (frames
/// `first..first + groups`), and waits for it to store them (dropping the sink joins it).
fn feed(db: &std::path::Path, station: EmitterId, first: usize, groups: usize) {
    feed_spaced(db, station, first, groups, 104);
}

/// [`feed`] with the groups `spacing_bits` apart in capture time (104 = back to back).
fn feed_spaced(
    db: &std::path::Path,
    station: EmitterId,
    first: usize,
    groups: usize,
    spacing_bits: u64,
) {
    let recipe = rds_recipe();
    let sink = MessagesSink::spawn_standalone_targeting(
        db,
        &recipe,
        "group-info",
        ContentClass::Unrestricted,
        Some(station),
        16,
        Arc::new(PipelineStats::default()),
        |_, _| {},
    );
    let mut sink = sink.unwrap();
    let mut out = Output::for_port(&PortInfo {
        ty: PortType::Frames,
        rate_hz: 1187.5,
        max_items: 64,
        hold_items: 0,
    });
    let PortVec::Frames(buf) = &mut out.data else {
        unreachable!("a frames port")
    };
    for i in first..first + groups {
        // One group every 104 bits at 1187.5 Bd.
        let mut info = FrameInfo::new(i as u64, spacing_bits * i as u64, 0);
        info.bit_len = 104;
        info.check = CrcStatus::Valid;
        info.layers = Some(group_tree(PI));
        buf.push(&[0u8; 13], info);
    }
    let ctx = FrameCtx {
        decoder: "recipe:rds@1",
        frame_model: "rds",
        emitter_id: Some(station),
        channel_hz: CENTER,
        channels_hz: &[],
        recipe_version: 1,
        edit_rev: 0,
    };
    let t_of = |bit: f64| Timestamp::from_unix_nanos(T0_NS + (bit / 1187.5 * 1e9) as i64);
    assert_eq!(sink.publish(&out, &ctx, &t_of), groups as u64);
    drop(sink);
}

fn pi_1704() -> DecodedIdentity {
    DecodedIdentity {
        scheme: IdentityScheme::RdsPi,
        value: "1704".into(),
    }
}

/// Route A alone: the evidence `ConfirmInventory::review` builds, other routes empty.
fn identity_decision(repo: &Repository, id: EmitterId) -> Option<(ConfirmRoute, String)> {
    let ev = ConfirmEvidence {
        identity: repo.identity_decode_evidence(id).unwrap(),
        track: None,
        verified: None,
    };
    ConfirmPolicy::default().decide_route(&ev)
}

/// The rows linked to `station`, oldest first.
fn linked_rows(repo: &Repository, station: EmitterId) -> Vec<hk_model::Decode> {
    let mut rows: Vec<_> = repo
        .emitter_links(repo.live_emitter_id(station).unwrap())
        .unwrap()
        .into_iter()
        .filter_map(|l| match l.target {
            LinkTarget::Decode(d) => repo.decode(d).ok(),
            _ => None,
        })
        .collect();
    rows.sort_by_key(|d| d.t);
    rows
}

#[test]
fn t962_three_recipe_groups_do_not_confirm_and_ten_do() {
    let dir = TempDir::new("t962-recipe");
    let db = dir.0.join("hk.sqlite");
    let station = {
        let mut repo = Repository::open(&db).unwrap();
        seed_station(&mut repo)
    };

    // The 98.088 MHz evidence: three agreeing CRC-valid groups through the recipe.
    feed(&db, station, 0, 3);
    let repo = Repository::open(&db).unwrap();
    let placed = repo.emitter_by_identity(&pi_1704()).unwrap();
    assert!(
        placed.is_none(),
        "[{T962}] three rds-recipe groups placed an Emitter under rds-pi:1704 ({:?}) — the \
         98.088 MHz 'PI committed 1704 (Confirmed, from about 3 groups)'. An RDS PI needs \
         {RDS_PI_COMMIT_VOTES} agreeing CRC-valid groups whichever decoder heard it; route A \
         would confirm on this: {:?}",
        placed.as_ref().map(|e| e.id),
        placed.as_ref().map(|e| identity_decision(&repo, e.id)),
    );
    assert_eq!(identity_decision(&repo, station), None, "[{T962}]");
    assert!(repo.decodes_for_identity(&pi_1704()).unwrap().is_empty());
    assert_eq!(
        repo.emitter_lifecycle_state(station).unwrap(),
        LifecycleState::Candidate,
        "[{T962}] the station stays a Candidate"
    );
    // The reading is kept: three rows linked to the station, no identity, each with its vote —
    // what "PI 1704 (3 groups, provisional)" is read from.
    let rows = linked_rows(&repo, station);
    let votes: Vec<_> = rows
        .iter()
        .map(|d| {
            assert!(d.identity.is_none(), "[{T962}] {d:?}");
            assert_eq!(d.metadata["identity_provisional"], json!(true), "{d:?}");
            assert_eq!(d.metadata["identity_value"], json!("1704"), "{d:?}");
            assert_eq!(d.metadata["identity_scheme"], json!("rds-pi"), "{d:?}");
            assert_eq!(
                d.metadata["identity_votes_needed"],
                json!(RDS_PI_COMMIT_VOTES),
                "{d:?}"
            );
            d.metadata["identity_votes"].as_u64().unwrap()
        })
        .collect();
    assert_eq!(votes, [1, 2, 3], "[{T962}] one vote per agreeing group");
    drop(repo);

    // The same station keeps transmitting: one writer (one pipeline run) reaches the bar at its
    // tenth agreeing group — under a second of real lock at 11.4 groups/s.
    let dir2 = TempDir::new("t962-recipe-strong");
    let db2 = dir2.0.join("hk.sqlite");
    let station2 = {
        let mut repo = Repository::open(&db2).unwrap();
        seed_station(&mut repo)
    };
    feed(&db2, station2, 0, RDS_PI_COMMIT_VOTES as usize);
    let repo = Repository::open(&db2).unwrap();
    let e = repo
        .emitter_by_identity(&pi_1704())
        .unwrap()
        .expect("[T-962] ten agreeing groups make the PI an identity");
    let (route, reason) =
        identity_decision(&repo, e.id).expect("[T-962] and route A confirms, exactly as before");
    assert_eq!(route, ConfirmRoute::Identity, "[{T962}] {reason}");
    let with_identity = repo.decodes_for_identity(&pi_1704()).unwrap();
    assert_eq!(
        with_identity.len(),
        1,
        "[{T962}] only the tenth row carries the identity"
    );
    assert_eq!(
        with_identity[0].metadata["identity_provisional"],
        json!(false)
    );
    assert_eq!(
        with_identity[0].metadata["identity_votes"],
        json!(RDS_PI_COMMIT_VOTES)
    );
}

/// T-962 round 2: the bar is a **rate**, so it is enforced as one. A recipe pipeline runs until
/// deleted; on a chance lock at one agreeing CRC-valid group per 15 s, a lifetime count reaches
/// 10 in ~150 s and route A confirms the false PI two and a half minutes late. Twenty agreeing
/// groups over 300 s of capture time — twice the bar — stay provisional: no identity, no confirm.
#[test]
fn t962_sparse_recipe_votes_over_minutes_never_commit() {
    let dir = TempDir::new("t962-recipe-sparse");
    let db = dir.0.join("hk.sqlite");
    let station = {
        let mut repo = Repository::open(&db).unwrap();
        seed_station(&mut repo)
    };
    // One group per 15 s of capture time (17 813 bits at 1187.5 Bd), all in one pipeline run.
    let groups = 2 * RDS_PI_COMMIT_VOTES as usize;
    feed_spaced(&db, station, 0, groups, 17_813);
    let repo = Repository::open(&db).unwrap();
    let placed = repo.emitter_by_identity(&pi_1704()).unwrap();
    assert!(
        placed.is_none(),
        "[{T962}] {groups} agreeing groups one per 15 s (300 s) placed an identity: the bar was \
         enforced as a lifetime count, not as {RDS_PI_COMMIT_VOTES} votes within \
         {} s of capture time. Route A would confirm on it: {:?}",
        hk_model::RDS_PI_COMMIT_WINDOW_NS / 1_000_000_000,
        placed.as_ref().map(|e| identity_decision(&repo, e.id)),
    );
    assert!(repo.decodes_for_identity(&pi_1704()).unwrap().is_empty());
    assert_eq!(identity_decision(&repo, station), None, "[{T962}]");
    assert_eq!(
        repo.emitter_lifecycle_state(station).unwrap(),
        LifecycleState::Candidate,
        "[{T962}] the station stays a Candidate"
    );
    let rows = linked_rows(&repo, station);
    assert_eq!(rows.len(), groups, "[{T962}] every reading is kept");
    for (k, d) in rows.iter().enumerate() {
        assert!(d.identity.is_none(), "[{T962}] {d:?}");
        assert_eq!(d.metadata["identity_provisional"], json!(true), "{d:?}");
        assert_eq!(d.metadata["identity_votes"], json!(k + 1), "{d:?}");
        assert_eq!(d.metadata["identity_votes_in_window"], json!(1), "{d:?}");
        assert_eq!(d.metadata["identity_votes_window_s"], json!(5.0), "{d:?}");
    }
}

/// T-962 round 2, the other side: the same PI with ten agreeing groups inside one second of
/// capture time — a real station's rate — commits on the tenth, and route A confirms.
#[test]
fn t962_dense_recipe_votes_inside_a_second_commit() {
    let dir = TempDir::new("t962-recipe-dense");
    let db = dir.0.join("hk.sqlite");
    let station = {
        let mut repo = Repository::open(&db).unwrap();
        seed_station(&mut repo)
    };
    // 10 groups 110 bits apart: the tenth lands 0.83 s after the first.
    feed_spaced(&db, station, 0, RDS_PI_COMMIT_VOTES as usize, 110);
    let repo = Repository::open(&db).unwrap();
    let e = repo
        .emitter_by_identity(&pi_1704())
        .unwrap()
        .expect("[T-962] ten agreeing groups inside a second make the PI an identity");
    let (route, reason) = identity_decision(&repo, e.id).expect("[T-962] route A confirms");
    assert_eq!(route, ConfirmRoute::Identity, "[{T962}] {reason}");
    let with_identity = repo.decodes_for_identity(&pi_1704()).unwrap();
    assert_eq!(with_identity.len(), 1, "[{T962}] the tenth row");
    assert_eq!(
        with_identity[0].metadata["identity_votes_in_window"],
        json!(RDS_PI_COMMIT_VOTES)
    );
}

/// A PS string as the recipe's `text` node (`ps`) emits it: key (PI) and text.
fn ps_tree(pi: u16, text: &str) -> Arc<LayerTree> {
    let node = |id: u32, path: &str, ty: NodeType, value: Value| LayerNode {
        id,
        parent: (id > 0).then_some(0),
        name: path.rsplit('.').next().unwrap().into(),
        path: path.into(),
        ty,
        bits: [0, 0],
        bytes: [0, 0],
        value: Some(value),
        text: None,
        label: None,
        error: false,
    };
    Arc::new(LayerTree {
        nodes: vec![
            LayerNode {
                value: None,
                ..node(0, "ps", NodeType::Layer, Value::Null)
            },
            node(1, "ps.key", NodeType::Uint, json!(pi)),
            node(2, "ps.text", NodeType::Ascii, json!(text)),
        ],
        byte_index: Vec::new(),
        fit: FitStatus::Ok,
        errors: Vec::new(),
    })
}

/// Runs output `output` of the `rds` recipe over `frames` (`(bit position, layer tree)`, all
/// CRC-valid) with the pipeline's `stats` — shared, as a running pipeline shares it between its
/// outputs — and waits for the writer to store them.
fn feed_output(
    db: &std::path::Path,
    station: EmitterId,
    output: &str,
    stats: &Arc<PipelineStats>,
    frames: &[(u64, Arc<LayerTree>)],
) {
    let recipe = rds_recipe();
    let mut sink = MessagesSink::spawn_standalone_targeting(
        db,
        &recipe,
        output,
        ContentClass::Unrestricted,
        Some(station),
        16,
        Arc::clone(stats),
        |_, _| {},
    )
    .unwrap();
    let mut out = Output::for_port(&PortInfo {
        ty: PortType::Frames,
        rate_hz: 1187.5,
        max_items: 64,
        hold_items: 0,
    });
    let PortVec::Frames(buf) = &mut out.data else {
        unreachable!("a frames port")
    };
    for (i, (bit, tree)) in frames.iter().enumerate() {
        let mut info = FrameInfo::new(i as u64, *bit, 0);
        info.bit_len = 64;
        info.check = CrcStatus::Valid;
        info.layers = Some(Arc::clone(tree));
        buf.push(&[0u8; 8], info);
    }
    let ctx = FrameCtx {
        decoder: "recipe:rds@1",
        frame_model: "rds",
        emitter_id: Some(station),
        channel_hz: CENTER,
        channels_hz: &[],
        recipe_version: 1,
        edit_rev: 0,
    };
    let t_of = |bit: f64| Timestamp::from_unix_nanos(T0_NS + (bit / 1187.5 * 1e9) as i64);
    assert_eq!(sink.publish(&out, &ctx, &t_of), frames.len() as u64);
    drop(sink);
}

/// The `station` rows linked to `station`: `(PS text, identity value if committed)`.
fn station_rows(repo: &Repository, station: EmitterId) -> Vec<(String, Option<String>)> {
    let mut rows: Vec<_> = repo
        .decodes_for_identity(&pi_1704())
        .unwrap()
        .into_iter()
        .chain(linked_rows(repo, station))
        .filter(|d| d.frame_model == "rds-ps")
        .map(|d| (d.t, d))
        .collect();
    rows.sort_by_key(|(t, d)| (*t, d.id));
    rows.dedup_by_key(|(_, d)| d.id);
    rows.into_iter()
        .map(|(_, d)| {
            (
                d.content.as_ref().unwrap()["text"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
                d.identity.map(|i| i.value),
            )
        })
        .collect()
}

/// T-962 round 3, the gate red (`listen_recipe_parity`): a `station` (PS) row is emitted once
/// per completed four-segment PS cycle — about one per second on a real station — so counted
/// per writer it could never put 10 votes in 5 s, and a clean station's PS never carried its PI.
/// The pipeline's outputs share one tally: the `group-info` output's per-group rows (11.4/s)
/// commit the PI, and the station's PS rows after it carry the identity.
#[test]
fn t962_station_ps_commits_on_the_pipelines_groups() {
    let dir = TempDir::new("t962-recipe-station");
    let db = dir.0.join("hk.sqlite");
    let station = {
        let mut repo = Repository::open(&db).unwrap();
        seed_station(&mut repo)
    };
    // Three seconds of clean air: 34 groups back to back, a PS completing every 11th group.
    let groups: Vec<(u64, Arc<LayerTree>)> =
        (0..34u64).map(|i| (104 * i, group_tree(PI))).collect();
    let ps: Vec<(u64, Arc<LayerTree>)> = [11u64, 22, 33]
        .iter()
        .map(|&i| (104 * i, ps_tree(PI, "RADIO 1 ")))
        .collect();

    // Counted per writer (the round-2 rule; still what a PS-only recipe gets): the PS rows alone
    // are three votes in three seconds, and none commits.
    feed_output(
        &db,
        station,
        "station",
        &Arc::new(PipelineStats::default()),
        &ps,
    );
    let repo = Repository::open(&db).unwrap();
    assert!(
        station_rows(&repo, station)
            .iter()
            .all(|(_, id)| id.is_none()),
        "[{T962}] {:?}",
        station_rows(&repo, station)
    );
    drop(repo);

    // One pipeline: its group rows and PS rows share the tally.
    let dir = TempDir::new("t962-recipe-station-shared");
    let db = dir.0.join("hk.sqlite");
    let station = {
        let mut repo = Repository::open(&db).unwrap();
        seed_station(&mut repo)
    };
    let stats = Arc::new(PipelineStats::default());
    feed_output(&db, station, "group-info", &stats, &groups);
    feed_output(&db, station, "station", &stats, &ps);
    let repo = Repository::open(&db).unwrap();
    let rows = station_rows(&repo, station);
    assert_eq!(rows.len(), 3, "[{T962}] {rows:?}");
    for (text, id) in &rows {
        assert_eq!(text, "RADIO 1 ");
        assert_eq!(
            id.as_deref(),
            Some("1704"),
            "[{T962}] a clean station's PS, after ten agreeing groups in under a second, carries \
             its PI: {rows:?}"
        );
    }
    let e = repo.emitter_by_identity(&pi_1704()).unwrap().unwrap();
    assert_eq!(
        identity_decision(&repo, e.id).map(|(r, _)| r),
        Some(ConfirmRoute::Identity)
    );
}

/// T-962 round 3, the property sharing must keep: outputs reporting the **same frames** add no
/// evidence. Five groups, each reported by two outputs (its group row and a PS row at the same
/// capture time), are five votes — ten rows, no identity.
#[test]
fn t962_outputs_reporting_the_same_frames_count_each_once() {
    let dir = TempDir::new("t962-recipe-dedupe");
    let db = dir.0.join("hk.sqlite");
    let station = {
        let mut repo = Repository::open(&db).unwrap();
        seed_station(&mut repo)
    };
    let stats = Arc::new(PipelineStats::default());
    let bits: Vec<u64> = (0..5u64).map(|i| 104 * i).collect();
    let groups: Vec<_> = bits.iter().map(|&b| (b, group_tree(PI))).collect();
    let ps: Vec<_> = bits.iter().map(|&b| (b, ps_tree(PI, "RADIO 1 "))).collect();
    feed_output(&db, station, "group-info", &stats, &groups);
    feed_output(&db, station, "station", &stats, &ps);
    let repo = Repository::open(&db).unwrap();
    assert!(
        repo.emitter_by_identity(&pi_1704()).unwrap().is_none(),
        "[{T962}] ten rows over five frames placed an identity"
    );
    let rows = linked_rows(&repo, station);
    assert_eq!(rows.len(), 10, "[{T962}] every reading is kept");
    for d in &rows {
        assert!(d.identity.is_none(), "{d:?}");
        assert!(
            d.metadata["identity_votes_in_window"].as_u64().unwrap() <= 5,
            "{d:?}"
        );
    }
    assert_eq!(identity_decision(&repo, station), None, "[{T962}]");
}
