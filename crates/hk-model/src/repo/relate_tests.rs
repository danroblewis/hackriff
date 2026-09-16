//! T-219 (C40, ADR-0015 §11.4): overlapping-candidate resolution and geometric artifact
//! attribution. Every threshold used here is the a-priori one from [`crate::relate`]; the truth
//! ("these two readings are one station", "this box is the image of that one") lives only in the
//! assertions, never in the system under test.

use super::Repository;
use crate::relate::*;
use crate::*;

fn t(sec: i64) -> Timestamp {
    Timestamp::from_unix_nanos(1_789_000_000_000_000_000 + sec * 1_000_000_000)
}

fn tr(a: i64, b: i64) -> TimeRange {
    TimeRange::new(t(a), t(b))
}

fn tol() -> Tolerances {
    Tolerances::default()
}

/// A repository with one survey to hang detections off.
fn scene() -> (Repository, SurveyId) {
    let mut r = Repository::open_in_memory().unwrap();
    let plan = ScanPlan {
        id: ScanPlanId::new(),
        version: 1,
        name: "fm".into(),
        created_at: t(0),
        regions: vec![PlanRegion {
            freq: FreqRange::new(88e6, 108e6),
            priority: 1.0,
            revisit_ns: None,
        }],
        policy: ScanPolicy::SweepThenDwell,
        gain_table: vec![],
        schedule: Schedule::Cron {
            expr: "* * * * *".into(),
        },
        extra: serde_json::Value::Null,
    };
    r.insert_scan_plan(&plan).unwrap();
    let survey = Survey {
        id: SurveyId::new(),
        plan_id: plan.id,
        plan_version: plan.version,
        device_id: "test".into(),
        state: SurveyState::Open,
        t_start: t(0),
        t_end: None,
        summary: None,
    };
    r.insert_survey(&survey).unwrap();
    let id = survey.id;
    (r, id)
}

/// A front-end state tuned to `lo`.
fn prov(r: &mut Repository, lo: f64) -> ProvenanceId {
    r.intern_provenance(&Provenance {
        device_id: "test".into(),
        tune: Tune {
            center_hz: lo,
            sample_rate_hz: 2.4e6,
            lna_db: 32.0,
            vga_db: 30.0,
            amp_on: true,
            bandwidth_hz: 1.75e6,
        },
        overload: false,
        quantisation_limited: false,
        temperature_c: None,
        antenna_port: None,
        clock_source: ClockSource::Internal,
        clock_locked: true,
        calibration_state_ref: None,
        spur_mask_ref: None,
        timestamp_method: TimestampMethod::Synthetic,
        timestamp_error_budget_ns: None,
    })
    .unwrap()
}

/// One inventory row from a track sighting, with a detection linked through its track so the
/// rules can read its level, −3 dB width, trust and tuning centre.
#[allow(clippy::too_many_arguments)]
fn station(
    r: &mut Repository,
    survey: SurveyId,
    provenance: ProvenanceId,
    f: f64,
    obw: f64,
    xdb: f64,
    snr: f64,
    peak_dbfs: f32,
    seen: TimeRange,
) -> EmitterId {
    let track_id = TrackId::new();
    let fp = Fingerprint {
        duty_cycle: Some(1.0),
        ..Fingerprint::new(f, obw)
    };
    let id = r
        .record_sighting(
            &Sighting {
                source: LinkTarget::Track(track_id),
                seen,
                count: 4,
                f_center_hz: f,
                bandwidth_hz: obw,
                fingerprint: Some(fp),
                identity: None,
                context: None,
                classification: None,
                tags: Vec::new(),
            },
            None,
        )
        .unwrap()
        .emitter_id;
    let d = Detection {
        id: DetectionId::new(),
        survey_id: survey,
        time: seen,
        f_center_hz: f,
        obw_hz: obw,
        xdb_bandwidth_hz: Some(xdb),
        xdb_level_db: Some(-3.0),
        snr_peak_db: snr,
        snr_mean_db: snr - 3.0,
        peak_level_dbfs: peak_dbfs,
        peak_level_dbm: None,
        sk: None,
        clip_count: 0,
        detector_version: "test@1".into(),
        provenance_ref: provenance,
        flags: DetectionFlags::default(),
    };
    r.insert_detections(std::slice::from_ref(&d)).unwrap();
    r.upsert_track(&Track {
        id: track_id,
        state: TrackState::Closed,
        split_from: None,
        time: seen,
        f_center_hz: f,
        bandwidth_hz: obw,
        detection_count: 1,
        timing: TimingFeatures {
            duty_cycle: Some(1.0),
            ..TimingFeatures::default()
        },
        updated_at: seen.end,
    })
    .unwrap();
    r.link_detections_to_track(track_id, &[d.id], seen.end)
        .unwrap();
    id
}

fn confirm(r: &mut Repository, id: EmitterId, at: Timestamp) {
    r.change_emitter_lifecycle(
        id,
        LifecycleState::Confirmed,
        LifecycleAuthor::Auto,
        "test/confirm@1",
        "steady trusted track",
        at,
    )
    .unwrap();
}

