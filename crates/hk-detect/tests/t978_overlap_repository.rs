//! **T-978: the overlap the rows cannot resolve is resolved by the spectrum, in the inventory.**
//!
//! The explorer's live HackRF window of 2026-09-25 (window 2, 05:52–06:23, San Francisco) served
//! **two** Candidate rows for one P25 emission — 852.8586 MHz at 9.3 kHz and 852.8591 MHz at
//! 26.2 kHz — and at 861.4346 MHz one 557 kHz box beside the 20–80 kHz fragments it covered. Both
//! violate CLAUDE.md's inventory invariant: *"overlapping Confirmed/Candidate boxes are proof the
//! analysis is wrong … the system detects the overlap and automatically re-analyzes that region to
//! resolve it to the real signal(s), rather than leaving competing boxes stacked."*
//!
//! These drive the real `Repository` through the two stages in order, and assert on the docs/07
//! objects — Emitter rows as the inventory serves them, and the `EmitterRelation` that carries the
//! reasoning:
//!
//! 1. `resolve_overlaps` (T-219 + T-369) — which **detects** the overlap and, on both of the
//!    explorer's geometries, resolves nothing: two rows in, two rows shown. That step's assertions
//!    are the red proof, and they are what a run on `main` does today.
//! 2. `resolve_measured_region` (T-978) over a re-measurement of the union band — one row shown for
//!    the P25 pair, and the 557 kHz box retired in favour of the two emissions it merged.

mod common;

use common::{GammaFrames, Scene, add_line, flat, provenance};
use hk_detect::DetectorConfig;
use hk_detect::overlap::{OverlapConfig, measure_region};
use hk_detect::rules::Geometry;
use hk_detect::{EdgeRule, IntegratedSnapshot};
use hk_model::{
    Detection, DetectionFlags, DetectionId, EmitterId, Fingerprint, FreqRange, InventoryQuery,
    LinkTarget, PlanRegion, Provenance, ProvenanceId, RelationKind, RelationVisibility, Repository,
    ScanPlan, ScanPlanId, ScanPolicy, Schedule, Sighting, SpurMaskId, Survey, SurveyId,
    SurveyState, TimeRange, Timestamp, TimingFeatures, Tolerances, Track, TrackId, TrackState,
    Tune, UnresolvedRegion,
};

const RULE: &str = "test/t978@1";

fn t(sec: i64) -> Timestamp {
    Timestamp::from_unix_nanos(1_789_000_000_000_000_000 + sec * 1_000_000_000)
}

