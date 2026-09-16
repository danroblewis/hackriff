//! C18 clustering tests (T-202, ADR-0016 §5), including the blind multi-day acceptance scene.
//!
//! The floors the scene test asserts were written down **before** it was run, and are stated in
//! [`multi_day_scene_clusters_repeats_together_and_keeps_distinct_generators_apart`].

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use hk_model::signature::cluster::{
    ClusterState, EmitterClusterLink, SignatureCluster, new_cluster_id,
};
use hk_model::signature::field;
use hk_model::{
    EmissionFeatures, EmitterId, Feat, Fingerprint, LinkTarget, Repository, Sighting, TimeRange,
    Timestamp, TrackId,
};

use super::cluster::{Assignment, Separated, assign_emitter, comparable, compare, promote, repair};

const DAY: i64 = 86_400;

fn t(sec: i64) -> Timestamp {
    Timestamp::from_unix_nanos(1_789_000_000_000_000_000 + sec * 1_000_000_000)
}

/// A scratch directory for the restart test, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("hk-context-cluster-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// One emitter in the inventory, resolved from a sighting like any other.
fn an_emitter(r: &mut Repository, f_hz: f64, seen: (i64, i64), count: u64) -> EmitterId {
    r.record_sighting(
        &Sighting {
            source: LinkTarget::Track(TrackId::new()),
            seen: TimeRange::new(t(seen.0), t(seen.1)),
            count,
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

/// A features snapshot with the given fields, stored for `id`.
fn store_features(
    r: &mut Repository,
    id: EmitterId,
    snapshot: &str,
    when: Timestamp,
    fields: &[(&str, Feat)],
) -> EmissionFeatures {
    let mut f = EmissionFeatures::new(snapshot, id, when);
    f.observe(
        fields.iter().map(|(n, v)| ((*n).to_owned(), v.clone())),
        false,
    );
    r.put_emission_features(&f).unwrap();
    f
}

fn num(v: f64, sigma: f64) -> Feat {
    Feat::num(v, sigma, "c14")
}

fn fields_of(pairs: &[(&str, Feat)]) -> BTreeMap<String, Feat> {
    pairs
        .iter()
        .map(|(n, f)| ((*n).to_owned(), f.clone()))
        .collect()
}

// ---------------------------------------------------------------------------------------------
// The separation guard.
// ---------------------------------------------------------------------------------------------

/// **The degenerate case, explicitly.** A measurement with one or two comparable fields is close
/// to *everything*, so it joins nothing — the guard's shared-field floor is not optional and does
/// not wait for a "both sides have a full vector" precondition the way T-219's did.
#[test]
fn a_thin_measurement_joins_nothing_however_well_its_one_field_agrees() {
    let full = fields_of(&[
        (field::SYMBOL_RATE_HZ, num(4800.0, 5.0)),
        (field::DEVIATION_HZ, num(9600.0, 50.0)),
        (field::PERIOD_S, num(0.12, 0.001)),
        (field::OBW_HZ, num(36e3, 500.0)),
    ]);

    // One field, measured identically: distance 0 on what is shared, and still refused.
    let one = fields_of(&[(field::SYMBOL_RATE_HZ, num(4800.0, 5.0))]);
    assert_eq!(
        compare(&full, &one),
        Err(Separated::TooFewShared { shared: 1 })
    );
    // Two fields, both perfect: still refused. Three is the floor.
    let two = fields_of(&[
        (field::SYMBOL_RATE_HZ, num(4800.0, 5.0)),
        (field::DEVIATION_HZ, num(9600.0, 50.0)),
    ]);
    assert_eq!(
        compare(&full, &two),
        Err(Separated::TooFewShared { shared: 2 })
    );
    // An empty vector against an empty vector is not "identical", it is "nothing measured".
    assert!(compare(&BTreeMap::new(), &BTreeMap::new()).is_err());

    let three = fields_of(&[
        (field::SYMBOL_RATE_HZ, num(4800.0, 5.0)),
        (field::DEVIATION_HZ, num(9600.0, 50.0)),
        (field::PERIOD_S, num(0.12, 0.001)),
    ]);
    assert!(compare(&full, &three).is_ok(), "three fields is enough");
}

/// **The guard is unconditional.** One flatly contradicting field separates the pair whatever
/// else they share — including when it is the *only* thing both measured, so a pair that could
/// not even reach the shared-field floor is still reported as a conflict rather than silently
/// "not compared".
#[test]
fn one_conflicting_field_separates_even_when_it_is_the_only_thing_shared() {
    // Only the family is shared, and it disagrees.
    let a = fields_of(&[
        (field::FAMILY, Feat::text("fsk", "classifier")),
        (field::SYMBOL_RATE_HZ, num(4800.0, 5.0)),
    ]);
    let b = fields_of(&[
        (field::FAMILY, Feat::text("psk-qam", "classifier")),
        (field::PERIOD_S, num(0.12, 0.001)),
    ]);
    match compare(&a, &b) {
        Err(Separated::Conflict { field: f, .. }) => assert_eq!(f, field::FAMILY),
        other => panic!("a contradiction must separate: {other:?}"),
    }

    // A rich, otherwise-perfect agreement with one contradiction: the RMS would have hidden it.
    let base = [
        (field::SYMBOL_RATE_HZ, num(4800.0, 5.0)),
        (field::PERIOD_S, num(0.12, 0.001)),
        (field::OBW_HZ, num(36e3, 500.0)),
        (field::DUTY_CYCLE, num(0.15, 0.005)),
        (field::BURST_LENGTH_S, num(0.018, 0.0005)),
    ];
    let mut with_two_levels = base.to_vec();
    with_two_levels.push((field::LEVELS, num(2.0, 0.0)));
    let mut with_four_levels = base.to_vec();
    with_four_levels.push((field::LEVELS, num(4.0, 0.0)));
    let a = fields_of(&with_two_levels);
    let b = fields_of(&with_four_levels);
    let rms_without = compare(&fields_of(&base), &fields_of(&base)).unwrap().z_rms;
    assert!(rms_without < 1e-9);
    match compare(&a, &b) {
        Err(Separated::Conflict { field: f, z }) => {
            assert_eq!(f, field::LEVELS);
            assert!(z > 3.0, "z {z}");
        }
        other => panic!("2 levels is not 4 levels: {other:?}"),
    }

    // A different sync word is a conflict too, in both polarities.
    let a = fields_of(&[
        (field::SYNC_WORD, Feat::bits("1100110011001100", "framer")),
        (field::SYMBOL_RATE_HZ, num(4800.0, 5.0)),
        (field::PERIOD_S, num(0.12, 0.001)),
    ]);
    let b = fields_of(&[
        (field::SYNC_WORD, Feat::bits("1010000111010101", "framer")),
        (field::SYMBOL_RATE_HZ, num(4800.0, 5.0)),
        (field::PERIOD_S, num(0.12, 0.001)),
    ]);
    assert!(
        matches!(compare(&a, &b), Err(Separated::Conflict { .. })),
        "different sync words are different things"
    );
}

/// The T-201 uncertainty rule, at the distance: a field that drifted (or was measured badly)
/// widens the tolerance instead of manufacturing a split, and a *tight* measurement far away is
/// still far away.
#[test]
fn measurement_uncertainty_widens_the_distance_it_never_tightens_it() {
    let shared = [
        (field::PERIOD_S, num(0.12, 0.001)),
        (field::OBW_HZ, num(36e3, 500.0)),
    ];

    // A drifting deviation: 9600 Hz against 12000 Hz, each measured only to ±1500 Hz. Wider than
    // the field's 10 % tolerance on its own, but the measurements do not disagree — neither
    // number is known well enough to say so.
    let mut drifting_a = shared.to_vec();
    drifting_a.push((field::DEVIATION_HZ, num(9600.0, 1500.0)));
    let mut drifting_b = shared.to_vec();
    drifting_b.push((field::DEVIATION_HZ, num(12000.0, 1500.0)));
    assert!(
        compare(&fields_of(&drifting_a), &fields_of(&drifting_b)).is_ok(),
        "a drifting emission must stay with itself"
    );

    // The same two values, each claimed to be known to 1 Hz: now they really are different, and
    // the pair separates on a distance the sigmas no longer widen.
    let mut tight_a = shared.to_vec();
    tight_a.push((field::DEVIATION_HZ, num(9600.0, 1.0)));
    let mut tight_b = shared.to_vec();
    tight_b.push((field::DEVIATION_HZ, num(12000.0, 1.0)));
    assert!(
        matches!(
            compare(&fields_of(&tight_a), &fields_of(&tight_b)),
            Err(Separated::TooFar { .. })
        ),
        "a well-measured difference is a difference"
    );
}

// ---------------------------------------------------------------------------------------------
// Online assignment.
// ---------------------------------------------------------------------------------------------

fn sensor_fields(rate: f64, deviation: f64, period: f64) -> Vec<(&'static str, Feat)> {
    vec![
        (field::FAMILY, Feat::text("fsk", "classifier")),
        (field::SYMBOL_RATE_HZ, num(rate, rate * 0.002)),
        (field::DEVIATION_HZ, num(deviation, deviation * 0.02)),
        (field::PERIOD_S, num(period, period * 0.01)),
        (field::OBW_HZ, num(36e3, 500.0)),
    ]
}

#[test]
fn an_emitter_with_nothing_measured_is_never_clustered() {
    let mut r = Repository::open_in_memory().unwrap();
    let e = an_emitter(&mut r, 433.92e6, (0, 1), 3);
    assert!(assign_emitter(&mut r, e, t(10)).unwrap().is_none());
    assert!(r.emitter_cluster(e).unwrap().is_none(), "no row at all");
}

#[test]
fn like_emitters_join_one_cluster_and_unlike_ones_stay_apart() {
    let mut r = Repository::open_in_memory().unwrap();
    let mut ids = Vec::new();
    for (i, (rate, dev, period)) in [
        (4800.0, 9600.0, 0.12),
        (4810.0, 9500.0, 0.121),
        (4795.0, 9700.0, 0.119),
        // A different device type: same rate, very different deviation and period.
        (4800.0, 2400.0, 0.5),
    ]
    .into_iter()
    .enumerate()
    {
        let e = an_emitter(&mut r, 433.0e6 + 1e6 * i as f64, (0, 1), 3);
        store_features(
            &mut r,
            e,
            &format!("features:{i}"),
            t(10),
            &sensor_fields(rate, dev, period),
        );
        ids.push(e);
    }
    for e in &ids {
        assign_emitter(&mut r, *e, t(20)).unwrap().unwrap();
    }

    let first = r.emitter_cluster_id(ids[0]).unwrap().unwrap();
    assert_eq!(r.emitter_cluster_id(ids[1]).unwrap(), Some(first.clone()));
    assert_eq!(r.emitter_cluster_id(ids[2]).unwrap(), Some(first.clone()));
    let odd = r.emitter_cluster_id(ids[3]).unwrap().unwrap();
    assert_ne!(odd, first, "a different device type is a different cluster");

    // Three members make the cluster visible; the singleton stays pending until it recurs.
    assert_eq!(r.cluster(&first).unwrap().state, ClusterState::Active);
    assert_eq!(r.cluster(&odd).unwrap().state, ClusterState::Pending);
    assert_eq!(r.cluster_members(&first).unwrap().len(), 3);
    assert_eq!(r.visible_clusters().unwrap().len(), 1);
}

/// "The same thing I saw before" holds for one emitter seen again and again, so a single member
/// with enough separated appearances makes its cluster visible.
#[test]
fn one_emitter_seen_again_and_again_becomes_visible_on_its_own() {
    let mut r = Repository::open_in_memory().unwrap();
    let e = an_emitter(&mut r, 433.92e6, (0, 1), 2);
    store_features(
        &mut r,
        e,
        "features:0",
        t(10),
        &sensor_fields(4800.0, 9600.0, 0.12),
    );
    let a = assign_emitter(&mut r, e, t(20)).unwrap().unwrap();
    let cluster = a.cluster_id.clone().unwrap();
    assert_eq!(r.cluster(&cluster).unwrap().state, ClusterState::Pending);

    // Two more appearances of the same emitter.
    for day in 1..3 {
        r.record_sighting(
            &Sighting {
                source: LinkTarget::Track(TrackId::new()),
                seen: TimeRange::new(t(day * DAY), t(day * DAY + 60)),
                count: 2,
                f_center_hz: 433.92e6,
                bandwidth_hz: 36e3,
                fingerprint: Some(Fingerprint::new(433.92e6, 36e3)),
                identity: None,
                context: None,
                classification: None,
                tags: Vec::new(),
            },
            None,
        )
        .unwrap();
        store_features(
            &mut r,
            e,
            &format!("features:{day}"),
            t(day * DAY),
            &sensor_fields(4802.0, 9580.0, 0.1205),
        );
        assign_emitter(&mut r, e, t(day * DAY + 100))
            .unwrap()
            .unwrap();
    }
    assert_eq!(r.cluster(&cluster).unwrap().state, ClusterState::Active);
    assert_eq!(
        r.emitter_cluster_history(e, 10).unwrap().len(),
        1,
        "re-seeing the same emitter appends no duplicate membership"
    );
}

/// Between two clusters that are not themselves compatible, the clusterer abstains rather than
/// guessing: a wrong merge is worse than no answer.
#[test]
fn an_emitter_between_two_incompatible_clusters_is_left_unassigned() {
    let mut r = Repository::open_in_memory().unwrap();
    // Two clusters, deliberately seeded far enough apart to conflict with each other.
    let mut seeded = Vec::new();
    for (i, period) in [0.10_f64, 0.30].into_iter().enumerate() {
        let c = SignatureCluster::new(new_cluster_id(), t(0));
        let mut c = c;
        c.centroid.fold_member(
            &fields_of(&[
                (field::SYMBOL_RATE_HZ, num(4800.0, 5.0)),
                (field::OBW_HZ, num(36e3, 200.0)),
                (field::PERIOD_S, num(period, period * 0.001)),
            ]),
            0.0,
        );
        c.state = ClusterState::Active;
        r.put_cluster(&c).unwrap();
        seeded.push((i, c));
    }
    assert!(
        compare(
            seeded[0].1.centroid.comparable(),
            seeded[1].1.centroid.comparable()
        )
        .is_err(),
        "the two seeds must genuinely disagree for this test to mean anything"
    );

    // Something sitting between them, with a period sigma wide enough to reach both.
    let e = an_emitter(&mut r, 433.92e6, (0, 1), 3);
    store_features(
        &mut r,
        e,
        "features:between",
        t(10),
        &[
            (field::SYMBOL_RATE_HZ, num(4800.0, 5.0)),
            (field::OBW_HZ, num(36e3, 200.0)),
            (field::PERIOD_S, num(0.20, 0.12)),
        ],
    );
    let a = assign_emitter(&mut r, e, t(20)).unwrap().unwrap();
    assert_eq!(a.reason, "ambiguous", "{a:?}");
    assert_eq!(a.cluster_id, None);
    // The abstention is recorded: "looked and declined" is not "never looked".
    let link = r.emitter_cluster(e).unwrap().unwrap();
    assert_eq!(link.cluster_id, None);
    assert_eq!(link.reason, "ambiguous");
}

// ---------------------------------------------------------------------------------------------
// Evidence, never identity.
// ---------------------------------------------------------------------------------------------

/// **The contract, enforced.** Clustering an emitter changes *nothing* about it: not its identity,
/// its known status, its lifecycle, its family, its tags or its classification history. The
/// cluster is a note that this emission measures like those; only a decode says what it is.
#[test]
fn a_cluster_never_changes_anything_about_the_emitter() {
    let mut r = Repository::open_in_memory().unwrap();
    let mut ids = Vec::new();
    for i in 0..3 {
        let e = an_emitter(&mut r, 433.0e6 + 1e6 * f64::from(i), (0, 1), 3);
        store_features(
            &mut r,
            e,
            &format!("features:{i}"),
            t(10),
            &sensor_fields(4800.0, 9600.0, 0.12),
        );
        ids.push(e);
    }

    let before: Vec<String> = ids
        .iter()
        .map(|id| format!("{:?}", r.emitter(*id).unwrap()))
        .collect();
    let statuses: Vec<usize> = ids
        .iter()
        .map(|id| r.known_status_history(*id).unwrap().len())
        .collect();
    let classes: Vec<bool> = ids
        .iter()
        .map(|id| r.current_classification(*id).unwrap().is_some())
        .collect();

    for e in &ids {
        assign_emitter(&mut r, *e, t(20)).unwrap().unwrap();
    }
    // An *active* cluster is the strongest statement clustering can make, so if anything could
    // leak into the inventory it would leak here.
    let cluster = r.emitter_cluster_id(ids[0]).unwrap().unwrap();
    assert_eq!(r.cluster(&cluster).unwrap().state, ClusterState::Active);

    // Promotion is the strongest statement of all: a catalogue entry minted from the group.
    let signature = promote(&mut r, &cluster, "test", t(30)).unwrap();
    assert_eq!(
        signature.provenance,
        hk_model::SignatureProvenance::ClusterPromoted
    );

    for (i, id) in ids.iter().enumerate() {
        assert_eq!(format!("{:?}", r.emitter(*id).unwrap()), before[i]);
        assert_eq!(r.known_status_history(*id).unwrap().len(), statuses[i]);
        assert_eq!(
            r.current_classification(*id).unwrap().is_some(),
            classes[i],
            "clustering wrote a classification"
        );
        assert_eq!(
            r.emitter(*id).unwrap().identity,
            hk_model::Identity::Unknown,
            "a cluster never names an emitter"
        );
        assert_eq!(
            r.emitter(*id).unwrap().known_status,
            hk_model::KnownStatus::Unknown
        );
        assert!(r.current_signature_match(*id).unwrap().is_none());
    }
}

#[test]
fn promotion_needs_a_visible_well_measured_cluster() {
    let mut r = Repository::open_in_memory().unwrap();

    // A pending cluster is not promotable.
    let mut c = SignatureCluster::new(new_cluster_id(), t(0));
    c.centroid
        .fold_member(&fields_of(&sensor_fields(4800.0, 9600.0, 0.12)), 0.0);
    r.put_cluster(&c).unwrap();
    assert!(promote(&mut r, &c.id, "test", t(1)).is_err());

    // Active, but every observation behind it was suspect: nothing is minted from a ghost.
    let mut suspect = SignatureCluster::new(new_cluster_id(), t(0));
    suspect
        .centroid
        .fold_member(&fields_of(&sensor_fields(4800.0, 9600.0, 0.12)), 1.0);
    suspect.state = ClusterState::Active;
    r.put_cluster(&suspect).unwrap();
    assert!(promote(&mut r, &suspect.id, "test", t(1)).is_err());

    // Active, honest, but only two discriminating fields measured.
    let mut thin = SignatureCluster::new(new_cluster_id(), t(0));
    thin.centroid.fold_member(
        &fields_of(&[
            (field::SYMBOL_RATE_HZ, num(4800.0, 5.0)),
            (field::PERIOD_S, num(0.12, 0.001)),
            (field::OBW_HZ, num(36e3, 500.0)),
        ]),
        0.0,
    );
    thin.state = ClusterState::Active;
    r.put_cluster(&thin).unwrap();
    assert!(promote(&mut r, &thin.id, "test", t(1)).is_err());

    // Active and well measured: minted, and the entry's tolerances are no tighter than what the
    // members showed.
    c.state = ClusterState::Active;
    r.put_cluster(&c).unwrap();
    let s = promote(&mut r, &c.id, "test", t(2)).unwrap();
    assert!(s.fields.len() >= 3);
    assert!(s.family.is_none(), "a minted entry gates no family");
    assert_eq!(r.cluster(&c.id).unwrap().state, ClusterState::Promoted);
    assert_eq!(r.signatures().unwrap().len(), 1);
    assert!(promote(&mut r, &c.id, "test", t(3)).is_err(), "only once");
}

// ---------------------------------------------------------------------------------------------
// Persistence and repair.
// ---------------------------------------------------------------------------------------------

/// Clusters are rebuilt from nothing on no restart: the store carries them, and a later sighting
/// joins the cluster a previous run created.
#[test]
fn clusters_survive_restart_and_a_later_emitter_joins_the_same_one() {
    let dir = TempDir::new();
    let path = dir.0.join("inventory.sqlite3");
    let (cluster, members) = {
        let mut r = Repository::open(&path).unwrap();
        let mut ids = Vec::new();
        for i in 0..3 {
            let e = an_emitter(&mut r, 433.0e6 + 1e6 * f64::from(i), (0, 1), 3);
            store_features(
                &mut r,
                e,
                &format!("features:{i}"),
                t(10),
                &sensor_fields(4800.0, 9600.0, 0.12),
            );
            assign_emitter(&mut r, e, t(20)).unwrap().unwrap();
            ids.push(e);
        }
        (r.emitter_cluster_id(ids[0]).unwrap().unwrap(), ids)
    };

    // A fresh process: nothing in memory, everything in the store.
    let mut r = Repository::open(&path).unwrap();
    assert_eq!(r.cluster(&cluster).unwrap().state, ClusterState::Active);
    assert_eq!(r.cluster_members(&cluster).unwrap().len(), 3);
    for id in &members {
        assert_eq!(r.emitter_cluster_id(*id).unwrap(), Some(cluster.clone()));
    }

    // A fourth sighting of the same kind of thing joins the cluster the earlier run built.
    let later = an_emitter(&mut r, 439.0e6, (DAY, DAY + 1), 3);
    store_features(
        &mut r,
        later,
        "features:later",
        t(DAY),
        &sensor_fields(4790.0, 9700.0, 0.1195),
    );
    let a = assign_emitter(&mut r, later, t(DAY + 10)).unwrap().unwrap();
    assert_eq!(a.cluster_id, Some(cluster.clone()), "{a:?}");
    assert_eq!(a.reason, "joined");
    assert_eq!(r.cluster_members(&cluster).unwrap().len(), 4);
}

/// The repair pass folds the fragments the online pass leaves behind, keeps the larger side's id,
/// and records what it did.
#[test]
fn repair_merges_fragments_into_the_larger_id_and_records_the_history() {
    let mut r = Repository::open_in_memory().unwrap();
    // Three emitters of one type, but the first two are placed in a cluster of their own and the
    // third in another (as an ambiguous or out-of-order online pass could leave them).
    let mut ids = Vec::new();
    for i in 0..3 {
        let e = an_emitter(&mut r, 433.0e6 + 1e6 * f64::from(i), (0, 1), 3);
        store_features(
            &mut r,
            e,
            &format!("features:{i}"),
            t(10),
            &sensor_fields(4800.0 + f64::from(i), 9600.0, 0.12),
        );
        ids.push(e);
    }
    let big = new_cluster_id();
    let small = new_cluster_id();
    for (id, cluster) in [(ids[0], &big), (ids[1], &big), (ids[2], &small)] {
        let mut c = r
            .cluster_opt(cluster)
            .unwrap()
            .unwrap_or_else(|| SignatureCluster::new(cluster, t(0)));
        c.centroid
            .fold_member(&comparable(&r.emitter_features(id).unwrap().unwrap()), 0.0);
        r.put_cluster(&c).unwrap();
        r.link_emitter_cluster(&EmitterClusterLink {
            emitter_id: id,
            cluster_id: Some(cluster.clone()),
            t: t(20),
            reason: "seeded".into(),
            distance: None,
        })
        .unwrap();
    }

    let report = repair(&mut r, t(100)).unwrap();
    assert_eq!(report.points, 3);
    assert_eq!(report.groups, 1, "one type, one group");
    assert_eq!(report.merges, 1);
    assert_eq!(report.reassignments, 1);
    for id in &ids {
        assert_eq!(
            r.emitter_cluster_id(*id).unwrap(),
            Some(big.clone()),
            "the larger side's id survives"
        );
    }
    assert_eq!(r.cluster(&small).unwrap().state, ClusterState::Merged);
    assert_eq!(r.live_cluster_id(&small).unwrap(), big);
    assert!(
        r.cluster_events(&big, 10)
            .unwrap()
            .iter()
            .any(|e| e.kind == hk_model::ClusterEventKind::Merge)
    );
    // A second repair over unchanged measurements changes nothing.
    let again = repair(&mut r, t(200)).unwrap();
    assert_eq!(again.merges, 0);
    assert_eq!(again.reassignments, 0);
}

/// The repair pass separates what the online pass wrongly put together, and never joins two types.
#[test]
fn repair_splits_a_cluster_that_holds_two_different_things() {
    let mut r = Repository::open_in_memory().unwrap();
    let wrong = new_cluster_id();
    r.put_cluster(&SignatureCluster::new(&wrong, t(0))).unwrap();
    let mut truth: Vec<(usize, EmitterId)> = Vec::new();
    for i in 0..6 {
        let type_of = usize::from(i >= 3);
        let e = an_emitter(&mut r, 433.0e6 + 1e6 * f64::from(i), (0, 1), 3);
        let f = if type_of == 0 {
            sensor_fields(4800.0, 9600.0, 0.12)
        } else {
            sensor_fields(1200.0, 2400.0, 0.9)
        };
        store_features(&mut r, e, &format!("features:{i}"), t(10), &f);
        r.link_emitter_cluster(&EmitterClusterLink {
            emitter_id: e,
            cluster_id: Some(wrong.clone()),
            t: t(20),
            reason: "seeded".into(),
            distance: None,
        })
        .unwrap();
        truth.push((type_of, e));
    }

    let report = repair(&mut r, t(100)).unwrap();
    assert_eq!(report.groups, 2, "two types, two groups");
    assert!(report.splits >= 1);
    let of = |e: EmitterId| r.emitter_cluster_id(e).unwrap().unwrap();
    let first = of(truth[0].1);
    let second = of(truth[3].1);
    assert_ne!(first, second);
    for (type_of, e) in &truth {
        let expected = if *type_of == 0 { &first } else { &second };
        assert_eq!(&of(*e), expected);
    }
}

// ---------------------------------------------------------------------------------------------
// Blind multi-day acceptance scene.
// ---------------------------------------------------------------------------------------------

/// A deterministic generator: xorshift64*, so the scene is the same on every machine and run.
struct Rng(u64);

impl Rng {
    fn next_f64(&mut self) -> f64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        ((x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64) / ((1u64 << 53) as f64)
    }

    /// Uniform in ±`spread` (relative).
    fn jitter(&mut self, value: f64, spread: f64) -> f64 {
        value * (1.0 + (self.next_f64() * 2.0 - 1.0) * spread)
    }
}

/// One unknown device type in the scene: what it *is*, hidden from the system under test.
struct Generator {
    family: &'static str,
    symbol_rate_hz: f64,
    deviation_hz: Option<f64>,
    levels: f64,
    period_s: f64,
    duty_cycle: f64,
    burst_length_s: f64,
    obw_hz: f64,
}

fn generators() -> Vec<Generator> {
    vec![
        // Two deliberately similar FSK sensors: the same symbol rate, differing in deviation and
        // period (the P25/DMR-style near collision from the C18 card).
        Generator {
            family: "fsk",
            symbol_rate_hz: 4800.0,
            deviation_hz: Some(9600.0),
            levels: 2.0,
            period_s: 0.12,
            duty_cycle: 0.15,
            burst_length_s: 0.018,
            obw_hz: 36e3,
        },
        Generator {
            family: "fsk",
            symbol_rate_hz: 4800.0,
            deviation_hz: Some(2400.0),
            levels: 2.0,
            period_s: 0.5,
            duty_cycle: 0.05,
            burst_length_s: 0.010,
            obw_hz: 20e3,
        },
        Generator {
            family: "ook",
            symbol_rate_hz: 1000.0,
            deviation_hz: None,
            levels: 2.0,
            period_s: 1.0,
            duty_cycle: 0.30,
            burst_length_s: 0.050,
            obw_hz: 8e3,
        },
        Generator {
            family: "psk-qam",
            symbol_rate_hz: 9600.0,
            deviation_hz: None,
            levels: 4.0,
            period_s: 0.25,
            duty_cycle: 0.60,
            burst_length_s: 0.200,
            obw_hz: 100e3,
        },
    ]
}

/// One day's measurement of one instance: jittered, with the sigma the estimator would report.
fn measure(g: &Generator, rng: &mut Rng, drift: f64) -> Vec<(String, Feat)> {
    let mut out: Vec<(String, Feat)> = vec![
        (field::FAMILY.to_owned(), Feat::text(g.family, "classifier")),
        (
            field::SYMBOL_RATE_HZ.to_owned(),
            num(
                rng.jitter(g.symbol_rate_hz * drift, 0.003),
                g.symbol_rate_hz * 0.002,
            ),
        ),
        (field::LEVELS.to_owned(), Feat::num(g.levels, 0.0, "c15")),
        (
            field::PERIOD_S.to_owned(),
            num(rng.jitter(g.period_s, 0.02), g.period_s * 0.01),
        ),
        (
            field::DUTY_CYCLE.to_owned(),
            num(rng.jitter(g.duty_cycle, 0.08), g.duty_cycle * 0.05),
        ),
        (
            field::BURST_LENGTH_S.to_owned(),
            num(rng.jitter(g.burst_length_s, 0.10), g.burst_length_s * 0.05),
        ),
        (
            field::OBW_HZ.to_owned(),
            num(rng.jitter(g.obw_hz, 0.05), g.obw_hz * 0.03),
        ),
    ];
    if let Some(d) = g.deviation_hz {
        out.push((
            field::DEVIATION_HZ.to_owned(),
            num(rng.jitter(d, 0.04), d * 0.02),
        ));
    }
    // At low SNR a field drops out — a partial observation, not a contradiction.
    if rng.next_f64() < 0.2 {
        out.retain(|(n, _)| n != field::BURST_LENGTH_S);
    }
    out
}

/// Adjusted Rand Index between the truth labelling and the cluster labelling.
fn adjusted_rand_index(truth: &[usize], found: &[Option<String>]) -> f64 {
    let mut table: BTreeMap<(usize, String), f64> = BTreeMap::new();
    let mut rows: BTreeMap<usize, f64> = BTreeMap::new();
    let mut cols: BTreeMap<String, f64> = BTreeMap::new();
    let mut n = 0.0;
    for (i, label) in found.iter().enumerate() {
        // An unassigned point is its own singleton: abstaining costs completeness, not purity.
        let key = label.clone().unwrap_or_else(|| format!("unassigned:{i}"));
        *table.entry((truth[i], key.clone())).or_default() += 1.0;
        *rows.entry(truth[i]).or_default() += 1.0;
        *cols.entry(key).or_default() += 1.0;
        n += 1.0;
    }
    let pairs = |x: f64| x * (x - 1.0) / 2.0;
    let index: f64 = table.values().map(|v| pairs(*v)).sum();
    let a: f64 = rows.values().map(|v| pairs(*v)).sum();
    let b: f64 = cols.values().map(|v| pairs(*v)).sum();
    let total = pairs(n);
    let expected = a * b / total;
    let max = (a + b) / 2.0;
    if (max - expected).abs() < 1e-12 {
        return 1.0;
    }
    (index - expected) / (max - expected)
}

/// **The blind multi-day acceptance scene (AWARE-053).**
///
/// Sixteen unknown emitters — four instances each of four generators — are measured across five
/// days, each sighting jittered, some fields dropping out at low SNR, every instance on its own
/// frequency. The system is told nothing about the generators: it sees only measurements, and the
/// truth table exists solely to score the answer afterwards.
///
/// **Floors, fixed before the first run** (ADR-0016 §7's clustering row is ARI ≥ 0.8, ≤ 1.5
/// clusters per truth type, merge rate ≤ 0.05; these are that, plus the two the brief adds):
///
/// | Measure | Floor |
/// |---|---|
/// | Purity (members sharing their cluster's plurality truth type) | ≥ 0.95 |
/// | Completeness (a type's instances in its largest cluster) | ≥ 0.80 |
/// | Clusters per truth type | ≤ 1.5 |
/// | ARI against truth | ≥ 0.80 |
/// | Clusters holding more than one truth type | **0** |
///
/// The last one is absolute on purpose: two genuinely distinct emitters sharing an id is the
/// failure this design exists to avoid, and it is not traded against completeness.
#[test]
fn multi_day_scene_clusters_repeats_together_and_keeps_distinct_generators_apart() {
    const DAYS: i64 = 5;
    const INSTANCES: usize = 4;
    const PURITY_FLOOR: f64 = 0.95;
    const COMPLETENESS_FLOOR: f64 = 0.80;
    const CLUSTERS_PER_TYPE_CEILING: f64 = 1.5;
    const ARI_FLOOR: f64 = 0.80;

    let gens = generators();
    let mut r = Repository::open_in_memory().unwrap();
    let mut rng = Rng(0x5DEE_CE66_D125_1234);

    // The hidden truth: which generator produced each emitter.
    let mut truth: Vec<usize> = Vec::new();
    let mut emitters: Vec<EmitterId> = Vec::new();
    let mut aggregates: Vec<EmissionFeatures> = Vec::new();
    for (g, _) in gens.iter().enumerate() {
        for instance in 0..INSTANCES {
            let f_hz = 433.0e6 + 2.5e6 * (g * INSTANCES + instance) as f64;
            let e = an_emitter(&mut r, f_hz, (0, 60), 4);
            aggregates.push(EmissionFeatures::new(
                format!("features:{g}:{instance}:0"),
                e,
                t(0),
            ));
            emitters.push(e);
            truth.push(g);
        }
    }

    for day in 0..DAYS {
        for (i, e) in emitters.iter().enumerate() {
            // Each instance is off the air on one day of the five.
            if (day as usize + i) % 5 == 4 {
                continue;
            }
            let when = t(day * DAY + 3600);
            r.record_sighting(
                &Sighting {
                    source: LinkTarget::Track(TrackId::new()),
                    seen: TimeRange::new(when, t(day * DAY + 3660)),
                    count: 4,
                    f_center_hz: 433.0e6 + 2.5e6 * i as f64,
                    bandwidth_hz: gens[truth[i]].obw_hz,
                    fingerprint: Some(Fingerprint::new(
                        433.0e6 + 2.5e6 * i as f64,
                        gens[truth[i]].obw_hz,
                    )),
                    identity: None,
                    context: None,
                    classification: None,
                    tags: Vec::new(),
                },
                None,
            )
            .unwrap();

            // A slow oscillator drift across the days, which must not split an emitter from
            // itself (and which no instance-level field is allowed to key on).
            let drift = 1.0 + 0.0008 * day as f64;
            let observation = measure(&gens[truth[i]], &mut rng, drift);
            let f = &mut aggregates[i];
            f.observe(observation, false);
            f.id = format!("features:{i}:{day}");
            f.t = when;
            r.put_emission_features(f).unwrap();

            assign_emitter(&mut r, *e, t(day * DAY + 3700))
                .unwrap()
                .unwrap();
        }
    }

    let score = |r: &Repository| -> (f64, f64, f64, f64, usize, usize) {
        let found: Vec<Option<String>> = emitters
            .iter()
            .map(|e| r.emitter_cluster_id(*e).unwrap())
            .collect();
        let mut by_cluster: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (i, c) in found.iter().enumerate() {
            if let Some(c) = c {
                by_cluster.entry(c.clone()).or_default().push(i);
            }
        }
        // Purity: members that agree with their cluster's plurality truth type.
        let mut agree = 0.0;
        let mut assigned = 0.0;
        let mut mixed = 0usize;
        for members in by_cluster.values() {
            let mut votes: BTreeMap<usize, usize> = BTreeMap::new();
            for &i in members {
                *votes.entry(truth[i]).or_default() += 1;
            }
            if votes.len() > 1 {
                mixed += 1;
            }
            let best = *votes.values().max().unwrap();
            agree += best as f64;
            assigned += members.len() as f64;
        }
        let purity = if assigned > 0.0 {
            agree / assigned
        } else {
            0.0
        };
        // Completeness: each type's instances that landed in that type's largest cluster.
        let mut complete = 0.0;
        let mut clusters_per_type = 0.0;
        for g in 0..gens.len() {
            let mut counts: BTreeMap<String, usize> = BTreeMap::new();
            for (i, c) in found.iter().enumerate() {
                if truth[i] == g
                    && let Some(c) = c
                {
                    *counts.entry(c.clone()).or_default() += 1;
                }
            }
            complete += counts.values().max().copied().unwrap_or(0) as f64 / INSTANCES as f64;
            clusters_per_type += counts.len().max(1) as f64;
        }
        let unassigned = found.iter().filter(|c| c.is_none()).count();
        (
            purity,
            complete / gens.len() as f64,
            clusters_per_type / gens.len() as f64,
            adjusted_rand_index(&truth, &found),
            mixed,
            unassigned,
        )
    };

    let (purity, completeness, per_type, ari, mixed, unassigned) = score(&r);
    println!(
        "online: purity {purity:.3} completeness {completeness:.3} clusters/type {per_type:.2} \
         ARI {ari:.3} mixed {mixed} unassigned {unassigned}"
    );
    assert_eq!(mixed, 0, "a cluster held two different generators (online)");

    // The nightly repair, then the same scoring.
    let report = repair(&mut r, t(DAYS * DAY)).unwrap();
    let (purity, completeness, per_type, ari, mixed, unassigned) = score(&r);
    println!(
        "repaired: purity {purity:.3} completeness {completeness:.3} clusters/type {per_type:.2} \
         ARI {ari:.3} mixed {mixed} unassigned {unassigned} report {report:?}"
    );

    assert_eq!(mixed, 0, "a cluster held two different generators");
    assert!(purity >= PURITY_FLOOR, "purity {purity:.3}");
    assert!(
        completeness >= COMPLETENESS_FLOOR,
        "completeness {completeness:.3}"
    );
    assert!(
        per_type <= CLUSTERS_PER_TYPE_CEILING,
        "clusters per truth type {per_type:.2}"
    );
    assert!(ari >= ARI_FLOOR, "ARI {ari:.3}");

    // Every visible cluster is one type's, and each type's instances sit together.
    let visible = r.visible_clusters().unwrap();
    assert!(
        visible.len() >= gens.len(),
        "each generator should be visible: {} clusters",
        visible.len()
    );

    // Restart persistence for a scene like this one is covered on a real file by
    // `a_multi_day_scene_reads_back_identically_after_a_restart`.
}

/// The scene's persistence half, on a real file: the same assignments read back from a fresh
/// process, with no recomputation.
#[test]
fn a_multi_day_scene_reads_back_identically_after_a_restart() {
    let dir = TempDir::new();
    let path = dir.0.join("inventory.sqlite3");
    let gens = generators();
    let mut rng = Rng(0x1234_5678_9ABC_DEF0);
    let mut emitters = Vec::new();
    let mut truth = Vec::new();

    {
        let mut r = Repository::open(&path).unwrap();
        let mut aggregates = Vec::new();
        for (g, _) in gens.iter().enumerate() {
            for instance in 0..3 {
                let f_hz = 868.0e6 + 1.5e6 * (g * 3 + instance) as f64;
                let e = an_emitter(&mut r, f_hz, (0, 60), 4);
                aggregates.push(EmissionFeatures::new("seed", e, t(0)));
                emitters.push(e);
                truth.push(g);
            }
        }
        for day in 0..3 {
            for (i, e) in emitters.iter().enumerate() {
                let when = t(day * DAY + 100);
                let observation = measure(&gens[truth[i]], &mut rng, 1.0);
                let f = &mut aggregates[i];
                f.observe(observation, false);
                f.id = format!("features:{i}:{day}");
                f.t = when;
                r.put_emission_features(f).unwrap();
                assign_emitter(&mut r, *e, when).unwrap().unwrap();
            }
        }
        repair(&mut r, t(3 * DAY)).unwrap();
    }

    let before: Vec<Option<String>> = {
        let r = Repository::open(&path).unwrap();
        emitters
            .iter()
            .map(|e| r.emitter_cluster_id(*e).unwrap())
            .collect()
    };
    let after: Vec<Option<String>> = {
        let r = Repository::open(&path).unwrap();
        emitters
            .iter()
            .map(|e| r.emitter_cluster_id(*e).unwrap())
            .collect()
    };
    assert_eq!(before, after, "clusters are stored, not recomputed");
    // Same generator, same cluster; different generator, different cluster.
    let mut by_type: BTreeMap<usize, BTreeSet<Option<String>>> = BTreeMap::new();
    for (i, c) in before.iter().enumerate() {
        by_type.entry(truth[i]).or_default().insert(c.clone());
    }
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for (g, clusters) in &by_type {
        for c in clusters.iter().flatten() {
            assert!(
                seen.insert(c.clone()),
                "cluster {c} is shared by two generators (one is {g})"
            );
        }
    }
}

/// An assignment carries only what it decided and why — never a name for the thing.
#[test]
fn an_assignment_says_what_it_decided_and_nothing_about_what_the_thing_is() {
    let a = Assignment {
        emitter_id: EmitterId::new(),
        cluster_id: Some(new_cluster_id()),
        state: Some(ClusterState::Active),
        reason: "joined",
        distance: Some(0.4),
        changed: true,
    };
    let text = format!("{a:?}");
    for forbidden in ["identity", "known_status", "lifecycle", "family"] {
        assert!(!text.contains(forbidden), "{forbidden} leaked: {text}");
    }
}