/// The rows the inventory shows by default.
fn shown(r: &Repository) -> Vec<EmitterId> {
    r.query_inventory(&InventoryQuery::default())
        .unwrap()
        .entries
        .iter()
        .map(|e| e.emitter.id)
        .collect()
}

/// Every live row, deferring or not.
fn every(r: &Repository) -> Vec<EmitterId> {
    r.query_inventory(&InventoryQuery {
        relations: RelationVisibility::All,
        ..InventoryQuery::default()
    })
    .unwrap()
    .entries
    .iter()
    .map(|e| e.emitter.id)
    .collect()
}

/// A Confirmed entry suppresses a later candidate overlapping its band: one shown row, the other
/// kept in full with a reason and a link, and the claim reversible.
#[test]
fn t219_a_confirmed_entry_suppresses_an_overlapping_candidate_and_it_is_reversible() {
    let (mut r, sv) = scene();
    let p = prov(&mut r, 100.8e6);
    let station_a = station(&mut r, sv, p, 101.3e6, 180e3, 180e3, 24.0, -18.0, tr(0, 5));
    confirm(&mut r, station_a, t(5));
    // 60 kHz into the skirt: too far for entity resolution to cluster it (the centre tolerance is
    // 45 kHz at this bandwidth), close enough that the two bands are 67 % the same.
    let offset = station(&mut r, sv, p, 101.36e6, 180e3, 180e3, 14.0, -30.0, tr(1, 5));
    assert_ne!(station_a, offset, "two entries before resolution");

    let out = r
        .resolve_overlaps(offset, "test/overlap@1", t(5), &tol())
        .unwrap();
    assert_eq!(out.suppressed.len(), 1, "{out:?}");
    assert_eq!(shown(&r), vec![station_a], "one physical station, one row");

    // Raw data and history are always kept: the row, its count and its detections are untouched.
    assert_eq!(every(&r).len(), 2);
    assert_eq!(r.emitter(offset).unwrap().count, 4);
    assert!(r.emitter_latest_measurement(offset).unwrap().is_some());

    let rel = r.emitter_relations(offset).unwrap();
    assert_eq!(rel.len(), 1);
    assert_eq!(rel[0].kind, RelationKind::SuppressedBy);
    assert_eq!(rel[0].source_id, station_a);
    assert!(
        rel[0].reason.contains("confirmed emitter"),
        "the reason is disclosed: {}",
        rel[0].reason
    );

    // Reversible: a revocation is appended, never an edit, and the row is listed again.
    r.record_emitter_relation(&RelationClaim {
        emitter_id: offset,
        source_id: station_a,
        kind: RelationKind::SuppressedBy,
        artifact: None,
        active: false,
        t: t(6),
        author: RelationAuthor::User,
        actor: "token:abc".into(),
        reason: "a person disagreed".into(),
        score: None,
        detail: None,
    })
    .unwrap();
    assert!(shown(&r).contains(&offset));
    assert!(r.emitter_relations(offset).unwrap().is_empty());
    assert_eq!(
        r.emitter_relation_history(offset).unwrap().len(),
        2,
        "the claim and its revocation are both kept"
    );
}

/// Overlapping candidates compete: the strongest on the SNR x duty x trust proxy is shown and the
/// others are marked superseded with a link, never deleted.
#[test]
fn t219_overlapping_candidates_compete_and_only_the_strongest_is_shown() {
    let (mut r, sv) = scene();
    let p = prov(&mut r, 100.8e6);
    let strong = station(&mut r, sv, p, 101.3e6, 180e3, 180e3, 26.0, -18.0, tr(0, 5));
    let weak = station(&mut r, sv, p, 101.36e6, 180e3, 180e3, 12.0, -34.0, tr(0, 5));

    let out = r
        .resolve_overlaps(weak, "test/overlap@1", t(5), &tol())
        .unwrap();
    assert_eq!(out.duplicates.len(), 1, "{out:?}");
    assert_eq!(shown(&r), vec![strong]);
    assert_eq!(every(&r).len(), 2, "the losing row is kept in full");

    let rel = r.emitter_relations(weak).unwrap();
    assert_eq!(rel[0].kind, RelationKind::DuplicateOf);
    assert_eq!(rel[0].source_id, strong);
    assert!(rel[0].score.is_some_and(|s| s > 0.0));
    assert!(rel[0].reason.contains("SNR x duty x trust"));
    assert!(r.emitter_relations(strong).unwrap().is_empty());
}

