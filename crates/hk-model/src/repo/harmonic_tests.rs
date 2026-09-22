//! T-374 (C40): storage and resolution for harmonic families.
//!
//! The scene is T-317's: harmonics 43, 44 and 45 of a ~2.3364 MHz oscillator that was never
//! detected, sitting in an FM band alongside real unrelated broadcast stations. Truth lives only
//! in the assertions — the repository is handed measured centres and widths and nothing else.

use super::Repository;
use crate::relate::RelationAuthor;
use crate::*;

fn t(sec: i64) -> Timestamp {
    Timestamp::from_unix_nanos(1_789_000_000_000_000_000 + sec * 1_000_000_000)
}

fn tr(a: i64, b: i64) -> TimeRange {
    TimeRange::new(t(a), t(b))
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
        device_id: "hackrf-0".into(),
        state: SurveyState::Open,
        t_start: t(0),
        t_end: None,
        summary: None,
    };
    r.insert_survey(&survey).unwrap();
    let id = survey.id;
    (r, id)
}

fn prov_on(r: &mut Repository, device: &str) -> ProvenanceId {
    r.intern_provenance(&Provenance {
        device_id: device.into(),
        tune: Tune {
            center_hz: 98e6,
            sample_rate_hz: 20e6,
            lna_db: 32.0,
            vga_db: 30.0,
            amp_on: true,
            bandwidth_hz: 15e6,
        },
        overload: false,
        quantisation_limited: false,
        noise_sigma_lsb: None,
        temperature_c: None,
        antenna_port: None,
        bias_tee: BiasTee::Unknown,
        clock_source: ClockSource::Internal,
        clock_locked: true,
        calibration_state_ref: None,
        spur_mask_ref: None,
        timestamp_method: TimestampMethod::Synthetic,
        timestamp_error_budget_ns: None,
        capture_artefacts: Vec::new(),
    })
    .unwrap()
}