fn scene() -> (Repository, SurveyId, ProvenanceId) {
    let mut r = Repository::open_in_memory().unwrap();
    let plan = ScanPlan {
        id: ScanPlanId::new(),
        version: 1,
        name: "t-978".into(),
        created_at: t(0),
        regions: vec![PlanRegion {
            freq: FreqRange::new(850e6, 870e6),
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
    let p = r
        .intern_provenance(&Provenance {
            device_id: "test".into(),
            tune: Tune {
                center_hz: 856e6,
                sample_rate_hz: 2.4e6,
                lna_db: 32.0,
                vga_db: 30.0,
                amp_on: true,
                bandwidth_hz: 1.75e6,
            },
            overload: false,
            quantisation_limited: false,
            noise_sigma_lsb: None,
            temperature_c: None,
            antenna_port: None,
            bias_tee: hk_model::BiasTee::Unknown,
            clock_source: hk_model::ClockSource::Internal,
            clock_locked: true,
            calibration_state_ref: None,
            spur_mask_ref: None::<SpurMaskId>,
            timestamp_method: hk_model::TimestampMethod::Synthetic,
            timestamp_error_budget_ns: None,
            capture_artefacts: Vec::new(),
        })
        .unwrap();
    (r, survey.id, p)
}

/// One Candidate row from a track sighting, seen over the whole scene, so every row made this way
/// overlaps every other in **time** as well as frequency.
fn candidate(
    r: &mut Repository,
    survey: SurveyId,
    provenance: ProvenanceId,
    f: f64,
    obw: f64,
    snr: f64,
) -> EmitterId {
    candidate_seen(
        r,
        survey,
        provenance,
        f,
        obw,
        snr,
        TimeRange::new(t(0), t(20)),
    )
}

/// One Candidate row from a track sighting over an explicit window, with a detection linked through
/// its track so the rules can read its level, −3 dB width and tuning centre.
#[allow(clippy::too_many_arguments)]
fn candidate_seen(
    r: &mut Repository,
    survey: SurveyId,
    provenance: ProvenanceId,
    f: f64,
    obw: f64,
    snr: f64,
    seen: TimeRange,
) -> EmitterId {
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
        xdb_bandwidth_hz: Some(obw),
        xdb_level_db: Some(-3.0),
        snr_peak_db: snr,
        snr_mean_db: snr - 3.0,
        peak_level_dbfs: -20.0,
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

/// Rows the inventory shows (deferring rows hidden — the Candidate list the user reads).
fn shown(r: &Repository) -> Vec<EmitterId> {
    r.query_inventory(&InventoryQuery::default())
        .unwrap()
        .entries
        .iter()
        .map(|e| e.emitter.id)
        .collect()
}

/// Every live row, deferring or not: nothing is ever deleted.
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

/// The widest unresolved region stage 4 reported, with the members it named. The measured pass takes
/// its members from here and never from a search over the band: see `UnresolvedRegion`.
fn widest(out: &hk_model::repo::OverlapOutcome) -> UnresolvedRegion {
    out.unresolved
        .iter()
        .cloned()
        .reduce(|a, b| {
            if b.region.width_hz() > a.region.width_hz() {
                b
            } else {
                a
            }
        })
        .expect("the region is unresolved from the rows alone")
}

/// The standing `DuplicateOf` claim on `id`, if it defers.
fn defers_to(r: &Repository, id: EmitterId) -> Option<(EmitterId, String)> {
    r.emitter_relations(id)
        .unwrap()
        .into_iter()
        .find(|x| x.active && x.kind == RelationKind::DuplicateOf)
        .map(|x| (x.source_id, x.reason))
}

/// A spectrum over `center_hz` with the named emissions raised over a flat floor.
fn spectrum(
    center_hz: f64,
    bin_hz: f64,
    bins: usize,
    em: &[(f64, f64, f64)],
) -> IntegratedSnapshot {
    let geometry = Geometry::new(
        center_hz,
        bin_hz * bins as f64,
        bins,
        0.0,
        &EdgeRule::default(),
    );
    let floor = 1e-9_f64;
    let mut mean_psd = vec![floor; bins];
    for &(f, w, snr) in em {
        let lo = geometry.bin_at_or_above(f - w / 2.0);
        let hi = geometry.bin_at_or_above(f + w / 2.0).max(lo + 1).min(bins);
        for p in mean_psd.iter_mut().take(hi).skip(lo) {
            *p = floor * 10f64.powf(snr / 10.0);
        }
    }
    IntegratedSnapshot {
        geometry,
        span_s: 1.0,
        mean_psd: mean_psd.clone(),
        mean_floor: vec![floor; bins],
        block_psd: vec![mean_psd],
    }
}

/// **Two candidates for one P25 emission resolve to one.** 9.3 kHz inside 26.2 kHz, 500 Hz apart:
/// the ranking stages never see them (they do not compete) and stage 4's guard calls them two
/// emissions because their recorded widths differ by 2.8x — so on `main` both are shown, for ever.
/// The spectrum over their union measures one emission, and one row is shown.
#[test]
fn two_candidates_for_one_p25_emission_resolve_to_one() {
    let (mut r, sv, p) = scene();
    let narrow = candidate(&mut r, sv, p, 852_858_600.0, 9_300.0, 18.0);
    let wide = candidate(&mut r, sv, p, 852_859_100.0, 26_200.0, 22.0);

    // 1. Stage 4 detects the overlap and cannot resolve it: the red proof.
    let out = r
        .resolve_overlaps(wide, RULE, t(20), &Tolerances::default())
        .unwrap();
    assert!(
        out.duplicates.is_empty() && out.suppressed.is_empty(),
        "the rows themselves resolve nothing: {out:?}"
    );
    assert_eq!(
        out.unresolved.len(),
        1,
        "but the region is reported unresolved, which is what triggers the re-analysis"
    );
    assert_eq!(
        shown(&r).len(),
        2,
        "on the rows alone both boxes are served for one emission - the explorer's report"
    );

    // 2. Re-measure the union band and apply it. 2.4 Msps / 2048 bins = 1.17 kHz cells.
    let snap = spectrum(
        852_860_000.0,
        1_171.875,
        2048,
        &[(852_859_100.0, 26_200.0, 22.0)],
    );
    let u = out.unresolved[0].clone();
    let m = measure_region(u.region, &snap, &OverlapConfig::default()).unwrap();
    assert_eq!(m.emissions.len(), 1, "one emission is measured there");
    let applied = r
        .resolve_measured_region(&u, &m, RULE, t(20), &Tolerances::default())
        .unwrap();
    assert_eq!(applied.duplicates.len(), 1, "exactly one row is retired");

    assert_eq!(
        shown(&r),
        vec![wide],
        "the box that reads the measured emission is the one shown"
    );
    assert_eq!(every(&r).len(), 2, "and nothing was deleted");
    let (of, why) = defers_to(&r, narrow).expect("the narrow box defers");
    assert_eq!(of, wide);
    assert!(
        why.contains("another reading") && why.contains("852.859"),
        "the reason states the measurement: {why}"
    );
}

/// **A merged box over two separated emitters splits.** The explorer's 861.43 MHz case: a 557 kHz
/// candidate beside the fragments it covered. The spectrum separates two emissions there, so the
/// wide box is a merge of them and not an emission — it is retired and the two real emissions stay.
#[test]
fn a_merged_box_over_two_emitters_splits() {
    let (mut r, sv, p) = scene();
    let merged = candidate(&mut r, sv, p, 861_434_600.0, 557_000.0, 24.0);
    let low = candidate(&mut r, sv, p, 861_335_000.0, 30_000.0, 22.0);
    let high = candidate(&mut r, sv, p, 861_450_000.0, 40_000.0, 26.0);

    let out = r
        .resolve_overlaps(merged, RULE, t(20), &Tolerances::default())
        .unwrap();
    assert!(
        out.duplicates.is_empty(),
        "the rows themselves resolve nothing: {out:?}"
    );
    assert_eq!(
        shown(&r).len(),
        3,
        "three boxes over two emissions, on main"
    );
    assert!(!out.unresolved.is_empty(), "and the region is unresolved");

    let snap = spectrum(
        861_430_000.0,
        1_171.875,
        2048,
        &[
            (861_335_000.0, 30_000.0, 22.0),
            (861_450_000.0, 40_000.0, 26.0),
        ],
    );
    let u = widest(&out);
    let m = measure_region(u.region, &snap, &OverlapConfig::default()).unwrap();
    assert_eq!(m.emissions.len(), 2, "the spectrum separates two emissions");
    r.resolve_measured_region(&u, &m, RULE, t(20), &Tolerances::default())
        .unwrap();

    let mut listed = shown(&r);
    listed.sort();
    let mut real = vec![low, high];
    real.sort();
    assert_eq!(
        listed, real,
        "the two measured emissions are what is shown; the 557 kHz merge is not"
    );
    assert_eq!(every(&r).len(), 3, "and nothing was deleted");
    let (_, why) = defers_to(&r, merged).expect("the merged box defers");
    assert!(
        why.contains("covers 2 emissions the spectrum separates"),
        "the reason states what it merged: {why}"
    );
}

/// The claim is **revocable**: when the spectrum stops showing one emission there, the retired row
/// comes back on the next re-analysis. Exploration-first — a relationship is ranked evidence, never
/// truth, and never an automatic delete.
#[test]
fn the_retirement_is_revoked_when_the_spectrum_changes() {
    let (mut r, sv, p) = scene();
    let narrow = candidate(&mut r, sv, p, 852_858_600.0, 9_300.0, 18.0);
    let wide = candidate(&mut r, sv, p, 852_859_100.0, 26_200.0, 22.0);
    let cfg = OverlapConfig::default();
    let u = widest(
        &r.resolve_overlaps(wide, RULE, t(20), &Tolerances::default())
            .unwrap(),
    );

    let one = spectrum(
        852_860_000.0,
        1_171.875,
        2048,
        &[(852_859_100.0, 26_200.0, 22.0)],
    );
    let m = measure_region(u.region, &one, &cfg).unwrap();
    r.resolve_measured_region(&u, &m, RULE, t(20), &Tolerances::default())
        .unwrap();
    assert_eq!(shown(&r), vec![wide]);

    // Now the region measures two separated emissions, one under each box.
    let two = spectrum(
        852_860_000.0,
        1_171.875,
        2048,
        &[
            (852_852_000.0, 8_000.0, 18.0),
            (852_868_000.0, 8_000.0, 22.0),
        ],
    );
    let m2 = measure_region(u.region, &two, &cfg).unwrap();
    assert_eq!(m2.emissions.len(), 2);
    let out = r
        .resolve_measured_region(&u, &m2, RULE, t(40), &Tolerances::default())
        .unwrap();
    assert!(!out.revoked.is_empty(), "the standing claim is revoked");
    let mut listed = shown(&r);
    listed.sort();
    let mut both = vec![narrow, wide];
    both.sort();
    assert_eq!(listed, both, "both rows are shown again");
}

// ---------------------------------------------------------------------------------------------
// The measurement taken from the real detector, and the state that persists in a live run
// ---------------------------------------------------------------------------------------------

/// The integrated spectrum **the real `Detector` measured**, after frames carrying `emissions`
/// (absolute centre, width, SNR over the floor) were pushed through `Detector::process` exactly as
/// the pipeline's detect reader pushes them. Not a hand-built PSD: `mean_psd` and `mean_floor` here
/// are the detector's own sliding integration over Gamma-distributed frames.
fn detector_spectrum(
    fc: f64,
    fs: f64,
    bins: usize,
    emissions: &[(f64, f64, f64)],
) -> IntegratedSnapshot {
    const N_AVG: u32 = 10;
    let mut s = Scene::new(
        DetectorConfig::new(SurveyId::new()),
        GammaFrames::new(bins, N_AVG, provenance(fc, fs, 24.0), 978),
    );
    let bin_hz = fs / bins as f64;
    let mut p = flat(bins);
    for &(f, w, snr) in emissions {
        let centre = s.bin_of(f).round().max(0.0) as usize;
        add_line(&mut p, centre, (w / bin_hz).round().max(1.0) as usize, snr);
    }
    // Two integration blocks (`IntegrationConfig`: 0.25 s each) so an evaluation exists.
    let frames = (2.0 * 0.25 / s.src.frame_period_s()).ceil() as usize + 4;
    for _ in 0..frames {
        s.step(&p);
    }
    s.det
        .integrated_snapshot()
        .expect("the detector integrated at least one block")
}

/// **The regression the review named.** Stage 3's pairwise pass revokes standing `DuplicateOf`
/// claims whenever `bands_compete` fails or `distinguishing_evidence` fires — which is exactly when
/// the measured pass runs — so with only T-369's marker filtered out of `revoke_kind`, every later
/// touch of either row undid the measurement, stage 4 reported the region unresolved again and the
/// retired box came back until a fresh snapshot arrived. The inventory flickered and two relation
/// rows were appended per touch, unbounded.
///
/// So: resolve, measure, apply — then touch **both** rows again, repeatedly, as a live run does, and
/// the region must stay settled at one row with no further relation rows written.
#[test]
fn the_retirement_survives_every_later_touch() {
    let (mut r, sv, p) = scene();
    let narrow = candidate(&mut r, sv, p, 852_858_600.0, 9_300.0, 18.0);
    let wide = candidate(&mut r, sv, p, 852_859_100.0, 26_200.0, 22.0);
    let tol = Tolerances::default();

    let out = r.resolve_overlaps(wide, RULE, t(20), &tol).unwrap();
    let u = out.unresolved[0].clone();
    let snap = detector_spectrum(
        852_860_000.0,
        2.4e6,
        2048,
        &[(852_859_100.0, 26_200.0, 22.0)],
    );
    let m = measure_region(u.region, &snap, &OverlapConfig::default()).unwrap();
    assert_eq!(
        m.emissions.len(),
        1,
        "the detector's own integrated spectrum shows one emission: {:?}",
        m.emissions
    );
    r.resolve_measured_region(&u, &m, RULE, t(20), &tol)
        .unwrap();
    assert_eq!(shown(&r), vec![wide]);

    let after_first = r.emitter_relations(narrow).unwrap().len();
    for round in 1..=6 {
        for id in [wide, narrow] {
            let again = r.resolve_overlaps(id, RULE, t(20 + round), &tol).unwrap();
            assert_eq!(
                shown(&r),
                vec![wide],
                "round {round}: the retired box came back on a later touch ({again:?})"
            );
            assert!(
                !again.unresolved.is_empty(),
                "round {round}: the region stays reported, so a fresh measurement keeps coming \
                 and the claim stays revocable"
            );
        }
    }
    assert_eq!(
        r.emitter_relations(narrow).unwrap().len(),
        after_first,
        "a settled region writes no relation rows per touch"
    );
    assert!(
        defers_to(&r, narrow).is_some(),
        "and the claim is still the one the measurement made"
    );
}

/// The same for the merged box: three boxes over two emissions the real detector separates, settled
/// at the two real emissions and staying settled across later touches of every row.
#[test]
fn the_split_survives_every_later_touch() {
    let (mut r, sv, p) = scene();
    let merged = candidate(&mut r, sv, p, 861_434_600.0, 557_000.0, 24.0);
    let low = candidate(&mut r, sv, p, 861_335_000.0, 30_000.0, 22.0);
    let high = candidate(&mut r, sv, p, 861_450_000.0, 40_000.0, 26.0);
    let tol = Tolerances::default();

    let out = r.resolve_overlaps(merged, RULE, t(20), &tol).unwrap();
    let u = widest(&out);
    let snap = detector_spectrum(
        861_430_000.0,
        2.4e6,
        2048,
        &[
            (861_335_000.0, 30_000.0, 22.0),
            (861_450_000.0, 40_000.0, 26.0),
        ],
    );
    let m = measure_region(u.region, &snap, &OverlapConfig::default()).unwrap();
    assert_eq!(
        m.emissions.len(),
        2,
        "the detector's own integrated spectrum separates two emissions: {:?}",
        m.emissions
    );
    r.resolve_measured_region(&u, &m, RULE, t(20), &tol)
        .unwrap();

    let mut real = vec![low, high];
    real.sort();
    for round in 1..=4 {
        for id in [merged, low, high] {
            r.resolve_overlaps(id, RULE, t(20 + round), &tol).unwrap();
            let mut listed = shown(&r);
            listed.sort();
            assert_eq!(
                listed, real,
                "round {round}: the 557 kHz merge came back on a later touch"
            );
        }
    }
    assert_eq!(every(&r).len(), 3, "and nothing was deleted");
}

/// **The review's second finding, and the rule it breaks.** The measured pass used to collect its
/// members with a frequency-only query over `ANY_TIME`, so a spectrum measured *now* could retire a
/// live box as a duplicate of a row that was on air hours ago — and the default inventory filter
/// hides a deferring row while the past row sits outside the viewed window, so the emission actually
/// transmitting would have no box at all. That is "the inventory is time-scoped to the view" and
/// "whenever data exists for that window it must be shown", both broken at once, and it is likeliest
/// in the 902–928 MHz band, where repeated bursts leave many past rows on the same frequencies.
///
/// The members now travel with the region from stage 4 (`UnresolvedRegion`), which grew them from the
/// touched row under `boxes_overlap` — time **and** frequency. The proof below is structural: the two
/// past rows' bands do overlap the measured region, so a frequency-only search would have found
/// them, and they are excluded anyway because they were not on air when it was measured.
///
/// The past rows are 60 and 100 kHz against the live 26.2 and 9.3 kHz: wide enough apart that entity
/// resolution keeps them separate rows rather than folding each into the live emitter as a second
/// presence interval, which is what it correctly does to two boxes of similar width at one centre.
#[test]
fn a_past_row_never_takes_a_live_emissions_box() {
    let (mut r, sv, p) = scene();
    let tol = Tolerances::default();
    let this_morning = TimeRange::new(t(0), t(5));
    let now = TimeRange::new(t(100), t(120));
    let past_a = candidate_seen(&mut r, sv, p, 852_869_000.0, 60_000.0, 20.0, this_morning);
    let past_b = candidate_seen(&mut r, sv, p, 852_869_000.0, 100_000.0, 21.0, this_morning);
    let live_wide = candidate_seen(&mut r, sv, p, 852_859_100.0, 26_200.0, 22.0, now);
    let live_narrow = candidate_seen(&mut r, sv, p, 852_858_600.0, 9_300.0, 18.0, now);
    for (a, b) in [(past_a, past_b), (past_a, live_wide), (past_b, live_narrow)] {
        assert_ne!(a, b, "the scene needs four distinct rows");
    }

    let out = r.resolve_overlaps(live_wide, RULE, t(120), &tol).unwrap();
    let u = widest(&out);
    assert!(
        u.members.contains(&live_wide) && u.members.contains(&live_narrow),
        "the live overlap is the region: {:?}",
        u.members
    );
    for past in [past_a, past_b] {
        let band = r.emitter(past).unwrap().freq();
        assert!(
            band.overlaps(&u.region),
            "the proof needs the past row inside the measured band, so that only time excludes it"
        );
        assert!(
            !u.members.contains(&past),
            "a row that stopped at t+5 s is not part of an overlap measured at t+120 s"
        );
    }

    let snap = detector_spectrum(
        852_860_000.0,
        2.4e6,
        2048,
        &[(852_869_000.0, 60_000.0, 20.0)],
    );
    let m = measure_region(u.region, &snap, &OverlapConfig::default()).unwrap();
    r.resolve_measured_region(&u, &m, RULE, t(120), &tol)
        .unwrap();

    // Neither past row was claimed against — a spectrum measured now never rewrites the catalogue.
    for past in [past_a, past_b] {
        assert!(defers_to(&r, past).is_none());
    }
    // And no live box was retired in favour of one.
    for live in [live_wide, live_narrow] {
        if let Some((of, why)) = defers_to(&r, live) {
            assert!(
                of != past_a && of != past_b,
                "a live box was retired as a duplicate of a row that had stopped: {why}"
            );
        }
    }
    let listed = shown(&r);
    assert!(
        listed.contains(&live_wide) || listed.contains(&live_narrow),
        "the emission on air must keep a box; shown = {listed:?}"
    );
}
