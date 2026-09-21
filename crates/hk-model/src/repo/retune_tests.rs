//! T-598 (AWARE-011): the cross-centre retune verdict, **persisted**.
//!
//! The scene is T-586's physics in miniature, built directly as stored rows: one region visited
//! from three centres 500 kHz apart, holding
//!
//! - a **real emitter** at a fixed absolute frequency, seen from every centre (entity resolution
//!   already gives it one row), and
//! - an **LO-relative spur** at a fixed +370 kHz offset from the tuning centre, which therefore
//!   lands at a *different absolute frequency* at each centre and mints **three** rows.
//!
//! Nothing about a line's shape distinguishes the two here: both are the same width, level and
//! duration. Only the behaviour across centres separates them, which is the point.
//!
//! The truth ("this one is on the air, that one is in the receiver") lives only in the assertions.

use super::Repository;
use crate::detection::SpurReason;
use crate::relate::*;
use crate::retune::{RetuneSlope, RetuneTolerance};
use crate::*;

/// The spur's fixed offset from the tuning centre, Hz. Arbitrary, and deliberately not a
/// round fraction of the sample rate, so no single-capture rule can recognise it.
const SPUR_OFFSET_HZ: f64 = 370e3;

/// The centres the region was surveyed from.
const CENTRES: [f64; 3] = [100.0e6, 100.5e6, 101.0e6];

/// The real emitter's absolute frequency: in every capture's passband, and not on any centre.
const EMITTER_HZ: f64 = 100.68e6;

fn t(sec: i64) -> Timestamp {
    Timestamp::from_unix_nanos(1_789_000_000_000_000_000 + sec * 1_000_000_000)
}

fn tr(a: i64, b: i64) -> TimeRange {
    TimeRange::new(t(a), t(b))
}