/// One inventory row with a detection linked through its track, so `evidence` can read its level
/// and the receive chain that measured it.
fn row(
    r: &mut Repository,
    survey: SurveyId,
    provenance: ProvenanceId,
    f: f64,
    obw: f64,
) -> EmitterId {
    let seen = tr(0, 45);
    let track_id = TrackId::new();
    let id = r
        .record_sighting(
            &Sighting {
                source: LinkTarget::Track(track_id),
                seen,
                count: 4,
                f_center_hz: f,
                bandwidth_hz: obw,
                fingerprint: Some(Fingerprint {
                    duty_cycle: Some(1.0),
                    ..Fingerprint::new(f, obw)
                }),
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
        xdb_bandwidth_hz: Some(obw * 0.5),
        xdb_level_db: Some(-3.0),
        snr_peak_db: 20.0,
        snr_mean_db: 17.0,
        peak_level_dbfs: -30.0,
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

/// T-317's three members plus real unrelated broadcast stations, all on one front end. See
/// `crate::harmonic_tests` for where the member frequencies come from.
fn t317_scene(device: &str) -> (Repository, Vec<EmitterId>) {
    let (mut r, survey) = scene();
    let p = prov_on(&mut r, device);
    let f = (100_465_339.0 - 400.0) / 43.0;
    let members = vec![
        row(&mut r, survey, p, 100_465_339.0, 43.0 * 138.0),
        row(&mut r, survey, p, 44.0 * f, 44.0 * 120.0),
        row(&mut r, survey, p, 45.0 * f + 409.0, 45.0 * 137.0),
    ];
    for station in [91_300_000.0, 95_800_000.0, 99_692_500.0, 103_400_000.0] {
        row(&mut r, survey, p, station, 180_000.0);
    }
    (r, members)
}

#[test]
fn the_resolution_pass_finds_t317s_family_and_stores_it() {
    let (mut r, members) = t317_scene("hackrf-0");
    let out = r
        .resolve_harmonic_families("hk-pipeline/harmonic@1", t(50))
        .unwrap();
    assert_eq!(out.len(), 1, "{out:#?}");
    let stored = r.harmonic_families().unwrap();
    assert_eq!(stored.len(), 1);
    let fam = &stored[0];
    assert!(fam.active);
    assert_eq!(fam.family.device_id, "hackrf-0");
    assert_eq!(
        fam.family
            .members
            .iter()
            .map(|m| m.index)
            .collect::<Vec<_>>(),
        vec![43, 44, 45]
    );
    assert_eq!(
        fam.family
            .members
            .iter()
            .map(|m| m.emitter_id)
            .collect::<Vec<_>>(),
        members
    );
    assert!((fam.family.f0_hz - 2_336_398.0).abs() < 50.0);
    // The reasoning is stored and disclosed, arithmetic included.
    assert!(fam.reason.contains("never detected"), "{}", fam.reason);
    assert!(fam.reason.contains("2.336"), "{}", fam.reason);
    // Every member can find the family from its own row — the hook an emitter-relations view
    // needs.
    for m in &members {
        let rows = r.harmonic_families_for_emitter(*m).unwrap();
        assert_eq!(rows.len(), 1, "emitter {m} carries its family");
        assert_eq!(rows[0].family_id, fam.family_id);
    }
    // None of the real stations was swept in.
    assert_eq!(fam.family.members.len(), 3);
}

#[test]
fn the_resolution_pass_is_idempotent() {
    let (mut r, _) = t317_scene("hackrf-0");
    assert_eq!(
        r.resolve_harmonic_families("rule@1", t(50)).unwrap().len(),
        1
    );
    assert!(
        r.resolve_harmonic_families("rule@1", t(60))
            .unwrap()
            .is_empty(),
        "a second pass over unchanged measurements appends nothing"
    );
    assert_eq!(r.harmonic_families().unwrap().len(), 1);
}

#[test]
fn a_family_is_revoked_by_appending_and_the_retired_claim_stays_readable() {
    let (mut r, members) = t317_scene("hackrf-0");
    let claimed = r.resolve_harmonic_families("rule@1", t(50)).unwrap();
    let id = claimed[0].family_id;
    let revocation = r
        .revoke_harmonic_family(id, RelationAuthor::User, "token:abc", t(60), "not ours")
        .unwrap();
    assert!(!revocation.active);
    assert_eq!(revocation.supersedes, Some(id));
    // Nothing stands any more...
    assert!(r.harmonic_families().unwrap().is_empty());
    // ...but both rows, and the reasoning of each, remain readable from every member.
    let history = r.harmonic_families_for_emitter(members[0]).unwrap();
    assert_eq!(history.len(), 2, "{history:#?}");
    assert_eq!(history[0].family_id, revocation.family_id);
    assert_eq!(history[1].family_id, id);
    assert!(history[1].reason.contains("never detected"));
    assert_eq!(history[0].reason, "not ours");
    // The members' own rows are untouched by any of it.
    for m in &members {
        assert_eq!(r.emitter(*m).unwrap().id, *m);
    }
}

#[test]
fn a_family_row_can_never_be_updated_or_deleted() {
    let (mut r, _) = t317_scene("hackrf-0");
    let claimed = r.resolve_harmonic_families("rule@1", t(50)).unwrap();
    let id = claimed[0].family_id;
    // The triggers are the guarantee, not the API: go behind it.
    let err = r
        .conn
        .execute(
            "UPDATE harmonic_family SET active = 0 WHERE family_id = ?1",
            [id],
        )
        .unwrap_err()
        .to_string();
    assert!(err.contains("append-only"), "{err}");
    let err = r
        .conn
        .execute("DELETE FROM harmonic_family WHERE family_id = ?1", [id])
        .unwrap_err()
        .to_string();
    assert!(err.contains("append-only"), "{err}");
    let err = r
        .conn
        .execute(
            "DELETE FROM harmonic_family_member WHERE family_id = ?1",
            [id],
        )
        .unwrap_err()
        .to_string();
    assert!(err.contains("append-only"), "{err}");
}

#[test]
fn a_family_never_spans_two_front_ends() {
    // T-302/T-259. The same three emissions, but member 45 measured only on a second device. The
    // arithmetic is identical and no family is claimed, because an oscillator in one front end
    // cannot put a line into another front end's antenna.
    let (mut r, survey) = scene();
    let a = prov_on(&mut r, "hackrf-0");
    let b = prov_on(&mut r, "hackrf-1");
    let f = (100_465_339.0 - 400.0) / 43.0;
    row(&mut r, survey, a, 100_465_339.0, 43.0 * 138.0);
    row(&mut r, survey, a, 44.0 * f, 44.0 * 120.0);
    row(&mut r, survey, b, 45.0 * f + 409.0, 45.0 * 137.0);
    assert!(
        r.resolve_harmonic_families("rule@1", t(50))
            .unwrap()
            .is_empty()
    );
    assert!(r.harmonic_families().unwrap().is_empty());
    // ...and the same geometry on one chain still claims it.
    let (mut r2, _) = t317_scene("hackrf-0");
    assert_eq!(
        r2.resolve_harmonic_families("rule@1", t(50)).unwrap().len(),
        1
    );
}

#[test]
fn real_unrelated_stations_alone_are_never_declared_a_family() {
    let (mut r, survey) = scene();
    let p = prov_on(&mut r, "hackrf-0");
    for station in [
        88_600_000.0,
        91_300_000.0,
        95_800_000.0,
        97_700_000.0,
        99_692_500.0,
        101_100_000.0,
        103_400_000.0,
        107_100_000.0,
    ] {
        row(&mut r, survey, p, station, 180_000.0);
    }
    assert!(
        r.resolve_harmonic_families("rule@1", t(50))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_family_survives_the_round_trip_with_its_numbers() {
    let (mut r, _) = t317_scene("hackrf-0");
    let claimed = r.resolve_harmonic_families("rule@1", t(50)).unwrap();
    let read = r.harmonic_families().unwrap();
    let (a, b) = (&claimed[0].family, &read[0].family);
    assert_eq!(a.f0_hz, b.f0_hz);
    assert_eq!(a.intercept_hz, b.intercept_hz);
    assert_eq!(a.intercept_se_hz, b.intercept_se_hz);
    assert_eq!(a.index_pin, b.index_pin);
    assert_eq!(a.origin_sigmas, b.origin_sigmas);
    assert_eq!(a.origin_fraction, b.origin_fraction);
    assert_eq!(a.residual_rms_hz, b.residual_rms_hz);
    assert_eq!(a.width.sigma0_hz, b.width.sigma0_hz);
    assert_eq!(a.width.separates, b.width.separates);
    assert_eq!(a.members, b.members);
    assert_eq!(claimed[0].t, read[0].t);
    assert_eq!(claimed[0].actor, read[0].actor);
}