/// The guard: two genuinely distinct adjacent stations are never collapsed. Their measured
/// occupied bands overlap by 67 % — but their −3 dB extents are separated by far more than the
/// measurement uncertainty, and that blocks both suppression and competition.
#[test]
fn t219_two_adjacent_stations_with_separated_extents_stay_two_entries() {
    let (mut r, sv) = scene();
    let p = prov(&mut r, 100.8e6);
    let a = station(&mut r, sv, p, 101.3e6, 600e3, 150e3, 24.0, -18.0, tr(0, 5));
    let b = station(&mut r, sv, p, 101.5e6, 600e3, 150e3, 22.0, -19.0, tr(0, 5));
    assert_ne!(a, b);
    assert!(
        overlap_fraction(
            FreqRange::centered(101.3e6, 600e3),
            FreqRange::centered(101.5e6, 600e3)
        ) >= OVERLAP_MIN_FRACTION,
        "the case is only interesting if the bands do overlap"
    );

    confirm(&mut r, a, t(5));
    for id in [a, b] {
        r.resolve_overlaps(id, "test/overlap@1", t(5), &tol())
            .unwrap();
    }
    assert!(r.emitter_relations(a).unwrap().is_empty());
    assert!(
        r.emitter_relations(b).unwrap().is_empty(),
        "a distinct adjacent station is neither suppressed nor superseded"
    );
    assert_eq!(shown(&r).len(), 2);
}

/// An injected image is attributed to its source with the arithmetic disclosed; a real emitter
/// beside it, too strong for the mechanism, is left an independent emitter.
#[test]
fn t219_an_image_is_attributed_to_its_source_and_a_real_neighbour_is_not() {
    let (mut r, sv) = scene();
    let p = prov(&mut r, 100.8e6);
    let source = station(&mut r, sv, p, 101.3e6, 180e3, 180e3, 26.0, -18.0, tr(0, 5));
    confirm(&mut r, source, t(5));
    // Tuned at 100.8 MHz, the mirror of the 101.3 MHz station lands at 100.3 MHz, 30 dB down.
    let image = station(&mut r, sv, p, 100.3e6, 180e3, 180e3, 8.0, -48.0, tr(1, 4));
    // A real emitter 60 kHz away, only 2 dB below the source: no mechanism explains it.
    let real = station(&mut r, sv, p, 100.36e6, 180e3, 180e3, 24.0, -20.0, tr(0, 5));

    let out = r
        .resolve_overlaps(image, "test/overlap@1", t(5), &tol())
        .unwrap();
    assert_eq!(out.artifacts.len(), 1, "{out:?}");
    let rel = r.emitter_relations(image).unwrap();
    assert_eq!(rel[0].kind, RelationKind::ArtifactOf);
    assert_eq!(rel[0].artifact, Some(ArtifactKind::Image));
    assert_eq!(rel[0].source_id, source);
    assert!(
        rel[0].reason.contains("image") && rel[0].reason.contains("100.800000 MHz"),
        "the arithmetic is disclosed: {}",
        rel[0].reason
    );
    assert!(!shown(&r).contains(&image));
    assert!(every(&r).contains(&image), "the row is kept, never deleted");

    assert!(
        !r.emitter_relations(real)
            .unwrap()
            .iter()
            .any(|x| x.kind == RelationKind::ArtifactOf),
        "a real adjacent emitter is not attributed to the strong station"
    );
    assert!(shown(&r).contains(&real));
}

/// Presence is required: an emission seen while its supposed source was absent stays an
/// independent emitter, whatever the frequency arithmetic says.
#[test]
fn t219_an_emission_seen_without_its_supposed_source_stays_independent() {
    let (mut r, sv) = scene();
    let p = prov(&mut r, 100.8e6);
    let source = station(&mut r, sv, p, 101.3e6, 180e3, 180e3, 26.0, -18.0, tr(0, 5));
    confirm(&mut r, source, t(5));
    // Exactly on the predicted image frequency and at a plausible level — but an hour later.
    let late = station(
        &mut r,
        sv,
        p,
        100.3e6,
        180e3,
        180e3,
        8.0,
        -48.0,
        tr(3600, 3605),
    );

    let out = r
        .resolve_overlaps(late, "test/overlap@1", t(3605), &tol())
        .unwrap();
    assert!(out.artifacts.is_empty(), "{out:?}");
    assert!(r.emitter_relations(late).unwrap().is_empty());
    assert!(shown(&r).contains(&late));
}

/// A relationship row is append-only: the table refuses an update or a delete, so a losing
/// candidate can always be revived from the record.
#[test]
fn t219_relationship_rows_are_append_only() {
    let (mut r, sv) = scene();
    let p = prov(&mut r, 100.8e6);
    let a = station(&mut r, sv, p, 101.3e6, 180e3, 180e3, 26.0, -18.0, tr(0, 5));
    let b = station(&mut r, sv, p, 101.36e6, 180e3, 180e3, 12.0, -34.0, tr(0, 5));
    r.resolve_overlaps(b, "test/overlap@1", t(5), &tol())
        .unwrap();
    for sql in [
        "UPDATE emitter_relation SET active = 0",
        "DELETE FROM emitter_relation",
    ] {
        assert!(
            r.conn.execute(sql, []).is_err(),
            "{sql} must be refused by the append-only trigger"
        );
    }
    assert_eq!(r.emitter_relations(b).unwrap()[0].source_id, a);
}