fn scene() -> (Repository, SurveyId) {
    let mut r = Repository::open_in_memory().unwrap();
    let plan = ScanPlan {
        id: ScanPlanId::new(),
        version: 1,
        name: "retune".into(),
        created_at: t(0),
        regions: vec![PlanRegion {
            freq: FreqRange::new(99e6, 102e6),
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

fn prov(r: &mut Repository, lo: f64) -> ProvenanceId {
    r.intern_provenance(&Provenance {
        device_id: "test".into(),
        tune: Tune {
            center_hz: lo,
            sample_rate_hz: 4.0e6,
            lna_db: 32.0,
            vga_db: 30.0,
            amp_on: true,
            bandwidth_hz: 3.5e6,
        },
        overload: false,
        quantisation_limited: false,
        temperature_c: None,
        antenna_port: None,
        bias_tee: crate::BiasTee::Unknown,
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

/// One sighting: a track, its detection (carrying the LO through its provenance), and the
/// inventory row entity resolution makes of it. Returns the row and the detection.
fn sighting(
    r: &mut Repository,
    survey: SurveyId,
    lo: f64,
    f: f64,
    seen: TimeRange,
) -> (EmitterId, DetectionId) {
    sighting_flagged(r, survey, lo, f, seen, DetectionFlags::default())
}

/// [`sighting`] whose detection already carries flags from another (single-capture) rule.
fn sighting_flagged(
    r: &mut Repository,
    survey: SurveyId,
    lo: f64,
    f: f64,
    seen: TimeRange,
    flags: DetectionFlags,
) -> (EmitterId, DetectionId) {
    let p = prov(r, lo);
    let track_id = TrackId::new();
    let obw = 12e3;
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
        xdb_bandwidth_hz: Some(obw),
        xdb_level_db: Some(-3.0),
        snr_peak_db: 22.0,
        snr_mean_db: 19.0,
        peak_level_dbfs: -24.0,
        peak_level_dbm: None,
        sk: None,
        clip_count: 0,
        detector_version: "test@1".into(),
        provenance_ref: p,
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
    (id, d.id)
}

/// The survey: three dwells, each holding the real emitter and the LO-relative spur.
struct Survey3 {
    repo: Repository,
    /// The real emitter's row(s) — one, if entity resolution did its job.
    emitter: Vec<EmitterId>,
    /// The spur's rows, one per centre, in centre order.
    spur: Vec<EmitterId>,
    /// The spur's detections, in centre order.
    spur_dets: Vec<DetectionId>,
}

fn survey_three_centres() -> Survey3 {
    survey_three_centres_flagged(DetectionFlags::default())
}

/// The same survey, with `spur_flags` already on the *second* centre's spur detection — standing
/// in for a single-capture rule having flagged it before any retune verdict existed.
fn survey_three_centres_flagged(spur_flags: DetectionFlags) -> Survey3 {
    let (mut repo, sv) = scene();
    let (mut emitter, mut spur, mut spur_dets) = (Vec::new(), Vec::new(), Vec::new());
    for (i, lo) in CENTRES.iter().enumerate() {
        let window = tr(i as i64 * 10, i as i64 * 10 + 5);
        let (e, _) = sighting(&mut repo, sv, *lo, EMITTER_HZ, window);
        if !emitter.contains(&e) {
            emitter.push(e);
        }
        let flags = if i == 1 {
            spur_flags
        } else {
            DetectionFlags::default()
        };
        let (s, d) = sighting_flagged(&mut repo, sv, *lo, lo + SPUR_OFFSET_HZ, window, flags);
        spur.push(s);
        spur_dets.push(d);
    }
    Survey3 {
        repo,
        emitter,
        spur,
        spur_dets,
    }
}

fn shown(r: &Repository) -> Vec<EmitterId> {
    r.query_inventory(&InventoryQuery::default())
        .unwrap()
        .entries
        .iter()
        .map(|e| e.emitter.id)
        .collect()
}

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

/// **The bug, reproduced.** Before the verdict is persisted, one LO-relative spur is three
/// inventory rows at three different absolute frequencies — the user's 2026-09-21 field report.
#[test]
fn t598_before_the_verdict_is_written_one_spur_is_three_emitters() {
    let s = survey_three_centres();
    assert_eq!(s.emitter.len(), 1, "the real emitter is one row");
    assert_eq!(s.spur.len(), CENTRES.len());
    assert_eq!(
        s.spur
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        CENTRES.len(),
        "the spur minted a separate row per centre"
    );
    assert_eq!(
        shown(&s.repo).len(),
        1 + CENTRES.len(),
        "4 rows shown: one real emitter and the same artefact counted three times"
    );
    for (i, id) in s.spur.iter().enumerate() {
        let row = s.repo.emitter(*id).unwrap();
        assert!(
            (row.f_center_hz - (CENTRES[i] + SPUR_OFFSET_HZ)).abs() < 1e3,
            "spur row {i} sits at its own absolute frequency"
        );
        assert_eq!(
            s.repo.detection_retune(s.spur_dets[i]).unwrap(),
            None,
            "no verdict is stored until one is resolved"
        );
    }
}

/// **The fix.** One pass resolves the family: the three sightings become one shown artefact, the
/// real emitter is untouched in number *and* in absolute frequency, and every stored detection
/// carries its verdict.
#[test]
fn t598_the_verdict_is_persisted_and_the_inventory_shows_one_artefact() {
    let mut s = survey_three_centres();
    let out = s
        .repo
        .resolve_retune(
            s.spur[0],
            "test/retune@1",
            t(30),
            &RetuneTolerance::default(),
        )
        .unwrap();

    // Anti-vacuity: the pass really did compare three centres and every line of the scene.
    assert_eq!(out.centres, CENTRES.len(), "distinct LOs compared");
    assert_eq!(out.rows, 1 + CENTRES.len(), "inventory rows compared");
    assert_eq!(
        out.observations,
        2 * CENTRES.len(),
        "detections compared: two lines at each of three centres"
    );

    // One family, one shown artefact.
    assert_eq!(out.families.len(), 1, "families: {:?}", out.families);
    let family = &out.families[0];
    assert_eq!(family.slope, RetuneSlope::LoLocked);
    assert!(
        (family.invariant_hz - SPUR_OFFSET_HZ).abs() < 10e3,
        "the family's invariant coordinate is the LO offset: {:.1} Hz",
        family.invariant_hz
    );
    assert_eq!(family.centres, CENTRES.len());
    assert_eq!(family.deferred.len(), CENTRES.len() - 1);

    let shown_now = shown(&s.repo);
    assert_eq!(
        shown_now.len(),
        2,
        "one real emitter and ONE artefact, not {}",
        1 + CENTRES.len()
    );
    assert!(
        shown_now.contains(&s.emitter[0]),
        "the emitter is still shown"
    );
    assert_eq!(
        shown_now.iter().filter(|id| s.spur.contains(id)).count(),
        1,
        "exactly one of the three spur sightings represents the family"
    );
    // The emitter's own measurement is untouched.
    let row = s.repo.emitter(s.emitter[0]).unwrap();
    assert!((row.f_center_hz - EMITTER_HZ).abs() < 10e3);

    // Nothing was deleted: every sighting is still a row, with its own time extent.
    assert_eq!(
        every(&s.repo).len(),
        1 + CENTRES.len(),
        "no row was deleted"
    );
    for id in &family.deferred {
        let kept = s.repo.emitter(*id).unwrap();
        assert!(kept.last_seen >= kept.first_seen, "its extent is intact");
        let rel = s.repo.emitter_relations(*id).unwrap();
        assert_eq!(rel.len(), 1);
        assert_eq!(rel[0].kind, RelationKind::RetuneSiblingOf);
        assert_eq!(rel[0].source_id, family.primary);
        assert!(rel[0].active);
        assert!(
            rel[0].reason.contains("lo-locked"),
            "the claim discloses its arithmetic: {}",
            rel[0].reason
        );
    }

    // The verdict is on every stored detection row, and it is the right one.
    assert_eq!(out.detections_marked, 2 * CENTRES.len());
    for det in &s.spur_dets {
        let v = s.repo.detection_retune(*det).unwrap().unwrap();
        assert_eq!(v.slope, RetuneSlope::LoLocked);
        assert!((v.invariant_hz - SPUR_OFFSET_HZ).abs() < 10e3);
        assert_eq!(v.centres, CENTRES.len());
        let d = s.repo.detection(*det).unwrap();
        assert!(d.flags.spur_candidate, "the row carries the suspect flag");
        assert_eq!(d.flags.spur_reason, Some(SpurReason::LoRelative));
    }
}

/// **`Absolute` is the weak claim.** The real emitter is resolved absolute-invariant, which
/// records evidence and sets no flag: "not LO-relative" is not "real" (a reference harmonic is
/// absolute-fixed and still an artefact), so nothing here promotes or clears anything.
#[test]
fn t598_an_absolute_verdict_records_evidence_and_flags_nothing() {
    let mut s = survey_three_centres();
    s.repo
        .resolve_retune(
            s.spur[0],
            "test/retune@1",
            t(30),
            &RetuneTolerance::default(),
        )
        .unwrap();
    let dets = s
        .repo
        .detections_in_region(&Region::new(
            FreqRange::centered(EMITTER_HZ, 20e3),
            tr(-1, 100),
        ))
        .unwrap();
    assert_eq!(
        dets.len(),
        CENTRES.len(),
        "the emitter was seen at each centre"
    );
    for d in &dets {
        let v = s.repo.detection_retune(d.id).unwrap().unwrap();
        assert_eq!(v.slope, RetuneSlope::Absolute);
        assert!((v.invariant_hz - EMITTER_HZ).abs() < 20e3);
        assert_eq!(v.centres, CENTRES.len());
        assert_eq!(v.flag_bits, 0, "absolute implies no flag bits");
        assert!(!d.flags.spur_candidate, "absolute sets no suspect flag");
        assert!(!d.flags.image_candidate);
        assert_eq!(d.flags.spur_reason, None);
    }
    assert!(
        s.repo.emitter_relations(s.emitter[0]).unwrap().is_empty(),
        "an absolute row defers to nothing"
    );
}

/// **The claim is revocable, exactly.** A resolved family whose evidence is then withdrawn —
/// here by deleting the other sightings' rows, so the surviving row is alone on its coordinate —
/// gets its relation revoked by an appended row, and its detections go back to *exactly* the
/// flags the single-capture flaggers gave them, not to a guess about which bits to unset.
#[test]
fn t598_an_lo_relative_verdict_is_revocable_and_restores_the_prior_flags() {
    // A single-capture rule already flagged the middle centre's spur detection for its own
    // reason. The retune verdict must not destroy that on the way back out.
    let mut s = survey_three_centres_flagged(DetectionFlags {
        marginal: true,
        ..DetectionFlags::default()
    });
    let marked = s.spur_dets[1];
    let out = s
        .repo
        .resolve_retune(
            s.spur[0],
            "test/retune@1",
            t(30),
            &RetuneTolerance::default(),
        )
        .unwrap();
    let deferred = out.families[0].deferred.clone();
    assert!(!deferred.is_empty());
    assert_eq!(shown(&s.repo).len(), 2);

    // Withdraw the evidence: the other centres' sightings are deleted by a user, so nothing is
    // left to share the invariant coordinate.
    for id in s.spur.iter().skip(1) {
        s.repo
            .change_emitter_lifecycle(
                *id,
                LifecycleState::Deleted,
                LifecycleAuthor::User,
                "test",
                "withdrawn",
                t(40),
            )
            .unwrap();
    }
    let again = s
        .repo
        .resolve_retune(
            s.spur[0],
            "test/retune@1",
            t(50),
            &RetuneTolerance::default(),
        )
        .unwrap();
    assert!(
        again.families.is_empty(),
        "no family survives the withdrawal: {:?}",
        again.families
    );
    assert_eq!(
        s.repo.detection_retune(s.spur_dets[0]).unwrap(),
        None,
        "the verdict on the surviving sighting is withdrawn, not left standing"
    );
    let d = s.repo.detection(s.spur_dets[0]).unwrap();
    assert!(
        !d.flags.spur_candidate,
        "its prior flags are restored exactly"
    );
    assert_eq!(d.flags.spur_reason, None);
    assert!(again.detections_cleared >= 1);

    // The revocation is an appended row, and the history keeps the claim that was made.
    for id in &deferred {
        assert!(
            s.repo
                .emitter_relations(*id)
                .unwrap()
                .iter()
                .all(|r| r.kind != RelationKind::RetuneSiblingOf),
            "no retune claim stands on a row the evidence no longer supports"
        );
        assert!(
            s.repo
                .emitter_relation_history(*id)
                .unwrap()
                .iter()
                .any(|r| r.kind == RelationKind::RetuneSiblingOf),
            "but the claim that was made is kept"
        );
    }
    // The other rule's flag on the deleted sighting's detection survived the round trip.
    assert!(s.repo.detection(marked).unwrap().flags.marginal);
}

/// One centre is no evidence: below two distinct LOs the classifier says nothing rather than
/// guessing, so nothing is written and nothing is hidden.
#[test]
fn t598_one_centre_claims_nothing() {
    let (mut repo, sv) = scene();
    let (a, _) = sighting(&mut repo, sv, CENTRES[0], EMITTER_HZ, tr(0, 5));
    let (b, _) = sighting(
        &mut repo,
        sv,
        CENTRES[0],
        CENTRES[0] + SPUR_OFFSET_HZ,
        tr(0, 5),
    );
    let out = repo
        .resolve_retune(b, "test/retune@1", t(10), &RetuneTolerance::default())
        .unwrap();
    assert_eq!(out.centres, 0, "nothing was compared");
    assert!(out.families.is_empty());
    assert_eq!(out.detections_marked, 0);
    assert_eq!(shown(&repo).len(), 2, "both rows stay visible: {a} {b}");
}

/// T-600: `EMITTER_DETECTION_EVIDENCE_SQL` counted only the flag bits stored ON the detection
/// row, so a detection T-598 resolved as LO-relative still ranked as though nothing were known
/// about it — the standing verdict lives in `detection_retune` (read-path only, per T-598's
/// immutability rule) and the evidence proxy never joined it in.
///
/// The scene gives the real emitter and the spur's family representative *identical* measured
/// evidence (same SNR, peak level and duty cycle, by construction of [`sighting`]), so any rank
/// difference can only come from the retune verdict, not from the underlying measurement —
/// otherwise this would be a vacuous comparison of two rows that already differed.
#[test]
fn t600_resolved_lo_relative_artefact_ranks_below_a_clean_row() {
    let mut s = survey_three_centres();
    let out = s
        .repo
        .resolve_retune(
            s.spur[0],
            "test/retune@1",
            t(30),
            &RetuneTolerance::default(),
        )
        .unwrap();
    assert_eq!(out.families.len(), 1, "families: {:?}", out.families);
    let primary = out.families[0].primary;

    let clean = super::relate::evidence(&s.repo.conn, s.emitter[0])
        .unwrap()
        .unwrap();
    let artefact = super::relate::evidence(&s.repo.conn, primary)
        .unwrap()
        .unwrap();

    // Anti-vacuity: the two rows were measured identically. `evidence()` read three detections
    // for each (one per centre) and the measured evidence agrees exactly.
    assert_eq!(clean.snr_db, artefact.snr_db, "identical measured SNR");
    assert_eq!(
        clean.peak_dbfs, artefact.peak_dbfs,
        "identical measured level"
    );
    assert_eq!(
        clean.duty_cycle, artefact.duty_cycle,
        "identical measured duty cycle"
    );
    assert_eq!(
        clean.suspect_fraction, 0.0,
        "the real emitter's own detections were never flagged"
    );
    assert_eq!(
        artefact.suspect_fraction, 1.0,
        "every one of the artefact's detections now carries the retune verdict's suspect bits"
    );

    assert!(
        artefact.rank() < clean.rank(),
        "a resolved LO-relative artefact (rank {}) must rank strictly below a clean row \
         with equal measured evidence (rank {})",
        artefact.rank(),
        clean.rank()
    );
}

/// T-600, second half: **a one-way join would leave the ranking outliving the claim.** T-598
/// built revocation deliberately (a fourth centre, or here the evidence being withdrawn, appends
/// `active = 0` over a standing verdict), so the evidence proxy must read the *current* standing
/// verdict, not cache the bits from the moment it was first resolved.
#[test]
fn t600_revoking_the_verdict_restores_the_artefacts_rank() {
    let mut s = survey_three_centres();
    let out = s
        .repo
        .resolve_retune(
            s.spur[0],
            "test/retune@1",
            t(30),
            &RetuneTolerance::default(),
        )
        .unwrap();
    let family = out.families[0].clone();
    let primary = family.primary;

    let clean = super::relate::evidence(&s.repo.conn, s.emitter[0])
        .unwrap()
        .unwrap();
    let flagged = super::relate::evidence(&s.repo.conn, primary)
        .unwrap()
        .unwrap();
    assert_eq!(flagged.suspect_fraction, 1.0);
    assert!(
        flagged.rank() < clean.rank(),
        "sanity: the resolved artefact starts ranked below the clean row"
    );

    // Withdraw the diversity behind the verdict, exactly as `t598_an_lo_relative_verdict_is_revocable_...`
    // does: delete the sibling sightings, so a second pass finds nothing left to support the
    // family and appends the revocation.
    for id in &family.deferred {
        s.repo
            .change_emitter_lifecycle(
                *id,
                LifecycleState::Deleted,
                LifecycleAuthor::User,
                "test",
                "withdrawn",
                t(40),
            )
            .unwrap();
    }
    let again = s
        .repo
        .resolve_retune(primary, "test/retune@1", t(50), &RetuneTolerance::default())
        .unwrap();
    assert!(
        again.families.is_empty(),
        "no family survives the withdrawal: {:?}",
        again.families
    );
    assert!(
        again.detections_cleared >= 1,
        "the standing verdict was actually withdrawn, not left standing"
    );
    let idx = s
        .spur
        .iter()
        .position(|id| *id == primary)
        .expect("primary is one of the spur sightings");
    assert_eq!(
        s.repo.detection_retune(s.spur_dets[idx]).unwrap(),
        None,
        "no verdict stands on the primary's own detection after revocation"
    );

    let restored = super::relate::evidence(&s.repo.conn, primary)
        .unwrap()
        .unwrap();
    assert_eq!(
        restored.suspect_fraction, 0.0,
        "revoking the verdict restores the measured (unflagged) evidence"
    );
    assert_eq!(
        restored.rank(),
        clean.rank(),
        "revoking the verdict restores the row's rank — a one-way join would leave the \
         ranking outliving the claim"
    );
}
