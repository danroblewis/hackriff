//! T-904 review, blocker 2: the channel plan learned over detection **rollups** (what occupancy
//! reads past the retention age, and relearns from after a restart) publishes no phantom channel.
//!
//! Rows no track links (T-075 short bursts are stored untracked; tentative tracks that never
//! confirm drop their links) used to share one rollup per survey and provenance whatever their
//! frequency, so two bursts at opposite edges of a 20 MHz window inside the 10 s gap became one
//! rollup spanning the window — and `DetectionExtent::of_rollup` read it as an emission at the
//! window's middle, where nothing ever transmitted. The store is driven through the real
//! `Repository::prune_detections`; the assertion is on the published `Channel`s.

use hk_context::occupancy::channels::{ChannelPlan, DetectionExtent, LearnConfig};
use hk_model::*;

const T0: i64 = 1_789_000_000_000_000_000;

fn t(ms: i64) -> Timestamp {
    Timestamp::from_unix_nanos(T0 + ms * 1_000_000)
}

fn seed(repo: &mut Repository) -> (SurveyId, ProvenanceId) {
    let plan = ScanPlan {
        id: ScanPlanId::new(),
        version: 1,
        name: "t-904".into(),
        created_at: t(0),
        regions: vec![PlanRegion {
            freq: FreqRange::new(89e6, 109e6),
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
    repo.insert_scan_plan(&plan).unwrap();
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
    repo.insert_survey(&survey).unwrap();
    let prov = repo
        .intern_provenance(&Provenance {
            device_id: "test".into(),
            tune: Tune {
                center_hz: 99e6,
                sample_rate_hz: 20e6,
                lna_db: 24.0,
                vga_db: 20.0,
                amp_on: false,
                bandwidth_hz: 20e6,
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
        .unwrap();
    (survey.id, prov)
}

fn burst(survey: SurveyId, prov: ProvenanceId, f_hz: f64, start_ms: i64) -> Vec<Detection> {
    (0..3)
        .map(|i| {
            let a = start_ms + i * 100;
            Detection {
                id: DetectionId::new(),
                survey_id: survey,
                time: TimeRange::new(t(a), t(a + 100)),
                f_center_hz: f_hz,
                obw_hz: 12e3,
                xdb_bandwidth_hz: None,
                xdb_level_db: None,
                snr_peak_db: 20.0,
                snr_mean_db: 15.0,
                peak_level_dbfs: -40.0,
                peak_level_dbm: None,
                sk: None,
                clip_count: 0,
                detector_version: "test@1".into(),
                provenance_ref: prov,
                flags: DetectionFlags::default(),
            }
        })
        .collect()
}

#[test]
fn a_channel_plan_learned_over_untracked_rollups_has_no_phantom_channel() {
    let mut repo = Repository::open_in_memory().unwrap();
    let (survey, prov) = seed(&mut repo);
    let (lo, hi) = (90.2e6, 107.8e6);
    // Five minutes of untracked 300 ms bursts: the low edge every 4 s, the high edge 2 s later.
    for k in 0..75 {
        let at = k * 4_000;
        repo.insert_detections(&burst(survey, prov, lo, at))
            .unwrap();
        repo.insert_detections(&burst(survey, prov, hi, at + 2_000))
            .unwrap();
    }
    // Two hours later: the watermark that ages all of it out.
    repo.insert_detections(&burst(survey, prov, 99.5e6, 7_200_000))
        .unwrap();
    let report = repo
        .prune_detections(
            &DetectionRetention {
                max_age_ns: 600_000_000_000,
                keep_per_emitter: 0,
                ..DetectionRetention::default()
            },
            || true,
        )
        .unwrap();
    assert_eq!(report.deleted, 450, "{report:?}");

    let window = Region::new(
        FreqRange::new(89e6, 109e6),
        TimeRange::new(t(0), t(400_000)),
    );
    let rollups = repo.detection_rollups_in_region(&window).unwrap();
    assert!(!rollups.is_empty());
    for r in &rollups {
        assert!(
            r.freq.width_hz() < 1e6,
            "a rollup spans only its own emission, not the window: {r:?}"
        );
    }
    let extents: Vec<DetectionExtent> = rollups.iter().map(DetectionExtent::of_rollup).collect();
    let mut plan = ChannelPlan::new(1, 6_250.0, LearnConfig::default());
    plan.learn(&extents);
    let phantom = plan.channels_in(FreqRange::new(lo + 1e6, hi - 1e6));
    assert!(
        phantom.is_empty(),
        "no channel between the two edges, where nothing transmitted: {phantom:?}"
    );
    for f in [lo, hi] {
        assert!(
            !plan
                .channels_in(FreqRange::new(f - 50e3, f + 50e3))
                .is_empty(),
            "the burst channel at {f} Hz is learned from its rollups"
        );
    }
}
