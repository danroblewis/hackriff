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

/// A front-end state tuned to `lo`, on the single default device.
fn prov(r: &mut Repository, lo: f64) -> ProvenanceId {
    prov_on(r, "test", None, lo)
}

/// A front-end state on a named device and antenna port, tuned to `lo`.
fn prov_on(r: &mut Repository, device: &str, port: Option<&str>, lo: f64) -> ProvenanceId {
    r.intern_provenance(&Provenance {
        device_id: device.into(),
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
        antenna_port: port.map(Into::into),
        bias_tee: crate::BiasTee::Unknown,
        clock_source: ClockSource::Internal,
        clock_locked: true,
        calibration_state_ref: None,
        spur_mask_ref: None,
        timestamp_method: TimestampMethod::Synthetic,
        timestamp_error_budget_ns: None,
    })
    .unwrap()
}

/// One inventory row from a track sighting, its detection carrying no suspect flag.
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
    station_flagged(
        r,
        survey,
        provenance,
        f,
        obw,
        xdb,
        snr,
        peak_dbfs,
        seen,
        DetectionFlags::default(),
    )
}

/// One inventory row from a track sighting, with a detection linked through its track so the
/// rules can read its level, −3 dB width, trust, suspect flags and tuning centre.
#[allow(clippy::too_many_arguments)]
fn station_flagged(
    r: &mut Repository,
    survey: SurveyId,
    provenance: ProvenanceId,
    f: f64,
    obw: f64,
    xdb: f64,
    snr: f64,
    peak_dbfs: f32,
    seen: TimeRange,
    flags: DetectionFlags,
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
        flags,
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

/// One inventory row from a bare track sighting with an explicit fingerprint and observation span.
/// No linked detection, so the duplicate rule ranks it on the neutral unmeasured-SNR proxy and the
/// duty cycle decides — which is the point: these two rows differ in nothing else.
fn sighted(
    r: &mut Repository,
    f: f64,
    bw: f64,
    duty: f64,
    burst: f64,
    seen: TimeRange,
) -> EmitterId {
    r.record_sighting(
        &Sighting {
            source: LinkTarget::Track(TrackId::new()),
            seen,
            count: 4,
            f_center_hz: f,
            bandwidth_hz: bw,
            fingerprint: Some(Fingerprint {
                duty_cycle: Some(duty),
                burst_length_s: Some(burst),
                ..Fingerprint::new(f, bw)
            }),
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

/// T-250's scene, now fixed one layer earlier by TM-5 (T-262): the user's own 99.8 MHz station of
/// 2026-09-16, seen over two **disjoint** windows (94–399 s, then 471–530 s), is **one row from
/// the start**. Their bands overlap essentially exactly; the only thing that had separated them
/// was the burst length each window happened to measure (0.68 s vs 0.37 s), a statistic of the
/// watching, not of the signal, and it is excluded while the two intervals are disjoint
/// (`Fingerprint::compare_across_silence`).
///
/// T-250 collapsed this pair at the **merge** layer, leaving a deferring second row, and recorded
/// that entity resolution was still minting it (ADR-0017 §1.1 left that to TM-5). Now the returning
/// station revives its own emitter, so the duplicate never exists and there is nothing for the
/// merge rules to hide. The merge layer keeps its own coverage in the `t219_*` tests below and in
/// `relate::tests`.
#[test]
fn t262_a_station_seen_in_two_disjoint_windows_is_one_row_from_the_start() {
    let (mut r, _sv) = scene();
    let first = sighted(&mut r, 99_814_800.0, 377_500.0, 0.4952, 0.6821, tr(94, 399));
    let second = sighted(
        &mut r,
        99_815_100.0,
        377_600.0,
        0.3720,
        0.3648,
        tr(471, 530),
    );
    assert_eq!(
        first, second,
        "the returning station revives its own emitter rather than minting a second row"
    );
    assert_eq!(every(&r).len(), 1, "no duplicate row was ever created");
    assert_eq!(shown(&r), vec![first], "one physical station, one row");

    // Two presence intervals on that one emitter: the 72 s silence closed the first.
    let iv = r
        .presence_intervals(first, crate::presence::IdleGap::conservative(), t(530))
        .unwrap();
    assert_eq!(iv.len(), 2, "{iv:?}");
    assert_eq!(iv[0].time, tr(94, 399));
    assert_eq!(iv[1].time, tr(471, 530));

    // And the merge layer is left with nothing to claim.
    let out = r
        .resolve_overlaps(second, "test/overlap@1", t(530), &tol())
        .unwrap();
    assert!(out.duplicates.is_empty(), "{out:?}");
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
    // Tuned at 100.8 MHz, the mirror of the 101.3 MHz station lands at 100.3 MHz, 30 dB down, at
    // the source's width, and the detector's own mirror test flagged it.
    let image = station_flagged(
        &mut r,
        sv,
        p,
        100.3e6,
        180e3,
        180e3,
        8.0,
        -48.0,
        tr(1, 4),
        DetectionFlags {
            image_candidate: true,
            ..DetectionFlags::default()
        },
    );
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

/// The guard's central case: a narrow emission inside a confirmed wide station — a subcarrier, a
/// data burst, a pager channel in a broadcast skirt — is a real signal until something says
/// otherwise. It overlaps its host's band completely, carries no identity and no fingerprint of its
/// own, and must still be listed.
#[test]
fn t219_a_narrow_signal_inside_a_confirmed_wide_station_stays_visible() {
    let (mut r, sv) = scene();
    let p = prov(&mut r, 100.8e6);
    let wide = station(&mut r, sv, p, 100.3e6, 180e3, 180e3, 26.0, -18.0, tr(0, 5));
    confirm(&mut r, wide, t(5));
    // 12.5 kHz on the same centre: wholly inside the station's band.
    let narrow = station(
        &mut r,
        sv,
        p,
        100.3e6,
        12.5e3,
        12.5e3,
        14.0,
        -30.0,
        tr(1, 5),
    );
    assert_ne!(wide, narrow, "two entries before resolution");
    assert_eq!(
        overlap_fraction(
            FreqRange::centered(100.3e6, 180e3),
            FreqRange::centered(100.3e6, 12.5e3)
        ),
        1.0,
        "the case is only interesting because the narrower band is wholly covered"
    );

    for id in [wide, narrow] {
        r.resolve_overlaps(id, "test/overlap@1", t(5), &tol())
            .unwrap();
    }
    assert!(
        r.emitter_relations(narrow).unwrap().is_empty(),
        "a narrow signal inside a wide one is never suppressed as a duplicate of its host"
    );
    assert!(shown(&r).contains(&narrow), "and it stays listed");
    assert_eq!(shown(&r).len(), 2);
}

/// The user's field case of 2026-09-15: short intermittent ~8.5 kHz bursts at 100.300 MHz while a
/// wideband station sits at 101.303 MHz and the front end is tuned to 100.800 MHz. The image
/// arithmetic fits to 3 kHz and the detector even flagged the burst `image_candidate` — but an
/// image preserves its source's width, and 8.5 kHz is not 180 kHz, so the burst stays an
/// independent emitter that a person can look at.
#[test]
fn t219_a_narrow_burst_on_a_predicted_image_frequency_is_not_attributed_to_a_wide_station() {
    let (mut r, sv) = scene();
    let p = prov(&mut r, 100.8e6);
    let wfm = station(
        &mut r,
        sv,
        p,
        101.303e6,
        180e3,
        180e3,
        30.0,
        -18.0,
        tr(0, 10),
    );
    confirm(&mut r, wfm, t(10));
    let burst = station_flagged(
        &mut r,
        sv,
        p,
        100.300e6,
        8.5e3,
        8.5e3,
        12.0,
        -48.0,
        tr(2, 4),
        DetectionFlags {
            image_candidate: true,
            ..DetectionFlags::default()
        },
    );

    let out = r
        .resolve_overlaps(burst, "test/overlap@1", t(10), &tol())
        .unwrap();
    assert!(out.artifacts.is_empty(), "{out:?}");
    assert!(
        r.emitter_relations(burst).unwrap().is_empty(),
        "the arithmetic alone never attributes a narrow burst to a wideband station"
    );
    assert!(shown(&r).contains(&burst));
}

/// The image scene of `t219_an_image_is_attributed_to_its_source_and_a_real_neighbour_is_not`,
/// with the source and the row being explained measured on **named front ends**. Everything else —
/// the geometry, the levels, the widths, the presence window and the detector's own
/// `image_candidate` flag — is identical whichever devices are named, so the receive chain is the
/// only variable between the two runs below. Both front ends are even tuned to the *same* centre.
fn image_scene(source_dev: &str, target_dev: &str) -> (Repository, EmitterId, EmitterId) {
    let (mut r, sv) = scene();
    let ps = prov_on(&mut r, source_dev, None, 100.8e6);
    let pt = prov_on(&mut r, target_dev, None, 100.8e6);
    let source = station(&mut r, sv, ps, 101.3e6, 180e3, 180e3, 26.0, -18.0, tr(0, 5));
    confirm(&mut r, source, t(5));
    let image = station_flagged(
        &mut r,
        sv,
        pt,
        100.3e6,
        180e3,
        180e3,
        8.0,
        -48.0,
        tr(1, 4),
        DetectionFlags {
            image_candidate: true,
            ..DetectionFlags::default()
        },
    );
    (r, source, image)
}

/// T-302 (blind): an image is a property of **one receive chain**. Two front ends are two mixers
/// with two LOs, so a signal arriving at device B's antenna can never manufacture anything in
/// device A's output — and claiming otherwise is not a near-miss but a confident, plausible-looking
/// statement about physics that cannot happen.
///
/// The scene makes the false attribution maximally tempting: both front ends are tuned to the same
/// 100.800 MHz, 2 x 100.800 - 101.300 = 100.300 MHz **exactly**, the row is 30 dB down, its width
/// matches the mechanism, it is present only while the source is, and the detector itself flagged
/// it `image_candidate`. Every test the rule applies passes except the receive chain.
///
/// Both halves are required. The single-device control proves the fix is a device predicate rather
/// than a silent disabling of T-219: without it, "no claim" would also be the answer of a rule that
/// had stopped working altogether. Which row is the image, and that one of them is an image at all,
/// lives only in these assertions.
#[test]
fn t302_an_image_is_never_attributed_across_two_front_ends() {
    // Control: one front end, one mixer. The mirror is attributed, as T-219 requires.
    let (mut r, source, image) = image_scene("hackrf:A", "hackrf:A");
    let out = r
        .resolve_overlaps(image, "test/overlap@1", t(5), &tol())
        .unwrap();
    assert_eq!(out.artifacts.len(), 1, "{out:?}");
    let rel = r.emitter_relations(image).unwrap();
    assert_eq!(rel[0].kind, RelationKind::ArtifactOf);
    assert_eq!(rel[0].artifact, Some(ArtifactKind::Image));
    assert_eq!(rel[0].source_id, source);
    assert!(!shown(&r).contains(&image));

    // The same geometry, with the source seen only on the *other* front end.
    let (mut r, _source, image) = image_scene("hackrf:B", "hackrf:A");
    let out = r
        .resolve_overlaps(image, "test/overlap@1", t(5), &tol())
        .unwrap();
    assert!(
        out.artifacts.is_empty(),
        "device A's LO cannot mirror an emitter only device B received: {out:?}"
    );
    assert!(
        !r.emitter_relations(image)
            .unwrap()
            .iter()
            .any(|x| x.kind == RelationKind::ArtifactOf),
        "no artifact relation is ever claimed across two receive chains"
    );
    assert!(
        shown(&r).contains(&image),
        "and the row stays listed as the independent emission it is"
    );
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

// ---------------------------------------------------------------------------------------------
// T-369: overlap is an error signal, and the resolution is re-analysis of the region.
// ---------------------------------------------------------------------------------------------

/// Every contested verdict recorded against `id` (append-only, `active = 0`, so it hides nothing).
fn contested(r: &Repository, id: EmitterId) -> Vec<EmitterRelation> {
    r.emitter_relation_history(id)
        .unwrap()
        .into_iter()
        .filter(|x| {
            x.detail
                .as_ref()
                .and_then(|d| d.get("verdict"))
                .and_then(|v| v.as_str())
                == Some("contested")
        })
        .collect()
}

/// **The measured case, from the user's own 45 s FM capture of 2026-09-15.** Three candidate boxes
/// sat stacked around 101.67-101.71 MHz: 18.2 kHz at 101.6745, 17.1 kHz at 101.68365 and 14.1 kHz
/// at 101.69835, all on the air over the same 40 s. Two overlapping pairs, drawn on top of each
/// other on the waterfall, and **T-219 claimed nothing at all**: the middle box overlaps the first
/// by 49.7 % of the narrower band and the third by 6 %, both under `OVERLAP_MIN_FRACTION`, so
/// `bands_compete` refuses and every ranking stage below it is unreachable. That is the diagnosis
/// this test pins: not a collapse that failed to reach the wire — a collapse that never ran.
///
/// The region re-analysis asks the air instead of the rows. The detections behind all three boxes
/// form **one** contiguous mode, so they are cuts of one emission: the best-supported box is kept
/// and the middle one defers to it. What remains is two boxes that do not overlap each other — and
/// the third, whose extents are genuinely separated from the survivor's, is contested rather than
/// merged, because merging is the dangerous direction.
#[test]
fn t369_a_staircase_of_offset_boxes_over_one_emission_stops_overlapping() {
    let (mut r, sv) = scene();
    let p = prov(&mut r, 100.8e6);
    let seen = tr(0, 40);
    let a = station(
        &mut r,
        sv,
        p,
        101_674_500.0,
        18_200.0,
        18_200.0,
        6.42,
        -70.0,
        seen,
    );
    let mid = station(
        &mut r,
        sv,
        p,
        101_683_650.0,
        17_100.0,
        17_100.0,
        5.56,
        -71.0,
        seen,
    );
    let b = station(
        &mut r,
        sv,
        p,
        101_698_350.0,
        14_100.0,
        14_100.0,
        5.61,
        -71.0,
        seen,
    );
    assert_eq!(every(&r).len(), 3, "three rows before re-analysis");

    // The measured geometry, so the test states the case it is about rather than trusting it.
    let (fa, fm, fb) = (
        FreqRange::centered(101_674_500.0, 18_200.0),
        FreqRange::centered(101_683_650.0, 17_100.0),
        FreqRange::centered(101_698_350.0, 14_100.0),
    );
    assert!(
        (overlap_fraction(fa, fm) - 0.497).abs() < 0.01,
        "49.7 % of the narrower band"
    );
    assert!(
        !bands_compete(fa, fm),
        "so the T-219 ranking stages never see this pair"
    );
    assert!(!bands_compete(fm, fb));
    assert!(
        !fa.overlaps(&fb),
        "the outer two never overlapped each other"
    );

    for id in [a, mid, b] {
        r.resolve_overlaps(id, "test/overlap@1", t(40), &tol())
            .unwrap();
    }

    let left = shown(&r);
    assert_eq!(
        left.len(),
        2,
        "three stacked boxes resolve to two: {left:?}"
    );
    assert!(left.contains(&a), "the best-supported box is kept");
    assert!(
        left.contains(&b),
        "and the box the guard protects is never merged away"
    );
    assert_eq!(
        every(&r).len(),
        3,
        "nothing was deleted; the deferring row is kept in full"
    );

    // The middle box defers, and the record says it was the region's measurement that decided.
    let rel = r.emitter_relations(mid).unwrap();
    assert_eq!(rel.len(), 1, "{rel:?}");
    assert_eq!(rel[0].kind, RelationKind::DuplicateOf);
    assert_eq!(rel[0].source_id, a);
    assert!(
        rel[0].reason.contains("region re-analysis"),
        "the reason names the re-analysis, not a ranking: {}",
        rel[0].reason
    );
    let detail = rel[0].detail.as_ref().unwrap();
    assert_eq!(detail["verdict"], "one-emission");
    assert_eq!(detail["members"], 3);
    // The mode the detections measured spans the whole region, not one row's band.
    assert!((detail["mode_lo_hz"].as_f64().unwrap() - fa.lo_hz).abs() < 1.0);
    assert!((detail["mode_hi_hz"].as_f64().unwrap() - fb.hi_hz).abs() < 1.0);

    // The third box was looked at and deliberately left alone, with the block disclosed.
    let c = contested(&r, b);
    assert_eq!(
        c.len(),
        REGION_MAX_ROUNDS,
        "the unresolved box is examined by each member's resolve and then stops at the bound: {c:?}"
    );
    for v in &c {
        assert!(
            !v.active,
            "a contested verdict is never in force: it hides nothing"
        );
        assert_eq!(
            v.detail.as_ref().unwrap()["blocked_by"],
            "separated -3 dB extents"
        );
        assert_eq!(
            v.detail.as_ref().unwrap()["members"],
            3,
            "every member of the region is named"
        );
    }

    // And the property the user is actually looking at: nothing overlaps any more.
    assert_eq!(overlapping_pairs(&r), 0);
}

/// Shown rows that overlap in time **and** frequency — the error signal, counted straight off the
/// inventory the way the waterfall would draw it.
fn overlapping_pairs(r: &Repository) -> usize {
    let rows = r
        .query_inventory(&InventoryQuery::default())
        .unwrap()
        .entries;
    let mut n = 0;
    for (i, a) in rows.iter().enumerate() {
        for b in rows.iter().skip(i + 1) {
            let (fa, fb) = (
                FreqRange::centered(a.emitter.f_center_hz, a.emitter.bandwidth_hz),
                FreqRange::centered(b.emitter.f_center_hz, b.emitter.bandwidth_hz),
            );
            let t_ov = a.emitter.first_seen.max(b.emitter.first_seen)
                <= a.emitter.last_seen.min(b.emitter.last_seen);
            if fa.overlaps(&fb) && t_ov {
                n += 1;
            }
        }
    }
    n
}

/// **The control that matters most, built from the real raster.** Two genuinely distinct FM
/// broadcast stations on adjacent 200 kHz channels, each measured at 220 kHz occupied bandwidth
/// (skirts included, which is why adjacent channels abut on air) and 150 kHz at −3 dB, on the air
/// together. Their **occupied bands really do overlap** — 101.190-101.410 against 101.390-101.610
/// share 20 kHz — so this is a real overlap, the region re-analysis really does run on it, and the
/// detections behind them merge into **one** contiguous mode. Neither row carries a decoded
/// identity and their bandwidths are identical, so nothing but the measured geometry can save
/// them.
///
/// It is enough: their −3 dB extents are separated by more than the measurement uncertainty, which
/// is what `distinguishing_evidence` exists to see. Both stay shown, neither defers, and the region
/// is recorded contested instead. A blind test must never merge two distinct emitters (T-233), so
/// the re-analysis may bypass `bands_compete` — a test of the rows — but never the guard.
#[test]
fn t369_two_stations_on_the_200_khz_raster_are_never_merged_by_re_analysis() {
    let (mut r, sv) = scene();
    let p = prov(&mut r, 101.4e6);
    let seen = tr(0, 40);
    let low = station(
        &mut r,
        sv,
        p,
        101_300_000.0,
        220e3,
        150e3,
        24.0,
        -20.0,
        seen,
    );
    let high = station(
        &mut r,
        sv,
        p,
        101_500_000.0,
        220e3,
        150e3,
        18.0,
        -26.0,
        seen,
    );
    let (fl, fh) = (
        FreqRange::centered(101_300_000.0, 220e3),
        FreqRange::centered(101_500_000.0, 220e3),
    );
    assert!(fl.overlaps(&fh), "the occupied bands really do overlap");
    assert_eq!(
        modes(&[fl, fh], center_uncertainty_hz(101.4e6, &tol())).len(),
        1,
        "and the measurements alone merge into one mode: only the guard separates these two"
    );

    for id in [low, high] {
        r.resolve_overlaps(id, "test/overlap@1", t(40), &tol())
            .unwrap();
    }

    assert_eq!(
        shown(&r).len(),
        2,
        "two stations on the raster stay two shown rows"
    );
    for id in [low, high] {
        assert!(
            r.emitter_relations(id).unwrap().is_empty(),
            "neither adjacent station ever defers to the other"
        );
    }
    // The overlap was seen and recorded rather than ignored: one verdict per member's resolve,
    // and every one of them says the same thing.
    let c = contested(&r, high);
    assert_eq!(c.len(), 2, "{c:?}");
    for v in &c {
        assert!(!v.active, "a contested verdict never hides a row");
        assert_eq!(
            v.detail.as_ref().unwrap()["blocked_by"],
            "separated -3 dB extents"
        );
        assert_eq!(
            v.detail.as_ref().unwrap()["modes"],
            1,
            "the measurements did merge into one mode - only the guard kept these two apart"
        );
    }
}

/// **The bound, asserted as a count and not as "it finished".** Re-analysis runs on a live serving
/// path, so a region it cannot settle must stop writing rather than churn for as long as the signal
/// is on the air. Resolve the contested raster pair twenty times — which is what a running pipeline
/// does, once per sighting — and the verdicts stop at `REGION_MAX_ROUNDS`, with the shown set
/// unchanged from the first round onward.
#[test]
fn t369_region_re_analysis_terminates_at_its_bound() {
    let (mut r, sv) = scene();
    let p = prov(&mut r, 101.4e6);
    let seen = tr(0, 40);
    let low = station(
        &mut r,
        sv,
        p,
        101_300_000.0,
        220e3,
        150e3,
        24.0,
        -20.0,
        seen,
    );
    let high = station(
        &mut r,
        sv,
        p,
        101_500_000.0,
        220e3,
        150e3,
        18.0,
        -26.0,
        seen,
    );

    let mut first_shown = None;
    let mut wrote = Vec::new();
    for round in 0..20 {
        let mut n = 0;
        for id in [low, high] {
            let out = r
                .resolve_overlaps(id, "test/overlap@1", t(40 + round), &tol())
                .unwrap();
            n += out.contested.len();
            assert!(
                out.duplicates.is_empty(),
                "round {round}: nothing is ever merged here"
            );
        }
        wrote.push(n);
        let now = shown(&r);
        assert_eq!(now.len(), 2, "round {round}");
        match &first_shown {
            None => first_shown = Some(now),
            Some(f) => assert_eq!(&now, f, "round {round}: the shown set never changes again"),
        }
    }

    assert_eq!(REGION_MAX_ROUNDS, 3);
    // Each row is examined once per `resolve_overlaps` call of either row, so the cap is reached
    // inside the second round; what matters is that it *is* reached and nothing is written after.
    assert_eq!(
        contested(&r, high).len(),
        REGION_MAX_ROUNDS,
        "the loser of the region accumulates exactly the bound, and no more"
    );
    assert_eq!(wrote.iter().sum::<usize>(), REGION_MAX_ROUNDS);
    assert!(
        wrote[2..].iter().all(|&n| n == 0),
        "and every later round writes nothing at all: {wrote:?}"
    );
}
