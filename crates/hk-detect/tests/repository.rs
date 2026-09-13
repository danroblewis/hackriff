//! Detection rows round-trip through the repository (docs/07 §2.9): batched writes via
//! `DetectionWriter`, provenance interned by value, and every flag combination the detector
//! produces (clipped with counts, overloaded provenance, spur reasons incl. spur-map, image,
//! impulsive, marginal, edge) accepted by the schema.

mod common;

use common::*;
use hk_core::Discontinuity;
use hk_detect::{ClipCount, DetectionWriter, DetectorConfig};
use hk_model::{
    FreqRange, GainTableEntry, PlanRegion, Region, Repository, ScanPlan, ScanPlanId, ScanPolicy,
    Schedule, SpurMask, SpurMaskId, SpurRule, Survey, SurveyId, SurveyState, TimeRange, Timestamp,
};

#[test]
fn detection_records_round_trip_through_the_repository() {
    let mut repo = Repository::open_in_memory().unwrap();
    let plan = ScanPlan {
        id: ScanPlanId::new(),
        version: 1,
        name: "t-006".into(),
        created_at: Timestamp::UNIX_EPOCH,
        regions: vec![PlanRegion {
            freq: FreqRange::new(88e6, 108e6),
            priority: 1.0,
            revisit_ns: None,
        }],
        policy: ScanPolicy::SweepThenDwell,
        gain_table: vec![GainTableEntry {
            freq: FreqRange::new(1e6, 6e9),
            lna_db: 24.0,
            vga_db: 20.0,
            amp_on: false,
            antenna_port: None,
        }],
        schedule: Schedule::Cron {
            expr: "0 * * * *".into(),
        },
        extra: serde_json::json!({}),
    };
    let survey = Survey {
        id: SurveyId::new(),
        plan_id: plan.id,
        plan_version: 1,
        device_id: "synthetic:hk-detect-test".into(),
        state: SurveyState::Open,
        t_start: Timestamp::UNIX_EPOCH,
        t_end: None,
        summary: None,
    };
    let mask = SpurMask {
        id: SpurMaskId::new(),
        supersedes: None,
        device_id: "synthetic:hk-detect-test".into(),
        measured_at: Timestamp::UNIX_EPOCH,
        rules: vec![SpurRule::Spur {
            freq: FreqRange::centered(96.123e6, 4e3),
            level_dbfs: -80.0,
            gain_db: None,
        }],
    };
    repo.insert_scan_plan(&plan).unwrap();
    repo.insert_survey(&survey).unwrap();
    repo.insert_spur_mask(&mask).unwrap();

    let fc = 98e6;
    let mut config = DetectorConfig::new(survey.id);
    config.spur_mask = Some(mask.clone());
    // Two provenance states: normal, then overloaded (a gain step up).
    let normal = provenance(fc, FS, 24.0);
    let overloaded = provenance_full(fc, FS, 32.0, 15e6, true, false);
    let mut s = Scene::new(config, GammaFrames::new(BINS, N_AVG, normal, 31));
    let mut p = flat(BINS);
    add_line(&mut p, s.bin_of(100e6).round() as usize, 3, 20.0); // ref-harmonic
    add_line(&mut p, BINS / 2, 3, 25.0); // dc
    add_line(&mut p, s.bin_of(96.123e6).round() as usize, 3, 18.0); // spur-map
    add_line(&mut p, 2048 + 1700, 3, 20.0); // edge
    add_line(&mut p, 900, 12, 4.0); // marginal
    for (j, w) in [0.3f32, 1.0, 0.5].iter().enumerate() {
        p[2662 + j] += undb(40.0) as f32 * w;
        p[BINS - 2662 - j] += undb(15.0) as f32 * w; // image
    }
    let mut imp = p.clone();
    for k in 0..10 {
        add_line(&mut imp, 1100 + 60 * k, 3, 15.0);
    }
    let samples = u64::from(N_AVG) * BINS as u64;
    for i in 0..30 {
        let clip = if (5..8).contains(&i) {
            ClipCount::new(400, samples)
        } else {
            ClipCount::NONE
        };
        let impulsive = (12..17).contains(&i);
        s.step_with(
            if impulsive { &imp } else { &p },
            Discontinuity::NONE,
            clip,
            impulsive,
        );
    }
    s.switch(overloaded);
    for i in 0..10 {
        let flags = if i == 0 {
            Discontinuity::GAIN_CHANGE
        } else {
            Discontinuity::NONE
        };
        s.step_with(&p, flags, ClipCount::NONE, false);
    }
    s.finish();

    let records = &s.out.detections;
    let flags: Vec<_> = records.iter().map(|d| d.detection.flags).collect();
    assert!(flags.iter().any(|f| f.clipped) && records.iter().any(|d| d.detection.clip_count > 0));
    assert!(flags.iter().any(|f| f.impulsive));
    assert!(flags.iter().any(|f| f.image_candidate));
    assert!(flags.iter().any(|f| f.edge && f.marginal));
    for reason in ["ref-harmonic", "dc", "spur-map"] {
        assert!(
            flags
                .iter()
                .any(|f| f.spur_reason.is_some_and(|r| r.kind_str() == reason)),
            "no {reason} detection"
        );
    }

    let mut writer = DetectionWriter::new(4);
    for r in records {
        writer
            .push(&mut repo, r)
            .expect("the repository accepts every detector record");
    }
    writer.flush(&mut repo).unwrap();
    assert_eq!(writer.written(), records.len() as u64);
    assert_eq!(repo.detection_count().unwrap(), records.len() as u64);
    for r in records {
        let back = repo.detection(r.detection.id).unwrap();
        let mut want = r.detection.clone();
        want.provenance_ref = back.provenance_ref;
        assert_eq!(back, want);
        let chain = repo.provenance_chain(back.provenance_ref).unwrap();
        assert_eq!(chain.provenance, *r.provenance.get());
        assert!(!chain.provenance.overload || back.flags.clipped);
    }
    let region = Region::new(
        FreqRange::new(88e6, 108e6),
        TimeRange::new(
            Timestamp::UNIX_EPOCH,
            Timestamp::from_unix_nanos(10_000_000_000),
        ),
    );
    assert_eq!(
        repo.detections_in_region(&region).unwrap().len(),
        records.len()
    );
}
